//! Laying out again after the DOM changed, inside one box instead of the whole page.
//!
//! Layout records anchors: boxes that clip their overflow, sit in normal flow and were built
//! where they are, with the arguments they were built from. When only what is inside such a
//! box changed, it is built again with the same arguments; if its size (and, for an inline
//! block, its baseline) comes out the same, nothing outside it can move, so its fragments
//! replace the old ones and every index after them shifts. Otherwise the next anchor up is
//! tried, and the caller lays out the whole page when none fits.
//!
//! The result is the layout a full pass would give: a box is built from the same arguments,
//! clipped content never reaches the scrollable height, and boxes whose position or size
//! depends on their content (flex and grid items, table cells, floats, positioned and
//! shrink-to-fit boxes) are never anchors or the ancestors of one that is used.

use css::{ComputedStyle, Display, Float, Position, StyleMap};
use font::TextMetrics;
use html::{Dom, NodeId};

use crate::{Around, Engine, Layout, NaturalSize, Place, TextFragment, CHUNK};

impl Layout {
    /// Lay out again after what is inside each of `changed` changed: the children of an
    /// element, or the text or style of a node (pass its parent). Styles must be those of
    /// the changed DOM, and equal to the old ones outside the changed nodes. Returns false
    /// when no box around the changes can be laid out alone; then lay out the whole page.
    pub fn relayout(
        &mut self,
        dom: &Dom,
        styles: &StyleMap,
        metrics: &dyn TextMetrics,
        natural: NaturalSize,
        changed: &[NodeId],
    ) -> bool {
        let Some(chain) = common_ancestors(dom, changed) else {
            return changed.is_empty();
        };
        for &node in &chain {
            let Some(a) = self.anchors.iter().position(|a| a.node == node) else {
                continue;
            };
            if matches!(self.anchors[a].place, Place::Moved | Place::Origin(_))
                || !in_plain_flow(dom, styles, node)
                || !self_contained(dom, styles, node)
            {
                continue;
            }
            if self.rebuild(a, dom, styles, metrics, natural) {
                return true;
            }
        }
        false
    }

    /// Build anchor `a` again; replace its fragments when its size came out the same.
    fn rebuild(
        &mut self,
        a: usize,
        dom: &Dom,
        styles: &StyleMap,
        metrics: &dyn TextMetrics,
        natural: NaturalSize,
    ) -> bool {
        let anchor = self.anchors[a];
        let mut engine = Engine::new(dom, styles, metrics, natural, self.width, self.height);
        match anchor.place {
            Place::Flow {
                x,
                y,
                content_width,
                cb_width,
                cb_height,
            } => {
                engine.build_box(anchor.node, x, y, content_width, cb_width, cb_height, None);
                if engine.out.boxes[0].rect != self.boxes[anchor.boxes.0 as usize].rect {
                    return false;
                }
            }
            Place::Line(old, dx, dy) => {
                engine.line_width = old.cb;
                let (min, max) = engine.intrinsic_width(anchor.node);
                let (new, (boxes, texts, clips)) = engine.build_inline_block(anchor.node, min, max);
                if new != old {
                    return false;
                }
                engine.shift_range(boxes, texts, clips, dx, dy);
            }
            Place::Origin(_) | Place::Moved => return false,
        }
        let out = &mut engine.out;
        // Whatever escaped the box (a layer, a deferred box) means it was not self-contained.
        if !engine.deferred.is_empty()
            || out.layers.len() != 1
            || out.anchors.first().map(|n| (n.node, n.boxes.0)) != Some((anchor.node, 0))
        {
            return false;
        }
        out.anchors[0].place = anchor.place;
        let new = std::mem::replace(&mut engine.out, Layout::empty());
        self.splice(a, new, styles);
        true
    }

