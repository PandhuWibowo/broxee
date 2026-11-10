//! WebP lossy (VP8 key frames, RFC 6386): boolean entropy decoding, intra prediction, the
//! inverse DCT and WHT, and both loop filters.
//!
//! Macroblock rows are decoded one at a time. Two rows are kept (the loop filter of a row
//! changes the three pixel rows above it), and once a row is final its pixels go to the
//! caller as Y, U and V rows. Memory is a few rows of the image, never the whole frame.
//! Edge handling follows libwebp, the reference decoder for WebP.

use crate::vp8_tables::{
    AC_QLOOKUP, COEFF_UPDATE_PROBS, DC_QLOOKUP, DEFAULT_COEFF_PROBS, KF_BMODE_PROBS,
};
use crate::Error;

struct BoolDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    value: u32,
    range: u32,
    bit_count: u32,
}

impl<'a> BoolDecoder<'a> {
    fn new(data: &'a [u8]) -> BoolDecoder<'a> {
        let b = |i: usize| u32::from(data.get(i).copied().unwrap_or(0));
        BoolDecoder {
            data,
            pos: 2,
            value: b(0) << 8 | b(1),
            range: 255,
            bit_count: 0,
        }
    }

    #[inline]
    fn read(&mut self, prob: u8) -> bool {
        let split = 1 + (((self.range - 1) * u32::from(prob)) >> 8);
        let big = split << 8;
        let bit = if self.value >= big {
            self.range -= split;
            self.value -= big;
            true
        } else {
            self.range = split;
            false
        };
        // Normalize: shift until the range is back in [128, 255], a byte at a time.
        let shift = self.range.leading_zeros().saturating_sub(24);
        if shift > 0 {
            self.range <<= shift;
            for _ in 0..shift {
                self.value <<= 1;
                self.bit_count += 1;
                if self.bit_count == 8 {
                    self.bit_count = 0;
                    self.value |= u32::from(self.data.get(self.pos).copied().unwrap_or(0));
                    self.pos += 1;
                }
            }
        }
        bit
    }

    fn literal(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| v << 1 | u32::from(self.read(128)))
    }

    fn signed(&mut self, n: u32) -> i32 {
        let v = self.literal(n) as i32;
        if self.read(128) {
            -v
        } else {
            v
        }
    }

    fn optional_signed(&mut self, n: u32) -> i32 {
        if self.read(128) {
            self.signed(n)
        } else {
            0
        }
    }

    fn tree(&mut self, tree: &[i8], probs: &[u8]) -> u8 {
        let mut i = 0usize;
        loop {
            let next = tree[i + usize::from(self.read(probs[i >> 1]))];
            if next <= 0 {
                return (-next) as u8;
            }
            i = next as usize;
        }
    }
}

const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const TM_PRED: u8 = 3;
const B_PRED: u8 = 4;

const KF_YMODE_TREE: [i8; 8] = [
    -(B_PRED as i8),
    2,
    4,
    6,
    -(DC_PRED as i8),
    -(V_PRED as i8),
    -(H_PRED as i8),
    -(TM_PRED as i8),
];
const KF_YMODE_PROBS: [u8; 4] = [145, 156, 163, 128];
const UV_MODE_TREE: [i8; 6] = [
    -(DC_PRED as i8),
    2,
    -(V_PRED as i8),
    4,
    -(H_PRED as i8),
    -(TM_PRED as i8),
];
const KF_UV_MODE_PROBS: [u8; 3] = [142, 114, 183];
const SEGMENT_TREE: [i8; 6] = [2, 4, 0, -1, -2, -3];

const B_DC: u8 = 0;
const B_TM: u8 = 1;
const B_VE: u8 = 2;
const B_HE: u8 = 3;
const B_LD: u8 = 4;
const B_RD: u8 = 5;
const B_VR: u8 = 6;
const B_VL: u8 = 7;
const B_HD: u8 = 8;
const B_HU: u8 = 9;
const BMODE_TREE: [i8; 18] = [
    -(B_DC as i8),
    2,
    -(B_TM as i8),
    4,
    -(B_VE as i8),
    6,
    8,
    12,
    -(B_HE as i8),
    10,
    -(B_RD as i8),
    -(B_VR as i8),
    -(B_LD as i8),
    14,
    -(B_VL as i8),
    16,
    -(B_HD as i8),
    -(B_HU as i8),
];

const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const BANDS: [usize; 17] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7, 0];
const CAT3456: [&[u8]; 4] = [
    &[173, 148, 140],
    &[176, 155, 140, 135],
    &[180, 157, 141, 134, 130],
    &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129],
];

type Probs = [[[[u8; 11]; 3]; 8]; 4];

#[derive(Clone, Copy, Default)]
struct Quant {
    y1: [i32; 2],
    y2: [i32; 2],
    uv: [i32; 2],
}

#[derive(Clone, Copy, Default)]
struct Strength {
    limit: i32,
    inner_limit: i32,
    hev: i32,
}

struct Header {
    simple_filter: bool,
    /// Per segment and per "is 4x4": the filter strength, `limit == 0` meaning none.
    strengths: [[Strength; 2]; 4],
    quant: [Quant; 4],
    segment_map: Option<[u8; 3]>,
    skip_prob: Option<u8>,
    probs: Box<Probs>,
    partitions: usize,
}

