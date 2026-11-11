//! Block and inline layout: turns the styled DOM into positioned boxes and lines of text.
//!
//! Supported: block boxes with width/height/min/max, auto margins, percentages, padding and
//! borders; vertical margin collapsing between siblings; anonymous blocks around inline
//! content; line breaking with white-space collapsing, `<br>`, `pre`, `nowrap`, `pre-wrap`;
//! text-align; half-leading baseline alignment; `<img>` as a sized inline box.
//!
//! Positioning: `relative` shifts a box and its contents after layout; `absolute` and `fixed`
//! leave the flow, are laid out last against their containing block, and land in their own
//! paint layers (`Layout::layers`) so they draw over normal flow; `fixed` layers do not scroll.
//! `overflow: hidden` records a clip range (`Layout::clips`). Neither costs memory per fragment.
//!
//! Floats: `float` boxes are placed against the floats already in their block formatting
//! context (`Engine::floats`, page coordinates, dropped when the context ends), line boxes
//! shorten beside them, `clear` moves a box below them, and boxes that start a formatting
//! context sit beside them. Floats paint in document order, so a later block's background can
//! cover one.
//!
//! Not yet: z-index (positioned boxes paint in document order), flex and grid (laid out
//! as blocks), tables, inline-block shrink-to-fit (laid out as blocks), margin collapsing through
//! parents, padding and borders on inline elements, justify (treated as left).

mod flex;
mod grid;
mod table;

use std::collections::HashMap;
use std::ops::Range;

use css::{
    Align, Clear, ComputedStyle, Display, Float, Justify, Length, LineHeight, Position, StyleMap,
    TextAlign, VAlign, WhiteSpace,
};
use font::TextMetrics;
use html::{ChunkVec, Dom, FastBuildHasher, NodeId, NodeKind};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A block-level box or a replaced inline (an image). The rect is the border box.
#[derive(Clone, Copy, Debug)]
pub struct BoxFragment {
    pub node: NodeId,
    pub rect: Rect,
}

/// A run of text on one line, from one text node.
#[derive(Clone, Copy, Debug)]
/// 24 bytes: the vertical extent is not stored because it follows from the baseline and the
/// node's style, and pages can have hundreds of thousands of these.
pub struct TextFragment {
    pub node: NodeId,
    pub x: f32,
    pub width: f32,
    pub baseline: f32,
    start: u32,
    len: u32,
}

/// A paint layer: where it starts in `boxes` and `texts`. Layer 0 is the normal flow; each
/// absolutely or fixed positioned box starts a new layer, painted after all earlier ones.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layer {
    pub boxes: u32,
    pub texts: u32,
    /// Fixed layers stay in place when the page scrolls.
    pub fixed: bool,
}

/// Fragments in these index ranges are clipped to `rect` (`overflow: hidden`). Ranges nest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub boxes: (u32, u32),
    pub texts: (u32, u32),
    pub rect: Rect,
}

/// Fragments per chunk of `Layout::box_extents` / `text_extents`.
pub const CHUNK: usize = 256;

pub struct Layout {
    pub boxes: Vec<BoxFragment>,
    /// Chunked: the stress page has 160k of these, and Vec growth would briefly hold two copies.
    pub texts: ChunkVec<TextFragment>,
    /// The laid-out text of every fragment, back to back.
    text: String,
    pub layers: Vec<Layer>,
    pub clips: Vec<Clip>,
    pub width: f32,
    pub height: f32,
    /// For each run of `CHUNK` boxes, the top of the highest and the bottom of the lowest, in
    /// page coordinates. Painting skips runs outside the view, so the cost of a frame follows
    /// what is on screen, not the page's length. Runs may mix layers; that only widens them.
    pub box_extents: Vec<(f32, f32)>,
    /// The same for text runs, from the baseline up and down by the font size (what painting
    /// tests a text run against).
    pub text_extents: Vec<(f32, f32)>,
}

impl Layout {
    /// Each layer's box range, text range, and whether it is fixed, in paint order.
    pub fn layer_ranges(&self) -> Vec<(Range<usize>, Range<usize>, bool)> {
        (0..self.layers.len())
            .map(|i| {
                let l = self.layers[i];
                let next = self.layers.get(i + 1);
                let boxes_end = next.map_or(self.boxes.len(), |n| n.boxes as usize);
                let texts_end = next.map_or(self.texts.len(), |n| n.texts as usize);
                (
                    l.boxes as usize..boxes_end,
                    l.texts as usize..texts_end,
                    l.fixed,
                )
            })
            .collect()
    }

    /// Whether box `i` is in a fixed layer.
    pub fn box_is_fixed(&self, i: usize) -> bool {
        let k = self.layers.partition_point(|l| l.boxes as usize <= i);
        k > 0 && self.layers[k - 1].fixed
    }

    /// Whether text fragment `i` is in a fixed layer.
    pub fn text_is_fixed(&self, i: usize) -> bool {
        let k = self.layers.partition_point(|l| l.texts as usize <= i);
        k > 0 && self.layers[k - 1].fixed
    }

    /// A layout with nothing in it, as a placeholder before the first layout runs.
    pub fn empty() -> Layout {
        Layout {
            boxes: Vec::new(),
            texts: ChunkVec::new(),
            text: String::new(),
            layers: vec![FLOW_LAYER],
            clips: Vec::new(),
            width: 0.0,
            height: 0.0,
            box_extents: Vec::new(),
            text_extents: Vec::new(),
        }
    }

    /// Group fragments into runs of `CHUNK` and record each run's vertical extent.
    fn index(&mut self, styles: &StyleMap) {
        let extent = |runs: &mut Vec<(f32, f32)>, i: usize, top: f32, bottom: f32| {
            if i.is_multiple_of(CHUNK) {
                runs.push((top, bottom));
            } else if let Some(run) = runs.last_mut() {
                run.0 = run.0.min(top);
                run.1 = run.1.max(bottom);
            }
        };
        let mut boxes = Vec::with_capacity(self.boxes.len().div_ceil(CHUNK));
        for (i, b) in self.boxes.iter().enumerate() {
            extent(&mut boxes, i, b.rect.y, b.rect.y + b.rect.height);
        }
        let mut texts = Vec::with_capacity(self.texts.len().div_ceil(CHUNK));
        for i in 0..self.texts.len() {
            let t = &self.texts[i];
            let size = styles.style(t.node).font_size.to_f32();
            extent(&mut texts, i, t.baseline - size, t.baseline + size);
        }
        self.box_extents = boxes;
        self.text_extents = texts;
    }

    /// The fragments of `range` (indices into `boxes` or `texts`, per `extents`) whose run
    /// reaches between `top` and `bottom`, in order.
    pub fn in_view(
        extents: &[(f32, f32)],
        range: Range<usize>,
        top: f32,
        bottom: f32,
    ) -> impl Iterator<Item = usize> + '_ {
        let first = range.start / CHUNK;
        let last = range.end.div_ceil(CHUNK);
        (first..last.min(extents.len()))
            .filter(move |&c| extents[c].1 >= top && extents[c].0 <= bottom)
            .flat_map(move |c| range.start.max(c * CHUNK)..range.end.min((c + 1) * CHUNK))
    }

    pub fn text(&self, fragment: &TextFragment) -> &str {
        &self.text[fragment.start as usize..(fragment.start + fragment.len) as usize]
    }
}

const FLOW_LAYER: Layer = Layer {
    boxes: 0,
    texts: 0,
    fixed: false,
};

/// Lay out the document for a viewport of `width` by `height` CSS px.
/// Natural size of an image element in CSS px, when its file is loaded and readable.
pub type NaturalSize<'a> = &'a dyn Fn(NodeId) -> Option<(f32, f32)>;

/// `NaturalSize` for documents without images.
pub fn no_images(_: NodeId) -> Option<(f32, f32)> {
    None
}

pub fn layout(
    dom: &Dom,
    styles: &StyleMap,
    metrics: &dyn TextMetrics,
    natural: NaturalSize,
    width: f32,
    height: f32,
) -> Layout {
    let mut engine = Engine {
        dom,
        natural,
        styles,
        metrics,
        out: Layout {
            // At most one box per element: reserving avoids the up-to-2x overshoot of Vec growth.
            boxes: Vec::with_capacity(dom.element_count()),
            texts: ChunkVec::new(),
            text: String::new(),
            layers: vec![FLOW_LAYER],
            clips: Vec::new(),
            width,
            height: 0.0,
            box_extents: Vec::new(),
            text_extents: Vec::new(),
        },
        items: Vec::new(),
        words: String::new(),
        bottom: 0.0,
        deferred: Vec::new(),
        containing: HashMap::default(),
        current_box: None,
        intrinsic: HashMap::default(),
        measuring: false,
        atomics: Vec::new(),
        line_width: width,
        centering: false,
        img: dom.atom("img"),
        viewport: Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        ifc_origin: (0.0, 0.0),
        in_fixed: false,
        floats: Vec::new(),
        float_floor: f32::MIN,
    };
    let content = engine.layout_children(Dom::ROOT, 0.0, 0.0, width, Some(height));
    // Out-of-flow boxes last, in document order; laying one out can defer more (nested ones).
    let mut next = 0;
    while next < engine.deferred.len() {
        let deferred = engine.deferred[next];
        next += 1;
        engine.layout_positioned(deferred);
    }
    // The scrollable height includes content that overflows its box (`html { height: 100% }`).
    engine.out.height = content.max(engine.bottom).max(height);
    engine.out.index(styles);
    engine.out
}

struct Engine<'a> {
    dom: &'a Dom,
    natural: NaturalSize<'a>,
    styles: &'a StyleMap,
    metrics: &'a dyn TextMetrics,
    out: Layout,
    /// Scratch buffers reused by every inline formatting context.
    items: Vec<Item>,
    words: String,
    /// Lowest edge of any box or line so far, for the scrollable height.
    bottom: f32,
    /// Absolutely and fixed positioned boxes waiting for the end of normal flow.
    deferred: Vec<Deferred>,
    /// Box index of each positioned element: its padding box is the containing block of its
    /// absolute descendants. Read when they are laid out, so later shifts are accounted for.
    containing: HashMap<NodeId, u32, FastBuildHasher>,
    /// The box whose children are being laid out; static positions are kept relative to it.
    current_box: Option<u32>,
    /// Intrinsic (min-content, max-content) border-box widths, computed on demand.
    intrinsic: HashMap<NodeId, (f32, f32), FastBuildHasher>,
    /// Collecting inline content only to measure it: out-of-flow boxes are not deferred and
    /// inline blocks are not laid out.
    measuring: bool,
    /// Fragment ranges (boxes, texts, clips) of inline blocks laid out while collecting a line,
    /// waiting to be moved into place.
    atomics: Vec<(Range<usize>, Range<usize>, Range<usize>)>,
    /// Width of the line box being collected: what inline blocks shrink to fit.
    line_width: f32,
    /// The container being laid out is `<center>`-like (`text-align: -webkit-center`): its
    /// narrower block children are centered. Set once per container, not looked up per child.
    centering: bool,
    /// The interned name of `img`, if the document has any: replaced-element checks compare
    /// one integer per box instead of a tag name.
    img: Option<html::Atom>,
    /// The initial containing block, and the containing block of fixed boxes.
    viewport: Rect,
    /// Where the current inline run started: the static position of out-of-flow inlines.
    ifc_origin: (f32, f32),
    /// Laying out a fixed box: it does not extend the scrollable height.
    in_fixed: bool,
    /// Floats placed so far in the current block formatting context, in page coordinates.
    /// A box that starts a new context sets the list aside while its content is laid out, so
    /// moving that box later (inline blocks, flex items) moves its floats with it.
    floats: Vec<Placed>,
    /// The top of the last float placed: no later float may sit higher (CSS 2.1 9.5.1).
    float_floor: f32,
}

/// A placed float's margin box.
#[derive(Clone, Copy, Debug)]
struct Placed {
    left: bool,
    x0: f32,
    x1: f32,
    top: f32,
    bottom: f32,
}

#[derive(Clone, Copy)]
struct Deferred {
    node: NodeId,
    /// Where the box would have been in normal flow, as an offset from the top left of the
    /// box it sits in (`anchor`), so moving that box later moves the static position too.
    anchor: Option<u32>,
    static_dx: f32,
    static_dy: f32,
    /// The positioned ancestor whose padding box is the containing block; `None` for the viewport.
    containing_block: Option<NodeId>,
    fixed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Skip,
    Block,
    Inline,
    /// `position: absolute` or `fixed`: laid out after normal flow.
    OutOfFlow,
    /// `float: left` or `right`.
    Float,
}

/// One unbreakable piece of inline content.
#[derive(Clone, Copy)]
enum Item {
    Word {
        node: NodeId,
        /// Range in `Engine::words`.
        start: u32,
        len: u32,
        width: f32,
        /// Width of the collapsed space after this word, 0 if none.
        space_after: f32,
        /// Whether a line may break after this word (false for `nowrap` and `pre`).
        breakable: bool,
    },
    /// An image, or an `inline-block`/`inline-flex` box already laid out at the origin
    /// (`laid` indexes `Engine::atomics`; `NO_FRAGMENTS` for images). Sizes are margin boxes.
    Atomic {
        node: NodeId,
        width: f32,
        height: f32,
        /// Distance from the top of the margin box to its baseline.
        above: f32,
        /// Min-content width, for measuring.
        min: f32,
        laid: u32,
        /// Width of the collapsed space after it, 0 if none.
        space_after: f32,
    },
    /// The start of an inline element with margin, border, padding or a background:
    /// `margin` is its left margin, `width` that plus its left border and padding.
    Open {
        node: NodeId,
        margin: f32,
        width: f32,
        space_after: f32,
    },
    /// The end of such an element: right padding, border and margin (`margin` of `width`).
    Close {
        node: NodeId,
        margin: f32,
        width: f32,
        space_after: f32,
    },
    /// A float: placed on the line where it appears if it fits there, else below that line.
    /// It takes no width on the line itself.
    Float {
        node: NodeId,
        space_after: f32,
    },
    Break,
    /// `<br clear>`: the next line starts below these floats.
    Clear(Clear),
}

