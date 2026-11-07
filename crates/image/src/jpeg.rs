//! Baseline JPEG decoding (ITU T.81): Huffman coding, 8-bit samples, gray or three-component
//! images with any sampling factors, restart intervals, and Adobe's RGB marker.
//!
//! The image is decoded one row of MCUs at a time: blocks go through the inverse DCT into a
//! strip a few block rows tall, which is color converted and handed to the scaler.
//!
//! Progressive files are decoded in `progressive.rs`.
//!
//! Not yet: arithmetic-coded and differential files, 12-bit samples, CMYK, non-interleaved
//! scans of baseline color images, EXIF orientation.

use crate::progressive::{Progressive, Scan};
use crate::{Bitmap, Error, Format, Info, Scaler, MAX_SIDE};

pub(crate) fn probe(data: &[u8]) -> Option<Info> {
    let mut at = 2;
    while at + 4 <= data.len() {
        if data[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = data[at + 1];
        match marker {
            0xff => at += 1,
            0x01 | 0xd0..=0xd8 => at += 2,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                let d = data.get(at + 5..at + 9)?;
                return Some(Info {
                    height: u32::from(u16::from_be_bytes([d[0], d[1]])),
                    width: u32::from(u16::from_be_bytes([d[2], d[3]])),
                    format: Format::Jpeg,
                });
            }
            _ => at += 2 + usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]])),
        }
    }
    None
}

pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone, Copy, Default)]
pub(crate) struct Component {
    pub(crate) id: u8,
    pub(crate) h: usize,
    pub(crate) v: usize,
    pub(crate) quant: usize,
    dc: usize,
    ac: usize,
    pred: i32,
}

