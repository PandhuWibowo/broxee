//! CSS Grid values: track lists, template areas and line placement (CSS Grid Layout 1).
//!
//! Grid values are lists and names, which do not fit the integer-only `ComputedStyle`. The
//! parser turns them into these structures once per stylesheet; the cascade resolves their
//! lengths per element and interns the result in `GridTables`, so a style only carries two ids.

use std::collections::HashMap;

use html::FastBuildHasher;

use crate::values::{Length, SpecLength};

/// One end of a track's size range: `minmax(min, max)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Breadth<L> {
    Length(L),
    /// Flexible fraction, in hundredths (`1fr` is 100). Only valid as a maximum.
    Fr(u32),
    Auto,
    MinContent,
    MaxContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Track<L> {
    pub min: Breadth<L>,
    pub max: Breadth<L>,
}

/// `repeat(auto-fill | auto-fit, ...)`: as many repetitions as fit, inserted at `at`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AutoRepeat<L> {
    pub at: usize,
    pub tracks: Vec<Track<L>>,
    /// `auto-fit` (empty repetitions collapse; laid out like `auto-fill` for now).
    pub fit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackList<L> {
    pub tracks: Vec<Track<L>>,
    pub auto_repeat: Option<AutoRepeat<L>>,
}

impl<L> Default for TrackList<L> {
    fn default() -> TrackList<L> {
        TrackList {
            tracks: Vec::new(),
            auto_repeat: None,
        }
    }
}

/// A named area from `grid-template-areas`: zero-based line indices, end exclusive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Area {
    pub name: Box<str>,
    pub rows: (u16, u16),
    pub columns: (u16, u16),
}

/// One of `grid-row-start`, `grid-column-end`, ...
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum GridLine {
    #[default]
    Auto,
    /// A line number, 1-based; negative counts from the end.
    Line(i32),
    Span(u32),
    /// An area name (`grid-area: footer`): its start or end line.
    Name(Box<str>),
}

/// Grid properties that the cascade carries as structures rather than `Value`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridSlot {
    Columns,
    Rows,
    Areas,
    AutoColumns,
    AutoRows,
    AutoFlow,
    RowStart,
    ColumnStart,
    RowEnd,
    ColumnEnd,
}

pub const GRID_SLOTS: usize = GridSlot::ColumnEnd as usize + 1;

#[derive(Clone, Debug, PartialEq)]
pub enum GridValue {
    Tracks(TrackList<SpecLength>),
    Areas(Vec<Area>),
    /// `grid-auto-flow`: true for `column`.
    ColumnFlow(bool),
    Line(GridLine),
}

/// A grid container's resolved template.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GridTemplate {
    pub columns: TrackList<Length>,
    pub rows: TrackList<Length>,
    pub areas: Vec<Area>,
    pub auto_columns: Option<Track<Length>>,
    pub auto_rows: Option<Track<Length>>,
    pub column_flow: bool,
}

impl GridTemplate {
    pub fn area(&self, name: &str) -> Option<&Area> {
        self.areas.iter().find(|a| &*a.name == name)
    }
}

/// A grid item's placement: row start, column start, row end, column end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GridPlacement {
    pub row: [GridLine; 2],
    pub column: [GridLine; 2],
}

/// Interned templates and placements. Index 0 of each is the default (no grid, auto placement).
#[derive(Debug)]
pub struct GridTables {
    pub templates: Vec<GridTemplate>,
    pub placements: Vec<GridPlacement>,
    template_index: HashMap<GridTemplate, u32, FastBuildHasher>,
    placement_index: HashMap<GridPlacement, u32, FastBuildHasher>,
}

impl Default for GridTables {
    fn default() -> GridTables {
        GridTables {
            templates: vec![GridTemplate::default()],
            placements: vec![GridPlacement::default()],
            template_index: HashMap::default(),
            placement_index: HashMap::default(),
        }
    }
}

