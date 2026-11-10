//! WebP lossless (VP8L) decoding: prefix codes, backward references, the color cache, and the
//! four transforms (predictor, cross color, subtract green, color indexing).
//!
//! Backward references reach at most about a million pixels back, so the coded pixels are
//! kept in a ring of that size (or of the whole image, when smaller), not at full size. Each
//! finished row is inverse-transformed into a row of output, which goes to the caller.

use crate::Error;

const MAX_BITS: usize = 15;
const FAST_BITS: u32 = 9;
/// The farthest a backward reference can reach, in pixels.
const MAX_DISTANCE: usize = 1 << 20;

/// LSB-first bit reader.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Bits<'a> {
        Bits {
            data,
            pos: 0,
            buf: 0,
            count: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&b) = self.data.get(self.pos) else {
                return;
            };
            self.buf |= u64::from(b) << self.count;
            self.pos += 1;
            self.count += 8;
        }
    }

    #[inline]
    pub(crate) fn take(&mut self, n: u32) -> Result<u32, Error> {
        if n == 0 {
            return Ok(0);
        }
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(Error::Truncated);
            }
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.count -= n;
        Ok(v)
    }

    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.refill();
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }
}

/// A canonical prefix code. A code with a single symbol takes no bits.
struct Huffman {
    fast: Vec<u16>,
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
    single: Option<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, Error> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let used: u32 = counts.iter().map(|&c| u32::from(c)).sum();
        if used == 0 {
            return Err(Error::Corrupt("empty prefix code"));
        }
        if used == 1 {
            let symbol = lengths.iter().position(|&l| l != 0).unwrap_or(0) as u16;
            return Ok(Huffman {
                fast: Vec::new(),
                counts,
                symbols: Vec::new(),
                single: Some(symbol),
            });
        }
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err(Error::Corrupt("over-subscribed prefix code"));
            }
        }
        if left != 0 {
            return Err(Error::Corrupt("incomplete prefix code"));
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; used as usize];
        for (symbol, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offsets[usize::from(l)])] = symbol as u16;
                offsets[usize::from(l)] += 1;
            }
        }
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let (mut code, mut index) = (0u32, 0usize);
        for len in 1..=MAX_BITS as u32 {
            for _ in 0..counts[len as usize] {
                if len <= FAST_BITS {
                    let reversed = code.reverse_bits() >> (32 - len);
                    // Symbols fit 12 bits (at most 280 + a 2048-entry cache).
                    let entry = symbols[index] << 4 | len as u16;
                    let mut slot = reversed as usize;
                    while slot < fast.len() {
                        fast[slot] = entry;
                        slot += 1 << len;
                    }
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        Ok(Huffman {
            fast,
            counts,
            symbols,
            single: None,
        })
    }

    #[inline]
    fn decode(&self, bits: &mut Bits) -> Result<u16, Error> {
        if let Some(s) = self.single {
            return Ok(s);
        }
        let entry = self.fast[bits.peek(FAST_BITS) as usize];
        if entry != 0 {
            bits.take(u32::from(entry & 0xf))?;
            return Ok(entry >> 4);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::Corrupt("invalid prefix code"))
    }
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

fn read_code(bits: &mut Bits, alphabet: usize) -> Result<Huffman, Error> {
    let mut lengths = vec![0u8; alphabet];
    if bits.take(1)? == 1 {
        let two = bits.take(1)? == 1;
        let first_bits = if bits.take(1)? == 1 { 8 } else { 1 };
        let s0 = bits.take(first_bits)? as usize;
        *lengths.get_mut(s0).ok_or(Error::Corrupt("prefix symbol"))? = 1;
        if two {
            let s1 = bits.take(8)? as usize;
            *lengths.get_mut(s1).ok_or(Error::Corrupt("prefix symbol"))? = 1;
        }
        return Huffman::new(&lengths);
    }
    let mut cl_lengths = [0u8; 19];
    let n = 4 + bits.take(4)? as usize;
    for &i in &CODE_LENGTH_ORDER[..n] {
        cl_lengths[i] = bits.take(3)? as u8;
    }
    let cl = Huffman::new(&cl_lengths)?;
    let mut max_symbol = if bits.take(1)? == 1 {
        let nbits = 2 + 2 * bits.take(3)?;
        2 + bits.take(nbits)? as usize
    } else {
        alphabet
    };
    let mut previous = 8u8;
    let mut symbol = 0;
    while symbol < alphabet {
        if max_symbol == 0 {
            break;
        }
        max_symbol -= 1;
        let len = cl.decode(bits)?;
        if len < 16 {
            lengths[symbol] = len as u8;
            symbol += 1;
            if len != 0 {
                previous = len as u8;
            }
        } else {
            let (extra, offset, value) = match len {
                16 => (2, 3, previous),
                17 => (3, 3, 0),
                _ => (7, 11, 0),
            };
            let repeat = bits.take(extra)? as usize + offset;
            if symbol + repeat > alphabet {
                return Err(Error::Corrupt("prefix code lengths"));
            }
            lengths[symbol..symbol + repeat].fill(value);
            symbol += repeat;
        }
    }
    Huffman::new(&lengths)
}

/// The five codes of one group: green (with lengths and cache), red, blue, alpha, distance.
struct Group([Huffman; 5]);

fn read_group(bits: &mut Bits, cache_bits: u32) -> Result<Group, Error> {
    let cache = if cache_bits > 0 { 1 << cache_bits } else { 0 };
    Ok(Group([
        read_code(bits, 256 + 24 + cache)?,
        read_code(bits, 256)?,
        read_code(bits, 256)?,
        read_code(bits, 256)?,
        read_code(bits, 40)?,
    ]))
}

fn prefix_value(bits: &mut Bits, symbol: u32) -> Result<usize, Error> {
    if symbol < 4 {
        return Ok(symbol as usize + 1);
    }
    let extra = (symbol - 2) >> 1;
    let offset = (2 + (symbol & 1)) << extra;
    Ok((offset + bits.take(extra)?) as usize + 1)
}

/// Offsets of the first 120 distance codes, as (dx, dy) packed into one byte each.
const PLANE: [u8; 120] = [
    0x18, 0x07, 0x17, 0x19, 0x28, 0x06, 0x27, 0x29, 0x16, 0x1a, 0x26, 0x2a, 0x38, 0x05, 0x37, 0x39,
    0x15, 0x1b, 0x36, 0x3a, 0x25, 0x2b, 0x48, 0x04, 0x47, 0x49, 0x14, 0x1c, 0x35, 0x3b, 0x46, 0x4a,
    0x24, 0x2c, 0x58, 0x45, 0x4b, 0x34, 0x3c, 0x03, 0x57, 0x59, 0x13, 0x1d, 0x56, 0x5a, 0x23, 0x2d,
    0x44, 0x4c, 0x55, 0x5b, 0x33, 0x3d, 0x68, 0x02, 0x67, 0x69, 0x12, 0x1e, 0x66, 0x6a, 0x22, 0x2e,
    0x54, 0x5c, 0x43, 0x4d, 0x65, 0x6b, 0x32, 0x3e, 0x78, 0x01, 0x77, 0x79, 0x53, 0x5d, 0x11, 0x1f,
    0x64, 0x6c, 0x42, 0x4e, 0x76, 0x7a, 0x21, 0x2f, 0x75, 0x7b, 0x31, 0x3f, 0x63, 0x6d, 0x52, 0x5e,
    0x00, 0x74, 0x7c, 0x41, 0x4f, 0x10, 0x20, 0x62, 0x6e, 0x30, 0x73, 0x7d, 0x51, 0x5f, 0x40, 0x72,
    0x7e, 0x61, 0x6f, 0x50, 0x71, 0x7f, 0x60, 0x70,
];

fn plane_distance(code: usize, width: usize) -> usize {
    if code > 120 {
        return code - 120;
    }
    let v = PLANE[code - 1];
    let dy = usize::from(v >> 4);
    let dx = 8 - i64::from(v & 0xf);
    let d = dy as i64 * width as i64 + dx;
    d.max(1) as usize
}

/// Entropy-coded pixels, decoded a row at a time into a ring of `ring.len()` pixels.
struct Stream {
    width: usize,
    height: usize,
    groups: Vec<Group>,
    /// Meta prefix codes: block size bits, blocks per row, and the entropy image.
    meta: Option<(u32, usize, Vec<u32>)>,
    cache: Vec<u32>,
    cache_shift: u32,
    ring: Vec<u32>,
    /// Pixels decoded so far, and the position in the current row.
    i: usize,
    x: usize,
    y: usize,
    /// A backward reference that continues into the next row: pixels left, distance.
    pending: (usize, usize),
}

impl Stream {
    fn new(
        bits: &mut Bits,
        width: usize,
        height: usize,
        level0: bool,
        ring_len: usize,
    ) -> Result<Stream, Error> {
        let cache_bits = if bits.take(1)? == 1 {
            let b = bits.take(4)?;
            if !(1..=11).contains(&b) {
                return Err(Error::Corrupt("color cache size"));
            }
            b
        } else {
            0
        };
        let mut meta = None;
        let mut groups_n = 1;
        if level0 && bits.take(1)? == 1 {
            let b = bits.take(3)? + 2;
            let mw = width.div_ceil(1 << b);
            let image = sub_image(bits, mw, height.div_ceil(1 << b))?;
            groups_n = image.iter().map(|p| (p >> 8) & 0xffff).max().unwrap_or(0) as usize + 1;
            meta = Some((b, mw, image));
        }
        let groups = (0..groups_n)
            .map(|_| read_group(bits, cache_bits))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Stream {
            width,
            height,
            groups,
            meta,
            cache: vec![0; if cache_bits > 0 { 1 << cache_bits } else { 0 }],
            cache_shift: 32 - cache_bits,
            ring: vec![0; ring_len.max(width).max(1)],
            i: 0,
            x: 0,
            y: 0,
            pending: (0, 0),
        })
    }

    fn group(&self) -> &Group {
        let g = match &self.meta {
            Some((b, mw, image)) => {
                ((image[(self.y >> b) * mw + (self.x >> b)] >> 8) & 0xffff) as usize
            }
            None => 0,
        };
        &self.groups[g.min(self.groups.len() - 1)]
    }

    #[inline]
    fn put(&mut self, p: u32) {
        let n = self.ring.len();
        self.ring[self.i % n] = p;
        if !self.cache.is_empty() {
            self.cache[(p.wrapping_mul(0x1e35_a7bd) >> self.cache_shift) as usize] = p;
        }
        self.i += 1;
        self.x += 1;
    }

    /// Decode the next row. Returns the ring index of its first pixel.
    fn next_row(&mut self, bits: &mut Bits) -> Result<usize, Error> {
        if self.y >= self.height {
            return Err(Error::Corrupt("VP8L reads past the image"));
        }
        let start = self.i;
        let n = self.ring.len();
        while self.x < self.width {
            if self.pending.0 > 0 {
                let (len, dist) = self.pending;
                let take = len.min(self.width - self.x);
                for _ in 0..take {
                    let p = self.ring[(self.i - dist) % n];
                    self.put(p);
                }
                self.pending.0 -= take;
                continue;
            }
            let group = self.group();
            let g = u32::from(group.0[0].decode(bits)?);
            if g < 256 {
                let r = u32::from(group.0[1].decode(bits)?);
                let b = u32::from(group.0[2].decode(bits)?);
                let a = u32::from(group.0[3].decode(bits)?);
                self.put(a << 24 | r << 16 | g << 8 | b);
            } else if g < 280 {
                let len = prefix_value(bits, g - 256)?;
                let dsym = u32::from(group.0[4].decode(bits)?);
                let dist = plane_distance(prefix_value(bits, dsym)?, self.width);
                let total = self.width * self.height;
                if dist > self.i || dist > n || len > total - self.i {
                    return Err(Error::Corrupt("backward reference"));
                }
                self.pending = (len, dist);
            } else {
                let p = *self
                    .cache
                    .get((g - 280) as usize)
                    .ok_or(Error::Corrupt("color cache"))?;
                self.put(p);
            }
        }
        self.x = 0;
        self.y += 1;
        Ok(start)
    }
}

/// A small image (transform data, entropy image, palette) decoded whole.
fn sub_image(bits: &mut Bits, width: usize, height: usize) -> Result<Vec<u32>, Error> {
    let total = width * height;
    if total > 1 << 24 {
        return Err(Error::TooLarge);
    }
    let mut s = Stream::new(bits, width, height, false, total)?;
    for _ in 0..height {
        s.next_row(bits)?;
    }
    Ok(s.ring)
}

enum Transform {
    Predictor {
        bits: u32,
        width: usize,
        data: Vec<u32>,
    },
    CrossColor {
        bits: u32,
        width: usize,
        data: Vec<u32>,
    },
    SubtractGreen,
    ColorIndexing {
        bits: u32,
        width: usize,
        palette: Vec<u32>,
    },
}

/// A VP8L image, decoded a row of ARGB at a time.
pub(crate) struct Decoder<'a> {
    bits: Bits<'a>,
    stream: Stream,
    transforms: Vec<Transform>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// The previous output row of each predictor transform.
    previous: Vec<Vec<u32>>,
    a: Vec<u32>,
    b: Vec<u32>,
    y: usize,
}

