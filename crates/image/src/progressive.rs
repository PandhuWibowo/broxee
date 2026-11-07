//! Progressive JPEG (ITU T.81, G.1.2): the image arrives in several scans that each add
//! frequencies (spectral selection) or bits (successive approximation), so coefficients must be
//! kept until the last scan.
//!
//! Kept at full size that would be 2 bytes per coefficient, 36 MB for a 12-megapixel photo.
//! Instead only the k x k lowest frequencies of each block are stored, where k (1, 2, 4 or 8)
//! is the smallest that still covers the size the image is drawn at, and the image is rebuilt
//! with a k-point inverse DCT (as libjpeg's `-scale` does). Refinement scans need to know which
//! coefficients are non-zero, including the ones not stored: one 64-bit mask per block holds
//! that. A photo drawn at a quarter of its size keeps 2x2 coefficients and a mask per block.

use std::sync::Mutex;

use crate::jpeg::{
    color_row, extend, idct_scaled, luma_scale, plane_scale, BitReader, Component, Frame, Huffman,
    ZIGZAG,
};
use crate::{Bitmap, Error, Scaler};

/// One progressive decode at a time: its coefficients are the largest buffer the decoders
/// hold, and the painter decodes images in parallel.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Coefficient storage a single image may use.
const MAX_COEFFICIENT_BYTES: usize = 24 << 20;

pub(crate) struct Scan {
    /// Indices into the frame's components.
    pub components: Vec<usize>,
    pub dc_tables: Vec<usize>,
    pub ac_tables: Vec<usize>,
    pub start: usize,
    pub end: usize,
    pub high: u32,
    pub low: u32,
}

struct Plane {
    /// Blocks per row and column, padded to whole MCUs.
    blocks_w: usize,
    blocks_h: usize,
    /// Coefficients kept per side. Subsampled chroma keeps more (as libjpeg scales it), so it
    /// comes out at the luma resolution with no upsampling.
    k: usize,
    /// Natural index to storage slot in a block, or -1 when not kept.
    slot: [i8; 64],
    /// k x k coefficients per block, natural order, not dequantized.
    coefs: Vec<i16>,
    /// Bit n set when natural coefficient n is non-zero.
    nonzero: Vec<u64>,
    pred: i32,
}

pub(crate) struct Progressive {
    width: usize,
    height: usize,
    components: Vec<Component>,
    planes: Vec<Plane>,
    hmax: usize,
    vmax: usize,
    mcus_x: usize,
    mcus_y: usize,
    /// Coefficients kept per side for full-resolution (luma) planes.
    k: usize,
    eobrun: u32,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl Progressive {
    pub(crate) fn new(
        width: usize,
        height: usize,
        mut components: Vec<Component>,
        target: (u32, u32),
    ) -> Result<Progressive, Error> {
        if components.len() == 1 {
            components[0].h = 1;
            components[0].v = 1;
        }
        let hmax = components.iter().map(|c| c.h).max().unwrap_or(1);
        let vmax = components.iter().map(|c| c.v).max().unwrap_or(1);
        let mcus_x = width.div_ceil(8 * hmax);
        let mcus_y = height.div_ceil(8 * vmax);
        // The fewest frequencies whose image is still at least as large as the target.
        let k = luma_scale(width, height, target);
        let plane_k = |c: &Component| plane_scale(k, hmax, vmax, c);
        let bytes: usize = components
            .iter()
            .map(|c| mcus_x * c.h * mcus_y * c.v * (plane_k(c).pow(2) * 2 + 8))
            .sum();
        if bytes > MAX_COEFFICIENT_BYTES {
            return Err(Error::TooLarge);
        }
        let planes = components
            .iter()
            .map(|c| {
                let (bw, bh) = (mcus_x * c.h, mcus_y * c.v);
                let pk = plane_k(c);
                let mut slot = [-1i8; 64];
                for v in 0..pk {
                    for u in 0..pk {
                        slot[v * 8 + u] = (v * pk + u) as i8;
                    }
                }
                Plane {
                    blocks_w: bw,
                    blocks_h: bh,
                    k: pk,
                    slot,
                    coefs: vec![0; bw * bh * pk * pk],
                    nonzero: vec![0; bw * bh],
                    pred: 0,
                }
            })
            .collect();
        Ok(Progressive {
            width,
            height,
            components,
            planes,
            hmax,
            vmax,
            mcus_x,
            mcus_y,
            k,
            eobrun: 0,
            _turn: ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner()),
        })
    }