pub(crate) fn decode(data: &[u8], target: (u32, u32)) -> Result<Bitmap, Error> {
    let mut quant = [[0u16; 64]; 4];
    let mut dc_tables: [Option<Huffman>; 4] = Default::default();
    let mut ac_tables: [Option<Huffman>; 4] = Default::default();
    let mut components: Vec<Component> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);
    let mut restart_interval = 0usize;
    let mut adobe_transform: Option<u8> = None;
    let mut progressive_frame = false;
    let mut progressive: Option<Progressive> = None;
    let is_rgb = |adobe: Option<u8>, components: &[Component]| {
        adobe == Some(0) || (components.len() == 3 && components.iter().map(|c| c.id).eq(*b"RGB"))
    };

    if !data.starts_with(&[0xff, 0xd8]) {
        return Err(Error::Corrupt("not a JPEG"));
    }
    let mut at = 2;
    loop {
        while data.get(at) == Some(&0xff) && data.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        let (Some(&ff), Some(&marker)) = (data.get(at), data.get(at + 1)) else {
            // A progressive file cut short still shows the scans that arrived.
            return match progressive {
                Some(p) => p.finish(&quant, is_rgb(adobe_transform, &components), target),
                None => Err(Error::Truncated),
            };
        };
        if ff != 0xff {
            return Err(Error::Corrupt("expected a marker"));
        }
        if matches!(marker, 0x01 | 0xd0..=0xd7) {
            at += 2;
            continue;
        }
        if marker == 0xd9 {
            return match progressive {
                Some(p) => p.finish(&quant, is_rgb(adobe_transform, &components), target),
                None => Err(Error::Corrupt("no image data")),
            };
        }
        let len = usize::from(u16::from_be_bytes([
            *data.get(at + 2).ok_or(Error::Truncated)?,
            *data.get(at + 3).ok_or(Error::Truncated)?,
        ]));
        let seg = data.get(at + 4..at + 2 + len).ok_or(Error::Truncated)?;
        match marker {
            0xdb => {
                let mut s = seg;
                while !s.is_empty() {
                    let (precision, id) = (s[0] >> 4, usize::from(s[0] & 3));
                    let size = if precision == 0 { 64 } else { 128 };
                    let table = s.get(1..1 + size).ok_or(Error::Truncated)?;
                    for k in 0..64 {
                        quant[id][k] = if precision == 0 {
                            u16::from(table[k])
                        } else {
                            u16::from_be_bytes([table[k * 2], table[k * 2 + 1]])
                        };
                    }
                    s = &s[1 + size..];
                }
            }
            0xc4 => {
                let mut s = seg;
                while !s.is_empty() {
                    let (class, id) = (s[0] >> 4, usize::from(s[0] & 3));
                    let counts: [u8; 16] = s
                        .get(1..17)
                        .ok_or(Error::Truncated)?
                        .try_into()
                        .map_err(|_| Error::Truncated)?;
                    let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
                    let values = s.get(17..17 + total).ok_or(Error::Truncated)?;
                    let table = Huffman::new(&counts, values)?;
                    if class == 0 {
                        dc_tables[id] = Some(table);
                    } else {
                        ac_tables[id] = Some(table);
                    }
                    s = &s[17 + total..];
                }
            }
            0xc0..=0xc2 => {
                progressive_frame = marker == 0xc2;
                if seg.len() < 6 || seg[0] != 8 {
                    return Err(Error::Unsupported(
                        "JPEG sample precision other than 8 bits",
                    ));
                }
                height = usize::from(u16::from_be_bytes([seg[1], seg[2]]));
                width = usize::from(u16::from_be_bytes([seg[3], seg[4]]));
                let n = usize::from(seg[5]);
                if n != 1 && n != 3 {
                    return Err(Error::Unsupported("JPEG with other than 1 or 3 components"));
                }
                components.clear();
                for i in 0..n {
                    let c = seg.get(6 + i * 3..9 + i * 3).ok_or(Error::Truncated)?;
                    let (h, v) = (usize::from(c[1] >> 4), usize::from(c[1] & 15));
                    if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                        return Err(Error::Corrupt("sampling factor"));
                    }
                    components.push(Component {
                        id: c[0],
                        h,
                        v,
                        quant: usize::from(c[2] & 3),
                        ..Component::default()
                    });
                }
            }
            0xc6 | 0xca | 0xce => return Err(Error::Unsupported("differential JPEG")),
            0xc3 | 0xc5 | 0xc7 | 0xc9 | 0xcb | 0xcd | 0xcf => {
                return Err(Error::Unsupported(
                    "lossless, hierarchical or arithmetic JPEG",
                ))
            }
            0xdd => {
                restart_interval = usize::from(u16::from_be_bytes([
                    *seg.first().ok_or(Error::Truncated)?,
                    *seg.get(1).ok_or(Error::Truncated)?,
                ]));
            }
            0xee if seg.starts_with(b"Adobe") && seg.len() >= 12 => adobe_transform = Some(seg[11]),
            0xda if progressive_frame => {
                if components.is_empty() {
                    return Err(Error::Corrupt("scan before frame"));
                }
                let n = usize::from(*seg.first().ok_or(Error::Truncated)?);
                let mut scan = Scan {
                    components: Vec::with_capacity(n),
                    dc_tables: Vec::with_capacity(n),
                    ac_tables: Vec::with_capacity(n),
                    start: 0,
                    end: 0,
                    high: 0,
                    low: 0,
                };
                for i in 0..n {
                    let s = seg.get(1 + i * 2..3 + i * 2).ok_or(Error::Truncated)?;
                    let c = components
                        .iter()
                        .position(|c| c.id == s[0])
                        .ok_or(Error::Corrupt("scan names an unknown component"))?;
                    scan.components.push(c);
                    scan.dc_tables.push(usize::from(s[1] >> 4) & 3);
                    scan.ac_tables.push(usize::from(s[1] & 15) & 3);
                }
                let tail = seg.get(1 + n * 2..4 + n * 2).ok_or(Error::Truncated)?;
                scan.start = usize::from(tail[0]);
                scan.end = usize::from(tail[1]);
                scan.high = u32::from(tail[2] >> 4);
                scan.low = u32::from(tail[2] & 15);
                if scan.low > 13 || scan.high > 13 {
                    return Err(Error::Corrupt("successive approximation"));
                }
                if progressive.is_none() {
                    if width == 0
                        || height == 0
                        || width > MAX_SIDE as usize
                        || height > MAX_SIDE as usize
                    {
                        return Err(Error::TooLarge);
                    }
                    progressive =
                        Some(Progressive::new(width, height, components.clone(), target)?);
                }
                let p = progressive.as_mut().ok_or(Error::Corrupt("no frame"))?;
                let entropy = at + 2 + len;
                let used = p.decode_scan(
                    &data[entropy..],
                    &scan,
                    &dc_tables,
                    &ac_tables,
                    restart_interval,
                )?;
                at = entropy + used;
                continue;
            }
            0xda => {
                let n = usize::from(*seg.first().ok_or(Error::Truncated)?);
                if components.is_empty() {
                    return Err(Error::Corrupt("scan before frame"));
                }
                if n != components.len() {
                    return Err(Error::Unsupported("non-interleaved color JPEG"));
                }
                for i in 0..n {
                    let s = seg.get(1 + i * 2..3 + i * 2).ok_or(Error::Truncated)?;
                    let c = components
                        .iter_mut()
                        .find(|c| c.id == s[0])
                        .ok_or(Error::Corrupt("scan names an unknown component"))?;
                    c.dc = usize::from(s[1] >> 4) & 3;
                    c.ac = usize::from(s[1] & 15) & 3;
                }
                let tables = Tables {
                    quant: &quant,
                    dc: &dc_tables,
                    ac: &ac_tables,
                };
                let rgb = is_rgb(adobe_transform, &components);
                let frame = Frame {
                    width,
                    height,
                    components,
                    restart_interval,
                    rgb,
                };
                return decode_scan(&data[at + 2 + len..], frame, tables, target);
            }
            _ => {}
        }
        at += 2 + len;
    }
}

