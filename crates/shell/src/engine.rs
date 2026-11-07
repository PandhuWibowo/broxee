//! The page pipeline (stages 1 to 9 of `docs/pipeline.md`), shared by the CLI and the window.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use css::{Origin, StyleMap};
use font::{Approximate, Font, FontSet, TextMetrics};
use html::{Dom, NodeId};
use layout::Layout;

use crate::images::{self, ImageStore};
use crate::pool;

/// External stylesheets fetched per page. Keeps a page with hundreds of `<link>`s from stalling.
const MAX_STYLESHEETS: usize = 32;
/// Concurrent subresource requests per page, the per-host limit browsers use.
const FETCH_WORKERS: usize = 6;
/// Until Figtree is bundled (see DESIGN.md), text uses a common system font.
const FONT_CANDIDATES: [(&str, &str); 3] = [
    (
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    ),
    (
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ),
    (
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    ),
];

/// What the page is laid out for.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub width: f32,
    pub height: f32,
    pub dark: bool,
    pub fetch_css: bool,
}

impl Settings {
    fn media(&self) -> css::Media {
        css::Media {
            width: self.width,
            height: self.height,
            dark: self.dark,
            ..css::Media::default()
        }
    }
}

/// The loaded fonts and the name of the regular face.
type Fonts = Option<(FontSet, String)>;

/// Fonts load once per process on their own thread (stage 1) and are shared by every page.
/// Only shared references are needed, so a page can load while the window renders with them.
#[derive(Default)]
pub struct FontStore {
    pending: Mutex<Option<JoinHandle<Fonts>>>,
    loaded: OnceLock<Fonts>,
}

impl FontStore {
    pub fn start() -> FontStore {
        FontStore {
            pending: Mutex::new(Some(std::thread::spawn(load_fonts))),
            loaded: OnceLock::new(),
        }
    }

    /// Wait for the loader if it is still running; returns the wait in ms.
    pub fn ready(&self) -> f64 {
        if self.loaded.get().is_some() {
            return 0.0;
        }
        let started = Instant::now();
        let handle = self.pending.lock().map(|mut p| p.take()).unwrap_or(None);
        let fonts = handle.and_then(|h| h.join().ok()).flatten();
        let _ = self.loaded.set(fonts);
        millis(started)
    }

    pub fn set(&self) -> Option<&FontSet> {
        self.loaded.get()?.as_ref().map(|(set, _)| set)
    }

    pub fn name(&self) -> &str {
        match self.loaded.get() {
            Some(Some((_, name))) => name,
            _ => "approximate",
        }
    }

    pub fn metrics(&self) -> &dyn TextMetrics {
        match self.set() {
            Some(set) => set,
            None => &Approximate,
        }
    }
}

fn load_fonts() -> Fonts {
    for (regular, bold) in FONT_CANDIDATES {
        if let Ok(font) = Font::load(Path::new(regular)) {
            let bold = Font::load(Path::new(bold)).ok();
            let name = Path::new(regular)
                .file_stem()
                .map_or(String::new(), |s| s.to_string_lossy().into_owned());
            return Some((
                FontSet {
                    regular: font,
                    bold,
                },
                name,
            ));
        }
    }
    None
}

/// Everything the engine produces for one page.
pub struct Loaded {
    pub url: net::Url,
    pub status: u16,
    pub bytes: usize,
    pub dom: Dom,
    pub css: LoadedCss,
    pub images: ImageStore,
    /// The HTML and stylesheets as they arrived, to rebuild the page without the network.
    pub sources: Sources,
    pub styles: StyleMap,
    pub layout: Layout,
    pub fetch_ms: f64,
    pub parse_ms: f64,
    pub cascade_ms: f64,
    pub fonts_wait_ms: f64,
    pub layout_ms: f64,
}

/// What a page was built from, as it arrived (compressed when the server compressed it): a
/// sleeping tab keeps this instead of the page, which is tens of times smaller.
#[derive(Default)]
pub struct Sources {
    pub html: net::Wire,
    /// External stylesheets by URL, in document order.
    pub stylesheets: Vec<(String, net::Wire)>,
}

