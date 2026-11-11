use crate::grid::{expand_grid, is_grid_property, GridSlot, GridValue};
use crate::media::{Media, MediaList};
use crate::selector::{parse_selector_list, Selector};
use crate::values::*;
use crate::vocabulary::Vocabulary;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decl {
    pub prop: Prop,
    pub value: Value,
    pub important: bool,
}

/// One declaration as written in a rule or a `style` attribute.
#[derive(Clone, Debug, PartialEq)]
pub enum Declaration {
    /// Parsed once, when the stylesheet loads.
    Typed(Decl),
    /// `--name: value`: a custom property, inherited, used through `var()`.
    Custom {
        name: Box<str>,
        value: Box<str>,
        important: bool,
    },
    /// A grid property, parsed into a structure (see `grid.rs`).
    Grid {
        slot: GridSlot,
        value: Box<GridValue>,
        important: bool,
    },
    /// A value that uses `var()`. Substituted and parsed per element during the cascade.
    Pending {
        name: Box<str>,
        value: Box<str>,
        important: bool,
    },
}

impl Declaration {
    pub fn important(&self) -> bool {
        match self {
            Declaration::Typed(d) => d.important,
            Declaration::Custom { important, .. }
            | Declaration::Pending { important, .. }
            | Declaration::Grid { important, .. } => *important,
        }
    }
}

#[derive(Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<Declaration>,
    /// Index into `Stylesheet::conditions` when the rule sits inside `@media`.
    pub condition: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// Rules dropped because no selector could match the document (see `Vocabulary`).
    pub skipped: usize,
    /// Each condition is the media lists of every enclosing `@media`; all must match.
    pub conditions: Vec<Vec<MediaList>>,
}

impl Stylesheet {
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn condition_matches(&self, condition: Option<u32>, media: &Media) -> bool {
        condition.is_none_or(|c| {
            self.conditions[c as usize]
                .iter()
                .all(|list| list.matches(media))
        })
    }
}

/// Parse a stylesheet. Unknown properties and invalid values are skipped one declaration at
/// a time, as browsers do. `@media` rules are kept with their condition; `@supports` and
/// `@layer` blocks are read as if their condition held; other at-rules are skipped.
pub fn parse_stylesheet(src: &str) -> Stylesheet {
    parse_stylesheet_owned(src.to_owned())
}

/// Like `parse_stylesheet`, reusing the caller's buffer instead of copying a fetched sheet.
pub fn parse_stylesheet_owned(src: String) -> Stylesheet {
    parse_with(src, None)
}

/// Parse a stylesheet for one document, keeping only rules that could match it.
pub fn parse_stylesheet_for(src: String, vocabulary: &Vocabulary) -> Stylesheet {
    parse_with(src, Some(vocabulary))
}

fn parse_with(src: String, vocabulary: Option<&Vocabulary>) -> Stylesheet {
    let src = blank_comments(src);
    let mut sheet = Stylesheet::default();
    parse_rules(&src, None, &mut sheet, vocabulary);
    sheet.rules.shrink_to_fit();
    sheet
}

fn parse_rules(
    s: &str,
    condition: Option<u32>,
    sheet: &mut Stylesheet,
    vocabulary: Option<&Vocabulary>,
) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() || b[i] == b'}' || b[i] == b';' {
            i += 1;
            continue;
        }
        if b[i] == b'@' {
            match find_outside_strings(s, i, |c| c == b';' || c == b'{') {
                Some(open) if b[open] == b'{' => {
                    let close = block_end(s, open);
                    let prelude = &s[i + 1..open];
                    let name_end = prelude
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                        .unwrap_or(prelude.len());
                    let (name, rest) = prelude.split_at(name_end);
                    let body = &s[open + 1..close];
                    match name.to_ascii_lowercase().as_str() {
                        "media" => {
                            let mut lists = condition
                                .map(|c| sheet.conditions[c as usize].clone())
                                .unwrap_or_default();
                            lists.push(MediaList::parse(rest));
                            sheet.conditions.push(lists);
                            let nested = (sheet.conditions.len() - 1) as u32;
                            parse_rules(body, Some(nested), sheet, vocabulary);
                        }
                        "supports"
                            if !rest.trim_start().to_ascii_lowercase().starts_with("not") =>
                        {
                            parse_rules(body, condition, sheet, vocabulary);
                        }
                        "layer" => parse_rules(body, condition, sheet, vocabulary),
                        _ => {}
                    }
                    i = close + 1;
                }
                Some(semicolon) => i = semicolon + 1,
                None => break,
            }
            continue;
        }
        let Some(open) = find_outside_strings(s, i, |c| c == b'{') else {
            break;
        };
        let close = block_end(s, open);
        // Most dropped rules fail on a class or id the page lacks: find those from the text,
        // before building selectors for them.
        if vocabulary.is_some_and(|v| !v.text_could_match(&s[i..open])) {
            sheet.skipped += 1;
            i = close + 1;
            continue;
        }
        let mut selectors = parse_selector_list(&s[i..open]);
        if let Some(vocabulary) = vocabulary {
            let before = selectors.len();
            selectors.retain(|selector| vocabulary.could_match(selector));
            if before > 0 && selectors.is_empty() {
                // Its declarations are never parsed.
                sheet.skipped += 1;
            }
        }
        if !selectors.is_empty() {
            let mut decls = parse_declarations(&s[open + 1..close]);
            if !decls.is_empty() {
                // Rules live as long as the page; drop the growth slack `push` left behind.
                decls.shrink_to_fit();
                selectors.shrink_to_fit();
                for selector in &mut selectors {
                    selector.shrink_to_fit();
                }
                sheet.rules.push(Rule {
                    selectors,
                    decls,
                    condition,
                });
            }
        }
        i = close + 1;
    }
}

