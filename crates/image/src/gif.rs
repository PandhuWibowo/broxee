//! GIF decoding (GIF87a and GIF89a): the first frame, placed on the logical screen, with its
//! transparent index. Animation is not played. Rows are decoded from the LZW stream one at a
//! time and handed to the `Scaler`; interlaced frames are first assembled as one palette index
//! per pixel, so they are capped like interlaced PNGs.

use crate::{Bitmap, Error, Scaler, MAX_SIDE};

/// Interlaced frames are assembled at one byte per pixel before scaling.
const MAX_INTERLACED_PIXELS: u64 = 16 << 20;

pub(crate) fn decode(data: &[u8], target: (u32, u32)) -> Result<Bitmap, Error> {
    let lsd = data.get(6..13).ok_or(Error::Truncated)?;
    let width = u32::from(u16::from_le_bytes([lsd[0], lsd[1]]));
    let height = u32::from(u16::from_le_bytes([lsd[2], lsd[3]]));
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(Error::TooLarge);
    }
    let mut at = 13;
    let mut global: &[u8] = &[];
    if lsd[4] & 0x80 != 0 {
        let len = 3 << ((lsd[4] & 7) + 1);
        global = data.get(at..at + len).ok_or(Error::Truncated)?;
        at += len;
    }
    let mut transparent: Option<u8> = None;
    loop {
        match *data.get(at).ok_or(Error::Truncated)? {
            0x21 => {
                let label = *data.get(at + 1).ok_or(Error::Truncated)?;
                at += 2;
                if label == 0xf9 {
                    let body = data.get(at..at + 5).ok_or(Error::Truncated)?;
                    if body[0] >= 4 && body[1] & 1 != 0 {
                        transparent = Some(body[4]);
                    }
                }
                at = skip_sub_blocks(data, at)?;
            }
            0x2c => {
                let d = data.get(at + 1..at + 10).ok_or(Error::Truncated)?;
                at += 10;
                let frame = Frame {
                    left: u32::from(u16::from_le_bytes([d[0], d[1]])),
                    top: u32::from(u16::from_le_bytes([d[2], d[3]])),
                    width: u32::from(u16::from_le_bytes([d[4], d[5]])),
                    height: u32::from(u16::from_le_bytes([d[6], d[7]])),
                    interlaced: d[8] & 0x40 != 0,
                };
                let mut palette = global;
                if d[8] & 0x80 != 0 {
                    let len = 3 << ((d[8] & 7) + 1);
                    palette = data.get(at..at + len).ok_or(Error::Truncated)?;
                    at += len;
                }
                if palette.is_empty() {
                    return Err(Error::Corrupt("GIF without a palette"));
                }
                let min_code = *data.get(at).ok_or(Error::Truncated)?;
                if !(1..=11).contains(&min_code) {
                    return Err(Error::Corrupt("GIF code size"));
                }
                let lzw = Lzw::new(data, at + 1, min_code);
                let canvas = Canvas {
                    width,
                    height,
                    palette,
                    transparent,
                };
                return canvas.draw(&frame, lzw, target);
            }
            0x3b => return Err(Error::Corrupt("GIF without an image")),
            _ => return Err(Error::Corrupt("GIF block")),
        }
    }
}

fn skip_sub_blocks(data: &[u8], mut at: usize) -> Result<usize, Error> {
    loop {
        let len = usize::from(*data.get(at).ok_or(Error::Truncated)?);
        at += 1 + len;
        if len == 0 {
            return Ok(at);
        }
    }
}

struct Frame {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    interlaced: bool,
}

struct Canvas<'a> {
    width: u32,
    height: u32,
    palette: &'a [u8],
    transparent: Option<u8>,
}

impl Canvas<'_> {
    fn draw(&self, frame: &Frame, mut lzw: Lzw, target: (u32, u32)) -> Result<Bitmap, Error> {
        let fw = frame.width as usize;
        let mut indices = vec![0u8; fw];
        let assembled = if frame.interlaced {
            if u64::from(frame.width) * u64::from(frame.height) > MAX_INTERLACED_PIXELS {
                return Err(Error::TooLarge);
            }
            let mut all = vec![0u8; fw * frame.height as usize];
            for y in interlaced_rows(frame.height) {
                let at = y as usize * fw;
                lzw.fill(&mut all[at..at + fw]);
            }
            all
        } else {
            Vec::new()
        };
        let mut scaler = Scaler::new(self.width, self.height, target);
        let mut row = vec![0u8; self.width as usize * 4];
        for y in 0..self.height {
            row.fill(0);
            if y >= frame.top && y - frame.top < frame.height {
                let fy = (y - frame.top) as usize;
                let src: &[u8] = if frame.interlaced {
                    &assembled[fy * fw..fy * fw + fw]
                } else {
                    lzw.fill(&mut indices);
                    &indices
                };
                for (fx, &index) in src.iter().enumerate() {
                    let x = frame.left as usize + fx;
                    if x >= self.width as usize || Some(index) == self.transparent {
                        continue;
                    }
                    let p = usize::from(index) * 3;
                    if let Some(rgb) = self.palette.get(p..p + 3) {
                        row[x * 4..x * 4 + 3].copy_from_slice(rgb);
                        row[x * 4 + 3] = 255;
                    }
                }
            }
            scaler.push(&row);
        }
        scaler.finish()
    }
}

