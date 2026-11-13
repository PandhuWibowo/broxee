//! The page pipeline (stages 1 to 9 of `docs/pipeline.md`), shared by the CLI and the window.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use css::{Origin, StyleMap};
use font::{Font, Fonts, Role, SystemFace, SystemFaces};
use html::{Dom, NodeId};
use layout::Layout;

use crate::forms::Forms;
use crate::images::{self, ImageStore};
use crate::pool;
use crate::webfonts::{self, FontFrom, WebFonts};

/// External stylesheets fetched per page. Keeps a page with hundreds of `<link>`s from stalling.
const MAX_STYLESHEETS: usize = 32;
/// Concurrent subresource requests per page, the per-host limit browsers use.
const FETCH_WORKERS: usize = 6;
/// The default face of pages: a common system font. Broxee's own interface uses Figtree, which
/// is bundled (DESIGN.md).
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
/// Fallback for characters the other faces lack: the modifier key symbols ⌘ ⇧ ⌥ (macOS).
const SYMBOL_FONTS: [&str; 1] = ["/System/Library/Fonts/Keyboard.ttf"];

/// What the page is laid out for.
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// The process's own faces and the name of the default page face.
type ProcessFonts = (SystemFaces, String);

/// Fonts load once per process on their own thread (stage 1) and are shared by every page.
/// Only shared references are needed, so a page can load while the window renders with them.
#[derive(Default)]
pub struct FontStore {
    pending: Mutex<Option<JoinHandle<ProcessFonts>>>,
    loaded: OnceLock<ProcessFonts>,
}

/// Before the fonts load (and in tests), text is measured as `font::Approximate` does.
static NO_FACES: OnceLock<SystemFaces> = OnceLock::new();

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
        if let Some(fonts) = handle.and_then(|h| h.join().ok()) {
            let _ = self.loaded.set(fonts);
        }
        millis(started)
    }

    pub fn system(&self) -> &SystemFaces {
        match self.loaded.get() {
            Some((faces, _)) => faces,
            None => NO_FACES.get_or_init(SystemFaces::default),
        }
    }

    pub fn name(&self) -> &str {
        match self.loaded.get() {
            Some((faces, name)) if !faces.is_empty() => name,
            _ => "approximate",
        }
    }

    /// Measurement and glyphs for Broxee's interface (`font::FontKey::UI`) and for pages
    /// without `@font-face`.
    pub fn ui(&self) -> Fonts<'_> {
        Fonts::system(self.system())
    }
}

fn load_fonts() -> ProcessFonts {
    let mut faces = Vec::new();
    let mut name = String::new();
    for (regular, bold) in FONT_CANDIDATES {
        if let Ok(font) = Font::load(Path::new(regular)) {
            faces.push((SystemFace::Ready(Box::new(font)), Role::Page, 400));
            if let Ok(bold) = Font::load(Path::new(bold)) {
                faces.push((SystemFace::Ready(Box::new(bold)), Role::Page, 700));
            }
            name = Path::new(regular)
                .file_stem()
                .map_or(String::new(), |s| s.to_string_lossy().into_owned());
            break;
        }
    }
    if name.is_empty() {
        name = "Figtree".into();
    }
    // Decoded the first time something draws with them (see `SystemFace::Lazy`).
    for (data, weight) in [(font::FIGTREE_REGULAR, 400), (font::FIGTREE_SEMIBOLD, 600)] {
        let load = move || {
            Font::parse_any(data)
                .map_err(|e| eprintln!("font: bundled Figtree: {e}"))
                .ok()
        };
        faces.push((SystemFace::Lazy(Box::new(load)), Role::Ui, weight));
    }
    for path in SYMBOL_FONTS {
        if Path::new(path).exists() {
            let load = move || Font::load(Path::new(path)).ok();
            faces.push((SystemFace::Lazy(Box::new(load)), Role::Symbols, 400));
        }
    }
    (SystemFaces::new(faces), name)
}

/// Everything the engine produces for one page.
pub struct Loaded {
    pub url: net::Url,
    pub status: u16,
    pub bytes: usize,
    pub dom: Dom,
    pub css: LoadedCss,
    pub images: ImageStore,
    /// Form controls and what has been typed into them.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub forms: Forms,
    /// The page's scripts, when it has any.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub script: Option<Box<crate::script::PageScript>>,
    /// The HTML and stylesheets as they arrived, to rebuild the page without the network.
    pub sources: Sources,
    /// `@font-face` fonts the page's text uses.
    pub webfonts: WebFonts,
    pub styles: StyleMap,
    pub layout: Layout,
    /// What the pointer is over, what is pressed and what has focus, for `:hover` and the
    /// other state selectors.
    pub states: css::States,
    /// How many times the page was styled and laid out again after loading.
    #[cfg_attr(not(test), allow(dead_code))] // read by tests
    pub restyles: u32,
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
    /// External scripts by URL, in document order.
    pub scripts: Vec<(String, net::Wire)>,
    /// `@font-face` files the page used, by URL.
    pub fonts: Vec<(String, net::Wire)>,
}

