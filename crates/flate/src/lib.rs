//! DEFLATE decompression (RFC 1951) with its zlib (RFC 1950) and gzip (RFC 1952) wrappers, and
//! the CRC-32 and Adler-32 checksums they use. Written for Broxee: HTTP `Content-Encoding`
//! and PNG image data both arrive deflated.
//!
//! Decoding is table driven: codes up to `FAST_BITS` long resolve with one lookup, longer ones
//! fall back to walking the canonical code lengths. Output is capped by the caller, so a small
//! compressed body cannot expand into gigabytes.

mod brotli;
mod brotli_tables;

pub use brotli::brotli;

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The input ended in the middle of the stream.
    Truncated,
    /// The stream is not valid DEFLATE / zlib / gzip.
    Corrupt(&'static str),
    /// The output would exceed the caller's limit.
    TooLarge,
    /// The stored checksum does not match the data.
    Checksum,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "compressed data is truncated"),
            Error::Corrupt(why) => write!(f, "corrupt compressed data: {why}"),
            Error::TooLarge => write!(f, "decompressed data exceeds the size limit"),
            Error::Checksum => write!(f, "checksum mismatch in compressed data"),
        }
    }
}

/// CRC-32 tables for slicing-by-8 (as zlib does): `TABLES[0]` is the classic byte table, and
/// `TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes. Eight input bytes then cost
/// eight independent lookups instead of eight dependent steps. 8 KiB, built at compile time.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut t = 1;
        while t < 8 {
            let previous = tables[t - 1][n];
            tables[t][n] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            t += 1;
        }
        n += 1;
    }
    tables
};

/// CRC-32 (ISO 3309) over the concatenation of `parts`.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    parts.iter().fold(0, |crc, part| crc32_update(crc, part))
}

/// Continue a CRC-32 over more data (start from 0), as zlib's `crc32()` does.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the CPU has the ARMv8 CRC32 instructions, checked just above.
        return unsafe { crc32_arm(crc, data) };
    }
    crc32_tables(crc, data)
}

/// ARMv8's CRC32 instructions compute this same CRC (the reflected 0xEDB88320 polynomial, not
/// CRC-32C), 8 bytes per instruction. Every Apple Silicon Mac has them.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32_arm(crc: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32b, __crc32d};
    let mut c = !crc;
    let (chunks, rest) = data.as_chunks::<8>();
    for chunk in chunks {
        c = __crc32d(c, u64::from_le_bytes(*chunk));
    }
    for &b in rest {
        c = __crc32b(c, b);
    }
    !c
}