fn parse_header(d: &mut BoolDecoder) -> Result<Header, Error> {
    d.read(128); // color space
    d.read(128); // clamping type
    let mut segment_map = None;
    let mut seg_absolute = false;
    let mut seg_quant = [0i32; 4];
    let mut seg_filter = [0i32; 4];
    let segments = d.read(128);
    if segments {
        let update_map = d.read(128);
        if d.read(128) {
            seg_absolute = d.read(128);
            for q in &mut seg_quant {
                *q = d.optional_signed(7);
            }
            for f in &mut seg_filter {
                *f = d.optional_signed(6);
            }
        }
        if update_map {
            let mut probs = [255u8; 3];
            for p in &mut probs {
                if d.read(128) {
                    *p = d.literal(8) as u8;
                }
            }
            segment_map = Some(probs);
        }
    }
    let simple_filter = d.read(128);
    let level = d.literal(6) as i32;
    let sharpness = d.literal(3) as i32;
    let mut ref_delta = 0;
    let mut mode_delta = 0;
    let use_deltas = d.read(128);
    if use_deltas && d.read(128) {
        for i in 0..4 {
            let v = d.optional_signed(6);
            if i == 0 {
                ref_delta = v;
            }
        }
        for i in 0..4 {
            let v = d.optional_signed(6);
            if i == 0 {
                mode_delta = v;
            }
        }
    }
    let partitions_log2 = d.literal(2);
    let base_q = d.literal(7) as i32;
    let y1_dc = d.optional_signed(4);
    let y2_dc = d.optional_signed(4);
    let y2_ac = d.optional_signed(4);
    let uv_dc = d.optional_signed(4);
    let uv_ac = d.optional_signed(4);
    d.read(128); // refresh entropy probabilities: irrelevant for one frame
    let mut probs = Box::new(DEFAULT_COEFF_PROBS);
    for (i, plane) in probs.iter_mut().enumerate() {
        for (j, band) in plane.iter_mut().enumerate() {
            for (k, ctx) in band.iter_mut().enumerate() {
                for (l, p) in ctx.iter_mut().enumerate() {
                    if d.read(COEFF_UPDATE_PROBS[i][j][k][l]) {
                        *p = d.literal(8) as u8;
                    }
                }
            }
        }
    }
    let skip_prob = d.read(128).then(|| d.literal(8) as u8);

    let mut quant = [Quant::default(); 4];
    let mut strengths = [[Strength::default(); 2]; 4];
    for s in 0..4 {
        let q = if segments {
            seg_quant[s] + if seg_absolute { 0 } else { base_q }
        } else {
            base_q
        };
        let dc = |delta: i32| i32::from(DC_QLOOKUP[(q + delta).clamp(0, 127) as usize]);
        let ac = |delta: i32| i32::from(AC_QLOOKUP[(q + delta).clamp(0, 127) as usize]);
        quant[s] = Quant {
            y1: [dc(y1_dc), ac(0)],
            y2: [dc(y2_dc) * 2, (ac(y2_ac) * 155 / 100).max(8)],
            uv: [dc(uv_dc).min(132), ac(uv_ac)],
        };
        let base_level = if segments {
            seg_filter[s] + if seg_absolute { 0 } else { level }
        } else {
            level
        };
        for (i4x4, st) in strengths[s].iter_mut().enumerate() {
            let mut l = base_level;
            if use_deltas {
                l += ref_delta;
                if i4x4 == 1 {
                    l += mode_delta;
                }
            }
            let l = l.clamp(0, 63);
            if l > 0 {
                let mut inner = l;
                if sharpness > 0 {
                    inner >>= if sharpness > 4 { 2 } else { 1 };
                    inner = inner.min(9 - sharpness);
                }
                let inner = inner.max(1);
                *st = Strength {
                    limit: 2 * l + inner,
                    inner_limit: inner,
                    hev: if l >= 40 {
                        2
                    } else if l >= 15 {
                        1
                    } else {
                        0
                    },
                };
            }
        }
    }
    Ok(Header {
        partitions: 1 << partitions_log2,
        simple_filter,
        strengths,
        quant,
        segment_map,
        skip_prob,
        probs,
    })
}

/// Reads one block's coefficients, dequantized, into `out` (in raster order). Returns the
/// position after the last coefficient read, as libwebp does.
fn coefficients(
    d: &mut BoolDecoder,
    probs: &[[[u8; 11]; 3]; 8],
    ctx: usize,
    dq: [i32; 2],
    first: usize,
    out: &mut [i32],
) -> usize {
    let mut n = first;
    let mut p = &probs[BANDS[n]][ctx];
    while n < 16 {
        if !d.read(p[0]) {
            return n;
        }
        while !d.read(p[1]) {
            n += 1;
            if n == 16 {
                return 16;
            }
            p = &probs[BANDS[n]][0];
        }
        let next = &probs[BANDS[n + 1]];
        let v = if !d.read(p[2]) {
            p = &next[1];
            1
        } else {
            let v = large_value(d, p);
            p = &next[2];
            v
        };
        let v = if d.read(128) { -v } else { v };
        out[ZIGZAG[n]] = v * dq[usize::from(n > 0)];
        n += 1;
    }
    16
}

