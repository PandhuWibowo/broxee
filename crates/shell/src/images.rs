//! The images of one page: which `<img>` shows which file, and the compressed bytes.
//!
//! Only compressed data is kept here, with the size read from its header for layout. Pixels
//! are decoded by the painter at the size they are drawn, and only while they are visible.

use std::collections::HashMap;

use html::{Dom, FastBuildHasher, NodeId};

/// Images fetched per page, in document order.
pub const MAX_IMAGES: usize = 128;
/// Compressed bytes kept per page; images past this are left unloaded. Together with the
/// pixel cache and the window's frames this has to fit the 60 MB budget.
pub const MAX_TOTAL_BYTES: usize = 12 << 20;
/// A single image larger than this is not kept.
pub const MAX_IMAGE_BYTES: usize = 4 << 20;

pub struct Entry {
    pub data: Vec<u8>,
    pub info: Option<image::Info>,
}

#[derive(Default)]
pub struct ImageStore {
    entries: Vec<Entry>,
    by_node: HashMap<NodeId, u32, FastBuildHasher>,
}

/// Where an `<img>` gets its pixels.
pub enum Source {
    Url(net::Url),
    /// A `data:` URL, already decoded.
    Inline(Vec<u8>),
}

/// The `<img>` elements worth loading and their sources, in document order. Several elements
/// can share one source.
pub fn discover(dom: &Dom, base: &net::Url) -> (Vec<Source>, Vec<(NodeId, usize)>) {
    let mut sources: Vec<Source> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut uses = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if dom.tag_name(id) != Some("img") {
            continue;
        }
        let Some(src) = image_src(dom, id) else {
            continue;
        };
        let index = match seen.get(src) {
            Some(&i) => i,
            None => {
                if sources.len() >= MAX_IMAGES {
                    continue;
                }
                let source = if src.trim_start().starts_with("data:") {
                    match net::decode_data_url(src) {
                        Some(bytes) => Source::Inline(bytes),
                        None => continue,
                    }
                } else {
                    match base.join(src) {
                        Ok(url) => Source::Url(url),
                        Err(_) => continue,
                    }
                };
                sources.push(source);
                seen.insert(src.to_string(), sources.len() - 1);
                sources.len() - 1
            }
        };
        uses.push((id, index));
    }
    (sources, uses)
}

/// `src`, or the first candidate of `srcset` when `src` is missing.
fn image_src(dom: &Dom, id: NodeId) -> Option<&str> {
    let src = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty());
    src.or_else(|| {
        let srcset = dom.attr(id, "srcset")?;
        srcset.split(',').next()?.split_whitespace().next()
    })
}

impl ImageStore {
    /// Keep the loaded images (`None` for failures), within the byte budget.
    pub fn new(loaded: Vec<Option<Vec<u8>>>, uses: Vec<(NodeId, usize)>) -> ImageStore {
        let mut total = 0;
        let mut kept: Vec<Option<u32>> = Vec::with_capacity(loaded.len());
        let mut entries = Vec::new();
        for data in loaded {
            let entry =
                data.filter(|d| d.len() <= MAX_IMAGE_BYTES && total + d.len() <= MAX_TOTAL_BYTES);
            kept.push(entry.map(|mut data| {
                total += data.len();
                data.shrink_to_fit();
                let info = image::probe(&data);
                entries.push(Entry { data, info });
                (entries.len() - 1) as u32
            }));
        }
        let by_node = uses
            .into_iter()
            .filter_map(|(node, source)| Some((node, kept.get(source).copied().flatten()?)))
            .collect();
        ImageStore { entries, by_node }
    }

    fn entry(&self, node: NodeId) -> Option<(u32, &Entry)> {
        let i = *self.by_node.get(&node)?;
        Some((i, &self.entries[i as usize]))
    }

    /// Natural size in CSS px, for layout.
    pub fn natural_size(&self, node: NodeId) -> Option<(f32, f32)> {
        let info = self.entry(node)?.1.info?;
        Some((info.width as f32, info.height as f32))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn bytes(&self) -> usize {
        self.entries.iter().map(|e| e.data.len()).sum()
    }
}

impl paint::ImageSource for ImageStore {
    fn image(&self, node: NodeId) -> Option<(u64, &[u8])> {
        let (i, entry) = self.entry(node)?;
        entry.info?;
        Some((u64::from(i), &entry.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_shared_and_inline_sources() {
        let dom = html::parse(
            "<img src=a.png><img src=a.png><img srcset='b.jpg 2x, c.jpg 3x'><img src=''>\
             <img src='data:image/gif;base64,R0lGODlh'><img>",
        );
        let base = net::Url::parse("https://e.com/dir/page").unwrap();
        let (sources, uses) = discover(&dom, &base);
        assert_eq!(sources.len(), 3);
        assert!(matches!(&sources[0], Source::Url(u) if u.path == "/dir/a.png"));
        assert!(matches!(&sources[1], Source::Url(u) if u.path == "/dir/b.jpg"));
        assert!(matches!(&sources[2], Source::Inline(b) if b.starts_with(b"GIF89a")));
        assert_eq!(uses.iter().map(|u| u.1).collect::<Vec<_>>(), [0, 0, 1, 2]);
    }

    #[test]
    fn keeps_images_within_the_budget() {
        let png = include_bytes!("../../image/src/testdata/rgba8.png").to_vec();
        let store = ImageStore::new(
            vec![Some(png), None, Some(vec![0; MAX_IMAGE_BYTES + 1])],
            vec![(1, 0), (2, 1), (3, 2), (4, 0)],
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.natural_size(1), Some((13.0, 9.0)));
        assert_eq!(store.natural_size(4), Some((13.0, 9.0)), "shared source");
        assert_eq!(store.natural_size(2), None, "failed fetch");
        assert_eq!(store.natural_size(3), None, "too large");
    }
}
