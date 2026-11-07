//! A minimal PNG writer: 8-bit RGB, one IDAT of uncompressed ("stored") deflate blocks.
//! Files are large but exact, which is what a debugging snapshot needs.

use flate::{adler32_update, crc32, crc32_update};

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[tag, data]).to_be_bytes());
}

/// Most bytes a stored DEFLATE block holds.
const BLOCK: usize = 65_535;

/// Encode RGBA pixels (alpha ignored) as a PNG file. Rows are written straight into the file
/// as stored DEFLATE blocks, with the checksums kept running, so the only copy of the image
/// made here is the file itself.
pub fn encode(width: usize, height: usize, rgba: &[u8]) -> Vec<u8> {
    let raw_len = (width * 3 + 1) * height;
    let blocks = raw_len.div_ceil(BLOCK).max(1);
    let zlib_len = 2 + blocks * 5 + raw_len + 4;

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = Vec::with_capacity(8 + 25 + 12 + zlib_len + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    chunk(&mut out, b"IHDR", &header);

    out.extend_from_slice(&(zlib_len as u32).to_be_bytes());
    out.extend_from_slice(b"IDAT");
    let mut w = Stored {
        out: &mut out,
        crc: crc32_update(0, b"IDAT"),
        adler: 1,
        remaining: raw_len,
        left_in_block: 0,
    };
    w.write(&[0x78, 0x01]);
    if raw_len == 0 {
        w.write(&[1, 0, 0, 0xFF, 0xFF]);
    }
    let mut line = Vec::with_capacity(width * 3 + 1);
    for row in rgba.chunks(width * 4).take(height) {
        line.clear();
        line.push(0); // filter: none
        for px in row.chunks(4) {
            line.extend_from_slice(&px[..3]);
        }
        w.raw(&line);
    }
    let adler = w.adler;
    w.write(&adler.to_be_bytes());
    let crc = w.crc;
    out.extend_from_slice(&crc.to_be_bytes());
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Writes IDAT data, keeping its CRC and the zlib Adler-32 of the image bytes.
struct Stored<'a> {
    out: &'a mut Vec<u8>,
    crc: u32,
    adler: u32,
    /// Image bytes not written yet.
    remaining: usize,
    left_in_block: usize,
}

impl Stored<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.crc = crc32_update(self.crc, bytes);
        self.out.extend_from_slice(bytes);
    }

    /// Image bytes, split into stored blocks as needed.
    fn raw(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.left_in_block == 0 {
                let len = self.remaining.min(BLOCK);
                let last = u8::from(self.remaining <= BLOCK);
                let len16 = len as u16;
                let mut head = [last, 0, 0, 0, 0];
                head[1..3].copy_from_slice(&len16.to_le_bytes());
                head[3..5].copy_from_slice(&(!len16).to_le_bytes());
                self.write(&head);
                self.left_in_block = len;
            }
            let take = self.left_in_block.min(bytes.len());
            self.adler = adler32_update(self.adler, &bytes[..take]);
            self.write(&bytes[..take]);
            self.left_in_block -= take;
            self.remaining -= take;
            bytes = &bytes[take..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_data_round_trips_through_the_decoder() {
        let pixels = [255, 0, 0, 255, 0, 0, 255, 255];
        let png = encode(2, 1, &pixels);
        // IHDR is 8 + 13 + 4 bytes after the 8-byte signature; IDAT's data follows its header.
        let idat = 8 + 25;
        assert_eq!(&png[idat + 4..idat + 8], b"IDAT");
        let len = u32::from_be_bytes(png[idat..idat + 4].try_into().unwrap()) as usize;
        let raw = flate::zlib_decompress(&png[idat + 8..idat + 8 + len], 1 << 10).unwrap();
        assert_eq!(
            raw,
            [0, 255, 0, 0, 0, 0, 255],
            "filter byte, then RGB per pixel"
        );
    }

    #[test]
    fn large_images_span_many_blocks_and_still_decode() {
        let (w, h) = (300, 250); // 225,250 bytes of rows: four stored blocks
        let pixels: Vec<u8> = (0..w * h * 4).map(|i| (i % 253) as u8).collect();
        let png = encode(w, h, &pixels);
        let idat = 8 + 25;
        let len = u32::from_be_bytes(png[idat..idat + 4].try_into().unwrap()) as usize;
        let data = &png[idat + 8..idat + 8 + len];
        let crc = u32::from_be_bytes(png[idat + 8 + len..idat + 12 + len].try_into().unwrap());
        assert_eq!(crc, crc32(&[b"IDAT", data]));
        let raw = flate::zlib_decompress(data, 1 << 20).unwrap();
        assert_eq!(raw.len(), (w * 3 + 1) * h);
        assert_eq!(&raw[1..4], &pixels[0..3]);
        assert_eq!(png.len(), png.capacity(), "sized exactly up front");
    }

    #[test]
    fn writes_a_well_formed_file() {
        let png = encode(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}
