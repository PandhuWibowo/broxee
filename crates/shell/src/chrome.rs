//! Broxee's own interface from DESIGN.md: the sidebar (profile switcher, the search field, tabs,
//! new tab), the pane header (back, forward, reload, the host and profile, the address field,
//! the loading line) and the ⌘K palette. Drawn with the engine's painter around the page.

use std::borrow::Cow;
use std::collections::HashMap;

use css::Color;
use font::{FontSet, TextMetrics};
use paint::{GlyphBitmap, Op, Painter, Rasterizer, Target};

/// Pane header height in points (DESIGN.md, section 7).
pub const HEADER_HEIGHT: f32 = 36.0;
const BUTTON_START: f32 = 8.0;
const BUTTON_WIDTH: f32 = 28.0;
const ICON: f32 = 16.0;
const FIELD_LEFT: f32 = 100.0;
/// The ⋯ button sits this far from the header's right edge.
const MORE_RIGHT: f32 = 8.0;
/// The host label and the address field stop before the ⋯ button.
const FIELD_RIGHT_GAP: f32 = MORE_RIGHT + BUTTON_WIDTH + 4.0;
/// The ⋯ menu.
const MENU_WIDTH: f32 = 230.0;
const MENU_ROW: f32 = 30.0;
const MENU_PAD: f32 = 6.0;
const FIELD_TOP: f32 = 5.0;
const FIELD_HEIGHT: f32 = 26.0;
const FIELD_PADDING: f32 = 10.0;

/// Sidebar width in points (DESIGN.md, section 7): 240 by default, resizable from 200 to 320.
pub const SIDEBAR_WIDTH: f32 = 240.0;
pub const SIDEBAR_MIN: f32 = 200.0;
pub const SIDEBAR_MAX: f32 = 320.0;
/// The resize handle: this many points inside the sidebar's edge, and one point past it, so
/// clicks on the page's left edge still reach the page.
const EDGE_GRAB: f32 = 5.0;
/// The profile switcher row at the top of the sidebar.
const PROFILE_ROW: f32 = 40.0;
/// "Search or go to ⌘K": opens the palette.
const SEARCH_TOP: f32 = 44.0;
const SEARCH_HEIGHT: f32 = 30.0;
const TABS_TOP: f32 = 82.0;
/// A tab row: at least the 28-point hit area of DESIGN.md, section 13.
pub const TAB_ROW: f32 = 30.0;
const ROW_INSET: f32 = 8.0;
const ROW_TEXT: f32 = 16.0;
/// The close button at the right end of the active tab row.
const CLOSE_SIZE: f32 = 24.0;
/// The profile edge (DESIGN.md, section 6).
const EDGE: f32 = 2.0;

/// The ⌘K palette, in points from the pane's left edge and the window's top.
const PALETTE_TOP: f32 = HEADER_HEIGHT + 12.0;
const PALETTE_MAX_WIDTH: f32 = 560.0;
const PALETTE_INPUT: f32 = 44.0;
const PALETTE_ROW: f32 = 44.0;
/// Results shown at once; the list scrolls to keep the selected one visible.
pub const PALETTE_ROWS: usize = 8;
const PALETTE_PAD: f32 = 6.0;
const PALETTE_TEXT: f32 = 16.0;
/// The one soft shadow of things that float (DESIGN.md, section 5).
const SHADOW_BLUR: f32 = 14.0;
const SHADOW_DROP: f32 = 6.0;
/// Modifier symbols drawn as icons: the UI font has no ⌘, ⇧ or ⌥.
const KEY_ICON: f32 = 12.0;

/// Profile colors (DESIGN.md, section 3, "Profile colors"), each checked to 4.5:1 on `canvas`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Emerald,
    Amber,
    Rose,
    Violet,
}

impl Hue {
    /// Every profile color, in the order new profiles take them.
    pub const ALL: [Hue; 4] = [Hue::Emerald, Hue::Amber, Hue::Rose, Hue::Violet];

    /// The name saved in the session file.
    pub fn key(self) -> &'static str {
        match self {
            Hue::Emerald => "emerald",
            Hue::Amber => "amber",
            Hue::Rose => "rose",
            Hue::Violet => "violet",
        }
    }

    pub fn from_key(key: &str) -> Option<Hue> {
        Hue::ALL.into_iter().find(|h| h.key() == key.trim())
    }
}

/// Color tokens from DESIGN.md, section 3. Every text pair was checked for WCAG AA there.
#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub canvas: Color,
    pub surface: Color,
    pub selected: Color,
    pub line: Color,
    pub text: Color,
    pub text_2: Color,
    pub muted: Color,
    pub accent: Color,
}

impl Theme {
    pub fn new(dark: bool) -> Theme {
        let hex = |v: u32| Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        if dark {
            Theme {
                dark,
                canvas: hex(0x070d18),
                surface: hex(0x0f1a2c),
                selected: hex(0x16233a),
                line: hex(0x1c2a40),
                text: hex(0xe6eef8),
                text_2: hex(0xa3b4c9),
                muted: hex(0x8a9cb4),
                accent: hex(0x38bdf8),
            }
        } else {
            Theme {
                dark,
                canvas: hex(0xeef4fa),
                surface: hex(0xffffff),
                selected: hex(0xdcebf7),
                line: hex(0xd3dfea),
                text: hex(0x0d1b2a),
                text_2: hex(0x4b5d71),
                muted: hex(0x5f7186),
                accent: hex(0x0369a1),
            }
        }
    }
}

impl Theme {
    pub fn profile(&self, hue: Hue) -> Color {
        let hex = |v: u32| Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        match (hue, self.dark) {
            (Hue::Emerald, false) => hex(0x047857),
            (Hue::Emerald, true) => hex(0x34d399),
            (Hue::Amber, false) => hex(0xb45309),
            (Hue::Amber, true) => hex(0xfbbf24),
            // Checked to 4.5:1 on `canvas`: rose 5.67 / 7.23, violet 6.41 / 7.15.
            (Hue::Rose, false) => hex(0xbe123c),
            (Hue::Rose, true) => hex(0xfb7185),
            (Hue::Violet, false) => hex(0x6d28d9),
            (Hue::Violet, true) => hex(0xa78bfa),
        }
    }
}

/// Something in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SidebarItem {
    /// The profile switcher.
    Profile,
    /// "Search or go to ⌘K".
    Search,
    /// A row of the list (a tab, a bookmark).
    Row(usize),
    /// The close button of a row.
    CloseRow(usize),
    /// The line after the list ("+ New tab").
    Footer,
    /// A section label (a tab group: clicking folds it).
    Label(usize),
    /// The sidebar's right edge, which resizes it.
    Edge,
}

/// The top of the list's first row, in points: rows are `TAB_ROW` apart below it.
pub const LIST_TOP: f32 = TABS_TOP;

/// How far the list can scroll: its rows (and footer) past the window's `height`.
pub fn sidebar_max_scroll(rows: usize, footer: bool, height: f32) -> f32 {
    let content = TAB_ROW * (rows + usize::from(footer)) as f32 + 8.0;
    (content - (height - TABS_TOP)).max(0.0)
}

/// What is at (x, y) in a sidebar `width` points wide whose list is scrolled by `scroll`, for
/// `rows` and, with `footer`, a line after them.
pub fn sidebar_hit(
    x: f32,
    y: f32,
    width: f32,
    scroll: f32,
    rows: &[ListRow],
    footer: bool,
) -> Option<SidebarItem> {
    if (width - EDGE_GRAB..width + 1.0).contains(&x) && y >= 0.0 {
        return Some(SidebarItem::Edge);
    }
    if !(0.0..width).contains(&x) || y < 0.0 {
        return None;
    }
    if y < PROFILE_ROW {
        return Some(SidebarItem::Profile);
    }
    if !(ROW_INSET..=width - ROW_INSET).contains(&x) {
        return None;
    }
    if (SEARCH_TOP..SEARCH_TOP + SEARCH_HEIGHT).contains(&y) {
        return Some(SidebarItem::Search);
    }
    if y < TABS_TOP {
        return None;
    }
    let row = ((y - TABS_TOP + scroll) / TAB_ROW) as usize;
    match rows.get(row) {
        Some(ListRow::Label(_)) => Some(SidebarItem::Label(row)),
        Some(ListRow::Item { closable, .. }) => {
            let on_close = x >= width - ROW_INSET - CLOSE_SIZE;
            Some(if *closable && on_close {
                SidebarItem::CloseRow(row)
            } else {
                SidebarItem::Row(row)
            })
        }
        None if footer && row == rows.len() => Some(SidebarItem::Footer),
        None => None,
    }
}

/// Where a row dragged to `y` would land: before row `n` of the list (`n == rows` for the end).
pub fn sidebar_drop(y: f32, scroll: f32, rows: usize) -> usize {
    (((y - TABS_TOP + scroll) / TAB_ROW).round().max(0.0) as usize).min(rows)
}

/// A row of the sidebar's list.
pub enum ListRow<'a> {
    /// A section label, 12px semibold in `muted` (DESIGN.md, section 4): "Pinned", a tab
    /// group, a bookmark folder.
    Label(Cow<'a, str>),
    Item {
        title: &'a str,
        /// Drawn in `muted`: a sleeping tab.
        muted: bool,
        /// With a close button at its right end.
        closable: bool,
    },
}