/// Parse the inside of a `{ ... }` block or a `style="..."` attribute.
pub fn parse_declarations(src: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for part in split_top_level(src, b';') {
        let Some((name, value)) = part.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let mut value = value.trim();
        let mut important = false;
        if let Some(pos) = value.rfind('!') {
            if value[pos + 1..].trim().eq_ignore_ascii_case("important") {
                important = true;
                value = value[..pos].trim();
            }
        }
        if name.starts_with("--") {
            // Custom property names are case-sensitive and their values are kept verbatim.
            out.push(Declaration::Custom {
                name: name.into(),
                value: value.into(),
                important,
            });
            continue;
        }
        let name = name.to_ascii_lowercase();
        if contains_var(value) {
            out.push(Declaration::Pending {
                name: name.into(),
                value: value.into(),
                important,
            });
            continue;
        }
        out.extend(expand_declaration(&name, value, important));
    }
    out
}

/// Expand one declaration whose value no longer contains `var()` into longhand declarations.
pub fn expand_declaration(name: &str, value: &str, important: bool) -> Vec<Declaration> {
    let mut sink = Sink {
        out: Vec::new(),
        important,
    };
    expand(name, &value.to_ascii_lowercase(), &mut sink);
    sink.out
}

fn contains_var(value: &str) -> bool {
    value
        .as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"var("))
}

struct Sink {
    out: Vec<Declaration>,
    important: bool,
}

impl Sink {
    fn set(&mut self, prop: Prop, value: Value) {
        self.out.push(Declaration::Typed(Decl {
            prop,
            value,
            important: self.important,
        }));
    }
}

fn expand(name: &str, v: &str, sink: &mut Sink) {
    let global = match v {
        "inherit" => Some(Value::Inherit),
        "initial" | "unset" | "revert" | "revert-layer" => Some(Value::Initial),
        _ => None,
    };
    if let Some(g) = global {
        for prop in longhands(name) {
            sink.set(prop, g);
        }
        return;
    }

    if is_grid_property(name) {
        let mut slots = Vec::new();
        if expand_grid(name, v, &mut slots) {
            for (slot, value) in slots {
                sink.out.push(Declaration::Grid {
                    slot,
                    value: Box::new(value),
                    important: sink.important,
                });
            }
        }
        return;
    }
    match name {
        "margin" => box4(v, MARGIN, sink),
        "inset" => box4(v, INSET, sink),
        "overflow" => {
            // `overflow: hidden auto` clips if either axis clips.
            let clips = v.split_whitespace().any(|t| matches!(t, "hidden" | "clip"));
            let visible = v.split_whitespace().all(|t| t == "visible");
            if v.split_whitespace()
                .all(|t| matches!(t, "visible" | "hidden" | "clip" | "auto" | "scroll"))
            {
                sink.set(Prop::Overflow, Value::Overflow { clips, visible });
            }
        }
        "padding" => box4(v, PADDING, sink),
        "border-width" => box4(v, BORDER_WIDTH, sink),
        "border-style" => box4(v, BORDER_STYLE, sink),
        "border-color" => box4(v, BORDER_COLOR, sink),
        "border" => border(v, &[0, 1, 2, 3], sink),
        "background" => background(v, sink),
        "font" => font(v, sink),
        "text-decoration-line" => expand("text-decoration", v, sink),
        "flex" => flex(v, sink),
        "border-spacing" => {
            let parts: Vec<SpecLength> = match v
                .split_whitespace()
                .map(|t| parse_length(t).filter(|l| non_negative(l) && *l != SpecLength::Auto))
                .collect::<Option<Vec<_>>>()
            {
                Some(p) if (1..=2).contains(&p.len()) => p,
                _ => return,
            };
            sink.set(Prop::BorderSpacingH, Value::Length(parts[0]));
            sink.set(
                Prop::BorderSpacingV,
                Value::Length(*parts.last().unwrap_or(&parts[0])),
            );
        }
        "flex-flow" => {
            for token in v.split_whitespace() {
                for prop in [Prop::FlexDirection, Prop::FlexWrap] {
                    if let Some(value) = parse_for(prop, token) {
                        sink.set(prop, value);
                    }
                }
            }
        }
        "gap" | "grid-gap" => {
            let tokens: Vec<&str> = v.split_whitespace().collect();
            let row = tokens.first().and_then(|t| parse_for(Prop::RowGap, t));
            let column = match tokens.get(1) {
                Some(t) => parse_for(Prop::ColumnGap, t),
                None => row,
            };
            if let (Some(row), Some(column), true) = (row, column, tokens.len() <= 2) {
                sink.set(Prop::RowGap, row);
                sink.set(Prop::ColumnGap, column);
            }
        }
        _ => {
            if let Some(side) = name.strip_prefix("border-").and_then(side_index) {
                border(v, &[side], sink);
            } else if let Some(prop) = single_prop(name) {
                if let Some(value) = parse_for(prop, v) {
                    sink.set(prop, value);
                }
            }
        }
    }
}

