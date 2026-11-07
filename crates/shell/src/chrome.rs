//! Broxee's own interface: the pane header from DESIGN.md (back, forward, reload, the host,
//! the address field, the loading line). Drawn with the engine's painter on top of the page.

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

/// Color tokens from DESIGN.md, section 3. Every text pair was checked for WCAG AA there.
#[derive(Clone, Copy)]
pub struct Theme {
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

pub struct FieldView<'a> {
    pub text: &'a str,
    /// Byte offset of the caret.
    pub cursor: usize,
    pub all_selected: bool,
}

pub struct HeaderView<'a> {
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
    /// Rounded rectangle; the key also holds its device size.
    Rounded,
}

pub struct Chrome {
    theme: Theme,
    masks: HashMap<(Shape, u32, u32), GlyphBitmap>,
}

pub struct Frame<'a> {
    pub pixels: &'a mut [u8],
    pub stride: usize,
    /// Visible width in device pixels.
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
        for shape in [Shape::Back, Shape::Forward, Shape::Reload] {
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
            (Shape::Reload, view.loading.is_none()),
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
        match &view.field {
            None => {
                text_ops.push(Op::Text {
                    x: (FIELD_LEFT + 4.0) * s,
                    baseline: px(22.5) as f32,
                    text: view.host,
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

        let stride = frame.stride;
        painter.overlay(
            Target {
                pixels: &mut *frame.pixels,
                stride,
                bgra: true,
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
            },
            frame.width,
            text_clip,
            fonts,
            metrics,
            &text_ops,
        );
    }
}

/// Fill rows of a BGRA frame with one color (the page area before anything has loaded).
pub fn fill(pixels: &mut [u8], stride: usize, width: usize, color: Color) {
    for row in pixels.chunks_mut(stride) {
        let visible = (width * 4).min(row.len());
        for px in row[..visible].chunks_exact_mut(4) {
            px.copy_from_slice(&[color.b, color.g, color.r, 255]);
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
        Shape::Rounded => Vec::new(),
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
    fn icons_and_field_shapes_have_ink() {
        for shape in [Shape::Back, Shape::Forward, Shape::Reload] {
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
