//! The window: tabs and profiles, navigation, history, scrolling, links and the address field.
//!
//! Only the visible tab holds a built page (DOM, styles, layout). Every other tab sleeps: what
//! its page was built from, compressed as it arrived, goes to a private file on disk
//! (`hibernate`), or the tab keeps just its history once those files pass their budget.
//! Selecting a tab rebuilds it from its file without the network, at its old scroll position.
//!
//! Runs on the engine thread. Input arrives from the main thread over a channel; every event
//! queued while the engine was busy is handled before the next frame, so a burst of scroll
//! events costs one render.
//!
//! The browser draws through the `Screen` trait: the real window on macOS, an in-memory screen
//! in tests. Tests drive it with synthetic events and local files, never the real desktop.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use html::NodeId;
use paint::{Painter, Target, Viewport};
use platform::{Event, Key, KeyEvent};

use crate::chrome::{
    self, Button, Chrome, DialogView, FieldView, Frame, HeaderView, HintView, Hue, ListRow,
    PaletteHit, PaletteRow, PaletteView, SidebarItem, SidebarView, HEADER_HEIGHT, SIDEBAR_WIDTH,
};
use crate::engine::{self, FontStore, Loaded, Settings};
use crate::hibernate::{Stored, TabStore};
use crate::palette::{self, Action, Command, Hint};
use crate::Options;

/// Arrow keys scroll by this many points.
const LINE_STEP: f32 = 40.0;
/// Page Up/Down keep this much of the previous view visible.
const PAGE_OVERLAP: f32 = 40.0;
/// The glyph cache is dropped past this size, so long sessions cannot grow it without bound.
const GLYPH_CACHE_LIMIT: usize = 8 << 20;
/// Sleeping tabs' files together stay under this on disk; past it, the tabs used longest ago
/// keep only their history and reload from the network when selected.
const SNAPSHOT_BUDGET: u64 = 256 << 20;
/// The folder of a first bookmark.
const DEFAULT_FOLDER: &str = "Bookmarks";
/// A bookmarks file past this many is cut there when read.
const BOOKMARK_LIMIT: usize = 10_000;
/// How often the engine thread looks at a running load's progress: once per frame at 60 Hz.
const LOAD_POLL: Duration = Duration::from_millis(16);
/// Closed tabs that ⌘⇧T can reopen, per profile.
const CLOSED_LIMIT: usize = 20;
/// Session file limits, so a damaged file cannot make startup slow.
const SESSION_TABS: usize = 500;
const SESSION_ENTRIES: usize = 100;

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
    /// Nothing has happened for a while: free what only drawing needs.
    fn idle(&self) {}
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

    fn idle(&self) {
        self.release_spare();
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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let session = home
        .as_ref()
        .map(|home| home.join("Library/Application Support/Broxee/session"));
    let store = home.and_then(|home| TabStore::open(&home.join("Library/Caches/Broxee/tabs")).ok());
    platform::run_app("Broxee", width, height, move |window, events| {
        Browser::new(opts, window, session, store).run(&events);
    })
}

#[derive(Clone)]
struct Entry {
    url: net::Url,
    scroll: f32,
    /// The page's title when it was last shown, for the palette's history results.
    title: String,
}

/// A tab. The visible one's page lives in `Browser::page`; the others sleep.
#[derive(Default)]
struct Tab {
    history: Vec<Entry>,
    index: usize,
    title: String,
    /// The file a sleeping tab is rebuilt from. `None` for the visible tab, for a tab whose
    /// file went over budget, and for tabs restored from a session.
    snapshot: Option<Stored>,
    /// When the tab was last visible, for choosing which snapshots to drop first.
    seen: u64,
    /// The link that had keyboard focus when the tab went to sleep; the page is rebuilt
    /// identically from its snapshot, so the node id still names it.
    focused_link: Option<NodeId>,
}

impl Tab {
    fn entry(&self) -> Option<&Entry> {
        self.history.get(self.index)
    }

    fn title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if self.history.is_empty() {
            "New tab"
        } else {
            "Untitled"
        }
    }
}

/// A page loading on its own thread for the visible tab. Dropping it cancels the load: the
/// loader's next progress report fails and it stops, freeing what it built.
struct Load {
    url: net::Url,
    to: Destination,
    settings: Settings,
    messages: Receiver<LoadMessage>,
}

/// Where a finished load goes in the tab's history.
#[derive(Clone, Copy)]
enum Destination {
    /// A new entry after the current one (forward entries are dropped).
    Push,
    /// This existing entry (back, forward, reload, waking), restoring its scroll position.
    Entry(usize),
}

enum LoadMessage {
    Progress(f32),
    /// Boxed: a channel slot is as large as its largest message.
    Done(Box<Result<Loaded, String>>),
}

/// A focused link and where it is drawn: one rectangle per line it spans, in page coordinates
/// (`true` when the rectangle is in a fixed layer and does not scroll).
struct LinkFocus {
    node: NodeId,
    rects: Vec<(layout::Rect, bool)>,
}

/// A saved page.
#[derive(Clone, Debug, PartialEq)]
struct Bookmark {
    title: String,
    url: net::Url,
    folder: String,
}

/// ⌘D's dialog: the page's name and folder.
struct BookmarkDialog {
    url: net::Url,
    /// Name, then folder.
    fields: [Field; 2],
    active: usize,
    /// Editing this bookmark rather than adding one.
    existing: Option<usize>,
}

/// What the sidebar lists.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SidebarList {
    Tabs,
    Bookmarks,
}

/// What a sidebar row stands for.
#[derive(Clone, Copy)]
enum RowTarget {
    Tab(usize),
    Bookmark(usize),
    Folder,
}

/// A closed tab ⌘⇧T can bring back: its history, not its page.
struct Closed {
    history: Vec<Entry>,
    index: usize,
    title: String,
    position: usize,
}

/// Work and personal browsing kept apart: each profile has its own tabs and history, and
/// switching drops every open connection and TLS session (`net::forget_sessions`).
struct Profile {
    name: &'static str,
    hue: Hue,
    tabs: Vec<Tab>,
    active: usize,
    closed: Vec<Closed>,
    /// This profile's cookies: logins in one profile never reach the other.
    cookies: Arc<net::Cookies>,
    bookmarks: Vec<Bookmark>,
    /// Preselected in ⌘D's dialog (DESIGN.md, section 7).
    last_folder: String,
}

impl Profile {
    fn new(name: &'static str, hue: Hue) -> Profile {
        Profile {
            name,
            hue,
            tabs: vec![Tab::default()],
            active: 0,
            closed: Vec::new(),
            cookies: Arc::new(net::Cookies::new()),
            bookmarks: Vec::new(),
            last_folder: DEFAULT_FOLDER.into(),
        }
    }
}

/// The ⌘K palette while it is open.
struct Palette {
    query: Field,
    /// What matches the query, best first.
    items: Vec<palette::Item>,
    selected: usize,
    /// The first visible result: the list scrolls to keep `selected` in view.
    first: usize,
}

#[derive(Default)]
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
    /// Shared with the loader thread.
    fonts: Arc<FontStore>,
    width: f32,
    height: f32,
    scale: f32,
    dark: bool,
    /// The visible tab's page and scroll position.
    page: Option<Loaded>,
    scroll: f32,
    /// The page shown is the visible tab's current entry, as loaded (not an error or a message),
    /// so it may be kept as the tab's snapshot.
    page_is_entry: bool,
    profiles: Vec<Profile>,
    /// The visible profile.
    profile: usize,
    sidebar: bool,
    clock: u64,
    snapshot_budget: u64,
    session: Option<PathBuf>,
    /// Where sleeping tabs go; without it they keep only their history.
    store: Option<TabStore>,
    field: Option<Field>,
    palette: Option<Palette>,
    dialog: Option<BookmarkDialog>,
    list: SidebarList,
    /// The bookmark row chosen with ↑ ↓ while the bookmarks panel is open.
    bookmark_row: Option<usize>,
    /// Header button with keyboard focus, reached with Tab.
    focus: Option<Button>,
    /// A link in the page with keyboard focus, reached with Tab after the header.
    link_focus: Option<LinkFocus>,
    /// Progress of `load`, for the header's loading line.
    loading: Option<f32>,
    load: Option<Load>,
}

impl<S: Screen> Browser<S> {
    fn new(
        opts: Options,
        window: S,
        session: Option<PathBuf>,
        store: Option<TabStore>,
    ) -> Browser<S> {
        let (width, height) = window.size();
        let (scale, dark) = (window.scale() as f32, window.dark());
        let mut profiles = vec![
            Profile::new("Personal", Hue::Emerald),
            Profile::new("Work", Hue::Amber),
        ];
        for profile in &mut profiles {
            let read = |file| {
                profile_file(session.as_deref(), profile.name, file)
                    .and_then(|path| std::fs::read_to_string(path).ok())
            };
            if let Some(text) = read("cookies") {
                profile.cookies = Arc::new(net::Cookies::load(&text));
            }
            if let Some(text) = read("bookmarks") {
                let (bookmarks, last) = parse_bookmarks(&text);
                profile.bookmarks = bookmarks;
                if let Some(last) = last {
                    profile.last_folder = last;
                }
            }
        }
        Browser {
            opts,
            window,
            painter: Painter::new(),
            chrome: Chrome::new(dark),
            fonts: Arc::new(FontStore::start()),
            width: width as f32,
            height: height as f32,
            scale,
            dark,
            page: None,
            scroll: 0.0,
            page_is_entry: false,
            profiles,
            profile: 0,
            sidebar: true,
            clock: 0,
            snapshot_budget: SNAPSHOT_BUDGET,
            session,
            store,
            field: None,
            palette: None,
            dialog: None,
            list: SidebarList::Tabs,
            bookmark_row: None,
            focus: None,
            link_focus: None,
            loading: None,
            load: None,
        }
    }

