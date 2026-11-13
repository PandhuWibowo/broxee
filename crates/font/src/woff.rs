//! WOFF and WOFF2: the compressed font containers the web serves. Both decode to a plain
//! sfnt (the bytes of a `.ttf`), which `Font::parse` then reads like any other font.
//!
//! WOFF compresses each table with zlib. WOFF2 compresses all tables together with Brotli and
//! usually stores `glyf` and `loca` transformed (split into streams that compress better), and
//! sometimes `hmtx`; those are rebuilt here (W3C WOFF2, section 5).

/// The largest font Broxee rebuilds. Real web fonts are a few hundred KB; CJK fonts split by
/// `unicode-range` stay under a few MB.
pub const MAX_FONT_BYTES: usize = 24 << 20;

/// What kind of font file this is, from its first four bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    Sfnt,
    Woff,
    Woff2,
}

pub fn container(data: &[u8]) -> Option<Container> {
    match data.get(0..4)? {
        b"wOFF" => Some(Container::Woff),
        b"wOF2" => Some(Container::Woff2),
        [0, 1, 0, 0] | b"true" | b"OTTO" | b"ttcf" => Some(Container::Sfnt),
        _ => None,
    }
}

/// Assemble an sfnt from its tables. Checksums are left at zero: nothing here checks them.
pub(crate) fn build_sfnt(flavor: u32, tables: &mut [([u8; 4], Vec<u8>)]) -> Vec<u8> {
    tables.sort_by_key(|(tag, _)| *tag);
    let count = tables.len() as u16;
    let total: usize = tables
        .iter()
        .map(|(_, d)| d.len().next_multiple_of(4))
        .sum();
    let mut out = Vec::with_capacity(12 + tables.len() * 16 + total);
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    // searchRange, entrySelector, rangeShift: hints for binary search, unused by readers here.
    let mut entry_selector = 0u16;
    while 1u32 << (entry_selector + 1) <= u32::from(count) {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector).wrapping_mul(16);
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&(count.wrapping_mul(16).wrapping_sub(search_range)).to_be_bytes());
    let mut offset = 12 + tables.len() * 16;
    for (tag, data) in tables.iter() {
        out.extend_from_slice(tag);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in tables.iter() {
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

/// Decode a WOFF 1.0 file into an sfnt.
pub fn decode_woff(data: &[u8]) -> Result<Vec<u8>, String> {
    let (flavor, mut tables) = woff_tables(data)?;
    Ok(build_sfnt(flavor, &mut tables))
}

/// A decoded font's flavor (sfnt version) and its tables.
pub(crate) type Tables = (u32, Vec<([u8; 4], Vec<u8>)>);

/// The tables of a WOFF 1.0 file.
pub(crate) fn woff_tables(data: &[u8]) -> Result<Tables, String> {
    let r = Reader::new(data);
    if r.bytes(0, 4) != Some(b"wOFF") {
        return Err("not a WOFF file".into());
    }
    let flavor = r.u32(4).ok_or("truncated WOFF header")?;
    let count = r.u16(12).ok_or("truncated WOFF header")? as usize;
    let mut tables = Vec::with_capacity(count);
    let mut total = 0usize;
    for i in 0..count {
        let at = 44 + i * 20;
        let tag: [u8; 4] = r
            .bytes(at, 4)
            .and_then(|t| t.try_into().ok())
            .ok_or("truncated WOFF directory")?;
        let offset = r.u32(at + 4).ok_or("truncated WOFF directory")? as usize;
        let compressed = r.u32(at + 8).ok_or("truncated WOFF directory")? as usize;
        let original = r.u32(at + 12).ok_or("truncated WOFF directory")? as usize;
        total = total.saturating_add(original);
        if total > MAX_FONT_BYTES {
            return Err("WOFF font too large".into());
        }
        let stored = r
            .bytes(offset, compressed)
            .ok_or("WOFF table out of bounds")?;
        let table = if compressed < original {
            let out = flate::zlib_decompress(stored, original)
                .map_err(|e| format!("WOFF table: {e:?}"))?;
            if out.len() != original {
                return Err("WOFF table has the wrong length".into());
            }
            out
        } else if compressed == original {
            stored.to_vec()
        } else {
            return Err("WOFF table larger than its original".into());
        };
        tables.push((tag, table));
    }
    Ok((flavor, tables))
}

/// Tags WOFF2 refers to by index (section 4.1, table 1); index 63 means the tag follows.
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
    b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
    b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
    b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
    b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
    b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
    b"Gloc", b"Feat", b"Sill",
];

struct Woff2Table {
    tag: [u8; 4],
    original: usize,
    /// Bytes this table takes in the decompressed stream.
    stored: usize,
    transformed: bool,
}

/// Decode a WOFF2 file into an sfnt. Font collections are not supported.
pub fn decode_woff2(data: &[u8]) -> Result<Vec<u8>, String> {
    let (flavor, mut tables) = woff2_tables(data)?;
    Ok(build_sfnt(flavor, &mut tables))
}

/// The tables of a WOFF2 file, with `glyf`, `loca` and `hmtx` rebuilt from their transforms.
pub(crate) fn woff2_tables(data: &[u8]) -> Result<Tables, String> {
    let r = Reader::new(data);
    if r.bytes(0, 4) != Some(b"wOF2") {
        return Err("not a WOFF2 file".into());
    }
    let flavor = r.u32(4).ok_or("truncated WOFF2 header")?;
    if flavor == u32::from_be_bytes(*b"ttcf") {
        return Err("WOFF2 font collections are not supported".into());
    }
    let count = r.u16(12).ok_or("truncated WOFF2 header")? as usize;
    let compressed_size = r.u32(20).ok_or("truncated WOFF2 header")? as usize;
    let mut at = 48;
    let mut tables = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let flags = r.u8(at).ok_or("truncated WOFF2 directory")?;
        at += 1;
        let tag = match flags & 63 {
            63 => {
                let tag: [u8; 4] = r
                    .bytes(at, 4)
                    .and_then(|t| t.try_into().ok())
                    .ok_or("truncated WOFF2 directory")?;
                at += 4;
                tag
            }
            i => *KNOWN_TAGS[i as usize],
        };
        let version = flags >> 6;
        let original = r.base128(&mut at).ok_or("bad WOFF2 table length")? as usize;
        // glyf and loca are transformed at version 0 and plain at 3; everything else the reverse.
        let glyf_or_loca = &tag == b"glyf" || &tag == b"loca";
        let transformed = if glyf_or_loca {
            version == 0
        } else {
            version != 0
        };
        let stored = if transformed {
            r.base128(&mut at).ok_or("bad WOFF2 table length")? as usize
        } else {
            original
        };
        total = total.saturating_add(original).saturating_add(stored);
        if total > MAX_FONT_BYTES * 2 {
            return Err("WOFF2 font too large".into());
        }
        tables.push(Woff2Table {
            tag,
            original,
            stored,
            transformed,
        });
    }
    let compressed = r
        .bytes(at, compressed_size)
        .ok_or("WOFF2 data out of bounds")?;
    let expected: usize = tables.iter().map(|t| t.stored).sum();
    let stream = flate::brotli(compressed, expected).map_err(|e| format!("WOFF2: {e:?}"))?;
    if stream.len() != expected {
        return Err("WOFF2 data has the wrong length".into());
    }

    let mut offset = 0;
    let mut parts: Vec<(&Woff2Table, &[u8])> = Vec::with_capacity(tables.len());
    for table in &tables {
        parts.push((table, &stream[offset..offset + table.stored]));
        offset += table.stored;
    }
    let find = |tag: &[u8; 4]| parts.iter().find(|(t, _)| &t.tag == tag);

    let mut out: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(parts.len());
    // glyf first: loca comes out of it, and a transformed hmtx needs its x minimums.
    let mut x_mins = Vec::new();
    let mut loca_long = None;
    if let Some((glyf, data)) = find(b"glyf") {
        if glyf.transformed {
            let rebuilt = rebuild_glyf(data)?;
            loca_long = Some(rebuilt.long);
            x_mins = rebuilt.x_mins;
            out.push((*b"glyf", rebuilt.glyf));
            out.push((*b"loca", rebuilt.loca));
        }
    }
    for (table, data) in &parts {
        let tag = table.tag;
        if (&tag == b"glyf" || &tag == b"loca") && loca_long.is_some() {
            continue;
        }
        if table.transformed {
            if &tag == b"hmtx" {
                let hhea = find(b"hhea").ok_or("WOFF2 hmtx without hhea")?.1;
                let metrics = Reader::new(hhea).u16(34).ok_or("bad hhea")? as usize;
                out.push((tag, rebuild_hmtx(data, metrics, &x_mins)?));
                continue;
            }
            return Err(format!(
                "WOFF2 transform of {} is not supported",
                String::from_utf8_lossy(&tag)
            ));
        }
        if data.len() != table.original {
            return Err("WOFF2 table has the wrong length".into());
        }
        let mut data = data.to_vec();
        if &tag == b"head" {
            if let Some(long) = loca_long {
                // indexToLocFormat must describe the loca written here.
                if data.len() >= 54 {
                    data[50..52].copy_from_slice(&i16::from(long).to_be_bytes());
                }
            }
        }
        out.push((tag, data));
    }
    Ok((flavor, out))
}

