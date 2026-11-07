//! Image decoding for Broxee: PNG and baseline JPEG, written here.
//!
//! Images are decoded straight to the size they are shown at. Decoders hand rows to a
//! `Scaler`, which box-filters them down as they arrive, so a 4000x3000 photo shown at 400x300
//! never exists at full size in memory (PNG data streams through `flate`, JPEG is decoded one
//! row of blocks at a time). Output is premultiplied RGBA, ready to composite.
//!
//! `probe` reads only the header, so layout can size images before anything is decoded. It
//! also recognises GIF and WebP, which are sized but not decoded yet.

mod jpeg;
mod png;

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub format: Format,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Corrupt(&'static str),
    Unsupported(&'static str),
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "image data is truncated"),
            Error::Corrupt(why) => write!(f, "corrupt image: {why}"),
            Error::Unsupported(what) => write!(f, "unsupported image: {what}"),
            Error::TooLarge => write!(f, "image is too large"),
        }
    }
}

impl From<flate::Error> for Error {
    fn from(e: flate::Error) -> Error {
        match e {
            flate::Error::Truncated => Error::Truncated,
            flate::Error::TooLarge => Error::TooLarge,
            flate::Error::Corrupt(why) => Error::Corrupt(why),
            flate::Error::Checksum => Error::Corrupt("compressed data checksum"),
        }
    }
}

/// Images wider or taller than this are refused.
pub const MAX_SIDE: u32 = 16_384;

/// Premultiplied RGBA pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Bitmap {
    pub fn bytes(&self) -> usize {
        self.pixels.len()
    }
}

/// Format and size from the first bytes of an image.
pub fn probe(data: &[u8]) -> Option<Info> {
    let info = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png::probe(data)?
    } else if data.starts_with(&[0xff, 0xd8]) {
        jpeg::probe(data)?
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Info {
            width: u32::from(u16::from_le_bytes([*data.get(6)?, *data.get(7)?])),
            height: u32::from(u16::from_le_bytes([*data.get(8)?, *data.get(9)?])),
            format: Format::Gif,
        }
    } else if data.len() >= 30 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        probe_webp(data)?
    } else {
        return None;
    };
    (info.width > 0 && info.height > 0 && info.width <= MAX_SIDE && info.height <= MAX_SIDE)
        .then_some(info)
}

fn probe_webp(d: &[u8]) -> Option<Info> {
    let le24 = |i: usize| u32::from(d[i]) | u32::from(d[i + 1]) << 8 | u32::from(d[i + 2]) << 16;
    let (width, height) = match &d[12..16] {
        // Lossy: a 14-bit width and height after the frame tag and start code.
        b"VP8 " => (
            u32::from(u16::from_le_bytes([d[26], d[27]]) & 0x3fff),
            u32::from(u16::from_le_bytes([d[28], d[29]]) & 0x3fff),
        ),
        // Lossless: 14-bit width-1 and height-1 packed after a signature byte.
        b"VP8L" => {
            let b = u32::from_le_bytes([d[21], d[22], d[23], d[24]]);
            ((b & 0x3fff) + 1, ((b >> 14) & 0x3fff) + 1)
        }
        // Extended: 24-bit canvas width-1 and height-1.
        b"VP8X" => (le24(24) + 1, le24(27) + 1),
        _ => return None,
    };
    Some(Info {
        width,
        height,
        format: Format::Webp,
    })
}

/// Decode `data` scaled down to at most `max_width` x `max_height` (never up: a smaller image
/// is returned at its own size). The aspect ratio is the caller's business: pass the size the
/// image is drawn at.
pub fn decode(data: &[u8], max_width: u32, max_height: u32) -> Result<Bitmap, Error> {
    let info = probe(data).ok_or(Error::Unsupported("unknown format"))?;
    let target = (
        max_width.clamp(1, info.width),
        max_height.clamp(1, info.height),
    );
    match info.format {
        Format::Png => png::decode(data, target),
        Format::Jpeg => jpeg::decode(data, target),
        Format::Gif => Err(Error::Unsupported("GIF")),
        Format::Webp => Err(Error::Unsupported("WebP")),
    }
}

/// Box-filters straight-alpha RGBA rows of a `width` x `height` image down to the target size,
/// premultiplying as it goes. Holds one accumulator row; finished rows go to the output.
pub(crate) struct Scaler {
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
    /// Target column of each source column.
    columns: Vec<u32>,
    /// Source columns per target column.
    column_counts: Vec<u32>,
    acc: Vec<u32>,
    acc_rows: u32,
    acc_target_row: u32,
    source_row: u32,
    out: Vec<u8>,
}