/// A page put to sleep: its sources and compressed images, and nothing built from them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub struct Snapshot {
    pub url: net::Url,
    pub status: u16,
    pub sources: Sources,
    pub images: ImageStore,
}

/// Drop everything built from a page (DOM, styles, layout), keeping what rebuilds it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn snapshot(page: Loaded) -> Snapshot {
    Snapshot {
        url: page.url,
        status: page.status,
        sources: page.sources,
        images: page.images,
    }
}

/// Rebuild a sleeping page from its snapshot, without the network.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
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
        &mut |_| true,
    )
    .expect("a load without a stop request finishes");
    page.sources.html = html;
    Ok(page)
}

/// A profile's cookie jar, for a load. `same_site_initiator`: the person started this
/// navigation (typed, palette, back, reload) or a link on the same site did, so
/// `SameSite=Strict` cookies go with it.
#[derive(Clone, Copy)]
pub struct Jar<'a> {
    pub cookies: &'a net::Cookies,
    pub same_site_initiator: bool,
}

/// Where a page's subresources come from.
enum From<'a> {
    /// Fetched, with the profile's cookies for subresources of the page's own site.
    Network(Option<&'a net::Cookies>),
    /// A sleeping page waking: stylesheets from their stored bytes, images as they were.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Snapshot(Sources, ImageStore),
    /// Pages Broxee writes itself.
    Nowhere,
}

/// The error `load` returns when its `progress` callback asked it to stop.
pub const CANCELLED: &str = "cancelled";

/// Fetch a page and build it. `progress` receives a fraction after each stage, for a
/// loading indicator, and returns whether to go on: `false` stops the load at that point with
/// `CANCELLED`, freeing what was built. A stage already running (a slow fetch) finishes first.
pub fn load(
    url: &net::Url,
    settings: Settings,
    fonts: &FontStore,
    jar: Option<Jar>,
    progress: &mut dyn FnMut(f32) -> bool,
) -> Result<Loaded, String> {
    load_with(url, None, settings, fonts, jar, progress)
}

/// `load`, POSTing `body` when there is one (a submitted form).
pub fn load_with(
    url: &net::Url,
    body: Option<&net::Body>,
    settings: Settings,
    fonts: &FontStore,
    jar: Option<Jar>,
    progress: &mut dyn FnMut(f32) -> bool,
) -> Result<Loaded, String> {
    let started = Instant::now();
    let context = net::Context::top_level(jar.is_none_or(|j| j.same_site_initiator));
    let resp = net::send_in(url, body, jar.map(|j| (j.cookies, &context)))?;
    let fetch_ms = millis(started);
    if !progress(0.4) {
        return Err(CANCELLED.into());
    }

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
    if !progress(0.5) {
        return Err(CANCELLED.into());
    }

    let from = if settings.fetch_css {
        From::Network(jar.map(|j| j.cookies))
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
    )
    .ok_or(CANCELLED)?;
    page.sources.html = html;
    Ok(page)
}