pub(crate) struct Frame {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) components: Vec<Component>,
    pub(crate) restart_interval: usize,
    /// Samples are R, G, B rather than Y, Cb, Cr.
    pub(crate) rgb: bool,
}

struct Tables<'a> {
    quant: &'a [[u16; 64]; 4],
    dc: &'a [Option<Huffman>; 4],
    ac: &'a [Option<Huffman>; 4],
}

fn decode_scan(
    entropy: &[u8],
    mut frame: Frame,
    tables: Tables,
    target: (u32, u32),
) -> Result<Bitmap, Error> {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 || w > MAX_SIDE as usize || h > MAX_SIDE as usize {
        return Err(Error::TooLarge);
    }
    // A single-component scan is not interleaved: one block per MCU, whatever its factors.
    if frame.components.len() == 1 {
        frame.components[0].h = 1;
        frame.components[0].v = 1;
    }
    let hmax = frame.components.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = frame.components.iter().map(|c| c.v).max().unwrap_or(1);
    let blocks_per_mcu: usize = frame.components.iter().map(|c| c.h * c.v).sum();
    if blocks_per_mcu > 10 {
        return Err(Error::Corrupt("too many blocks per MCU"));
    }
    let mcus_x = w.div_ceil(8 * hmax);
    let mcus_y = h.div_ceil(8 * vmax);
    for c in &frame.components {
        if tables.dc[c.dc].is_none() || tables.ac[c.ac].is_none() {
            return Err(Error::Corrupt("missing Huffman table"));
        }
    }

    // Decode straight to k/8 of the size when the image is drawn smaller: every block is still
    // read, but only its k x k lowest frequencies go through the inverse DCT.
    let k = luma_scale(w, h, target);
    let scales: Vec<usize> = frame
        .components
        .iter()
        .map(|c| plane_scale(k, hmax, vmax, c))
        .collect();
    let (out_w, out_h) = ((w * k).div_ceil(8), (h * k).div_ceil(8));
    let out = Frame {
        width: out_w,
        height: out_h,
        components: frame
            .components
            .iter()
            .zip(&scales)
            .map(|(c, &s)| {
                let mut scaled = *c;
                scaled.h = c.h * s;
                scaled.v = c.v * s;
                scaled
            })
            .collect(),
        restart_interval: 0,
        rgb: frame.rgb,
    };
    // One strip per component, one MCU row tall.
    let strides: Vec<usize> = frame
        .components
        .iter()
        .zip(&scales)
        .map(|(c, &s)| mcus_x * c.h * s)
        .collect();
    let mut strips: Vec<Vec<u8>> = frame
        .components
        .iter()
        .zip(&scales)
        .zip(&strides)
        .map(|((c, &s), &stride)| vec![0u8; stride * c.v * s])
        .collect();
    let mut scaler = Scaler::new(out_w as u32, out_h as u32, target);
    let mut rgba = vec![0u8; out_w * 4];
    let mut reduced = [0f32; 64];
    let mut bits = BitReader::new(entropy);
    let mut coefs = [0i32; 64];
    let mut mcu = 0usize;

    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if frame.restart_interval > 0 && mcu > 0 && mcu.is_multiple_of(frame.restart_interval) {
                bits.restart();
                for c in frame.components.iter_mut() {
                    c.pred = 0;
                }
            }
            for (ci, c) in frame.components.iter_mut().enumerate() {
                let dc = tables.dc[c.dc]
                    .as_ref()
                    .ok_or(Error::Corrupt("missing table"))?;
                let ac = tables.ac[c.ac]
                    .as_ref()
                    .ok_or(Error::Corrupt("missing table"))?;
                let q = &tables.quant[c.quant];
                for by in 0..c.v {
                    for bx in 0..c.h {
                        let only_dc = decode_block(&mut bits, dc, ac, &mut c.pred, q, &mut coefs)?;
                        let (stride, s) = (strides[ci], scales[ci]);
                        let offset = by * s * stride + (mx * c.h + bx) * s;
                        if s == 8 || only_dc {
                            idct_block(&coefs, only_dc, s, &mut strips[ci][offset..], stride);
                        } else {
                            for v in 0..s {
                                for u in 0..s {
                                    reduced[v * s + u] = coefs[v * 8 + u] as f32;
                                }
                            }
                            idct_scaled(&reduced[..s * s], s, &mut strips[ci][offset..], stride);
                        }
                    }
                }
            }
            mcu += 1;
        }
        let rows = (k * vmax).min(out_h.saturating_sub(my * k * vmax));
        for r in 0..rows {
            color_row(&out, &strips, &strides, r, hmax * k, vmax * k, &mut rgba);
            scaler.push(&rgba);
        }
    }
    scaler.finish()
}

