//! The images of one page: which `<img>` shows which file, which CSS background URL is which
//! file, and the compressed bytes.
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
    /// CSS `background-image` files by absolute URL (or the `data:` URL as written).
    by_url: HashMap<String, u32, FastBuildHasher>,
}

/// Where an `<img>` gets its pixels.
pub enum Source {
    Url(net::Url),
    /// A `data:` URL, already decoded.
    Inline(Vec<u8>),
}

/// The `<img>` elements worth loading and their sources, in document order. Several elements
/// can share one source.
/// Only nodes below `limit` count: the ones the parser made.
pub fn discover(dom: &Dom, base: &net::Url, limit: usize) -> (Vec<Source>, Vec<(NodeId, usize)>) {
    let mut sources: Vec<Source> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut uses = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if dom.tag_name(id) != Some("img") || id as usize >= limit {
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
        ImageStore {
            entries,
            by_node,
            by_url: HashMap::default(),
        }
    }

    /// Add CSS background files by URL (`None` for failures), within what is left of the
    /// budgets the page's `<img>` files share.
    pub fn add_backgrounds(&mut self, loaded: Vec<(String, Option<Vec<u8>>)>) {
        let mut total = self.bytes();
        for (url, data) in loaded {
            if self.by_url.contains_key(&url) || self.entries.len() >= MAX_IMAGES {
                continue;
            }
            let Some(mut data) =
                data.filter(|d| d.len() <= MAX_IMAGE_BYTES && total + d.len() <= MAX_TOTAL_BYTES)
            else {
                continue;
            };
            total += data.len();
            data.shrink_to_fit();
            let info = image::probe(&data);
            self.entries.push(Entry { data, info });
            self.by_url.insert(url, (self.entries.len() - 1) as u32);
        }
    }

    /// Whether a background URL is loaded already (a waking tab keeps its files).
    pub fn has_background(&self, url: &str) -> bool {
        self.by_url.contains_key(url)
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

    /// The compressed files, which element shows which, and which background URL is which,
    /// for storing a sleeping tab.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn parts(&self) -> Parts<'_> {
        let mut uses: Vec<(NodeId, u32)> = self.by_node.iter().map(|(&n, &i)| (n, i)).collect();
        uses.sort_unstable();
        let mut urls: Vec<(&str, u32)> =
            self.by_url.iter().map(|(u, &i)| (u.as_str(), i)).collect();
        urls.sort_unstable();
        (
            self.entries.iter().map(|e| e.data.as_slice()).collect(),
            uses,
            urls,
        )
    }

    /// Rebuild a store from `parts`. Uses that point past the files are dropped.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn from_parts(
        files: Vec<Vec<u8>>,
        uses: Vec<(NodeId, u32)>,
        urls: Vec<(String, u32)>,
    ) -> ImageStore {
        let entries: Vec<Entry> = files
            .into_iter()
            .map(|data| Entry {
                info: image::probe(&data),
                data,
            })
            .collect();
        let by_node = uses
            .into_iter()
            .filter(|&(_, i)| (i as usize) < entries.len())
            .collect();
        let by_url = urls
            .into_iter()
            .filter(|&(_, i)| (i as usize) < entries.len())
            .collect();
        ImageStore {
            entries,
            by_node,
            by_url,
        }
    }
}

/// What a sleeping tab stores of a page's images: files, element uses, background URLs.
pub type Parts<'a> = (Vec<&'a [u8]>, Vec<(NodeId, u32)>, Vec<(&'a str, u32)>);

impl paint::ImageSource for ImageStore {
    fn image(&self, node: NodeId) -> Option<(u64, &[u8])> {
        let (i, entry) = self.entry(node)?;
        entry.info?;
        Some((u64::from(i), &entry.data))
    }

    fn background(&self, url: &str) -> Option<(u64, &[u8], (u32, u32))> {
        let i = *self.by_url.get(url)?;
        let entry = &self.entries[i as usize];
        let info = entry.info?;
        Some((u64::from(i), &entry.data, (info.width, info.height)))
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
        let (sources, uses) = discover(&dom, &base, usize::MAX);
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