    fn tab(&self) -> &Tab {
        let p = &self.profiles[self.profile];
        &p.tabs[p.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        let p = &mut self.profiles[self.profile];
        &mut p.tabs[p.active]
    }

    /// Left edge of the pane, in points.
    fn pane_left(&self) -> f32 {
        if self.sidebar {
            SIDEBAR_WIDTH.min(self.width)
        } else {
            0.0
        }
    }

    fn pane_width(&self) -> f32 {
        (self.width - self.pane_left()).max(1.0)
    }

    fn settings(&self) -> Settings {
        Settings {
            width: self.pane_width(),
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
        /// How long without events before the window frees its spare frame.
        const IDLE_AFTER: Duration = Duration::from_secs(1);
        self.start();
        let mut trimmed = false;
        loop {
            // While a page loads, wake every frame to show its progress; otherwise sleep until
            // an event arrives.
            let wait = if self.load.is_some() {
                LOAD_POLL
            } else {
                IDLE_AFTER
            };
            let mut dirty = false;
            let mut quiet = false;
            match events.recv_timeout(wait) {
                Ok(event) => {
                    dirty |= self.handle(event);
                    for event in events.try_iter() {
                        dirty |= self.handle(event);
                    }
                }
                Err(RecvTimeoutError::Timeout) => quiet = true,
                Err(RecvTimeoutError::Disconnected) => break,
            }
            dirty |= self.poll_load();
            if dirty {
                self.render();
                trimmed = false;
            } else if quiet && self.load.is_none() && !trimmed {
                self.window.idle();
                trimmed = true;
            }
        }
    }

    /// Show the first frame, bring back the last session, and open the page from the command
    /// line (selecting its tab if the session already has it).
    fn start(&mut self) {
        // The header needs fonts; they load in a few milliseconds, before the first frame.
        self.fonts.ready();
        self.render();
        if let Some(text) = self
            .session
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            if let Some((profiles, visible)) = parse_session(&text) {
                for (profile, (tabs, active)) in self.profiles.iter_mut().zip(profiles) {
                    if !tabs.is_empty() {
                        profile.active = active.min(tabs.len() - 1);
                        profile.tabs = tabs;
                    }
                }
                self.profile = visible.min(self.profiles.len() - 1);
            }
        }
        let targets: Vec<String> = std::iter::once(&self.opts.target)
            .chain(&self.opts.more)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if targets.is_empty() {
            self.wake();
        }
        let last = targets.len().saturating_sub(1);
        for (i, target) in targets.iter().enumerate() {
            self.open_target(target);
            // Each page sleeps before the next one loads; only the last one loads in the
            // background while the window answers input.
            if i < last {
                self.wait_for_load();
            }
        }
    }

    /// An address from the command line: select its tab if one already shows it, otherwise
    /// open it in a new tab (or in the first, empty one).
    fn open_target(&mut self, target: &str) {
        let url = match engine::to_url(target) {
            Ok(url) => url,
            Err(e) => return self.show_message("Broxee couldn't open that", &e),
        };
        let p = &self.profiles[self.profile];
        let open = p
            .tabs
            .iter()
            .position(|t| t.entry().is_some_and(|e| e.url == url));
        if self.page.is_some() {
            self.sleep();
        }
        let p = &mut self.profiles[self.profile];
        match open {
            Some(i) => {
                p.active = i;
                self.wake();
            }
            None => {
                if !p.tabs[p.active].history.is_empty() {
                    p.tabs.push(Tab::default());
                    p.active = p.tabs.len() - 1;
                }
                self.navigate(url, Destination::Push);
            }
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
                let size = (width as f32, height as f32, scale as f32);
                if size == (self.width, self.height, self.scale) {
                    // macOS repeats notifications (focus, the same backing scale): nothing to do.
                    return false;
                }
                self.width = width as f32;
                self.height = height as f32;
                self.scale = scale as f32;
                self.relayout();
                true
            }
            Event::Click { x, y } => self.click(x as f32, y as f32),
            Event::Key(key) => self.key(key),
            Event::Paste(text) => {
                let line = text.trim().lines().next().unwrap_or_default();
                if let Some(palette) = self.palette.as_mut() {
                    palette.query.insert(line);
                    self.refresh_palette();
                    return true;
                }
                match self.field.as_mut() {
                    Some(field) => {
                        field.insert(line);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// Lay the page out again for a new pane size.
    fn relayout(&mut self) {
        let settings = self.settings();
        if let Some(page) = self.page.as_mut() {
            engine::relayout(page, settings, &self.fonts);
        }
        self.clamp_scroll();
        // The focused link moved with the layout.
        if let Some(node) = self.link_focus.take().map(|l| l.node) {
            self.focus_link(node, false);
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
        let left = self.pane_left();
        let pane_width = self.pane_width();
        if let Some(d) = &mut self.dialog {
            if x >= left {
                return match chrome::dialog_hit(x - left, y, pane_width, d.fields.len()) {
                    Some(Some(field)) => {
                        d.active = field;
                        true
                    }
                    Some(None) => false,
                    None => {
                        self.dialog = None;
                        self.chrome.forget_palette();
                        true
                    }
                };
            }
            self.dialog = None;
            self.chrome.forget_palette();
        }
        if let Some(p) = &self.palette {
            if x >= left {
                let rows = (p.items.len() - p.first).min(chrome::palette_capacity(self.height));
                return match chrome::palette_hit(x - left, y, self.pane_width(), rows) {
                    PaletteHit::Row(i) => self.activate(p.first + i),
                    PaletteHit::Inside => false,
                    PaletteHit::Outside => self.close_palette(),
                };
            }
            self.close_palette();
        }
        if x < left {
            self.field = None;
            self.focus = None;
            let hit = {
                let list = self.sidebar_rows();
                chrome::sidebar_hit(x, y, &list.rows, list.footer.is_some()).map(
                    |item| match item {
                        SidebarItem::Row(r) | SidebarItem::CloseRow(r) => {
                            (item, Some(list.targets[r]))
                        }
                        _ => (item, None),
                    },
                )
            };
            return match hit {
                Some((SidebarItem::Profile, _)) => self.switch_profile(),
                Some((SidebarItem::Search, _)) => self.open_palette(),
                Some((SidebarItem::CloseRow(_), _)) => self.close_tab(),
                Some((SidebarItem::Footer, _)) => self.new_tab(),
                Some((_, Some(RowTarget::Tab(i)))) => self.select_tab(i),
                Some((_, Some(RowTarget::Bookmark(i)))) => self.open_bookmark(i),
                _ => false,
            };
        }
        let x = x - left;
        if y < HEADER_HEIGHT {
            self.focus = None;
            return match chrome::hit(x, self.pane_width()) {
                Some(Button::Back) => self.back(),
                Some(Button::Forward) => self.forward(),
                Some(Button::Reload) => self.reload_or_stop(),
                Some(Button::Address) => self.focus_field(),
                None => false,
            };
        }
        let was_editing = self.field.take().is_some() | self.focus.take().is_some();
        let was_editing = was_editing | self.link_focus.take().is_some();
        if let Some(url) = self.link_at(x, y - HEADER_HEIGHT) {
            self.follow(url);
            return true;
        }
        was_editing
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if self.dialog.is_some() {
            if !k.command {
                return self.dialog_key(k);
            }
            // ⌘D closes it; any other shortcut closes it and runs.
            self.dialog = None;
            self.chrome.forget_palette();
            if k.key == Key::Char('d') {
                return true;
            }
        }
        if self.palette.is_some() {
            if !k.command {
                return self.palette_key(k);
            }
            // ⌘K closes it; any other shortcut closes it and runs.
            self.close_palette();
            if k.key == Key::Char('k') {
                return true;
            }
        }
        if k.command {
            return match k.key {
                Key::Char('k') => self.open_palette(),
                Key::Char('b') if k.option => self.run_command(Command::ShowBookmarks),
                Key::Char('d') => self.run_command(Command::BookmarkPage),
                Key::Char('t') if k.shift => self.run_command(Command::ReopenTab),
                Key::Char('t') => self.run_command(Command::NewTab),
                Key::Char('w') => self.run_command(Command::CloseTab),
                Key::Char('p') if k.shift => self.run_command(Command::SwitchProfile),
                Key::Char('\\') => self.run_command(Command::ToggleSidebar),
                // ⌘9 is the last tab, as in other browsers.
                Key::Char('9') => {
                    let last = self.profiles[self.profile].tabs.len() - 1;
                    self.select_tab(last)
                }
                Key::Char(c @ '1'..='8') => self.select_tab(c as usize - '1' as usize),
                Key::Char('l') => self.run_command(Command::FocusAddress),
                Key::Char('[') => self.run_command(Command::Back),
                Key::Char(']') => self.run_command(Command::Forward),
                Key::Char('r') => self.run_command(Command::Reload),
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
                // `move_focus` closes the field, after reading where focus was.
                Key::Tab => return self.move_focus(k.shift),
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
                        Button::Reload => self.reload_or_stop(),
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
        if self.list == SidebarList::Bookmarks && self.focus.is_none() && self.link_focus.is_none()
        {
            if let Some(done) = self.bookmarks_key(k.key) {
                return done;
            }
        }
        if let Some(node) = self.link_focus.as_ref().map(|l| l.node) {
            match k.key {
                Key::Enter => {
                    let url = self.page.as_ref().and_then(|p| link_for(p, node));
                    return match url {
                        Some(url) => {
                            self.follow(url);
                            true
                        }
                        None => false,
                    };
                }
                Key::Escape => {
                    self.link_focus = None;
                    return true;
                }
                _ => {}
            }
        }
        if k.key == Key::Escape {
            return self.stop_load();
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

    /// Tab order: back, forward, reload, the address field, then the page's links in document
    /// order. Disabled buttons and links that are not shown are skipped; the order wraps.
    fn move_focus(&mut self, backwards: bool) -> bool {
        #[derive(Clone, Copy, PartialEq)]
        enum Stop {
            Header(Button),
            Link(NodeId),
        }
        let tab = self.tab();
        let enabled = |b: Button| match b {
            Button::Back => tab.index > 0,
            Button::Forward => tab.index + 1 < tab.history.len(),
            Button::Reload => !tab.history.is_empty() || self.load.is_some(),
            Button::Address => true,
        };
        let mut order: Vec<Stop> = [
            Button::Back,
            Button::Forward,
            Button::Reload,
            Button::Address,
        ]
        .into_iter()
        .filter(|&b| enabled(b))
        .map(Stop::Header)
        .collect();
        if let Some(page) = &self.page {
            order.extend(focusable_links(page).into_iter().map(Stop::Link));
        }
        let current = if self.field.is_some() {
            Some(Stop::Header(Button::Address))
        } else if let Some(link) = &self.link_focus {
            Some(Stop::Link(link.node))
        } else {
            self.focus.map(Stop::Header)
        };
        let n = order.len();
        let next = match (
            current.and_then(|c| order.iter().position(|&o| o == c)),
            backwards,
        ) {
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
            (None, false) => 0,
            (None, true) => n - 1,
        };
        self.field = None;
        self.focus = None;
        self.link_focus = None;
        match order[next] {
            Stop::Header(Button::Address) => {
                self.focus_field();
            }
            Stop::Header(button) => self.focus = Some(button),
            Stop::Link(node) => self.focus_link(node, true),
        }
        true
    }

    /// Give `node` (a link) keyboard focus; with `reveal`, scroll it into view.
    fn focus_link(&mut self, node: NodeId, reveal: bool) {
        let Some(page) = &self.page else {
            return;
        };
        let rects = link_rects(page, node, self.fonts.metrics());
        if rects.is_empty() {
            return;
        }
        if reveal {
            const MARGIN: f32 = 40.0;
            let view = self.page_height();
            let scrolling = rects.iter().filter(|(_, fixed)| !fixed).map(|(r, _)| r);
            let top = scrolling.clone().map(|r| r.y).fold(f32::MAX, f32::min);
            let bottom = scrolling.map(|r| r.y + r.height).fold(f32::MIN, f32::max);
            if top < f32::MAX {
                if top < self.scroll + MARGIN {
                    self.scroll_to(top - MARGIN);
                } else if bottom > self.scroll + view - MARGIN {
                    self.scroll_to((bottom - view + MARGIN).min(top - MARGIN));
                }
            }
        }
        self.link_focus = Some(LinkFocus { node, rects });
    }

    /// Follow a link the person chose, by click or with Enter.
    fn follow(&mut self, url: net::Url) {
        self.save_scroll();
        // A link from another site does not carry SameSite=Strict cookies.
        let from = self
            .page
            .as_ref()
            .map(|p| net::site(&p.url.host).to_string());
        let same_site = from.is_none_or(|from| from == net::site(&url.host));
        self.navigate_from(url, Destination::Push, same_site);
    }

    /// Run a command, from its shortcut or the palette.
    fn run_command(&mut self, command: Command) -> bool {
        match command {
            Command::NewTab => self.new_tab(),
            Command::CloseTab => self.close_tab(),
            Command::ReopenTab => self.reopen_tab(),
            Command::SwitchProfile => self.switch_profile(),
            Command::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                self.relayout();
                true
            }
            Command::FocusAddress => self.focus_field(),
            Command::BookmarkPage => self.open_bookmark_dialog(),
            Command::ShowBookmarks => {
                self.list = match self.list {
                    SidebarList::Tabs => SidebarList::Bookmarks,
                    SidebarList::Bookmarks => SidebarList::Tabs,
                };
                self.bookmark_row = None;
                if !self.sidebar {
                    self.sidebar = true;
                    self.relayout();
                }
                true
            }
            Command::RemoveBookmark => {
                let url = self.tab().entry().map(|e| e.url.clone());
                let p = &mut self.profiles[self.profile];
                let before = p.bookmarks.len();
                p.bookmarks.retain(|b| Some(&b.url) != url.as_ref());
                let removed = p.bookmarks.len() != before;
                if removed {
                    self.save_bookmarks();
                }
                removed
            }
            Command::ClearCookies => {
                let cookies = &self.profiles[self.profile].cookies;
                cookies.clear();
                self.save_cookies();
                // Connections may still be tied to the old cookies on the server's side.
                net::forget_sessions();
                true
            }
            Command::Back => self.back(),
            Command::Forward => self.forward(),
            Command::Reload => self.reload(),
        }
    }

    fn open_palette(&mut self) -> bool {
        self.field = None;
        self.focus = None;
        self.palette = Some(Palette {
            query: Field::default(),
            items: Vec::new(),
            selected: 0,
            first: 0,
        });
        self.refresh_palette();
        true
    }

    /// ⌘D: name and file the visible page; a page already saved opens its bookmark.
    fn open_bookmark_dialog(&mut self) -> bool {
        let Some(entry) = self.tab().entry() else {
            return false;
        };
        let url = entry.url.clone();
        let title = self.tab().title().to_string();
        let p = &self.profiles[self.profile];
        let existing = p.bookmarks.iter().position(|b| b.url == url);
        let (name, folder) = match existing {
            Some(i) => (p.bookmarks[i].title.clone(), p.bookmarks[i].folder.clone()),
            None => (title, p.last_folder.clone()),
        };
        let field = |text: String, selected| Field {
            cursor: text.len(),
            text,
            all_selected: selected,
        };
        self.close_palette();
        self.field = None;
        self.focus = None;
        self.dialog = Some(BookmarkDialog {
            url,
            fields: [field(name, true), field(folder, false)],
            active: 0,
            existing,
        });
        true
    }

    fn dialog_key(&mut self, k: KeyEvent) -> bool {
        let Some(d) = self.dialog.as_mut() else {
            return false;
        };
        let field = &mut d.fields[d.active];
        match k.key {
            Key::Char(c) => field.insert(c.encode_utf8(&mut [0; 4])),
            Key::Backspace => field.backspace(),
            Key::Left => field.left(),
            Key::Right => field.right(),
            Key::Home => {
                field.all_selected = false;
                field.cursor = 0;
            }
            Key::End => {
                field.all_selected = false;
                field.cursor = field.text.len();
            }
            Key::Tab | Key::Up | Key::Down => {
                field.all_selected = false;
                d.active = 1 - d.active;
                // Typing replaces the field just reached, as in macOS dialogs.
                d.fields[d.active].all_selected = true;
            }
            Key::Escape => {
                self.dialog = None;
                self.chrome.forget_palette();
            }
            Key::Enter => return self.save_bookmark_dialog(),
            _ => return false,
        }
        true
    }

    fn save_bookmark_dialog(&mut self) -> bool {
        let Some(d) = self.dialog.take() else {
            return false;
        };
        self.chrome.forget_palette();
        let clean = |s: &str| s.trim().replace(['\t', '\n', '\r'], " ");
        let title = match clean(&d.fields[0].text) {
            t if t.is_empty() => d.url.to_string(),
            t => t,
        };
        let folder = match clean(&d.fields[1].text) {
            f if f.is_empty() => DEFAULT_FOLDER.to_string(),
            f => f,
        };
        let p = &mut self.profiles[self.profile];
        let bookmark = Bookmark {
            title,
            url: d.url,
            folder: folder.clone(),
        };
        match d.existing.filter(|&i| i < p.bookmarks.len()) {
            Some(i) => p.bookmarks[i] = bookmark,
            None => p.bookmarks.push(bookmark),
        }
        p.last_folder = folder;
        self.save_bookmarks();
        true
    }

    fn open_bookmark(&mut self, index: usize) -> bool {
        let Some(url) = self.profiles[self.profile]
            .bookmarks
            .get(index)
            .map(|b| b.url.clone())
        else {
            return false;
        };
        self.save_scroll();
        self.navigate(url, Destination::Push);
        true
    }

    /// ↑ ↓ Enter Esc while the bookmarks panel is open; `None` for keys it leaves alone.
    fn bookmarks_key(&mut self, key: Key) -> Option<bool> {
        let targets = self.sidebar_rows().targets;
        let items: Vec<usize> = (0..targets.len())
            .filter(|&r| matches!(targets[r], RowTarget::Bookmark(_)))
            .collect();
        let at = self
            .bookmark_row
            .and_then(|r| items.iter().position(|&i| i == r));
        match key {
            Key::Down | Key::Up if !items.is_empty() => {
                let next = match (at, key == Key::Down) {
                    (None, true) => 0,
                    (None, false) => items.len() - 1,
                    (Some(i), true) => (i + 1).min(items.len() - 1),
                    (Some(i), false) => i.saturating_sub(1),
                };
                self.bookmark_row = Some(items[next]);
                Some(true)
            }
            Key::Enter => {
                let row = self.bookmark_row?;
                match targets.get(row) {
                    Some(&RowTarget::Bookmark(i)) => Some(self.open_bookmark(i)),
                    _ => None,
                }
            }
            Key::Escape => {
                self.list = SidebarList::Tabs;
                self.bookmark_row = None;
                Some(true)
            }
            _ => None,
        }
    }

    fn sidebar_rows(&self) -> SidebarRows<'_> {
        sidebar_rows(&self.profiles[self.profile], self.list, self.bookmark_row)
    }

    /// Write the visible profile's bookmarks, readable by this user only.
    fn save_bookmarks(&self) {
        let profile = &self.profiles[self.profile];
        let Some(path) = profile_file(self.session.as_deref(), profile.name, "bookmarks") else {
            return;
        };
        let text = write_bookmarks(&profile.bookmarks, &profile.last_folder);
        if let Err(e) = write_private(&path, text.as_bytes()) {
            self.window
                .set_title(&format!("Broxee: couldn't save bookmarks ({e})"));
        }
    }

    fn close_palette(&mut self) -> bool {
        if self.palette.take().is_none() {
            return false;
        }
        self.chrome.forget_palette();
        true
    }

    /// Match the query again: a typed address first, then ranked tabs, history and commands.
    fn refresh_palette(&mut self) {
        let Some(query) = self.palette.as_ref().map(|p| p.query.text.clone()) else {
            return;
        };
        let mut items = Vec::new();
        if let Ok(url) = parse_input(&query) {
            items.push(palette::Item {
                title: format!("Open {}", url),
                detail: "Address".into(),
                hint: Hint::Label(String::new()),
                action: Action::Open(url),
            });
        }
        items.extend(palette::rank(&query, self.palette_candidates()));
        if let Some(p) = self.palette.as_mut() {
            p.items = items;
            p.selected = 0;
            p.first = 0;
        }
    }

    /// Everything the palette can find, in the order shown for an empty query: open tabs (this
    /// profile's first, most recent first), this profile's history, then commands. History stays
    /// within the profile; tabs of the other profiles are listed with their profile's name.
    fn palette_candidates(&self) -> Vec<palette::Item> {
        let mut items = Vec::new();
        let others = (0..self.profiles.len()).filter(|&p| p != self.profile);
        for p in std::iter::once(self.profile).chain(others) {
            let profile = &self.profiles[p];
            let mut tabs: Vec<(usize, &Tab)> = profile
                .tabs
                .iter()
                .enumerate()
                .filter(|&(i, t)| {
                    !(p == self.profile && i == profile.active) && t.entry().is_some()
                })
                .collect();
            tabs.sort_by_key(|&(_, t)| std::cmp::Reverse(t.seen));
            for (index, tab) in tabs {
                items.push(palette::Item {
                    title: tab.title().to_string(),
                    detail: tab.entry().map(|e| e.url.to_string()).unwrap_or_default(),
                    hint: Hint::Label(format!("Tab · {}", profile.name)),
                    action: Action::Tab { profile: p, index },
                });
            }
        }
        let profile = &self.profiles[self.profile];
        for b in &profile.bookmarks {
            items.push(palette::Item {
                title: b.title.clone(),
                detail: b.url.to_string(),
                hint: Hint::Label(format!("Bookmark · {}", b.folder)),
                action: Action::Visit(b.url.clone()),
            });
        }
        const HISTORY_LIMIT: usize = 200;
        let profile = &self.profiles[self.profile];
        let mut tabs: Vec<&Tab> = profile.tabs.iter().collect();
        // The visible tab is the most recent.
        tabs.sort_by_key(|t| std::cmp::Reverse(t.seen));
        tabs.sort_by_key(|t| !std::ptr::eq(*t, &profile.tabs[profile.active]));
        let mut seen = std::collections::HashSet::new();
        'history: for tab in tabs {
            for entry in tab.history.iter().rev() {
                if seen.len() >= HISTORY_LIMIT {
                    break 'history;
                }
                let url = entry.url.to_string();
                if !seen.insert(url.clone()) {
                    continue;
                }
                let title = if entry.title.is_empty() {
                    host_of(&entry.url)
                } else {
                    entry.title.clone()
                };
                items.push(palette::Item {
                    title,
                    detail: url,
                    hint: Hint::Label("History".into()),
                    action: Action::Visit(entry.url.clone()),
                });
            }
        }
        for command in Command::ALL {
            items.push(palette::Item {
                title: command.label().into(),
                detail: "Command".into(),
                hint: command
                    .keys()
                    .map_or(Hint::Label(String::new()), Hint::Keys),
                action: Action::Run(command),
            });
        }
        items
    }

    /// A key while the palette is open: edit the query, move through results, open one.
    fn palette_key(&mut self, k: KeyEvent) -> bool {
        let rows = chrome::palette_capacity(self.height);
        let Some(p) = self.palette.as_mut() else {
            return false;
        };
        let step = |p: &mut Palette, delta: isize| {
            if p.items.is_empty() {
                return;
            }
            let last = p.items.len() as isize - 1;
            p.selected = (p.selected as isize + delta).clamp(0, last) as usize;
            if p.selected < p.first {
                p.first = p.selected;
            } else if p.selected >= p.first + rows {
                p.first = p.selected + 1 - rows;
            }
        };
        match k.key {
            Key::Char(c) => {
                p.query.insert(c.encode_utf8(&mut [0; 4]));
                self.refresh_palette();
            }
            Key::Backspace => {
                p.query.backspace();
                self.refresh_palette();
            }
            Key::Left => p.query.left(),
            Key::Right => p.query.right(),
            Key::Home => p.query.cursor = 0,
            Key::End => p.query.cursor = p.query.text.len(),
            Key::Down => step(p, 1),
            Key::Up => step(p, -1),
            Key::Tab => step(p, if k.shift { -1 } else { 1 }),
            Key::PageDown => step(p, rows as isize),
            Key::PageUp => step(p, -(rows as isize)),
            Key::Enter => {
                let selected = p.selected;
                return self.activate(selected);
            }
            Key::Escape => return self.close_palette(),
        }
        true
    }

    /// Open the palette's result `index`.
    fn activate(&mut self, index: usize) -> bool {
        let Some(action) = self
            .palette
            .as_ref()
            .and_then(|p| p.items.get(index))
            .map(|item| item.action.clone())
        else {
            return false;
        };
        self.close_palette();
        match action {
            Action::Open(url) | Action::Visit(url) => {
                self.save_scroll();
                self.navigate(url, Destination::Push);
                true
            }
            Action::Tab { profile, index } => self.show_tab(profile, index),
            Action::Run(command) => self.run_command(command),
        }
    }

    fn focus_field(&mut self) -> bool {
        self.close_palette();
        self.focus = None;
        let text = self
            .tab()
            .entry()
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
        let tab = self.tab_mut();
        let index = tab.index;
        if let Some(entry) = tab.history.get_mut(index) {
            entry.scroll = scroll;
        }
    }

    fn back(&mut self) -> bool {
        let index = self.tab().index;
        if index == 0 {
            return false;
        }
        self.go_to_entry(index - 1);
        true
    }

    fn forward(&mut self) -> bool {
        let index = self.tab().index;
        if index + 1 >= self.tab().history.len() {
            return false;
        }
        self.go_to_entry(index + 1);
        true
    }

    fn reload(&mut self) -> bool {
        if self.tab().entry().is_none() {
            return false;
        }
        self.go_to_entry(self.tab().index);
        true
    }

    /// The reload button turns into a stop button while a page loads.
    fn reload_or_stop(&mut self) -> bool {
        if self.load.is_some() {
            self.stop_load()
        } else {
            self.reload()
        }
    }

    /// Load the tab's history entry `index`. The tab moves to it only when the page arrives,
    /// so a stopped load leaves the tab where it was.
    fn go_to_entry(&mut self, index: usize) {
        self.save_scroll();
        let url = self.tab().history[index].url.clone();
        self.navigate(url, Destination::Entry(index));
    }

    /// What the user typed in the address field.
    fn open_input(&mut self, input: &str) {
        match parse_input(input) {
            Ok(url) => {
                self.save_scroll();
                self.navigate(url, Destination::Push);
            }
            Err(message) => self.show_message("Broxee can't search yet", &message),
        }
    }

    /// Start loading `url` on its own thread; `to` says where it goes in the history when it
    /// arrives. The current page stays on screen and the window keeps answering input. A load
    /// already running is cancelled: one page is built at a time.
    fn navigate(&mut self, url: net::Url, to: Destination) {
        // Typed, from the palette, back, forward, reload: started by the person.
        self.navigate_from(url, to, true);
    }

    /// `navigate`, saying whether the navigation started on the same site as `url`.
    fn navigate_from(&mut self, url: net::Url, to: Destination, same_site_initiator: bool) {
        self.stop_load();
        self.field = None;
        self.focus = None;
        let settings = self.settings();
        let (sender, messages) = mpsc::channel();
        let fonts = Arc::clone(&self.fonts);
        let cookies = Arc::clone(&self.profiles[self.profile].cookies);
        let target = url.clone();
        let spawned = std::thread::Builder::new()
            .name("load".into())
            .spawn(move || {
                let jar = engine::Jar {
                    cookies: &cookies,
                    same_site_initiator,
                };
                // A failed send means the browser dropped the load: stop at the next stage.
                let result = engine::load(&target, settings, &fonts, Some(jar), &mut |progress| {
                    sender.send(LoadMessage::Progress(progress)).is_ok()
                });
                let _ = sender.send(LoadMessage::Done(Box::new(result)));
            });
        let load = Load {
            url,
            to,
            settings,
            messages,
        };
        if let Err(e) = spawned {
            return self.finish_load(load, Err(e.to_string()));
        }
        self.load = Some(load);
        self.loading = Some(0.1);
        self.render();
    }

    /// Apply what the loader has sent since the last look; returns whether to repaint.
    fn poll_load(&mut self) -> bool {
        let mut dirty = false;
        loop {
            let Some(load) = &self.load else {
                return dirty;
            };
            match load.messages.try_recv() {
                Ok(LoadMessage::Progress(p)) => {
                    self.loading = Some(p);
                    dirty = true;
                }
                Ok(LoadMessage::Done(result)) => {
                    let load = self.load.take().expect("checked above");
                    self.finish_load(load, *result);
                    // `show` painted the new page already.
                    return false;
                }
                Err(TryRecvError::Empty) => return dirty,
                Err(TryRecvError::Disconnected) => {
                    let load = self.load.take().expect("checked above");
                    self.finish_load(load, Err("the page stopped loading".into()));
                    return false;
                }
            }
        }
    }

    /// Block until the running load, if any, has finished and is shown.
    fn wait_for_load(&mut self) {
        while let Some(load) = &self.load {
            let message = load.messages.recv();
            let load = match message {
                Ok(LoadMessage::Progress(p)) => {
                    self.loading = Some(p);
                    continue;
                }
                Ok(LoadMessage::Done(result)) => (self.load.take(), *result),
                Err(_) => (self.load.take(), Err("the page stopped loading".into())),
            };
            if let (Some(l), result) = load {
                self.finish_load(l, result);
            }
        }
    }

    /// Stop the running load, keeping the page that is on screen.
    fn stop_load(&mut self) -> bool {
        if self.load.take().is_none() {
            return false;
        }
        self.loading = None;
        true
    }

    /// Show a finished load (or its error page) and record it in the tab's history.
    fn finish_load(&mut self, load: Load, result: Result<Loaded, String>) {
        self.loading = None;
        self.save_cookies();
        let url = load.url;
        if matches!(&result, Err(e) if e == engine::CANCELLED) {
            return;
        }
        let loaded = result.is_ok();
        let mut page = result.unwrap_or_else(|e| {
            engine::page_from_html(
                url.clone(),
                &error_page(&url, &e, &self.chrome),
                load.settings,
                &self.fonts,
            )
        });
        // The window may have changed size while the page loaded.
        let settings = self.settings();
        if settings != load.settings {
            engine::relayout(&mut page, settings, &self.fonts);
        }

        let tab = self.tab_mut();
        let scroll = match load.to {
            Destination::Push => {
                if !tab.history.is_empty() {
                    tab.history.truncate(tab.index + 1);
                }
                tab.history.push(Entry {
                    url: page.url.clone(),
                    scroll: 0.0,
                    title: String::new(),
                });
                tab.index = tab.history.len() - 1;
                0.0
            }
            Destination::Entry(i) if i < tab.history.len() => {
                tab.index = i;
                tab.history[i].scroll
            }
            Destination::Entry(_) => 0.0,
        };
        let title = self.show(page, scroll);
        self.set_tab_title(title);
        self.page_is_entry = loaded;
        self.save_session();
    }

    /// Put the visible tab to sleep: keep what rebuilds its page, drop the page.
    fn sleep(&mut self) {
        self.stop_load();
        self.save_scroll();
        self.field = None;
        self.focus = None;
        let focused_link = self.link_focus.take().map(|l| l.node);
        self.tab_mut().focused_link = focused_link;
        let page = self.page.take();
        self.painter.forget_images();
        let keep = std::mem::take(&mut self.page_is_entry);
        self.clock += 1;
        let clock = self.clock;
        // The page's DOM, styles and layout are freed first; then the sources are written and
        // freed too. A tab that cannot be written keeps its history and reloads when selected.
        let snapshot = page.filter(|_| keep).map(engine::snapshot);
        let stored = match (snapshot, self.store.as_mut()) {
            (Some(snapshot), Some(store)) => store.put(&snapshot).ok(),
            _ => None,
        };
        let tab = self.tab_mut();
        tab.seen = clock;
        tab.snapshot = stored;
        self.trim_snapshots();
    }

    /// Fit the sleeping tabs' files in their budget, removing those seen longest ago first.
    fn trim_snapshots(&mut self) {
        let mut sleeping: Vec<(u64, usize, usize)> = Vec::new();
        let mut total = 0;
        for (p, profile) in self.profiles.iter().enumerate() {
            for (t, tab) in profile.tabs.iter().enumerate() {
                if let Some(stored) = &tab.snapshot {
                    sleeping.push((tab.seen, p, t));
                    total += stored.bytes;
                }
            }
        }
        sleeping.sort_unstable_by_key(|s| s.0);
        for (_, p, t) in sleeping {
            if total <= self.snapshot_budget {
                break;
            }
            if let Some(stored) = self.profiles[p].tabs[t].snapshot.take() {
                total -= stored.bytes;
                if let Some(store) = &self.store {
                    store.remove(stored);
                }
            }
        }
    }

    /// Show the visible tab: from its snapshot, from the network, or as a new tab.
    fn wake(&mut self) {
        let settings = self.settings();
        let tab = self.tab_mut();
        let stored = tab.snapshot.take();
        let entry = tab.entry().cloned();
        let title = tab.title().to_string();
        let snapshot = match (stored, &self.store) {
            (Some(stored), Some(store)) => store.take(stored).ok(),
            _ => None,
        };
        if let (Some(snapshot), Some(entry)) = (snapshot, &entry) {
            if let Ok(page) = engine::restore(snapshot, settings, &self.fonts) {
                let title = self.show(page, entry.scroll);
                self.set_tab_title(title);
                if let Some(node) = self.tab_mut().focused_link.take() {
                    self.focus_link(node, false);
                    self.render();
                }
                self.page_is_entry = true;
                self.save_session();
                return;
            }
        }
        match entry {
            Some(entry) => {
                let index = self.tab().index;
                self.navigate(entry.url, Destination::Entry(index));
            }
            None => {
                // A new tab: the palette over an empty pane (DESIGN.md, section 7).
                self.page = None;
                self.page_is_entry = false;
                self.scroll = 0.0;
                self.window.set_title(&title);
                self.open_palette();
                self.save_session();
                self.render();
            }
        }
    }

    fn select_tab(&mut self, i: usize) -> bool {
        let p = &self.profiles[self.profile];
        if i >= p.tabs.len() || i == p.active {
            return false;
        }
        self.sleep();
        self.profiles[self.profile].active = i;
        self.wake();
        true
    }

    fn new_tab(&mut self) -> bool {
        self.sleep();
        let p = &mut self.profiles[self.profile];
        p.tabs.push(Tab::default());
        p.active = p.tabs.len() - 1;
        self.wake();
        true
    }

    /// Close the visible tab. The last tab of a profile is replaced by a new one.
    fn close_tab(&mut self) -> bool {
        self.stop_load();
        self.save_scroll();
        self.page = None;
        self.page_is_entry = false;
        self.field = None;
        self.focus = None;
        let p = &mut self.profiles[self.profile];
        let tab = p.tabs.remove(p.active);
        if !tab.history.is_empty() {
            if p.closed.len() >= CLOSED_LIMIT {
                p.closed.remove(0);
            }
            p.closed.push(Closed {
                history: tab.history,
                index: tab.index,
                title: tab.title,
                position: p.active,
            });
        }
        if p.tabs.is_empty() {
            p.tabs.push(Tab::default());
        }
        p.active = p.active.min(p.tabs.len() - 1);
        self.wake();
        true
    }

    fn reopen_tab(&mut self) -> bool {
        if self.profiles[self.profile].closed.is_empty() {
            return false;
        }
        self.sleep();
        let p = &mut self.profiles[self.profile];
        let closed = p.closed.pop().expect("checked above");
        let at = closed.position.min(p.tabs.len());
        p.tabs.insert(
            at,
            Tab {
                history: closed.history,
                index: closed.index,
                title: closed.title,
                ..Tab::default()
            },
        );
        p.active = at;
        self.wake();
        true
    }

    fn switch_profile(&mut self) -> bool {
        let next = (self.profile + 1) % self.profiles.len();
        let active = self.profiles[next].active;
        self.show_tab(next, active)
    }

    /// Show tab `index` of `profile`, switching profile if needed.
    fn show_tab(&mut self, profile: usize, index: usize) -> bool {
        if profile == self.profile {
            return self.select_tab(index);
        }
        self.sleep();
        // Nothing a server could use to link the two profiles carries over.
        net::forget_sessions();
        self.profile = profile;
        let p = &mut self.profiles[profile];
        p.active = index.min(p.tabs.len() - 1);
        self.wake();
        true
    }

    /// The visible tab's title, and its current history entry's.
    fn set_tab_title(&mut self, title: String) {
        let tab = self.tab_mut();
        let index = tab.index;
        if let Some(entry) = tab.history.get_mut(index) {
            entry.title.clone_from(&title);
        }
        tab.title = title;
    }

    /// Write the visible profile's persistent cookies if they changed (atomically, readable by
    /// this user only). Session cookies are never written.
    fn save_cookies(&self) {
        let profile = &self.profiles[self.profile];
        if !profile.cookies.take_changed() {
            return;
        }
        let Some(path) = profile_file(self.session.as_deref(), profile.name, "cookies") else {
            return;
        };
        if let Err(e) = write_private(&path, profile.cookies.save().as_bytes()) {
            self.window
                .set_title(&format!("Broxee: couldn't save cookies ({e})"));
        }
    }

    /// Write the tabs and their history (not their pages) so the next start brings them back.
    fn save_session(&self) {
        let Some(path) = &self.session else {
            return;
        };
        let text = write_session(&self.profiles, self.profile);
        // History is private: the file is this user's only, like the cookies.
        if let Err(e) = write_private(path, text.as_bytes()) {
            self.window
                .set_title(&format!("Broxee: couldn't save the session ({e})"));
        }
    }

    /// A page Broxee writes itself, without a history entry.
    fn show_message(&mut self, title: &str, detail: &str) {
        self.stop_load();
        let url = net::Url::parse("file:///").expect("a file URL always parses");
        let html = message_page(title, detail, None, &self.chrome);
        let page = engine::page_from_html(url, &html, self.settings(), &self.fonts);
        self.show(page, 0.0);
    }

    /// Show `page` in the pane; returns its title.
    fn show(&mut self, page: Loaded, scroll: f32) -> String {
        let title = page.dom.title().unwrap_or_else(|| host_of(&page.url));
        self.window.set_title(&title);
        self.page_is_entry = false;
        self.page = Some(page);
        self.link_focus = None;
        self.loading = None;
        self.scroll = scroll;
        self.clamp_scroll();
        self.painter.trim(GLYPH_CACHE_LIMIT);
        self.painter.forget_images();
        // Loading freed a lot (the previous page, stylesheet text, parse scratch): hand it back
        // to the system before the frame that needs a second surface.
        #[cfg(target_os = "macos")]
        platform::release_free_memory();
        self.render();
        title
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
        let fonts = Arc::clone(&self.fonts);
        self.render_with(&fonts);
    }

    fn render_with(&mut self, fonts: &FontStore) {
        let scale = self.scale;
        let width_px = (self.width * scale).round().max(1.0) as usize;
        let height_px = (self.height * scale).round().max(1.0) as usize;
        let header_rows = ((HEADER_HEIGHT * scale).round() as usize).min(height_px);
        let page_rows = height_px - header_rows;
        let sidebar_px = ((self.pane_left() * scale).round() as usize).min(width_px - 1);
        let pane_px = width_px - sidebar_px;

        let profile = &self.profiles[self.profile];
        let tab = &profile.tabs[profile.active];
        let host = tab.entry().map(|e| host_of(&e.url)).unwrap_or_default();
        let header = HeaderView {
            profile: profile.name,
            hue: profile.hue,
            can_back: tab.index > 0,
            can_forward: tab.index + 1 < tab.history.len(),
            loading: self.loading,
            host: &host,
            field: self.field.as_ref().map(|f| FieldView {
                text: &f.text,
                cursor: f.cursor,
                all_selected: f.all_selected,
            }),
            focused: self.focus,
        };
        let list = sidebar_rows(profile, self.list, self.bookmark_row);
        let sidebar = (sidebar_px > 0).then_some(SidebarView {
            profile: profile.name,
            hue: profile.hue,
            rows: &list.rows,
            selected: list.selected,
            footer: list.footer,
            empty: list.empty,
        });
        let dialog_fields: Vec<(&str, &str, usize)> =
            self.dialog.as_ref().map_or(Vec::new(), |d| {
                vec![
                    ("Name", d.fields[0].text.as_str(), d.fields[0].cursor),
                    ("Folder", d.fields[1].text.as_str(), d.fields[1].cursor),
                ]
            });
        let dialog_view = self.dialog.as_ref().map(|d| DialogView {
            heading: if d.existing.is_some() {
                "Edit bookmark"
            } else {
                "Bookmark this page"
            },
            fields: &dialog_fields,
            active: d.active,
            help: "Enter to save · Tab for the other field · Esc to cancel",
        });
        let palette_rows: Vec<PaletteRow> = self.palette.as_ref().map_or(Vec::new(), |p| {
            p.items
                .iter()
                .skip(p.first)
                .take(chrome::palette_capacity(self.height))
                .map(|item| PaletteRow {
                    title: &item.title,
                    detail: &item.detail,
                    hint: match &item.hint {
                        Hint::Label(label) => HintView::Label(label),
                        Hint::Keys(keys) => HintView::Keys(keys),
                    },
                })
                .collect()
        });
        let nothing = self
            .palette
            .as_ref()
            .filter(|p| p.items.is_empty())
            .map(|p| format!("Nothing matches \"{}\".", p.query.text.trim()));
        let palette_view = self.palette.as_ref().map(|p| PaletteView {
            query: &p.query.text,
            cursor: p.query.cursor,
            rows: &palette_rows,
            selected: (!p.items.is_empty()).then(|| p.selected - p.first),
            // Honest until a search engine is chosen (DESIGN.md asks for web search).
            empty: nothing
                .as_deref()
                .map(|line| (line, "Broxee can't search the web yet.")),
        });
        // The focused link's rectangles in page-area points.
        let ring: Vec<(f32, f32, f32, f32)> = self.link_focus.as_ref().map_or(Vec::new(), |l| {
            l.rects
                .iter()
                .map(|(r, fixed)| {
                    let y = if *fixed { r.y } else { r.y - self.scroll };
                    (r.x, y, r.width, r.height)
                })
                .collect()
        });
        let view = self.page.as_ref().map(|p| engine::page_view(p, fonts));
        let viewport = Viewport {
            width: pane_px as f32 / scale,
            height: page_rows as f32 / scale,
            scale,
            scroll_y: self.scroll,
        };
        let (painter, chrome) = (&mut self.painter, &mut self.chrome);
        let blank = chrome.theme().surface;
        let shown = self
            .window
            .render(width_px, height_px, &mut |pixels, stride| {
                if let Some(sidebar) = &sidebar {
                    let frame = Frame {
                        pixels: &mut *pixels,
                        stride,
                        left: 0,
                        width: sidebar_px,
                        scale,
                    };
                    chrome.draw_sidebar(painter, frame, fonts.set(), fonts.metrics(), sidebar);
                }
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
                                left: sidebar_px,
                            },
                        );
                    }
                    _ => chrome::fill(bottom, stride, sidebar_px, pane_px, blank),
                }
                if !ring.is_empty() && page_rows > 0 {
                    let frame = Frame {
                        pixels: &mut *bottom,
                        stride,
                        left: sidebar_px,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_focus_ring(painter, frame, &ring);
                }
                let frame = Frame {
                    pixels: top,
                    stride,
                    left: sidebar_px,
                    width: pane_px,
                    scale,
                };
                chrome.draw(painter, frame, fonts.set(), fonts.metrics(), &header);
                if let Some(dialog) = &dialog_view {
                    let frame = Frame {
                        pixels: &mut *pixels,
                        stride,
                        left: sidebar_px,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_dialog(painter, frame, fonts.set(), fonts.metrics(), dialog);
                }
                if let Some(palette) = &palette_view {
                    let frame = Frame {
                        pixels: &mut *pixels,
                        stride,
                        left: sidebar_px,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_palette(painter, frame, fonts.set(), fonts.metrics(), palette);
                }
            });
        if let Err(e) = shown {
            self.window.set_title(&format!("Broxee: {e}"));
        }
    }
}

/// The sidebar's rows for the current list, what each stands for, and its footer and
/// empty-state text.
fn sidebar_rows(
    profile: &Profile,
    list: SidebarList,
    bookmark_row: Option<usize>,
) -> SidebarRows<'_> {
    match list {
        SidebarList::Tabs => SidebarRows {
            rows: profile
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| ListRow::Item {
                    title: t.title(),
                    muted: i != profile.active,
                    closable: i == profile.active,
                })
                .collect(),
            targets: (0..profile.tabs.len()).map(RowTarget::Tab).collect(),
            selected: Some(profile.active),
            footer: Some("+ New tab"),
            empty: None,
        },
        SidebarList::Bookmarks => {
            // Folders in name order; bookmarks in the order they were saved.
            let mut folders: Vec<&str> = profile
                .bookmarks
                .iter()
                .map(|b| b.folder.as_str())
                .collect();
            folders.sort_by_key(|f| f.to_lowercase());
            folders.dedup();
            let mut rows = Vec::new();
            let mut targets = Vec::new();
            for folder in folders {
                rows.push(ListRow::Label(folder));
                targets.push(RowTarget::Folder);
                for (i, b) in profile.bookmarks.iter().enumerate() {
                    if b.folder == folder {
                        rows.push(ListRow::Item {
                            title: &b.title,
                            muted: false,
                            closable: false,
                        });
                        targets.push(RowTarget::Bookmark(i));
                    }
                }
            }
            SidebarRows {
                rows,
                targets,
                selected: bookmark_row,
                footer: None,
                empty: Some(("No bookmarks yet.", "Press ⌘D on any page to save it.")),
            }
        }
    }
}

/// The sidebar's list as rows, ready to draw or hit-test.
struct SidebarRows<'a> {
    rows: Vec<ListRow<'a>>,
    targets: Vec<RowTarget>,
    selected: Option<usize>,
    footer: Option<&'static str>,
    empty: Option<(&'static str, &'static str)>,
}

/// Where a profile keeps `file` (its cookies, its bookmarks): beside the session file, one
/// folder per profile.
fn profile_file(session: Option<&std::path::Path>, profile: &str, file: &str) -> Option<PathBuf> {
    Some(session?.parent()?.join("profiles").join(profile).join(file))
}

/// Bookmarks as text: the last folder used, then one bookmark per line (folder, URL, title).
fn write_bookmarks(bookmarks: &[Bookmark], last_folder: &str) -> String {
    let mut out = format!("broxee-bookmarks 1\nlast\t{last_folder}\n");
    for b in bookmarks {
        out += &format!("mark\t{}\t{}\t{}\n", b.folder, b.url, b.title);
    }
    out
}

/// Read `write_bookmarks` text; lines that do not parse are skipped.
fn parse_bookmarks(text: &str) -> (Vec<Bookmark>, Option<String>) {
    let mut lines = text.lines();
    if lines.next() != Some("broxee-bookmarks 1") {
        return (Vec::new(), None);
    }
    let (mut bookmarks, mut last) = (Vec::new(), None);
    for line in lines {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        match fields.as_slice() {
            ["last", folder] if !folder.trim().is_empty() => last = Some(folder.to_string()),
            ["mark", folder, url, title] if bookmarks.len() < BOOKMARK_LIMIT => {
                if let Ok(url) = net::Url::parse(url) {
                    let folder = if folder.trim().is_empty() {
                        DEFAULT_FOLDER
                    } else {
                        folder
                    };
                    bookmarks.push(Bookmark {
                        title: title.to_string(),
                        url,
                        folder: folder.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    (bookmarks, last)
}

/// Write `data` to `path` through a temporary file and a rename, readable by this user only.
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(data)?;
    std::fs::rename(&tmp, path)
}

/// The session as text: one line per profile, tab and history entry, tab-separated. Pages
/// are not saved; restored tabs reload when selected.
fn write_session(profiles: &[Profile], visible: usize) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = format!("broxee-session 1\nvisible\t{visible}\n");
    for profile in profiles {
        out += &format!("profile\t{}\t{}\n", profile.active, profile.name);
        for tab in &profile.tabs {
            out += &format!("tab\t{}\t{}\n", tab.index, clean(&tab.title));
            for entry in &tab.history {
                out += &format!(
                    "entry\t{}\t{}\t{}\n",
                    entry.scroll,
                    entry.url,
                    clean(&entry.title)
                );
            }
        }
    }
    out
}

type SessionProfile = (Vec<Tab>, usize);

/// Read a session written by `write_session`; anything malformed is skipped, never trusted.
fn parse_session(text: &str) -> Option<(Vec<SessionProfile>, usize)> {
    let mut lines = text.lines();
    if lines.next()? != "broxee-session 1" {
        return None;
    }
    let mut visible = 0;
    let mut profiles: Vec<SessionProfile> = Vec::new();
    for line in lines {
        let mut parts = line.splitn(3, '\t');
        let (kind, a, b) = (parts.next()?, parts.next(), parts.next());
        let number = a.and_then(|a| a.parse::<usize>().ok()).unwrap_or(0);
        match kind {
            "visible" => visible = number,
            "profile" => profiles.push((Vec::new(), number)),
            "tab" => {
                if let Some((tabs, _)) = profiles.last_mut().filter(|p| p.0.len() < SESSION_TABS) {
                    tabs.push(Tab {
                        index: number,
                        title: b.unwrap_or_default().to_string(),
                        ..Tab::default()
                    });
                }
            }
            "entry" => {
                let tab = profiles.last_mut().and_then(|p| p.0.last_mut());
                // "url" or, since titles are kept, "url<tab>title".
                let (url, title) = match b.map(|b| b.split_once('\t')) {
                    Some(Some((url, title))) => (Some(url), title),
                    _ => (b, ""),
                };
                let url = url.and_then(|u| net::Url::parse(u).ok());
                let scroll = a
                    .and_then(|a| a.parse::<f32>().ok())
                    .filter(|s| s.is_finite());
                if let (Some(tab), Some(url)) = (tab, url) {
                    if tab.history.len() < SESSION_ENTRIES {
                        tab.history.push(Entry {
                            url,
                            scroll: scroll.unwrap_or(0.0).max(0.0),
                            title: title.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    for (tabs, _) in &mut profiles {
        for tab in tabs.iter_mut() {
            tab.index = tab.index.min(tab.history.len().saturating_sub(1));
        }
    }
    Some((profiles, visible))
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

/// The links Tab can reach, in document order: `<a href>` elements that are visible and have
/// something laid out.
fn focusable_links(page: &Loaded) -> Vec<NodeId> {
    let (dom, layout) = (&page.dom, &page.layout);
    let mut shown = vec![false; dom.len()];
    for t in &layout.texts {
        shown[t.node as usize] = true;
    }
    for b in &layout.boxes {
        shown[b.node as usize] = true;
    }
    dom.descendants(html::Dom::ROOT)
        .filter(|&id| {
            dom.tag_name(id) == Some("a")
                && dom.attr(id, "href").is_some()
                && {
                    let style = page.styles.style(id);
                    style.visible && !style.transparent
                }
                && dom.descendants(id).any(|d| shown[d as usize])
        })
        .collect()
}

/// Where link `node` is drawn: its boxes and text runs, with runs on one line joined.
fn link_rects(
    page: &Loaded,
    node: NodeId,
    metrics: &dyn font::TextMetrics,
) -> Vec<(layout::Rect, bool)> {
    let layout = &page.layout;
    let inside: std::collections::HashSet<NodeId> = page.dom.descendants(node).collect();
    let mut rects: Vec<(layout::Rect, bool)> = Vec::new();
    for (i, b) in layout.boxes.iter().enumerate() {
        if inside.contains(&b.node) {
            rects.push((b.rect, layout.box_is_fixed(i)));
        }
    }
    for (i, t) in layout.texts.iter().enumerate() {
        if !inside.contains(&t.node) {
            continue;
        }
        let style = page.styles.style(t.node);
        if !style.visible {
            continue;
        }
        let size = style.font_size.to_f32();
        let top = t.baseline - metrics.ascent_em() * size;
        let rect = layout::Rect {
            x: t.x,
            y: top,
            width: t.width,
            height: t.baseline + metrics.descent_em() * size - top,
        };
        let fixed = layout.text_is_fixed(i);
        // Join with the previous run when it is on the same line and close by.
        match rects.last_mut() {
            Some((last, last_fixed))
                if *last_fixed == fixed
                    && (last.y - rect.y).abs() < 0.5
                    && (last.height - rect.height).abs() < 0.5
                    && rect.x >= last.x
                    && rect.x <= last.x + last.width + size =>
            {
                last.width = rect.x + rect.width - last.x;
            }
            _ => rects.push((rect, fixed)),
        }
    }
    rects
}

/// Typed text to a URL: full URLs as they are, `example.com` as https.
fn parse_input(input: &str) -> Result<net::Url, String> {
    let input = input.trim();
    engine::several_addresses(input)?;
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
        let path = url.decoded_path();
        path.rsplit('/').next().unwrap_or_default().to_string()
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

    /// An in-memory screen: 640 x 300 points at 1x (a 400-point pane beside the sidebar), rows
    /// padded to exercise the stride.
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
            (640.0, 300.0)
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
        browser_with(target, None)
    }

    fn browser_with(target: &Path, session: Option<PathBuf>) -> Browser<FakeScreen> {
        let root = std::env::temp_dir().join(format!("broxee-tabs-{}", std::process::id()));
        let store = TabStore::open(&root).expect("a folder for sleeping tabs");
        let opts = Options {
            target: target.display().to_string(),
            more: Vec::new(),
            tree: false,
            styles: false,
            layout: false,
            png: None,
            paint: false,
            window: true,
            depth: 8,
            width: 640.0,
            height: 300.0,
            scale: 1.0,
            fetch_css: false,
        };
        let mut b = Browser::new(opts, FakeScreen::default(), session, Some(store));
        b.start();
        b.wait_for_load();
        b
    }

    /// Apply an event and wait for any page it started loading, as a person would before
    /// looking at the result.
    fn act(b: &mut Browser<FakeScreen>, event: Event) -> bool {
        let dirty = b.handle(event);
        b.wait_for_load();
        dirty
    }

    fn key(key: Key) -> Event {
        Event::Key(KeyEvent {
            key,
            command: false,
            shift: false,
            option: false,
        })
    }

    fn command(c: char) -> Event {
        Event::Key(KeyEvent {
            key: Key::Char(c),
            command: true,
            shift: false,
            option: false,
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
            SIDEBAR_WIDTH + run.x + run.width / 2.0,
            run.baseline - 4.0 - b.scroll + HEADER_HEIGHT,
        );
        act(
            b,
            Event::Click {
                x: f64::from(x),
                y: f64::from(y),
            },
        )
    }

    #[test]
    fn follows_links_and_goes_back_and_forward() {
        let (a, _) = site("links");
        let mut b = browser(&a);
        assert_eq!(title(&b), "Page A");
        assert!(click_text(&mut b, "Go to B"));
        assert_eq!(title(&b), "Page B");
        assert_eq!((b.tab().history.len(), b.tab().index), (2, 1));
        assert!(act(&mut b, command('[')));
        assert_eq!(title(&b), "Page A");
        assert!(act(&mut b, command(']')));
        assert_eq!(title(&b), "Page B");
        assert!(
            !act(&mut b, command(']')),
            "nothing forward of the last page"
        );
        assert!(click_text(&mut b, "Back to A"));
        assert_eq!(
            (b.tab().history.len(), b.tab().index),
            (3, 2),
            "a new page after going back keeps the trail"
        );
    }

    #[test]
    fn scrolls_within_the_page_and_restores_position_on_back() {
        let (a, bpath) = site("scroll");
        let mut b = browser(&a);
        assert!(act(&mut b, key(Key::PageDown)));
        let saved = b.scroll;
        assert!(saved > 0.0);
        assert!(act(&mut b, key(Key::End)));
        let bottom = b.scroll;
        assert!(!act(&mut b, key(Key::Down)), "already at the bottom");
        assert!((bottom - b.max_scroll()).abs() < 0.01);
        assert!(act(&mut b, Event::Scroll { dy: 10_000.0 }));
        assert_eq!(b.scroll, 0.0, "scrolling up past the top stops at 0");
        act(&mut b, key(Key::PageDown));

        // Open B through the address field, then come back to A at the same position.
        assert!(act(&mut b, command('l')));
        assert!(b.field.as_ref().is_some_and(|f| f.all_selected));
        for c in format!("file://{}", bpath.display()).chars() {
            act(&mut b, key(Key::Char(c)));
        }
        act(&mut b, key(Key::Enter));
        assert_eq!(title(&b), "Page B");
        assert_eq!(b.scroll, 0.0);
        act(&mut b, command('['));
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
                option: false,
            })
        };
        // Only one page: back and forward are disabled, so Tab lands on reload first.
        assert!(act(&mut b, tab(false)));
        assert_eq!(b.focus, Some(Button::Reload));
        assert!(act(&mut b, tab(false)));
        assert!(b.field.is_some(), "then the address field");
        assert!(act(&mut b, tab(false)));
        assert!(b.link_focus.is_some(), "then the page's first link");
        assert!(act(&mut b, tab(true)));
        assert!(b.field.is_some(), "Shift+Tab goes backwards");

        click_text(&mut b, "Go to B");
        assert_eq!(b.focus, None);
        act(&mut b, tab(false));
        assert_eq!(b.focus, Some(Button::Back));
        b.render();
        assert!(act(&mut b, key(Key::Enter)));
        assert_eq!(
            title(&b),
            "Page A",
            "Enter on the focused back button goes back"
        );
        assert!(act(&mut b, key(Key::Escape)) || b.focus.is_none());
    }

    #[test]
    fn tab_reaches_page_links_scrolls_to_them_and_enter_follows() {
        let dir = std::env::temp_dir().join(format!("broxee-focus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("links.html");
        std::fs::write(
            &page,
            format!(
                "<title>Links</title><style>body {{ margin: 0 }} p {{ margin: 0; line-height: 20px }} \
                 .gone {{ display: none }} #ghost {{ visibility: hidden }}</style>\
                 <p><a href=one.html>First</a> <a class=gone href=x.html>Gone</a> \
                 <a id=ghost href=y.html>Ghost</a> <a name=anchor>No href</a></p>{}\
                 <p><a href=two.html>Far down</a></p>",
                "<p>line</p>".repeat(60)
            ),
        )
        .unwrap();
        std::fs::write(dir.join("one.html"), "<title>One</title>").unwrap();
        std::fs::write(dir.join("two.html"), "<title>Two</title>").unwrap();
        let mut b = browser(&page);
        let tab = |shift| {
            Event::Key(KeyEvent {
                key: Key::Tab,
                command: false,
                shift,
                option: false,
            })
        };
        let focused = |b: &Browser<FakeScreen>| {
            let node = b.link_focus.as_ref()?.node;
            let page = b.page.as_ref()?;
            Some(page.dom.text_content(node))
        };
        act(&mut b, tab(false)); // reload
        act(&mut b, tab(false)); // address field
        act(&mut b, tab(false));
        assert_eq!(focused(&b).as_deref(), Some("First"));
        assert_eq!(b.scroll, 0.0);

        // The focus ring: 2px of accent, 2px outside the link's text.
        b.render();
        let rect = b.link_focus.as_ref().unwrap().rects[0].0;
        let accent = b.chrome.theme().accent;
        let ring_x = (SIDEBAR_WIDTH + rect.x + rect.width) as usize + 3;
        let ring_y = (HEADER_HEIGHT + rect.y + rect.height / 2.0) as usize;
        assert_eq!(
            b.window.pixel(ring_x, ring_y),
            [accent.b, accent.g, accent.r, 255]
        );

        // Hidden links and anchors without href are skipped; the next one is scrolled to.
        act(&mut b, tab(false));
        assert_eq!(focused(&b).as_deref(), Some("Far down"));
        assert!(b.scroll > 0.0, "scrolled into view");
        let far = b.link_focus.as_ref().unwrap().rects[0].0;
        assert!(far.y >= b.scroll && far.y + far.height <= b.scroll + b.page_height());

        // Wrapping goes back to the header; Shift+Tab comes back.
        act(&mut b, tab(false));
        assert!(b.link_focus.is_none() && b.focus.is_some());
        act(&mut b, tab(true));
        assert_eq!(focused(&b).as_deref(), Some("Far down"));

        // The focus comes back with a sleeping tab; Enter follows the link.
        act(&mut b, command('t'));
        act(&mut b, key(Key::Escape));
        act(&mut b, command('1'));
        assert_eq!(
            focused(&b).as_deref(),
            Some("Far down"),
            "restored on waking"
        );
        assert!(act(&mut b, key(Key::Enter)));
        assert_eq!(title(&b), "Two");
        assert!(b.link_focus.is_none(), "a new page starts without focus");
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
        act(&mut b, key(Key::PageDown));
        assert!(b.scroll > 100.0);
        // The pinned link is still at the top of the page area after scrolling.
        let layout = &b.page.as_ref().unwrap().layout;
        let pinned = (0..layout.texts.len())
            .find(|&i| layout.text(&layout.texts[i]) == "Pinned")
            .unwrap();
        let t = layout.texts[pinned];
        assert!(layout.text_is_fixed(pinned));
        let (x, y) = (SIDEBAR_WIDTH + t.x + 2.0, t.baseline - 4.0 + HEADER_HEIGHT);
        assert!(act(
            &mut b,
            Event::Click {
                x: f64::from(x),
                y: f64::from(y)
            }
        ));
        assert_eq!(title(&b), "Target");

        act(&mut b, command('['));
        act(&mut b, key(Key::Home));
        let layout = &b.page.as_ref().unwrap().layout;
        let ghost = (0..layout.texts.len())
            .find(|&i| layout.text(&layout.texts[i]) == "Ghost")
            .unwrap();
        let t = layout.texts[ghost];
        act(
            &mut b,
            Event::Click {
                x: f64::from(SIDEBAR_WIDTH + t.x + 2.0),
                y: f64::from(t.baseline - 4.0 + HEADER_HEIGHT),
            },
        );
        assert_eq!(title(&b), "Fixed", "a hidden link does not navigate");
    }

    #[test]
    fn shows_error_and_message_pages() {
        let (a, _) = site("errors");
        let mut b = browser(&a);
        act(&mut b, command('l'));
        act(
            &mut b,
            Event::Paste("file:///no/such/broxee/page.html\nsecond line".into()),
        );
        assert_eq!(
            b.field.as_ref().map(|f| f.text.as_str()),
            Some("file:///no/such/broxee/page.html")
        );
        act(&mut b, key(Key::Enter));
        assert_eq!(title(&b), "Couldn't reach page.html");
        assert!(b
            .page
            .as_ref()
            .unwrap()
            .dom
            .text_content(html::Dom::ROOT)
            .contains("Try again"));

        act(&mut b, command('l'));
        for c in "what is rust".chars() {
            act(&mut b, key(Key::Char(c)));
        }
        act(&mut b, key(Key::Enter));
        assert_eq!(title(&b), "Broxee can't search yet");
    }

    #[test]
    fn draws_the_header_and_relays_out_on_resize() {
        let (a, _) = site("render");
        let mut b = browser(&a);
        b.render();
        let theme = b.chrome.theme();
        let bgra = |c: css::Color| [c.b, c.g, c.r, 255];
        let edge = theme.profile(Hue::Emerald);
        assert_eq!(
            b.window.pixel(440, 0),
            bgra(edge),
            "profile edge over the pane"
        );
        assert_eq!(b.window.pixel(100, 1), bgra(edge), "and over the switcher");
        assert_eq!(b.window.pixel(440, 2), bgra(theme.surface));
        assert_eq!(b.window.pixel(440, 35), bgra(theme.line), "bottom hairline");
        assert_eq!(
            b.window.pixel(239, 200),
            bgra(theme.line),
            "sidebar hairline"
        );
        assert_eq!(b.window.pixel(4, 280), bgra(theme.canvas), "sidebar");
        assert_eq!(
            b.window.pixel(639, 200),
            [255, 255, 255, 255],
            "page canvas below the header"
        );
        assert!(act(
            &mut b,
            Event::Resize {
                width: 490.0,
                height: 300.0,
                scale: 1.0
            }
        ));
        assert_eq!(b.page.as_ref().unwrap().layout.width, 250.0);
        assert!(act(&mut b, command('\\')), "⌘\\ hides the sidebar");
        assert_eq!(b.page.as_ref().unwrap().layout.width, 490.0);
    }

    fn command_shift(c: char) -> Event {
        Event::Key(KeyEvent {
            key: Key::Char(c),
            command: true,
            shift: true,
            option: false,
        })
    }

    fn open(b: &mut Browser<FakeScreen>, path: &Path) {
        act(b, command('l'));
        act(b, Event::Paste(format!("file://{}", path.display())));
        act(b, key(Key::Enter));
    }

    fn tab_titles(b: &Browser<FakeScreen>) -> Vec<String> {
        let p = &b.profiles[b.profile];
        p.tabs.iter().map(|t| t.title().to_string()).collect()
    }

    #[test]
    fn sleeping_tabs_wake_from_their_snapshot_without_the_network() {
        let (a, bpath) = site("sleep");
        let mut b = browser(&a);
        act(&mut b, key(Key::PageDown));
        let saved = b.scroll;
        assert!(saved > 0.0);

        assert!(act(&mut b, command('t')));
        assert!(b.page.is_none(), "a new tab is empty");
        assert!(b.palette.is_some(), "with the palette open");
        assert_eq!(tab_titles(&b), ["Page A", "New tab"]);
        open(&mut b, &bpath);
        assert_eq!(title(&b), "Page B");
        assert!(
            b.profiles[0].tabs[0].snapshot.is_some(),
            "A sleeps as a snapshot"
        );

        // With A's file gone, only the snapshot can bring it back.
        std::fs::remove_file(&a).unwrap();
        assert!(act(&mut b, command('1')));
        assert_eq!(title(&b), "Page A");
        assert_eq!(b.scroll, saved, "at the same scroll position");
        assert!(b.profiles[0].tabs[1].snapshot.is_some());
        assert!(
            b.profiles[0].tabs[0].snapshot.is_none(),
            "the visible tab is awake"
        );
        assert!(!act(&mut b, command('1')), "already selected");

        // Clicking the second row in the sidebar selects it.
        assert!(act(&mut b, Event::Click { x: 100.0, y: 127.0 }));
        assert_eq!(title(&b), "Page B");
        act(&mut b, command('1'));
        assert!(act(&mut b, command('9')), "⌘9 is the last tab");
        assert_eq!(b.profiles[0].active, 1);
    }

    #[test]
    fn snapshots_past_the_budget_fall_back_to_the_network() {
        let (a, bpath) = site("budget");
        let mut b = browser(&a);
        b.snapshot_budget = 0;
        act(&mut b, command('t'));
        open(&mut b, &bpath);
        assert!(b.profiles[0].tabs[0].snapshot.is_none());
        assert!(act(&mut b, command('1')));
        assert_eq!(title(&b), "Page A", "reloaded from its URL");
    }

    #[test]
    fn closes_and_reopens_tabs() {
        let (a, bpath) = site("close");
        let mut b = browser(&a);
        act(&mut b, command('t'));
        open(&mut b, &bpath);
        assert!(act(&mut b, command('w')));
        assert_eq!(tab_titles(&b), ["Page A"]);
        assert_eq!(title(&b), "Page A");
        assert!(act(&mut b, command_shift('t')));
        assert_eq!(tab_titles(&b), ["Page A", "Page B"]);
        assert_eq!((b.profiles[0].active, title(&b).as_str()), (1, "Page B"));
        assert!(!act(&mut b, command_shift('t')), "nothing left to reopen");

        // The close button on the active row.
        assert!(act(&mut b, Event::Click { x: 220.0, y: 127.0 }));
        assert!(act(&mut b, command('w')));
        assert_eq!(
            tab_titles(&b),
            ["New tab"],
            "the last tab becomes a new tab"
        );
        assert!(b.page.is_none());
        assert!(act(&mut b, command_shift('t')));
        assert_eq!(tab_titles(&b), ["Page A", "New tab"]);
    }

    #[test]
    fn profiles_keep_their_own_tabs() {
        let (a, bpath) = site("profiles");
        let mut b = browser(&a);
        assert!(act(&mut b, command_shift('p')));
        assert_eq!(b.profiles[b.profile].name, "Work");
        assert_eq!(tab_titles(&b), ["New tab"]);
        assert_eq!(title(&b), "New tab");
        b.render();
        let theme = b.chrome.theme();
        let amber = theme.profile(Hue::Amber);
        assert_eq!(b.window.pixel(440, 0), [amber.b, amber.g, amber.r, 255]);
        open(&mut b, &bpath);
        assert_eq!(title(&b), "Page B");

        assert!(act(&mut b, command_shift('p')));
        assert_eq!(tab_titles(&b), ["Page A"]);
        assert_eq!(title(&b), "Page A");
        assert!(
            act(&mut b, Event::Click { x: 100.0, y: 10.0 }),
            "the switcher"
        );
        assert_eq!(title(&b), "Page B");
    }

    #[test]
    fn restores_the_session_with_tabs_asleep() {
        let (a, bpath) = site("session");
        let path = a.with_file_name("session");
        let _ = std::fs::remove_file(&path);
        let mut b = browser_with(&a, Some(path.clone()));
        act(&mut b, command('t'));
        open(&mut b, &bpath);
        act(&mut b, key(Key::Down));
        act(&mut b, command_shift('p'));
        act(&mut b, command_shift('p'));
        drop(b);

        let b = browser_with(Path::new(""), Some(path.clone()));
        assert_eq!(tab_titles(&b), ["Page A", "Page B"]);
        assert_eq!(b.profiles[0].active, 1);
        assert_eq!(title(&b), "Page B", "only the visible tab loads");
        assert!(b.profiles[0].tabs[0].snapshot.is_none());
        assert_eq!(b.profiles[1].tabs.len(), 1);

        // Opening an address the session already has selects its tab.
        let b = browser_with(&a, Some(path));
        assert_eq!(tab_titles(&b).len(), 2);
        assert_eq!((b.profiles[0].active, title(&b).as_str()), (0, "Page A"));
    }

    #[test]
    fn reads_damaged_sessions_safely() {
        assert!(parse_session("").is_none());
        assert!(parse_session("something else\nvisible\t1").is_none());
        let (profiles, visible) = parse_session(
            "broxee-session 1\nvisible\t7\nentry\t0\thttps://orphan.example/\n\
             profile\t0\tPersonal\ntab\t9\tKept\nentry\tNaN\thttps://a.example/\n\
             entry\t5\tnot a url\nbogus line\n",
        )
        .unwrap();
        assert_eq!(visible, 7, "clamped by the caller");
        let tab = &profiles[0].0[0];
        assert_eq!(
            (tab.title.as_str(), tab.index, tab.history.len()),
            ("Kept", 0, 1)
        );
        assert_eq!(tab.history[0].scroll, 0.0);

        let mut personal = Profile::new("Personal", Hue::Emerald);
        personal.tabs[0].title = "A\ttab\nname".into();
        personal.tabs[0].history.push(Entry {
            url: net::Url::parse("https://a.example/x").unwrap(),
            scroll: 12.5,
            title: "Page\tX".into(),
        });
        let text = write_session(&[personal], 0);
        let (profiles, _) = parse_session(&text).unwrap();
        let tab = &profiles[0].0[0];
        assert_eq!(tab.title, "A tab name");
        assert_eq!(tab.history[0].scroll, 12.5);
        assert_eq!(tab.history[0].title, "Page X");
    }

    #[test]
    fn a_new_page_never_reuses_the_last_pages_images() {
        let dir = std::env::temp_dir().join(format!("broxee-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Two pages, each with one 20x20 image at the same size: both are image 0.
        let page = |name: &str, rgb: [u8; 3]| {
            let pixels: Vec<u8> = (0..400)
                .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
                .collect();
            let png: String = paint::png::encode(20, 20, &pixels)
                .iter()
                .map(|b| format!("%{b:02X}"))
                .collect();
            let path = dir.join(name);
            std::fs::write(
                &path,
                format!("<style>body {{ margin: 0 }}</style><img src=\"data:image/png,{png}\">"),
            )
            .unwrap();
            path
        };
        let (red, blue) = (
            page("red.html", [255, 0, 0]),
            page("blue.html", [0, 0, 255]),
        );
        let mut b = browser(&red);
        b.opts.fetch_css = true;
        act(&mut b, command('r'));
        let at = (SIDEBAR_WIDTH as usize + 10, HEADER_HEIGHT as usize + 10);
        assert_eq!(b.window.pixel(at.0, at.1), [0, 0, 255, 255], "red, as BGRA");
        open(&mut b, &blue);
        assert_eq!(
            b.window.pixel(at.0, at.1),
            [255, 0, 0, 255],
            "blue, not the cached red"
        );
    }

    /// A local HTTP server that holds each response until `release` sends a message or is
    /// dropped: a slow site, without the network.
    fn slow_server(title: &'static str) -> (String, mpsc::Sender<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = wait.recv();
                let body = format!("<title>{title}</title><p>slow");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{address}/"), release)
    }

    /// Start loading `url` from the address field without waiting for it.
    fn start_loading(b: &mut Browser<FakeScreen>, url: &str) {
        act(b, command('l'));
        act(b, Event::Paste(url.to_string()));
        b.handle(key(Key::Enter));
        assert!(b.load.is_some() && b.loading.is_some(), "loading {url}");
    }

    #[test]
    fn the_window_answers_while_a_page_loads() {
        let (a, _) = site("async");
        let (slow, release) = slow_server("Slow");
        let mut b = browser(&a);

        // A new tab while a page loads: answered at once, and the load is dropped.
        start_loading(&mut b, &slow);
        let started = std::time::Instant::now();
        assert!(b.handle(command('t')));
        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(b.load.is_none() && b.loading.is_none());
        assert_eq!(tab_titles(&b), ["Page A", "New tab"]);
        act(&mut b, command('w'));
        assert_eq!(title(&b), "Page A");

        // Esc stops a load and keeps the page and history as they were.
        start_loading(&mut b, &slow);
        assert!(b.handle(key(Key::Escape)));
        assert!(b.load.is_none() && b.loading.is_none());
        assert_eq!(title(&b), "Page A");
        assert_eq!(b.tab().history.len(), 1);
        assert!(!b.handle(key(Key::Escape)), "nothing left to stop");

        // So does the reload button, which is a stop button while loading.
        start_loading(&mut b, &slow);
        assert!(b.handle(Event::Click {
            x: f64::from(SIDEBAR_WIDTH + 80.0),
            y: 18.0
        }));
        assert!(b.load.is_none());
        assert_eq!(title(&b), "Page A");

        // Following a link while a page loads replaces that load.
        start_loading(&mut b, &slow);
        assert!(click_text(&mut b, "Go to B"));
        assert_eq!(title(&b), "Page B");
        assert_eq!(b.tab().history.len(), 2);

        // Going back while a page loads cancels it and goes back.
        start_loading(&mut b, &slow);
        assert!(act(&mut b, command('[')));
        assert_eq!((title(&b).as_str(), b.tab().index), ("Page A", 0));

        // Switching profile while loading answers at once too.
        start_loading(&mut b, &slow);
        assert!(b.handle(command_shift('p')));
        assert!(b.load.is_none());
        assert_eq!(b.profiles[b.profile].name, "Work");
        drop(release);
    }

    #[test]
    fn a_page_loading_in_the_background_arrives_in_its_tab() {
        let (a, _) = site("arrive");
        let (slow, release) = slow_server("Slow page");
        let mut b = browser(&a);
        start_loading(&mut b, &slow);
        // Scrolling the current page still works meanwhile.
        assert!(b.handle(key(Key::PageDown)));
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while b.load.is_some() && std::time::Instant::now() < deadline {
            b.poll_load();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(title(&b), "Slow page");
        assert_eq!(b.tab().history.len(), 2);
        assert_eq!(b.scroll, 0.0, "a new page starts at the top");
        assert!(b.loading.is_none());
    }

    fn type_text(b: &mut Browser<FakeScreen>, text: &str) {
        for c in text.chars() {
            act(b, key(Key::Char(c)));
        }
    }

    fn results(b: &Browser<FakeScreen>) -> Vec<String> {
        let p = b.palette.as_ref().expect("the palette is open");
        p.items.iter().map(|i| i.title.clone()).collect()
    }

    #[test]
    fn the_palette_finds_tabs_in_every_profile_history_and_commands() {
        let (a, bpath) = site("palette");
        let mut b = browser(&a);
        // Work profile: one tab showing B. Personal: A, with B in its history.
        act(&mut b, command_shift('p'));
        act(&mut b, key(Key::Escape));
        open(&mut b, &bpath);
        act(&mut b, command_shift('p'));
        assert_eq!(title(&b), "Page A");
        assert!(click_text(&mut b, "Go to B"));
        act(&mut b, command('['));

        assert!(act(&mut b, command('k')));
        let all = results(&b);
        assert_eq!(
            all[0], "Page B",
            "the other profile's tab first: Personal has no other tab"
        );
        assert!(
            all.contains(&"Reopen closed tab".to_string()),
            "commands are listed"
        );
        let p = b.palette.as_ref().unwrap();
        assert_eq!(p.items[0].hint, Hint::Label("Tab · Work".into()));
        assert!(p
            .items
            .iter()
            .any(|i| i.hint == Hint::Label("History".into())));

        // Typing narrows; Enter on another profile's tab switches to it.
        type_text(&mut b, "page b");
        assert_eq!(results(&b)[0], "Page B");
        assert!(act(&mut b, key(Key::Enter)));
        assert!(b.palette.is_none());
        assert_eq!(b.profiles[b.profile].name, "Work");
        assert_eq!(title(&b), "Page B");

        // Commands run from the palette, with their shortcut shown.
        act(&mut b, command('k'));
        type_text(&mut b, "switch prof");
        let p = b.palette.as_ref().unwrap();
        assert_eq!(p.items[0].hint, Hint::Keys("⌘⇧P"));
        act(&mut b, key(Key::Enter));
        assert_eq!(b.profiles[b.profile].name, "Personal");

        // History stays within the profile: Work's history is not offered here.
        act(&mut b, command('k'));
        let history: Vec<_> = b
            .palette
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|i| i.hint == Hint::Label("History".into()))
            .map(|i| i.title.clone())
            .collect();
        assert_eq!(history, ["Page B", "Page A"], "most recent first");
        assert!(act(&mut b, key(Key::Escape)));
        assert!(b.palette.is_none());
    }

    #[test]
    fn the_palette_opens_typed_addresses_and_says_when_nothing_matches() {
        let (a, bpath) = site("palette-open");
        let mut b = browser(&a);
        act(&mut b, command('k'));
        type_text(&mut b, "zzqx");
        assert!(results(&b).is_empty());
        b.render();
        assert!(!act(&mut b, key(Key::Enter)), "nothing to open");
        assert!(b.palette.is_some());

        for _ in 0..4 {
            act(&mut b, key(Key::Backspace));
        }
        act(&mut b, Event::Paste(format!("file://{}", bpath.display())));
        assert!(results(&b)[0].starts_with("Open file://"));
        act(&mut b, key(Key::Enter));
        assert_eq!(title(&b), "Page B");
        assert_eq!(b.tab().history.len(), 2);

        // A new tab opens with the palette; Esc leaves an empty tab.
        act(&mut b, command('t'));
        assert!(b.palette.is_some());
        act(&mut b, key(Key::Escape));
        assert!(b.palette.is_none() && b.page.is_none());
        // ⌘K toggles; another shortcut closes it and runs.
        act(&mut b, command('k'));
        assert!(act(&mut b, command('k')));
        assert!(b.palette.is_none());
        act(&mut b, command('k'));
        act(&mut b, command('l'));
        assert!(b.palette.is_none() && b.field.is_some());
    }

    #[test]
    fn the_palette_is_drawn_and_answers_the_mouse() {
        let (a, _) = site("palette-mouse");
        let mut b = browser(&a);
        // The sidebar's search field opens it.
        assert!(act(&mut b, Event::Click { x: 100.0, y: 58.0 }));
        assert!(b.palette.is_some());
        b.render();
        let theme = b.chrome.theme();
        // Pane 400 points wide, window 300 tall: 368 wide from x = 16, top 48, four rows fit
        // (bottom at 48 + 57 + 176 = 281).
        assert_eq!(chrome::palette_capacity(300.0), 4);
        let (left, top) = (SIDEBAR_WIDTH as usize + 16, 48);
        assert_eq!(
            b.window.pixel(left + 100, top + 30),
            [theme.surface.b, theme.surface.g, theme.surface.r, 255],
            "the input row"
        );
        let shadow = b.window.pixel(left + 100, 287);
        assert!(
            shadow[0] < 255 && shadow[0] > 200,
            "a soft shadow under it: {shadow:?}"
        );

        // Results are rows; clicking the second runs it.
        type_text(&mut b, "tab");
        let second = results(&b)[1].clone();
        assert!(act(&mut b, key(Key::Down)));
        let y = 48.0 + 44.0 + 1.0 + 6.0 + 44.0 + 20.0;
        let x = f64::from(SIDEBAR_WIDTH + 100.0);
        assert_eq!(second, "Close tab");
        assert!(act(&mut b, Event::Click { x, y }));
        assert_eq!(tab_titles(&b), ["New tab"], "Close tab ran");
        assert!(
            b.palette.is_some(),
            "the new tab that replaced it opens the palette"
        );

        // A click outside closes it without doing anything else.
        assert!(act(
            &mut b,
            Event::Click {
                x: f64::from(SIDEBAR_WIDTH + 5.0),
                y: 280.0
            }
        ));
        assert!(b.palette.is_none());
    }

    /// A local HTTP server that sets `id=<n>` (persistent) and `s=1` (session) on its first
    /// visit from a jar without them, and titles each page with the Cookie header it received.
    fn cookie_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 4096];
                let len = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..len]).into_owned();
                let cookie = request
                    .lines()
                    .find_map(|l| l.strip_prefix("Cookie: "))
                    .unwrap_or("none")
                    .to_string();
                let set = if cookie.contains("id=") {
                    String::new()
                } else {
                    format!("Set-Cookie: id={n}; Max-Age=3600\r\nSet-Cookie: s=1\r\n")
                };
                let body = format!("<title>{cookie}</title>");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n{set}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{address}/")
    }

    fn visit(b: &mut Browser<FakeScreen>, url: &str) {
        act(b, command('l'));
        act(b, Event::Paste(url.to_string()));
        act(b, key(Key::Enter));
    }

    #[test]
    fn cookies_stay_in_their_profile_and_survive_a_restart() {
        let (a, _) = site("cookies");
        let server = cookie_server();
        let session = a.with_file_name("cookie-session");
        let _ = std::fs::remove_dir_all(a.with_file_name("profiles"));
        let mut b = browser_with(&a, Some(session.clone()));

        visit(&mut b, &server);
        assert_eq!(title(&b), "none", "first visit: nothing to send");
        act(&mut b, command('r'));
        let personal = title(&b);
        assert!(
            personal.starts_with("id=") && personal.ends_with("; s=1"),
            "{personal}"
        );

        // Work has its own jar.
        act(&mut b, command_shift('p'));
        act(&mut b, key(Key::Escape));
        visit(&mut b, &server);
        assert_eq!(title(&b), "none", "Personal's cookies do not reach Work");
        act(&mut b, command('r'));
        let work = title(&b);
        assert!(
            work.starts_with("id=") && work != personal,
            "{work} vs {personal}"
        );

        // Saved per profile, private to this user; session cookies are not saved.
        let file = a
            .with_file_name("profiles")
            .join("Personal")
            .join("cookies");
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(
            saved.contains("id\t") && !saved.contains("s\t1\t"),
            "{saved}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(b);

        // After a restart the persistent cookie comes back; the session one does not.
        let mut b = browser_with(Path::new(""), Some(session.clone()));
        act(&mut b, command_shift('p'));
        if b.profiles[b.profile].name != "Personal" {
            act(&mut b, command_shift('p'));
        }
        visit(&mut b, &server);
        assert_eq!(title(&b), personal.trim_end_matches("; s=1"));

        // Clearing works from the palette, for this profile only.
        act(&mut b, command('k'));
        type_text(&mut b, "clear this");
        act(&mut b, key(Key::Enter));
        assert!(b.profiles[b.profile].cookies.is_empty());
        assert!(!b.profiles[1 - b.profile].cookies.is_empty());
    }

    fn command_option(c: char) -> Event {
        Event::Key(KeyEvent {
            key: Key::Char(c),
            command: true,
            shift: false,
            option: true,
        })
    }

    #[test]
    fn bookmarks_are_saved_in_folders_found_and_opened() {
        let (a, _) = site("bookmarks");
        let session = a.with_file_name("bookmark-session");
        let _ = std::fs::remove_dir_all(a.with_file_name("profiles"));
        let mut b = browser_with(&a, Some(session.clone()));

        // ⌘D on page A: its title, the default folder; Enter saves.
        assert!(act(&mut b, command('d')));
        let d = b.dialog.as_ref().unwrap();
        assert_eq!(
            (d.fields[0].text.as_str(), d.fields[1].text.as_str()),
            ("Page A", "Bookmarks")
        );
        b.render();
        act(&mut b, key(Key::Enter));
        assert!(b.dialog.is_none());

        // Page B, renamed, in a new folder: typing replaces the field Tab reaches.
        click_text(&mut b, "Go to B");
        act(&mut b, command('d'));
        type_text(&mut b, "Second page");
        act(&mut b, key(Key::Tab));
        type_text(&mut b, "Reading");
        act(&mut b, key(Key::Enter));
        let p = &b.profiles[0];
        assert_eq!(p.bookmarks.len(), 2);
        assert_eq!(
            (
                p.bookmarks[1].title.as_str(),
                p.bookmarks[1].folder.as_str()
            ),
            ("Second page", "Reading")
        );
        assert_eq!(p.last_folder, "Reading");

        // ⌘D on a saved page edits it; Esc leaves it as it was.
        act(&mut b, command('d'));
        assert_eq!(b.dialog.as_ref().unwrap().existing, Some(1));
        type_text(&mut b, "x");
        act(&mut b, key(Key::Escape));
        assert_eq!(b.profiles[0].bookmarks[1].title, "Second page");

        // ⌘⌥B: folders in name order, as labels; ↓ Enter opens a bookmark.
        assert!(act(&mut b, command_option('b')));
        let list = b.sidebar_rows();
        let rows: Vec<String> = list
            .rows
            .iter()
            .map(|r| match r {
                ListRow::Label(l) => format!("[{l}]"),
                ListRow::Item { title, .. } => title.to_string(),
            })
            .collect();
        assert_eq!(rows, ["[Bookmarks]", "Page A", "[Reading]", "Second page"]);
        b.render();
        act(&mut b, key(Key::Down));
        assert_eq!(b.bookmark_row, Some(1));
        act(&mut b, key(Key::Enter));
        assert_eq!(title(&b), "Page A");
        // A click on a row opens it too (rows start at 82, 30 points each).
        assert!(act(
            &mut b,
            Event::Click {
                x: 100.0,
                y: 82.0 + 30.0 * 3.0 + 15.0
            }
        ));
        assert_eq!(title(&b), "Page B");
        act(&mut b, key(Key::Escape));
        assert_eq!(b.list, SidebarList::Tabs);

        // The palette finds bookmarks, with their folder.
        act(&mut b, command('k'));
        type_text(&mut b, "second");
        let p = b.palette.as_ref().unwrap();
        assert_eq!(p.items[0].hint, Hint::Label("Bookmark · Reading".into()));
        act(&mut b, key(Key::Escape));

        // Work has none, and says how to add one.
        act(&mut b, command_shift('p'));
        act(&mut b, key(Key::Escape));
        act(&mut b, command_option('b'));
        assert!(b.sidebar_rows().rows.is_empty() && b.sidebar_rows().empty.is_some());
        b.render();
        act(&mut b, command_option('b'));
        act(&mut b, command_shift('p'));

        // Saved per profile, private, and back after a restart; removing works too.
        let file = a
            .with_file_name("profiles")
            .join("Personal")
            .join("bookmarks");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(b);
        let mut b = browser_with(Path::new(""), Some(session));
        assert_eq!(b.profiles[0].bookmarks.len(), 2);
        assert_eq!(b.profiles[0].last_folder, "Reading");
        if b.profiles[b.profile].name != "Personal" {
            act(&mut b, command_shift('p'));
        }
        act(&mut b, command('k'));
        type_text(&mut b, "remove this");
        act(&mut b, key(Key::Enter));
        assert_eq!(b.profiles[0].bookmarks.len(), 1);
    }

    #[test]
    fn reads_damaged_bookmark_files_safely() {
        assert_eq!(parse_bookmarks(""), (Vec::new(), None));
        let (marks, last) = parse_bookmarks(
            "broxee-bookmarks 1\nlast\tWork stuff\nmark\t\thttps://a.example/\tA\n\
             mark\tF\tnot a url\tB\nmark\tshort\nnonsense\nmark\tF\thttps://c.example/\tC\twith tab\n",
        );
        assert_eq!(last.as_deref(), Some("Work stuff"));
        assert_eq!(marks.len(), 2);
        assert_eq!(
            marks[0].folder, DEFAULT_FOLDER,
            "an empty folder becomes the default"
        );
        assert_eq!(marks[1].title, "C\twith tab");
        let text = write_bookmarks(&marks, "F");
        assert_eq!(parse_bookmarks(&text).0, marks);
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
        assert!(parse_input("https://a.com/x https://b.com/").is_err());
        assert_eq!(
            parse_input("https://a.com/my page").unwrap().to_string(),
            "https://a.com/my%20page"
        );
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