struct Rebuilt {
    glyf: Vec<u8>,
    loca: Vec<u8>,
    long: bool,
    /// Each glyph's x minimum, for rebuilding left side bearings (0 for empty glyphs).
    x_mins: Vec<i16>,
}

/// Rebuild `glyf` and `loca` from the transformed glyf table (WOFF2 section 5.1).
fn rebuild_glyf<'a>(data: &'a [u8]) -> Result<Rebuilt, String> {
    let r = Reader::new(data);
    let options = r.u16(2).ok_or("truncated glyf transform")?;
    let glyphs = r.u16(4).ok_or("truncated glyf transform")? as usize;
    let long = r.u16(6).ok_or("truncated glyf transform")? != 0;
    let mut sizes = [0usize; 7];
    for (i, size) in sizes.iter_mut().enumerate() {
        *size = r.u32(8 + i * 4).ok_or("truncated glyf transform")? as usize;
    }
    let mut at = 36;
    let mut slices = [&data[..0]; 8];
    for (i, slice) in slices.iter_mut().enumerate() {
        let len = match i {
            7 if options & 1 != 0 => glyphs.div_ceil(8),
            7 => 0,
            _ => sizes[i],
        };
        *slice = r.bytes(at, len).ok_or("glyf stream out of bounds")?;
        at += len;
    }
    let stream = |d: &'a [u8]| Stream { d, at: 0 };
    let mut contours = stream(slices[0]);
    let mut points = stream(slices[1]);
    let mut flags = stream(slices[2]);
    let mut coords = stream(slices[3]);
    let mut components = stream(slices[4]);
    let mut instructions = stream(slices[6]);
    let overlap = (options & 1 != 0).then_some(slices[7]);
    let bitmap_len = glyphs.div_ceil(32) * 4;
    let bbox_bitmap = slices[5].get(..bitmap_len).ok_or("bbox bitmap truncated")?;
    let mut bboxes = stream(&slices[5][bitmap_len..]);
    let has_bit = |bits: &[u8], i: usize| bits[i / 8] & (0x80 >> (i % 8)) != 0;

    let mut glyf = Vec::with_capacity(data.len() + data.len() / 2);
    let mut offsets = Vec::with_capacity(glyphs + 1);
    let mut x_mins = vec![0i16; glyphs];
    let mut ends = Vec::new();
    let mut pts: Vec<(i32, i32, bool)> = Vec::new();
    for (g, x_min) in x_mins.iter_mut().enumerate() {
        offsets.push(glyf.len());
        let n = contours.i16().ok_or("contour stream truncated")?;
        let explicit_bbox = has_bit(bbox_bitmap, g);
        if n == 0 {
            if explicit_bbox {
                return Err("empty glyph with a bounding box".into());
            }
            continue;
        }
        if n > 0 {
            ends.clear();
            pts.clear();
            let mut total = 0usize;
            for _ in 0..n {
                total += points.u255().ok_or("point stream truncated")? as usize;
                if total > 0xFFFF {
                    return Err("glyph has too many points".into());
                }
                ends.push(total as u16 - 1);
            }
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..total {
                let flag = flags.u8().ok_or("flag stream truncated")?;
                let (dx, dy) = coords
                    .triplet(flag & 0x7F)
                    .ok_or("glyph stream truncated")?;
                x += dx;
                y += dy;
                pts.push((x, y, flag & 0x80 == 0));
            }
            let instruction_len = coords.u255().ok_or("glyph stream truncated")? as usize;
            let bbox = if explicit_bbox {
                bboxes.bbox().ok_or("bbox stream truncated")?
            } else {
                points_bbox(&pts)
            };
            push_bbox(&mut glyf, n, bbox);
            for end in &ends {
                glyf.extend_from_slice(&end.to_be_bytes());
            }
            glyf.extend_from_slice(&(instruction_len as u16).to_be_bytes());
            glyf.extend_from_slice(
                instructions
                    .take(instruction_len)
                    .ok_or("instruction stream truncated")?,
            );
            let overlaps = overlap.is_some_and(|bits| has_bit(bits, g));
            write_points(&mut glyf, &pts, overlaps);
            *x_min = bbox[0];
        } else {
            if !explicit_bbox {
                return Err("composite glyph without a bounding box".into());
            }
            let bbox = bboxes.bbox().ok_or("bbox stream truncated")?;
            push_bbox(&mut glyf, -1, bbox);
            let begin = components.at;
            let mut has_instructions = false;
            loop {
                let flags = components.u16().ok_or("composite stream truncated")?;
                let mut len = 2; // glyph index
                len += if flags & 0x0001 != 0 { 4 } else { 2 };
                len += match flags {
                    f if f & 0x0008 != 0 => 2,
                    f if f & 0x0040 != 0 => 4,
                    f if f & 0x0080 != 0 => 8,
                    _ => 0,
                };
                components.take(len).ok_or("composite stream truncated")?;
                has_instructions |= flags & 0x0100 != 0;
                if flags & 0x0020 == 0 {
                    break;
                }
            }
            glyf.extend_from_slice(&components.d[begin..components.at]);
            if has_instructions {
                let len = coords.u255().ok_or("glyph stream truncated")? as usize;
                glyf.extend_from_slice(&(len as u16).to_be_bytes());
                glyf.extend_from_slice(
                    instructions
                        .take(len)
                        .ok_or("instruction stream truncated")?,
                );
            }
            *x_min = bbox[0];
        }
        if glyf.len() > MAX_FONT_BYTES {
            return Err("rebuilt glyf too large".into());
        }
        // Short loca stores offsets / 2, so every glyph starts on an even offset; 4 keeps
        // glyphs aligned either way.
        glyf.resize(glyf.len().next_multiple_of(4), 0);
    }
    offsets.push(glyf.len());
    let long = long || glyf.len() > 0x1FFFE;
    let mut loca = Vec::with_capacity(offsets.len() * if long { 4 } else { 2 });
    for offset in offsets {
        if long {
            loca.extend_from_slice(&(offset as u32).to_be_bytes());
        } else {
            loca.extend_from_slice(&((offset / 2) as u16).to_be_bytes());
        }
    }
    Ok(Rebuilt {
        glyf,
        loca,
        long,
        x_mins,
    })
}