fn crc32_tables(crc: u32, data: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let mut c = !crc;
    let (chunks, rest) = data.as_chunks::<8>();
    for chunk in chunks {
        let lo = c ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in rest {
        c = t[0][((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

pub fn adler32(data: &[u8]) -> u32 {
    adler32_update(1, data)
}

/// Continue an Adler-32 over more data (start from 1).
pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let (mut a, mut b) = (adler & 0xffff, adler >> 16);
    // 5552 is the most bytes that can be summed before `b` could overflow.
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// Decompress a gzip member (`Content-Encoding: gzip`).
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    if data.len() < 18 {
        return Err(Error::Truncated);
    }
    if data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err(Error::Corrupt("not a gzip stream"));
    }
    let flags = data[3];
    let mut at = 10;
    let skip_zero_terminated = |at: usize| -> Result<usize, Error> {
        let end = data[at..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::Truncated)?;
        Ok(at + end + 1)
    };
    if flags & 0x04 != 0 {
        let extra = usize::from(u16::from_le_bytes([
            *data.get(at).ok_or(Error::Truncated)?,
            *data.get(at + 1).ok_or(Error::Truncated)?,
        ]));
        at += 2 + extra;
    }
    if flags & 0x08 != 0 {
        at = skip_zero_terminated(at.min(data.len()))?;
    }
    if flags & 0x10 != 0 {
        at = skip_zero_terminated(at.min(data.len()))?;
    }
    if flags & 0x02 != 0 {
        at += 2;
    }
    let body = data.get(at..).ok_or(Error::Truncated)?;
    // The trailer ends with the uncompressed size (mod 4 GiB): allocate the output once.
    let size_hint = data
        .len()
        .checked_sub(4)
        .map(|i| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize);
    let (out, used) = inflate_with_len(body, limit, None, size_hint)?;
    let trailer = body.get(used..used + 8).ok_or(Error::Truncated)?;
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crc != crc32(&[&out]) || size != out.len() as u32 {
        return Err(Error::Checksum);
    }
    Ok(out)
}

/// Decompress a zlib stream (PNG `IDAT`, and what most servers send as `deflate`).
pub fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    zlib_header(data)?;
    let (out, used) = inflate_with_len(&data[2..], limit, None, None)?;
    check_adler(data, used, adler32(&out))?;
    Ok(out)
}

/// Decompress a zlib stream, handing the output to `sink` in pieces instead of keeping it:
/// only the 32 KiB window DEFLATE refers back into stays in memory. PNG decoding uses this to
/// turn rows into pixels as they arrive.
pub fn zlib_stream(
    data: &[u8],
    limit: usize,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    zlib_header(data)?;
    let mut adler = 1;
    let mut counting = |bytes: &[u8]| {
        adler = adler32_update(adler, bytes);
        sink(bytes)
    };
    let (_, used) = inflate_with_len(&data[2..], limit, Some(&mut counting), None)?;
    check_adler(data, used, adler)
}

fn zlib_header(data: &[u8]) -> Result<(), Error> {
    if data.len() < 6 {
        return Err(Error::Truncated);
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err(Error::Corrupt("not a zlib stream"));
    }
    if flg & 0x20 != 0 {
        return Err(Error::Corrupt("zlib preset dictionary"));
    }
    Ok(())
}

fn check_adler(data: &[u8], used: usize, adler: u32) -> Result<(), Error> {
    let trailer = data.get(2 + used..2 + used + 4).ok_or(Error::Truncated)?;
    if u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) != adler {
        return Err(Error::Checksum);
    }
    Ok(())
}

/// Decompress raw DEFLATE data.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    inflate_with_len(data, limit, None, None).map(|(out, _)| out)
}

type Sink<'s> = &'s mut dyn FnMut(&[u8]) -> Result<(), Error>;

/// Back-references reach at most this far, so a streaming inflater keeps only this much.
const WINDOW: usize = 32 * 1024;

/// Decompress raw DEFLATE data; also returns how many input bytes the stream used. With a
/// sink, output goes there in pieces and the returned buffer is empty.
/// `size_hint`: the expected output size, when the container says (gzip's trailer).
fn inflate_with_len(
    data: &[u8],
    limit: usize,
    sink: Option<Sink>,
    size_hint: Option<usize>,
) -> Result<(Vec<u8>, usize), Error> {
    let capacity = match (&sink, size_hint) {
        (Some(_), _) => 3 * WINDOW,
        (None, Some(size)) => size.min(limit),
        (None, None) => (data.len() * 4).min(limit),
    };
    let mut inflater = Inflater {
        bits: Bits::new(data),
        out: Vec::with_capacity(capacity),
        limit,
        flushed: 0,
        sink,
    };
    loop {
        let last = inflater.bits.take(1)? == 1;
        match inflater.bits.take(2)? {
            0 => inflater.stored()?,
            1 => inflater.codes(&FIXED.0, &FIXED.1)?,
            2 => {
                let (lit, dist) = inflater.dynamic_tables()?;
                inflater.codes(&lit, &dist)?;
            }
            _ => return Err(Error::Corrupt("reserved block type")),
        }
        if last {
            break;
        }
    }
    inflater.flush(0)?;
    let used = inflater.bits.bytes_used();
    Ok((inflater.out, used))
}

/// Append `len` bytes copied from `distance` bytes back (a DEFLATE or Brotli match). Matches
/// are mostly a few to a few dozen bytes: 8-byte chunks beat a `memmove` call per match.
#[inline]
pub(crate) fn copy_back(out: &mut Vec<u8>, distance: usize, len: usize) {
    let start = out.len() - distance;
    if distance == 1 {
        // A run of one byte (spaces, zeros).
        let b = out[start];
        out.resize(out.len() + len, b);
    } else if distance >= 8 {
        // Each 8-byte chunk lies wholly in bytes already written, overlapping or not.
        let mut from = start;
        let end = start + len;
        while from + 8 <= end {
            let chunk: [u8; 8] = out[from..from + 8].try_into().unwrap_or([0; 8]);
            out.extend_from_slice(&chunk);
            from += 8;
        }
        while from < end {
            let b = out[from];
            out.push(b);
            from += 1;
        }
    } else {
        // Short repeating patterns.
        for k in 0..len {
            let b = out[start + k];
            out.push(b);
        }
    }
}

/// Little-endian bit reader.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits {
            data,
            pos: 0,
            buf: 0,
            count: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        // Eight bytes at once while there are eight to read: OR them in above the bits held,
        // and count only the whole bytes that fit. The bytes past those are ORed in again,
        // unchanged, next time, so the extra bits do no harm.
        if let Some(chunk) = self.data.get(self.pos..self.pos + 8) {
            let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
            self.buf |= word << self.count;
            let bytes = (63 - self.count) >> 3;
            self.pos += bytes as usize;
            self.count += bytes * 8;
            return;
        }
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
    fn take(&mut self, n: u32) -> Result<u32, Error> {
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

    /// The next `n` bits without consuming them; missing bits read as zero.
    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.refill();
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    /// Forget bits read ahead of `count`. Needed before moving `pos` directly (raw bytes):
    /// the word-at-a-time refill relies on them matching the bytes that follow.
    fn discard_ahead(&mut self) {
        self.buf &= (1u64 << self.count).wrapping_sub(1);
    }

    fn align_to_byte(&mut self) {
        let drop = self.count % 8;
        self.buf >>= drop;
        self.count -= drop;
    }

    fn bytes_used(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }
}

const FAST_BITS: u32 = 9;
const MAX_BITS: usize = 15;

/// A canonical Huffman code. `fast` maps the next `FAST_BITS` input bits to
/// `symbol << 4 | length` for codes that short; 0 means "take the slow path".
struct Huffman {
    fast: Vec<u16>,
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, Error> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        // Over-subscribed codes are invalid; incomplete ones are allowed (single-code trees).
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err(Error::Corrupt("over-subscribed Huffman code"));
            }
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offsets[usize::from(l)])] = symbol as u16;
                offsets[usize::from(l)] += 1;
            }
        }

        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut code = 0u32;
        let mut index = 0usize;
        for len in 1..=MAX_BITS as u32 {
            for _ in 0..counts[len as usize] {
                if len <= FAST_BITS {
                    // Codes are sent most significant bit first, but read least significant first.
                    let reversed = code.reverse_bits() >> (32 - len);
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
        })
    }

    #[inline]
    fn decode(&self, bits: &mut Bits) -> Result<u16, Error> {
        let entry = self.fast[bits.peek(FAST_BITS) as usize];
        if entry != 0 {
            bits.take(u32::from(entry & 0xf))?;
            return Ok(entry >> 4);
        }
        // Slow path: walk the lengths one bit at a time (as zlib's puff does).
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
        Err(Error::Corrupt("invalid Huffman code"))
    }
}