fn large_value(d: &mut BoolDecoder, p: &[u8; 11]) -> i32 {
    if !d.read(p[3]) {
        if !d.read(p[4]) {
            2
        } else {
            3 + i32::from(d.read(p[5]))
        }
    } else if !d.read(p[6]) {
        if !d.read(p[7]) {
            5 + i32::from(d.read(159))
        } else {
            7 + 2 * i32::from(d.read(165)) + i32::from(d.read(145))
        }
    } else {
        let bit1 = usize::from(d.read(p[8]));
        let bit0 = usize::from(d.read(p[9 + bit1]));
        let cat = 2 * bit1 + bit0;
        let mut v = 0i32;
        for &prob in CAT3456[cat] {
            v = v + v + i32::from(d.read(prob));
        }
        v + 3 + (8 << cat)
    }
}

fn inverse_wht(input: &[i32; 16], coeffs: &mut [i32]) {
    let mut tmp = [0i32; 16];
    for i in 0..4 {
        let a0 = input[i] + input[12 + i];
        let a1 = input[4 + i] + input[8 + i];
        let a2 = input[4 + i] - input[8 + i];
        let a3 = input[i] - input[12 + i];
        tmp[i] = a0 + a1;
        tmp[8 + i] = a0 - a1;
        tmp[4 + i] = a3 + a2;
        tmp[12 + i] = a3 - a2;
    }
    for i in 0..4 {
        let dc = tmp[i * 4] + 3;
        let a0 = dc + tmp[3 + i * 4];
        let a1 = tmp[1 + i * 4] + tmp[2 + i * 4];
        let a2 = tmp[1 + i * 4] - tmp[2 + i * 4];
        let a3 = dc - tmp[3 + i * 4];
        let block = i * 4;
        coeffs[block * 16] = (a0 + a1) >> 3;
        coeffs[(block + 1) * 16] = (a3 + a2) >> 3;
        coeffs[(block + 2) * 16] = (a0 - a1) >> 3;
        coeffs[(block + 3) * 16] = (a3 - a2) >> 3;
    }
}

#[inline]
fn mul1(a: i32) -> i32 {
    ((a * 20091) >> 16) + a
}

#[inline]
fn mul2(a: i32) -> i32 {
    (a * 35468) >> 16
}

/// Adds the inverse DCT of `c` to the 4x4 block at `at` in `buf`.
fn add_idct(c: &[i32], buf: &mut [u8], at: usize, stride: usize) {
    if c.iter().all(|&v| v == 0) {
        return;
    }
    let mut tmp = [0i32; 16];
    for i in 0..4 {
        let a = c[i] + c[8 + i];
        let b = c[i] - c[8 + i];
        let cc = mul2(c[4 + i]) - mul1(c[12 + i]);
        let d = mul1(c[4 + i]) + mul2(c[12 + i]);
        tmp[i * 4] = a + d;
        tmp[i * 4 + 1] = b + cc;
        tmp[i * 4 + 2] = b - cc;
        tmp[i * 4 + 3] = a - d;
    }
    for i in 0..4 {
        let dc = tmp[i] + 4;
        let a = dc + tmp[8 + i];
        let b = dc - tmp[8 + i];
        let cc = mul2(tmp[4 + i]) - mul1(tmp[12 + i]);
        let d = mul1(tmp[4 + i]) + mul2(tmp[12 + i]);
        let row = at + i * stride;
        for (x, v) in [a + d, b + cc, b - cc, a - d].into_iter().enumerate() {
            let px = &mut buf[row + x];
            *px = (i32::from(*px) + (v >> 3)).clamp(0, 255) as u8;
        }
    }
}

// Prediction workspaces: luma is 4 + 16 + 4 columns by 1 + 16 rows, chroma 4 + 8 by 1 + 8.
// The four columns on the left receive the previous macroblock's right edge.
const YS: usize = 32;
const CS: usize = 16;
const Y0: usize = YS + 4;
const C0: usize = CS + 4;

fn avg3(a: u8, b: u8, c: u8) -> u8 {
    ((u32::from(a) + 2 * u32::from(b) + u32::from(c) + 2) >> 2) as u8
}

fn avg2(a: u8, b: u8) -> u8 {
    ((u32::from(a) + u32::from(b) + 1) >> 1) as u8
}

/// Whole-block prediction (16x16 luma or 8x8 chroma) at `at` in `w`.
fn predict_block(
    w: &mut [u8],
    at: usize,
    stride: usize,
    size: usize,
    mode: u8,
    top: bool,
    left: bool,
) {
    match mode {
        DC_PRED => {
            let shift = size.trailing_zeros();
            let sum_top: u32 = (0..size).map(|x| u32::from(w[at - stride + x])).sum();
            let sum_left: u32 = (0..size).map(|y| u32::from(w[at + y * stride - 1])).sum();
            let dc = match (top, left) {
                (true, true) => (sum_top + sum_left + size as u32) >> (shift + 1),
                (true, false) => (sum_top + (size as u32 >> 1)) >> shift,
                (false, true) => (sum_left + (size as u32 >> 1)) >> shift,
                (false, false) => 128,
            } as u8;
            for y in 0..size {
                w[at + y * stride..at + y * stride + size].fill(dc);
            }
        }
        V_PRED => {
            for y in 0..size {
                w.copy_within(at - stride..at - stride + size, at + y * stride);
            }
        }
        H_PRED => {
            for y in 0..size {
                let l = w[at + y * stride - 1];
                w[at + y * stride..at + y * stride + size].fill(l);
            }
        }
        _ => tm(w, at, stride, size),
    }
}

