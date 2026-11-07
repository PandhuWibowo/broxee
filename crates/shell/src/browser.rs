//! The window: navigation, history, scrolling, links and the address field.
//!
//! Runs on the engine thread. Input arrives from the main thread over a channel; every event
//! queued while the engine was busy is handled before the next frame, so a burst of scroll
//! events costs one render.
//!
//! The browser draws through the `Screen` trait: the real window on macOS, an in-memory screen
//! in tests. Tests drive it with synthetic events and local files, never the real desktop.

use std::sync::mpsc::Receiver;

use html::NodeId;
use paint::{Painter, Target, Viewport};
use platform::{Event, Key, KeyEvent};

use crate::chrome::{self, Button, Chrome, FieldView, Frame, HeaderView, HEADER_HEIGHT};
use crate::engine::{self, FontStore, Loaded, Settings};
use crate::Options;

/// Arrow keys scroll by this many points.
const LINE_STEP: f32 = 40.0;
/// Page Up/Down keep this much of the previous view visible.
const PAGE_OVERLAP: f32 = 40.0;
/// The glyph cache is dropped past this size, so long sessions cannot grow it without bound.
const GLYPH_CACHE_LIMIT: usize = 8 << 20;

/// Where the browser draws.
pub trait Screen {
    /// Device pixels per point.
    fn scale(&self) -> f64;
    /// Size in points.
    fn size(&self) -> (f64, f64);
    fn dark(&self) -> bool;
    fn set_title(&self, title: &str);
    /// Paint a `width` x `height` device-pixel frame (BGRA rows of `stride` bytes) and show it.
    fn render(
        &self,
        width: usize,
        height: usize,
        draw: &mut dyn FnMut(&mut [u8], usize),
    ) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
impl Screen for platform::Window {
    fn scale(&self) -> f64 {
        platform::Window::scale(self)
    }

    fn size(&self) -> (f64, f64) {
        platform::Window::size(self)
    }

    fn dark(&self) -> bool {
        platform::Window::dark(self)
    }

    fn set_title(&self, title: &str) {
        platform::Window::set_title(self, title);
    }

