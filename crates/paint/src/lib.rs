//! Painting: layout to a display list to pixels.
//!
//! Stages 10 to 12 of `docs/pipeline.md`: the display list is built in one pass, every glyph
//! it needs is rasterized into the cache (sequential, the only writer), then the framebuffer
//! is composited in horizontal bands on scoped threads that only read the list and the cache.

pub mod png;
mod raster;
mod shape;

use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use css::{Color, Length, StyleMap};
use font::{FaceId, FontKey, TextMetrics, WEB_FACES};
use html::{Dom, FastBuildHasher, NodeId};
use layout::{Layout, Rect};
use shape::{blur_span, device_radii, shadow_coverage, DeviceGradient, RRect};

pub use raster::{rasterize_glyph, rasterize_glyph_at, GlyphBitmap, Rasterizer};

/// RGBA8 pixels, row by row from the top, fully opaque.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// What part of the page to draw, in CSS px, and the device pixel ratio.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
    pub scroll_y: f32,
}

impl Viewport {
    /// The frame size in device pixels.
    pub fn device_size(&self) -> (usize, usize) {
        (
            (self.width * self.scale).round().max(1.0) as usize,
            (self.height * self.scale).round().max(1.0) as usize,
        )
    }
}

/// Where to paint: rows of `stride` bytes, four bytes per pixel.
pub struct Target<'a> {
    pub pixels: &'a mut [u8],
    pub stride: usize,
    /// Blue, green, red, alpha byte order (what macOS surfaces use) instead of RGBA.
    pub bgra: bool,
    /// First column drawn into, in device pixels: a pane right of the sidebar starts there.
    pub left: usize,
}

#[derive(Clone, Copy)]
struct Band {
    width: usize,
    /// Column offset of x = 0 within each row.
    left: usize,
    stride: usize,
    canvas: Color,
    bgra: bool,
    /// Fill the band with `canvas` before drawing (page paint) or draw over what is there (overlay).
    fill_canvas: bool,
    /// Horizontal clip, in device pixels.
    clip: (i32, i32),
}

pub struct Page<'a> {
    pub dom: &'a Dom,
    pub styles: &'a StyleMap,
    pub layout: &'a Layout,
    /// Text is positioned with these metrics, so it lines up with layout, and drawn with their
    /// faces. Without fonts (`font::Approximate`), text is laid out but not drawn.
    pub metrics: &'a dyn TextMetrics,
    /// Compressed image files for `<img>` elements; without them images are not drawn.
    pub images: Option<&'a dyn ImageSource>,
}

/// Where the painter finds an image's compressed file: an id that stays the same for the
/// file (so several elements share decoded pixels) and its bytes.
pub trait ImageSource {
    fn image(&self, node: NodeId) -> Option<(u64, &[u8])>;
}

/// Decoded images kept between frames. Images on screen are never evicted, so this bounds what
/// is kept for scrolling back. With two Retina frames (about 31 MB) the window must fit in 60 MB.
pub const IMAGE_CACHE_BYTES: usize = 8 << 20;

/// A decoded image: which file, at what device size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ImageKey {
    id: u64,
    width: u32,
    height: u32,
}

struct CachedImage {
    bitmap: image::Bitmap,
    /// The last frame that drew it, for eviction.
    used: u64,
}

type ImageCache = HashMap<ImageKey, CachedImage, FastBuildHasher>;

#[derive(Clone, Copy, Debug, Default)]
pub struct PaintStats {
    pub items: usize,
    pub images_decoded: usize,
    pub image_cache_bytes: usize,
    pub image_ms: f64,
    pub glyphs_rasterized: usize,
    pub glyph_cache_bytes: usize,
    pub bands: usize,
    pub display_list_ms: f64,
    pub glyph_ms: f64,
    pub composite_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: FaceId,
    glyph: u32,
    /// Device pixel size in quarter pixels.
    size: u32,
    /// Where the glyph starts within its pixel, in quarters (0 to 3).
    phase: u8,
}

/// A pen position in device pixels as the pixel to draw at and the quarter-pixel phase to
/// rasterize with: glyphs keep the spacing their advances give instead of snapping to whole
/// pixels, with at most four bitmaps per glyph and size.
fn place(x: f32) -> (i32, u8) {
    let quarters = (x * 4.0).round() as i32;
    (quarters.div_euclid(4), quarters.rem_euclid(4) as u8)
}

/// x0, y0, x1, y1 in frame pixels; drawing outside is discarded.
type DeviceClip = [i32; 4];
const NO_CLIP: DeviceClip = [i32::MIN, i32::MIN, i32::MAX, i32::MAX];

enum Item<'a> {
    Fill {
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        color: Color,
        clip: DeviceClip,
    },
    Glyph {
        x: i32,
        y: i32,
        key: GlyphKey,
        color: Color,
        clip: DeviceClip,
    },
    /// A decoded image stretched over the device rectangle, cut to `round` when the box has
    /// rounded corners.
    Image {
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        key: ImageKey,
        round: Option<RRect>,
        clip: DeviceClip,
    },
    /// Boxed: every band walks every item, so the list's common items must stay small.
    Effect(Box<Effect>),
    /// The items up to the matching `GroupEnd` are drawn, then blended back over what was
    /// there before them: at `opacity`, and only inside `round` when it is set.
    GroupStart {
        opacity: u8,
        round: Option<RRect>,
        /// Device pixels the group's items can touch: only these are copied and blended.
        bounds: [i32; 4],
    },
    GroupEnd,
    /// A coverage mask drawn in one color, such as an icon. (x, y) is its top left.
    Mask {
        x: i32,
        y: i32,
        mask: &'a GlyphBitmap,
        color: Color,
        clip: DeviceClip,
    },
}

enum Effect {
    /// A rounded background, or a gradient layer, inside `shape`.
    Shape {
        shape: RRect,
        fill: ShapeFill,
        clip: DeviceClip,
    },
    /// Borders of a box with rounded corners: between `outer` and `inner`, each side its color.
    Border {
        outer: RRect,
        inner: RRect,
        widths: [f32; 4],
        colors: [Color; 4],
        clip: DeviceClip,
    },
    /// A `box-shadow`: `shape` blurred by `sigma`, drawn only outside `bound` (the border box)
    /// or, for `inset`, only inside it (the padding box), where the shape does not reach.
    Shadow {
        shape: RRect,
        bound: RRect,
        sigma: f32,
        color: Color,
        inset: bool,
        clip: DeviceClip,
    },
}

enum ShapeFill {
    Solid(Color),
    Gradient(Box<DeviceGradient>),
}

/// Walks clip ranges (which nest) alongside ascending fragment indices, keeping the
/// intersection of every clip that covers the current index on a stack.
struct ClipTracker {
    ranges: Vec<(u32, u32, Rect)>,
    next: usize,
    stack: Vec<(u32, Rect)>,
    last: u32,
}

impl ClipTracker {
    fn new(mut ranges: Vec<(u32, u32, Rect)>) -> ClipTracker {
        // Outer clips first when several start at the same index.
        ranges.sort_by_key(|r| (r.0, std::cmp::Reverse(r.1)));
        ClipTracker {
            ranges,
            next: 0,
            stack: Vec::new(),
            last: 0,
        }
    }

    /// The clip for fragment `i`. Calls are cheapest with non-decreasing `i`; going back
    /// (another layer, a group's fragments) starts the walk over.
    fn at(&mut self, i: u32) -> Option<Rect> {
        if i < self.last {
            self.next = 0;
            self.stack.clear();
        }
        self.last = i;
        while self.stack.last().is_some_and(|&(end, _)| end <= i) {
            self.stack.pop();
        }
        while self.next < self.ranges.len() && self.ranges[self.next].0 <= i {
            let (_, end, rect) = self.ranges[self.next];
            self.next += 1;
            if end > i {
                let rect = match self.stack.last() {
                    Some(&(_, outer)) => intersect(outer, rect),
                    None => rect,
                };
                self.stack.push((end, rect));
            }
        }
        self.stack.last().map(|&(_, r)| r)
    }
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0.0),
        height: (y1 - y0).max(0.0),
    }
}

/// One drawing operation for `Painter::overlay`, in device pixels.
pub enum Op<'a> {
    Fill {
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        color: Color,
    },
    Text {
        x: f32,
        baseline: f32,
        text: &'a str,
        size: f32,
        font: FontKey,
        color: Color,
    },
    Mask {
        x: i32,
        y: i32,
        mask: &'a GlyphBitmap,
        color: Color,
    },
}

/// Keeps the glyph cache between frames, so scrolling and repaints only rasterize new glyphs.
#[derive(Default)]
pub struct Painter {
    glyphs: HashMap<GlyphKey, GlyphBitmap, FastBuildHasher>,
    images: ImageCache,
    frame: u64,
}

impl Painter {
    pub fn new() -> Painter {
        Painter::default()
    }

    /// Paint into a new RGBA buffer.
    pub fn paint(&mut self, page: &Page, view: Viewport) -> (Frame, PaintStats) {
        let (width, height) = view.device_size();
        let mut pixels = vec![0u8; width * height * 4];
        let target = Target {
            pixels: &mut pixels,
            stride: width * 4,
            bgra: false,
            left: 0,
        };
        let stats = self.paint_into(page, view, target);
        (
            Frame {
                width,
                height,
                pixels,
            },
            stats,
        )
    }

    /// Paint into memory someone else owns, such as a surface the window system shows
    /// directly. The target must hold `view.device_size()` pixels at the given stride.
    pub fn paint_into(&mut self, page: &Page, view: Viewport, target: Target) -> PaintStats {
        let (width, height) = view.device_size();
        assert!(
            target.stride >= (target.left + width) * 4,
            "stride shorter than a row"
        );
        assert!(
            target.pixels.len() >= target.stride * height,
            "target smaller than the frame"
        );
        let mut stats = PaintStats::default();

        let started = Instant::now();
        let mut items = Vec::new();
        let mut wanted: Vec<GlyphKey> = Vec::new();
        let canvas = canvas_color(page);
        let mut pictures = Vec::new();
        self.build(
            page,
            view,
            height as i32,
            &mut items,
            &mut wanted,
            &mut pictures,
        );
        stats.items = items.len();
        stats.display_list_ms = ms(started);

        let started = Instant::now();
        for key in wanted {
            if self.glyphs.contains_key(&key) {
                continue;
            }
            let bitmap = match page.metrics.face(key.face) {
                Some(face) => rasterize_glyph_at(
                    face,
                    key.glyph,
                    key.size as f32 / 4.0,
                    f32::from(key.phase) / 4.0,
                ),
                None => GlyphBitmap::default(),
            };
            stats.glyphs_rasterized += 1;
            self.glyphs.insert(key, bitmap);
        }
        stats.glyph_cache_bytes = self.glyphs.values().map(|g| g.coverage.len()).sum();
        stats.glyph_ms = ms(started);

        let started = Instant::now();
        stats.images_decoded = self.decode_images(pictures);
        stats.image_cache_bytes = self.images.values().map(|c| c.bitmap.bytes()).sum();
        stats.image_ms = ms(started);

        let started = Instant::now();
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(1, 16);
        let rows_per_band = height.div_ceil(threads).max(1);
        let band = Band {
            width,
            left: target.left,
            stride: target.stride,
            canvas,
            bgra: target.bgra,
            fill_canvas: true,
            clip: (0, width as i32),
        };
        let items = &items;
        let glyphs = &self.glyphs;
        let images = &self.images;
        let pixels = &mut target.pixels[..target.stride * height];
        std::thread::scope(|scope| {
            for (index, rows) in pixels.chunks_mut(rows_per_band * target.stride).enumerate() {
                stats.bands += 1;
                scope.spawn(move || {
                    let y0 = (index * rows_per_band) as i32;
                    composite_band(rows, band, y0, items, glyphs, images);
                });
            }
        });
        stats.composite_ms = ms(started);
        stats
    }

