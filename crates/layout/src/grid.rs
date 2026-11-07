//! Grid layout (CSS Grid Layout 1).
//!
//! Like flex, every item is laid out once: columns are sized from intrinsic widths (no layout),
//! items are laid out at their column width, their heights size the rows, and then they are
//! moved into their rows and aligned or stretched.
//!
//! Supported: track lists with lengths, percentages, `fr`, `auto`, `min-content`,
//! `max-content`, `minmax()`, `fit-content()`, `repeat()` including `auto-fill`/`auto-fit`;
//! `grid-template-areas` and `grid-area` names; line numbers (negative too) and `span`; sparse
//! row-major auto-placement; implicit tracks from `grid-auto-rows`/`-columns`; gaps;
//! `align-items`/`align-self`; `order`.
//!
//! Not yet: column auto-flow and `dense` (placed row-major), named lines, `justify-items`/
//! `justify-self` (items stretch or start), `align-content`/`justify-content`, baseline
//! alignment, subgrid, and intrinsic sizes of grid containers (measured as blocks).

use super::*;
use css::grid::{Breadth, GridLine, GridTemplate, Track};

#[derive(Clone, Copy)]
enum Content {
    Element(NodeId),
    Anonymous(NodeId, usize),
}

struct GridItem {
    content: Content,
    order: i16,
    /// Resolved lines, zero-based, end exclusive. `None` start means auto-placed.
    row: (Option<usize>, usize),
    column: (Option<usize>, usize),
    align: Align,
    auto_height: bool,
    margin: [f32; 2],
    boxes: Range<usize>,
    texts: Range<usize>,
    clips: Range<usize>,
    /// Outer (margin-box) height after layout.
    height: f32,
    laid_at: f32,
}

impl GridItem {
    fn rows(&self) -> Range<usize> {
        let start = self.row.0.unwrap_or(0);
        start..start + self.row.1
    }

    fn columns(&self) -> Range<usize> {
        let start = self.column.0.unwrap_or(0);
        start..start + self.column.1
    }
}

/// A track being sized: its base size and growth limit (`INFINITY` while unknown).
#[derive(Clone, Copy)]
struct Sizing {
    track: Track<Length>,
    base: f32,
    limit: f32,
}

const AUTO_TRACK: Track<Length> = Track {
    min: Breadth::Auto,
    max: Breadth::Auto,
};

/// Tracks a grid may create; placements beyond are clamped (a page cannot make us allocate
/// millions of cells).
const MAX_TRACKS: usize = 1000;

