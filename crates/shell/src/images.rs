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
    /// `<img>` elements whose current source is not a 1x file sized by its own attributes
    /// (`srcset`, `<picture>`): (density, element with the size attributes). Set by `bind`.
    chosen: HashMap<NodeId, (f32, NodeId), FastBuildHasher>,
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
pub fn key_of(dom: &Dom, base: &net::Url, device: &Device, id: NodeId) -> Option<String> {
    let src = image_src(dom, id, device)?;
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

/// Sources whose `<img>` names its own `referrerpolicy`, by key: the first such element of
/// each source decides. Image hosts that refuse other sites' pages are often reached with
/// `referrerpolicy=no-referrer`.
pub fn own_policies(
    dom: &Dom,
    base: &net::Url,
    device: &Device,
) -> HashMap<String, net::ReferrerPolicy> {
    let mut policies = HashMap::new();
    for id in img_nodes(dom, Dom::ROOT) {
        let Some(policy) = dom
            .attr(id, "referrerpolicy")
            .and_then(net::ReferrerPolicy::parse)
        else {
            continue;
        };
        if let Some(key) = key_of(dom, base, device, id) {
            policies.entry(key).or_insert(policy);
        }
    }
    policies
}

/// Each source once, with its key, and which source each `<img>` node uses.
pub type Discovered = (Vec<(String, Source)>, Vec<(NodeId, usize)>);

/// A file fetched by source: (key, bytes or `None` for a failure).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub type Fetched = (String, Option<Vec<u8>>);

/// The `<img>` elements worth loading and their sources (with their keys), in document
/// order. Several elements can share one source.
/// Only nodes below `limit` count: the ones the parser made.
pub fn discover(dom: &Dom, base: &net::Url, device: &Device, limit: usize) -> Discovered {
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
        let Some(src) = image_src(dom, id, device) else {
            continue;
        };
        let index = match seen.get(src) {
            Some(&i) => i,
            None => {
                if sources.len() >= MAX_IMAGES {
                    continue;
                }
                let Some(key) = key_of(dom, base, device, id) else {
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

/// What an `<img>`'s file is chosen for: the viewport (`sizes`, `<source media>`) and the
/// device pixels per CSS px. The same for every `<img>` of a page while it is built, so an
/// element's key stays the same; a page built at another width or scale may choose other
/// files, which are then just sources it does not have yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Device {
    pub media: css::Media,
    pub scale: f32,
}

impl Default for Device {
    fn default() -> Self {
        Device {
            media: css::Media::default(),
            scale: 1.0,
        }
    }
}

/// The file an `<img>` shows and how to size it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Choice<'a> {
    src: &'a str,
    /// File pixels per CSS px: the natural size is the file's divided by this.
    density: f32,
    /// The element whose `width`/`height` attributes size the image (`layout::Images`).
    sized_by: NodeId,
}

/// The current source of an `<img>` (HTML, "select an image source"): the first `<source>`
/// before it in a `<picture>` whose `media` matches, whose `type` Broxee decodes and whose
/// `srcset` has a candidate; else its own `srcset`, with `src` as the 1x candidate. Among
/// the candidates, the smallest density at least the device scale, else the largest.
fn choose<'a>(dom: &'a Dom, id: NodeId, device: &Device) -> Option<Choice<'a>> {
    let src = dom.attr(id, "src").map(str::trim).filter(|s| !s.is_empty());
    let picture = dom
        .parent(id)
        .filter(|&p| dom.tag_name(p) == Some("picture"));
    if let Some(picture) = picture {
        for child in dom.children(picture) {
            if child == id {
                break;
            }
            if dom.tag_name(child) != Some("source") {
                continue;
            }
            let Some(srcset) = dom.attr(child, "srcset") else {
                continue;
            };
            let media = dom.attr(child, "media");
            if media.is_some_and(|m| !css::MediaList::parse(m).matches(&device.media))
                || dom.attr(child, "type").is_some_and(|t| !decodable(t))
            {
                continue;
            }
            let candidates = parse_srcset(srcset);
            let size = source_size(dom.attr(child, "sizes"), &device.media);
            let Some((src, density)) = pick(&candidates, size, device.scale) else {
                continue;
            };
            let sized = dom.attr(child, "width").is_some() || dom.attr(child, "height").is_some();
            return Some(Choice {
                src,
                density,
                sized_by: if sized { child } else { id },
            });
        }
    }
    let Some(srcset) = dom.attr(id, "srcset") else {
        return src.map(|src| Choice {
            src,
            density: 1.0,
            sized_by: id,
        });
    };
    let mut candidates = parse_srcset(srcset);
    let widths = candidates
        .iter()
        .any(|c| matches!(c.descriptor, Descriptor::Width(_)));
    if let Some(src) = src {
        if !widths
            && !candidates
                .iter()
                .any(|c| c.descriptor == Descriptor::Density(1.0))
        {
            candidates.push(Candidate {
                url: src,
                descriptor: Descriptor::Density(1.0),
            });
        }
    }
    let size = if widths {
        source_size(dom.attr(id, "sizes"), &device.media)
    } else {
        0.0
    };
    let (src, density) = pick(&candidates, size, device.scale)?;
    Some(Choice {
        src,
        density,
        sized_by: id,
    })
}

/// The URL (or `data:` URL) of an `<img>`'s current source.
fn image_src<'a>(dom: &'a Dom, id: NodeId, device: &Device) -> Option<&'a str> {
    choose(dom, id, device).map(|c| c.src)
}