pub struct SidebarView<'a> {
    pub profile: &'a str,
    pub hue: Hue,
    pub rows: &'a [ListRow<'a>],
    /// The highlighted row: the active tab, or the bookmark chosen with the keyboard.
    pub selected: Option<usize>,
    /// The line after the list, if any ("+ New tab").
    pub footer: Option<&'a str>,
    /// How far the list is scrolled, in points.
    pub scroll: f32,
    /// While a row is dragged: the 2px line showing where it lands (before this row).
    pub drop_at: Option<usize>,
    /// Shown when the list is empty: a line, and a quieter one that may hold ⌘ keys.
    pub empty: Option<(&'a str, &'a str)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Back,
    Forward,
    Reload,
    Address,
    /// ⋯: the pane's menu.
    More,
}

/// Which header control is at `x` (points), for a header `width` points wide.
pub fn hit(x: f32, width: f32) -> Option<Button> {
    let slot = ((x - BUTTON_START) / BUTTON_WIDTH).floor();
    match slot as i32 {
        _ if x < BUTTON_START => None,
        0 => Some(Button::Back),
        1 => Some(Button::Forward),
        2 => Some(Button::Reload),
        _ if (width - MORE_RIGHT - BUTTON_WIDTH..width - MORE_RIGHT).contains(&x) => {
            Some(Button::More)
        }
        _ if x >= FIELD_LEFT && x <= width - FIELD_RIGHT_GAP => Some(Button::Address),
        _ => None,
    }
}

/// How many result rows fit in a window `height` points tall, at most `PALETTE_ROWS`, with a
/// 16-point margin under the palette.
pub fn palette_capacity(height: f32) -> usize {
    let room = height - PALETTE_TOP - 16.0 - PALETTE_INPUT - 1.0 - 2.0 * PALETTE_PAD;
    ((room / PALETTE_ROW).floor().max(1.0) as usize).min(PALETTE_ROWS)
}

/// Where the palette sits in a pane `pane_width` points wide with `rows` rows: x, y, w, h.
fn palette_rect(pane_width: f32, rows: usize) -> (f32, f32, f32, f32) {
    let w = PALETTE_MAX_WIDTH.min(pane_width - 32.0).max(160.0);
    let x = ((pane_width - w) / 2.0).max(0.0);
    let h = PALETTE_INPUT + 1.0 + 2.0 * PALETTE_PAD + PALETTE_ROW * rows.max(1) as f32;
    (x, PALETTE_TOP, w, h)
}

/// The ⋯ menu: under the header, its right edge on the ⋯ button's. x, y, w, h in points from
/// the pane's left edge and the window's top.
fn menu_rect(pane_width: f32, items: usize) -> (f32, f32, f32, f32) {
    let w = MENU_WIDTH.min(pane_width - 16.0).max(120.0);
    let x = (pane_width - MORE_RIGHT - w).max(0.0);
    (
        x,
        HEADER_HEIGHT - 2.0,
        w,
        2.0 * MENU_PAD + MENU_ROW * items as f32,
    )
}

/// What is at (x, y) over the ⋯ menu: `Some(Some(i))` on item `i`, `Some(None)` elsewhere on
/// it, `None` outside it. x from the pane's left edge.
pub fn menu_hit(x: f32, y: f32, pane_width: f32, items: usize) -> Option<Option<usize>> {
    let (mx, my, w, h) = menu_rect(pane_width, items);
    if !(mx..mx + w).contains(&x) || !(my..my + h).contains(&y) {
        return None;
    }
    let row = ((y - my - MENU_PAD) / MENU_ROW).floor();
    Some((row >= 0.0 && (row as usize) < items).then_some(row as usize))
}

pub struct MenuView<'a> {
    pub items: &'a [&'a str],
    pub selected: Option<usize>,
}

/// A dialog: where the palette sits, `DIALOG_WIDTH` wide at most.
const DIALOG_WIDTH: f32 = 420.0;
const DIALOG_PAD: f32 = 16.0;
/// From the dialog's top to its first field's label.
const DIALOG_HEAD: f32 = 48.0;
/// One labelled field: label, box and the gap after it.
const DIALOG_ROW: f32 = 62.0;
const DIALOG_FIELD: f32 = 32.0;

fn dialog_rect(pane_width: f32, fields: usize) -> (f32, f32, f32, f32) {
    let w = DIALOG_WIDTH.min(pane_width - 32.0).max(160.0);
    let x = ((pane_width - w) / 2.0).max(0.0);
    let h = DIALOG_HEAD + DIALOG_ROW * fields as f32 + 20.0;
    (x, PALETTE_TOP, w, h)
}

/// What is at (x, y) over a dialog with `fields` fields: `Some(Some(i))` on field `i`,
/// `Some(None)` elsewhere on the dialog, `None` outside it.
pub fn dialog_hit(x: f32, y: f32, pane_width: f32, fields: usize) -> Option<Option<usize>> {
    let (dx, dy, w, h) = dialog_rect(pane_width, fields);
    if !(dx..dx + w).contains(&x) || !(dy..dy + h).contains(&y) {
        return None;
    }
    let field = (0..fields).find(|&i| {
        let top = dy + DIALOG_HEAD + DIALOG_ROW * i as f32 + 18.0;
        (top..top + DIALOG_FIELD).contains(&y)
    });
    Some(field)
}

pub struct DialogView<'a> {
    pub heading: &'a str,
    /// Label, text and the caret's byte offset, per field.
    pub fields: &'a [(&'a str, &'a str, usize)],
    pub active: usize,
    pub help: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaletteHit {
    /// The `n`th visible row.
    Row(usize),
    /// The palette, but not a row (the input, the padding).
    Inside,
    Outside,
}

/// What is at (x, y) for a palette showing `rows` rows; x from the pane's left edge.
pub fn palette_hit(x: f32, y: f32, pane_width: f32, rows: usize) -> PaletteHit {
    let (px, py, w, h) = palette_rect(pane_width, rows);
    if !(px..px + w).contains(&x) || !(py..py + h).contains(&y) {
        return PaletteHit::Outside;
    }
    let first = py + PALETTE_INPUT + 1.0 + PALETTE_PAD;
    let row = ((y - first) / PALETTE_ROW).floor();
    if y >= first && (row as usize) < rows {
        PaletteHit::Row(row as usize)
    } else {
        PaletteHit::Inside
    }
}

pub enum HintView<'a> {
    Label(&'a str),
    Keys(&'a str),
}

pub struct PaletteRow<'a> {
    pub title: &'a str,
    pub detail: &'a str,
    pub hint: HintView<'a>,
}

pub struct PaletteView<'a> {
    pub query: &'a str,
    /// Byte offset of the caret in `query`.
    pub cursor: usize,
    /// The visible rows (at most `palette_capacity` for the window).
    pub rows: &'a [PaletteRow<'a>],
    /// Index into `rows`.
    pub selected: Option<usize>,
    /// Shown in place of rows when nothing matches: a line and a second, quieter one.
    pub empty: Option<(&'a str, &'a str)>,
}

pub struct FieldView<'a> {
    pub text: &'a str,
    /// Byte offset of the caret.
    pub cursor: usize,
    pub all_selected: bool,
}

pub struct HeaderView<'a> {
    /// The pane's profile: its name follows the host and its color is the pane's top edge.
    pub profile: &'a str,
    pub hue: Hue,
    pub can_back: bool,
    pub can_forward: bool,
    /// Load progress from 0 to 1 while a page loads.
    pub loading: Option<f32>,
    pub host: &'a str,
    pub field: Option<FieldView<'a>>,
    /// The header button with keyboard focus (Tab), drawn with a focus ring.
    pub focused: Option<Button>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Shape {
    Back,
    Forward,
    Reload,
    Close,
    More,
    /// Modifier keys, `KEY_ICON` points.
    Command,
    Shift,
    Option,
    /// Rounded rectangle; the key also holds its device size.
    Rounded,
    /// The palette's radius-10 body.
    Panel,
    /// A blurred, faint panel under the palette.
    Shadow,
}

pub struct Chrome {
    theme: Theme,
    masks: HashMap<(Shape, u32, u32), GlyphBitmap>,
}

pub struct Frame<'a> {
    pub pixels: &'a mut [u8],
    pub stride: usize,
    /// First column drawn, in device pixels (the pane header starts right of the sidebar).
    pub left: usize,
    /// Visible width in device pixels, from `left`.
    pub width: usize,
    pub scale: f32,
}