fn longhands(name: &str) -> Vec<Prop> {
    match name {
        "margin" => MARGIN.to_vec(),
        "inset" => INSET.to_vec(),
        "overflow" => vec![Prop::Overflow],
        "padding" => PADDING.to_vec(),
        "border-width" => BORDER_WIDTH.to_vec(),
        "border-style" => BORDER_STYLE.to_vec(),
        "border-color" => BORDER_COLOR.to_vec(),
        "border" => [BORDER_WIDTH, BORDER_STYLE, BORDER_COLOR].concat(),
        "background" => vec![Prop::BackgroundColor],
        "font" => vec![
            Prop::FontSize,
            Prop::FontWeight,
            Prop::FontStyle,
            Prop::LineHeight,
        ],
        "text-decoration-line" => vec![Prop::TextDecoration],
        "flex" => vec![Prop::FlexGrow, Prop::FlexShrink, Prop::FlexBasis],
        "border-spacing" => vec![Prop::BorderSpacingH, Prop::BorderSpacingV],
        "flex-flow" => vec![Prop::FlexDirection, Prop::FlexWrap],
        "gap" | "grid-gap" => vec![Prop::RowGap, Prop::ColumnGap],
        _ => match name.strip_prefix("border-").and_then(side_index) {
            Some(side) => vec![BORDER_WIDTH[side], BORDER_STYLE[side], BORDER_COLOR[side]],
            None => single_prop(name).into_iter().collect(),
        },
    }
}

fn single_prop(name: &str) -> Option<Prop> {
    Some(match name {
        "display" => Prop::Display,
        "color" => Prop::Color,
        "background-color" => Prop::BackgroundColor,
        "font-size" => Prop::FontSize,
        "font-weight" => Prop::FontWeight,
        "font-style" => Prop::FontStyle,
        "line-height" => Prop::LineHeight,
        "text-align" => Prop::TextAlign,
        "white-space" => Prop::WhiteSpace,
        "text-decoration" => Prop::TextDecoration,
        "width" => Prop::Width,
        "height" => Prop::Height,
        "min-width" => Prop::MinWidth,
        "max-width" => Prop::MaxWidth,
        "min-height" => Prop::MinHeight,
        "max-height" => Prop::MaxHeight,
        "position" => Prop::Position,
        "top" => Prop::Top,
        "right" => Prop::Right,
        "bottom" => Prop::Bottom,
        "left" => Prop::Left,
        "visibility" => Prop::Visibility,
        "opacity" => Prop::Opacity,
        "overflow-x" | "overflow-y" => Prop::Overflow,
        "box-sizing" => Prop::BoxSizing,
        "flex-direction" => Prop::FlexDirection,
        "flex-wrap" => Prop::FlexWrap,
        "justify-content" => Prop::JustifyContent,
        "align-items" => Prop::AlignItems,
        "align-self" => Prop::AlignSelf,
        "flex-grow" => Prop::FlexGrow,
        "flex-shrink" => Prop::FlexShrink,
        "flex-basis" => Prop::FlexBasis,
        "row-gap" | "grid-row-gap" => Prop::RowGap,
        "column-gap" | "grid-column-gap" => Prop::ColumnGap,
        "order" => Prop::Order,
        "vertical-align" => Prop::VerticalAlign,
        "border-collapse" => Prop::BorderCollapse,
        "caption-side" => Prop::CaptionSide,
        "float" => Prop::Float,
        "clear" => Prop::Clear,
        _ => {
            if let Some(side) = name.strip_prefix("margin-").and_then(side_index) {
                return Some(MARGIN[side]);
            }
            if let Some(side) = name.strip_prefix("padding-").and_then(side_index) {
                return Some(PADDING[side]);
            }
            let (side, part) = name.strip_prefix("border-")?.split_once('-')?;
            let side = side_index(side)?;
            return match part {
                "width" => Some(BORDER_WIDTH[side]),
                "style" => Some(BORDER_STYLE[side]),
                "color" => Some(BORDER_COLOR[side]),
                _ => None,
            };
        }
    })
}