    /// Put `new` (the fragments of anchor `a`, built alone) in place of the old ones.
    fn splice(&mut self, a: usize, mut new: Layout, styles: &StyleMap) {
        let old = self.anchors[a];
        let delta = |range: (u32, u32), len: usize| len as i64 - i64::from(range.1 - range.0);
        let d_boxes = delta(old.boxes, new.boxes.len());
        let d_texts = delta(old.texts, new.texts.len());
        let d_clips = delta(old.clips, new.clips.len());
        let d_groups = delta(old.groups, new.groups.len());
        let shift = |i: u32, d: i64| (i64::from(i) + d) as u32;
        // Entries outside the box: before it nothing moves; one that encloses it ends later;
        // after it everything moves. Box indices decide which (the box has at least one).
        let around = |boxes: (u32, u32)| {
            if boxes.1 <= old.boxes.0 {
                Around::Before
            } else if boxes.0 >= old.boxes.1 {
                Around::After
            } else {
                Around::Encloses
            }
        };
        let remap = |range: (u32, u32), d: i64, at: Around| match at {
            Around::Before => range,
            Around::Encloses => (range.0, shift(range.1, d)),
            Around::After => (shift(range.0, d), shift(range.1, d)),
        };

        let (b0, t0, c0, g0) = (old.boxes.0, old.texts.0, old.clips.0, old.groups.0);
        for c in &mut new.clips {
            c.boxes = (c.boxes.0 + b0, c.boxes.1 + b0);
            c.texts = (c.texts.0 + t0, c.texts.1 + t0);
        }
        for g in &mut new.groups {
            g.boxes = (g.boxes.0 + b0, g.boxes.1 + b0);
            g.texts = (g.texts.0 + t0, g.texts.1 + t0);
        }
        for n in &mut new.anchors {
            n.boxes = (n.boxes.0 + b0, n.boxes.1 + b0);
            n.texts = (n.texts.0 + t0, n.texts.1 + t0);
            n.clips = (n.clips.0 + c0, n.clips.1 + c0);
            n.groups = (n.groups.0 + g0, n.groups.1 + g0);
        }
        // Entries inside the box are remapped too, then replaced.
        for c in &mut self.clips {
            let at = around(c.boxes);
            c.boxes = remap(c.boxes, d_boxes, at);
            c.texts = remap(c.texts, d_texts, at);
        }
        for g in &mut self.groups {
            let at = around(g.boxes);
            g.boxes = remap(g.boxes, d_boxes, at);
            g.texts = remap(g.texts, d_texts, at);
        }
        let end = a + 1 + old.inner as usize;
        for n in &mut self.anchors {
            let at = around(n.boxes);
            n.boxes = remap(n.boxes, d_boxes, at);
            n.texts = remap(n.texts, d_texts, at);
            n.clips = remap(n.clips, d_clips, at);
            n.groups = remap(n.groups, d_groups, at);
        }
        for l in &mut self.layers {
            // A layer that starts after the box starts later; one starting at it stays.
            if l.boxes >= old.boxes.1 {
                l.boxes = shift(l.boxes, d_boxes);
                l.texts = shift(l.texts, d_texts);
            }
        }

        // Text: the new fragments' characters go at the end of the buffer.
        let base = self.text.len() as u32;
        self.text.push_str(&new.text);
        for i in old.texts.0..old.texts.1 {
            self.text_garbage += self.texts[i as usize].len as usize;
        }
        let texts: Vec<TextFragment> = new
            .texts
            .iter()
            .map(|t| TextFragment {
                start: t.start + base,
                ..*t
            })
            .collect();
        let new_boxes = new.boxes.len();
        let new_texts = texts.len();
        self.boxes
            .splice(old.boxes.0 as usize..old.boxes.1 as usize, new.boxes);
        self.texts
            .splice(old.texts.0 as usize..old.texts.1 as usize, texts);
        self.clips
            .splice(old.clips.0 as usize..old.clips.1 as usize, new.clips);
        self.groups
            .splice(old.groups.0 as usize..old.groups.1 as usize, new.groups);
        self.anchors.splice(a..end, new.anchors);
        if self.text_garbage > 4096 && self.text_garbage * 2 > self.text.len() {
            self.compact_text();
        }

        let boxes = b0 as usize..b0 as usize + new_boxes;
        let texts = t0 as usize..t0 as usize + new_texts;
        self.reindex(styles, boxes, d_boxes != 0, texts, d_texts != 0);
    }