fn tm(w: &mut [u8], at: usize, stride: usize, size: usize) {
    let tl = i32::from(w[at - stride - 1]);
    for y in 0..size {
        let l = i32::from(w[at + y * stride - 1]) - tl;
        for x in 0..size {
            let t = i32::from(w[at - stride + x]);
            w[at + y * stride + x] = (t + l).clamp(0, 255) as u8;
        }
    }
}

fn predict4(w: &mut [u8], at: usize, mode: u8) {
    let s = YS;
    let t = |i: isize| w[(at as isize - s as isize + i) as usize];
    let top: [u8; 8] = std::array::from_fn(|i| t(i as isize));
    let x = t(-1);
    let l: [u8; 4] = std::array::from_fn(|i| w[at + i * s - 1]);
    let [a, b, c, d, e, f, g, h] = top;
    let [i, j, k, ll] = l;
    let mut o = [[0u8; 4]; 4]; // o[y][x]
    match mode {
        B_DC => {
            let sum: u32 = top[..4].iter().chain(&l).map(|&v| u32::from(v)).sum();
            o = [[((sum + 4) >> 3) as u8; 4]; 4];
        }
        B_TM => {
            tm(w, at, s, 4);
            return;
        }
        B_VE => {
            let v = [avg3(x, a, b), avg3(a, b, c), avg3(b, c, d), avg3(c, d, e)];
            o = [v; 4];
        }
        B_HE => {
            let r = [
                avg3(x, i, j),
                avg3(i, j, k),
                avg3(j, k, ll),
                avg3(k, ll, ll),
            ];
            for (y, v) in r.into_iter().enumerate() {
                o[y] = [v; 4];
            }
        }
        B_RD => {
            o[3][0] = avg3(j, k, ll);
            let v = avg3(i, j, k);
            o[3][1] = v;
            o[2][0] = v;
            let v = avg3(x, i, j);
            o[3][2] = v;
            o[2][1] = v;
            o[1][0] = v;
            let v = avg3(a, x, i);
            o[3][3] = v;
            o[2][2] = v;
            o[1][1] = v;
            o[0][0] = v;
            let v = avg3(b, a, x);
            o[2][3] = v;
            o[1][2] = v;
            o[0][1] = v;
            let v = avg3(c, b, a);
            o[1][3] = v;
            o[0][2] = v;
            o[0][3] = avg3(d, c, b);
        }
        B_LD => {
            let v = [
                avg3(a, b, c),
                avg3(b, c, d),
                avg3(c, d, e),
                avg3(d, e, f),
                avg3(e, f, g),
                avg3(f, g, h),
                avg3(g, h, h),
            ];
            for (y, row) in o.iter_mut().enumerate() {
                for (xx, px) in row.iter_mut().enumerate() {
                    *px = v[xx + y];
                }
            }
        }
        B_VR => {
            o[0][0] = avg2(x, a);
            o[2][1] = o[0][0];
            o[0][1] = avg2(a, b);
            o[2][2] = o[0][1];
            o[0][2] = avg2(b, c);
            o[2][3] = o[0][2];
            o[0][3] = avg2(c, d);
            o[3][0] = avg3(k, j, i);
            o[2][0] = avg3(j, i, x);
            o[1][0] = avg3(i, x, a);
            o[3][1] = o[1][0];
            o[1][1] = avg3(x, a, b);
            o[3][2] = o[1][1];
            o[1][2] = avg3(a, b, c);
            o[3][3] = o[1][2];
            o[1][3] = avg3(b, c, d);
        }
        B_VL => {
            o[0][0] = avg2(a, b);
            o[0][1] = avg2(b, c);
            o[2][0] = o[0][1];
            o[0][2] = avg2(c, d);
            o[2][1] = o[0][2];
            o[0][3] = avg2(d, e);
            o[2][2] = o[0][3];
            o[1][0] = avg3(a, b, c);
            o[1][1] = avg3(b, c, d);
            o[3][0] = o[1][1];
            o[1][2] = avg3(c, d, e);
            o[3][1] = o[1][2];
            o[1][3] = avg3(d, e, f);
            o[3][2] = o[1][3];
            o[2][3] = avg3(e, f, g);
            o[3][3] = avg3(f, g, h);
        }
        B_HD => {
            o[0][0] = avg2(i, x);
            o[1][2] = o[0][0];
            o[1][0] = avg2(j, i);
            o[2][2] = o[1][0];
            o[2][0] = avg2(k, j);
            o[3][2] = o[2][0];
            o[3][0] = avg2(ll, k);
            o[0][3] = avg3(a, b, c);
            o[0][2] = avg3(x, a, b);
            o[0][1] = avg3(i, x, a);
            o[1][3] = o[0][1];
            o[1][1] = avg3(j, i, x);
            o[2][3] = o[1][1];
            o[2][1] = avg3(k, j, i);
            o[3][3] = o[2][1];
            o[3][1] = avg3(ll, k, j);
        }
        _ => {
            // B_HU
            o[0][0] = avg2(i, j);
            o[0][2] = avg2(j, k);
            o[1][0] = o[0][2];
            o[1][2] = avg2(k, ll);
            o[2][0] = o[1][2];
            o[0][1] = avg3(i, j, k);
            o[0][3] = avg3(j, k, ll);
            o[1][1] = o[0][3];
            o[1][3] = avg3(k, ll, ll);
            o[2][1] = o[1][3];
            o[2][2] = ll;
            o[2][3] = ll;
            o[3] = [ll; 4];
        }
    }
    for (y, row) in o.iter().enumerate() {
        w[at + y * s..at + y * s + 4].copy_from_slice(row);
    }
}

