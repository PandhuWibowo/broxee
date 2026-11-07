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
    pub styles: StyleMap,
    pub layout: Layout,
    pub fetch_ms: f64,
    pub parse_ms: f64,
    pub cascade_ms: f64,
    pub fonts_wait_ms: f64,
    pub layout_ms: f64,
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
    // The DOM holds everything later stages need; free the raw HTML before they run.
    let (response_url, status, bytes) = (resp.url, resp.status, resp.body.len());
    drop(resp.body);
    progress(0.5);

    Ok(finish(
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
        progress,
    ))
}

/// Build a page from HTML Broxee generated itself (error and message pages). No network.
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
    finish(origin, dom, settings, fonts, &mut |_| {})
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
    progress: &mut dyn FnMut(f32),
) -> Loaded {
    let (css, images) = load_subresources(&dom, &source.url, settings.fetch_css);
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
fn load_subresources(dom: &Dom, base: &net::Url, fetch: bool) -> (LoadedCss, ImageStore) {
    let mut jobs = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if jobs.len() >= MAX_STYLESHEETS {
            break;
        }
        match dom.tag_name(id) {
            Some("style") => jobs.push(Job::Css(CssSource::Inline(dom.text_content(id)))),
            Some("link") if fetch && is_stylesheet_link(dom, id) => {
                if let Some(url) = dom.attr(id, "href").and_then(|href| base.join(href).ok()) {
                    jobs.push(Job::Css(CssSource::External(url)));
                }
            }
            _ => {}
        }
    }
    let (image_sources, image_uses) = if fetch {
        images::discover(dom, base)
    } else {
        Default::default()
    };
    jobs.extend(image_sources.into_iter().map(Job::Image));

    let started = Instant::now();
    let results = pool::map_ordered(jobs, FETCH_WORKERS, |job| match job {
        Job::Css(source) => Done::Css(load_css(source)),
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
    for result in results {
        match result {
            Done::Css(result) => {
                loaded.parse_ms += result.parse_ms;
                loaded.fetched += usize::from(result.fetched);
                match result.sheet {
                    Some(sheet) => loaded.sheets.push(sheet),
                    None => loaded.failed += 1,
                }
            }
            Done::Image(bytes) => fetched_images.push(bytes),
        }
    }
    (loaded, ImageStore::new(fetched_images, image_uses))
}

fn load_css(source: CssSource) -> CssResult {
    let (text, fetched) = match source {
        CssSource::Inline(text) => (text, false),
        CssSource::External(url) => match net::fetch(&url) {
            // Stylesheets are almost always valid UTF-8: keep the buffer instead of copying.
            Ok(resp) if resp.status == 200 => match String::from_utf8(resp.body) {
                Ok(text) => (text, true),
                Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
            },
            _ => {
                return CssResult {
                    sheet: None,
                    fetched: false,
                    parse_ms: 0.0,
                }
            }
        },
    };
    let parse_started = Instant::now();
    let sheet = css::parse_stylesheet_owned(text);
    CssResult {
        sheet: Some(sheet),
        fetched,
        parse_ms: millis(parse_started),
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
