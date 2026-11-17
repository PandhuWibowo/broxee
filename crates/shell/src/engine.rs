//! The page pipeline (stages 1 to 9 of `docs/pipeline.md`), shared by the CLI and the window.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use css::{Origin, StyleMap};
use font::{Font, Fonts, Role, SystemFace, SystemFaces};
use html::{Dom, NodeId};
use layout::Layout;

use crate::backgrounds::{self, BackgroundFrom};
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
            pending: Mutex::new(Some(std::thread::spawn(|| load_fonts(false)))),
            loaded: OnceLock::new(),
        }
    }

    /// Only the bundled Figtree, for pages too: text renders the same on every machine,
    /// whatever fonts it has installed (`--replay`, the site regression suite).
    pub fn bundled() -> FontStore {
        FontStore {
            pending: Mutex::new(Some(std::thread::spawn(|| load_fonts(true)))),
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
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn ui(&self) -> Fonts<'_> {
        Fonts::system(self.system())
    }
}

fn load_fonts(bundled_only: bool) -> ProcessFonts {
    let mut faces = Vec::new();
    let mut name = String::new();
    let installed = if bundled_only {
        &[][..]
    } else {
        &FONT_CANDIDATES[..]
    };
    for &(regular, bold) in installed {
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
    let symbols = if bundled_only {
        &[][..]
    } else {
        &SYMBOL_FONTS[..]
    };
    for &path in symbols {
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
    /// What relative URLs resolve against: the first `<base href>`, or `url`
    /// (`document_base`).
    pub base: net::Url,
    /// `<img>` sources the page names that are not loaded yet (added or changed by scripts
    /// after loading): the window fetches them and lays the page out again.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub pending_images: Vec<String>,
    /// The response's `Refresh` header, when it had one (a woken tab has none).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub refresh_header: Option<String>,
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
    /// How many of those were laid out inside one box (`Layout::relayout`).
    #[cfg_attr(not(test), allow(dead_code))] // read by tests
    pub in_place: u32,
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
    /// Downloaded in the background and never built: the fonts and CSS background images,
    /// which only styling finds, are not here yet, so building it fetches them (`restore`).
    pub partial: bool,
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
/// Build a sleeping page again from its snapshot, without the network, except that a
/// partial snapshot (`Sources::partial`) fetches its fonts, CSS background images and any
/// stylesheet it lacks, with `cookies`.
pub fn restore(
    snapshot: Snapshot,
    settings: Settings,
    fonts: &FontStore,
    cookies: Option<&net::Cookies>,
) -> Result<Loaded, String> {
    restore_in_tab(snapshot, settings, fonts, cookies, None, &mut |_| true)
        .map(|o| o.page().expect("never sent to the background"))
}

/// `restore` on a tab's loader thread (a partial snapshot, which fetches): like
/// `load_in_tab`, it reports progress, stops when `progress` says so, and once `background`
/// is set it ends with the snapshot again instead of building the page.
pub fn restore_in_tab(
    snapshot: Snapshot,
    settings: Settings,
    fonts: &FontStore,
    cookies: Option<&net::Cookies>,
    background: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut dyn FnMut(f32) -> bool,
) -> Result<Outcome, String> {
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
    let mut outcome = finish(
        source,
        dom,
        settings,
        fonts,
        From::Snapshot(sources, images, cookies),
        progress,
        background,
    )
    .ok_or(CANCELLED)?;
    match &mut outcome {
        Outcome::Page(page) => page.sources.html = html,
        Outcome::Downloaded(snapshot, _) => {
            snapshot.sources.html = html;
            // Its fonts and background images were not fetched this time either.
            snapshot.sources.partial = true;
        }
    }
    Ok(outcome)
}

/// A profile's cookie jar, for a load. `same_site_initiator`: the person started this
/// navigation (typed, palette, back, reload) or a link on the same site did, so
/// `SameSite=Strict` cookies go with it.
#[derive(Clone, Copy)]
pub struct Jar<'a> {
    pub cookies: &'a net::Cookies,
    pub same_site_initiator: bool,
}

/// Where a page's subresources come from. Made once per load and moved into `finish`,
/// never kept, so the snapshot variant's size costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
enum From<'a> {
    /// Fetched, with the profile's cookies for subresources of the page's own site.
    Network(Option<&'a net::Cookies>),
    /// A sleeping page waking: stylesheets from their stored bytes, images as they were. A
    /// partial snapshot fetches what it lacks, with these cookies.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Snapshot(Sources, ImageStore, Option<&'a net::Cookies>),
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
    load_in_tab(url, body, settings, fonts, jar, None, progress)
        .map(|o| o.page().expect("never sent to the background"))
}

/// What a load gave: the built page, or, once it was sent to the background, what the page
/// is built from (a partial snapshot) and its title.
pub enum Outcome {
    /// Both boxed: the two are large, and different in size.
    Page(Box<Loaded>),
    Downloaded(Box<Snapshot>, Option<String>),
}

impl Outcome {
    fn page(self) -> Option<Loaded> {
        match self {
            Outcome::Page(page) => Some(*page),
            Outcome::Downloaded(..) => None,
        }
    }
}

/// `load_with` for a tab, which may go to the background while it loads: once `background`
/// is set, the page is not built. Its scripts, stylesheets and images are still fetched, and
/// the load ends with `Outcome::Downloaded` before styling (or with the page, when the flag
/// came after styling began). Memory stays at the bytes downloaded and the DOM.
pub fn load_in_tab(
    url: &net::Url,
    body: Option<&net::Body>,
    settings: Settings,
    fonts: &FontStore,
    jar: Option<Jar>,
    background: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut dyn FnMut(f32) -> bool,
) -> Result<Outcome, String> {
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
    let refresh_header = resp.header("refresh").map(str::to_string);
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
    let mut outcome = finish(
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
        background,
    )
    .ok_or(CANCELLED)?;
    match &mut outcome {
        Outcome::Page(page) => {
            page.sources.html = html;
            page.refresh_header = refresh_header;
        }
        Outcome::Downloaded(snapshot, _) => snapshot.sources.html = html,
    }
    Ok(outcome)
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
    let mut page = finish(
        origin,
        dom,
        settings,
        fonts,
        From::Nowhere,
        &mut |_| true,
        None,
    )
    .and_then(Outcome::page)
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
    background: Option<&std::sync::atomic::AtomicBool>,
) -> Option<Outcome> {
    let downloading = || background.is_some_and(|b| b.load(std::sync::atomic::Ordering::Relaxed));
    let font_from = match &mut from {
        From::Network(cookies) => FontFrom::Network(*cookies),
        From::Snapshot(sources, _, cookies) if sources.partial => FontFrom::Network(*cookies),
        From::Snapshot(sources, ..) => FontFrom::Stored(std::mem::take(&mut sources.fonts)),
        From::Nowhere => FontFrom::Nowhere,
    };
    let background_from = match &from {
        From::Network(cookies) => BackgroundFrom::Network(*cookies),
        From::Snapshot(sources, _, cookies) if sources.partial => BackgroundFrom::Network(*cookies),
        From::Snapshot(..) => BackgroundFrom::Stored,
        From::Nowhere => BackgroundFrom::Inline,
    };
    // Controls get the nodes they draw with before anything reads the DOM.
    let mut forms = Forms::prepare(&mut dom);
    // Images are found among the parser's nodes only: nodes scripts add are not rebuilt the
    // same way when a sleeping tab wakes, and stored images refer to nodes by id.
    let parser_nodes = dom.len();
    let base = document_base(&dom, &source.url);
    let context = net::Context::subresource(&source.url);
    let script_from = match &from {
        From::Network(cookies) => {
            crate::script::ScriptFrom::Network(cookies.map(|c| (c, &context)))
        }
        From::Snapshot(sources, ..) => crate::script::ScriptFrom::Stored(&sources.scripts),
        From::Nowhere => crate::script::ScriptFrom::Inline,
    };
    let (script, scripts) =
        // A page downloading in the background only fetches its scripts.
        crate::script::run_page_scripts(
            &mut dom,
            &mut forms,
            (&source.url, &base),
            script_from,
            !downloading(),
        );
    let (css, mut images, stylesheets) = load_subresources(
        &dom,
        (&source.url, &base),
        from,
        script.is_some(),
        parser_nodes,
    );
    if !progress(0.75) {
        return None;
    }
    if downloading() {
        let title = dom.title();
        return Some(Outcome::Downloaded(
            Box::new(Snapshot {
                url: source.url,
                status: source.status,
                sources: Sources {
                    html: net::Wire::default(),
                    stylesheets,
                    scripts,
                    fonts: Vec::new(),
                    partial: true,
                },
                images,
            }),
            title,
        ));
    }
    // Join stage 1 before layout, the first stage that measures text.
    let fonts_wait_ms = fonts.ready();
    let states = css::States::default();
    // The first cascade sees everything controls and scripts did so far.
    dom.take_changes();
    let (styles, cascade_ms) = cascade(&mut dom, &css, &states, settings);
    // Fonts and background images are found from the styles, so they load between the
    // cascade and layout, at the same time.
    // `<img>` elements scripts added, or whose source they set: found by source, and
    // fetched with the backgrounds.
    let added = images.bind(&dom, &base, images::img_nodes(&dom, Dom::ROOT));
    let ((mut webfonts, font_files), backgrounds) = std::thread::scope(|scope| {
        let backgrounds = scope.spawn(|| {
            backgrounds::load(
                &dom,
                &styles,
                (&source.url, &base),
                background_from,
                &images,
                added,
            )
        });
        let fonts = webfonts::load(&dom, &styles, &css.sheets, &source.url, font_from, fonts);
        (fonts, backgrounds.join().unwrap_or_default())
    });
    images.add_backgrounds(backgrounds);
    // What is still missing (a woken page whose scripts named other files) loads later.
    let pending_images = images.bind(&dom, &base, images::img_nodes(&dom, Dom::ROOT));
    if !progress(0.85) {
        return None;
    }
    webfonts.plan(&styles, fonts);
    let (layout, layout_ms) = lay_out(&dom, &styles, &images, &webfonts, settings, fonts);
    progress(0.95);
    Some(Outcome::Page(Box::new(Loaded {
        url: source.url,
        base,
        pending_images,
        refresh_header: None,
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
            partial: false,
        },
        webfonts,
        styles,
        layout,
        states,
        restyles: 0,
        in_place: 0,
        fetch_ms: source.fetch_ms,
        parse_ms: source.parse_ms,
        cascade_ms,
        fonts_wait_ms,
        layout_ms,
    })))
}

/// Cascade and lay out again, for a new viewport size or color scheme. `@media` rules can
/// change, so the cascade reruns as well.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn relayout(page: &mut Loaded, settings: Settings, fonts: &FontStore) {
    page.dom.take_changes();
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

/// Bring the page up to date after controls or scripts changed its DOM (`Dom::take_changes`),
/// or after `:hover`/`:focus` changed when `states_changed`. Everything changed since the last
/// update is handled at once. The cascade is skipped when only text changed and no rule reads
/// text; layout is redone only inside the smallest box around the changed nodes that can be
/// laid out alone (`Layout::relayout`), and in full when there is none.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn update(page: &mut Loaded, settings: Settings, fonts: &FontStore, states_changed: bool) {
    let Some(changes) = page.dom.take_changes() else {
        return relayout(page, settings, fonts);
    };
    if changes.is_empty() && !states_changed {
        return;
    }
    let dom = &page.dom;
    // Where content changed: the parent of a changed text node or element, or the element
    // whose children changed. Detached nodes are not drawn.
    let mut changed: Vec<NodeId> = changes
        .iter()
        .filter_map(|c| match *c {
            html::Change::Text(n) | html::Change::Element(n) => dom.parent(n),
            html::Change::Children(n) => Some(n),
        })
        .filter(|&n| in_document(dom, n))
        .collect();
    // `<img>` elements added, or whose attributes changed: shown with the file of their
    // current source, or asked for.
    let mut imgs: Vec<NodeId> = Vec::new();
    for change in &changes {
        match *change {
            html::Change::Element(n) if dom.tag_name(n) == Some("img") => imgs.push(n),
            html::Change::Children(n) => imgs.extend(images::img_nodes(dom, n)),
            _ => {}
        }
    }
    if !imgs.is_empty() {
        for url in page.images.bind(&page.dom, &page.base, imgs) {
            if !page.pending_images.contains(&url) {
                page.pending_images.push(url);
            }
        }
    }
    let text_only = changes.iter().all(|c| matches!(c, html::Change::Text(_)));
    if states_changed || !text_only || page.styles.reads_text() {
        let (styles, cascade_ms) = cascade(&mut page.dom, &page.css, &page.states, settings);
        let dom = &page.dom;
        changed.extend(
            styles
                .changed_since(&page.styles)
                .into_iter()
                .filter_map(|n| dom.parent(n))
                .filter(|&n| in_document(dom, n)),
        );
        page.webfonts.plan(&styles, fonts);
        page.styles = styles;
        page.cascade_ms = cascade_ms;
    } else {
        page.cascade_ms = 0.0;
    }
    changed.sort_unstable();
    changed.dedup();
    page.restyles += 1;
    if changed.is_empty() {
        page.layout_ms = 0.0;
        return;
    }
    let started = Instant::now();
    let metrics = page.webfonts.metrics(fonts);
    let images = &page.images;
    let natural = |node: NodeId| images.natural_size(node);
    let done = page
        .layout
        .relayout(&page.dom, &page.styles, &metrics, &natural, &changed);
    if done {
        page.layout_ms = millis(started);
        page.in_place += 1;
    } else {
        let (layout, layout_ms) = lay_out(
            &page.dom,
            &page.styles,
            &page.images,
            &page.webfonts,
            settings,
            fonts,
        );
        page.layout = layout;
        page.layout_ms = layout_ms;
    }
}