impl Sources {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn bytes(&self) -> usize {
        self.html.bytes.len()
            + self
                .stylesheets
                .iter()
                .map(|(url, w)| url.len() + w.bytes.len())
                .sum::<usize>()
    }
}

/// A page put to sleep: its sources and compressed images, and nothing built from them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Snapshot {
    pub url: net::Url,
    pub status: u16,
    pub sources: Sources,
    pub images: ImageStore,
}

impl Snapshot {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn bytes(&self) -> usize {
        self.sources.bytes() + self.images.bytes()
    }
}

/// Drop everything built from a page (DOM, styles, layout), keeping what rebuilds it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn snapshot(page: Loaded) -> Snapshot {
    Snapshot {
        url: page.url,
        status: page.status,
        sources: page.sources,
        images: page.images,
    }
}

/// Rebuild a sleeping page from its snapshot, without the network.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn restore(
    snapshot: Snapshot,
    settings: Settings,
    fonts: &FontStore,
) -> Result<Loaded, String> {
    let started = Instant::now();
    let Snapshot {
        url,
        status,
        mut sources,
        images,
    } = snapshot;
    // The HTML goes back into the page as it was kept, ready for the next sleep.
    let html = std::mem::take(&mut sources.html);
    let (dom, bytes) = if html.encoding == net::Encoding::Identity {
        let text = String::from_utf8_lossy(&html.bytes);
        (html::parse(&text), html.bytes.len())
    } else {
        let decoded = html.decode(net::MAX_BODY)?;
        (
            html::parse(&String::from_utf8_lossy(&decoded)),
            decoded.len(),
        )
    };
    let source = Source {
        url,
        status,
        bytes,
        fetch_ms: 0.0,
        parse_ms: millis(started),
    };
    let mut page = finish(
        source,
        dom,
        settings,
        fonts,
        From::Snapshot(sources, images),
        &mut |_| {},
    );
    page.sources.html = html;
    Ok(page)
}

/// Where a page's subresources come from.
enum From {
    Network,
    /// A sleeping page waking: stylesheets from their stored bytes, images as they were.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Snapshot(Sources, ImageStore),
    /// Pages Broxee writes itself.
    Nowhere,
}

/// Fetch a page and build it. `progress` receives a fraction after each stage, for a
/// loading indicator.
pub fn load(
    url: &net::Url,
    settings: Settings,
    fonts: &FontStore,
    progress: &mut dyn FnMut(f32),
) -> Result<Loaded, String> {
    let started = Instant::now();
    let resp = net::fetch(url)?;
    let fetch_ms = millis(started);
    progress(0.4);

    let started = Instant::now();
    let dom = html::parse(&String::from_utf8_lossy(&resp.body));
    let parse_ms = millis(started);
    let (response_url, status, bytes) = (resp.url, resp.status, resp.body.len());
    // The DOM holds everything later stages need. Keep the HTML only as it arrived: the
    // compressed bytes when there are some, otherwise the body itself.
    let html = if resp.wire.bytes.is_empty() {
        net::Wire {
            encoding: net::Encoding::Identity,
            bytes: resp.body,
        }
    } else {
        drop(resp.body);
        resp.wire
    };
    progress(0.5);

    let from = if settings.fetch_css {
        From::Network
    } else {
        From::Nowhere
    };
    let mut page = finish(
        Source {
            url: response_url,
            status,
            bytes,
            fetch_ms,
            parse_ms,
        },
        dom,
        settings,
        fonts,
        from,
        progress,
    );
    page.sources.html = html;
    Ok(page)
}

/// Build a page from HTML Broxee generated itself (error and message pages). No network.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn page_from_html(
    url: net::Url,
    source: &str,
    settings: Settings,
    fonts: &FontStore,
) -> Loaded {
    let dom = html::parse(source);
    let settings = Settings {
        fetch_css: false,
        ..settings
    };
    let origin = Source {
        url,
        status: 200,
        bytes: source.len(),
        fetch_ms: 0.0,
        parse_ms: 0.0,
    };
    let mut page = finish(origin, dom, settings, fonts, From::Nowhere, &mut |_| {});
    page.sources.html = net::Wire {
        encoding: net::Encoding::Identity,
        bytes: source.as_bytes().to_vec(),
    };
    page
}