fn side_index(side: &str) -> Option<usize> {
    ["top", "right", "bottom", "left"]
        .iter()
        .position(|s| *s == side)
}

/// Parse a value for one longhand property.
fn parse_for(prop: Prop, v: &str) -> Option<Value> {
    use Prop::*;
    match prop {
        Display => parse_display(v).map(Value::Display),
        Color | BackgroundColor => parse_color_value(v),
        FontSize => parse_font_size(v).map(Value::Length),
        FontWeight => parse_font_weight(v),
        FontStyle => parse_font_style(v),
        LineHeight => parse_line_height(v),
        TextAlign => parse_text_align(v).map(Value::TextAlign),
        WhiteSpace => parse_white_space(v).map(Value::WhiteSpace),
        TextDecoration => Some(Value::Underline(
            v.split_whitespace().any(|t| t == "underline"),
        )),
        Width | Height | MinWidth | MinHeight => {
            parse_length(v).filter(non_negative).map(Value::Length)
        }
        // `none` is stored as `auto`, which layout reads as "no maximum".
        MaxWidth | MaxHeight if v == "none" => Some(Value::Length(SpecLength::Auto)),
        MaxWidth | MaxHeight => parse_length(v)
            .filter(|l| non_negative(l) && *l != SpecLength::Auto)
            .map(Value::Length),
        p if MARGIN.contains(&p) || INSET.contains(&p) => parse_length(v).map(Value::Length),
        Position => match v {
            "static" => Some(Value::Position(crate::values::Position::Static)),
            // Sticky is laid out as relative until scroll-aware positioning exists.
            "relative" | "sticky" | "-webkit-sticky" => {
                Some(Value::Position(crate::values::Position::Relative))
            }
            "absolute" => Some(Value::Position(crate::values::Position::Absolute)),
            "fixed" => Some(Value::Position(crate::values::Position::Fixed)),
            _ => None,
        },
        Visibility => match v {
            "visible" => Some(Value::Visible(true)),
            "hidden" | "collapse" => Some(Value::Visible(false)),
            _ => None,
        },
        Opacity => match v.strip_suffix('%') {
            Some(pct) => pct
                .parse::<f32>()
                .ok()
                .map(|p| Value::Number((p / 100.0).clamp(0.0, 1.0))),
            None => v
                .parse::<f32>()
                .ok()
                .map(|n| Value::Number(n.clamp(0.0, 1.0))),
        },
        BoxSizing => match v {
            "border-box" => Some(Value::BorderBox(true)),
            "content-box" => Some(Value::BorderBox(false)),
            _ => None,
        },
        FlexDirection => Some(Value::FlexDirection(match v {
            "row" => crate::values::FlexDirection::Row,
            "row-reverse" => crate::values::FlexDirection::RowReverse,
            "column" => crate::values::FlexDirection::Column,
            "column-reverse" => crate::values::FlexDirection::ColumnReverse,
            _ => return None,
        })),
        FlexWrap => match v {
            "nowrap" => Some(Value::Wrap(false)),
            "wrap" | "wrap-reverse" => Some(Value::Wrap(true)),
            _ => None,
        },
        // `safe center` and `unsafe end` align as their last keyword.
        JustifyContent => Some(Value::Justify(match v.split_whitespace().last()? {
            "flex-start" | "start" | "left" | "normal" | "stretch" => Justify::Start,
            "flex-end" | "end" | "right" => Justify::End,
            "center" => Justify::Center,
            "space-between" => Justify::SpaceBetween,
            "space-around" => Justify::SpaceAround,
            "space-evenly" => Justify::SpaceEvenly,
            _ => return None,
        })),
        AlignItems | AlignSelf => Some(Value::Align(match v.split_whitespace().last()? {
            "auto" if prop == AlignSelf => Align::Auto,
            "flex-start" | "start" | "self-start" => Align::Start,
            "flex-end" | "end" | "self-end" => Align::End,
            "center" => Align::Center,
            "stretch" | "normal" => Align::Stretch,
            "baseline" => Align::Baseline,
            _ => return None,
        })),
        FlexGrow | FlexShrink => v
            .parse::<f32>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(Value::Number),
        FlexBasis => match v {
            "auto" | "content" => Some(Value::Length(SpecLength::Auto)),
            _ => parse_length(v)
                .filter(|l| non_negative(l) && *l != SpecLength::Auto)
                .map(Value::Length),
        },
        RowGap | ColumnGap => match v {
            "normal" => Some(Value::Length(SpecLength::Px(0.0))),
            _ => parse_length(v)
                .filter(|l| non_negative(l) && *l != SpecLength::Auto)
                .map(Value::Length),
        },
        Order => v.parse::<i32>().ok().map(|n| Value::Number(n as f32)),
        VerticalAlign => Some(Value::VAlign(match v {
            "top" => VAlign::Top,
            "middle" => VAlign::Middle,
            "bottom" => VAlign::Bottom,
            _ => VAlign::Baseline,
        })),
        Float => Some(Value::Float(match v {
            "left" | "inline-start" => crate::values::Float::Left,
            "right" | "inline-end" => crate::values::Float::Right,
            "none" => crate::values::Float::None,
            _ => return None,
        })),
        Clear => Some(Value::Clear(match v {
            "left" | "inline-start" => crate::values::Clear::Left,
            "right" | "inline-end" => crate::values::Clear::Right,
            "both" => crate::values::Clear::Both,
            "none" => crate::values::Clear::None,
            _ => return None,
        })),
        CaptionSide => match v {
            "bottom" => Some(Value::CaptionBottom(true)),
            "top" => Some(Value::CaptionBottom(false)),
            _ => None,
        },
        BorderCollapse => match v {
            "collapse" => Some(Value::Collapse(true)),
            "separate" => Some(Value::Collapse(false)),
            _ => None,
        },
        // `auto` and `scroll` stay unclipped until boxes can scroll their own content.
        Overflow => match v {
            "hidden" | "clip" => Some(Value::Overflow {
                clips: true,
                visible: false,
            }),
            "auto" | "scroll" => Some(Value::Overflow {
                clips: false,
                visible: false,
            }),
            "visible" => Some(Value::Overflow {
                clips: false,
                visible: true,
            }),
            _ => None,
        },
        p if PADDING.contains(&p) => parse_length(v)
            .filter(|l| non_negative(l) && *l != SpecLength::Auto)
            .map(Value::Length),
        p if BORDER_WIDTH.contains(&p) => parse_border_width(v).map(Value::Length),
        p if BORDER_STYLE.contains(&p) => parse_border_style(v).map(Value::BorderStyle),
        p if BORDER_COLOR.contains(&p) => parse_color_value(v),
        _ => None,
    }
}

