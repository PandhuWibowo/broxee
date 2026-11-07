//! Brotli decompression (RFC 7932), for `Content-Encoding: br`.
//!
//! CDNs serve Brotli where they can: GitHub's stylesheets are half their gzip size, as is
//! Cloudflare's home page. Its gains come from context modeling (the prefix code for the next
//! byte depends on the two before it) and a built-in dictionary of common web words.
//!
//! The whole body decodes into one buffer, which doubles as the sliding window: no separate
//! ring buffer, so memory is the output plus small code tables. The static dictionary and the
//! word transforms are data from the RFC (`brotli_tables.rs`, `brotli_dictionary.bin`),
//! checked against its CRCs in the tests.

use crate::brotli_tables::{LUT0, LUT1, LUT2, TRANSFORMS};
use crate::{copy_back, Bits, Error, Huffman};

static DICTIONARY: &[u8; 122_784] = include_bytes!("brotli_dictionary.bin");

/// Bits per dictionary word index, by word length (4..=24).
const NDBITS: [u32; 25] = [
    0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5,
];

/// Start of the words of each length in the dictionary.
const DOFFSET: [usize; 25] = {
    let mut offsets = [0usize; 25];
    let mut length = 0;
    while length < 24 {
        let words = if length < 4 { 0 } else { 1 << NDBITS[length] };
        offsets[length + 1] = offsets[length] + length * words;
        length += 1;
    }
    offsets
};

/// (base, extra bits) of insert length codes, copy length codes and block count codes.
const INSERT_LENGTHS: [(u32, u32); 24] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 1),
    (8, 1),
    (10, 2),
    (14, 2),
    (18, 3),
    (26, 3),
    (34, 4),
    (50, 4),
    (66, 5),
    (98, 5),
    (130, 6),
    (194, 7),
    (322, 8),
    (578, 9),
    (1090, 10),
    (2114, 12),
    (6210, 14),
    (22594, 24),
];
const COPY_LENGTHS: [(u32, u32); 24] = [
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 1),
    (12, 1),
    (14, 2),
    (18, 2),
    (22, 3),
    (30, 3),
    (38, 4),
    (54, 4),
    (70, 5),
    (102, 5),
    (134, 6),
    (198, 7),
    (326, 8),
    (582, 9),
    (1094, 10),
    (2118, 24),
];
const BLOCK_COUNTS: [(u32, u32); 26] = [
    (1, 2),
    (5, 2),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 3),
    (41, 3),
    (49, 4),
    (65, 4),
    (81, 4),
    (97, 4),
    (113, 5),
    (145, 5),
    (177, 5),
    (209, 5),
    (241, 6),
    (305, 6),
    (369, 7),
    (497, 8),
    (753, 9),
    (1265, 10),
    (2289, 11),
    (4337, 12),
    (8433, 13),
    (16625, 24),
];
/// Insert and copy length code offsets for each 64-symbol cell of the command alphabet.
const CELL_INSERT: [u32; 11] = [0, 0, 0, 0, 8, 8, 0, 16, 8, 16, 16];
const CELL_COPY: [u32; 11] = [0, 8, 0, 8, 0, 8, 16, 0, 16, 8, 16];
/// Distance symbols 0..15: (which past distance, adjustment).
const SHORT_DISTANCES: [(usize, i64); 16] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (0, -1),
    (0, 1),
    (0, -2),
    (0, 2),
    (0, -3),
    (0, 3),
    (1, -1),
    (1, 1),
    (1, -2),
    (1, 2),
    (1, -3),
    (1, 3),
];
/// Code lengths of the code length alphabet are sent in this order.
const CODE_LENGTH_ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// A prefix code. Brotli allows a code with one symbol, which takes no bits at all.
enum Prefix {
    One(u16),
    Code(Huffman),
}

impl Prefix {
    #[inline]
    fn decode(&self, bits: &mut Bits) -> Result<u16, Error> {
        match self {
            Prefix::One(symbol) => Ok(*symbol),
            Prefix::Code(code) => code.decode(bits),
        }
    }
}