struct Source {
    url: net::Url,
    status: u16,
    bytes: usize,
    fetch_ms: f64,
    parse_ms: f64,
}

fn finish(
    source: Source,
    dom: Dom,
    settings: Settings,
    fonts: &FontStore,
    from: From,
    progress: &mut dyn FnMut(f32),
) -> Loaded {
    let (css, images, stylesheets) = load_subresources(&dom, &source.url, from);
    progress(0.75);
    // Join stage 1 before layout, the first stage that measures text.
    let fonts_wait_ms = fonts.ready();
    let (styles, cascade_ms, layout, layout_ms) =
        style_and_layout(&dom, &css, &images, settings, fonts);
    progress(0.95);
    Loaded {
        url: source.url,
        status: source.status,
        bytes: source.bytes,
        dom,
        css,
        images,
        sources: Sources {
            html: net::Wire::default(),
            stylesheets,
        },
        styles,
        layout,
        fetch_ms: source.fetch_ms,
        parse_ms: source.parse_ms,
        cascade_ms,
        fonts_wait_ms,
        layout_ms,
    }
}

/// Cascade and lay out again, for a new viewport size or color scheme. `@media` rules can
/// change, so the cascade reruns as well.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn relayout(page: &mut Loaded, settings: Settings, fonts: &FontStore) {
    let (styles, cascade_ms, layout, layout_ms) =
        style_and_layout(&page.dom, &page.css, &page.images, settings, fonts);
    page.styles = styles;
    page.cascade_ms = cascade_ms;
    page.layout = layout;
    page.layout_ms = layout_ms;
}

fn style_and_layout(
    dom: &Dom,
    css: &LoadedCss,
    images: &ImageStore,
    settings: Settings,
    fonts: &FontStore,
) -> (StyleMap, f64, Layout, f64) {
    let media = settings.media();
    let mut sheets: Vec<(Origin, &css::Stylesheet)> =
        vec![(Origin::UserAgent, css::user_agent_stylesheet())];
    let quirks = !dom
        .children(Dom::ROOT)
        .any(|c| matches!(dom.kind(c), html::NodeKind::Doctype(_)));
    if quirks {
        sheets.push((Origin::UserAgent, css::quirks_stylesheet()));
    }
    sheets.extend(css.sheets.iter().map(|s| (Origin::Author, s)));
    let started = Instant::now();
    let styles = css::cascade(dom, &sheets, &media);
    let cascade_ms = millis(started);
    let started = Instant::now();
    let natural = |node: NodeId| images.natural_size(node);
    let layout = layout::layout(
        dom,
        &styles,
        fonts.metrics(),
        &natural,
        media.width,
        media.height,
    );
    (styles, cascade_ms, layout, millis(started))
}

pub fn page_view<'a>(page: &'a Loaded, fonts: &'a FontStore) -> paint::Page<'a> {
    paint::Page {
        dom: &page.dom,
        styles: &page.styles,
        layout: &page.layout,
        metrics: fonts.metrics(),
        fonts: fonts.set(),
        images: Some(&page.images),
    }
}

pub struct LoadedCss {
    pub sheets: Vec<css::Stylesheet>,
    pub fetched: usize,
    pub failed: usize,
    /// Wall-clock time of the whole stage (fetches and parses overlap).
    pub wall_ms: f64,
    /// CPU time spent parsing, summed across threads.
    pub parse_ms: f64,
}

enum CssSource {
    Inline(String),
    External(net::Url),
}

struct CssResult {
    sheet: Option<css::Stylesheet>,
    fetched: bool,
    parse_ms: f64,
    /// An external sheet's URL and bytes as they arrived.
    wire: Option<(String, net::Wire)>,
}

/// One request of the subresource stage.
enum Job {
    Css(CssSource),
    Image(images::Source),
}

enum Done {
    Css(CssResult),
    Image(Option<Vec<u8>>),
}

