//! Transcendental functions that give the same bits on every platform.
//!
//! `f32::sin`, `cos`, `tan`, `atan2` and `exp` call the system's libm, and Apple's and glibc's
//! differ in the last bit for some inputs. One bit is enough to change a pixel's coverage, so
//! an SVG circle or a rotated box rendered differently on macOS and Linux (`scripts/sites.sh`
//! compares exact pixels). These use only addition, multiplication, division, `sqrt` and
//! `round`, which IEEE 754 makes exact everywhere, computed in f64 and rounded once to f32.
//! Rust never fuses a multiply and an add on its own, so the results are fixed.

use std::f64::consts::{FRAC_PI_2, PI};

/// π/2 split in two, so `x - k·π/2` stays exact for the angles pages use (Cody and Waite).
const PIO2_HI: f64 = FRAC_PI_2;
const PIO2_LO: f64 = 6.123_233_995_736_766e-17;

/// sin(r) and cos(r) for |r| ≤ π/4: Taylor series to r¹⁹ and r¹⁸ (error below 1e-19).
fn kernel(r: f64) -> (f64, f64) {
    let r2 = r * r;
    let (mut s, mut c) = (0.0, 0.0);
    // Horner from the highest term: 1/(n!) with alternating signs.
    let mut n = 19.0;
    while n > 1.0 {
        s = 1.0 / (n * (n - 1.0)) * (1.0 - r2 * s);
        n -= 2.0;
    }
    let mut n = 18.0;
    while n > 0.0 {
        c = 1.0 / (n * (n - 1.0)) * (1.0 - r2 * c);
        n -= 2.0;
    }
    (r * (1.0 - r2 * s), 1.0 - r2 * c)
}

fn sin_cos64(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let k = (x / FRAC_PI_2).round();
    let r = (x - k * PIO2_HI) - k * PIO2_LO;
    let (s, c) = kernel(r);
    match (k.rem_euclid(4.0)) as u8 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// Sine and cosine of `x` radians.
pub fn sin_cos(x: f32) -> (f32, f32) {
    let (s, c) = sin_cos64(f64::from(x));
    (s as f32, c as f32)
}

pub fn sin(x: f32) -> f32 {
    sin_cos(x).0
}

pub fn cos(x: f32) -> f32 {
    sin_cos(x).1
}

pub fn tan(x: f32) -> f32 {
    let (s, c) = sin_cos64(f64::from(x));
    (s / c) as f32
}

/// atan(t) for |t| ≤ 1: the argument halved twice (atan t = 2·atan(t / (1 + √(1 + t²)))),
/// leaving |t| ≤ tan(π/16) ≈ 0.2, then the series to t²⁷ (error below 1e-19).
fn atan_small(t: f64) -> f64 {
    let mut t = t;
    for _ in 0..2 {
        t /= 1.0 + (1.0 + t * t).sqrt();
    }
    let t2 = t * t;
    let mut sum = 0.0;
    let mut n = 27.0;
    while n > 1.0 {
        sum = 1.0 / n - t2 * sum;
        n -= 2.0;
    }
    4.0 * t * (1.0 - t2 * sum)
}

/// The angle of the point (x, y) from the positive x axis, in (-π, π].
pub fn atan2(y: f32, x: f32) -> f32 {
    let (y, x) = (f64::from(y), f64::from(x));
    if y.is_nan() || x.is_nan() {
        return f32::NAN;
    }
    if x == 0.0 && y == 0.0 {
        return if x.is_sign_negative() {
            (PI as f32).copysign(y as f32)
        } else {
            0.0f32.copysign(y as f32)
        };
    }
    // Infinities become the largest finite values, which gives the same limits
    // (atan2(∞, ∞) = π/4, atan2(y, -∞) = ±π).
    let (y, x) = (y.clamp(-f64::MAX, f64::MAX), x.clamp(-f64::MAX, f64::MAX));
    if x == 0.0 {
        return FRAC_PI_2.copysign(y) as f32;
    }
    // Reduce to |t| ≤ 1, then place the angle in its octant.
    let a = if y.abs() <= x.abs() {
        atan_small(y / x)
    } else {
        FRAC_PI_2.copysign(y / x) - atan_small(x / y)
    };
    let angle = if x > 0.0 {
        a
    } else if y >= 0.0 {
        a + PI
    } else {
        a - PI
    };
    angle as f32
}

/// e to the power `x`.
pub fn exp(x: f32) -> f32 {
    let x = f64::from(x);
    if x.is_nan() {
        return f32::NAN;
    }
    if x > 89.0 {
        return f32::INFINITY;
    }
    if x < -104.0 {
        return 0.0;
    }
    // x = k·ln 2 + r with |r| ≤ ln 2 / 2, e^r from its series to r¹⁴, times 2^k.
    const LN2: f64 = std::f64::consts::LN_2;
    let k = (x / LN2).round();
    let r = x - k * LN2;
    let mut sum = 1.0;
    let mut n = 14.0;
    while n > 0.0 {
        sum = 1.0 + r / n * sum;
        n -= 1.0;
    }
    let scale = f64::from_bits(((k as i64 + 1023) as u64) << 52);
    (sum * scale) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 2.0 * f32::EPSILON * b.abs().max(1.0)
    }

    #[test]
    fn agrees_with_the_system_within_rounding() {
        for i in -20_000..=20_000 {
            let x = i as f32 * 0.001_7;
            let (s, c) = sin_cos(x);
            assert!(close(s, x.sin()), "sin {x}: {s} vs {}", x.sin());
            assert!(close(c, x.cos()), "cos {x}: {c} vs {}", x.cos());
            assert!(close(exp(x / 400.0), (x / 400.0).exp()), "exp {x}");
            let y = (i as f32 * 0.37).sin() * 5.0;
            assert!(
                close(atan2(y, x), y.atan2(x)),
                "atan2 {y} {x}: {} vs {}",
                atan2(y, x),
                y.atan2(x)
            );
        }
        for deg in [0.0f32, 10.0, 30.0, 45.0, 60.0, 80.0, -15.0] {
            let r = deg.to_radians();
            assert!(close(tan(r), r.tan()), "tan {deg}");
        }
        assert!(close(exp(-10.0), (-10.0f32).exp()));
        assert!(close(exp(20.0), 20.0f32.exp()));
    }

    #[test]
    fn exact_at_the_quarter_turns() {
        assert_eq!(sin_cos(0.0), (0.0, 1.0));
        assert_eq!(sin(std::f32::consts::FRAC_PI_2), 1.0);
        assert_eq!(cos(std::f32::consts::PI), -1.0);
        assert_eq!(atan2(0.0, 1.0), 0.0);
        assert_eq!(atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2);
        assert_eq!(atan2(0.0, -1.0), std::f32::consts::PI);
        assert_eq!(atan2(-1.0, 0.0), -std::f32::consts::FRAC_PI_2);
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(-200.0), 0.0);
        assert!(sin(f32::NAN).is_nan() && sin(f32::INFINITY).is_nan());
    }
}
