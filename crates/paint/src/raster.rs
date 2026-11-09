//! Anti-aliased outline rasterizer using signed-area accumulation.
//!
//! Every line segment adds the area it covers to the cells it crosses, signed by direction.
//! A running sum over each row then gives the exact coverage of each pixel, so curves are
//! anti-aliased without supersampling. Overlapping contours of the same direction saturate
//! at full coverage.

use font::{Font, PathEl};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    x: f32,
    y: f32,
}

pub struct Rasterizer {
    width: usize,
    height: usize,
    /// One cell per pixel plus slack, because a segment touching the right edge writes one
    /// cell past the end of its row.
    cells: Vec<f32>,
}

impl Rasterizer {
    pub fn new(width: usize, height: usize) -> Rasterizer {
        Rasterizer {
            width,
            height,
            cells: vec![0.0; width * height + width + 4],
        }
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.draw_line(Point { x: x0, y: y0 }, Point { x: x1, y: y1 });
    }

    fn draw_line(&mut self, p0: Point, p1: Point) {
        if (p0.y - p1.y).abs() <= f32::EPSILON {
            return;
        }
        let (dir, p0, p1) = if p0.y < p1.y {
            (1.0, p0, p1)
        } else {
            (-1.0, p1, p0)
        };
        let dxdy = (p1.x - p0.x) / (p1.y - p0.y);
        let mut x = p0.x;
        if p0.y < 0.0 {
            x -= p0.y * dxdy;
        }
        let first_row = p0.y.max(0.0) as usize;
        let last_row = self.height.min(p1.y.ceil().max(0.0) as usize);
        for y in first_row..last_row {
            let row = y * self.width;
            let dy = ((y + 1) as f32).min(p1.y) - (y as f32).max(p0.y);
            let x_next = x + dxdy * dy;
            let d = dy * dir;
            let (xa, xb) = if x < x_next { (x, x_next) } else { (x_next, x) };
            let xa = xa.max(0.0);
            let xb = xb.max(0.0);
            let xa_floor = xa.floor();
            let ia = xa_floor as usize;
            let xb_ceil = xb.ceil();
            let ib = xb_ceil as usize;
            if ib <= ia + 1 {
                // The segment stays within one pixel column on this row.
                let mid = 0.5 * (x + x_next) - xa_floor;
                self.add(row + ia, d - d * mid);
                self.add(row + ia + 1, d * mid);
            } else {
                let s = (xb - xa).recip();
                let fa = xa - xa_floor;
                let a0 = 0.5 * s * (1.0 - fa) * (1.0 - fa);
                let fb = xb - xb_ceil + 1.0;
                let am = 0.5 * s * fb * fb;
                self.add(row + ia, d * a0);
                if ib == ia + 2 {
                    self.add(row + ia + 1, d * (1.0 - a0 - am));
                } else {
                    let a1 = s * (1.5 - fa);
                    self.add(row + ia + 1, d * (a1 - a0));
                    for i in ia + 2..ib - 1 {
                        self.add(row + i, d * s);
                    }
                    let a2 = a1 + (ib - ia - 3) as f32 * s;
                    self.add(row + ib - 1, d * (1.0 - a2 - am));
                }
                self.add(row + ib, d * am);
            }
            x = x_next;
        }
    }

    fn add(&mut self, index: usize, value: f32) {
        if let Some(cell) = self.cells.get_mut(index) {
            *cell += value;
        }
    }

    /// Flatten a quadratic curve into lines; the step count grows with the curve's deviation.
    fn draw_quad(&mut self, p0: Point, p1: Point, p2: Point) {
        let dev_x = p0.x - 2.0 * p1.x + p2.x;
        let dev_y = p0.y - 2.0 * p1.y + p2.y;
        let dev = dev_x * dev_x + dev_y * dev_y;
        if dev < 0.333 {
            self.draw_line(p0, p2);
            return;
        }
        let steps = 1 + (3.0 * dev).sqrt().sqrt().floor() as usize;
        let step = (steps as f32).recip();
        let mut prev = p0;
        let mut t = 0.0;
        for _ in 0..steps - 1 {
            t += step;
            let a = lerp(t, p0, p1);
            let b = lerp(t, p1, p2);
            let next = lerp(t, a, b);
            self.draw_line(prev, next);
            prev = next;
        }
        self.draw_line(prev, p2);
    }

    /// Running sum of the cells, clamped to [0, 1] and scaled to a byte per pixel.
    pub fn coverage(self) -> Vec<u8> {
        let mut acc = 0.0f32;
        self.cells[..self.width * self.height]
            .iter()
            .map(|&c| {
                acc += c;
                (acc.abs().min(1.0) * 255.0 + 0.5) as u8
            })
            .collect()
    }
}

fn lerp(t: f32, a: Point, b: Point) -> Point {
    Point {
        x: a.x + t * (b.x - a.x),
        y: a.y + t * (b.y - a.y),
    }
}

/// A rasterized glyph. `left` and `top` place the bitmap relative to the pen position on the
/// baseline (`top` is negative: the bitmap starts above the baseline).
#[derive(Clone, Debug, Default)]
pub struct GlyphBitmap {
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub coverage: Vec<u8>,
}

