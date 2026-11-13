//! The parts of a TrueType / OpenType font that layout and paint need: glyph advances,
//! vertical metrics and outlines; WOFF and WOFF2 decoding (`woff`); and choosing a face per
//! character by family, weight and style (`faces`).
//!
//! Only the tables Broxee reads are kept in memory, never the whole font file.

mod faces;
pub mod woff;

/// Figtree, Broxee's interface face (DESIGN.md), bundled as WOFF2 (SIL Open Font License 1.1,
/// `figtree/OFL.txt`).
pub const FIGTREE_REGULAR: &[u8] = include_bytes!("figtree/Figtree-Regular.woff2");
pub const FIGTREE_SEMIBOLD: &[u8] = include_bytes!("figtree/Figtree-SemiBold.woff2");

pub use faces::{
    match_family, page_chain, FaceDesc, FaceId, FaceLoader, FacePlan, FontKey, Fonts, Role,
    SystemFace, SystemFaces, UI_FAMILY, WEB_FACES,
};

/// A face's vertical metrics in em: what a line needs, in one lookup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VMetrics {
    pub ascent: f32,
    /// Positive distance below the baseline.
    pub descent: f32,
    pub line_gap: f32,
}

use std::path::Path;

/// Text measurement used by layout, and glyph lookup used by paint. Values are in em, so
/// multiply by the font size in px. `key` names the family, weight and style asked for.
pub trait TextMetrics {
    fn advance_em(&self, ch: char, key: FontKey) -> f32;
    fn ascent_em(&self, key: FontKey) -> f32;
    /// Positive distance below the baseline.
    fn descent_em(&self, key: FontKey) -> f32;
    fn line_gap_em(&self, key: FontKey) -> f32;

    fn vmetrics(&self, key: FontKey) -> VMetrics {
        VMetrics {
            ascent: self.ascent_em(key),
            descent: self.descent_em(key),
            line_gap: self.line_gap_em(key),
        }
    }

    fn width(&self, text: &str, size: f32, key: FontKey) -> f32 {
        text.chars().map(|c| self.advance_em(c, key)).sum::<f32>() * size
    }

    /// The face and glyph that draw `ch`; `None` when nothing can draw text.
    fn glyph(&self, _ch: char, _key: FontKey) -> Option<(FaceId, u32)> {
        None
    }

    /// A face returned by `glyph`, for rasterizing.
    fn face(&self, _id: FaceId) -> Option<&Font> {
        None
    }
}

/// Finds a table of a font by tag.
type TableLookup<'a> = dyn Fn(&[u8; 4]) -> Result<&'a [u8], String> + 'a;

/// Fallback when no font file is available: every character is half an em wide.
/// Keeps layout deterministic in tests and on machines without the expected fonts.
pub struct Approximate;

impl TextMetrics for Approximate {
    fn advance_em(&self, ch: char, key: FontKey) -> f32 {
        let base = if ch == ' ' { 0.25 } else { 0.5 };
        if key.weight >= 600 {
            base * 1.1
        } else {
            base
        }
    }

    fn ascent_em(&self, _: FontKey) -> f32 {
        0.8
    }

    fn descent_em(&self, _: FontKey) -> f32 {
        0.2
    }

    fn line_gap_em(&self, _: FontKey) -> f32 {
        0.0
    }
}