/// Per-macroblock state needed after parsing: for prediction and for the loop filter.
#[derive(Clone, Copy, Default)]
struct MbInfo {
    strength: Strength,
    inner: bool,
}

/// The decoded size and the planes of each finished pixel row.
pub(crate) struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
}

pub(crate) fn probe(data: &[u8]) -> Option<(u32, u32)> {
    let d = data.get(..10)?;
    if d[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u32::from(u16::from_le_bytes([d[6], d[7]]) & 0x3fff);
    let h = u32::from(u16::from_le_bytes([d[8], d[9]]) & 0x3fff);
    Some((w, h))
}

/// Decode a VP8 key frame. `row` gets each pixel row in order: luma at full width, chroma
/// at half width for that row.
pub(crate) fn decode(data: &[u8], row: &mut dyn FnMut(Planes)) -> Result<(u32, u32), Error> {
    let tag = data.get(..3).ok_or(Error::Truncated)?;
    let bits = u32::from(tag[0]) | u32::from(tag[1]) << 8 | u32::from(tag[2]) << 16;
    if bits & 1 != 0 {
        return Err(Error::Unsupported("VP8 inter frame"));
    }
    let part0_len = (bits >> 5) as usize;
    let (width, height) = probe(data).ok_or(Error::Corrupt("VP8 start code"))?;
    if width == 0 || height == 0 {
        return Err(Error::Corrupt("VP8 size"));
    }
    let part0 = data.get(10..10 + part0_len).ok_or(Error::Truncated)?;
    let mut d = BoolDecoder::new(part0);
    let header = parse_header(&mut d)?;

    let partitions = header.partitions;
    let rest = &data[10 + part0_len..];
    let sizes_len = 3 * (partitions - 1);
    let sizes = rest.get(..sizes_len).ok_or(Error::Truncated)?;
    let mut tokens = Vec::with_capacity(partitions);
    let mut at = sizes_len;
    for p in 0..partitions {
        let len = if p + 1 < partitions {
            let s = &sizes[p * 3..p * 3 + 3];
            usize::from(s[0]) | usize::from(s[1]) << 8 | usize::from(s[2]) << 16
        } else {
            rest.len().saturating_sub(at)
        };
        let end = (at + len).min(rest.len());
        tokens.push(BoolDecoder::new(&rest[at.min(end)..end]));
        at = end;
    }

    let mbw = width.div_ceil(16) as usize;
    let mbh = height.div_ceil(16) as usize;
    let yw = mbw * 16;
    let cw = mbw * 8;
    // Two macroblock rows of pixels: the one being decoded and the one above it.
    let mut cur_y = vec![0u8; yw * 16];
    let mut cur_u = vec![0u8; cw * 8];
    let mut cur_v = vec![0u8; cw * 8];
    let mut prev_y = vec![0u8; yw * 16];
    let mut prev_u = vec![0u8; cw * 8];
    let mut prev_v = vec![0u8; cw * 8];
    // Unfiltered bottom row of the previous macroblock row, for intra prediction.
    let mut top_y = vec![0u8; yw];
    let mut top_u = vec![0u8; cw];
    let mut top_v = vec![0u8; cw];
    let mut intra_t = vec![B_DC; mbw * 4];
    let mut nz_top = vec![[0u8; 9]; mbw];
    let mut info = vec![MbInfo::default(); mbw];

    let mut ws_y = [0u8; YS * 17];
    let mut ws_u = [0u8; CS * 9];
    let mut ws_v = [0u8; CS * 9];
    let mut coeffs = [0i32; 384];

    for mby in 0..mbh {
        let tok = &mut tokens[mby % partitions];
        let mut intra_l = [B_DC; 4];
        let mut nz_left = [0u8; 9];
        for j in 0..16 {
            ws_y[Y0 + j * YS - 1] = 129;
        }
        for j in 0..8 {
            ws_u[C0 + j * CS - 1] = 129;
            ws_v[C0 + j * CS - 1] = 129;
        }
        if mby > 0 {
            ws_y[Y0 - YS - 1] = 129;
            ws_u[C0 - CS - 1] = 129;
            ws_v[C0 - CS - 1] = 129;
        } else {
            ws_y[Y0 - YS - 1..Y0 - YS + 20].fill(127);
            ws_u[C0 - CS - 1..C0 - CS + 8].fill(127);
            ws_v[C0 - CS - 1..C0 - CS + 8].fill(127);
        }
        for mbx in 0..mbw {
            // Modes, from the first partition.
            let segment = match &header.segment_map {
                Some(p) => d.tree(&SEGMENT_TREE, p),
                None => 0,
            } as usize;
            let skip = header.skip_prob.is_some_and(|p| d.read(p));
            let ymode = d.tree(&KF_YMODE_TREE, &KF_YMODE_PROBS);
            let mut bmodes = [B_DC; 16];
            let is4 = ymode == B_PRED;
            if is4 {
                for y in 0..4 {
                    let mut left = intra_l[y];
                    for x in 0..4 {
                        let above = intra_t[mbx * 4 + x];
                        let m = d.tree(
                            &BMODE_TREE,
                            &KF_BMODE_PROBS[usize::from(above)][usize::from(left)],
                        );
                        bmodes[y * 4 + x] = m;
                        intra_t[mbx * 4 + x] = m;
                        left = m;
                    }
                    intra_l[y] = left;
                }
            } else {
                let implied = match ymode {
                    V_PRED => B_VE,
                    H_PRED => B_HE,
                    TM_PRED => B_TM,
                    _ => B_DC,
                };
                intra_t[mbx * 4..mbx * 4 + 4].fill(implied);
                intra_l = [implied; 4];
            }
            let uvmode = d.tree(&UV_MODE_TREE, &KF_UV_MODE_PROBS);

            // Residuals, from this row's token partition.
            coeffs.fill(0);
            let mut nonzero = false;
            let top_nz = &mut nz_top[mbx];
            if !skip {
                let q = header.quant[segment];
                let (first, ytype) = if is4 {
                    (0, 3)
                } else {
                    let ctx = usize::from(top_nz[8] + nz_left[8]);
                    let mut dc = [0i32; 16];
                    let n = coefficients(tok, &header.probs[1], ctx, q.y2, 0, &mut dc);
                    let flag = u8::from(n > 0);
                    top_nz[8] = flag;
                    nz_left[8] = flag;
                    inverse_wht(&dc, &mut coeffs);
                    (1, 0)
                };
                for y in 0..4 {
                    let mut l = nz_left[y];
                    for x in 0..4 {
                        let ctx = usize::from(l + top_nz[x]);
                        let block = &mut coeffs[(y * 4 + x) * 16..(y * 4 + x + 1) * 16];
                        let n = coefficients(tok, &header.probs[ytype], ctx, q.y1, first, block);
                        l = u8::from(n > first);
                        top_nz[x] = l;
                        nonzero |= n > 1 || block[0] != 0;
                    }
                    nz_left[y] = l;
                }
                for ch in 0..2 {
                    for y in 0..2 {
                        let mut l = nz_left[4 + ch * 2 + y];
                        for x in 0..2 {
                            let ctx = usize::from(l + top_nz[4 + ch * 2 + x]);
                            let b = 16 + ch * 4 + y * 2 + x;
                            let block = &mut coeffs[b * 16..(b + 1) * 16];
                            let n = coefficients(tok, &header.probs[2], ctx, q.uv, 0, block);
                            l = u8::from(n > 0);
                            top_nz[4 + ch * 2 + x] = l;
                            nonzero |= n > 1 || block[0] != 0;
                        }
                        nz_left[4 + ch * 2 + y] = l;
                    }
                }
            } else {
                top_nz[..8].fill(0);
                nz_left[..8].fill(0);
                if !is4 {
                    top_nz[8] = 0;
                    nz_left[8] = 0;
                }
            }
            info[mbx] = MbInfo {
                strength: header.strengths[segment][usize::from(is4)],
                inner: is4 || nonzero,
            };

            // Reconstruction, in the workspace.
            if mbx > 0 {
                for j in 0..17 {
                    let r = Y0 + j * YS - YS;
                    ws_y.copy_within(r + 12..r + 16, r - 4);
                }
                for j in 0..9 {
                    let r = C0 + j * CS - CS;
                    ws_u.copy_within(r + 4..r + 8, r - 4);
                    ws_v.copy_within(r + 4..r + 8, r - 4);
                }
            }
            if mby > 0 {
                ws_y[Y0 - YS..Y0 - YS + 16].copy_from_slice(&top_y[mbx * 16..mbx * 16 + 16]);
                ws_u[C0 - CS..C0 - CS + 8].copy_from_slice(&top_u[mbx * 8..mbx * 8 + 8]);
                ws_v[C0 - CS..C0 - CS + 8].copy_from_slice(&top_v[mbx * 8..mbx * 8 + 8]);
            }
            if is4 {
                let tr = Y0 - YS + 16;
                if mby > 0 {
                    if mbx + 1 >= mbw {
                        let v = top_y[mbx * 16 + 15];
                        ws_y[tr..tr + 4].fill(v);
                    } else {
                        ws_y[tr..tr + 4].copy_from_slice(&top_y[mbx * 16 + 16..mbx * 16 + 20]);
                    }
                }
                for r in [4, 8, 12] {
                    ws_y.copy_within(tr..tr + 4, tr + r * YS);
                }
                for (n, &m) in bmodes.iter().enumerate() {
                    let at = Y0 + (n / 4) * 4 * YS + (n % 4) * 4;
                    predict4(&mut ws_y, at, m);
                    add_idct(&coeffs[n * 16..n * 16 + 16], &mut ws_y, at, YS);
                }
            } else {
                predict_block(&mut ws_y, Y0, YS, 16, ymode, mby > 0, mbx > 0);
                for n in 0..16 {
                    let at = Y0 + (n / 4) * 4 * YS + (n % 4) * 4;
                    add_idct(&coeffs[n * 16..n * 16 + 16], &mut ws_y, at, YS);
                }
            }
            predict_block(&mut ws_u, C0, CS, 8, uvmode, mby > 0, mbx > 0);
            predict_block(&mut ws_v, C0, CS, 8, uvmode, mby > 0, mbx > 0);
            for n in 0..4 {
                let at = C0 + (n / 2) * 4 * CS + (n % 2) * 4;
                add_idct(&coeffs[(16 + n) * 16..(17 + n) * 16], &mut ws_u, at, CS);
                add_idct(&coeffs[(20 + n) * 16..(21 + n) * 16], &mut ws_v, at, CS);
            }
            for y in 0..16 {
                let src = Y0 + y * YS;
                cur_y[y * yw + mbx * 16..y * yw + mbx * 16 + 16]
                    .copy_from_slice(&ws_y[src..src + 16]);
            }
            for y in 0..8 {
                let src = C0 + y * CS;
                cur_u[y * cw + mbx * 8..y * cw + mbx * 8 + 8].copy_from_slice(&ws_u[src..src + 8]);
                cur_v[y * cw + mbx * 8..y * cw + mbx * 8 + 8].copy_from_slice(&ws_v[src..src + 8]);
            }
        }
        // Keep the unfiltered bottom rows for the next row's prediction.
        top_y.copy_from_slice(&cur_y[15 * yw..16 * yw]);
        top_u.copy_from_slice(&cur_u[7 * cw..8 * cw]);
        top_v.copy_from_slice(&cur_v[7 * cw..8 * cw]);

        // Loop filter, which also reaches into the bottom of the previous row.
        let mut planes = FilterPlanes {
            prev_y: &mut prev_y,
            cur_y: &mut cur_y,
            prev_u: &mut prev_u,
            cur_u: &mut cur_u,
            prev_v: &mut prev_v,
            cur_v: &mut cur_v,
            yw,
            cw,
        };
        for (mbx, mb) in info.iter().enumerate() {
            planes.filter_mb(mbx, mby, mb, header.simple_filter);
        }

        // The previous row is final now.
        if mby > 0 {
            let base = (mby - 1) * 16;
            emit(
                row, &prev_y, &prev_u, &prev_v, yw, cw, base, 16, height, width,
            );
        }
        std::mem::swap(&mut prev_y, &mut cur_y);
        std::mem::swap(&mut prev_u, &mut cur_u);
        std::mem::swap(&mut prev_v, &mut cur_v);
    }
    emit(
        row,
        &prev_y,
        &prev_u,
        &prev_v,
        yw,
        cw,
        (mbh - 1) * 16,
        16,
        height,
        width,
    );
    Ok((width, height))
}

#[allow(clippy::too_many_arguments)]
fn emit(
    row: &mut dyn FnMut(Planes),
    y: &[u8],
    u: &[u8],
    v: &[u8],
    yw: usize,
    cw: usize,
    base: usize,
    rows: usize,
    height: u32,
    width: u32,
) {
    let w = width as usize;
    let cwid = w.div_ceil(2);
    for r in 0..rows {
        if base + r >= height as usize {
            return;
        }
        let c = (r / 2) * cw;
        row(Planes {
            y: &y[r * yw..r * yw + w],
            u: &u[c..c + cwid],
            v: &v[c..c + cwid],
        });
    }
}

struct FilterPlanes<'a> {
    prev_y: &'a mut [u8],
    cur_y: &'a mut [u8],
    prev_u: &'a mut [u8],
    cur_u: &'a mut [u8],
    prev_v: &'a mut [u8],
    cur_v: &'a mut [u8],
    yw: usize,
    cw: usize,
}

