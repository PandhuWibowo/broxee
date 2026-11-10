//! WebP container: simple lossy (`VP8 `) and lossless (`VP8L`) files, and extended files
//! (`VP8X`) with an alpha chunk or animation, of which the first frame is drawn.
//!
//! Every path streams rows into the `Scaler`: VP8 hands over macroblock rows as they become
//! final, VP8L and its alpha variant are pulled a row at a time.

use crate::{vp8, vp8l, Bitmap, Error, Scaler, MAX_SIDE};

struct Chunks<'a> {
    alpha: Option<&'a [u8]>,
    lossy: Option<&'a [u8]>,
    lossless: Option<&'a [u8]>,
}

fn chunks(mut d: &[u8]) -> Chunks<'_> {
    let mut c = Chunks {
        alpha: None,
        lossy: None,
        lossless: None,
    };
    while d.len() >= 8 {
        let len = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
        let body = &d[8..(8 + len).min(d.len())];
        match &d[..4] {
            b"ALPH" if c.alpha.is_none() => c.alpha = Some(body),
            b"VP8 " if c.lossy.is_none() => c.lossy = Some(body),
            b"VP8L" if c.lossless.is_none() => c.lossless = Some(body),
            _ => {}
        }
        let next = 8 + len + (len & 1);
        if next > d.len() {
            break;
        }
        d = &d[next..];
    }
    c
}

fn le24(d: &[u8]) -> usize {
    usize::from(d[0]) | usize::from(d[1]) << 8 | usize::from(d[2]) << 16
}

pub(crate) fn decode(data: &[u8], target: (u32, u32)) -> Result<Bitmap, Error> {
    let body = data.get(12..).ok_or(Error::Truncated)?;
    let riff_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let body = &body[..riff_len.saturating_sub(4).min(body.len())];
    let mut canvas = None;
    let mut frame_at = (0usize, 0usize);
    let mut parts = chunks(body);
    if body.starts_with(b"VP8X") {
        let x = body.get(8..18).ok_or(Error::Truncated)?;
        canvas = Some((le24(&x[4..7]) + 1, le24(&x[7..10]) + 1));
        // Animation: the first frame, placed at its offset.
        let mut d = body;
        while d.len() >= 8 {
            let len = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
            if &d[..4] == b"ANMF" {
                let f = d.get(8..(8 + len).min(d.len())).ok_or(Error::Truncated)?;
                let head = f.get(..16).ok_or(Error::Truncated)?;
                frame_at = (le24(&head[0..3]) * 2, le24(&head[3..6]) * 2);
                parts = chunks(&f[16..]);
                break;
            }
            let next = 8 + len + (len & 1);
            if next > d.len() {
                break;
            }
            d = &d[next..];
        }
    }
    let size = if let Some(lossy) = parts.lossy {
        vp8::probe(lossy).map(|(w, h)| (w as usize, h as usize))
    } else if let Some(lossless) = parts.lossless {
        let b = lossless.get(1..5).ok_or(Error::Truncated)?;
        let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        Some(((v & 0x3fff) as usize + 1, ((v >> 14) & 0x3fff) as usize + 1))
    } else {
        None
    };
    let (fw, fh) = size.ok_or(Error::Corrupt("WebP without an image"))?;
    let (cw, ch) = canvas.unwrap_or((fw, fh));
    if cw == 0 || ch == 0 || cw > MAX_SIDE as usize || ch > MAX_SIDE as usize {
        return Err(Error::TooLarge);
    }
    let mut out = Placed {
        scaler: Scaler::new(cw as u32, ch as u32, target),
        row: vec![0u8; cw * 4],
        canvas: (cw, ch),
        at: frame_at,
        y: 0,
    };
    out.blank_until(frame_at.1);
    if let Some(lossy) = parts.lossy {
        let mut alpha = match parts.alpha {
            Some(a) => Some(Alpha::new(a, fw, fh)?),
            None => None,
        };
        let mut rgba = vec![0u8; fw * 4];
        let mut a_row = vec![255u8; fw];
        let mut failed = None;
        vp8::decode(lossy, &mut |p: vp8::Planes| {
            if let Some(alpha) = &mut alpha {
                if failed.is_none() {
                    if let Err(e) = alpha.next_row(&mut a_row) {
                        // Damaged alpha: what arrived stays, the rest is opaque.
                        failed = Some(e);
                        a_row.fill(255);
                    }
                }
            }
            for x in 0..p.y.len() {
                let (y, u, v) = (
                    i32::from(p.y[x]),
                    i32::from(p.u[x / 2]),
                    i32::from(p.v[x / 2]),
                );
                rgba[x * 4..x * 4 + 4].copy_from_slice(&[
                    yuv_r(y, v),
                    yuv_g(y, u, v),
                    yuv_b(y, u),
                    a_row[x],
                ]);
            }
            out.frame_row(&rgba);
        })?;
    } else if let Some(lossless) = parts.lossless {
        let mut d = vp8l::Decoder::new(lossless, None)?;
        let mut rgba = vec![0u8; d.width * 4];
        for _ in 0..d.height {
            for (px, &p) in rgba.chunks_exact_mut(4).zip(d.next_row()?) {
                px.copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]);
            }
            out.frame_row(&rgba);
        }
    }
    out.blank_until(ch);
    out.scaler.finish()
}

