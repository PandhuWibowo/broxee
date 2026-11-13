//! Rounded rectangles, gradients and shadows: what `border-radius`, `background-image`
//! gradients and `box-shadow` draw. Everything is in device pixels and evaluated per pixel
//! during compositing, so nothing is rasterized ahead and nothing is cached.

use css::{Color, Gradient, GradientKind, Length, RadialSize};

/// A rectangle with elliptical corners, in device pixels. Corners are top left, top right,
/// bottom right, bottom left, each `[horizontal, vertical]` radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub r: [[f32; 2]; 4],
}

impl RRect {
    /// A rectangle with these radii, scaled down together when adjacent ones would overlap
    /// (CSS Backgrounds 5.5).
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32, r: [[f32; 2]; 4]) -> RRect {
        let (w, h) = ((x1 - x0).max(0.0), (y1 - y0).max(0.0));
        let mut f = 1.0f32;
        for (sum, side) in [
            (r[0][0] + r[1][0], w),
            (r[3][0] + r[2][0], w),
            (r[0][1] + r[3][1], h),
            (r[1][1] + r[2][1], h),
        ] {
            if sum > side {
                f = f.min(side / sum);
            }
        }
        let r = r.map(|[a, b]| {
            let (a, b) = ((a * f).max(0.0), (b * f).max(0.0));
            // A corner with one radius zero is square.
            if a == 0.0 || b == 0.0 {
                [0.0, 0.0]
            } else {
                [a, b]
            }
        });
        RRect { x0, y0, x1, y1, r }
    }

    /// The edge `d` pixels outside (negative: inside), radii growing or shrinking with it.
    pub fn outset(&self, d: [f32; 4]) -> RRect {
        let [top, right, bottom, left] = d;
        let grow = |[rx, ry]: [f32; 2], dx: f32, dy: f32| {
            if rx == 0.0 {
                [0.0, 0.0]
            } else {
                [(rx + dx).max(0.0), (ry + dy).max(0.0)]
            }
        };
        RRect::new(
            self.x0 - left,
            self.y0 - top,
            self.x1 + right,
            self.y1 + bottom,
            [
                grow(self.r[0], left, top),
                grow(self.r[1], right, top),
                grow(self.r[2], right, bottom),
                grow(self.r[3], left, bottom),
            ],
        )
    }

    pub fn translate(&self, dx: f32, dy: f32) -> RRect {
        RRect {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
            r: self.r,
        }
    }

    pub fn is_rounded(&self) -> bool {
        self.r.iter().any(|c| c[0] > 0.0)
    }

    /// Signed distance from (x, y) to the edge: negative inside. Exact for circular corners,
    /// a first-order estimate for elliptical ones.
    pub fn distance(&self, x: f32, y: f32) -> f32 {
        let corner = if x < self.x0 + self.r[0][0] && y < self.y0 + self.r[0][1] {
            Some((self.x0 + self.r[0][0], self.y0 + self.r[0][1], self.r[0]))
        } else if x > self.x1 - self.r[1][0] && y < self.y0 + self.r[1][1] {
            Some((self.x1 - self.r[1][0], self.y0 + self.r[1][1], self.r[1]))
        } else if x > self.x1 - self.r[2][0] && y > self.y1 - self.r[2][1] {
            Some((self.x1 - self.r[2][0], self.y1 - self.r[2][1], self.r[2]))
        } else if x < self.x0 + self.r[3][0] && y > self.y1 - self.r[3][1] {
            Some((self.x0 + self.r[3][0], self.y1 - self.r[3][1], self.r[3]))
        } else {
            None
        };
        match corner {
            Some((cx, cy, [rx, ry])) if rx > 0.0 => {
                let (dx, dy) = (x - cx, y - cy);
                if rx == ry {
                    return (dx * dx + dy * dy).sqrt() - rx;
                }
                let k = ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt();
                if k < 1e-6 {
                    return -rx.min(ry);
                }
                let gx = dx / (rx * rx * k);
                let gy = dy / (ry * ry * k);
                (k - 1.0) / (gx * gx + gy * gy).sqrt().max(1e-6)
            }
            _ => {
                let dx = (self.x0 - x).max(x - self.x1);
                let dy = (self.y0 - y).max(y - self.y1);
                if dx > 0.0 && dy > 0.0 {
                    (dx * dx + dy * dy).sqrt()
                } else {
                    dx.max(dy)
                }
            }
        }
    }

    /// The shape's left and right edge along the horizontal line at `y`, if it crosses it.
    pub fn span(&self, y: f32) -> Option<(f32, f32)> {
        if y < self.y0 || y > self.y1 {
            return None;
        }
        // How far a corner's curve is inset at `y`, when `y` is in that corner's rows.
        let inset = |[rx, ry]: [f32; 2], from_edge: f32| {
            if rx <= 0.0 || from_edge >= ry {
                return 0.0;
            }
            let t = (ry - from_edge) / ry;
            rx * (1.0 - (1.0 - t * t).max(0.0).sqrt())
        };
        let (top, bottom) = (y - self.y0, self.y1 - y);
        let left = inset(self.r[0], top).max(inset(self.r[3], bottom));
        let right = inset(self.r[1], top).max(inset(self.r[2], bottom));
        Some((self.x0 + left, self.x1 - right))
    }

    /// For the pixel row from `y` to `y + 1`: the columns every pixel left of which lies
    /// outside (and right of `out.1`), and the columns between which pixels lie wholly inside.
    /// The shape is convex, so a pixel square is inside when its four corners are.
    pub fn row_spans(&self, y: f32) -> RowSpans {
        let (top, mid, bottom) = (self.span(y), self.span(y + 0.5), self.span(y + 1.0));
        let inside = match (top, bottom) {
            (Some(t), Some(b)) => Some((t.0.max(b.0), t.1.min(b.1))),
            _ => None,
        };
        let mut outside: Option<(f32, f32)> = None;
        for (l, r) in [top, mid, bottom].into_iter().flatten() {
            outside = Some(match outside {
                Some((ol, or)) => (ol.min(l), or.max(r)),
                None => (l, r),
            });
        }
        RowSpans {
            // One pixel of slack: the curve can bulge between the three samples.
            outside: outside.map(|(l, r)| (l - 1.0, r + 1.0)),
            inside,
        }
    }

    /// How much of the pixel whose center is (x, y) the shape covers, 0 to 1.
    pub fn coverage(&self, x: f32, y: f32) -> f32 {
        (0.5 - self.distance(x, y)).clamp(0.0, 1.0)
    }

    /// Pixels certainly fully inside (coverage 1), as an integer rectangle shrunk by `margin`:
    /// past the corners' radii, so no distance needs computing there. May be empty.
    pub fn interior(&self, margin: f32) -> [i32; 4] {
        let left = self.r[0][0].max(self.r[3][0]);
        let right = self.r[1][0].max(self.r[2][0]);
        let top = self.r[0][1].max(self.r[1][1]);
        let bottom = self.r[2][1].max(self.r[3][1]);
        [
            (self.x0 + left + margin).ceil() as i32,
            (self.y0 + top + margin).ceil() as i32,
            (self.x1 - right - margin).floor() as i32,
            (self.y1 - bottom - margin).floor() as i32,
        ]
    }

    /// Integer pixel bounds, grown by `margin`.
    pub fn bounds(&self, margin: f32) -> [i32; 4] {
        [
            (self.x0 - margin).floor() as i32,
            (self.y0 - margin).floor() as i32,
            (self.x1 + margin).ceil() as i32,
            (self.y1 + margin).ceil() as i32,
        ]
    }
}

