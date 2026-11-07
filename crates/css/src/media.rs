//! Media queries: `@media screen and (max-width: 640px) { ... }`.

/// What the page is being displayed on. Media queries are evaluated against this at cascade
/// time, so a window resize only needs a new cascade, not a new parse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Media {
    pub width: f32,
    pub height: f32,
    pub dark: bool,
    pub reduced_motion: bool,
}

impl Default for Media {
    fn default() -> Self {
        Media {
            width: 1280.0,
            height: 800.0,
            dark: false,
            reduced_motion: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Feature {
    MinWidth(f32),
    MaxWidth(f32),
    MinHeight(f32),
    MaxHeight(f32),
    Dark(bool),
    Landscape(bool),
    ReducedMotion(bool),
    /// Something Broxee cannot evaluate (hover, resolution, an unknown feature): never matches.
    Never,
}

impl Feature {
    fn matches(self, m: &Media) -> bool {
        match self {
            Feature::MinWidth(v) => m.width >= v,
            Feature::MaxWidth(v) => m.width <= v,
            Feature::MinHeight(v) => m.height >= v,
            Feature::MaxHeight(v) => m.height <= v,
            Feature::Dark(d) => m.dark == d,
            Feature::Landscape(l) => (m.width >= m.height) == l,
            Feature::ReducedMotion(r) => m.reduced_motion == r,
            Feature::Never => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MediaQuery {
    negated: bool,
    /// False for `print`, `speech` and unknown media types.
    screen: bool,
    features: Vec<Feature>,
}

/// A comma-separated list of queries; it matches when any query matches.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaList(Vec<MediaQuery>);

impl MediaList {
    pub fn parse(src: &str) -> MediaList {
        let src = src.trim().to_ascii_lowercase();
        if src.is_empty() {
            return MediaList(vec![MediaQuery {
                negated: false,
                screen: true,
                features: Vec::new(),
            }]);
        }
        MediaList(src.split(',').map(parse_query).collect())
    }

    pub fn matches(&self, media: &Media) -> bool {
        self.0.iter().any(|q| {
            let hit = q.screen && q.features.iter().all(|f| f.matches(media));
            hit != q.negated
        })
    }
}

fn parse_query(q: &str) -> MediaQuery {
    let mut q = q.trim();
    let mut negated = false;
    if let Some(rest) = q.strip_prefix("not ") {
        negated = true;
        q = rest.trim();
    } else if let Some(rest) = q.strip_prefix("only ") {
        q = rest.trim();
    }
    let mut screen = true;
    let mut features = Vec::new();
    for (i, part) in split_and(q).into_iter().enumerate() {
        let part = part.trim();
        if let Some(inner) = part.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            features.push(parse_feature(inner));
        } else if i == 0 {
            screen = matches!(part, "screen" | "all");
        } else {
            features.push(Feature::Never);
        }
    }
    MediaQuery {
        negated,
        screen,
        features,
    }
}

/// Split on ` and ` outside parentheses.
fn split_and(q: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let b = q.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && q[i..].starts_with(" and ") => {
                parts.push(&q[start..i]);
                i += 5;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&q[start..]);
    parts
}

fn parse_feature(f: &str) -> Feature {
    if let Some((name, value)) = f.split_once(':') {
        let (name, value) = (name.trim(), value.trim());
        return match name {
            "min-width" => length(value).map_or(Feature::Never, Feature::MinWidth),
            "max-width" => length(value).map_or(Feature::Never, Feature::MaxWidth),
            "min-height" => length(value).map_or(Feature::Never, Feature::MinHeight),
            "max-height" => length(value).map_or(Feature::Never, Feature::MaxHeight),
            "prefers-color-scheme" => Feature::Dark(value == "dark"),
            "orientation" => Feature::Landscape(value == "landscape"),
            "prefers-reduced-motion" => Feature::ReducedMotion(value == "reduce"),
            _ => Feature::Never,
        };
    }
    // Range syntax: `width >= 600px`, `height < 40em`.
    for (op, min, strict) in [
        (">=", true, false),
        ("<=", false, false),
        (">", true, true),
        ("<", false, true),
    ] {
        if let Some((name, value)) = f.split_once(op) {
            let Some(v) = length(value.trim()) else {
                return Feature::Never;
            };
            // A strict bound becomes inclusive one hundredth of a pixel away.
            let v = if strict {
                if min {
                    v + 0.01
                } else {
                    v - 0.01
                }
            } else {
                v
            };
            return match (name.trim(), min) {
                ("width", true) => Feature::MinWidth(v),
                ("width", false) => Feature::MaxWidth(v),
                ("height", true) => Feature::MinHeight(v),
                ("height", false) => Feature::MaxHeight(v),
                _ => Feature::Never,
            };
        }
    }
    Feature::Never
}

/// A length in px. Media queries resolve `em` and `rem` against the initial 16px, and accept
/// simple `calc(a - b)` / `calc(a + b)`.
fn length(v: &str) -> Option<f32> {
    if let Some(inner) = v.strip_prefix("calc(").and_then(|c| c.strip_suffix(')')) {
        if let Some((a, b)) = inner.split_once(" - ") {
            return Some(length(a.trim())? - length(b.trim())?);
        }
        if let Some((a, b)) = inner.split_once(" + ") {
            return Some(length(a.trim())? + length(b.trim())?);
        }
        return length(inner.trim());
    }
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    match &v[split..] {
        "px" => Some(n),
        "em" | "rem" => Some(n * 16.0),
        "" if n == 0.0 => Some(0.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(width: f32) -> Media {
        Media {
            width,
            ..Media::default()
        }
    }

    #[test]
    fn evaluates_types_and_widths() {
        let q = MediaList::parse("screen and (max-width: calc(1120px - 1px))");
        assert!(q.matches(&at(1000.0)));
        assert!(!q.matches(&at(1120.0)));
        assert!(MediaList::parse("only screen and (min-width: 40em)").matches(&at(640.0)));
        assert!(!MediaList::parse("print").matches(&at(1000.0)));
        assert!(MediaList::parse("not print").matches(&at(1000.0)));
        assert!(MediaList::parse("print, screen").matches(&at(1000.0)));
        assert!(MediaList::parse("").matches(&at(1000.0)));
    }

    #[test]
    fn evaluates_preferences_and_range_syntax() {
        assert!(!MediaList::parse("(prefers-color-scheme: dark)").matches(&Media::default()));
        assert!(
            MediaList::parse("(prefers-color-scheme: dark)").matches(&Media {
                dark: true,
                ..Media::default()
            })
        );
        assert!(MediaList::parse("(width >= 600px)").matches(&at(600.0)));
        assert!(!MediaList::parse("(width < 600px)").matches(&at(600.0)));
        assert!(!MediaList::parse("(hover: hover)").matches(&at(600.0)));
    }
}
