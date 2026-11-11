//! Flex layout (CSS Flexible Box Layout 1).
//!
//! Every item is laid out once. Main sizes come from intrinsic widths (computed without layout
//! and memoized), so nested flex containers do not lay their contents out repeatedly. After an
//! item is laid out at its final main size, cross-axis alignment moves its fragments and
//! `stretch` grows its box; neither needs a second layout.
//!
//! Supported: `row`/`column` and their reverses, `wrap` for rows, `flex-grow`/`-shrink`/`-basis`
//! with min/max clamping (section 9.7), automatic minimum sizes, auto margins on the main axis,
//! `justify-content`, `align-items`/`align-self`, `gap`, `order`, anonymous items for text.
//!
//! Not yet: `align-content` (lines pack at the start), wrapping columns, baseline alignment
//! (laid out as `start`), auto margins on the cross axis.

use super::*;

#[derive(Clone, Copy)]
enum Content {
    Element(NodeId),
    /// A run of text siblings (`first`, `count`) wrapped in an anonymous item.
    Anonymous(NodeId, usize),
}

struct FlexItem {
    content: Content,
    order: i16,
    grow: f32,
    shrink: f32,
    /// Flex base size and min/max main sizes, all content-box. `NAN` base or min means "from
    /// the laid-out content" (columns only, where the main size is a height).
    base: f32,
    min: f32,
    max: f32,
    /// Padding plus border along the main axis.
    extra: f32,
    /// Main-axis margins (start, end); `None` is `auto`.
    margin: [Option<f32>; 2],
    /// Cross-axis margins (start, end).
    cross_margin: [f32; 2],
    align: Align,
    /// The cross size is `auto`, so `stretch` may grow it.
    auto_cross: bool,
    /// The resolved content-box main size.
    size: f32,
    frozen: bool,
    violation: f32,
    boxes: Range<usize>,
    texts: Range<usize>,
    clips: Range<usize>,
    /// Outer cross size after layout, margins included.
    cross: f32,
    /// Main-axis position the item was laid out at (columns: moved into place afterwards).
    laid_at: f32,
}

impl FlexItem {
    fn clamp(&self, v: f32) -> f32 {
        v.min(self.max).max(self.min)
    }

    /// Margin-box main size for a content size.
    fn outer(&self, size: f32) -> f32 {
        size + self.extra + self.margin[0].unwrap_or(0.0) + self.margin[1].unwrap_or(0.0)
    }

    fn auto_margins(&self) -> usize {
        self.margin.iter().filter(|m| m.is_none()).count()
    }
}