    fn render(
        &self,
        width: usize,
        height: usize,
        draw: &mut dyn FnMut(&mut [u8], usize),
    ) -> Result<(), String> {
        platform::Window::render(self, width, height, |pixels, stride| draw(pixels, stride))
    }
}

#[cfg(target_os = "macos")]
pub fn run(opts: Options) -> std::process::ExitCode {
    let (width, height) = (f64::from(opts.width), f64::from(opts.height));
    platform::run_app("Broxee", width, height, move |window, events| {
        Browser::new(opts, window).run(&events);
    })
}

struct Entry {
    url: net::Url,
    scroll: f32,
}

struct Field {
    text: String,
    /// Byte offset of the caret, always on a char boundary.
    cursor: usize,
    /// The whole text is selected: typing replaces it (as when the field opens).
    all_selected: bool,
}

impl Field {
    fn insert(&mut self, s: &str) {
        if self.all_selected {
            self.text.clear();
            self.cursor = 0;
            self.all_selected = false;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.all_selected {
            self.text.clear();
            self.cursor = 0;
            self.all_selected = false;
            return;
        }
        if let Some((prev, _)) = self.text[..self.cursor].char_indices().last() {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    fn left(&mut self) {
        if std::mem::take(&mut self.all_selected) {
            self.cursor = 0;
        } else if let Some((prev, _)) = self.text[..self.cursor].char_indices().last() {
            self.cursor = prev;
        }
    }

    fn right(&mut self) {
        if std::mem::take(&mut self.all_selected) {
            self.cursor = self.text.len();
        } else if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
}

struct Browser<S: Screen> {
    opts: Options,
    window: S,
    painter: Painter,
    chrome: Chrome,
    fonts: FontStore,
    width: f32,
    height: f32,
    scale: f32,
    dark: bool,
    page: Option<Loaded>,
    scroll: f32,
    history: Vec<Entry>,
    index: usize,
    field: Option<Field>,
    /// Header button with keyboard focus, reached with Tab.
    focus: Option<Button>,
    loading: Option<f32>,
}

impl<S: Screen> Browser<S> {
    fn new(opts: Options, window: S) -> Browser<S> {
        let (width, height) = window.size();
        let (scale, dark) = (window.scale() as f32, window.dark());
        Browser {
            opts,
            window,
            painter: Painter::new(),
            chrome: Chrome::new(dark),
            fonts: FontStore::start(),
            width: width as f32,
            height: height as f32,
            scale,
            dark,
            page: None,
            scroll: 0.0,
            history: Vec::new(),
            index: 0,
            field: None,
            focus: None,
            loading: None,
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            width: self.width,
            height: self.page_height(),
            dark: self.dark,
            fetch_css: self.opts.fetch_css,
        }
    }

    fn page_height(&self) -> f32 {
        (self.height - HEADER_HEIGHT).max(1.0)
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn run(&mut self, events: &Receiver<Event>) {
        self.start();
        while let Ok(event) = events.recv() {
            let mut dirty = self.handle(event);
            for event in events.try_iter() {
                dirty |= self.handle(event);
            }
            if dirty {
                self.render();
            }
        }
    }

    /// Show the first frame and open the page from the command line.
    fn start(&mut self) {
        // The header needs fonts; they load in a few milliseconds, before the first frame.
        self.fonts.ready();
        self.render();
        let target = self.opts.target.clone();
        match engine::to_url(&target) {
            Ok(url) => self.navigate(url, true),
            Err(e) => self.show_message("Broxee couldn't open that", &e),
        }
    }

    /// Apply one event; returns whether the frame needs repainting.
    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Scroll { dy } => self.scroll_by(-(dy as f32)),
            Event::Resize {
                width,
                height,
                scale,
            } => {
                self.width = width as f32;
                self.height = height as f32;
                self.scale = scale as f32;
                let settings = self.settings();
                if let Some(page) = self.page.as_mut() {
                    engine::relayout(page, settings, &self.fonts);
                }
                self.clamp_scroll();
                true
            }
            Event::Click { x, y } => self.click(x as f32, y as f32),
            Event::Key(key) => self.key(key),
            Event::Paste(text) => match self.field.as_mut() {
                Some(field) => {
                    field.insert(text.trim().lines().next().unwrap_or_default());
                    true
                }
                None => false,
            },
        }
    }

    fn max_scroll(&self) -> f32 {
        self.page
            .as_ref()
            .map_or(0.0, |p| (p.layout.height - self.page_height()).max(0.0))
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    fn scroll_by(&mut self, delta: f32) -> bool {
        self.scroll_to(self.scroll + delta)
    }

    fn scroll_to(&mut self, y: f32) -> bool {
        let before = self.scroll;
        self.scroll = y.clamp(0.0, self.max_scroll());
        self.scroll != before
    }

    fn click(&mut self, x: f32, y: f32) -> bool {
        if y < HEADER_HEIGHT {
            self.focus = None;
            return match chrome::hit(x, self.width) {
                Some(Button::Back) => self.back(),
                Some(Button::Forward) => self.forward(),
                Some(Button::Reload) => self.reload(),
                Some(Button::Address) => self.focus_field(),
                None => false,
            };
        }
        let was_editing = self.field.take().is_some() | self.focus.take().is_some();
        if let Some(url) = self.link_at(x, y - HEADER_HEIGHT) {
            self.save_scroll();
            self.navigate(url, true);
            return true;
        }
        was_editing
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if k.command {
            return match k.key {
                Key::Char('l') => self.focus_field(),
                Key::Char('[') => self.back(),
                Key::Char(']') => self.forward(),
                Key::Char('r') => self.reload(),
                Key::Char('a') => match self.field.as_mut() {
                    Some(field) => {
                        field.all_selected = true;
                        true
                    }
                    None => false,
                },
                _ => false,
            };
        }
        if let Some(field) = self.field.as_mut() {
            match k.key {
                Key::Char(c) => field.insert(c.encode_utf8(&mut [0; 4])),
                Key::Backspace => field.backspace(),
                Key::Left => field.left(),
                Key::Right => field.right(),
                Key::Home | Key::Up => {
                    field.all_selected = false;
                    field.cursor = 0;
                }
                Key::End | Key::Down => {
                    field.all_selected = false;
                    field.cursor = field.text.len();
                }
                Key::Enter => {
                    let input = std::mem::take(&mut field.text);
                    self.field = None;
                    self.open_input(&input);
                }
                Key::Escape => self.field = None,
                Key::Tab => {
                    self.field = None;
                    return self.move_focus(k.shift);
                }
                _ => return false,
            }
            return true;
        }
        if let Some(button) = self.focus {
            match k.key {
                Key::Tab => return self.move_focus(k.shift),
                Key::Enter | Key::Char(' ') => {
                    return match button {
                        Button::Back => self.back(),
                        Button::Forward => self.forward(),
                        Button::Reload => self.reload(),
                        Button::Address => self.focus_field(),
                    };
                }
                Key::Escape => {
                    self.focus = None;
                    return true;
                }
                _ => {}
            }
        }
        if k.key == Key::Tab {
            return self.move_focus(k.shift);
        }
        let page = self.page_height() - PAGE_OVERLAP;
        match k.key {
            Key::Down => self.scroll_by(LINE_STEP),
            Key::Up => self.scroll_by(-LINE_STEP),
            Key::PageDown => self.scroll_by(page),
            Key::PageUp => self.scroll_by(-page),
            Key::Char(' ') => self.scroll_by(if k.shift { -page } else { page }),
            Key::Home => self.scroll_to(0.0),
            Key::End => self.scroll_to(f32::MAX),
            _ => false,
        }
    }

    /// Tab order through the header: back, forward, reload, address field. Disabled buttons
    /// are skipped; the order wraps around.
    fn move_focus(&mut self, backwards: bool) -> bool {
        let order = [
            Button::Back,
            Button::Forward,
            Button::Reload,
            Button::Address,
        ];
        let current = if self.field.is_some() {
            Some(Button::Address)
        } else {
            self.focus
        };
        let start = current.and_then(|b| order.iter().position(|&o| o == b));
        let enabled = |b: Button| match b {
            Button::Back => self.index > 0,
            Button::Forward => self.index + 1 < self.history.len(),
            Button::Reload => !self.history.is_empty(),
            Button::Address => true,
        };
        let n = order.len();
        for step in 1..=n {
            let i = match (start, backwards) {
                (Some(s), false) => (s + step) % n,
                (Some(s), true) => (s + n - step) % n,
                (None, false) => step - 1,
                (None, true) => n - step,
            };
            if enabled(order[i]) {
                self.field = None;
                self.focus = None;
                if order[i] == Button::Address {
                    self.focus_field();
                } else {
                    self.focus = Some(order[i]);
                }
                return true;
            }
        }
        false
    }

    fn focus_field(&mut self) -> bool {
        self.focus = None;
        let text = self
            .history
            .get(self.index)
            .map(|e| e.url.to_string())
            .unwrap_or_default();
        self.field = Some(Field {
            cursor: text.len(),
            text,
            all_selected: true,
        });
        true
    }

    fn save_scroll(&mut self) {
        let scroll = self.scroll;
        if let Some(entry) = self.history.get_mut(self.index) {
            entry.scroll = scroll;
        }
    }

    fn back(&mut self) -> bool {
        if self.index == 0 || self.history.is_empty() || self.loading.is_some() {
            return false;
        }
        self.save_scroll();
        self.index -= 1;
        self.navigate(self.history[self.index].url.clone(), false);
        true
    }

    fn forward(&mut self) -> bool {
        if self.index + 1 >= self.history.len() || self.loading.is_some() {
            return false;
        }
        self.save_scroll();
        self.index += 1;
        self.navigate(self.history[self.index].url.clone(), false);
        true
    }

    fn reload(&mut self) -> bool {
        let Some(entry) = self.history.get(self.index) else {
            return false;
        };
        let url = entry.url.clone();
        self.save_scroll();
        self.navigate(url, false);
        true
    }

    /// What the user typed in the address field.
    fn open_input(&mut self, input: &str) {
        match parse_input(input) {
            Ok(url) => {
                self.save_scroll();
                self.navigate(url, true);
            }
            Err(message) => self.show_message("Broxee can't search yet", &message),
        }
    }

    /// Load `url`. With `push`, it becomes a new history entry (dropping any forward entries);
    /// without, it replaces the current one and its saved scroll position is restored.
    fn navigate(&mut self, url: net::Url, push: bool) {
        self.field = None;
        self.focus = None;
        self.loading = Some(0.1);
        self.render();

        let settings = self.settings();
        // The fonts move out for the load so the progress callback can render with them too.
        let fonts = std::mem::take(&mut self.fonts);
        let result = engine::load(&url, settings, &fonts, &mut |progress| {
            self.loading = Some(progress);
            self.render_with(&fonts);
        });
        self.fonts = fonts;
        let page = result.unwrap_or_else(|e| {
            engine::page_from_html(
                url.clone(),
                &error_page(&url, &e, &self.chrome),
                settings,
                &self.fonts,
            )
        });

        let scroll = if push {
            if !self.history.is_empty() {
                self.history.truncate(self.index + 1);
            }
            self.history.push(Entry {
                url: page.url.clone(),
                scroll: 0.0,
            });
            self.index = self.history.len() - 1;
            0.0
        } else {
            self.history[self.index].scroll
        };
        self.show(page, scroll);
    }

    /// A page Broxee writes itself, without a history entry.
    fn show_message(&mut self, title: &str, detail: &str) {
        let url = net::Url::parse("file:///").expect("a file URL always parses");
        let html = message_page(title, detail, None, &self.chrome);
        let page = engine::page_from_html(url, &html, self.settings(), &self.fonts);
        self.show(page, 0.0);
    }

    fn show(&mut self, page: Loaded, scroll: f32) {
        let title = page.dom.title().unwrap_or_else(|| host_of(&page.url));
        self.window.set_title(&title);
        self.page = Some(page);
        self.loading = None;
        self.scroll = scroll;
        self.clamp_scroll();
        self.painter.trim(GLYPH_CACHE_LIMIT);
        self.render();
    }

    /// The link under a point in the page area (points, top of the page area at 0). The
    /// topmost fragment wins: later layers paint over earlier ones, and fixed layers do not
    /// scroll. Hidden fragments cannot be clicked.
    fn link_at(&self, x: f32, y_view: f32) -> Option<net::Url> {
        let page = self.page.as_ref()?;
        let layout = &page.layout;
        let metrics = self.fonts.metrics();
        let doc_y = |fixed: bool| if fixed { y_view } else { y_view + self.scroll };
        for i in (0..layout.texts.len()).rev() {
            let t = &layout.texts[i];
            let style = page.styles.style(t.node);
            let size = style.font_size.to_f32();
            let y = doc_y(layout.text_is_fixed(i));
            let top = t.baseline - metrics.ascent_em() * size;
            let bottom = t.baseline + metrics.descent_em() * size;
            if style.visible
                && !style.transparent
                && (t.x..=t.x + t.width).contains(&x)
                && (top..=bottom).contains(&y)
            {
                return link_for(page, t.node);
            }
        }
        // Innermost boxes come last in document order; an outer <a> can still wrap them.
        (0..layout.boxes.len()).rev().find_map(|i| {
            let r = layout.boxes[i].rect;
            let y = doc_y(layout.box_is_fixed(i));
            let inside = (r.x..=r.x + r.width).contains(&x) && (r.y..=r.y + r.height).contains(&y);
            (inside && {
                let st = page.styles.style(layout.boxes[i].node);
                st.visible && !st.transparent
            })
            .then(|| link_for(page, layout.boxes[i].node))
            .flatten()
        })
    }

    fn render(&mut self) {
        let fonts = std::mem::take(&mut self.fonts);
        self.render_with(&fonts);
        self.fonts = fonts;
    }

    fn render_with(&mut self, fonts: &FontStore) {
        let scale = self.scale;
        let width_px = (self.width * scale).round().max(1.0) as usize;
        let height_px = (self.height * scale).round().max(1.0) as usize;
        let header_rows = ((HEADER_HEIGHT * scale).round() as usize).min(height_px);
        let page_rows = height_px - header_rows;

        let host = self
            .history
            .get(self.index)
            .map(|e| host_of(&e.url))
            .unwrap_or_default();
        let header = HeaderView {
            can_back: self.index > 0 && self.loading.is_none(),
            can_forward: self.index + 1 < self.history.len() && self.loading.is_none(),
            loading: self.loading,
            host: &host,
            field: self.field.as_ref().map(|f| FieldView {
                text: &f.text,
                cursor: f.cursor,
                all_selected: f.all_selected,
            }),
            focused: self.focus,
        };
        let view = self.page.as_ref().map(|p| engine::page_view(p, fonts));
        let viewport = Viewport {
            width: self.width,
            height: page_rows as f32 / scale,
            scale,
            scroll_y: self.scroll,
        };
        let (painter, chrome) = (&mut self.painter, &mut self.chrome);
        let blank = chrome.theme().surface;
        let shown = self
            .window
            .render(width_px, height_px, &mut |pixels, stride| {
                let (top, bottom) = pixels.split_at_mut(header_rows * stride);
                match &view {
                    Some(view) if page_rows > 0 => {
                        painter.paint_into(
                            view,
                            viewport,
                            Target {
                                pixels: bottom,
                                stride,
                                bgra: true,
                            },
                        );
                    }
                    _ => chrome::fill(bottom, stride, width_px, blank),
                }
                let frame = Frame {
                    pixels: top,
                    stride,
                    width: width_px,
                    scale,
                };
                chrome.draw(painter, frame, fonts.set(), fonts.metrics(), &header);
            });
        if let Err(e) = shown {
            self.window.set_title(&format!("Broxee: {e}"));
        }
    }
}

/// The nearest `<a href>` at or above `node`, resolved against the page URL. Only links Broxee
/// can open (http, https, file) count; same-page anchors and `javascript:` do not navigate.
fn link_for(page: &Loaded, node: NodeId) -> Option<net::Url> {
    let mut current = Some(node);
    while let Some(id) = current {
        if page.dom.tag_name(id) == Some("a") {
            let href = page.dom.attr(id, "href")?.trim();
            let lower = href.to_ascii_lowercase();
            if href.starts_with('#')
                || ["javascript:", "mailto:", "tel:", "data:"]
                    .iter()
                    .any(|p| lower.starts_with(p))
            {
                return None;
            }
            let url = page.url.join(href).ok()?;
            return matches!(url.scheme.as_str(), "http" | "https" | "file").then_some(url);
        }
        current = page.dom.parent(id);
    }
    None
}

/// Typed text to a URL: full URLs as they are, `example.com` as https.
fn parse_input(input: &str) -> Result<net::Url, String> {
    let input = input.trim();
    if input.contains("://") {
        return net::Url::parse(input);
    }
    let looks_like_host = !input.is_empty()
        && !input.contains(char::is_whitespace)
        && (input.contains('.') || input.starts_with("localhost"));
    if looks_like_host {
        return net::Url::parse(&format!("https://{input}"));
    }
    Err("Type a full address, like example.com.".to_string())
}

fn host_of(url: &net::Url) -> String {
    if url.host.is_empty() {
        url.path.rsplit('/').next().unwrap_or_default().to_string()
    } else {
        url.host.clone()
    }
}

fn error_page(url: &net::Url, error: &str, chrome: &Chrome) -> String {
    let host = host_of(url);
    let lower = error.to_ascii_lowercase();
    let title = if lower.contains("certificate") {
        format!("{host}'s certificate isn't valid, so Broxee didn't open it")
    } else {
        format!("Couldn't reach {host}")
    };
    message_page(&title, error, Some(url), chrome)
}

/// A small page in Broxee's own style (DESIGN.md tokens), rendered by the engine itself.
fn message_page(title: &str, detail: &str, retry: Option<&net::Url>, chrome: &Chrome) -> String {
    let t = chrome.theme();
    let action = retry.map_or(String::new(), |url| {
        format!(
            "<p><a href=\"{}\">Try again</a></p>",
            escape(&url.to_string())
        )
    });
    format!(
        "<!doctype html><html><head><title>{title}</title><style>\
         html {{ background: {surface} }} \
         body {{ margin: 0; color: {text}; font-size: 14px; line-height: 1.5 }} \
         main {{ max-width: 32em; margin: 0 auto; padding: 120px 24px 0 }} \
         h1 {{ font-size: 20px; margin: 0 0 8px; line-height: 1.3 }} \
         p {{ margin: 0 0 16px; color: {text_2} }} \
         a {{ color: {accent}; text-decoration: none; font-weight: bold }}\
         </style></head><body><main><h1>{title}</h1><p>{detail}</p>{action}</main></body></html>",
        title = escape(title),
        detail = escape(detail),
        surface = t.surface,
        text = t.text,
        text_2 = t.text_2,
        accent = t.accent,
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    /// An in-memory screen: 400 x 300 points at 1x, rows padded to exercise the stride.
    #[derive(Default)]
    struct FakeScreen {
        title: RefCell<String>,
        frame: RefCell<(usize, usize, Vec<u8>)>,
    }

    const PAD: usize = 16;

    impl Screen for FakeScreen {
        fn scale(&self) -> f64 {
            1.0
        }

        fn size(&self) -> (f64, f64) {
            (400.0, 300.0)
        }

        fn dark(&self) -> bool {
            false
        }

        fn set_title(&self, title: &str) {
            *self.title.borrow_mut() = title.to_string();
        }

        fn render(
            &self,
            width: usize,
            height: usize,
            draw: &mut dyn FnMut(&mut [u8], usize),
        ) -> Result<(), String> {
            let stride = width * 4 + PAD;
            let mut pixels = vec![0u8; stride * height];
            draw(&mut pixels, stride);
            *self.frame.borrow_mut() = (width, height, pixels);
            Ok(())
        }
    }

    impl FakeScreen {
        fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
            let frame = self.frame.borrow();
            let at = y * (frame.0 * 4 + PAD) + x * 4;
            frame.2[at..at + 4].try_into().expect("four bytes")
        }
    }

    /// Two linked local pages; A is tall enough to scroll.
    fn site(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("broxee-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let style = "<style>body { margin: 0 } p { margin: 0; line-height: 20px }</style>";
        let lines = "<p>line</p>".repeat(60);
        let a = dir.join("a.html");
        let b = dir.join("b.html");
        std::fs::write(
            &a,
            format!("<title>Page A</title>{style}<p><a href=\"b.html\">Go to B</a></p>{lines}"),
        )
        .unwrap();
        std::fs::write(
            &b,
            format!("<title>Page B</title>{style}<p><a href=\"a.html\">Back to A</a></p>"),
        )
        .unwrap();
        (a, b)
    }

    fn browser(target: &Path) -> Browser<FakeScreen> {
        let opts = Options {
            target: target.display().to_string(),
            tree: false,
            styles: false,
            layout: false,
            png: None,
            paint: false,
            window: true,
            depth: 8,
            width: 400.0,
            height: 300.0,
            scale: 1.0,
            fetch_css: false,
        };
        let mut b = Browser::new(opts, FakeScreen::default());
        b.start();
        b
    }

    fn key(key: Key) -> Event {
        Event::Key(KeyEvent {
            key,
            command: false,
            shift: false,
        })
    }

    fn command(c: char) -> Event {
        Event::Key(KeyEvent {
            key: Key::Char(c),
            command: true,
            shift: false,
        })
    }

    fn title(b: &Browser<FakeScreen>) -> String {
        b.window.title.borrow().clone()
    }

    /// Click the middle of the first text run containing `text`.
    fn click_text(b: &mut Browser<FakeScreen>, text: &str) -> bool {
        let page = b.page.as_ref().expect("a page is loaded");
        let run = page
            .layout
            .texts
            .iter()
            .find(|t| page.layout.text(t).contains(text))
            .expect("text on the page");
        let (x, y) = (
            run.x + run.width / 2.0,
            run.baseline - 4.0 - b.scroll + HEADER_HEIGHT,
        );
        b.handle(Event::Click {
            x: f64::from(x),
            y: f64::from(y),
        })
    }

    #[test]
    fn follows_links_and_goes_back_and_forward() {
        let (a, _) = site("links");
        let mut b = browser(&a);
        assert_eq!(title(&b), "Page A");
        assert!(click_text(&mut b, "Go to B"));
        assert_eq!(title(&b), "Page B");
        assert_eq!((b.history.len(), b.index), (2, 1));
        assert!(b.handle(command('[')));
        assert_eq!(title(&b), "Page A");
        assert!(b.handle(command(']')));
        assert_eq!(title(&b), "Page B");
        assert!(!b.handle(command(']')), "nothing forward of the last page");
        assert!(click_text(&mut b, "Back to A"));
        assert_eq!(
            (b.history.len(), b.index),
            (3, 2),
            "a new page after going back keeps the trail"
        );
    }

    #[test]
    fn scrolls_within_the_page_and_restores_position_on_back() {
        let (a, bpath) = site("scroll");
        let mut b = browser(&a);
        assert!(b.handle(key(Key::PageDown)));
        let saved = b.scroll;
        assert!(saved > 0.0);
        assert!(b.handle(key(Key::End)));
        let bottom = b.scroll;
        assert!(!b.handle(key(Key::Down)), "already at the bottom");
        assert!((bottom - b.max_scroll()).abs() < 0.01);
        assert!(b.handle(Event::Scroll { dy: 10_000.0 }));
        assert_eq!(b.scroll, 0.0, "scrolling up past the top stops at 0");
        b.handle(key(Key::PageDown));

        // Open B through the address field, then come back to A at the same position.
        assert!(b.handle(command('l')));
        assert!(b.field.as_ref().is_some_and(|f| f.all_selected));
        for c in format!("file://{}", bpath.display()).chars() {
            b.handle(key(Key::Char(c)));
        }
        b.handle(key(Key::Enter));
        assert_eq!(title(&b), "Page B");
        assert_eq!(b.scroll, 0.0);
        b.handle(command('['));
        assert_eq!(title(&b), "Page A");
        assert_eq!(b.scroll, saved);
    }

    #[test]
    fn tabs_through_the_header_and_activates_with_enter() {
        let (a, _) = site("tab");
        let mut b = browser(&a);
        let tab = |shift| {
            Event::Key(KeyEvent {
                key: Key::Tab,
                command: false,
                shift,
            })
        };
        // Only one page: back and forward are disabled, so Tab lands on reload first.
        assert!(b.handle(tab(false)));
        assert_eq!(b.focus, Some(Button::Reload));
        assert!(b.handle(tab(false)));
        assert!(b.field.is_some(), "then the address field");
        assert!(b.handle(tab(false)));
        assert_eq!(
            b.focus,
            Some(Button::Reload),
            "wraps past the disabled buttons"
        );
        assert!(b.handle(tab(true)));
        assert!(b.field.is_some(), "Shift+Tab goes backwards");

        click_text(&mut b, "Go to B");
        assert_eq!(b.focus, None);
        b.handle(tab(false));
        assert_eq!(b.focus, Some(Button::Back));
        b.render();
        assert!(b.handle(key(Key::Enter)));
        assert_eq!(
            title(&b),
            "Page A",
            "Enter on the focused back button goes back"
        );
        assert!(b.handle(key(Key::Escape)) || b.focus.is_none());
    }

    #[test]
    fn clicks_reach_fixed_links_after_scrolling_and_skip_hidden_ones() {
        let dir = std::env::temp_dir().join(format!("broxee-fixed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("fixed.html");
        let lines = "<p>line</p>".repeat(80);
        std::fs::write(
            &page,
            format!(
                "<title>Fixed</title><style>body {{ margin: 0 }} p {{ margin: 0; line-height: 20px }} \
                 #bar {{ position: fixed; top: 0; left: 0; background: #fff }} \
                 #ghost {{ visibility: hidden }}</style>\
                 <div id=bar><a href=target.html>Pinned</a></div><p>line</p><p>line</p><p>line</p><p><a id=ghost href=ghost.html>Ghost</a></p>{lines}"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("target.html"), "<title>Target</title>").unwrap();
        let mut b = browser(&page);
        b.handle(key(Key::PageDown));
        assert!(b.scroll > 100.0);
        // The pinned link is still at the top of the page area after scrolling.
        let layout = &b.page.as_ref().unwrap().layout;
        let pinned = (0..layout.texts.len())
            .find(|&i| layout.text(&layout.texts[i]) == "Pinned")
            .unwrap();
        let t = layout.texts[pinned];
        assert!(layout.text_is_fixed(pinned));
        let (x, y) = (t.x + 2.0, t.baseline - 4.0 + HEADER_HEIGHT);
        assert!(b.handle(Event::Click {
            x: f64::from(x),
            y: f64::from(y)
        }));
        assert_eq!(title(&b), "Target");

        b.handle(command('['));
        b.handle(key(Key::Home));
        let layout = &b.page.as_ref().unwrap().layout;
        let ghost = (0..layout.texts.len())
            .find(|&i| layout.text(&layout.texts[i]) == "Ghost")
            .unwrap();
        let t = layout.texts[ghost];
        b.handle(Event::Click {
            x: f64::from(t.x + 2.0),
            y: f64::from(t.baseline - 4.0 + HEADER_HEIGHT),
        });
        assert_eq!(title(&b), "Fixed", "a hidden link does not navigate");
    }

    #[test]
    fn shows_error_and_message_pages() {
        let (a, _) = site("errors");
        let mut b = browser(&a);
        b.handle(command('l'));
        b.handle(Event::Paste(
            "file:///no/such/broxee/page.html\nsecond line".into(),
        ));
        assert_eq!(
            b.field.as_ref().map(|f| f.text.as_str()),
            Some("file:///no/such/broxee/page.html")
        );
        b.handle(key(Key::Enter));
        assert_eq!(title(&b), "Couldn't reach page.html");
        assert!(b
            .page
            .as_ref()
            .unwrap()
            .dom
            .text_content(html::Dom::ROOT)
            .contains("Try again"));

        b.handle(command('l'));
        for c in "what is rust".chars() {
            b.handle(key(Key::Char(c)));
        }
        b.handle(key(Key::Enter));
        assert_eq!(title(&b), "Broxee can't search yet");
    }

    #[test]
    fn draws_the_header_and_relays_out_on_resize() {
        let (a, _) = site("render");
        let mut b = browser(&a);
        b.render();
        let theme = b.chrome.theme();
        assert_eq!(
            b.window.pixel(200, 2),
            [theme.surface.b, theme.surface.g, theme.surface.r, 255]
        );
        assert_eq!(
            b.window.pixel(200, 35),
            [theme.line.b, theme.line.g, theme.line.r, 255],
            "bottom hairline"
        );
        assert_eq!(
            b.window.pixel(399, 200),
            [255, 255, 255, 255],
            "page canvas below the header"
        );
        assert!(b.handle(Event::Resize {
            width: 250.0,
            height: 300.0,
            scale: 1.0
        }));
        assert_eq!(b.page.as_ref().unwrap().layout.width, 250.0);
    }

    #[test]
    fn edits_the_address_field() {
        let mut f = Field {
            text: "https://a.com".into(),
            cursor: 13,
            all_selected: true,
        };
        f.insert("e");
        assert_eq!(
            (f.text.as_str(), f.cursor),
            ("e", 1),
            "typing replaces a full selection"
        );
        f.insert("xé");
        f.left();
        f.backspace();
        assert_eq!(f.text, "eé");
        f.right();
        f.backspace();
        f.backspace();
        assert_eq!((f.text.as_str(), f.cursor), ("", 0));
        f.backspace();
        assert_eq!(f.text, "", "backspace at the start does nothing");
    }

    #[test]
    fn turns_typed_text_into_urls() {
        assert_eq!(
            parse_input(" example.com ").unwrap().to_string(),
            "https://example.com/"
        );
        assert_eq!(
            parse_input("http://x.org/a").unwrap().to_string(),
            "http://x.org/a"
        );
        assert_eq!(parse_input("localhost:8080").unwrap().port, 8080);
        assert!(parse_input("what is rust").is_err());
        assert!(parse_input("").is_err());
    }

    #[test]
    fn escapes_message_text() {
        assert_eq!(
            escape("<a href=\"x\">&"),
            "&lt;a href=&quot;x&quot;&gt;&amp;"
        );
    }
}