/// The fixed literal/length and distance codes of block type 1.
static FIXED: std::sync::LazyLock<(Huffman, Huffman)> = std::sync::LazyLock::new(|| {
    let mut lit = [0u8; 288];
    lit[..144].fill(8);
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    lit[280..].fill(8);
    (
        Huffman::new(&lit).expect("fixed code is valid"),
        Huffman::new(&[5u8; 30]).expect("fixed code is valid"),
    )
});

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order in which code-length code lengths are sent.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

struct Inflater<'a, 's> {
    bits: Bits<'a>,
    out: Vec<u8>,
    limit: usize,
    /// Bytes already handed to the sink.
    flushed: usize,
    sink: Option<Sink<'s>>,
}

impl Inflater<'_, '_> {
    fn room(&self, n: usize) -> Result<(), Error> {
        if self.flushed + self.out.len() + n > self.limit {
            Err(Error::TooLarge)
        } else {
            Ok(())
        }
    }

    /// Hand all but the last `keep` bytes to the sink, if there is one.
    fn flush(&mut self, keep: usize) -> Result<(), Error> {
        if let Some(sink) = self.sink.as_mut() {
            if self.out.len() > keep {
                let n = self.out.len() - keep;
                sink(&self.out[..n])?;
                self.out.drain(..n);
                self.flushed += n;
            }
        }
        Ok(())
    }