/// Where a row of pixels is certainly outside or wholly inside a shape (`RRect::row_spans`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowSpans {
    /// Pixels ending at or before `.0`, or starting at or after `.1`, are outside; `None`: all.
    pub outside: Option<(f32, f32)>,
    /// Pixels starting at or after `.0` and ending by `.1` are wholly inside.
    pub inside: Option<(f32, f32)>,
}

impl RowSpans {
    /// 0 when the pixel at column `x` is outside, 1 when wholly inside, `None` when its
    /// coverage needs working out.
    #[inline]
    pub fn known(&self, x: i32) -> Option<f32> {
        let (x0, x1) = (x as f32, x as f32 + 1.0);
        match self.outside {
            None => return Some(0.0),
            Some((l, r)) if x1 <= l || x0 >= r => return Some(0.0),
            _ => {}
        }
        match self.inside {
            Some((l, r)) if x0 >= l && x1 <= r => Some(1.0),
            _ => None,
        }
    }
}

/// Resolve a style's radii (px or percentages of the box) to device pixels.
pub(crate) fn device_radii(radii: [[Length; 2]; 4], w: f32, h: f32, scale: f32) -> [[f32; 2]; 4] {
    let len = |l: Length, of: f32| match l {
        Length::Px(px) => px.to_f32() * scale,
        Length::Percent(p) => p as f32 / 10000.0 * of,
        Length::Auto => 0.0,
    };
    radii.map(|[rx, ry]| [len(rx, w), len(ry, h)])
}

