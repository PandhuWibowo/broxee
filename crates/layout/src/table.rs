//! Table layout (CSS 2.1 section 17): automatic column widths and separated borders.
//!
//! Columns are sized from the cells' intrinsic widths (no layout). Every cell is then laid out
//! once at its column width; row heights come from the tallest cells (row-spanning cells
//! included), and cells are moved into their rows, stretched to the row height and their
//! content aligned with `vertical-align`.
//!
//! Supported: captions (`caption-side` top or bottom), `thead` first and `tfoot` last, rows outside row groups,
//! cells outside rows (wrapped in anonymous rows), non-table wrappers such as `<form>` around
//! rows, `colspan`/`rowspan`, px and % cell widths, `border-spacing`, row backgrounds.
//!
//! Not yet: `table-layout: fixed`, collapsing borders (`border-collapse: collapse` only
//! removes the spacing), `<col>` widths, baseline alignment (laid out as top).

use super::*;

/// Spans and grids larger than this are clamped.
const MAX_SPAN: usize = 1000;

struct Cell {
    node: NodeId,
    row: usize,
    column: usize,
    rows: usize,
    columns: usize,
}

struct TableGrid {
    captions: Vec<NodeId>,
    /// Each row's element, if it has one (anonymous rows have none).
    rows: Vec<Option<NodeId>>,
    cells: Vec<Cell>,
    columns: usize,
}

/// One cell after layout.
struct Placed {
    boxes: Range<usize>,
    texts: Range<usize>,
    clips: Range<usize>,
    height: f32,
    laid_at: f32,
}