const NO_FRAGMENTS: u32 = u32::MAX;

/// The children of a node as layout sees them: the box CSS generated before them
/// (`::before`), the DOM children, then the one generated after. Generated boxes are not
/// among the DOM children (`Dom::generated`), so selectors and scripts never see them.
pub(crate) struct Kids<'a> {
    before: Option<NodeId>,
    rest: html::Children<'a>,
    after: Option<NodeId>,
}

impl Iterator for Kids<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.before.is_some() {
            return self.before.take();
        }
        match self.rest.next() {
            Some(n) => Some(n),
            None => self.after.take(),
        }
    }
}

/// `node`'s children, with its generated boxes.
pub(crate) fn kids(dom: &Dom, node: NodeId) -> Kids<'_> {
    let [before, after] = dom.generated(node);
    Kids {
        before,
        rest: dom.children(node),
        after,
    }
}

/// `first` and what follows it among its parent's children as `kids` lists them.
pub(crate) fn kids_from(dom: &Dom, first: NodeId) -> Kids<'_> {
    let [before, after] = dom.parent(first).map_or([None, None], |p| dom.generated(p));
    if before == Some(first) {
        let parent = dom.parent(first).unwrap_or(Dom::ROOT);
        Kids {
            before,
            rest: dom.children(parent),
            after,
        }
    } else if after == Some(first) {
        Kids {
            before: None,
            rest: dom.no_children(),
            after,
        }
    } else {
        Kids {
            before: None,
            rest: dom.siblings_from(first),
            after,
        }
    }
}

/// Vertical extent of an inline box around the baseline, after half-leading.
#[derive(Clone, Copy, Default)]
struct Extent {
    above: f32,
    below: f32,
}