    /// Draw Broxee's own interface (the header, icons, the address field) over whatever the
    /// target already holds, clipped horizontally to `clip`. Coordinates are device pixels.
    pub fn overlay(
        &mut self,
        target: Target,
        width: usize,
        clip: (i32, i32),
        metrics: &dyn TextMetrics,
        ops: &[Op<'_>],
    ) {
        let mut items: Vec<Item<'_>> = Vec::new();
        for op in ops {
            match *op {
                Op::Fill {
                    x0,
                    y0,
                    x1,
                    y1,
                    color,
                } => items.push(Item::Fill {
                    x0,
                    y0,
                    x1,
                    y1,
                    color,
                    clip: NO_CLIP,
                }),
                Op::Mask { x, y, mask, color } => items.push(Item::Mask {
                    x,
                    y,
                    mask,
                    color,
                    clip: NO_CLIP,
                }),
                Op::Text {
                    x,
                    baseline,
                    text,
                    size,
                    font,
                    color,
                } => {
                    let size_key = (size * 4.0).round() as u32;
                    let mut pen = x;
                    for ch in text.chars() {
                        if let Some((face, glyph)) =
                            metrics.glyph(ch, font).filter(|_| !ch.is_whitespace())
                        {
                            let (at, phase) = place(pen);
                            let key = GlyphKey {
                                face,
                                glyph,
                                size: size_key,
                                phase,
                            };
                            self.glyphs.entry(key).or_insert_with(|| {
                                let shift = f32::from(phase) / 4.0;
                                match metrics.face(face) {
                                    Some(f) => rasterize_glyph_at(f, glyph, size, shift),
                                    None => GlyphBitmap::default(),
                                }
                            });
                            items.push(Item::Glyph {
                                x: at,
                                y: baseline.round() as i32,
                                key,
                                color,
                                clip: NO_CLIP,
                            });
                        }
                        pen += metrics.advance_em(ch, font) * size;
                    }
                }
            }
        }
        let band = Band {
            width,
            left: target.left,
            stride: target.stride,
            canvas: Color::TRANSPARENT,
            bgra: target.bgra,
            fill_canvas: false,
            clip,
        };
        composite_band(target.pixels, band, 0, &items, &self.glyphs, &self.images);
    }

    /// Decode the images this frame draws that are not cached yet, in parallel, then evict
    /// images no longer drawn while the cache is over budget. Returns how many were decoded.
    fn decode_images(&mut self, pictures: Vec<(ImageKey, &[u8])>) -> usize {
        self.frame += 1;
        let mut missing: Vec<(ImageKey, &[u8])> = Vec::new();
        for (key, data) in pictures {
            match self.images.get_mut(&key) {
                Some(cached) => cached.used = self.frame,
                None if !missing.iter().any(|m| m.0 == key) => missing.push((key, data)),
                None => {}
            }
        }
        let decoded = missing.len();
        if !missing.is_empty() {
            let threads = std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .clamp(1, 8)
                .min(missing.len());
            let per_thread = missing.len().div_ceil(threads);
            let results: Vec<(ImageKey, image::Bitmap)> = std::thread::scope(|scope| {
                let handles: Vec<_> = missing
                    .chunks(per_thread)
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|&(key, data)| {
                                    // A file that fails to decode is remembered as empty, so it
                                    // is not retried every frame.
                                    let bitmap = image::decode(data, key.width, key.height)
                                        .unwrap_or_default();
                                    (key, bitmap)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            for (key, bitmap) in results {
                let used = self.frame;
                self.images.insert(key, CachedImage { bitmap, used });
            }
        }
        let mut bytes: usize = self.images.values().map(|c| c.bitmap.bytes()).sum();
        while bytes > IMAGE_CACHE_BYTES {
            let oldest = self
                .images
                .iter()
                .filter(|(_, c)| c.used < self.frame)
                .min_by_key(|(_, c)| c.used)
                .map(|(k, _)| *k);
            let Some(key) = oldest else { break };
            if let Some(c) = self.images.remove(&key) {
                bytes -= c.bitmap.bytes();
            }
        }
        decoded
    }

    /// Drop the glyph cache when it grows past `max_bytes`, so browsing many pages cannot grow it forever.
    pub fn trim(&mut self, max_bytes: usize) {
        let bytes: usize = self.glyphs.values().map(|g| g.coverage.len()).sum();
        if bytes > max_bytes {
            self.glyphs.clear();
        }
    }

    /// Forget every decoded image. Call it when the page changes: image ids are only unique
    /// within one page, so another page's image 0 at the same size must not reuse the pixels.
    /// Forget what belongs to the page being left: its decoded images, and the glyphs of its
    /// `@font-face` faces (their ids are per page; the process's own faces stay cached).
    pub fn forget_images(&mut self) {
        self.images = HashMap::default();
        self.glyphs.retain(|key, _| key.face < WEB_FACES);
    }

    fn build<'p>(
        &self,
        page: &Page<'p>,
        view: Viewport,
        height: i32,
        items: &mut Vec<Item>,
        wanted: &mut Vec<GlyphKey>,
        pictures: &mut Vec<(ImageKey, &'p [u8])>,
    ) {
        let s = view.scale;
        let layout = page.layout;
        let visible = |y0: i32, y1: i32| y1 > 0 && y0 < height;
        let mut box_clips = ClipTracker::new(
            layout
                .clips
                .iter()
                .map(|c| (c.boxes.0, c.boxes.1, c.rect))
                .collect(),
        );
        let mut text_clips = ClipTracker::new(
            layout
                .clips
                .iter()
                .map(|c| (c.texts.0, c.texts.1, c.rect))
                .collect(),
        );

        // Layers in paint order: normal flow, then each positioned box. Fixed layers ignore scroll.
        for (boxes, texts, fixed) in layout.layer_ranges() {
            let scroll = if fixed { 0.0 } else { view.scroll_y };
            let device = |r: Rect| {
                (
                    (r.x * s).round() as i32,
                    ((r.y - scroll) * s).round() as i32,
                    ((r.x + r.width) * s).round() as i32,
                    ((r.y + r.height - scroll) * s).round() as i32,
                )
            };
            let to_clip = |r: Option<Rect>| match r {
                Some(r) => {
                    let (x0, y0, x1, y1) = device(r);
                    [x0, y0, x1, y1]
                }
                None => NO_CLIP,
            };

            // Only runs of fragments that reach into the view, in page coordinates, in the
            // order groups need: each group's boxes and text together.
            let (top, bottom) = (scroll, scroll + height as f32 / s);
            let mut ops = Vec::new();
            emit_range(
                layout,
                (boxes, texts),
                (top, bottom),
                &mut Vec::new(),
                &mut ops,
            );
            // Groups open in the list: where each starts, which it is, its clip and opacity.
            let mut open: Vec<(usize, usize, Option<RRect>, u8)> = Vec::new();
            for op in ops {
                let i = match op {
                    Emit::Box(i) => i,
                    Emit::Text(i) => {
                        self.text_items(
                            page,
                            i,
                            (s, scroll, height),
                            &mut text_clips,
                            items,
                            wanted,
                        );
                        continue;
                    }
                    Emit::GroupStart(g) => {
                        let group = &layout.groups[g];
                        let style = page.styles.style(group.node);
                        let round = group.round.then(|| {
                            // A rounded clip holds its box's children: the box comes just before.
                            let b = (group.boxes.0 as usize)
                                .checked_sub(1)
                                .and_then(|i| layout.boxes.get(i))
                                .filter(|b| b.node == group.node);
                            b.map(|b| {
                                let (x0, y0, x1, y1) = device(b.rect);
                                let bw = style.border_width.map(|w| w.to_f32() * s);
                                let radii = page.styles.radii(style).map_or([[0.0; 2]; 4], |r| {
                                    device_radii(r, (x1 - x0) as f32, (y1 - y0) as f32, s)
                                });
                                RRect::new(x0 as f32, y0 as f32, x1 as f32, y1 as f32, radii)
                                    .outset(bw.map(|w| -w))
                            })
                        });
                        let round = round.flatten();
                        let bounds = match round {
                            Some(r) => r.bounds(1.0),
                            None => group_bounds(page, group, s, scroll),
                        };
                        let opacity = if group.round { 255 } else { style.opacity };
                        open.push((items.len(), g, round, opacity));
                        items.push(Item::GroupStart {
                            opacity,
                            round,
                            bounds,
                        });
                        continue;
                    }
                    Emit::GroupEnd => {
                        let Some((start, g, round, opacity)) = open.pop() else {
                            continue;
                        };
                        // Most groups need no backdrop: a rounded clip whose content stays off
                        // its corners changes nothing, and opacity over a single item is that
                        // item's alpha (exactly: nothing in the group overlaps it).
                        let inner = items.len() - start - 1;
                        let unneeded = match round {
                            _ if inner == 0 => true,
                            Some(r) => {
                                let b = group_bounds(page, &layout.groups[g], s, scroll);
                                let i = r.interior(0.0);
                                b[0] >= i[0] && b[1] >= i[1] && b[2] <= i[2] && b[3] <= i[3]
                            }
                            None => inner == 1 && fade(&mut items[start + 1], opacity),
                        };
                        if unneeded {
                            items.remove(start);
                        } else {
                            items.push(Item::GroupEnd);
                        }
                        continue;
                    }
                };
                let clip = to_clip(box_clips.at(i as u32));
                let b = &layout.boxes[i];
                let (x0, y0, x1, y1) = device(b.rect);
                let style = page.styles.style(b.node);
                let shadows = page.styles.shadows(style);
                // Outer shadows reach past the box.
                let reach = shadows
                    .iter()
                    .filter(|sh| !sh.inset)
                    .map(|sh| {
                        let d = sh.x.0.abs().max(sh.y.0.abs()) + sh.spread.0.max(0);
                        ((d as f32 / 64.0 + sh.blur.to_f32() * 1.5) * s).ceil() as i32
                    })
                    .max()
                    .unwrap_or(0);
                if !visible(y0 - reach, y1 + reach) || !style.visible || style.transparent {
                    continue;
                }
                let bw = style.border_width.map(|w| (w.to_f32() * s).round() as i32);
                let (w, h) = ((x1 - x0) as f32, (y1 - y0) as f32);
                let border_box = RRect::new(
                    x0 as f32,
                    y0 as f32,
                    x1 as f32,
                    y1 as f32,
                    page.styles
                        .radii(style)
                        .map_or([[0.0; 2]; 4], |r| device_radii(r, w, h, s)),
                );
                let inset = |d: [i32; 4]| d.map(|v| -(v as f32));
                let padding_box = border_box.outset(inset(bw));
                let rounded = border_box.is_rounded();
                for sh in shadows.iter().rev().filter(|sh| !sh.inset) {
                    let spread = sh.spread.to_f32() * s;
                    items.push(Item::Effect(Box::new(Effect::Shadow {
                        shape: border_box
                            .outset([spread; 4])
                            .translate(sh.x.to_f32() * s, sh.y.to_f32() * s),
                        bound: border_box,
                        sigma: sh.blur.to_f32() * s / 2.0,
                        color: sh.color,
                        inset: false,
                        clip,
                    })));
                }
                if style.background_color.a > 0 {
                    items.push(if rounded {
                        Item::Effect(Box::new(Effect::Shape {
                            shape: border_box,
                            fill: ShapeFill::Solid(style.background_color),
                            clip,
                        }))
                    } else {
                        Item::Fill {
                            x0,
                            y0,
                            x1,
                            y1,
                            color: style.background_color,
                            clip,
                        }
                    });
                }
                // Gradients are sized to the padding box and painted to the border box.
                let area = [
                    padding_box.x0,
                    padding_box.y0,
                    padding_box.x1,
                    padding_box.y1,
                ];
                for g in page.styles.gradients(style).iter().rev() {
                    if let Some(gradient) = DeviceGradient::new(g, area, s) {
                        items.push(Item::Effect(Box::new(Effect::Shape {
                            shape: border_box,
                            fill: ShapeFill::Gradient(Box::new(gradient)),
                            clip,
                        })));
                    }
                }
                for sh in shadows.iter().rev().filter(|sh| sh.inset) {
                    let spread = sh.spread.to_f32() * s;
                    items.push(Item::Effect(Box::new(Effect::Shadow {
                        shape: padding_box
                            .outset([-spread; 4])
                            .translate(sh.x.to_f32() * s, sh.y.to_f32() * s),
                        bound: padding_box,
                        sigma: sh.blur.to_f32() * s / 2.0,
                        color: sh.color,
                        inset: true,
                        clip,
                    })));
                }
                if rounded && bw.iter().any(|&w| w > 0) {
                    items.push(Item::Effect(Box::new(Effect::Border {
                        outer: border_box,
                        inner: padding_box,
                        widths: bw.map(|w| w as f32),
                        colors: style.border_color,
                        clip,
                    })));
                }
                let sides = [
                    (x0, y0, x1, y0 + bw[0]),
                    (x1 - bw[1], y0, x1, y1),
                    (x0, y1 - bw[2], x1, y1),
                    (x0, y0, x0 + bw[3], y1),
                ];
                for (side, (sx0, sy0, sx1, sy1)) in sides.into_iter().enumerate() {
                    let color = style.border_color[side];
                    if !rounded && bw[side] > 0 && color.a > 0 {
                        items.push(Item::Fill {
                            x0: sx0,
                            y0: sy0,
                            x1: sx1,
                            y1: sy1,
                            color,
                            clip,
                        });
                    }
                }
                if style.outline_width > css::Px::ZERO && style.outline_color.a > 0 {
                    // Outside the border box (moved out by the offset), taking no space.
                    let w = ((style.outline_width.to_f32() * s).round() as i32).max(1);
                    let off = (style.outline_offset.to_f32() * s).round() as i32;
                    let (ox0, oy0, ox1, oy1) =
                        (x0 - off - w, y0 - off - w, x1 + off + w, y1 + off + w);
                    let color = style.outline_color;
                    for (fx0, fy0, fx1, fy1) in [
                        (ox0, oy0, ox1, oy0 + w),
                        (ox0, oy1 - w, ox1, oy1),
                        (ox0, oy0 + w, ox0 + w, oy1 - w),
                        (ox1 - w, oy0 + w, ox1, oy1 - w),
                    ] {
                        items.push(Item::Fill {
                            x0: fx0,
                            y0: fy0,
                            x1: fx1,
                            y1: fy1,
                            color,
                            clip,
                        });
                    }
                }
                if let Some((id, data)) = page.images.and_then(|images| images.image(b.node)) {
                    // The content box: inside the border and any fixed padding.
                    let pad = style.padding.map(|p| match p {
                        Length::Px(px) => (px.to_f32() * s).round() as i32,
                        _ => 0,
                    });
                    let (ix0, iy0) = (x0 + bw[3] + pad[3], y0 + bw[0] + pad[0]);
                    let (ix1, iy1) = (x1 - bw[1] - pad[1], y1 - bw[2] - pad[2]);
                    let (w, h) = (ix1 - ix0, iy1 - iy0);
                    if w > 0 && h > 0 && w <= 8192 && h <= 8192 {
                        let key = ImageKey {
                            id,
                            width: w as u32,
                            height: h as u32,
                        };
                        pictures.push((key, data));
                        items.push(Item::Image {
                            x0: ix0,
                            y0: iy0,
                            x1: ix1,
                            y1: iy1,
                            key,
                            round: rounded.then_some(padding_box),
                            clip,
                        });
                    }
                }
            }
        }
    }

    /// The glyphs and underline of text fragment `i`.
    fn text_items(
        &self,
        page: &Page,
        i: usize,
        (s, scroll, height): (f32, f32, i32),
        text_clips: &mut ClipTracker,
        items: &mut Vec<Item>,
        wanted: &mut Vec<GlyphKey>,
    ) {
        let layout = page.layout;
        let visible = |y0: i32, y1: i32| y1 > 0 && y0 < height;
        let to_clip = |r: Option<Rect>| match r {
            Some(r) => [
                (r.x * s).round() as i32,
                ((r.y - scroll) * s).round() as i32,
                ((r.x + r.width) * s).round() as i32,
                ((r.y + r.height - scroll) * s).round() as i32,
            ],
            None => NO_CLIP,
        };
        {
            {
                let clip = to_clip(text_clips.at(i as u32));
                let t = &layout.texts[i];
                let style = page.styles.style(t.node);
                let size = style.font_size.to_f32();
                let baseline = t.baseline - scroll;
                if !style.visible
                    || style.transparent
                    || !visible(
                        ((baseline - size) * s) as i32,
                        ((baseline + size) * s) as i32,
                    )
                {
                    return;
                }
                let font = layout::font_key(style);
                let color = style.color;
                let device_baseline = (baseline * s).round() as i32;
                let size_key = (size * s * 4.0).round() as u32;
                let mut pen = t.x;
                for ch in layout.text(t).chars() {
                    if let Some((face, glyph)) =
                        page.metrics.glyph(ch, font).filter(|_| !ch.is_whitespace())
                    {
                        let (at, phase) = place(pen * s);
                        let key = GlyphKey {
                            face,
                            glyph,
                            size: size_key,
                            phase,
                        };
                        wanted.push(key);
                        items.push(Item::Glyph {
                            x: at,
                            y: device_baseline,
                            key,
                            color,
                            clip,
                        });
                    }
                    pen += page.metrics.advance_em(ch, font) * size;
                }
                if style.underline {
                    let y0 = ((baseline + size * 0.1) * s).round() as i32;
                    let thickness = (size * 0.06 * s).round().max(1.0) as i32;
                    let (x0, x1) = (
                        (t.x * s).round() as i32,
                        ((t.x + t.width) * s).round() as i32,
                    );
                    items.push(Item::Fill {
                        x0,
                        y0,
                        x1,
                        y1: y0 + thickness,
                        color,
                        clip,
                    });
                }
            }
        }
    }
}

/// One step of a layer's display list, in paint order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Emit {
    Box(usize),
    Text(usize),
    GroupStart(usize),
    GroupEnd,
}

/// The visible fragments of a layer (box and text index ranges) in paint order: boxes in
/// order, each group's boxes and text together between its start and end (groups nest), then
/// the text outside any group. Groups are cut to the layer: a part laid out in another layer
/// (a positioned descendant, a block with a z-index) is drawn there, outside the group.
/// `open` holds the groups already started around this range: a group cut by a layer is never
/// its own range, and picking it again recursed until the stack ran out on singapay.id.
fn emit_range(
    layout: &Layout,
    (boxes, texts): (Range<usize>, Range<usize>),
    (top, bottom): (f32, f32),
    open: &mut Vec<usize>,
    out: &mut Vec<Emit>,
) {
    let visible = |r: Range<usize>| Layout::in_view(&layout.box_extents, r, top, bottom);
    let visible_texts = |r: Range<usize>| Layout::in_view(&layout.text_extents, r, top, bottom);
    // Groups starting in this range, cut to it.
    let cut = |g: usize| {
        let gr = layout.groups[g];
        let b = (gr.boxes.0 as usize).max(boxes.start)..(gr.boxes.1 as usize).min(boxes.end);
        let t = (gr.texts.0 as usize).max(texts.start)..(gr.texts.1 as usize).min(texts.end);
        (b, t)
    };
    let candidates: Vec<usize> = (0..layout.groups.len())
        .filter(|g| !open.contains(g))
        .filter(|&g| {
            let gr = layout.groups[g];
            let start = gr.boxes.0 as usize;
            let (b, t) = cut(g);
            (boxes.start..boxes.end.max(boxes.start + 1)).contains(&start)
                && (!b.is_empty() || !t.is_empty())
        })
        .collect();
    // The outermost: not inside another candidate (on equal ranges, the lower index is outer).
    let inside = |o: usize, g: usize| {
        let (a, b) = (layout.groups[o], layout.groups[g]);
        o != g
            && a.boxes.0 <= b.boxes.0
            && b.boxes.1 <= a.boxes.1
            && a.texts.0 <= b.texts.0
            && b.texts.1 <= a.texts.1
            && ((a.boxes, a.texts) != (b.boxes, b.texts) || o < g)
    };
    let mut outer: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&g| !candidates.iter().any(|&o| inside(o, g)))
        .collect();
    outer.sort_by_key(|&g| (layout.groups[g].boxes.0, layout.groups[g].texts.0, g));
    let mut cursor = boxes.start;
    let mut grouped_texts: Vec<Range<usize>> = Vec::new();
    for g in outer {
        let (gb, gt) = cut(g);
        if gb.start < cursor {
            continue;
        }
        out.extend(visible(cursor..gb.start).map(Emit::Box));
        out.push(Emit::GroupStart(g));
        if open.len() < MAX_GROUP_DEPTH {
            open.push(g);
            emit_range(layout, (gb.clone(), gt.clone()), (top, bottom), open, out);
            open.pop();
        } else {
            out.extend(visible(gb.clone()).map(Emit::Box));
            out.extend(visible_texts(gt.clone()).map(Emit::Text));
        }
        out.push(Emit::GroupEnd);
        cursor = gb.end.max(cursor);
        grouped_texts.push(gt);
    }
    out.extend(visible(cursor..boxes.end).map(Emit::Box));
    // Text outside the groups, in order.
    let mut start = texts.start;
    grouped_texts.sort_by_key(|r| r.start);
    for r in grouped_texts {
        if r.start > start {
            out.extend(visible_texts(start..r.start).map(Emit::Text));
        }
        start = start.max(r.end);
    }
    out.extend(visible_texts(start..texts.end).map(Emit::Text));
}

/// Groups nested deeper than this are drawn flat: a bound on recursion and on the strips each
/// band keeps (one backdrop per open group).
const MAX_GROUP_DEPTH: usize = 64;

/// The page background: the root element's, else the body's, else white (CSS 2.1 14.2).
fn canvas_color(page: &Page) -> Color {
    let mut fallback = Color::rgb(255, 255, 255);
    for id in page.dom.descendants(Dom::ROOT) {
        match page.dom.tag_name(id) {
            Some("html") => {
                let c = page.styles.style(id).background_color;
                if c.a > 0 {
                    return opaque(c);
                }
            }
            Some("body") => {
                let c = page.styles.style(id).background_color;
                if c.a > 0 {
                    fallback = opaque(c);
                }
                break;
            }
            _ => {}
        }
    }
    fallback
}

fn opaque(c: Color) -> Color {
    let white = Color::rgb(255, 255, 255);
    let mut px = [white.r, white.g, white.b, 255];
    blend(&mut px, c, u32::from(c.a));
    Color::rgb(px[0], px[1], px[2])
}

/// Draw every item that touches this band. `y0` is the band's first row in frame coordinates.
fn composite_band(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    items: &[Item<'_>],
    glyphs: &HashMap<GlyphKey, GlyphBitmap, FastBuildHasher>,
    images: &ImageCache,
) {
    let order = |c: Color| {
        if band.bgra {
            Color {
                r: c.b,
                g: c.g,
                b: c.r,
                a: c.a,
            }
        } else {
            c
        }
    };
    let canvas = order(band.canvas);
    if band.fill_canvas {
        for row in rows.chunks_mut(band.stride) {
            for px in row[band.left * 4..(band.left + band.width) * 4]
                .as_chunks_mut::<4>()
                .0
            {
                *px = [canvas.r, canvas.g, canvas.b, 255];
            }
        }
    }
    draw_items(rows, band, y0, items, glyphs, images);
}

/// Draw `items` into this band's rows (or a strip of them, starting at frame row `y0`).
fn draw_items(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    items: &[Item<'_>],
    glyphs: &HashMap<GlyphKey, GlyphBitmap, FastBuildHasher>,
    images: &ImageCache,
) {
    let order = |c: Color| {
        if band.bgra {
            Color {
                r: c.b,
                g: c.g,
                b: c.r,
                a: c.a,
            }
        } else {
            c
        }
    };
    let band_height = (rows.len() / band.stride) as i32;
    let lo = band.clip.0.max(0);
    // Never below `lo`: the clamps against (lo, hi) panic on reversed bounds.
    let hi = band.clip.1.min(band.width as i32).max(lo);
    let pixel = |x: i32, y: i32| y as usize * band.stride + (band.left + x as usize) * 4;
    let mut k = 0;
    while k < items.len() {
        let item = &items[k];
        k += 1;
        match *item {
            Item::GroupStart {
                opacity,
                ref round,
                bounds,
            } => {
                let end = group_end(items, k - 1);
                let inner = &items[k..end];
                let how = (opacity, round.as_ref(), bounds);
                draw_group(rows, band, y0, inner, how, glyphs, images);
                k = end + 1;
            }
            Item::GroupEnd => {}
            Item::Fill {
                x0,
                y0: fy0,
                x1,
                y1,
                color,
                clip,
            } => {
                let color = order(color);
                let (cx0, cx1) = (x0.max(clip[0]).clamp(lo, hi), x1.min(clip[2]).clamp(lo, hi));
                let ry0 = (fy0.max(clip[1]) - y0).clamp(0, band_height);
                let ry1 = (y1.min(clip[3]) - y0).clamp(0, band_height);
                for y in ry0..ry1 {
                    for x in cx0..cx1 {
                        let at = pixel(x, y);
                        blend(&mut rows[at..at + 4], color, u32::from(color.a));
                    }
                }
            }
            Item::Glyph {
                x,
                y,
                key,
                color,
                clip,
            } => {
                let Some(g) = glyphs.get(&key) else { continue };
                let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
                draw_mask(rows, band, y0, x + g.left, y + g.top, g, order(color), rect);
            }
            Item::Mask {
                x,
                y,
                mask,
                color,
                clip,
            } => {
                let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
                draw_mask(rows, band, y0, x, y, mask, order(color), rect);
            }
            Item::Image {
                x0,
                y0: iy0,
                x1,
                y1,
                key,
                ref round,
                clip,
            } => {
                let Some(cached) = images.get(&key) else {
                    continue;
                };
                let bitmap = &cached.bitmap;
                if bitmap.width == 0 || bitmap.height == 0 {
                    continue;
                }
                let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
                draw_image(
                    rows,
                    band,
                    y0,
                    [x0, iy0, x1, y1],
                    bitmap,
                    round.as_ref(),
                    rect,
                );
            }
            Item::Effect(ref effect) => draw_effect(rows, band, y0, effect, (lo, hi)),
        }
    }
}

/// Draw `item` at `opacity` instead of through a group, when its colors carry the alpha:
/// fills, text, masks, rounded solids, borders and shadows. Not images or gradients.
fn fade(item: &mut Item<'_>, opacity: u8) -> bool {
    let mul = |c: &mut Color| c.a = ((u32::from(c.a) * u32::from(opacity) + 127) / 255) as u8;
    match item {
        Item::Fill { color, .. } | Item::Glyph { color, .. } | Item::Mask { color, .. } => {
            mul(color);
            true
        }
        Item::Effect(effect) => match &mut **effect {
            Effect::Shape {
                fill: ShapeFill::Solid(c),
                ..
            } => {
                mul(c);
                true
            }
            Effect::Border { colors, .. } => {
                colors.iter_mut().for_each(mul);
                true
            }
            Effect::Shadow { color, .. } => {
                mul(color);
                true
            }
            Effect::Shape { .. } => false,
        },
        _ => false,
    }
}

/// The device pixels an opacity group can draw on: its boxes (with how far their outer
/// shadows and outlines reach) and its text, in the group's layer coordinates.
fn group_bounds(page: &Page, group: &layout::Group, s: f32, scroll: f32) -> [i32; 4] {
    let layout = page.layout;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for i in group.boxes.0 as usize..(group.boxes.1 as usize).min(layout.boxes.len()) {
        let b = &layout.boxes[i];
        let style = page.styles.style(b.node);
        let reach = page
            .styles
            .shadows(style)
            .iter()
            .filter(|sh| !sh.inset)
            .map(|sh| {
                sh.x.to_f32().abs().max(sh.y.to_f32().abs())
                    + sh.spread.to_f32().max(0.0)
                    + sh.blur.to_f32() * 1.5
            })
            .fold(0.0f32, f32::max)
            + (style.outline_width.to_f32() + style.outline_offset.to_f32().max(0.0));
        x0 = x0.min(b.rect.x - reach);
        y0 = y0.min(b.rect.y - reach);
        x1 = x1.max(b.rect.x + b.rect.width + reach);
        y1 = y1.max(b.rect.y + b.rect.height + reach);
    }
    for i in group.texts.0 as usize..(group.texts.1 as usize).min(layout.texts.len()) {
        let t = &layout.texts[i];
        let size = page.styles.style(t.node).font_size.to_f32();
        x0 = x0.min(t.x - size);
        x1 = x1.max(t.x + t.width + size);
        y0 = y0.min(t.baseline - size * 2.0);
        y1 = y1.max(t.baseline + size);
    }
    if x0 > x1 {
        return [0, 0, 0, 0];
    }
    [
        (x0 * s).floor() as i32 - 2,
        ((y0 - scroll) * s).floor() as i32 - 2,
        (x1 * s).ceil() as i32 + 2,
        ((y1 - scroll) * s).ceil() as i32 + 2,
    ]
}

/// The index of the `GroupEnd` matching the `GroupStart` at `start` (the list's end if missing).
fn group_end(items: &[Item<'_>], start: usize) -> usize {
    let mut depth = 0;
    for (i, item) in items.iter().enumerate().skip(start) {
        match item {
            Item::GroupStart { .. } => depth += 1,
            Item::GroupEnd => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    items.len()
}

/// Rows drawn at a time inside a group: its backdrop is copied a strip at a time, so a group
/// costs width x 32 pixels of memory per band, not a copy of the band.
const GROUP_STRIP: usize = 32;

/// Draw a group: per strip, keep what is there, draw the group's items over it, then go back
/// toward what was there by the group's opacity (exactly group opacity, over an opaque
/// backdrop) and fully outside its rounded clip.
fn draw_group(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    inner: &[Item<'_>],
    (opacity, round, bounds): (u8, Option<&RRect>, [i32; 4]),
    glyphs: &HashMap<GlyphKey, GlyphBitmap, FastBuildHasher>,
    images: &ImageCache,
) {
    let band_rows = rows.len() / band.stride;
    // Only the group's own columns and rows: a small button must not cost the band's width.
    let (bx0, bx1) = (
        bounds[0].clamp(0, band.width as i32) as usize,
        bounds[2].clamp(0, band.width as i32) as usize,
    );
    let first = (bounds[1] - y0).clamp(0, band_rows as i32) as usize;
    let last = (bounds[3] - y0).clamp(0, band_rows as i32) as usize;
    if bx0 >= bx1 || first >= last {
        return;
    }
    let (c0, c1) = ((band.left + bx0) * 4, (band.left + bx1) * 4);
    let mut backdrop = vec![0u8; GROUP_STRIP.min(last - first) * (c1 - c0)];
    // Drawing is clipped to the bounds too, so nothing outside them changes.
    let inner_clip = (band.clip.0.max(bx0 as i32), band.clip.1.min(bx1 as i32));
    let mut start = first;
    while start < last {
        let end = (start + GROUP_STRIP).min(last);
        let strip = &mut rows[start * band.stride..end * band.stride];
        let fy0 = y0 + start as i32;
        for (k, row) in strip.chunks(band.stride).enumerate() {
            backdrop[k * (c1 - c0)..(k + 1) * (c1 - c0)].copy_from_slice(&row[c0..c1]);
        }
        let inner_band = Band {
            fill_canvas: false,
            clip: inner_clip,
            ..band
        };
        draw_items(strip, inner_band, fy0, inner, glyphs, images);
        for (k, row) in strip.chunks_mut(band.stride).enumerate() {
            let before = &backdrop[k * (c1 - c0)..(k + 1) * (c1 - c0)];
            let y = (fy0 + k as i32) as f32 + 0.5;
            let spans = round.map(|r| r.row_spans(fy0 as f32 + k as f32));
            for (x, (px, old)) in row[c0..c1]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(before.as_chunks::<4>().0)
                .enumerate()
            {
                let x = x + bx0;
                if px == old {
                    continue;
                }
                let mut keep = u32::from(opacity);
                if let (Some(r), Some(spans)) = (round, spans) {
                    let cover = match spans.known(x as i32) {
                        Some(c) => c,
                        None => r.coverage(x as f32 + 0.5, y),
                    };
                    keep = (keep as f32 * cover).round() as u32;
                }
                if keep >= 255 {
                    continue;
                }
                for c in 0..3 {
                    let (new, was) = (i32::from(px[c]), i32::from(old[c]));
                    px[c] = (was + ((new - was) * keep as i32 + 127) / 255) as u8;
                }
            }
        }
        start = end;
    }
}

/// Draw a rounded shape, a gradient, a rounded border or a shadow into this band.
fn draw_effect(rows: &mut [u8], band: Band, y0: i32, effect: &Effect, (lo, hi): (i32, i32)) {
    let order = |c: Color| {
        if band.bgra {
            Color {
                r: c.b,
                g: c.g,
                b: c.r,
                a: c.a,
            }
        } else {
            c
        }
    };
    match *effect {
        Effect::Shape {
            ref shape,
            ref fill,
            clip,
        } => {
            let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
            let interior = shape.interior(0.0);
            match fill {
                ShapeFill::Gradient(g) => fill_gradient(rows, band, y0, interior, rect, g),
                ShapeFill::Solid(c) => {
                    fill_shape(rows, band, y0, shape, rect, order(*c));
                    return;
                }
            }
            // The interior is filled above; the edges go through the shape.
            let edges = Edges {
                area: shape.bounds(0.0),
                hole: interior,
                straight: shape.interior(1.0),
                shape: Some((*shape, None)),
            };
            each_pixel(
                rows,
                band,
                y0,
                edges,
                rect,
                |x, y| shape.coverage(x, y),
                |x, y, dst, cover| {
                    let cover = (cover * 255.0).round() as u32;
                    match fill {
                        ShapeFill::Solid(c) => blend(dst, order(*c), cover * u32::from(c.a) / 255),
                        ShapeFill::Gradient(g) => {
                            let mut c = g.sample(x, y);
                            if band.bgra {
                                c.swap(0, 2);
                            }
                            blend_premultiplied(dst, c, cover);
                        }
                    }
                },
            );
        }
        Effect::Border {
            ref outer,
            ref inner,
            widths,
            colors,
            clip,
        } => {
            let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
            let edges = Edges {
                area: outer.bounds(0.0),
                hole: inner.interior(0.0),
                straight: narrower(outer.interior(1.0), inner.interior(1.0)),
                shape: Some((*outer, Some(*inner))),
            };
            each_pixel(
                rows,
                band,
                y0,
                edges,
                rect,
                |x, y| outer.coverage(x, y) - inner.coverage(x, y),
                |x, y, dst, cover| {
                    // The side whose band the pixel is deepest in, relative to its width:
                    // corners split along the line from the outer to the inner corner.
                    let depth = [y - outer.y0, outer.x1 - x, outer.y1 - y, x - outer.x0];
                    let side = (0..4)
                        .filter(|&i| widths[i] > 0.0)
                        .min_by(|&a, &b| (depth[a] / widths[a]).total_cmp(&(depth[b] / widths[b])))
                        .unwrap_or(0);
                    let c = colors[side];
                    blend(dst, order(c), (cover * f32::from(c.a)).round() as u32);
                },
            );
        }
        Effect::Shadow {
            ref shape,
            ref bound,
            sigma,
            color,
            inset,
            clip,
        } => {
            let rect = [clip[0].max(lo), clip[1], clip[2].min(hi), clip[3]];
            // Outer shadows are hidden under the box, inset ones fade out 3 sigma inside
            // the shape: neither needs computing there.
            let reach = sigma * 3.0 + 1.0;
            let straight = narrower(shape.interior(reach), bound.interior(1.0));
            let edges = if inset {
                Edges {
                    area: bound.bounds(0.0),
                    hole: shape.interior(reach),
                    straight,
                    shape: None,
                }
            } else {
                Edges {
                    area: shape.bounds(reach),
                    hole: bound.interior(0.0),
                    straight,
                    shape: None,
                }
            };
            let c = order(color);
            // When the blur is at least the corners' radius, the rounded corner makes no
            // visible difference: the shadow is a blurred rectangle, the product of two
            // one-dimensional blurs, one per column and one per row. Small cards with large
            // blurs are all corner: per-pixel distances cost 9 ms a band on singapay.id.
            let radius = shape.r.iter().flatten().fold(0.0f32, |m, &r| m.max(r));
            if sigma >= 0.5 && radius <= sigma {
                let shadow = BlurredRect {
                    shape,
                    bound,
                    sigma,
                    inset,
                };
                draw_blurred_rect(rows, band, y0, &shadow, edges, rect, c);
                return;
            }
            each_pixel(
                rows,
                band,
                y0,
                edges,
                rect,
                |x, y| {
                    let dark = shadow_coverage(shape, sigma, x, y);
                    if inset {
                        (1.0 - dark) * bound.coverage(x, y)
                    } else {
                        dark * (1.0 - bound.coverage(x, y))
                    }
                },
                |_, _, dst, cover| blend(dst, c, (cover * f32::from(c.a)).round() as u32),
            );
        }
    }
}

/// Where an effect draws: `area`, less `hole` (drawn already, or nothing to draw). Between
/// the corners the edges are straight: coverage is the same along a row between `straight`'s
/// left and right, and the same down a column between its top and bottom.
struct Edges {
    area: [i32; 4],
    hole: [i32; 4],
    straight: [i32; 4],
    /// The shape whose coverage this is, and the hole cut from it (a border's inner edge):
    /// pixels known to be outside, inside or in the hole skip working out their coverage.
    shape: Option<(RRect, Option<RRect>)>,
}

/// Where both shapes' edges are straight: the overlap of their straight spans.
fn narrower(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// Paint the edges of an effect: `paint` gets each pixel's center (frame coordinates), its
/// bytes and its coverage from `cover`, for the pixels of `edges.area` outside `edges.hole`,
/// inside `clip` and this band. Coverage is worked out once per row where the edge is straight,
/// and once per column where it is straight vertically, so a big box costs per-pixel distance
/// work only at its corners: singapay.id's large blurred shadows took 22 ms a frame at 2x
/// before columns were reused.
fn each_pixel(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    edges: Edges,
    clip: DeviceClip,
    cover: impl Fn(f32, f32) -> f32,
    mut paint: impl FnMut(f32, f32, &mut [u8], f32),
) {
    let Edges {
        area,
        hole: h,
        straight,
        shape,
    } = edges;
    let band_height = (rows.len() / band.stride) as i32;
    let (cx0, cx1) = (
        area[0].max(clip[0]).max(0),
        area[2].min(clip[2]).min(band.width as i32),
    );
    let ry0 = (area[1].max(clip[1]) - y0).clamp(0, band_height);
    let ry1 = (area[3].min(clip[3]) - y0).clamp(0, band_height);
    // Nothing of the area in this band's columns; the clamps below need cx0 <= cx1.
    if cx0 >= cx1 {
        return;
    }
    // Coverage per column for rows on the straight vertical stretch, worked out on first use.
    let mut columns: Vec<f32> = Vec::new();
    let straight_rows = straight[1]..straight[3];
    for ry in ry0..ry1 {
        let fy = ry + y0;
        let y = fy as f32 + 0.5;
        let row = ry as usize * band.stride + band.left * 4;
        let mut put = |x: i32, c: f32| {
            if c > 0.0 {
                let at = row + x as usize * 4;
                paint(x as f32 + 0.5, y, &mut rows[at..at + 4], c);
            }
        };
        // Pixels whose coverage the row's spans already tell (most of a big corner).
        let spans =
            shape.map(|(s, hole)| (s.row_spans(fy as f32), hole.map(|h| h.row_spans(fy as f32))));
        let known = |x: i32| -> Option<f32> {
            let (outer, hole) = spans?;
            match (outer.known(x)?, hole.map(|h| h.known(x))) {
                (0.0, _) => Some(0.0),
                (_, None) => Some(1.0),
                (_, Some(Some(1.0))) => Some(0.0),
                (_, Some(Some(_))) => Some(1.0),
                (_, Some(None)) => None,
            }
        };
        let cover = |x: i32| known(x).unwrap_or_else(|| cover(x as f32 + 0.5, y));
        if (h[1]..h[3]).contains(&fy) && h[0] < h[2] {
            // Beside the hole: the left and right edges.
            let (hx0, hx1) = (h[0].clamp(cx0, cx1), h[2].clamp(cx0, cx1));
            if straight_rows.contains(&fy) {
                if columns.is_empty() {
                    columns = (cx0..cx1)
                        .map(|x| {
                            let inside = (hx0..hx1).contains(&x);
                            if inside {
                                0.0
                            } else {
                                cover(x)
                            }
                        })
                        .collect();
                }
                for x in (cx0..hx0).chain(hx1..cx1) {
                    put(x, columns[(x - cx0) as usize]);
                }
            } else {
                for x in (cx0..hx0).chain(hx1..cx1) {
                    put(x, cover(x));
                }
            }
            continue;
        }
        // Above or below it: corners, and the straight edge between them.
        let (sx0, sx1) = if straight[0] < straight[2] {
            (straight[0].clamp(cx0, cx1), straight[2].clamp(cx0, cx1))
        } else {
            (cx1, cx1)
        };
        for x in cx0..sx0 {
            put(x, cover(x));
        }
        if sx0 < sx1 {
            let c = cover(sx0);
            for x in sx0..sx1 {
                put(x, c);
            }
        }
        for x in sx1..cx1 {
            put(x, cover(x));
        }
    }
}

/// A shadow drawn as a blurred rectangle (see `Effect::Shadow`).
struct BlurredRect<'s> {
    shape: &'s RRect,
    bound: &'s RRect,
    sigma: f32,
    inset: bool,
}

/// Draw a shadow whose blur hides its corners' rounding: darkness is a column's blur times
/// the row's, and the box's own edge comes from the row's spans, so a pixel costs a multiply
/// and, when it shows, a blend.
fn draw_blurred_rect(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    shadow: &BlurredRect,
    edges: Edges,
    clip: DeviceClip,
    c: Color,
) {
    let BlurredRect {
        shape,
        bound,
        sigma,
        inset,
    } = *shadow;
    let band_height = (rows.len() / band.stride) as i32;
    let area = edges.area;
    let (cx0, cx1) = (
        area[0].max(clip[0]).max(0),
        area[2].min(clip[2]).min(band.width as i32),
    );
    let ry0 = (area[1].max(clip[1]) - y0).clamp(0, band_height);
    let ry1 = (area[3].min(clip[3]) - y0).clamp(0, band_height);
    if cx0 >= cx1 || ry0 >= ry1 {
        return;
    }
    let columns: Vec<f32> = (cx0..cx1)
        .map(|x| blur_span(shape.x0, shape.x1, sigma, x as f32 + 0.5))
        .collect();
    let alpha = f32::from(c.a);
    let h = edges.hole;
    for ry in ry0..ry1 {
        let fy = ry + y0;
        let y = fy as f32 + 0.5;
        let row_dark = blur_span(shape.y0, shape.y1, sigma, y);
        if !inset && row_dark * alpha < 0.5 {
            continue;
        }
        let spans = bound.row_spans(fy as f32);
        let (hx0, hx1) = if (h[1]..h[3]).contains(&fy) && h[0] < h[2] {
            (h[0].clamp(cx0, cx1), h[2].clamp(cx0, cx1))
        } else {
            (cx1, cx1)
        };
        let row = ry as usize * band.stride + band.left * 4;
        for x in (cx0..hx0).chain(hx1..cx1) {
            let dark = columns[(x - cx0) as usize] * row_dark;
            let box_cover = match spans.known(x) {
                Some(k) => k,
                None => bound.coverage(x as f32 + 0.5, y),
            };
            let cover = if inset {
                (1.0 - dark) * box_cover
            } else {
                dark * (1.0 - box_cover)
            };
            let a = cover * alpha;
            if a >= 0.5 {
                let at = row + x as usize * 4;
                blend(&mut rows[at..at + 4], c, a.round() as u32);
            }
        }
    }
}

/// Fill a rounded shape with one color (byte order already applied), row by row: the run each
/// row's spans put wholly inside is filled directly, and only the few pixels on the curve work
/// out their coverage. Big pills and circles cost a row, not a pixel, each.
fn fill_shape(rows: &mut [u8], band: Band, y0: i32, shape: &RRect, clip: DeviceClip, c: Color) {
    let band_height = (rows.len() / band.stride) as i32;
    let area = shape.bounds(0.0);
    let (cx0, cx1) = (
        area[0].max(clip[0]).max(0),
        area[2].min(clip[2]).min(band.width as i32),
    );
    let ry0 = (area[1].max(clip[1]) - y0).clamp(0, band_height);
    let ry1 = (area[3].min(clip[3]) - y0).clamp(0, band_height);
    if cx0 >= cx1 {
        return;
    }
    let alpha = u32::from(c.a);
    for ry in ry0..ry1 {
        let fy = ry + y0;
        let spans = shape.row_spans(fy as f32);
        let row = ry as usize * band.stride + band.left * 4;
        let (in0, in1) = match spans.inside {
            Some((l, r)) => (
                (l.ceil() as i32).clamp(cx0, cx1),
                (r.floor() as i32).clamp(cx0, cx1),
            ),
            None => (cx1, cx1),
        };
        let (in0, in1) = if in0 < in1 { (in0, in1) } else { (cx1, cx1) };
        for px in rows[row + in0 as usize * 4..row + in1 as usize * 4]
            .as_chunks_mut::<4>()
            .0
        {
            blend(px, c, alpha);
        }
        let y = fy as f32 + 0.5;
        for x in (cx0..in0.min(cx1)).chain(in1.max(cx0)..cx1) {
            let cover = match spans.known(x) {
                Some(k) => k,
                None => shape.coverage(x as f32 + 0.5, y),
            };
            if cover > 0.0 {
                let at = row + x as usize * 4;
                blend(
                    &mut rows[at..at + 4],
                    c,
                    (cover * alpha as f32).round() as u32,
                );
            }
        }
    }
}

/// Fill `area` (fully inside its shape) with a gradient. Linear gradients step their color
/// table index along each row instead of computing every pixel's position: GitHub's loading
/// skeletons are a third of the frame.
fn fill_gradient(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    area: [i32; 4],
    clip: DeviceClip,
    g: &DeviceGradient,
) {
    let band_height = (rows.len() / band.stride) as i32;
    let (cx0, cx1) = (
        area[0].max(clip[0]).max(0),
        area[2].min(clip[2]).min(band.width as i32),
    );
    let ry0 = (area[1].max(clip[1]) - y0).clamp(0, band_height);
    let ry1 = (area[3].min(clip[3]) - y0).clamp(0, band_height);
    if cx0 >= cx1 || ry0 >= ry1 {
        return;
    }
    let ordered = |mut c: [u8; 4]| {
        if band.bgra {
            c.swap(0, 2);
        }
        c
    };
    let row_pixels = |ry: i32| {
        let start = ry as usize * band.stride + (band.left + cx0 as usize) * 4;
        start..start + (cx1 - cx0) as usize * 4
    };
    let opaque = g.opaque();
    match g.axis() {
        // Every row the same: work out one, then copy it.
        shape::Axis::Horizontal => {
            let (mut i, step) = g
                .row_index(cx0 as f32 + 0.5, 0.5)
                .expect("linear gradients have rows");
            let line: Vec<[u8; 4]> = (cx0..cx1)
                .map(|_| {
                    let c = ordered(g.at_index(i));
                    i += step;
                    c
                })
                .collect();
            let bytes = line.as_flattened();
            for ry in ry0..ry1 {
                let range = row_pixels(ry);
                let dst = &mut rows[range];
                if opaque {
                    dst.copy_from_slice(bytes);
                } else {
                    for (d, c) in dst.as_chunks_mut::<4>().0.iter_mut().zip(&line) {
                        *d = over(u32::from_le_bytes(*c), u32::from_le_bytes(*d)).to_le_bytes();
                    }
                }
            }
        }
        // One color per row.
        shape::Axis::Vertical => {
            for ry in ry0..ry1 {
                let y = (ry + y0) as f32 + 0.5;
                let (i, _) = g.row_index(cx0 as f32 + 0.5, y).expect("linear");
                let c = u32::from_le_bytes(ordered(g.at_index(i)));
                let range = row_pixels(ry);
                for d in rows[range].as_chunks_mut::<4>().0 {
                    *d = over(c, u32::from_le_bytes(*d)).to_le_bytes();
                }
            }
        }
        shape::Axis::Other => {
            for ry in ry0..ry1 {
                let y = (ry + y0) as f32 + 0.5;
                let range = row_pixels(ry);
                let pixels = rows[range].as_chunks_mut::<4>().0;
                match g.row_index(cx0 as f32 + 0.5, y) {
                    Some((i, step)) => {
                        // Fixed point, 16 fraction bits: no float conversions per pixel.
                        let mut fixed = (f64::from(i) * 65536.0) as i64;
                        let step = (f64::from(step) * 65536.0) as i64;
                        for d in pixels {
                            let src = g.pixel_at((fixed + 32768) >> 16, band.bgra);
                            *d = over(src, u32::from_le_bytes(*d)).to_le_bytes();
                            fixed += step;
                        }
                    }
                    None => {
                        let (mut ax, step, by) = g.radial_row(cx0 as f32 + 0.5, y);
                        for d in pixels {
                            let c = g.at_position((ax * ax + by).sqrt());
                            blend_premultiplied(d, ordered(c), 255);
                            ax += step;
                        }
                    }
                }
            }
        }
    }
}

/// Source-over of a premultiplied pixel onto an opaque one, two channels per multiply: the
/// translucent gradients of glass-style pages cover large areas, and this is their per-pixel
/// cost.
#[inline]
fn over(src: u32, dst: u32) -> u32 {
    let inv = 255 - (src >> 24);
    if inv == 0 {
        return src;
    }
    // dst * inv / 255 for the 0x00ff00ff lanes and the 0xff00ff00 lanes, rounded.
    let scale = |lanes: u32| {
        let x = lanes * inv + 0x0080_0080;
        ((x + ((x >> 8) & 0x00ff_00ff)) >> 8) & 0x00ff_00ff
    };
    let rb = scale(dst & 0x00ff_00ff);
    let ga = scale((dst >> 8) & 0x00ff_00ff) << 8;
    // The color channels never pass 255 (premultiplied); alpha stays opaque.
    (src + rb + ga) | 0xff00_0000
}

/// Source-over blend of a premultiplied color at `cover` (0 to 255) onto an opaque pixel.
#[inline]
fn blend_premultiplied(dst: &mut [u8], c: [u8; 4], cover: u32) {
    if cover >= 255 && c[3] == 255 {
        dst.copy_from_slice(&[c[0], c[1], c[2], 255]);
        return;
    }
    let a = u32::from(c[3]) * cover / 255;
    if a == 0 {
        return;
    }
    let inv = 255 - a;
    for k in 0..3 {
        let v = (u32::from(c[k]) * cover + u32::from(dst[k]) * inv + 127) / 255;
        dst[k] = v.min(255) as u8;
    }
    dst[3] = 255;
}

/// Draw premultiplied `bitmap` stretched over `dest` (frame pixels), inside `clip`. The bitmap
/// is decoded at the destination size, so this is a copy unless the file was smaller.
fn draw_image(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    dest: [i32; 4],
    bitmap: &image::Bitmap,
    round: Option<&RRect>,
    clip: DeviceClip,
) {
    let band_height = (rows.len() / band.stride) as i32;
    let [dx0, dy0, dx1, dy1] = dest;
    let (dw, dh) = (i64::from((dx1 - dx0).max(1)), i64::from((dy1 - dy0).max(1)));
    let (bw, bh) = (bitmap.width as usize, bitmap.height as usize);
    let (cx0, cx1) = (dx0.max(clip[0]), dx1.min(clip[2]));
    let ry0 = (dy0.max(clip[1]) - y0).clamp(0, band_height);
    let ry1 = (dy1.min(clip[3]) - y0).clamp(0, band_height);
    for ry in ry0..ry1 {
        let sy = (i64::from(ry + y0 - dy0) * bh as i64 / dh) as usize;
        let src = &bitmap.pixels[sy.min(bh - 1) * bw * 4..];
        for x in cx0..cx1 {
            let sx = ((i64::from(x - dx0) * bw as i64 / dw) as usize).min(bw - 1);
            let p = &src[sx * 4..sx * 4 + 4];
            let mut a = u32::from(p[3]);
            let (mut r, mut g, mut b) = if band.bgra {
                (p[2], p[1], p[0])
            } else {
                (p[0], p[1], p[2])
            };
            if let Some(shape) = round {
                // Premultiplied: scale every channel by the corner's coverage.
                let cover = shape.coverage(x as f32 + 0.5, (ry + y0) as f32 + 0.5);
                if cover < 1.0 {
                    let k = |v: u8| (f32::from(v) * cover).round() as u8;
                    (r, g, b) = (k(r), k(g), k(b));
                    a = (a as f32 * cover).round() as u32;
                }
            }
            if a == 0 {
                continue;
            }
            let at = ry as usize * band.stride + (band.left + x as usize) * 4;
            let dst = &mut rows[at..at + 4];
            if a == 255 {
                dst.copy_from_slice(&[r, g, b, 255]);
            } else {
                let inv = 255 - a;
                let mix =
                    |s: u8, d: u8| (u32::from(s) + (u32::from(d) * inv + 127) / 255).min(255) as u8;
                dst[0] = mix(r, dst[0]);
                dst[1] = mix(g, dst[1]);
                dst[2] = mix(b, dst[2]);
                dst[3] = 255;
            }
        }
    }
}

/// Blend a coverage mask whose top left is at frame position (x, y), inside `clip`.
#[allow(clippy::too_many_arguments)]
fn draw_mask(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    x: i32,
    y: i32,
    g: &GlyphBitmap,
    color: Color,
    clip: DeviceClip,
) {
    let band_height = (rows.len() / band.stride) as i32;
    let gy = y - y0;
    if gy >= band_height || gy + g.height as i32 <= 0 {
        return;
    }
    for row in 0..g.height as i32 {
        let by = gy + row;
        if !(0..band_height).contains(&by) || !(clip[1]..clip[3]).contains(&(by + y0)) {
            continue;
        }
        for col in 0..g.width as i32 {
            let bx = x + col;
            if !(clip[0]..clip[2]).contains(&bx) {
                continue;
            }
            let cov = u32::from(g.coverage[(row as usize) * g.width + col as usize]);
            if cov == 0 {
                continue;
            }
            let at = by as usize * band.stride + (band.left + bx as usize) * 4;
            blend(&mut rows[at..at + 4], color, cov * u32::from(color.a) / 255);
        }
    }
}

/// Source-over blend of `c` at `alpha` (0 to 255) onto an opaque pixel.
fn blend(dst: &mut [u8], c: Color, alpha: u32) {
    match alpha {
        0 => {}
        255.. => dst.copy_from_slice(&[c.r, c.g, c.b, 255]),
        a => {
            let inv = 255 - a;
            let mix =
                |src: u8, d: u8| ((u32::from(src) * a + u32::from(d) * inv + 127) / 255) as u8;
            dst[0] = mix(c.r, dst[0]);
            dst[1] = mix(c.g, dst[1]);
            dst[2] = mix(c.b, dst[2]);
            dst[3] = 255;
        }
    }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use css::{cascade, parse_stylesheet, user_agent_stylesheet, Media, Origin};
    use font::Approximate;

    fn render(html_src: &str, css_src: &str, width: f32, height: f32, scale: f32) -> Frame {
        let dom = html::parse(html_src);
        let author = parse_stylesheet(css_src);
        let media = Media {
            width,
            height,
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
        let laid_out = layout::layout(
            &dom,
            &styles,
            &Approximate,
            &layout::no_images,
            width,
            height,
        );
        let page = Page {
            dom: &dom,
            styles: &styles,
            layout: &laid_out,
            metrics: &Approximate,
            images: None,
        };
        Painter::new()
            .paint(
                &page,
                Viewport {
                    width,
                    height,
                    scale,
                    scroll_y: 0.0,
                },
            )
            .0
    }

    fn pixel(f: &Frame, x: usize, y: usize) -> [u8; 3] {
        let at = (y * f.width + x) * 4;
        [f.pixels[at], f.pixels[at + 1], f.pixels[at + 2]]
    }

    #[test]
    fn paints_canvas_backgrounds_and_borders() {
        let f = render(
            "<body><div></div></body>",
            "body { margin: 0; background: #0369a1 } div { margin: 10px; height: 20px; background: #fff; border: 2px solid #000 }",
            100.0,
            60.0,
            1.0,
        );
        assert_eq!((f.width, f.height), (100, 60));
        assert_eq!(
            pixel(&f, 5, 5),
            [3, 105, 161],
            "canvas takes the body background"
        );
        assert_eq!(pixel(&f, 10, 10), [0, 0, 0], "border");
        assert_eq!(pixel(&f, 50, 20), [255, 255, 255], "box background");
        assert_eq!(pixel(&f, 50, 55), [3, 105, 161]);
    }

    #[test]
    fn rounds_corners_of_backgrounds_and_borders() {
        let f = render(
            "<body><div></div><p></p></body>",
            "body { margin: 0; background: #fff }
             div { margin: 10px; width: 80px; height: 40px; background: #000; border-radius: 10px }
             p { margin: 10px; width: 40px; height: 40px; border-radius: 50%;
                 border: 4px solid #f00; border-right-color: #00f }",
            120.0,
            130.0,
            1.0,
        );
        assert_eq!(pixel(&f, 10, 10), [255, 255, 255], "the corner is cut");
        assert_eq!(pixel(&f, 11, 11), [255, 255, 255]);
        assert_eq!(pixel(&f, 20, 12), [0, 0, 0], "past the curve");
        assert_eq!(pixel(&f, 50, 30), [0, 0, 0]);
        // (13.5, 12.5) is 9.92 px from the arc center (20, 20): on the curve.
        let corner = pixel(&f, 13, 12);
        assert!(corner[0] > 0 && corner[0] < 255, "anti-aliased: {corner:?}");
        // The circle: border box 48x48 at (10, 60), the margins between the boxes collapsing.
        // Left side red, right side blue, inside clear.
        assert_eq!(pixel(&f, 11, 84), [255, 0, 0]);
        assert_eq!(pixel(&f, 56, 84), [0, 0, 255]);
        assert_eq!(pixel(&f, 34, 84), [255, 255, 255], "inside the ring");
        assert_eq!(pixel(&f, 11, 61), [255, 255, 255], "outside the circle");
    }

    #[test]
    fn draws_gradients_over_the_padding_box() {
        let f = render(
            "<body><div></div><p></p></body>",
            "body { margin: 0; background: #fff }
             div { width: 100px; height: 10px; background: linear-gradient(to right, #000, #fff) }
             p { margin: 0; width: 40px; height: 40px;
                 background: radial-gradient(circle closest-side, #f00, #f00 50%, #00f 50%) }",
            100.0,
            60.0,
            1.0,
        );
        let left = pixel(&f, 0, 5)[0];
        let mid = pixel(&f, 50, 5)[0];
        let right = pixel(&f, 99, 5)[0];
        assert!(
            left < 5 && right > 250 && (120..136).contains(&mid),
            "{left} {mid} {right}"
        );
        assert_eq!(pixel(&f, 20, 30), [255, 0, 0], "center of the circle");
        assert_eq!(pixel(&f, 2, 30), [0, 0, 255], "outside half the radius");
    }

    #[test]
    fn draws_outer_and_inset_shadows() {
        let f = render(
            "<body><div></div><p></p></body>",
            "body { margin: 0; background: #fff }
             div { margin: 20px; width: 40px; height: 20px; background: #fff;
                   box-shadow: 10px 0 0 #000 }
             p { margin: 20px; width: 40px; height: 40px; background: #fff;
                 box-shadow: inset 0 0 0 5px #00f, 0 0 8px #000 }",
            120.0,
            140.0,
            1.0,
        );
        assert_eq!(
            pixel(&f, 30, 30),
            [255, 255, 255],
            "the box covers its own shadow"
        );
        assert_eq!(pixel(&f, 65, 30), [0, 0, 0], "a hard shadow 10px right");
        assert_eq!(pixel(&f, 75, 30), [255, 255, 255]);
        // The second box is at (20, 60) to (60, 100): its top margin collapses with the first's.
        assert_eq!(
            pixel(&f, 22, 80),
            [0, 0, 255],
            "inset spread inside the edge"
        );
        assert_eq!(pixel(&f, 40, 80), [255, 255, 255], "the middle stays clear");
        let blur = pixel(&f, 15, 80)[0];
        assert!(
            blur > 60 && blur < 250,
            "the blurred shadow fades out: {blur}"
        );
        assert!(pixel(&f, 18, 80)[0] < blur, "darker nearer the box");
    }

    #[test]
    fn effects_past_the_edges_draw_nothing_and_never_panic() {
        // A window crashed on a rounded, shadowed box lying past the right edge: the span
        // left after clipping was empty and `clamp` panicked on its reversed bounds.
        for left in [-400, -90, -10, 0, 95, 120, 400] {
            for top in [-300, -40, 0, 50, 300] {
                let css = format!(
                    "body {{ margin: 0 }} div {{ position: absolute; left: {left}px; top: {top}px;
                       width: 60px; height: 30px; border: 3px solid red; border-radius: 12px;
                       box-shadow: 0 0 10px #000, inset 2px 2px 6px blue;
                       background: linear-gradient(red, blue) }}
                     p {{ position: absolute; left: {left}px; top: {top}px; margin: 0; width: 40px;
                       height: 40px; border-radius: 50%; background: radial-gradient(red, blue) }}"
                );
                for scale in [1.0, 2.0] {
                    render("<body><div></div><p></p></body>", &css, 100.0, 60.0, scale);
                }
            }
        }
    }

    #[test]
    fn over_matches_the_channel_by_channel_blend() {
        let dsts = [
            [0u8, 0, 0, 255],
            [255, 255, 255, 255],
            [12, 200, 77, 255],
            [3, 99, 254, 255],
        ];
        for dst in dsts {
            for a in [0u32, 1, 26, 128, 153, 254, 255] {
                for v in [0u32, 40, 255] {
                    let c = (v * a / 255) as u8;
                    let src = [c, (a / 2) as u8, 0, a as u8];
                    let mut slow = dst;
                    blend_premultiplied(&mut slow, src, 255);
                    let fast = over(u32::from_le_bytes(src), u32::from_le_bytes(dst)).to_le_bytes();
                    for k in 0..3 {
                        assert!(
                            slow[k].abs_diff(fast[k]) <= 1,
                            "{src:?} over {dst:?}: {slow:?} {fast:?}"
                        );
                    }
                    assert_eq!(fast[3], 255);
                }
            }
        }
    }

    #[test]
    fn groups_cut_by_layers_or_empty_never_recurse_forever() {
        // A group whose range a positioned descendant splits, a rounded clip with no child
        // boxes, nested groups, and a block with a z-index ending a rounded clip's range: its
        // layer cuts the group, which singapay.id turned into endless recursion.
        render(
            "<body><div class=o><p class=r>t<b>u</b></p><i class=a>x</i><span class=z>z</span>
             <div class=o><div class=o><section class=r>s</section></div></div></div>
             <div class=r><p>a</p><div class=z>z</div></div></body>",
            ".o { opacity: 0.5 } .r { overflow: hidden; border-radius: 6px }
             .a { position: absolute; top: 0 } .z { position: relative; z-index: 3 }",
            100.0,
            100.0,
            1.0,
        );
    }

    #[test]
    fn blends_groups_at_their_opacity() {
        // A black box inside a white one, the group at half opacity over red: the inner box
        // must not show the white box through it (as alpha per item would).
        let f = render(
            "<body><div><p></p></div></body>",
            "body { margin: 0; background: #f00 } div { width: 40px; height: 40px;
               background: #fff; opacity: 0.5 } p { margin: 0; width: 20px; height: 20px;
               background: #000 }",
            60.0,
            50.0,
            1.0,
        );
        assert_eq!(pixel(&f, 30, 30), [255, 128, 128], "white at half over red");
        assert_eq!(
            pixel(&f, 5, 5),
            [128, 0, 0],
            "black at half over red, not over white"
        );
        assert_eq!(pixel(&f, 50, 45), [255, 0, 0], "outside");
    }

    #[test]
    fn group_shortcuts_draw_what_the_group_would() {
        // One translucent box: drawn with its alpha instead of a group. A rounded clip whose
        // child stays off the corners: no group at all. Both must match the exact result.
        let f = render(
            "<body><div></div><section><p></p></section></body>",
            "body { margin: 0; background: #fff } div { width: 20px; height: 20px;
               background: #000; opacity: 0.5 } section { margin: 0; width: 40px; height: 40px;
               overflow: hidden; border-radius: 10px; padding: 12px } p { margin: 0;
               width: 16px; height: 16px; background: #00f }",
            60.0,
            80.0,
            1.0,
        );
        // Alpha and interpolation round differently: one either way is the same color.
        let half = pixel(&f, 10, 10);
        assert!(
            half.iter().all(|&c| (127..=128).contains(&c)),
            "black at half over white: {half:?}"
        );
        assert_eq!(pixel(&f, 20, 40), [0, 0, 255], "the child inside the clip");
        assert_eq!(pixel(&f, 2, 22), [255, 255, 255]);
    }

    #[test]
    fn clips_children_to_rounded_corners() {
        let f = render(
            "<body><div><p></p></div></body>",
            "body { margin: 0; background: #fff } div { width: 40px; height: 40px;
               overflow: hidden; border-radius: 20px } p { margin: 0; width: 40px;
               height: 40px; background: #00f }",
            60.0,
            50.0,
            1.0,
        );
        assert_eq!(pixel(&f, 20, 20), [0, 0, 255]);
        assert_eq!(
            pixel(&f, 1, 1),
            [255, 255, 255],
            "the child is cut at the corner"
        );
        assert_eq!(pixel(&f, 38, 38), [255, 255, 255]);
        assert_eq!(pixel(&f, 20, 1), [0, 0, 255], "the straight edge stays");
    }

    #[test]
    fn paints_positioned_boxes_in_z_order() {
        let f = render(
            "<body><div id=a></div><div id=b></div><section></section></body>",
            "body { margin: 0; background: #fff } div, section { width: 20px; height: 20px }
             #a { position: absolute; z-index: 2; background: #f00 }
             #b { position: absolute; z-index: 1; background: #00f }
             section { position: relative; z-index: 3; background: #0f0; top: -10px; left: 10px }",
            40.0,
            40.0,
            1.0,
        );
        assert_eq!(
            pixel(&f, 5, 5),
            [255, 0, 0],
            "z 2 over z 1, though laid out first"
        );
        assert_eq!(
            pixel(&f, 15, 5),
            [0, 255, 0],
            "the relative box with z 3 over both"
        );
    }

    #[test]
    fn scales_for_high_density_screens() {
        let f = render(
            "<body><div></div></body>",
            "body { margin: 0 } div { height: 10px; background: #000 }",
            50.0,
            30.0,
            2.0,
        );
        assert_eq!((f.width, f.height), (100, 60));
        assert_eq!(pixel(&f, 10, 19), [0, 0, 0]);
        assert_eq!(
            pixel(&f, 10, 20),
            [255, 255, 255],
            "10 CSS px is 20 device px"
        );
    }

    #[test]
    fn bands_cover_every_row() {
        // An odd height that does not divide evenly between threads.
        let f = render(
            "<body></body>",
            "body { background: #123456 }",
            37.0,
            97.0,
            1.0,
        );
        for y in [0, 48, 96] {
            assert_eq!(pixel(&f, 36, y), [0x12, 0x34, 0x56], "row {y}");
        }
    }

    #[test]
    fn places_glyphs_on_quarter_pixels() {
        assert_eq!(place(10.0), (10, 0));
        assert_eq!(place(10.3), (10, 1));
        assert_eq!(place(10.5), (10, 2));
        assert_eq!(place(10.74), (10, 3));
        assert_eq!(place(10.9), (11, 0), "rounds to the nearest quarter");
        assert_eq!(place(-0.3), (-1, 3), "left of the edge too");
    }

    #[test]
    fn skipping_runs_out_of_view_draws_the_same_pixels() {
        // A long page: rows with a border and text, a fixed bar, and an absolutely positioned
        // box far down (in its own layer, painted after the long flow).
        let rows: String = (0..2000)
            .map(|i| format!("<div class=r><p>Row {i} text</p></div>"))
            .collect();
        let dom = html::parse(&format!("<div id=bar></div><div id=abs></div>{rows}"));
        let author = parse_stylesheet(
            "body { margin: 0 } .r { border-bottom: 1px solid #888; height: 20px } \
             p { margin: 0; font-size: 12px } #bar { position: fixed; top: 0; height: 10px; \
             width: 50px; background: #f00 } #abs { position: absolute; top: 15000px; \
             left: 5px; width: 30px; height: 30px; background: #00f }",
        );
        let styles = cascade(
            &dom,
            &[
                (Origin::UserAgent, user_agent_stylesheet()),
                (Origin::Author, &author),
            ],
            &Media::default(),
        );
        let mut laid_out = layout::layout(
            &dom,
            &styles,
            &Approximate,
            &layout::no_images,
            200.0,
            100.0,
        );
        assert!(laid_out.boxes.len() > 2000 && laid_out.box_extents.len() > 5);
        let paint = |layout: &Layout, scroll_y: f32| {
            let page = Page {
                dom: &dom,
                styles: &styles,
                layout,
                metrics: &Approximate,
                images: None,
            };
            let view = Viewport {
                width: 200.0,
                height: 100.0,
                scale: 2.0,
                scroll_y,
            };
            Painter::new().paint(&page, view).0.pixels
        };
        // Views whose edge is 40 points into a run of boxes (borders every 21 points are drawn
        // there), where a wrong extent would drop them. Text draws nothing without fonts here.
        let mut scrolls = vec![0.0, 5000.0, 14_980.0, 39_900.0];
        for &(top, bottom) in &laid_out.box_extents[1..5] {
            scrolls.push(bottom - 40.0);
            scrolls.push(top - 100.0 + 40.0);
        }
        let fast: Vec<Vec<u8>> = scrolls.iter().map(|&y| paint(&laid_out, y)).collect();
        // Without the index: one run covering everything.
        laid_out.box_extents = vec![(f32::MIN, f32::MAX); laid_out.box_extents.len()];
        laid_out.text_extents = vec![(f32::MIN, f32::MAX); laid_out.text_extents.len()];
        for (i, &y) in scrolls.iter().enumerate() {
            assert!(fast[i] == paint(&laid_out, y), "same pixels at scroll {y}");
        }
    }

    #[test]
    fn paints_into_padded_bgra_surfaces() {
        let dom = html::parse("<body></body>");
        let sheet = parse_stylesheet("body { background: #102030 }");
        let styles = cascade(&dom, &[(Origin::Author, &sheet)], &Media::default());
        let laid_out = layout::layout(&dom, &styles, &Approximate, &layout::no_images, 3.0, 2.0);
        let page = Page {
            dom: &dom,
            styles: &styles,
            layout: &laid_out,
            metrics: &Approximate,
            images: None,
        };
        // 3 pixels per row padded to 16 bytes; the padding must stay untouched.
        let mut pixels = vec![7u8; 16 * 2];
        let target = Target {
            pixels: &mut pixels,
            stride: 16,
            bgra: true,
            left: 0,
        };
        Painter::new().paint_into(
            &page,
            Viewport {
                width: 3.0,
                height: 2.0,
                scale: 1.0,
                scroll_y: 0.0,
            },
            target,
        );
        assert_eq!(&pixels[0..4], &[0x30, 0x20, 0x10, 255], "blue first");
        assert_eq!(&pixels[12..16], &[7, 7, 7, 7], "row padding untouched");
        assert_eq!(&pixels[16..20], &[0x30, 0x20, 0x10, 255]);

        // The same page as a pane two pixels in from the left of a 6-pixel-wide frame.
        let mut pixels = vec![7u8; 24 * 2];
        let viewport = Viewport {
            width: 3.0,
            height: 2.0,
            scale: 1.0,
            scroll_y: 0.0,
        };
        let target = Target {
            pixels: &mut pixels,
            stride: 24,
            bgra: true,
            left: 2,
        };
        Painter::new().paint_into(&page, viewport, target);
        for row in [0, 24] {
            assert_eq!(&pixels[row..row + 8], &[7; 8], "left of the pane untouched");
            assert_eq!(&pixels[row + 8..row + 12], &[0x30, 0x20, 0x10, 255]);
            assert_eq!(
                &pixels[row + 20..row + 24],
                &[7; 4],
                "right of the pane untouched"
            );
        }
    }

    #[test]
    fn overlays_clipped_fills_and_masks_without_clearing() {
        let mut pixels = vec![9u8; 4 * 4 * 2];
        let mask = GlyphBitmap {
            left: 0,
            top: 0,
            width: 2,
            height: 1,
            coverage: vec![255, 0],
        };
        let ops = [
            Op::Fill {
                x0: 0,
                y0: 1,
                x1: 4,
                y1: 2,
                color: Color::rgb(255, 0, 0),
            },
            Op::Mask {
                x: 0,
                y: 0,
                mask: &mask,
                color: Color::rgb(0, 0, 255),
            },
        ];
        let target = Target {
            pixels: &mut pixels,
            stride: 16,
            bgra: false,
            left: 0,
        };
        Painter::new().overlay(target, 4, (0, 3), &Approximate, &ops);
        assert_eq!(&pixels[0..4], &[0, 0, 255, 255], "mask pixel");
        assert_eq!(
            &pixels[4..8],
            &[9, 9, 9, 9],
            "zero coverage leaves the pixel alone"
        );
        assert_eq!(&pixels[16..20], &[255, 0, 0, 255], "fill");
        assert_eq!(&pixels[28..32], &[9, 9, 9, 9], "column 3 is clipped");
    }

    /// Every `<img>` shows the same 13x9 test PNG (top-left pixel 0,0,40 with alpha 0).
    struct OneImage;

    impl ImageSource for OneImage {
        fn image(&self, _: NodeId) -> Option<(u64, &[u8])> {
            Some((7, include_bytes!("../../image/src/testdata/rgb8.png")))
        }
    }

    #[test]
    fn draws_images_and_reuses_decoded_pixels() {
        let dom = html::parse("<img id=a width=26 height=18><img id=b width=26 height=18>");
        let author = parse_stylesheet("body { margin: 0 } img { display: block }");
        let media = Media {
            width: 40.0,
            height: 40.0,
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
        let natural = |_: NodeId| Some((13.0, 9.0));
        let laid_out = layout::layout(&dom, &styles, &Approximate, &natural, 40.0, 40.0);
        let page = Page {
            dom: &dom,
            styles: &styles,
            layout: &laid_out,
            metrics: &Approximate,
            images: Some(&OneImage),
        };
        let view = Viewport {
            width: 40.0,
            height: 40.0,
            scale: 1.0,
            scroll_y: 0.0,
        };
        let mut painter = Painter::new();
        let (frame, stats) = painter.paint(&page, view);
        assert_eq!(
            stats.images_decoded, 1,
            "two elements, one file at one size: one decode"
        );
        // rgb8.png's pixel (x, y) is ((x*19+y*7)%256, (x*5+y*29)%256, (x*y*3+40)%256); drawn 2x.
        assert_eq!(
            pixel(&frame, 2 * 3 + 1, 2 * 2 + 1),
            [
                (3 * 19 + 2 * 7) as u8,
                (3 * 5 + 2 * 29) as u8,
                (3 * 2 * 3 + 40) as u8
            ]
        );
        assert_eq!(
            pixel(&frame, 7, 18 + 5),
            pixel(&frame, 7, 5),
            "the second image below the first"
        );
        assert_eq!(
            pixel(&frame, 30, 5),
            [255, 255, 255],
            "nothing beyond the image"
        );
        let (_, again) = painter.paint(&page, view);
        assert_eq!(
            again.images_decoded, 0,
            "decoded pixels are kept between frames"
        );
        assert_eq!(
            again.image_cache_bytes,
            13 * 9 * 4,
            "kept at the file's size, stretched when drawn"
        );
    }

    #[test]
    fn clips_overflow_hides_invisible_and_pins_fixed_layers() {
        let css = "body { margin: 0 } \
                   #clip { height: 10px; overflow: hidden } #clip div { height: 40px; background: #f00 } \
                   #ghost { height: 10px; background: #0f0; visibility: hidden } \
                   #fix { position: fixed; top: 0; right: 0; width: 5px; height: 5px; background: #00f } \
                   #tall { height: 400px }";
        let html_src = "<div id=clip><div></div></div><div id=ghost></div><div id=fix></div><div id=tall></div>";
        let dom = html::parse(html_src);
        let author = parse_stylesheet(css);
        let media = Media {
            width: 50.0,
            height: 60.0,
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
        let laid_out = layout::layout(&dom, &styles, &Approximate, &layout::no_images, 50.0, 60.0);
        let page = Page {
            dom: &dom,
            styles: &styles,
            layout: &laid_out,
            metrics: &Approximate,
            images: None,
        };
        let mut painter = Painter::new();
        let at = |f: &Frame, x: usize, y: usize| pixel(f, x, y);

        let (f, _) = painter.paint(
            &page,
            Viewport {
                width: 50.0,
                height: 60.0,
                scale: 1.0,
                scroll_y: 0.0,
            },
        );
        assert_eq!(at(&f, 10, 5), [255, 0, 0], "inside the clip");
        assert_eq!(
            at(&f, 10, 15),
            [255, 255, 255],
            "the 40px child is cut at 10px; the hidden box is not drawn"
        );
        assert_eq!(at(&f, 47, 2), [0, 0, 255], "fixed box");

        let (f, _) = painter.paint(
            &page,
            Viewport {
                width: 50.0,
                height: 60.0,
                scale: 1.0,
                scroll_y: 100.0,
            },
        );
        assert_eq!(
            at(&f, 47, 2),
            [0, 0, 255],
            "fixed box stays put when scrolled"
        );
        assert_eq!(at(&f, 10, 5), [255, 255, 255]);
    }

    #[test]
    fn blends_translucent_colors() {
        let mut px = [255, 255, 255, 255];
        blend(
            &mut px,
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 128,
            },
            128,
        );
        assert_eq!(px, [127, 127, 127, 255]);
    }
}
