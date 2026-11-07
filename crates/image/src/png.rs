//! PNG decoding (ISO/IEC 15948): every color type and bit depth, `tRNS` transparency, and
//! Adam7 interlacing. Ancillary chunks (gamma, color profiles, text) are ignored, as are CRCs.

use crate::{Bitmap, Error, Format, Info, Scaler, MAX_SIDE};

/// Interlaced images are assembled at full size before scaling, so they are capped lower.
const MAX_INTERLACED_PIXELS: u64 = 4 << 20;

struct Header {
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
    interlaced: bool,
}

impl Header {
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }

    /// Bytes per complete pixel for filtering (at least 1).
    fn filter_bpp(&self) -> usize {
        (self.channels() * usize::from(self.depth)).div_ceil(8)
    }

    fn row_bytes(&self, width: u32) -> usize {
        (width as usize * self.channels() * usize::from(self.depth)).div_ceil(8)
    }
}

pub(crate) fn probe(data: &[u8]) -> Option<Info> {
    let ihdr = data.get(8..33)?;
    if &ihdr[4..8] != b"IHDR" {
        return None;
    }
    Some(Info {
        width: u32::from_be_bytes(ihdr[8..12].try_into().ok()?),
        height: u32::from_be_bytes(ihdr[12..16].try_into().ok()?),
        format: Format::Png,
    })
}