/// Rows of a frame placed on the canvas, with transparent pixels around it.
struct Placed {
    scaler: Scaler,
    row: Vec<u8>,
    canvas: (usize, usize),
    at: (usize, usize),
    y: usize,
}

impl Placed {
    fn blank_until(&mut self, y: usize) {
        self.row.fill(0);
        while self.y < y.min(self.canvas.1) {
            self.scaler.push(&self.row);
            self.y += 1;
        }
    }

    fn frame_row(&mut self, rgba: &[u8]) {
        if self.y >= self.canvas.1 {
            return;
        }
        self.row.fill(0);
        let x0 = self.at.0.min(self.canvas.0);
        let n = rgba.len().min((self.canvas.0 - x0) * 4);
        self.row[x0 * 4..x0 * 4 + n].copy_from_slice(&rgba[..n]);
        self.scaler.push(&self.row);
        self.y += 1;
    }
}

// libwebp's fixed-point BT.601 conversion (14-bit intermediate, 6 fractional bits).
fn mult_hi(v: i32, c: i32) -> i32 {
    (v * c) >> 8
}

fn clip8(v: i32) -> u8 {
    if v & !0x3fff == 0 {
        (v >> 6) as u8
    } else if v < 0 {
        0
    } else {
        255
    }
}

fn yuv_r(y: i32, v: i32) -> u8 {
    clip8(mult_hi(y, 19077) + mult_hi(v, 26149) - 14234)
}

fn yuv_g(y: i32, u: i32, v: i32) -> u8 {
    clip8(mult_hi(y, 19077) - mult_hi(u, 6419) - mult_hi(v, 13320) + 8708)
}

fn yuv_b(y: i32, u: i32) -> u8 {
    clip8(mult_hi(y, 19077) + mult_hi(u, 33050) - 17685)
}

/// The `ALPH` chunk: raw or VP8L-compressed alpha, with an optional prediction filter.
struct Alpha<'a> {
    source: AlphaSource<'a>,
    filter: u8,
    previous: Vec<u8>,
    y: usize,
}