    /// Decode one scan's entropy-coded data. Returns the bytes used, up to the next marker.
    pub(crate) fn decode_scan(
        &mut self,
        data: &[u8],
        scan: &Scan,
        dc: &[Option<Huffman>; 4],
        ac: &[Option<Huffman>; 4],
        restart_interval: usize,
    ) -> Result<usize, Error> {
        if scan.start > scan.end || scan.end > 63 || (scan.start == 0 && scan.end != 0) {
            return Err(Error::Corrupt("spectral selection"));
        }
        if scan.start > 0 && scan.components.len() != 1 {
            return Err(Error::Corrupt("AC scan with several components"));
        }
        let mut bits = BitReader::new(data);
        self.eobrun = 0;
        for &c in &scan.components {
            self.planes[c].pred = 0;
        }
        let mut unit = 0usize;
        let mut restart = |p: &mut Progressive, bits: &mut BitReader| {
            if restart_interval > 0 && unit > 0 && unit.is_multiple_of(restart_interval) {
                bits.restart();
                p.eobrun = 0;
                for &c in &scan.components {
                    p.planes[c].pred = 0;
                }
            }
            unit += 1;
        };
        if scan.components.len() > 1 {
            // Interleaved (DC only): MCUs as in a baseline scan.
            for my in 0..self.mcus_y {
                for mx in 0..self.mcus_x {
                    restart(self, &mut bits);
                    for (i, &c) in scan.components.iter().enumerate() {
                        let table = dc[scan.dc_tables[i]]
                            .as_ref()
                            .ok_or(Error::Corrupt("missing table"))?;
                        let (h, v) = (self.components[c].h, self.components[c].v);
                        for by in 0..v {
                            for bx in 0..h {
                                let block = (my * v + by) * self.planes[c].blocks_w + mx * h + bx;
                                self.decode_dc(&mut bits, c, block, table, scan)?;
                            }
                        }
                    }
                }
            }
        } else {
            // One component: its own blocks, not padded to MCUs.
            let c = scan.components[0];
            let comp = &self.components[c];
            let cw = (self.width * comp.h).div_ceil(self.hmax);
            let ch = (self.height * comp.v).div_ceil(self.vmax);
            let (bw, bh) = (cw.div_ceil(8), ch.div_ceil(8));
            let stride = self.planes[c].blocks_w;
            for by in 0..bh {
                for bx in 0..bw {
                    restart(self, &mut bits);
                    let block = by * stride + bx;
                    if scan.start == 0 {
                        let table = dc[scan.dc_tables[0]]
                            .as_ref()
                            .ok_or(Error::Corrupt("missing table"))?;
                        self.decode_dc(&mut bits, c, block, table, scan)?;
                    } else {
                        let table = ac[scan.ac_tables[0]]
                            .as_ref()
                            .ok_or(Error::Corrupt("missing table"))?;
                        if scan.high == 0 {
                            self.decode_ac_first(&mut bits, c, block, table, scan)?;
                        } else {
                            self.decode_ac_refine(&mut bits, c, block, table, scan)?;
                        }
                    }
                }
            }
        }
        Ok(bits.position())
    }

    fn set(&mut self, c: usize, block: usize, natural: usize, value: i32) {
        let plane = &mut self.planes[c];
        let slot = plane.slot[natural];
        if slot >= 0 {
            plane.coefs[block * plane.k * plane.k + slot as usize] =
                value.clamp(-32768, 32767) as i16;
        }
        if value != 0 {
            plane.nonzero[block] |= 1 << natural;
        } else {
            plane.nonzero[block] &= !(1 << natural);
        }
    }