impl Engine<'_> {
    /// Lay out the grid container `container`, whose content box starts at (x, y), and return
    /// its content height.
    pub(crate) fn layout_grid(
        &mut self,
        container: NodeId,
        x: f32,
        y: f32,
        width: f32,
        height: Option<f32>,
    ) -> f32 {
        let styles = self.styles;
        let s = self.style(container);
        let template = styles.grid_template(s);
        let column_gap = resolve(s.gap[1], width).unwrap_or(0.0);
        let row_gap = height
            .and_then(|h| resolve(s.gap[0], h))
            .or_else(|| resolve(s.gap[0], 0.0))
            .unwrap_or(0.0);

        let mut columns = explicit_columns(template, width, column_gap);
        let mut rows = template.rows.tracks.clone();
        // Areas define the explicit grid too.
        for area in &template.areas {
            while columns.len() < area.columns.1 as usize {
                columns.push(AUTO_TRACK);
            }
            while rows.len() < area.rows.1 as usize {
                rows.push(AUTO_TRACK);
            }
        }
        let (explicit_rows, explicit_columns) = (rows.len(), columns.len());

        let mut items = self.grid_items(container, x, y, template, explicit_rows, explicit_columns);
        if items.is_empty() {
            return 0.0;
        }
        items.sort_by_key(|i| i.order);
        let (row_count, column_count) = place(&mut items, explicit_columns);
        let auto_row = template.auto_rows.unwrap_or(AUTO_TRACK);
        let auto_column = template.auto_columns.unwrap_or(AUTO_TRACK);
        columns.resize(column_count.max(columns.len()), auto_column);
        rows.resize(row_count.max(rows.len()), auto_row);

        // Columns from intrinsic widths.
        let mut contributions = Vec::with_capacity(items.len());
        for item in &items {
            let c = match item.content {
                Content::Element(id) => self.outer_intrinsic(id),
                Content::Anonymous(first, count) => self.measure_inline(first, count),
            };
            contributions.push((item.columns(), c.0, c.1));
        }
        let column_sizes = size_tracks(&columns, Some(width), column_gap, &contributions);
        let column_pos = positions(x, &column_sizes, column_gap);

        // Lay out every item in its columns, stacked at the top for now.
        for item in items.iter_mut() {
            let cols = item.columns();
            let area_x = column_pos[cols.start];
            let area_width = span(&column_sizes, cols, column_gap);
            item.laid_at = y + item.margin[0];
            let border_height = match item.content {
                Content::Element(id) => {
                    let s = self.style(id);
                    let ex = Edges::of(s, area_width).x();
                    let ml = resolve(s.margin[3], area_width).unwrap_or(0.0);
                    let mr = resolve(s.margin[1], area_width).unwrap_or(0.0);
                    let available = (area_width - ml - mr - ex).max(0.0);
                    // Images keep their own width (justify-self: normal acts as start for
                    // them); other items stretch across their area.
                    let w = content_size(s, s.width, Some(area_width), ex).unwrap_or_else(|| {
                        if self.is_replaced(id) {
                            self.replaced_size(id, s, Some(area_width), None)
                                .0
                                .min(available)
                        } else {
                            available
                        }
                    });
                    let w = clamp_width(s, w, area_width, ex);
                    let start = (
                        self.out.boxes.len(),
                        self.out.texts.len(),
                        self.out.clips.len(),
                    );
                    let h =
                        self.build_box(id, area_x + ml, item.laid_at, w, area_width, None, None);
                    item.boxes = start.0..self.out.boxes.len();
                    item.texts = start.1..self.out.texts.len();
                    item.clips = start.2..self.out.clips.len();
                    h
                }
                Content::Anonymous(first, count) => {
                    let start = (
                        self.out.boxes.len(),
                        self.out.texts.len(),
                        self.out.clips.len(),
                    );
                    let h = self.layout_inline(
                        first,
                        count,
                        container,
                        area_x,
                        item.laid_at,
                        area_width,
                    );
                    item.boxes = start.0..self.out.boxes.len();
                    item.texts = start.1..self.out.texts.len();
                    item.clips = start.2..self.out.clips.len();
                    h
                }
            };
            item.height = border_height + item.margin[0] + item.margin[1];
        }

        // Rows from the laid-out heights.
        let contributions: Vec<_> = items
            .iter()
            .map(|i| (i.rows(), i.height, i.height))
            .collect();
        let row_sizes = size_tracks(&rows, height, row_gap, &contributions);
        let row_pos = positions(y, &row_sizes, row_gap);

        for item in &items {
            let rows = item.rows();
            let area_height = span(&row_sizes, rows.clone(), row_gap);
            let space = area_height - item.height;
            let mut dy = row_pos[rows.start] + item.margin[0] - item.laid_at;
            match item.align {
                Align::Center => dy += space / 2.0,
                Align::End => dy += space,
                Align::Stretch if item.auto_height => {
                    if let Content::Element(_) = item.content {
                        let target = area_height - item.margin[0] - item.margin[1];
                        self.set_box_height(item.boxes.start, item.clips.clone(), target);
                    }
                }
                _ => {}
            }
            self.shift_range(
                item.boxes.clone(),
                item.texts.clone(),
                item.clips.clone(),
                0.0,
                dy,
            );
        }
        row_sizes.iter().sum::<f32>() + row_gap * row_sizes.len().saturating_sub(1) as f32
    }

    /// In-flow children as grid items, with their lines resolved against the explicit grid.
    fn grid_items(
        &mut self,
        container: NodeId,
        x: f32,
        y: f32,
        template: &GridTemplate,
        explicit_rows: usize,
        explicit_columns: usize,
    ) -> Vec<GridItem> {
        let dom = self.dom;
        let styles = self.styles;
        let align_items = self.style(container).align_items;
        let anonymous = |first, count| GridItem {
            content: Content::Anonymous(first, count),
            order: 0,
            row: (None, 1),
            column: (None, 1),
            align: align_items,
            auto_height: true,
            margin: [0.0; 2],
            boxes: 0..0,
            texts: 0..0,
            clips: 0..0,
            height: 0.0,
            laid_at: 0.0,
        };
        let mut items = Vec::new();
        let mut run: Option<(NodeId, usize, bool)> = None;
        for child in dom.children(container) {
            match dom.kind(child) {
                NodeKind::Text(text) => {
                    let visible = !text.trim_ascii().is_empty();
                    match &mut run {
                        Some((_, count, any)) => {
                            *count += 1;
                            *any |= visible;
                        }
                        None => run = Some((child, 1, visible)),
                    }
                }
                NodeKind::Element(_) => {
                    if let Some((first, count, true)) = run.take() {
                        items.push(anonymous(first, count));
                    }
                    let s = self.style(child);
                    if s.display == Display::None {
                        continue;
                    }
                    if s.position.is_out_of_flow() {
                        self.defer(child, x, y);
                        continue;
                    }
                    let p = styles.grid_placement(s);
                    let width = 0.0;
                    items.push(GridItem {
                        content: Content::Element(child),
                        order: s.order,
                        row: resolve_lines(&p.row, explicit_rows, template, true),
                        column: resolve_lines(&p.column, explicit_columns, template, false),
                        align: if s.align_self == Align::Auto {
                            align_items
                        } else {
                            s.align_self
                        },
                        auto_height: s.height == Length::Auto,
                        margin: [0, 2].map(|i| resolve(s.margin[i], width).unwrap_or(0.0)),
                        boxes: 0..0,
                        texts: 0..0,
                        clips: 0..0,
                        height: 0.0,
                        laid_at: 0.0,
                    });
                }
                _ => {
                    if let Some((_, count, _)) = &mut run {
                        *count += 1;
                    }
                }
            }
        }
        if let Some((first, count, true)) = run {
            items.push(anonymous(first, count));
        }
        items
    }

    /// Give box `i` a new border-box height; its own overflow clip follows.
    pub(crate) fn set_box_height(&mut self, i: usize, clips: Range<usize>, border_height: f32) {
        let rect = self.out.boxes[i].rect;
        let delta = border_height - rect.height;
        if delta == 0.0 {
            return;
        }
        self.out.boxes[i].rect.height = border_height;
        for c in clips {
            if self.out.clips[c].boxes.0 as usize == i + 1 {
                self.out.clips[c].rect.height = (self.out.clips[c].rect.height + delta).max(0.0);
            }
        }
        if !self.in_fixed {
            self.bottom = self.bottom.max(rect.y + border_height);
        }
    }
}