/// Stylesheets and images (stages 5 to 7 in `docs/pipeline.md`). Both go through one bounded
/// pool; stylesheets are queued first because rendering waits for them. Stylesheets come back
/// in document order: `<style>` contents and `<link rel=stylesheet>` targets.
fn load_subresources(
    dom: &Dom,
    base: &net::Url,
    from: From,
) -> (LoadedCss, ImageStore, Vec<(String, net::Wire)>) {
    let external = !matches!(from, From::Nowhere);
    let mut jobs = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if jobs.len() >= MAX_STYLESHEETS {
            break;
        }
        match dom.tag_name(id) {
            Some("style") => jobs.push(Job::Css(CssSource::Inline(dom.text_content(id)))),
            Some("link") if external && is_stylesheet_link(dom, id) => {
                if let Some(url) = dom.attr(id, "href").and_then(|href| base.join(href).ok()) {
                    jobs.push(Job::Css(CssSource::External(url)));
                }
            }
            _ => {}
        }
    }
    let (stored_sheets, stored_images) = match from {
        From::Snapshot(sources, images) => (Some(sources.stylesheets), Some(images)),
        _ => (None, None),
    };
    let (image_sources, image_uses) = if external && stored_images.is_none() {
        images::discover(dom, base)
    } else {
        Default::default()
    };
    jobs.extend(image_sources.into_iter().map(Job::Image));

    // Rules that need names this document lacks are dropped while parsing.
    let vocabulary = css::Vocabulary::of(dom);
    let stored = stored_sheets.as_deref();
    let started = Instant::now();
    let results = pool::map_ordered(jobs, FETCH_WORKERS, |job| match job {
        Job::Css(source) => Done::Css(load_css(source, &vocabulary, stored)),
        Job::Image(images::Source::Inline(bytes)) => Done::Image(Some(bytes)),
        Job::Image(images::Source::Url(url)) => Done::Image(
            net::fetch(&url)
                .ok()
                .filter(|r| r.status == 200)
                .map(|r| r.body),
        ),
    });

    let mut loaded = LoadedCss {
        sheets: Vec::new(),
        fetched: 0,
        failed: 0,
        wall_ms: millis(started),
        parse_ms: 0.0,
    };
    let mut fetched_images = Vec::new();
    let mut wires = Vec::new();
    for result in results {
        match result {
            Done::Css(result) => {
                loaded.parse_ms += result.parse_ms;
                loaded.fetched += usize::from(result.fetched);
                match result.sheet {
                    Some(sheet) => loaded.sheets.push(sheet),
                    None => loaded.failed += 1,
                }
                wires.extend(result.wire);
            }
            Done::Image(bytes) => fetched_images.push(bytes),
        }
    }
    let images = match stored_images {
        Some(images) => images,
        None => ImageStore::new(fetched_images, image_uses),
    };
    (loaded, images, wires)
}

/// One stylesheet: inline text, or fetched (or, for a waking page, taken from its stored bytes).
fn load_css(
    source: CssSource,
    vocabulary: &css::Vocabulary,
    stored: Option<&[(String, net::Wire)]>,
) -> CssResult {
    let failed = CssResult {
        sheet: None,
        fetched: false,
        parse_ms: 0.0,
        wire: None,
    };
    let (bytes, wire, fetched) = match source {
        CssSource::Inline(text) => (text.into_bytes(), None, false),
        CssSource::External(url) => {
            let key = url.to_string();
            match stored {
                Some(sheets) => {
                    let Some((_, wire)) = sheets.iter().find(|(u, _)| *u == key) else {
                        return failed;
                    };
                    match wire.decode(net::MAX_BODY) {
                        Ok(bytes) => (bytes, Some((key, wire.clone())), false),
                        Err(_) => return failed,
                    }
                }
                None => match net::fetch(&url) {
                    Ok(mut resp) if resp.status == 200 => {
                        let wire = if resp.wire.bytes.is_empty() {
                            net::Wire {
                                encoding: net::Encoding::Identity,
                                bytes: resp.body.clone(),
                            }
                        } else {
                            std::mem::take(&mut resp.wire)
                        };
                        (resp.body, Some((key, wire)), true)
                    }
                    _ => return failed,
                },
            }
        }
    };
    // Stylesheets are almost always valid UTF-8: keep the buffer instead of copying.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let parse_started = Instant::now();
    let sheet = css::parse_stylesheet_for(text, vocabulary);
    CssResult {
        sheet: Some(sheet),
        fetched,
        parse_ms: millis(parse_started),
        wire,
    }
}