    fn decode_dc(
        &mut self,
        bits: &mut BitReader,
        c: usize,
        block: usize,
        table: &Huffman,
        scan: &Scan,
    ) -> Result<(), Error> {
        if scan.high == 0 {
            let t = u32::from(table.decode(bits)?);
            if t > 11 {
                return Err(Error::Corrupt("DC magnitude"));
            }
            let diff = if t == 0 { 0 } else { extend(bits.take(t), t) };
            let plane = &mut self.planes[c];
            plane.pred = plane.pred.wrapping_add(diff);
            let value = plane.pred.wrapping_shl(scan.low);
            self.set(c, block, 0, value);
        } else if bits.take(1) == 1 {
            let at = block * self.planes[c].k * self.planes[c].k;
            let value = i32::from(self.planes[c].coefs[at]) | (1 << scan.low);
            self.set(c, block, 0, value);
        }
        Ok(())
    }

    fn decode_ac_first(
        &mut self,
        bits: &mut BitReader,
        c: usize,
        block: usize,
        table: &Huffman,
        scan: &Scan,
    ) -> Result<(), Error> {
        if self.eobrun > 0 {
            self.eobrun -= 1;
            return Ok(());
        }
        let mut k = scan.start;
        while k <= scan.end {
            let rs = table.decode(bits)?;
            let (r, s) = (u32::from(rs >> 4), u32::from(rs & 15));
            if s == 0 {
                if r < 15 {
                    self.eobrun = (1 << r) - 1;
                    if r > 0 {
                        self.eobrun += bits.take(r);
                    }
                    break;
                }
                k += 16;
                continue;
            }
            k += r as usize;
            if k > 63 {
                return Err(Error::Corrupt("AC coefficient index"));
            }
            let value = extend(bits.take(s), s).wrapping_shl(scan.low);
            self.set(c, block, ZIGZAG[k], value);
            k += 1;
        }
        Ok(())
    }

    /// Successive-approximation refinement of AC coefficients (G.1.2.3), as libjpeg and stb_image
    /// implement it: coefficients already non-zero get one correction bit each; zero ones may
    /// become plus or minus one at this bit.
    fn decode_ac_refine(
        &mut self,
        bits: &mut BitReader,
        c: usize,
        block: usize,
        table: &Huffman,
        scan: &Scan,
    ) -> Result<(), Error> {
        let p1 = 1i32 << scan.low;
        let mut k = scan.start;
        if self.eobrun == 0 {
            while k <= scan.end {
                let rs = table.decode(bits)?;
                let (mut r, s) = (u32::from(rs >> 4), u32::from(rs & 15));
                let mut value = 0;
                if s == 0 {
                    if r < 15 {
                        self.eobrun = 1 << r;
                        if r > 0 {
                            self.eobrun += bits.take(r);
                        }
                        break;
                    }
                    // r = 15: sixteen zeros (correcting non-zero ones along the way).
                } else {
                    if s != 1 {
                        return Err(Error::Corrupt("refinement magnitude"));
                    }
                    value = if bits.take(1) == 1 { p1 } else { -p1 };
                }
                while k <= scan.end {
                    let natural = ZIGZAG[k];
                    k += 1;
                    if self.planes[c].nonzero[block] & (1 << natural) != 0 {
                        self.refine(bits, c, block, natural, p1);
                    } else {
                        if r == 0 {
                            if value != 0 {
                                self.set(c, block, natural, value);
                            }
                            break;
                        }
                        r -= 1;
                    }
                }
            }
        }
        if self.eobrun > 0 {
            // The rest of this block is in an end-of-band run: only corrections remain.
            while k <= scan.end {
                let natural = ZIGZAG[k];
                if self.planes[c].nonzero[block] & (1 << natural) != 0 {
                    self.refine(bits, c, block, natural, p1);
                }
                k += 1;
            }
            self.eobrun -= 1;
        }
        Ok(())
    }