/// A view of a plane where row -1..-4 are the bottom rows of the previous macroblock row.
struct Plane<'a> {
    prev: &'a mut [u8],
    cur: &'a mut [u8],
    stride: usize,
    rows: usize,
}

impl Plane<'_> {
    #[inline]
    fn at(&mut self, x: usize, y: isize) -> &mut u8 {
        if y < 0 {
            let r = (self.rows as isize + y) as usize;
            &mut self.prev[r * self.stride + x]
        } else {
            &mut self.cur[y as usize * self.stride + x]
        }
    }

    /// The 8 pixels across an edge at (x, y): p3 p2 p1 p0 | q0 q1 q2 q3. `vertical` edges
    /// run up and down (pixels step along x).
    fn get(&mut self, x: usize, y: isize, vertical: bool) -> [i32; 8] {
        std::array::from_fn(|i| {
            let o = i as isize - 4;
            if vertical {
                i32::from(*self.at((x as isize + o) as usize, y))
            } else {
                i32::from(*self.at(x, y + o))
            }
        })
    }

    fn set(&mut self, x: usize, y: isize, vertical: bool, from: usize, values: &[i32]) {
        for (k, &v) in values.iter().enumerate() {
            let o = (from + k) as isize - 4;
            let px = if vertical {
                self.at((x as isize + o) as usize, y)
            } else {
                self.at(x, y + o)
            };
            *px = v.clamp(0, 255) as u8;
        }
    }
}