impl<'a> Engine<'a> {
    fn style(&self, id: NodeId) -> &'a ComputedStyle {
        self.styles.style(id)
    }

    fn level(&self, id: NodeId) -> Level {
        match self.dom.kind(id) {
            NodeKind::Text(_) => Level::Inline,
            NodeKind::Element(_) => {
                let s = self.style(id);
                match s.display {
                    Display::None => Level::Skip,
                    _ if s.position.is_out_of_flow() => Level::OutOfFlow,
                    _ if s.float != Float::None => Level::Float,
                    Display::TableColumn => Level::Skip,
                    Display::Block
                    | Display::ListItem
                    | Display::Flex
                    | Display::Grid
                    | Display::Table
                    // Table parts outside a table are laid out as blocks.
                    | Display::TableRowGroup
                    | Display::TableHeaderGroup
                    | Display::TableFooterGroup
                    | Display::TableRow
                    | Display::TableCell
                    | Display::TableCaption
                    | Display::FlowRoot => Level::Block,
                    Display::InlineBlock
                    | Display::InlineFlex
                    | Display::InlineGrid
                    | Display::InlineTable => Level::Inline,
                    // An inline element that directly holds a block (an <a> around a <div>) is
                    // laid out as a block so its block child gets a proper box.
                    Display::Inline
                        if self.dom.children(id).any(|c| self.level(c) == Level::Block)
                            || self
                                .dom
                                .generated(id)
                                .into_iter()
                                .flatten()
                                .any(|c| self.level(c) == Level::Block) =>
                    {
                        Level::Block
                    }
                    Display::Inline => Level::Inline,
                }
            }
            _ => Level::Skip,
        }
    }

    /// Lay out the children of a block container whose content box starts at (x, y).
    /// Returns the content height.
    fn layout_children(
        &mut self,
        node: NodeId,
        x: f32,
        y: f32,
        width: f32,
        definite_height: Option<f32>,
    ) -> f32 {
        // The plain DOM walk unless the element has generated boxes: it is the hot path.
        let dom = self.dom;
        if dom.generated(node) == [None, None] {
            self.layout_children_of(dom.children(node), node, x, y, width, definite_height)
        } else {
            self.layout_children_of(kids(dom, node), node, x, y, width, definite_height)
        }
    }

    fn layout_children_of(
        &mut self,
        children: impl Iterator<Item = NodeId>,
        node: NodeId,
        x: f32,
        y: f32,
        width: f32,
        definite_height: Option<f32>,
    ) -> f32 {
        let centering = self.dom.element(node).is_some()
            && self.style(node).text_align == TextAlign::WebkitCenter;
        let outer_centering = std::mem::replace(&mut self.centering, centering);
        let mut cursor = y;
        let mut pending_margin = 0.0;
        // The open inline run as a sibling range (first, count): no allocation per box.
        // Hidden and out-of-flow siblings inside it are handled by `collect`.
        let mut run: Option<(NodeId, usize)> = None;
        for child in children {
            match self.level(child) {
                Level::Inline => match &mut run {
                    Some((_, count)) => *count += 1,
                    None => run = Some((child, 1)),
                },
                Level::Skip | Level::OutOfFlow | Level::Float if run.is_some() => {
                    if let Some((_, count)) = &mut run {
                        *count += 1;
                    }
                }
                Level::Skip => {}
                Level::OutOfFlow => self.defer(child, x, cursor + pending_margin),
                Level::Float => self.place_float(child, x, width, cursor + pending_margin),
                Level::Block => {
                    if let Some((first, count)) = run.take() {
                        let height = self.layout_inline(
                            first,
                            count,
                            node,
                            x,
                            cursor + pending_margin,
                            width,
                        );
                        if height > 0.0 {
                            cursor += pending_margin + height;
                            pending_margin = 0.0;
                        }
                    }
                    let (bottom, margin_bottom) =
                        self.layout_block(child, x, cursor, pending_margin, width, definite_height);
                    cursor = bottom;
                    pending_margin = margin_bottom;
                }
            }
        }
        if let Some((first, count)) = run {
            let height = self.layout_inline(first, count, node, x, cursor + pending_margin, width);
            if height > 0.0 {
                cursor += pending_margin + height;
                pending_margin = 0.0;
            }
        }
        self.centering = outer_centering;
        cursor + pending_margin - y
    }

    /// Lay out one block box. `prev_margin` is the previous sibling's bottom margin, which
    /// collapses with this box's top margin. Returns the bottom border edge and bottom margin.
    fn layout_block(
        &mut self,
        id: NodeId,
        cb_x: f32,
        cursor: f32,
        prev_margin: f32,
        cb_width: f32,
        cb_height: Option<f32>,
    ) -> (f32, f32) {
        let s = self.style(id);
        let edges = Edges::of(s, cb_width);
        let extra = edges.x();
        let margin_top = resolve(s.margin[0], cb_width).unwrap_or(0.0);
        let margin_bottom = resolve(s.margin[2], cb_width).unwrap_or(0.0);
        let margin_left = resolve(s.margin[3], cb_width);
        let margin_right = resolve(s.margin[1], cb_width);

        let auto_width =
            (cb_width - margin_left.unwrap_or(0.0) - margin_right.unwrap_or(0.0) - extra).max(0.0);
        let content_width = if self.is_replaced(id) {
            self.replaced_size(id, s, Some(cb_width), cb_height).0
        } else if s.display.is_table() && s.width == Length::Auto {
            // Tables shrink to their columns, up to the available width.
            let (min, max) = self.intrinsic_width(id);
            let available = cb_width - margin_left.unwrap_or(0.0) - margin_right.unwrap_or(0.0);
            (max.min(available).max(min) - extra).max(0.0)
        } else {
            clamp_width(
                s,
                content_size(s, s.width, Some(cb_width), extra).unwrap_or(auto_width),
                cb_width,
                extra,
            )
        };
        // Auto margins take the space the width leaves over (centering when both are auto).
        let free = cb_width
            - content_width
            - extra
            - margin_left.unwrap_or(0.0)
            - margin_right.unwrap_or(0.0);
        let centered = self.centering;
        let margin_left = match (margin_left, margin_right) {
            (None, None) => (free / 2.0).max(0.0),
            (None, Some(_)) => free,
            (Some(m), _) if centered && free > 0.0 => m + free / 2.0,
            (Some(m), _) => m,
        };

        let mut top = cursor + collapse(prev_margin, margin_top);
        let mut x = cb_x + margin_left;
        let mut content_width = content_width;
        if !self.floats.is_empty() {
            if s.clear != Clear::None {
                top = top.max(self.clear_y(s.clear));
            }
            // A box that starts a formatting context does not overlap floats: it narrows (an
            // auto width) or moves down until it fits beside them.
            if self.starts_context(id, s) {
                let margins = margin_left + margin_right.unwrap_or(0.0);
                let auto =
                    s.width == Length::Auto && !self.is_replaced(id) && !s.display.is_table();
                loop {
                    let (left, avail) = self.band(top, 1.0, cb_x, cb_width);
                    if avail >= cb_width {
                        break;
                    }
                    if auto {
                        content_width =
                            clamp_width(s, (avail - margins - extra).max(0.0), cb_width, extra);
                    }
                    let fits = content_width + extra + margins <= avail + 0.01;
                    match self.next_float_bottom(top, cb_x, cb_width) {
                        Some(below) if !fits => top = below,
                        _ => {
                            x = left + margin_left;
                            break;
                        }
                    }
                }
            }
        }
        let border_height = self.build_box(id, x, top, content_width, cb_width, cb_height, None);
        (top + border_height, margin_bottom)
    }

    /// Lay out box `id` with its border box's top left at (x, y) and the given content width.
    /// Records its clip and containing-block rect, applies `position: relative`, and returns
    /// the border-box height. `forced_height` overrides the content height (absolute boxes
    /// stretched between `top` and `bottom`).
    #[allow(clippy::too_many_arguments)]
    fn build_box(
        &mut self,
        id: NodeId,
        x: f32,
        y: f32,
        content_width: f32,
        cb_width: f32,
        cb_height: Option<f32>,
        forced_height: Option<f32>,
    ) -> f32 {
        let s = self.style(id);
        let edges = Edges::of(s, cb_width);
        let (border, padding) = (edges.border, edges.padding);
        let extra = edges.x();
        let content_x = x + border[3] + padding[3];
        let content_y = y + border[0] + padding[0];
        let own_height = forced_height
            .or_else(|| content_size(s, s.height, cb_height, edges.y()))
            .or_else(|| {
                self.is_replaced(id)
                    .then(|| self.replaced_height(id, s, content_width, cb_height))
            });

        let index = self.out.boxes.len();
        let first_text = self.out.texts.len();
        let first_clip = self.out.clips.len();
        // The origin is known now; static positions of deferred descendants are relative to it.
        self.out.boxes.push(BoxFragment {
            node: id,
            rect: Rect {
                x,
                y,
                width: content_width + extra,
                height: 0.0,
            },
        });
        let outer_box = self.current_box.replace(index as u32);
        // Whether the box starts a formatting context is asked only when floats exist outside
        // it (set them aside) or appear inside it (take them back out): most pages have none.
        let outer_floats = (!self.floats.is_empty() && self.starts_context(id, s)).then(|| {
            (
                std::mem::take(&mut self.floats),
                std::mem::replace(&mut self.float_floor, f32::MIN),
            )
        });
        let had_floats = !self.floats.is_empty() || outer_floats.is_some();
        let mut content_height = if s.display.is_flex() {
            self.layout_flex(id, content_x, content_y, content_width, own_height)
        } else if s.display.is_grid() {
            self.layout_grid(id, content_x, content_y, content_width, own_height)
        } else if s.display.is_table() {
            self.layout_table(id, content_x, content_y, content_width)
        } else {
            self.layout_children(id, content_x, content_y, content_width, own_height)
        };
        self.current_box = outer_box;
        let inner = match outer_floats {
            Some((floats, floor)) => {
                self.float_floor = floor;
                Some(std::mem::replace(&mut self.floats, floats))
            }
            None if !had_floats && !self.floats.is_empty() && self.starts_context(id, s) => {
                self.float_floor = f32::MIN;
                Some(std::mem::take(&mut self.floats))
            }
            None => None,
        };
        // Floats grow the box that holds their formatting context, and no other.
        if let Some(bottom) = inner.and_then(|f| f.iter().map(|f| f.bottom).reduce(f32::max)) {
            content_height = content_height.max(bottom - content_y);
        }

        let mut height = own_height.unwrap_or(content_height);
        if let Some(max) = content_size(s, s.max_height, cb_height, edges.y()) {
            height = height.min(max);
        }
        if let Some(min) = content_size(s, s.min_height, cb_height, edges.y()) {
            height = height.max(min);
        }
        let border_height = height + edges.y();
        self.out.boxes[index].rect.height = border_height;

        let padding_box = Rect {
            x: x + border[3],
            y: y + border[0],
            width: content_width + padding[1] + padding[3],
            height: height + padding[0] + padding[2],
        };
        if s.clips {
            self.out.clips.push(Clip {
                boxes: (index as u32 + 1, self.out.boxes.len() as u32),
                texts: (first_text as u32, self.out.texts.len() as u32),
                rect: padding_box,
            });
        }
        let mut offset = (0.0, 0.0);
        if s.position == Position::Relative {
            let h = cb_height.unwrap_or(0.0);
            let dx =
                resolve(s.inset[3], cb_width).or_else(|| resolve(s.inset[1], cb_width).map(|r| -r));
            let dy = resolve(s.inset[0], h).or_else(|| resolve(s.inset[2], h).map(|b| -b));
            offset = (dx.unwrap_or(0.0), dy.unwrap_or(0.0));
            if offset != (0.0, 0.0) {
                self.shift(index, first_text, first_clip, offset.0, offset.1);
            }
        }
        if s.position.is_positioned() {
            self.containing.insert(id, index as u32);
        }
        if !self.in_fixed {
            self.bottom = self.bottom.max(y + offset.1 + border_height);
        }
        border_height
    }

    /// Whether box `id` starts a block formatting context: floats outside it stay out, floats
    /// inside it stay in (CSS 2.1 9.4.1).
    fn starts_context(&self, id: NodeId, s: &ComputedStyle) -> bool {
        s.flow_root
            || s.float != Float::None
            || s.position.is_out_of_flow()
            || !matches!(
                s.display,
                Display::Block | Display::ListItem | Display::Inline
            )
            || self
                .dom
                .parent(id)
                .filter(|&p| self.dom.element(p).is_some())
                .is_some_and(|p| {
                    let d = self.style(p).display;
                    d.is_flex() || d.is_grid()
                })
    }

    /// The left edge and width left between the floats that reach into the band from `top`
    /// down by `height`, inside the container from `x` with `width`.
    #[cold]
    #[inline(never)]
    fn band(&self, top: f32, height: f32, x: f32, width: f32) -> (f32, f32) {
        let (mut l, mut r) = (x, x + width);
        for f in &self.floats {
            if f.top < top + height && f.bottom > top {
                if f.left {
                    l = l.max(f.x1);
                } else {
                    r = r.min(f.x0);
                }
            }
        }
        let l = l.min(x + width);
        (l, (r - l).max(0.0))
    }

    /// The highest float bottom below `top` among the floats that narrow the container there:
    /// where to try next when something does not fit beside them.
    #[cold]
    #[inline(never)]
    fn next_float_bottom(&self, top: f32, x: f32, width: f32) -> Option<f32> {
        self.floats
            .iter()
            .filter(|f| f.top <= top && f.bottom > top && f.x1 > x && f.x0 < x + width)
            .map(|f| f.bottom)
            .reduce(f32::min)
    }

    /// The lowest bottom of the floats `clear` asks to clear.
    #[cold]
    #[inline(never)]
    fn clear_y(&self, clear: Clear) -> f32 {
        self.floats
            .iter()
            .filter(|f| clear.clears(f.left))
            .map(|f| f.bottom)
            .fold(f32::MIN, f32::max)
    }

    /// Content width and margin-box width of a float in a container `cb_width` wide: its
    /// `width`, or shrink-to-fit from its intrinsic widths.
    #[cold]
    #[inline(never)]
    fn float_width(&mut self, id: NodeId, cb_width: f32) -> (f32, f32) {
        let s = self.style(id);
        let extra = Edges::of(s, cb_width).x();
        let margins = resolve(s.margin[1], cb_width).unwrap_or(0.0)
            + resolve(s.margin[3], cb_width).unwrap_or(0.0);
        let content = if self.is_replaced(id) {
            self.replaced_size(id, s, Some(cb_width), None).0
        } else {
            match content_size(s, s.width, Some(cb_width), extra) {
                Some(w) => clamp_width(s, w, cb_width, extra),
                None => {
                    let (min, max) = self.intrinsic_width(id);
                    let available = (cb_width - margins - extra).max(0.0);
                    let w = (max - extra).min(available).max(min - extra).max(0.0);
                    clamp_width(s, w, cb_width, extra)
                }
            }
        };
        (content, content + extra + margins)
    }

    /// Place float `id` in the container from `x` with `width`, no higher than `y`: as far left
    /// or right as the floats already there allow, moving down until it fits (CSS 2.1 9.5.1).
    /// It is laid out once, in place.
    #[cold]
    #[inline(never)]
    fn place_float(&mut self, id: NodeId, x: f32, width: f32, y: f32) {
        if self.measuring {
            return;
        }
        let s = self.style(id);
        let left = s.float == Float::Left;
        let (content_width, outer) = self.float_width(id, width);
        let margin = [0, 1, 2, 3].map(|i| resolve(s.margin[i], width).unwrap_or(0.0));
        let mut top = y.max(self.float_floor);
        if s.clear != Clear::None {
            top = top.max(self.clear_y(s.clear));
        }
        let (band_left, room) = loop {
            let (l, room) = self.band(top, 1.0, x, width);
            if outer <= room + 0.01 {
                break (l, room);
            }
            match self.next_float_bottom(top, x, width) {
                Some(below) => top = below,
                None => break (l, room),
            }
        };
        let margin_x = if left {
            band_left
        } else {
            band_left + room - outer
        };
        // Its content runs inline layout, which reuses the scratch buffers of an open line.
        let items = std::mem::take(&mut self.items);
        let words = std::mem::take(&mut self.words);
        let origin = self.ifc_origin;
        let line_width = self.line_width;
        let height = self.build_box(
            id,
            margin_x + margin[3],
            top + margin[0],
            content_width,
            width,
            None,
            None,
        );
        self.items = items;
        self.words = words;
        self.ifc_origin = origin;
        self.line_width = line_width;
        self.floats.push(Placed {
            left,
            x0: margin_x,
            x1: margin_x + outer,
            top,
            bottom: top + margin[0] + height + margin[2],
        });
        self.float_floor = top;
    }

    /// A float met while filling a line: placed beside the line when it fits next to the
    /// `line_width` the line holds so far (returning the line's new room), else kept for below.
    #[cold]
    #[inline(never)]
    fn float_on_line(
        &mut self,
        node: NodeId,
        line_width: f32,
        room: f32,
        later: &mut Vec<NodeId>,
        (x, width): (f32, f32),
        (line_top, line_height): (f32, f32),
    ) -> Option<(f32, f32)> {
        if !later.is_empty() || line_width + self.float_width(node, width).1 > room {
            later.push(node);
            return None;
        }
        self.place_float(node, x, width, line_top);
        Some(self.band(line_top, line_height, x, width))
    }

    /// Images are replaced elements: sized by their file, not by children.
    fn is_replaced(&self, id: NodeId) -> bool {
        self.img
            .is_some_and(|img| self.dom.element(id).is_some_and(|e| e.tag == img))
    }

    /// A `width`/`height` attribute in px.
    fn attr_px(&self, id: NodeId, name: &str) -> Option<f32> {
        self.dom
            .attr(id, name)
            .and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
    }

    /// Natural width / height: from the loaded file, else from both size attributes.
    fn aspect_ratio(&self, id: NodeId) -> Option<f32> {
        let (w, h) = (self.natural)(id)
            .or_else(|| Some((self.attr_px(id, "width")?, self.attr_px(id, "height")?)))?;
        (w > 0.0 && h > 0.0).then_some(w / h)
    }

    /// Content-box size of an image (CSS 2.1 10.3.2, 10.6.2): CSS sizes, then the HTML size
    /// attributes, then the file's natural size, keeping the aspect ratio when only one side
    /// is known. `max-width` shrinks the height with it unless the height was set in CSS
    /// (`max-width: 100%; height: auto` keeps the proportions). Percentages need a base.
    pub(crate) fn replaced_size(
        &self,
        id: NodeId,
        s: &ComputedStyle,
        cb_width: Option<f32>,
        cb_height: Option<f32>,
    ) -> (f32, f32) {
        let edges = Edges::of(s, cb_width.unwrap_or(0.0));
        let (ex, ey) = (edges.x(), edges.y());
        let natural = (self.natural)(id);
        let ratio = self.aspect_ratio(id);
        let css_height = content_size(s, s.height, cb_height, ey);
        let w = content_size(s, s.width, cb_width, ex).or_else(|| self.attr_px(id, "width"));
        let h = css_height.or_else(|| self.attr_px(id, "height"));
        let (mut w, mut h) = match (w, h) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, ratio.map_or(natural.map_or(0.0, |n| n.1), |r| w / r)),
            (None, Some(h)) => (ratio.map_or(natural.map_or(0.0, |n| n.0), |r| h * r), h),
            (None, None) => natural.unwrap_or((0.0, 0.0)),
        };
        if let Some(cb) = cb_width {
            let clamped = clamp_width(s, w, cb, ex);
            if clamped != w {
                if let (None, Some(r)) = (css_height, ratio) {
                    h = clamped / r;
                }
                w = clamped;
            }
        }
        if let Some(max) = content_size(s, s.max_height, cb_height, ey) {
            if h > max {
                if let (Length::Auto, Some(r)) = (s.width, ratio) {
                    w = max * r;
                }
                h = max;
            }
        }
        (w.max(0.0), h.max(0.0))
    }

    /// An image's content height once its width is settled.
    fn replaced_height(
        &self,
        id: NodeId,
        s: &ComputedStyle,
        width: f32,
        cb_height: Option<f32>,
    ) -> f32 {
        let ey = Edges::of(s, 0.0).y();
        content_size(s, s.height, cb_height, ey)
            .or_else(|| self.aspect_ratio(id).map(|r| width / r))
            .or_else(|| self.attr_px(id, "height"))
            .unwrap_or(0.0)
    }

    /// Move every fragment and clip from the given indices onward.
    fn shift(&mut self, first_box: usize, first_text: usize, first_clip: usize, dx: f32, dy: f32) {
        let ends = (
            self.out.boxes.len(),
            self.out.texts.len(),
            self.out.clips.len(),
        );
        self.shift_range(
            first_box..ends.0,
            first_text..ends.1,
            first_clip..ends.2,
            dx,
            dy,
        );
    }

    /// Move the fragments and clips in these index ranges.
    fn shift_range(
        &mut self,
        boxes: Range<usize>,
        texts: Range<usize>,
        clips: Range<usize>,
        dx: f32,
        dy: f32,
    ) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for b in &mut self.out.boxes[boxes] {
            b.rect.x += dx;
            b.rect.y += dy;
        }
        for i in texts {
            let t = &mut self.out.texts[i];
            t.x += dx;
            t.baseline += dy;
        }
        for c in &mut self.out.clips[clips] {
            c.rect.x += dx;
            c.rect.y += dy;
        }
    }

    /// The padding box of box `i`.
    fn padding_box(&self, i: u32) -> Rect {
        let b = self.out.boxes[i as usize];
        let bw = self.style(b.node).border_width.map(|w| w.to_f32());
        Rect {
            x: b.rect.x + bw[3],
            y: b.rect.y + bw[0],
            width: (b.rect.width - bw[1] - bw[3]).max(0.0),
            height: (b.rect.height - bw[0] - bw[2]).max(0.0),
        }
    }

    /// Set an out-of-flow box aside until normal flow is done. (x, y) is its static position.
    fn defer(&mut self, node: NodeId, static_x: f32, static_y: f32) {
        if self.measuring {
            return;
        }
        let fixed = self.style(node).position == Position::Fixed;
        let containing_block = if fixed {
            None
        } else {
            let mut current = self.dom.parent(node);
            loop {
                match current {
                    Some(id)
                        if self.dom.element(id).is_some()
                            && self.style(id).position.is_positioned() =>
                    {
                        break Some(id);
                    }
                    Some(id) => current = self.dom.parent(id),
                    None => break None,
                }
            }
        };
        let anchor = self.current_box;
        let origin = anchor.map_or((0.0, 0.0), |i| {
            let r = self.out.boxes[i as usize].rect;
            (r.x, r.y)
        });
        self.deferred.push(Deferred {
            node,
            anchor,
            static_dx: static_x - origin.0,
            static_dy: static_y - origin.1,
            containing_block,
            fixed,
        });
    }

    /// Lay out an absolutely or fixed positioned box in its own paint layer (CSS 2.1 10.3.7, 10.6.4).
    fn layout_positioned(&mut self, d: Deferred) {
        let cb = d
            .containing_block
            .and_then(|id| self.containing.get(&id).copied())
            .map_or(self.viewport, |i| self.padding_box(i));
        let (static_x, static_y) = match d.anchor {
            Some(i) => {
                let r = self.out.boxes[i as usize].rect;
                (r.x + d.static_dx, r.y + d.static_dy)
            }
            None => (d.static_dx, d.static_dy),
        };
        let s = self.style(d.node);
        let edges = Edges::of(s, cb.width);
        let (border, padding) = (edges.border, edges.padding);
        let extra_x = edges.x();
        let extra_y = edges.y();
        let margin = [
            resolve(s.margin[0], cb.width).unwrap_or(0.0),
            resolve(s.margin[1], cb.width).unwrap_or(0.0),
            resolve(s.margin[2], cb.width).unwrap_or(0.0),
            resolve(s.margin[3], cb.width).unwrap_or(0.0),
        ];
        let top = resolve(s.inset[0], cb.height);
        let right = resolve(s.inset[1], cb.width);
        let bottom = resolve(s.inset[2], cb.height);
        let left = resolve(s.inset[3], cb.width);

        let clamp_width = |w: f32| clamp_width(s, w, cb.width, extra_x);
        let horizontal = margin[1] + margin[3] + extra_x;
        let specified = content_size(s, s.width, Some(cb.width), extra_x);
        let (content_width, shrink) = match (specified, left, right) {
            (Some(w), _, _) => (clamp_width(w), false),
            (None, Some(l), Some(r)) => {
                (clamp_width((cb.width - l - r - horizontal).max(0.0)), false)
            }
            // Shrink-to-fit: lay out in the space available, then narrow to the content.
            (None, l, _) => (
                clamp_width((cb.width - l.unwrap_or(0.0) - horizontal).max(0.0)),
                true,
            ),
        };
        let x = match (left, right) {
            (Some(l), _) => cb.x + l + margin[3],
            (None, Some(r)) => cb.x + cb.width - r - margin[1] - content_width - extra_x,
            (None, None) => static_x + margin[3],
        };
        let forced_height = match (
            content_size(s, s.height, Some(cb.height), extra_y),
            top,
            bottom,
        ) {
            (None, Some(t), Some(b)) => {
                Some((cb.height - t - b - margin[0] - margin[2] - extra_y).max(0.0))
            }
            _ => None,
        };
        let y = match (top, bottom) {
            (Some(t), _) => cb.y + t + margin[0],
            (None, Some(_)) => cb.y,
            (None, None) => static_y + margin[0],
        };

        self.out.layers.push(Layer {
            boxes: self.out.boxes.len() as u32,
            texts: self.out.texts.len() as u32,
            fixed: d.fixed,
        });
        let index = self.out.boxes.len();
        let first_text = self.out.texts.len();
        let first_clip = self.out.clips.len();
        let was_fixed = std::mem::replace(&mut self.in_fixed, d.fixed);
        let height = self.build_box(
            d.node,
            x,
            y,
            content_width,
            cb.width,
            Some(cb.height),
            forced_height,
        );

        let mut dx = 0.0;
        if shrink {
            let content_x = x + border[3] + padding[3];
            let right_edge = self.out.boxes[index + 1..]
                .iter()
                .map(|b| b.rect.x + b.rect.width)
                .chain(
                    (first_text..self.out.texts.len())
                        .map(|i| self.out.texts[i].x + self.out.texts[i].width),
                )
                .fold(content_x, f32::max);
            let used = (right_edge - content_x).min(content_width);
            self.out.boxes[index].rect.width = used + extra_x;
            if left.is_none() && right.is_some() {
                dx = content_width - used;
            }
        }
        let dy = match (top, bottom) {
            (None, Some(b)) => cb.height - b - margin[2] - height,
            _ => 0.0,
        };
        self.shift(index, first_text, first_clip, dx, dy);
        self.in_fixed = was_fixed;
    }

    /// Lay out `count` siblings starting at `first` (an inline-level run) as lines inside `container`'s content box.
    /// Returns the height of the lines, 0 when the run is only collapsible white space.
    #[allow(clippy::too_many_arguments)]
    fn layout_inline(
        &mut self,
        first: NodeId,
        count: usize,
        container: NodeId,
        x: f32,
        y: f32,
        width: f32,
    ) -> f32 {
        self.items.clear();
        self.words.clear();
        self.ifc_origin = (x, y);
        self.line_width = width;
        let atomics_start = self.atomics.len();
        let dom = self.dom;
        let generated = dom.parent(first).map_or([None, None], |p| dom.generated(p));
        if generated == [None, None] {
            for node in dom.siblings_from(first).take(count) {
                self.collect(node);
            }
        } else {
            for node in kids_from(dom, first).take(count) {
                self.collect(node);
            }
        }
        if self.items.is_empty() {
            return 0.0;
        }

        let container_style = self.style(container);
        let strut = self.extent(container_style);
        let align = match container_style.text_align {
            TextAlign::Center | TextAlign::WebkitCenter => 0.5,
            TextAlign::Right => 1.0,
            TextAlign::Left | TextAlign::Justify => 0.0,
        };

        let items = std::mem::take(&mut self.items);
        let mut line_top = y;
        let mut start = 0;
        // Decorated inline elements open on this line: (node, its box on this line, start x).
        let mut open: Vec<(NodeId, usize, f32)> = Vec::new();
        // Floats that did not fit on the line where they appear: placed below it.
        let mut later: Vec<NodeId> = Vec::new();
        let strut_height = strut.above + strut.below;
        while start < items.len() {
            for node in later.drain(..) {
                self.place_float(node, x, width, line_top);
            }
            // Floats and clears at the start of a line need no room on it.
            while let Some(&item) = items.get(start) {
                match item {
                    Item::Float { node, .. } => self.place_float(node, x, width, line_top),
                    Item::Clear(c) => line_top = line_top.max(self.clear_y(c)),
                    _ => break,
                }
                start += 1;
            }
            if start == items.len() {
                break;
            }
            // The line's room beside floats; move down while its first piece does not fit.
            let (mut left, mut room) = (x, width);
            if !self.floats.is_empty() {
                loop {
                    (left, room) = self.band(line_top, strut_height, x, width);
                    if room >= item_width(&items[start]) || room >= width {
                        break;
                    }
                    match self.next_float_bottom(line_top, x, width) {
                        Some(below) => line_top = below,
                        None => break,
                    }
                }
            }
            // Greedy line breaking: take items until the next one would overflow.
            let mut end = start;
            let mut line_width = 0.0;
            let mut forced_break = false;
            while end < items.len() {
                let item = items[end];
                if let Item::Break = item {
                    forced_break = true;
                    break;
                }
                if let Item::Float { node, .. } = item {
                    if let Some(band) = self.float_on_line(
                        node,
                        line_width,
                        room,
                        &mut later,
                        (x, width),
                        (line_top, strut_height),
                    ) {
                        (left, room) = band;
                    }
                    end += 1;
                    continue;
                }
                let gap = if end > start {
                    space_after(&items[end - 1])
                } else {
                    0.0
                };
                let fits = line_width + gap + item_width(&item) <= room;
                let before_close = matches!(item, Item::Close { .. });
                if end > start && !fits && breakable(&items[end - 1]) && !before_close {
                    break;
                }
                line_width += gap + item_width(&item);
                end += 1;
            }

            let mut extent = strut;
            for item in &items[start..end] {
                let e = self.item_extent(item);
                extent.above = extent.above.max(e.above);
                extent.below = extent.below.max(e.below);
            }
            let baseline = line_top + extent.above;
            let mut pen = left + (room - line_width).max(0.0) * align;
            let mut last_text: Option<usize> = None;
            // Elements continuing from the previous line get a new box here, before this
            // line's content, so their background paints underneath it.
            for entry in open.iter_mut() {
                entry.1 = self.inline_placeholder(entry.0);
                entry.2 = pen;
            }
            for (i, item) in items[start..end].iter().enumerate() {
                if i > 0 {
                    pen += space_after(&items[start + i - 1]);
                }
                match *item {
                    Item::Word {
                        node,
                        start: ws,
                        len,
                        width: w,
                        ..
                    } => {
                        let word = &self.words[ws as usize..(ws + len) as usize];
                        let merge = last_text.filter(|&f| self.out.texts[f].node == node);
                        if let Some(f) = merge {
                            self.out.text.push(' ');
                            self.out.text.push_str(word);
                            let frag = &mut self.out.texts[f];
                            frag.len = self.out.text.len() as u32 - frag.start;
                            frag.width = pen + w - frag.x;
                        } else {
                            let start_byte = self.out.text.len() as u32;
                            self.out.text.push_str(word);
                            self.out.texts.push(TextFragment {
                                node,
                                x: pen,
                                width: w,
                                baseline,
                                start: start_byte,
                                len,
                            });
                            last_text = Some(self.out.texts.len() - 1);
                        }
                        pen += w;
                    }
                    Item::Atomic {
                        node,
                        width: w,
                        height: h,
                        above,
                        laid,
                        ..
                    } => {
                        if laid == NO_FRAGMENTS {
                            self.out.boxes.push(BoxFragment {
                                node,
                                rect: Rect {
                                    x: pen,
                                    y: baseline - h,
                                    width: w,
                                    height: h,
                                },
                            });
                        } else {
                            // Laid out at the origin while collecting; move it onto the line.
                            let (boxes, texts, clips) = self.atomics[laid as usize].clone();
                            let top = baseline - above;
                            self.shift_range(boxes, texts, clips, pen, top);
                            if !self.in_fixed {
                                self.bottom = self.bottom.max(top + h);
                            }
                        }
                        last_text = None;
                        pen += w;
                    }
                    Item::Open {
                        node,
                        margin,
                        width: w,
                        ..
                    } => {
                        // Margins only take space; a box is drawn (and clickable) only with a
                        // background, border or padding.
                        if self.inline_needs_box(self.style(node)) {
                            let index = self.inline_placeholder(node);
                            open.push((node, index, pen + margin));
                        }
                        last_text = None;
                        pen += w;
                    }
                    Item::Close {
                        node,
                        margin,
                        width: w,
                        ..
                    } => {
                        if let Some(at) = open.iter().rposition(|e| e.0 == node) {
                            let (node, index, from) = open.remove(at);
                            self.inline_box(index, node, from, pen + w - margin, baseline);
                        }
                        last_text = None;
                        pen += w;
                    }
                    Item::Break | Item::Float { .. } | Item::Clear(_) => {}
                }
            }
            for &(node, index, from) in &open {
                self.inline_box(index, node, from, pen, baseline);
            }
            line_top = baseline + extent.below;
            if !self.in_fixed {
                self.bottom = self.bottom.max(line_top);
            }
            start = if forced_break { end + 1 } else { end };
            // A <br> at the very end still ends a line, but no empty line follows it.
            if forced_break && start == items.len() {
                break;
            }
        }
        for node in later {
            self.place_float(node, x, width, line_top);
        }
        self.items = items;
        self.atomics.truncate(atomics_start);
        line_top - y
    }

    /// Whether an inline element draws anything or takes space of its own; plain ones (most
    /// `<a>`, `<b>`, `<span>`) add no items and no boxes.
    fn decorated_inline(&self, s: &ComputedStyle) -> bool {
        self.inline_needs_box(s)
            || [s.margin[1], s.margin[3]]
                .iter()
                .any(|m| !matches!(m, Length::Auto) && *m != Length::ZERO)
    }

    fn inline_needs_box(&self, s: &ComputedStyle) -> bool {
        s.background_color.a > 0
            || s.outline_width != css::Px::ZERO
            || s.border_width.iter().any(|w| *w != css::Px::ZERO)
            || s.padding.iter().any(|p| *p != Length::ZERO)
    }

    /// Horizontal margin, and border plus padding, on side 3 (left) or 1 (right).
    fn inline_sides(&self, s: &ComputedStyle, side: usize) -> (f32, f32) {
        let edges = Edges::of(s, self.line_width);
        let margin = resolve(s.margin[side], self.line_width).unwrap_or(0.0);
        (margin, edges.padding[side] + edges.border[side])
    }

    /// Reserve the box of an inline element on the current line; `inline_box` sizes it.
    fn inline_placeholder(&mut self, node: NodeId) -> usize {
        self.out.boxes.push(BoxFragment {
            node,
            rect: Rect::default(),
        });
        self.out.boxes.len() - 1
    }

    /// Size an inline element's box on one line: from `x0` to `x1` horizontally, and its font's
    /// ascent and descent plus vertical padding and border around the baseline. It does not
    /// change the line's height (CSS 2.1 10.6.1).
    fn inline_box(&mut self, index: usize, node: NodeId, x0: f32, x1: f32, baseline: f32) {
        let s = self.style(node);
        let size = s.font_size.to_f32();
        let edges = Edges::of(s, self.line_width);
        let top = baseline - self.metrics.ascent_em() * size - edges.padding[0] - edges.border[0];
        let bottom =
            baseline + self.metrics.descent_em() * size + edges.padding[2] + edges.border[2];
        self.out.boxes[index].rect = Rect {
            x: x0,
            y: top,
            width: (x1 - x0).max(0.0),
            height: bottom - top,
        };
    }

    /// An `inline-block` or `inline-flex` element: shrink-to-fit width, laid out now at the
    /// origin (only measured while measuring), and moved onto its line once lines are broken.
    fn collect_inline_block(&mut self, id: NodeId) {
        let s = self.style(id);
        let cb = self.line_width;
        let edges = Edges::of(s, cb);
        let extra = edges.x();
        let margin = [0, 1, 2, 3].map(|i| resolve(s.margin[i], cb).unwrap_or(0.0));
        let horizontal = margin[1] + margin[3];
        let (min_content, max_content) = self.intrinsic_width(id);
        if self.measuring {
            self.items.push(Item::Atomic {
                node: id,
                width: max_content + horizontal,
                height: 0.0,
                above: 0.0,
                min: min_content + horizontal,
                laid: NO_FRAGMENTS,
                space_after: 0.0,
            });
            return;
        }
        let available = (cb - horizontal - extra).max(0.0);
        let content_width = match content_size(s, s.width, Some(cb), extra) {
            Some(w) => w,
            None => (max_content - extra)
                .min(available)
                .max(min_content - extra)
                .max(0.0),
        };
        let content_width = clamp_width(s, content_width, cb, extra);

        // Its own content runs inline layout, which reuses the scratch buffers of this line.
        let items = std::mem::take(&mut self.items);
        let words = std::mem::take(&mut self.words);
        let origin = self.ifc_origin;
        let start = (
            self.out.boxes.len(),
            self.out.texts.len(),
            self.out.clips.len(),
        );
        let height = self.build_box(id, margin[3], margin[0], content_width, cb, None, None);
        self.items = items;
        self.words = words;
        self.ifc_origin = origin;
        self.line_width = cb;

        let texts = start.1..self.out.texts.len();
        let outer_height = margin[0] + height + margin[2];
        // The baseline is that of its last line of text, or its bottom margin edge when it has
        // none or clips its overflow (CSS 2.1 10.8.1). Form controls clip but line up with the
        // text around them, empty or not, as in browsers.
        let control = matches!(
            self.dom.tag_name(id),
            Some("input" | "select" | "button" | "textarea")
        );
        let above = match texts.clone().last() {
            Some(i) if !s.clips || control => self.out.texts[i].baseline.min(outer_height),
            None if control => {
                let first_line = margin[0] + edges.border[0] + edges.padding[0];
                (first_line + self.extent(s).above).min(outer_height)
            }
            _ => outer_height,
        };
        self.atomics.push((
            start.0..self.out.boxes.len(),
            texts,
            start.2..self.out.clips.len(),
        ));
        self.items.push(Item::Atomic {
            node: id,
            width: content_width + extra + horizontal,
            height: outer_height,
            above,
            min: 0.0,
            laid: (self.atomics.len() - 1) as u32,
            space_after: 0.0,
        });
    }

    /// Flatten an inline node into items.
    fn collect(&mut self, id: NodeId) {
        match self.dom.kind(id) {
            NodeKind::Text(text) => self.collect_text(id, text),
            NodeKind::Element(_) => {
                let s = self.style(id);
                if s.display == Display::None {
                    return;
                }
                if s.position.is_out_of_flow() {
                    let (x, y) = self.ifc_origin;
                    self.defer(id, x, y);
                    return;
                }
                if s.float != Float::None {
                    if self.measuring {
                        // A float sits on a line beside the text, so it counts like an inline block.
                        let (min, max) = self.outer_intrinsic(id);
                        self.items.push(Item::Atomic {
                            node: id,
                            width: max,
                            height: 0.0,
                            above: 0.0,
                            min,
                            laid: NO_FRAGMENTS,
                            space_after: 0.0,
                        });
                    } else {
                        // It takes over the space before it, so a line may still break there
                        // and the words around it keep their gap.
                        let space_after = match self.items.last_mut() {
                            Some(
                                Item::Word { space_after, .. }
                                | Item::Atomic { space_after, .. }
                                | Item::Close { space_after, .. }
                                | Item::Float { space_after, .. },
                            ) => std::mem::take(space_after),
                            _ => 0.0,
                        };
                        self.items.push(Item::Float {
                            node: id,
                            space_after,
                        });
                    }
                    return;
                }
                match self.dom.tag_name(id) {
                    Some("br") => {
                        self.items.push(Item::Break);
                        if s.clear != Clear::None {
                            self.items.push(Item::Clear(s.clear));
                        }
                    }
                    Some("img") => {
                        let (w, h) = self.replaced_size(id, s, Some(self.line_width), None);
                        let edges = Edges::of(s, self.line_width);
                        let (width, height) = (w + edges.x(), h + edges.y());
                        self.items.push(Item::Atomic {
                            node: id,
                            width,
                            height,
                            above: height,
                            min: width,
                            laid: NO_FRAGMENTS,
                            space_after: 0.0,
                        });
                    }
                    _ if s.display.is_atomic_inline() => self.collect_inline_block(id),
                    _ => {
                        let block_inside = s.display != Display::Inline;
                        if block_inside {
                            self.items.push(Item::Break);
                        }
                        let decorated = !block_inside && self.decorated_inline(s);
                        if decorated {
                            let (margin, edge) = self.inline_sides(s, 3);
                            self.items.push(Item::Open {
                                node: id,
                                margin,
                                width: margin + edge,
                                space_after: 0.0,
                            });
                        }
                        let dom = self.dom;
                        let [before, after] = dom.generated(id);
                        if let Some(b) = before {
                            self.collect(b);
                        }
                        for child in dom.children(id) {
                            self.collect(child);
                        }
                        if let Some(a) = after {
                            self.collect(a);
                        }
                        if decorated {
                            let (margin, edge) = self.inline_sides(s, 1);
                            self.items.push(Item::Close {
                                node: id,
                                margin,
                                width: margin + edge,
                                space_after: 0.0,
                            });
                        }
                        if block_inside {
                            self.items.push(Item::Break);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_text(&mut self, id: NodeId, text: &str) {
        let s = self.style(id);
        let size = s.font_size.to_f32();
        let bold = s.font_weight >= 600;
        let space = self.metrics.width(" ", size, bold);
        match s.white_space {
            WhiteSpace::Normal | WhiteSpace::NoWrap => {
                let breakable = s.white_space == WhiteSpace::Normal;
                if text.starts_with(|c: char| c.is_ascii_whitespace()) {
                    self.mark_space(space);
                }
                for word in text.split_ascii_whitespace() {
                    self.push_word(id, word, size, bold, breakable);
                    self.mark_space(space);
                }
                if !text.ends_with(|c: char| c.is_ascii_whitespace()) {
                    if let Some(Item::Word { space_after, .. }) = self.items.last_mut() {
                        *space_after = 0.0;
                    }
                }
            }
            WhiteSpace::Pre | WhiteSpace::PreWrap => {
                let breakable = s.white_space == WhiteSpace::PreWrap;
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        self.items.push(Item::Break);
                    }
                    let line = line.replace('\t', "    ");
                    if breakable {
                        for (j, word) in line.split(' ').enumerate() {
                            if j > 0 {
                                self.mark_space(space);
                            }
                            if !word.is_empty() {
                                self.push_word(id, word, size, bold, true);
                            }
                        }
                    } else if !line.is_empty() {
                        self.push_word(id, &line, size, bold, false);
                    }
                }
            }
        }
    }

    fn push_word(&mut self, node: NodeId, word: &str, size: f32, bold: bool, breakable: bool) {
        let start = self.words.len() as u32;
        self.words.push_str(word);
        self.items.push(Item::Word {
            node,
            start,
            len: word.len() as u32,
            width: self.metrics.width(word, size, bold),
            space_after: 0.0,
            breakable,
        });
    }

    /// Collapsible white space: at most one space between words, none at the start of the run.
    fn mark_space(&mut self, width: f32) {
        if let Some(
            Item::Word { space_after, .. }
            | Item::Atomic { space_after, .. }
            | Item::Open { space_after, .. }
            | Item::Close { space_after, .. }
            | Item::Float { space_after, .. },
        ) = self.items.last_mut()
        {
            *space_after = width;
        }
    }

    fn extent(&self, s: &ComputedStyle) -> Extent {
        let size = s.font_size.to_f32();
        let ascent = self.metrics.ascent_em() * size;
        let descent = self.metrics.descent_em() * size;
        let line_height = match s.line_height {
            LineHeight::Normal => ascent + descent + self.metrics.line_gap_em() * size,
            LineHeight::Factor(f) => size * f as f32 / 100.0,
            LineHeight::Px(px) => px.to_f32(),
        };
        let half_leading = (line_height - ascent - descent) / 2.0;
        Extent {
            above: ascent + half_leading,
            below: descent + half_leading,
        }
    }

    fn item_extent(&self, item: &Item) -> Extent {
        match *item {
            Item::Word { node, .. } => self.extent(self.style(node)),
            Item::Atomic { height, above, .. } => Extent {
                above,
                below: height - above,
            },
            // Inline padding and borders do not make the line taller.
            Item::Open { .. }
            | Item::Close { .. }
            | Item::Float { .. }
            | Item::Break
            | Item::Clear(_) => Extent::default(),
        }
    }
}

fn item_width(item: &Item) -> f32 {
    match *item {
        Item::Word { width, .. }
        | Item::Atomic { width, .. }
        | Item::Open { width, .. }
        | Item::Close { width, .. } => width,
        Item::Float { .. } | Item::Break | Item::Clear(_) => 0.0,
    }
}

fn space_after(item: &Item) -> f32 {
    match *item {
        Item::Word { space_after, .. }
        | Item::Atomic { space_after, .. }
        | Item::Open { space_after, .. }
        | Item::Close { space_after, .. }
        | Item::Float { space_after, .. } => space_after,
        Item::Break | Item::Clear(_) => 0.0,
    }
}

/// Whether a line may break after this item.
fn breakable(item: &Item) -> bool {
    match *item {
        Item::Word { breakable, .. } => breakable,
        // Never between an element's start and its first content; after its end only where
        // white space follows (`foo</b>bar` is one word).
        Item::Open { .. } => false,
        Item::Close { space_after, .. } | Item::Float { space_after, .. } => space_after > 0.0,
        Item::Atomic { .. } | Item::Break | Item::Clear(_) => true,
    }
}

/// A length against a containing-block dimension; `None` for `auto`.
fn resolve(l: Length, base: f32) -> Option<f32> {
    match l {
        Length::Px(px) => Some(px.to_f32()),
        Length::Percent(p) => Some(base * p as f32 / 10_000.0),
        Length::Auto => None,
    }
}

/// Padding (percentages of the containing block's width) and border widths, per side.
#[derive(Clone, Copy)]
struct Edges {
    padding: [f32; 4],
    border: [f32; 4],
}

impl Edges {
    fn of(s: &ComputedStyle, cb_width: f32) -> Edges {
        Edges {
            padding: s.padding.map(|p| resolve(p, cb_width).unwrap_or(0.0)),
            border: s.border_width.map(|b| b.to_f32()),
        }
    }

    /// Horizontal padding plus border.
    fn x(&self) -> f32 {
        self.padding[1] + self.padding[3] + self.border[1] + self.border[3]
    }

    fn y(&self) -> f32 {
        self.padding[0] + self.padding[2] + self.border[0] + self.border[2]
    }
}

/// A `width`/`height`-like length as a content-box size. Percentages need a definite `base`;
/// with `box-sizing: border-box` the length includes the padding and border in `extra`.
fn content_size(s: &ComputedStyle, l: Length, base: Option<f32>, extra: f32) -> Option<f32> {
    let v = match l {
        Length::Percent(_) => resolve(l, base?)?,
        _ => resolve(l, 0.0)?,
    };
    Some(if s.border_box {
        (v - extra).max(0.0)
    } else {
        v
    })
}

/// Apply `min-width` and `max-width` (min wins) to a content-box width.
fn clamp_width(s: &ComputedStyle, w: f32, cb_width: f32, extra: f32) -> f32 {
    let w = content_size(s, s.max_width, Some(cb_width), extra).map_or(w, |max| w.min(max));
    content_size(s, s.min_width, Some(cb_width), extra).map_or(w, |min| w.max(min))
}

/// Adjoining vertical margins: the larger positive plus the more negative.
fn collapse(a: f32, b: f32) -> f32 {
    match (a >= 0.0, b >= 0.0) {
        (true, true) => a.max(b),
        (false, false) => a.min(b),
        _ => a + b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_runs_that_reach_the_view() {
        // Three runs: 0..256 spans 0-100, 256..512 spans 90-200, 512..600 spans 500-600.
        let extents = [(0.0, 100.0), (90.0, 200.0), (500.0, 600.0)];
        let pick = |range: Range<usize>, top, bottom| -> Vec<usize> {
            Layout::in_view(&extents, range, top, bottom).collect()
        };
        assert_eq!(pick(0..600, 150.0, 160.0), (256..512).collect::<Vec<_>>());
        assert_eq!(pick(0..600, 95.0, 96.0).len(), 512, "both runs reach it");
        assert_eq!(pick(0..600, 100.0, 100.0).len(), 512, "edges count");
        assert!(pick(0..600, 300.0, 400.0).is_empty());
        assert_eq!(
            pick(520..560, 550.0, 560.0),
            (520..560).collect::<Vec<_>>(),
            "a layer's range"
        );
        assert_eq!(pick(250..260, 0.0, 1000.0), (250..260).collect::<Vec<_>>());
    }
    use css::{cascade, parse_stylesheet, user_agent_stylesheet, Media, Origin};
    use font::Approximate;

    /// Approximate metrics: every character 0.5em, space 0.25em, ascent 0.8, descent 0.2.
    fn run(html_src: &str, css_src: &str, width: f32) -> (Dom, Layout) {
        let mut dom = html::parse(html_src);
        let author = parse_stylesheet(css_src);
        let media = Media {
            width,
            height: 600.0,
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
        let layout = layout(&dom, &styles, &Approximate, &no_images, width, 600.0);
        (dom, layout)
    }

    fn rect(dom: &Dom, l: &Layout, tag: &str) -> Rect {
        let id = dom.find_first(tag).unwrap();
        l.boxes
            .iter()
            .find(|b| b.node == id)
            .unwrap_or_else(|| panic!("no box for <{tag}>"))
            .rect
    }

    fn lines(l: &Layout) -> Vec<(f32, String)> {
        let mut out: Vec<(f32, String)> = Vec::new();
        for f in &l.texts {
            match out.last_mut() {
                Some((y, text)) if *y == f.baseline => {
                    text.push('|');
                    text.push_str(l.text(f));
                }
                _ => out.push((f.baseline, l.text(f).to_string())),
            }
        }
        out
    }

    const RESET: &str = "body { margin: 0 } p { margin: 0 }";

    #[test]
    fn stacks_blocks_and_collapses_sibling_margins() {
        let (dom, l) = run(
            "<body><div id=a></div><div id=b></div></body>",
            "body { margin: 0 } div { height: 10px; margin: 20px 0 30px }",
            800.0,
        );
        let divs: Vec<Rect> = l
            .boxes
            .iter()
            .filter(|b| dom.tag_name(b.node) == Some("div"))
            .map(|b| b.rect)
            .collect();
        assert_eq!(divs[0].y, 20.0);
        assert_eq!(
            divs[1].y,
            20.0 + 10.0 + 30.0,
            "30px and 20px collapse to 30px"
        );
        assert_eq!(divs[0].width, 800.0);
    }

    #[test]
    fn centers_with_auto_margins_and_max_width() {
        let (dom, l) = run(
            "<body><main>x</main></body>",
            "body { margin: 0 } main { max-width: 400px; margin: 0 auto; padding: 0 10px }",
            1000.0,
        );
        let main = rect(&dom, &l, "main");
        assert_eq!(main.width, 420.0, "400 content + 20 padding");
        assert_eq!(main.x, 290.0, "(1000 - 420) / 2");
    }

    #[test]
    fn resolves_percent_widths_padding_and_borders() {
        let (dom, l) = run(
            "<body><div><p>x</p></div></body>",
            "body { margin: 0 } div { width: 50%; padding: 5%; border: 2px solid } p { margin: 0 }",
            1000.0,
        );
        let div = rect(&dom, &l, "div");
        assert_eq!(div.width, 500.0 + 100.0 + 4.0);
        let p = rect(&dom, &l, "p");
        assert_eq!(p.x, 2.0 + 50.0);
        assert_eq!(p.width, 500.0);
    }

    #[test]
    fn wraps_text_and_collapses_white_space() {
        // 16px font: each letter 8px wide, a space 4px. "aaaa bbbb cccc" is 32+4+32+4+32 = 104px.
        let (_, l) = run("<p>  aaaa \n bbbb   cccc  </p>", RESET, 72.0);
        let lines = lines(&l);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1, "aaaa bbbb");
        assert_eq!(lines[1].1, "cccc");
        assert_eq!(l.texts[0].x, 0.0, "leading white space is dropped");
    }

    #[test]
    fn keeps_long_words_on_their_own_line() {
        let (_, l) = run("<p>a bbbbbbbbbbbbbbbbbbbb c</p>", RESET, 40.0);
        let lines = lines(&l);
        assert_eq!(
            lines.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
            ["a", "bbbbbbbbbbbbbbbbbbbb", "c"]
        );
    }

    #[test]
    fn respects_pre_nowrap_and_br() {
        let (_, l) = run(
            "<pre>a  b\nc</pre>",
            "body { margin: 0 } pre { margin: 0 }",
            20.0,
        );
        let text: Vec<String> = lines(&l).into_iter().map(|(_, t)| t).collect();
        assert_eq!(text, ["a  b", "c"]);

        let (_, l) = run(
            "<p>aaaa bbbb cccc</p>",
            "body { margin: 0 } p { margin: 0; white-space: nowrap }",
            40.0,
        );
        assert_eq!(lines(&l).len(), 1);

        let (_, l) = run("<p>a<br>b<br></p>", RESET, 400.0);
        assert_eq!(lines(&l).len(), 2, "a trailing <br> adds no empty line");
    }

    #[test]
    fn merges_styled_runs_on_one_line() {
        let (_, l) = run("<p>one <b>two</b> three</p>", RESET, 800.0);
        let lines = lines(&l);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].1, "one|two|three");
    }

    #[test]
    fn aligns_text_and_sizes_lines() {
        let (_, l) = run(
            "<p>ab<br>cd</p>",
            "body { margin: 0 } p { margin: 0; text-align: center; line-height: 30px }",
            100.0,
        );
        let f = l.texts[0];
        assert_eq!(f.x, 42.0, "(100 - 16) / 2");
        let spacing = l.texts[1].baseline - f.baseline;
        assert!((spacing - 30.0).abs() < 0.01, "line spacing {spacing}");
        // Half-leading: (30 - 16) / 2 = 7 above the 12.8px ascent.
        assert!(
            (f.baseline - (7.0 + 12.8)).abs() < 0.01,
            "baseline {}",
            f.baseline
        );
    }

    #[test]
    fn mixes_inline_and_block_children_with_anonymous_blocks() {
        let (dom, l) = run(
            "<div>before<p>para</p>after</div>",
            "body { margin: 0 } p { margin: 0 }",
            400.0,
        );
        let p = rect(&dom, &l, "p");
        let lines = lines(&l);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].0 < p.y && p.y < lines[2].0);
    }

    #[test]
    fn lays_out_images_from_attributes() {
        let (dom, l) = run("<p><img width=40 height=30>x</p>", RESET, 400.0);
        let img = rect(&dom, &l, "img");
        assert_eq!((img.width, img.height), (40.0, 30.0));
        assert_eq!(l.texts[0].x, 40.0);
    }

    #[test]
    fn skips_hidden_content() {
        let (_, l) = run(
            "<p>shown</p><p style='display:none'>hidden</p><head><title>t</title></head>",
            RESET,
            400.0,
        );
        assert_eq!(
            lines(&l)
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>(),
            ["shown"]
        );
    }

    #[test]
    fn document_height_includes_overflow() {
        let (_, l) = run(
            "<html><body><div>x</div></body></html>",
            "html, body { height: 100%; margin: 0 } div { height: 2000px }",
            300.0,
        );
        assert_eq!(l.height, 2000.0);
    }

    #[test]
    fn places_absolute_boxes_against_their_containing_block_in_a_new_layer() {
        let (dom, l) = run(
            "<div id=c><p>flow</p><span id=a>abs</span></div><p id=after>after</p>",
            "body { margin: 0 } p { margin: 0 } #c { position: relative; margin: 10px; padding: 5px; height: 100px } \
             #a { position: absolute; top: 3px; left: 4px }",
            400.0,
        );
        let a = dom.find_first("span").unwrap();
        let abs = l.boxes.iter().find(|b| b.node == a).unwrap().rect;
        assert_eq!(
            (abs.x, abs.y),
            (10.0 + 4.0, 10.0 + 3.0),
            "padding box of #c plus the insets"
        );
        assert_eq!(abs.width, 24.0, "shrink-to-fit: three 8px glyphs");
        assert_eq!(l.layers.len(), 2);
        assert!(
            l.layers[1].boxes as usize > 0,
            "the absolute box comes after normal flow"
        );
        assert!(!l.text_is_fixed(l.texts.len() - 1));
        // The absolute box takes no space in flow: #after follows #c directly.
        let after = l
            .boxes
            .iter()
            .find(|b| dom.attr(b.node, "id") == Some("after"))
            .unwrap()
            .rect;
        assert_eq!(after.y, 10.0 + 110.0 + 10.0);
    }

    #[test]
    fn anchors_to_the_right_and_bottom_and_keeps_fixed_boxes_out_of_the_scroll_height() {
        let (dom, l) = run(
            "<div id=r>ab</div><div id=f>fixed</div><div id=tall></div>",
            "body { margin: 0 } #r { position: absolute; right: 10px; bottom: 20px } \
             #f { position: fixed; top: 0; left: 0; width: 100%; height: 5000px } #tall { height: 50px }",
            300.0,
        );
        let rect = |id: &str| {
            l.boxes
                .iter()
                .find(|b| dom.attr(b.node, "id") == Some(id))
                .unwrap()
                .rect
        };
        let r = rect("r");
        assert_eq!((r.x + r.width, r.width), (290.0, 16.0));
        assert!(
            (r.y + r.height - 580.0).abs() < 0.01,
            "bottom edge 20px above the 600px viewport"
        );
        let f = rect("f");
        assert_eq!(f.width, 300.0);
        assert!(l.box_is_fixed(
            l.boxes
                .iter()
                .position(|b| dom.attr(b.node, "id") == Some("f"))
                .unwrap()
        ));
        assert_eq!(
            l.height, 600.0,
            "a tall fixed box does not make the page scroll"
        );
    }

    #[test]
    fn shifts_relative_boxes_with_their_content() {
        let (dom, l) = run(
            "<p>x</p>",
            "body { margin: 0 } p { margin: 0; position: relative; top: 7px; left: -3px }",
            300.0,
        );
        let p = rect(&dom, &l, "p");
        assert_eq!((p.x, p.y), (-3.0, 7.0));
        assert_eq!(l.texts[0].x, -3.0);
    }

    #[test]
    fn records_overflow_clips() {
        let (dom, l) = run(
            "<div><p>inside</p></div>",
            "body { margin: 0 } p { margin: 0 } div { height: 5px; overflow: hidden; padding: 2px; border: 1px solid }",
            300.0,
        );
        assert_eq!(l.clips.len(), 1);
        let clip = l.clips[0];
        let div = rect(&dom, &l, "div");
        assert_eq!(
            clip.rect,
            Rect {
                x: 1.0,
                y: 1.0,
                width: div.width - 2.0,
                height: 9.0
            }
        );
        assert_eq!(clip.texts, (0, 1));
        assert_eq!(
            clip.boxes.1 - clip.boxes.0,
            1,
            "the p inside, not the div itself"
        );
    }

    fn by_id(dom: &Dom, l: &Layout, id: &str) -> Rect {
        l.boxes
            .iter()
            .find(|b| dom.attr(b.node, "id") == Some(id))
            .unwrap_or_else(|| panic!("no box for #{id}"))
            .rect
    }

    const FLEX_BASE: &str = "body { margin: 0 } p { margin: 0 } ";

    #[test]
    fn flex_rows_size_items_to_content_and_stretch_them() {
        let (dom, l) = run(
            "<div><a id=a>ab</a><em id=b>cde</em><i id=c>f<br>g</i></div>",
            &format!("{FLEX_BASE} div {{ display: flex }}"),
            400.0,
        );
        let (a, b, c) = (
            by_id(&dom, &l, "a"),
            by_id(&dom, &l, "b"),
            by_id(&dom, &l, "c"),
        );
        assert_eq!(
            (a.x, a.width),
            (0.0, 16.0),
            "max-content width, inline elements become items"
        );
        assert_eq!((b.x, b.width), (16.0, 24.0));
        assert_eq!(c.x, 40.0);
        assert_eq!(c.height, 32.0, "two 16px lines");
        assert_eq!(
            (a.height, b.height),
            (c.height, c.height),
            "stretched to the line"
        );
    }

    #[test]
    fn flex_grow_and_shrink_respect_factors_and_minimums() {
        let (dom, l) = run(
            "<div class=f><p id=x></p><p id=y></p></div>\
             <div class=g><p id=long>aaaaaaaaaa</p><p id=short></p></div>",
            &format!(
                "{FLEX_BASE} .f {{ display: flex; width: 300px }} #x {{ flex: 1 }} #y {{ flex: 2 }} \
                 .g {{ display: flex; width: 150px }} .g p {{ flex: 0 1 100px }}"
            ),
            400.0,
        );
        assert_eq!(by_id(&dom, &l, "x").width, 100.0);
        assert_eq!(by_id(&dom, &l, "y").width, 200.0);
        // Shrinking 50px equally would make both 75px, but the long word needs 80px.
        assert_eq!(by_id(&dom, &l, "long").width, 80.0);
        assert_eq!(by_id(&dom, &l, "short").width, 70.0);
    }

    #[test]
    fn flex_justify_content_and_auto_margins() {
        let css = |extra: &str| {
            format!("{FLEX_BASE} div {{ display: flex; width: 300px }} p {{ width: 20px; height: 10px }} {extra}")
        };
        let xs = |extra: &str| {
            let (dom, l) = run(
                "<div><p id=a></p><p id=b></p><p id=c></p></div>",
                &css(extra),
                400.0,
            );
            ["a", "b", "c"].map(|id| by_id(&dom, &l, id).x)
        };
        assert_eq!(
            xs("div { justify-content: space-between }"),
            [0.0, 140.0, 280.0]
        );
        assert_eq!(xs("div { justify-content: center }"), [120.0, 140.0, 160.0]);
        assert_eq!(
            xs("div { justify-content: flex-end; gap: 5px }"),
            [230.0, 255.0, 280.0]
        );
        assert_eq!(xs("#c { margin-left: auto }"), [0.0, 20.0, 280.0]);
        assert_eq!(
            xs("div { flex-direction: row-reverse }"),
            [280.0, 260.0, 240.0]
        );
        assert_eq!(xs("#a { order: 2 }"), [40.0, 0.0, 20.0]);
    }

    #[test]
    fn flex_aligns_on_the_cross_axis() {
        let (dom, l) = run(
            "<div><p id=a></p><p id=b></p><p id=c></p></div>",
            &format!(
                "{FLEX_BASE} div {{ display: flex; height: 100px; align-items: center }} \
                 p {{ width: 10px; height: 20px }} #b {{ align-self: flex-end }} #c {{ align-self: stretch; height: auto }}"
            ),
            400.0,
        );
        assert_eq!(by_id(&dom, &l, "a").y, 40.0);
        assert_eq!(by_id(&dom, &l, "b").y, 80.0);
        let c = by_id(&dom, &l, "c");
        assert_eq!((c.y, c.height), (0.0, 100.0));
    }

    #[test]
    fn flex_wraps_rows() {
        let (dom, l) = run(
            "<div><p id=a></p><p id=b></p><p id=c></p></div><p id=after></p>",
            &format!(
                "{FLEX_BASE} div {{ display: flex; flex-wrap: wrap; width: 100px; row-gap: 6px; column-gap: 4px }} \
                 p {{ width: 40px; height: 10px }}"
            ),
            400.0,
        );
        let c = by_id(&dom, &l, "c");
        assert_eq!(by_id(&dom, &l, "b").x, 44.0);
        assert_eq!((c.x, c.y), (0.0, 16.0), "third item wraps below the gap");
        assert_eq!(by_id(&dom, &l, "after").y, 26.0);
    }

    #[test]
    fn flex_columns_grow_into_a_definite_height() {
        let (dom, l) = run(
            "<div><p id=a>x</p><p id=b></p><p id=c>hi</p></div>",
            &format!(
                "{FLEX_BASE} div {{ display: flex; flex-direction: column; height: 200px; width: 300px }} \
                 #a, #b {{ flex: 1 }} #c {{ align-self: center; flex: none }}"
            ),
            400.0,
        );
        let (a, b, c) = (
            by_id(&dom, &l, "a"),
            by_id(&dom, &l, "b"),
            by_id(&dom, &l, "c"),
        );
        assert_eq!(a.width, 300.0, "stretched across");
        assert!((a.height - b.height).abs() < 0.01);
        assert!((a.height + b.height + c.height - 200.0).abs() < 0.01);
        assert_eq!(b.y, a.y + a.height);
        assert_eq!((c.width, c.x), (16.0, 142.0), "centered and shrink-to-fit");
        assert!(l
            .texts
            .iter()
            .any(|t| l.text(t) == "x" && t.baseline < a.y + a.height));
    }

    #[test]
    fn border_box_sizes_include_padding_and_border() {
        let (dom, l) = run(
            "<div></div>",
            "body { margin: 0 } div { width: 100px; height: 50px; padding: 10px; border: 5px solid; box-sizing: border-box }",
            400.0,
        );
        let d = rect(&dom, &l, "div");
        assert_eq!((d.width, d.height), (100.0, 50.0));
    }

    #[test]
    fn flex_wraps_text_in_anonymous_items() {
        let (dom, l) = run(
            "<div>hello <b>x</b></div>",
            "body { margin: 0 } div { display: flex }",
            400.0,
        );
        let hello = l.texts.iter().find(|t| l.text(t) == "hello").unwrap();
        assert_eq!(hello.x, 0.0);
        assert_eq!(
            rect(&dom, &l, "b").x,
            40.0,
            "white space at the end of an anonymous item is dropped"
        );
    }

    #[test]
    fn absolute_boxes_follow_ancestors_moved_after_layout() {
        let (dom, l) = run(
            "<div class=c><p id=r><span id=a></span></p></div>",
            "body { margin: 0 } .c { display: flex; align-items: center; justify-content: center; width: 100px; height: 100px } \
             #r { position: relative; margin: 0; width: 20px; height: 20px } #a { position: absolute; top: 2px; left: 3px; width: 5px; height: 5px } \
             span { display: block }",
            400.0,
        );
        let r = by_id(&dom, &l, "r");
        let a = by_id(&dom, &l, "a");
        assert_eq!((r.x, r.y), (40.0, 40.0));
        assert_eq!(
            (a.x, a.y),
            (43.0, 42.0),
            "the containing block is read after #r was centered"
        );
    }

    #[test]
    fn inline_blocks_sit_on_the_line_and_shrink_to_fit() {
        let (dom, l) = run(
            "<p>ab <span id=ib>cd</span> ef</p><div><span id=f1>one</span><span id=f2><div>xy</div></span></div>",
            "body { margin: 0 } p { margin: 0 } #ib { display: inline-block; padding: 0 2px; margin-left: 1px } \
             #f1 { display: inline-flex } #f2 { display: inline-block; border: 1px solid }",
            400.0,
        );
        let ib = by_id(&dom, &l, "ib");
        assert_eq!(
            (ib.x, ib.width),
            (21.0, 20.0),
            "after \"ab \" and its margin; 16px of text plus padding"
        );
        let text = |w: &str| *l.texts.iter().find(|t| l.text(t) == w).unwrap();
        let (ab, cd, ef) = (text("ab"), text("cd"), text("ef"));
        assert_eq!(cd.baseline, ab.baseline, "baseline aligned with the line");
        assert_eq!(cd.x, 23.0);
        assert_eq!(ef.x, 21.0 + 20.0 + 4.0, "the line continues after it");
        let (f1, f2) = (by_id(&dom, &l, "f1"), by_id(&dom, &l, "f2"));
        assert_eq!((f1.x, f1.width), (0.0, 24.0));
        assert_eq!(
            (f2.x, f2.width),
            (24.0, 18.0),
            "a block inside shrinks to its text, plus borders"
        );
        assert_eq!(
            text("one").baseline,
            text("xy").baseline,
            "their last lines share the baseline"
        );
        assert_eq!(f1.y, f2.y + 1.0, "so the bordered one sits a border higher");
    }

    #[test]
    fn inline_blocks_wrap_as_one_piece() {
        let (dom, l) = run(
            "<p>aaaa <span id=ib>bb cc</span></p>",
            "body { margin: 0 } p { margin: 0; width: 70px } #ib { display: inline-block }",
            400.0,
        );
        let ib = by_id(&dom, &l, "ib");
        assert_eq!(
            (ib.x, ib.width),
            (0.0, 36.0),
            "moves to the next line whole"
        );
        assert!((ib.y - 16.0).abs() < 0.01, "on the second line");
    }

    #[test]
    fn grid_places_named_areas_like_a_page_layout() {
        let (dom, l) = run(
            "<div class=page><p id=n>notice</p><p id=s>side</p><p id=m>main<br>more</p><p id=f>foot</p></div>",
            "body { margin: 0 } p { margin: 0 } \
             .page { display: grid; width: 800px; column-gap: 24px; \
               grid-template: min-content 1fr min-content / 12.5rem minmax(0,1fr); \
               grid-template-areas: 'notice notice' 'side main' 'foot foot' } \
             #n { grid-area: notice } #s { grid-area: side } #m { grid-area: main } #f { grid-area: foot }",
            1000.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!(
            (r("n").x, r("n").width),
            (0.0, 800.0),
            "spans both columns and the gap"
        );
        assert_eq!((r("s").x, r("s").width), (0.0, 200.0), "12.5rem");
        assert_eq!(
            (r("m").x, r("m").width),
            (224.0, 576.0),
            "the rest after the gap"
        );
        assert_eq!(r("s").y, r("m").y, "same row");
        assert_eq!(
            r("s").height,
            r("m").height,
            "stretched to the row, which fits two lines"
        );
        assert_eq!(r("f").y, r("m").y + r("m").height);
    }

    #[test]
    fn grid_shares_fr_and_auto_places_into_new_rows() {
        let (dom, l) = run(
            "<div><p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e class=wide></p></div>",
            "body { margin: 0 } p { margin: 0; height: 10px } \
             div { display: grid; width: 320px; grid-template-columns: repeat(3, 1fr); gap: 5px 10px } \
             .wide { grid-column: 1 / -1 }",
            1000.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!([r("a").x, r("b").x, r("c").x], [0.0, 110.0, 220.0]);
        assert_eq!(r("a").width, 100.0);
        assert_eq!(
            (r("d").x, r("d").y),
            (0.0, 15.0),
            "wraps to the second row below the gap"
        );
        assert_eq!(
            (r("e").x, r("e").y, r("e").width),
            (0.0, 30.0, 320.0),
            "a full row after it"
        );
    }

    #[test]
    fn grid_sizes_auto_tracks_to_content_and_fills_auto_repeat() {
        let (dom, l) = run(
            "<div class=g><p id=label>abcd</p><p id=value>x</p></div>\
             <div class=h><p id=one></p><p id=two></p><p id=three></p><p id=four></p></div>",
            "body { margin: 0 } p { margin: 0 } \
             .g { display: grid; width: 300px; grid-template-columns: auto 1fr } \
             .h { display: grid; width: 350px; grid-template-columns: repeat(auto-fill, minmax(100px, 1fr)) }",
            1000.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!(
            r("label").width,
            32.0,
            "auto column fits its content when fr takes the rest"
        );
        assert_eq!((r("value").x, r("value").width), (32.0, 268.0));
        assert!(
            (r("one").width - 350.0 / 3.0).abs() < 0.01,
            "three 100px columns fit, sharing the rest"
        );
        assert_eq!(r("four").x, 0.0, "the fourth starts a new row");
    }

    #[test]
    fn grid_aligns_spans_and_definite_lines() {
        let (dom, l) = run(
            "<div><p id=tall></p><p id=mid></p><p id=last></p></div>",
            "body { margin: 0 } p { margin: 0 } \
             div { display: grid; width: 200px; grid-template-columns: 100px 100px } \
             #tall { height: 60px; grid-row: span 2 } #mid { height: 10px; align-self: center } \
             #last { grid-row: 2; grid-column: 2; height: 10px }",
            1000.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!((r("mid").x, r("last").x), (100.0, 100.0));
        assert_eq!(r("tall").height, 60.0);
        let row1 = r("last").y;
        assert!(
            (row1 - 30.0).abs() < 0.01,
            "the span's 40px extra is split evenly over its two rows: {row1}"
        );
        assert!(
            (r("mid").y - 10.0).abs() < 0.01,
            "centered in the 30px first row"
        );
    }

    /// Every <img> is a 200x100 file.
    fn run_with_images(html_src: &str, css_src: &str, width: f32) -> (Dom, Layout) {
        let dom = html::parse(html_src);
        let author = parse_stylesheet(css_src);
        let media = Media {
            width,
            height: 600.0,
            ..Media::default()
        };
        let styles = cascade(
            &dom,
            &[
                (Origin::UserAgent, user_agent_stylesheet()),
                (Origin::Author, &author),
            ],
            &media,
        );
        let natural = |id: NodeId| (dom.tag_name(id) == Some("img")).then_some((200.0, 100.0));
        let layout = layout(&dom, &styles, &Approximate, &natural, width, 600.0);
        (dom, layout)
    }

    #[test]
    fn sizes_images_from_their_files_and_keeps_proportions() {
        let (dom, l) = run_with_images(
            "<p><img id=n></p><p><img id=w width=50></p><p><img id=h style='height: 20px'></p>\
             <div class=narrow><img id=fit></div><img id=block class=b><img id=css style='width: 30px; height: 30px'>",
            "body { margin: 0 } p { margin: 0 } .narrow { width: 80px } .narrow img { max-width: 100%; height: auto } \
             .b { display: block; margin: 0 auto }",
            400.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!(
            (r("n").width, r("n").height),
            (200.0, 100.0),
            "natural size"
        );
        assert_eq!(
            (r("w").width, r("w").height),
            (50.0, 25.0),
            "width attribute, height from the ratio"
        );
        assert_eq!(
            (r("h").width, r("h").height),
            (40.0, 20.0),
            "CSS height, width from the ratio"
        );
        assert_eq!(
            (r("fit").width, r("fit").height),
            (80.0, 40.0),
            "max-width: 100% scales both sides"
        );
        assert_eq!(
            (r("block").x, r("block").width, r("block").height),
            (100.0, 200.0, 100.0),
            "a centered block"
        );
        assert_eq!(
            (r("css").width, r("css").height),
            (30.0, 30.0),
            "both sides given: stretched"
        );
    }

    #[test]
    fn images_in_flex_and_grid_keep_their_size() {
        let (dom, l) = run_with_images(
            "<div class=f><img id=a><p id=t>text</p></div><div class=g><img id=b></div>",
            "body { margin: 0 } p { margin: 0 } .f { display: flex; align-items: flex-start } \
             .g { display: grid; grid-template-columns: 300px }",
            400.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!((r("a").width, r("a").height), (200.0, 100.0));
        assert_eq!(r("t").x, 200.0);
        assert_eq!(
            (r("b").width, r("b").height),
            (200.0, 100.0),
            "not stretched across the 300px track"
        );
        let (dom, l) = run("<img id=x width=40 height=10>", "body { margin: 0 }", 400.0);
        assert_eq!(
            (by_id(&dom, &l, "x").width, by_id(&dom, &l, "x").height),
            (40.0, 10.0),
            "unloaded: the attributes"
        );
    }

    #[test]
    fn tables_size_columns_to_their_content() {
        let (dom, l) = run(
            "<table id=t><tr><td id=a>a</td><td id=b>bbbb</td></tr><tr><td id=c>cc</td><td id=d>d</td></tr></table><p id=after>x</p>",
            "body { margin: 0 } p { margin: 0 }",
            400.0,
        );
        let r = |id| by_id(&dom, &l, id);
        // Columns: max(a, cc) + 2px padding = 18, bbbb + 2 = 34; 2px spacing around them.
        assert_eq!(
            r("t").width,
            2.0 + 18.0 + 2.0 + 34.0 + 2.0,
            "shrinks to its columns"
        );
        assert_eq!((r("a").x, r("a").width), (2.0, 18.0));
        assert_eq!((r("b").x, r("b").width), (22.0, 34.0));
        assert_eq!(
            (r("c").x, r("c").y),
            (2.0, r("a").y + r("a").height + 2.0),
            "second row below the spacing"
        );
        assert_eq!(r("d").x, r("b").x, "columns line up");
        assert_eq!(r("after").y, r("t").y + r("t").height);
    }

    #[test]
    fn tables_fill_widths_spans_and_align_cells() {
        let (dom, l) = run(
            "<table id=t width=85% align=center cellspacing=0 cellpadding=0 bgcolor=#ff6600>\
               <tr id=row bgcolor=#eeeeee><td id=wide colspan=2>header</td><td id=tall rowspan=2>a<br>b<br>c<br>d</td></tr>\
               <tr><td id=x>x</td><td id=y valign=bottom>y</td></tr></table>",
            "body { margin: 0 } td { line-height: 10px }",
            400.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!(
            (r("t").x, r("t").width),
            (30.0, 340.0),
            "85% wide, centered"
        );
        assert_eq!(r("wide").x, 0.0 + 30.0);
        assert_eq!(
            r("wide").width,
            r("x").width + r("y").width,
            "spans two columns"
        );
        let tall = r("tall");
        assert_eq!(tall.height, 40.0, "four lines over two rows");
        assert_eq!(
            r("x").y + r("x").height,
            tall.y + tall.height,
            "the rows share the spanning cell's height"
        );
        let row = r("row");
        assert_eq!(
            (row.x, row.width),
            (30.0, 340.0),
            "the row's box spans the table for its background"
        );
        let text = |w: &str| *l.texts.iter().find(|t| l.text(t) == w).unwrap();
        let y_cell = r("y");
        assert!(
            text("y").baseline > y_cell.y + y_cell.height - 10.0,
            "valign=bottom"
        );
        let x_cell = r("x");
        let middle = x_cell.y + x_cell.height / 2.0;
        assert!(
            (text("x").baseline - 5.0 - middle).abs() < 4.0,
            "td is vertically centered by default"
        );
    }

    #[test]
    fn inline_margins_padding_and_backgrounds_take_space_and_draw() {
        let (dom, l) = run(
            "<p>ab <span class=m>cd</span> ef <em class=pad>gh</em> <b>plain</b></p>",
            "body { margin: 0 } p { margin: 0 } .m { margin: 0 10px } \
             .pad { padding: 2px 4px; background-color: #eee; border: 1px solid }",
            400.0,
        );
        let text = |w: &str| *l.texts.iter().find(|t| l.text(t) == w).unwrap();
        // "ab " is 20px; the span's left margin adds 10.
        assert_eq!(text("cd").x, 30.0);
        assert_eq!(
            text("ef").x,
            30.0 + 16.0 + 10.0 + 4.0,
            "right margin, then the space"
        );
        let em = dom.find_first("em").unwrap();
        let boxes: Vec<Rect> = l
            .boxes
            .iter()
            .filter(|b| b.node == em)
            .map(|b| b.rect)
            .collect();
        assert_eq!(boxes.len(), 1);
        let gh = text("gh");
        assert_eq!(boxes[0].x, gh.x - 5.0, "border and padding before the text");
        assert_eq!(boxes[0].width, 16.0 + 10.0);
        assert!(boxes[0].y < gh.baseline - 10.0 && boxes[0].y + boxes[0].height > gh.baseline);
        let span = dom.find_first("span").unwrap();
        let b = dom.find_first("b").unwrap();
        assert!(
            l.boxes.iter().all(|x| x.node != span && x.node != b),
            "margins alone or nothing: no box"
        );
    }

    #[test]
    fn decorated_inlines_split_across_lines_and_keep_words_whole() {
        let (dom, l) = run(
            "<p>aaaa <span class=hl>bb cc dd</span> e</p><p>xx<i class=p>yy</i>zz</p>",
            "body { margin: 0 } p { margin: 0; width: 60px } .hl { background-color: yellow } \
             .p { padding: 0 1px }",
            400.0,
        );
        let span = dom.find_first("span").unwrap();
        let pieces: Vec<Rect> = l
            .boxes
            .iter()
            .filter(|b| b.node == span)
            .map(|b| b.rect)
            .collect();
        assert_eq!(pieces.len(), 2, "one box per line: {pieces:?}");
        assert!(pieces[1].y > pieces[0].y);
        assert_eq!(
            pieces[1].x, 0.0,
            "the continuation starts at the line start"
        );
        // "xx", "yy" and "zz" touch: no line break may fall between them.
        let y = |w: &str| l.texts.iter().find(|t| l.text(t) == w).unwrap().baseline;
        assert_eq!(y("xx"), y("zz"));
    }

    #[test]
    fn non_table_content_in_a_table_gets_an_anonymous_cell() {
        let (dom, l) = run_with_images(
            "<figure><a href=x><img id=i></a><figcaption id=c>caption</figcaption></figure>\
             <table><form><tr><td id=f>in form</td></tr></form></table>",
            "body { margin: 0 } figure { display: table; margin: 0 } \
             figcaption { display: table-caption; caption-side: bottom } a { display: block }",
            400.0,
        );
        let r = |id| by_id(&dom, &l, id);
        assert_eq!(
            (r("i").width, r("i").height),
            (200.0, 100.0),
            "the image is laid out"
        );
        assert!(r("c").y >= r("i").y + r("i").height, "caption-side: bottom");
        assert!(
            r("f").width > 0.0,
            "rows wrapped in a form still form the table"
        );
    }

    #[test]
    fn document_is_at_least_the_viewport() {
        let (_, l) = run("<p>x</p>", "", 300.0);
        assert_eq!(l.height, 600.0);
        assert_eq!(l.width, 300.0);
    }

    #[test]
    fn lays_out_generated_boxes_around_the_content() {
        let (dom, l) = run(
            "<p>one <b>two</b></p><ul><li>x</li></ul><div id=c>c</div>",
            r#"body { margin: 0 } p, ul { margin: 0; padding: 0 }
               p::before { content: "> " } p::after { content: "!"; display: block }
               li::before { content: "- " } #c::after { content: "z"; float: none }"#,
            400.0,
        );
        assert_eq!(
            lines(&l).into_iter().map(|(_, t)| t).collect::<Vec<_>>(),
            [">|one|two", "!", "-|x", "c|z"]
        );
        // The generated text is in a node outside the tree, whose parent is the paragraph.
        let p = dom.find_first("p").unwrap();
        let gt = l.texts.iter().find(|t| l.text(t) == ">").unwrap().node;
        let generated = dom.parent(gt).unwrap();
        assert_eq!(dom.parent(generated), Some(p));
        assert!(dom.children(p).all(|c| c != generated));
        // The block after the paragraph's text has a box of its own.
        assert!(l
            .boxes
            .iter()
            .any(|b| dom.tag_name(b.node) == Some("::after")));
    }

    /// Text fragments as (x, line top, text), lines being 16px tall with `Approximate`.
    fn placed(l: &Layout) -> Vec<(f32, f32, String)> {
        (0..l.texts.len())
            .map(|i| {
                let t = &l.texts[i];
                let round = |v: f32| (v * 100.0).round() / 100.0;
                (round(t.x), round(t.baseline - 12.8), l.text(t).to_string())
            })
            .collect()
    }

    fn rect_of(dom: &Dom, l: &Layout, id: &str) -> Rect {
        let node = (0..dom.len() as NodeId)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .unwrap_or_else(|| panic!("no #{id}"));
        l.boxes.iter().find(|b| b.node == node).unwrap().rect
    }

    #[test]
    fn wraps_text_beside_a_left_float() {
        // Each word is 32px; beside the 50px float a 100px line holds one word.
        let (dom, l) = run(
            "<div id=f></div><p>aaaa bbbb cccc dddd eeee ffff</p>",
            "body { margin: 0 } p { margin: 0 } #f { float: left; width: 50px; height: 20px }",
            100.0,
        );
        assert_eq!(
            rect_of(&dom, &l, "f"),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 20.0
            }
        );
        let p = placed(&l);
        assert_eq!(
            p[0],
            (50.0, 0.0, "aaaa".into()),
            "first line beside the float"
        );
        assert_eq!(
            p[1],
            (50.0, 16.0, "bbbb".into()),
            "second line still reaches into it"
        );
        assert_eq!(
            p[2],
            (0.0, 32.0, "cccc dddd".into()),
            "below it the line is full width"
        );
        assert_eq!(
            rect(&dom, &l, "p").y,
            0.0,
            "the paragraph itself does not move"
        );
    }

    #[test]
    fn right_floats_and_stacking() {
        let (dom, l) = run(
            "<div id=a></div><div id=b></div><div id=c></div><div id=r></div><p>xx</p>",
            "body { margin: 0 } p { margin: 0 }
             #a, #b, #c { float: left; width: 40px; height: 10px }
             #c { height: 30px }
             #r { float: right; width: 30px; height: 50px }",
            100.0,
        );
        assert_eq!(rect_of(&dom, &l, "a").x, 0.0);
        assert_eq!(rect_of(&dom, &l, "b").x, 40.0, "next to the first");
        let c = rect_of(&dom, &l, "c");
        assert_eq!((c.x, c.y), (0.0, 10.0), "no room left: below the first two");
        let r = rect_of(&dom, &l, "r");
        assert_eq!((r.x, r.y), (70.0, 10.0), "not higher than an earlier float");
        // No room at the top; at 10px the gap between c and r holds it.
        assert_eq!(placed(&l)[0], (40.0, 10.0, "xx".into()));
    }

    #[test]
    fn clear_moves_a_block_below_floats() {
        let (dom, l) = run(
            "<div id=l></div><div id=r></div><p id=a>a</p><p id=b>b</p><p id=c>c</p>",
            "body { margin: 0 } p { margin: 0 }
             #l { float: left; width: 10px; height: 20px }
             #r { float: right; width: 10px; height: 40px }
             #a { clear: left } #b { clear: right } #c { clear: both }",
            100.0,
        );
        assert_eq!(rect_of(&dom, &l, "a").y, 20.0);
        assert_eq!(rect_of(&dom, &l, "b").y, 40.0);
        assert_eq!(rect_of(&dom, &l, "c").y, 56.0, "already below both");
    }

    #[test]
    fn a_formatting_context_sits_beside_floats() {
        let (dom, l) = run(
            "<div id=f></div><div id=o>x</div><div id=w></div>",
            "body { margin: 0 }
             #f { float: left; width: 30px; height: 40px }
             #o { overflow: hidden }
             #w { display: flow-root; width: 90px; height: 5px }",
            100.0,
        );
        let o = rect_of(&dom, &l, "o");
        assert_eq!(
            (o.x, o.y, o.width),
            (30.0, 0.0, 70.0),
            "narrowed beside the float"
        );
        let w = rect_of(&dom, &l, "w");
        assert_eq!(
            (w.x, w.y),
            (0.0, 40.0),
            "too wide to fit beside it: moved below"
        );
    }

    #[test]
    fn floats_grow_only_a_formatting_context_root() {
        let (dom, l) = run(
            "<div id=plain><div class=f></div></div><div id=root><div class=f></div></div>",
            "body { margin: 0 } .f { float: left; width: 10px; height: 25px }
             #root { overflow: hidden }",
            100.0,
        );
        assert_eq!(rect_of(&dom, &l, "plain").height, 0.0);
        let root = rect_of(&dom, &l, "root");
        assert_eq!(root.height, 25.0);
        assert_eq!((root.x, root.y), (10.0, 0.0), "beside the first float");
        assert_eq!(l.height, 600.0);
    }

    #[test]
    fn a_float_in_a_line_and_one_that_does_not_fit() {
        // "aaaa" is 32px; the 50px float fits beside it on a 100px line, the 80px one does not.
        let (dom, l) = run(
            "<p>aaaa <span id=a></span>bbbb <span id=b></span>cc</p>",
            "body { margin: 0 } p { margin: 0 }
             #a { float: right; width: 50px; height: 20px }
             #b { float: left; width: 80px; height: 10px }",
            100.0,
        );
        let a = rect_of(&dom, &l, "a");
        assert_eq!((a.x, a.y), (50.0, 0.0));
        let b = rect_of(&dom, &l, "b");
        assert_eq!(
            (b.x, b.y),
            (0.0, 32.0),
            "placed under the line it appears on"
        );
        let p = placed(&l);
        assert_eq!(p[0], (0.0, 0.0, "aaaa".into()));
        assert_eq!(
            p[1],
            (0.0, 16.0, "bbbb".into()),
            "no longer fits beside the float"
        );
        assert_eq!(p[2], (80.0, 32.0, "cc".into()), "beside the second float");
    }

    #[test]
    fn shrink_to_fit_floats_and_float_attributes() {
        let (dom, l) = run(
            "<div id=f>aa bb</div><img id=i align=right width=20 height=10><p>x<br clear=all>y</p>",
            "body { margin: 0 } p { margin: 0 } #f { float: left; padding: 2px }",
            200.0,
        );
        let f = rect_of(&dom, &l, "f");
        assert_eq!(f.width, 36.0 + 4.0, "as wide as its text, plus padding");
        let i = rect_of(&dom, &l, "i");
        assert_eq!((i.x, i.y), (180.0, 0.0), "align=right floats the image");
        let p = placed(&l);
        assert_eq!(p[1].0, 40.0, "\"x\" is beside the float");
        assert_eq!(
            p[2],
            (0.0, 20.0, "y".into()),
            "<br clear=all> moves below both"
        );
    }

    #[test]
    fn positioned_boxes_do_not_float() {
        let (dom, l) = run(
            "<div id=a></div><p>x</p>",
            "body { margin: 0 } p { margin: 0 }
             #a { float: left; position: absolute; width: 50px; height: 20px }",
            100.0,
        );
        assert_eq!(placed(&l)[0].0, 0.0);
        assert_eq!(rect_of(&dom, &l, "a").width, 50.0);
    }
}