/// Convert row `r` of the current MCU strip to RGBA, replicating subsampled chroma.
#[allow(clippy::too_many_arguments)]
pub(crate) fn color_row(
    frame: &Frame,
    strips: &[Vec<u8>],
    strides: &[usize],
    r: usize,
    hmax: usize,
    vmax: usize,
    out: &mut [u8],
) {
    let sample = |ci: usize, x: usize| -> i32 {
        let c = &frame.components[ci];
        let sx = x * c.h / hmax;
        let sy = r * c.v / vmax;
        i32::from(strips[ci][sy * strides[ci] + sx])
    };
    for x in 0..frame.width {
        let px = &mut out[x * 4..x * 4 + 4];
        if frame.components.len() == 1 {
            let y = sample(0, x) as u8;
            px.copy_from_slice(&[y, y, y, 255]);
            continue;
        }
        let (a, b, c) = (sample(0, x), sample(1, x), sample(2, x));
        let (red, green, blue) = if frame.rgb {
            (a, b, c)
        } else {
            let (cb, cr) = (b - 128, c - 128);
            (
                a + ((91_881 * cr) >> 16),
                a - ((22_554 * cb + 46_802 * cr) >> 16),
                a + ((116_130 * cb) >> 16),
            )
        };
        px.copy_from_slice(&[
            red.clamp(0, 255) as u8,
            green.clamp(0, 255) as u8,
            blue.clamp(0, 255) as u8,
            255,
        ]);
    }
}