impl Chrome {
    pub fn new(dark: bool) -> Chrome {
        Chrome {
            theme: Theme::new(dark),
            masks: HashMap::new(),
        }
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Draw the header into `frame`, whose rows are exactly the header.
    pub fn draw(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        view: &HeaderView,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let w = frame.width as i32;
        let rows = (frame.pixels.len() / frame.stride) as i32;
        let px = |v: f32| (v * s).round() as i32;
        let width_pt = frame.width as f32 / s;

        let icon_px = px(ICON) as u32;
        for shape in [
            Shape::Back,
            Shape::Forward,
            Shape::Reload,
            Shape::Close,
            Shape::More,
        ] {
            self.masks
                .entry((shape, icon_px, icon_px))
                .or_insert_with(|| icon(shape, s));
        }
        let field_w = px(width_pt - FIELD_RIGHT_GAP - FIELD_LEFT).max(1) as u32;
        let field_h = px(FIELD_HEIGHT) as u32;
        let inset = px(2.0) as u32;
        let button_px = px(BUTTON_WIDTH - 4.0) as u32;
        if view.focused.is_some() {
            for (mw, mh, radius) in [
                (button_px, button_px, 6.0),
                (
                    button_px - 2 * px(2.0) as u32,
                    button_px - 2 * px(2.0) as u32,
                    4.0,
                ),
            ] {
                self.masks
                    .entry((Shape::Rounded, mw, mh))
                    .or_insert_with(|| rounded_rect(mw, mh, radius * s));
            }
        }
        if view.field.is_some() {
            for (mw, mh, radius) in [
                (field_w, field_h, 6.0),
                (field_w - 2 * inset, field_h - 2 * inset, 4.0),
            ] {
                self.masks
                    .entry((Shape::Rounded, mw, mh))
                    .or_insert_with(|| rounded_rect(mw, mh, radius * s));
            }
        }

        let mut ops = vec![Op::Fill {
            x0: 0,
            y0: 0,
            x1: w,
            y1: rows,
            color: t.surface,
        }];
        // Bottom edge: a hairline, or the 2px loading line in the accent color.
        let hairline = px(1.0).max(1);
        ops.push(Op::Fill {
            x0: 0,
            y0: rows - hairline,
            x1: w,
            y1: rows,
            color: t.line,
        });
        // The profile edge along the top of the pane (DESIGN.md, section 6).
        ops.push(Op::Fill {
            x0: 0,
            y0: 0,
            x1: w,
            y1: px(EDGE).max(1),
            color: t.profile(view.hue),
        });
        if let Some(p) = view.loading {
            let thick = px(2.0).max(1);
            let x1 = (w as f32 * p.clamp(0.0, 1.0)).round() as i32;
            ops.push(Op::Fill {
                x0: 0,
                y0: rows - thick,
                x1,
                y1: rows,
                color: t.accent,
            });
        }

        let buttons = [
            (Shape::Back, view.can_back),
            (Shape::Forward, view.can_forward),
            // While a page loads, reload becomes stop.
            (
                if view.loading.is_some() {
                    Shape::Close
                } else {
                    Shape::Reload
                },
                true,
            ),
        ];
        // Left edge of each button's 28-point slot.
        let more_x = width_pt - MORE_RIGHT - BUTTON_WIDTH;
        let focused_x = match view.focused {
            Some(Button::Back) => Some(BUTTON_START),
            Some(Button::Forward) => Some(BUTTON_START + BUTTON_WIDTH),
            Some(Button::Reload) => Some(BUTTON_START + 2.0 * BUTTON_WIDTH),
            Some(Button::More) => Some(more_x),
            _ => None,
        };
        if let Some(slot_x) = focused_x {
            // Focus ring: a 2px accent ring around the 24px button area (DESIGN.md, section 13).
            let ring = px(2.0);
            let (bx, by) = (
                px(slot_x + 2.0),
                px((HEADER_HEIGHT - BUTTON_WIDTH + 4.0) / 2.0),
            );
            let inner = button_px - 2 * ring as u32;
            ops.push(Op::Mask {
                x: bx,
                y: by,
                mask: &self.masks[&(Shape::Rounded, button_px, button_px)],
                color: t.accent,
            });
            ops.push(Op::Mask {
                x: bx + ring,
                y: by + ring,
                mask: &self.masks[&(Shape::Rounded, inner, inner)],
                color: t.surface,
            });
        }
        for (i, (shape, enabled)) in buttons.into_iter().enumerate() {
            let x = BUTTON_START + BUTTON_WIDTH * i as f32 + (BUTTON_WIDTH - ICON) / 2.0;
            ops.push(Op::Mask {
                x: px(x),
                y: px((HEADER_HEIGHT - ICON) / 2.0),
                mask: &self.masks[&(shape, icon_px, icon_px)],
                color: if enabled { t.text_2 } else { t.muted },
            });
        }
        ops.push(Op::Mask {
            x: px(more_x + (BUTTON_WIDTH - ICON) / 2.0),
            y: px((HEADER_HEIGHT - ICON) / 2.0),
            mask: &self.masks[&(Shape::More, icon_px, icon_px)],
            color: t.text_2,
        });

        let mut text_ops = Vec::new();
        let mut text_clip = (px(FIELD_LEFT + 4.0), px(width_pt - FIELD_RIGHT_GAP));
        let field_text;
        // Always say which profile this pane uses, in words (DESIGN.md, principle 2).
        let label = if view.host.is_empty() {
            view.profile.to_string()
        } else {
            format!("{} · {}", view.host, view.profile)
        };
        match &view.field {
            None => {
                text_ops.push(Op::Text {
                    x: (FIELD_LEFT + 4.0) * s,
                    baseline: px(22.5) as f32,
                    text: &label,
                    size: 12.5 * s,
                    bold: false,
                    color: t.text_2,
                });
            }
            Some(field) => {
                // Focus ring (2px accent) around the field, then the field itself.
                ops.push(Op::Mask {
                    x: px(FIELD_LEFT),
                    y: px(FIELD_TOP),
                    mask: &self.masks[&(Shape::Rounded, field_w, field_h)],
                    color: t.accent,
                });
                ops.push(Op::Mask {
                    x: px(FIELD_LEFT) + inset as i32,
                    y: px(FIELD_TOP) + inset as i32,
                    mask: &self.masks[&(Shape::Rounded, field_w - 2 * inset, field_h - 2 * inset)],
                    color: t.canvas,
                });

                let size = 13.0 * s;
                let left = (FIELD_LEFT + FIELD_PADDING) * s;
                let right = (width_pt - FIELD_RIGHT_GAP - FIELD_PADDING) * s;
                let caret = metrics.width(&field.text[..field.cursor], size, false);
                let full = metrics.width(field.text, size, false);
                // Scroll the text left so the caret stays inside the field.
                let shift = (left + caret - right).max(0.0);
                let x = left - shift;
                let (top, bottom) = (px(FIELD_TOP + 5.0), px(FIELD_TOP + FIELD_HEIGHT - 5.0));
                if field.all_selected && !field.text.is_empty() {
                    text_ops.push(Op::Fill {
                        x0: x.round() as i32,
                        y0: top,
                        x1: (x + full).round() as i32,
                        y1: bottom,
                        color: t.selected,
                    });
                }
                field_text = field.text;
                text_ops.push(Op::Text {
                    x,
                    baseline: px(FIELD_TOP + 17.5) as f32,
                    text: field_text,
                    size,
                    bold: false,
                    color: t.text,
                });
                if !field.all_selected {
                    let cx = (x + caret).round() as i32;
                    text_ops.push(Op::Fill {
                        x0: cx,
                        y0: top,
                        x1: cx + px(1.0).max(1),
                        y1: bottom,
                        color: t.accent,
                    });
                }
                text_clip = (left.round() as i32, right.round() as i32 + px(1.0));
            }
        }

        let (stride, left) = (frame.stride, frame.left);
        painter.overlay(
            Target {
                pixels: &mut *frame.pixels,
                stride,
                bgra: true,
                left,
            },
            frame.width,
            (0, w),
            fonts,
            metrics,
            &ops,
        );
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left,
            },
            frame.width,
            text_clip,
            fonts,
            metrics,
            &text_ops,
        );
    }

    /// Draw the sidebar into `frame` (its full height, `frame.width` device pixels wide). The
    /// list scrolls under the profile row and the search field, which stay put.
    pub fn draw_sidebar(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        view: &SidebarView,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let w = frame.width as i32;
        let sw = frame.width as f32 / s;
        let rows = (frame.pixels.len() / frame.stride) as i32;
        let px = |v: f32| (v * s).round() as i32;
        let hair = px(1.0).max(1);
        let row_w = px(sw - 2.0 * ROW_INSET) as u32;
        let row_h = px(TAB_ROW - 2.0) as u32;
        let icon_px = px(ICON) as u32;
        let key_px = px(KEY_ICON) as u32;
        let search = (row_w, px(SEARCH_HEIGHT) as u32);
        let search_inner = (search.0 - 2 * hair as u32, search.1 - 2 * hair as u32);
        self.masks
            .entry((Shape::Close, icon_px, icon_px))
            .or_insert_with(|| icon(Shape::Close, s));
        self.masks
            .entry((Shape::Command, key_px, key_px))
            .or_insert_with(|| key_icon(Shape::Command, s));
        for (size, radius) in [((row_w, row_h), 6.0), (search, 6.0), (search_inner, 5.0)] {
            self.masks
                .entry((Shape::Rounded, size.0, size.1))
                .or_insert_with(|| rounded_rect(size.0, size.1, radius * s));
        }
        let masks = &self.masks;

        // The list, scrolled; only rows that can be seen are drawn.
        let row_top = |i: usize| TABS_TOP + TAB_ROW * i as f32 - view.scroll;
        let visible = |i: usize| row_top(i) + TAB_ROW > TABS_TOP && row_top(i) < rows as f32 / s;
        let fitted: Vec<Cow<str>> = view
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                _ if !visible(i) => Cow::Borrowed(""),
                ListRow::Label(text) => {
                    let room = (sw - ROW_INSET - 8.0 - ROW_TEXT) * s;
                    fit_in(text, room, 12.0 * s, true, metrics)
                }
                ListRow::Item {
                    title, closable, ..
                } => {
                    let right = if *closable { CLOSE_SIZE } else { 8.0 };
                    let room = (sw - ROW_INSET - right - ROW_TEXT) * s;
                    fit(title, room, 13.0 * s, metrics)
                }
            })
            .collect();
        let mut list = vec![Op::Fill {
            x0: 0,
            y0: 0,
            x1: w,
            y1: rows,
            color: t.canvas,
        }];
        let mut list_text = Vec::new();
        for (i, (row, text)) in view.rows.iter().zip(&fitted).enumerate() {
            if !visible(i) {
                continue;
            }
            let selected = view.selected == Some(i);
            match row {
                ListRow::Label(_) => list_text.push(Op::Text {
                    x: ROW_TEXT * s,
                    baseline: px(row_top(i) + 20.0) as f32,
                    text,
                    size: 12.0 * s,
                    bold: true,
                    color: t.muted,
                }),
                ListRow::Item {
                    muted, closable, ..
                } => {
                    if selected {
                        list.push(Op::Mask {
                            x: px(ROW_INSET),
                            y: px(row_top(i) + 1.0),
                            mask: &masks[&(Shape::Rounded, row_w, row_h)],
                            color: t.selected,
                        });
                    }
                    if *closable {
                        list.push(Op::Mask {
                            x: px(sw - ROW_INSET - CLOSE_SIZE + (CLOSE_SIZE - ICON) / 2.0),
                            y: px(row_top(i) + (TAB_ROW - ICON) / 2.0),
                            mask: &masks[&(Shape::Close, icon_px, icon_px)],
                            color: t.text_2,
                        });
                    }
                    // `muted` never sits on `selected` (DESIGN.md, section 3).
                    let color = if *muted && !selected { t.muted } else { t.text };
                    list_text.push(Op::Text {
                        x: ROW_TEXT * s,
                        baseline: px(row_top(i) + 19.5) as f32,
                        text,
                        size: 13.0 * s,
                        bold: false,
                        color,
                    });
                }
            }
        }
        if let (true, Some((line, second))) = (view.rows.is_empty(), view.empty) {
            list_text.push(Op::Text {
                x: ROW_TEXT * s,
                baseline: px(row_top(0) + 19.5) as f32,
                text: line,
                size: 13.0 * s,
                bold: false,
                color: t.text_2,
            });
            push_keys(
                &mut list,
                masks,
                second,
                (ROW_TEXT * s, px(row_top(0) + 37.5) as f32),
                (s, 12.0),
                t.text_2,
                metrics,
            );
        }
        if let Some(footer) = view.footer {
            list_text.push(Op::Text {
                x: ROW_TEXT * s,
                baseline: px(row_top(view.rows.len()) + 19.5) as f32,
                text: footer,
                size: 13.0 * s,
                bold: false,
                color: t.text_2,
            });
        }
        if let Some(at) = view.drop_at {
            let y = px(row_top(at)) - px(1.0);
            list.push(Op::Fill {
                x0: px(ROW_INSET),
                y0: y,
                x1: px(sw - ROW_INSET),
                y1: y + px(2.0).max(1),
                color: t.accent,
            });
        }

        // The top stays put: it covers whatever of the list scrolled under it.
        let mut top = vec![
            Op::Fill {
                x0: 0,
                y0: 0,
                x1: w,
                y1: px(TABS_TOP - 4.0),
                color: t.canvas,
            },
            // The profile switcher carries the same edge as the panes.
            Op::Fill {
                x0: 0,
                y0: 0,
                x1: w - hair,
                y1: px(EDGE).max(1),
                color: t.profile(view.hue),
            },
            Op::Mask {
                x: px(ROW_INSET),
                y: px(SEARCH_TOP),
                mask: &masks[&(Shape::Rounded, search.0, search.1)],
                color: t.line,
            },
            Op::Mask {
                x: px(ROW_INSET) + hair,
                y: px(SEARCH_TOP) + hair,
                mask: &masks[&(Shape::Rounded, search_inner.0, search_inner.1)],
                color: t.surface,
            },
            // Hairline between the sidebar and the pane.
            Op::Fill {
                x0: w - hair,
                y0: 0,
                x1: w,
                y1: rows,
                color: t.line,
            },
        ];
        let hint_x = px(sw - ROW_INSET - 10.0) as f32 - keys_width("⌘K", s, 12.0, metrics);
        push_keys(
            &mut top,
            masks,
            "⌘K",
            (hint_x, px(SEARCH_TOP + 19.5) as f32),
            (s, 12.0),
            t.text_2,
            metrics,
        );
        let top_text = [
            Op::Text {
                x: ROW_TEXT * s,
                baseline: px(26.0) as f32,
                text: view.profile,
                size: 13.0 * s,
                bold: true,
                color: t.text,
            },
            Op::Text {
                x: ROW_TEXT * s,
                baseline: px(SEARCH_TOP + 19.5) as f32,
                text: "Search or go to",
                size: 13.0 * s,
                bold: false,
                color: t.text_2,
            },
        ];

        let (stride, left) = (frame.stride, frame.left);
        let inside = (px(ROW_INSET), px(sw - ROW_INSET));
        for (ops, clip) in [
            (&list[..], (0, w)),
            (&list_text[..], inside),
            (&top[..], (0, w)),
            (&top_text[..], inside),
        ] {
            painter.overlay(
                Target {
                    pixels: &mut *frame.pixels,
                    stride,
                    bgra: true,
                    left,
                },
                frame.width,
                clip,
                fonts,
                metrics,
                ops,
            );
        }
    }

    /// Draw the ⌘K palette over the pane. `frame` is every row of the window, from the pane's
    /// left edge.
    pub fn draw_palette(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        view: &PaletteView,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let pane_w = frame.width as f32 / s;
        let shown = if view.empty.is_some() {
            1
        } else {
            view.rows.len()
        };
        let (x, y, w, h) = palette_rect(pane_w, shown);
        let hair = px(1.0).max(1);
        let panel = (px(w) as u32, px(h) as u32);
        let inner = (panel.0 - 2 * hair as u32, panel.1 - 2 * hair as u32);
        let row = (
            px(w - 2.0 * PALETTE_PAD) as u32,
            px(PALETTE_ROW - 4.0) as u32,
        );
        let blur = px(SHADOW_BLUR) as u32;
        let key_px = px(KEY_ICON) as u32;

        let shadow_key = self.panel_masks(panel, inner, blur, s);
        self.masks
            .entry((Shape::Rounded, row.0, row.1))
            .or_insert_with(|| rounded_rect(row.0, row.1, 6.0 * s));
        for shape in [Shape::Command, Shape::Shift, Shape::Option] {
            self.masks
                .entry((shape, key_px, key_px))
                .or_insert_with(|| key_icon(shape, s));
        }

        // Text first, fitted to its space, so the drawing ops can borrow it.
        let left = px(x + PALETTE_TEXT) as f32;
        let right = px(x + w - PALETTE_TEXT) as f32;
        let row_top = |i: usize| y + PALETTE_INPUT + 1.0 + PALETTE_PAD + PALETTE_ROW * i as f32;
        let hint_width = |hint: &HintView| match hint {
            HintView::Label(label) => metrics.width(label, 12.0 * s, false),
            HintView::Keys(keys) => keys_width(keys, s, 12.0, metrics),
        };
        let fitted: Vec<(Cow<str>, Cow<str>, f32)> = view
            .rows
            .iter()
            .map(|r| {
                let hw = hint_width(&r.hint);
                let room = right - left - hw - if hw > 0.0 { px(12.0) as f32 } else { 0.0 };
                (
                    fit(r.title, room, 13.5 * s, metrics),
                    fit(r.detail, room, 12.0 * s, metrics),
                    right - hw,
                )
            })
            .collect();
        let empty: Option<(Cow<str>, Cow<str>)> = view.empty.map(|(a, b)| {
            (
                fit(a, right - left, 13.5 * s, metrics),
                fit(b, right - left, 12.0 * s, metrics),
            )
        });

        let mut shapes = vec![
            Op::Mask {
                x: px(x) - blur as i32,
                y: px(y + SHADOW_DROP) - blur as i32,
                mask: &self.masks[&shadow_key],
                color: Color::rgb(0, 0, 0),
            },
            Op::Mask {
                x: px(x),
                y: px(y),
                mask: &self.masks[&(Shape::Panel, panel.0, panel.1)],
                color: t.line,
            },
            Op::Mask {
                x: px(x) + hair,
                y: px(y) + hair,
                mask: &self.masks[&(Shape::Panel, inner.0, inner.1)],
                color: t.surface,
            },
            Op::Fill {
                x0: px(x) + hair,
                y0: px(y + PALETTE_INPUT),
                x1: px(x + w) - hair,
                y1: px(y + PALETTE_INPUT) + hair,
                color: t.line,
            },
        ];
        if let Some(sel) = view.selected.filter(|&i| i < view.rows.len()) {
            shapes.push(Op::Mask {
                x: px(x + PALETTE_PAD),
                y: px(row_top(sel) + 2.0),
                mask: &self.masks[&(Shape::Rounded, row.0, row.1)],
                color: t.selected,
            });
        }

        let mut text = Vec::new();
        // The query, scrolled left so the caret stays visible, with its caret.
        let size = 14.0 * s;
        let caret = metrics.width(&view.query[..view.cursor], size, false);
        let shift = (left + caret - right).max(0.0);
        let baseline = px(y + 27.0) as f32;
        if view.query.is_empty() {
            text.push(Op::Text {
                x: left,
                baseline,
                text: "Search tabs, history and commands",
                size,
                bold: false,
                color: t.muted,
            });
        } else {
            text.push(Op::Text {
                x: left - shift,
                baseline,
                text: view.query,
                size,
                bold: false,
                color: t.text,
            });
        }
        let cx = (left - shift + caret).round() as i32;
        text.push(Op::Fill {
            x0: cx,
            y0: px(y + 13.0),
            x1: cx + hair,
            y1: px(y + PALETTE_INPUT - 13.0),
            color: t.accent,
        });
        for (i, (r, (title, detail, hint_x))) in view.rows.iter().zip(&fitted).enumerate() {
            let top = row_top(i);
            text.push(Op::Text {
                x: left,
                baseline: px(top + 19.0) as f32,
                text: title,
                size: 13.5 * s,
                bold: false,
                color: t.text,
            });
            text.push(Op::Text {
                x: left,
                baseline: px(top + 35.0) as f32,
                text: detail,
                size: 12.0 * s,
                bold: false,
                color: t.text_2,
            });
            // `text-2`, not `muted`: a hint may sit on `selected` (DESIGN.md, section 3).
            match &r.hint {
                HintView::Label(label) => text.push(Op::Text {
                    x: *hint_x,
                    baseline: px(top + 27.0) as f32,
                    text: label,
                    size: 12.0 * s,
                    bold: false,
                    color: t.text_2,
                }),
                HintView::Keys(keys) => push_keys(
                    &mut text,
                    &self.masks,
                    keys,
                    (*hint_x, px(top + 27.0) as f32),
                    (s, 12.0),
                    t.text_2,
                    metrics,
                ),
            }
        }
        if let Some((line, second)) = &empty {
            let top = row_top(0);
            text.push(Op::Text {
                x: left,
                baseline: px(top + 19.0) as f32,
                text: line,
                size: 13.5 * s,
                bold: false,
                color: t.text,
            });
            text.push(Op::Text {
                x: left,
                baseline: px(top + 35.0) as f32,
                text: second,
                size: 12.0 * s,
                bold: false,
                color: t.text_2,
            });
        }

        let (stride, frame_left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: &mut *frame.pixels,
                stride,
                bgra: true,
                left: frame_left,
            },
            width,
            (0, width as i32),
            fonts,
            metrics,
            &shapes,
        );
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left: frame_left,
            },
            width,
            (px(x + PALETTE_TEXT / 2.0), px(x + w - PALETTE_TEXT / 2.0)),
            fonts,
            metrics,
            &text,
        );
    }

    /// The keyboard focus ring around a page element: 2px `accent`, 2px outside each of its
    /// rectangles (DESIGN.md, section 13). Rectangles are in points from the page area's
    /// top-left corner; `frame` holds the page area's rows.
    pub fn draw_focus_ring(
        &self,
        painter: &mut Painter,
        frame: Frame,
        rects: &[(f32, f32, f32, f32)],
    ) {
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let (gap, ring) = (px(2.0), px(2.0).max(1));
        let mut ops = Vec::new();
        for &(x, y, w, h) in rects {
            let (x0, y0) = (px(x) - gap - ring, px(y) - gap - ring);
            let (x1, y1) = (px(x + w) + gap + ring, px(y + h) + gap + ring);
            for (a, b, c, d) in [
                (x0, y0, x1, y0 + ring),
                (x0, y1 - ring, x1, y1),
                (x0, y0, x0 + ring, y1),
                (x1 - ring, y0, x1, y1),
            ] {
                ops.push(Op::Fill {
                    x0: a,
                    y0: b,
                    x1: c,
                    y1: d,
                    color: self.theme.accent,
                });
            }
        }
        let (stride, left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left,
            },
            width,
            (0, width as i32),
            None,
            &font::Approximate,
            &ops,
        );
    }

    /// A pane whose page is not shown yet: the tab's title in the middle of a blank pane,
    /// until the pane gets focus.
    pub fn draw_placeholder(
        &self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        title: &str,
    ) {
        let s = frame.scale;
        let size = 13.0 * s;
        let height = frame.pixels.len() / frame.stride.max(1);
        let max = frame.width as f32 - 32.0 * s;
        let text = fit(title, max.max(0.0), size, metrics);
        let width: f32 = text
            .chars()
            .map(|c| metrics.advance_em(c, false) * size)
            .sum();
        let ops = [Op::Text {
            x: ((frame.width as f32 - width) / 2.0).max(0.0),
            baseline: (height as f32 / 2.0).round(),
            text: &text,
            size,
            bold: false,
            color: self.theme.text_2,
        }];
        let (stride, left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left,
            },
            width,
            (0, width as i32),
            fonts,
            metrics,
            &ops,
        );
    }

    /// The caret of a focused form field: (x, top, height) in points from the pane's top
    /// left, one device pixel wide (at least).
    pub fn draw_caret(&self, painter: &mut Painter, frame: Frame, caret: (f32, f32, f32)) {
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let (x, y, h) = caret;
        let ops = [Op::Fill {
            x0: px(x),
            y0: px(y),
            x1: px(x) + px(1.0).max(1),
            y1: px(y + h),
            color: Color::rgb(0, 0, 0),
        }];
        let (stride, left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left,
            },
            width,
            (0, width as i32),
            None,
            &font::Approximate,
            &ops,
        );
    }

    /// The shapes of a floating panel (palette, dialog): its shadow, hairline and body. Only the
    /// current size is kept, since a panel's height changes with its contents.
    fn panel_masks(
        &mut self,
        panel: (u32, u32),
        inner: (u32, u32),
        blur: u32,
        s: f32,
    ) -> (Shape, u32, u32) {
        let shadow_key = (Shape::Shadow, panel.0, panel.1);
        self.masks.retain(|k, _| match k.0 {
            Shape::Shadow => *k == shadow_key,
            Shape::Panel => (k.1, k.2) == panel || (k.1, k.2) == inner,
            _ => true,
        });
        self.masks
            .entry(shadow_key)
            .or_insert_with(|| shadow(panel.0, panel.1, 10.0 * s, blur));
        for (size, radius) in [(panel, 10.0), (inner, 9.0)] {
            self.masks
                .entry((Shape::Panel, size.0, size.1))
                .or_insert_with(|| rounded_rect(size.0, size.1, radius * s));
        }
        shadow_key
    }

    /// Draw a small dialog over the pane, where the palette sits: a heading, labelled text
    /// fields (the active one with a focus ring and caret) and a line of help.
    pub fn draw_dialog(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        view: &DialogView,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let pane_w = frame.width as f32 / s;
        let (x, y, w, h) = dialog_rect(pane_w, view.fields.len());
        let hair = px(1.0).max(1);
        let panel = (px(w) as u32, px(h) as u32);
        let inner = (panel.0 - 2 * hair as u32, panel.1 - 2 * hair as u32);
        let blur = px(SHADOW_BLUR) as u32;
        let shadow_key = self.panel_masks(panel, inner, blur, s);
        let field = (px(w - 2.0 * DIALOG_PAD) as u32, px(DIALOG_FIELD) as u32);
        let ring = px(2.0);
        for (size, radius) in [
            (field, 6.0),
            ((field.0 - 2 * hair as u32, field.1 - 2 * hair as u32), 5.0),
            ((field.0 - 2 * ring as u32, field.1 - 2 * ring as u32), 4.0),
        ] {
            self.masks
                .entry((Shape::Rounded, size.0, size.1))
                .or_insert_with(|| rounded_rect(size.0, size.1, radius * s));
        }
        let masks = &self.masks;
        let mut shapes = vec![
            Op::Mask {
                x: px(x) - blur as i32,
                y: px(y + SHADOW_DROP) - blur as i32,
                mask: &masks[&shadow_key],
                color: Color::rgb(0, 0, 0),
            },
            Op::Mask {
                x: px(x),
                y: px(y),
                mask: &masks[&(Shape::Panel, panel.0, panel.1)],
                color: t.line,
            },
            Op::Mask {
                x: px(x) + hair,
                y: px(y) + hair,
                mask: &masks[&(Shape::Panel, inner.0, inner.1)],
                color: t.surface,
            },
        ];
        let mut text = vec![Op::Text {
            x: px(x + DIALOG_PAD) as f32,
            baseline: px(y + 30.0) as f32,
            text: view.heading,
            size: 14.0 * s,
            bold: true,
            color: t.text,
        }];
        let size = 13.0 * s;
        let left = px(x + DIALOG_PAD + 10.0) as f32;
        let right = px(x + w - DIALOG_PAD - 10.0) as f32;
        for (i, (label, value, cursor)) in view.fields.iter().enumerate() {
            let top = y + DIALOG_HEAD + DIALOG_ROW * i as f32;
            text.push(Op::Text {
                x: px(x + DIALOG_PAD) as f32,
                baseline: px(top + 12.0) as f32,
                text: label,
                size: 12.0 * s,
                bold: false,
                color: t.text_2,
            });
            let box_y = top + 18.0;
            let active = i == view.active;
            // The active field: a 2px accent ring; the others: a hairline.
            let (edge, edge_px) = if active {
                (t.accent, ring)
            } else {
                (t.line, hair)
            };
            shapes.push(Op::Mask {
                x: px(x + DIALOG_PAD),
                y: px(box_y),
                mask: &masks[&(Shape::Rounded, field.0, field.1)],
                color: edge,
            });
            shapes.push(Op::Mask {
                x: px(x + DIALOG_PAD) + edge_px,
                y: px(box_y) + edge_px,
                mask: &masks[&(
                    Shape::Rounded,
                    field.0 - 2 * edge_px as u32,
                    field.1 - 2 * edge_px as u32,
                )],
                color: t.canvas,
            });
            let caret = metrics.width(&value[..*cursor], size, false);
            let shift = if active {
                (left + caret - right).max(0.0)
            } else {
                0.0
            };
            text.push(Op::Text {
                x: left - shift,
                baseline: px(box_y + 20.5) as f32,
                text: value,
                size,
                bold: false,
                color: t.text,
            });
            if active {
                let cx = (left - shift + caret).round() as i32;
                text.push(Op::Fill {
                    x0: cx,
                    y0: px(box_y + 7.0),
                    x1: cx + hair,
                    y1: px(box_y + DIALOG_FIELD - 7.0),
                    color: t.accent,
                });
            }
        }
        text.push(Op::Text {
            x: px(x + DIALOG_PAD) as f32,
            baseline: px(y + h - DIALOG_PAD) as f32,
            text: view.help,
            size: 12.0 * s,
            bold: false,
            color: t.text_2,
        });
        let (stride, frame_left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: &mut *frame.pixels,
                stride,
                bgra: true,
                left: frame_left,
            },
            width,
            (0, width as i32),
            fonts,
            metrics,
            &shapes,
        );
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left: frame_left,
            },
            width,
            (px(x + DIALOG_PAD), px(x + w - DIALOG_PAD)),
            fonts,
            metrics,
            &text,
        );
    }

    /// Draw the ⋯ menu under its button at the pane's right edge. `frame` is every row of the
    /// window, from the pane's left edge.
    pub fn draw_menu(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        view: &MenuView,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let (x, y, w, h) = menu_rect(frame.width as f32 / s, view.items.len());
        let hair = px(1.0).max(1);
        let panel = (px(w) as u32, px(h) as u32);
        let inner = (panel.0 - 2 * hair as u32, panel.1 - 2 * hair as u32);
        let blur = px(SHADOW_BLUR) as u32;
        let shadow_key = self.panel_masks(panel, inner, blur, s);
        let row = (px(w - 2.0 * MENU_PAD) as u32, px(MENU_ROW - 2.0) as u32);
        self.masks
            .entry((Shape::Rounded, row.0, row.1))
            .or_insert_with(|| rounded_rect(row.0, row.1, 6.0 * s));
        let masks = &self.masks;
        let mut shapes = vec![
            Op::Mask {
                x: px(x) - blur as i32,
                y: px(y + SHADOW_DROP) - blur as i32,
                mask: &masks[&shadow_key],
                color: Color::rgb(0, 0, 0),
            },
            Op::Mask {
                x: px(x),
                y: px(y),
                mask: &masks[&(Shape::Panel, panel.0, panel.1)],
                color: t.line,
            },
            Op::Mask {
                x: px(x) + hair,
                y: px(y) + hair,
                mask: &masks[&(Shape::Panel, inner.0, inner.1)],
                color: t.surface,
            },
        ];
        if let Some(i) = view.selected.filter(|&i| i < view.items.len()) {
            shapes.push(Op::Mask {
                x: px(x + MENU_PAD),
                y: px(y + MENU_PAD + MENU_ROW * i as f32 + 1.0),
                mask: &masks[&(Shape::Rounded, row.0, row.1)],
                color: t.selected,
            });
        }
        let room = (w - 2.0 * MENU_PAD - 20.0) * s;
        let fitted: Vec<Cow<str>> = view
            .items
            .iter()
            .map(|item| fit(item, room, 13.0 * s, metrics))
            .collect();
        let text: Vec<Op> = fitted
            .iter()
            .enumerate()
            .map(|(i, item)| Op::Text {
                x: px(x + MENU_PAD + 10.0) as f32,
                baseline: px(y + MENU_PAD + MENU_ROW * i as f32 + 19.5) as f32,
                text: item,
                size: 13.0 * s,
                bold: false,
                color: t.text,
            })
            .collect();
        let (stride, left, width) = (frame.stride, frame.left, frame.width);
        for (ops, clip) in [
            (&shapes[..], (0, width as i32)),
            (&text[..], (px(x), px(x + w))),
        ] {
            painter.overlay(
                Target {
                    pixels: &mut *frame.pixels,
                    stride,
                    bgra: true,
                    left,
                },
                width,
                clip,
                fonts,
                metrics,
                ops,
            );
        }
    }

    /// A tooltip near (x, y) in window points: a small labelled panel kept inside the window.
    /// `frame` is the whole window. ⌘ ⇧ ⌥ in the text are drawn as icons.
    pub fn draw_tooltip(
        &mut self,
        painter: &mut Painter,
        frame: Frame,
        fonts: Option<&FontSet>,
        metrics: &dyn TextMetrics,
        (x, y): (f32, f32),
        text: &str,
    ) {
        let t = self.theme;
        let s = frame.scale;
        let px = |v: f32| (v * s).round() as i32;
        let window = (
            frame.width as f32 / s,
            (frame.pixels.len() / frame.stride) as f32 / s,
        );
        let key_px = px(KEY_ICON) as u32;
        for shape in [Shape::Command, Shape::Shift, Shape::Option] {
            self.masks
                .entry((shape, key_px, key_px))
                .or_insert_with(|| key_icon(shape, s));
        }
        let w = keys_width(text, s, 12.0, metrics) / s + 16.0;
        let h = 24.0;
        // Below and right of the pointer, or above / left of it near the window's edges.
        let tx = (x + 12.0).min(window.0 - w - 4.0).max(4.0);
        let ty = if y + 20.0 + h < window.1 {
            y + 20.0
        } else {
            (y - h - 8.0).max(4.0)
        };
        let hair = px(1.0).max(1);
        let size = (px(w) as u32, px(h) as u32);
        let inner = (size.0 - 2 * hair as u32, size.1 - 2 * hair as u32);
        for (sz, radius) in [(size, 6.0), (inner, 5.0)] {
            self.masks
                .entry((Shape::Rounded, sz.0, sz.1))
                .or_insert_with(|| rounded_rect(sz.0, sz.1, radius * s));
        }
        let masks = &self.masks;
        let mut ops = vec![
            Op::Mask {
                x: px(tx),
                y: px(ty),
                mask: &masks[&(Shape::Rounded, size.0, size.1)],
                color: t.line,
            },
            Op::Mask {
                x: px(tx) + hair,
                y: px(ty) + hair,
                mask: &masks[&(Shape::Rounded, inner.0, inner.1)],
                color: t.surface,
            },
        ];
        push_keys(
            &mut ops,
            masks,
            text,
            (px(tx + 8.0) as f32, px(ty + 16.5) as f32),
            (s, 12.0),
            t.text,
            metrics,
        );
        let (stride, left, width) = (frame.stride, frame.left, frame.width);
        painter.overlay(
            Target {
                pixels: frame.pixels,
                stride,
                bgra: true,
                left,
            },
            width,
            (0, width as i32),
            fonts,
            metrics,
            &ops,
        );
    }

    /// Free the palette's shapes once it closes (its shadow alone is about 1 MB at 2x).
    pub fn forget_palette(&mut self) {
        self.masks
            .retain(|k, _| !matches!(k.0, Shape::Shadow | Shape::Panel));
    }
}