impl GridTables {
    pub fn template(&mut self, t: GridTemplate) -> u32 {
        if t == GridTemplate::default() {
            return 0;
        }
        intern(&mut self.templates, &mut self.template_index, t)
    }

    pub fn placement(&mut self, p: GridPlacement) -> u32 {
        if p == GridPlacement::default() {
            return 0;
        }
        intern(&mut self.placements, &mut self.placement_index, p)
    }
}

fn intern<T: Clone + Eq + std::hash::Hash>(
    list: &mut Vec<T>,
    index: &mut HashMap<T, u32, FastBuildHasher>,
    value: T,
) -> u32 {
    if let Some(&id) = index.get(&value) {
        return id;
    }
    let id = list.len() as u32;
    list.push(value.clone());
    index.insert(value, id);
    id
}

/// Resolve a specified track list's lengths.
pub fn resolve_tracks(
    list: &TrackList<SpecLength>,
    length: impl Fn(SpecLength) -> Length,
) -> TrackList<Length> {
    let map = |t: &Track<SpecLength>| Track {
        min: map_breadth(t.min, &length),
        max: map_breadth(t.max, &length),
    };
    TrackList {
        tracks: list.tracks.iter().map(map).collect(),
        auto_repeat: list.auto_repeat.as_ref().map(|r| AutoRepeat {
            at: r.at,
            tracks: r.tracks.iter().map(map).collect(),
            fit: r.fit,
        }),
    }
}

fn map_breadth(b: Breadth<SpecLength>, length: &impl Fn(SpecLength) -> Length) -> Breadth<Length> {
    match b {
        Breadth::Length(l) => Breadth::Length(length(l)),
        Breadth::Fr(f) => Breadth::Fr(f),
        Breadth::Auto => Breadth::Auto,
        Breadth::MinContent => Breadth::MinContent,
        Breadth::MaxContent => Breadth::MaxContent,
    }
}

/// Repeat counts above this are treated as invalid (a page cannot make us allocate millions).
const MAX_REPEAT: usize = 1000;

/// `grid-template-columns` / `-rows`: `none`, or tracks, `repeat()`, `minmax()`,
/// `fit-content()`. Line names in brackets are skipped.
pub fn parse_track_list(v: &str) -> Option<TrackList<SpecLength>> {
    if v == "none" {
        return Some(TrackList::default());
    }
    let mut list = TrackList::default();
    for token in tokens(v)? {
        if token.starts_with('[') {
            continue;
        }
        if let Some(inner) = function(token, "repeat") {
            let (count, rest) = inner.split_once(',')?;
            let inner_list = parse_track_list(rest.trim())?;
            if inner_list.auto_repeat.is_some() || inner_list.tracks.is_empty() {
                return None;
            }
            match count.trim() {
                "auto-fill" | "auto-fit" if list.auto_repeat.is_none() => {
                    list.auto_repeat = Some(AutoRepeat {
                        at: list.tracks.len(),
                        tracks: inner_list.tracks,
                        fit: count.trim() == "auto-fit",
                    });
                }
                n => {
                    let n: usize = n.parse().ok().filter(|&n| n >= 1)?;
                    if n * inner_list.tracks.len() + list.tracks.len() > MAX_REPEAT {
                        return None;
                    }
                    for _ in 0..n {
                        list.tracks.extend_from_slice(&inner_list.tracks);
                    }
                }
            }
        } else {
            list.tracks.push(parse_track(token)?);
        }
    }
    Some(list)
}

/// One track size: a breadth, `minmax(a, b)` or `fit-content(x)`.
pub fn parse_track(token: &str) -> Option<Track<SpecLength>> {
    if let Some(inner) = function(token, "minmax") {
        let (a, b) = inner.split_once(',')?;
        let min = parse_breadth(a.trim())?;
        if let Breadth::Fr(_) = min {
            return None;
        }
        return Some(Track {
            min,
            max: parse_breadth(b.trim())?,
        });
    }
    if let Some(inner) = function(token, "fit-content") {
        return Some(Track {
            min: Breadth::Auto,
            max: Breadth::Length(length(inner.trim())?),
        });
    }
    Some(match parse_breadth(token)? {
        // A bare `1fr` is `minmax(auto, 1fr)`.
        Breadth::Fr(f) => Track {
            min: Breadth::Auto,
            max: Breadth::Fr(f),
        },
        b => Track { min: b, max: b },
    })
}

