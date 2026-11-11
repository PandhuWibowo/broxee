//! SVG images, a useful subset: `path`, `rect`, `circle`, `ellipse`, `line`, `polyline`,
//! `polygon`, `g`, `use`, nested `svg`; `fill`, `stroke` (width, caps, joins), opacities,
//! `fill-rule`, `transform`, `viewBox` with `preserveAspectRatio`, inline `style` and
//! simple `<style>` rules (tag, `.class`, `#id`). Gradients are drawn with the average of
//! their stops. Text, filters, masks, clip paths, scripts and external references are
//! ignored.
//!
//! Shapes are flattened to polygons in output pixels and filled with an anti-aliased
//! scanline rasterizer that keeps one row of coverage. The only buffer the size of the
//! image is the output itself.

use crate::{Bitmap, Error, MAX_SIDE};

/// Default size of an SVG without width, height or viewBox, as for replaced elements.
const DEFAULT_SIZE: (f32, f32) = (300.0, 150.0);
/// Nested `use` references deeper than this are dropped (they can form cycles).
const MAX_DEPTH: u32 = 16;

// ---------------------------------------------------------------------------------------
// A small XML reader: elements, attributes and text, enough for SVG files.

struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<usize>,
    text: String,
}

struct Doc {
    nodes: Vec<Node>,
}

impl Doc {
    fn attr(&self, n: usize, name: &str) -> Option<&str> {
        self.nodes[n]
            .attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn find_id(&self, id: &str) -> Option<usize> {
        (0..self.nodes.len()).find(|&n| self.attr(n, "id") == Some(id))
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses `src` into elements. Malformed markup gives whatever could be read.
fn parse_xml(src: &str) -> Doc {
    let mut nodes = vec![Node {
        tag: String::new(),
        attrs: Vec::new(),
        children: Vec::new(),
        text: String::new(),
    }];
    let mut stack = vec![0usize];
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            let end = src[i..].find('<').map_or(b.len(), |e| i + e);
            if let Some(&top) = stack.last() {
                nodes[top].text.push_str(&decode_entities(&src[i..end]));
            }
            i = end;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").unwrap_or(rest.len());
            if let Some(&top) = stack.last() {
                nodes[top].text.push_str(&rest[9..end.max(9)]);
            }
            i += (end + 3).min(rest.len());
            continue;
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            i = rest.find('>').map_or(b.len(), |e| i + e + 1);
            continue;
        }
        if let Some(close) = rest.strip_prefix("</") {
            let end = close.find('>').unwrap_or(close.len());
            let name = close[..end].trim();
            if let Some(pos) = stack.iter().rposition(|&n| nodes[n].tag == name) {
                if pos > 0 {
                    stack.truncate(pos);
                }
            }
            i += 2 + end + 1;
            continue;
        }
        // Start tag.
        let mut j = i + 1;
        while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
            j += 1;
        }
        let tag = src[i + 1..j].to_string();
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= b.len() {
                break;
            }
            if b[j] == b'>' {
                j += 1;
                break;
            }
            if b[j] == b'/' {
                self_closing = true;
                j += 1;
                continue;
            }
            let start = j;
            while j < b.len() && !b[j].is_ascii_whitespace() && !matches!(b[j], b'=' | b'>' | b'/')
            {
                j += 1;
            }
            let name = src[start..j].to_string();
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut value = String::new();
            if j < b.len() && b[j] == b'=' {
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    let vs = j + 1;
                    let ve = b[vs..]
                        .iter()
                        .position(|&c| c == q)
                        .map_or(b.len(), |p| vs + p);
                    value = decode_entities(&src[vs..ve]);
                    j = (ve + 1).min(b.len());
                } else {
                    let vs = j;
                    while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                        j += 1;
                    }
                    value = src[vs..j].to_string();
                }
            }
            if name.is_empty() {
                j += 1;
                continue;
            }
            attrs.push((name, value));
        }
        i = j;
        // Namespaced names are matched without their prefix (`svg:path`, `xlink:href`).
        let tag = tag.rsplit(':').next().unwrap_or("").to_string();
        let id = nodes.len();
        nodes.push(Node {
            tag,
            attrs,
            children: Vec::new(),
            text: String::new(),
        });
        if let Some(&parent) = stack.last() {
            nodes[parent].children.push(id);
        }
        if !self_closing {
            stack.push(id);
        }
    }
    Doc { nodes }
}

// ---------------------------------------------------------------------------------------
// Geometry.

#[derive(Clone, Copy, Debug, PartialEq)]
struct Transform([f32; 6]);