impl Engine<'_> {
    /// Lay out table `table`, whose content box starts at (x, y) and is `width` wide. Returns
    /// its content height.
    pub(crate) fn layout_table(&mut self, table: NodeId, x: f32, y: f32, width: f32) -> f32 {
        let grid = self.table_grid(table);
        let s = self.style(table);
        let (hs, vs) = spacing(s);
        let mut cursor = y;
        let outer_centering =
            std::mem::replace(&mut self.centering, s.text_align == TextAlign::WebkitCenter);
        let (top_captions, bottom_captions): (Vec<NodeId>, Vec<NodeId>) = grid
            .captions
            .iter()
            .partition(|&&c| !self.style(c).caption_bottom);
        for &caption in &top_captions {
            let (bottom, margin) = self.layout_block(caption, x, cursor, 0.0, width, None);
            cursor = bottom + margin;
        }
        self.centering = outer_centering;
        if grid.cells.is_empty() {
            return self.bottom_captions(&bottom_captions, x, cursor, width) - y;
        }

        let (min, max, percent) = self.column_widths(&grid, hs);
        let widths = distribute(&min, &max, &percent, width - hs * (grid.columns + 1) as f32);
        let mut column_x = Vec::with_capacity(grid.columns + 1);
        let mut at = x + hs;
        for w in &widths {
            column_x.push(at);
            at += w + hs;
        }
        column_x.push(at);
        let span_width = |c: &Cell| {
            widths[c.column..c.column + c.columns].iter().sum::<f32>() + hs * (c.columns - 1) as f32
        };

        // Lay out every cell at the top of its row, recording natural heights.
        let rows_top = cursor + vs;
        let mut row_boxes = vec![None; grid.rows.len()];
        let mut placed: Vec<Placed> = Vec::with_capacity(grid.cells.len());
        let mut next_cell = 0;
        let mut heights = vec![0f32; grid.rows.len()];
        for r in 0..grid.rows.len() {
            if let Some(row) = grid.rows[r] {
                // The row's box goes first so its background paints under the cells.
                row_boxes[r] = Some(self.out.boxes.len());
                self.out.boxes.push(BoxFragment {
                    node: row,
                    rect: Rect::default(),
                });
                let rs = self.style(row);
                if let Some(h) = content_size(rs, rs.height, None, 0.0) {
                    heights[r] = h;
                }
            }
            while next_cell < grid.cells.len() && grid.cells[next_cell].row == r {
                let cell = &grid.cells[next_cell];
                let cs = self.style(cell.node);
                let w = span_width(cell);
                let content = (w - Edges::of(cs, width).x()).max(0.0);
                let start = (
                    self.out.boxes.len(),
                    self.out.texts.len(),
                    self.out.clips.len(),
                );
                let height = self.build_box(
                    cell.node,
                    column_x[cell.column],
                    rows_top,
                    content,
                    width,
                    None,
                    None,
                );
                placed.push(Placed {
                    boxes: start.0..self.out.boxes.len(),
                    texts: start.1..self.out.texts.len(),
                    clips: start.2..self.out.clips.len(),
                    height,
                    laid_at: rows_top,
                });
                if cell.rows == 1 {
                    heights[r] = heights[r].max(height);
                }
                next_cell += 1;
            }
        }
        // Row-spanning cells give their spanned rows what they lack, evenly.
        for (cell, p) in grid.cells.iter().zip(&placed) {
            if cell.rows > 1 {
                let spanned = cell.row..cell.row + cell.rows;
                let have: f32 =
                    heights[spanned.clone()].iter().sum::<f32>() + vs * (cell.rows - 1) as f32;
                let extra = (p.height - have).max(0.0) / cell.rows as f32;
                for h in &mut heights[spanned] {
                    *h += extra;
                }
            }
        }
        let mut row_top = Vec::with_capacity(grid.rows.len());
        let mut at = rows_top;
        for h in &heights {
            row_top.push(at);
            at += h + vs;
        }

        for (r, index) in row_boxes.into_iter().enumerate() {
            if let Some(i) = index {
                self.out.boxes[i].rect = Rect {
                    x: x + hs,
                    y: row_top[r],
                    width: (width - 2.0 * hs).max(0.0),
                    height: heights[r],
                };
            }
        }
        for (cell, p) in grid.cells.iter().zip(&placed) {
            let cell_height = heights[cell.row..cell.row + cell.rows].iter().sum::<f32>()
                + vs * (cell.rows - 1) as f32;
            let free = (cell_height - p.height).max(0.0);
            let inside = match self.style(cell.node).vertical_align {
                VAlign::Middle => free / 2.0,
                VAlign::Bottom => free,
                VAlign::Top | VAlign::Baseline => 0.0,
            };
            let dy = row_top[cell.row] - p.laid_at;
            // Content moves by the row offset plus the alignment; the cell's own box and clip
            // only by the row offset, then stretch to the row.
            self.shift_range(
                p.boxes.clone(),
                p.texts.clone(),
                p.clips.clone(),
                0.0,
                dy + inside,
            );
            let own = p.boxes.start;
            self.out.boxes[own].rect.y -= inside;
            for c in p.clips.clone() {
                if self.out.clips[c].boxes.0 as usize == own + 1 {
                    self.out.clips[c].rect.y -= inside;
                }
            }
            self.set_box_height(own, p.clips.clone(), cell_height);
        }
        self.bottom_captions(&bottom_captions, x, at, width) - y
    }

    /// Lay out `caption-side: bottom` captions from `y`; returns where they end.
    fn bottom_captions(&mut self, captions: &[NodeId], x: f32, mut y: f32, width: f32) -> f32 {
        for &caption in captions {
            let (bottom, margin) = self.layout_block(caption, x, y, 0.0, width, None);
            y = bottom + margin;
        }
        y
    }

    /// Min-content and max-content widths of a table's content box: its columns plus spacing.
    pub(crate) fn table_intrinsic(&mut self, table: NodeId) -> (f32, f32) {
        let grid = self.table_grid(table);
        let (hs, _) = spacing(self.style(table));
        let (min, max, _) = self.column_widths(&grid, hs);
        let gaps = hs * (grid.columns + 1) as f32;
        let mut widths = (
            min.iter().sum::<f32>() + gaps,
            max.iter().sum::<f32>() + gaps,
        );
        for &caption in &grid.captions {
            let (a, b) = self.outer_intrinsic(caption);
            widths = (widths.0.max(a), widths.1.max(b));
        }
        if grid.columns == 0 {
            widths = (widths.0 - gaps, widths.1 - gaps);
        }
        widths
    }

    /// Per column: min-content and max-content widths (border boxes of the cells) and the
    /// largest percentage asked for.
    /// `hs` is the table's horizontal spacing, which spanning cells cover.
    fn column_widths(&mut self, grid: &TableGrid, hs: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = grid.columns;
        let (mut min, mut max, mut percent) = (vec![0f32; n], vec![0f32; n], vec![0f32; n]);
        let mut spanning = Vec::new();
        for cell in &grid.cells {
            let (a, mut b) = self.intrinsic_width(cell.node);
            let cs = self.style(cell.node);
            let mut pct = 0.0;
            match cs.width {
                Length::Percent(p) => pct = p as f32 / 10_000.0,
                Length::Px(_) => {
                    // A fixed width is the preferred width; content still sets the minimum.
                    let extra = Edges::of(cs, 0.0).x();
                    if let Some(w) = content_size(cs, cs.width, None, extra) {
                        b = (w + extra).max(a);
                    }
                }
                Length::Auto => {}
            }
            if cell.columns == 1 {
                let c = cell.column;
                min[c] = min[c].max(a);
                max[c] = max[c].max(b);
                percent[c] = percent[c].max(pct);
            } else {
                spanning.push((cell.column..cell.column + cell.columns, a, b));
            }
        }
        // Spanning cells: spread what the spanned columns lack, by their current share.
        spanning.sort_by_key(|s| s.0.len());
        for (range, a, b) in spanning {
            let gaps = hs * (range.len() - 1) as f32;
            for (widths, want) in [(&mut min, a), (&mut max, b)] {
                let have: f32 = widths[range.clone()].iter().sum::<f32>() + gaps;
                if want > have {
                    let extra = want - have;
                    let total: f32 = widths[range.clone()].iter().sum();
                    for c in range.clone() {
                        let share = if total > 0.0 {
                            widths[c] / total
                        } else {
                            1.0 / range.len() as f32
                        };
                        widths[c] += extra * share;
                    }
                }
            }
        }
        for c in 0..n {
            max[c] = max[c].max(min[c]);
        }
        (min, max, percent)
    }

    /// The table's captions, rows and cells, with every cell placed in the grid.
    fn table_grid(&self, table: NodeId) -> TableGrid {
        let mut captions = Vec::new();
        let (mut head, mut body, mut foot) = (Vec::new(), Vec::new(), Vec::new());
        self.collect_rows(table, true, &mut captions, &mut head, &mut body, &mut foot);
        let rows: Vec<(Option<NodeId>, Vec<NodeId>)> =
            head.into_iter().chain(body).chain(foot).collect();

        let mut occupied: Vec<Vec<bool>> = vec![Vec::new(); rows.len()];
        let mut cells = Vec::new();
        let mut columns = 0;
        for (r, (_, row_cells)) in rows.iter().enumerate() {
            let mut c = 0;
            for &node in row_cells {
                while occupied[r].get(c).copied().unwrap_or(false) {
                    c += 1;
                }
                let span = |name: &str, zero: usize| {
                    self.dom
                        .attr(node, name)
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .map_or(1, |n| if n == 0 { zero } else { n })
                        .clamp(1, MAX_SPAN)
                };
                let colspan = span("colspan", 1);
                // `rowspan=0` reaches the end of the table.
                let rowspan = span("rowspan", rows.len() - r).min(rows.len() - r);
                if c + colspan > MAX_SPAN {
                    break;
                }
                for occupied_row in &mut occupied[r..r + rowspan] {
                    if occupied_row.len() < c + colspan {
                        occupied_row.resize(c + colspan, false);
                    }
                    occupied_row[c..c + colspan].fill(true);
                }
                cells.push(Cell {
                    node,
                    row: r,
                    column: c,
                    rows: rowspan,
                    columns: colspan,
                });
                columns = columns.max(c + colspan);
                c += colspan;
            }
        }
        TableGrid {
            captions,
            rows: rows.into_iter().map(|(row, _)| row).collect(),
            cells,
            columns,
        }
    }

    /// Gather rows below `parent`: row groups are entered, rows taken, loose cells wrapped in
    /// anonymous rows, and other elements (a `<form>` around rows) looked through.
    fn collect_rows(
        &self,
        parent: NodeId,
        in_table: bool,
        captions: &mut Vec<NodeId>,
        head: &mut Vec<(Option<NodeId>, Vec<NodeId>)>,
        body: &mut Vec<(Option<NodeId>, Vec<NodeId>)>,
        foot: &mut Vec<(Option<NodeId>, Vec<NodeId>)>,
    ) {
        let dom = self.dom;
        let mut loose: Vec<NodeId> = Vec::new();
        for child in dom.children(parent) {
            if dom.element(child).is_none() {
                continue;
            }
            let s = self.style(child);
            let display = s.display;
            // Content that is not a table part sits in an anonymous cell (CSS 2.1 17.2.1), like
            // a figure's image when the figure is `display: table`. Wrappers around rows (a
            // `<form>` around `<tr>`s) are looked through instead.
            let in_cell = display == Display::TableCell
                || (!is_table_part(display) && !self.holds_table_parts(child));
            if !in_cell && !loose.is_empty() {
                body.push((None, std::mem::take(&mut loose)));
            }
            match display {
                Display::None | Display::TableColumn => {}
                _ if s.position.is_out_of_flow() => {}
                Display::TableCaption if in_table => captions.push(child),
                Display::TableHeaderGroup => {
                    let mut rows = Vec::new();
                    self.collect_rows(
                        child,
                        false,
                        captions,
                        &mut Vec::new(),
                        &mut rows,
                        &mut Vec::new(),
                    );
                    head.extend(rows);
                }
                Display::TableFooterGroup => {
                    let mut rows = Vec::new();
                    self.collect_rows(
                        child,
                        false,
                        captions,
                        &mut Vec::new(),
                        &mut rows,
                        &mut Vec::new(),
                    );
                    foot.extend(rows);
                }
                Display::TableRowGroup => {
                    self.collect_rows(child, false, captions, head, body, foot)
                }
                Display::TableRow => {
                    let mut cells = Vec::new();
                    self.row_cells(child, &mut cells);
                    body.push((Some(child), cells));
                }
                Display::TableCell => loose.push(child),
                _ if in_cell => loose.push(child),
                _ => self.collect_rows(child, in_table, captions, head, body, foot),
            }
        }
        if !loose.is_empty() {
            body.push((None, loose));
        }
    }

    /// The cells of a row in document order, looking through non-table wrappers.
    fn row_cells(&self, parent: NodeId, cells: &mut Vec<NodeId>) {
        let dom = self.dom;
        for child in dom.children(parent) {
            if dom.element(child).is_none() {
                continue;
            }
            let s = self.style(child);
            match s.display {
                Display::TableCell if !s.position.is_out_of_flow() => cells.push(child),
                Display::None | Display::TableColumn | Display::TableRow => {}
                _ if s.position.is_out_of_flow() => {}
                // Non-cell content in a row is wrapped in an anonymous cell; wrappers around
                // cells are looked through.
                _ if !self.holds_table_parts(child) => cells.push(child),
                _ => self.row_cells(child, cells),
            }
        }
    }
}

