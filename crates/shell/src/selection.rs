//! Selecting text in a page, copying it, and finding text (⌘F), over the laid-out text runs.
//!
//! A position is a text node and a character offset into that node's runs joined by one
//! space each. Wrapping only moves where runs break (always at spaces), so a position keeps
//! meaning the same character when the page is laid out again at another width. Runs of one
//! node are consecutive in `Layout::texts`.

use css::{Display, StyleMap};
use font::TextMetrics;
use html::{Dom, NodeId};
use layout::Layout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub node: NodeId,
    pub offset: usize,
}

/// What is selected: from where the press started to where the pointer is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub focus: Pos,
}

/// A rectangle to mark, in page points; `fixed` when it scrolls with the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub fixed: bool,
}

fn chars(layout: &Layout, i: usize) -> usize {
    layout.text(&layout.texts[i]).chars().count()
}

/// The first run of the node run `i` belongs to.
fn first_run(layout: &Layout, i: usize) -> usize {
    let node = layout.texts[i].node;
    let mut j = i;
    while j > 0 && layout.texts[j - 1].node == node {
        j -= 1;
    }
    j
}

/// The position of character `c` of run `i`.
pub fn pos_of(layout: &Layout, i: usize, c: usize) -> Pos {
    let mut offset = 0;
    for k in first_run(layout, i)..i {
        offset += chars(layout, k) + 1;
    }
    Pos {
        node: layout.texts[i].node,
        offset: offset + c,
    }
}

/// The run and character `pos` falls on (the end of a run for a joining space).
pub fn locate(layout: &Layout, pos: Pos) -> Option<(usize, usize)> {
    let first = (0..layout.texts.len()).find(|&i| layout.texts[i].node == pos.node)?;
    let mut left = pos.offset;
    let mut i = first;
    loop {
        let n = chars(layout, i);
        if left <= n {
            return Some((i, left));
        }
        if i + 1 >= layout.texts.len() || layout.texts[i + 1].node != pos.node {
            return Some((i, n));
        }
        left -= n + 1;
        i += 1;
    }
}

/// The two ends of `sel` in document order, as (run, char).
fn ends(layout: &Layout, sel: Selection) -> Option<((usize, usize), (usize, usize))> {
    let a = locate(layout, sel.anchor)?;
    let b = locate(layout, sel.focus)?;
    Some(if a <= b { (a, b) } else { (b, a) })
}

/// x of each character edge of run `i`: `len + 1` values from its left to its right edge.
fn edges(layout: &Layout, styles: &StyleMap, metrics: &dyn TextMetrics, i: usize) -> Vec<f32> {
    let t = &layout.texts[i];
    let style = styles.style(t.node);
    let size = style.font_size.to_f32();
    let key = layout::font_key(style);
    let mut out = vec![0.0];
    let mut pen = 0.0;
    for ch in layout.text(t).chars() {
        pen += metrics.advance_em(ch, key) * size;
        out.push(pen);
    }
    // Layout measured the run as a whole; scale onto its width so the ends meet it.
    let scale = if pen > 0.0 { t.width / pen } else { 0.0 };
    out.iter().map(|e| t.x + e * scale).collect()
}

/// The line box of run `i`: top and height.
fn line(layout: &Layout, styles: &StyleMap, metrics: &dyn TextMetrics, i: usize) -> (f32, f32) {
    let t = &layout.texts[i];
    let style = styles.style(t.node);
    let size = style.font_size.to_f32();
    let key = layout::font_key(style);
    let ascent = metrics.ascent_em(key) * size;
    let descent = metrics.descent_em(key) * size;
    (t.baseline - ascent, ascent + descent)
}