/// Whether `node` is in the document (not detached by a script).
fn in_document(dom: &Dom, node: NodeId) -> bool {
    std::iter::successors(Some(node), |&n| dom.parent(n)).last() == Some(Dom::ROOT)
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

/// What relative URLs in the document resolve against: the first `<base href>` in tree order,
/// resolved against the document's URL, or that URL. Only http, https and file bases count
/// (no `data:` or `javascript:`).
pub fn document_base(dom: &Dom, url: &net::Url) -> net::Url {
    dom.descendants(Dom::ROOT)
        .find(|&id| dom.tag_name(id) == Some("base") && dom.attr(id, "href").is_some())
        .and_then(|id| web_url(url, dom.attr(id, "href")?))
        .unwrap_or_else(|| url.clone())
}

/// `href` resolved against `base` when it names an http, https or file address. `Url::join`
/// reads an unknown scheme (`javascript:`, `data:`) as a path, so those are refused first.
fn web_url(base: &net::Url, href: &str) -> Option<net::Url> {
    let href = href.trim();
    let scheme = href.split_once(':').map(|(s, _)| s).filter(|s| {
        s.starts_with(|c: char| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let web = |s: &str| {
        ["http", "https", "file"]
            .iter()
            .any(|k| s.eq_ignore_ascii_case(k))
    };
    if scheme.is_some_and(|s| !web(s)) {
        return None;
    }
    let url = base.join(href).ok()?;
    web(&url.scheme).then_some(url)
}

/// `target` of the first `<base>` that has one: where links without their own target open.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn base_target(dom: &Dom) -> Option<&str> {
    dom.descendants(Dom::ROOT)
        .find(|&id| dom.tag_name(id) == Some("base") && dom.attr(id, "target").is_some())
        .and_then(|id| dom.attr(id, "target"))
}

/// When and where the page asks to go by itself: its `Refresh` header, or else the first
/// `<meta http-equiv=refresh>`. The URL is resolved against the base; `None` reloads the page.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn refresh(page: &Loaded) -> Option<(f32, Option<net::Url>)> {
    let content = match &page.refresh_header {
        Some(h) => h.as_str(),
        None => {
            let dom = &page.dom;
            let meta = dom.descendants(Dom::ROOT).find(|&id| {
                dom.tag_name(id) == Some("meta")
                    && !in_noscript(dom, id)
                    && dom
                        .attr(id, "http-equiv")
                        .is_some_and(|e| e.trim().eq_ignore_ascii_case("refresh"))
            })?;
            dom.attr(meta, "content")?
        }
    };
    let (delay, url) = parse_refresh(content)?;
    match url {
        None => Some((delay, None)),
        Some(u) => Some((delay, Some(web_url(&page.base, u)?))),
    }
}

/// A refresh value, `5; url=https://example.com/`: seconds, then an optional URL after `;` or
/// `,`, with or without `url=` and quotes (the HTML "shared declarative refresh steps").
pub fn parse_refresh(content: &str) -> Option<(f32, Option<&str>)> {
    let s = content.trim_start();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits == 0 {
        return None;
    }
    let seconds: f32 = s[..digits].parse().ok()?;
    // A fraction is allowed and ignored.
    let rest = s[digits..].trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    let rest = rest.trim_start();
    let rest = match rest.chars().next() {
        None => return Some((seconds, None)),
        Some(';' | ',') => rest[1..].trim_start(),
        Some(_) if rest.len() < s[digits..].len() => rest,
        Some(_) => return None,
    };
    let rest = match rest.get(..3) {
        Some(u) if u.eq_ignore_ascii_case("url") => {
            let after = rest[3..].trim_start();
            match after.strip_prefix('=') {
                Some(v) => v.trim_start(),
                None => rest,
            }
        }
        _ => rest,
    };
    let url = match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &rest[1..];
            &inner[..inner.find(q).unwrap_or(inner.len())]
        }
        _ => rest.trim_end(),
    };
    Some((seconds, (!url.is_empty()).then_some(url)))
}

