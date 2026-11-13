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
    /// Background layers, first on top.
    Images(Vec<ImageSpec>),
    Transform(Vec<crate::transform::TransformSpec>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectSlot {
    BoxShadow,
    BackgroundImage,
    Transform,
}

pub const EFFECT_SLOTS: usize = 3;

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

pub(crate) fn parse_angle(v: &str) -> Option<f32> {
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

/// One `background-image` layer as written.
#[derive(Clone, Debug, PartialEq)]
pub enum ImageSpec {
    Gradient(GradientSpec),
    /// As written, case kept: a stylesheet's are made absolute after parsing
    /// (`Stylesheet::resolve_urls`), a `style` attribute's against the page.
    Url(String),
}

/// The layers of `background-image` (or the image part of `background`), first on top.
/// `None` when nothing in `v` is an image: the declaration does not set the property. `v`
/// keeps its case, for URLs.
pub fn parse_images(v: &str) -> Option<Vec<ImageSpec>> {
    if v.trim().eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut any = false;
    for layer in split_top_level(v, b',') {
        for token in split_values(layer.trim()) {
            let lower = token.to_ascii_lowercase();
            if lower.contains("gradient(") {
                any = true;
                out.extend(parse_gradient(&lower).map(ImageSpec::Gradient));
            } else if lower.starts_with("url(") {
                any = true;
                out.extend(crate::fontface::url_function(token).map(ImageSpec::Url));
            } else if lower == "none" {
                any = true;
            }
        }
    }
    any.then_some(out)
}

/// `background-size` as written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SizeSpec {
    Cover,
    Contain,
    /// Width and height; `Auto` keeps the image's proportions.
    Lengths(SpecLength, SpecLength),
}

pub(crate) fn parse_size(v: &str) -> Option<SizeSpec> {
    let first = split_top_level(v, b',')
        .into_iter()
        .next()?
        .trim()
        .to_ascii_lowercase();
    match first.as_str() {
        "cover" => Some(SizeSpec::Cover),
        "contain" => Some(SizeSpec::Contain),
        _ => {
            let parts: Vec<SpecLength> = first
                .split_ascii_whitespace()
                .map(parse_length)
                .collect::<Option<_>>()?;
            match parts.as_slice() {
                [w] => Some(SizeSpec::Lengths(*w, SpecLength::Auto)),
                [w, h] => Some(SizeSpec::Lengths(*w, *h)),
                _ => None,
            }
        }
    }
}

/// `background-repeat`: whether it repeats across, and down. `space` and `round` repeat.
pub(crate) fn parse_repeat(v: &str) -> Option<(bool, bool)> {
    let first = split_top_level(v, b',')
        .into_iter()
        .next()?
        .trim()
        .to_ascii_lowercase();
    let one = |t: &str| match t {
        "repeat" | "space" | "round" => Some(true),
        "no-repeat" => Some(false),
        _ => None,
    };
    let parts: Vec<&str> = first.split_ascii_whitespace().collect();
    match parts.as_slice() {
        ["repeat-x"] => Some((true, false)),
        ["repeat-y"] => Some((false, true)),
        [t] => one(t).map(|r| (r, r)),
        [x, y] => Some((one(x)?, one(y)?)),
        _ => None,
    }
}

/// `background-position` for the first layer.
pub(crate) fn parse_bg_position(v: &str) -> Option<(SpecLength, SpecLength)> {
    let first = split_top_level(v, b',')
        .into_iter()
        .next()?
        .trim()
        .to_ascii_lowercase();
    Some(position_or_keywords(&first))
}

/// A position, or for the three and four value forms (`right 10px top 20%`), which offset
/// from an edge, the edges alone: the offset would need `calc(100% - 10px)`.
fn position_or_keywords(v: &str) -> (SpecLength, SpecLength) {
    let zero = (SpecLength::Percent(0.0), SpecLength::Percent(0.0));
    parse_position(v).unwrap_or_else(|| {
        let keywords: Vec<&str> = v
            .split_ascii_whitespace()
            .filter(|w| matches!(*w, "left" | "right" | "top" | "bottom" | "center"))
            .collect();
        parse_position(&keywords.join(" ")).unwrap_or(zero)
    })
}

/// What `background` sets besides its color and images: position, size and repeat of the
/// first layer, each back to its initial value when not given.
pub(crate) fn parse_background_rest(v: &str) -> ((SpecLength, SpecLength), SizeSpec, (bool, bool)) {
    let first = split_top_level(v, b',')
        .into_iter()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut position_words = Vec::new();
    let mut size_words = Vec::new();
    let mut repeat = (true, true);
    let mut after_slash = false;
    for token in split_values(first.trim()) {
        if token.contains('(') || parse_color(token).is_some() {
            continue;
        }
        for (k, piece) in token.split('/').enumerate() {
            if k > 0 {
                after_slash = true;
            }
            if piece.is_empty() {
                continue;
            }
            if let Some(r) = parse_repeat(piece) {
                repeat = r;
            } else if after_slash {
                size_words.push(piece.to_string());
            } else if matches!(piece, "left" | "right" | "top" | "bottom" | "center")
                || parse_length(piece).is_some()
            {
                position_words.push(piece.to_string());
            }
        }
    }
    let position = position_or_keywords(&position_words.join(" "));
    let position = if position_words.is_empty() {
        (SpecLength::Percent(0.0), SpecLength::Percent(0.0))
    } else {
        position
    };
    let size = parse_size(&size_words.join(" "))
        .unwrap_or(SizeSpec::Lengths(SpecLength::Auto, SpecLength::Auto));
    (position, size, repeat)
}

/// `background-size` computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackgroundSize {
    Cover,
    Contain,
    /// Px, percentages of the positioning area, or `Auto`.
    Lengths(Length, Length),
}