impl Scaler {
    pub(crate) fn new(width: u32, height: u32, target: (u32, u32)) -> Scaler {
        let (target_w, target_h) = (target.0.clamp(1, width), target.1.clamp(1, height));
        let columns: Vec<u32> = (0..width)
            .map(|x| (u64::from(x) * u64::from(target_w) / u64::from(width)) as u32)
            .collect();
        let mut column_counts = vec![0u32; target_w as usize];
        for &c in &columns {
            column_counts[c as usize] += 1;
        }
        Scaler {
            width,
            height,
            target_w,
            target_h,
            columns,
            column_counts,
            acc: vec![0; target_w as usize * 4],
            acc_rows: 0,
            acc_target_row: 0,
            source_row: 0,
            out: Vec::with_capacity(target_w as usize * target_h as usize * 4),
        }
    }

    /// One source row of straight-alpha RGBA.
    pub(crate) fn push(&mut self, rgba: &[u8]) {
        if self.source_row >= self.height {
            return;
        }
        let target_row =
            (u64::from(self.source_row) * u64::from(self.target_h) / u64::from(self.height)) as u32;
        if target_row != self.acc_target_row && self.acc_rows > 0 {
            self.emit();
        }
        self.acc_target_row = target_row;
        for (x, px) in rgba.chunks_exact(4).take(self.width as usize).enumerate() {
            let a = u32::from(px[3]);
            let at = self.columns[x] as usize * 4;
            self.acc[at] += u32::from(px[0]) * a;
            self.acc[at + 1] += u32::from(px[1]) * a;
            self.acc[at + 2] += u32::from(px[2]) * a;
            self.acc[at + 3] += a * 255;
        }
        self.acc_rows += 1;
        self.source_row += 1;
    }

    fn emit(&mut self) {
        for (t, &count) in self.column_counts.iter().enumerate() {
            let n = count * self.acc_rows * 255;
            let at = t * 4;
            for c in 0..4 {
                self.out.push(((self.acc[at + c] + n / 2) / n.max(1)) as u8);
            }
        }
        self.acc.fill(0);
        self.acc_rows = 0;
    }

    pub(crate) fn finish(mut self) -> Result<Bitmap, Error> {
        if self.acc_rows > 0 {
            self.emit();
        }
        if self.source_row < self.height {
            return Err(Error::Truncated);
        }
        Ok(Bitmap {
            width: self.target_w,
            height: self.target_h,
            pixels: self.out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_sizes_from_headers() {
        let png = include_bytes!("testdata/rgba8.png");
        assert_eq!(
            probe(png),
            Some(Info {
                width: 13,
                height: 9,
                format: Format::Png
            })
        );
        let jpeg = include_bytes!("testdata/j420.jpg");
        assert_eq!(probe(jpeg).map(|i| (i.width, i.height)), Some((48, 40)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x20, 0x01, 0x10, 0x00]);
        assert_eq!(
            probe(&gif).map(|i| (i.width, i.height, i.format)),
            Some((288, 16, Format::Gif))
        );
        assert_eq!(probe(b"<svg/>"), None);
        assert_eq!(probe(&[]), None);
    }

    #[test]
    fn scales_down_with_premultiplied_box_filtering() {
        // 4x2: left half opaque red, right half transparent; scaled to 2x1.
        let mut s = Scaler::new(4, 2, (2, 1));
        let row = [255, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 0, 0, 255, 0, 0];
        s.push(&row);
        s.push(&row);
        let b = s.finish().unwrap();
        assert_eq!((b.width, b.height), (2, 1));
        assert_eq!(
            b.pixels,
            [255, 0, 0, 255, 0, 0, 0, 0],
            "transparent green adds no color"
        );

        let mut half = Scaler::new(2, 1, (1, 1));
        half.push(&[255, 255, 255, 255, 0, 0, 0, 0]);
        assert_eq!(
            half.finish().unwrap().pixels,
            [128, 128, 128, 128],
            "premultiplied average"
        );

        let mut short = Scaler::new(1, 2, (1, 2));
        short.push(&[0, 0, 0, 255]);
        assert_eq!(short.finish(), Err(Error::Truncated));
    }
}