/// One step of a glyph outline, in font units with y pointing up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    /// Control point, then end point.
    QuadTo(f32, f32, f32, f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outline {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    /// Closed contours: each starts with `MoveTo` and ends back at its start point.
    pub path: Vec<PathEl>,
}

/// x' = a*x + c*y + e, y' = b*x + d*y + f
#[derive(Clone, Copy)]
struct Transform {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Transform {
    const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// `self` applied after `inner`.
    fn then(&self, inner: &Transform) -> Transform {
        Transform {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }
}

/// Composite glyphs may nest; real fonts go two or three deep. Anything deeper is treated as broken.
const MAX_COMPONENT_DEPTH: u32 = 8;

enum Cmap {
    /// Segment mapping to delta values, BMP only.
    Format4(Vec<u8>),
    /// Segmented coverage: (first char, last char, first glyph).
    Format12(Vec<(u32, u32, u32)>),
}

pub struct Font {
    units_per_em: f32,
    ascent: f32,
    descent: f32,
    line_gap: f32,
    /// Advance per glyph id, in font units.
    advances: Vec<u16>,
    /// Glyph ids and advances for ASCII, looked up once at load so common text never touches
    /// the cmap or the advance list.
    ascii: [u16; 128],
    ascii_advance: [u16; 128],
    cmap: Cmap,
    /// Glyph outlines (`glyf`) and where each glyph starts in it (`loca`, one extra entry for the end).
    /// Empty for fonts with CFF outlines, which Broxee does not draw yet.
    glyf: Vec<u8>,
    loca: Vec<u32>,
}

impl Font {
    pub fn load(path: &Path) -> Result<Font, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Font::parse(&data).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse a font in any container the web uses: WOFF2, WOFF, or a plain sfnt.
    /// WOFF and WOFF2 tables go straight into the font, without assembling an sfnt first.
    pub fn parse_any(data: &[u8]) -> Result<Font, String> {
        match woff::container(data) {
            Some(woff::Container::Woff2) => Font::from_tables(woff::woff2_tables(data)?.1),
            Some(woff::Container::Woff) => Font::from_tables(woff::woff_tables(data)?.1),
            Some(woff::Container::Sfnt) => Font::parse(data),
            None => Err("not a font file".into()),
        }
    }

    /// Build a font from decoded tables, keeping the `glyf` buffer instead of copying it.
    fn from_tables(mut tables: Vec<([u8; 4], Vec<u8>)>) -> Result<Font, String> {
        let glyf = tables
            .iter()
            .position(|(tag, _)| tag == b"glyf")
            .map(|i| tables.swap_remove(i).1);
        let table = |tag: &[u8; 4]| -> Result<&[u8], String> {
            tables
                .iter()
                .find(|(t, _)| t == tag)
                .map(|(_, d)| d.as_slice())
                .ok_or_else(|| format!("missing table {}", String::from_utf8_lossy(tag)))
        };
        Font::build(&table, glyf)
    }

    /// Parse a `.ttf`, `.otf`, or the first face of a `.ttc` collection.
    pub fn parse(data: &[u8]) -> Result<Font, String> {
        let base = if data.get(0..4) == Some(b"ttcf") {
            read_u32(data, 12).ok_or("truncated collection header")? as usize
        } else {
            0
        };
        let table = |tag: &[u8; 4]| -> Result<&[u8], String> {
            let count = read_u16(data, base + 4).ok_or("truncated header")? as usize;
            for i in 0..count {
                let rec = base + 12 + i * 16;
                if data.get(rec..rec + 4) == Some(tag) {
                    let offset = read_u32(data, rec + 8).ok_or("truncated table record")? as usize;
                    let len = read_u32(data, rec + 12).ok_or("truncated table record")? as usize;
                    return data.get(offset..offset + len).ok_or_else(|| {
                        format!("table {} out of bounds", String::from_utf8_lossy(tag))
                    });
                }
            }
            Err(format!("missing table {}", String::from_utf8_lossy(tag)))
        };
        Font::build(&table, None)
    }

    /// Read the tables Broxee uses. `glyf` is moved in when the caller owns it, else copied.
    fn build<'a>(table: &TableLookup<'a>, owned_glyf: Option<Vec<u8>>) -> Result<Font, String> {
        let head = table(b"head")?;
        let hhea = table(b"hhea")?;
        let maxp = table(b"maxp")?;
        let hmtx = table(b"hmtx")?;
        let cmap = table(b"cmap")?;

        let units_per_em = f32::from(
            read_u16(head, 18)
                .filter(|&u| u > 0)
                .ok_or("bad unitsPerEm")?,
        );
        let ascent = f32::from(read_i16(hhea, 4).ok_or("bad hhea")?);
        let descent = -f32::from(read_i16(hhea, 6).ok_or("bad hhea")?);
        let line_gap = f32::from(read_i16(hhea, 8).ok_or("bad hhea")?);
        let metric_count = read_u16(hhea, 34).ok_or("bad hhea")? as usize;
        let glyph_count = read_u16(maxp, 4).ok_or("bad maxp")? as usize;

        let mut advances = Vec::with_capacity(glyph_count.max(metric_count));
        for i in 0..metric_count {
            advances.push(read_u16(hmtx, i * 4).ok_or("truncated hmtx")?);
        }
        let last = advances.last().copied().unwrap_or(0);
        advances.resize(glyph_count.max(metric_count), last);

        let glyf = match owned_glyf {
            Some(glyf) => Ok(glyf),
            None => table(b"glyf").map(<[u8]>::to_vec),
        };
        let (mut glyf, loca) = match (glyf, table(b"loca")) {
            (Ok(glyf), Ok(loca)) => {
                let long = read_i16(head, 50).ok_or("bad head")? == 1;
                let count = glyph_count + 1;
                let offsets = (0..count)
                    .map(|i| {
                        if long {
                            read_u32(loca, i * 4)
                        } else {
                            read_u16(loca, i * 2).map(|o| u32::from(o) * 2)
                        }
                    })
                    .collect::<Option<Vec<u32>>>()
                    .ok_or("truncated loca")?;
                (glyf, offsets)
            }
            _ => (Vec::new(), Vec::new()),
        };

        glyf.shrink_to_fit();
        let mut font = Font {
            units_per_em,
            ascent,
            descent,
            line_gap,
            advances,
            ascii: [0; 128],
            ascii_advance: [0; 128],
            cmap: parse_cmap(cmap)?,
            glyf,
            loca,
        };
        for c in 0..128usize {
            let glyph = font.lookup(char::from(c as u8));
            font.ascii[c] = glyph as u16;
            font.ascii_advance[c] = font.advance_units(glyph);
        }
        Ok(font)
    }

    /// Glyph id for `ch`, or 0 (the missing glyph).
    pub fn glyph(&self, ch: char) -> u32 {
        match ch as u32 {
            c @ 0..128 => u32::from(self.ascii[c as usize]),
            _ => self.lookup(ch),
        }
    }

    fn lookup(&self, ch: char) -> u32 {
        let c = ch as u32;
        match &self.cmap {
            Cmap::Format12(groups) => {
                let i = groups.partition_point(|&(_, end, _)| end < c);
                match groups.get(i) {
                    Some(&(start, _, glyph)) if start <= c => glyph + (c - start),
                    _ => 0,
                }
            }
            Cmap::Format4(table) => format4_lookup(table, c).unwrap_or(0),
        }
    }

    fn advance_units(&self, glyph: u32) -> u16 {
        self.advances
            .get(glyph as usize)
            .or(self.advances.first())
            .copied()
            .unwrap_or(0)
    }

    pub fn advance_em(&self, ch: char) -> f32 {
        self.glyph_advance_em(self.glyph(ch))
    }

    /// For ASCII `ch` this font has: its advance in font units.
    #[inline]
    fn ascii_units(&self, ch: char) -> Option<u16> {
        let c = ch as usize;
        (c < 128 && self.ascii[c] != 0).then(|| self.ascii_advance[c])
    }

    /// Bytes this font holds in memory: outlines, their offsets, advances and the char map.
    pub fn memory(&self) -> usize {
        let cmap = match &self.cmap {
            Cmap::Format4(t) => t.len(),
            Cmap::Format12(groups) => groups.len() * 12,
        };
        self.glyf.len() + self.loca.len() * 4 + self.advances.len() * 2 + cmap + 256
    }

    pub fn glyph_count(&self) -> usize {
        self.advances.len()
    }

    pub fn glyph_advance_em(&self, glyph: u32) -> f32 {
        f32::from(self.advance_units(glyph)) / self.units_per_em
    }

    /// Whether glyphs can be drawn: TrueType outlines. Fonts with CFF outlines measure text
    /// but cannot draw it yet.
    pub fn has_outlines(&self) -> bool {
        !self.loca.is_empty()
    }

    pub fn ascent_em(&self) -> f32 {
        self.ascent / self.units_per_em
    }

    pub fn descent_em(&self) -> f32 {
        self.descent / self.units_per_em
    }

    pub fn line_gap_em(&self) -> f32 {
        self.line_gap / self.units_per_em
    }

    pub fn units_per_em(&self) -> f32 {
        self.units_per_em
    }

    fn glyph_data(&self, glyph: u32) -> Option<&[u8]> {
        let start = *self.loca.get(glyph as usize)? as usize;
        let end = *self.loca.get(glyph as usize + 1)? as usize;
        if end <= start {
            return None;
        }
        self.glyf.get(start..end)
    }

    /// The outline of a glyph in font units, or `None` for empty glyphs (the space) and fonts
    /// without TrueType outlines.
    pub fn outline(&self, glyph: u32) -> Option<Outline> {
        let data = self.glyph_data(glyph)?;
        let mut path = Vec::new();
        self.append_glyph(glyph, &Transform::IDENTITY, &mut path, 0)?;
        if path.is_empty() {
            return None;
        }
        Some(Outline {
            x_min: f32::from(read_i16(data, 2)?),
            y_min: f32::from(read_i16(data, 4)?),
            x_max: f32::from(read_i16(data, 6)?),
            y_max: f32::from(read_i16(data, 8)?),
            path,
        })
    }

    fn append_glyph(
        &self,
        glyph: u32,
        t: &Transform,
        path: &mut Vec<PathEl>,
        depth: u32,
    ) -> Option<()> {
        if depth > MAX_COMPONENT_DEPTH {
            return None;
        }
        let Some(data) = self.glyph_data(glyph) else {
            return Some(());
        };
        let contours = read_i16(data, 0)?;
        if contours >= 0 {
            append_simple(data, contours as usize, t, path)
        } else {
            self.append_composite(data, t, path, depth)
        }
    }

    fn append_composite(
        &self,
        d: &[u8],
        t: &Transform,
        path: &mut Vec<PathEl>,
        depth: u32,
    ) -> Option<()> {
        const WORDS: u16 = 0x0001;
        const XY_VALUES: u16 = 0x0002;
        const SCALE: u16 = 0x0008;
        const MORE: u16 = 0x0020;
        const XY_SCALE: u16 = 0x0040;
        const TWO_BY_TWO: u16 = 0x0080;
        let f2dot14 = |at: usize| read_i16(d, at).map(|v| f32::from(v) / 16384.0);

        let mut p = 10;
        loop {
            let flags = read_u16(d, p)?;
            let component = u32::from(read_u16(d, p + 2)?);
            p += 4;
            let (arg1, arg2) = if flags & WORDS != 0 {
                p += 4;
                (
                    f32::from(read_i16(d, p - 4)?),
                    f32::from(read_i16(d, p - 2)?),
                )
            } else {
                p += 2;
                (
                    f32::from(*d.get(p - 2)? as i8),
                    f32::from(*d.get(p - 1)? as i8),
                )
            };
            let mut local = Transform::IDENTITY;
            // Point-matching placement (without XY_VALUES) is rare and not supported: offset 0.
            if flags & XY_VALUES != 0 {
                local.e = arg1;
                local.f = arg2;
            }
            if flags & SCALE != 0 {
                local.a = f2dot14(p)?;
                local.d = local.a;
                p += 2;
            } else if flags & XY_SCALE != 0 {
                local.a = f2dot14(p)?;
                local.d = f2dot14(p + 2)?;
                p += 4;
            } else if flags & TWO_BY_TWO != 0 {
                local.a = f2dot14(p)?;
                local.b = f2dot14(p + 2)?;
                local.c = f2dot14(p + 4)?;
                local.d = f2dot14(p + 6)?;
                p += 8;
            }
            self.append_glyph(component, &t.then(&local), path, depth + 1)?;
            if flags & MORE == 0 {
                return Some(());
            }
        }
    }
}

/// Decode a simple glyph: contour end points, flags (with repeats), then delta-encoded x and y.
fn append_simple(d: &[u8], contours: usize, t: &Transform, path: &mut Vec<PathEl>) -> Option<()> {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;

    if contours == 0 {
        return Some(());
    }
    let ends: Vec<usize> = (0..contours)
        .map(|i| read_u16(d, 10 + i * 2).map(usize::from))
        .collect::<Option<_>>()?;
    let points = ends.last()? + 1;
    let instructions = read_u16(d, 10 + contours * 2)? as usize;
    let mut p = 10 + contours * 2 + 2 + instructions;

    let mut flags = Vec::with_capacity(points);
    while flags.len() < points {
        let f = *d.get(p)?;
        p += 1;
        flags.push(f);
        if f & REPEAT != 0 {
            let n = *d.get(p)?;
            p += 1;
            for _ in 0..n {
                flags.push(f);
            }
        }
    }
    flags.truncate(points);

    let mut read_coords = |short: u8, same_or_positive: u8| -> Option<Vec<f32>> {
        let mut value = 0i32;
        let mut out = Vec::with_capacity(points);
        for &f in &flags {
            if f & short != 0 {
                let delta = i32::from(*d.get(p)?);
                p += 1;
                value += if f & same_or_positive != 0 {
                    delta
                } else {
                    -delta
                };
            } else if f & same_or_positive == 0 {
                value += i32::from(read_i16(d, p)?);
                p += 2;
            }
            out.push(value as f32);
        }
        Some(out)
    };
    let xs = read_coords(X_SHORT, X_SAME_OR_POSITIVE)?;
    let ys = read_coords(Y_SHORT, Y_SAME_OR_POSITIVE)?;

    let mut start = 0;
    for &end in &ends {
        if end < start || end >= points {
            return None;
        }
        let contour: Vec<(f32, f32, bool)> = (start..=end)
            .map(|i| {
                let (x, y) = t.apply(xs[i], ys[i]);
                (x, y, flags[i] & ON_CURVE != 0)
            })
            .collect();
        append_contour(contour, path);
        start = end + 1;
    }
    Some(())
}

/// Turn TrueType points into path elements. Two off-curve points in a row imply an on-curve
/// point halfway between them.
fn append_contour(mut pts: Vec<(f32, f32, bool)>, path: &mut Vec<PathEl>) {
    let Some(&first) = pts.first() else { return };
    let last = *pts.last().expect("non-empty");
    if !first.2 {
        if last.2 {
            pts.rotate_right(1);
        } else {
            pts.insert(
                0,
                ((first.0 + last.0) / 2.0, (first.1 + last.1) / 2.0, true),
            );
        }
    }
    let (sx, sy, _) = pts[0];
    path.push(PathEl::MoveTo(sx, sy));
    let mut control: Option<(f32, f32)> = None;
    for &(x, y, on) in pts[1..].iter().chain(std::iter::once(&(sx, sy, true))) {
        match (on, control) {
            (true, Some((cx, cy))) => {
                path.push(PathEl::QuadTo(cx, cy, x, y));
                control = None;
            }
            (true, None) => path.push(PathEl::LineTo(x, y)),
            (false, Some((cx, cy))) => {
                path.push(PathEl::QuadTo(cx, cy, (cx + x) / 2.0, (cy + y) / 2.0));
                control = Some((x, y));
            }
            (false, None) => control = Some((x, y)),
        }
    }
}

fn parse_cmap(cmap: &[u8]) -> Result<Cmap, String> {
    let count = read_u16(cmap, 2).ok_or("truncated cmap")? as usize;
    let mut format4 = None;
    for i in 0..count {
        let rec = 4 + i * 8;
        let platform = read_u16(cmap, rec).ok_or("truncated cmap")?;
        let encoding = read_u16(cmap, rec + 2).ok_or("truncated cmap")?;
        let offset = read_u32(cmap, rec + 4).ok_or("truncated cmap")? as usize;
        let Some(sub) = cmap.get(offset..) else {
            continue;
        };
        let unicode = platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10));
        if !unicode {
            continue;
        }
        match read_u16(sub, 0) {
            Some(12) => {
                let groups = read_u32(sub, 12).ok_or("truncated cmap 12")? as usize;
                let mut out = Vec::with_capacity(groups);
                for g in 0..groups {
                    let at = 16 + g * 12;
                    out.push((
                        read_u32(sub, at).ok_or("truncated cmap 12")?,
                        read_u32(sub, at + 4).ok_or("truncated cmap 12")?,
                        read_u32(sub, at + 8).ok_or("truncated cmap 12")?,
                    ));
                }
                // Full Unicode coverage wins over the BMP-only format 4.
                return Ok(Cmap::Format12(out));
            }
            Some(4) if format4.is_none() => {
                let len = read_u16(sub, 2).ok_or("truncated cmap 4")? as usize;
                format4 = Some(sub.get(..len).ok_or("truncated cmap 4")?.to_vec());
            }
            _ => {}
        }
    }
    format4
        .map(Cmap::Format4)
        .ok_or_else(|| "no Unicode cmap".to_string())
}