fn sclip1(v: i32) -> i32 {
    v.clamp(-128, 127)
}

fn sclip2(v: i32) -> i32 {
    v.clamp(-16, 15)
}

fn needs_filter(p: &[i32; 8], t: i32) -> bool {
    4 * (p[3] - p[4]).abs() + (p[2] - p[5]).abs() <= t
}

fn needs_filter2(p: &[i32; 8], t: i32, it: i32) -> bool {
    needs_filter(p, t)
        && (p[0] - p[1]).abs() <= it
        && (p[1] - p[2]).abs() <= it
        && (p[2] - p[3]).abs() <= it
        && (p[7] - p[6]).abs() <= it
        && (p[6] - p[5]).abs() <= it
        && (p[5] - p[4]).abs() <= it
}

fn hev(p: &[i32; 8], t: i32) -> bool {
    (p[2] - p[3]).abs() > t || (p[5] - p[4]).abs() > t
}

impl FilterPlanes<'_> {
    fn filter_mb(&mut self, mbx: usize, mby: usize, mb: &MbInfo, simple: bool) {
        let s = mb.strength;
        if s.limit == 0 {
            return;
        }
        let mut y = Plane {
            prev: self.prev_y,
            cur: self.cur_y,
            stride: self.yw,
            rows: 16,
        };
        let x0 = mbx * 16;
        if simple {
            if mbx > 0 {
                simple_edge(&mut y, x0, 0, true, s.limit + 4);
            }
            if mb.inner {
                for k in [4, 8, 12] {
                    simple_edge(&mut y, x0 + k, 0, true, s.limit);
                }
            }
            if mby > 0 {
                simple_edge(&mut y, x0, 0, false, s.limit + 4);
            }
            if mb.inner {
                for k in [4, 8, 12] {
                    simple_edge(&mut y, x0, k, false, s.limit);
                }
            }
            return;
        }
        let mut u = Plane {
            prev: self.prev_u,
            cur: self.cur_u,
            stride: self.cw,
            rows: 8,
        };
        let mut v = Plane {
            prev: self.prev_v,
            cur: self.cur_v,
            stride: self.cw,
            rows: 8,
        };
        let c0 = mbx * 8;
        if mbx > 0 {
            normal_edge(&mut y, x0, 0, 16, true, s, true);
            normal_edge(&mut u, c0, 0, 8, true, s, true);
            normal_edge(&mut v, c0, 0, 8, true, s, true);
        }
        if mb.inner {
            for k in [4, 8, 12] {
                normal_edge(&mut y, x0 + k, 0, 16, true, s, false);
            }
            normal_edge(&mut u, c0 + 4, 0, 8, true, s, false);
            normal_edge(&mut v, c0 + 4, 0, 8, true, s, false);
        }
        if mby > 0 {
            normal_edge(&mut y, x0, 0, 16, false, s, true);
            normal_edge(&mut u, c0, 0, 8, false, s, true);
            normal_edge(&mut v, c0, 0, 8, false, s, true);
        }
        if mb.inner {
            for k in [4, 8, 12] {
                normal_edge(&mut y, x0, k, 16, false, s, false);
            }
            normal_edge(&mut u, c0, 4, 8, false, s, false);
            normal_edge(&mut v, c0, 4, 8, false, s, false);
        }
    }
}