impl Transform {
    const IDENTITY: Transform = Transform([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn then(self, o: Transform) -> Transform {
        // `self` applied first, then `o`... expressed as o * self.
        let [a, b, c, d, e, f] = self.0;
        let [oa, ob, oc, od, oe, of] = o.0;
        Transform([
            oa * a + oc * b,
            ob * a + od * b,
            oa * c + oc * d,
            ob * c + od * d,
            oa * e + oc * f + oe,
            ob * e + od * f + of,
        ])
    }

    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// How much lengths grow on average, for stroke widths.
    fn scale(&self) -> f32 {
        let [a, b, c, d, ..] = self.0;
        (a * d - b * c).abs().sqrt()
    }
}

fn numbers(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut p = Numbers::new(s);
    while let Some(v) = p.next() {
        out.push(v);
    }
    out
}

fn parse_transform(s: &str) -> Transform {
    let mut t = Transform::IDENTITY;
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim_matches(|c: char| c.is_whitespace() || c == ',');
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        let v = numbers(&rest[open + 1..open + close]);
        let g = |i: usize| v.get(i).copied().unwrap_or(0.0);
        let m = match (name, v.len()) {
            ("matrix", 6) => Transform([g(0), g(1), g(2), g(3), g(4), g(5)]),
            ("translate", 1 | 2) => Transform([1.0, 0.0, 0.0, 1.0, g(0), g(1)]),
            ("scale", 1) => Transform([g(0), 0.0, 0.0, g(0), 0.0, 0.0]),
            ("scale", 2) => Transform([g(0), 0.0, 0.0, g(1), 0.0, 0.0]),
            ("rotate", 1 | 3) => {
                let (s, c) = g(0).to_radians().sin_cos();
                let r = Transform([c, s, -s, c, 0.0, 0.0]);
                if v.len() == 3 {
                    Transform([1.0, 0.0, 0.0, 1.0, -g(1), -g(2)])
                        .then(r)
                        .then(Transform([1.0, 0.0, 0.0, 1.0, g(1), g(2)]))
                } else {
                    r
                }
            }
            ("skewX", 1) => Transform([1.0, 0.0, g(0).to_radians().tan(), 1.0, 0.0, 0.0]),
            ("skewY", 1) => Transform([1.0, g(0).to_radians().tan(), 0.0, 1.0, 0.0, 0.0]),
            _ => return Transform::IDENTITY,
        };
        // Listed transforms apply right to left: the last one touches the shape first.
        t = m.then(t);
        rest = &rest[open + close + 1..];
    }
    t
}

/// Reads numbers in SVG syntax: `1.5e3`, `-2`, `.5.5` (two numbers), separators optional.
struct Numbers<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Numbers<'a> {
    fn new(s: &'a str) -> Numbers<'a> {
        Numbers {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn skip_separators(&mut self) {
        while self.i < self.b.len()
            && (self.b[self.i].is_ascii_whitespace() || self.b[self.i] == b',')
        {
            self.i += 1;
        }
    }

    fn next(&mut self) -> Option<f32> {
        self.skip_separators();
        let b = self.b;
        let start = self.i;
        let mut j = self.i;
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            j += 1;
        }
        let mut digits = false;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            digits = true;
        }
        if j < b.len() && b[j] == b'.' {
            j += 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
                digits = true;
            }
        }
        if !digits {
            return None;
        }
        if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
            let mut k = j + 1;
            if k < b.len() && (b[k] == b'-' || b[k] == b'+') {
                k += 1;
            }
            if k < b.len() && b[k].is_ascii_digit() {
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
                j = k;
            }
        }
        self.i = j;
        std::str::from_utf8(&b[start..j]).ok()?.parse().ok()
    }

    /// An arc flag: a single `0` or `1`, which may be written without separators.
    fn flag(&mut self) -> Option<bool> {
        self.skip_separators();
        let c = *self.b.get(self.i)?;
        if c == b'0' || c == b'1' {
            self.i += 1;
            Some(c == b'1')
        } else {
            None
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip_separators();
        let c = *self.b.get(self.i)?;
        if c.is_ascii_alphabetic() && c != b'e' && c != b'E' {
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_separators();
        self.i >= self.b.len()
    }
}

/// A path in user space, already flattened to polylines.
#[derive(Default)]
struct Path {
    contours: Vec<(Vec<(f32, f32)>, bool)>,
}

impl Path {
    fn move_to(&mut self, p: (f32, f32)) {
        self.contours.push((vec![p], false));
    }

    fn line_to(&mut self, p: (f32, f32)) {
        match self.contours.last_mut() {
            Some((pts, _)) => pts.push(p),
            None => self.contours.push((vec![p], false)),
        }
    }

    fn close(&mut self) {
        if let Some((pts, closed)) = self.contours.last_mut() {
            *closed = true;
            let start = pts[0];
            self.contours.push((vec![start], false));
        }
    }

    fn current(&self) -> (f32, f32) {
        self.contours
            .last()
            .and_then(|(p, _)| p.last().copied())
            .unwrap_or((0.0, 0.0))
    }

    fn cubic(&mut self, p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), tolerance: f32) {
        let p0 = self.current();
        let len = dist(p0, p1) + dist(p1, p2) + dist(p2, p3);
        let n = ((len / tolerance).sqrt().ceil() as usize).clamp(1, 256);
        for k in 1..=n {
            let t = k as f32 / n as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            self.line_to((
                a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
                a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
            ));
        }
    }

    fn quad(&mut self, p1: (f32, f32), p2: (f32, f32), tolerance: f32) {
        let p0 = self.current();
        let c1 = (
            p0.0 + 2.0 / 3.0 * (p1.0 - p0.0),
            p0.1 + 2.0 / 3.0 * (p1.1 - p0.1),
        );
        let c2 = (
            p2.0 + 2.0 / 3.0 * (p1.0 - p2.0),
            p2.1 + 2.0 / 3.0 * (p1.1 - p2.1),
        );
        self.cubic(c1, c2, p2, tolerance);
    }

    #[allow(clippy::too_many_arguments)]
    fn arc(
        &mut self,
        rx: f32,
        ry: f32,
        angle: f32,
        large: bool,
        sweep: bool,
        to: (f32, f32),
        tolerance: f32,
    ) {
        let from = self.current();
        let (mut rx, mut ry) = (rx.abs(), ry.abs());
        if rx == 0.0 || ry == 0.0 || from == to {
            self.line_to(to);
            return;
        }
        // Endpoint to center parameterization (SVG 1.1, appendix F.6).
        let (s, c) = angle.to_radians().sin_cos();
        let dx = (from.0 - to.0) / 2.0;
        let dy = (from.1 - to.1) / 2.0;
        let x1 = c * dx + s * dy;
        let y1 = -s * dx + c * dy;
        let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
        if lambda > 1.0 {
            rx *= lambda.sqrt();
            ry *= lambda.sqrt();
        }
        let num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
        let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
        let mut k = (num / den).max(0.0).sqrt();
        if large == sweep {
            k = -k;
        }
        let cx1 = k * rx * y1 / ry;
        let cy1 = -k * ry * x1 / rx;
        let cx = c * cx1 - s * cy1 + (from.0 + to.0) / 2.0;
        let cy = s * cx1 + c * cy1 + (from.1 + to.1) / 2.0;
        let angle_of = |ux: f32, uy: f32| uy.atan2(ux);
        let t1 = angle_of((x1 - cx1) / rx, (y1 - cy1) / ry);
        let t2 = angle_of((-x1 - cx1) / rx, (-y1 - cy1) / ry);
        let mut dt = t2 - t1;
        let tau = std::f32::consts::TAU;
        if sweep && dt < 0.0 {
            dt += tau;
        } else if !sweep && dt > 0.0 {
            dt -= tau;
        }
        let r = rx.max(ry);
        let n = ((dt.abs() * (r / tolerance).sqrt()).ceil() as usize).clamp(1, 512);
        for i in 1..=n {
            let t = t1 + dt * i as f32 / n as f32;
            let (st, ct) = t.sin_cos();
            let (x, y) = (rx * ct, ry * st);
            self.line_to((c * x - s * y + cx, s * x + c * y + cy));
        }
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn parse_path(d: &str, tolerance: f32) -> Path {
    let mut path = Path::default();
    let mut n = Numbers::new(d);
    let mut cmd = 0u8;
    let mut last_ctrl: Option<(f32, f32)> = None;
    loop {
        if let Some(c) = n.command() {
            cmd = c;
        } else if n.at_end() || cmd == 0 {
            break;
        }
        let rel = cmd.is_ascii_lowercase();
        let cur = path.current();
        let off = |p: (f32, f32)| if rel { (p.0 + cur.0, p.1 + cur.1) } else { p };
        let prev_cmd = cmd.to_ascii_uppercase();
        let ok = match prev_cmd {
            b'M' => n.pair().map(|p| {
                let p = off(p);
                path.move_to(p);
                // Further pairs after a move are lines.
                cmd = if rel { b'l' } else { b'L' };
                last_ctrl = None;
            }),
            b'L' => n.pair().map(|p| {
                path.line_to(off(p));
                last_ctrl = None;
            }),
            b'H' => n.next().map(|x| {
                path.line_to((if rel { cur.0 + x } else { x }, cur.1));
                last_ctrl = None;
            }),
            b'V' => n.next().map(|y| {
                path.line_to((cur.0, if rel { cur.1 + y } else { y }));
                last_ctrl = None;
            }),
            b'C' => (|| {
                let (a, b, c) = (n.pair()?, n.pair()?, n.pair()?);
                let (a, b, c) = (off(a), off(b), off(c));
                path.cubic(a, b, c, tolerance);
                last_ctrl = Some(b);
                Some(())
            })(),
            b'S' => (|| {
                let (b, c) = (n.pair()?, n.pair()?);
                let a = reflect(last_ctrl, cur);
                let (b, c) = (off(b), off(c));
                path.cubic(a, b, c, tolerance);
                last_ctrl = Some(b);
                Some(())
            })(),
            b'Q' => (|| {
                let (a, b) = (n.pair()?, n.pair()?);
                let (a, b) = (off(a), off(b));
                path.quad(a, b, tolerance);
                last_ctrl = Some(a);
                Some(())
            })(),
            b'T' => n.pair().map(|b| {
                let a = reflect(last_ctrl, cur);
                path.quad(a, off(b), tolerance);
                last_ctrl = Some(a);
            }),
            b'A' => (|| {
                let rx = n.next()?;
                let ry = n.next()?;
                let angle = n.next()?;
                let large = n.flag()?;
                let sweep = n.flag()?;
                let to = off(n.pair()?);
                path.arc(rx, ry, angle, large, sweep, to, tolerance);
                last_ctrl = None;
                Some(())
            })(),
            b'Z' => {
                path.close();
                last_ctrl = None;
                // Z takes no arguments: anything after it needs a new command.
                if n.command_peek_is_number() {
                    break;
                }
                cmd = 0;
                Some(())
            }
            _ => None,
        };
        if ok.is_none() {
            break;
        }
        // Reflected control points only follow curves of the same family.
        if !matches!(prev_cmd, b'C' | b'S' | b'Q' | b'T') {
            last_ctrl = None;
        }
    }
    path
}

impl Numbers<'_> {
    fn pair(&mut self) -> Option<(f32, f32)> {
        Some((self.next()?, self.next()?))
    }

    fn command_peek_is_number(&mut self) -> bool {
        self.skip_separators();
        self.b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }
}

fn reflect(ctrl: Option<(f32, f32)>, cur: (f32, f32)) -> (f32, f32) {
    match ctrl {
        Some(c) => (2.0 * cur.0 - c.0, 2.0 * cur.1 - c.1),
        None => cur,
    }
}

// ---------------------------------------------------------------------------------------
// Rasterizing.

struct Edge {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    dir: i32,
}

/// Anti-aliased polygon filling, four sub-scanlines per row with exact horizontal coverage.
fn fill(
    canvas: &mut [u8],
    width: usize,
    height: usize,
    polys: &[Vec<(f32, f32)>],
    even_odd: bool,
    color: [f32; 4],
) {
    let mut edges = Vec::new();
    for poly in polys {
        for k in 0..poly.len() {
            let a = poly[k];
            let b = poly[(k + 1) % poly.len()];
            if a.1 == b.1
                || !(a.0.is_finite() && a.1.is_finite() && b.0.is_finite() && b.1.is_finite())
            {
                continue;
            }
            let (dir, a, b) = if a.1 < b.1 { (1, a, b) } else { (-1, b, a) };
            edges.push(Edge {
                x0: a.0,
                y0: a.1,
                x1: b.0,
                y1: b.1,
                dir,
            });
        }
    }
    if edges.is_empty() || color[3] <= 0.0 {
        return;
    }
    edges.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    let ymin = edges[0].y0.max(0.0).floor() as usize;
    let ymax = edges
        .iter()
        .map(|e| e.y1)
        .fold(0.0f32, f32::max)
        .ceil()
        .min(height as f32) as usize;
    let mut cover = vec![0f32; width + 2];
    let mut active: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut crossings: Vec<(f32, i32)> = Vec::new();
    const SUB: usize = 4;
    for y in ymin..ymax {
        let mut touched = (usize::MAX, 0usize);
        for s in 0..SUB {
            let sy = y as f32 + (s as f32 + 0.5) / SUB as f32;
            while next < edges.len() && edges[next].y0 <= sy {
                active.push(next);
                next += 1;
            }
            active.retain(|&e| edges[e].y1 > sy);
            crossings.clear();
            for &e in &active {
                let e = &edges[e];
                if e.y0 <= sy && sy < e.y1 {
                    let t = (sy - e.y0) / (e.y1 - e.y0);
                    crossings.push((e.x0 + t * (e.x1 - e.x0), e.dir));
                }
            }
            crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut wind = 0;
            for k in 0..crossings.len() {
                let before = wind;
                wind += crossings[k].1;
                let inside = |w: i32| if even_odd { w & 1 != 0 } else { w != 0 };
                if !inside(before) && inside(wind) {
                    // Find where this span ends.
                    let mut w = wind;
                    let mut end = crossings[k].0;
                    for c in &crossings[k + 1..] {
                        w += c.1;
                        end = c.0;
                        if !inside(w) {
                            break;
                        }
                    }
                    let start = crossings[k].0;
                    span(
                        &mut cover,
                        width,
                        start,
                        end,
                        1.0 / SUB as f32,
                        &mut touched,
                    );
                }
            }
        }
        if touched.0 == usize::MAX {
            continue;
        }
        let row = y * width * 4;
        for x in touched.0..=touched.1.min(width - 1) {
            let c = cover[x].min(1.0);
            cover[x] = 0.0;
            if c <= 0.0 {
                continue;
            }
            let a = color[3] * c;
            let px = &mut canvas[row + x * 4..row + x * 4 + 4];
            for ch in 0..3 {
                let v = color[ch] * a * 255.0 + f32::from(px[ch]) * (1.0 - a);
                px[ch] = v.round().clamp(0.0, 255.0) as u8;
            }
            let v = a * 255.0 + f32::from(px[3]) * (1.0 - a);
            px[3] = v.round().clamp(0.0, 255.0) as u8;
        }
        cover[width] = 0.0;
        cover[width + 1] = 0.0;
    }
}

/// Adds coverage `weight` for the span [x0, x1) of one sub-scanline.
fn span(
    cover: &mut [f32],
    width: usize,
    x0: f32,
    x1: f32,
    weight: f32,
    touched: &mut (usize, usize),
) {
    let x0 = x0.clamp(0.0, width as f32);
    let x1 = x1.clamp(0.0, width as f32);
    if x1 <= x0 {
        return;
    }
    let (i0, i1) = (x0.floor() as usize, x1.floor() as usize);
    touched.0 = touched.0.min(i0);
    touched.1 = touched.1.max(i1.min(width - 1));
    if i0 == i1 {
        cover[i0] += (x1 - x0) * weight;
        return;
    }
    cover[i0] += (i0 as f32 + 1.0 - x0) * weight;
    for c in &mut cover[i0 + 1..i1] {
        *c += weight;
    }
    if i1 < width {
        cover[i1] += (x1 - i1 as f32) * weight;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Cap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, PartialEq)]
enum Join {
    Miter,
    Round,
    Bevel,
}

fn circle(c: (f32, f32), r: f32) -> Vec<(f32, f32)> {
    let n = ((r * 2.0).sqrt() * 4.0).ceil().clamp(8.0, 64.0) as usize;
    (0..n)
        .map(|i| {
            let (s, co) = (i as f32 / n as f32 * std::f32::consts::TAU).sin_cos();
            (c.0 + r * co, c.1 + r * s)
        })
        .collect()
}

/// The outline of a stroked polyline, as polygons to fill with the nonzero rule.
fn stroke_polys(
    pts: &[(f32, f32)],
    closed: bool,
    w: f32,
    cap: Cap,
    join: Join,
    miter_limit: f32,
) -> Vec<Vec<(f32, f32)>> {
    let mut pts: Vec<(f32, f32)> = pts.to_vec();
    pts.dedup_by(|a, b| dist(*a, *b) < 1e-4);
    if closed && pts.len() > 1 && dist(pts[0], pts[pts.len() - 1]) < 1e-4 {
        pts.pop();
    }
    let h = w / 2.0;
    let mut out = Vec::new();
    if pts.len() == 1 {
        match cap {
            Cap::Round => out.push(circle(pts[0], h)),
            Cap::Square => {
                let p = pts[0];
                out.push(vec![
                    (p.0 - h, p.1 - h),
                    (p.0 + h, p.1 - h),
                    (p.0 + h, p.1 + h),
                    (p.0 - h, p.1 + h),
                ]);
            }
            Cap::Butt => {}
        }
        return out;
    }
    let n = pts.len();
    let segs = if closed { n } else { n - 1 };
    let normal = |a: (f32, f32), b: (f32, f32)| {
        let l = dist(a, b).max(1e-6);
        ((a.1 - b.1) / l * h, (b.0 - a.0) / l * h)
    };
    for s in 0..segs {
        let (mut a, mut b) = (pts[s], pts[(s + 1) % n]);
        let (nx, ny) = normal(a, b);
        if !closed && cap == Cap::Square {
            let l = dist(a, b).max(1e-6);
            let (dx, dy) = ((b.0 - a.0) / l * h, (b.1 - a.1) / l * h);
            if s == 0 {
                a = (a.0 - dx, a.1 - dy);
            }
            if s == segs - 1 {
                b = (b.0 + dx, b.1 + dy);
            }
        }
        // Wound consistently so overlapping pieces add up under the nonzero rule.
        let quad = vec![
            (a.0 + nx, a.1 + ny),
            (b.0 + nx, b.1 + ny),
            (b.0 - nx, b.1 - ny),
            (a.0 - nx, a.1 - ny),
        ];
        out.push(orient(quad));
    }
    // Joins at interior vertices (every vertex when closed).
    let joins: Vec<usize> = if closed {
        (0..n).collect()
    } else {
        (1..n - 1).collect()
    };
    for v in joins {
        let p = pts[v];
        let prev = pts[(v + n - 1) % n];
        let next = pts[(v + 1) % n];
        match join {
            Join::Round => out.push(circle(p, h)),
            Join::Bevel | Join::Miter => {
                let n1 = normal(prev, p);
                let n2 = normal(p, next);
                let cross = (p.0 - prev.0) * (next.1 - p.1) - (p.1 - prev.1) * (next.0 - p.0);
                // The outer side of the turn.
                let sgn = if cross > 0.0 { -1.0 } else { 1.0 };
                let a = (p.0 + sgn * n1.0, p.1 + sgn * n1.1);
                let b = (p.0 + sgn * n2.0, p.1 + sgn * n2.1);
                out.push(orient(vec![p, a, b]));
                if join == Join::Miter {
                    let (mx, my) = (n1.0 + n2.0, n1.1 + n2.1);
                    let ml = (mx * mx + my * my).sqrt();
                    if ml > 1e-6 {
                        // Distance from the vertex to the miter tip, over the half width.
                        let cos_half = ml / (2.0 * h);
                        let ratio = 1.0 / cos_half.max(1e-6);
                        if ratio <= miter_limit {
                            let len = h / cos_half;
                            let tip = (p.0 + sgn * mx / ml * len, p.1 + sgn * my / ml * len);
                            out.push(orient(vec![p, a, tip, b]));
                        }
                    }
                }
            }
        }
    }
    if !closed && cap == Cap::Round {
        out.push(circle(pts[0], h));
        out.push(circle(pts[n - 1], h));
    }
    out
}

/// Makes a polygon wind counter-clockwise (positive area).
fn orient(mut p: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    let mut area = 0.0;
    for k in 0..p.len() {
        let (a, b) = (p[k], p[(k + 1) % p.len()]);
        area += a.0 * b.1 - b.0 * a.1;
    }
    if area < 0.0 {
        p.reverse();
    }
    p
}

// ---------------------------------------------------------------------------------------
// Styles and drawing.

#[derive(Clone, Copy, PartialEq)]
enum Paint {
    None,
    Color([f32; 4]),
}

#[derive(Clone)]
struct Style {
    fill: Paint,
    stroke: Paint,
    stroke_width: f32,
    fill_opacity: f32,
    stroke_opacity: f32,
    opacity: f32,
    even_odd: bool,
    cap: Cap,
    join: Join,
    miter_limit: f32,
    color: [f32; 4],
    hidden: bool,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            fill: Paint::Color([0.0, 0.0, 0.0, 1.0]),
            stroke: Paint::None,
            stroke_width: 1.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
            opacity: 1.0,
            even_odd: false,
            cap: Cap::Butt,
            join: Join::Miter,
            miter_limit: 4.0,
            color: [0.0, 0.0, 0.0, 1.0],
            hidden: false,
        }
    }
}