/// Block types and counts of one category (literals, commands, distances).
struct Blocks {
    types: usize,
    type_code: Option<Prefix>,
    count_code: Option<Prefix>,
    current: usize,
    previous: usize,
    remaining: u32,
}

impl Blocks {
    fn read(bits: &mut Bits) -> Result<Blocks, Error> {
        let types = read_var_u8(bits)? as usize + 1;
        let mut blocks = Blocks {
            types,
            type_code: None,
            count_code: None,
            current: 0,
            previous: 1,
            remaining: u32::MAX,
        };
        if types >= 2 {
            blocks.type_code = Some(read_prefix(bits, types + 2)?);
            let count_code = read_prefix(bits, 26)?;
            blocks.remaining = read_block_count(bits, &count_code)?;
            blocks.count_code = Some(count_code);
        }
        Ok(blocks)
    }

    /// Account for one more symbol of this category, switching blocks when one runs out.
    #[inline]
    fn next(&mut self, bits: &mut Bits) -> Result<(), Error> {
        if self.remaining == 0 {
            let (Some(type_code), Some(count_code)) = (&self.type_code, &self.count_code) else {
                return Err(Error::Corrupt("brotli block switch"));
            };
            let new = match type_code.decode(bits)? {
                0 => self.previous,
                1 => (self.current + 1) % self.types,
                symbol => usize::from(symbol) - 2,
            };
            if new >= self.types {
                return Err(Error::Corrupt("brotli block type"));
            }
            self.previous = self.current;
            self.current = new;
            self.remaining = read_block_count(bits, count_code)?;
        }
        self.remaining -= 1;
        Ok(())
    }
}

fn read_block_count(bits: &mut Bits, code: &Prefix) -> Result<u32, Error> {
    let (base, extra) = BLOCK_COUNTS[usize::from(code.decode(bits)?).min(25)];
    Ok(base + bits.take(extra)?)
}

/// 1 to 11 bits: 0, or 1 + 2^n + n more bits (NBLTYPES, NTREES).
fn read_var_u8(bits: &mut Bits) -> Result<u32, Error> {
    if bits.take(1)? == 0 {
        return Ok(0);
    }
    let n = bits.take(3)?;
    if n == 0 {
        return Ok(1);
    }
    Ok((1 << n) + bits.take(n)?)
}

