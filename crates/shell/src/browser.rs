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

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use font::TextMetrics;
use html::NodeId;
use paint::{Painter, Target, Viewport};
use platform::{Event, Key, KeyEvent};

use crate::chrome::{
    self, Button, Chrome, DialogView, FieldView, Frame, HeaderView, HintView, Hue, ListRow,
    MenuView, PaletteHit, PaletteRow, PaletteView, SidebarItem, SidebarView, HEADER_HEIGHT,
    SIDEBAR_MAX, SIDEBAR_MIN, SIDEBAR_WIDTH,
};
use crate::engine::{self, FontStore, Loaded, Settings};
use crate::forms;
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
/// A tooltip shows after the pointer rests this long on something that has one.
const TOOLTIP_DELAY: Duration = Duration::from_millis(600);
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
    /// `draw` also gets the previous frame when it has the same layout, to copy from.
    fn render(&self, width: usize, height: usize, draw: &mut DrawFrame) -> Result<(), String>;
    /// Paint only the header strip: `width` x `height` device pixels at (`left`, `top`),
    /// shown over the last frame. `Ok(false)` when the screen cannot, and a full frame is
    /// needed instead.
    fn render_header(
        &self,
        _at: (usize, usize),
        _width: usize,
        _height: usize,
        _draw: &mut dyn FnMut(&mut [u8], usize),
    ) -> Result<bool, String> {
        Ok(false)
    }
    /// Nothing has happened for a while: free what only drawing needs.
    fn idle(&self) {}
    /// Put text on the clipboard.
    fn copy(&self, _text: &str) {}
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

    fn copy(&self, text: &str) {
        self.copy_text(text);
    }

    fn render(&self, width: usize, height: usize, draw: &mut DrawFrame) -> Result<(), String> {
        platform::Window::render(self, width, height, |pixels, stride, previous| {
            draw(pixels, stride, previous)
        })
    }

    fn render_header(
        &self,
        at: (usize, usize),
        width: usize,
        height: usize,
        draw: &mut dyn FnMut(&mut [u8], usize),
    ) -> Result<bool, String> {
        platform::Window::render_header(self, at, width, height, |pixels, stride| {
            draw(pixels, stride)
        })?;
        Ok(true)
    }
}

/// Paints a frame: its pixels and stride, and the previous frame to copy from, if any.
type DrawFrame<'a> = dyn FnMut(&mut [u8], usize, Option<&[u8]>) + 'a;

/// Frame width and height, header rows, sidebar edge (device pixels), and pane count.
type Geometry = (usize, usize, usize, usize, usize);

/// How much of the window a change needs repainted. Ordered: combining changes keeps the
/// larger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Repaint {
    Nothing,
    /// Only the pane header (the address field, the loading line).
    Header,
    Frame,
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
struct Tab {
    /// Names the tab for the panes of a split, whatever its position in the list.
    id: u64,
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
    /// Kept at the top of the list.
    pinned: bool,
    /// The tab group it belongs to, by name.
    group: Option<String>,
}

impl Default for Tab {
    fn default() -> Tab {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Tab {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            history: Vec::new(),
            index: 0,
            title: String::new(),
            snapshot: None,
            seen: 0,
            focused_link: None,
            pinned: false,
            group: None,
        }
    }
}

/// One pane of a split. The focused pane shows the visible tab, live; the others keep the
/// pixels they had when focus left them (copied from frame to frame), so they cost no memory.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pane {
    profile: usize,
    tab: u64,
    /// Its area of the last frame holds its page (it was focused since the layout changed).
    painted: bool,
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

/// The ⋯ menu while it is open.
struct Menu {
    items: Vec<(String, MenuAction)>,
    /// Chosen with ↑ ↓; none when opened with the mouse.
    selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuAction {
    CopyAddress,
    /// Open this page in a new tab of profile `n`.
    OpenIn(usize),
    Bookmark,
    Reload,
    CloseTab,
}

/// The pointer resting on something with a tooltip.
struct Hover {
    at: (f32, f32),
    text: String,
    since: std::time::Instant,
    shown: bool,
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

/// A small dialog over the pane: ⌘D's bookmark (name, folder) or a tab's group (name).
struct Dialog {
    kind: DialogKind,
    fields: Vec<Field>,
    active: usize,
    /// Why Enter did not go through, shown in place of the help line.
    error: Option<String>,
}

enum DialogKind {
    Bookmark {
        url: net::Url,
        /// Editing this bookmark rather than adding one.
        existing: Option<usize>,
    },
    /// Move the visible tab to a group; an empty name takes it out of its group.
    Group,
    NewProfile,
    RenameProfile,
    /// Remove the visible profile once its name is typed.
    RemoveProfile,
}

/// A press in the sidebar that may become a drag.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// On a tab row; it becomes a move once the pointer travels a few points.
    Pressed { tab: usize, y: f32 },
    /// A tab being moved; it lands before row `drop` of the list.
    Moving { tab: usize, drop: usize },
    /// The sidebar's edge, resizing it.
    Resizing,
}

/// What the sidebar lists.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SidebarList {
    Tabs,
    Bookmarks,
}

/// What a sidebar row stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RowTarget {
    Tab(usize),
    Bookmark(usize),
    Folder,
    /// A section label of the tab list.
    Section(Section),
}

/// Where a tab sits in the list.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Section {
    Pinned,
    /// The group of tab `n` (its first tab).
    Group(usize),
    Rest,
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
    /// Shown everywhere the profile is named; the person can change it.
    name: String,
    /// The profile's folder under `profiles/`; fixed, so renaming moves nothing.
    id: String,
    hue: Hue,
    tabs: Vec<Tab>,
    active: usize,
    closed: Vec<Closed>,
    /// This profile's cookies: logins in one profile never reach the other.
    cookies: Arc<net::Cookies>,
    bookmarks: Vec<Bookmark>,
    /// Preselected in ⌘D's dialog (DESIGN.md, section 7).
    last_folder: String,
    /// Tab groups shown folded, by name.
    folded: Vec<String>,
}

impl Profile {
    /// Read this profile's cookies and bookmarks from its folder.
    fn load_files(&mut self, session: Option<&std::path::Path>) {
        let read = |file| {
            profile_file(session, &self.id, file)
                .and_then(|path| std::fs::read_to_string(path).ok())
        };
        if let Some(text) = read("cookies") {
            self.cookies = Arc::new(net::Cookies::load(&text));
        }
        if let Some(text) = read("bookmarks") {
            let (bookmarks, last) = parse_bookmarks(&text);
            self.bookmarks = bookmarks;
            if let Some(last) = last {
                self.last_folder = last;
            }
        }
    }

    /// Keep tabs in the order the sidebar shows them: pinned tabs, then each group (in the
    /// order groups first appear), then the rest. The sort is stable and the active tab stays
    /// active, so `⌘1` … `⌘9` follow what is on screen.
    fn normalize(&mut self) {
        let mut groups: Vec<String> = Vec::new();
        for tab in &self.tabs {
            if let Some(g) = tab.group.as_ref().filter(|_| !tab.pinned) {
                if !groups.contains(g) {
                    groups.push(g.clone());
                }
            }
        }
        let key = |tab: &Tab| match (&tab.group, tab.pinned) {
            (_, true) => 0,
            (Some(g), false) => 1 + groups.iter().position(|x| x == g).unwrap_or(0),
            (None, false) => usize::MAX,
        };
        let mut order: Vec<usize> = (0..self.tabs.len()).collect();
        order.sort_by_key(|&i| key(&self.tabs[i]));
        let active = order.iter().position(|&i| i == self.active).unwrap_or(0);
        let mut old: Vec<Option<Tab>> = std::mem::take(&mut self.tabs)
            .into_iter()
            .map(Some)
            .collect();
        self.tabs = order.iter().filter_map(|&i| old[i].take()).collect();
        self.active = active;
        self.folded.retain(|f| groups.contains(f));
    }
}