/// Decode one 8x8 block into dequantized coefficients in natural order. Returns true when only
/// the DC coefficient is non-zero.
fn decode_block(
    bits: &mut BitReader,
    dc: &Huffman,
    ac: &Huffman,
    pred: &mut i32,
    q: &[u16; 64],
    out: &mut [i32; 64],
) -> Result<bool, Error> {
    out.fill(0);
    let t = u32::from(dc.decode(bits)?);
    if t > 11 {
        return Err(Error::Corrupt("DC magnitude"));
    }
    let diff = if t == 0 { 0 } else { extend(bits.take(t), t) };
    *pred = pred.wrapping_add(diff);
    out[0] = pred.wrapping_mul(i32::from(q[0]));
    let mut only_dc = true;
    let mut k = 1;
    while k < 64 {
        let rs = ac.decode(bits)?;
        let (run, size) = (usize::from(rs >> 4), u32::from(rs & 15));
        if size == 0 {
            if run == 15 {
                k += 16;
                continue;
            }
            break; // end of block
        }
        k += run;
        if k > 63 {
            return Err(Error::Corrupt("AC coefficient index"));
        }
        out[ZIGZAG[k]] = extend(bits.take(size), size).wrapping_mul(i32::from(q[k]));
        only_dc = false;
        k += 1;
    }
    Ok(only_dc)
}

/// A `size`-bit magnitude category value as a signed coefficient (F.2.2.1).
pub(crate) fn extend(v: u32, size: u32) -> i32 {
    let v = v as i32;
    if v < 1 << (size - 1) {
        v - (1 << size) + 1
    } else {
        v
    }
}