/// A prefix code over `alphabet` symbols (sections 3.4 and 3.5).
fn read_prefix(bits: &mut Bits, alphabet: usize) -> Result<Prefix, Error> {
    let hskip = bits.take(2)?;
    if hskip == 1 {
        // Simple code: up to four symbols with fixed code lengths.
        let width = usize::BITS - (alphabet - 1).leading_zeros();
        let count = bits.take(2)? as usize + 1;
        let mut symbols = [0u16; 4];
        for i in 0..count {
            let s = bits.take(width.max(1))? as u16;
            if usize::from(s) >= alphabet || symbols[..i].contains(&s) {
                return Err(Error::Corrupt("brotli simple prefix code"));
            }
            symbols[i] = s;
        }
        let lengths: &[u8] = match count {
            1 => return Ok(Prefix::One(symbols[0])),
            2 => &[1, 1],
            3 => &[1, 2, 2],
            _ if bits.take(1)? == 0 => &[2, 2, 2, 2],
            _ => &[1, 2, 3, 3],
        };
        let mut all = vec![0u8; alphabet];
        for (&s, &l) in symbols.iter().zip(lengths) {
            all[usize::from(s)] = l;
        }
        return Ok(Prefix::Code(Huffman::new(&all)?));
    }

    // Complex code: the code lengths are themselves prefix coded (section 3.5).
    // A 4-bit lookup of the fixed code for code length code lengths, read right to left.
    const LENGTH_OF: [u32; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];
    const VALUE_OF: [u8; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];
    let mut code_lengths = [0u8; 18];
    let mut space = 32i32;
    let mut used = 0;
    for &symbol in &CODE_LENGTH_ORDER[hskip as usize..] {
        let v = bits.peek(4) as usize;
        bits.take(LENGTH_OF[v])?;
        let length = VALUE_OF[v];
        code_lengths[symbol] = length;
        if length != 0 {
            space -= 32 >> length;
            used += 1;
            if space <= 0 {
                break;
            }
        }
    }
    if used != 1 && space != 0 {
        return Err(Error::Corrupt("brotli code length code"));
    }
    let length_code = if used == 1 {
        let symbol = code_lengths.iter().position(|&l| l != 0).unwrap_or(0);
        Prefix::One(symbol as u16)
    } else {
        Prefix::Code(Huffman::new(&code_lengths)?)
    };

    let mut lengths = vec![0u8; alphabet];
    let (mut symbol, mut space) = (0usize, 32768i32);
    let (mut previous, mut repeat, mut repeat_length) = (8u8, 0usize, 0u8);
    while symbol < alphabet && space > 0 {
        let code = length_code.decode(bits)?;
        if code < 16 {
            repeat = 0;
            lengths[symbol] = code as u8;
            if code != 0 {
                previous = code as u8;
                space -= 32768 >> code;
            }
            symbol += 1;
            continue;
        }
        // 16 repeats the previous non-zero length, 17 repeats zero; consecutive repeats of the
        // same kind extend the previous count instead of adding to it.
        let (extra, length) = if code == 16 { (2, previous) } else { (3, 0) };
        if repeat_length != length {
            repeat = 0;
            repeat_length = length;
        }
        let old = repeat;
        if repeat > 0 {
            repeat = (repeat - 2) << extra;
        }
        repeat += bits.take(extra)? as usize + 3;
        let delta = repeat - old;
        if symbol + delta > alphabet {
            return Err(Error::Corrupt("brotli code length repeat"));
        }
        lengths[symbol..symbol + delta].fill(length);
        symbol += delta;
        if length != 0 {
            space -= (delta as i32) << (15 - length);
        }
    }
    if space != 0 {
        return Err(Error::Corrupt("brotli incomplete prefix code"));
    }
    Ok(Prefix::Code(Huffman::new(&lengths)?))
}

/// A context map (section 7.3): which prefix code each (block type, context) uses.
fn read_context_map(bits: &mut Bits, size: usize, trees: usize) -> Result<Vec<u8>, Error> {
    let mut map = vec![0u8; size];
    if trees < 2 {
        return Ok(map);
    }
    let rle_max = if bits.take(1)? == 1 {
        bits.take(4)? + 1
    } else {
        0
    };
    let code = read_prefix(bits, trees + rle_max as usize)?;
    let mut i = 0;
    while i < size {
        let symbol = u32::from(code.decode(bits)?);
        if symbol == 0 {
            i += 1;
        } else if symbol <= rle_max {
            let run = (1usize << symbol) + bits.take(symbol)? as usize;
            if i + run > size {
                return Err(Error::Corrupt("brotli context map run"));
            }
            i += run; // the map starts zeroed
        } else {
            map[i] = (symbol - rle_max) as u8;
            i += 1;
        }
    }
    if bits.take(1)? == 1 {
        // Inverse move-to-front.
        let mut mtf: [u8; 256] = std::array::from_fn(|i| i as u8);
        for v in map.iter_mut() {
            let index = usize::from(*v);
            let value = mtf[index];
            *v = value;
            mtf.copy_within(0..index, 1);
            mtf[0] = value;
        }
    }
    if map.iter().any(|&t| usize::from(t) >= trees) {
        return Err(Error::Corrupt("brotli context map value"));
    }
    Ok(map)
}