/// Filter one 16-pixel luma edge with the simple filter. A vertical edge sits left of
/// column `x`; a horizontal one above row `y`.
fn simple_edge(pl: &mut Plane, x: usize, y: isize, vertical: bool, limit: i32) {
    let t = 2 * limit + 1;
    for i in 0..16 {
        let (px, py) = if vertical {
            (x, y + i as isize)
        } else {
            (x + i, y)
        };
        let p = pl.get(px, py, vertical);
        if needs_filter(&p, t) {
            let f = filter2(&p);
            pl.set(px, py, vertical, 3, &f);
        }
    }
}

fn filter2(p: &[i32; 8]) -> [i32; 2] {
    let a = 3 * (p[4] - p[3]) + sclip1(p[2] - p[5]);
    let a1 = sclip2((a + 4) >> 3);
    let a2 = sclip2((a + 3) >> 3);
    [p[3] + a2, p[4] - a1]
}

fn normal_edge(
    pl: &mut Plane,
    x: usize,
    y: isize,
    len: usize,
    vertical: bool,
    s: Strength,
    mb_edge: bool,
) {
    let t = 2 * if mb_edge { s.limit + 4 } else { s.limit } + 1;
    for i in 0..len {
        let (px, py) = if vertical {
            (x, y + i as isize)
        } else {
            (x + i, y)
        };
        let p = pl.get(px, py, vertical);
        if !needs_filter2(&p, t, s.inner_limit) {
            continue;
        }
        if hev(&p, s.hev) {
            let f = filter2(&p);
            pl.set(px, py, vertical, 3, &f);
        } else if mb_edge {
            let a = sclip1(3 * (p[4] - p[3]) + sclip1(p[2] - p[5]));
            let a1 = (27 * a + 63) >> 7;
            let a2 = (18 * a + 63) >> 7;
            let a3 = (9 * a + 63) >> 7;
            pl.set(
                px,
                py,
                vertical,
                1,
                &[
                    p[1] + a3,
                    p[2] + a2,
                    p[3] + a1,
                    p[4] - a1,
                    p[5] - a2,
                    p[6] - a3,
                ],
            );
        } else {
            let a = 3 * (p[4] - p[3]);
            let a1 = sclip2((a + 4) >> 3);
            let a2 = sclip2((a + 3) >> 3);
            let a3 = (a1 + 1) >> 1;
            pl.set(
                px,
                py,
                vertical,
                2,
                &[p[2] + a3, p[3] + a2, p[4] - a1, p[5] - a3],
            );
        }
    }
}
