//! `font-family` lists and `@font-face` rules.

use crate::parser::{css_string, split_top_level};

/// A `font-family` value: family names, lowercase (they compare without case).
pub type FamilyList = Box<[Box<str>]>;

/// Parse `"Open Sans", Arial, sans-serif`. Unquoted names are identifiers separated by white
/// space, which collapses to one space. `None` when any name is invalid, as browsers drop the
/// whole declaration then.
pub fn parse_family_list(v: &str) -> Option<FamilyList> {
    let mut out = Vec::new();
    for part in split_top_level(v, b',') {
        out.push(family_name(part)?);
    }
    (!out.is_empty()).then(|| out.into())
}

fn family_name(part: &str) -> Option<Box<str>> {
    let part = part.trim();
    if let Some(q) = part.chars().next().filter(|&c| c == '"' || c == '\'') {
        let (name, rest) = css_string(&part[1..], q)?;
        return (rest.trim().is_empty()).then(|| name.to_lowercase().into());
    }
    let words: Vec<&str> = part.split_ascii_whitespace().collect();
    let valid = |w: &&str| {
        !w.is_empty()
            && !w.starts_with(|c: char| c.is_ascii_digit())
            && w.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
    };
    if words.is_empty() || !words.iter().all(valid) {
        return None;
    }
    Some(words.join(" ").to_lowercase().into())
}

/// One `src` entry: a URL as written (relative to the stylesheet) and its declared format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontSource {
    pub url: String,
    /// `format("woff2")` and the like, lowercase; `None` when not given.
    pub format: Option<String>,
}

/// An `@font-face` rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFace {
    /// Lowercase.
    pub family: String,
    /// `url()` sources Broxee can load, in the order given; `local()` sources are skipped.
    pub sources: Vec<FontSource>,
    /// Inclusive; `font-weight: 100 900` on a variable font is a range.
    pub weight: (u16, u16),
    pub italic: bool,
    /// Inclusive code point ranges; empty means every character.
    pub ranges: Vec<(u32, u32)>,
}

/// Formats Broxee reads. Faces offering only others (EOT, SVG fonts) are skipped.
fn supported(format: &str) -> bool {
    matches!(
        format,
        "woff2" | "woff" | "truetype" | "opentype" | "woff2-variations" | "truetype-variations"
    )
}

/// Parse the inside of `@font-face { ... }`. `None` without a family or a usable source.
pub fn parse_font_face(body: &str) -> Option<FontFace> {
    let mut face = FontFace {
        family: String::new(),
        sources: Vec::new(),
        weight: (400, 400),
        italic: false,
        ranges: Vec::new(),
    };
    for decl in split_top_level(body, b';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "font-family" => {
                face.family = family_name(value).map(String::from).unwrap_or_default();
            }
            "src" => face.sources = parse_sources(value),
            "font-weight" => {
                if let Some(w) = parse_weight_range(&value.to_ascii_lowercase()) {
                    face.weight = w;
                }
            }
            "font-style" => {
                let v = value.to_ascii_lowercase();
                face.italic = v.starts_with("italic") || v.starts_with("oblique");
            }
            "unicode-range" => face.ranges = parse_unicode_ranges(value),
            _ => {}
        }
    }
    (!face.family.is_empty() && !face.sources.is_empty()).then_some(face)
}

fn parse_sources(v: &str) -> Vec<FontSource> {
    let mut out = Vec::new();
    for part in split_top_level(v, b',') {
        let part = part.trim();
        let Some(url) = url_function(part) else {
            continue;
        };
        let lower = part.to_ascii_lowercase();
        let format = lower.find("format(").and_then(|at| {
            let inner = &lower[at + 7..];
            let end = inner.find(')')?;
            let name = inner[..end].trim().trim_matches(|c| c == '"' || c == '\'');
            Some(name.to_string())
        });
        if format.as_deref().is_some_and(|f| !supported(f)) {
            continue;
        }
        out.push(FontSource { url, format });
    }
    out
}