/// Context ID of the next literal (section 7.1) from the previous two bytes.
#[inline]
fn literal_context(mode: u8, p1: u8, p2: u8) -> usize {
    usize::from(match mode {
        0 => p1 & 0x3f,
        1 => p1 >> 2,
        2 => LUT0[usize::from(p1)] | LUT1[usize::from(p2)],
        _ => (LUT2[usize::from(p1)] << 3) | LUT2[usize::from(p2)],
    })
}

/// Uppercase the UTF-8 character at `at` the way RFC 7932 defines it; returns its length.
fn ferment(word: &mut [u8], at: usize) -> usize {
    let b = word[at];
    if b < 192 {
        if b.is_ascii_lowercase() {
            word[at] ^= 32;
        }
        1
    } else if b < 224 {
        if at + 1 < word.len() {
            word[at + 1] ^= 32;
        }
        2
    } else {
        if at + 2 < word.len() {
            word[at + 2] ^= 5;
        }
        3
    }
}

/// Append dictionary word `index` of `length` bytes, transformed by `transform` (section 8).
fn push_dictionary_word(out: &mut Vec<u8>, length: usize, index: usize, transform: usize) {
    let start = DOFFSET[length] + index * length;
    let (prefix, kind, suffix) = TRANSFORMS[transform];
    let mut word = [0u8; 24];
    word[..length].copy_from_slice(&DICTIONARY[start..start + length]);
    let (mut from, mut to) = (0, length);
    match kind {
        3..=11 => from = usize::from(kind - 2).min(length),
        12..=20 => to = length.saturating_sub(usize::from(kind - 11)),
        _ => {}
    }
    let word = &mut word[from..to.max(from)];
    match kind {
        1 if !word.is_empty() => {
            ferment(word, 0);
        }
        2 => {
            let mut i = 0;
            while i < word.len() {
                i += ferment(word, i);
            }
        }
        _ => {}
    }
    out.extend_from_slice(prefix);
    out.extend_from_slice(word);
    out.extend_from_slice(suffix);
}