    /// Rewrite the text buffer with only the characters fragments use.
    fn compact_text(&mut self) {
        let mut text = String::with_capacity(self.text.len() - self.text_garbage);
        for i in 0..self.texts.len() {
            let t = self.texts[i];
            let start = text.len() as u32;
            text.push_str(&self.text[t.start as usize..(t.start + t.len) as usize]);
            self.texts[i].start = start;
        }
        self.text = text;
        self.text_garbage = 0;
    }

    /// Recompute the extents of the chunks holding `boxes` and `texts`; from there to the end
    /// when the count changed (everything after moved to another chunk).
    fn reindex(
        &mut self,
        styles: &StyleMap,
        boxes: std::ops::Range<usize>,
        boxes_moved: bool,
        texts: std::ops::Range<usize>,
        texts_moved: bool,
    ) {
        let chunks = |range: &std::ops::Range<usize>, moved: bool, len: usize| {
            let first = range.start / CHUNK;
            let last = if moved {
                len.div_ceil(CHUNK)
            } else {
                range.end.max(range.start + 1).div_ceil(CHUNK)
            };
            first..last.min(len.div_ceil(CHUNK))
        };
        let box_chunks = chunks(&boxes, boxes_moved, self.boxes.len());
        self.box_extents
            .resize(self.boxes.len().div_ceil(CHUNK), (0.0, 0.0));
        for c in box_chunks {
            let run = &self.boxes[c * CHUNK..((c + 1) * CHUNK).min(self.boxes.len())];
            self.box_extents[c] = run.iter().fold((f32::MAX, f32::MIN), |(lo, hi), b| {
                (lo.min(b.rect.y), hi.max(b.rect.y + b.rect.height))
            });
        }
        let text_chunks = chunks(&texts, texts_moved, self.texts.len());
        self.text_extents
            .resize(self.texts.len().div_ceil(CHUNK), (0.0, 0.0));
        for c in text_chunks {
            let mut extent = (f32::MAX, f32::MIN);
            for i in c * CHUNK..((c + 1) * CHUNK).min(self.texts.len()) {
                let t = &self.texts[i];
                let size = styles.style(t.node).font_size.to_f32();
                extent = (
                    extent.0.min(t.baseline - size),
                    extent.1.max(t.baseline + size),
                );
            }
            self.text_extents[c] = extent;
        }
    }
}

/// The inclusive ancestors every node in `nodes` has, nearest first; `None` for no nodes or
/// a node outside the document.
fn common_ancestors(dom: &Dom, nodes: &[NodeId]) -> Option<Vec<NodeId>> {
    let (&first, rest) = nodes.split_first()?;
    let mut chain: Vec<NodeId> = std::iter::successors(Some(first), |&n| dom.parent(n)).collect();
    if chain.last() != Some(&Dom::ROOT) {
        return None;
    }
    for &node in rest {
        let mut at = Some(node);
        loop {
            match at {
                Some(n) => match chain.iter().position(|&c| c == n) {
                    Some(i) => {
                        chain.drain(..i);
                        break;
                    }
                    None => at = dom.parent(n),
                },
                None => return None,
            }
        }
    }
    Some(chain)
}

/// Whether every ancestor of `node` is a block container or inline box in normal flow: then
/// nothing about its place depends on its content.
fn in_plain_flow(dom: &Dom, styles: &StyleMap, node: NodeId) -> bool {
    std::iter::successors(dom.parent(node), |&n| dom.parent(n))
        .filter(|&n| dom.element(n).is_some())
        .all(|n| {
            let s = styles.style(n);
            matches!(
                s.display,
                Display::Block | Display::ListItem | Display::Inline
            ) && s.position == Position::Static
                && s.float == Float::None
        })
}