/// Premultiplied RGBA, 0 to 1.
type Rgba = [f32; 4];

fn premultiply(c: Color) -> Rgba {
    let a = f32::from(c.a) / 255.0;
    [
        f32::from(c.r) / 255.0 * a,
        f32::from(c.g) / 255.0 * a,
        f32::from(c.b) / 255.0 * a,
        a,
    ]
}

#[derive(Clone, Debug, PartialEq)]
enum Line {
    /// t = (p - start) . dir, with `dir` already divided by the line's length.
    Linear { sx: f32, sy: f32, dx: f32, dy: f32 },
    /// t = the point's distance from the center, in units of the radii.
    Radial { cx: f32, cy: f32, rx: f32, ry: f32 },
}

/// Colors per gradient, sampled evenly between the first and last stop: per pixel, finding
/// the stops and interpolating in floats tripled GitHub's composite time.
const LUT: usize = 1024;

/// A gradient laid over one box, ready to evaluate per device pixel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceGradient {
    line: Line,
    /// Positions along the line (0 at the start, 1 at the end), non-decreasing.
    stops: Vec<(f32, Rgba)>,
    repeating: bool,
    /// Premultiplied colors from the first stop's position to the last's.
    lut: Box<[[u8; 4]]>,
    /// The same colors as little-endian pixels, in RGBA and in BGRA order, for row fills.
    pixels: [Box<[u32]>; 2],
    first: f32,
    span: f32,
}