/// Row order of an interlaced frame: every 8th from 0, every 8th from 4, every 4th from 2,
/// every 2nd from 1.
fn interlaced_rows(height: u32) -> impl Iterator<Item = u32> {
    [(0, 8), (4, 8), (2, 4), (1, 2)]
        .into_iter()
        .flat_map(move |(start, step)| (start..height).step_by(step))
}

/// LZW decoder reading codes straight out of the data sub-blocks.
struct Lzw<'a> {
    data: &'a [u8],
    /// Next unread byte of the current sub-block, and where that sub-block ends.
    at: usize,
    block_end: usize,
    bits: u32,
    nbits: u32,
    min_code: u8,
    code_size: u32,
    clear: u16,
    next: u16,
    previous: Option<u16>,
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// First byte of each code's string, to add new codes without walking the chain.
    first: Vec<u8>,
    /// The tail of the last string, waiting to be handed out (reversed).
    pending: Vec<u8>,
    done: bool,
}

impl<'a> Lzw<'a> {
    fn new(data: &'a [u8], at: usize, min_code: u8) -> Lzw<'a> {
        let clear = 1u16 << min_code;
        let mut prefix = vec![0u16; 4096];
        let mut suffix = vec![0u8; 4096];
        let mut first = vec![0u8; 4096];
        for i in 0..clear {
            prefix[usize::from(i)] = u16::MAX;
            suffix[usize::from(i)] = i as u8;
            first[usize::from(i)] = i as u8;
        }
        Lzw {
            data,
            at,
            block_end: at,
            bits: 0,
            nbits: 0,
            min_code,
            code_size: u32::from(min_code) + 1,
            clear,
            next: clear + 2,
            previous: None,
            prefix,
            suffix,
            first,
            pending: Vec::with_capacity(4096),
            done: false,
        }
    }

    fn byte(&mut self) -> Option<u8> {
        if self.at == self.block_end {
            let len = usize::from(*self.data.get(self.at)?);
            if len == 0 {
                return None;
            }
            self.at += 1;
            self.block_end = self.at + len;
        }
        let b = *self.data.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    fn code(&mut self) -> Option<u16> {
        while self.nbits < self.code_size {
            self.bits |= u32::from(self.byte()?) << self.nbits;
            self.nbits += 8;
        }
        let code = (self.bits & ((1 << self.code_size) - 1)) as u16;
        self.bits >>= self.code_size;
        self.nbits -= self.code_size;
        Some(code)
    }

    /// Fill `out` with palette indices. A stream that ends early leaves zeros, so a truncated
    /// file still shows what arrived.
    fn fill(&mut self, out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            if let Some(b) = self.pending.pop() {
                out[i] = b;
                i += 1;
                continue;
            }
            if self.done || !self.step() {
                self.done = true;
                out[i..].fill(0);
                return;
            }
        }
    }

    /// Decode one code into `pending`. False at the end of the stream.
    fn step(&mut self) -> bool {
        let Some(code) = self.code() else {
            return false;
        };
        if code == self.clear {
            self.code_size = u32::from(self.min_code) + 1;
            self.next = self.clear + 2;
            self.previous = None;
            return true;
        }
        if code == self.clear + 1 {
            return false;
        }
        let Some(previous) = self.previous else {
            if code >= self.clear {
                return false;
            }
            self.pending.push(code as u8);
            self.previous = Some(code);
            return true;
        };
        let walk_from = if code < self.next {
            code
        } else if code == self.next {
            // KwKwK: the string of `previous` followed by its own first byte.
            self.pending.push(self.first[usize::from(previous)]);
            previous
        } else {
            return false;
        };
        let mut c = walk_from;
        while c != u16::MAX && self.pending.len() < 4096 {
            self.pending.push(self.suffix[usize::from(c)]);
            c = self.prefix[usize::from(c)];
        }
        if self.next < 4096 {
            let n = usize::from(self.next);
            self.prefix[n] = previous;
            self.first[n] = self.first[usize::from(previous)];
            self.suffix[n] = self.first[usize::from(walk_from)];
            self.next += 1;
            if u32::from(self.next) == 1 << self.code_size && self.code_size < 12 {
                self.code_size += 1;
            }
        }
        self.previous = Some(code);
        true
    }
}

#[cfg(test)]
mod tests {
    fn read_pnm(data: &[u8]) -> (u32, u32, &[u8]) {
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while data[at].is_ascii_whitespace() {
                at += 1;
            }
            let start = at;
            while !data[at].is_ascii_whitespace() {
                at += 1;
            }
            fields.push(std::str::from_utf8(&data[start..at]).unwrap().to_string());
        }
        (
            fields[1].parse().unwrap(),
            fields[2].parse().unwrap(),
            &data[at + 1..],
        )
    }