/// Decompress a Brotli stream.
pub fn brotli(data: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    let mut bits = Bits::new(data);
    let window_bits = if bits.take(1)? == 0 {
        16
    } else {
        match bits.take(3)? {
            0 => match bits.take(3)? {
                0 => 17,
                1 => return Err(Error::Corrupt("brotli window size")),
                m => 8 + m,
            },
            n => 17 + n,
        }
    };
    let window = (1usize << window_bits) - 16;
    let mut out: Vec<u8> = Vec::with_capacity((data.len() * 4).min(limit));
    let mut distances: [i64; 4] = [16, 15, 11, 4];
    let mut last = 3usize;

    loop {
        let is_last = bits.take(1)? == 1;
        if is_last && bits.take(1)? == 1 {
            break; // ISLASTEMPTY
        }
        let nibbles = match bits.take(2)? {
            3 => 0,
            n => n + 4,
        };
        if nibbles == 0 {
            // A metadata block: skip it.
            if bits.take(1)? != 0 {
                return Err(Error::Corrupt("brotli reserved bit"));
            }
            let skip_bytes = bits.take(2)?;
            let skip = if skip_bytes == 0 {
                0
            } else {
                bits.take(skip_bytes * 8)? as usize + 1
            };
            bits.align_to_byte();
            skip_raw(&mut bits, skip)?;
            if is_last {
                break;
            }
            continue;
        }
        let mlen = bits.take(nibbles * 4)? as usize + 1;
        if out.len() + mlen > limit {
            return Err(Error::TooLarge);
        }
        if !is_last && bits.take(1)? == 1 {
            // Uncompressed meta-block.
            bits.align_to_byte();
            copy_raw(&mut bits, mlen, &mut out)?;
            continue;
        }

        let mut literal_blocks = Blocks::read(&mut bits)?;
        let mut command_blocks = Blocks::read(&mut bits)?;
        let mut distance_blocks = Blocks::read(&mut bits)?;
        let postfix = bits.take(2)?;
        let direct = bits.take(4)? << postfix;
        let modes: Vec<u8> = (0..literal_blocks.types)
            .map(|_| bits.take(2).map(|m| m as u8))
            .collect::<Result<_, _>>()?;
        let literal_trees = read_var_u8(&mut bits)? as usize + 1;
        let literal_map = read_context_map(&mut bits, 64 * literal_blocks.types, literal_trees)?;
        let distance_trees = read_var_u8(&mut bits)? as usize + 1;
        let distance_map = read_context_map(&mut bits, 4 * distance_blocks.types, distance_trees)?;
        let literal_codes: Vec<Prefix> = (0..literal_trees)
            .map(|_| read_prefix(&mut bits, 256))
            .collect::<Result<_, _>>()?;
        let command_codes: Vec<Prefix> = (0..command_blocks.types)
            .map(|_| read_prefix(&mut bits, 704))
            .collect::<Result<_, _>>()?;
        let distance_alphabet = 16 + direct as usize + (48 << postfix);
        let distance_codes: Vec<Prefix> = (0..distance_trees)
            .map(|_| read_prefix(&mut bits, distance_alphabet))
            .collect::<Result<_, _>>()?;

        let end = out.len() + mlen;
        while out.len() < end {
            command_blocks.next(&mut bits)?;
            let command = u32::from(command_codes[command_blocks.current].decode(&mut bits)?);
            let cell = (command >> 6) as usize;
            if cell >= 11 {
                return Err(Error::Corrupt("brotli command"));
            }
            let (ib, ie) = INSERT_LENGTHS[(CELL_INSERT[cell] + ((command >> 3) & 7)) as usize];
            let (cb, ce) = COPY_LENGTHS[(CELL_COPY[cell] + (command & 7)) as usize];
            let insert = (ib + bits.take(ie)?) as usize;
            let copy = (cb + bits.take(ce)?) as usize;
            if out.len() + insert > end {
                return Err(Error::Corrupt("brotli insert length"));
            }
            for _ in 0..insert {
                literal_blocks.next(&mut bits)?;
                let n = out.len();
                let p1 = if n > 0 { out[n - 1] } else { 0 };
                let p2 = if n > 1 { out[n - 2] } else { 0 };
                let block = literal_blocks.current;
                let context = literal_context(modes[block], p1, p2);
                let tree = usize::from(literal_map[64 * block + context]);
                out.push(literal_codes[tree].decode(&mut bits)? as u8);
            }
            if out.len() == end {
                break; // the last command's copy length is ignored
            }

            let symbol = if cell < 2 {
                0 // the last distance, implicitly
            } else {
                distance_blocks.next(&mut bits)?;
                let context = if copy > 4 { 3 } else { copy - 2 };
                let tree = usize::from(distance_map[4 * distance_blocks.current + context]);
                usize::from(distance_codes[tree].decode(&mut bits)?)
            };
            let distance = if symbol < 16 {
                let (which, delta) = SHORT_DISTANCES[symbol];
                let d = distances[(last + 4 - which) & 3] + delta;
                if d <= 0 {
                    return Err(Error::Corrupt("brotli distance"));
                }
                d as usize
            } else if symbol < 16 + direct as usize {
                symbol - 15
            } else {
                let code = (symbol - 16 - direct as usize) as u32;
                let distance_bits = 1 + (code >> (postfix + 1));
                let high = code >> postfix;
                let low = code & ((1 << postfix) - 1);
                let offset = ((2 + (high & 1)) << distance_bits) - 4;
                let extra = bits.take(distance_bits)?;
                ((((offset + extra) as usize) << postfix) + low as usize) + direct as usize + 1
            };

            let max = window.min(out.len());
            if distance > max {
                // A word from the static dictionary.
                if !(4..=24).contains(&copy) {
                    return Err(Error::Corrupt("brotli dictionary word length"));
                }
                let id = distance - max - 1;
                let index = id & ((1 << NDBITS[copy]) - 1);
                let transform = id >> NDBITS[copy];
                if transform >= TRANSFORMS.len() {
                    return Err(Error::Corrupt("brotli dictionary transform"));
                }
                push_dictionary_word(&mut out, copy, index, transform);
                if out.len() > end {
                    return Err(Error::Corrupt("brotli dictionary word overruns"));
                }
            } else {
                if out.len() + copy > end {
                    return Err(Error::Corrupt("brotli copy length"));
                }
                copy_back(&mut out, distance, copy);
                if symbol != 0 {
                    last = (last + 1) & 3;
                    distances[last] = distance as i64;
                }
            }
        }
        if is_last {
            break;
        }
    }
    Ok(out)
}