/// A `url()` background layer, computed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackgroundImage {
    pub url: Box<str>,
    pub size: BackgroundSize,
    /// Px offsets or percentages, against the positioning area less the image.
    pub position: (Length, Length),
    /// Repeats across, and down.
    pub repeat: (bool, bool),
}

/// One computed background layer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackgroundLayer {
    Gradient(Gradient),
    Image(BackgroundImage),
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

pub(crate) fn compute_gradient_list(
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

/// The layers of a style's background, computed: gradients as they are, images with the
/// size, position and repeat set for them.
pub(crate) fn compute_backgrounds(
    specs: &[ImageSpec],
    current: Color,
    resolve: &impl Fn(SpecLength) -> Length,
    (size, position, repeat): (SizeSpec, (SpecLength, SpecLength), (bool, bool)),
) -> Vec<BackgroundLayer> {
    specs
        .iter()
        .map(|layer| match layer {
            ImageSpec::Gradient(g) => BackgroundLayer::Gradient(
                compute_gradient_list(std::slice::from_ref(g), current, resolve)
                    .pop()
                    .expect("one gradient in, one out"),
            ),
            ImageSpec::Url(url) => BackgroundLayer::Image(BackgroundImage {
                url: url.as_str().into(),
                size: match size {
                    SizeSpec::Cover => BackgroundSize::Cover,
                    SizeSpec::Contain => BackgroundSize::Contain,
                    SizeSpec::Lengths(w, h) => BackgroundSize::Lengths(resolve(w), resolve(h)),
                },
                position: (resolve(position.0), resolve(position.1)),
                repeat,
            }),
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
        let layers =
            parse_images("url(Img/A.PNG) no-repeat, linear-gradient(red, blue) #fff").unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(
            layers[0],
            ImageSpec::Url("Img/A.PNG".into()),
            "URLs keep their case"
        );
        assert!(matches!(layers[1], ImageSpec::Gradient(_)));
        assert_eq!(
            parse_images("url('x y.svg')").unwrap(),
            [ImageSpec::Url("x y.svg".into())]
        );
        assert!(parse_images("#fff").is_none(), "only a color: no image");
        assert_eq!(parse_images("none").unwrap(), []);
    }

    #[test]
    fn parses_background_size_position_and_repeat() {
        assert_eq!(parse_size("cover"), Some(SizeSpec::Cover));
        assert_eq!(
            parse_size("50% auto"),
            Some(SizeSpec::Lengths(
                SpecLength::Percent(50.0),
                SpecLength::Auto
            ))
        );
        assert_eq!(parse_repeat("repeat-x"), Some((true, false)));
        assert_eq!(parse_repeat("no-repeat repeat"), Some((false, true)));
        assert_eq!(parse_repeat("sideways"), None);
        let (position, size, repeat) =
            parse_background_rest("url(a.png) center / cover no-repeat #fff");
        assert_eq!(
            position,
            (SpecLength::Percent(50.0), SpecLength::Percent(50.0))
        );
        assert_eq!(size, SizeSpec::Cover);
        assert_eq!(repeat, (false, false));
        let (position, size, repeat) = parse_background_rest("#fff");
        assert_eq!(
            position,
            (SpecLength::Percent(0.0), SpecLength::Percent(0.0))
        );
        assert_eq!(size, SizeSpec::Lengths(SpecLength::Auto, SpecLength::Auto));
        assert_eq!(repeat, (true, true));
        let (position, _, _) = parse_background_rest("url(a.png) right 10px top 20%/40px");
        assert_eq!(position.0, SpecLength::Percent(100.0));
    }
}