fn is_stylesheet_link(dom: &Dom, id: NodeId) -> bool {
    dom.attr(id, "rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|r| r.eq_ignore_ascii_case("stylesheet"))
    })
}

/// A URL from the command line: an absolute URL or a local file path.
pub fn to_url(target: &str) -> Result<net::Url, String> {
    if target.contains("://") {
        return net::Url::parse(target);
    }
    let path = std::fs::canonicalize(target).map_err(|e| format!("{target}: {e}"))?;
    net::Url::parse(&format!("file://{}", path.display()))
}

pub fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_rebuilds_the_same_page() {
        let src = "<title>T</title><style>p { margin: 4px }</style><p>one <b>two</b></p>\
                   <img src=\"data:image/png;base64,AAAA\"><p>three</p>";
        let before = page(src);
        let (boxes, texts) = (before.layout.boxes.len(), before.layout.texts.len());
        let height = before.layout.height;
        let snapshot = snapshot(before);
        assert_eq!(snapshot.sources.html.bytes, src.as_bytes());
        let settings = Settings {
            width: 400.0,
            height: 300.0,
            dark: false,
            fetch_css: false,
        };
        let after = restore(snapshot, settings, &FontStore::default()).expect("restores");
        assert_eq!(
            after.sources.html.bytes,
            src.as_bytes(),
            "kept for the next sleep"
        );
        assert_eq!(after.dom.title().as_deref(), Some("T"));
        assert_eq!(after.layout.boxes.len(), boxes);
        assert_eq!(after.layout.texts.len(), texts);
        assert_eq!(after.layout.height, height);
        assert_eq!(
            after.css.sheets.len(),
            1,
            "inline styles come from the HTML"
        );
    }

    #[test]
    #[ignore = "measures live sites"]
    fn measure_snapshots() {
        for site in [
            "https://github.com/",
            "https://en.wikipedia.org/wiki/Cat",
            "https://example.com/",
            "https://developer.mozilla.org/en-US/",
        ] {
            let fonts = FontStore::start();
            let settings = Settings {
                width: 1200.0,
                height: 800.0,
                dark: false,
                fetch_css: true,
            };
            let url = net::Url::parse(site).unwrap();
            let page = load(&url, settings, &fonts, &mut |_| {}).unwrap();
            let html_raw = page.bytes;
            let css: usize = page
                .sources
                .stylesheets
                .iter()
                .map(|(_, w)| w.bytes.len())
                .sum();
            let started = Instant::now();
            let snap = snapshot(page);
            let (src, img) = (snap.sources.bytes(), snap.images.bytes());
            let restored = restore(snap, settings, &fonts).unwrap();
            println!("{site}: html {} raw -> {} kept ({:?}), css kept {css}, images {img}, snapshot {} KB, wake {:.1} ms",
                html_raw, restored.sources.html.bytes.len(), restored.sources.html.encoding, (src + img) / 1024, millis(started));
        }
    }

    fn page(src: &str) -> Loaded {
        let settings = Settings {
            width: 400.0,
            height: 300.0,
            dark: false,
            fetch_css: false,
        };
        let url = net::Url::parse("file:///test.html").expect("valid URL");
        page_from_html(url, src, settings, &FontStore::default())
    }

    #[test]
    fn center_centers_tables_and_quirks_mode_resets_their_text() {
        let src = "<center><table width=100><tr><td>x</td></tr></table></center>";
        let quirky = page(src);
        let td = quirky.dom.find_first("td").expect("a cell");
        assert_eq!(
            quirky.styles.style(td).text_align,
            css::TextAlign::Left,
            "no doctype: tables reset text-align"
        );
        let table = quirky.dom.find_first("table").expect("a table");
        let rect = quirky
            .layout
            .boxes
            .iter()
            .find(|b| b.node == table)
            .expect("a table box")
            .rect;
        let body_margin = 8.0;
        assert_eq!(
            rect.x,
            body_margin + (400.0 - 2.0 * body_margin - 100.0) / 2.0
        );

        let standard = page(&format!("<!doctype html>{src}"));
        let td = standard.dom.find_first("td").expect("a cell");
        assert_eq!(
            standard.styles.style(td).text_align,
            css::TextAlign::WebkitCenter
        );
    }
}