impl Engine<'_> {
    /// Lay out the flex container `container`, whose content box starts at (x, y), and return
    /// its content height.
    pub(crate) fn layout_flex(
        &mut self,
        container: NodeId,
        x: f32,
        y: f32,
        width: f32,
        height: Option<f32>,
    ) -> f32 {
        let s = self.style(container);
        let row = s.flex_direction.is_row();
        let reverse = s.flex_direction.is_reverse();
        // `column-gap` separates items in a row and `row-gap` separates rows; in a column,
        // `row-gap` separates the items.
        let (main_gap, cross_gap) = if row {
            (
                resolve(s.gap[1], width).unwrap_or(0.0),
                height
                    .and_then(|h| resolve(s.gap[0], h))
                    .or_else(|| resolve(s.gap[0], 0.0)),
            )
        } else {
            (
                height
                    .and_then(|h| resolve(s.gap[0], h))
                    .unwrap_or_else(|| resolve(s.gap[0], 0.0).unwrap_or(0.0)),
                Some(0.0),
            )
        };
        let cross_gap = cross_gap.unwrap_or(0.0);

        let mut items = self.flex_items(container, x, y, row, width, height);
        if items.is_empty() {
            return 0.0;
        }
        // Stable, so equal `order` keeps document order.
        items.sort_by_key(|i| i.order);

        if !row {
            return self.flex_column(
                &mut items, container, x, y, width, height, main_gap, reverse,
            );
        }

        // Break into lines by hypothetical sizes when wrapping.
        let mut lines: Vec<Range<usize>> = Vec::new();
        let mut start = 0;
        let mut used = 0.0;
        for (i, item) in items.iter().enumerate() {
            let outer = item.outer(item.clamp(item.base));
            let gap = if i > start { main_gap } else { 0.0 };
            if s.flex_wrap && i > start && used + gap + outer > width {
                lines.push(start..i);
                start = i;
                used = outer;
            } else {
                used += gap + outer;
            }
        }
        lines.push(start..items.len());

        let single = lines.len() == 1;
        let mut cursor = y;
        for (n, line) in lines.into_iter().enumerate() {
            if n > 0 {
                cursor += cross_gap;
            }
            let definite_cross = if single { height } else { None };
            let line_cross = self.flex_row_line(
                &mut items[line],
                container,
                x,
                cursor,
                width,
                height,
                definite_cross,
                main_gap,
                reverse,
            );
            cursor += line_cross;
        }
        cursor - y
    }

    /// Collect the in-flow children of `container` as flex items. Out-of-flow children are
    /// deferred with the content box's start as their static position.
    fn flex_items(
        &mut self,
        container: NodeId,
        x: f32,
        y: f32,
        row: bool,
        width: f32,
        height: Option<f32>,
    ) -> Vec<FlexItem> {
        let dom = self.dom;
        let align_items = self.style(container).align_items;
        let mut items = Vec::new();
        // (first, count, has visible text)
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
                        items.push(self.anonymous_item(first, count, row, align_items));
                    }
                    let s = self.style(child);
                    if s.display == Display::None {
                        continue;
                    }
                    if s.position.is_out_of_flow() {
                        self.defer(child, x, y);
                        continue;
                    }
                    items.push(self.element_item(child, row, width, height, align_items));
                }
                _ => {
                    if let Some((_, count, _)) = &mut run {
                        *count += 1;
                    }
                }
            }
        }
        if let Some((first, count, true)) = run {
            items.push(self.anonymous_item(first, count, row, align_items));
        }
        items
    }

    fn element_item(
        &mut self,
        id: NodeId,
        row: bool,
        width: f32,
        height: Option<f32>,
        align_items: Align,
    ) -> FlexItem {
        let s = self.style(id);
        let edges = Edges::of(s, width);
        let (extra, cross_extra) = if row {
            (edges.x(), edges.y())
        } else {
            (edges.y(), edges.x())
        };
        let main_ref = if row { Some(width) } else { height };
        let (size, min_size, max_size) = if row {
            (s.width, s.min_width, s.max_width)
        } else {
            (s.height, s.min_height, s.max_height)
        };
        let specified = content_size(s, size, main_ref, extra);
        let basis = match s.flex_basis {
            Length::Auto => specified,
            b => content_size(s, b, main_ref, extra),
        };
        let max = content_size(s, max_size, main_ref, extra).unwrap_or(f32::INFINITY);
        let min = match min_size {
            // Automatic minimum (4.5): the content's min-content size, capped by a specified
            // size; none for boxes that clip their overflow.
            Length::Auto if s.clips => 0.0,
            Length::Auto if row => {
                let min_content = (self.intrinsic_width(id).0 - extra).max(0.0);
                specified
                    .map_or(min_content, |w| w.min(min_content))
                    .min(max)
            }
            Length::Auto => f32::NAN,
            l => content_size(s, l, main_ref, extra).unwrap_or(0.0),
        };
        let base = match basis {
            Some(b) => b,
            None if row => (self.intrinsic_width(id).1 - extra).max(0.0),
            None => f32::NAN,
        };
        let sides = if row { [3, 1] } else { [0, 2] };
        let cross_sides = if row { [0, 2] } else { [3, 1] };
        let cross_size = if row {
            content_size(s, s.height, height, cross_extra)
        } else {
            content_size(s, s.width, Some(width), cross_extra)
        };
        FlexItem {
            content: Content::Element(id),
            order: s.order,
            grow: s.flex_grow as f32 / 100.0,
            shrink: s.flex_shrink as f32 / 100.0,
            base,
            min,
            max,
            extra,
            margin: sides.map(|i| resolve(s.margin[i], width)),
            cross_margin: cross_sides.map(|i| resolve(s.margin[i], width).unwrap_or(0.0)),
            align: if s.align_self == Align::Auto {
                align_items
            } else {
                s.align_self
            },
            auto_cross: cross_size.is_none(),
            size: 0.0,
            frozen: false,
            violation: 0.0,
            boxes: 0..0,
            texts: 0..0,
            clips: 0..0,
            cross: 0.0,
            laid_at: 0.0,
        }
    }

    fn anonymous_item(&mut self, first: NodeId, count: usize, row: bool, align: Align) -> FlexItem {
        let (min, max) = if row {
            self.measure_inline(first, count)
        } else {
            (f32::NAN, f32::NAN)
        };
        FlexItem {
            content: Content::Anonymous(first, count),
            order: 0,
            grow: 0.0,
            shrink: 1.0,
            base: max,
            min,
            max: f32::INFINITY,
            extra: 0.0,
            margin: [Some(0.0); 2],
            cross_margin: [0.0; 2],
            align,
            auto_cross: true,
            size: 0.0,
            frozen: false,
            violation: 0.0,
            boxes: 0..0,
            texts: 0..0,
            clips: 0..0,
            cross: 0.0,
            laid_at: 0.0,
        }
    }

    /// Lay out one item with its border box's top left at (x, y) and the given content width.
    /// Records its fragment ranges and returns its border-box height.
    #[allow(clippy::too_many_arguments)]
    fn place_item(
        &mut self,
        item: &mut FlexItem,
        container: NodeId,
        x: f32,
        y: f32,
        content_width: f32,
        cb_width: f32,
        cb_height: Option<f32>,
        forced_height: Option<f32>,
    ) -> f32 {
        let start = (
            self.out.boxes.len(),
            self.out.texts.len(),
            self.out.clips.len(),
        );
        let height = match item.content {
            Content::Element(id) => {
                self.build_box(id, x, y, content_width, cb_width, cb_height, forced_height)
            }
            Content::Anonymous(first, count) => {
                self.layout_inline(first, count, container, x, y, content_width)
            }
        };
        item.boxes = start.0..self.out.boxes.len();
        item.texts = start.1..self.out.texts.len();
        item.clips = start.2..self.out.clips.len();
        height
    }

    /// Lay out one line of a row container. Returns the line's cross size (height).
    #[allow(clippy::too_many_arguments)]
    fn flex_row_line(
        &mut self,
        items: &mut [FlexItem],
        container: NodeId,
        x: f32,
        top: f32,
        width: f32,
        cb_height: Option<f32>,
        definite_cross: Option<f32>,
        gap: f32,
        reverse: bool,
    ) -> f32 {
        let gaps = gap * (items.len() - 1) as f32;
        resolve_flexible(items, width - gaps);
        let positions = main_positions(items, width, gap, self.style(container).justify_content);

        for (item, pos) in items.iter_mut().zip(positions) {
            let border_width = item.size + item.extra;
            let left = if reverse {
                x + width - pos - border_width
            } else {
                x + pos
            };
            let height = self.place_item(
                item,
                container,
                left,
                top + item.cross_margin[0],
                item.size,
                width,
                cb_height,
                None,
            );
            item.cross = height + item.cross_margin[0] + item.cross_margin[1];
        }

        let line_cross =
            definite_cross.unwrap_or_else(|| items.iter().map(|i| i.cross).fold(0.0, f32::max));
        for item in items.iter() {
            let space = line_cross - item.cross;
            let dy = match item.align {
                Align::Center => space / 2.0,
                Align::End => space,
                Align::Stretch if item.auto_cross => {
                    let height = line_cross - item.cross_margin[0] - item.cross_margin[1];
                    self.stretch(item, height);
                    0.0
                }
                _ => 0.0,
            };
            self.shift_range(
                item.boxes.clone(),
                item.texts.clone(),
                item.clips.clone(),
                0.0,
                dy,
            );
        }
        line_cross
    }

    /// Lay out a single-line column container. Items are laid out stacked at their natural
    /// heights first (a column's main size is the content height), then resolved and moved.
    #[allow(clippy::too_many_arguments)]
    fn flex_column(
        &mut self,
        items: &mut [FlexItem],
        container: NodeId,
        x: f32,
        y: f32,
        width: f32,
        height: Option<f32>,
        gap: f32,
        reverse: bool,
    ) -> f32 {
        let mut cursor = y;
        for item in items.iter_mut() {
            // Cross size: the item's width.
            let (left, content_width, cross) = match item.content {
                Content::Element(id) => {
                    let s = self.style(id);
                    let ex = Edges::of(s, width).x();
                    let [ml, mr] = item.cross_margin;
                    let available = (width - ml - mr - ex).max(0.0);
                    let w = match content_size(s, s.width, Some(width), ex) {
                        Some(w) => w,
                        None if item.align == Align::Stretch => available,
                        None => (self.intrinsic_width(id).1 - ex).clamp(0.0, available),
                    };
                    let w = clamp_width(s, w, width, ex);
                    (ml, w, w + ex + ml + mr)
                }
                Content::Anonymous(..) => (0.0, width, width),
            };
            let space = width - cross;
            let dx = match item.align {
                Align::Center => space / 2.0,
                Align::End => space,
                _ => 0.0,
            };
            item.laid_at = cursor + item.margin[0].unwrap_or(0.0);
            let border_height = self.place_item(
                item,
                container,
                x + left + dx,
                item.laid_at,
                content_width,
                width,
                height,
                None,
            );
            let natural = (border_height - item.extra).max(0.0);
            if item.base.is_nan() {
                item.base = natural;
            }
            if item.min.is_nan() {
                item.min = natural.min(item.max);
            }
            if item.max.is_nan() {
                item.max = f32::INFINITY;
            }
            item.cross = natural;
            cursor = item.laid_at + border_height + item.margin[1].unwrap_or(0.0) + gap;
        }

        let gaps = gap * (items.len() - 1) as f32;
        match height {
            Some(h) => resolve_flexible(items, h - gaps),
            None => {
                for item in items.iter_mut() {
                    item.size = item.clamp(item.base);
                }
            }
        }
        let total: f32 = items.iter().map(|i| i.outer(i.size)).sum::<f32>() + gaps;
        let main = height.unwrap_or(total);
        let justify = self.style(container).justify_content;
        let positions = main_positions(items, main, gap, justify);
        for (item, pos) in items.iter().zip(positions) {
            if (item.size - item.cross).abs() > 0.01 {
                self.stretch(item, item.size + item.extra);
            }
            let target = if reverse {
                y + main - pos - item.size - item.extra
            } else {
                y + pos
            };
            self.shift_range(
                item.boxes.clone(),
                item.texts.clone(),
                item.clips.clone(),
                0.0,
                target - item.laid_at,
            );
        }
        main
    }

    /// Give an item's box a new border-box height (stretching, or a resolved column size).
    fn stretch(&mut self, item: &FlexItem, border_height: f32) {
        if let Content::Element(_) = item.content {
            self.set_box_height(item.boxes.start, item.clips.clone(), border_height);
        }
    }

    /// Intrinsic (min-content, max-content) border-box widths of `id`, without margins
    /// (CSS Sizing 3). Percentages count as `auto`, as they do for intrinsic sizes.
    pub(crate) fn intrinsic_width(&mut self, id: NodeId) -> (f32, f32) {
        if let Some(&v) = self.intrinsic.get(&id) {
            return v;
        }
        let s = self.style(id);
        let edges = Edges::of(s, 0.0);
        let extra = edges.x();
        let px = |l: Length| match l {
            Length::Px(p) => Some(p.to_f32()),
            _ => None,
        };
        let border_box = |w: f32| {
            if s.border_box {
                w.max(extra)
            } else {
                w + extra
            }
        };
        let (mut min, mut max) = match px(s.width) {
            Some(w) => (border_box(w), border_box(w)),
            None if self.is_replaced(id) => {
                let w = self.replaced_size(id, s, None, None).0;
                (w + extra, w + extra)
            }
            None => {
                let (a, b) = self.intrinsic_content(id);
                (a + extra, b + extra)
            }
        };
        if let Some(m) = px(s.max_width).map(border_box) {
            min = min.min(m);
            max = max.min(m);
        }
        if let Some(m) = px(s.min_width).map(border_box) {
            min = min.max(m);
            max = max.max(m);
        }
        self.intrinsic.insert(id, (min, max));
        (min, max)
    }

    fn intrinsic_content(&mut self, id: NodeId) -> (f32, f32) {
        let s = self.style(id);
        let dom = self.dom;
        if s.display.is_table() {
            return self.table_intrinsic(id);
        }
        if s.display.is_flex() {
            let row = s.flex_direction.is_row();
            let gap = match s.gap[1] {
                Length::Px(p) if row => p.to_f32(),
                _ => 0.0,
            };
            let (mut min, mut max, mut count) = (0.0f32, 0.0f32, 0);
            for child in dom.children(id) {
                let (a, b) = match dom.kind(child) {
                    NodeKind::Text(t) if t.trim_ascii().is_empty() => continue,
                    NodeKind::Text(_) => self.measure_inline(child, 1),
                    NodeKind::Element(_) => {
                        let cs = self.style(child);
                        if cs.display == Display::None || cs.position.is_out_of_flow() {
                            continue;
                        }
                        self.outer_intrinsic(child)
                    }
                    _ => continue,
                };
                if row {
                    max += b;
                    min = if s.flex_wrap { min.max(a) } else { min + a };
                } else {
                    min = min.max(a);
                    max = max.max(b);
                }
                count += 1;
            }
            if count > 1 {
                let gaps = gap * (count - 1) as f32;
                max += gaps;
                if !s.flex_wrap {
                    min += gaps;
                }
            }
            return (min, max);
        }
        // A block container: the widest of its inline runs and block children.
        let (mut min, mut max) = (0.0f32, 0.0f32);
        let mut run: Option<(NodeId, usize)> = None;
        let mut take = |(a, b): (f32, f32)| {
            min = min.max(a);
            max = max.max(b);
        };
        for child in dom.children(id) {
            match self.level(child) {
                // A float sits beside the text of its run, so it is measured with it.
                Level::Inline | Level::Float => match &mut run {
                    Some((_, count)) => *count += 1,
                    None => run = Some((child, 1)),
                },
                Level::Skip | Level::OutOfFlow => {
                    if let Some((_, count)) = &mut run {
                        *count += 1;
                    }
                }
                Level::Block => {
                    if let Some((first, count)) = run.take() {
                        take(self.measure_inline(first, count));
                    }
                    take(self.outer_intrinsic(child));
                }
            }
        }
        if let Some((first, count)) = run {
            take(self.measure_inline(first, count));
        }
        (min, max)
    }

    /// Intrinsic widths plus fixed horizontal margins.
    pub(crate) fn outer_intrinsic(&mut self, id: NodeId) -> (f32, f32) {
        let (a, b) = self.intrinsic_width(id);
        let s = self.style(id);
        let margin = |l: Length| match l {
            Length::Px(p) => p.to_f32(),
            _ => 0.0,
        };
        let m = margin(s.margin[1]) + margin(s.margin[3]);
        ((a + m).max(0.0), (b + m).max(0.0))
    }

    /// Min-content and max-content widths of an inline run: its widest unbreakable piece, and
    /// its longest line when only forced breaks end lines.
    pub(crate) fn measure_inline(&mut self, first: NodeId, count: usize) -> (f32, f32) {
        let saved_items = std::mem::take(&mut self.items);
        let saved_words = std::mem::take(&mut self.words);
        let was_measuring = std::mem::replace(&mut self.measuring, true);
        let dom = self.dom;
        for node in dom.siblings_from(first).take(count) {
            self.collect(node);
        }
        let (mut min, mut max, mut line, mut piece) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut prev: Option<Item> = None;
        for &item in &self.items {
            if let Item::Clear(_) = item {
                continue;
            }
            if let Item::Break = item {
                max = max.max(line);
                line = 0.0;
                piece = 0.0;
                prev = None;
                continue;
            }
            let gap = prev.as_ref().map_or(0.0, space_after);
            let width = item_width(&item);
            let unbreakable = match item {
                Item::Atomic { min, .. } => min,
                _ => width,
            };
            line += gap + width;
            piece = match prev {
                Some(p) if !breakable(&p) => piece + gap + width,
                _ => unbreakable,
            };
            min = min.max(piece);
            prev = Some(item);
        }
        max = max.max(line);
        self.items = saved_items;
        self.words = saved_words;
        self.measuring = was_measuring;
        (min, max)
    }
}