struct Rule {
    tag: Option<String>,
    class: Option<String>,
    id: Option<String>,
    decls: Vec<(String, String)>,
}

fn parse_decls(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let v = v.trim().trim_end_matches("!important").trim();
            Some((k.trim().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

/// Rules of `<style>` elements with simple selectors; anything else is dropped.
fn parse_rules(css: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut rest = css;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let selectors = &rest[..open];
        let decls = parse_decls(&rest[open + 1..open + close]);
        for sel in selectors.split(',') {
            let sel = sel.trim();
            if sel.is_empty() || sel.contains(|c: char| c.is_whitespace() || "[]:>+~*@".contains(c))
            {
                continue;
            }
            let (tag_part, rest_sel) = match sel.find(['.', '#']) {
                Some(i) => (&sel[..i], &sel[i..]),
                None => (sel, ""),
            };
            let mut rule = Rule {
                tag: (!tag_part.is_empty()).then(|| tag_part.to_string()),
                class: None,
                id: None,
                decls: decls.clone(),
            };
            if let Some(c) = rest_sel.strip_prefix('.') {
                if c.contains(['.', '#']) {
                    continue;
                }
                rule.class = Some(c.to_string());
            } else if let Some(i) = rest_sel.strip_prefix('#') {
                if i.contains(['.', '#']) {
                    continue;
                }
                rule.id = Some(i.to_string());
            }
            rules.push(rule);
        }
        rest = &rest[open + close + 1..];
    }
    rules
}

struct Renderer<'a> {
    doc: &'a Doc,
    rules: Vec<Rule>,
    canvas: Vec<u8>,
    width: usize,
    height: usize,
}

fn length(v: &str, reference: f32) -> Option<f32> {
    let v = v.trim();
    let (num, unit) = match v.find(|c: char| c.is_ascii_alphabetic() || c == '%') {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let n: f32 = num.trim().parse().ok()?;
    Some(match unit {
        "" | "px" => n,
        "%" => n / 100.0 * reference,
        "em" => n * 16.0,
        "pt" => n * 4.0 / 3.0,
        "pc" => n * 16.0,
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        _ => return None,
    })
}

fn view_box(doc: &Doc, n: usize) -> Option<[f32; 4]> {
    let v = numbers(doc.attr(n, "viewBox")?);
    (v.len() == 4 && v[2] > 0.0 && v[3] > 0.0).then(|| [v[0], v[1], v[2], v[3]])
}

/// The viewBox-to-viewport transform, honouring `preserveAspectRatio`.
fn viewport_transform(doc: &Doc, n: usize, vw: f32, vh: f32) -> Transform {
    let Some([x, y, w, h]) = view_box(doc, n) else {
        return Transform::IDENTITY;
    };
    let par = doc
        .attr(n, "preserveAspectRatio")
        .unwrap_or("xMidYMid meet");
    let mut words = par.split_whitespace();
    let align = words.next().unwrap_or("xMidYMid");
    let slice = words.next() == Some("slice");
    let (sx, sy) = (vw / w, vh / h);
    if align == "none" {
        return Transform([sx, 0.0, 0.0, sy, -x * sx, -y * sy]);
    }
    let s = if slice { sx.max(sy) } else { sx.min(sy) };
    let fx = if align.contains("xMid") {
        0.5
    } else if align.contains("xMax") {
        1.0
    } else {
        0.0
    };
    let fy = if align.contains("YMid") {
        0.5
    } else if align.contains("YMax") {
        1.0
    } else {
        0.0
    };
    let tx = (vw - w * s) * fx - x * s;
    let ty = (vh - h * s) * fy - y * s;
    Transform([s, 0.0, 0.0, s, tx, ty])
}

impl Renderer<'_> {
    fn color(&self, v: &str, current: [f32; 4]) -> Option<Paint> {
        let v = v.trim();
        if v == "none" || v == "transparent" {
            return Some(Paint::None);
        }
        if v == "currentColor" {
            return Some(Paint::Color(current));
        }
        if let Some(rest) = v.strip_prefix("url(") {
            let id = rest
                .split(')')
                .next()?
                .trim()
                .trim_matches(['"', '\''])
                .trim_start_matches('#');
            let after = rest.split_once(')').map(|x| x.1.trim()).unwrap_or("");
            return match self
                .doc
                .find_id(id)
                .and_then(|g| self.gradient_average(g, 0))
            {
                Some(c) => Some(Paint::Color(c)),
                None if !after.is_empty() => self.color(after, current),
                None => Some(Paint::None),
            };
        }
        let c = css::parse_color(&v.to_ascii_lowercase())?;
        Some(Paint::Color([
            f32::from(c.r) / 255.0,
            f32::from(c.g) / 255.0,
            f32::from(c.b) / 255.0,
            f32::from(c.a) / 255.0,
        ]))
    }

    /// A gradient drawn as one color: the average of its stops (following `href`).
    fn gradient_average(&self, g: usize, depth: u32) -> Option<[f32; 4]> {
        if depth > MAX_DEPTH || !self.doc.nodes[g].tag.ends_with("Gradient") {
            return None;
        }
        let mut sum = [0f32; 4];
        let mut count = 0.0;
        for &c in &self.doc.nodes[g].children {
            if self.doc.nodes[c].tag != "stop" {
                continue;
            }
            let decls = self
                .doc
                .attr(c, "style")
                .map(parse_decls)
                .unwrap_or_default();
            let get = |name: &str| {
                decls
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
                    .or_else(|| self.doc.attr(c, name))
            };
            let Some(Paint::Color(col)) = get("stop-color")
                .and_then(|v| self.color(v, [0.0, 0.0, 0.0, 1.0]))
                .or(Some(Paint::Color([0.0, 0.0, 0.0, 1.0])))
            else {
                continue;
            };
            let op = get("stop-opacity")
                .and_then(|v| v.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            let a = col[3] * op.clamp(0.0, 1.0);
            for k in 0..3 {
                sum[k] += col[k] * a;
            }
            sum[3] += a;
            count += 1.0;
        }
        if count == 0.0 {
            let href = self
                .doc
                .attr(g, "href")
                .or_else(|| self.doc.attr(g, "xlink:href"))?;
            let target = self.doc.find_id(href.trim_start_matches('#'))?;
            return self.gradient_average(target, depth + 1);
        }
        if sum[3] <= 0.0 {
            return Some([0.0; 4]);
        }
        Some([
            sum[0] / sum[3],
            sum[1] / sum[3],
            sum[2] / sum[3],
            sum[3] / count,
        ])
    }

    fn apply(&self, style: &mut Style, name: &str, value: &str) {
        let num = |v: &str| v.trim().parse::<f32>().ok();
        let opacity = |v: &str| {
            let v = v.trim();
            match v.strip_suffix('%') {
                Some(p) => p.parse::<f32>().ok().map(|p| p / 100.0),
                None => v.parse::<f32>().ok(),
            }
            .map(|o| o.clamp(0.0, 1.0))
        };
        match name {
            "fill" => {
                if let Some(p) = self.color(value, style.color) {
                    style.fill = p;
                }
            }
            "stroke" => {
                if let Some(p) = self.color(value, style.color) {
                    style.stroke = p;
                }
            }
            "color" => {
                if let Some(Paint::Color(c)) = self.color(value, style.color) {
                    style.color = c;
                }
            }
            "stroke-width" => {
                if let Some(w) = length(value, 100.0) {
                    style.stroke_width = w.max(0.0);
                }
            }
            "fill-opacity" => style.fill_opacity = opacity(value).unwrap_or(style.fill_opacity),
            "stroke-opacity" => {
                style.stroke_opacity = opacity(value).unwrap_or(style.stroke_opacity)
            }
            "opacity" => style.opacity *= opacity(value).unwrap_or(1.0),
            "fill-rule" => style.even_odd = value.trim() == "evenodd",
            "stroke-linecap" => {
                style.cap = match value.trim() {
                    "round" => Cap::Round,
                    "square" => Cap::Square,
                    _ => Cap::Butt,
                }
            }
            "stroke-linejoin" => {
                style.join = match value.trim() {
                    "round" => Join::Round,
                    "bevel" => Join::Bevel,
                    _ => Join::Miter,
                }
            }
            "stroke-miterlimit" => style.miter_limit = num(value).unwrap_or(4.0).max(1.0),
            "display" if value.trim() == "none" => style.hidden = true,
            "visibility" => style.hidden = matches!(value.trim(), "hidden" | "collapse"),
            _ => {}
        }
    }

    /// The style of element `n` inside `parent`: attributes, then `<style>` rules, then
    /// the `style` attribute. `opacity` does not inherit; it multiplies down instead.
    fn style(&self, n: usize, parent: &Style) -> Style {
        let mut s = parent.clone();
        s.hidden = false;
        let node = &self.doc.nodes[n];
        // `color` first, so currentColor in the same element sees it.
        for (k, v) in &node.attrs {
            if k == "color" {
                self.apply(&mut s, k, v);
            }
        }
        for (k, v) in &node.attrs {
            if k != "color" && k != "style" {
                self.apply(&mut s, k, v);
            }
        }
        let classes: Vec<&str> = self
            .doc
            .attr(n, "class")
            .map(|c| c.split_whitespace().collect())
            .unwrap_or_default();
        let id = self.doc.attr(n, "id");
        for r in &self.rules {
            let tag_ok = r.tag.as_deref().is_none_or(|t| t == node.tag);
            let class_ok = r.class.as_deref().is_none_or(|c| classes.contains(&c));
            let id_ok = r.id.as_deref().is_none_or(|i| Some(i) == id);
            if tag_ok && class_ok && id_ok {
                for (k, v) in &r.decls {
                    self.apply(&mut s, k, v);
                }
            }
        }
        if let Some(inline) = self.doc.attr(n, "style") {
            for (k, v) in parse_decls(inline) {
                self.apply(&mut s, &k, &v);
            }
        }
        s
    }

    fn render(&mut self, n: usize, parent: &Style, ctm: Transform, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        let style = self.style(n, parent);
        if style.hidden {
            return;
        }
        let mut ctm = match self.doc.attr(n, "transform") {
            Some(t) => parse_transform(t).then(ctm),
            None => ctm,
        };
        let tag = self.doc.nodes[n].tag.as_str();
        // Precision of curve flattening in user units: a quarter pixel on the output.
        let tolerance = 0.25 / ctm.scale().max(1e-6);
        let num = |name: &str| {
            self.doc
                .attr(n, name)
                .and_then(|v| length(v, 100.0))
                .unwrap_or(0.0)
        };
        let path = match tag {
            "svg" | "g" | "a" | "switch" => {
                if tag == "svg" && depth > 0 {
                    let (x, y) = (num("x"), num("y"));
                    let w = self
                        .doc
                        .attr(n, "width")
                        .and_then(|v| length(v, 100.0))
                        .unwrap_or(100.0);
                    let h = self
                        .doc
                        .attr(n, "height")
                        .and_then(|v| length(v, 100.0))
                        .unwrap_or(100.0);
                    ctm = viewport_transform(self.doc, n, w, h)
                        .then(Transform([1.0, 0.0, 0.0, 1.0, x, y]))
                        .then(ctm);
                }
                let children = self.doc.nodes[n].children.clone();
                for c in children {
                    self.render(c, &style, ctm, depth + 1);
                }
                return;
            }
            "use" => {
                let href = self
                    .doc
                    .attr(n, "href")
                    .or_else(|| self.doc.attr(n, "xlink:href"));
                let Some(target) = href.and_then(|h| self.doc.find_id(h.trim_start_matches('#')))
                else {
                    return;
                };
                let t = Transform([1.0, 0.0, 0.0, 1.0, num("x"), num("y")]).then(ctm);
                if self.doc.nodes[target].tag == "symbol" {
                    let w = self.doc.attr(n, "width").and_then(|v| length(v, 100.0));
                    let h = self.doc.attr(n, "height").and_then(|v| length(v, 100.0));
                    let vb = view_box(self.doc, target);
                    let (w, h) = (
                        w.or(vb.map(|v| v[2])).unwrap_or(100.0),
                        h.or(vb.map(|v| v[3])).unwrap_or(100.0),
                    );
                    let t = viewport_transform(self.doc, target, w, h).then(t);
                    let sym_style = self.style(target, &style);
                    let children = self.doc.nodes[target].children.clone();
                    for c in children {
                        self.render(c, &sym_style, t, depth + 1);
                    }
                } else {
                    self.render(target, &style, t, depth + 1);
                }
                return;
            }
            "path" => parse_path(self.doc.attr(n, "d").unwrap_or(""), tolerance),
            "rect" => {
                let (x, y, w, h) = (num("x"), num("y"), num("width"), num("height"));
                if w <= 0.0 || h <= 0.0 {
                    return;
                }
                let rx_attr = self.doc.attr(n, "rx").and_then(|v| length(v, w));
                let ry_attr = self.doc.attr(n, "ry").and_then(|v| length(v, h));
                let rx = rx_attr.or(ry_attr).unwrap_or(0.0).clamp(0.0, w / 2.0);
                let ry = ry_attr.or(rx_attr).unwrap_or(0.0).clamp(0.0, h / 2.0);
                let mut p = Path::default();
                if rx > 0.0 && ry > 0.0 {
                    p.move_to((x + rx, y));
                    p.line_to((x + w - rx, y));
                    p.arc(rx, ry, 0.0, false, true, (x + w, y + ry), tolerance);
                    p.line_to((x + w, y + h - ry));
                    p.arc(rx, ry, 0.0, false, true, (x + w - rx, y + h), tolerance);
                    p.line_to((x + rx, y + h));
                    p.arc(rx, ry, 0.0, false, true, (x, y + h - ry), tolerance);
                    p.line_to((x, y + ry));
                    p.arc(rx, ry, 0.0, false, true, (x + rx, y), tolerance);
                } else {
                    p.move_to((x, y));
                    p.line_to((x + w, y));
                    p.line_to((x + w, y + h));
                    p.line_to((x, y + h));
                }
                p.close();
                p
            }
            "circle" | "ellipse" => {
                let (cx, cy) = (num("cx"), num("cy"));
                let (rx, ry) = if tag == "circle" {
                    (num("r"), num("r"))
                } else {
                    (num("rx"), num("ry"))
                };
                if rx <= 0.0 || ry <= 0.0 {
                    return;
                }
                let mut p = Path::default();
                p.move_to((cx + rx, cy));
                p.arc(rx, ry, 0.0, false, true, (cx - rx, cy), tolerance);
                p.arc(rx, ry, 0.0, false, true, (cx + rx, cy), tolerance);
                p.close();
                p
            }
            "line" => {
                let mut p = Path::default();
                p.move_to((num("x1"), num("y1")));
                p.line_to((num("x2"), num("y2")));
                p
            }
            "polyline" | "polygon" => {
                let v = numbers(self.doc.attr(n, "points").unwrap_or(""));
                let mut p = Path::default();
                for (k, xy) in v.as_chunks::<2>().0.iter().enumerate() {
                    if k == 0 {
                        p.move_to((xy[0], xy[1]));
                    } else {
                        p.line_to((xy[0], xy[1]));
                    }
                }
                if tag == "polygon" {
                    p.close();
                }
                p
            }
            _ => return,
        };
        self.draw(&path, &style, ctm);
    }

    fn draw(&mut self, path: &Path, style: &Style, ctm: Transform) {
        let device: Vec<(Vec<(f32, f32)>, bool)> = path
            .contours
            .iter()
            .filter(|(p, _)| !p.is_empty())
            .map(|(p, closed)| (p.iter().map(|&(x, y)| ctm.apply(x, y)).collect(), *closed))
            .collect();
        if let Paint::Color(c) = style.fill {
            let polys: Vec<Vec<(f32, f32)>> = device
                .iter()
                .filter(|(p, _)| p.len() > 2)
                .map(|(p, _)| p.clone())
                .collect();
            let a = c[3] * style.fill_opacity * style.opacity;
            fill(
                &mut self.canvas,
                self.width,
                self.height,
                &polys,
                style.even_odd,
                [c[0], c[1], c[2], a],
            );
        }
        if let Paint::Color(c) = style.stroke {
            let w = style.stroke_width * ctm.scale();
            if w <= 0.0 {
                return;
            }
            let mut polys = Vec::new();
            for (p, closed) in &device {
                // A lone point from a trailing "M" or a "Z" draws nothing, except caps.
                if p.len() == 1 && (style.cap == Cap::Butt || !*closed && path.contours.len() > 1) {
                    continue;
                }
                polys.extend(stroke_polys(
                    p,
                    *closed,
                    w,
                    style.cap,
                    style.join,
                    style.miter_limit,
                ));
            }
            let a = c[3] * style.stroke_opacity * style.opacity;
            fill(
                &mut self.canvas,
                self.width,
                self.height,
                &polys,
                false,
                [c[0], c[1], c[2], a],
            );
        }
    }
}

// ---------------------------------------------------------------------------------------

fn root(doc: &Doc) -> Option<usize> {
    (1..doc.nodes.len()).find(|&n| doc.nodes[n].tag == "svg")
}

/// The natural size: `width` and `height`, else the viewBox size (or the other side
/// from the viewBox ratio), else 300x150.
fn natural_size(doc: &Doc, r: usize) -> (f32, f32) {
    let w = doc
        .attr(r, "width")
        .filter(|v| !v.trim().ends_with('%'))
        .and_then(|v| length(v, 0.0));
    let h = doc
        .attr(r, "height")
        .filter(|v| !v.trim().ends_with('%'))
        .and_then(|v| length(v, 0.0));
    let vb = view_box(doc, r);
    match (w, h, vb) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(v)) => (w, w * v[3] / v[2]),
        (None, Some(h), Some(v)) => (h * v[2] / v[3], h),
        (None, None, Some(v)) => (v[2], v[3]),
        (Some(w), None, None) => (w, DEFAULT_SIZE.1),
        (None, Some(h), None) => (DEFAULT_SIZE.0, h),
        (None, None, None) => DEFAULT_SIZE,
    }
}

