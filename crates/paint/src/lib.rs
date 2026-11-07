//! Painting: layout to a display list to pixels.
//!
//! Stages 10 to 12 of `docs/pipeline.md`: the display list is built in one pass, every glyph
//! it needs is rasterized into the cache (sequential, the only writer), then the framebuffer
//! is composited in horizontal bands on scoped threads that only read the list and the cache.

pub mod png;
mod raster;

use std::collections::HashMap;
use std::time::Instant;

use css::{Color, Length, StyleMap};
use font::{FontSet, TextMetrics};
use html::{Dom, FastBuildHasher, NodeId};
use layout::{Layout, Rect};

pub use raster::{rasterize_glyph, GlyphBitmap, Rasterizer};

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
    /// Text is positioned with these metrics, so it lines up with layout.
    pub metrics: &'a dyn TextMetrics,
    /// Outlines for drawing glyphs. Without fonts, text is laid out but not drawn.
    pub fonts: Option<&'a FontSet>,
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
    bold: bool,
    glyph: u32,
    /// Device pixel size in quarter pixels.
    size: u32,
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
    /// A decoded image stretched over the device rectangle.
    Image {
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        key: ImageKey,
        clip: DeviceClip,
    },
    /// A coverage mask drawn in one color, such as an icon. (x, y) is its top left.
    Mask {
        x: i32,
        y: i32,
        mask: &'a GlyphBitmap,
        color: Color,
        clip: DeviceClip,
    },
}

/// Walks clip ranges (which nest) alongside ascending fragment indices, keeping the
/// intersection of every clip that covers the current index on a stack.
struct ClipTracker {
    ranges: Vec<(u32, u32, Rect)>,
    next: usize,
    stack: Vec<(u32, Rect)>,
}

impl ClipTracker {
    fn new(mut ranges: Vec<(u32, u32, Rect)>) -> ClipTracker {
        // Outer clips first when several start at the same index.
        ranges.sort_by_key(|r| (r.0, std::cmp::Reverse(r.1)));
        ClipTracker {
            ranges,
            next: 0,
            stack: Vec::new(),
        }
    }

    /// The clip for fragment `i`. Calls must come with non-decreasing `i`.
    fn at(&mut self, i: u32) -> Option<Rect> {
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
        bold: bool,
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
            let bitmap = match page.fonts {
                Some(fonts) => {
                    rasterize_glyph(fonts.face(key.bold), key.glyph, key.size as f32 / 4.0)
                }
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
        fonts: Option<&FontSet>,
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
                    bold,
                    color,
                } => {
                    let Some(fonts) = fonts else { continue };
                    let size_key = (size * 4.0).round() as u32;
                    let mut pen = x;
                    for ch in text.chars() {
                        if !ch.is_whitespace() {
                            let key = GlyphKey {
                                bold,
                                glyph: fonts.face(bold).glyph(ch),
                                size: size_key,
                            };
                            self.glyphs.entry(key).or_insert_with(|| {
                                rasterize_glyph(fonts.face(bold), key.glyph, size)
                            });
                            items.push(Item::Glyph {
                                x: pen.round() as i32,
                                y: baseline.round() as i32,
                                key,
                                color,
                                clip: NO_CLIP,
                            });
                        }
                        pen += metrics.advance_em(ch, bold) * size;
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
    pub fn forget_images(&mut self) {
        self.images = HashMap::default();
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

            for i in boxes {
                let clip = to_clip(box_clips.at(i as u32));
                let b = &layout.boxes[i];
                let (x0, y0, x1, y1) = device(b.rect);
                let style = page.styles.style(b.node);
                if !visible(y0, y1) || !style.visible || style.transparent {
                    continue;
                }
                if style.background_color.a > 0 {
                    items.push(Item::Fill {
                        x0,
                        y0,
                        x1,
                        y1,
                        color: style.background_color,
                        clip,
                    });
                }
                let bw = style.border_width.map(|w| (w.to_f32() * s).round() as i32);
                let sides = [
                    (x0, y0, x1, y0 + bw[0]),
                    (x1 - bw[1], y0, x1, y1),
                    (x0, y1 - bw[2], x1, y1),
                    (x0, y0, x0 + bw[3], y1),
                ];
                for (side, (sx0, sy0, sx1, sy1)) in sides.into_iter().enumerate() {
                    let color = style.border_color[side];
                    if bw[side] > 0 && color.a > 0 {
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
                            clip,
                        });
                    }
                }
            }

            for i in texts {
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
                    continue;
                }
                let bold = style.font_weight >= 600;
                let color = style.color;
                let device_baseline = (baseline * s).round() as i32;
                let size_key = (size * s * 4.0).round() as u32;
                let mut pen = t.x;
                for ch in layout.text(t).chars() {
                    if !ch.is_whitespace() {
                        if let Some(fonts) = page.fonts {
                            let key = GlyphKey {
                                bold,
                                glyph: fonts.face(bold).glyph(ch),
                                size: size_key,
                            };
                            wanted.push(key);
                            items.push(Item::Glyph {
                                x: (pen * s).round() as i32,
                                y: device_baseline,
                                key,
                                color,
                                clip,
                            });
                        }
                    }
                    pen += page.metrics.advance_em(ch, bold) * size;
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
    let band_height = (rows.len() / band.stride) as i32;
    let canvas = order(band.canvas);
    if band.fill_canvas {
        for row in rows.chunks_mut(band.stride) {
            for px in row[band.left * 4..(band.left + band.width) * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&[canvas.r, canvas.g, canvas.b, 255]);
            }
        }
    }
    let (lo, hi) = (band.clip.0.max(0), band.clip.1.min(band.width as i32));
    let pixel = |x: i32, y: i32| y as usize * band.stride + (band.left + x as usize) * 4;
    for item in items {
        match *item {
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
                draw_image(rows, band, y0, [x0, iy0, x1, y1], bitmap, rect);
            }
        }
    }
}

/// Draw premultiplied `bitmap` stretched over `dest` (frame pixels), inside `clip`. The bitmap
/// is decoded at the destination size, so this is a copy unless the file was smaller.
fn draw_image(
    rows: &mut [u8],
    band: Band,
    y0: i32,
    dest: [i32; 4],
    bitmap: &image::Bitmap,
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
            let a = u32::from(p[3]);
            if a == 0 {
                continue;
            }
            let (r, g, b) = if band.bgra {
                (p[2], p[1], p[0])
            } else {
                (p[0], p[1], p[2])
            };
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
            fonts: None,
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
            fonts: None,
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
        Painter::new().overlay(target, 4, (0, 3), None, &Approximate, &ops);
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
            fonts: None,
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
            fonts: None,
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