fn parse_breadth(v: &str) -> Option<Breadth<SpecLength>> {
    Some(match v {
        "auto" => Breadth::Auto,
        "min-content" => Breadth::MinContent,
        "max-content" => Breadth::MaxContent,
        _ => match v.strip_suffix("fr") {
            Some(n) => {
                let n: f32 = n
                    .parse()
                    .ok()
                    .filter(|n: &f32| n.is_finite() && *n >= 0.0)?;
                Breadth::Fr((n * 100.0).round() as u32)
            }
            None => Breadth::Length(length(v)?),
        },
    })
}

fn length(v: &str) -> Option<SpecLength> {
    crate::parser::parse_length(v).filter(|l| *l != SpecLength::Auto && non_negative(*l))
}

fn non_negative(l: SpecLength) -> bool {
    match l {
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

/// `grid-template-areas`: `none`, or one quoted string per row with the same number of cells.
/// Every name must cover a rectangle; `.` cells are empty.
pub fn parse_areas(v: &str) -> Option<Vec<Area>> {
    if v == "none" {
        return Some(Vec::new());
    }
    let rows = quoted_strings(v)?;
    parse_area_rows(&rows)
}

fn parse_area_rows(rows: &[&str]) -> Option<Vec<Area>> {
    let grid: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.split_whitespace().collect())
        .collect();
    let width = grid.first()?.len();
    if width == 0
        || grid.iter().any(|r| r.len() != width)
        || grid.len() > MAX_REPEAT
        || width > MAX_REPEAT
    {
        return None;
    }
    let mut areas: Vec<Area> = Vec::new();
    for (r, row) in grid.iter().enumerate() {
        for (c, &cell) in row.iter().enumerate() {
            if cell.chars().all(|ch| ch == '.') {
                continue;
            }
            match areas.iter_mut().find(|a| &*a.name == cell) {
                Some(a) => {
                    a.rows.1 = a.rows.1.max(r as u16 + 1);
                    a.columns.0 = a.columns.0.min(c as u16);
                    a.columns.1 = a.columns.1.max(c as u16 + 1);
                }
                None => areas.push(Area {
                    name: cell.into(),
                    rows: (r as u16, r as u16 + 1),
                    columns: (c as u16, c as u16 + 1),
                }),
            }
        }
    }
    // Each area's bounding box must be filled with its own name only.
    for a in &areas {
        for row in &grid[a.rows.0 as usize..a.rows.1 as usize] {
            if row[a.columns.0 as usize..a.columns.1 as usize]
                .iter()
                .any(|&c| c != &*a.name)
            {
                return None;
            }
        }
    }
    Some(areas)
}

/// `grid-row-start` and friends: `auto`, an integer, `span n`, or a name.
pub fn parse_line(v: &str) -> Option<GridLine> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    Some(match parts.as_slice() {
        ["auto"] => GridLine::Auto,
        ["span"] => return None,
        ["span", x] | [x, "span"] => match x.parse::<i32>() {
            Ok(n) if n >= 1 => GridLine::Span(n.min(MAX_REPEAT as i32) as u32),
            Ok(_) => return None,
            // `span name`: spanning to a named line is laid out as a single track.
            Err(_) => GridLine::Span(1),
        },
        [x] => match x.parse::<i32>() {
            Ok(0) => return None,
            Ok(n) => GridLine::Line(n.clamp(-(MAX_REPEAT as i32), MAX_REPEAT as i32)),
            Err(_) if is_ident(x) => GridLine::Name((*x).into()),
            Err(_) => return None,
        },
        // `name 2` (the second line called name) is not supported.
        _ => return None,
    })
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b >= 0x80)
}