/// Whether `id` is inside a `<noscript>`. Broxee runs scripts, so that content is not shown
/// (ua.css) and nothing in it is fetched or run.
pub fn in_noscript(dom: &Dom, id: NodeId) -> bool {
    let mut current = dom.parent(id);
    while let Some(p) = current {
        if dom.tag_name(p) == Some("noscript") {
            return true;
        }
        current = dom.parent(p);
    }
    false
}

/// Stylesheets and images (stages 5 to 7 in `docs/pipeline.md`). Both go through one bounded
/// pool; stylesheets are queued first because rendering waits for them. Stylesheets come back
/// in document order: `<style>` contents and `<link rel=stylesheet>` targets.
/// `page` is the document's URL (whose site subresources belong to), `base` what relative
/// URLs resolve against.
fn load_subresources(
    dom: &Dom,
    (page, base): (&net::Url, &net::Url),
    from: From,
    scripted: bool,
    parser_nodes: usize,
) -> (LoadedCss, ImageStore, Vec<(String, net::Wire)>) {
    let external = !matches!(from, From::Nowhere);
    // A partial snapshot fetches the stylesheets it lacks (added by scripts it never ran).
    let fallback = matches!(&from, From::Snapshot(s, ..) if s.partial);
    let cookies = match &from {
        From::Network(cookies) => *cookies,
        From::Snapshot(s, _, cookies) if s.partial => *cookies,
        _ => None,
    };
    // Subresources send and store cookies only for the page's own site.
    let context = net::Context::subresource(page);
    let jar = cookies.map(|c| (c, &context));
    let mut jobs = Vec::new();
    for id in dom.descendants(Dom::ROOT) {
        if jobs.len() >= MAX_STYLESHEETS {
            break;
        }
        match dom.tag_name(id) {
            // With scripts on, `<noscript>` is inert: its stylesheets are not for us.
            Some("style" | "link") if in_noscript(dom, id) => {}
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
        From::Snapshot(sources, images, _) => (Some(sources.stylesheets), Some(images)),
        _ => (None, None),
    };
    let (image_sources, image_uses) = if external && stored_images.is_none() {
        images::discover(dom, base, parser_nodes)
    } else {
        Default::default()
    };
    let image_keys: Vec<String> = image_sources.iter().map(|(k, _)| k.clone()).collect();
    jobs.extend(image_sources.into_iter().map(|(_, s)| Job::Image(s)));

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
        Job::Css(source) => Done::Css(load_css(source, base, &vocabulary, (stored, fallback), jar)),
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
        None => ImageStore::new(image_keys, fetched_images, image_uses),
    };
    (loaded, images, wires)
}