impl<'a> Decoder<'a> {
    /// `size` is given for alpha data, whose stream has no header.
    pub(crate) fn new(data: &'a [u8], size: Option<(usize, usize)>) -> Result<Decoder<'a>, Error> {
        let mut bits = Bits::new(data);
        let (width, height) = match size {
            Some(size) => size,
            None => {
                if bits.take(8)? != 0x2f {
                    return Err(Error::Corrupt("VP8L signature"));
                }
                let w = bits.take(14)? as usize + 1;
                let h = bits.take(14)? as usize + 1;
                bits.take(1)?;
                if bits.take(3)? != 0 {
                    return Err(Error::Unsupported("VP8L version"));
                }
                (w, h)
            }
        };
        let mut transforms = Vec::new();
        let mut seen = 0u8;
        let mut coded_width = width;
        while bits.take(1)? == 1 {
            let kind = bits.take(2)?;
            if seen & (1 << kind) != 0 {
                return Err(Error::Corrupt("repeated transform"));
            }
            seen |= 1 << kind;
            let t = match kind {
                0 | 1 => {
                    let b = bits.take(3)? + 2;
                    let w = coded_width.div_ceil(1 << b);
                    let data = sub_image(&mut bits, w, height.div_ceil(1 << b))?;
                    let width = coded_width;
                    if kind == 0 {
                        Transform::Predictor {
                            bits: b,
                            width,
                            data,
                        }
                    } else {
                        Transform::CrossColor {
                            bits: b,
                            width,
                            data,
                        }
                    }
                }
                2 => Transform::SubtractGreen,
                _ => {
                    let n = bits.take(8)? as usize + 1;
                    let mut palette = sub_image(&mut bits, n, 1)?;
                    for i in 1..n {
                        palette[i] = add_pixels(palette[i - 1], palette[i]);
                    }
                    let b = match n {
                        0..=2 => 3,
                        3..=4 => 2,
                        5..=16 => 1,
                        _ => 0,
                    };
                    let width = coded_width;
                    coded_width = coded_width.div_ceil(1 << b);
                    Transform::ColorIndexing {
                        bits: b,
                        width,
                        palette,
                    }
                }
            };
            transforms.push(t);
        }
        let ring_len = (coded_width * height).min(MAX_DISTANCE + coded_width + 8);
        let stream = Stream::new(&mut bits, coded_width, height, true, ring_len)?;
        let previous = transforms
            .iter()
            .map(|t| match t {
                Transform::Predictor { width, .. } => vec![0u32; *width],
                _ => Vec::new(),
            })
            .collect();
        Ok(Decoder {
            bits,
            stream,
            transforms,
            width,
            height,
            previous,
            a: vec![0; width],
            b: vec![0; width],
            y: 0,
        })
    }

    /// The next row of ARGB pixels.
    pub(crate) fn next_row(&mut self) -> Result<&[u32], Error> {
        let start = self.stream.next_row(&mut self.bits)?;
        let y = self.y;
        self.y += 1;
        let ring = &self.stream.ring;
        let mut w = self.stream.width;
        for x in 0..w {
            self.a[x] = ring[(start + x) % ring.len()];
        }
        for (ti, t) in self.transforms.iter().enumerate().rev() {
            match t {
                Transform::SubtractGreen => {
                    for p in &mut self.a[..w] {
                        let g = (*p >> 8) & 0xff;
                        let r = ((*p >> 16) + g) & 0xff;
                        let b = (*p + g) & 0xff;
                        *p = (*p & 0xff00_ff00) | r << 16 | b;
                    }
                }
                Transform::CrossColor { bits, width, data } => {
                    let row = &data[(y >> bits) * width.div_ceil(1 << bits)..];
                    for (x, p) in self.a[..w].iter_mut().enumerate() {
                        let m = row[x >> bits];
                        let g = (*p >> 8) as u8 as i8;
                        let r = ((*p >> 16) as u8).wrapping_add(delta(m as u8 as i8, g));
                        let b = (*p as u8)
                            .wrapping_add(delta((m >> 8) as u8 as i8, g))
                            .wrapping_add(delta((m >> 16) as u8 as i8, r as i8));
                        *p = (*p & 0xff00_ff00) | u32::from(r) << 16 | u32::from(b);
                    }
                }
                Transform::Predictor { bits, width, data } => {
                    let modes = &data[(y >> bits) * width.div_ceil(1 << bits)..];
                    let top = &mut self.previous[ti];
                    for x in 0..w {
                        let predicted = if y == 0 {
                            if x == 0 {
                                0xff00_0000
                            } else {
                                self.a[x - 1]
                            }
                        } else if x == 0 {
                            top[0]
                        } else {
                            let mode = (modes[x >> bits] >> 8) & 0xf;
                            // The rightmost pixel's top-right is the leftmost pixel of the
                            // current row.
                            let tr = if x + 1 < w { top[x + 1] } else { self.a[0] };
                            predict(mode, self.a[x - 1], top[x], top[x - 1], tr)
                        };
                        self.a[x] = add_pixels(self.a[x], predicted);
                    }
                    top[..w].copy_from_slice(&self.a[..w]);
                }
                Transform::ColorIndexing {
                    bits,
                    width,
                    palette,
                } => {
                    let per = 1usize << bits;
                    let size = 8 >> bits;
                    let mask = (1u32 << size) - 1;
                    for x in 0..*width {
                        let packed = (self.a[x / per] >> 8) & 0xff;
                        let index = (packed >> ((x % per) * size)) & mask;
                        self.b[x] = palette.get(index as usize).copied().unwrap_or(0);
                    }
                    std::mem::swap(&mut self.a, &mut self.b);
                    w = *width;
                }
            }
        }
        Ok(&self.a[..self.width])
    }
}

fn delta(t: i8, c: i8) -> u8 {
    ((i32::from(t) * i32::from(c)) >> 5) as u8
}

fn add_pixels(a: u32, b: u32) -> u32 {
    let ag = (a & 0xff00_ff00).wrapping_add(b & 0xff00_ff00) & 0xff00_ff00;
    let rb = (a & 0x00ff_00ff).wrapping_add(b & 0x00ff_00ff) & 0x00ff_00ff;
    ag | rb
}

fn average2(a: u32, b: u32) -> u32 {
    (((a ^ b) & 0xfefe_fefe) >> 1) + (a & b)
}

fn channels(p: u32) -> [i32; 4] {
    [
        (p >> 24) as i32,
        ((p >> 16) & 0xff) as i32,
        ((p >> 8) & 0xff) as i32,
        (p & 0xff) as i32,
    ]
}

fn pack(c: [i32; 4]) -> u32 {
    (c[0].clamp(0, 255) as u32) << 24
        | (c[1].clamp(0, 255) as u32) << 16
        | (c[2].clamp(0, 255) as u32) << 8
        | c[3].clamp(0, 255) as u32
}

fn predict(mode: u32, l: u32, t: u32, tl: u32, tr: u32) -> u32 {
    match mode {
        0 => 0xff00_0000,
        1 => l,
        2 => t,
        3 => tr,
        4 => tl,
        5 => average2(average2(l, tr), t),
        6 => average2(l, tl),
        7 => average2(l, t),
        8 => average2(tl, t),
        9 => average2(t, tr),
        10 => average2(average2(l, tl), average2(t, tr)),
        11 => {
            // Select: whichever of left and top is closer to the gradient guess.
            let (cl, ct, ctl) = (channels(l), channels(t), channels(tl));
            let pl: i32 = (0..4).map(|i| (ct[i] - ctl[i]).abs()).sum();
            let pt: i32 = (0..4).map(|i| (cl[i] - ctl[i]).abs()).sum();
            if pl < pt {
                l
            } else {
                t
            }
        }
        12 => {
            let (cl, ct, ctl) = (channels(l), channels(t), channels(tl));
            pack([0, 1, 2, 3].map(|i| cl[i] + ct[i] - ctl[i]))
        }
        13 => {
            let a = channels(average2(l, t));
            let ctl = channels(tl);
            pack([0, 1, 2, 3].map(|i| a[i] + (a[i] - ctl[i]) / 2))
        }
        _ => 0xff00_0000,
    }
}