/// Build a page from HTML Broxee generated itself (error and message pages). No network.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
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
    let mut page = finish(origin, dom, settings, fonts, From::Nowhere, &mut |_| true)
        .expect("a load without a stop request finishes");
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
    mut dom: Dom,
    settings: Settings,
    fonts: &FontStore,
    mut from: From,
    progress: &mut dyn FnMut(f32) -> bool,
) -> Option<Loaded> {
    let font_from = match &mut from {
        From::Network(cookies) => FontFrom::Network(*cookies),
        From::Snapshot(sources, _) => FontFrom::Stored(std::mem::take(&mut sources.fonts)),
        From::Nowhere => FontFrom::Nowhere,
    };
    // Controls get the nodes they draw with before anything reads the DOM.
    let mut forms = Forms::prepare(&mut dom);
    // Images are found among the parser's nodes only: nodes scripts add are not rebuilt the
    // same way when a sleeping tab wakes, and stored images refer to nodes by id.
    let parser_nodes = dom.len();
    let context = net::Context::subresource(&source.url);
    let script_from = match &from {
        From::Network(cookies) => {
            crate::script::ScriptFrom::Network(cookies.map(|c| (c, &context)))
        }
        From::Snapshot(sources, _) => crate::script::ScriptFrom::Stored(&sources.scripts),
        From::Nowhere => crate::script::ScriptFrom::Inline,
    };
    let (script, scripts) =
        crate::script::run_page_scripts(&mut dom, &mut forms, &source.url, script_from);
    let (css, images, stylesheets) =
        load_subresources(&dom, &source.url, from, script.is_some(), parser_nodes);
    if !progress(0.75) {
        return None;
    }
    // Join stage 1 before layout, the first stage that measures text.
    let fonts_wait_ms = fonts.ready();
    let states = css::States::default();
    let (styles, cascade_ms) = cascade(&mut dom, &css, &states, settings);
    // Fonts are found from the styled text, so they load between the cascade and layout.
    let (mut webfonts, font_files) =
        webfonts::load(&dom, &styles, &css.sheets, &source.url, font_from, fonts);
    if !progress(0.85) {
        return None;
    }
    webfonts.plan(&styles, fonts);
    let (layout, layout_ms) = lay_out(&dom, &styles, &images, &webfonts, settings, fonts);
    progress(0.95);
    Some(Loaded {
        url: source.url,
        status: source.status,
        bytes: source.bytes,
        dom,
        css,
        images,
        forms,
        script,
        sources: Sources {
            html: net::Wire::default(),
            stylesheets,
            scripts,
            fonts: font_files,
        },
        webfonts,
        styles,
        layout,
        states,
        restyles: 0,
        fetch_ms: source.fetch_ms,
        parse_ms: source.parse_ms,
        cascade_ms,
        fonts_wait_ms,
        layout_ms,
    })
}

/// Cascade and lay out again, for a new viewport size or color scheme. `@media` rules can
/// change, so the cascade reruns as well.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn relayout(page: &mut Loaded, settings: Settings, fonts: &FontStore) {
    let (styles, cascade_ms) = cascade(&mut page.dom, &page.css, &page.states, settings);
    page.webfonts.plan(&styles, fonts);
    let (layout, layout_ms) = lay_out(
        &page.dom,
        &styles,
        &page.images,
        &page.webfonts,
        settings,
        fonts,
    );
    page.restyles += 1;
    page.styles = styles;
    page.cascade_ms = cascade_ms;
    page.layout = layout;
    page.layout_ms = layout_ms;
}

fn cascade(
    dom: &mut Dom,
    css: &LoadedCss,
    states: &css::States,
    settings: Settings,
) -> (StyleMap, f64) {
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
    let mut styles = css::cascade_with(dom, &sheets, &media, states);
    // Boxes `::before` and `::after` generate become nodes outside the tree.
    styles.attach_generated(dom);
    (styles, millis(started))
}

fn lay_out(
    dom: &Dom,
    styles: &StyleMap,
    images: &ImageStore,
    webfonts: &WebFonts,
    settings: Settings,
    fonts: &FontStore,
) -> (Layout, f64) {
    let started = Instant::now();
    let media = settings.media();
    let natural = |node: NodeId| images.natural_size(node);
    let metrics = webfonts.metrics(fonts);
    let layout = layout::layout(dom, styles, &metrics, &natural, media.width, media.height);
    (layout, millis(started))
}

impl Loaded {
    /// How this page's text is measured and drawn: its `@font-face` faces, then the process's.
    pub fn metrics<'a>(&'a self, fonts: &'a FontStore) -> Fonts<'a> {
        self.webfonts.metrics(fonts)
    }
}