impl Profile {
    fn new(id: &str, name: &str, hue: Hue) -> Profile {
        Profile {
            name: name.to_string(),
            id: id.to_string(),
            hue,
            tabs: vec![Tab::default()],
            active: 0,
            closed: Vec::new(),
            cookies: Arc::new(net::Cookies::new()),
            bookmarks: Vec::new(),
            last_folder: DEFAULT_FOLDER.into(),
            folded: Vec::new(),
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
    /// The session read at startup, applied by `start`.
    restored: Option<Session>,
    /// Where sleeping tabs go; without it they keep only their history.
    store: Option<TabStore>,
    field: Option<Field>,
    palette: Option<Palette>,
    dialog: Option<Dialog>,
    menu: Option<Menu>,
    hover: Option<Hover>,
    /// Sidebar width in points, 200 to 320.
    sidebar_width: f32,
    /// How far the sidebar's list is scrolled.
    sidebar_scroll: f32,
    drag: Option<Drag>,
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
    /// The text of the focused form field, with its caret. The control itself is
    /// `link_focus.node`.
    editing: Option<Field>,
    /// One, two or four panes, and which has focus (and the live page).
    panes: Vec<Pane>,
    focused: usize,
    /// Set by an event handler whose change shows only in the header.
    header_change: bool,
    /// Window size, header rows, sidebar edge and pane count of the last full frame: a
    /// header alone, or a pane's old pixels, can be reused only while these hold.
    frame_geometry: Option<Geometry>,
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
        // The profiles of the last session, or Personal and Work; their files are read now,
        // their tabs in `start`.
        let restored = session
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| parse_session(&text))
            .filter(|s| !s.profiles.is_empty());
        let mut profiles: Vec<Profile> = match &restored {
            Some(session) => session
                .profiles
                .iter()
                .map(|p| Profile::new(&p.id, &p.name, p.hue))
                .collect(),
            None => vec![
                Profile::new("Personal", "Personal", Hue::Emerald),
                Profile::new("Work", "Work", Hue::Amber),
            ],
        };
        for profile in &mut profiles {
            profile.load_files(session.as_deref());
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
            restored,
            session,
            store,
            field: None,
            palette: None,
            dialog: None,
            menu: None,
            hover: None,
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_scroll: 0.0,
            drag: None,
            list: SidebarList::Tabs,
            bookmark_row: None,
            focus: None,
            link_focus: None,
            loading: None,
            load: None,
            editing: None,
            panes: vec![Pane {
                profile: 0,
                tab: 0,
                painted: false,
            }],
            focused: 0,
            header_change: false,
            frame_geometry: None,
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

    /// Right edge of the sidebar (0 when hidden), in points: where the panes begin.
    fn sidebar_edge(&self) -> f32 {
        if self.sidebar {
            self.sidebar_width.min(self.width)
        } else {
            0.0
        }
    }

    /// Each pane's x, y, width and height in points: one, two side by side, or a 2x2 grid,
    /// with a 1-point hairline between them.
    fn pane_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let x0 = self.sidebar_edge();
        let (w, h) = ((self.width - x0).max(1.0), self.height.max(1.0));
        let half = |len: f32| ((len - 1.0) / 2.0).floor().max(1.0);
        let (a, b) = (half(w), half(h));
        match self.panes.len() {
            2 => vec![
                (x0, 0.0, a, h),
                (x0 + a + 1.0, 0.0, (w - a - 1.0).max(1.0), h),
            ],
            4 => {
                let (rw, bh) = ((w - a - 1.0).max(1.0), (h - b - 1.0).max(1.0));
                vec![
                    (x0, 0.0, a, b),
                    (x0 + a + 1.0, 0.0, rw, b),
                    (x0, b + 1.0, a, bh),
                    (x0 + a + 1.0, b + 1.0, rw, bh),
                ]
            }
            _ => vec![(x0, 0.0, w, h)],
        }
    }

    /// The focused pane.
    fn pane_rect(&self) -> (f32, f32, f32, f32) {
        let rects = self.pane_rects();
        rects[self.focused.min(rects.len() - 1)]
    }

    /// Left edge of the focused pane, in points.
    fn pane_left(&self) -> f32 {
        self.pane_rect().0
    }

    fn pane_top(&self) -> f32 {
        self.pane_rect().1
    }

    fn pane_width(&self) -> f32 {
        self.pane_rect().2.max(1.0)
    }

    fn pane_height(&self) -> f32 {
        self.pane_rect().3.max(1.0)
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
        (self.pane_height() - HEADER_HEIGHT).max(1.0)
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
            let mut wait = if self.load.is_some() {
                LOAD_POLL
            } else {
                IDLE_AFTER
            };
            // Wake when a waiting tooltip is due, or a script timer.
            if let Some(h) = self.hover.as_ref().filter(|h| !h.shown) {
                wait = wait.min(TOOLTIP_DELAY.saturating_sub(h.since.elapsed()));
            }
            if let Some(due) = self.next_page_timer() {
                wait = wait.min(due);
            }
            let mut repaint = Repaint::Nothing;
            let mut quiet = false;
            match events.recv_timeout(wait) {
                Ok(event) => {
                    repaint = repaint.max(self.apply(event));
                    for event in events.try_iter() {
                        repaint = repaint.max(self.apply(event));
                    }
                }
                Err(RecvTimeoutError::Timeout) => quiet = true,
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.poll_load() {
                // Progress moves the loading line; a finished load painted its page already.
                repaint = repaint.max(Repaint::Header);
            }
            if self.tick() || self.run_page_timers() {
                repaint = Repaint::Frame;
            }
            if repaint != Repaint::Nothing {
                self.repaint(repaint);
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
        if let Some(session) = self.restored.take() {
            for (profile, saved) in self.profiles.iter_mut().zip(session.profiles) {
                if !saved.tabs.is_empty() {
                    profile.active = saved.active.min(saved.tabs.len() - 1);
                    profile.tabs = saved.tabs;
                    profile.folded = saved.folded;
                    profile.normalize();
                }
            }
            self.profile = session.visible.min(self.profiles.len() - 1);
            if let Some((width, shown)) = session.sidebar {
                self.sidebar_width = width;
                self.sidebar = shown;
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
        if self.opts.panes > 1 {
            self.wait_for_load();
            self.set_split(self.opts.panes);
            self.render();
            // Show each pane once, last to first, so every one has its page on screen.
            for j in (0..self.panes.len()).rev() {
                self.focus_pane(j);
                self.wait_for_load();
                self.render();
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
    /// Handle `event` and say what needs repainting.
    fn apply(&mut self, event: Event) -> Repaint {
        self.header_change = false;
        match (self.handle(event), self.header_change) {
            (false, _) => Repaint::Nothing,
            (true, true) => Repaint::Header,
            (true, false) => Repaint::Frame,
        }
    }

    fn repaint(&mut self, repaint: Repaint) {
        match repaint {
            Repaint::Nothing => {}
            Repaint::Header => self.render_header(),
            Repaint::Frame => self.render(),
        }
    }

    fn handle(&mut self, event: Event) -> bool {
        let event = match self.route(event) {
            Ok(event) => event,
            Err(dirty) => return dirty,
        };
        if let Event::Hover { x, y } = event {
            return self.hover_at(x as f32, y as f32);
        }
        let from_key = matches!(event, Event::Key(_));
        let focus_before = self.link_focus.as_ref().map(|l| l.node);
        // Anything else hides a tooltip, which needs a full frame.
        let hid = self.hover.take().is_some_and(|h| h.shown);
        let dirty = match event {
            Event::Scroll { dy, x, .. } if (x as f32) < self.sidebar_edge() => {
                let rows = self.sidebar_rows();
                let max =
                    chrome::sidebar_max_scroll(rows.rows.len(), rows.footer.is_some(), self.height);
                let before = self.sidebar_scroll;
                self.sidebar_scroll = (self.sidebar_scroll - dy as f32).clamp(0.0, max);
                self.sidebar_scroll != before
            }
            Event::Scroll { dy, .. } => self.scroll_by(-(dy as f32)),
            Event::Drag { x, y } => self.drag_to(x as f32, y as f32),
            Event::Release { x, y } => self.release(x as f32, y as f32),
            Event::Hover { .. } => false,
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
                let control = self.focused_control();
                if let (Some(field), Some(i)) = (self.editing.as_mut(), control) {
                    field.insert(line);
                    let text = field.text.clone();
                    if let Some(page) = self.page.as_mut() {
                        page.forms.set_value(&mut page.dom, i, &text);
                    }
                    self.relayout_page();
                    return true;
                }
                match self.field.as_mut() {
                    Some(field) => {
                        field.insert(line);
                        self.header_change = true;
                        true
                    }
                    None => false,
                }
            }
        };
        if hid {
            self.header_change = false;
        }
        let restyled = self.sync_focus(focus_before, from_key);
        hid | dirty | restyled
    }

    /// Change the page's interactive states and style it again when a rule can see the
    /// change (`StyleMap::state_change_matters`). Returns whether it was styled again.
    fn set_states(&mut self, change: impl FnOnce(&mut css::States)) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        let mut new = page.states;
        change(&mut new);
        if new == page.states {
            return false;
        }
        let matters = page
            .styles
            .state_change_matters(&page.dom, &page.states, &new);
        page.states = new;
        if matters {
            self.relayout_page();
        }
        matters
    }

    /// After an event: `:focus` follows the focused link or control. Focus is visible
    /// (`:focus-visible`) when the keyboard moved it, or when it is in a text field.
    fn sync_focus(&mut self, before: Option<NodeId>, from_key: bool) -> bool {
        let focus = self.link_focus.as_ref().map(|l| l.node);
        let text_field = self
            .focused_control()
            .zip(self.page.as_ref())
            .is_some_and(|(i, page)| page.forms.is_text(i));
        let moved = focus != before;
        self.set_states(|s| {
            if moved || s.focus != focus {
                s.focus_visible = focus.is_some() && (from_key || text_field);
            }
            s.focus = focus;
        })
    }

    /// The page node under the pointer, for `:hover` and `:active`: only looked for when
    /// some rule uses that state, since it means hit-testing every move.
    fn state_target(&self, x: f32, y: f32, state: u8) -> Option<NodeId> {
        let page = self.page.as_ref()?;
        if page.styles.states_used() & state == 0 {
            return None;
        }
        if (self.sidebar && x < self.sidebar_edge() + 1.0) || y < HEADER_HEIGHT {
            return None;
        }
        if self.menu.is_some() || self.palette.is_some() || self.dialog.is_some() {
            return None;
        }
        self.node_at(x - self.pane_left(), y - HEADER_HEIGHT)
    }

    /// Pointer events over the panes. In the focused pane, y becomes relative to its top, so
    /// everything below works as with one pane; a click on another pane focuses it, and other
    /// events there are dropped (`Err` says whether to repaint).
    fn route(&mut self, event: Event) -> Result<Event, bool> {
        let (x, y) = match event {
            Event::Click { x, y }
            | Event::Hover { x, y }
            | Event::Drag { x, y }
            | Event::Release { x, y }
            | Event::Scroll { x, y, .. } => (x as f32, y as f32),
            _ => return Ok(event),
        };
        // The sidebar, and drags that started there, keep window coordinates.
        if self.drag.is_some() || x < self.sidebar_edge() {
            return Ok(event);
        }
        let rects = self.pane_rects();
        let inside = |r: &(f32, f32, f32, f32)| {
            x >= r.0 && x < r.0 + r.2 + 1.0 && y >= r.1 && y < r.1 + r.3 + 1.0
        };
        let Some(j) = rects.iter().position(inside) else {
            return Ok(event);
        };
        if j != self.focused.min(rects.len() - 1) {
            return Err(match event {
                Event::Click { .. } => self.focus_pane(j),
                Event::Hover { .. } => self.hover.take().is_some_and(|h| h.shown),
                _ => false,
            });
        }
        let top = f64::from(rects[j].1);
        Ok(match event {
            Event::Click { x, y } => Event::Click { x, y: y - top },
            Event::Hover { x, y } => Event::Hover { x, y: y - top },
            Event::Drag { x, y } => Event::Drag { x, y: y - top },
            Event::Release { x, y } => Event::Release { x, y: y - top },
            Event::Scroll { dy, x, y } => Event::Scroll { dy, x, y: y - top },
            other => other,
        })
    }

    /// Record which tab the focused pane shows.
    fn sync_pane(&mut self) {
        let p = &self.profiles[self.profile];
        let tab = p.tabs[p.active].id;
        let f = self.focused.min(self.panes.len() - 1);
        self.panes[f].profile = self.profile;
        self.panes[f].tab = tab;
    }

    /// Give pane `j` focus: the focused tab sleeps (its pixels stay on screen) and pane
    /// `j`'s tab wakes at that pane's size.
    fn focus_pane(&mut self, j: usize) -> bool {
        if j >= self.panes.len() || j == self.focused {
            return false;
        }
        self.sync_pane();
        let current = self.geometry();
        self.panes[self.focused].painted = self.frame_geometry == Some(current);
        self.menu = None;
        self.dialog = None;
        if self.palette.is_some() {
            self.close_palette();
        }
        self.hover = None;
        self.focused = j;
        let pane = self.panes[j];
        let profile = pane.profile.min(self.profiles.len() - 1);
        let p = &self.profiles[profile];
        let index = p
            .tabs
            .iter()
            .position(|t| t.id == pane.tab)
            .unwrap_or(p.active);
        if profile == self.profile && index == p.active {
            // Both panes show this tab: lay it out for its new size.
            self.relayout();
        } else {
            self.show_tab(profile, index);
        }
        true
    }

    /// Show one, two or four panes (⌘⇧1, ⌘⇧2, ⌘⇧4). The focused pane comes first and keeps
    /// focus; new panes show the profile's next tabs that are in no pane, or new tabs.
    fn set_split(&mut self, n: usize) -> bool {
        if n == self.panes.len() || !matches!(n, 1 | 2 | 4) {
            return false;
        }
        self.sync_pane();
        let focused = self.panes[self.focused];
        let mut panes = vec![focused];
        panes.extend(
            self.panes
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != self.focused)
                .map(|(_, p)| *p),
        );
        panes.truncate(n);
        let profile = self.profile;
        while panes.len() < n {
            let shown: Vec<u64> = panes
                .iter()
                .filter(|q| q.profile == profile)
                .map(|q| q.tab)
                .collect();
            let p = &mut self.profiles[profile];
            let count = p.tabs.len();
            let next = (1..=count)
                .map(|k| &p.tabs[(p.active + k) % count])
                .find(|t| !shown.contains(&t.id))
                .map(|t| t.id);
            let tab = match next {
                Some(id) => id,
                None => {
                    let t = Tab::default();
                    let id = t.id;
                    p.tabs.push(t);
                    id
                }
            };
            panes.push(Pane {
                profile,
                tab,
                painted: false,
            });
        }
        for pane in &mut panes {
            pane.painted = false;
        }
        self.panes = panes;
        self.focused = 0;
        self.menu = None;
        self.relayout();
        self.save_session();
        true
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
        if let Some(menu) = &self.menu {
            let hit = (x >= left)
                .then(|| chrome::menu_hit(x - left, y, pane_width, menu.items.len()))
                .flatten();
            return match hit {
                Some(Some(i)) => self.run_menu(i),
                Some(None) => false,
                None => {
                    self.close_menu();
                    true
                }
            };
        }
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
                let rows =
                    (p.items.len() - p.first).min(chrome::palette_capacity(self.pane_height()));
                return match chrome::palette_hit(x - left, y, self.pane_width(), rows) {
                    PaletteHit::Row(i) => self.activate(p.first + i),
                    PaletteHit::Inside => false,
                    PaletteHit::Outside => self.close_palette(),
                };
            }
            self.close_palette();
        }
        let edge = self.sidebar_edge();
        let in_sidebar = self.sidebar && x < edge + 1.0;
        if in_sidebar {
            let hit = {
                let list = self.sidebar_rows();
                chrome::sidebar_hit(
                    x,
                    y,
                    edge,
                    self.sidebar_scroll,
                    &list.rows,
                    list.footer.is_some(),
                )
                .map(|item| match item {
                    SidebarItem::Row(r) | SidebarItem::CloseRow(r) | SidebarItem::Label(r) => {
                        (item, Some(list.targets[r]))
                    }
                    _ => (item, None),
                })
            };
            if let Some((SidebarItem::Edge, _)) = hit {
                self.drag = Some(Drag::Resizing);
                return false;
            }
            if x < edge {
                self.field = None;
                self.focus = None;
                return match hit {
                    Some((SidebarItem::Profile, _)) => self.switch_profile(),
                    Some((SidebarItem::Search, _)) => self.open_palette(),
                    Some((SidebarItem::CloseRow(_), _)) => self.close_tab(),
                    Some((SidebarItem::Footer, _)) => self.new_tab(),
                    Some((_, Some(RowTarget::Tab(i)))) => {
                        self.drag = Some(Drag::Pressed { tab: i, y });
                        self.select_tab(i)
                    }
                    Some((_, Some(RowTarget::Bookmark(i)))) => self.open_bookmark(i),
                    Some((_, Some(RowTarget::Section(Section::Group(i))))) => self.fold_group(i),
                    _ => false,
                };
            }
        }
        let x = x - left;
        if y < HEADER_HEIGHT {
            self.focus = None;
            return match chrome::hit(x, self.pane_width()) {
                Some(Button::Back) => self.back(),
                Some(Button::Forward) => self.forward(),
                Some(Button::Reload) => self.reload_or_stop(),
                Some(Button::Address) => self.focus_field(),
                Some(Button::More) => self.open_menu(false),
                None => false,
            };
        }
        let was_editing = self.field.take().is_some() | self.focus.take().is_some();
        let was_editing = was_editing | self.link_focus.take().is_some();
        self.editing = None;
        let pressed = self.state_target(x + left, y, css::ACTIVE);
        let was_editing = was_editing | self.set_states(|s| s.active = pressed);
        // Scripts hear the click first; preventDefault() keeps links and buttons still.
        if let Some(node) = self.node_at(x, y - HEADER_HEIGHT) {
            let (changed, prevented) = self.dispatch_page(node, "click");
            if prevented {
                return true;
            }
            if changed
                && self.control_at(x, y - HEADER_HEIGHT).is_none()
                && self.link_at(x, y - HEADER_HEIGHT).is_none()
            {
                return true;
            }
        }
        if let Some(i) = self.control_at(x, y - HEADER_HEIGHT) {
            self.focus_control(i);
            self.press_control(i);
            return true;
        }
        if let Some(url) = self.link_at(x, y - HEADER_HEIGHT) {
            self.follow(url);
            return true;
        }
        was_editing
    }

    /// Where the caret of the focused field is: x, top and height in page-area points.
    fn caret(&self, metrics: &dyn font::TextMetrics) -> Option<(f32, f32, f32)> {
        let field = self.editing.as_ref()?;
        let i = self.focused_control()?;
        let page = self.page.as_ref()?;
        let c = &page.forms.controls[i];
        let layout = &page.layout;
        let b = (0..layout.boxes.len()).find(|&k| layout.boxes[k].node == c.node)?;
        let (r, fixed) = (layout.boxes[b].rect, layout.box_is_fixed(b));
        let style = page.styles.style(c.node);
        let size = style.font_size.to_f32();
        let key = layout::font_key(style);
        let (ascent, descent) = (
            metrics.ascent_em(key) * size,
            metrics.descent_em(key) * size,
        );
        // Where its text starts: the first run of the value or placeholder.
        let (shown, placeholder) = page.forms.text_nodes(i);
        let first = layout
            .texts
            .iter()
            .find(|t| Some(t.node) == shown || Some(t.node) == placeholder)
            .map(|t| (t.x, t.baseline));
        let edge = style.border_width[3].to_f32() + 2.0;
        let (x0, baseline) = first.unwrap_or((
            r.x + edge,
            r.y + style.border_width[0].to_f32() + 1.0 + ascent,
        ));
        let before = &field.text[..field.cursor];
        let line = before.matches('\n').count() as f32;
        let tail = before.rsplit('\n').next().unwrap_or("");
        let width: f32 = if c.kind == forms::Kind::Password {
            tail.chars().count() as f32 * metrics.advance_em('•', key) * size
        } else {
            tail.chars()
                .map(|ch| metrics.advance_em(ch, key) * size)
                .sum()
        };
        let line_height = (ascent + descent) * 1.15;
        let x = (x0 + width).min(r.x + r.width - edge).max(r.x + 1.0);
        let top = baseline - ascent + line * line_height;
        let top = if fixed { top } else { top - self.scroll };
        Some((x, top, ascent + descent))
    }

    /// The innermost node drawn at (x, y) in pane points: a run of text, else a box.
    fn node_at(&self, x: f32, y_view: f32) -> Option<NodeId> {
        let page = self.page.as_ref()?;
        let layout = &page.layout;
        let metrics = page.metrics(&self.fonts);
        let doc_y = |fixed: bool| if fixed { y_view } else { y_view + self.scroll };
        for i in layout.texts_top_down() {
            let t = &layout.texts[i];
            let style = page.styles.style(t.node);
            let (size, key) = (style.font_size.to_f32(), layout::font_key(style));
            let y = doc_y(layout.text_is_fixed(i));
            // Through the transforms around it, where the pointer falls on the fragment.
            let Some((x, y)) = layout.untransform(&page.styles, i, false, (x, y)) else {
                continue;
            };
            let top = t.baseline - metrics.ascent_em(key) * size;
            let bottom = t.baseline + metrics.descent_em(key) * size;
            if (t.x..=t.x + t.width).contains(&x) && (top..=bottom).contains(&y) {
                return Some(t.node);
            }
        }
        layout.boxes_top_down().find_map(|i| {
            let r = layout.boxes[i].rect;
            let y = doc_y(layout.box_is_fixed(i));
            let (x, y) = layout.untransform(&page.styles, i, true, (x, y))?;
            ((r.x..=r.x + r.width).contains(&x) && (r.y..=r.y + r.height).contains(&y))
                .then_some(layout.boxes[i].node)
        })
    }

    /// Fire `kind` at `node` for the page's scripts. Returns (the page changed and was laid
    /// out again, a listener prevented the default action).
    fn dispatch_page(&mut self, node: NodeId, kind: &str) -> (bool, bool) {
        let Some(page) = self.page.as_mut() else {
            return (false, false);
        };
        let Some(script) = page.script.as_mut() else {
            return (false, false);
        };
        // Scripts never see generated boxes: the event goes to the element they belong to.
        let dom = &page.dom;
        let mut node = node;
        if matches!(dom.kind(node), html::NodeKind::Text(_))
            && dom.parent(node).is_some_and(|p| dom.is_generated(p))
        {
            node = dom.parent(node).unwrap_or(node);
        }
        if dom.is_generated(node) {
            node = dom.parent(node).unwrap_or(node);
        }
        let r = crate::script::dispatch(
            script,
            &mut page.dom,
            &mut page.forms,
            &page.url,
            node,
            kind,
        );
        if r.0 {
            self.relayout_page();
        }
        r
    }

    /// How long until the page's next script timer.
    fn next_page_timer(&self) -> Option<Duration> {
        self.page.as_ref()?.script.as_ref()?.next_timer()
    }

    /// Run the page's due timers; returns whether the page changed (and was laid out again).
    fn run_page_timers(&mut self) -> bool {
        if !self.next_page_timer().is_some_and(|d| d.is_zero()) {
            return false;
        }
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        let Some(script) = page.script.as_mut() else {
            return false;
        };
        let changed = crate::script::run_timers(script, &mut page.dom, &mut page.forms, &page.url);
        if changed {
            self.relayout_page();
        }
        changed
    }

    /// The form control at (x, y) in pane points, if any.
    fn control_at(&self, x: f32, y_view: f32) -> Option<usize> {
        let page = self.page.as_ref()?;
        let layout = &page.layout;
        layout.boxes_top_down().find_map(|i| {
            let b = layout.boxes[i];
            let y = if layout.box_is_fixed(i) {
                y_view
            } else {
                y_view + self.scroll
            };
            let (x, y) = layout.untransform(&page.styles, i, true, (x, y))?;
            let r = b.rect;
            let inside = (r.x..=r.x + r.width).contains(&x) && (r.y..=r.y + r.height).contains(&y);
            let style = page.styles.style(b.node);
            let control = page.forms.at(&page.dom, b.node)?;
            (inside && style.visible && page.forms.focusable(&page.dom, control)).then_some(control)
        })
    }

    /// The form control with keyboard focus.
    fn focused_control(&self) -> Option<usize> {
        let page = self.page.as_ref()?;
        let node = self.link_focus.as_ref()?.node;
        page.forms
            .controls
            .binary_search_by_key(&node, |c| c.node)
            .ok()
    }

    /// Give control `i` keyboard focus; a text field gets its caret at the end.
    fn focus_control(&mut self, i: usize) {
        let Some(page) = &self.page else {
            return;
        };
        let c = &page.forms.controls[i];
        let node = c.node;
        self.editing = page.forms.is_text(i).then(|| Field {
            text: c.value.clone(),
            cursor: c.value.len(),
            all_selected: false,
        });
        self.focus_link(node, true);
    }

    /// A click on control `i`, or Space on it: toggle, choose, submit or reset.
    fn press_control(&mut self, i: usize) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let changed = match page.forms.controls[i].kind {
            forms::Kind::Checkbox | forms::Kind::Radio => page.forms.toggle(&mut page.dom, i),
            forms::Kind::Select => page.forms.choose(&mut page.dom, i, 1),
            forms::Kind::Submit => {
                self.submit(i);
                return;
            }
            forms::Kind::Reset => {
                page.forms.reset(&mut page.dom, i);
                true
            }
            _ => false,
        };
        if changed {
            self.relayout_page();
        }
    }

    /// Lay the page out again after a control changed what it shows.
    fn relayout_page(&mut self) {
        let node = self.link_focus.as_ref().map(|l| l.node);
        self.relayout();
        if let Some(node) = node {
            self.focus_link(node, false);
        }
    }

    /// Submit the form of control `i` (the button pressed, or the field where Enter was).
    fn submit(&mut self, i: usize) {
        let Some(page) = &self.page else {
            return;
        };
        let Some(sent) = page.forms.submission(&page.dom, i, &page.url) else {
            return;
        };
        self.editing = None;
        self.link_focus = None;
        self.save_scroll();
        self.navigate_with(sent.url, sent.body, Destination::Push, true);
    }

    /// Keys for the focused control `i`. `None` lets the key do what it does elsewhere.
    fn control_key(&mut self, i: usize, k: KeyEvent) -> Option<bool> {
        let kind = self.page.as_ref()?.forms.controls[i].kind;
        if let Some(field) = self.editing.as_mut() {
            match k.key {
                Key::Char(c) => field.insert(c.encode_utf8(&mut [0; 4])),
                Key::Backspace => field.backspace(),
                Key::Left => field.left(),
                Key::Right => field.right(),
                Key::Home | Key::Up => field.cursor = 0,
                Key::End | Key::Down => field.cursor = field.text.len(),
                Key::Enter if kind == forms::Kind::TextArea => field.insert("\n"),
                Key::Enter => {
                    self.submit(i);
                    return Some(true);
                }
                Key::Escape => {
                    self.editing = None;
                    self.link_focus = None;
                    return Some(true);
                }
                _ => return None,
            }
            let text = field.text.clone();
            let page = self.page.as_mut()?;
            page.forms.set_value(&mut page.dom, i, &text);
            self.relayout_page();
            return Some(true);
        }
        match (kind, k.key) {
            (forms::Kind::Select, Key::Up) => {
                let page = self.page.as_mut()?;
                let changed = page.forms.choose(&mut page.dom, i, -1);
                changed.then(|| self.relayout_page());
                Some(changed)
            }
            (forms::Kind::Select, Key::Down) => {
                let page = self.page.as_mut()?;
                let changed = page.forms.choose(&mut page.dom, i, 1);
                changed.then(|| self.relayout_page());
                Some(changed)
            }
            (_, Key::Char(' ')) | (forms::Kind::Submit | forms::Kind::Reset, Key::Enter) => {
                self.press_control(i);
                Some(true)
            }
            (_, Key::Escape) => {
                self.link_focus = None;
                Some(true)
            }
            _ => None,
        }
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if self.menu.is_some() {
            if !k.command {
                return self.menu_key(k.key);
            }
            self.close_menu();
        }
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
                // With Shift, macOS reports the shifted character on some layouts.
                Key::Char('1' | '!') if k.shift => self.set_split(1),
                Key::Char('2' | '@') if k.shift => self.set_split(2),
                Key::Char('4' | '$') if k.shift => self.set_split(4),
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
                        self.header_change = true;
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
                    return true;
                }
                Key::Escape => self.field = None,
                // `move_focus` closes the field, after reading where focus was.
                Key::Tab => return self.move_focus(k.shift),
                _ => return false,
            }
            // Editing the field changes nothing but the header.
            self.header_change = true;
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
                        Button::More => self.open_menu(true),
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
        if let Some(i) = self.focused_control() {
            if let Some(done) = self.control_key(i, k) {
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

    /// Tab order: back, forward, reload, the address field, then the page's links and form
    /// controls in document order. Disabled buttons and links that are not shown are skipped;
    /// the order wraps.
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
            Button::Address | Button::More => true,
        };
        let mut order: Vec<Stop> = [
            Button::Back,
            Button::Forward,
            Button::Reload,
            Button::Address,
            Button::More,
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
        self.editing = None;
        match order[next] {
            Stop::Header(Button::Address) => {
                self.focus_field();
            }
            Stop::Header(button) => self.focus = Some(button),
            Stop::Link(node) => {
                let control = self.page.as_ref().and_then(|p| {
                    p.forms
                        .controls
                        .binary_search_by_key(&node, |c| c.node)
                        .ok()
                });
                match control {
                    Some(i) => self.focus_control(i),
                    None => self.focus_link(node, true),
                }
            }
        }
        true
    }

    /// Give `node` (a link) keyboard focus; with `reveal`, scroll it into view.
    fn focus_link(&mut self, node: NodeId, reveal: bool) {
        let Some(page) = &self.page else {
            return;
        };
        let rects = if page
            .forms
            .controls
            .binary_search_by_key(&node, |c| c.node)
            .is_ok()
        {
            // A control's ring goes around its border box, not each run of its text.
            (0..page.layout.boxes.len())
                .filter(|&i| page.layout.boxes[i].node == node)
                .map(|i| (page.layout.boxes[i].rect, page.layout.box_is_fixed(i)))
                .collect()
        } else {
            link_rects(page, node, &page.metrics(&self.fonts))
        };
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
            Command::TogglePin => {
                let p = &mut self.profiles[self.profile];
                let tab = &mut p.tabs[p.active];
                tab.pinned = !tab.pinned;
                p.normalize();
                self.reveal_active_tab();
                self.save_session();
                true
            }
            Command::MoveToGroup => {
                let group = self.tab().group.clone().unwrap_or_default();
                self.open_dialog(DialogKind::Group, vec![group]);
                true
            }
            Command::FoldGroup => {
                let p = &self.profiles[self.profile];
                match p.tabs[p.active].group.is_some() {
                    true => self.fold_group(p.active),
                    false => false,
                }
            }
            Command::NewProfile => {
                self.open_dialog(DialogKind::NewProfile, vec![String::new()]);
                true
            }
            Command::RenameProfile => {
                let name = self.profiles[self.profile].name.clone();
                self.open_dialog(DialogKind::RenameProfile, vec![name]);
                true
            }
            Command::RecolorProfile => {
                let p = &mut self.profiles[self.profile];
                let at = Hue::ALL.iter().position(|&h| h == p.hue).unwrap_or(0);
                p.hue = Hue::ALL[(at + 1) % Hue::ALL.len()];
                self.save_session();
                true
            }
            Command::RemoveProfile => {
                self.open_dialog(DialogKind::RemoveProfile, vec![String::new()]);
                true
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
        self.open_dialog(DialogKind::Bookmark { url, existing }, vec![name, folder]);
        true
    }

    /// Open a dialog with these field values; the first is selected, so typing replaces it.
    fn open_dialog(&mut self, kind: DialogKind, values: Vec<String>) {
        self.close_palette();
        self.field = None;
        self.focus = None;
        let fields = values
            .into_iter()
            .enumerate()
            .map(|(i, text)| Field {
                cursor: text.len(),
                text,
                all_selected: i == 0,
            })
            .collect();
        self.dialog = Some(Dialog {
            kind,
            fields,
            active: 0,
            error: None,
        });
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
                d.active = (d.active + 1) % d.fields.len();
                // Typing replaces the field just reached, as in macOS dialogs.
                d.fields[d.active].all_selected = true;
            }
            Key::Escape => {
                self.dialog = None;
                self.chrome.forget_palette();
            }
            Key::Enter => return self.finish_dialog(),
            _ => return false,
        }
        true
    }

    /// Enter in a dialog: save what it edits.
    fn finish_dialog(&mut self) -> bool {
        let Some(d) = self.dialog.take() else {
            return false;
        };
        self.chrome.forget_palette();
        let clean = |s: &str| s.trim().replace(['\t', '\n', '\r'], " ");
        let (url, existing) = match d.kind {
            DialogKind::Bookmark { url, existing } => (url, existing),
            DialogKind::NewProfile | DialogKind::RenameProfile | DialogKind::RemoveProfile => {
                return self.finish_profile_dialog(d);
            }
            DialogKind::Group => {
                let name = clean(&d.fields[0].text);
                let p = &mut self.profiles[self.profile];
                let tab = &mut p.tabs[p.active];
                tab.group = (!name.is_empty()).then_some(name);
                tab.pinned = false;
                p.normalize();
                self.reveal_active_tab();
                self.save_session();
                return true;
            }
        };
        let title = match clean(&d.fields[0].text) {
            t if t.is_empty() => url.to_string(),
            t => t,
        };
        let folder = match clean(&d.fields[1].text) {
            f if f.is_empty() => DEFAULT_FOLDER.to_string(),
            f => f,
        };
        let p = &mut self.profiles[self.profile];
        let bookmark = Bookmark {
            title,
            url,
            folder: folder.clone(),
        };
        match existing.filter(|&i| i < p.bookmarks.len()) {
            Some(i) => p.bookmarks[i] = bookmark,
            None => p.bookmarks.push(bookmark),
        }
        p.last_folder = folder;
        self.save_bookmarks();
        true
    }

    /// Enter in a profile dialog. A name that cannot be used keeps the dialog open and says why.
    fn finish_profile_dialog(&mut self, mut d: Dialog) -> bool {
        let name: String = d.fields[0]
            .text
            .trim()
            .replace(['\t', '\n', '\r'], " ")
            .chars()
            .take(MAX_NAME)
            .collect();
        let taken = |except: Option<usize>| {
            self.profiles
                .iter()
                .enumerate()
                .any(|(i, p)| Some(i) != except && p.name.eq_ignore_ascii_case(&name))
        };
        let error = match d.kind {
            DialogKind::RemoveProfile if self.profiles.len() == 1 => {
                Some("Broxee needs at least one profile.".to_string())
            }
            DialogKind::RemoveProfile if name != self.profiles[self.profile].name => Some(format!(
                "Type {} exactly to remove it.",
                self.profiles[self.profile].name
            )),
            DialogKind::RemoveProfile => None,
            _ if name.is_empty() => Some("A profile needs a name.".to_string()),
            DialogKind::RenameProfile if taken(Some(self.profile)) => {
                Some(format!("There is already a profile called {name}."))
            }
            DialogKind::NewProfile if taken(None) => {
                Some(format!("There is already a profile called {name}."))
            }
            DialogKind::NewProfile if self.profiles.len() >= MAX_PROFILES => {
                Some(format!("Broxee keeps at most {MAX_PROFILES} profiles."))
            }
            _ => None,
        };
        if let Some(error) = error {
            d.error = Some(error);
            self.dialog = Some(d);
            return true;
        }
        match d.kind {
            DialogKind::RenameProfile => {
                self.profiles[self.profile].name = name;
            }
            DialogKind::NewProfile => {
                // The first color no profile uses yet, or the least used.
                let used = |h: Hue| self.profiles.iter().filter(|p| p.hue == h).count();
                let hue = Hue::ALL
                    .iter()
                    .copied()
                    .min_by_key(|&h| used(h))
                    .unwrap_or(Hue::Emerald);
                let mut n = self.profiles.len() + 1;
                let id = loop {
                    let id = format!("profile-{n}");
                    if !self.profiles.iter().any(|p| p.id.eq_ignore_ascii_case(&id)) {
                        break id;
                    }
                    n += 1;
                };
                let mut profile = Profile::new(&id, &name, hue);
                profile.load_files(self.session.as_deref());
                self.profiles.push(profile);
                let last = self.profiles.len() - 1;
                self.show_tab(last, 0);
            }
            DialogKind::RemoveProfile => {
                self.stop_load();
                self.page = None;
                self.page_is_entry = false;
                let gone = self.profiles.remove(self.profile);
                // Its tabs, history, cookies and bookmarks go with it.
                if let Some(dir) = profile_file(self.session.as_deref(), &gone.id, "") {
                    let _ = std::fs::remove_dir_all(dir);
                }
                net::forget_sessions();
                self.profile = self.profile.min(self.profiles.len() - 1);
                self.wake();
            }
            _ => {}
        }
        self.save_session();
        true
    }

    /// Open the ⋯ menu; from the keyboard, with its first item chosen.
    fn open_menu(&mut self, keyboard: bool) -> bool {
        self.close_palette();
        self.field = None;
        let entry = self.tab().entry().is_some();
        let mut items = Vec::new();
        if entry {
            items.push(("Copy address".to_string(), MenuAction::CopyAddress));
            for (i, p) in self.profiles.iter().enumerate() {
                if i != self.profile {
                    items.push((format!("Open in {}", p.name), MenuAction::OpenIn(i)));
                }
            }
            items.push(("Bookmark this page".to_string(), MenuAction::Bookmark));
            items.push(("Reload".to_string(), MenuAction::Reload));
        }
        items.push(("Close tab".to_string(), MenuAction::CloseTab));
        self.menu = Some(Menu {
            items,
            selected: keyboard.then_some(0),
        });
        true
    }

    fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.chrome.forget_palette();
        }
    }

    fn menu_key(&mut self, key: Key) -> bool {
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        let last = menu.items.len() - 1;
        match key {
            Key::Down | Key::Tab => {
                menu.selected = Some(menu.selected.map_or(0, |i| (i + 1).min(last)));
            }
            Key::Up => menu.selected = Some(menu.selected.map_or(last, |i| i.saturating_sub(1))),
            Key::Enter | Key::Char(' ') => {
                return match menu.selected {
                    Some(i) => self.run_menu(i),
                    None => false,
                };
            }
            Key::Escape => self.close_menu(),
            _ => return false,
        }
        true
    }

    fn run_menu(&mut self, index: usize) -> bool {
        let Some(action) = self
            .menu
            .as_ref()
            .and_then(|m| m.items.get(index))
            .map(|(_, a)| *a)
        else {
            return false;
        };
        self.close_menu();
        let url = self.tab().entry().map(|e| e.url.clone());
        match (action, url) {
            (MenuAction::CopyAddress, Some(url)) => {
                self.window.copy(&url.to_string());
                true
            }
            (MenuAction::OpenIn(p), Some(url)) if p < self.profiles.len() => {
                // A new tab in the other profile, with that profile's cookies.
                self.sleep();
                net::forget_sessions();
                self.profile = p;
                let profile = &mut self.profiles[p];
                profile.tabs.push(Tab::default());
                profile.active = profile.tabs.len() - 1;
                self.navigate(url, Destination::Push);
                self.reveal_active_tab();
                self.save_session();
                true
            }
            (MenuAction::Bookmark, Some(_)) => self.open_bookmark_dialog(),
            (MenuAction::Reload, Some(_)) => self.reload(),
            (MenuAction::CloseTab, _) => self.close_tab(),
            _ => false,
        }
    }

    /// What a tooltip at (x, y) would say: icon buttons and their shortcuts, sleeping tabs.
    fn tooltip_at(&self, x: f32, y: f32) -> Option<String> {
        let left = self.pane_left();
        if x >= left && y < HEADER_HEIGHT {
            return Some(
                match chrome::hit(x - left, self.pane_width())? {
                    Button::Back => "Back ⌘[",
                    Button::Forward => "Forward ⌘]",
                    Button::Reload if self.load.is_some() => "Stop Esc",
                    Button::Reload => "Reload ⌘R",
                    Button::More => "More",
                    Button::Address => return None,
                }
                .to_string(),
            );
        }
        if !self.sidebar || x >= left {
            return None;
        }
        let list = self.sidebar_rows();
        let item = chrome::sidebar_hit(x, y, left, self.sidebar_scroll, &list.rows, false)?;
        let active = self.profiles[self.profile].active;
        let text = match item {
            SidebarItem::Profile => "Switch profile ⌘⇧P",
            SidebarItem::CloseRow(_) => "Close tab ⌘W",
            SidebarItem::Row(r) => match list.targets.get(r) {
                Some(&RowTarget::Tab(i)) if i != active => "Asleep to save memory. Select to wake.",
                _ => return None,
            },
            _ => return None,
        };
        Some(text.to_string())
    }

    /// The pointer moved: start waiting for a new tooltip, or hide the one shown.
    fn hover_at(&mut self, x: f32, y: f32) -> bool {
        let hovered = self.state_target(x, y, css::HOVER);
        let restyled = self.set_states(|s| s.hover = hovered);
        let text = self.tooltip_at(x, y);
        let same = self.hover.as_ref().map(|h| &h.text) == text.as_ref();
        if same {
            return restyled;
        }
        let was_shown = self.hover.as_ref().is_some_and(|h| h.shown);
        self.hover = text.map(|text| Hover {
            at: (x, y),
            text,
            since: std::time::Instant::now(),
            shown: false,
        });
        was_shown | restyled
    }

    /// Timers: show a tooltip once the pointer has rested long enough.
    fn tick(&mut self) -> bool {
        match self.hover.as_mut() {
            Some(h) if !h.shown && h.since.elapsed() >= TOOLTIP_DELAY => {
                h.shown = true;
                true
            }
            _ => false,
        }
    }

    /// Fold or unfold the group of tab `tab`.
    fn fold_group(&mut self, tab: usize) -> bool {
        let p = &mut self.profiles[self.profile];
        let Some(group) = p.tabs.get(tab).and_then(|t| t.group.clone()) else {
            return false;
        };
        match p.folded.iter().position(|g| *g == group) {
            Some(i) => {
                p.folded.remove(i);
            }
            None => p.folded.push(group),
        }
        let max = {
            let rows = self.sidebar_rows();
            chrome::sidebar_max_scroll(rows.rows.len(), rows.footer.is_some(), self.height)
        };
        self.sidebar_scroll = self.sidebar_scroll.min(max);
        self.save_session();
        true
    }

    /// Scroll the sidebar so the active tab's row is in view.
    fn reveal_active_tab(&mut self) {
        if self.list != SidebarList::Tabs {
            return;
        }
        let active = self.profiles[self.profile].active;
        let (row, count, footer) = {
            let rows = self.sidebar_rows();
            let row = rows
                .targets
                .iter()
                .position(|t| *t == RowTarget::Tab(active));
            (row, rows.rows.len(), rows.footer.is_some())
        };
        let Some(row) = row else {
            return;
        };
        let view = self.height - chrome::LIST_TOP;
        let top = chrome::TAB_ROW * row as f32;
        if top < self.sidebar_scroll {
            self.sidebar_scroll = top;
        } else if top + chrome::TAB_ROW > self.sidebar_scroll + view {
            self.sidebar_scroll = top + chrome::TAB_ROW - view;
        }
        let max = chrome::sidebar_max_scroll(count, footer, self.height);
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, max);
    }

    /// The pointer moved with the button down: resize the sidebar or move a tab.
    fn drag_to(&mut self, x: f32, y: f32) -> bool {
        match self.drag {
            Some(Drag::Resizing) => {
                let width = x.round().clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                let changed = width != self.sidebar_width;
                self.sidebar_width = width;
                changed
            }
            Some(Drag::Pressed { tab, y: from }) if (y - from).abs() > 4.0 => {
                let rows = self.sidebar_rows().rows.len();
                let drop = chrome::sidebar_drop(y, self.sidebar_scroll, rows);
                self.drag = Some(Drag::Moving { tab, drop });
                true
            }
            Some(Drag::Moving { tab, drop }) => {
                let rows = self.sidebar_rows().rows.len();
                let to = chrome::sidebar_drop(y, self.sidebar_scroll, rows);
                self.drag = Some(Drag::Moving { tab, drop: to });
                to != drop
            }
            _ => false,
        }
    }

    /// The button came up: finish a resize or a move.
    fn release(&mut self, _x: f32, _y: f32) -> bool {
        if self.set_states(|s| s.active = None) {
            return true;
        }
        match self.drag.take() {
            Some(Drag::Resizing) => {
                // The page is laid out again once, for the final width.
                self.relayout();
                self.save_session();
                true
            }
            Some(Drag::Moving { tab, drop }) => self.move_tab(tab, drop),
            _ => false,
        }
    }

    /// Move tab `tab` to land before row `drop` of the tab list. It joins the section it
    /// lands in: pinned, a group, or neither.
    fn move_tab(&mut self, tab: usize, drop: usize) -> bool {
        let targets = self.sidebar_rows().targets;
        let p = &mut self.profiles[self.profile];
        if tab >= p.tabs.len() {
            return false;
        }
        // The section of the row just above the landing point (or the first row's).
        let above = drop.checked_sub(1).or((!targets.is_empty()).then_some(0));
        let section = match above.and_then(|r| targets.get(r)) {
            Some(RowTarget::Section(section)) => *section,
            Some(&RowTarget::Tab(i)) if p.tabs[i].pinned => Section::Pinned,
            Some(&RowTarget::Tab(i)) => match p.tabs[i].group {
                Some(_) => Section::Group(i),
                None => Section::Rest,
            },
            _ => Section::Rest,
        };
        let group = match section {
            Section::Group(i) => p.tabs[i].group.clone(),
            _ => None,
        };
        // Where it goes among the other tabs: after the tab above, or first in a section.
        let after = targets[..drop.min(targets.len())]
            .iter()
            .rev()
            .find_map(|t| match t {
                RowTarget::Tab(i) if *i != tab => Some(*i),
                _ => None,
            });
        let active_was = p.active;
        let moved = p.tabs.remove(tab);
        let index = |i: usize| if i > tab { i - 1 } else { i };
        let at = match after {
            Some(i) => index(i) + 1,
            None => 0,
        };
        p.tabs.insert(at.min(p.tabs.len()), moved);
        let tab_now = at.min(p.tabs.len() - 1);
        p.tabs[tab_now].pinned = section == Section::Pinned;
        p.tabs[tab_now].group = group;
        // Keep the active tab active wherever it went.
        p.active = if active_was == tab {
            tab_now
        } else {
            let a = index(active_was);
            if a >= tab_now {
                a + 1
            } else {
                a
            }
        };
        p.normalize();
        self.save_session();
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
        let Some(path) = profile_file(self.session.as_deref(), &profile.id, "bookmarks") else {
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
        for (i, p) in self.profiles.iter().enumerate() {
            if i != self.profile {
                items.push(palette::Item {
                    title: format!("Switch to {}", p.name),
                    detail: "Profile".into(),
                    hint: Hint::Label(String::new()),
                    action: Action::Profile(i),
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
        let rows = chrome::palette_capacity(self.pane_height());
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
            Action::Profile(i) if i < self.profiles.len() => {
                let active = self.profiles[i].active;
                self.show_tab(i, active)
            }
            Action::Profile(_) => false,
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
        self.navigate_with(url, None, to, same_site_initiator);
    }

    /// `navigate_from`, POSTing `body` when there is one (a submitted form).
    fn navigate_with(
        &mut self,
        url: net::Url,
        body: Option<net::Body>,
        to: Destination,
        same_site_initiator: bool,
    ) {
        self.stop_load();
        self.editing = None;
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
                let result = engine::load_with(
                    &target,
                    body.as_ref(),
                    settings,
                    &fonts,
                    Some(jar),
                    &mut |progress| sender.send(LoadMessage::Progress(progress)).is_ok(),
                );
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
        self.reveal_active_tab();
        true
    }

    fn new_tab(&mut self) -> bool {
        self.sleep();
        let p = &mut self.profiles[self.profile];
        p.tabs.push(Tab::default());
        p.active = p.tabs.len() - 1;
        self.wake();
        self.reveal_active_tab();
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
        let Some(path) = profile_file(self.session.as_deref(), &profile.id, "cookies") else {
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
        let text = write_session(
            &self.profiles,
            self.profile,
            (self.sidebar_width, self.sidebar),
        );
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
        self.editing = None;
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
        let metrics = page.metrics(&self.fonts);
        let doc_y = |fixed: bool| if fixed { y_view } else { y_view + self.scroll };
        for i in layout.texts_top_down() {
            let t = &layout.texts[i];
            let style = page.styles.style(t.node);
            let (size, key) = (style.font_size.to_f32(), layout::font_key(style));
            let y = doc_y(layout.text_is_fixed(i));
            // Through the transforms around it, where the pointer falls on the fragment.
            let Some((x, y)) = layout.untransform(&page.styles, i, false, (x, y)) else {
                continue;
            };
            let top = t.baseline - metrics.ascent_em(key) * size;
            let bottom = t.baseline + metrics.descent_em(key) * size;
            if style.visible
                && !style.transparent
                && (t.x..=t.x + t.width).contains(&x)
                && (top..=bottom).contains(&y)
            {
                return link_for(page, t.node);
            }
        }
        // Innermost boxes come last in document order; an outer <a> can still wrap them.
        layout.boxes_top_down().find_map(|i| {
            let r = layout.boxes[i].rect;
            let y = doc_y(layout.box_is_fixed(i));
            let (x, y) = layout.untransform(&page.styles, i, true, (x, y))?;
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

    /// The frame in device pixels: width, height, header rows, the sidebar's edge, and how
    /// many panes there are.
    fn geometry(&self) -> Geometry {
        let scale = self.scale;
        let width_px = (self.width * scale).round().max(1.0) as usize;
        let height_px = (self.height * scale).round().max(1.0) as usize;
        let header_rows = ((HEADER_HEIGHT * scale).round() as usize).min(height_px);
        let sidebar_px = ((self.sidebar_edge() * scale).round() as usize).min(width_px - 1);
        (
            width_px,
            height_px,
            header_rows,
            sidebar_px,
            self.panes.len(),
        )
    }

    /// Each pane's x, y, width and height in device pixels, inside a frame of
    /// `width` x `height`.
    fn pane_rects_px(&self, width: usize, height: usize) -> Vec<(usize, usize, usize, usize)> {
        let s = self.scale;
        self.pane_rects()
            .into_iter()
            .map(|(x, y, w, h)| {
                let x0 = ((x * s).round() as usize).min(width.saturating_sub(1));
                let y0 = ((y * s).round() as usize).min(height.saturating_sub(1));
                let x1 = (((x + w) * s).round() as usize).clamp(x0 + 1, width);
                let y1 = (((y + h) * s).round() as usize).clamp(y0 + 1, height);
                (x0, y0, x1 - x0, y1 - y0)
            })
            .collect()
    }

    /// Repaint only the header, over the last frame. Falls back to a full frame when
    /// something drawn over the header (a tooltip, the menu, a dialog) or a new window size
    /// would be lost.
    fn render_header(&mut self) {
        let geometry = self.geometry();
        let (width_px, height_px, header_rows, _, _) = geometry;
        let (pane_x, pane_y, pane_px, pane_h) =
            self.pane_rects_px(width_px, height_px)[self.focused.min(self.panes.len() - 1)];
        let header_rows = header_rows.min(pane_h);
        let covered = self.menu.is_some()
            || self.dialog.is_some()
            || self.hover.as_ref().is_some_and(|h| h.shown);
        if covered || header_rows == 0 || self.frame_geometry != Some(geometry) {
            return self.render();
        }
        let fonts = Arc::clone(&self.fonts);
        let scale = self.scale;
        let profile = &self.profiles[self.profile];
        let host = header_host(profile);
        let header = header_view(
            profile,
            &host,
            self.field.as_ref(),
            self.focus,
            self.loading,
        );
        let (painter, chrome) = (&mut self.painter, &mut self.chrome);
        let shown = self.window.render_header(
            (pane_x, pane_y),
            pane_px,
            header_rows,
            &mut |pixels, stride| {
                let frame = Frame {
                    pixels,
                    stride,
                    left: 0,
                    width: pane_px,
                    scale,
                };
                chrome.draw(painter, frame, &fonts.ui(), &header);
            },
        );
        match shown {
            Ok(true) => {}
            Ok(false) => self.render(),
            Err(e) => self.window.set_title(&format!("Broxee: {e}")),
        }
    }

    fn render_with(&mut self, fonts: &FontStore) {
        let scale = self.scale;
        let geometry = self.geometry();
        let (width_px, height_px, header_rows, sidebar_px, _) = geometry;
        // A pane's old pixels can be reused only if the panes have not moved since.
        if self.frame_geometry != Some(geometry) {
            for pane in &mut self.panes {
                pane.painted = false;
            }
        }
        self.frame_geometry = Some(geometry);
        self.sync_pane();
        let rects_px = self.pane_rects_px(width_px, height_px);
        let focused = self.focused.min(rects_px.len() - 1);
        let (pane_x, pane_y, pane_px, pane_h) = rects_px[focused];
        let header_rows = header_rows.min(pane_h);
        let page_rows = pane_h - header_rows;
        // The other panes: their headers, and their old pixels or a placeholder.
        let others: Vec<(usize, bool, String, String)> = self
            .panes
            .iter()
            .enumerate()
            .filter(|&(j, _)| j != focused)
            .map(|(j, pane)| {
                let profile = &self.profiles[pane.profile.min(self.profiles.len() - 1)];
                let tab = profile.tabs.iter().find(|t| t.id == pane.tab);
                let host = tab
                    .and_then(|t| t.entry())
                    .map(|e| host_of(&e.url))
                    .unwrap_or_default();
                let title = tab.map_or("New tab", |t| t.title()).to_string();
                (j, pane.painted, host, title)
            })
            .collect();
        let other_headers: Vec<HeaderView> = others
            .iter()
            .map(|(j, _, host, _)| {
                let pane = self.panes[*j];
                let profile = &self.profiles[pane.profile.min(self.profiles.len() - 1)];
                let tab = profile.tabs.iter().find(|t| t.id == pane.tab);
                HeaderView {
                    profile: &profile.name,
                    hue: profile.hue,
                    can_back: tab.is_some_and(|t| t.index > 0),
                    can_forward: tab.is_some_and(|t| t.index + 1 < t.history.len()),
                    loading: None,
                    host,
                    field: None,
                    focused: None,
                }
            })
            .collect();

        let profile = &self.profiles[self.profile];
        let host = header_host(profile);
        let header = header_view(
            profile,
            &host,
            self.field.as_ref(),
            self.focus,
            self.loading,
        );
        let list = sidebar_rows(profile, self.list, self.bookmark_row);
        let drop_at = match self.drag {
            Some(Drag::Moving { drop, .. }) => Some(drop),
            _ => None,
        };
        let sidebar = (sidebar_px > 0).then_some(SidebarView {
            profile: &profile.name,
            hue: profile.hue,
            rows: &list.rows,
            selected: list.selected,
            footer: list.footer,
            scroll: self.sidebar_scroll,
            drop_at,
            empty: list.empty,
        });
        let remove_label = format!("Type {} to remove it", self.profiles[self.profile].name);
        let (labels, heading, help): (Vec<&str>, &str, &str) =
            match self.dialog.as_ref().map(|d| &d.kind) {
                Some(DialogKind::Bookmark {
                    existing: Some(_), ..
                }) => (
                    vec!["Name", "Folder"],
                    "Edit bookmark",
                    "Enter to save · Tab for the other field · Esc to cancel",
                ),
                Some(DialogKind::Bookmark { .. }) => (
                    vec!["Name", "Folder"],
                    "Bookmark this page",
                    "Enter to save · Tab for the other field · Esc to cancel",
                ),
                Some(DialogKind::Group) => (
                    vec!["Group"],
                    "Move this tab to a group",
                    "Enter to move · an empty name takes it out · Esc to cancel",
                ),
                Some(DialogKind::NewProfile) => (
                    vec!["Name"],
                    "New profile",
                    "Its own tabs, history, cookies and bookmarks · Enter to create",
                ),
                Some(DialogKind::RenameProfile) => (
                    vec!["Name"],
                    "Rename profile",
                    "Enter to rename · Esc to cancel",
                ),
                Some(DialogKind::RemoveProfile) => (
                    vec![remove_label.as_str()],
                    "Remove profile",
                    "Its tabs, history, cookies and bookmarks are deleted · Esc to cancel",
                ),
                None => (Vec::new(), "", ""),
            };
        let dialog_fields: Vec<(&str, &str, usize)> =
            self.dialog.as_ref().map_or(Vec::new(), |d| {
                d.fields
                    .iter()
                    .zip(&labels)
                    .map(|(f, label)| (*label, f.text.as_str(), f.cursor))
                    .collect()
            });
        let dialog_view = self.dialog.as_ref().map(|d| DialogView {
            heading,
            fields: &dialog_fields,
            active: d.active,
            help: d.error.as_deref().unwrap_or(help),
        });
        let palette_rows: Vec<PaletteRow> = self.palette.as_ref().map_or(Vec::new(), |p| {
            p.items
                .iter()
                .skip(p.first)
                .take(chrome::palette_capacity(self.pane_height()))
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
        let page_metrics = self.page.as_ref().map(|p| p.metrics(fonts));
        let caret = page_metrics.as_ref().and_then(|m| self.caret(m));
        let view = self
            .page
            .as_ref()
            .zip(page_metrics.as_ref())
            .map(|(p, m)| engine::page_view(p, m));
        let viewport = Viewport {
            width: pane_px as f32 / scale,
            height: page_rows as f32 / scale,
            scale,
            scroll_y: self.scroll,
        };
        let menu_items: Vec<&str> = self.menu.as_ref().map_or(Vec::new(), |m| {
            m.items.iter().map(|(label, _)| label.as_str()).collect()
        });
        let menu_view = self.menu.as_ref().map(|m| MenuView {
            items: &menu_items,
            selected: m.selected,
        });
        // Hover positions over the pane are relative to its top (see `route`).
        let edge = self.sidebar_edge();
        let top_pt = self.pane_top();
        let tooltip = self.hover.as_ref().filter(|h| h.shown).map(|h| {
            let (x, y) = h.at;
            ((x, if x >= edge { y + top_pt } else { y }), h.text.as_str())
        });
        let (painter, chrome) = (&mut self.painter, &mut self.chrome);
        let blank = chrome.theme().surface;
        let line = chrome.theme().line;
        let shown = self
            .window
            .render(width_px, height_px, &mut |pixels, stride, previous| {
                // Hairlines between panes: the column right of the first pane, and with four
                // panes the row under the top ones.
                if rects_px.len() > 1 {
                    let (x, _, w, h) = rects_px[0];
                    let right = rects_px[1].0;
                    chrome::fill(pixels, stride, x + w, right - (x + w), line);
                    if rects_px.len() == 4 {
                        let below = rects_px[2].1;
                        let strip = &mut pixels[h * stride..below * stride];
                        chrome::fill(strip, stride, sidebar_px, width_px - sidebar_px, line);
                    }
                }
                for ((j, painted, _, title), header) in others.iter().zip(&other_headers) {
                    let (x, y, w, h) = rects_px[*j];
                    let rows = &mut pixels[y * stride..(y + h) * stride];
                    let head = header_rows.min(h);
                    match previous {
                        Some(old) if *painted => {
                            let old = &old[y * stride..(y + h) * stride];
                            for (row, from) in rows.chunks_mut(stride).zip(old.chunks(stride)) {
                                row[x * 4..(x + w) * 4].copy_from_slice(&from[x * 4..(x + w) * 4]);
                            }
                        }
                        _ => {
                            chrome::fill(rows, stride, x, w, blank);
                            let frame = Frame {
                                pixels: &mut rows[head * stride..],
                                stride,
                                left: x,
                                width: w,
                                scale,
                            };
                            chrome.draw_placeholder(painter, frame, &fonts.ui(), title);
                        }
                    }
                    let frame = Frame {
                        pixels: &mut rows[..head * stride],
                        stride,
                        left: x,
                        width: w,
                        scale,
                    };
                    chrome.draw(painter, frame, &fonts.ui(), header);
                }
                if let Some(sidebar) = &sidebar {
                    let frame = Frame {
                        pixels: &mut *pixels,
                        stride,
                        left: 0,
                        width: sidebar_px,
                        scale,
                    };
                    chrome.draw_sidebar(painter, frame, &fonts.ui(), sidebar);
                }
                let pane_rows = &mut pixels[pane_y * stride..(pane_y + pane_h) * stride];
                let (top, bottom) = pane_rows.split_at_mut(header_rows * stride);
                match &view {
                    Some(view) if page_rows > 0 => {
                        painter.paint_into(
                            view,
                            viewport,
                            Target {
                                pixels: bottom,
                                stride,
                                bgra: true,
                                left: pane_x,
                            },
                        );
                    }
                    _ => chrome::fill(bottom, stride, pane_x, pane_px, blank),
                }
                if !ring.is_empty() && page_rows > 0 {
                    let frame = Frame {
                        pixels: &mut *bottom,
                        stride,
                        left: pane_x,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_focus_ring(painter, frame, &ring);
                }
                if let (Some(caret), true) = (caret, page_rows > 0) {
                    let frame = Frame {
                        pixels: &mut *bottom,
                        stride,
                        left: pane_x,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_caret(painter, frame, caret);
                }
                let frame = Frame {
                    pixels: top,
                    stride,
                    left: pane_x,
                    width: pane_px,
                    scale,
                };
                chrome.draw(painter, frame, &fonts.ui(), &header);
                if let Some(dialog) = &dialog_view {
                    let frame = Frame {
                        pixels: &mut *pane_rows,
                        stride,
                        left: pane_x,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_dialog(painter, frame, &fonts.ui(), dialog);
                }
                if let Some(palette) = &palette_view {
                    let frame = Frame {
                        pixels: &mut *pane_rows,
                        stride,
                        left: pane_x,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_palette(painter, frame, &fonts.ui(), palette);
                }
                if let Some(menu) = &menu_view {
                    let frame = Frame {
                        pixels: &mut *pane_rows,
                        stride,
                        left: pane_x,
                        width: pane_px,
                        scale,
                    };
                    chrome.draw_menu(painter, frame, &fonts.ui(), menu);
                }
                if let Some((at, text)) = tooltip {
                    let frame = Frame {
                        pixels: &mut *pixels,
                        stride,
                        left: 0,
                        width: width_px,
                        scale,
                    };
                    chrome.draw_tooltip(painter, frame, &fonts.ui(), at, text);
                }
            });
        if let Err(e) = shown {
            self.window.set_title(&format!("Broxee: {e}"));
        }
    }
}

/// The host the header shows for the visible tab of `profile`.
fn header_host(profile: &Profile) -> String {
    let tab = &profile.tabs[profile.active];
    tab.entry().map(|e| host_of(&e.url)).unwrap_or_default()
}

fn header_view<'a>(
    profile: &'a Profile,
    host: &'a str,
    field: Option<&'a Field>,
    focus: Option<Button>,
    loading: Option<f32>,
) -> HeaderView<'a> {
    let tab = &profile.tabs[profile.active];
    HeaderView {
        profile: &profile.name,
        hue: profile.hue,
        can_back: tab.index > 0,
        can_forward: tab.index + 1 < tab.history.len(),
        loading,
        host,
        field: field.map(|f| FieldView {
            text: &f.text,
            cursor: f.cursor,
            all_selected: f.all_selected,
        }),
        focused: focus,
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
        SidebarList::Tabs => {
            // Tabs are kept in this order (`Profile::normalize`): pinned, groups, the rest.
            // Labels appear once there is more than one kind of section.
            let sectioned = profile.tabs.iter().any(|t| t.pinned || t.group.is_some());
            let mut rows = Vec::new();
            let mut targets = Vec::new();
            let mut current: Option<Section> = None;
            for (i, tab) in profile.tabs.iter().enumerate() {
                let section = match (&tab.group, tab.pinned) {
                    (_, true) => Section::Pinned,
                    (Some(_), false) => match current {
                        Some(Section::Group(first)) if profile.tabs[first].group == tab.group => {
                            Section::Group(first)
                        }
                        _ => Section::Group(i),
                    },
                    (None, false) => Section::Rest,
                };
                let folded = matches!(section, Section::Group(_))
                    && tab
                        .group
                        .as_ref()
                        .is_some_and(|g| profile.folded.contains(g));
                if sectioned && current != Some(section) {
                    let label = match section {
                        Section::Pinned => Cow::Borrowed("Pinned"),
                        Section::Rest => Cow::Borrowed("Tabs"),
                        Section::Group(_) => {
                            let name = tab.group.as_deref().unwrap_or_default();
                            if folded {
                                let count =
                                    profile.tabs.iter().filter(|t| t.group == tab.group).count();
                                Cow::Owned(format!("{name} ({count})"))
                            } else {
                                Cow::Borrowed(name)
                            }
                        }
                    };
                    rows.push(ListRow::Label(label));
                    targets.push(RowTarget::Section(section));
                    current = Some(section);
                }
                // A folded group still shows its active tab.
                if folded && i != profile.active {
                    continue;
                }
                rows.push(ListRow::Item {
                    title: tab.title(),
                    muted: i != profile.active,
                    closable: i == profile.active,
                });
                targets.push(RowTarget::Tab(i));
            }
            let selected = targets
                .iter()
                .position(|t| *t == RowTarget::Tab(profile.active));
            SidebarRows {
                rows,
                targets,
                selected,
                footer: Some("+ New tab"),
                empty: None,
            }
        }
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
                rows.push(ListRow::Label(Cow::Borrowed(folder)));
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

/// Profiles at most, and characters in a profile's name.
const MAX_PROFILES: usize = 12;
const MAX_NAME: usize = 32;

/// A profile folder name: letters, digits, `-` and `_` only, so it cannot leave `profiles/`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
fn write_session(profiles: &[Profile], visible: usize, sidebar: (f32, bool)) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = format!(
        "broxee-session 1\nvisible\t{visible}\nsidebar\t{}\t{}\n",
        sidebar.0,
        u8::from(sidebar.1)
    );
    for profile in profiles {
        out += &format!(
            "profile\t{}\t{}\t{}\t{}\n",
            profile.active,
            clean(&profile.name),
            profile.id,
            profile.hue.key()
        );
        for group in &profile.folded {
            out += &format!("folded\t{}\n", clean(group));
        }
        for tab in &profile.tabs {
            out += &format!("tab\t{}\t{}\n", tab.index, clean(&tab.title));
            if tab.pinned {
                out += "pinned\n";
            }
            if let Some(group) = &tab.group {
                out += &format!("group\t{}\n", clean(group));
            }
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

/// A profile as saved: who it is, its tabs, the active one and its folded groups.
struct SavedProfile {
    name: String,
    id: String,
    hue: Hue,
    tabs: Vec<Tab>,
    active: usize,
    folded: Vec<String>,
}

struct Session {
    profiles: Vec<SavedProfile>,
    visible: usize,
    /// Sidebar width and whether it is shown.
    sidebar: Option<(f32, bool)>,
}

/// Read a session written by `write_session`; anything malformed is skipped, never trusted.
fn parse_session(text: &str) -> Option<Session> {
    let mut lines = text.lines();
    if lines.next()? != "broxee-session 1" {
        return None;
    }
    let mut visible = 0;
    let mut sidebar = None;
    let mut profiles: Vec<SavedProfile> = Vec::new();
    for line in lines {
        let mut parts = line.splitn(3, '\t');
        let (kind, a, b) = (parts.next()?, parts.next(), parts.next());
        let number = a.and_then(|a| a.parse::<usize>().ok()).unwrap_or(0);
        let name = a.map(str::trim).filter(|n| !n.is_empty()).map(String::from);
        match kind {
            "visible" => visible = number,
            "sidebar" => {
                let width = a
                    .and_then(|a| a.parse::<f32>().ok())
                    .filter(|w| w.is_finite());
                if let Some(width) = width {
                    sidebar = Some((width.clamp(SIDEBAR_MIN, SIDEBAR_MAX), b != Some("0")));
                }
            }
            "profile" => {
                // "name" in older sessions; "name<tab>id<tab>hue" since profiles can change.
                let mut fields = b.unwrap_or_default().split('\t');
                let name = fields.next().unwrap_or_default().trim();
                let id = fields.next().unwrap_or(name);
                let hue = fields.next().and_then(Hue::from_key);
                // The id names a folder: anything that is not a plain name is not trusted.
                if valid_id(id) && !name.is_empty() && profiles.len() < MAX_PROFILES {
                    let n = profiles.len();
                    profiles.push(SavedProfile {
                        name: name.chars().take(MAX_NAME).collect(),
                        id: id.to_string(),
                        // Older sessions: Personal emerald, Work amber, as before.
                        hue: hue.unwrap_or(Hue::ALL[n % Hue::ALL.len()]),
                        tabs: Vec::new(),
                        active: number,
                        folded: Vec::new(),
                    });
                } else {
                    // Keep later lines from landing in the previous profile.
                    profiles.push(SavedProfile {
                        name: String::new(),
                        id: String::new(),
                        hue: Hue::Emerald,
                        tabs: Vec::new(),
                        active: 0,
                        folded: Vec::new(),
                    });
                }
            }
            "folded" => {
                if let (Some(p), Some(name)) = (profiles.last_mut(), name) {
                    p.folded.push(name);
                }
            }
            "tab" => {
                if let Some(p) = profiles.last_mut().filter(|p| p.tabs.len() < SESSION_TABS) {
                    p.tabs.push(Tab {
                        index: number,
                        title: b.unwrap_or_default().to_string(),
                        ..Tab::default()
                    });
                }
            }
            "pinned" => {
                if let Some(tab) = profiles.last_mut().and_then(|p| p.tabs.last_mut()) {
                    tab.pinned = true;
                }
            }
            "group" => {
                if let Some(tab) = profiles.last_mut().and_then(|p| p.tabs.last_mut()) {
                    tab.group = name;
                }
            }
            "entry" => {
                let tab = profiles.last_mut().and_then(|p| p.tabs.last_mut());
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
    profiles.retain(|p| !p.id.is_empty());
    let mut seen = std::collections::HashSet::new();
    profiles.retain(|p| seen.insert(p.id.to_lowercase()));
    for p in &mut profiles {
        for tab in p.tabs.iter_mut() {
            tab.index = tab.index.min(tab.history.len().saturating_sub(1));
        }
    }
    Some(Session {
        profiles,
        visible,
        sidebar,
    })
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
    let control = |id: NodeId| match page.forms.controls.binary_search_by_key(&id, |c| c.node) {
        Ok(i) => page.forms.focusable(dom, i),
        Err(_) => false,
    };
    dom.descendants(html::Dom::ROOT)
        .filter(|&id| {
            (dom.tag_name(id) == Some("a") && dom.attr(id, "href").is_some() || control(id))
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
        let (size, key) = (style.font_size.to_f32(), layout::font_key(style));
        let top = t.baseline - metrics.ascent_em(key) * size;
        let rect = layout::Rect {
            x: t.x,
            y: top,
            width: t.width,
            height: t.baseline + metrics.descent_em(key) * size - top,
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
        clipboard: RefCell<String>,
        /// Full frames and header-only frames painted.
        frames: std::cell::Cell<usize>,
        headers: std::cell::Cell<usize>,
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

        fn copy(&self, text: &str) {
            *self.clipboard.borrow_mut() = text.to_string();
        }

        fn render(&self, width: usize, height: usize, draw: &mut DrawFrame) -> Result<(), String> {
            let stride = width * 4 + PAD;
            let mut pixels = vec![0u8; stride * height];
            {
                let old = self.frame.borrow();
                let previous = ((old.0, old.1) == (width, height)).then_some(old.2.as_slice());
                draw(&mut pixels, stride, previous);
            }
            *self.frame.borrow_mut() = (width, height, pixels);
            self.frames.set(self.frames.get() + 1);
            Ok(())
        }

        /// Paints the header into its own buffer, then copies it over the last frame, as
        /// the window's header layer covers it.
        fn render_header(
            &self,
            (left, top): (usize, usize),
            width: usize,
            height: usize,
            draw: &mut dyn FnMut(&mut [u8], usize),
        ) -> Result<bool, String> {
            let stride = width * 4 + PAD;
            let mut strip = vec![0u8; stride * height];
            draw(&mut strip, stride);
            let mut frame = self.frame.borrow_mut();
            let frame_stride = frame.0 * 4 + PAD;
            for y in 0..height {
                let to = (top + y) * frame_stride + left * 4;
                frame.2[to..to + width * 4]
                    .copy_from_slice(&strip[y * stride..y * stride + width * 4]);
            }
            self.headers.set(self.headers.get() + 1);
            Ok(true)
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
            panes: 1,
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
    fn clicks_land_where_transformed_links_are_drawn() {
        // A link moved 200px right: clicking where it was laid out does nothing, clicking
        // where it is drawn follows it.
        let (_, target) = site("transformed-links");
        let page = page_file(
            "moved-link.html",
            &format!(
                "<title>Moved</title><style>body {{ margin: 0 }} a {{ display: block; width: 80px;
                 height: 20px; transform: translateX(200px) }}</style>
                 <a href=\"{}\">Moved link</a>",
                target.display()
            ),
        );
        let mut b = browser(&page);
        let run = {
            let p = b.page.as_ref().expect("a page is loaded");
            let t = p
                .layout
                .texts
                .iter()
                .find(|t| p.layout.text(t).contains("Moved"))
                .unwrap();
            (t.x + 10.0, t.baseline - 4.0)
        };
        let at = |dx: f32| Event::Click {
            x: f64::from(SIDEBAR_WIDTH + run.0 + dx),
            y: f64::from(run.1 + HEADER_HEIGHT),
        };
        act(&mut b, at(0.0));
        assert_eq!(title(&b), "Moved", "nothing is drawn where it was laid out");
        act(&mut b, at(200.0));
        assert_eq!(title(&b), "Page B", "the link follows where it is drawn");
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
        assert!(act(
            &mut b,
            Event::Scroll {
                dy: 10_000.0,
                x: 400.0,
                y: 100.0
            }
        ));
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

    /// A page in its own temporary folder.
    fn page_file(name: &str, html: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("broxee-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("page.html");
        std::fs::write(&path, html).unwrap();
        path
    }

    /// The window point at the middle of the first box drawn for an element with `id`.
    fn box_center(b: &Browser<FakeScreen>, id: &str) -> (f64, f64) {
        let page = b.page.as_ref().unwrap();
        let r = page
            .layout
            .boxes
            .iter()
            .find(|f| page.dom.attr(f.node, "id") == Some(id))
            .expect("a box for the element")
            .rect;
        (
            f64::from(SIDEBAR_WIDTH + r.x + r.width / 2.0),
            f64::from(HEADER_HEIGHT + r.y + r.height / 2.0 - b.scroll),
        )
    }

    fn red(p: [u8; 4]) -> bool {
        // The frame is BGRA.
        p[2] > 200 && p[1] < 60 && p[0] < 60
    }

    #[test]
    fn hover_restyles_a_link_and_only_when_a_rule_can_see_it() {
        let page = page_file(
            "hover",
            "<style>body { margin: 0 } p { margin: 0; line-height: 20px; width: 200px }
             a { color: blue } a:hover { background: rgb(255, 0, 0) }
             span { padding: 2px }</style>
             <p id=plain>plain text</p><p id=other>more text</p>
             <p><a id=link href=x.html><span id=inner>A link</span></a></p>",
        );
        let mut b = browser(&page);
        let restyles = |b: &Browser<FakeScreen>| b.page.as_ref().unwrap().restyles;
        let hover =
            |b: &mut Browser<FakeScreen>, (x, y): (f64, f64)| b.handle(Event::Hover { x, y });

        // Between two paragraphs no rule can see the change: nothing is styled again.
        let plain = box_center(&b, "plain");
        let other = box_center(&b, "other");
        hover(&mut b, plain);
        hover(&mut b, other);
        assert_eq!(restyles(&b), 0);

        let link = box_center(&b, "inner");
        assert!(hover(&mut b, link), "entering the link repaints");
        assert_eq!(restyles(&b), 1);
        b.render();
        // Inside the span's left padding: background only, whatever fonts draw the text.
        let (cx, y) = box_center(&b, "link");
        let page = b.page.as_ref().unwrap();
        let left = page
            .layout
            .boxes
            .iter()
            .find(|f| page.dom.attr(f.node, "id") == Some("link"))
            .unwrap()
            .rect
            .x;
        let x = f64::from(SIDEBAR_WIDTH + left + 1.0);
        assert!(x < cx);
        assert!(
            red(b.window.pixel(x as usize, y as usize)),
            "a:hover draws its background"
        );
        assert!(
            !hover(&mut b, link),
            "moving within the link changes nothing"
        );
        assert_eq!(restyles(&b), 1);

        hover(&mut b, plain);
        assert_eq!(restyles(&b), 2);
        b.render();
        assert!(
            !red(b.window.pixel(x as usize, y as usize)),
            "and loses it on leaving"
        );
    }

    #[test]
    fn a_page_without_state_rules_never_restyles_for_the_pointer() {
        let page = page_file(
            "nohover",
            "<style>body { margin: 0 } a { color: blue; padding: 1px }</style>
             <p id=p>text</p><p><a id=a href=x.html>link</a></p><input id=i>",
        );
        let mut b = browser(&page);
        assert_eq!(b.page.as_ref().unwrap().styles.states_used(), 0);
        for id in ["p", "a", "i", "p"] {
            let (x, y) = box_center(&b, id);
            b.handle(Event::Hover { x, y });
        }
        let (x, y) = box_center(&b, "i");
        act(&mut b, Event::Click { x, y });
        act(&mut b, Event::Release { x, y });
        let page = b.page.as_ref().unwrap();
        assert_eq!(page.restyles, 0);
        assert_eq!(page.states.hover, None, "the pointer is not even looked up");
    }

    #[test]
    fn focus_visible_outlines_a_field_reached_with_the_keyboard() {
        let page = page_file(
            "focus",
            "<style>body { margin: 0 } input, button { margin: 20px }
             input:focus-visible { outline: 3px solid rgb(255, 0, 0) }
             button:focus-visible { outline: 3px solid rgb(255, 0, 0) }
             button:active { background: rgb(0, 0, 255) }
             form:focus-within { padding-left: 7px }</style>
             <form id=f><input id=i><button id=go type=button>Go</button></form>",
        );
        let mut b = browser(&page);
        let outline_point = |b: &Browser<FakeScreen>, id: &str| {
            let page = b.page.as_ref().unwrap();
            let r = page
                .layout
                .boxes
                .iter()
                .find(|f| page.dom.attr(f.node, "id") == Some(id))
                .unwrap()
                .rect;
            (
                (SIDEBAR_WIDTH + r.x - 2.0) as usize,
                (HEADER_HEIGHT + r.y + r.height / 2.0) as usize,
            )
        };
        // Tab goes through the header first, then into the page.
        for _ in 0..6 {
            if b.link_focus.is_some() {
                break;
            }
            act(&mut b, key(Key::Tab));
        }
        let node_of = |b: &Browser<FakeScreen>, id: &str| {
            let dom = &b.page.as_ref().unwrap().dom;
            dom.descendants(html::Dom::ROOT)
                .find(|&n| dom.attr(n, "id") == Some(id))
                .expect("an element with that id")
        };
        let field = node_of(&b, "i");
        assert_eq!(b.link_focus.as_ref().map(|l| l.node), Some(field));
        let states = b.page.as_ref().unwrap().states;
        assert!(states.focus_visible);
        b.render();
        let (x, y) = outline_point(&b, "i");
        assert!(
            red(b.window.pixel(x, y)),
            "the outline is drawn outside the field"
        );
        let page = b.page.as_ref().unwrap();
        let field_box = page.layout.boxes.iter().find(|f| f.node == field).unwrap();
        assert_eq!(
            field_box.rect.x,
            20.0 + 7.0,
            "form:focus-within pads the form"
        );

        // A button pressed with the pointer: :active while down, and no visible focus.
        let go = node_of(&b, "go");
        let (bx, by) = box_center(&b, "go");
        act(&mut b, Event::Click { x: bx, y: by });
        let page = b.page.as_ref().unwrap();
        assert_eq!(
            page.styles.style(go).background_color,
            css::Color::rgb(0, 0, 255)
        );
        act(&mut b, Event::Release { x: bx, y: by });
        let page = b.page.as_ref().unwrap();
        assert_ne!(
            page.styles.style(go).background_color,
            css::Color::rgb(0, 0, 255)
        );
        assert_eq!(page.styles.style(go).outline_width, css::Px::ZERO);
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
        assert_eq!(b.focus, Some(Button::More), "then ⋯");
        assert!(act(&mut b, tab(false)));
        assert!(b.link_focus.is_some(), "then the page's first link");
        assert!(act(&mut b, tab(true)));
        assert_eq!(b.focus, Some(Button::More), "Shift+Tab goes backwards");
        assert!(act(&mut b, tab(true)));
        assert!(b.field.is_some());

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
        act(&mut b, tab(false)); // ⋯
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

    #[test]
    fn interface_text_is_figtree_and_modifiers_are_glyphs() {
        use font::FontKey;
        let page = page_file("fonts-ui.html", "<title>Fonts</title><p>Page text</p>");
        let mut b = browser_with(&page, None);
        act(&mut b, command('k'));
        assert!(b.palette.is_some());
        b.render();
        let fonts = b.fonts.clone();
        let ui = fonts.ui();
        let figtree = font::Font::parse_any(font::FIGTREE_REGULAR).unwrap();
        assert_eq!(
            ui.advance_em('a', FontKey::UI),
            figtree.advance_em('a'),
            "interface text is measured with Figtree"
        );
        // Figtree has no ⌘; the system's symbol font draws it where there is one.
        let symbol_font = std::path::Path::new("/System/Library/Fonts/Keyboard.ttf").exists();
        let drawn = ui.glyph('⌘', FontKey::UI).is_some_and(|(_, g)| g != 0);
        assert_eq!(drawn, symbol_font);
        if let Ok(path) = std::env::var("FONTS_PNG") {
            let frame = b.window.frame.borrow();
            let stride = frame.0 * 4 + PAD;
            let rgba: Vec<u8> = (0..frame.1)
                .flat_map(|y| {
                    frame.2[y * stride..y * stride + frame.0 * 4]
                        .chunks(4)
                        .flat_map(|p| [p[2], p[1], p[0], 255])
                        .collect::<Vec<_>>()
                })
                .collect();
            std::fs::write(path, paint::png::encode(frame.0, frame.1, &rgba)).unwrap();
        }
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
        let session = parse_session(
            "broxee-session 1\nvisible\t7\nentry\t0\thttps://orphan.example/\n\
             profile\t0\tPersonal\ntab\t9\tKept\nentry\tNaN\thttps://a.example/\n\
             entry\t5\tnot a url\nbogus line\n",
        )
        .unwrap();
        assert_eq!(session.visible, 7, "clamped by the caller");
        assert_eq!(session.sidebar, None);
        let tab = &session.profiles[0].tabs[0];
        assert_eq!(
            (tab.title.as_str(), tab.index, tab.history.len()),
            ("Kept", 0, 1)
        );
        assert_eq!(tab.history[0].scroll, 0.0);

        let mut personal = Profile::new("Personal", "Personal", Hue::Emerald);
        personal.tabs[0].title = "A\ttab\nname".into();
        personal.tabs[0].history.push(Entry {
            url: net::Url::parse("https://a.example/x").unwrap(),
            scroll: 12.5,
            title: "Page\tX".into(),
        });
        personal.tabs[0].pinned = true;
        personal.tabs.push(Tab {
            group: Some("Re\tsearch".into()),
            ..Tab::default()
        });
        personal.folded.push("Research".into());
        let text = write_session(&[personal], 0, (999.0, false));
        let session = parse_session(&text).unwrap();
        assert_eq!(
            session.sidebar,
            Some((SIDEBAR_MAX, false)),
            "width kept in range"
        );
        let saved = &session.profiles[0];
        assert_eq!(saved.folded, ["Research"]);
        assert!(saved.tabs[0].pinned && saved.tabs[1].group.as_deref() == Some("Re search"));
        let tab = &saved.tabs[0];
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
        // The server has not answered: a window waiting on it would not get here at all. The
        // bound only guards against a regression that waits for the network's timeout.
        assert!(started.elapsed() < Duration::from_secs(2));
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

    #[test]
    fn typing_in_the_address_field_repaints_only_the_header() {
        let (a, _) = site("header-layer");
        let mut b = browser(&a);
        b.render();
        assert_eq!(b.apply(command('l')), Repaint::Frame, "focusing the field");
        b.repaint(Repaint::Frame);
        let frames = b.window.frames.get();
        for c in "example.org".chars() {
            let r = b.apply(key(Key::Char(c)));
            assert_eq!(r, Repaint::Header);
            b.repaint(r);
        }
        for k in [Key::Left, Key::Backspace, Key::Home, Key::End] {
            assert_eq!(b.apply(key(k)), Repaint::Header);
            b.repaint(Repaint::Header);
        }
        assert_eq!(b.apply(Event::Paste("x".into())), Repaint::Header);
        b.repaint(Repaint::Header);
        assert_eq!(
            b.window.frames.get(),
            frames,
            "the page surface is untouched"
        );
        assert_eq!(b.window.headers.get(), 16);
        // What the header layer shows over the old frame equals a full repaint.
        let layered = b.window.frame.borrow().2.clone();
        b.render();
        assert!(
            layered == b.window.frame.borrow().2,
            "header strip matches a full frame"
        );
        assert_eq!(
            b.apply(key(Key::Escape)),
            Repaint::Header,
            "closing the field"
        );

        // Loading progress moves only the loading line.
        b.loading = Some(0.5);
        let frames = b.window.frames.get();
        b.repaint(Repaint::Header);
        assert_eq!(b.window.frames.get(), frames);
        // With the menu open, the header is drawn as part of a full frame.
        b.loading = None;
        assert!(b.open_menu(false));
        b.repaint(Repaint::Header);
        assert_eq!(
            b.window.frames.get(),
            frames + 1,
            "the menu overlaps the header"
        );
    }

    /// A local server for form tests: `/` is a page with a GET form and a POST form; other
    /// requests answer with their request line and body as the title.
    fn form_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let done = text.find("\r\n\r\n").is_some_and(|end| {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        buf.len() >= end + 4 + len
                    });
                    if done || n == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or("").to_string();
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                let page = if line.starts_with("GET / ") {
                    "<!doctype html><style>body { margin: 0 }</style>\
                     <form action=/s><input name=q><input type=checkbox name=c>\
                     <input type=submit value=Go></form>\
                     <form method=post action=/p><input type=password name=pw></form>"
                        .to_string()
                } else {
                    let path = line.split(' ').nth(1).unwrap_or("");
                    let method = line.split(' ').next().unwrap_or("");
                    format!("<title>{method} {path} {body}</title>")
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        format!("http://{address}/")
    }

    /// Click the middle of control `i` of the page on screen.
    fn click_control(b: &mut Browser<FakeScreen>, i: usize) -> bool {
        let page = b.page.as_ref().expect("a page");
        let node = page.forms.controls[i].node;
        let r = page
            .layout
            .boxes
            .iter()
            .find(|x| x.node == node)
            .expect("the control has a box")
            .rect;
        let (x, y) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
        let event = Event::Click {
            x: f64::from(x + b.pane_left()),
            y: f64::from(y + HEADER_HEIGHT - b.scroll),
        };
        act(b, event)
    }

    #[test]
    fn fills_in_and_submits_forms() {
        let url = form_server();
        let mut b = browser(Path::new(&url));
        let value = |b: &Browser<FakeScreen>, i: usize| {
            b.page.as_ref().unwrap().forms.controls[i].value.clone()
        };
        assert!(click_control(&mut b, 0), "clicking the field focuses it");
        assert_eq!(b.focused_control(), Some(0));
        type_text(&mut b, "hello wörld");
        act(&mut b, key(Key::Left));
        act(&mut b, key(Key::Backspace));
        assert_eq!(value(&b, 0), "hello wörd");
        let shown = b.page.as_ref().unwrap();
        assert_eq!(
            shown.dom.text_content(shown.forms.controls[0].node),
            "hello wörd"
        );
        b.render();
        let metrics = b.page.as_ref().unwrap().metrics(&b.fonts);
        assert!(b.caret(&metrics).is_some(), "a caret is drawn");

        assert!(click_control(&mut b, 1));
        assert!(
            b.page.as_ref().unwrap().forms.controls[1].checked,
            "click checks the box"
        );
        assert_eq!(b.apply(key(Key::Char(' '))), Repaint::Frame);
        assert!(
            !b.page.as_ref().unwrap().forms.controls[1].checked,
            "Space unchecks it"
        );
        b.apply(key(Key::Char(' ')));

        // Back in the field, Enter submits with GET.
        click_control(&mut b, 0);
        act(&mut b, key(Key::Enter));
        assert_eq!(
            b.window.title.borrow().trim(),
            "GET /s?q=hello+w%C3%B6rd&c=on"
        );
        assert_eq!(
            b.tab().entry().unwrap().url.path,
            "/s?q=hello+w%C3%B6rd&c=on"
        );

        // The POST form, reached with Tab from the address field.
        act(&mut b, command('['));
        let i = b.page.as_ref().unwrap().forms.controls.len() - 1;
        click_control(&mut b, i);
        type_text(&mut b, "s3cret pw");
        act(&mut b, key(Key::Enter));
        assert_eq!(b.window.title.borrow().trim(), "POST /p pw=s3cret+pw");
        assert_eq!(
            b.tab().entry().unwrap().url.path,
            "/p",
            "the body is not in the history"
        );
    }

    impl Browser<FakeScreen> {
        /// A point inside the first text run of the first `tag` element: x, y in page points.
        fn link_rect_of(&self, tag: &str) -> (f32, f32) {
            let p = self.page.as_ref().unwrap();
            let node = p.dom.find_first(tag).unwrap();
            let inside: Vec<NodeId> = p.dom.descendants(node).collect();
            let t = p
                .layout
                .texts
                .iter()
                .find(|t| inside.contains(&t.node))
                .unwrap();
            (t.x + 2.0, t.baseline - 4.0)
        }
    }

    /// The pixels of a pane's area in the last frame.
    fn pane_pixels(b: &Browser<FakeScreen>, j: usize) -> Vec<u8> {
        let frame = b.window.frame.borrow();
        let (x, y, w, h) = b.pane_rects_px(frame.0, frame.1)[j];
        let stride = frame.0 * 4 + PAD;
        (y..y + h)
            .flat_map(|row| frame.2[row * stride + x * 4..row * stride + (x + w) * 4].to_vec())
            .collect()
    }

    #[test]
    fn splits_into_panes_with_one_live_page() {
        let (a, bpath) = site("split");
        let mut b = browser(&a);
        // A second tab with page B, then back to A.
        act(&mut b, command('t'));
        b.close_palette();
        let url = engine::to_url(&bpath.display().to_string()).unwrap();
        b.navigate(url, Destination::Push);
        b.wait_for_load();
        act(&mut b, command('1'));
        assert_eq!(b.tab().title(), "Page A");

        assert!(b.apply(command_shift('2')) == Repaint::Frame, "⌘⇧2");
        b.render();
        assert_eq!(b.panes.len(), 2);
        let (_, _, w, _) = b.pane_rects()[0];
        assert_eq!(
            b.settings().width,
            w,
            "the page is laid out at the pane's width"
        );
        assert!(w < 200.0);
        let left_before = pane_pixels(&b, 0);

        // A click in the right pane gives it focus: B wakes there, A's pixels stay.
        let (x1, _, _, _) = b.pane_rects()[1];
        assert!(act(
            &mut b,
            Event::Click {
                x: f64::from(x1) + 40.0,
                y: 120.0
            }
        ));
        assert_eq!(b.focused, 1);
        assert_eq!(b.tab().title(), "Page B");
        b.render();
        assert!(
            pane_pixels(&b, 0) == left_before,
            "the left pane is copied, not rebuilt"
        );
        let (_, _, w1, _) = b.pane_rects()[1];
        assert_eq!(b.settings().width, w1);

        // The right pane switches profile; the left keeps its own.
        act(&mut b, command_shift('p'));
        assert_eq!(b.profile, 1);
        let x0 = f64::from(b.pane_rects()[0].0);
        act(
            &mut b,
            Event::Click {
                x: x0 + 40.0,
                y: 120.0,
            },
        );
        assert_eq!((b.focused, b.profile), (0, 0));
        assert_eq!(b.tab().title(), "Page A");
        assert_eq!(
            b.panes[1].profile, 1,
            "the right pane remembers its profile"
        );

        // Four panes, then back to one: the focused pane's tab stays.
        act(&mut b, command_shift('4'));
        assert_eq!(b.panes.len(), 4);
        b.render();
        if let Ok(path) = std::env::var("SPLIT_PNG") {
            let frame = b.window.frame.borrow();
            let stride = frame.0 * 4 + PAD;
            let rgba: Vec<u8> = (0..frame.1)
                .flat_map(|y| {
                    frame.2[y * stride..y * stride + frame.0 * 4]
                        .chunks(4)
                        .flat_map(|p| [p[2], p[1], p[0], 255])
                        .collect::<Vec<_>>()
                })
                .collect();
            std::fs::write(path, paint::png::encode(frame.0, frame.1, &rgba)).unwrap();
        }
        let rects = b.pane_rects();
        assert!(rects[2].1 > 100.0, "a 2x2 grid");
        act(&mut b, command_shift('1'));
        assert_eq!(b.panes.len(), 1);
        assert_eq!(b.tab().title(), "Page A");
        assert_eq!(b.settings().width, 640.0 - b.sidebar_edge());
    }

    #[test]
    fn runs_page_scripts_on_load_click_and_timer() {
        let dir = std::env::temp_dir().join(format!("broxee-js-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("js.html");
        std::fs::write(
            &page,
            "<!doctype html><style>body { margin: 0 } .big { font-size: 40px }</style>\
             <title>Before</title><button id=b>0</button><p><a href=other.html id=l>stay</a></p>\
             <script>document.title = 'Counter'; let n = 0;\
             const b = document.getElementById('b');\
             b.addEventListener('click', () => { b.textContent = ++n; b.classList.add('big') });\
             document.getElementById('l').addEventListener('click', e => e.preventDefault());\
             setTimeout(() => document.querySelector('p').append(' and later'), 30);</script>",
        )
        .unwrap();
        let mut b = browser(&page);
        assert_eq!(
            b.tab().title(),
            "Counter",
            "the script set the title before the tab took it"
        );
        let node_text = |b: &Browser<FakeScreen>, tag: &str| {
            let p = b.page.as_ref().unwrap();
            p.dom.text_content(p.dom.find_first(tag).unwrap())
        };
        let button = {
            let p = b.page.as_ref().unwrap();
            let node = p.dom.find_first("button").unwrap();
            p.layout.boxes.iter().find(|x| x.node == node).unwrap().rect
        };
        let width_before = button.width;
        let click = Event::Click {
            x: f64::from(button.x + 3.0 + b.pane_left()),
            y: f64::from(button.y + button.height / 2.0 + HEADER_HEIGHT),
        };
        assert!(act(&mut b, click));
        assert_eq!(node_text(&b, "button"), "1");
        let wider = {
            let p = b.page.as_ref().unwrap();
            let node = p.dom.find_first("button").unwrap();
            p.layout
                .boxes
                .iter()
                .find(|x| x.node == node)
                .unwrap()
                .rect
                .width
        };
        assert!(
            wider > width_before,
            "the new class was styled and laid out"
        );

        // The link's listener prevents navigation.
        let entries = b.tab().history.len();
        let link = b.link_rect_of("a");
        let at = Event::Click {
            x: f64::from(link.0 + b.pane_left()),
            y: f64::from(link.1 + HEADER_HEIGHT),
        };
        act(&mut b, at);
        assert_eq!(
            b.tab().history.len(),
            entries,
            "preventDefault kept the page"
        );
        assert!(b.load.is_none());

        // The timer, as the run loop would fire it.
        assert!(b.next_page_timer().is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(b.run_page_timers());
        assert!(node_text(&b, "p").ends_with("and later"));
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
        assert!(matches!(
            b.dialog.as_ref().unwrap().kind,
            DialogKind::Bookmark {
                existing: Some(1),
                ..
            }
        ));
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

    /// The tab list as labels in brackets and tab titles, as drawn.
    fn sidebar_text(b: &Browser<FakeScreen>) -> Vec<String> {
        b.sidebar_rows()
            .rows
            .iter()
            .map(|r| match r {
                ListRow::Label(l) => format!("[{l}]"),
                ListRow::Item { title, .. } => title.to_string(),
            })
            .collect()
    }

    fn row_center(row: usize) -> f64 {
        f64::from(chrome::LIST_TOP + chrome::TAB_ROW * row as f32 + 15.0)
    }

    fn run_palette(b: &mut Browser<FakeScreen>, text: &str) {
        act(b, command('k'));
        type_text(b, text);
        act(b, key(Key::Enter));
    }

    #[test]
    fn tabs_can_be_pinned_grouped_folded_dragged_and_restored() {
        let (a, bpath) = site("organise");
        let session = a.with_file_name("organise-session");
        let _ = std::fs::remove_file(&session);
        let mut b = browser_with(&a, Some(session.clone()));
        act(&mut b, command('t'));
        open(&mut b, &bpath);
        act(&mut b, command('t'));
        open(&mut b, &a);
        assert_eq!(sidebar_text(&b), ["Page A", "Page B", "Page A"]);

        // Pinning moves the tab to the top, under a label, and ⌘1 follows the new order.
        run_palette(&mut b, "pin or unpin");
        assert_eq!(
            sidebar_text(&b),
            ["[Pinned]", "Page A", "[Tabs]", "Page A", "Page B"]
        );
        assert_eq!(b.profiles[0].active, 0, "the pinned tab stays active");

        // A group, through its dialog.
        act(&mut b, command('2'));
        run_palette(&mut b, "move this tab");
        assert!(matches!(b.dialog.as_ref().unwrap().kind, DialogKind::Group));
        type_text(&mut b, "Research");
        act(&mut b, key(Key::Enter));
        assert_eq!(
            sidebar_text(&b),
            [
                "[Pinned]",
                "Page A",
                "[Research]",
                "Page A",
                "[Tabs]",
                "Page B"
            ]
        );

        // Folding hides the group's tabs except the active one; a click on the label folds.
        act(&mut b, command('3'));
        assert!(act(
            &mut b,
            Event::Click {
                x: 100.0,
                y: row_center(2)
            }
        ));
        assert_eq!(
            sidebar_text(&b),
            ["[Pinned]", "Page A", "[Research (1)]", "[Tabs]", "Page B"]
        );
        assert!(act(
            &mut b,
            Event::Click {
                x: 100.0,
                y: row_center(2)
            }
        ));
        assert_eq!(sidebar_text(&b).len(), 6, "unfolded");

        // Dragging Page B (row 5) to just under the Research label joins the group.
        act(
            &mut b,
            Event::Click {
                x: 100.0,
                y: row_center(5),
            },
        );
        assert!(act(
            &mut b,
            Event::Drag {
                x: 100.0,
                y: row_center(5) - 20.0
            }
        ));
        assert!(act(
            &mut b,
            Event::Drag {
                x: 100.0,
                y: row_center(3) - 15.0
            }
        ));
        assert!(matches!(b.drag, Some(Drag::Moving { drop: 3, .. })));
        b.render();
        assert!(act(
            &mut b,
            Event::Release {
                x: 100.0,
                y: row_center(3) - 15.0
            }
        ));
        assert_eq!(
            sidebar_text(&b),
            ["[Pinned]", "Page A", "[Research]", "Page B", "Page A"]
        );
        assert_eq!(
            title(&b),
            "Page B",
            "the dragged tab is still the one shown"
        );

        // Resizing by the edge lays the page out once, at the end.
        assert!(!act(&mut b, Event::Click { x: 238.0, y: 200.0 }));
        assert!(act(&mut b, Event::Drag { x: 300.0, y: 200.0 }));
        assert!(act(&mut b, Event::Drag { x: 900.0, y: 200.0 }));
        assert_eq!(b.sidebar_width, SIDEBAR_MAX);
        assert!(act(&mut b, Event::Release { x: 900.0, y: 200.0 }));
        assert_eq!(b.page.as_ref().unwrap().layout.width, 640.0 - SIDEBAR_MAX);
        drop(b);

        // All of it comes back.
        let b = browser_with(Path::new(""), Some(session));
        assert_eq!(b.sidebar_width, SIDEBAR_MAX);
        assert_eq!(
            sidebar_text(&b),
            ["[Pinned]", "Page A", "[Research]", "Page B", "Page A"]
        );
    }

    #[test]
    fn the_sidebar_scrolls_and_keeps_the_active_tab_in_view() {
        let (a, _) = site("sidebar-scroll");
        let mut b = browser(&a);
        for _ in 0..12 {
            act(&mut b, command('t'));
            act(&mut b, key(Key::Escape));
        }
        // 13 tabs and "+ New tab" in 218 points: the list scrolls, and showed the new tab.
        assert!(b.sidebar_scroll > 0.0);
        let bottom = b.sidebar_scroll;
        assert!(act(&mut b, command('1')));
        assert_eq!(b.sidebar_scroll, 0.0, "⌘1 scrolls back to the first tab");
        assert!(act(
            &mut b,
            Event::Scroll {
                dy: -1000.0,
                x: 100.0,
                y: 200.0
            }
        ));
        // Revealing a tab shows its row at the bottom edge; the wheel goes on to the footer.
        assert_eq!(bottom, chrome::TAB_ROW * 13.0 - (300.0 - chrome::LIST_TOP));
        let max = chrome::sidebar_max_scroll(13, true, 300.0);
        assert_eq!(
            b.sidebar_scroll, max,
            "the wheel scrolls to the end, no further"
        );
        assert_eq!(b.scroll, 0.0, "the page did not scroll");
        b.render();
        // Clicks land on the rows as scrolled.
        let first_visible = (b.sidebar_scroll / chrome::TAB_ROW).ceil() as usize;
        let y = row_center(first_visible) - f64::from(b.sidebar_scroll);
        assert!(act(&mut b, Event::Click { x: 100.0, y }));
        assert_eq!(b.profiles[0].active, first_visible);
    }

    #[test]
    fn profiles_can_be_added_renamed_recoloured_and_removed() {
        let (a, bpath) = site("manage-profiles");
        let session = a.with_file_name("manage-session");
        let _ = std::fs::remove_file(&session);
        let _ = std::fs::remove_dir_all(a.with_file_name("profiles"));
        let mut b = browser_with(&a, Some(session.clone()));

        // A new profile: named in a dialog, given the first unused color, shown at once.
        run_palette(&mut b, "new profile");
        type_text(&mut b, "work");
        act(&mut b, key(Key::Enter));
        let error = b.dialog.as_ref().and_then(|d| d.error.clone());
        assert_eq!(
            error.as_deref(),
            Some("There is already a profile called work.")
        );
        for _ in 0..4 {
            act(&mut b, key(Key::Backspace));
        }
        type_text(&mut b, "Side project");
        act(&mut b, key(Key::Enter));
        assert!(b.dialog.is_none());
        let names: Vec<&str> = b.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Personal", "Work", "Side project"]);
        assert_eq!(b.profile, 2);
        assert_eq!(
            (b.profiles[2].hue, b.profiles[2].id.as_str()),
            (Hue::Rose, "profile-3")
        );
        act(&mut b, key(Key::Escape));
        open(&mut b, &bpath);
        b.render();
        let rose = b.chrome.theme().profile(Hue::Rose);
        assert_eq!(
            b.window.pixel(440, 0),
            [rose.b, rose.g, rose.r, 255],
            "its edge"
        );

        // Rename and recolor; the folder stays.
        run_palette(&mut b, "rename this");
        type_text(&mut b, "Projects");
        act(&mut b, key(Key::Enter));
        run_palette(&mut b, "change this profile");
        assert_eq!(
            (b.profiles[2].name.as_str(), b.profiles[2].hue),
            ("Projects", Hue::Violet)
        );
        assert_eq!(b.profiles[2].id, "profile-3");

        // ⌘⇧P cycles through all three; the palette switches by name.
        act(&mut b, command_shift('p'));
        assert_eq!(b.profile, 0);
        act(&mut b, key(Key::Escape));
        run_palette(&mut b, "switch to projects");
        assert_eq!(b.profile, 2);
        assert_eq!(title(&b), "Page B");
        drop(b);

        // Back after a restart, with names, colors and tabs.
        let mut b = browser_with(Path::new(""), Some(session.clone()));
        let names: Vec<&str> = b.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Personal", "Work", "Projects"]);
        assert_eq!(b.profiles[2].hue, Hue::Violet);
        assert_eq!((b.profile, title(&b).as_str()), (2, "Page B"));

        // Removing asks for the name, then deletes the profile's folder.
        b.profiles[2].cookies.store(
            &net::Url::parse("https://a.example/").unwrap(),
            "k=1; Max-Age=60",
            &net::Context::top_level(true),
        );
        b.save_cookies();
        let folder = a.with_file_name("profiles").join("profile-3");
        assert!(folder.exists());
        run_palette(&mut b, "remove this profile");
        type_text(&mut b, "projects");
        act(&mut b, key(Key::Enter));
        assert!(
            b.dialog.as_ref().is_some_and(|d| d.error.is_some()),
            "exact name only"
        );
        for _ in 0..8 {
            act(&mut b, key(Key::Backspace));
        }
        type_text(&mut b, "Projects");
        act(&mut b, key(Key::Enter));
        assert_eq!(b.profiles.len(), 2);
        assert!(!folder.exists(), "its cookies went with it");
        assert_eq!(b.profile, 1);

        // The last profile cannot go. (Work's new tab opened the palette: close it first.)
        act(&mut b, key(Key::Escape));
        run_palette(&mut b, "remove this profile");
        type_text(&mut b, "Work");
        act(&mut b, key(Key::Enter));
        run_palette(&mut b, "remove this profile");
        type_text(&mut b, "Personal");
        act(&mut b, key(Key::Enter));
        let error = b.dialog.as_ref().and_then(|d| d.error.clone());
        assert_eq!(error.as_deref(), Some("Broxee needs at least one profile."));
    }

    #[test]
    fn session_profile_folders_are_never_trusted() {
        let session = parse_session(
            "broxee-session 1\nprofile\t0\tEvil\t../../etc\trose\ntab\t0\tStolen\n\
             profile\t0\tFine\tprofile-2\tviolet\nprofile\t0\tTwin\tPROFILE-2\tamber\n\
             profile\t0\tOld\n",
        )
        .unwrap();
        let ids: Vec<&str> = session.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["profile-2", "Old"],
            "unsafe and duplicate folders dropped"
        );
        assert!(
            session.profiles[0].tabs.is_empty(),
            "the bad profile's tab did not move over"
        );
        assert_eq!(session.profiles[0].hue, Hue::Violet);
        assert!(!valid_id("a/b") && !valid_id("..") && !valid_id("") && valid_id("Work_2-x"));
    }

    #[test]
    fn the_more_menu_copies_opens_in_another_profile_and_closes_tabs() {
        let (a, _) = site("menu");
        let mut b = browser(&a);
        let more = (f64::from(SIDEBAR_WIDTH) + 400.0 - 8.0 - 14.0, 18.0);

        // Mouse: the ⋯ button opens it; an item runs; outside closes.
        assert!(act(
            &mut b,
            Event::Click {
                x: more.0,
                y: more.1
            }
        ));
        let labels: Vec<&str> = b
            .menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|(l, _)| l.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "Copy address",
                "Open in Work",
                "Bookmark this page",
                "Reload",
                "Close tab"
            ]
        );
        b.render();
        // Rows from 34 + 6, 30 points each; the menu's right edge is 8 from the pane's.
        assert!(act(
            &mut b,
            Event::Click {
                x: more.0 - 50.0,
                y: 40.0 + 15.0
            }
        ));
        assert!(b.menu.is_none());
        assert!(b.window.clipboard.borrow().starts_with("file://"));
        assert!(b.window.clipboard.borrow().ends_with("a.html"));
        act(
            &mut b,
            Event::Click {
                x: more.0,
                y: more.1,
            },
        );
        assert!(act(
            &mut b,
            Event::Click {
                x: f64::from(SIDEBAR_WIDTH) + 20.0,
                y: 250.0
            }
        ));
        assert!(b.menu.is_none(), "a click outside closes it");

        // Keyboard: Tab to ⋯, Enter opens with the first item chosen, ↓ Enter runs the second.
        for _ in 0..3 {
            act(&mut b, key(Key::Tab));
        }
        assert_eq!(b.focus, Some(Button::More));
        act(&mut b, key(Key::Enter));
        assert_eq!(b.menu.as_ref().unwrap().selected, Some(0));
        act(&mut b, key(Key::Down));
        act(&mut b, key(Key::Enter));
        assert_eq!(b.profiles[b.profile].name, "Work");
        assert_eq!(title(&b), "Page A", "opened in a new tab of Work");
        assert_eq!(b.profiles[1].tabs.len(), 2);

        // A new tab's menu has only what makes sense there.
        act(&mut b, command('t'));
        act(&mut b, key(Key::Escape));
        act(
            &mut b,
            Event::Click {
                x: more.0,
                y: more.1,
            },
        );
        let labels: Vec<&str> = b
            .menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|(l, _)| l.as_str())
            .collect();
        assert_eq!(labels, ["Close tab"]);
        act(&mut b, key(Key::Escape));
        assert!(b.menu.is_none());
    }

    #[test]
    fn tooltips_wait_then_show_and_hide_on_any_input() {
        let (a, _) = site("tooltips");
        let mut b = browser(&a);
        act(&mut b, command('t'));
        act(&mut b, key(Key::Escape));
        // Pointer on the back button: nothing at first, the tooltip after the delay.
        let back = Event::Hover {
            x: f64::from(SIDEBAR_WIDTH) + 20.0,
            y: 18.0,
        };
        assert!(!b.handle(back.clone()));
        assert!(!b.tick(), "not yet");
        assert_eq!(b.hover.as_ref().unwrap().text, "Back ⌘[");
        b.hover.as_mut().unwrap().since -= TOOLTIP_DELAY;
        assert!(b.tick());
        b.render();
        // Moving within the same button keeps it; moving off hides it.
        assert!(!b.handle(Event::Hover {
            x: f64::from(SIDEBAR_WIDTH) + 22.0,
            y: 19.0
        }));
        assert!(b.hover.as_ref().unwrap().shown);
        assert!(b.handle(Event::Hover {
            x: f64::from(SIDEBAR_WIDTH) + 200.0,
            y: 200.0
        }));
        assert!(b.hover.is_none());

        // A sleeping tab says why it is quiet; the active tab has no tooltip.
        b.handle(Event::Hover {
            x: 100.0,
            y: row_center(0),
        });
        assert_eq!(
            b.hover.as_ref().unwrap().text,
            "Asleep to save memory. Select to wake."
        );
        b.handle(Event::Hover {
            x: 100.0,
            y: row_center(1),
        });
        assert!(b.hover.is_none());
        b.handle(Event::Hover {
            x: 220.0,
            y: row_center(1),
        });
        assert_eq!(b.hover.as_ref().unwrap().text, "Close tab ⌘W");

        // Any other input hides a shown tooltip.
        b.hover.as_mut().unwrap().since -= TOOLTIP_DELAY;
        b.tick();
        assert!(b.handle(key(Key::Down)) || b.hover.is_none());
        assert!(b.hover.is_none());
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