enum AlphaSource<'a> {
    Raw(&'a [u8], usize),
    Lossless(Box<vp8l::Decoder<'a>>),
}

impl<'a> Alpha<'a> {
    fn new(chunk: &'a [u8], width: usize, height: usize) -> Result<Alpha<'a>, Error> {
        let (&head, data) = chunk.split_first().ok_or(Error::Truncated)?;
        let source = match head & 3 {
            0 => AlphaSource::Raw(data, width),
            1 => AlphaSource::Lossless(Box::new(vp8l::Decoder::new(data, Some((width, height)))?)),
            _ => return Err(Error::Unsupported("alpha compression")),
        };
        Ok(Alpha {
            source,
            filter: (head >> 2) & 3,
            previous: vec![0; width],
            y: 0,
        })
    }

    fn next_row(&mut self, out: &mut [u8]) -> Result<(), Error> {
        match &mut self.source {
            AlphaSource::Raw(data, width) => {
                let at = self.y * *width;
                let row = data.get(at..at + *width).ok_or(Error::Truncated)?;
                out.copy_from_slice(row);
            }
            AlphaSource::Lossless(d) => {
                for (a, &p) in out.iter_mut().zip(d.next_row()?) {
                    *a = (p >> 8) as u8;
                }
            }
        }
        let prev = &self.previous;
        let first = self.y == 0;
        match self.filter {
            0 => {}
            // Horizontal, vertical, gradient. The first row and the first column always
            // predict from the pixel before them (left on row 0, above in column 0).
            f => {
                if first {
                    for x in 1..out.len() {
                        out[x] = out[x].wrapping_add(out[x - 1]);
                    }
                } else {
                    out[0] = out[0].wrapping_add(prev[0]);
                    for x in 1..out.len() {
                        let pred = match f {
                            1 => out[x - 1],
                            2 => prev[x],
                            _ => {
                                let g = i32::from(out[x - 1]) + i32::from(prev[x])
                                    - i32::from(prev[x - 1]);
                                g.clamp(0, 255) as u8
                            }
                        };
                        out[x] = out[x].wrapping_add(pred);
                    }
                }
            }
        }
        self.previous.copy_from_slice(out);
        self.y += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// RGBA from a PAM file, premultiplied the way the decoder outputs it.
    fn reference(pam: &[u8]) -> (u32, u32, Vec<u8>) {
        let end = pam.windows(7).position(|w| w == b"ENDHDR\n").unwrap() + 7;
        let head = std::str::from_utf8(&pam[..end]).unwrap();
        let field = |name: &str| -> u32 {
            let line = head.lines().find(|l| l.starts_with(name)).unwrap();
            line[name.len()..].trim().parse().unwrap()
        };
        let pixels = pam[end..]
            .chunks(4)
            .flat_map(|p| {
                let a = u32::from(p[3]);
                let pm = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
                [pm(p[0]), pm(p[1]), pm(p[2]), p[3]]
            })
            .collect();
        (field("WIDTH"), field("HEIGHT"), pixels)
    }

    /// Largest channel difference from the reference.
    fn max_error(webp: &[u8], pam: &[u8]) -> u8 {
        let (w, h, want) = reference(pam);
        let got = crate::decode(webp, w, h).unwrap();
        assert_eq!((got.width, got.height), (w, h));
        got.pixels
            .iter()
            .zip(&want)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap()
    }

    macro_rules! fixture {
        ($name:literal) => {
            (
                &include_bytes!(concat!("testdata/", $name, ".webp"))[..],
                &include_bytes!(concat!("testdata/", $name, ".ref.pam"))[..],
            )
        };
    }

    #[test]
    fn lossy_matches_libwebp() {
        // References come from `dwebp -nofancy`: chroma is point-sampled, as here.
        for (name, (webp, pam)) in [
            ("q80", fixture!("fractal_q80")),
            ("simple filter, sharpness", fixture!("fractal_simple")),
            ("no filter", fixture!("fractal_nofilter")),
        ] {
            assert_eq!(max_error(webp, pam), 0, "{name}");
        }
    }

    #[test]
    fn lossless_matches_libwebp() {
        for (name, (webp, pam)) in [
            ("best", fixture!("fractal_lossless")),
            ("fastest", fixture!("fractal_lossless_fast")),
            ("near lossless", fixture!("fractal_nearlossless")),
            ("alpha", fixture!("rgba_lossless")),
        ] {
            assert_eq!(max_error(webp, pam), 0, "{name}");
        }
    }

    #[test]
    fn lossy_with_alpha_matches_libwebp() {
        for (name, (webp, pam)) in [
            ("alpha", fixture!("rgba_lossy")),
            ("filtered alpha", fixture!("rgba_filtered")),
        ] {
            assert_eq!(max_error(webp, pam), 0, "{name}");
        }
    }

    #[test]
    fn scales_while_decoding_and_survives_damage() {
        let (webp, _) = fixture!("fractal_q80");
        let small = crate::decode(webp, 25, 18).unwrap();
        assert_eq!((small.width, small.height), (25, 18));
        for (webp, _) in [
            fixture!("fractal_q80"),
            fixture!("fractal_lossless"),
            fixture!("rgba_lossy"),
        ] {
            for len in [0, 12, 20, 30, 40, webp.len() / 2, webp.len() - 1] {
                let _ = crate::decode(&webp[..len], 50, 35);
            }
            let mut bad = webp.to_vec();
            for i in (30..bad.len()).step_by(11) {
                bad[i] ^= 0x5a;
            }
            let _ = crate::decode(&bad, 50, 35);
        }
    }
}