/// Whether nothing inside `node` is laid out outside it: no floats, no positioned boxes
/// that leave the flow or start a layer.
fn self_contained(dom: &Dom, styles: &StyleMap, node: NodeId) -> bool {
    let escapes = |s: &ComputedStyle| {
        s.float != Float::None
            || s.position.is_out_of_flow()
            || (s.position == Position::Relative && s.z_index.is_some())
    };
    dom.descendants(node)
        .filter(|&n| dom.element(n).is_some())
        .all(|n| {
            (n == node || !escapes(styles.style(n))) && generated_stay(dom, styles, n, &escapes)
        })
}

fn generated_stay(
    dom: &Dom,
    styles: &StyleMap,
    owner: NodeId,
    escapes: &impl Fn(&ComputedStyle) -> bool,
) -> bool {
    dom.generated(owner)
        .into_iter()
        .flatten()
        .all(|g| !escapes(styles.style(g)))
}

#[cfg(test)]
mod tests {
    use css::{cascade, parse_stylesheet, user_agent_stylesheet, Media, Origin, Stylesheet};
    use font::Approximate;
    use html::{Change, Dom, NodeId, NodeKind};

    use crate::{layout, Layout};

    const PAGE: &str = "<body>\
        <p>Intro text that wraps across a few lines in the narrow column here.</p>\
        <div class=box><p>Inside a clipped box</p><span>more</span></div>\
        <p>Before <input> after <span class=ib>inline block <b>text</b></span> tail.</p>\
        <div style='float:right;width:50px;height:40px'>f</div>\
        <div class=box2><div class=box><b>nested</b> text</div><p>sibling</p></div>\
        <div style='display:flex'><div class=box>flex item</div><div>other</div></div>\
        <div style='position:relative'><div class=box>in relative</div></div>\
        <div style='opacity:.5' class=box>faded <i>words</i></div>\
        <ul><li>one<li>two <span class=ib>three</span></ul>\
        <div class=box style='height:60px'>fixed height <p>para</p></div>\
        <div style='position:absolute;top:0;left:0' class=box>abs</div>\
        </body>";

    const CSS: &str = ".box { overflow: hidden; width: 200px; border: 1px solid; padding: 2px }
        .box2 { overflow: hidden }
        .ib { display: inline-block; overflow: hidden; width: 80px }
        .big { font-size: 30px }
        .pad { padding: 7px }
        .none { display: none }
        .wide { width: 150px }
        .round { border-radius: 4px; opacity: 0.8 }
        p:first-child { margin-top: 3px }
        .box p + span { padding-left: 5px }";

    fn styles(dom: &mut Dom, author: &Stylesheet) -> css::StyleMap {
        let media = Media {
            width: 400.0,
            height: 300.0,
            ..Media::default()
        };
        let sheets = [
            (Origin::UserAgent, user_agent_stylesheet()),
            (Origin::Author, author),
        ];
        let mut styles = cascade(dom, &sheets, &media);
        styles.attach_generated(dom);
        styles
    }

    fn full(dom: &Dom, styles: &css::StyleMap) -> Layout {
        layout(dom, styles, &Approximate, &|_| None, 400.0, 300.0)
    }