impl DeviceGradient {
    /// Lay `g` over the box (x0, y0, x1, y1) in device pixels. `None` when it has no size.
    pub fn new(g: &Gradient, rect: [f32; 4], scale: f32) -> Option<DeviceGradient> {
        let [x0, y0, x1, y1] = rect;
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let (line, length) = match g.kind {
            GradientKind::Linear(_) | GradientKind::LinearCorner(..) => {
                let (dx, dy) = match g.kind {
                    GradientKind::Linear(angle) => {
                        let a = (angle as f32 / 100.0).to_radians();
                        (a.sin(), -a.cos())
                    }
                    GradientKind::LinearCorner(cx, cy) => {
                        // Perpendicular to the diagonal between the other two corners.
                        let (vx, vy) = (f32::from(cx) * h, f32::from(cy) * w);
                        let n = (vx * vx + vy * vy).sqrt().max(1e-6);
                        (vx / n, vy / n)
                    }
                    _ => unreachable!(),
                };
                let length = (w * dx).abs() + (h * dy).abs();
                let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                let line = Line::Linear {
                    sx: cx - dx * length / 2.0,
                    sy: cy - dy * length / 2.0,
                    dx: dx / length.max(1e-6),
                    dy: dy / length.max(1e-6),
                };
                (line, length)
            }
            GradientKind::Radial { circle, size, at } => {
                let pos = |l: Length, of: f32| match l {
                    Length::Px(px) => px.to_f32() * scale,
                    Length::Percent(p) => p as f32 / 10000.0 * of,
                    Length::Auto => of / 2.0,
                };
                let (cx, cy) = (x0 + pos(at.0, w), y0 + pos(at.1, h));
                let sides_x = [(cx - x0).abs(), (x1 - cx).abs()];
                let sides_y = [(cy - y0).abs(), (y1 - cy).abs()];
                let (near_x, far_x) = (sides_x[0].min(sides_x[1]), sides_x[0].max(sides_x[1]));
                let (near_y, far_y) = (sides_y[0].min(sides_y[1]), sides_y[0].max(sides_y[1]));
                let (rx, ry) = match size {
                    Err((rx, ry)) => {
                        let rx = pos(rx, w);
                        (rx, if circle { rx } else { pos(ry, h) })
                    }
                    Ok(keyword) => {
                        let (side_x, side_y) = match keyword {
                            RadialSize::ClosestSide | RadialSize::ClosestCorner => (near_x, near_y),
                            _ => (far_x, far_y),
                        };
                        let corner = matches!(
                            keyword,
                            RadialSize::ClosestCorner | RadialSize::FarthestCorner
                        );
                        match (circle, corner) {
                            (true, false) => {
                                let r = if keyword == RadialSize::ClosestSide {
                                    side_x.min(side_y)
                                } else {
                                    side_x.max(side_y)
                                };
                                (r, r)
                            }
                            (true, true) => {
                                let r = (side_x * side_x + side_y * side_y).sqrt();
                                (r, r)
                            }
                            (false, false) => (side_x, side_y),
                            // The ellipse through the corner keeps the sides' proportions.
                            (false, true) => (
                                side_x * std::f32::consts::SQRT_2,
                                side_y * std::f32::consts::SQRT_2,
                            ),
                        }
                    }
                };
                (
                    Line::Radial {
                        cx,
                        cy,
                        rx: rx.max(1e-3),
                        ry: ry.max(1e-3),
                    },
                    rx.max(1e-3),
                )
            }
        };
        let stops = resolve_stops(&g.stops, length, scale);
        let (first, last) = (stops[0].0, stops[stops.len() - 1].0);
        let mut gradient = DeviceGradient {
            line,
            stops,
            repeating: g.repeating,
            lut: Box::default(),
            pixels: [Box::default(), Box::default()],
            first,
            span: (last - first).max(1e-6),
        };
        gradient.lut = (0..LUT)
            .map(|i| {
                let t = first + gradient.span * i as f32 / (LUT - 1) as f32;
                gradient
                    .color(t)
                    .map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
            })
            .collect();
        gradient.pixels = [false, true].map(|bgra| {
            gradient
                .lut
                .iter()
                .map(|&[r, g, b, a]| {
                    u32::from_le_bytes(if bgra { [b, g, r, a] } else { [r, g, b, a] })
                })
                .collect()
        });
        Some(gradient)
    }

    /// The premultiplied color, as bytes, at the device pixel whose center is (x, y).
    #[inline]
    pub fn sample(&self, x: f32, y: f32) -> [u8; 4] {
        let mut t = self.position(x, y);
        if self.repeating && self.span > 1e-4 {
            t = self.first + (t - self.first).rem_euclid(self.span);
        }
        let i = ((t - self.first) / self.span * (LUT - 1) as f32).round();
        self.lut[i.clamp(0.0, (LUT - 1) as f32) as usize]
    }

    /// For a linear gradient, the color table index at the pixel center (x, y) and its step per
    /// pixel to the right: along a row it changes by a constant, so rows are filled without
    /// working out each pixel's position. `None` for radial gradients.
    #[inline]
    pub fn row_index(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let Line::Linear { dx, .. } = self.line else {
            return None;
        };
        let k = (LUT - 1) as f32 / self.span;
        Some(((self.position(x, y) - self.first) * k, dx * k))
    }

    /// Whether every row is the same (a horizontal gradient) or every row is one color (a
    /// vertical one): the two most common gradients, which fill by copying.
    pub fn axis(&self) -> Axis {
        match self.line {
            Line::Linear { dy, .. } if dy.abs() < 1e-6 => Axis::Horizontal,
            Line::Linear { dx, .. } if dx.abs() < 1e-6 => Axis::Vertical,
            _ => Axis::Other,
        }
    }

