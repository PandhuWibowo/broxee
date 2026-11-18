//! The images of one page: which `<img>` shows which file, which CSS background URL is which
//! file, and the compressed bytes.
//!
//! Only compressed data is kept here, with the size read from its header for layout. Pixels
//! are decoded by the painter at the size they are drawn, and only while they are visible.
//!
//! Every file is kept by the source it came from (`key_of`: the absolute URL, or a hash of a
//! `data:` URL), and an `<img>` shows the file of its *current* source (`bind`). So images
//! that scripts add, or whose `src` they set later (lazy loading), find their file the same
//! way as the parser's, and a woken tab finds them again whatever ids its scripts' nodes get.
//! The parser's `<img>` elements are also bound by node, which a snapshot stores.

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
    /// Files by source: `<img>` sources by `key_of`, CSS `background-image` files by the URL
    /// as the style holds it (or the `data:` URL as written).
    by_url: HashMap<String, u32, FastBuildHasher>,
    /// Sources that failed or did not fit: not asked for again on this page.
    failed: std::collections::HashSet<String, FastBuildHasher>,
}

/// Where an `<img>` gets its pixels.
pub enum Source {
    Url(net::Url),
    /// A `data:` URL, already decoded.
    Inline(Vec<u8>),
}

/// The key an `<img>`'s current source is kept under: its absolute http, https or file URL,
/// or for a `data:` URL a hash of it (the URL itself can be large).
pub fn key_of(dom: &Dom, base: &net::Url, id: NodeId) -> Option<String> {
    let src = image_src(dom, id)?;
    if src.trim_start().starts_with("data:") {
        return Some(data_key(src));
    }
    let url = base.join(src).ok()?;
    matches!(url.scheme.as_str(), "http" | "https" | "file").then(|| url.to_string())
}

fn data_key(src: &str) -> String {
    // FNV-1a: the same text gives the same key on every machine and run.
    let hash = src.trim().bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("data:#{hash:016x}")
}

/// The `<img>` elements in the document (outside `<noscript>`), in order.
pub fn img_nodes(dom: &Dom, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    dom.descendants(root)
        .filter(move |&id| dom.tag_name(id) == Some("img") && !crate::engine::in_noscript(dom, id))
}

/// Each source once, with its key, and which source each `<img>` node uses.
pub type Discovered = (Vec<(String, Source)>, Vec<(NodeId, usize)>);

/// Files fetched by source: (key, bytes or `None` for a failure).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub type Fetched = Vec<(String, Option<Vec<u8>>)>;