/// `COS[x][u] = C(u) cos((2x + 1) u pi / 16) / 2`.
static COS: std::sync::LazyLock<[[f32; 8]; 8]> = std::sync::LazyLock::new(|| {
    let mut t = [[0f32; 8]; 8];
    for (x, row) in t.iter_mut().enumerate() {
        for (u, v) in row.iter_mut().enumerate() {
            let c = if u == 0 {
                std::f32::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            *v = c * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos() / 2.0;
        }
    }
    t
});

/// Inverse DCT of one block at scale `s` (8 is full size). A block with only a DC term is
/// flat at any scale.
fn idct_block(coefs: &[i32; 64], only_dc: bool, s: usize, out: &mut [u8], stride: usize) {
    if only_dc {
        let v = (coefs[0] as f32 / 8.0 + 128.0).round().clamp(0.0, 255.0) as u8;
        for row in 0..s {
            out[row * stride..row * stride + s].fill(v);
        }
        return;
    }
    idct(coefs, false, out, stride);
}

/// Inverse DCT of one block into 8 rows of `out` spaced `stride` apart, level-shifted by 128.
fn idct(coefs: &[i32; 64], only_dc: bool, out: &mut [u8], stride: usize) {
    if only_dc {
        let v = (coefs[0] as f32 / 8.0 + 128.0).round().clamp(0.0, 255.0) as u8;
        for row in 0..8 {
            out[row * stride..row * stride + 8].fill(v);
        }
        return;
    }
    let cos = &*COS;
    let mut tmp = [0f32; 64];
    for y in 0..8 {
        let input = &coefs[y * 8..y * 8 + 8];
        if input[1..].iter().all(|&c| c == 0) {
            let v = input[0] as f32 * cos[0][0];
            tmp[y * 8..y * 8 + 8].fill(v);
            continue;
        }
        for x in 0..8 {
            tmp[y * 8 + x] = (0..8).map(|u| cos[x][u] * input[u] as f32).sum();
        }
    }
    for x in 0..8 {
        for y in 0..8 {
            let v: f32 = (0..8).map(|v| cos[y][v] * tmp[v * 8 + x]).sum();
            out[y * stride + x] = (v + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Coefficients per side to keep for full-resolution planes: the smallest k (1, 2, 4 or 8)
/// whose k/8-scaled image still covers `target`. Images drawn smaller than they are decode
/// with a k-point inverse DCT instead of the full one plus downscaling.
pub(crate) fn luma_scale(width: usize, height: usize, target: (u32, u32)) -> usize {
    [1usize, 2, 4, 8]
        .into_iter()
        .find(|&k| {
            (width * k).div_ceil(8) >= target.0 as usize
                && (height * k).div_ceil(8) >= target.1 as usize
        })
        .unwrap_or(8)
}

/// The scale of one component: subsampled chroma uses a larger one (as libjpeg does), so it
/// comes out at the luma resolution with no upsampling.
pub(crate) fn plane_scale(k: usize, hmax: usize, vmax: usize, c: &Component) -> usize {
    let wanted = (k * hmax / c.h).min(k * vmax / c.v).min(8);
    [8usize, 4, 2, 1]
        .into_iter()
        .find(|&p| p <= wanted)
        .unwrap_or(1)
}

/// `C(u) cos((2x + 1) u pi / 2k) / 2` for each k: the k-point inverse DCT of a block's lowest
/// k x k frequencies, which equals the 8-point one averaged over (8/k)^2 pixels.
static COS_SCALED: std::sync::LazyLock<[Vec<f32>; 4]> = std::sync::LazyLock::new(|| {
    [1usize, 2, 4, 8].map(|k| {
        let mut t = vec![0f32; k * k];
        for x in 0..k {
            for u in 0..k {
                let c = if u == 0 {
                    std::f32::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                t[x * k + u] = c
                    * (((2 * x + 1) * u) as f32 * std::f32::consts::PI / (2 * k) as f32).cos()
                    / 2.0;
            }
        }
        t
    })
});

pub(crate) fn idct_scaled(coefs: &[f32], k: usize, out: &mut [u8], stride: usize) {
    let table = &COS_SCALED[k.trailing_zeros() as usize];
    let mut tmp = [0f32; 64];
    for y in 0..k {
        for x in 0..k {
            tmp[y * k + x] = (0..k).map(|u| table[x * k + u] * coefs[y * k + u]).sum();
        }
    }
    for x in 0..k {
        for y in 0..k {
            let v: f32 = (0..k).map(|v| table[y * k + v] * tmp[v * k + x]).sum();
            out[y * stride + x] = (v + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Entropy-coded data, most significant bit first, with 0xFF00 unstuffed. A marker ends the
/// data: zeros are read past it until a restart moves over it.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader {
            data,
            pos: 0,
            buf: 0,
            count: 0,
        }
    }

    #[inline]
    fn fill(&mut self) {
        while self.count <= 56 {
            let mut byte = 0u8;
            if let Some(&b) = self.data.get(self.pos) {
                if b == 0xff {
                    // Anything but a stuffed zero is a marker: stop here and feed zeros.
                    if self.data.get(self.pos + 1) == Some(&0x00) {
                        byte = 0xff;
                        self.pos += 2;
                    }
                } else {
                    byte = b;
                    self.pos += 1;
                }
            }
            self.buf |= u64::from(byte) << (56 - self.count);
            self.count += 8;
        }
    }

    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.fill();
        }
        (self.buf >> (64 - n)) as u32
    }

    #[inline]
    pub(crate) fn take(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.buf <<= n;
        self.count -= n;
        v
    }

    /// Bytes consumed so far: where the next marker begins once the scan has ended.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Drop buffered bits and step over the restart marker.
    pub(crate) fn restart(&mut self) {
        self.buf = 0;
        self.count = 0;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xff && (0xd0..=0xd7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }
}

const FAST: u32 = 9;

pub(crate) struct Huffman {
    /// `value << 5 | length` for codes up to `FAST` bits; 0 means "take the slow path".
    fast: Vec<u16>,
    max_code: [i32; 17],
    min_code: [i32; 17],
    first_value: [usize; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: &[u8]) -> Result<Huffman, Error> {
        let mut fast = vec![0u16; 1 << FAST];
        let mut max_code = [-1i32; 17];
        let mut min_code = [0i32; 17];
        let mut first_value = [0usize; 17];
        let (mut code, mut k) = (0i32, 0usize);
        for len in 1..=16usize {
            let n = usize::from(counts[len - 1]);
            first_value[len] = k;
            min_code[len] = code;
            for _ in 0..n {
                if code >= 1 << len {
                    return Err(Error::Corrupt("Huffman table"));
                }
                if len as u32 <= FAST {
                    let shift = FAST - len as u32;
                    let entry = u16::from(values[k]) << 5 | len as u16;
                    for j in 0..(1u32 << shift) {
                        fast[((code as u32) << shift | j) as usize] = entry;
                    }
                }
                code += 1;
                k += 1;
            }
            if n > 0 {
                max_code[len] = code - 1;
            }
            code <<= 1;
        }
        Ok(Huffman {
            fast,
            max_code,
            min_code,
            first_value,
            values: values.to_vec(),
        })
    }

    #[inline]
    pub(crate) fn decode(&self, bits: &mut BitReader) -> Result<u8, Error> {
        let entry = self.fast[bits.peek(FAST) as usize];
        if entry != 0 {
            bits.take(u32::from(entry & 31));
            return Ok((entry >> 5) as u8);
        }
        let mut code = 0i32;
        for len in 1..=16 {
            code = code << 1 | bits.take(1) as i32;
            if code <= self.max_code[len] {
                let i = self.first_value[len] + (code - self.min_code[len]) as usize;
                return self
                    .values
                    .get(i)
                    .copied()
                    .ok_or(Error::Corrupt("Huffman code"));
            }
        }
        Err(Error::Corrupt("Huffman code"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a binary PPM (P6) or PGM (P5) from djpeg into RGB triples.
    fn reference(pnm: &[u8]) -> (usize, usize, Vec<[u8; 3]>) {
        let text = String::from_utf8_lossy(&pnm[..20]);
        let mut fields = text.split_ascii_whitespace();
        let magic = fields.next().unwrap();
        let w: usize = fields.next().unwrap().parse().unwrap();
        let h: usize = fields.next().unwrap().parse().unwrap();
        let _max = fields.next().unwrap();
        // The header is four whitespace-separated fields followed by one whitespace byte.
        let mut seen = 0;
        let mut start = 0;
        let mut in_field = false;
        for (i, &b) in pnm.iter().enumerate() {
            if b.is_ascii_whitespace() {
                if in_field {
                    seen += 1;
                    in_field = false;
                    if seen == 4 {
                        start = i + 1;
                        break;
                    }
                }
            } else {
                in_field = true;
            }
        }
        let body = &pnm[start..];
        let pixels = if magic == "P6" {
            body.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect()
        } else {
            body.iter().map(|&g| [g, g, g]).collect()
        };
        (w, h, pixels)
    }

    fn compare(jpeg: &[u8], pnm: &[u8], name: &str) {
        let (w, h, expected) = reference(pnm);
        let b = decode(jpeg, (w as u32, h as u32)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((b.width as usize, b.height as usize), (w, h));
        let mut total = 0u64;
        let mut worst = 0u8;
        for (i, e) in expected.iter().enumerate() {
            for (c, &want) in e.iter().enumerate() {
                let d = b.pixels[i * 4 + c].abs_diff(want);
                total += u64::from(d);
                worst = worst.max(d);
            }
            assert_eq!(b.pixels[i * 4 + 3], 255);
        }
        let mean = total as f64 / (expected.len() * 3) as f64;
        // libjpeg's float IDCT and ours differ only in rounding.
        assert!(
            mean < 1.0 && worst <= 6,
            "{name}: mean {mean:.2}, worst {worst}"
        );
    }

    #[test]
    fn matches_libjpeg_for_common_layouts() {
        compare(
            include_bytes!("testdata/j420.jpg"),
            include_bytes!("testdata/j420.ref.pnm"),
            "4:2:0",
        );
        compare(
            include_bytes!("testdata/j422.jpg"),
            include_bytes!("testdata/j422.ref.pnm"),
            "4:2:2",
        );
        compare(
            include_bytes!("testdata/j444.jpg"),
            include_bytes!("testdata/j444.ref.pnm"),
            "4:4:4",
        );
        compare(
            include_bytes!("testdata/jgray.jpg"),
            include_bytes!("testdata/jgray.ref.pnm"),
            "gray",
        );
        compare(
            include_bytes!("testdata/jrst.jpg"),
            include_bytes!("testdata/jrst.ref.pnm"),
            "restarts",
        );
    }

    #[test]
    fn decodes_progressive_files_like_libjpeg() {
        compare(
            include_bytes!("testdata/jprog.jpg"),
            include_bytes!("testdata/jprog.ref.pnm"),
            "progressive 4:2:0",
        );
        compare(
            include_bytes!("testdata/jprog444.jpg"),
            include_bytes!("testdata/jprog444.ref.pnm"),
            "progressive 4:4:4",
        );
        compare(
            include_bytes!("testdata/jproggray.jpg"),
            include_bytes!("testdata/jproggray.ref.pnm"),
            "progressive gray",
        );
        compare(
            include_bytes!("testdata/jprogrst.jpg"),
            include_bytes!("testdata/jprogrst.ref.pnm"),
            "progressive restarts",
        );
    }

    #[test]
    fn keeps_only_the_frequencies_a_smaller_target_needs() {
        // 96x80 drawn at 48x40 keeps 4x4 coefficients, at 24x20 2x2: compared with libjpeg's
        // scaled decoding (`djpeg -scale 1/2`, `1/4`).
        let data = include_bytes!("testdata/jprog96.jpg");
        compare(data, include_bytes!("testdata/jprog96.ref.pnm"), "full");
        compare(
            data,
            include_bytes!("testdata/jprog96_half.ref.pnm"),
            "half",
        );
        compare(
            data,
            include_bytes!("testdata/jprog96_quarter.ref.pnm"),
            "quarter",
        );
    }

    #[test]
    fn baseline_files_drawn_smaller_use_the_scaled_transform_too() {
        let data = include_bytes!("testdata/jbase96.jpg");
        compare(
            data,
            include_bytes!("testdata/jbase96_half.ref.pnm"),
            "baseline half",
        );
        compare(
            data,
            include_bytes!("testdata/jbase96_quarter.ref.pnm"),
            "baseline quarter",
        );
    }

    #[test]
    fn shows_partial_progressive_files_and_survives_damage() {
        let data = include_bytes!("testdata/jprog.jpg");
        let partial = decode(&data[..data.len() * 2 / 3], (48, 40))
            .expect("earlier scans still make an image");
        assert_eq!((partial.width, partial.height), (48, 40));
        for i in 2..data.len() {
            let mut damaged = data.to_vec();
            damaged[i] ^= 0x33;
            let _ = decode(&damaged, (48, 40));
        }
        let baseline = include_bytes!("testdata/j420.jpg");
        for cut in [2, 10, 100, 300, 600] {
            let _ = decode(&baseline[..cut], (48, 40));
        }
        for i in 2..baseline.len() {
            let mut damaged = baseline.to_vec();
            damaged[i] ^= 0x33;
            let _ = decode(&damaged, (48, 40));
        }
        let small = decode(baseline, (12, 10)).unwrap();
        assert_eq!((small.width, small.height), (12, 10));
    }
}