/// Expand a grid shorthand or longhand into slots. Returns false when the value is invalid.
pub fn expand_grid(name: &str, v: &str, out: &mut Vec<(GridSlot, GridValue)>) -> bool {
    let tracks = |v: &str| parse_track_list(v).map(GridValue::Tracks);
    let line = |v: &str| parse_line(v).map(GridValue::Line);
    let set = |out: &mut Vec<(GridSlot, GridValue)>, slot, value: Option<GridValue>| match value {
        Some(value) => {
            out.push((slot, value));
            true
        }
        None => false,
    };
    match name {
        "grid-template-columns" => set(out, GridSlot::Columns, tracks(v)),
        "grid-template-rows" => set(out, GridSlot::Rows, tracks(v)),
        "grid-template-areas" => set(out, GridSlot::Areas, parse_areas(v).map(GridValue::Areas)),
        "grid-auto-columns" => set(out, GridSlot::AutoColumns, tracks(v)),
        "grid-auto-rows" => set(out, GridSlot::AutoRows, tracks(v)),
        "grid-auto-flow" => {
            let column = v.split_whitespace().any(|t| t == "column");
            let valid = v
                .split_whitespace()
                .all(|t| matches!(t, "row" | "column" | "dense"));
            set(
                out,
                GridSlot::AutoFlow,
                valid.then_some(GridValue::ColumnFlow(column)),
            )
        }
        "grid-row-start" => set(out, GridSlot::RowStart, line(v)),
        "grid-row-end" => set(out, GridSlot::RowEnd, line(v)),
        "grid-column-start" => set(out, GridSlot::ColumnStart, line(v)),
        "grid-column-end" => set(out, GridSlot::ColumnEnd, line(v)),
        "grid-row" | "grid-column" => {
            let (start, end) = if name == "grid-row" {
                (GridSlot::RowStart, GridSlot::RowEnd)
            } else {
                (GridSlot::ColumnStart, GridSlot::ColumnEnd)
            };
            let parts: Vec<&str> = v.split('/').map(str::trim).collect();
            let (Some(a), b) = (parts.first().and_then(|p| parse_line(p)), parts.get(1)) else {
                return false;
            };
            let b = match b {
                Some(b) => match parse_line(b) {
                    Some(b) => b,
                    None => return false,
                },
                // A lone name sets both ends; anything else leaves the end auto.
                None => match &a {
                    GridLine::Name(_) => a.clone(),
                    _ => GridLine::Auto,
                },
            };
            if parts.len() > 2 {
                return false;
            }
            out.push((start, GridValue::Line(a)));
            out.push((end, GridValue::Line(b)));
            true
        }
        "grid-area" => {
            let parts: Vec<GridLine> = match v.split('/').map(|p| parse_line(p.trim())).collect() {
                Some(p) => p,
                None => return false,
            };
            if parts.is_empty() || parts.len() > 4 {
                return false;
            }
            // Missing values copy a name from the opposite side, otherwise they are auto.
            let fallback = |i: usize| match &parts[i] {
                GridLine::Name(_) => parts[i].clone(),
                _ => GridLine::Auto,
            };
            let row_start = parts[0].clone();
            let column_start = parts.get(1).cloned().unwrap_or_else(|| fallback(0));
            let row_end = parts.get(2).cloned().unwrap_or_else(|| fallback(0));
            let column_end = parts
                .get(3)
                .cloned()
                .unwrap_or_else(|| match &column_start {
                    GridLine::Name(_) => column_start.clone(),
                    _ => GridLine::Auto,
                });
            for (slot, l) in [
                (GridSlot::RowStart, row_start),
                (GridSlot::ColumnStart, column_start),
                (GridSlot::RowEnd, row_end),
                (GridSlot::ColumnEnd, column_end),
            ] {
                out.push((slot, GridValue::Line(l)));
            }
            true
        }
        "grid-template" | "grid" => {
            if v == "none" {
                out.push((GridSlot::Rows, GridValue::Tracks(TrackList::default())));
                out.push((GridSlot::Columns, GridValue::Tracks(TrackList::default())));
                out.push((GridSlot::Areas, GridValue::Areas(Vec::new())));
                return true;
            }
            // `grid: auto-flow ...` forms are not supported.
            if v.contains("auto-flow") {
                return false;
            }
            let Some((rows, columns)) = split_slash(v) else {
                return false;
            };
            let Some(columns) = parse_track_list(columns.trim()) else {
                return false;
            };
            if rows.contains(['"', '\'']) {
                // `"a a" 40px "b b" / 1fr 1fr`: area strings, each optionally followed by a size.
                let Some(parts) = tokens(rows) else {
                    return false;
                };
                let mut strings = Vec::new();
                let mut sizes = TrackList::default();
                for part in parts {
                    if let Some(s) = unquote(part) {
                        strings.push(s);
                        sizes.tracks.push(Track {
                            min: Breadth::Auto,
                            max: Breadth::Auto,
                        });
                    } else if part.starts_with('[') {
                        continue;
                    } else {
                        let (Some(track), Some(last)) =
                            (parse_track(part), sizes.tracks.last_mut())
                        else {
                            return false;
                        };
                        *last = track;
                    }
                }
                let Some(areas) = parse_area_rows(&strings) else {
                    return false;
                };
                out.push((GridSlot::Areas, GridValue::Areas(areas)));
                out.push((GridSlot::Rows, GridValue::Tracks(sizes)));
            } else {
                let Some(rows) = parse_track_list(rows.trim()) else {
                    return false;
                };
                out.push((GridSlot::Areas, GridValue::Areas(Vec::new())));
                out.push((GridSlot::Rows, GridValue::Tracks(rows)));
            }
            out.push((GridSlot::Columns, GridValue::Tracks(columns)));
            true
        }
        _ => false,
    }
}

