//! `transform` and `transform-origin`: two-dimensional transforms. Functions without a 2D
//! meaning (`rotateX`, `perspective`, the z parts of `translate3d`) are left out, as identity.

use crate::effects::parse_angle;
use crate::parser::{parse_length, split_top_level};
use crate::values::{Length, SpecLength};

/// One transform function as written.
#[derive(Clone, Debug, PartialEq)]
pub enum TransformSpec {
    Translate(SpecLength, SpecLength),
    Scale(f32, f32),
    /// Degrees, clockwise.
    Rotate(f32),
    Skew(f32, f32),
    Matrix([f32; 6]),
}

/// A computed transform function. Integers so styles stay hashable: factors in ten
/// thousandths, angles in hundredths of a degree. The list a style holds starts with its
/// `transform-origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransformOp {
    /// Where the functions apply from, against the border box (50% 50% unless set).
    Origin(Length, Length),
    Translate(Length, Length),
    Scale(i32, i32),
    Rotate(i32),
    Skew(i32, i32),
    /// `a b c d` in ten thousandths, `e f` in px.
    Matrix([i32; 4], Length, Length),
}

/// Parse `transform`: `none` is an empty list; `None` when a function is malformed, which
/// drops the declaration as browsers do.
pub fn parse_transform(v: &str) -> Option<Vec<TransformSpec>> {
    let v = v.trim();
    if v == "none" {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut rest = v;
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim();
        let close = open + rest[open..].find(')')?;
        let args: Vec<&str> = split_top_level(&rest[open + 1..close], b',')
            .into_iter()
            .map(str::trim)
            .collect();
        rest = rest[close + 1..].trim_start();
        let len = |i: usize| args.get(i).and_then(|a| parse_length(a));
        let num = |i: usize| {
            args.get(i)
                .and_then(|a| a.parse::<f32>().ok())
                .filter(|n| n.is_finite())
        };
        let angle = |i: usize| args.get(i).and_then(|a| parse_angle(a));
        let zero = SpecLength::Px(0.0);
        out.push(match name {
            "translate" => TransformSpec::Translate(len(0)?, len(1).unwrap_or(zero)),
            "translatex" => TransformSpec::Translate(len(0)?, zero),
            "translatey" => TransformSpec::Translate(zero, len(0)?),
            "translate3d" => TransformSpec::Translate(len(0)?, len(1)?),
            "scale" => {
                let s = num(0)?;
                TransformSpec::Scale(s, num(1).unwrap_or(s))
            }
            "scalex" => TransformSpec::Scale(num(0)?, 1.0),
            "scaley" => TransformSpec::Scale(1.0, num(0)?),
            "scale3d" => TransformSpec::Scale(num(0)?, num(1)?),
            "rotate" | "rotatez" => TransformSpec::Rotate(angle(0)?),
            "skew" => TransformSpec::Skew(angle(0)?, angle(1).unwrap_or(0.0)),
            "skewx" => TransformSpec::Skew(angle(0)?, 0.0),
            "skewy" => TransformSpec::Skew(0.0, angle(0)?),
            "matrix" => {
                let m: Vec<f32> = (0..6).map(num).collect::<Option<_>>()?;
                TransformSpec::Matrix([m[0], m[1], m[2], m[3], m[4], m[5]])
            }
            // No 2D meaning: identity, but well formed.
            "translatez" | "scalez" | "rotatex" | "rotatey" | "rotate3d" | "perspective"
            | "matrix3d" => continue,
            _ => return None,
        });
    }
    Some(out)
}

pub(crate) fn compute_transform(
    specs: &[TransformSpec],
    origin: (Length, Length),
    resolve: &impl Fn(SpecLength) -> Length,
) -> Vec<TransformOp> {
    if specs.is_empty() {
        return Vec::new();
    }
    let k = |f: f32| (f * 10000.0).round().clamp(-2e9, 2e9) as i32;
    let deg = |a: f32| (a * 100.0).round().clamp(-2e9, 2e9) as i32;
    let px = |v: f32| Length::Px(crate::values::Px::new(v));
    std::iter::once(TransformOp::Origin(origin.0, origin.1))
        .chain(specs.iter().map(|s| match *s {
            TransformSpec::Translate(x, y) => TransformOp::Translate(resolve(x), resolve(y)),
            TransformSpec::Scale(x, y) => TransformOp::Scale(k(x), k(y)),
            TransformSpec::Rotate(a) => TransformOp::Rotate(deg(a)),
            TransformSpec::Skew(x, y) => TransformOp::Skew(deg(x), deg(y)),
            TransformSpec::Matrix([a, b, c, d, e, f]) => {
                TransformOp::Matrix([k(a), k(b), k(c), k(d)], px(e), px(f))
            }
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transform_lists() {
        let t = parse_transform("translate(-50%, -50%) rotate(45deg) scale(1.5)").unwrap();
        assert_eq!(
            t,
            [
                TransformSpec::Translate(SpecLength::Percent(-50.0), SpecLength::Percent(-50.0)),
                TransformSpec::Rotate(45.0),
                TransformSpec::Scale(1.5, 1.5),
            ]
        );
        assert_eq!(parse_transform("none").unwrap(), []);
        assert_eq!(
            parse_transform("translatez(0) translate3d(10px, 0, 0)").unwrap(),
            [TransformSpec::Translate(
                SpecLength::Px(10.0),
                SpecLength::Px(0.0)
            )],
            "the z part is identity"
        );
        assert_eq!(
            parse_transform("rotate(0.5turn) skewx(10deg)").unwrap(),
            [TransformSpec::Rotate(180.0), TransformSpec::Skew(10.0, 0.0)]
        );
        assert_eq!(
            parse_transform("matrix(1, 0, 0, 1, 30, 40)").unwrap(),
            [TransformSpec::Matrix([1.0, 0.0, 0.0, 1.0, 30.0, 40.0])]
        );
        assert!(
            parse_transform("rotate(45)").is_none(),
            "an angle needs a unit"
        );
        assert!(parse_transform("wobble(1)").is_none());
        assert!(parse_transform("scale(").is_none());
    }
}