/// Explicit columns with any `repeat(auto-fill, ...)` expanded to as many repetitions as fit.
fn explicit_columns(template: &GridTemplate, width: f32, gap: f32) -> Vec<Track<Length>> {
    let list = &template.columns;
    let mut tracks = list.tracks.clone();
    let Some(repeat) = &list.auto_repeat else {
        return tracks;
    };
    // A track's size for counting repetitions: its fixed maximum, else its fixed minimum.
    let fixed = |t: &Track<Length>| match (t.max, t.min) {
        (Breadth::Length(l), _) | (_, Breadth::Length(l)) => resolve(l, width),
        _ => None,
    };
    let others: f32 = tracks.iter().filter_map(fixed).sum();
    let each: Option<f32> = repeat.tracks.iter().map(fixed).sum();
    let count = match each {
        Some(each) if each > 0.0 => {
            let per = each + gap * repeat.tracks.len() as f32;
            let room = width - others - gap * tracks.len() as f32 + gap;
            ((room / per).floor() as usize).clamp(1, MAX_TRACKS / repeat.tracks.len().max(1))
        }
        _ => 1,
    };
    let at = repeat.at.min(tracks.len());
    let expanded: Vec<_> = (0..count)
        .flat_map(|_| repeat.tracks.iter().copied())
        .collect();
    tracks.splice(at..at, expanded);
    tracks
}