/// The URL in `url(x)`, `url("x")` or `url('x')` at the start of `s`, case kept.
fn url_function(s: &str) -> Option<String> {
    if !s.get(..4)?.eq_ignore_ascii_case("url(") {
        return None;
    }
    let inner = s[4..].trim_start();
    if let Some(q) = inner.chars().next().filter(|&c| c == '"' || c == '\'') {
        let (url, _) = css_string(&inner[1..], q)?;
        return (!url.is_empty()).then_some(url);
    }
    let end = inner.find(')')?;
    let url = inner[..end].trim();
    (!url.is_empty()).then(|| url.to_string())
}

fn parse_weight_range(v: &str) -> Option<(u16, u16)> {
    let weight = |w: &str| match w {
        "normal" => Some(400),
        "bold" => Some(700),
        n => n
            .parse::<f32>()
            .ok()
            .filter(|n| (1.0..=1000.0).contains(n))
            .map(|n| n.round() as u16),
    };
    let mut parts = v.split_ascii_whitespace();
    let lo = weight(parts.next()?)?;
    let hi = match parts.next() {
        Some(p) => weight(p)?,
        None => lo,
    };
    Some((lo.min(hi), lo.max(hi)))
}

/// `U+0000-00FF, U+0131, U+4??` into inclusive ranges. Invalid entries are skipped.
fn parse_unicode_ranges(v: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for part in v.split(',') {
        let part = part.trim();
        let Some(hex) = part.strip_prefix("U+").or_else(|| part.strip_prefix("u+")) else {
            continue;
        };
        let range = if let Some((a, b)) = hex.split_once('-') {
            u32::from_str_radix(a, 16)
                .ok()
                .zip(u32::from_str_radix(b, 16).ok())
        } else if hex.contains('?') {
            let lo = u32::from_str_radix(&hex.replace('?', "0"), 16).ok();
            let hi = u32::from_str_radix(&hex.replace('?', "F"), 16).ok();
            lo.zip(hi)
        } else {
            u32::from_str_radix(hex, 16).ok().map(|c| (c, c))
        };
        if let Some((lo, hi)) = range.filter(|&(lo, hi)| lo <= hi && hi <= 0x10FFFF) {
            out.push((lo, hi));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_family_lists() {
        let list =
            |v| parse_family_list(v).map(|l| l.iter().map(|n| n.to_string()).collect::<Vec<_>>());
        assert_eq!(
            list(r#""Open Sans", Helvetica  Neue, sans-serif"#).unwrap(),
            ["open sans", "helvetica neue", "sans-serif"]
        );
        assert_eq!(
            list("'Font Awesome 6 Free'").unwrap(),
            ["font awesome 6 free"]
        );
        assert_eq!(
            list("-apple-system, system-ui").unwrap(),
            ["-apple-system", "system-ui"]
        );
        assert_eq!(list("Arial, 12px"), None, "a number is not a name");
        assert_eq!(list(r#""a" b"#), None);
        assert_eq!(list(""), None);
    }

    #[test]
    fn parses_font_face_rules() {
        let face = parse_font_face(
            r#"font-family: "Inter Var"; font-style: normal; font-weight: 100 900;
               font-display: swap;
               src: local("Inter"), url(/fonts/Inter.eot) format("embedded-opentype"),
                    url("/fonts/Inter.woff2?v=3") format("woff2"), url(Inter.woff) format('woff');
               unicode-range: U+0000-00FF, U+0131, u+4??, U+zz, U+20-10"#,
        )
        .unwrap();
        assert_eq!(face.family, "inter var");
        assert_eq!(face.weight, (100, 900));
        assert!(!face.italic);
        assert_eq!(
            face.sources,
            [
                FontSource {
                    url: "/fonts/Inter.woff2?v=3".into(),
                    format: Some("woff2".into())
                },
                FontSource {
                    url: "Inter.woff".into(),
                    format: Some("woff".into())
                },
            ]
        );
        assert_eq!(face.ranges, [(0, 0xFF), (0x131, 0x131), (0x400, 0x4FF)]);
    }

    #[test]
    fn needs_a_family_and_a_source() {
        assert!(parse_font_face("src: url(a.woff2)").is_none());
        assert!(parse_font_face("font-family: x; src: local(Arial)").is_none());
        let bold =
            parse_font_face("font-family: x; src: url(a); font-weight: bold; font-style: italic")
                .unwrap();
        assert_eq!((bold.weight, bold.italic), ((700, 700), true));
    }
}
