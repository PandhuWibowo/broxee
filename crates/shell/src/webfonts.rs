//! `@font-face` fonts: which faces a page's text uses, fetching them, and the face plan layout
//! and paint measure and draw with.
//!
//! Only faces that some styled text needs are fetched: after the cascade, every text node
//! (generated `::before`/`::after` text too) is matched against its family list, and each
//! character picks the first face whose `unicode-range` covers it. Font files are kept as they
//! arrived in the page's sources, so a sleeping tab wakes with its fonts and no network.

use std::collections::HashMap;
use std::time::Instant;

use css::{FontFace, StyleMap};
use font::{FaceDesc, FacePlan, Font, FontKey, Fonts};
use html::{Dom, NodeKind};

use crate::engine::{millis, FontStore};
use crate::pool;

/// Faces fetched per page at most; Google Fonts pages use two to six.
const MAX_FACES: usize = 24;
/// A font file larger than this is not loaded. Icon fonts are 100 to 400 KB as WOFF2.
const MAX_FILE_BYTES: usize = 4 << 20;
/// What a page's parsed faces may hold in memory together (outlines, advances, maps).
const MAX_FONT_MEMORY: usize = 8 << 20;
const FETCH_WORKERS: usize = 6;

/// The `@font-face` fonts a page loaded, and the plan built from its current styles.
#[derive(Default)]
pub struct WebFonts {
    /// Loaded faces; face `i` has id `font::WEB_FACES + i`.
    pub faces: Vec<Font>,
    descs: Vec<FaceDesc>,
    plan: FacePlan,
    /// Faces declared by the page's stylesheets.
    pub declared: usize,
    /// Faces wanted that did not load (no source worked, or over the limits).
    pub failed: usize,
    /// Memory the loaded faces hold.
    pub bytes: usize,
    pub ms: f64,
}

impl WebFonts {
    /// Rebuild the face chains for new styles (after any cascade).
    pub fn plan(&mut self, styles: &StyleMap, store: &FontStore) {
        self.plan = FacePlan::new(styles.families(), &self.descs, store.system());
    }

    /// Measurement and glyphs for this page: its faces, then the process's own.
    pub fn metrics<'a>(&'a self, store: &'a FontStore) -> Fonts<'a> {
        Fonts::new(store.system(), &self.faces, &self.plan)
    }
}

/// Where font files come from.
pub enum FontFrom<'a> {
    Network(Option<&'a net::Cookies>),
    /// A waking page: the files it stored, by URL.
    Stored(Vec<(String, net::Wire)>),
    Nowhere,
}

/// Find the faces the page's text uses, load them, and plan. Returns the fonts and the files as
/// they arrived, for the page's sources.
pub fn load(
    dom: &Dom,
    styles: &StyleMap,
    sheets: &[css::Stylesheet],
    page_url: &net::Url,
    from: FontFrom,
    store: &FontStore,
) -> (WebFonts, Vec<(String, net::Wire)>) {
    let started = Instant::now();
    let declared: Vec<&FontFace> = sheets.iter().flat_map(|s| &s.font_faces).collect();
    let mut fonts = WebFonts {
        declared: declared.len(),
        ..WebFonts::default()
    };
    if declared.is_empty() || matches!(from, FontFrom::Nowhere) {
        fonts.plan(styles, store);
        return (fonts, Vec::new());
    }
    let descs: Vec<FaceDesc> = declared.iter().map(|f| describe(f)).collect();
    let wanted = needed(dom, styles, &descs);

    let context = net::Context::subresource(page_url);
    let jar = match &from {
        FontFrom::Network(cookies) => cookies.map(|c| (c, &context)),
        _ => None,
    };
    let stored = match &from {
        FontFrom::Stored(files) => Some(files.as_slice()),
        _ => None,
    };
    let jobs: Vec<usize> = wanted.into_iter().take(MAX_FACES).collect();
    let results = pool::map_ordered(jobs.clone(), FETCH_WORKERS, |i| {
        fetch_face(declared[i], stored, jar)
    });
    let mut files = Vec::new();
    for (i, result) in jobs.into_iter().zip(results) {
        match result {
            Some((font, file)) if fonts.bytes + font.memory() <= MAX_FONT_MEMORY => {
                fonts.bytes += font.memory();
                fonts.faces.push(font);
                fonts.descs.push(descs[i].clone());
                files.extend(file);
            }
            _ => fonts.failed += 1,
        }
    }
    fonts.plan(styles, store);
    fonts.ms = millis(started);
    (fonts, files)
}

