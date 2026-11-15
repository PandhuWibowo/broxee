//! Comparing rendered frames, for the site regression suite (`scripts/sites.sh`).
//!
//! A frame is summed up as one hash per square tile, written as text: a few kilobytes that
//! can live in the repository, where the frames themselves would be megabytes each. Renders
//! are exact (the same build gives the same bytes on one platform), so any changed tile is a
//! change. `diff` then shows where two frames differ, pixel by pixel.

use std::fmt::Write;

/// One line of header, then one line per row of tiles: each tile's hash in hex, space-separated.
pub fn tiles(width: usize, height: usize, rgba: &[u8], tile: usize) -> String {
    let tile = tile.max(1);
    let (cols, rows) = (width.div_ceil(tile), height.div_ceil(tile));
    let mut out = format!("# tiles {tile} px, frame {width}x{height}\n");
    let mut hashes = vec![0xcbf2_9ce4_8422_2325u64; cols];
    for row in 0..rows {
        hashes.fill(0xcbf2_9ce4_8422_2325);
        for y in row * tile..((row + 1) * tile).min(height) {
            let line = &rgba[y * width * 4..(y + 1) * width * 4];
            for (col, hash) in hashes.iter_mut().enumerate() {
                let start = col * tile * 4;
                let end = ((col + 1) * tile).min(width) * 4;
                // FNV-1a over 4-byte pixels: a pixel either matches or it does not.
                for px in line[start..end].as_chunks::<4>().0 {
                    let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                    *hash = (*hash ^ u64::from(v)).wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        for (col, hash) in hashes.iter().enumerate() {
            if col > 0 {
                out.push(' ');
            }
            let _ = write!(out, "{hash:016x}");
        }
        out.push('\n');
    }
    out
}

/// Tiles that differ between two `tiles` texts, as (column, row). A different header (frame
/// size or tile size) or a missing row makes every tile of the larger one differ.
pub fn changed_tiles(old: &str, new: &str) -> Vec<(usize, usize)> {
    let mut old_lines = old.lines();
    let mut new_lines = new.lines();
    let same_header = old_lines.next() == new_lines.next();
    let old_rows: Vec<Vec<&str>> = old_lines.map(|l| l.split(' ').collect()).collect();
    let new_rows: Vec<Vec<&str>> = new_lines.map(|l| l.split(' ').collect()).collect();
    let mut changed = Vec::new();
    for row in 0..old_rows.len().max(new_rows.len()) {
        let a = old_rows.get(row).map_or(&[][..], |r| &r[..]);
        let b = new_rows.get(row).map_or(&[][..], |r| &r[..]);
        for col in 0..a.len().max(b.len()) {
            if !same_header || a.get(col) != b.get(col) {
                changed.push((col, row));
            }
        }
    }
    changed
}

/// Where two frames of one size differ.
#[derive(Debug, PartialEq)]
pub struct Diff {
    /// Pixels whose color differs at all.
    pub pixels: usize,
    /// The smallest rectangle holding them: x, y, width, height (zero when none differ).
    pub bounds: (usize, usize, usize, usize),
    /// The new frame faded, with differing pixels in red: what a person looks at.
    pub image: Vec<u8>,
}

/// Compare two RGBA frames of `width` x `height`.
pub fn diff(width: usize, height: usize, old: &[u8], new: &[u8]) -> Diff {
    let mut image = Vec::with_capacity(width * height * 4);
    let mut pixels = 0;
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (i, (a, b)) in old
        .as_chunks::<4>()
        .0
        .iter()
        .zip(new.as_chunks::<4>().0)
        .enumerate()
    {
        if a == b {
            // Faded toward white, so the red stands out.
            image.extend(b[..3].iter().map(|&c| 192 + c / 4));
            image.push(255);
        } else {
            pixels += 1;
            let (x, y) = (i % width, i / width);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            image.extend_from_slice(&[230, 0, 0, 255]);
        }
    }
    image.resize(width * height * 4, 255);
    let bounds = if pixels == 0 {
        (0, 0, 0, 0)
    } else {
        (x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    };
    Diff {
        pixels,
        bounds,
        image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, color: [u8; 4]) -> Vec<u8> {
        color.repeat(width * height)
    }

    #[test]
    fn one_changed_pixel_changes_one_tile() {
        let (w, h) = (100, 70);
        let old = frame(w, h, [255, 255, 255, 255]);
        let mut new = old.clone();
        let (x, y) = (70, 40);
        new[(y * w + x) * 4] = 254;
        let (a, b) = (tiles(w, h, &old, 32), tiles(w, h, &new, 32));
        // 4 columns (the last 4 px wide) and 3 rows (the last 6 px tall).
        assert_eq!(a.lines().count(), 1 + 3);
        assert_eq!(a.lines().nth(1).unwrap().split(' ').count(), 4);
        assert_eq!(changed_tiles(&a, &b), vec![(2, 1)]);
        assert!(changed_tiles(&a, &a).is_empty());

        let d = diff(w, h, &old, &new);
        assert_eq!(d.pixels, 1);
        assert_eq!(d.bounds, (70, 40, 1, 1));
        assert_eq!(&d.image[(y * w + x) * 4..][..4], &[230, 0, 0, 255]);
        assert_eq!(d.image.len(), w * h * 4);
    }

    #[test]
    fn a_shifted_block_shows_where_it_moved() {
        let (w, h) = (64, 64);
        let mut old = frame(w, h, [255, 255, 255, 255]);
        let mut new = old.clone();
        for y in 10..20 {
            for x in 10..20 {
                old[(y * w + x) * 4..][..4].copy_from_slice(&[0, 0, 0, 255]);
                new[(y * w + x + 30) * 4..][..4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let d = diff(w, h, &old, &new);
        assert_eq!(d.pixels, 200);
        assert_eq!(d.bounds, (10, 10, 40, 10));
        let changed = changed_tiles(&tiles(w, h, &old, 16), &tiles(w, h, &new, 16));
        assert_eq!(
            changed,
            vec![
                (0, 0),
                (1, 0),
                (2, 0),
                (3, 0),
                (0, 1),
                (1, 1),
                (2, 1),
                (3, 1)
            ]
        );
    }

    #[test]
    fn a_new_size_changes_every_tile() {
        let a = tiles(32, 32, &frame(32, 32, [1, 2, 3, 255]), 16);
        let b = tiles(32, 48, &frame(32, 48, [1, 2, 3, 255]), 16);
        assert_eq!(changed_tiles(&a, &b).len(), 6);
        // Damaged text never panics.
        assert!(!changed_tiles("", "# x\nzz").is_empty());
    }
}