/// `margin: 1px 2px` style shorthands: one to four values for top, right, bottom, left.
fn box4(v: &str, props: [Prop; 4], sink: &mut Sink) {
    let Some(vals) = split_values(v)
        .iter()
        .map(|p| parse_for(props[0], p))
        .collect::<Option<Vec<Value>>>()
    else {
        return;
    };
    let sides = match vals[..] {
        [a] => [a, a, a, a],
        [a, b] => [a, b, a, b],
        [a, b, c] => [a, b, c, b],
        [a, b, c, d] => [a, b, c, d],
        _ => return,
    };
    for (prop, value) in props.into_iter().zip(sides) {
        sink.set(prop, value);
    }
}

fn border(v: &str, sides: &[usize], sink: &mut Sink) {
    let mut width = Value::Length(SpecLength::Px(3.0));
    let mut style = Value::BorderStyle(false);
    let mut color = Value::CurrentColor;
    for token in split_values(v) {
        if let Some(s) = parse_border_style(token) {
            style = Value::BorderStyle(s);
        } else if let Some(w) = parse_border_width(token) {
            width = Value::Length(w);
        } else if let Some(c) = parse_color_value(token) {
            color = c;
        } else {
            return;
        }
    }
    for &side in sides {
        sink.set(BORDER_WIDTH[side], width);
        sink.set(BORDER_STYLE[side], style);
        sink.set(BORDER_COLOR[side], color);
    }
}

/// Only the color part of `background` is used; images and positions come later.
fn background(v: &str, sink: &mut Sink) {
    let color = split_values(v)
        .into_iter()
        .find_map(parse_color_value)
        .unwrap_or(Value::Color(Color::TRANSPARENT));
    sink.set(Prop::BackgroundColor, color);
}

/// `font: italic bold 16px/1.5 system-ui, sans-serif`. The family is not used yet.
fn font(v: &str, sink: &mut Sink) {
    let tokens = split_values(v);
    let mut weight = Value::FontWeight(400);
    let mut style = Value::FontStyle(crate::values::FontStyle::Normal);
    for (i, token) in tokens.iter().enumerate() {
        let (size_part, inline_lh) = match token.split_once('/') {
            Some((size, lh)) => (size, Some(lh)),
            None => (*token, None),
        };
        if let Some(size) = parse_font_size(size_part) {
            let lh_text = match inline_lh {
                Some(lh) if !lh.is_empty() => Some(lh),
                Some(_) => tokens.get(i + 1).copied(),
                None => match tokens.get(i + 1) {
                    Some(&"/") => tokens.get(i + 2).copied(),
                    Some(t) => t.strip_prefix('/').filter(|t| !t.is_empty()),
                    None => None,
                },
            };
            let line = match lh_text {
                Some(t) => match parse_line_height(t) {
                    Some(l) => l,
                    None => return,
                },
                None => Value::Normal,
            };
            sink.set(Prop::FontSize, Value::Length(size));
            sink.set(Prop::LineHeight, line);
            sink.set(Prop::FontWeight, weight);
            sink.set(Prop::FontStyle, style);
            return;
        }
        if let Some(w) = parse_font_weight(token) {
            weight = w;
        } else if let Some(s) = parse_font_style(token) {
            style = s;
        } else if !matches!(*token, "small-caps" | "normal") {
            return;
        }
    }
}