/// Fill `width` columns from `left` of a BGRA frame with one color (an empty pane).
pub fn fill(pixels: &mut [u8], stride: usize, left: usize, width: usize, color: Color) {
    for row in pixels.chunks_mut(stride) {
        let end = ((left + width) * 4).min(row.len());
        for px in row[(left * 4).min(end)..end].as_chunks_mut::<4>().0 {
            *px = [color.b, color.g, color.r, 255];
        }
    }
}

/// `text` cut to fit `max` device pixels at `size`, ending with "…" when cut.
fn fit<'t>(text: &'t str, max: f32, size: f32, metrics: &dyn TextMetrics) -> Cow<'t, str> {
    fit_in(text, max, size, false, metrics)
}

fn fit_in<'t>(
    text: &'t str,
    max: f32,
    size: f32,
    bold: bool,
    metrics: &dyn TextMetrics,
) -> Cow<'t, str> {
    if metrics.width(text, size, bold) <= max {
        return Cow::Borrowed(text);
    }
    let room = max - metrics.width("…", size, bold);
    // The most characters, ending on a char boundary, that leave room for the ellipsis.
    // `ends[k - 1]` is the byte length of the first `k` characters.
    let ends: Vec<usize> = text
        .char_indices()
        .skip(1)
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let (mut lo, mut hi) = (0, ends.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if metrics.width(&text[..ends[mid - 1]], size, bold) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let end = if lo == 0 { 0 } else { ends[lo - 1] };
    Cow::Owned(format!("{}…", text[..end].trim_end()))
}