fn is_table_part(display: Display) -> bool {
    matches!(
        display,
        Display::TableRow
            | Display::TableCell
            | Display::TableCaption
            | Display::TableColumn
            | Display::TableRowGroup
            | Display::TableHeaderGroup
            | Display::TableFooterGroup
    )
}

impl Engine<'_> {
    /// Whether an element directly contains rows, cells or row groups.
    fn holds_table_parts(&self, id: NodeId) -> bool {
        self.dom
            .children(id)
            .any(|c| self.dom.element(c).is_some() && is_table_part(self.style(c).display))
    }
}

/// (horizontal, vertical) spacing between cells and around them.
fn spacing(s: &ComputedStyle) -> (f32, f32) {
    if s.border_collapse {
        (0.0, 0.0)
    } else {
        (s.border_spacing[0].to_f32(), s.border_spacing[1].to_f32())
    }
}

/// Column widths for `available` px (CSS 2.1 17.5.2.2, as browsers implement it): percent
/// columns take their share first; the rest get their max-content widths and share any surplus
/// by those widths, or, when space is short, the minimum plus a share of the gap up to their
/// maximum.
fn distribute(min: &[f32], max: &[f32], percent: &[f32], available: f32) -> Vec<f32> {
    let n = min.len();
    let available = available.max(0.0);
    let mut widths = vec![0f32; n];
    let mut rest = available;
    for c in 0..n {
        if percent[c] > 0.0 {
            widths[c] = (percent[c] * available).max(min[c]);
            rest -= widths[c];
        }
    }
    let auto: Vec<usize> = (0..n).filter(|&c| percent[c] == 0.0).collect();
    let sum_min: f32 = auto.iter().map(|&c| min[c]).sum();
    let sum_max: f32 = auto.iter().map(|&c| max[c]).sum();
    if rest >= sum_max {
        let surplus = rest - sum_max;
        for &c in &auto {
            let share = if sum_max > 0.0 {
                max[c] / sum_max
            } else {
                1.0 / auto.len() as f32
            };
            widths[c] = max[c] + surplus * share;
        }
    } else if rest > sum_min && sum_max > sum_min {
        let t = (rest - sum_min) / (sum_max - sum_min);
        for &c in &auto {
            widths[c] = min[c] + (max[c] - min[c]) * t;
        }
    } else {
        for &c in &auto {
            widths[c] = min[c];
        }
    }
    widths
}