fn points_bbox(pts: &[(i32, i32, bool)]) -> [i16; 4] {
    let mut b = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
    for &(x, y, _) in pts {
        b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
    }
    if pts.is_empty() {
        return [0; 4];
    }
    b.map(|v| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
}

fn push_bbox(out: &mut Vec<u8>, contours: i16, bbox: [i16; 4]) {
    out.extend_from_slice(&contours.to_be_bytes());
    for v in bbox {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

/// Write the flags and coordinates of a simple glyph, in the TrueType encoding: one-byte
/// deltas where they fit, nothing for a repeated coordinate. Flags are not run-length packed.
fn write_points(out: &mut Vec<u8>, pts: &[(i32, i32, bool)], overlaps: bool) {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;
    const OVERLAP_SIMPLE: u8 = 0x40;
    let mut xs = Vec::with_capacity(pts.len() * 2);
    let mut ys = Vec::with_capacity(pts.len() * 2);
    let (mut last_x, mut last_y) = (0i32, 0i32);
    for (i, &(x, y, on)) in pts.iter().enumerate() {
        let mut flag = if on { ON_CURVE } else { 0 };
        if i == 0 && overlaps {
            flag |= OVERLAP_SIMPLE;
        }
        let (dx, dy) = (x - last_x, y - last_y);
        (last_x, last_y) = (x, y);
        if dx == 0 {
            flag |= X_SAME_OR_POSITIVE;
        } else if dx.abs() < 256 {
            flag |= X_SHORT | if dx > 0 { X_SAME_OR_POSITIVE } else { 0 };
            xs.push(dx.unsigned_abs() as u8);
        } else {
            xs.extend_from_slice(&(dx as i16).to_be_bytes());
        }
        if dy == 0 {
            flag |= Y_SAME_OR_POSITIVE;
        } else if dy.abs() < 256 {
            flag |= Y_SHORT | if dy > 0 { Y_SAME_OR_POSITIVE } else { 0 };
            ys.push(dy.unsigned_abs() as u8);
        } else {
            ys.extend_from_slice(&(dy as i16).to_be_bytes());
        }
        out.push(flag);
    }
    out.extend_from_slice(&xs);
    out.extend_from_slice(&ys);
}

/// Rebuild `hmtx` from its transform (WOFF2 section 5.4): left side bearings that equal the
/// glyph's x minimum may be left out and come back from `glyf`.
fn rebuild_hmtx(data: &[u8], metrics: usize, x_mins: &[i16]) -> Result<Vec<u8>, String> {
    let mut s = Stream { d: data, at: 0 };
    let flags = s.u8().ok_or("truncated hmtx transform")?;
    let glyphs = x_mins.len();
    if metrics == 0 || metrics > glyphs {
        return Err("hmtx transform needs glyf and a valid hhea".into());
    }
    let mut advances = Vec::with_capacity(metrics);
    for _ in 0..metrics {
        advances.push(s.u16().ok_or("truncated hmtx transform")?);
    }
    let mut out = Vec::with_capacity(metrics * 4 + (glyphs - metrics) * 2);
    let mut proportional = Vec::with_capacity(metrics);
    for &x_min in &x_mins[..metrics] {
        proportional.push(if flags & 1 == 0 {
            s.i16().ok_or("truncated hmtx transform")?
        } else {
            x_min
        });
    }
    for (advance, lsb) in advances.iter().zip(&proportional) {
        out.extend_from_slice(&advance.to_be_bytes());
        out.extend_from_slice(&lsb.to_be_bytes());
    }
    for &x_min in &x_mins[metrics..] {
        let lsb = if flags & 2 == 0 {
            s.i16().ok_or("truncated hmtx transform")?
        } else {
            x_min
        };
        out.extend_from_slice(&lsb.to_be_bytes());
    }
    Ok(out)
}

/// Bounds-checked big-endian reads at fixed offsets.
struct Reader<'a> {
    d: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Reader<'a> {
        Reader { d }
    }

    fn bytes(&self, at: usize, len: usize) -> Option<&'a [u8]> {
        self.d.get(at..at.checked_add(len)?)
    }

    fn u8(&self, at: usize) -> Option<u8> {
        self.d.get(at).copied()
    }

    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(self.bytes(at, 2)?.try_into().ok()?))
    }

    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(self.bytes(at, 4)?.try_into().ok()?))
    }

    /// UIntBase128: up to five bytes of seven bits, most significant first, no leading zeros.
    fn base128(&self, at: &mut usize) -> Option<u32> {
        let mut value = 0u32;
        for i in 0..5 {
            let byte = self.u8(*at)?;
            *at += 1;
            if i == 0 && byte == 0x80 {
                return None;
            }
            if value & 0xFE00_0000 != 0 {
                return None;
            }
            value = (value << 7) | u32::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
}

/// A sequential reader over one of the glyf transform's streams.
struct Stream<'a> {
    d: &'a [u8],
    at: usize,
}