/// Resolve an item's start and end lines on one axis into (start, span) (Grid 8.3).
fn resolve_lines(
    lines: &[GridLine; 2],
    explicit: usize,
    template: &GridTemplate,
    rows: bool,
) -> (Option<usize>, usize) {
    enum L {
        At(usize),
        Span(usize),
        Auto,
    }
    let line = |l: &GridLine, end: bool| match l {
        GridLine::Auto => L::Auto,
        GridLine::Span(n) => L::Span((*n as usize).clamp(1, MAX_TRACKS)),
        GridLine::Line(n) if *n > 0 => L::At((*n as usize - 1).min(MAX_TRACKS)),
        // -1 is the last explicit line.
        GridLine::Line(n) => L::At((explicit as i64 + 1 + *n as i64).max(0) as usize),
        GridLine::Name(name) => match template.area(name) {
            Some(a) => {
                let (s, e) = if rows { a.rows } else { a.columns };
                L::At(if end { e } else { s } as usize)
            }
            None => L::Auto,
        },
    };
    match (line(&lines[0], false), line(&lines[1], true)) {
        (L::At(a), L::At(b)) if a == b => (Some(a), 1),
        (L::At(a), L::At(b)) => (Some(a.min(b)), a.abs_diff(b)),
        (L::At(a), L::Span(n)) => (Some(a), n),
        (L::At(a), L::Auto) => (Some(a), 1),
        (L::Span(n), L::At(b)) => (Some(b.saturating_sub(n)), n.min(b.max(1))),
        (L::Auto, L::At(b)) => (Some(b.saturating_sub(1)), 1),
        (L::Span(n), _) | (L::Auto, L::Span(n)) => (None, n),
        (L::Auto, L::Auto) => (None, 1),
    }
}

/// Sparse, row-major auto-placement (Grid 8.5). Returns the row and column counts used.
fn place(items: &mut [GridItem], explicit_columns: usize) -> (usize, usize) {
    let columns = items
        .iter()
        .map(|i| match i.column.0 {
            Some(start) => start + i.column.1,
            None => i.column.1,
        })
        .fold(explicit_columns.max(1), usize::max)
        .min(MAX_TRACKS);
    let mut occupied: Vec<bool> = Vec::new();
    let mut rows = 0;
    let free = |occupied: &Vec<bool>, r: Range<usize>, c: Range<usize>| {
        r.into_iter().all(|r| {
            c.clone()
                .all(|c| !occupied.get(r * columns + c).copied().unwrap_or(false))
        })
    };
    let mark = |occupied: &mut Vec<bool>, rows: &mut usize, r: Range<usize>, c: Range<usize>| {
        *rows = (*rows).max(r.end);
        if occupied.len() < r.end * columns {
            occupied.resize(r.end * columns, false);
        }
        for row in r {
            for col in c.clone() {
                occupied[row * columns + col] = true;
            }
        }
    };
    let fit_column = |item: &mut GridItem| {
        item.column.1 = item.column.1.min(columns);
        if let Some(start) = item.column.0 {
            item.column.0 = Some(start.min(columns - item.column.1));
        }
    };

    // 1. Items fixed on both axes.
    for item in items.iter_mut() {
        fit_column(item);
        item.row.1 = item.row.1.min(MAX_TRACKS);
        if let (Some(_), Some(_)) = (item.row.0, item.column.0) {
            mark(&mut occupied, &mut rows, item.rows(), item.columns());
        }
    }
    // 2. Items with a fixed row: the first columns free in that row.
    for item in items.iter_mut() {
        if let (Some(_), None) = (item.row.0, item.column.0) {
            let start = (0..=columns - item.column.1)
                .find(|&c| free(&occupied, item.rows(), c..c + item.column.1))
                .unwrap_or(0);
            item.column.0 = Some(start);
            mark(&mut occupied, &mut rows, item.rows(), item.columns());
        }
    }
    // 3. Everything else, from a cursor that only moves forward.
    let (mut cursor_row, mut cursor_col) = (0usize, 0usize);
    for item in items.iter_mut() {
        if item.row.0.is_some() {
            continue;
        }
        let span_c = item.column.1;
        let span_r = item.row.1;
        if let Some(col) = item.column.0 {
            if col < cursor_col {
                cursor_row += 1;
            }
            let mut r = cursor_row;
            while !free(&occupied, r..r + span_r, col..col + span_c) && r < MAX_TRACKS {
                r += 1;
            }
            item.row.0 = Some(r);
            cursor_row = r;
            cursor_col = col;
        } else {
            let (mut r, mut c) = (cursor_row, cursor_col);
            loop {
                if c + span_c > columns {
                    r += 1;
                    c = 0;
                }
                if free(&occupied, r..r + span_r, c..c + span_c) || r >= MAX_TRACKS {
                    break;
                }
                c += 1;
            }
            item.row.0 = Some(r);
            item.column.0 = Some(c);
            cursor_row = r;
            cursor_col = c + span_c;
        }
        mark(&mut occupied, &mut rows, item.rows(), item.columns());
    }
    (rows, columns)
}

