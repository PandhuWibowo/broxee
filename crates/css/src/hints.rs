//! Presentational hints (HTML standard, "Rendering"): old attributes such as `bgcolor`,
//! `width="85%"`, `align` and `<font color>` that still style much of the web. They apply as
//! CSS between the user-agent and author stylesheets, so any author rule overrides them.

use html::{Dom, NodeId};

/// The hints of element `id` as CSS declarations, or `None` when it has none.
pub(crate) fn hints(dom: &Dom, id: NodeId) -> Option<String> {
    // Most elements have no attributes at all.
    dom.element(id)?.attrs().next()?;
    let tag = dom.tag_name(id)?;
    let attr = |name: &str| dom.attr(id, name).map(str::trim).filter(|v| !v.is_empty());
    let mut css = String::new();
    let mut add = |property: &str, value: Option<String>| {
        if let Some(value) = value {
            css.push_str(property);
            css.push(':');
            css.push_str(&value);
            css.push(';');
        }
    };
    match tag {
        "table" => {
            add("width", attr("width").and_then(length));
            add("height", attr("height").and_then(length));
            add("background-color", attr("bgcolor").and_then(color));
            if attr("align").is_some_and(|a| a.eq_ignore_ascii_case("center")) {
                add("margin-left", Some("auto".into()));
                add("margin-right", Some("auto".into()));
            }
            add("float", attr("align").and_then(float));
            add("border-spacing", attr("cellspacing").and_then(pixels));
            if let Some(border) = attr("border")
                .and_then(|b| b.parse::<u32>().ok())
                .filter(|&b| b > 0)
            {
                add("border", Some(format!("{border}px outset #808080")));
            }
        }
        "td" | "th" => {
            add("width", attr("width").and_then(length));
            add("height", attr("height").and_then(length));
            add("background-color", attr("bgcolor").and_then(color));
            add("text-align", attr("align").and_then(text_align));
            add("vertical-align", attr("valign").and_then(vertical_align));
            // `nowrap` is a boolean attribute: present, usually with no value.
            if dom.attr(id, "nowrap").is_some() {
                add("white-space", Some("nowrap".into()));
            }
            // `cellpadding` and `border` on the table reach its cells.
            if let Some(table) = enclosing_table(dom, id) {
                let table_attr = |name: &str| dom.attr(table, name).map(str::trim);
                add("padding", table_attr("cellpadding").and_then(pixels));
                if table_attr("border")
                    .and_then(|b| b.parse::<u32>().ok())
                    .is_some_and(|b| b > 0)
                {
                    add("border", Some("1px inset #808080".into()));
                }
            }
        }
        "tr" | "tbody" | "thead" | "tfoot" => {
            add("height", attr("height").and_then(length));
            add("background-color", attr("bgcolor").and_then(color));
            add("text-align", attr("align").and_then(text_align));
            add("vertical-align", attr("valign").and_then(vertical_align));
        }
        "body" => {
            add("background-color", attr("bgcolor").and_then(color));
            add("color", attr("text").and_then(color));
        }
        "font" => {
            add("color", attr("color").and_then(color));
            add("font-size", attr("size").and_then(font_size));
            add("font-family", attr("face").map(String::from));
        }
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "caption" => {
            add("text-align", attr("align").and_then(text_align));
        }
        "img" => {
            add("float", attr("align").and_then(float));
            add(
                "border",
                attr("border")
                    .and_then(pixels)
                    .map(|b| format!("{b} solid")),
            );
        }
        "hr" => add("width", attr("width").and_then(length)),
        "br" => add(
            "clear",
            attr("clear").and_then(|c| {
                let c = c.to_ascii_lowercase();
                match c.as_str() {
                    "left" | "right" | "both" => Some(c),
                    "all" => Some("both".into()),
                    _ => None,
                }
            }),
        ),
        _ => return None,
    }
    (!css.is_empty()).then_some(css)
}

/// `align=left` and `align=right` on images and tables float them.
fn float(v: &str) -> Option<String> {
    ["left", "right"]
        .into_iter()
        .find(|side| v.eq_ignore_ascii_case(side))
        .map(String::from)
}

/// The table a cell belongs to: through its row and, if present, its row group.
fn enclosing_table(dom: &Dom, cell: NodeId) -> Option<NodeId> {
    let mut at = dom.parent(cell)?;
    for _ in 0..3 {
        if dom.tag_name(at) == Some("table") {
            return Some(at);
        }
        at = dom.parent(at)?;
    }
    None
}

/// `85%`, `300` or `300px` as CSS.
fn length(v: &str) -> Option<String> {
    if let Some(pct) = v.strip_suffix('%') {
        let n: f32 = pct
            .trim()
            .parse()
            .ok()
            .filter(|n: &f32| n.is_finite() && *n >= 0.0)?;
        return Some(format!("{n}%"));
    }
    pixels(v)
}

fn pixels(v: &str) -> Option<String> {
    let n: f32 = v
        .trim_end_matches("px")
        .trim()
        .parse()
        .ok()
        .filter(|n: &f32| n.is_finite() && *n >= 0.0)?;
    Some(format!("{n}px"))
}

/// Legacy colors: names, `#rgb`, `#rrggbb`, and hex digits without the `#`.
fn color(v: &str) -> Option<String> {
    if v.starts_with('#') {
        return Some(v.to_string());
    }
    let hex = v.bytes().all(|b| b.is_ascii_hexdigit());
    if hex && matches!(v.len(), 3 | 6) {
        return Some(format!("#{v}"));
    }
    v.bytes()
        .all(|b| b.is_ascii_alphabetic())
        .then(|| v.to_string())
}

fn text_align(v: &str) -> Option<String> {
    let v = v.to_ascii_lowercase();
    matches!(v.as_str(), "left" | "right" | "center" | "justify").then_some(v)
}

fn vertical_align(v: &str) -> Option<String> {
    let v = v.to_ascii_lowercase();
    matches!(v.as_str(), "top" | "middle" | "bottom" | "baseline").then_some(v)
}

/// `<font size>`: 1 to 7, or relative to the default 3.
fn font_size(v: &str) -> Option<String> {
    let n: i32 = v.trim_start_matches('+').parse().ok()?;
    let size = if v.starts_with('+') || v.starts_with('-') {
        3 + n
    } else {
        n
    };
    let px = [10, 13, 16, 18, 24, 32, 48][size.clamp(1, 7) as usize - 1];
    Some(format!("{px}px"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(src: &str, tag: &str) -> Option<String> {
        let dom = html::parse(src);
        hints(&dom, dom.find_first(tag).unwrap())
    }

    #[test]
    fn maps_legacy_attributes() {
        assert_eq!(
            of("<table width=85% bgcolor=ff6600 align=center cellspacing=0>", "table").as_deref(),
            Some("width:85%;background-color:#ff6600;margin-left:auto;margin-right:auto;border-spacing:0px;")
        );
        assert_eq!(
            of("<table cellpadding=4 border=1><tr><td valign=top align=RIGHT nowrap>x", "td").as_deref(),
            Some("text-align:right;vertical-align:top;white-space:nowrap;padding:4px;border:1px inset #808080;")
        );
        assert_eq!(
            of("<font color=red size=+1>", "font").as_deref(),
            Some("color:red;font-size:18px;")
        );
        assert_eq!(
            of("<div align=middle>", "div"),
            None,
            "invalid values are ignored"
        );
        assert_eq!(of("<span bgcolor=red>", "span"), None);
    }
}
