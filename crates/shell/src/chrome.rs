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
const FIELD_RIGHT_GAP: f32 = 8.0;
const FIELD_TOP: f32 = 5.0;
const FIELD_HEIGHT: f32 = 26.0;
const FIELD_PADDING: f32 = 10.0;

/// Sidebar width in points (DESIGN.md, section 7).
pub const SIDEBAR_WIDTH: f32 = 240.0;
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
    Tab(usize),
    CloseTab(usize),
    NewTab,
}

/// What is at (x, y) in the sidebar, in points, with `tabs` rows and the active one at `active`.
pub fn sidebar_hit(x: f32, y: f32, tabs: usize, active: usize) -> Option<SidebarItem> {
    if !(0.0..SIDEBAR_WIDTH).contains(&x) || y < 0.0 {
        return None;
    }
    if y < PROFILE_ROW {
        return Some(SidebarItem::Profile);
    }
    if !(ROW_INSET..=SIDEBAR_WIDTH - ROW_INSET).contains(&x) {
        return None;
    }
    if (SEARCH_TOP..SEARCH_TOP + SEARCH_HEIGHT).contains(&y) {
        return Some(SidebarItem::Search);
    }
    if y < TABS_TOP {
        return None;
    }
    let row = ((y - TABS_TOP) / TAB_ROW) as usize;
    if row < tabs {
        let on_close = x >= SIDEBAR_WIDTH - ROW_INSET - CLOSE_SIZE;
        Some(if row == active && on_close {
            SidebarItem::CloseTab(row)
        } else {
            SidebarItem::Tab(row)
        })
    } else if row == tabs {
        Some(SidebarItem::NewTab)
    } else {
        None
    }
}

pub struct TabView<'a> {
    pub title: &'a str,
    /// Asleep to save memory: the title is drawn in `muted`.
    pub asleep: bool,
}

pub struct SidebarView<'a> {
    pub profile: &'a str,
    pub hue: Hue,
    pub tabs: &'a [TabView<'a>],
    pub active: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Back,
    Forward,
    Reload,
    Address,
}