/// The text position nearest (x, y) in page points; runs in fixed layers are left out.
pub fn hit(
    layout: &Layout,
    styles: &StyleMap,
    metrics: &dyn TextMetrics,
    x: f32,
    y: f32,
) -> Option<Pos> {
    let mut best: Option<(f32, f32, usize)> = None;
    let near = Layout::in_view(
        &layout.text_extents,
        0..layout.texts.len(),
        y - 400.0,
        y + 400.0,
    );
    for i in near {
        if layout.text_is_fixed(i) {
            continue;
        }
        let t = &layout.texts[i];
        let (top, height) = line(layout, styles, metrics, i);
        let dy = if y < top {
            top - y
        } else if y > top + height {
            y - top - height
        } else {
            0.0
        };
        let dx = if x < t.x {
            t.x - x
        } else if x > t.x + t.width {
            x - t.x - t.width
        } else {
            0.0
        };
        if best.is_none_or(|(by, bx, _)| (dy, dx) < (by, bx)) {
            best = Some((dy, dx, i));
        }
    }
    let (_, _, i) = best?;
    let edges = edges(layout, styles, metrics, i);
    // The nearest character edge.
    let c = (0..edges.len())
        .min_by(|&a, &b| {
            (edges[a] - x)
                .abs()
                .partial_cmp(&(edges[b] - x).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    Some(pos_of(layout, i, c))
}

/// The marks that draw `sel`: one per run it covers.
pub fn marks(
    layout: &Layout,
    styles: &StyleMap,
    metrics: &dyn TextMetrics,
    sel: Selection,
) -> Vec<Mark> {
    let Some(((r0, c0), (r1, c1))) = ends(layout, sel) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in r0..=r1 {
        let edges = edges(layout, styles, metrics, i);
        let from = if i == r0 { c0 } else { 0 };
        let to = if i == r1 { c1 } else { edges.len() - 1 };
        if to <= from {
            continue;
        }
        let (top, height) = line(layout, styles, metrics, i);
        out.push(Mark {
            x: edges[from],
            y: top,
            width: edges[to] - edges[from],
            height,
            fixed: layout.text_is_fixed(i),
        });
    }
    out
}

/// The block an inline node sits in: what separates paragraphs when copying.
fn block_of(dom: &Dom, styles: &StyleMap, node: NodeId) -> NodeId {
    let mut at = dom.parent(node);
    while let Some(n) = at {
        if dom.element(n).is_some() && styles.style(n).display != Display::Inline {
            return n;
        }
        at = dom.parent(n);
    }
    Dom::ROOT
}

/// What goes between runs `a` and `b` (consecutive) in copied or searched text.
fn between(layout: &Layout, dom: &Dom, styles: &StyleMap, a: usize, b: usize) -> &'static str {
    let (p, t) = (&layout.texts[a], &layout.texts[b]);
    if (p.baseline - t.baseline).abs() < 0.5 && t.x >= p.x {
        // The same line: a gap was a space; touching runs are one word split by markup.
        return if t.x > p.x + p.width + 0.5 { " " } else { "" };
    }
    if block_of(dom, styles, p.node) != block_of(dom, styles, t.node) {
        "\n"
    } else {
        " "
    }
}

/// The selected text, with line breaks between blocks.
pub fn text(layout: &Layout, dom: &Dom, styles: &StyleMap, sel: Selection) -> String {
    let Some(((r0, c0), (r1, c1))) = ends(layout, sel) else {
        return String::new();
    };
    let mut out = String::new();
    for i in r0..=r1 {
        if i > r0 {
            out.push_str(between(layout, dom, styles, i - 1, i));
        }
        let run = layout.text(&layout.texts[i]);
        let n = run.chars().count();
        let from = if i == r0 { c0 } else { 0 };
        let to = if i == r1 { c1 } else { n };
        out.extend(run.chars().skip(from).take(to.saturating_sub(from)));
    }
    out
}

/// The word around `pos` (letters, digits and `_`), or the character there.
pub fn word(layout: &Layout, pos: Pos) -> Option<Selection> {
    let (i, c) = locate(layout, pos)?;
    let run: Vec<char> = layout.text(&layout.texts[i]).chars().collect();
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let at = c.min(run.len().saturating_sub(1));
    if run.is_empty() {
        return None;
    }
    let (mut from, mut to) = (at, at + 1);
    if is_word(run[at]) {
        while from > 0 && is_word(run[from - 1]) {
            from -= 1;
        }
        while to < run.len() && is_word(run[to]) {
            to += 1;
        }
    }
    Some(Selection {
        anchor: pos_of(layout, i, from),
        focus: pos_of(layout, i, to),
    })
}

/// The runs of the block around `pos`: a paragraph.
pub fn paragraph(layout: &Layout, dom: &Dom, styles: &StyleMap, pos: Pos) -> Option<Selection> {
    let (i, _) = locate(layout, pos)?;
    let block = block_of(dom, styles, layout.texts[i].node);
    let same = |k: usize| block_of(dom, styles, layout.texts[k].node) == block;
    let mut from = i;
    while from > 0 && same(from - 1) {
        from -= 1;
    }
    let mut to = i;
    while to + 1 < layout.texts.len() && same(to + 1) {
        to += 1;
    }
    Some(Selection {
        anchor: pos_of(layout, from, 0),
        focus: pos_of(layout, to, chars(layout, to)),
    })
}

/// Everything: from the first run to the end of the last.
pub fn all(layout: &Layout) -> Option<Selection> {
    let last = layout.texts.len().checked_sub(1)?;
    Some(Selection {
        anchor: pos_of(layout, 0, 0),
        focus: pos_of(layout, last, chars(layout, last)),
    })
}

/// Where `query` appears in the page, case-insensitive, across runs and inline elements, in
/// document order. At most `limit`.
pub fn find(
    layout: &Layout,
    dom: &Dom,
    styles: &StyleMap,
    query: &str,
    limit: usize,
) -> Vec<Selection> {
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let needle: Vec<char> = query.chars().map(fold).collect();
    if needle.is_empty() || layout.texts.is_empty() {
        return Vec::new();
    }
    // The page's text in one stream (blocks joined by a space: a match may not cross them,
    // but a space may), with where each run starts in it.
    let mut stream: Vec<char> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(layout.texts.len());
    for i in 0..layout.texts.len() {
        if i > 0 {
            let sep = between(layout, dom, styles, i - 1, i);
            stream.extend(sep.chars().map(|c| if c == '\n' { '\u{0}' } else { c }));
        }
        starts.push(stream.len());
        stream.extend(layout.text(&layout.texts[i]).chars().map(fold));
    }
    // A stream index as (run, char): the run starting at or before it.
    let at = |k: usize| {
        let run = starts.partition_point(|&s| s <= k).saturating_sub(1);
        (run, k - starts[run])
    };
    let mut out = Vec::new();
    let mut k = 0;
    while k + needle.len() <= stream.len() && out.len() < limit {
        if stream[k..k + needle.len()] == needle[..] {
            let (r0, c0) = at(k);
            let (r1, c1) = at(k + needle.len() - 1);
            out.push(Selection {
                anchor: pos_of(layout, r0, c0),
                focus: pos_of(layout, r1, c1 + 1),
            });
            k += needle.len();
        } else {
            k += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use css::{cascade, parse_stylesheet, user_agent_stylesheet, Media, Origin};
    use font::Approximate;

    fn page(src: &str, width: f32) -> (Dom, StyleMap, Layout) {
        let mut dom = html::parse(src);
        let author = parse_stylesheet("body { margin: 0 } p { margin: 0 0 10px }");
        let media = Media {
            width,
            height: 400.0,
            ..Media::default()
        };
        let mut styles = cascade(
            &dom,
            &[
                (Origin::UserAgent, user_agent_stylesheet()),
                (Origin::Author, &author),
            ],
            &media,
        );
        styles.attach_generated(&mut dom);
        let l = layout::layout(&dom, &styles, &Approximate, &|_| None, width, 400.0);
        (dom, styles, l)
    }

    #[test]
    fn positions_survive_rewrapping() {
        let src = "<p>one two three four five six seven</p>";
        let (dom, styles, wide) = page(src, 800.0);
        let (_, _, narrow) = page(src, 60.0);
        assert!(
            narrow.texts.len() > wide.texts.len(),
            "the narrow page wraps"
        );
        let sel = Selection {
            anchor: Pos {
                node: wide.texts[0].node,
                offset: 4,
            },
            focus: Pos {
                node: wide.texts[0].node,
                offset: 18,
            },
        };
        assert_eq!(text(&wide, &dom, &styles, sel), "two three four");
        assert_eq!(text(&narrow, &dom, &styles, sel), "two three four");
    }

    #[test]
    fn selects_copies_and_finds_across_blocks_and_inline_elements() {
        let (dom, styles, l) = page(
            "<p>first <b>bold</b> line</p><p>second line here</p>",
            800.0,
        );
        // From the start of the first paragraph to the middle of the second.
        let start = hit(&l, &styles, &Approximate, 0.0, 5.0).unwrap();
        let second = l
            .texts
            .iter()
            .position(|t| l.text(t).starts_with("second"))
            .unwrap();
        let t = &l.texts[second];
        let end = hit(
            &l,
            &styles,
            &Approximate,
            t.x + 4.0 * 8.0 + 1.0,
            t.baseline - 3.0,
        )
        .unwrap();
        let sel = Selection {
            anchor: start,
            focus: end,
        };
        assert_eq!(text(&l, &dom, &styles, sel), "first bold line\nseco");
        let marks = marks(&l, &styles, &Approximate, sel);
        assert_eq!(marks.len(), 4, "one per run");
        let found = find(&l, &dom, &styles, "LINE", 10);
        assert_eq!(found.len(), 2);
        assert_eq!(text(&l, &dom, &styles, found[1]), "line");
        assert_eq!(
            find(&l, &dom, &styles, "bold line", 10).len(),
            1,
            "across an element"
        );
        assert_eq!(
            find(&l, &dom, &styles, "line second", 10).len(),
            0,
            "not across paragraphs"
        );
        let w = word(&l, end).unwrap();
        assert_eq!(text(&l, &dom, &styles, w), "second");
        let p = paragraph(&l, &dom, &styles, start).unwrap();
        assert_eq!(text(&l, &dom, &styles, p), "first bold line");
        assert_eq!(
            text(&l, &dom, &styles, all(&l).unwrap()),
            "first bold line\nsecond line here"
        );
    }
}