pub(crate) fn parse_length(v: &str) -> Option<SpecLength> {
    if v == "auto" {
        return Some(SpecLength::Auto);
    }
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: f32 = num.parse().ok().filter(|n: &f32| n.is_finite())?;
    Some(match unit {
        "px" => SpecLength::Px(n),
        "" if n == 0.0 => SpecLength::Px(0.0),
        "em" => SpecLength::Em(n),
        "rem" => SpecLength::Rem(n),
        "%" => SpecLength::Percent(n),
        "pt" => SpecLength::Px(n * 4.0 / 3.0),
        "pc" => SpecLength::Px(n * 16.0),
        "in" => SpecLength::Px(n * 96.0),
        "cm" => SpecLength::Px(n * 96.0 / 2.54),
        "mm" => SpecLength::Px(n * 96.0 / 25.4),
        "ex" | "ch" => SpecLength::Em(n * 0.5),
        "vw" => SpecLength::Vw(n),
        "vh" | "svh" | "lvh" | "dvh" => SpecLength::Vh(n),
        "vmin" => SpecLength::Vmin(n),
        "vmax" => SpecLength::Vmax(n),
        _ => return None,
    })
}

fn non_negative(l: &SpecLength) -> bool {
    match *l {
        SpecLength::Px(n)
        | SpecLength::Em(n)
        | SpecLength::Rem(n)
        | SpecLength::Vw(n)
        | SpecLength::Vh(n)
        | SpecLength::Vmin(n)
        | SpecLength::Vmax(n)
        | SpecLength::Percent(n) => n >= 0.0,
        SpecLength::Auto => true,
    }
}

fn parse_font_size(v: &str) -> Option<SpecLength> {
    Some(match v {
        "xx-small" => SpecLength::Px(9.0),
        "x-small" => SpecLength::Px(10.0),
        "small" => SpecLength::Px(13.0),
        "medium" => SpecLength::Px(16.0),
        "large" => SpecLength::Px(18.0),
        "x-large" => SpecLength::Px(24.0),
        "xx-large" => SpecLength::Px(32.0),
        "smaller" => SpecLength::Em(0.833),
        "larger" => SpecLength::Em(1.2),
        _ => parse_length(v).filter(|l| non_negative(l) && *l != SpecLength::Auto)?,
    })
}

fn parse_font_weight(v: &str) -> Option<Value> {
    Some(match v {
        "normal" => Value::FontWeight(400),
        "bold" => Value::FontWeight(700),
        "bolder" => Value::Bolder,
        "lighter" => Value::Lighter,
        _ => Value::FontWeight(v.parse::<u16>().ok().filter(|w| (1..=1000).contains(w))?),
    })
}

fn parse_font_style(v: &str) -> Option<Value> {
    match v {
        "normal" => Some(Value::FontStyle(FontStyle::Normal)),
        "italic" | "oblique" => Some(Value::FontStyle(FontStyle::Italic)),
        _ => None,
    }
}

fn parse_line_height(v: &str) -> Option<Value> {
    if v == "normal" {
        return Some(Value::Normal);
    }
    if let Ok(n) = v.parse::<f32>() {
        return (n.is_finite() && n >= 0.0).then_some(Value::Number(n));
    }
    parse_length(v)
        .filter(|l| non_negative(l) && *l != SpecLength::Auto)
        .map(Value::Length)
}

/// `flex: none | auto | <grow> [<shrink>]? || <basis>` (CSS Flexbox 7.1). A bare number sets
/// the basis to 0%, which is what makes `flex: 1` share space equally.
fn flex(v: &str, sink: &mut Sink) {
    let (grow, shrink, basis) = match v {
        "none" => (0.0, 0.0, SpecLength::Auto),
        "auto" => (1.0, 1.0, SpecLength::Auto),
        _ => {
            let mut numbers = Vec::new();
            let mut basis = None;
            for token in v.split_whitespace() {
                match token.parse::<f32>() {
                    Ok(n) if n.is_finite() && n >= 0.0 && numbers.len() < 2 => numbers.push(n),
                    _ if basis.is_none() => match parse_for(Prop::FlexBasis, token) {
                        Some(Value::Length(l)) => basis = Some(l),
                        _ => return,
                    },
                    _ => return,
                }
            }
            if numbers.is_empty() && basis.is_none() {
                return;
            }
            let grow = numbers.first().copied().unwrap_or(1.0);
            let shrink = numbers.get(1).copied().unwrap_or(1.0);
            (grow, shrink, basis.unwrap_or(SpecLength::Percent(0.0)))
        }
    };
    sink.set(Prop::FlexGrow, Value::Number(grow));
    sink.set(Prop::FlexShrink, Value::Number(shrink));
    sink.set(Prop::FlexBasis, Value::Length(basis));
}

