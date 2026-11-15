//! `cargo run --release -p paint --example framediff -- OLD.png NEW.png DIFF.png`
//!
//! Compares two renders of the same size (`broxee --png`), writes DIFF.png (the new render
//! faded, with differing pixels in red) and prints where they differ. Exits 1 when they do.
//! `scripts/sites.sh --base REV` runs it on each page rendered at two commits.

use std::process::ExitCode;

fn load(path: &str) -> Result<image::Bitmap, String> {
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let info = image::probe(&data).ok_or(format!("{path}: not an image"))?;
    image::decode(&data, info.width, info.height).map_err(|e| format!("{path}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [old, new, out] = &args[..] else {
        eprintln!("usage: framediff OLD.png NEW.png DIFF.png");
        return ExitCode::from(2);
    };
    let (a, b) = match (load(old), load(new)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if (a.width, a.height) != (b.width, b.height) {
        println!(
            "size changed: {}x{} -> {}x{}",
            a.width, a.height, b.width, b.height
        );
        return ExitCode::FAILURE;
    }
    let (w, h) = (b.width as usize, b.height as usize);
    let d = paint::compare::diff(w, h, &a.pixels, &b.pixels);
    if d.pixels == 0 {
        println!("identical");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = std::fs::write(out, paint::png::encode(w, h, &d.image)) {
        eprintln!("{out}: {e}");
        return ExitCode::from(2);
    }
    let (x, y, bw, bh) = d.bounds;
    println!(
        "{} pixels differ ({:.3}%), within {bw}x{bh} at {x},{y}: {out}",
        d.pixels,
        d.pixels as f64 * 100.0 / (w * h) as f64
    );
    ExitCode::FAILURE
}