impl<'a> Stream<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let bytes = self.d.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }

    fn bbox(&mut self) -> Option<[i16; 4]> {
        Some([self.i16()?, self.i16()?, self.i16()?, self.i16()?])
    }

    /// 255UInt16: one byte below 253, or a code byte followed by one or two more.
    fn u255(&mut self) -> Option<u16> {
        match self.u8()? {
            253 => self.u16(),
            254 => self.u8().map(|b| u16::from(b) + 506),
            255 => self.u8().map(|b| u16::from(b) + 253),
            b => Some(u16::from(b)),
        }
    }

    /// One point's (dx, dy) in the triplet encoding (WOFF2 section 5.2, table 2), `flag` without
    /// its on-curve bit.
    fn triplet(&mut self, flag: u8) -> Option<(i32, i32)> {
        let sign = |bit: u8, v: i32| if bit & 1 != 0 { v } else { -v };
        let f = i32::from(flag);
        Some(match flag {
            0..10 => {
                let b = i32::from(self.u8()?);
                (0, sign(flag, ((f & 14) << 7) + b))
            }
            10..20 => {
                let b = i32::from(self.u8()?);
                (sign(flag, (((f - 10) & 14) << 7) + b), 0)
            }
            20..84 => {
                let b0 = f - 20;
                let b1 = i32::from(self.u8()?);
                (
                    sign(flag, 1 + (b0 & 0x30) + (b1 >> 4)),
                    sign(flag >> 1, 1 + ((b0 & 0x0C) << 2) + (b1 & 0x0F)),
                )
            }
            84..120 => {
                let b0 = f - 84;
                let b = self.take(2)?;
                (
                    sign(flag, 1 + ((b0 / 12) << 8) + i32::from(b[0])),
                    sign(flag >> 1, 1 + (((b0 % 12) >> 2) << 8) + i32::from(b[1])),
                )
            }
            120..124 => {
                let b = self.take(3)?;
                let (b0, b1, b2) = (i32::from(b[0]), i32::from(b[1]), i32::from(b[2]));
                (
                    sign(flag, (b0 << 4) + (b1 >> 4)),
                    sign(flag >> 1, ((b1 & 0x0F) << 8) + b2),
                )
            }
            _ => {
                let b = self.take(4)?;
                (
                    sign(flag, (i32::from(b[0]) << 8) + i32::from(b[1])),
                    sign(flag >> 1, (i32::from(b[2]) << 8) + i32::from(b[3])),
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Font;

    const FIGTREE_TTF: &[u8] = include_bytes!("testdata/Figtree-Regular.ttf");
    const FIGTREE_WOFF2: &[u8] = include_bytes!("figtree/Figtree-Regular.woff2");
    const FIGTREE_LATIN_WOFF: &[u8] = include_bytes!("testdata/figtree-latin.woff");
    const ICONS_TTF: &[u8] = include_bytes!("testdata/icons.ttf");
    const ICONS_WOFF2: &[u8] = include_bytes!("testdata/icons.woff2");

    /// Every glyph's advance and outline, and the vertical metrics, must match.
    fn assert_same(a: &Font, b: &Font) {
        assert_eq!(a.glyph_count(), b.glyph_count());
        assert_eq!(
            (a.ascent_em(), a.descent_em(), a.line_gap_em()),
            (b.ascent_em(), b.descent_em(), b.line_gap_em())
        );
        for g in 0..a.glyph_count() as u32 {
            assert_eq!(a.glyph_advance_em(g), b.glyph_advance_em(g), "advance {g}");
            assert_eq!(a.outline(g), b.outline(g), "outline {g}");
        }
    }

    #[test]
    fn figtree_woff2_matches_its_ttf() {
        assert_eq!(container(FIGTREE_WOFF2), Some(Container::Woff2));
        let ttf = Font::parse(FIGTREE_TTF).unwrap();
        let woff2 = Font::parse_any(FIGTREE_WOFF2).unwrap();
        assert!(ttf.glyph_count() > 400 && woff2.has_outlines());
        assert_same(&ttf, &woff2);
        for ch in "Broxee → … × é ß".chars() {
            assert_eq!(ttf.glyph(ch), woff2.glyph(ch), "{ch}");
        }
    }

    #[test]
    fn transformed_hmtx_comes_back() {
        let ttf = Font::parse(ICONS_TTF).unwrap();
        let woff2 = Font::parse_any(ICONS_WOFF2).unwrap();
        assert_same(&ttf, &woff2);
        assert_eq!(woff2.glyph('\u{E001}'), 2);
        assert_eq!(woff2.advance_em('\u{E002}'), 1.0);
        assert_eq!(woff2.advance_em(' '), 0.25);
    }

    #[test]
    fn woff1_matches_the_sfnt() {
        let woff = Font::parse_any(FIGTREE_LATIN_WOFF).unwrap();
        let ttf = Font::parse(FIGTREE_TTF).unwrap();
        for ch in ('!'..='~').chain([' ']) {
            assert_eq!(woff.advance_em(ch), ttf.advance_em(ch), "{ch}");
            let (a, b) = (woff.outline(woff.glyph(ch)), ttf.outline(ttf.glyph(ch)));
            assert_eq!(a, b, "{ch}");
        }
        assert_eq!(woff.glyph('é'), 0, "outside the subset");
    }

    #[test]
    fn damaged_files_fail_without_panicking() {
        for data in [ICONS_WOFF2, FIGTREE_LATIN_WOFF] {
            for len in 0..data.len() {
                let _ = Font::parse_any(&data[..len]);
            }
            // Flip bits through the header, directory and the start of the data.
            for i in 0..data.len().min(400) {
                let mut bad = data.to_vec();
                bad[i] ^= 0x5A;
                let _ = Font::parse_any(&bad);
            }
        }
        assert!(Font::parse_any(b"wOF2").is_err());
        assert!(Font::parse_any(b"GIF89a").is_err());
    }

    #[test]
    fn reads_base128_and_255uint16() {
        let r = Reader::new(&[0x3F, 0x81, 0x00, 0x80, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        let mut at = 0;
        assert_eq!(r.base128(&mut at), Some(63));
        assert_eq!(r.base128(&mut at), Some(128));
        assert_eq!(r.base128(&mut at), None, "leading zero byte");
        let mut at = 5;
        assert_eq!(r.base128(&mut at), None, "overflows 32 bits");
        let mut s = Stream {
            d: &[100, 255, 7, 254, 0, 253, 0x12, 0x34],
            at: 0,
        };
        assert_eq!(
            (s.u255(), s.u255(), s.u255(), s.u255()),
            (Some(100), Some(260), Some(506), Some(0x1234))
        );
    }
}