    /// Whether every color in the table is opaque, so pixels are copied rather than blended.
    pub fn opaque(&self) -> bool {
        self.lut.iter().all(|c| c[3] == 255)
    }

    /// For a radial gradient, along a row: the horizontal term at (x, y), its step per pixel,
    /// and the row's vertical term, so the position is `sqrt(ax * ax + by)` with `ax` stepping.
    pub fn radial_row(&self, x: f32, y: f32) -> (f32, f32, f32) {
        match self.line {
            Line::Radial { cx, cy, rx, ry } => ((x - cx) / rx, 1.0 / rx, ((y - cy) / ry).powi(2)),
            Line::Linear { .. } => (0.0, 0.0, 0.0),
        }
    }

    /// The color at position `t` along the line, from the table.
    #[inline]
    pub fn at_position(&self, t: f32) -> [u8; 4] {
        self.at_index((t - self.first) * ((LUT - 1) as f32 / self.span))
    }

    /// The color at a whole table index as a pixel in the given byte order.
    #[inline]
    pub fn pixel_at(&self, i: i64, bgra: bool) -> u32 {
        let last = (LUT - 1) as i64;
        let i = if self.repeating {
            i.rem_euclid(last)
        } else {
            i.clamp(0, last)
        };
        self.pixels[usize::from(bgra)][i as usize]
    }

    /// The color for a table index from `row_index`, wrapping when the gradient repeats.
    #[inline]
    pub fn at_index(&self, i: f32) -> [u8; 4] {
        let last = (LUT - 1) as f32;
        let i = if self.repeating {
            i.rem_euclid(last)
        } else {
            i.clamp(0.0, last)
        };
        self.lut[(i + 0.5) as usize]
    }

    #[inline]
    fn position(&self, x: f32, y: f32) -> f32 {
        match self.line {
            Line::Linear { sx, sy, dx, dy } => (x - sx) * dx + (y - sy) * dy,
            Line::Radial { cx, cy, rx, ry } => {
                (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt()
            }
        }
    }

    /// The premultiplied color at the device pixel whose center is (x, y), exactly.
    #[cfg(test)]
    pub fn at(&self, x: f32, y: f32) -> Rgba {
        let mut t = self.position(x, y);
        if self.repeating && self.span > 1e-4 {
            t = self.first + (t - self.first).rem_euclid(self.span);
        }
        self.color(t)
    }

    /// The premultiplied color at position `t` along the line.
    fn color(&self, t: f32) -> Rgba {
        let i = self.stops.partition_point(|&(p, _)| p <= t);
        if i == 0 {
            return self.stops[0].1;
        }
        if i == self.stops.len() {
            return self.stops[i - 1].1;
        }
        let ((p0, c0), (p1, c1)) = (self.stops[i - 1], self.stops[i]);
        let f = if p1 > p0 { (t - p0) / (p1 - p0) } else { 1.0 };
        [0, 1, 2, 3].map(|k| c0[k] + (c1[k] - c0[k]) * f)
    }
}

/// Stop positions along a line of `length` device pixels (CSS Images 3.5.3): missing first and
/// last are 0 and 1, positions never go backwards, missing ones are spread evenly.
fn resolve_stops(stops: &[(Color, Length)], length: f32, scale: f32) -> Vec<(f32, Rgba)> {
    let n = stops.len();
    let mut pos: Vec<Option<f32>> = stops
        .iter()
        .map(|(_, l)| match *l {
            Length::Px(px) => Some(px.to_f32() * scale / length.max(1e-6)),
            Length::Percent(p) => Some(p as f32 / 10000.0),
            Length::Auto => None,
        })
        .collect();
    if pos[0].is_none() {
        pos[0] = Some(0.0);
    }
    if pos[n - 1].is_none() {
        pos[n - 1] = Some(1.0);
    }
    let mut max = f32::MIN;
    for p in pos.iter_mut().flatten() {
        max = max.max(*p);
        *p = max;
    }
    let mut i = 0;
    while i < n {
        if pos[i].is_some() {
            i += 1;
            continue;
        }
        let start = i - 1;
        let end = (i..n).find(|&j| pos[j].is_some()).unwrap_or(n - 1);
        let (a, b) = (pos[start].unwrap_or(0.0), pos[end].unwrap_or(1.0));
        for (k, p) in pos.iter_mut().enumerate().take(end).skip(i) {
            *p = Some(a + (b - a) * (k - start) as f32 / (end - start) as f32);
        }
        i = end;
    }
    stops
        .iter()
        .zip(pos)
        .map(|((c, _), p)| (p.unwrap_or(0.0), premultiply(*c)))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
    Other,
}

/// The error function, within 1.5e-7 (Abramowitz and Stegun 7.1.26).
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_7) * t - 0.284_496_74) * t
            + 0.254_829_6)
            * t
            * (-x * x).exp();
    y.copysign(x)
}