    fn stored(&mut self) -> Result<(), Error> {
        self.bits.align_to_byte();
        let len = self.bits.take(16)? as usize;
        let nlen = self.bits.take(16)? as usize;
        if len != !nlen & 0xffff {
            return Err(Error::Corrupt("stored block length"));
        }
        self.room(len)?;
        // Whole bytes still in the bit buffer come first.
        let mut remaining = len;
        while remaining > 0 && self.bits.count >= 8 {
            let b = self.bits.take(8)? as u8;
            self.out.push(b);
            remaining -= 1;
        }
        self.bits.discard_ahead();
        let start = self.bits.pos;
        let bytes = self
            .bits
            .data
            .get(start..start + remaining)
            .ok_or(Error::Truncated)?;
        self.out.extend_from_slice(bytes);
        self.bits.pos += remaining;
        if self.out.len() >= 2 * WINDOW {
            self.flush(WINDOW)?;
        }
        Ok(())
    }

    fn dynamic_tables(&mut self) -> Result<(Huffman, Huffman), Error> {
        let hlit = self.bits.take(5)? as usize + 257;
        let hdist = self.bits.take(5)? as usize + 1;
        let hclen = self.bits.take(4)? as usize + 4;
        if hlit > 286 || hdist > 30 {
            return Err(Error::Corrupt("too many codes"));
        }
        let mut clen = [0u8; 19];
        for &i in &CLEN_ORDER[..hclen] {
            clen[i] = self.bits.take(3)? as u8;
        }
        let clen_code = Huffman::new(&clen)?;
        let mut lengths = [0u8; 286 + 30];
        let mut i = 0;
        while i < hlit + hdist {
            let symbol = clen_code.decode(&mut self.bits)?;
            let (value, repeat) = match symbol {
                0..=15 => (symbol as u8, 1),
                16 => {
                    let previous = *lengths[..i]
                        .last()
                        .ok_or(Error::Corrupt("repeat with no length"))?;
                    (previous, 3 + self.bits.take(2)? as usize)
                }
                17 => (0, 3 + self.bits.take(3)? as usize),
                18 => (0, 11 + self.bits.take(7)? as usize),
                _ => return Err(Error::Corrupt("code length symbol")),
            };
            if i + repeat > hlit + hdist {
                return Err(Error::Corrupt("code lengths overflow"));
            }
            lengths[i..i + repeat].fill(value);
            i += repeat;
        }
        if lengths[256] == 0 {
            return Err(Error::Corrupt("no end-of-block code"));
        }
        Ok((
            Huffman::new(&lengths[..hlit])?,
            Huffman::new(&lengths[hlit..hlit + hdist])?,
        ))
    }