/// Resolve flexible lengths (CSS Flexbox 9.7): grow or shrink unfrozen items toward the free
/// space, clamp them to min/max, freeze the violators, and repeat.
fn resolve_flexible(items: &mut [FlexItem], available: f32) {
    let hypothetical: f32 = items.iter().map(|i| i.outer(i.clamp(i.base))).sum();
    let growing = hypothetical < available;
    for item in items.iter_mut() {
        item.size = item.clamp(item.base);
        let factor = if growing { item.grow } else { item.shrink };
        item.frozen = factor == 0.0
            || (growing && item.base > item.size)
            || (!growing && item.base < item.size);
    }
    // Every round freezes at least one item.
    for _ in 0..=items.len() {
        if items.iter().all(|i| i.frozen) {
            break;
        }
        let used: f32 = items
            .iter()
            .map(|i| i.outer(if i.frozen { i.size } else { i.base }))
            .sum();
        let mut free = available - used;
        let factors: f32 = items
            .iter()
            .filter(|i| !i.frozen)
            .map(|i| if growing { i.grow } else { i.shrink })
            .sum();
        // Factors summing below 1 take only that fraction of the free space.
        if factors < 1.0 {
            free *= factors;
        }
        let scaled: f32 = items
            .iter()
            .filter(|i| !i.frozen)
            .map(|i| i.shrink * i.base)
            .sum();
        let mut total = 0.0;
        for item in items.iter_mut().filter(|i| !i.frozen) {
            let target = if growing {
                item.base + free * item.grow / factors
            } else if scaled > 0.0 {
                item.base + free * item.shrink * item.base / scaled
            } else {
                item.base
            };
            let clamped = item.clamp(target).max(0.0);
            item.violation = clamped - target;
            item.size = clamped;
            total += item.violation;
        }
        for item in items.iter_mut().filter(|i| !i.frozen) {
            item.frozen = total.abs() < 0.01
                || (total > 0.0 && item.violation > 0.0)
                || (total < 0.0 && item.violation < 0.0);
        }
    }
}