/// The `<img>` elements worth loading and their sources (with their keys), in document
/// order. Several elements can share one source.
/// Only nodes below `limit` count: the ones the parser made.
pub fn discover(dom: &Dom, base: &net::Url, limit: usize) -> Discovered {
    let mut sources: Vec<(String, Source)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut uses = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if dom.tag_name(id) != Some("img")
            || id as usize >= limit
            || crate::engine::in_noscript(dom, id)
        {
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
                let Some(key) = key_of(dom, base, id) else {
                    continue;
                };
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
                sources.push((key, source));
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
    /// The parser's images: `loaded` (bytes or `None`) for each of `keys`, and which source
    /// each `<img>` node uses.
    pub fn new(
        keys: Vec<String>,
        loaded: Vec<Option<Vec<u8>>>,
        uses: Vec<(NodeId, usize)>,
    ) -> ImageStore {
        let mut store = ImageStore::default();
        let kept: Vec<Option<u32>> = keys
            .into_iter()
            .zip(loaded)
            .map(|(key, data)| store.add(key, data))
            .collect();
        store.by_node = uses
            .into_iter()
            .filter_map(|(node, source)| Some((node, kept.get(source).copied().flatten()?)))
            .collect();
        store
    }

    /// Keep `data` as the file of `key`, if it fits the budget; `None` (a failed fetch) and
    /// files that do not fit are remembered as failed. Returns the file's index.
    fn add(&mut self, key: String, data: Option<Vec<u8>>) -> Option<u32> {
        if let Some(&i) = self.by_url.get(&key) {
            return Some(i);
        }
        let total = self.bytes();
        let fits = |d: &Vec<u8>| d.len() <= MAX_IMAGE_BYTES && total + d.len() <= MAX_TOTAL_BYTES;
        let Some(mut data) = data.filter(|d| fits(d) && self.entries.len() < MAX_IMAGES) else {
            self.failed.insert(key);
            return None;
        };
        data.shrink_to_fit();
        let info = image::probe(&data);
        self.entries.push(Entry { data, info });
        let i = (self.entries.len() - 1) as u32;
        self.by_url.insert(key, i);
        Some(i)
    }

    /// Files fetched by source (CSS backgrounds, `<img>` sources a page added): kept by key.
    pub fn add_backgrounds(&mut self, loaded: Vec<(String, Option<Vec<u8>>)>) {
        for (key, data) in loaded {
            self.add(key, data);
        }
    }

    /// Whether a source is kept already (a waking tab keeps its files).
    pub fn has_background(&self, url: &str) -> bool {
        self.by_url.contains_key(url)
    }

    /// Point each `<img>` among `nodes` at the file of its current source. Returns the
    /// sources not kept yet (absolute URLs to fetch, each once, never one that failed); a
    /// `data:` source is decoded and kept at once. An `<img>` whose source is not kept shows
    /// nothing until it is.
    pub fn bind(
        &mut self,
        dom: &Dom,
        base: &net::Url,
        nodes: impl IntoIterator<Item = NodeId>,
    ) -> Vec<String> {
        let mut missing: Vec<String> = Vec::new();
        for id in nodes {
            let Some(key) = key_of(dom, base, id) else {
                self.by_node.remove(&id);
                continue;
            };
            if !self.by_url.contains_key(&key) && key.starts_with("data:#") {
                let bytes = image_src(dom, id).and_then(net::decode_data_url);
                self.add(key.clone(), bytes);
            }
            match self.by_url.get(&key) {
                Some(&i) => {
                    self.by_node.insert(id, i);
                }
                None => {
                    self.by_node.remove(&id);
                    if !self.failed.contains(&key) && !missing.contains(&key) {
                        missing.push(key);
                    }
                }
            }
        }
        let room = MAX_IMAGES.saturating_sub(self.entries.len());
        missing.truncate(room);
        missing
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
            failed: Default::default(),
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
        assert!(
            matches!(&sources[0], (k, Source::Url(u)) if u.path == "/dir/a.png"
            && k == "https://e.com/dir/a.png")
        );
        assert!(matches!(&sources[1], (_, Source::Url(u)) if u.path == "/dir/b.jpg"));
        assert!(
            matches!(&sources[2], (k, Source::Inline(b)) if b.starts_with(b"GIF89a")
            && k.starts_with("data:#"))
        );
        assert_eq!(uses.iter().map(|u| u.1).collect::<Vec<_>>(), [0, 0, 1, 2]);
    }

    #[test]
    fn keeps_images_within_the_budget() {
        let png = include_bytes!("../../image/src/testdata/rgba8.png").to_vec();
        let store = ImageStore::new(
            vec!["a".into(), "b".into(), "c".into()],
            vec![Some(png), None, Some(vec![0; MAX_IMAGE_BYTES + 1])],
            vec![(1, 0), (2, 1), (3, 2), (4, 0)],
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.natural_size(1), Some((13.0, 9.0)));
        assert_eq!(store.natural_size(4), Some((13.0, 9.0)), "shared source");
        assert_eq!(store.natural_size(2), None, "failed fetch");
        assert_eq!(store.natural_size(3), None, "too large");
    }

    #[test]
    fn binds_by_current_source_and_asks_once() {
        let png = include_bytes!("../../image/src/testdata/rgba8.png").to_vec();
        let base = net::Url::parse("https://e.com/dir/").unwrap();
        let mut dom = html::parse("<img src=a.png><img src=b.png><img src='data:,x'>");
        let imgs: Vec<NodeId> = img_nodes(&dom, Dom::ROOT).collect();
        let mut store = ImageStore::new(
            vec!["https://e.com/dir/a.png".into()],
            vec![Some(png)],
            vec![],
        );
        // Nothing bound by node: found by source; b.png is asked for, the data: URL decoded
        // (and kept as failed: it is not an image).
        assert_eq!(
            store.bind(&dom, &base, imgs.clone()),
            ["https://e.com/dir/b.png"]
        );
        assert_eq!(store.natural_size(imgs[0]), Some((13.0, 9.0)));
        assert_eq!(store.natural_size(imgs[1]), None);
        // b.png failed: not asked for again.
        store.add_backgrounds(vec![("https://e.com/dir/b.png".into(), None)]);
        assert!(store.bind(&dom, &base, imgs.clone()).is_empty());
        // A script points the first image elsewhere: unbound until that file is kept.
        dom.set_attr(imgs[0], "src", "c.png");
        assert_eq!(
            store.bind(&dom, &base, [imgs[0]]),
            ["https://e.com/dir/c.png"]
        );
        assert_eq!(store.natural_size(imgs[0]), None);
    }
}