/// Which header control is at `x` (points), for a header `width` points wide.
pub fn hit(x: f32, width: f32) -> Option<Button> {
    let slot = ((x - BUTTON_START) / BUTTON_WIDTH).floor();
    match slot as i32 {
        _ if x < BUTTON_START => None,
        0 => Some(Button::Back),
        1 => Some(Button::Forward),
        2 => Some(Button::Reload),
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
        for shape in [Shape::Back, Shape::Forward, Shape::Reload, Shape::Close] {
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
        let focused_slot = match view.focused {
            Some(Button::Back) => Some(0),
            Some(Button::Forward) => Some(1),
            Some(Button::Reload) => Some(2),
            _ => None,
        };
        if let Some(slot) = focused_slot {
            // Focus ring: a 2px accent ring around the 24px button area (DESIGN.md, section 13).
            let ring = px(2.0);
            let (bx, by) = (
                px(BUTTON_START + BUTTON_WIDTH * slot as f32 + 2.0),
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

    /// Draw the sidebar into `frame` (its full height, `frame.width` device pixels wide).
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
        let rows = (frame.pixels.len() / frame.stride) as i32;
        let px = |v: f32| (v * s).round() as i32;
        let row_w = px(SIDEBAR_WIDTH - 2.0 * ROW_INSET) as u32;
        let row_h = px(TAB_ROW - 2.0) as u32;
        let icon_px = px(ICON) as u32;
        self.masks
            .entry((Shape::Close, icon_px, icon_px))
            .or_insert_with(|| icon(Shape::Close, s));
        self.masks
            .entry((Shape::Rounded, row_w, row_h))
            .or_insert_with(|| rounded_rect(row_w, row_h, 6.0 * s));

        let mut ops = vec![
            Op::Fill {
                x0: 0,
                y0: 0,
                x1: w,
                y1: rows,
                color: t.canvas,
            },
            // Hairline between the sidebar and the pane.
            Op::Fill {
                x0: w - px(1.0).max(1),
                y0: 0,
                x1: w,
                y1: rows,
                color: t.line,
            },
            // The profile switcher carries the same edge as the panes.
            Op::Fill {
                x0: 0,
                y0: 0,
                x1: w - px(1.0).max(1),
                y1: px(EDGE).max(1),
                color: t.profile(view.hue),
            },
        ];
        let mut text_ops = Vec::new();
        text_ops.push(Op::Text {
            x: ROW_TEXT * s,
            baseline: px(26.0) as f32,
            text: view.profile,
            size: 13.0 * s,
            bold: true,
            color: t.text,
        });
        // "Search or go to ⌘K": a field look-alike that opens the palette.
        let search = (row_w, px(SEARCH_HEIGHT) as u32);
        let key_px = px(KEY_ICON) as u32;
        self.masks
            .entry((Shape::Command, key_px, key_px))
            .or_insert_with(|| key_icon(Shape::Command, s));
        for (mw, mh, radius) in [
            (search.0, search.1, 6.0),
            (
                search.0 - 2 * px(1.0).max(1) as u32,
                search.1 - 2 * px(1.0).max(1) as u32,
                5.0,
            ),
        ] {
            self.masks
                .entry((Shape::Rounded, mw, mh))
                .or_insert_with(|| rounded_rect(mw, mh, radius * s));
        }
        let hair = px(1.0).max(1);
        ops.push(Op::Mask {
            x: px(ROW_INSET),
            y: px(SEARCH_TOP),
            mask: &self.masks[&(Shape::Rounded, search.0, search.1)],
            color: t.line,
        });
        ops.push(Op::Mask {
            x: px(ROW_INSET) + hair,
            y: px(SEARCH_TOP) + hair,
            mask: &self.masks[&(
                Shape::Rounded,
                search.0 - 2 * hair as u32,
                search.1 - 2 * hair as u32,
            )],
            color: t.surface,
        });
        text_ops.push(Op::Text {
            x: ROW_TEXT * s,
            baseline: px(SEARCH_TOP + 19.5) as f32,
            text: "Search or go to",
            size: 13.0 * s,
            bold: false,
            color: t.text_2,
        });
        // Past the titles' clip, so it goes with the shapes.
        let hint_x =
            px(SIDEBAR_WIDTH - ROW_INSET - 10.0) as f32 - keys_width("⌘K", s, 12.0, metrics);
        push_keys(
            &mut ops,
            &self.masks,
            "⌘K",
            (hint_x, px(SEARCH_TOP + 19.5) as f32),
            (s, 12.0),
            t.text_2,
            metrics,
        );
        let row_top = |i: usize| TABS_TOP + TAB_ROW * i as f32;
        let masks = &self.masks;
        for (i, tab) in view.tabs.iter().enumerate() {
            let active = i == view.active;
            if active {
                ops.push(Op::Mask {
                    x: px(ROW_INSET),
                    y: px(row_top(i) + 1.0),
                    mask: &masks[&(Shape::Rounded, row_w, row_h)],
                    color: t.selected,
                });
                ops.push(Op::Mask {
                    x: px(SIDEBAR_WIDTH - ROW_INSET - CLOSE_SIZE + (CLOSE_SIZE - ICON) / 2.0),
                    y: px(row_top(i) + (TAB_ROW - ICON) / 2.0),
                    mask: &masks[&(Shape::Close, icon_px, icon_px)],
                    color: t.text_2,
                });
            }
            // `muted` never sits on `selected` (DESIGN.md, section 3): a selected tab is awake.
            let color = if tab.asleep && !active {
                t.muted
            } else {
                t.text
            };
            text_ops.push(Op::Text {
                x: ROW_TEXT * s,
                baseline: px(row_top(i) + 19.5) as f32,
                text: tab.title,
                size: 13.0 * s,
                bold: false,
                color,
            });
        }
        let new_row = view.tabs.len();
        text_ops.push(Op::Text {
            x: ROW_TEXT * s,
            baseline: px(row_top(new_row) + 19.5) as f32,
            text: "+ New tab",
            size: 13.0 * s,
            bold: false,
            color: t.text_2,
        });
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
        // Titles stop short of the close button.
        let text_clip = (px(ROW_INSET), px(SIDEBAR_WIDTH - ROW_INSET - CLOSE_SIZE));
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

        // Palette masks depend on the number of rows: keep only the current size's.
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
        for px in row[(left * 4).min(end)..end].chunks_exact_mut(4) {
            px.copy_from_slice(&[color.b, color.g, color.r, 255]);
        }
    }
}

/// `text` cut to fit `max` device pixels at `size`, ending with "…" when cut.
fn fit<'t>(text: &'t str, max: f32, size: f32, metrics: &dyn TextMetrics) -> Cow<'t, str> {
    if metrics.width(text, size, false) <= max {
        return Cow::Borrowed(text);
    }
    let room = max - metrics.width("…", size, false);
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
        if metrics.width(&text[..ends[mid - 1]], size, false) <= room {
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
        assert_eq!(hit(797.0, 800.0), None);
    }

    #[test]
    fn hit_tests_the_sidebar() {
        let hit = |x, y| sidebar_hit(x, y, 3, 1);
        assert_eq!(hit(100.0, 10.0), Some(SidebarItem::Profile));
        assert_eq!(hit(100.0, 50.0), Some(SidebarItem::Search));
        assert_eq!(hit(100.0, 78.0), None, "gap above the tabs");
        assert_eq!(hit(100.0, 90.0), Some(SidebarItem::Tab(0)));
        assert_eq!(
            hit(220.0, 90.0),
            Some(SidebarItem::Tab(0)),
            "only the active row has a close button"
        );
        assert_eq!(hit(220.0, 120.0), Some(SidebarItem::CloseTab(1)));
        assert_eq!(hit(100.0, 150.0), Some(SidebarItem::Tab(2)));
        assert_eq!(hit(100.0, 180.0), Some(SidebarItem::NewTab));
        assert_eq!(hit(100.0, 215.0), None);
        assert_eq!(hit(250.0, 90.0), None, "the pane");
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
        for shape in [Shape::Back, Shape::Forward, Shape::Reload, Shape::Close] {
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