fn describe(face: &FontFace) -> FaceDesc {
    FaceDesc {
        family: face.family.clone(),
        weight: face.weight,
        italic: face.italic,
        ranges: face.ranges.clone(),
    }
}

/// Indices of the faces some text draws with, in declaration order.
fn needed(dom: &Dom, styles: &StyleMap, descs: &[FaceDesc]) -> Vec<usize> {
    let families = styles.families();
    let mut chains: HashMap<FontKey, Vec<usize>> = HashMap::new();
    let mut used = vec![false; descs.len()];
    for id in 0..dom.len() as html::NodeId {
        let NodeKind::Text(text) = dom.kind(id) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if dom
            .parent(id)
            .is_some_and(|p| styles.style(p).display == css::Display::None)
        {
            continue;
        }
        let style = styles.style(id);
        if style.font_family == 0 {
            continue;
        }
        let key = layout::font_key(style);
        let chain = chains.entry(key).or_insert_with(|| {
            let list = families.get(usize::from(key.family)).map_or(&[][..], |l| l);
            font::page_chain(list, key.weight, key.italic, descs)
        });
        if chain.iter().all(|&i| used[i]) {
            continue;
        }
        for ch in text.chars().filter(|c| !c.is_whitespace()) {
            if let Some(&i) = chain.iter().find(|&&i| descs[i].covers(ch as u32)) {
                used[i] = true;
            }
        }
    }
    (0..descs.len()).filter(|&i| used[i]).collect()
}

/// A face's file: its URL and its bytes as they arrived.
type FontFile = (String, net::Wire);

/// The first source of a face that loads as a font Broxee can draw, with its file to keep.
/// `data:` fonts are not kept: their stylesheet holds them already.
fn fetch_face(
    face: &FontFace,
    stored: Option<&[FontFile]>,
    jar: Option<(&net::Cookies, &net::Context)>,
) -> Option<(Font, Option<FontFile>)> {
    for source in &face.sources {
        if source.url.starts_with("data:") {
            let Some(bytes) = net::decode_data_url(&source.url) else {
                continue;
            };
            match Font::parse_any(&bytes) {
                Ok(font) if font.has_outlines() => return Some((font, None)),
                _ => continue,
            }
        }
        let (bytes, wire) = match stored {
            Some(files) => {
                let Some((_, wire)) = files.iter().find(|(u, _)| *u == source.url) else {
                    continue;
                };
                (wire.decode(MAX_FILE_BYTES).ok()?, wire.clone())
            }
            None => {
                let Ok(url) = net::Url::parse(&source.url) else {
                    continue;
                };
                match net::fetch_in(&url, jar) {
                    Ok(resp) if resp.status == 200 && resp.body.len() <= MAX_FILE_BYTES => {
                        let wire = if resp.wire.bytes.is_empty() {
                            net::Wire {
                                encoding: net::Encoding::Identity,
                                bytes: resp.body.clone(),
                            }
                        } else {
                            resp.wire
                        };
                        (resp.body, wire)
                    }
                    _ => continue,
                }
            }
        };
        match Font::parse_any(&bytes) {
            // CFF outlines measure but cannot be drawn yet: fall back to the next face.
            Ok(font) if font.has_outlines() => {
                return Some((font, Some((source.url.clone(), wire))))
            }
            Ok(_) => eprintln!("font: {} has CFF outlines, not drawn yet", source.url),
            Err(e) => eprintln!("font: {}: {e}", source.url),
        }
    }
    None
}

/// Resolve every `@font-face` source of a stylesheet against the URL it came from.
pub fn resolve_sources(sheet: &mut css::Stylesheet, base: &net::Url) {
    for face in &mut sheet.font_faces {
        face.sources.retain_mut(|source| {
            if source.url.starts_with("data:") {
                return true;
            }
            match base.join(&source.url) {
                Ok(url) => {
                    source.url = url.to_string();
                    true
                }
                Err(_) => false,
            }
        });
    }
    sheet.font_faces.retain(|f| !f.sources.is_empty());
}