/// How dark a shadow of `shape` blurred with standard deviation `sigma` is at (x, y), 0 to 1:
/// a Gaussian across the edge, which is exact for straight edges and close at round corners.
pub(crate) fn shadow_coverage(shape: &RRect, sigma: f32, x: f32, y: f32) -> f32 {
    if sigma < 0.5 {
        return shape.coverage(x, y);
    }
    half_erfc(shape.distance(x, y) / (sigma * std::f32::consts::SQRT_2))
}

/// How much of a Gaussian of standard deviation `sigma` centered at `c` falls between `lo` and
/// `hi`: one axis of a blurred rectangle, which is the product of its two axes.
pub(crate) fn blur_span(lo: f32, hi: f32, sigma: f32, c: f32) -> f32 {
    let k = 1.0 / (sigma * std::f32::consts::SQRT_2);
    (half_erfc((lo - c) * k) - half_erfc((hi - c) * k)).max(0.0)
}

/// `(1 - erf(x)) / 2` from a table, interpolated: `exp` per pixel made large blurred shadows
/// the slowest thing on singapay.id.
fn half_erfc(x: f32) -> f32 {
    const RANGE: f32 = 3.5;
    const N: usize = 512;
    static TABLE: std::sync::OnceLock<[f32; N + 1]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0.0; N + 1];
        for (i, v) in t.iter_mut().enumerate() {
            let x = -RANGE + 2.0 * RANGE * i as f32 / N as f32;
            *v = 0.5 * (1.0 - erf(x));
        }
        t
    });
    if x <= -RANGE {
        return 1.0;
    }
    if x >= RANGE {
        return 0.0;
    }
    let f = (x + RANGE) / (2.0 * RANGE) * N as f32;
    let i = (f as usize).min(N - 1);
    let k = f - i as f32;
    table[i] + (table[i + 1] - table[i]) * k
}

#[cfg(test)]
mod tests {
    use super::*;
    use css::Px;

    #[test]
    fn rounded_corners_cover_by_distance() {
        let r = RRect::new(0.0, 0.0, 100.0, 50.0, [[10.0, 10.0]; 4]);
        assert_eq!(r.coverage(50.0, 25.0), 1.0, "inside");
        assert_eq!(r.coverage(0.5, 0.5), 0.0, "the corner is cut");
        assert_eq!(r.coverage(10.5, 0.5), 1.0, "where the curve ends");
        let on_arc = 10.0 - 10.0 * std::f32::consts::FRAC_1_SQRT_2;
        assert!(r.distance(on_arc, on_arc).abs() < 1e-3);
        assert_eq!(r.distance(150.0, 25.0), 50.0);
        assert_eq!(r.distance(50.0, 10.0), -10.0);
    }

