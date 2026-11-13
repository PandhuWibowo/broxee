//! Paint effects whose values are lists: `box-shadow` and gradient backgrounds
//! (`linear-gradient()`, `radial-gradient()` and their `repeating-` forms).
//!
//! Specified values keep their lengths unresolved; the cascade resolves them against the
//! element's font size into computed values, which are integers so styles stay hashable, and
//! interns the lists in `StyleMap` (the style holds an index).

use crate::parser::{parse_length, split_top_level, split_values};
use crate::values::{parse_color, Color, Length, Px, SpecLength};

/// A color as written: `None` is `currentcolor`.
pub type SpecColor = Option<Color>;

#[derive(Clone, Debug, PartialEq)]
pub struct ShadowSpec {
    pub x: SpecLength,
    pub y: SpecLength,
    pub blur: SpecLength,
    pub spread: SpecLength,
    pub color: SpecColor,
    pub inset: bool,
}

/// Where a gradient's line or center is, as written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    /// Clockwise from "to top", in degrees.
    Angle(f32),
    /// `to left top` and the like: (-1, 0, 1) horizontally and vertically.
    Corner(i8, i8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RadialSize {
    ClosestSide,
    ClosestCorner,
    FarthestSide,
    FarthestCorner,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GradientShape {
    Linear(Direction),
    Radial {
        circle: bool,
        size: Result<RadialSize, (SpecLength, SpecLength)>,
        at: (SpecLength, SpecLength),
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GradientSpec {
    pub shape: GradientShape,
    pub repeating: bool,
    /// Each stop's color and position (`None`: spread evenly between its neighbours).
    pub stops: Vec<(SpecColor, Option<SpecLength>)>,
}

/// A list value, as parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectSpec {
    Shadows(Vec<ShadowSpec>),
    /// Background layers, first on top. Only gradients for now; others are dropped.
    Images(Vec<GradientSpec>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectSlot {
    BoxShadow,
    BackgroundImage,
}

pub const EFFECT_SLOTS: usize = 2;

/// A computed shadow: lengths in px, the color resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shadow {
    pub x: Px,
    pub y: Px,
    pub blur: Px,
    pub spread: Px,
    pub color: Color,
    pub inset: bool,
}

/// A computed gradient line or center.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GradientKind {
    /// Angle in hundredths of a degree, clockwise from "to top".
    Linear(i32),
    /// Towards a corner: the angle depends on the box's proportions.
    LinearCorner(i8, i8),
    Radial {
        circle: bool,
        /// A keyword, or explicit radii (horizontal, vertical; equal for a circle).
        size: Result<RadialSize, (Length, Length)>,
        at: (Length, Length),
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Gradient {
    pub kind: GradientKind,
    pub repeating: bool,
    /// Colors and positions (px or hundredths of a percent; `Auto` when spread evenly).
    pub stops: Box<[(Color, Length)]>,
}

/// Parse `box-shadow`. `none` is an empty list; `None` when invalid.
pub fn parse_shadows(v: &str) -> Option<Vec<ShadowSpec>> {
    if v.trim() == "none" {
        return Some(Vec::new());
    }
    split_top_level(v, b',')
        .into_iter()
        .map(|part| {
            let mut lengths = Vec::new();
            let mut color = None;
            let mut inset = false;
            for token in split_values(part.trim()) {
                if token == "inset" {
                    inset = true;
                } else if let Some(l) = parse_length(token).filter(|l| *l != SpecLength::Auto) {
                    lengths.push(l);
                } else if token == "currentcolor" {
                    color = Some(None);
                } else {
                    color = Some(Some(parse_color(token)?));
                }
            }
            if !(2..=4).contains(&lengths.len()) {
                return None;
            }
            let zero = SpecLength::Px(0.0);
            Some(ShadowSpec {
                x: lengths[0],
                y: lengths[1],
                blur: lengths.get(2).copied().unwrap_or(zero),
                spread: lengths.get(3).copied().unwrap_or(zero),
                color: color.unwrap_or(None),
                inset,
            })
        })
        .collect()
}

/// Parse a gradient function, or `None` if `v` is not one Broxee reads.
pub fn parse_gradient(v: &str) -> Option<GradientSpec> {
    let v = v.trim();
    let open = v.find('(')?;
    let inner = v.get(open + 1..v.len().checked_sub(1)?)?;
    if !v.ends_with(')') {
        return None;
    }
    let (repeating, linear) = match &v[..open] {
        "linear-gradient" => (false, true),
        "repeating-linear-gradient" => (true, true),
        "radial-gradient" => (false, false),
        "repeating-radial-gradient" => (true, false),
        _ => return None,
    };
    let parts = split_top_level(inner, b',');
    let (first, rest) = parts.split_first()?;
    let (shape, stops) = if linear {
        match parse_direction(first) {
            Some(d) => (GradientShape::Linear(d), rest),
            None => (GradientShape::Linear(Direction::Angle(180.0)), &parts[..]),
        }
    } else {
        match parse_radial(first) {
            Some(shape) => (shape, rest),
            None => (default_radial(), &parts[..]),
        }
    };
    let mut out = Vec::new();
    for stop in stops {
        let tokens = split_values(stop.trim());
        let (color, positions) = match tokens.split_first()? {
            (&"currentcolor", p) => (None, p),
            (c, p) => match parse_color(c) {
                Some(c) => (Some(c), p),
                // A lone position is a color hint: skipped, the midpoint stays halfway.
                None if p.is_empty() && parse_length(c).is_some() => continue,
                None => return None,
            },
        };
        match positions {
            [] => out.push((color, None)),
            [p] => out.push((color, Some(parse_length(p)?))),
            [a, b] => {
                out.push((color, Some(parse_length(a)?)));
                out.push((color, Some(parse_length(b)?)));
            }
            _ => return None,
        }
    }
    (out.len() >= 2).then_some(GradientSpec {
        shape,
        repeating,
        stops: out,
    })
}

fn parse_direction(v: &str) -> Option<Direction> {
    let v = v.trim();
    if let Some(sides) = v.strip_prefix("to ") {
        let (mut dx, mut dy) = (0i8, 0i8);
        for side in sides.split_ascii_whitespace() {
            match side {
                "left" => dx = -1,
                "right" => dx = 1,
                "top" => dy = -1,
                "bottom" => dy = 1,
                _ => return None,
            }
        }
        return match (dx, dy) {
            (0, 0) => None,
            (0, -1) => Some(Direction::Angle(0.0)),
            (1, 0) => Some(Direction::Angle(90.0)),
            (0, 1) => Some(Direction::Angle(180.0)),
            (-1, 0) => Some(Direction::Angle(270.0)),
            (dx, dy) => Some(Direction::Corner(dx, dy)),
        };
    }
    parse_angle(v).map(Direction::Angle)
}

fn parse_angle(v: &str) -> Option<f32> {
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: f32 = num.parse().ok().filter(|n: &f32| n.is_finite())?;
    Some(match unit {
        "deg" => n,
        "turn" => n * 360.0,
        "rad" => n.to_degrees(),
        "grad" => n * 0.9,
        "" if n == 0.0 => 0.0,
        _ => return None,
    })
}

fn default_radial() -> GradientShape {
    let center = SpecLength::Percent(50.0);
    GradientShape::Radial {
        circle: false,
        size: Ok(RadialSize::FarthestCorner),
        at: (center, center),
    }
}

/// `circle closest-side at 30% top` and the like; `None` if `v` is a color stop instead.
fn parse_radial(v: &str) -> Option<GradientShape> {
    let v = v.trim();
    let (shape_part, at_part) = match v.find("at ") {
        Some(0) => ("", Some(&v[3..])),
        Some(i) if v[..i].ends_with(' ') => (&v[..i], Some(&v[i + 3..])),
        _ => (v, None),
    };
    let mut circle = None;
    let mut size = None;
    let mut lengths = Vec::new();
    for token in shape_part.split_ascii_whitespace() {
        match token {
            "circle" => circle = Some(true),
            "ellipse" => circle = Some(false),
            "closest-side" => size = Some(RadialSize::ClosestSide),
            "closest-corner" => size = Some(RadialSize::ClosestCorner),
            "farthest-side" => size = Some(RadialSize::FarthestSide),
            "farthest-corner" | "cover" => size = Some(RadialSize::FarthestCorner),
            t => lengths.push(parse_length(t).filter(|l| *l != SpecLength::Auto)?),
        }
    }
    let size = match (size, lengths.as_slice()) {
        (Some(keyword), []) => Ok(keyword),
        (None, []) => Ok(RadialSize::FarthestCorner),
        (None, [r]) => Err((*r, *r)),
        (None, [rx, ry]) => Err((*rx, *ry)),
        _ => return None,
    };
    let circle = circle.unwrap_or(matches!(size, Err((a, b)) if a == b && lengths.len() == 1));
    let at = match at_part {
        Some(p) => parse_position(p)?,
        None => (SpecLength::Percent(50.0), SpecLength::Percent(50.0)),
    };
    if shape_part.is_empty() && at_part.is_none() {
        return None;
    }
    Some(GradientShape::Radial { circle, size, at })
}

/// `center`, `left top`, `30% 70%`, `10px`: a horizontal and a vertical position.
pub(crate) fn parse_position(v: &str) -> Option<(SpecLength, SpecLength)> {
    let pct = SpecLength::Percent;
    let mut x = None;
    let mut y = None;
    let mut loose = Vec::new();
    for token in v.split_ascii_whitespace() {
        match token {
            "left" => x = Some(pct(0.0)),
            "right" => x = Some(pct(100.0)),
            "top" => y = Some(pct(0.0)),
            "bottom" => y = Some(pct(100.0)),
            "center" => loose.push(pct(50.0)),
            t => loose.push(parse_length(t).filter(|l| *l != SpecLength::Auto)?),
        }
    }
    for value in loose {
        if x.is_none() {
            x = Some(value);
        } else if y.is_none() {
            y = Some(value);
        } else {
            return None;
        }
    }
    Some((x.unwrap_or(pct(50.0)), y.unwrap_or(pct(50.0))))
}

/// The layers of `background-image` (or the image part of `background`) Broxee can draw.
/// `None` when nothing in `v` is an image: the declaration does not set the property.
pub fn parse_images(v: &str) -> Option<Vec<GradientSpec>> {
    if v.trim() == "none" {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut any = false;
    for layer in split_top_level(v, b',') {
        for token in split_values(layer.trim()) {
            if token.contains("gradient(") {
                any = true;
                out.extend(parse_gradient(token));
            } else if token.starts_with("url(") || token == "none" {
                any = true;
            }
        }
    }
    any.then_some(out)
}

/// Resolve a specified length against the font sizes; `None` for percentages and `auto`.
pub(crate) fn px(l: SpecLength, resolve: &impl Fn(SpecLength) -> Length) -> Px {
    match resolve(l) {
        Length::Px(p) => p,
        _ => Px::ZERO,
    }
}

pub(crate) fn compute_shadows(
    specs: &[ShadowSpec],
    current: Color,
    resolve: &impl Fn(SpecLength) -> Length,
) -> Vec<Shadow> {
    specs
        .iter()
        .map(|s| Shadow {
            x: px(s.x, resolve),
            y: px(s.y, resolve),
            blur: px(s.blur, resolve).max(Px::ZERO),
            spread: px(s.spread, resolve),
            color: s.color.unwrap_or(current),
            inset: s.inset,
        })
        .filter(|s| s.color.a > 0)
        .collect()
}

pub(crate) fn compute_gradients(
    specs: &[GradientSpec],
    current: Color,
    resolve: &impl Fn(SpecLength) -> Length,
) -> Vec<Gradient> {
    specs
        .iter()
        .map(|g| Gradient {
            kind: match &g.shape {
                GradientShape::Linear(Direction::Angle(a)) => {
                    GradientKind::Linear((a.rem_euclid(360.0) * 100.0).round() as i32)
                }
                GradientShape::Linear(Direction::Corner(dx, dy)) => {
                    GradientKind::LinearCorner(*dx, *dy)
                }
                GradientShape::Radial { circle, size, at } => GradientKind::Radial {
                    circle: *circle,
                    size: size.map_err(|(rx, ry)| (resolve(rx), resolve(ry))),
                    at: (resolve(at.0), resolve(at.1)),
                },
            },
            repeating: g.repeating,
            stops: g
                .stops
                .iter()
                .map(|(c, p)| (c.unwrap_or(current), p.map_or(Length::Auto, resolve)))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shadows() {
        let s = parse_shadows("0 1px 2px rgba(0, 0, 0, 0.5), inset 0 0 0 1px #fff").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(
            (s[0].y, s[0].blur, s[0].spread),
            (
                SpecLength::Px(1.0),
                SpecLength::Px(2.0),
                SpecLength::Px(0.0)
            )
        );
        assert_eq!(s[0].color.unwrap().a, 128);
        assert!(s[1].inset && !s[0].inset);
        assert_eq!(s[1].spread, SpecLength::Px(1.0));
        assert_eq!(parse_shadows("none").unwrap(), []);
        assert!(parse_shadows("1px").is_none());
        assert!(parse_shadows("1px 2px bogus").is_none());
        assert_eq!(
            parse_shadows("2px 2px currentcolor").unwrap()[0].color,
            None
        );
    }

    #[test]
    fn parses_linear_gradients() {
        let g = parse_gradient("linear-gradient(to right, #fff, rgba(0,0,0,.5) 40%, red 3px 50%)")
            .unwrap();
        assert_eq!(g.shape, GradientShape::Linear(Direction::Angle(90.0)));
        assert_eq!(g.stops.len(), 4, "a double position makes two stops");
        assert_eq!(g.stops[1].1, Some(SpecLength::Percent(40.0)));
        let g = parse_gradient("repeating-linear-gradient(45deg, red, blue 10px)").unwrap();
        assert!(g.repeating);
        assert_eq!(g.shape, GradientShape::Linear(Direction::Angle(45.0)));
        let g = parse_gradient("linear-gradient(to top left, red, blue)").unwrap();
        assert_eq!(g.shape, GradientShape::Linear(Direction::Corner(-1, -1)));
        let g = parse_gradient("linear-gradient(red, 30%, blue)").unwrap();
        assert_eq!(g.stops.len(), 2, "color hints are skipped");
        assert_eq!(
            parse_gradient("linear-gradient(0.25turn, red, blue)")
                .unwrap()
                .shape,
            GradientShape::Linear(Direction::Angle(90.0))
        );
        assert!(parse_gradient("linear-gradient(red)").is_none());
        assert!(parse_gradient("linear-gradient(to middle, red, blue)").is_none());
        assert!(parse_gradient("conic-gradient(red, blue)").is_none());
    }

    #[test]
    fn parses_radial_gradients() {
        let g = parse_gradient("radial-gradient(circle at top left, red, blue)").unwrap();
        assert_eq!(
            g.shape,
            GradientShape::Radial {
                circle: true,
                size: Ok(RadialSize::FarthestCorner),
                at: (SpecLength::Percent(0.0), SpecLength::Percent(0.0)),
            }
        );
        let g = parse_gradient("radial-gradient(20px 40%, red, blue)").unwrap();
        assert!(matches!(
            g.shape,
            GradientShape::Radial {
                circle: false,
                size: Err((SpecLength::Px(_), SpecLength::Percent(_))),
                ..
            }
        ));
        let g = parse_gradient("radial-gradient(red, blue)").unwrap();
        assert_eq!(g.shape, default_radial());
    }

    #[test]
    fn finds_gradient_layers_in_backgrounds() {
        let layers = parse_images("url(a.png) no-repeat, linear-gradient(red, blue) #fff").unwrap();
        assert_eq!(layers.len(), 1, "urls come later; the gradient is kept");
        assert!(parse_images("#fff").is_none(), "only a color: no image");
        assert_eq!(parse_images("none").unwrap(), []);
    }
}