/// Width in device pixels of a shortcut such as "⌘⇧T" at `size` points.
fn keys_width(keys: &str, scale: f32, size: f32, metrics: &dyn TextMetrics) -> f32 {
    keys.chars()
        .map(|c| match c {
            '⌘' | '⇧' | '⌥' => ((KEY_ICON + 1.0) * scale).round(),
            _ => metrics.width(c.encode_utf8(&mut [0; 4]), size * scale, false),
        })
        .sum()
}

/// Draw a shortcut from (x, baseline), device pixels: modifier icons, then the key in text.
#[allow(clippy::too_many_arguments)]
fn push_keys<'m>(
    out: &mut Vec<Op<'m>>,
    masks: &'m HashMap<(Shape, u32, u32), GlyphBitmap>,
    keys: &'m str,
    (mut x, baseline): (f32, f32),
    (scale, size): (f32, f32),
    color: Color,
    metrics: &dyn TextMetrics,
) {
    let icon_px = (KEY_ICON * scale).round() as u32;
    for (i, c) in keys.char_indices() {
        let shape = match c {
            '⌘' => Some(Shape::Command),
            '⇧' => Some(Shape::Shift),
            '⌥' => Some(Shape::Option),
            _ => None,
        };
        match shape.and_then(|shape| masks.get(&(shape, icon_px, icon_px))) {
            Some(mask) => {
                out.push(Op::Mask {
                    x: x.round() as i32,
                    y: (baseline - icon_px as f32 * 0.9).round() as i32,
                    mask,
                    color,
                });
                x += ((KEY_ICON + 1.0) * scale).round();
            }
            None => {
                let key = &keys[i..i + c.len_utf8()];
                out.push(Op::Text {
                    x,
                    baseline,
                    text: key,
                    size: size * scale,
                    bold: false,
                    color,
                });
                x += metrics.width(key, size * scale, false);
            }
        }
    }
}