/// Size tracks (Grid 12.3 to 12.8, simplified). `contributions` are (tracks spanned,
/// min-content, max-content) outer sizes of the items. With no definite `available` size,
/// flexible tracks size to their content like `auto`.
fn size_tracks(
    tracks: &[Track<Length>],
    available: Option<f32>,
    gap: f32,
    contributions: &[(Range<usize>, f32, f32)],
) -> Vec<f32> {
    let percent_base = available.unwrap_or(0.0);
    let length = |l: Length| match l {
        Length::Percent(_) if available.is_none() => None,
        l => resolve(l, percent_base),
    };
    let flexible = |t: &Track<Length>| available.is_some() && matches!(t.max, Breadth::Fr(_));
    let mut sizing: Vec<Sizing> = tracks
        .iter()
        .map(|&track| {
            let base = match track.min {
                Breadth::Length(l) => length(l).unwrap_or(0.0),
                _ => 0.0,
            };
            let limit = match track.max {
                Breadth::Length(l) => length(l).map_or(f32::INFINITY, |v| v.max(base)),
                _ => f32::INFINITY,
            };
            Sizing { track, base, limit }
        })
        .collect();
    let intrinsic_min = |t: &Track<Length>| {
        matches!(
            t.min,
            Breadth::Auto | Breadth::MinContent | Breadth::MaxContent
        ) || matches!(t.min, Breadth::Length(Length::Percent(_)) if available.is_none())
    };

    // Content: single-track items first, then spanning ones spread their excess evenly.
    let mut content_limit = vec![0.0f32; sizing.len()];
    let mut by_span: Vec<&(Range<usize>, f32, f32)> = contributions.iter().collect();
    by_span.sort_by_key(|c| c.0.len());
    for (range, min_c, max_c) in by_span {
        let range = range.start.min(sizing.len())..range.end.min(sizing.len());
        if range.is_empty() || range.clone().any(|i| flexible(&sizing[i].track)) && range.len() > 1
        {
            continue;
        }
        let gaps = gap * (range.len() - 1) as f32;
        let targets: Vec<usize> = range
            .clone()
            .filter(|&i| intrinsic_min(&sizing[i].track))
            .collect();
        if !targets.is_empty() {
            let wants = |t: &Track<Length>| match t.min {
                Breadth::MaxContent => *max_c,
                _ => *min_c,
            };
            let have: f32 = range.clone().map(|i| sizing[i].base).sum::<f32>() + gaps;
            let want = targets
                .iter()
                .map(|&i| wants(&sizing[i].track))
                .fold(0.0, f32::max);
            let extra = (want - have).max(0.0) / targets.len() as f32;
            for &i in &targets {
                sizing[i].base += extra;
            }
        }
        // Growth limits: one track takes its content size; a span spreads what it lacks.
        if range.len() == 1 {
            let i = range.start;
            let c = match sizing[i].track.max {
                Breadth::MinContent => *min_c,
                _ => *max_c,
            };
            content_limit[i] = content_limit[i].max(c);
        } else {
            let have: f32 = range.clone().map(|i| content_limit[i]).sum::<f32>() + gaps;
            let extra = (max_c - have).max(0.0) / range.len() as f32;
            for i in range {
                content_limit[i] += extra;
            }
        }
    }
    for (s, &c) in sizing.iter_mut().zip(&content_limit) {
        if s.limit.is_infinite() && !flexible(&s.track) {
            s.limit = c;
        }
        if let Breadth::Length(l) = s.track.max {
            // `fit-content(x)`: content, but no more than x.
            if let (Breadth::Auto, Some(cap)) = (s.track.min, length(l)) {
                s.limit = c.min(cap);
            }
        }
        s.limit = s.limit.max(s.base);
    }

    let gaps = gap * sizing.len().saturating_sub(1) as f32;
    let Some(available) = available else {
        // No definite size: every track grows to its limit.
        return sizing
            .iter()
            .map(|s| if s.limit.is_finite() { s.limit } else { s.base })
            .collect();
    };

    // Maximize: grow tracks toward their limits with the free space.
    let mut free = available - gaps - sizing.iter().map(|s| s.base).sum::<f32>();
    while free > 0.01 {
        let growable: Vec<usize> = (0..sizing.len())
            .filter(|&i| !flexible(&sizing[i].track) && sizing[i].limit > sizing[i].base + 0.01)
            .collect();
        if growable.is_empty() {
            break;
        }
        let share = free / growable.len() as f32;
        for i in growable {
            let grow = share.min(sizing[i].limit - sizing[i].base);
            sizing[i].base += grow;
            free -= grow;
        }
    }

    // Flexible tracks share what is left by their factors.
    let flex: Vec<usize> = (0..sizing.len())
        .filter(|&i| flexible(&sizing[i].track))
        .collect();
    if !flex.is_empty() {
        let factors_of: Vec<f32> = sizing
            .iter()
            .map(|s| match s.track.max {
                Breadth::Fr(f) => f as f32 / 100.0,
                _ => 0.0,
            })
            .collect();
        let factor = |i: usize| factors_of[i];
        let mut inflexible: Vec<bool> = vec![false; sizing.len()];
        loop {
            let space = available
                - gaps
                - (0..sizing.len())
                    .filter(|&i| !flexible(&sizing[i].track) || inflexible[i])
                    .map(|i| sizing[i].base)
                    .sum::<f32>();
            let factors: f32 = flex
                .iter()
                .filter(|&&i| !inflexible[i])
                .map(|&i| factor(i))
                .sum();
            let unit = space.max(0.0) / factors.max(1.0);
            let mut changed = false;
            for &i in &flex {
                if !inflexible[i] && sizing[i].base > unit * factor(i) {
                    inflexible[i] = true;
                    changed = true;
                }
            }
            if !changed {
                for &i in &flex {
                    if !inflexible[i] {
                        sizing[i].base = unit * factor(i);
                    }
                }
                break;
            }
        }
    } else {
        // Stretch `auto` tracks over what is left (justify-content: normal).
        let free = available - gaps - sizing.iter().map(|s| s.base).sum::<f32>();
        let autos: Vec<usize> = (0..sizing.len())
            .filter(|&i| sizing[i].track.max == Breadth::Auto)
            .collect();
        if free > 0.0 && !autos.is_empty() {
            for &i in &autos {
                sizing[i].base += free / autos.len() as f32;
            }
        }
    }
    sizing.iter().map(|s| s.base).collect()
}

/// Start offset of each track.
fn positions(origin: f32, sizes: &[f32], gap: f32) -> Vec<f32> {
    let mut at = origin;
    sizes
        .iter()
        .map(|&size| {
            let start = at;
            at += size + gap;
            start
        })
        .collect()
}

/// Size of the tracks in `range` plus the gaps between them.
fn span(sizes: &[f32], range: Range<usize>, gap: f32) -> f32 {
    let n = range.len();
    sizes[range].iter().sum::<f32>() + gap * n.saturating_sub(1) as f32
}