/// What paint needs to draw a page. `metrics` is `page.metrics(fonts)`, made by the caller so
/// it outlives the view.
pub fn page_view<'a>(page: &'a Loaded, metrics: &'a Fonts<'a>) -> paint::Page<'a> {
    paint::Page {
        dom: &page.dom,
        styles: &page.styles,
        layout: &page.layout,
        metrics,
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
    scripted: bool,
    parser_nodes: usize,
) -> (LoadedCss, ImageStore, Vec<(String, net::Wire)>) {
    let external = !matches!(from, From::Nowhere);
    let cookies = match from {
        From::Network(cookies) => cookies,
        _ => None,
    };
    // Subresources send and store cookies only for the page's own site.
    let context = net::Context::subresource(base);
    let jar = cookies.map(|c| (c, &context));
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
        images::discover(dom, base, parser_nodes)
    } else {
        Default::default()
    };
    jobs.extend(image_sources.into_iter().map(Job::Image));

    // Rules that need names this document lacks are dropped while parsing, unless scripts
    // can add those names later.
    let vocabulary = if scripted {
        css::Vocabulary::everything()
    } else {
        css::Vocabulary::of(dom)
    };
    let stored = stored_sheets.as_deref();
    let started = Instant::now();
    let results = pool::map_ordered(jobs, FETCH_WORKERS, |job| match job {
        Job::Css(source) => Done::Css(load_css(source, base, &vocabulary, stored, jar)),
        Job::Image(images::Source::Inline(bytes)) => Done::Image(Some(bytes)),
        Job::Image(images::Source::Url(url)) => Done::Image(
            net::fetch_in(&url, jar)
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
    page_url: &net::Url,
    vocabulary: &css::Vocabulary,
    stored: Option<&[(String, net::Wire)]>,
    jar: Option<(&net::Cookies, &net::Context)>,
) -> CssResult {
    let failed = CssResult {
        sheet: None,
        fetched: false,
        parse_ms: 0.0,
        wire: None,
    };
    let sheet_url = match &source {
        CssSource::Inline(_) => page_url.clone(),
        CssSource::External(url) => url.clone(),
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
                None => match net::fetch_in(&url, jar) {
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
    let mut sheet = css::parse_stylesheet_for(text, vocabulary);
    // `@font-face` URLs are relative to the sheet they are in.
    webfonts::resolve_sources(&mut sheet, &sheet_url);
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
    several_addresses(target)?;
    if target.contains("://") {
        return net::Url::parse(target);
    }
    let path = std::fs::canonicalize(target).map_err(|e| format!("{target}: {e}"))?;
    net::Url::parse(&format!("file://{}", path.display()))
}

/// Text with a space and a second `scheme://` is several addresses pasted together, not one
/// URL with a space in its path (which the URL standard would accept).
pub fn several_addresses(text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.contains(char::is_whitespace) && text.matches("://").count() > 1 {
        return Err(format!(
            "That is more than one address. Open them one at a time: {text}"
        ));
    }
    Ok(())
}

pub fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_files_with_spaces_in_their_names() {
        let dir = std::env::temp_dir().join(format!("broxee space {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("my page.html");
        std::fs::write(&file, "<title>Spaced</title>").unwrap();
        let url = to_url(&file.display().to_string()).unwrap();
        assert!(!url.to_string().contains(' '), "{url}");
        let settings = Settings {
            width: 400.0,
            height: 300.0,
            dark: false,
            fetch_css: false,
        };
        let page = load(&url, settings, &FontStore::default(), None, &mut |_| true).unwrap();
        assert_eq!(page.dom.title().as_deref(), Some("Spaced"));
        assert!(to_url("https://a.com https://b.com").is_err());
    }

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
    fn loads_only_the_font_faces_its_text_uses() {
        let dir = std::env::temp_dir().join(format!("broxee-fonts-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("f")).unwrap();
        let icons = include_bytes!("../../font/src/testdata/icons.woff2");
        std::fs::write(dir.join("f/icons.woff2"), icons).unwrap();
        // Two faces: the icon font (used through ::before) and one nothing uses.
        let html = r#"<!doctype html><style>
            @font-face { font-family: "Icons"; src: url(missing.woff2) format("woff2"),
                         url(f/icons.woff2) format("woff2"); unicode-range: U+E000-F8FF }
            @font-face { font-family: Unused; src: url(f/unused.woff2) }
            i::before { content: "\e001\e002"; font-family: Icons, sans-serif }
            em { font-family: Icons }
            </style><p><i></i></p><p><em>AB</em></p>"#;
        std::fs::write(dir.join("page.html"), html).unwrap();
        let url = to_url(&dir.join("page.html").display().to_string()).unwrap();
        let settings = Settings {
            width: 400.0,
            height: 300.0,
            dark: false,
            fetch_css: true,
        };
        let fonts = FontStore::default();
        let page = load(&url, settings, &fonts, None, &mut |_| true).unwrap();
        let web = &page.webfonts;
        assert_eq!((web.declared, web.faces.len(), web.failed), (2, 1, 0));
        assert_eq!(page.sources.fonts.len(), 1, "kept for a sleeping tab");
        assert!(page.sources.fonts[0].0.ends_with("/f/icons.woff2"));
        // Each icon is 1000 units of a 1000-unit em: 16 px at the default size, where the
        // approximate fallback would give 8.
        let width = |page: &Loaded, text: &str| {
            let t = page
                .layout
                .texts
                .iter()
                .find(|t| page.layout.text(t) == text)
                .expect("text run");
            t.width
        };
        assert_eq!(width(&page, "\u{e001}\u{e002}"), 32.0);
        assert_eq!(
            width(&page, "AB"),
            16.0,
            "letters fall back past the icon font"
        );

        // A sleeping tab wakes with its fonts, without the files.
        std::fs::remove_file(dir.join("f/icons.woff2")).unwrap();
        let woken = restore(snapshot(page), settings, &fonts).expect("restores");
        assert_eq!(woken.webfonts.faces.len(), 1);
        assert_eq!(width(&woken, "\u{e001}\u{e002}"), 32.0);
        let _ = std::fs::remove_dir_all(&dir);
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