/// Glyphs larger than this (device px) are not drawn: they would be headings at absurd sizes,
/// and the bitmap would cost more memory than the whole page.
const MAX_GLYPH_PX: f32 = 512.0;

pub fn rasterize_glyph(face: &Font, glyph: u32, size_px: f32) -> GlyphBitmap {
    rasterize_glyph_at(face, glyph, size_px, 0.0)
}

/// `rasterize_glyph` with the outline moved right by `shift` pixels (0 to 1): the part of a
/// glyph's position that falls between pixels, so text is spaced as its advances say.
pub fn rasterize_glyph_at(face: &Font, glyph: u32, size_px: f32, shift: f32) -> GlyphBitmap {
    let Some(outline) = face.outline(glyph) else {
        return GlyphBitmap::default();
    };
    if !(size_px > 0.0 && size_px <= MAX_GLYPH_PX) {
        return GlyphBitmap::default();
    }
    let scale = size_px / face.units_per_em();
    let x0 = (outline.x_min * scale).floor() - 1.0;
    let top = (outline.y_max * scale).ceil() + 1.0;
    let width = ((outline.x_max * scale + shift).ceil() - x0 + 1.0).max(1.0) as usize;
    let height = (top - (outline.y_min * scale).floor() + 1.0).max(1.0) as usize;
    let map = |x: f32, y: f32| Point {
        x: x * scale - x0 + shift,
        y: top - y * scale,
    };

    let mut r = Rasterizer::new(width, height);
    let mut current = Point { x: 0.0, y: 0.0 };
    for el in &outline.path {
        match *el {
            PathEl::MoveTo(x, y) => current = map(x, y),
            PathEl::LineTo(x, y) => {
                let p = map(x, y);
                r.draw_line(current, p);
                current = p;
            }
            PathEl::QuadTo(cx, cy, x, y) => {
                let p = map(x, y);
                r.draw_quad(current, map(cx, cy), p);
                current = p;
            }
        }
    }
    GlyphBitmap {
        left: x0 as i32,
        top: -(top as i32),
        width,
        height,
        coverage: r.coverage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(r: &mut Rasterizer, x0: f32, y0: f32, x1: f32, y1: f32) {
        r.line(x0, y0, x1, y0);
        r.line(x1, y0, x1, y1);
        r.line(x1, y1, x0, y1);
        r.line(x0, y1, x0, y0);
    }

    #[test]
    fn shifted_glyphs_move_by_the_shift() {
        // Any font the machine has; the CI images have one of these.
        let font = [
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ]
        .iter()
        .find_map(|p| Font::load(std::path::Path::new(p)).ok());
        let Some(font) = font else {
            return;
        };
        let glyph = font.glyph('l');
        // The horizontal centre of the ink, in the bitmap's own coordinates plus `left`.
        let centre = |shift: f32| {
            let g = rasterize_glyph_at(&font, glyph, 24.0, shift);
            let (mut sum, mut weight) = (0.0, 0.0);
            for y in 0..g.height {
                for x in 0..g.width {
                    let c = f32::from(g.coverage[y * g.width + x]);
                    sum += c * (x as f32 + 0.5 + g.left as f32);
                    weight += c;
                }
            }
            sum / weight
        };
        let base = centre(0.0);
        for shift in [0.25, 0.5, 0.75] {
            let moved = centre(shift) - base;
            assert!(
                (moved - shift).abs() < 0.05,
                "shift {shift} moved the ink by {moved}"
            );
        }
    }

    #[test]
    fn fills_an_aligned_square_exactly() {
        let mut r = Rasterizer::new(6, 6);
        square(&mut r, 1.0, 1.0, 4.0, 4.0);
        let cov = r.coverage();
        for y in 0..6 {
            for x in 0..6 {
                let inside = (1..4).contains(&x) && (1..4).contains(&y);
                assert_eq!(
                    cov[y * 6 + x],
                    if inside { 255 } else { 0 },
                    "pixel {x},{y}"
                );
            }
        }
    }

    #[test]
    fn half_covered_pixels_are_half_grey() {
        let mut r = Rasterizer::new(4, 4);
        square(&mut r, 1.5, 1.0, 3.0, 3.0);
        let cov = r.coverage();
        assert_eq!(cov[4 + 1], 128, "left column is half covered");
        assert_eq!(cov[4 + 2], 255);
        assert_eq!(cov[4], 0);
    }

    #[test]
    fn handles_diagonals_and_out_of_bounds_points() {
        let mut r = Rasterizer::new(4, 4);
        r.line(-5.0, -5.0, 10.0, 2.0);
        r.line(10.0, 2.0, -5.0, -5.0);
        let _ = r.coverage();
        let mut r = Rasterizer::new(8, 8);
        r.line(0.0, 0.0, 8.0, 8.0);
        r.line(8.0, 8.0, 0.0, 8.0);
        r.line(0.0, 8.0, 0.0, 0.0);
        let cov = r.coverage();
        assert_eq!(cov[7 * 8], 255, "bottom-left is inside the triangle");
        assert_eq!(cov[7], 0, "top-right is outside");
        assert!(
            (100..160).contains(&cov[3 * 8 + 3]),
            "the diagonal pixel is about half: {}",
            cov[3 * 8 + 3]
        );
    }
}