    #[test]
    fn decodes_a_photo_like_the_reference() {
        // photo.gif and its reference were made with ffmpeg from photo.ppm.
        let gif = include_bytes!("testdata/photo.gif");
        let (w, h, rgb) = read_pnm(include_bytes!("testdata/photo_gif.ref.ppm"));
        let b = crate::decode(gif, w, h).unwrap();
        assert_eq!((b.width, b.height), (w, h));
        for (px, reference) in b.pixels.chunks(4).zip(rgb.chunks(3)) {
            assert_eq!(&px[..3], reference);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn interlaced_matches_sequential() {
        let a = crate::decode(include_bytes!("testdata/photo.gif"), 64, 64).unwrap();
        let b = crate::decode(include_bytes!("testdata/photo_interlaced.gif"), 64, 64).unwrap();
        assert_eq!(a, b);
    }

    /// A 3x2 GIF with a transparent index, drawn at an offset on a 4x3 screen. Its LZW stream
    /// is written by hand with no compression: every code is a literal, and a clear code
    /// comes before the table would grow the code size.
    fn tiny_gif() -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[4, 0, 3, 0, 0x80, 0, 0]); // 4x3, 2-entry global palette
        g.extend_from_slice(&[255, 0, 0, 0, 0, 255]); // red, blue
        g.extend_from_slice(&[0x21, 0xf9, 4, 1, 0, 0, 1, 0]); // index 1 is transparent
        g.extend_from_slice(&[0x2c, 1, 0, 1, 0, 3, 0, 2, 0, 0]); // 3x2 at (1, 1)
        g.push(2); // minimum code size: codes start at 3 bits
                   // clear(4), 0, 1, clear, 0, 1, clear, 0, 0, end(5): 3 bits each, LSB first.
        let codes = [4u32, 0, 1, 4, 0, 1, 4, 0, 0, 5];
        let mut bits = 0u64;
        for (i, c) in codes.iter().enumerate() {
            bits |= u64::from(*c) << (i * 3);
        }
        let bytes: Vec<u8> = (0..(codes.len() * 3).div_ceil(8))
            .map(|i| (bits >> (i * 8)) as u8)
            .collect();
        g.push(bytes.len() as u8);
        g.extend_from_slice(&bytes);
        g.extend_from_slice(&[0, 0x3b]);
        g
    }

    #[test]
    fn places_the_frame_and_keeps_transparency() {
        let b = crate::decode(&tiny_gif(), 4, 3).unwrap();
        let px = |x: usize, y: usize| &b.pixels[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(0, 0), [0, 0, 0, 0], "outside the frame");
        assert_eq!(px(1, 1), [255, 0, 0, 255]);
        assert_eq!(px(2, 1), [0, 0, 0, 0], "transparent index");
        assert_eq!(px(3, 1), [255, 0, 0, 255]);
        assert_eq!(px(1, 2), [0, 0, 0, 0]);
        assert_eq!(px(2, 2), [255, 0, 0, 255]);
    }

    #[test]
    fn survives_damage() {
        let gif = include_bytes!("testdata/photo.gif");
        for len in [0, 6, 13, 20, 100, 800, gif.len() - 3] {
            let _ = crate::decode(&gif[..len], 32, 32);
        }
        let mut bad = gif.to_vec();
        for i in (13..bad.len()).step_by(7) {
            bad[i] = bad[i].wrapping_mul(31).wrapping_add(7);
        }
        let _ = crate::decode(&bad, 48, 48);
    }
}