fn parse_display(v: &str) -> Option<Display> {
    Some(match v {
        "block" => Display::Block,
        "flow-root" => Display::FlowRoot,
        "table" => Display::Table,
        "inline-table" => Display::InlineTable,
        "table-row-group" => Display::TableRowGroup,
        "table-header-group" => Display::TableHeaderGroup,
        "table-footer-group" => Display::TableFooterGroup,
        "table-row" => Display::TableRow,
        "table-cell" => Display::TableCell,
        "table-caption" => Display::TableCaption,
        "table-column" | "table-column-group" => Display::TableColumn,
        "inline" | "contents" => Display::Inline,
        "inline-block" => Display::InlineBlock,
        "grid" => Display::Grid,
        "inline-grid" => Display::InlineGrid,
        "list-item" => Display::ListItem,
        "flex" | "-webkit-box" | "-webkit-flex" => Display::Flex,
        "inline-flex" | "-webkit-inline-box" | "-webkit-inline-flex" => Display::InlineFlex,
        "none" => Display::None,
        _ => return None,
    })
}

fn parse_text_align(v: &str) -> Option<TextAlign> {
    Some(match v {
        "left" | "start" => TextAlign::Left,
        "right" | "end" => TextAlign::Right,
        "center" => TextAlign::Center,
        "-webkit-center" | "-moz-center" => TextAlign::WebkitCenter,
        "justify" => TextAlign::Justify,
        _ => return None,
    })
}

fn parse_white_space(v: &str) -> Option<WhiteSpace> {
    Some(match v {
        "normal" | "pre-line" => WhiteSpace::Normal,
        "pre" => WhiteSpace::Pre,
        "nowrap" => WhiteSpace::NoWrap,
        "pre-wrap" | "break-spaces" => WhiteSpace::PreWrap,
        _ => return None,
    })
}

fn parse_border_width(v: &str) -> Option<SpecLength> {
    Some(match v {
        "thin" => SpecLength::Px(1.0),
        "medium" => SpecLength::Px(3.0),
        "thick" => SpecLength::Px(5.0),
        _ => parse_length(v).filter(|l| {
            non_negative(l) && !matches!(l, SpecLength::Auto | SpecLength::Percent(_))
        })?,
    })
}

fn parse_border_style(v: &str) -> Option<bool> {
    match v {
        "none" | "hidden" => Some(false),
        "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => {
            Some(true)
        }
        _ => None,
    }
}

fn parse_color_value(v: &str) -> Option<Value> {
    if v == "currentcolor" {
        return Some(Value::CurrentColor);
    }
    parse_color(v).map(Value::Color)
}