/// A `<source type>` Broxee can decode.
fn decodable(mime: &str) -> bool {
    let mime = mime.split(';').next().unwrap_or("").trim();
    [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/gif",
        "image/webp",
        "image/svg+xml",
    ]
    .iter()
    .any(|m| m.eq_ignore_ascii_case(mime))
}

/// A `srcset` candidate's descriptor (`h` is parsed and ignored, as the standard says).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Descriptor {
    /// `2x`; a candidate without a descriptor is `1x`.
    Density(f32),
    /// `640w`: the file's width in pixels, compared with `sizes`.
    Width(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Candidate<'a> {
    url: &'a str,
    descriptor: Descriptor,
}

/// The candidates of a `srcset` attribute (HTML, "parse a srcset attribute"): a URL ends at
/// whitespace, commas ending it end the candidate, descriptors run to the next comma outside
/// parentheses. Candidates with descriptors that do not parse are dropped. Slices only at
/// ASCII bytes.
fn parse_srcset(s: &str) -> Vec<Candidate<'_>> {
    let b = s.as_bytes();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c);
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && (space(b[i]) || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            return out;
        }
        let start = i;
        while i < b.len() && !space(b[i]) {
            i += 1;
        }
        let mut url = &s[start..i];
        let mut descriptors: Vec<&str> = Vec::new();
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            // Descriptors: tokens split at whitespace, up to a comma outside parentheses.
            let mut token = i;
            let mut depth = 0u32;
            while i < b.len() {
                match b[i] {
                    b'(' => depth += 1,
                    b')' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => break,
                    c if space(c) && depth == 0 => {
                        if token < i {
                            descriptors.push(&s[token..i]);
                        }
                        token = i + 1;
                    }
                    _ => {}
                }
                i += 1;
            }
            if token < i {
                descriptors.push(&s[token..i]);
            }
            // Past the comma.
            i += 1;
        }
        if let Some(descriptor) = parse_descriptors(&descriptors) {
            out.push(Candidate { url, descriptor });
        }
    }
}