/// Main-axis offsets of each item's border box from the start of the line: auto margins take
/// positive free space first, then `justify-content` distributes it.
fn main_positions(items: &[FlexItem], main: f32, gap: f32, justify: Justify) -> Vec<f32> {
    let n = items.len() as f32;
    let used: f32 = items.iter().map(|i| i.outer(i.size)).sum::<f32>() + gap * (n - 1.0);
    let free = main - used;
    let autos: usize = items.iter().map(FlexItem::auto_margins).sum();
    let auto_share = if autos > 0 && free > 0.0 {
        free / autos as f32
    } else {
        0.0
    };
    let (start, between) = if autos > 0 && free > 0.0 {
        (0.0, 0.0)
    } else {
        match justify {
            Justify::Start => (0.0, 0.0),
            Justify::End => (free, 0.0),
            Justify::Center => (free / 2.0, 0.0),
            // Negative free space falls back to start (between) or center (around, evenly).
            Justify::SpaceBetween if free > 0.0 && n > 1.0 => (0.0, free / (n - 1.0)),
            Justify::SpaceAround if free > 0.0 => (free / n / 2.0, free / n),
            Justify::SpaceEvenly if free > 0.0 => (free / (n + 1.0), free / (n + 1.0)),
            Justify::SpaceAround | Justify::SpaceEvenly => (free / 2.0, 0.0),
            Justify::SpaceBetween => (0.0, 0.0),
        }
    };
    let mut pos = start;
    items
        .iter()
        .map(|item| {
            let [m0, m1] = item.margin.map(|m| m.unwrap_or(auto_share));
            let at = pos + m0;
            pos = at + item.size + item.extra + m1 + gap + between;
            at
        })
        .collect()
}