fn format4_lookup(t: &[u8], c: u32) -> Option<u32> {
    if c > 0xFFFF {
        return None;
    }
    let seg_x2 = read_u16(t, 6)? as usize;
    let ends = 14;
    let starts = ends + seg_x2 + 2;
    let deltas = starts + seg_x2;
    let ranges = deltas + seg_x2;
    let segments = seg_x2 / 2;
    // End codes are sorted; find the first segment ending at or after c.
    let (mut lo, mut hi) = (0, segments);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if u32::from(read_u16(t, ends + mid * 2)?) < c {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == segments {
        return None;
    }
    let start = u32::from(read_u16(t, starts + lo * 2)?);
    if start > c {
        return None;
    }
    let delta = u32::from(read_u16(t, deltas + lo * 2)?);
    let range_offset = read_u16(t, ranges + lo * 2)? as usize;
    if range_offset == 0 {
        return Some((c + delta) & 0xFFFF);
    }
    let at = ranges + lo * 2 + range_offset + (c - start) as usize * 2;
    let glyph = u32::from(read_u16(t, at)?);
    (glyph != 0).then_some((glyph + delta) & 0xFFFF)
}

fn read_u16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn read_i16(d: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16(v: &mut Vec<u8>, x: i32) {
        v.extend_from_slice(&(x as i16).to_be_bytes());
    }

    /// Glyph 1: a 100-unit square. Glyph 2: glyph 1 shifted right by 50. Glyph 3: a curve
    /// (on, off, on). Glyphs 0 and 4 are empty.
    fn glyph_tables() -> (Vec<u8>, Vec<u8>) {
        let mut square = Vec::new();
        for v in [1, 0, 0, 100, 100, 3, 0] {
            be16(&mut square, v);
        }
        square.extend_from_slice(&[1, 1, 1, 1]);
        for v in [0, 100, 0, -100, 0, 0, 100, 0] {
            be16(&mut square, v);
        }
        let mut composite = Vec::new();
        for v in [-1, 50, 0, 150, 100, 0x0003, 1, 50, 0] {
            be16(&mut composite, v);
        }
        let mut curve = Vec::new();
        for v in [1, 0, 0, 100, 100, 2, 0] {
            be16(&mut curve, v);
        }
        curve.extend_from_slice(&[1, 0, 1]);
        for v in [0, 50, 50, 0, 100, -100] {
            be16(&mut curve, v);
        }
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        for g in [Vec::new(), square, composite, curve, Vec::new()] {
            loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
            glyf.extend_from_slice(&g);
        }
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        (glyf, loca)
    }

    /// Build a minimal sfnt in memory so tests do not depend on system fonts.
    fn build_font(cmap_sub: Vec<u8>) -> Vec<u8> {
        let mut head = vec![0u8; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[50..52].copy_from_slice(&1i16.to_be_bytes());
        let (glyf, loca) = glyph_tables();
        let mut hhea = vec![0u8; 36];
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[8..10].copy_from_slice(&100i16.to_be_bytes());
        hhea[34..36].copy_from_slice(&3u16.to_be_bytes());
        let mut maxp = vec![0u8; 6];
        maxp[4..6].copy_from_slice(&5u16.to_be_bytes());
        // Glyph 0: 500, glyph 1: 600, glyph 2: 700; glyphs 3 and 4 reuse the last advance.
        let mut hmtx = Vec::new();
        for adv in [500u16, 600, 700] {
            hmtx.extend_from_slice(&adv.to_be_bytes());
            hmtx.extend_from_slice(&0i16.to_be_bytes());
        }
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&0u16.to_be_bytes());
        cmap.extend_from_slice(&1u16.to_be_bytes());
        cmap.extend_from_slice(&3u16.to_be_bytes());
        cmap.extend_from_slice(&(if cmap_sub[1] == 12 { 10u16 } else { 1u16 }).to_be_bytes());
        cmap.extend_from_slice(&12u32.to_be_bytes());
        cmap.extend_from_slice(&cmap_sub);

        let tables: [(&[u8; 4], Vec<u8>); 7] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
        ];
        let mut out = vec![0, 1, 0, 0];
        out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        let mut offset = 12 + tables.len() * 16;
        let mut body = Vec::new();
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            offset += data.len();
        }
        out.extend_from_slice(&body);
        out
    }

    /// Format 4 mapping 'A'..='B' to glyphs 1..=2, plus the required 0xFFFF end segment.
    fn format4() -> Vec<u8> {
        let mut t = Vec::new();
        let words: [u16; 16] = [
            4,
            32,
            0, // format, length, language
            4,
            0,
            0,
            0, // segCountX2, searchRange, entrySelector, rangeShift
            0x42,
            0xFFFF, // end codes
            0,      // reserved
            0x41,
            0xFFFF, // start codes
            (1u16).wrapping_sub(0x41),
            1, // deltas
            0,
            0, // range offsets
        ];
        for w in words {
            t.extend_from_slice(&w.to_be_bytes());
        }
        t
    }

    fn format12() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&12u16.to_be_bytes());
        t.extend_from_slice(&0u16.to_be_bytes());
        for v in [28u32, 0, 1, 0x1F600, 0x1F601, 3] {
            t.extend_from_slice(&v.to_be_bytes());
        }
        t
    }

    #[test]
    fn reads_metrics_and_format4() {
        let font = Font::parse(&build_font(format4())).unwrap();
        assert_eq!(font.glyph('A'), 1);
        assert_eq!(font.glyph('B'), 2);
        assert_eq!(font.glyph('C'), 0);
        assert_eq!(font.advance_em('A'), 0.6);
        assert_eq!(font.advance_em('B'), 0.7);
        assert_eq!(font.advance_em('Z'), 0.5, "missing glyph uses glyph 0");
        assert_eq!(
            (font.ascent_em(), font.descent_em(), font.line_gap_em()),
            (0.8, 0.2, 0.1)
        );
    }

    #[test]
    fn reads_format12_beyond_the_bmp() {
        let font = Font::parse(&build_font(format12())).unwrap();
        assert_eq!(font.glyph('\u{1F600}'), 3);
        assert_eq!(font.glyph('\u{1F601}'), 4);
        assert_eq!(
            font.advance_em('\u{1F601}'),
            0.7,
            "glyphs past the metrics reuse the last advance"
        );
        assert_eq!(font.glyph('A'), 0);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(Font::parse(b"").is_err());
        assert!(Font::parse(b"ttcf").is_err());
        assert!(Font::parse(&[0u8; 64]).is_err());
        let mut truncated = build_font(format4());
        truncated.truncate(100);
        assert!(Font::parse(&truncated).is_err());
    }

    #[test]
    fn reads_simple_composite_and_curved_outlines() {
        use PathEl::*;
        let font = Font::parse(&build_font(format4())).unwrap();
        let square = font.outline(1).unwrap();
        assert_eq!((square.x_min, square.y_max), (0.0, 100.0));
        assert_eq!(
            square.path,
            [
                MoveTo(0.0, 0.0),
                LineTo(100.0, 0.0),
                LineTo(100.0, 100.0),
                LineTo(0.0, 100.0),
                LineTo(0.0, 0.0)
            ]
        );
        let shifted = font.outline(2).unwrap();
        assert_eq!(shifted.path[0], MoveTo(50.0, 0.0));
        assert_eq!(shifted.path[2], LineTo(150.0, 100.0));
        let curve = font.outline(3).unwrap();
        assert_eq!(
            curve.path,
            [
                MoveTo(0.0, 0.0),
                QuadTo(50.0, 100.0, 100.0, 0.0),
                LineTo(0.0, 0.0)
            ]
        );
        assert!(font.outline(0).is_none(), "empty glyph");
        assert!(font.outline(99).is_none(), "out of range");
    }

    #[test]
    fn approximate_metrics_are_deterministic() {
        assert_eq!(Approximate.width("ab c", 10.0, FontKey::weight(400)), 17.5);
    }
}