/// Whether `data` is an SVG document: its first element, after any XML declaration,
/// comments and doctype, is `svg` (possibly with a namespace prefix).
pub(crate) fn sniff(data: &[u8]) -> bool {
    let mut d = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    for _ in 0..64 {
        let start = d
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(d.len());
        d = &d[start..];
        let skip_past = |d: &[u8], end: &[u8]| {
            d.windows(end.len())
                .position(|w| w == end)
                .map(|i| i + end.len())
        };
        let next = if d.starts_with(b"<?") {
            skip_past(d, b"?>")
        } else if d.starts_with(b"<!--") {
            skip_past(d, b"-->")
        } else if d.starts_with(b"<!") {
            skip_past(d, b">")
        } else {
            let Some(tag) = d.strip_prefix(b"<") else {
                return false;
            };
            let end = tag
                .iter()
                .position(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
                .unwrap_or(tag.len());
            let name = &tag[..end];
            let local = name.rsplit(|&b| b == b':').next().unwrap_or(name);
            return local == b"svg";
        };
        match next {
            Some(n) => d = &d[n..],
            None => return false,
        }
    }
    false
}

pub(crate) fn probe(data: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(data).ok()?;
    // Only the root start tag is needed for the size.
    let at = text.find("<svg")?;
    let end = text[at..].find('>').map_or(text.len(), |e| at + e + 1);
    let doc = parse_xml(&text[at..end]);
    let r = root(&doc)?;
    let (w, h) = natural_size(&doc, r);
    let (w, h) = (w.round(), h.round());
    (w >= 1.0 && h >= 1.0 && w <= MAX_SIDE as f32 && h <= MAX_SIDE as f32)
        .then_some((w as u32, h as u32))
}

/// Draws the SVG at exactly `target` (vector images scale both ways).
pub(crate) fn decode(data: &[u8], target: (u32, u32)) -> Result<Bitmap, Error> {
    let text = std::str::from_utf8(data).map_err(|_| Error::Corrupt("SVG is not UTF-8"))?;
    let doc = parse_xml(text);
    let r = root(&doc).ok_or(Error::Corrupt("no <svg> element"))?;
    let (width, height) = (
        target.0.clamp(1, MAX_SIDE) as usize,
        target.1.clamp(1, MAX_SIDE) as usize,
    );
    let css: String = doc
        .nodes
        .iter()
        .filter(|n| n.tag == "style")
        .map(|n| n.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut renderer = Renderer {
        doc: &doc,
        rules: parse_rules(&css),
        canvas: vec![0; width * height * 4],
        width,
        height,
    };
    let (nw, nh) = natural_size(&doc, r);
    // Viewport: the natural size, stretched to the target; then the viewBox inside it.
    let ctm = viewport_transform(&doc, r, nw, nh).then(Transform([
        width as f32 / nw,
        0.0,
        0.0,
        height as f32 / nh,
        0.0,
        0.0,
    ]));
    renderer.render(r, &Style::default(), ctm, 0);
    Ok(Bitmap {
        width: width as u32,
        height: height as u32,
        pixels: renderer.canvas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(b: &Bitmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * b.width + x) * 4) as usize;
        b.pixels[i..i + 4].try_into().unwrap()
    }

    fn draw(svg: &str, w: u32, h: u32) -> Bitmap {
        crate::decode(svg.as_bytes(), w, h).unwrap()
    }

    #[test]
    fn sizes_from_attributes_and_view_box() {
        let size = |s: &str| crate::probe(s.as_bytes()).map(|i| (i.width, i.height, i.format));
        assert_eq!(
            size("<svg width='24' height='16'></svg>"),
            Some((24, 16, crate::Format::Svg))
        );
        assert_eq!(
            size("<?xml version='1.0'?>\n<svg viewBox='0 0 48 32'/>").map(|s| (s.0, s.1)),
            Some((48, 32))
        );
        assert_eq!(
            size("<svg width='10' viewBox='0 0 40 20'/>").map(|s| (s.0, s.1)),
            Some((10, 5))
        );
        assert_eq!(
            size("<svg xmlns='http://www.w3.org/2000/svg'/>").map(|s| (s.0, s.1)),
            Some((300, 150))
        );
        assert_eq!(
            size("<svg width='2em' height='1in'/>").map(|s| (s.0, s.1)),
            Some((32, 96))
        );
        assert_eq!(
            size("<html><svg/></html>"),
            None,
            "an HTML page is not an SVG image"
        );
    }

    #[test]
    fn fills_shapes_with_coverage() {
        let b = draw(
            "<svg viewBox='0 0 10 10'><rect x='2' y='2' width='6' height='6' fill='#ff0000'/></svg>",
            10,
            10,
        );
        assert_eq!(px(&b, 5, 5), [255, 0, 0, 255]);
        assert_eq!(px(&b, 0, 0), [0, 0, 0, 0]);
        assert_eq!(px(&b, 1, 5), [0, 0, 0, 0]);
        let half = draw(
            "<svg viewBox='0 0 4 4'><rect x='0' y='0' width='2.5' height='4'/></svg>",
            4,
            4,
        );
        assert_eq!(px(&half, 1, 1), [0, 0, 0, 255]);
        assert_eq!(px(&half, 2, 1)[3], 128, "half covered");
        assert_eq!(px(&half, 3, 1)[3], 0);
    }

    #[test]
    fn view_box_scales_to_the_target() {
        let svg = "<svg width='10' height='10' viewBox='0 0 100 100'>\
                   <circle cx='50' cy='50' r='40' fill='blue'/></svg>";
        let b = draw(svg, 40, 40);
        assert_eq!((b.width, b.height), (40, 40), "vector images scale up");
        assert_eq!(px(&b, 20, 20), [0, 0, 255, 255]);
        assert_eq!(px(&b, 1, 1)[3], 0);
        assert_eq!(px(&b, 20, 3)[3], 0, "outside the radius");
    }

    #[test]
    fn path_commands_and_fill_rules() {
        // Two nested squares in one path: a hole with evenodd, solid with nonzero.
        let d = "M0 0H10V10H0Z M3 3h4v4h-4z";
        let even = draw(
            &format!("<svg viewBox='0 0 10 10'><path fill-rule='evenodd' d='{d}'/></svg>"),
            10,
            10,
        );
        assert_eq!(px(&even, 5, 5)[3], 0);
        assert_eq!(px(&even, 1, 1)[3], 255);
        let nonzero = draw(
            &format!("<svg viewBox='0 0 10 10'><path d='{d}'/></svg>"),
            10,
            10,
        );
        assert_eq!(px(&nonzero, 5, 5)[3], 255);
        // Curves and arcs: a full circle drawn with two arcs has its center filled.
        let arcs = draw(
            "<svg viewBox='0 0 10 10'><path d='M1 5a4 4 0 1 0 8 0a4 4 0 1 0-8 0'/></svg>",
            10,
            10,
        );
        assert_eq!(px(&arcs, 5, 5)[3], 255);
        assert_eq!(px(&arcs, 0, 0)[3], 0);
        let curve = draw(
            "<svg viewBox='0 0 10 10'><path d='M0 10C0 0 10 0 10 10z'/></svg>",
            10,
            10,
        );
        assert_eq!(px(&curve, 5, 8)[3], 255);
        assert_eq!(px(&curve, 0, 0)[3], 0);
    }

    #[test]
    fn strokes_groups_transforms_and_styles() {
        let svg = "<svg viewBox='0 0 20 20'>\
            <style>.a { fill: none; stroke: #00ff00; stroke-width: 2 }</style>\
            <g transform='translate(10 0)' fill='red'><rect width='5' height='5'/></g>\
            <line class='a' x1='0' y1='15' x2='20' y2='15'/>\
            <rect x='0' y='0' width='4' height='4' style='fill:blue;opacity:.5'/>\
            </svg>";
        let b = draw(svg, 20, 20);
        assert_eq!(px(&b, 12, 2), [255, 0, 0, 255], "group fill and translate");
        assert_eq!(
            px(&b, 5, 15),
            [0, 255, 0, 255],
            "stroke from a <style> rule"
        );
        assert_eq!(px(&b, 5, 12)[3], 0, "the stroke is 2 wide");
        assert_eq!(px(&b, 2, 2), [0, 0, 128, 128], "inline style and opacity");
    }

    #[test]
    fn use_symbols_gradients_and_hidden_parts() {
        let svg = "<svg viewBox='0 0 20 10' xmlns:xlink='http://www.w3.org/1999/xlink'>\
            <defs><linearGradient id='g'><stop offset='0' stop-color='#000'/>\
            <stop offset='1' stop-color='#fff'/></linearGradient>\
            <symbol id='s' viewBox='0 0 1 1'><rect width='1' height='1' fill='url(#g)'/></symbol></defs>\
            <use xlink:href='#s' x='10' width='10' height='10'/>\
            <rect width='10' height='10' fill='red' display='none'/>\
            </svg>";
        let b = draw(svg, 20, 10);
        assert_eq!(
            px(&b, 15, 5),
            [128, 128, 128, 255],
            "symbol, gradient as its average"
        );
        assert_eq!(px(&b, 5, 5)[3], 0, "display: none and <defs> draw nothing");
    }

    #[test]
    fn survives_malformed_input() {
        for svg in [
            "<svg",
            "<svg viewBox='0 0 0 0'><path d='M'/></svg>",
            "<svg><path d='M 1 1 L x y Z 3'/>",
            "<svg><use href='#a' id='a'/></svg>",
            "<svg><g><g><rect width='1e30' height='-1'/></svg>",
            "<svg><path d='M0 0 A 0 0 0 1 1 5 5 Q'/><circle r='nan'/></svg>",
            "<svg><polygon points='1 2 3'/><![CDATA[ x <svg>]]><!-- </svg> --></svg>",
        ] {
            let _ = crate::decode(svg.as_bytes(), 16, 16);
        }
        // A self-referencing <use> stops at the nesting limit.
        let b = draw("<svg viewBox='0 0 4 4'><g id='a'><rect width='4' height='4'/><use href='#a'/></g></svg>", 4, 4);
        assert_eq!(px(&b, 1, 1)[3], 255);
    }
}