/// Copy `n` raw bytes after the bit reader has been aligned to a byte.
fn copy_raw(bits: &mut Bits, n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
    let mut left = n;
    while left > 0 && bits.count >= 8 {
        out.push(bits.take(8)? as u8);
        left -= 1;
    }
    bits.discard_ahead();
    let start = bits.pos;
    out.extend_from_slice(bits.data.get(start..start + left).ok_or(Error::Truncated)?);
    bits.pos += left;
    Ok(())
}

fn skip_raw(bits: &mut Bits, n: usize) -> Result<(), Error> {
    let mut left = n;
    while left > 0 && bits.count >= 8 {
        bits.take(8)?;
        left -= 1;
    }
    bits.discard_ahead();
    if bits.pos + left > bits.data.len() {
        return Err(Error::Truncated);
    }
    bits.pos += left;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32;

    #[test]
    fn tables_match_the_rfc() {
        assert_eq!(crc32(&[DICTIONARY]), 0x5136_cb04);
        let mut transforms = Vec::new();
        for (prefix, kind, suffix) in TRANSFORMS {
            transforms.extend_from_slice(prefix);
            transforms.extend_from_slice(&[0, kind]);
            transforms.extend_from_slice(suffix);
            transforms.push(0);
        }
        assert_eq!(
            (transforms.len(), crc32(&[&transforms])),
            (648, 0x3d96_5f81)
        );
        assert_eq!(crc32(&[&LUT0]), 0x8e91_efb7);
        assert_eq!(crc32(&[&LUT1]), 0xd01a_32f4);
        assert_eq!(crc32(&[&LUT2]), 0x0dd7_a0d6);
        assert_eq!(DOFFSET[24] + 24 * (1 << NDBITS[24]), DICTIONARY.len());
    }

    const LIMIT: usize = 1 << 22;

    #[test]
    fn decodes_reference_streams_exactly() {
        let html = include_bytes!("testdata/sample.html");
        for (name, stream) in [
            ("quality 11", &include_bytes!("testdata/sample.q11.br")[..]),
            ("quality 5", &include_bytes!("testdata/sample.q5.br")[..]),
            ("quality 1", &include_bytes!("testdata/sample.q1.br")[..]),
            (
                "1 KiB window",
                &include_bytes!("testdata/sample.w10.br")[..],
            ),
        ] {
            let out = brotli(stream, LIMIT).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out == html, "{name}: output differs from the original");
        }
        let random = include_bytes!("testdata/random.bin");
        assert_eq!(
            brotli(include_bytes!("testdata/random.br"), LIMIT).unwrap(),
            random,
            "uncompressed meta-blocks"
        );
        assert_eq!(
            brotli(include_bytes!("testdata/empty.br"), LIMIT).unwrap(),
            b""
        );
    }

    #[test]
    fn rejects_damage_and_oversized_output_without_panicking() {
        let stream = include_bytes!("testdata/sample.q11.br");
        assert_eq!(brotli(stream, 1000), Err(Error::TooLarge));
        for cut in (0..stream.len()).step_by(97) {
            assert!(brotli(&stream[..cut], LIMIT).is_err(), "truncated at {cut}");
        }
        for i in (0..stream.len()).step_by(13) {
            let mut damaged = stream.to_vec();
            damaged[i] ^= 0x5a;
            let _ = brotli(&damaged, LIMIT);
        }
    }
}