/// A candidate's descriptors, or `None` when they are invalid.
fn parse_descriptors(list: &[&str]) -> Option<Descriptor> {
    let (mut width, mut density, mut height) = (None, None, None);
    for d in list {
        let (&kind, value) = d.as_bytes().split_last()?;
        // A descriptor ending inside a multi-byte character is invalid anyway.
        let value = std::str::from_utf8(value).ok()?;
        match kind {
            b'w' if width.is_none() && density.is_none() => {
                width = Some(integer(value).filter(|&w| w > 0)?);
            }
            b'x' if width.is_none() && density.is_none() && height.is_none() => {
                density = Some(float(value).filter(|&x| x > 0.0)?);
            }
            b'h' if height.is_none() && density.is_none() => {
                height = Some(integer(value).filter(|&h| h > 0)?);
            }
            _ => return None,
        }
    }
    if height.is_some() && width.is_none() {
        return None;
    }
    Some(match (width, density) {
        (Some(w), _) => Descriptor::Width(w),
        (None, Some(x)) => Descriptor::Density(x),
        (None, None) => Descriptor::Density(1.0),
    })
}

/// A valid non-negative integer: ASCII digits only.
fn integer(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// A valid floating-point number (HTML): `-`?, digits with an optional fraction (or only a
/// fraction), an optional exponent. Stricter than Rust's parser (`1.`, `+1`, `inf`).
fn float(s: &str) -> Option<f32> {
    let b = s.strip_prefix('-').unwrap_or(s).as_bytes();
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut i = digits(0);
    let whole = i;
    if b.get(i) == Some(&b'.') {
        let fraction = digits(i + 1);
        if fraction == 0 {
            return None;
        }
        i += 1 + fraction;
    } else if whole == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent = digits(i);
        if exponent == 0 {
            return None;
        }
        i += exponent;
    }
    (i == b.len())
        .then(|| s.parse::<f32>().ok())
        .flatten()
        .filter(|v| v.is_finite())
}

/// The width in CSS px an image with `w` candidates is drawn at, from `sizes` (HTML, "parse a
/// sizes attribute"): the first `(media condition) length` whose condition matches, or a
/// length alone, which ends the list. Entries that do not parse (`calc()`, `auto`, unknown
/// units) are skipped. Without one, the viewport width.
fn source_size(sizes: Option<&str>, media: &css::Media) -> f32 {
    let sizes = sizes.unwrap_or("");
    let b = sizes.as_bytes();
    let mut depth = 0u32;
    let mut start = 0;
    for i in 0..=b.len() {
        match b.get(i) {
            Some(b'(') => {
                depth += 1;
                continue;
            }
            Some(b')') => {
                depth = depth.saturating_sub(1);
                continue;
            }
            Some(b',') if depth == 0 => {}
            None => {}
            _ => continue,
        }
        let entry = sizes[start..i].trim();
        start = i + 1;
        if let Some(size) = size_entry(entry, media) {
            return size;
        }
    }
    media.width
}

/// One `sizes` entry's length, when it parses and its condition (if any) matches.
fn size_entry(entry: &str, media: &css::Media) -> Option<f32> {
    let (condition, length) = match entry.rfind(|c: char| c.is_ascii_whitespace() || c == ')') {
        Some(at) => (&entry[..=at], &entry[at + 1..]),
        None => ("", entry),
    };
    let size = source_length(length, media)?;
    let condition = condition.trim();
    if condition.is_empty() {
        return Some(size);
    }
    let lower = condition.to_ascii_lowercase();
    let is_condition = lower.starts_with('(') || lower.starts_with("not ");
    (is_condition && css::MediaList::parse(condition).matches(media)).then_some(size)
}

/// A `sizes` length in CSS px: px, em and rem (16 px), vw, vh, vmin, vmax, or a bare 0.
fn source_length(s: &str, media: &css::Media) -> Option<f32> {
    if s == "0" {
        return Some(0.0);
    }
    let number = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &s[number.len()..];
    let v = float(number).filter(|&v| v >= 0.0)?;
    let unit = unit.to_ascii_lowercase();
    let per = match unit.as_str() {
        "px" => 1.0,
        "em" | "rem" => 16.0,
        "vw" => media.width / 100.0,
        "vh" => media.height / 100.0,
        "vmin" => media.width.min(media.height) / 100.0,
        "vmax" => media.width.max(media.height) / 100.0,
        _ => return None,
    };
    Some(v * per)
}