/// One stylesheet: inline text, or fetched (or, for a waking page, taken from its stored bytes).
fn load_css(
    source: CssSource,
    page_url: &net::Url,
    vocabulary: &css::Vocabulary,
    (stored, fallback): (Option<&[(String, net::Wire)]>, bool),
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
            let kept = stored.map(|sheets| sheets.iter().find(|(u, _)| *u == key));
            match kept {
                Some(Some((_, wire))) => match wire.decode(net::MAX_BODY) {
                    Ok(bytes) => (bytes, Some((key, wire.clone())), false),
                    Err(_) => return failed,
                },
                // A snapshot without it: only a partial one may fetch it.
                Some(None) if !fallback => return failed,
                _ => match net::fetch_in(&url, jar) {
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
    // `@font-face` and background URLs are relative to the sheet they are in.
    webfonts::resolve_sources(&mut sheet, &sheet_url);
    sheet.resolve_urls(|url| {
        (!url.starts_with("data:"))
            .then(|| sheet_url.join(url).ok().map(|u| u.to_string()))
            .flatten()
    });
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
        let after = restore(snapshot, settings, &FontStore::default(), None).expect("restores");
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
        let woken = restore(snapshot(page), settings, &fonts, None).expect("restores");
        assert_eq!(woken.webfonts.faces.len(), 1);
        assert_eq!(width(&woken, "\u{e001}\u{e002}"), 32.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_background_images_relative_to_their_sheet() {
        let dir = std::env::temp_dir().join(format!("broxee-backgrounds-{}", std::process::id()));
        for sub in ["css", "img"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let png = include_bytes!("../../image/src/testdata/rgba8.png");
        std::fs::write(dir.join("img/a.png"), png).unwrap();
        std::fs::write(dir.join("img/b.png"), png).unwrap();
        // The sheet's URL is relative to the sheet; the attribute's to the page; the hidden
        // element's file is never asked for (it does not exist).
        std::fs::write(
            dir.join("css/site.css"),
            ".a { background: url(../img/a.png) } .gone { background: url(missing.png) }",
        )
        .unwrap();
        std::fs::write(
            dir.join("page.html"),
            r#"<!doctype html><link rel=stylesheet href="css/site.css">
               <div class=a>a</div><div style="background-image: url(img/b.png)">b</div>
               <div style="display: none"><p class=gone>x</p></div>"#,
        )
        .unwrap();
        let url = to_url(&dir.join("page.html").display().to_string()).unwrap();
        let settings = Settings {
            width: 400.0,
            height: 300.0,
            dark: false,
            fetch_css: true,
        };
        let fonts = FontStore::default();
        let page = load(&url, settings, &fonts, None, &mut |_| true).unwrap();
        let sheet_url = net::Url::parse(&format!("{}", url))
            .unwrap()
            .join("img/a.png")
            .unwrap()
            .to_string();
        use paint::ImageSource;
        assert!(page.images.background(&sheet_url).is_some(), "{sheet_url}");
        assert!(
            page.images.background("img/b.png").is_some(),
            "kept by the URL as written"
        );
        assert_eq!(
            page.images.len(),
            2,
            "the hidden element's file was not fetched"
        );
        // A sleeping tab wakes with its backgrounds, without the files.
        std::fs::remove_dir_all(dir.join("img")).unwrap();
        let woken = restore(snapshot(page), settings, &fonts, None).expect("restores");
        assert!(woken.images.background(&sheet_url).is_some());
        assert!(woken.images.background("img/b.png").is_some());
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

    #[test]
    fn parses_refresh_values() {
        assert_eq!(parse_refresh("5"), Some((5.0, None)));
        assert_eq!(parse_refresh(" 3.5 "), Some((3.0, None)));
        assert_eq!(parse_refresh("0; url=/next"), Some((0.0, Some("/next"))));
        assert_eq!(
            parse_refresh("0;URL = 'a b.html' x"),
            Some((0.0, Some("a b.html")))
        );
        assert_eq!(parse_refresh("2, \"x.html\""), Some((2.0, Some("x.html"))));
        assert_eq!(parse_refresh("1 next.html"), Some((1.0, Some("next.html"))));
        assert_eq!(parse_refresh("1;"), Some((1.0, None)));
        assert_eq!(parse_refresh("url=x"), None);
        assert_eq!(parse_refresh("-1; url=x"), None);
        assert_eq!(parse_refresh("5x"), None);
        assert_eq!(parse_refresh(""), None);
    }

    #[test]
    fn the_first_base_with_an_href_counts() {
        let url = net::Url::parse("https://example.com/a/page.html").unwrap();
        let base = |html: &str| document_base(&html::parse(html), &url).to_string();
        assert_eq!(base("<p>none"), "https://example.com/a/page.html");
        assert_eq!(
            base("<base target=_top><base href=\"../b/\"><base href=\"/c/\">"),
            "https://example.com/b/"
        );
        assert_eq!(
            base("<base href=\"javascript:x\">"),
            "https://example.com/a/page.html"
        );
        let dom = html::parse("<base href=x><base target=\"_blank\">");
        assert_eq!(base_target(&dom), Some("_blank"));
    }
}