pub fn is_grid_property(name: &str) -> bool {
    name.starts_with("grid")
}

/// Split at the first `/` outside quotes and parentheses.
fn split_slash(v: &str) -> Option<(&str, &str)> {
    let (mut depth, mut quote) = (0i32, None);
    for (i, ch) in v.char_indices() {
        match (ch, quote) {
            ('"' | '\'', None) => quote = Some(ch),
            (c, Some(q)) if c == q => quote = None,
            ('(', None) => depth += 1,
            (')', None) => depth -= 1,
            ('/', None) if depth == 0 => return Some((&v[..i], &v[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Whitespace-separated tokens, keeping `fn(a, b)`, `[names]` and quoted strings whole.
fn tokens(v: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None, None);
    for (i, ch) in v.char_indices() {
        match (ch, quote) {
            (c, Some(q)) if c == q => quote = None,
            (_, Some(_)) => {}
            ('"' | '\'', None) => {
                quote = Some(ch);
                start.get_or_insert(i);
            }
            ('(' | '[', None) => {
                depth += 1;
                start.get_or_insert(i);
            }
            (')' | ']', None) => depth -= 1,
            (c, None) if c.is_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(&v[s..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
        if depth < 0 {
            return None;
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    if let Some(s) = start {
        out.push(&v[s..]);
    }
    Some(out)
}

fn quoted_strings(v: &str) -> Option<Vec<&str>> {
    tokens(v)?.into_iter().map(unquote).collect()
}

fn unquote(s: &str) -> Option<&str> {
    let q = s.chars().next()?;
    (matches!(q, '"' | '\'') && s.len() >= 2 && s.ends_with(q)).then(|| &s[1..s.len() - 1])
}

/// The arguments of `name(...)` when `token` is that function.
fn function<'a>(token: &'a str, name: &str) -> Option<&'a str> {
    token
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(n: f32) -> Breadth<SpecLength> {
        Breadth::Length(SpecLength::Px(n))
    }

    #[test]
    fn parses_track_lists() {
        let list = parse_track_list(
            "[full-start] 12.25rem minmax(0,1fr) repeat(2, 10px auto) min-content",
        )
        .unwrap();
        assert_eq!(list.tracks.len(), 7);
        assert_eq!(list.tracks[0].min, Breadth::Length(SpecLength::Rem(12.25)));
        assert_eq!(
            list.tracks[1],
            Track {
                min: px(0.0),
                max: Breadth::Fr(100)
            }
        );
        assert_eq!(
            list.tracks[2],
            Track {
                min: px(10.0),
                max: px(10.0)
            }
        );
        assert_eq!(list.tracks[6].max, Breadth::MinContent);
        let auto = parse_track_list("100px repeat(auto-fill, minmax(200px, 1fr))").unwrap();
        assert_eq!(auto.tracks.len(), 1);
        assert_eq!(
            auto.auto_repeat
                .as_ref()
                .map(|r| (r.at, r.tracks.len(), r.fit)),
            Some((1, 1, false))
        );
        for bad in [
            "minmax(1fr, 2px)",
            "repeat(0, 1px)",
            "repeat(99999, 1px)",
            "10px)",
            "-5px",
            "repeat(2, repeat(auto-fill, 1px))",
        ] {
            assert_eq!(parse_track_list(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_areas_and_rejects_non_rectangles() {
        let areas =
            parse_areas("'sitenotice sitenotice' 'columnstart pagecontent' 'footer footer'")
                .unwrap();
        let find = |n: &str| areas.iter().find(|a| &*a.name == n).unwrap().clone();
        assert_eq!(
            (find("sitenotice").rows, find("sitenotice").columns),
            ((0, 1), (0, 2))
        );
        assert_eq!(
            (find("pagecontent").rows, find("pagecontent").columns),
            ((1, 2), (1, 2))
        );
        assert_eq!(parse_areas("'a .' 'a b'").map(|a| a.len()), Some(2));
        assert_eq!(parse_areas("'a b' 'b a'"), None);
        assert_eq!(parse_areas("'a b' 'c'"), None);
    }

    #[test]
    fn expands_shorthands() {
        let mut out = Vec::new();
        assert!(expand_grid(
            "grid-template",
            "min-content 1fr min-content / 12.25rem minmax(0,1fr)",
            &mut out
        ));
        assert!(matches!(&out[1], (GridSlot::Rows, GridValue::Tracks(t)) if t.tracks.len() == 3));
        assert!(
            matches!(&out[2], (GridSlot::Columns, GridValue::Tracks(t)) if t.tracks.len() == 2)
        );

        out.clear();
        assert!(expand_grid(
            "grid-template",
            "\"a a\" 40px \"b c\" / 1fr 2fr",
            &mut out
        ));
        assert!(matches!(&out[0], (GridSlot::Areas, GridValue::Areas(a)) if a.len() == 3));
        assert!(
            matches!(&out[1], (GridSlot::Rows, GridValue::Tracks(t)) if t.tracks[0].min == px(40.0) && t.tracks[1].min == Breadth::Auto)
        );

        out.clear();
        assert!(expand_grid("grid-area", "footer", &mut out));
        assert!(out
            .iter()
            .all(|(_, v)| *v == GridValue::Line(GridLine::Name("footer".into()))));

        out.clear();
        assert!(expand_grid("grid-column", "1 / -1", &mut out));
        assert_eq!(out[1].1, GridValue::Line(GridLine::Line(-1)));
        out.clear();
        assert!(expand_grid("grid-row", "span 2", &mut out));
        assert_eq!(out[0].1, GridValue::Line(GridLine::Span(2)));
        assert_eq!(out[1].1, GridValue::Line(GridLine::Auto));
        assert!(!expand_grid("grid-row", "0", &mut out));
    }
}