    fn codes(&mut self, lit: &Huffman, dist: &Huffman) -> Result<(), Error> {
        loop {
            if self.sink.is_some() && self.out.len() >= 3 * WINDOW {
                self.flush(WINDOW)?;
            }
            let symbol = lit.decode(&mut self.bits)?;
            match symbol {
                0..=255 => {
                    self.room(1)?;
                    self.out.push(symbol as u8);
                }
                256 => return Ok(()),
                257..=285 => {
                    let i = usize::from(symbol - 257);
                    let len = usize::from(LENGTH_BASE[i])
                        + self.bits.take(u32::from(LENGTH_EXTRA[i]))? as usize;
                    let d = usize::from(dist.decode(&mut self.bits)?);
                    if d >= 30 {
                        return Err(Error::Corrupt("distance symbol"));
                    }
                    let distance = usize::from(DIST_BASE[d])
                        + self.bits.take(u32::from(DIST_EXTRA[d]))? as usize;
                    if distance > self.out.len() {
                        return Err(Error::Corrupt("distance before start"));
                    }
                    self.room(len)?;
                    copy_back(&mut self.out, distance, len);
                }
                _ => return Err(Error::Corrupt("literal/length symbol")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 1 << 20;

    #[test]
    fn inflates_fixed_and_dynamic_blocks() {
        let fixed = [
            203, 72, 205, 201, 201, 87, 200, 64, 34, 203, 243, 139, 114, 82, 0,
        ];
        assert_eq!(inflate(&fixed, LIMIT).unwrap(), b"hello hello hello world");
        let dynamic = include_bytes!("testdata/dynamic.deflate");
        let expected = include_bytes!("testdata/dynamic.txt");
        assert_eq!(inflate(dynamic, LIMIT).unwrap(), expected);
    }

    #[test]
    fn unwraps_gzip_and_zlib() {
        let gz = [
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 179, 41, 176, 75, 175, 202, 44, 80, 72, 202, 79,
            169, 180, 209, 47, 176, 3, 0, 82, 122, 105, 209, 16, 0, 0, 0,
        ];
        assert_eq!(gunzip(&gz, LIMIT).unwrap(), b"<p>gzip body</p>");
        let zl = [
            120, 156, 171, 202, 201, 76, 82, 40, 47, 74, 44, 40, 72, 77, 81, 40, 73, 173, 40, 209,
            81, 168, 66, 23, 2, 0, 251, 209, 13, 159,
        ];
        assert_eq!(
            zlib_decompress(&zl, LIMIT).unwrap(),
            b"zlib wrapped text, zlib wrapped text"
        );
        let mut bad = gz;
        bad[30] ^= 1;
        assert_eq!(gunzip(&bad, LIMIT), Err(Error::Checksum));
    }

    #[test]
    fn reads_stored_blocks() {
        let data = b"stored, not compressed";
        let mut stream = vec![1u8];
        stream.extend_from_slice(&(data.len() as u16).to_le_bytes());
        stream.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
        stream.extend_from_slice(data);
        assert_eq!(inflate(&stream, LIMIT).unwrap(), data);
    }

    #[test]
    fn rejects_bad_input_without_panicking() {
        let dynamic = include_bytes!("testdata/dynamic.deflate");
        assert_eq!(
            inflate(dynamic, 100),
            Err(Error::TooLarge),
            "the output limit holds"
        );
        for cut in 0..dynamic.len() {
            assert!(
                inflate(&dynamic[..cut], LIMIT).is_err(),
                "truncated at {cut}"
            );
        }
        // Flipping bits anywhere must give an error or some output, never a panic.
        for i in 0..dynamic.len() {
            let mut damaged = dynamic.to_vec();
            damaged[i] ^= 0x55;
            let _ = inflate(&damaged, LIMIT);
        }
        assert!(gunzip(b"not gzip at all, really", LIMIT).is_err());
        assert!(zlib_decompress(&[0x78, 0x9c], LIMIT).is_err());
    }

    #[test]
    fn streams_large_output_with_a_small_window() {
        // A stored block followed by long back-references, larger than several windows.
        let text: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut zl = vec![0x78, 0x01];
        for (i, block) in text.chunks(65_535).enumerate() {
            zl.push(u8::from((i + 1) * 65_535 >= text.len()));
            zl.extend_from_slice(&(block.len() as u16).to_le_bytes());
            zl.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
            zl.extend_from_slice(block);
        }
        zl.extend_from_slice(&adler32(&text).to_be_bytes());
        let mut got = Vec::new();
        let mut largest = 0;
        zlib_stream(&zl, 1 << 20, &mut |piece| {
            largest = largest.max(piece.len());
            got.extend_from_slice(piece);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, text);
        assert!(largest <= 3 * WINDOW, "pieces stay window-sized: {largest}");
        // Back-references reaching across flushed pieces.
        let long = include_bytes!("testdata/long.zlib");
        let whole = zlib_decompress(long, 1 << 22).unwrap();
        let mut pieces = Vec::new();
        zlib_stream(long, 1 << 22, &mut |p| {
            pieces.extend_from_slice(p);
            Ok(())
        })
        .unwrap();
        assert!(whole.len() > 8 * WINDOW);
        assert_eq!(pieces, whole);

        let dynamic = include_bytes!("testdata/dynamic.deflate");
        let mut streamed = Vec::new();
        inflate_with_len(
            dynamic,
            1 << 20,
            Some(&mut |p: &[u8]| {
                streamed.extend_from_slice(p);
                Ok(())
            }),
            None,
        )
        .unwrap();
        assert_eq!(streamed, include_bytes!("testdata/dynamic.txt"));
    }

    #[test]
    fn hardware_and_table_crcs_agree() {
        let data: Vec<u8> = (0..10_007u32).map(|i| (i * 31 + i / 7) as u8).collect();
        for len in [0, 1, 7, 8, 9, 63, 64, 1000, data.len()] {
            assert_eq!(
                crc32_update(0, &data[..len]),
                crc32_tables(0, &data[..len]),
                "length {len}"
            );
        }
        let split = crc32_update(crc32_update(0, &data[..333]), &data[333..]);
        assert_eq!(split, crc32_tables(0, &data));
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"IEND"]), 0xAE42_6082);
        assert_eq!(
            crc32(&[b"IE", b"ND"]),
            0xAE42_6082,
            "split input gives the same CRC"
        );
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