/// A modifier key symbol, `KEY_ICON` points, 1.2-point strokes.
fn key_icon(shape: Shape, scale: f32) -> GlyphBitmap {
    let circle = |cx: f32, cy: f32, r: f32| -> Vec<(f32, f32)> {
        (0..=16)
            .map(|i| {
                let a = (i as f32 / 16.0) * std::f32::consts::TAU;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    };
    let paths: Vec<Vec<(f32, f32)>> = match shape {
        // ⌘: a square with a loop at each corner.
        Shape::Command => vec![
            vec![(4.2, 4.2), (7.8, 4.2), (7.8, 7.8), (4.2, 7.8), (4.2, 4.2)],
            circle(3.0, 3.0, 1.7),
            circle(9.0, 3.0, 1.7),
            circle(9.0, 9.0, 1.7),
            circle(3.0, 9.0, 1.7),
        ],
        // ⇧: an outlined arrow pointing up.
        Shape::Shift => vec![vec![
            (6.0, 1.5),
            (11.0, 6.5),
            (8.5, 6.5),
            (8.5, 10.5),
            (3.5, 10.5),
            (3.5, 6.5),
            (1.0, 6.5),
            (6.0, 1.5),
        ]],
        // ⌥: the option key's switch.
        Shape::Option => vec![
            vec![(1.0, 3.0), (4.5, 3.0), (7.5, 9.5), (11.0, 9.5)],
            vec![(7.5, 3.0), (11.0, 3.0)],
        ],
        _ => Vec::new(),
    };
    stroke(&paths, KEY_ICON, 1.2, scale)
}

/// A faint, blurred rounded rectangle `blur` pixels larger on each side than the panel.
fn shadow(width: u32, height: u32, radius: f32, blur: u32) -> GlyphBitmap {
    const STRENGTH: f32 = 0.16;
    let (w, h, b) = (width as usize, height as usize, blur as usize);
    let (ow, oh) = (w + 2 * b, h + 2 * b);
    let base = rounded_rect(width, height, radius);
    let mut cov = vec![0f32; ow * oh];
    for y in 0..h {
        for x in 0..w {
            cov[(y + b) * ow + x + b] = f32::from(base.coverage[y * w + x]) / 255.0;
        }
    }
    // Two box blurs in each direction approximate a Gaussian.
    let r = (b / 2).max(1);
    for _ in 0..2 {
        box_blur(&mut cov, ow, oh, r, 1, ow);
        box_blur(&mut cov, oh, ow, r, ow, 1);
    }
    GlyphBitmap {
        left: 0,
        top: 0,
        width: ow,
        height: oh,
        coverage: cov
            .iter()
            .map(|c| (c * STRENGTH * 255.0).round().clamp(0.0, 255.0) as u8)
            .collect(),
    }
}

/// Blur `lines` lines of `len` samples each with a running sum of radius `r`. Samples of a
/// line are `step` apart; lines start `stride` apart.
fn box_blur(data: &mut [f32], len: usize, lines: usize, r: usize, step: usize, stride: usize) {
    let mut line = vec![0f32; len];
    let window = (2 * r + 1) as f32;
    for l in 0..lines {
        for (i, v) in line.iter_mut().enumerate() {
            *v = data[l * stride + i * step];
        }
        let mut sum: f32 = line.iter().take(r + 1).sum();
        for i in 0..len {
            data[l * stride + i * step] = sum / window;
            if i + r + 1 < len {
                sum += line[i + r + 1];
            }
            if i >= r {
                sum -= line[i - r];
            }
        }
    }
}

/// A 16-point icon drawn as 1.5-point strokes with square ends (DESIGN.md, section 8).
fn icon(shape: Shape, scale: f32) -> GlyphBitmap {
    let paths: Vec<Vec<(f32, f32)>> = match shape {
        Shape::Back => vec![vec![(10.0, 3.0), (5.0, 8.0), (10.0, 13.0)]],
        Shape::Forward => vec![vec![(6.0, 3.0), (11.0, 8.0), (6.0, 13.0)]],
        Shape::Reload => {
            // An arc open at the top, with an arrowhead at its end.
            let arc: Vec<(f32, f32)> = (0..=18)
                .map(|i| {
                    let a = (-60.0 + 300.0 * i as f32 / 18.0).to_radians();
                    (8.0 + 5.0 * a.cos(), 8.0 + 5.0 * a.sin())
                })
                .collect();
            let (ex, ey) = *arc.last().expect("arc has points");
            let end = 240f32.to_radians();
            let (tx, ty) = (-end.sin(), end.cos());
            let head = |turn: f32| {
                let (c, s) = (turn.cos(), turn.sin());
                (ex - 3.0 * (tx * c - ty * s), ey - 3.0 * (tx * s + ty * c))
            };
            vec![arc, vec![head(0.8), (ex, ey), head(-0.8)]]
        }
        // Three dots: zero-length strokes with square caps.
        Shape::More => [3.5, 8.0, 12.5]
            .iter()
            .map(|&x| vec![(x, 8.0), (x + 0.01, 8.0)])
            .collect(),
        Shape::Close => vec![
            vec![(4.5, 4.5), (11.5, 11.5)],
            vec![(11.5, 4.5), (4.5, 11.5)],
        ],
        _ => Vec::new(),
    };
    stroke(&paths, ICON, 1.5, scale)
}

/// Rasterize polylines as strokes of `width` points inside a `size`-point square.
fn stroke(paths: &[Vec<(f32, f32)>], size: f32, width: f32, scale: f32) -> GlyphBitmap {
    let px = (size * scale).round().max(1.0) as usize;
    let mut r = Rasterizer::new(px, px);
    let half = width * scale / 2.0;
    for path in paths {
        for seg in path.windows(2) {
            let (p, q) = (
                (seg[0].0 * scale, seg[0].1 * scale),
                (seg[1].0 * scale, seg[1].1 * scale),
            );
            let len = ((q.0 - p.0).powi(2) + (q.1 - p.1).powi(2))
                .sqrt()
                .max(f32::EPSILON);
            let (dx, dy) = ((q.0 - p.0) / len * half, (q.1 - p.1) / len * half);
            // Square caps: extend both ends by half the width, then outline the quad.
            let (p0, q0) = ((p.0 - dx, p.1 - dy), (q.0 + dx, q.1 + dy));
            let corners = [
                (p0.0 - dy, p0.1 + dx),
                (q0.0 - dy, q0.1 + dx),
                (q0.0 + dy, q0.1 - dx),
                (p0.0 + dy, p0.1 - dx),
            ];
            for i in 0..4 {
                let (a, b) = (corners[i], corners[(i + 1) % 4]);
                r.line(a.0, a.1, b.0, b.1);
            }
        }
    }
    GlyphBitmap {
        left: 0,
        top: 0,
        width: px,
        height: px,
        coverage: r.coverage(),
    }
}

fn rounded_rect(width: u32, height: u32, radius: f32) -> GlyphBitmap {
    let (w, h) = (width as f32, height as f32);
    let r = radius.min(w / 2.0).min(h / 2.0);
    let mut points = Vec::new();
    let corners = [
        (w - r, r, -90.0),
        (w - r, h - r, 0.0),
        (r, h - r, 90.0),
        (r, r, 180.0),
    ];
    for (cx, cy, start) in corners {
        for i in 0..=6 {
            let a = (start + 90.0 * i as f32 / 6.0f32).to_radians();
            points.push((cx + r * a.cos(), cy + r * a.sin()));
        }
    }
    let mut raster = Rasterizer::new(width as usize, height as usize);
    for i in 0..points.len() {
        let (a, b) = (points[i], points[(i + 1) % points.len()]);
        raster.line(a.0, a.1, b.0, b.1);
    }
    GlyphBitmap {
        left: 0,
        top: 0,
        width: width as usize,
        height: height as usize,
        coverage: raster.coverage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_tests_header_controls() {
        assert_eq!(hit(4.0, 800.0), None);
        assert_eq!(hit(20.0, 800.0), Some(Button::Back));
        assert_eq!(hit(50.0, 800.0), Some(Button::Forward));
        assert_eq!(hit(80.0, 800.0), Some(Button::Reload));
        assert_eq!(hit(95.0, 800.0), None, "gap between reload and the field");
        assert_eq!(hit(400.0, 800.0), Some(Button::Address));
        assert_eq!(hit(770.0, 800.0), Some(Button::More));
        assert_eq!(hit(762.0, 800.0), None, "gap before ⋯");
        assert_eq!(hit(797.0, 800.0), None);
        // The menu: 230 wide from x = 562 (right edge 8 from the pane's), rows from 34 + 6.
        assert_eq!(menu_hit(700.0, 50.0, 800.0, 3), Some(Some(0)));
        assert_eq!(menu_hit(700.0, 105.0, 800.0, 3), Some(Some(2)));
        assert_eq!(menu_hit(700.0, 37.0, 800.0, 3), Some(None), "its padding");
        assert_eq!(menu_hit(500.0, 50.0, 800.0, 3), None);
    }

    #[test]
    fn hit_tests_the_sidebar() {
        let item = |title| ListRow::Item {
            title,
            muted: false,
            closable: false,
        };
        let rows = [
            item("a"),
            ListRow::Item {
                title: "b",
                muted: false,
                closable: true,
            },
            ListRow::Label("Folder".into()),
            item("c"),
        ];
        let hit = |x, y| sidebar_hit(x, y, 240.0, 0.0, &rows, true);
        assert_eq!(hit(100.0, 10.0), Some(SidebarItem::Profile));
        assert_eq!(hit(100.0, 50.0), Some(SidebarItem::Search));
        assert_eq!(hit(100.0, 78.0), None, "gap above the list");
        assert_eq!(hit(100.0, 90.0), Some(SidebarItem::Row(0)));
        assert_eq!(
            hit(220.0, 90.0),
            Some(SidebarItem::Row(0)),
            "no close button"
        );
        assert_eq!(hit(220.0, 120.0), Some(SidebarItem::CloseRow(1)));
        assert_eq!(
            hit(100.0, 150.0),
            Some(SidebarItem::Label(2)),
            "a label folds its group"
        );
        assert_eq!(hit(100.0, 180.0), Some(SidebarItem::Row(3)));
        assert_eq!(hit(100.0, 210.0), Some(SidebarItem::Footer));
        assert_eq!(
            sidebar_hit(100.0, 210.0, 240.0, 0.0, &rows, false),
            None,
            "no footer"
        );
        assert_eq!(hit(238.0, 150.0), Some(SidebarItem::Edge));
        assert_eq!(
            hit(240.5, 150.0),
            Some(SidebarItem::Edge),
            "and a point past it"
        );
        assert_eq!(hit(242.0, 150.0), None, "the page's edge stays the page's");
        assert_eq!(
            sidebar_hit(100.0, 90.0, 240.0, 60.0, &rows, true),
            Some(SidebarItem::Label(2)),
            "scrolled by two rows"
        );
        assert_eq!(sidebar_drop(90.0, 0.0, 4), 0);
        assert_eq!(sidebar_drop(130.0, 0.0, 4), 2, "nearest gap between rows");
        assert_eq!(sidebar_drop(900.0, 0.0, 4), 4);
        assert_eq!(sidebar_max_scroll(4, true, 300.0), 0.0);
        assert_eq!(
            sidebar_max_scroll(20, true, 300.0),
            30.0 * 21.0 + 8.0 - 218.0
        );
        assert_eq!(hit(100.0, 245.0), None);
        assert_eq!(hit(250.0, 90.0), None, "the pane");
    }

    #[test]
    fn hit_tests_dialogs() {
        // A 400-point pane: the dialog is 368 wide from x = 16, top 48; field 0's box starts at
        // 48 + 48 + 18 = 114, field 1's at 176.
        assert_eq!(dialog_hit(10.0, 120.0, 400.0, 2), None);
        assert_eq!(dialog_hit(100.0, 60.0, 400.0, 2), Some(None));
        assert_eq!(dialog_hit(100.0, 120.0, 400.0, 2), Some(Some(0)));
        assert_eq!(dialog_hit(100.0, 180.0, 400.0, 2), Some(Some(1)));
        assert_eq!(dialog_hit(100.0, 400.0, 400.0, 2), None);
    }

    #[test]
    fn fits_text_with_an_ellipsis_and_measures_shortcuts() {
        let m = font::Approximate;
        // Approximate: every glyph 0.5em, so 10 px per glyph at 20 px.
        assert_eq!(fit("short", 100.0, 20.0, &m), "short");
        let cut = fit("a long title", 60.0, 20.0, &m);
        assert!(
            cut.ends_with('…') && m.width(&cut, 20.0, false) <= 60.0,
            "{cut}"
        );
        assert_eq!(
            fit("éééééé", 30.0, 20.0, &m),
            "éé…",
            "cuts on char boundaries"
        );
        assert_eq!(fit("abc", 5.0, 20.0, &m), "…");
        assert_eq!(keys_width("⌘⇧T", 2.0, 12.0, &m), 26.0 * 2.0 + 12.0);
    }

    #[test]
    fn modifier_icons_and_the_shadow_have_ink() {
        for shape in [Shape::Command, Shape::Shift, Shape::Option] {
            let mask = key_icon(shape, 2.0);
            assert_eq!((mask.width, mask.height), (24, 24));
            assert!(
                mask.coverage.iter().any(|&c| c > 150),
                "{:?} drawn",
                shape as u8
            );
        }
        let s = shadow(40, 20, 6.0, 8);
        assert_eq!((s.width, s.height), (56, 36));
        let mid = s.coverage[18 * 56 + 28];
        assert!(mid > 30 && mid <= 41, "faint in the middle: {mid}");
        assert!(s.coverage[0] < mid / 4, "fades out at the edges");
    }

    #[test]
    fn hit_tests_the_palette() {
        // A 400-point pane: 368 wide from x = 16; rows start at 48 + 44 + 1 + 6 = 99.
        assert_eq!(palette_hit(10.0, 60.0, 400.0, 3), PaletteHit::Outside);
        assert_eq!(palette_hit(100.0, 60.0, 400.0, 3), PaletteHit::Inside);
        assert_eq!(palette_hit(100.0, 100.0, 400.0, 3), PaletteHit::Row(0));
        assert_eq!(
            palette_hit(100.0, 99.0 + 44.0 * 2.5, 400.0, 3),
            PaletteHit::Row(2)
        );
        assert_eq!(
            palette_hit(100.0, 99.0 + 44.0 * 3.0 + 2.0, 400.0, 3),
            PaletteHit::Inside
        );
        assert_eq!(palette_hit(100.0, 400.0, 400.0, 3), PaletteHit::Outside);
    }

    #[test]
    fn profile_colors_follow_the_theme() {
        assert_ne!(
            Theme::new(false).profile(Hue::Emerald),
            Theme::new(true).profile(Hue::Emerald)
        );
        assert_ne!(
            Theme::new(false).profile(Hue::Emerald),
            Theme::new(false).profile(Hue::Amber)
        );
    }

    #[test]
    fn icons_and_field_shapes_have_ink() {
        for shape in [
            Shape::Back,
            Shape::Forward,
            Shape::Reload,
            Shape::Close,
            Shape::More,
        ] {
            let mask = icon(shape, 2.0);
            assert_eq!((mask.width, mask.height), (32, 32));
            assert!(mask.coverage.iter().any(|&c| c > 200), "icon is drawn");
            assert!(mask.coverage[0] == 0, "corner is empty");
        }
        let field = rounded_rect(40, 20, 6.0);
        assert_eq!(field.coverage[0], 0, "rounded corner is empty");
        assert_eq!(field.coverage[10 * 40 + 20], 255, "middle is filled");
    }
}