    #[test]
    fn row_spans_agree_with_coverage() {
        for r in [
            RRect::new(3.0, 2.0, 61.0, 40.0, [[12.0, 12.0]; 4]),
            RRect::new(0.0, 0.0, 300.0, 40.0, [[150.0, 20.0]; 4]),
            RRect::new(
                5.0,
                5.0,
                50.0,
                90.0,
                [[20.0, 30.0], [0.0, 0.0], [8.0, 8.0], [40.0, 10.0]],
            ),
        ] {
            for y in -2..95 {
                let spans = r.row_spans(y as f32);
                for x in -3..310 {
                    if let Some(known) = spans.known(x) {
                        let exact = r.coverage(x as f32 + 0.5, y as f32 + 0.5);
                        assert_eq!(known, exact, "{r:?} at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_blurred_rectangle_is_the_product_of_its_axes() {
        let r = RRect::new(0.0, 0.0, 100.0, 60.0, [[0.0; 2]; 4]);
        for (x, y) in [
            (50.0, 30.0),
            (100.0, 30.0),
            (110.0, 30.0),
            (100.0, 60.0),
            (-8.0, 70.0),
        ] {
            let separable = blur_span(r.x0, r.x1, 6.0, x) * blur_span(r.y0, r.y1, 6.0, y);
            let exact_edge = shadow_coverage(&r, 6.0, x, y);
            // Along a straight edge, away from corners, both are the same 1D profile.
            if (0.0..60.0).contains(&y) && (y - 30.0).abs() < 1.0 {
                assert!((separable - exact_edge).abs() < 1e-3, "{x},{y}");
            }
            assert!((0.0..=1.0).contains(&separable));
        }
        assert!(
            (blur_span(0.0, 100.0, 6.0, 0.0) - 0.5).abs() < 1e-3,
            "half at an edge"
        );
    }

    #[test]
    fn the_erfc_table_is_close() {
        for i in -50..50 {
            let x = i as f32 / 10.0;
            assert!((half_erfc(x) - 0.5 * (1.0 - erf(x))).abs() < 2e-4, "{x}");
        }
    }

    #[test]
    fn overlapping_radii_shrink_together() {
        let r = RRect::new(0.0, 0.0, 100.0, 40.0, [[50.0, 50.0]; 4]);
        assert_eq!(r.r[0], [20.0, 20.0], "50 + 50 on a 40 side: scaled by 0.4");
        let pill = RRect::new(
            0.0,
            0.0,
            100.0,
            40.0,
            device_radii([[Length::Percent(5000); 2]; 4], 100.0, 40.0, 1.0),
        );
        assert_eq!(pill.r[0], [50.0, 20.0], "50% is an ellipse");
    }

    #[test]
    fn insets_shrink_radii() {
        let r = RRect::new(0.0, 0.0, 100.0, 100.0, [[10.0, 10.0]; 4]);
        let inner = r.outset([-4.0; 4]);
        assert_eq!((inner.x0, inner.r[0]), (4.0, [6.0, 6.0]));
        assert_eq!(r.outset([-20.0; 4]).r[0], [0.0, 0.0]);
    }

    fn stop(c: Color, l: Length) -> (Color, Length) {
        (c, l)
    }

    #[test]
    fn linear_gradients_run_along_their_angle() {
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        let g = Gradient {
            kind: GradientKind::Linear(9000),
            repeating: false,
            stops: vec![stop(red, Length::Auto), stop(blue, Length::Auto)].into(),
        };
        let d = DeviceGradient::new(&g, [0.0, 0.0, 100.0, 10.0], 1.0).unwrap();
        let near = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4);
        assert!(near(d.at(0.0, 5.0), [1.0, 0.0, 0.0, 1.0]));
        assert!(near(d.at(100.0, 5.0), [0.0, 0.0, 1.0, 1.0]));
        let mid = d.at(50.0, 0.0);
        assert!((mid[0] - 0.5).abs() < 1e-4 && (mid[2] - 0.5).abs() < 1e-4);
        // To the bottom right corner: the far corner gets the last color.
        let corner = Gradient {
            kind: GradientKind::LinearCorner(1, 1),
            ..g.clone()
        };
        let d = DeviceGradient::new(&corner, [0.0, 0.0, 200.0, 100.0], 1.0).unwrap();
        assert!(d.at(200.0, 100.0)[2] > 0.99 && d.at(0.0, 0.0)[0] > 0.99);
        let (a, b) = (d.at(200.0, 0.0), d.at(0.0, 100.0));
        assert!(
            (a[0] - 0.5).abs() < 1e-3 && (b[0] - 0.5).abs() < 1e-3,
            "other corners at 50%"
        );
    }

    #[test]
    fn the_color_table_matches_exact_colors() {
        let g = Gradient {
            kind: GradientKind::Linear(13500),
            repeating: false,
            stops: vec![
                stop(Color::rgb(253, 230, 138), Length::Auto),
                stop(
                    Color {
                        a: 128,
                        ..Color::rgb(244, 63, 94)
                    },
                    Length::Percent(5000),
                ),
                stop(Color::rgb(30, 58, 138), Length::Auto),
            ]
            .into(),
        };
        let d = DeviceGradient::new(&g, [0.0, 0.0, 300.0, 120.0], 1.0).unwrap();
        for (x, y) in [
            (0.5, 0.5),
            (150.5, 60.5),
            (299.5, 119.5),
            (77.5, 10.5),
            (200.5, 3.5),
        ] {
            let exact = d.at(x, y).map(|v| v * 255.0);
            let table = d.sample(x, y);
            for k in 0..4 {
                assert!(
                    (exact[k] - f32::from(table[k])).abs() <= 1.0,
                    "{x},{y}: {exact:?} {table:?}"
                );
            }
        }
    }

    #[test]
    fn stops_are_fixed_up_and_repeat() {
        let c = Color::rgb(0, 0, 0);
        let s = resolve_stops(
            &[
                stop(c, Length::Auto),
                stop(c, Length::Percent(8000)),
                stop(c, Length::Percent(2000)),
                stop(c, Length::Auto),
                stop(c, Length::Auto),
            ],
            100.0,
            1.0,
        );
        let p: Vec<f32> = s.iter().map(|s| s.0).collect();
        assert_eq!(p, [0.0, 0.8, 0.8, 0.9, 1.0], "never backwards, then spread");
        let px = resolve_stops(
            &[stop(c, Length::Px(Px::new(10.0))), stop(c, Length::Auto)],
            50.0,
            2.0,
        );
        assert_eq!(px[0].0, 0.4, "device pixels over the line length");
        let white = Color::rgb(255, 255, 255);
        let g = Gradient {
            kind: GradientKind::Linear(9000),
            repeating: true,
            stops: vec![
                stop(c, Length::Px(Px::ZERO)),
                stop(white, Length::Px(Px::new(10.0))),
            ]
            .into(),
        };
        let d = DeviceGradient::new(&g, [0.0, 0.0, 100.0, 10.0], 1.0).unwrap();
        assert!(
            (d.at(25.0, 5.0)[0] - d.at(5.0, 5.0)[0]).abs() < 1e-4,
            "repeats every 10px"
        );
    }

    #[test]
    fn radial_gradients_reach_the_farthest_corner() {
        let g = Gradient {
            kind: GradientKind::Radial {
                circle: true,
                size: Ok(RadialSize::FarthestCorner),
                at: (Length::Percent(5000), Length::Percent(5000)),
            },
            repeating: false,
            stops: vec![
                stop(Color::rgb(255, 0, 0), Length::Auto),
                stop(Color::rgb(0, 0, 255), Length::Auto),
            ]
            .into(),
        };
        let d = DeviceGradient::new(&g, [0.0, 0.0, 60.0, 80.0], 1.0).unwrap();
        assert_eq!(d.at(30.0, 40.0), [1.0, 0.0, 0.0, 1.0], "center");
        assert!(d.at(0.0, 0.0)[2] > 0.999, "corner");
    }

    #[test]
    fn shadows_fade_across_the_edge() {
        let r = RRect::new(0.0, 0.0, 100.0, 100.0, [[0.0; 2]; 4]);
        assert!(
            (shadow_coverage(&r, 4.0, 100.0, 50.0) - 0.5).abs() < 1e-3,
            "half at the edge"
        );
        assert!(
            shadow_coverage(&r, 4.0, 112.0, 50.0) < 0.002,
            "gone 3 sigma out"
        );
        assert!(shadow_coverage(&r, 4.0, 90.0, 50.0) > 0.99);
        assert!((erf(1.0) - 0.842_700_8).abs() < 1e-6);
    }
}