pub(crate) fn decode(data: &[u8], target: (u32, u32)) -> Result<Bitmap, Error> {
    let mut header = None;
    let mut palette: &[u8] = &[];
    let mut trns: &[u8] = &[];
    let mut idat = Vec::new();
    let mut at = 8;
    while at + 8 <= data.len() {
        let len =
            u32::from_be_bytes(data[at..at + 4].try_into().map_err(|_| Error::Truncated)?) as usize;
        let kind = &data[at + 4..at + 8];
        let body = data.get(at + 8..at + 8 + len).ok_or(Error::Truncated)?;
        match kind {
            b"IHDR" if body.len() >= 13 => {
                let h = Header {
                    width: u32::from_be_bytes(body[0..4].try_into().map_err(|_| Error::Truncated)?),
                    height: u32::from_be_bytes(
                        body[4..8].try_into().map_err(|_| Error::Truncated)?,
                    ),
                    depth: body[8],
                    color: body[9],
                    interlaced: body[12] == 1,
                };
                let valid = matches!(
                    (h.color, h.depth),
                    (0, 1 | 2 | 4 | 8 | 16) | (3, 1 | 2 | 4 | 8) | (2 | 4 | 6, 8 | 16)
                );
                if !valid || body[10] != 0 || body[11] != 0 || body[12] > 1 {
                    return Err(Error::Corrupt("invalid PNG header"));
                }
                if h.width == 0 || h.height == 0 || h.width > MAX_SIDE || h.height > MAX_SIDE {
                    return Err(Error::TooLarge);
                }
                header = Some(h);
            }
            b"PLTE" => palette = body,
            b"tRNS" => trns = body,
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    let header = header.ok_or(Error::Corrupt("no PNG header"))?;
    if header.color == 3 && palette.is_empty() {
        return Err(Error::Corrupt("palette image without a palette"));
    }
    let convert = Converter {
        header: &header,
        palette,
        trns,
    };
    if header.interlaced {
        decode_interlaced(&header, &idat, &convert, target)
    } else {
        decode_sequential(&header, &idat, &convert, target)
    }
}

/// Rows stream out of the decompressor, are unfiltered against the previous row, converted
/// and scaled; only two rows exist at a time.
fn decode_sequential(
    header: &Header,
    idat: &[u8],
    convert: &Converter,
    target: (u32, u32),
) -> Result<Bitmap, Error> {
    let stride = header.row_bytes(header.width);
    let bpp = header.filter_bpp();
    let mut scaler = Scaler::new(header.width, header.height, target);
    let mut row = Vec::with_capacity(stride + 1);
    let mut previous = vec![0u8; stride];
    let mut rgba = vec![0u8; header.width as usize * 4];
    let mut rows = 0u32;
    let limit = (stride + 1) * header.height as usize;
    flate::zlib_stream(idat, limit, &mut |mut bytes| {
        while !bytes.is_empty() {
            let take = (stride + 1 - row.len()).min(bytes.len());
            row.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if row.len() == stride + 1 {
                unfilter(row[0], &mut row[1..], &previous, bpp)
                    .map_err(|_| flate::Error::Corrupt("PNG filter"))?;
                convert.row(&row[1..], header.width, &mut rgba);
                scaler.push(&rgba);
                previous.copy_from_slice(&row[1..]);
                row.clear();
                rows += 1;
            }
        }
        Ok(())
    })?;
    if rows < header.height {
        return Err(Error::Truncated);
    }
    scaler.finish()
}

const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn decode_interlaced(
    header: &Header,
    idat: &[u8],
    convert: &Converter,
    target: (u32, u32),
) -> Result<Bitmap, Error> {
    let (w, h) = (header.width, header.height);
    if u64::from(w) * u64::from(h) > MAX_INTERLACED_PIXELS {
        return Err(Error::TooLarge);
    }
    let raw = flate::zlib_decompress(idat, interlaced_size(header))?;
    let mut image = vec![0u8; w as usize * h as usize * 4];
    let bpp = header.filter_bpp();
    let mut at = 0;
    for (x0, y0, dx, dy) in ADAM7 {
        let pw = (w + dx - 1 - x0.min(w)) / dx;
        let ph = (h + dy - 1 - y0.min(h)) / dy;
        if x0 >= w || y0 >= h || pw == 0 || ph == 0 {
            continue;
        }
        let stride = header.row_bytes(pw);
        let mut previous = vec![0u8; stride];
        let mut rgba = vec![0u8; pw as usize * 4];
        for r in 0..ph {
            let line = raw.get(at..at + stride + 1).ok_or(Error::Truncated)?;
            at += stride + 1;
            let mut row = line[1..].to_vec();
            unfilter(line[0], &mut row, &previous, bpp)?;
            convert.row(&row, pw, &mut rgba);
            let y = y0 + r * dy;
            for c in 0..pw {
                let x = x0 + c * dx;
                let to = (y as usize * w as usize + x as usize) * 4;
                image[to..to + 4].copy_from_slice(&rgba[c as usize * 4..c as usize * 4 + 4]);
            }
            previous = row;
        }
    }
    let mut scaler = Scaler::new(w, h, target);
    for row in image.chunks_exact(w as usize * 4) {
        scaler.push(row);
    }
    scaler.finish()
}

/// Exact size of the decompressed data of an interlaced image.
fn interlaced_size(header: &Header) -> usize {
    let (w, h) = (header.width, header.height);
    ADAM7
        .iter()
        .map(|&(x0, y0, dx, dy)| {
            if x0 >= w || y0 >= h {
                return 0;
            }
            let pw = (w - x0).div_ceil(dx);
            let ph = (h - y0).div_ceil(dy);
            (header.row_bytes(pw) + 1) * ph as usize
        })
        .sum()
}

/// Undo a row filter in place (section 9). `previous` is the unfiltered row above.
fn unfilter(filter: u8, row: &mut [u8], previous: &[u8], bpp: usize) -> Result<(), Error> {
    match filter {
        0 => {}
        1 => {
            for i in bpp..row.len() {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        2 => {
            for (b, &up) in row.iter_mut().zip(previous) {
                *b = b.wrapping_add(up);
            }
        }
        3 => {
            for i in 0..row.len() {
                let left = if i >= bpp { u16::from(row[i - bpp]) } else { 0 };
                row[i] = row[i].wrapping_add(((left + u16::from(previous[i])) / 2) as u8);
            }
        }
        4 => {
            for i in 0..row.len() {
                let (a, c) = if i >= bpp {
                    (row[i - bpp], previous[i - bpp])
                } else {
                    (0, 0)
                };
                row[i] = row[i].wrapping_add(paeth(a, previous[i], c));
            }
        }
        _ => return Err(Error::Corrupt("unknown PNG filter")),
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Turns unfiltered rows of any color type and depth into straight-alpha RGBA.
struct Converter<'a> {
    header: &'a Header,
    palette: &'a [u8],
    trns: &'a [u8],
}

impl Converter<'_> {
    fn row(&self, row: &[u8], width: u32, out: &mut [u8]) {
        let h = self.header;
        let depth = usize::from(h.depth);
        // Sample `i` of the row, at its original depth.
        let sample = |i: usize| -> u16 {
            match depth {
                16 => u16::from_be_bytes([row[i * 2], row[i * 2 + 1]]),
                8 => u16::from(row[i]),
                _ => {
                    let bit = i * depth;
                    let byte = row[bit / 8];
                    u16::from((byte >> (8 - depth - bit % 8)) & ((1 << depth) - 1) as u8)
                }
            }
        };
        let to8 = |v: u16| -> u8 {
            match depth {
                16 => (v >> 8) as u8,
                8 => v as u8,
                _ => (u32::from(v) * 255 / ((1u32 << depth) - 1)) as u8,
            }
        };
        let key = |i: usize| -> Option<u16> {
            self.trns
                .get(i * 2..i * 2 + 2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
        };
        for x in 0..width as usize {
            let px = &mut out[x * 4..x * 4 + 4];
            match h.color {
                0 => {
                    let v = sample(x);
                    let g = to8(v);
                    let a = if key(0) == Some(v) { 0 } else { 255 };
                    px.copy_from_slice(&[g, g, g, a]);
                }
                2 => {
                    let (r, g, b) = (sample(x * 3), sample(x * 3 + 1), sample(x * 3 + 2));
                    let keyed = key(0) == Some(r) && key(1) == Some(g) && key(2) == Some(b);
                    px.copy_from_slice(&[to8(r), to8(g), to8(b), if keyed { 0 } else { 255 }]);
                }
                3 => {
                    let i = usize::from(sample(x));
                    let rgb = self.palette.get(i * 3..i * 3 + 3).unwrap_or(&[0, 0, 0]);
                    let a = self.trns.get(i).copied().unwrap_or(255);
                    px.copy_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
                }
                4 => {
                    let g = to8(sample(x * 2));
                    px.copy_from_slice(&[g, g, g, to8(sample(x * 2 + 1))]);
                }
                _ => {
                    for (c, out) in px.iter_mut().enumerate() {
                        *out = to8(sample(x * 4 + c));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 13;
    const H: u32 = 9;

    /// The pattern the fixtures were generated from (see the generator in the commit that
    /// added them): straight-alpha RGBA.
    fn reference(x: u32, y: u32) -> [u32; 4] {
        [
            (x * 19 + y * 7) % 256,
            (x * 5 + y * 29) % 256,
            (x * y * 3 + 40) % 256,
            if !(x + y).is_multiple_of(4) {
                255
            } else {
                (x * 20) % 256
            },
        ]
    }

    fn at_full_size(data: &[u8]) -> Bitmap {
        let b = decode(data, (W, H)).unwrap();
        assert_eq!((b.width, b.height), (W, H));
        b
    }

    fn premultiplied(b: &Bitmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * W + x) * 4) as usize;
        b.pixels[i..i + 4].try_into().unwrap()
    }

    fn premultiply(r: u32, g: u32, b: u32, a: u32) -> [u8; 4] {
        // The scaler's rounding: one sample, so the sum divides by 255 with +127.
        let p = |c: u32| ((c * a + 127) / 255) as u8;
        [p(r), p(g), p(b), a as u8]
    }

    #[test]
    fn decodes_truecolor_with_alpha_at_8_and_16_bits() {
        for name in ["rgba8", "rgba16", "rgba8_interlaced"] {
            let data = match name {
                "rgba8" => &include_bytes!("testdata/rgba8.png")[..],
                "rgba16" => &include_bytes!("testdata/rgba16.png")[..],
                _ => &include_bytes!("testdata/rgba8_interlaced.png")[..],
            };
            let b = at_full_size(data);
            for y in 0..H {
                for x in 0..W {
                    let [r, g, bl, a] = reference(x, y);
                    // 16-bit fixtures scale samples by 65535/255 and back, losing nothing.
                    assert_eq!(
                        premultiplied(&b, x, y),
                        premultiply(r, g, bl, a),
                        "{name} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn decodes_gray_at_every_depth() {
        let files: [(&[u8], u32); 6] = [
            (include_bytes!("testdata/gray1.png"), 1),
            (include_bytes!("testdata/gray2.png"), 2),
            (include_bytes!("testdata/gray4.png"), 4),
            (include_bytes!("testdata/gray8.png"), 8),
            (include_bytes!("testdata/gray16.png"), 16),
            (include_bytes!("testdata/gray4_interlaced.png"), 4),
        ];
        for (data, depth) in files {
            let b = at_full_size(data);
            let max = (1u32 << depth) - 1;
            for y in 0..H {
                for x in 0..W {
                    let g = reference(x, y)[1];
                    let expected = if depth == 16 {
                        (g * max / 255) >> 8
                    } else {
                        g * max / 255 * 255 / max
                    } as u8;
                    assert_eq!(
                        premultiplied(&b, x, y),
                        [expected, expected, expected, 255],
                        "depth {depth} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn decodes_palettes_with_transparency() {
        let files: [(&[u8], u32); 4] = [
            (include_bytes!("testdata/pal1.png"), 1),
            (include_bytes!("testdata/pal2.png"), 2),
            (include_bytes!("testdata/pal4.png"), 4),
            (include_bytes!("testdata/pal8.png"), 8),
        ];
        for (data, depth) in files {
            let b = at_full_size(data);
            for y in 0..H {
                for x in 0..W {
                    let i = if depth < 8 {
                        (x + y * W) % (1 << depth)
                    } else {
                        (x * 7 + y) % 200
                    };
                    let rgb = [(i * 3) % 256, (i * 7) % 256, (255 - i) % 256];
                    let alpha = [0, 128, 255].get(i as usize).copied().unwrap_or(255);
                    assert_eq!(
                        premultiplied(&b, x, y),
                        premultiply(rgb[0], rgb[1], rgb[2], alpha),
                        "depth {depth} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn decodes_gray_alpha_and_color_keys() {
        let b = at_full_size(include_bytes!("testdata/graya8.png"));
        let [_, g, _, a] = reference(3, 0);
        assert_eq!(premultiplied(&b, 3, 0), premultiply(g, g, g, a));
        let keyed = at_full_size(include_bytes!("testdata/rgb8_trns.png"));
        assert_eq!(
            premultiplied(&keyed, 1, 0),
            [0, 0, 0, 0],
            "the tRNS color is transparent"
        );
        assert_eq!(premultiplied(&keyed, 2, 0)[3], 255);
        let rgb16 = at_full_size(include_bytes!("testdata/rgb16.png"));
        let [r, g, bl, _] = reference(5, 5);
        assert_eq!(
            premultiplied(&rgb16, 5, 5),
            [r as u8, g as u8, bl as u8, 255]
        );
    }

    #[test]
    fn scales_while_decoding_and_survives_damage() {
        let data = include_bytes!("testdata/rgba8.png");
        let small = decode(data, (4, 3)).unwrap();
        assert_eq!((small.width, small.height, small.pixels.len()), (4, 3, 48));
        // Cuts inside the header or the image data; a file missing only IEND still decodes.
        for cut in [0, 8, 20, 40, data.len() - 60, data.len() - 30] {
            assert!(decode(&data[..cut], (W, H)).is_err(), "cut at {cut}");
        }
        assert!(decode(&data[..data.len() - 12], (W, H)).is_ok(), "no IEND");
        for i in 33..data.len() {
            let mut damaged = data.to_vec();
            damaged[i] ^= 0x5a;
            let _ = decode(&damaged, (W, H));
        }
    }
}