    /// Everything a reader of the layout sees.
    fn assert_same(a: &Layout, b: &Layout, step: usize, seed: u64) {
        let step = format!("{step} (seed {seed})");
        let boxes = |l: &Layout| l.boxes.iter().map(|b| (b.node, b.rect)).collect::<Vec<_>>();
        assert_eq!(boxes(a), boxes(b), "boxes after step {step}");
        let texts = |l: &Layout| {
            l.texts
                .iter()
                .map(|t| (t.node, t.x, t.width, t.baseline, l.text(t).to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(a), texts(b), "texts after step {step}");
        assert_eq!(a.clips, b.clips, "clips after step {step}");
        assert_eq!(a.groups, b.groups, "groups after step {step}");
        assert_eq!(a.layers, b.layers, "layers after step {step}");
        assert_eq!(a.anchors, b.anchors, "anchors after step {step}");
        assert_eq!(
            (a.width, a.height),
            (b.width, b.height),
            "size after step {step}"
        );
        assert_eq!(
            a.box_extents, b.box_extents,
            "box extents after step {step}"
        );
        assert_eq!(
            a.text_extents, b.text_extents,
            "text extents after step {step}"
        );
    }

    /// What `relayout` is told for each change: where content changed.
    fn changed(dom: &Dom, changes: &[Change]) -> Vec<NodeId> {
        changes
            .iter()
            .filter_map(|c| match *c {
                Change::Text(n) | Change::Element(n) => dom.parent(n),
                Change::Children(n) => Some(n),
            })
            .collect()
    }

    #[test]
    fn random_changes_lay_out_as_a_full_pass() {
        for seed in [0x2545_f491_4f6c_dd1d, 7, 0xdead_beef, 1 << 40] {
            random_changes(seed);
        }
    }

    fn random_changes(start: u64) {
        let mut seed = start;
        let author = parse_stylesheet(CSS);
        let mut dom = html::parse(PAGE);
        let mut map = styles(&mut dom, &author);
        let mut incremental = full(&dom, &map);
        dom.take_changes();
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let words = [
            "a",
            "bb",
            "ccc dd",
            "a much longer run of several words",
            "",
            "x y",
        ];
        let classes = ["big", "pad", "none", "wide", "round", "box", "ib", ""];
        let tags = ["span", "b", "div", "p"];
        let mut partial = 0;
        for step in 0..400 {
            let nodes: Vec<NodeId> = dom.descendants(Dom::ROOT).collect();
            let elements: Vec<NodeId> = nodes
                .iter()
                .copied()
                .filter(|&n| {
                    !matches!(dom.tag_name(n), None | Some("html" | "head" | "body"))
                        && !dom.is_generated(n)
                })
                .collect();
            let texts: Vec<NodeId> = nodes
                .iter()
                .copied()
                .filter(|&n| matches!(dom.kind(n), NodeKind::Text(_)))
                .collect();
            // Changes inside a box are the common case; one in three steps changes two places.
            for _ in 0..1 + usize::from(next(3) == 0) {
                match next(5) {
                    0 | 1 if !texts.is_empty() => {
                        let t = texts[next(texts.len())];
                        dom.set_text(t, words[next(words.len())]);
                    }
                    2 if !elements.is_empty() => {
                        let e = elements[next(elements.len())];
                        dom.set_attr(e, "class", classes[next(classes.len())]);
                    }
                    3 if !elements.is_empty() => {
                        let e = elements[next(elements.len())];
                        let child = dom.create_element(tags[next(tags.len())]);
                        let text = dom.create_text(words[next(words.len())]);
                        dom.append_child(child, text);
                        dom.append_child(e, child);
                    }
                    _ if !elements.is_empty() => {
                        let e = elements[next(elements.len())];
                        dom.detach(e);
                    }
                    _ => {}
                }
            }
            let changes = dom.take_changes().expect("a few changes");
            map = styles(&mut dom, &author);
            let expected = full(&dom, &map);
            let nodes = changed(&dom, &changes);
            if incremental.relayout(&dom, &map, &Approximate, &|_| None, &nodes) {
                partial += 1;
                assert_same(&incremental, &expected, step, start);
            } else {
                incremental = expected;
            }
        }
        assert!(
            partial > 80,
            "seed {start}: only {partial} of 400 changes were laid out in place"
        );
    }
}