/// The candidate for a device `scale`, with its density: the smallest density at least the
/// scale, else the largest (the first of equal ones). `w` candidates have the density of their
/// width over the drawn `size`.
fn pick<'a>(candidates: &[Candidate<'a>], size: f32, scale: f32) -> Option<(&'a str, f32)> {
    let density = |c: &Candidate| match c.descriptor {
        Descriptor::Density(x) => x,
        Descriptor::Width(w) => w as f32 / size.max(1.0),
    };
    let mut enough: Option<(&str, f32)> = None;
    let mut largest: Option<(&str, f32)> = None;
    for c in candidates {
        let d = density(c);
        if d >= scale && enough.is_none_or(|(_, e)| d < e) {
            enough = Some((c.url, d));
        }
        if largest.is_none_or(|(_, l)| d > l) {
            largest = Some((c.url, d));
        }
    }
    enough.or(largest)
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

    /// Whether a source is neither kept nor known to fail.
    pub fn wants(&self, url: &str) -> bool {
        !self.by_url.contains_key(url) && !self.failed.contains(url)
    }

    /// Point each `<img>` among `nodes` at the file of its current source. Returns the
    /// sources not kept yet (absolute URLs to fetch, each once, never one that failed); a
    /// `data:` source is decoded and kept at once. An `<img>` whose source is not kept shows
    /// nothing until it is.
    pub fn bind(
        &mut self,
        dom: &Dom,
        base: &net::Url,
        device: &Device,
        nodes: impl IntoIterator<Item = NodeId>,
    ) -> Vec<String> {
        let mut missing: Vec<String> = Vec::new();
        for id in nodes {
            match choose(dom, id, device) {
                Some(c) if c.density != 1.0 || c.sized_by != id => {
                    self.chosen.insert(id, (c.density, c.sized_by));
                }
                _ => {
                    self.chosen.remove(&id);
                }
            }
            let Some(key) = key_of(dom, base, device, id) else {
                self.by_node.remove(&id);
                continue;
            };
            if !self.by_url.contains_key(&key) && key.starts_with("data:#") {
                let bytes = image_src(dom, id, device).and_then(net::decode_data_url);
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

    /// Natural size in CSS px, for layout: the file's size over the density it was chosen
    /// for (a `2x` file of 200 pixels is 100 CSS px wide).
    pub fn natural_size(&self, node: NodeId) -> Option<(f32, f32)> {
        let info = self.entry(node)?.1.info?;
        let density = self.chosen.get(&node).map_or(1.0, |c| c.0);
        Some((info.width as f32 / density, info.height as f32 / density))
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
            chosen: Default::default(),
            by_url,
            failed: Default::default(),
        }
    }
}

/// What a sleeping tab stores of a page's images: files, element uses, background URLs.
pub type Parts<'a> = (Vec<&'a [u8]>, Vec<(NodeId, u32)>, Vec<(&'a str, u32)>);

impl layout::Images for ImageStore {
    fn natural(&self, node: NodeId) -> Option<(f32, f32)> {
        self.natural_size(node)
    }

    fn sized_by(&self, node: NodeId) -> NodeId {
        self.chosen.get(&node).map_or(node, |c| c.1)
    }
}

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
        let (sources, uses) = discover(&dom, &base, &Device::default(), usize::MAX);
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
            store.bind(&dom, &base, &Device::default(), imgs.clone()),
            ["https://e.com/dir/b.png"]
        );
        assert_eq!(store.natural_size(imgs[0]), Some((13.0, 9.0)));
        assert_eq!(store.natural_size(imgs[1]), None);
        // b.png failed: not asked for again.
        store.add_backgrounds(vec![("https://e.com/dir/b.png".into(), None)]);
        assert!(store
            .bind(&dom, &base, &Device::default(), imgs.clone())
            .is_empty());
        // A script points the first image elsewhere: unbound until that file is kept.
        dom.set_attr(imgs[0], "src", "c.png");
        assert_eq!(
            store.bind(&dom, &base, &Device::default(), [imgs[0]]),
            ["https://e.com/dir/c.png"]
        );
        assert_eq!(store.natural_size(imgs[0]), None);
    }

    fn device(width: f32, scale: f32) -> Device {
        Device {
            media: css::Media {
                width,
                height: 800.0,
                ..css::Media::default()
            },
            scale,
        }
    }

    fn urls(srcset: &str) -> Vec<(&str, Descriptor)> {
        parse_srcset(srcset)
            .into_iter()
            .map(|c| (c.url, c.descriptor))
            .collect()
    }

    #[test]
    fn parses_srcset_candidates() {
        use Descriptor::{Density, Width};
        assert_eq!(
            urls(" a.png, b.png 2x ,c.png 640w\n,d.png 1.5x"),
            [
                ("a.png", Density(1.0)),
                ("b.png", Density(2.0)),
                ("c.png", Width(640)),
                ("d.png", Density(1.5)),
            ]
        );
        // Commas ending a URL end its candidate; commas inside it stay (data: URLs).
        assert_eq!(urls("a.png,b.png 2x"), [("a.png,b.png", Density(2.0))]);
        assert_eq!(
            urls("a.png,, b.png 2x,,, c.png"),
            [
                ("a.png", Density(1.0)),
                ("b.png", Density(2.0)),
                ("c.png", Density(1.0)),
            ]
        );
        assert_eq!(
            urls("data:image/png;base64,AA== 1x, b.png 2x"),
            [
                ("data:image/png;base64,AA==", Density(1.0)),
                ("b.png", Density(2.0)),
            ]
        );
        // `h` goes with `w` and is ignored.
        assert_eq!(urls("a.png 100w 50h"), [("a.png", Width(100))]);
        assert_eq!(urls("a.png 1e1x"), [("a.png", Density(10.0))]);
    }

    #[test]
    fn drops_malformed_srcset_candidates() {
        for bad in [
            "a.png 2x 3x",
            "a.png 1.x",
            "a.png .x",
            "a.png +2x",
            "a.png infx",
            "a.png 10w 2x",
            "a.png 2x 10w",
            "a.png 100h",
            "a.png 0w",
            "a.png -1x",
            "a.png 0x",
            "a.png 2y",
            "a.png 2X",
            "a.png 10.5w",
            "a.png 2é",
            "a.png (1x)",
        ] {
            assert!(parse_srcset(bad).is_empty(), "{bad}");
        }
        assert_eq!(
            urls("a.png 2x 3x, b.png 2x"),
            [("b.png", Descriptor::Density(2.0))]
        );
        assert!(parse_srcset("").is_empty());
        assert!(parse_srcset(" ,, \t,").is_empty());
        // Nothing panics, wherever the input is cut.
        let nasty = "a.png 2é, (b, c) 1x,é.png  3x ,( ,, d.png 1e, e.png 1e+2x))";
        for (i, _) in nasty.char_indices() {
            parse_srcset(&nasty[..i]);
            parse_srcset(&nasty[i..]);
            source_size(Some(&nasty[i..]), &css::Media::default());
        }
    }

    #[test]
    fn evaluates_sizes() {
        let wide = device(1280.0, 1.0).media;
        let narrow = device(500.0, 1.0).media;
        assert_eq!(source_size(None, &wide), 1280.0);
        assert_eq!(source_size(Some(""), &wide), 1280.0);
        let sizes = Some("(max-width: 600px) 50vw, 300px");
        assert_eq!(source_size(sizes, &wide), 300.0);
        assert_eq!(source_size(sizes, &narrow), 250.0);
        let em = Some("(min-width:600px)10em,2rem");
        assert_eq!(source_size(em, &wide), 160.0);
        assert_eq!(source_size(em, &narrow), 32.0);
        // Entries that do not parse are skipped: functions, media types, negative or
        // unitless lengths, unknown units.
        assert_eq!(source_size(Some("calc(100vw - 2em), 20em"), &wide), 320.0);
        assert_eq!(source_size(Some("screen 10px, 2rem"), &wide), 32.0);
        assert_eq!(source_size(Some("-5px, 12, 3cm, auto, 10px"), &wide), 10.0);
        assert_eq!(source_size(Some("(min-width: 9999px) 10px"), &wide), 1280.0);
        assert_eq!(
            source_size(Some("not (max-width: 600px) 640px"), &wide),
            640.0
        );
        assert_eq!(source_size(Some("0"), &wide), 0.0);
        assert_eq!(source_size(Some("50vh, 10px"), &wide), 400.0);
        assert_eq!(source_size(Some("10vmin"), &narrow), 50.0);
        // A length alone ends the list.
        assert_eq!(
            source_size(Some("100px, (min-width: 1px) 5px"), &wide),
            100.0
        );
    }

    #[test]
    fn picks_by_density_and_scale() {
        let x = parse_srcset("a 1x, b 2x, c 3x");
        assert_eq!(pick(&x, 0.0, 1.0), Some(("a", 1.0)));
        assert_eq!(pick(&x, 0.0, 1.5), Some(("b", 2.0)));
        assert_eq!(pick(&x, 0.0, 2.0), Some(("b", 2.0)));
        assert_eq!(pick(&x, 0.0, 4.0), Some(("c", 3.0)), "else the largest");
        // `w` candidates: their width over the drawn size.
        let w = parse_srcset("s 400w, l 1600w, m 800w");
        assert_eq!(pick(&w, 400.0, 1.0), Some(("s", 1.0)));
        assert_eq!(pick(&w, 400.0, 2.0), Some(("m", 2.0)));
        assert_eq!(pick(&w, 1280.0, 1.0), Some(("l", 1.25)));
        // Equal densities: the first.
        let same = parse_srcset("p 2x, q 2x");
        assert_eq!(pick(&same, 0.0, 1.0), Some(("p", 2.0)));
        assert_eq!(pick(&[], 0.0, 1.0), None);
    }

    fn chosen<'a>(dom: &'a Dom, d: &Device) -> Vec<(&'a str, f32, bool)> {
        img_nodes(dom, Dom::ROOT)
            .map(|id| {
                let c = choose(dom, id, d).unwrap();
                (c.src, c.density, c.sized_by != id)
            })
            .collect()
    }

    #[test]
    fn img_src_is_the_1x_candidate() {
        let dom = html::parse(
            "<img src=a.png srcset='b.png 2x'>\
             <img src=a.png srcset='b.png 1x, c.png 2x'>\
             <img src=a.png srcset='b.png 400w' sizes=400px>\
             <img src=a.png srcset='nonsense 2y'>\
             <img srcset='b.png 2x'>\
             <img src=a.png>",
        );
        assert_eq!(
            chosen(&dom, &device(1280.0, 1.0)),
            [
                ("a.png", 1.0, false),
                ("b.png", 1.0, false),
                ("b.png", 1.0, false),
                ("a.png", 1.0, false),
                ("b.png", 2.0, false),
                ("a.png", 1.0, false),
            ]
        );
        assert_eq!(
            chosen(&dom, &device(1280.0, 2.0)),
            [
                ("b.png", 2.0, false),
                ("c.png", 2.0, false),
                ("b.png", 1.0, false),
                ("a.png", 1.0, false),
                ("b.png", 2.0, false),
                ("a.png", 1.0, false),
            ]
        );
    }

    #[test]
    fn picture_takes_the_first_matching_source() {
        let dom = html::parse(
            "<picture>\
               <source type=image/avif srcset=a.avif>\
               <source srcset=''>\
               <source media='(max-width: 600px)' srcset=narrow.png>\
               <source type='image/WebP; codecs=x' srcset='wide.webp, wide2.webp 2x' \
                       width=88 height=31>\
               <img src=fallback.png width=25 height=25>\
               <source srcset=after.png>\
             </picture>\
             <picture><source type=image/avif srcset=a.avif><img src=only.png></picture>\
             <picture><source media=print srcset=p.png>\
               <img src=own.png srcset='own2.png 2x'></picture>",
        );
        assert_eq!(
            chosen(&dom, &device(1280.0, 1.0)),
            [
                ("wide.webp", 1.0, true),
                ("only.png", 1.0, false),
                ("own.png", 1.0, false),
            ]
        );
        assert_eq!(
            chosen(&dom, &device(1280.0, 2.0)),
            [
                ("wide2.webp", 2.0, true),
                ("only.png", 1.0, false),
                ("own2.png", 2.0, false),
            ]
        );
        assert_eq!(
            chosen(&dom, &device(500.0, 2.0))[0],
            ("narrow.png", 1.0, false)
        );
    }

    #[test]
    fn wikipedia_thumbnails_at_2x() {
        let dom = html::parse(
            "<img src='//upload.wikimedia.org/thumb/R.png/220px-R.png' \
             srcset='//upload.wikimedia.org/thumb/R.png/330px-R.png 1.5x, \
             //upload.wikimedia.org/thumb/R.png/440px-R.png 2x' width=220 height=180>",
        );
        let base = net::Url::parse("https://en.wikipedia.org/wiki/Rust").unwrap();
        let img = img_nodes(&dom, Dom::ROOT).next().unwrap();
        let key = |scale| key_of(&dom, &base, &device(1280.0, scale), img).unwrap();
        let thumb = |px: u32| format!("https://upload.wikimedia.org/thumb/R.png/{px}px-R.png");
        assert_eq!(key(1.0), thumb(220));
        assert_eq!(key(1.5), thumb(330));
        assert_eq!(key(2.0), thumb(440));
        // The 2x file is kept by its URL and drawn at half its pixel size.
        let png = include_bytes!("../../image/src/testdata/rgba8.png").to_vec();
        let mut store = ImageStore::new(vec![key(2.0)], vec![Some(png)], vec![]);
        assert!(store
            .bind(&dom, &base, &device(1280.0, 2.0), [img])
            .is_empty());
        assert_eq!(store.natural_size(img), Some((6.5, 4.5)));
        assert_eq!(layout::Images::sized_by(&store, img), img);
        // At 1x the same page asks for the 1x file.
        assert_eq!(
            store.bind(&dom, &base, &device(1280.0, 1.0), [img]),
            [key(1.0)]
        );
    }

    #[test]
    fn a_chosen_source_sizes_its_img() {
        let dom = html::parse(
            "<picture><source srcset=wide.png width=88 height=31>\
             <img src=small.png width=25 height=25></picture>",
        );
        let base = net::Url::parse("https://e.com/").unwrap();
        let img = img_nodes(&dom, Dom::ROOT).next().unwrap();
        let source = dom
            .parent(img)
            .and_then(|p| dom.children(p).next())
            .unwrap();
        let mut store = ImageStore::default();
        let d = device(1280.0, 1.0);
        assert_eq!(
            store.bind(&dom, &base, &d, [img]),
            ["https://e.com/wide.png"]
        );
        assert_eq!(layout::Images::sized_by(&store, img), source);
        // An `<img>` outside a `<picture>` is sized by itself.
        let plain = html::parse("<img src=small.png width=25 height=25>");
        let only = img_nodes(&plain, Dom::ROOT).next().unwrap();
        store.bind(&plain, &base, &d, [only]);
        assert_eq!(layout::Images::sized_by(&store, only), only);
    }
}