    /// A correction bit for a non-zero coefficient: add `p1` away from zero when set.
    fn refine(&mut self, bits: &mut BitReader, c: usize, block: usize, natural: usize, p1: i32) {
        if bits.take(1) == 0 {
            return;
        }
        let plane = &self.planes[c];
        let slot = plane.slot[natural];
        if slot < 0 {
            return; // not kept: the bit only had to be read
        }
        let at = block * plane.k * plane.k + slot as usize;
        let value = i32::from(self.planes[c].coefs[at]);
        if value & p1 == 0 {
            let value = if value >= 0 { value + p1 } else { value - p1 };
            self.planes[c].coefs[at] = value.clamp(-32768, 32767) as i16;
        }
    }

    /// Rebuild the image from the stored coefficients and scale it to `target`.
    pub(crate) fn finish(
        self,
        quant: &[[u16; 64]; 4],
        rgb: bool,
        target: (u32, u32),
    ) -> Result<Bitmap, Error> {
        let k = self.k;
        let out_w = (self.width * k).div_ceil(8);
        let out_h = (self.height * k).div_ceil(8);
        // Plane c holds c.h * plane.k samples per MCU across, against hmax * k for the image:
        // `color_row` maps x to x * h / hmax, so give it those as factors.
        let components = self
            .components
            .iter()
            .zip(&self.planes)
            .map(|(c, p)| {
                let mut scaled = *c;
                scaled.h = c.h * p.k;
                scaled.v = c.v * p.k;
                scaled
            })
            .collect();
        let frame = Frame {
            width: out_w,
            height: out_h,
            components,
            restart_interval: 0,
            rgb,
        };
        let (hmax, vmax) = (self.hmax * k, self.vmax * k);
        let strides: Vec<usize> = self
            .components
            .iter()
            .zip(&self.planes)
            .map(|(c, p)| self.mcus_x * c.h * p.k)
            .collect();
        let mut strips: Vec<Vec<u8>> = self
            .components
            .iter()
            .zip(&self.planes)
            .zip(&strides)
            .map(|((c, p), &stride)| vec![0u8; stride * c.v * p.k])
            .collect();
        // Zigzag position of each natural index, to find its quantizer.
        let mut zigzag_of = [0usize; 64];
        for (z, &n) in ZIGZAG.iter().enumerate() {
            zigzag_of[n] = z;
        }
        let mut scaler = Scaler::new(out_w as u32, out_h as u32, target);
        let mut rgba = vec![0u8; out_w * 4];
        let mut block_coefs = [0f32; 64];
        for my in 0..self.mcus_y {
            for (ci, c) in self.components.iter().enumerate() {
                let plane = &self.planes[ci];
                let pk = plane.k;
                let q = &quant[c.quant];
                for by in 0..c.v {
                    for bx in 0..plane.blocks_w {
                        let block = (my * c.v + by) * plane.blocks_w + bx;
                        let stored = &plane.coefs[block * pk * pk..(block + 1) * pk * pk];
                        for v in 0..pk {
                            for u in 0..pk {
                                let quantizer = f32::from(q[zigzag_of[v * 8 + u]]);
                                block_coefs[v * pk + u] = f32::from(stored[v * pk + u]) * quantizer;
                            }
                        }
                        let offset = by * pk * strides[ci] + bx * pk;
                        idct_scaled(
                            &block_coefs[..pk * pk],
                            pk,
                            &mut strips[ci][offset..],
                            strides[ci],
                        );
                    }
                }
                debug_assert!(plane.blocks_h >= (my + 1) * c.v);
            }
            let rows = vmax.min(out_h.saturating_sub(my * vmax));
            for r in 0..rows {
                color_row(&frame, &strips, &strides, r, hmax, vmax, &mut rgba);
                scaler.push(&rgba);
            }
        }
        scaler.finish()
    }
}