/// Split on whitespace that is not inside parentheses, so `rgb(0, 0, 0)` stays one token.
fn split_values(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    for (i, b) in v.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        if b.is_ascii_whitespace() && depth == 0 {
            if let Some(s) = start.take() {
                out.push(&v[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&v[s..]);
    }
    out
}

/// Split on `sep` outside parentheses, brackets and quotes.
fn split_top_level(s: &str, sep: u8) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if b == sep && depth <= 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// Overwrite every `/* ... */` comment with spaces, in place. All the bytes become ASCII, so
/// the text stays valid UTF-8, and no copy of the stylesheet is made.
fn blank_comments(s: String) -> String {
    if !s.contains("/*") {
        return s;
    }
    let mut bytes = s.into_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let end = bytes[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |j| i + 2 + j + 2);
            bytes[i..end].fill(b' ');
            i = end;
        } else {
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn find_outside_strings(s: &str, from: usize, want: impl Fn(u8) -> bool) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if want(b) => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

/// Index of the `}` matching the `{` at `open`, or the end of the input if unterminated.
fn block_end(s: &str, open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    loop {
        let Some(j) = find_outside_strings(s, i, |c| c == b'{' || c == b'}') else {
            return s.len();
        };
        if s.as_bytes()[j] == b'{' {
            depth += 1;
        } else {
            depth -= 1;
            if depth == 0 {
                return j;
            }
        }
        i = j + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Vec<Decl> {
        parse_declarations(s)
            .into_iter()
            .filter_map(|d| match d {
                Declaration::Typed(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    fn decls(s: &str) -> Vec<(Prop, Value)> {
        typed(s).into_iter().map(|d| (d.prop, d.value)).collect()
    }

    #[test]
    fn keeps_custom_properties_and_defers_var() {
        let d = parse_declarations(
            "--Brand-Color: #0369A1; color: var(--Brand-Color, red) !important; margin: 0",
        );
        assert_eq!(
            d[0],
            Declaration::Custom {
                name: "--Brand-Color".into(),
                value: "#0369A1".into(),
                important: false
            }
        );
        assert_eq!(
            d[1],
            Declaration::Pending {
                name: "color".into(),
                value: "var(--Brand-Color, red)".into(),
                important: true
            }
        );
        assert_eq!(d.len(), 2 + 4, "margin still expands to four longhands");
    }

    #[test]
    fn expands_box_shorthands() {
        let d = decls("margin: 1px 2em; padding: 4px");
        assert_eq!(d.len(), 8);
        assert_eq!(d[0], (Prop::MarginTop, Value::Length(SpecLength::Px(1.0))));
        assert_eq!(
            d[1],
            (Prop::MarginRight, Value::Length(SpecLength::Em(2.0)))
        );
        assert_eq!(d[3], (Prop::MarginLeft, Value::Length(SpecLength::Em(2.0))));
        assert_eq!(
            d[7],
            (Prop::PaddingLeft, Value::Length(SpecLength::Px(4.0)))
        );
    }

    #[test]
    fn expands_border_and_font() {
        let d = decls("border-bottom: 2px solid #f00");
        assert!(d.contains(&(Prop::BorderBottomWidth, Value::Length(SpecLength::Px(2.0)))));
        assert!(d.contains(&(Prop::BorderBottomStyle, Value::BorderStyle(true))));
        assert!(d.contains(&(Prop::BorderBottomColor, Value::Color(Color::rgb(255, 0, 0)))));

        let f = decls("font: italic bold 16px/1.6 system-ui, sans-serif");
        assert!(f.contains(&(Prop::FontSize, Value::Length(SpecLength::Px(16.0)))));
        assert!(f.contains(&(Prop::LineHeight, Value::Number(1.6))));
        assert!(f.contains(&(Prop::FontWeight, Value::FontWeight(700))));
        assert!(f.contains(&(Prop::FontStyle, Value::FontStyle(FontStyle::Italic))));
    }

    #[test]
    fn reads_important_and_skips_invalid() {
        let d =
            typed("color: red !important; width: -5px; nonsense: 1; padding: auto; color: blue");
        assert_eq!(d.len(), 2);
        assert!(d[0].important);
        assert!(!d[1].important);
    }

    #[test]
    fn handles_comments_and_at_rules() {
        let sheet = parse_stylesheet(
            "/* c */ @import url(x.css); @media (max-width: 1px) { p { color: red } } \
             h1 { color: blue } @font-face { font-family: x } @keyframes k { to { color: red } } \
             @supports (display: grid) { div { color: red } } @supports not (display: grid) { b { color: red } } \
             p{margin:0}",
        );
        let tags: Vec<_> = sheet
            .rules
            .iter()
            .map(|r| r.selectors[0].parts[0].0.tag.as_deref())
            .collect();
        assert_eq!(tags, [Some("p"), Some("h1"), Some("div"), Some("p")]);
        let media = Media::default();
        assert!(
            !sheet.condition_matches(sheet.rules[0].condition, &media),
            "max-width: 1px is off"
        );
        assert!(sheet.condition_matches(sheet.rules[1].condition, &media));
    }

    #[test]
    fn nests_media_conditions() {
        let sheet = parse_stylesheet(
            "@media screen { a { color: red } @media (min-width: 2000px) { b { color: red } } }",
        );
        let media = Media::default();
        assert!(sheet.condition_matches(sheet.rules[0].condition, &media));
        assert!(
            !sheet.condition_matches(sheet.rules[1].condition, &media),
            "inner condition must also hold"
        );
    }

    #[test]
    fn keeps_semicolons_inside_strings_and_urls() {
        let sheet = parse_stylesheet(r#"a { background: url("data:x;y") #fff; color: red }"#);
        let values: Vec<Value> = sheet.rules[0]
            .decls
            .iter()
            .filter_map(|d| match d {
                Declaration::Typed(d) => Some(d.value),
                _ => None,
            })
            .collect();
        assert_eq!(
            values,
            [
                Value::Color(Color::rgb(255, 255, 255)),
                Value::Color(Color::rgb(255, 0, 0))
            ]
        );
    }

    #[test]
    fn blanks_comments_in_place() {
        assert_eq!(
            blank_comments("a/* é ✓ */b".into()),
            format!("a{}b", " ".repeat(12))
        );
        assert_eq!(
            blank_comments("p { color: red } /* open".into()),
            format!("p {{ color: red }}{}", " ".repeat(8))
        );
        let sheet = parse_stylesheet(
            "/* x */ p /* y */ { color: /* z */ red } /*# sourceMappingURL=a.map */",
        );
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn survives_malformed_css() {
        for css in [
            "a {",
            "a { color: ",
            "}}}",
            "@media {",
            "a { b { c } }",
            "{color:red}",
            "\"unterminated",
        ] {
            let _ = parse_stylesheet(css);
        }
    }
}
