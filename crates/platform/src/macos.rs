//! AppKit through the Objective-C runtime, with no bindings crate.
//!
//! Every message send goes through `objc_msgSend`, cast to the exact C signature of the method
//! being called. The casts are the dangerous part: a wrong argument or return type is undefined
//! behaviour, so each call site states its signature in the `send!` macro.
//!
//! Threading (see `docs/pipeline.md`): `run_app` keeps the main thread in the AppKit run loop
//! and runs the caller's work on a separate engine thread. Input travels main to engine over a
//! channel; titles and frames travel engine to main with `dispatch_async_f`. The engine never
//! touches AppKit.
//!
//! Frames are painted straight into `IOSurface`s, which Core Animation shows as textures
//! without copying. Two surfaces alternate (double buffering), so the engine never paints into
//! the frame that is on screen.
//!
//! The pane header has a layer of its own above the frame, with its own small surfaces. A
//! change to the header alone (typing in the address field, the loading line) paints only
//! that strip; a full frame includes the header and hides the layer again.

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

use crate::{Event, Key, KeyEvent};

type Id = *mut c_void;
type Sel = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

// libSystem (always linked): `size_t malloc_zone_pressure_relief(malloc_zone_t *zone, size_t goal)`.
extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
}

/// Give memory that has been freed but is still held by the allocator back to the system.
/// Loading a page frees a lot (stylesheet text, parse scratch); without this it keeps counting
/// against the process until the system runs short.
pub fn release_free_memory() -> usize {
    // SAFETY: a null zone means every zone; a goal of 0 means as much as possible.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    fn objc_registerClassPair(class: Id);
    fn class_addMethod(class: Id, name: Sel, imp: *const c_void, types: *const c_char) -> bool;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    static NSPasteboardTypeString: Id;
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    static kCAGravityTopLeft: Id;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreateMutable(
        allocator: *const c_void,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
    fn CFNumberCreate(
        allocator: *const c_void,
        number_type: isize,
        value: *const c_void,
    ) -> *mut c_void;
    fn CFRetain(object: *const c_void) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: *const c_void;
    static kIOSurfaceHeight: *const c_void;
    static kIOSurfaceBytesPerElement: *const c_void;
    static kIOSurfacePixelFormat: *const c_void;
    fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    fn IOSurfaceIsInUse(surface: *mut c_void) -> bool;
}

// libdispatch is part of libSystem, which every macOS binary links.
extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Send an Objective-C message with an explicit C signature:
/// `send!(ReturnType; receiver, c"selector:", arg => ArgType, ...)`.
macro_rules! send {
    ($ret:ty; $obj:expr, $sel:expr $(, $arg:expr => $ty:ty)* $(,)?) => {{
        let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f($obj, sel($sel) $(, $arg)*)
    }};
}

fn sel(name: &CStr) -> Sel {
    // SAFETY: sel_registerName only reads the NUL-terminated name.
    unsafe { sel_registerName(name.as_ptr()) }
}

fn class(name: &CStr) -> Id {
    // SAFETY: objc_getClass only reads the NUL-terminated name.
    unsafe { objc_getClass(name.as_ptr()) }
}

/// An `NSString` holding a copy of `s`. The caller owns it.
unsafe fn ns_string(s: &str) -> Id {
    const UTF8: usize = 4;
    let alloc = send!(Id; class(c"NSString"), c"alloc");
    send!(Id; alloc, c"initWithBytes:length:encoding:", s.as_ptr() => *const u8, s.len() => usize, UTF8 => usize)
}

/// Copy an `NSString` into a Rust string. `nil` gives an empty string.
unsafe fn rust_string(ns: Id) -> String {
    if ns.is_null() {
        return String::new();
    }
    let utf8 = send!(*const c_char; ns, c"UTF8String");
    if utf8.is_null() {
        return String::new();
    }
    CStr::from_ptr(utf8).to_string_lossy().into_owned()
}

/// `-[NSView bounds]`. A 32-byte struct return: registers on arm64, a hidden pointer on x86_64.
unsafe fn bounds(view: Id) -> CGRect {
    rect_of(view, c"bounds")
}

/// Call a no-argument method returning `CGRect` (`- (NSRect)bounds`, `frame`, `visibleFrame`).
/// x86_64 returns structs this large through a hidden pointer (`objc_msgSend_stret`).
unsafe fn rect_of(object: Id, selector: &CStr) -> CGRect {
    #[cfg(target_arch = "x86_64")]
    {
        let f: unsafe extern "C" fn(*mut CGRect, Id, Sel) =
            std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn());
        let mut rect = CGRect::default();
        f(&mut rect, object, sel(selector));
        rect
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        send!(CGRect; object, selector)
    }
}

/// The window, its content view and layer: created on the main thread and only used there.
static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static VIEW: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static LAYER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static HEADER_LAYER: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static EVENTS: OnceLock<Sender<Event>> = OnceLock::new();

fn emit(event: Event) {
    if let Some(tx) = EVENTS.get() {
        // The engine may have exited; dropping the event is fine then.
        let _ = tx.send(event);
    }
}

struct Pool {
    width: usize,
    height: usize,
    surfaces: Vec<*mut c_void>,
    /// The surface most recently handed to the main thread: on screen or about to be.
    last: *mut c_void,
}

impl Default for Pool {
    fn default() -> Pool {
        Pool {
            width: 0,
            height: 0,
            surfaces: Vec::new(),
            last: std::ptr::null_mut(),
        }
    }
}

/// The engine thread's handle to the window. It holds no AppKit objects; every effect is
/// scheduled onto the main thread. Not `Sync`: it lives on the engine thread only.
pub struct Window {
    scale: f64,
    width: f64,
    height: f64,
    dark: bool,
    pool: RefCell<Pool>,
    /// Surfaces of the header layer: as wide as the pane and as tall as the header.
    header_pool: RefCell<Pool>,
}

/// A header surface on its way to the main thread, with where to show it (device pixels
/// from the content area's top left).
struct HeaderFrame {
    surface: *mut c_void,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    scale: f64,
}

// SAFETY: the surfaces in the pool are IOSurfaceRefs, which are thread-safe reference-counted
// objects. The Window moves to the engine thread once and is used only there.
unsafe impl Send for Window {}

impl Window {
    /// Device pixels per point (2 on Retina displays).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The content area at launch, in points. Later sizes arrive as `Event::Resize`.
    pub fn size(&self) -> (f64, f64) {
        (self.width, self.height)
    }

    /// Whether macOS uses the dark appearance.
    pub fn dark(&self) -> bool {
        self.dark
    }

    pub fn set_title(&self, title: &str) {
        let boxed = Box::new(title.to_string());
        // SAFETY: the main queue outlives the process; the context is reclaimed by `title_on_main`.
        unsafe {
            dispatch_async_f(
                &_dispatch_main_q,
                Box::into_raw(boxed).cast(),
                title_on_main,
            )
        };
    }

    /// Put `text` on the clipboard, as plain text.
    pub fn copy_text(&self, text: &str) {
        let boxed = Box::new(text.to_string());
        // SAFETY: the main queue outlives the process; the context is reclaimed by `copy_on_main`.
        unsafe { dispatch_async_f(&_dispatch_main_q, Box::into_raw(boxed).cast(), copy_on_main) };
    }

    /// Paint a frame of `width` x `height` device pixels and show it. `draw` receives surface
    /// memory (BGRA, rows of `stride` bytes), which it must fill, and the previous frame when it
    /// has the same size and stride, to copy parts that did not change (panes that are not
    /// focused). The frame includes the header, so the header layer is hidden when it appears.
    pub fn render(
        &self,
        width: usize,
        height: usize,
        draw: impl FnOnce(&mut [u8], usize, Option<&[u8]>),
    ) -> Result<(), String> {
        let surface = paint(&mut self.pool.borrow_mut(), width, height, draw)?;
        // SAFETY: the main queue outlives the process; `surface_on_main` releases the
        // reference `paint` took for it.
        unsafe { dispatch_async_f(&_dispatch_main_q, surface, surface_on_main) };
        Ok(())
    }

    /// Paint only the header: `width` x `height` device pixels at `left`, `top` from the
    /// content area's top left, shown in the header layer above the last frame.
    pub fn render_header(
        &self,
        (left, top): (usize, usize),
        width: usize,
        height: usize,
        draw: impl FnOnce(&mut [u8], usize),
    ) -> Result<(), String> {
        let surface = paint(
            &mut self.header_pool.borrow_mut(),
            width,
            height,
            |m, s, _| draw(m, s),
        )?;
        let frame = Box::new(HeaderFrame {
            surface,
            left,
            top,
            width,
            height,
            scale: self.scale,
        });
        // SAFETY: as in `render`; `header_on_main` reclaims the box and the reference.
        unsafe {
            dispatch_async_f(
                &_dispatch_main_q,
                Box::into_raw(frame).cast(),
                header_on_main,
            )
        };
        Ok(())
    }
}

/// Paint into a surface of `pool` that is not on screen. Returns it with an extra reference
/// for the main thread.
fn paint(
    pool: &mut Pool,
    width: usize,
    height: usize,
    draw: impl FnOnce(&mut [u8], usize, Option<&[u8]>),
) -> Result<*mut c_void, String> {
    // SAFETY: IOSurface calls are thread-safe. The memory is touched only between lock and
    // unlock, and never while the surface is on screen (see `pick`).
    unsafe {
        if (pool.width, pool.height) != (width, height) {
            for s in pool.surfaces.drain(..) {
                CFRelease(s);
            }
            pool.width = width;
            pool.height = height;
            pool.last = std::ptr::null_mut();
        }
        let surface = pick(pool)?;
        if IOSurfaceLock(surface, 0, std::ptr::null_mut()) != 0 {
            return Err("could not lock the frame surface".into());
        }
        let stride = IOSurfaceGetBytesPerRow(surface);
        let rows = IOSurfaceGetHeight(surface);
        let memory = std::slice::from_raw_parts_mut(
            IOSurfaceGetBaseAddress(surface).cast::<u8>(),
            stride * rows,
        );
        // The previous frame, locked for reading only (the window server may be showing it).
        const READ_ONLY: u32 = 1;
        let last = pool.last;
        let previous = (!last.is_null()
            && last != surface
            && IOSurfaceGetBytesPerRow(last) == stride
            && IOSurfaceGetHeight(last) == rows
            && IOSurfaceLock(last, READ_ONLY, std::ptr::null_mut()) == 0)
            .then(|| {
                std::slice::from_raw_parts(
                    IOSurfaceGetBaseAddress(last).cast::<u8>().cast_const(),
                    stride * rows,
                )
            });
        let locked_previous = previous.is_some();
        draw(memory, stride, previous);
        if locked_previous {
            IOSurfaceUnlock(last, READ_ONLY, std::ptr::null_mut());
        }
        IOSurfaceUnlock(surface, 0, std::ptr::null_mut());
        pool.last = surface;
        // The main thread gets its own reference and releases it after handing the surface
        // to the layer.
        CFRetain(surface);
        Ok(surface)
    }
}

impl Window {
    /// Release the frame surface that is not on screen. Double buffering needs it only while
    /// frames are being drawn; idle, it is 16 MB of a Retina window doing nothing. The next
    /// frame creates it again.
    pub fn release_spare(&self) {
        release_free_memory();
        for pool in [&self.pool, &self.header_pool] {
            let mut pool = pool.borrow_mut();
            let last = pool.last;
            pool.surfaces.retain(|&s| {
                if s == last {
                    return true;
                }
                // SAFETY: the pool owns one reference to each surface; the layer keeps its own.
                unsafe { CFRelease(s) };
                false
            });
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        for pool in [self.pool.get_mut(), self.header_pool.get_mut()] {
            for s in pool.surfaces.drain(..) {
                // SAFETY: the pool owns one reference to each surface.
                unsafe { CFRelease(s) };
            }
        }
    }
}

/// A surface to paint the next frame into: never the latest frame (on screen or about to be).
/// At most two exist, so a Retina window holds two frames (32 MB at 2560x1600), not more.
/// When the window server still holds the other one, wait up to a few frames for it.
unsafe fn pick(pool: &mut Pool) -> Result<*mut c_void, String> {
    const WAIT_STEP: std::time::Duration = std::time::Duration::from_millis(2);
    const MAX_WAIT_STEPS: u32 = 25;
    if pool.surfaces.len() < 2
        && pool
            .surfaces
            .iter()
            .all(|&s| s == pool.last || IOSurfaceIsInUse(s))
    {
        let surface = create_surface(pool.width, pool.height)?;
        pool.surfaces.push(surface);
        return Ok(surface);
    }
    let other = *pool
        .surfaces
        .iter()
        .find(|&&s| s != pool.last)
        .ok_or("no spare frame surface")?;
    for _ in 0..MAX_WAIT_STEPS {
        if !IOSurfaceIsInUse(other) {
            break;
        }
        std::thread::sleep(WAIT_STEP);
    }
    // After the wait the surface is used regardless: a rare torn frame beats a third 16 MB buffer.
    Ok(other)
}

/// A BGRA surface. The caller owns the returned reference.
unsafe fn create_surface(width: usize, height: usize) -> Result<*mut c_void, String> {
    const SINT32: isize = 3;
    const BGRA: i32 = 0x4247_5241; // 'BGRA'
    let props = CFDictionaryCreateMutable(
        std::ptr::null(),
        4,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    );
    for (key, value) in [
        (kIOSurfaceWidth, width as i32),
        (kIOSurfaceHeight, height as i32),
        (kIOSurfaceBytesPerElement, 4),
        (kIOSurfacePixelFormat, BGRA),
    ] {
        let number = CFNumberCreate(std::ptr::null(), SINT32, (&value as *const i32).cast());
        CFDictionarySetValue(props, key, number);
        CFRelease(number);
    }
    let surface = IOSurfaceCreate(props);
    CFRelease(props);
    if surface.is_null() {
        return Err(format!("could not create a {width}x{height} frame surface"));
    }
    Ok(surface)
}

/// Run `change` inside a Core Animation transaction without implicit animations: layers
/// that do not back a view would otherwise fade new contents and slide to new frames.
unsafe fn without_animation(change: impl FnOnce()) {
    let transaction = class(c"CATransaction");
    send!((); transaction, c"begin");
    send!((); transaction, c"setDisableActions:", true => bool);
    change();
    send!((); transaction, c"commit");
}

extern "C" fn surface_on_main(surface: *mut c_void) {
    let layer = LAYER.load(Ordering::Acquire);
    let header = HEADER_LAYER.load(Ordering::Acquire);
    // SAFETY: main thread. The layer retains the surface; the reference sent by `render` is
    // released here. -setHidden: takes a BOOL.
    unsafe {
        without_animation(|| {
            if !layer.is_null() {
                send!((); layer, c"setContents:", surface => Id);
            }
            // The full frame has the current header in it.
            if !header.is_null() {
                send!((); header, c"setHidden:", true => bool);
            }
        });
        CFRelease(surface);
    }
}

extern "C" fn header_on_main(context: *mut c_void) {
    // SAFETY: `context` came from Box::into_raw in render_header and is used exactly once.
    let frame = unsafe { Box::from_raw(context.cast::<HeaderFrame>()) };
    let (header, view) = (
        HEADER_LAYER.load(Ordering::Acquire),
        VIEW.load(Ordering::Acquire),
    );
    // SAFETY: main thread; header is a live CALayer and view our content view. -setFrame:
    // takes a CGRect, -setContentsScale: a CGFloat, -setHidden: a BOOL.
    unsafe {
        if !header.is_null() && !view.is_null() {
            let s = frame.scale.max(1.0);
            let height = frame.height as f64 / s;
            // The view's layer has its origin at the bottom left: the header sits at the top.
            let rect = CGRect {
                x: frame.left as f64 / s,
                y: bounds(view).height - frame.top as f64 / s - height,
                width: frame.width as f64 / s,
                height,
            };
            without_animation(|| {
                send!((); header, c"setContentsScale:", s => f64);
                send!((); header, c"setFrame:", rect => CGRect);
                send!((); header, c"setContents:", frame.surface => Id);
                send!((); header, c"setHidden:", false => bool);
            });
        }
        CFRelease(frame.surface);
    }
}

extern "C" fn title_on_main(context: *mut c_void) {
    // SAFETY: `context` came from Box::into_raw in set_title and is used exactly once.
    let title = unsafe { Box::from_raw(context.cast::<String>()) };
    let window = WINDOW.load(Ordering::Acquire);
    if window.is_null() {
        return;
    }
    // SAFETY: main thread; window is a live NSWindow.
    unsafe {
        let s = ns_string(&title);
        send!((); window, c"setTitle:", s => Id);
        send!((); s, c"release");
    }
}

extern "C" fn copy_on_main(context: *mut c_void) {
    // SAFETY: `context` came from Box::into_raw in copy_text and is used exactly once.
    let text = unsafe { Box::from_raw(context.cast::<String>()) };
    // SAFETY: main thread. -[NSPasteboard clearContents] returns NSInteger;
    // -setString:forType: takes two objects and returns BOOL.
    unsafe {
        let board = send!(Id; class(c"NSPasteboard"), c"generalPasteboard");
        send!(i64; board, c"clearContents");
        let s = ns_string(&text);
        send!(bool; board, c"setString:forType:", s => Id, NSPasteboardTypeString => Id);
        send!((); s, c"release");
    }
}

// ---- Methods of the custom view and delegates (called by AppKit on the main thread) ----

extern "C" fn returns_yes(_this: Id, _sel: Sel) -> bool {
    true
}

extern "C" fn should_terminate_after_last_window(_this: Id, _sel: Sel, _app: Id) -> bool {
    true
}

/// Where `event` happened, in content points from the top left.
fn location(view: Id, event: Id) -> (f64, f64) {
    // SAFETY: `event` is an NSEvent with a location (mouse or scroll), `view` our content view.
    let (point, rect) = unsafe { (send!(CGPoint; event, c"locationInWindow"), bounds(view)) };
    // Window coordinates have their origin at the bottom left of the content area.
    (point.x, rect.height - point.y)
}

extern "C" fn scroll_wheel(this: Id, _sel: Sel, event: Id) {
    // SAFETY: `event` is the NSEvent AppKit passes to scrollWheel:.
    let (dy, precise) = unsafe {
        (
            send!(f64; event, c"scrollingDeltaY"),
            send!(bool; event, c"hasPreciseScrollingDeltas"),
        )
    };
    let (x, y) = location(this, event);
    // Mouse wheels report lines; trackpads report points.
    emit(Event::Scroll {
        dy: if precise { dy } else { dy * 16.0 },
        x,
        y,
    });
}

extern "C" fn mouse_down(this: Id, _sel: Sel, event: Id) {
    const SHIFT: u64 = 1 << 17;
    let (x, y) = location(this, event);
    // SAFETY: `event` is the NSEvent passed to mouseDown:; -modifierFlags returns NSUInteger.
    let flags = unsafe { send!(u64; event, c"modifierFlags") };
    if flags & SHIFT != 0 {
        emit(Event::ShiftClick { x, y });
    } else {
        emit(Event::Click { x, y });
    }
}

extern "C" fn mouse_dragged(this: Id, _sel: Sel, event: Id) {
    let (x, y) = location(this, event);
    emit(Event::Drag { x, y });
}

extern "C" fn mouse_up(this: Id, _sel: Sel, event: Id) {
    let (x, y) = location(this, event);
    emit(Event::Release { x, y });
}

extern "C" fn mouse_moved(this: Id, _sel: Sel, event: Id) {
    let (x, y) = location(this, event);
    emit(Event::Hover { x, y });
}

extern "C" fn key_down(_this: Id, _sel: Sel, event: Id) {
    const COMMAND: u64 = 1 << 20;
    const SHIFT: u64 = 1 << 17;
    const OPTION: u64 = 1 << 19;
    // SAFETY: `event` is the NSEvent passed to keyDown:.
    let (code, flags) = unsafe {
        (
            send!(u16; event, c"keyCode"),
            send!(u64; event, c"modifierFlags"),
        )
    };
    let command = flags & COMMAND != 0;
    let shift = flags & SHIFT != 0;
    let option = flags & OPTION != 0;
    let key = match code {
        36 | 76 => Some(Key::Enter),
        53 => Some(Key::Escape),
        51 => Some(Key::Backspace),
        48 => Some(Key::Tab),
        123 => Some(Key::Left),
        124 => Some(Key::Right),
        125 => Some(Key::Down),
        126 => Some(Key::Up),
        116 => Some(Key::PageUp),
        121 => Some(Key::PageDown),
        115 => Some(Key::Home),
        119 => Some(Key::End),
        _ => None,
    };
    if let Some(key) = key {
        emit(Event::Key(KeyEvent {
            key,
            command,
            shift,
            option,
        }));
        return;
    }
    // SAFETY: as above; both selectors return an NSString (possibly empty).
    let text = unsafe {
        if command {
            rust_string(send!(Id; event, c"charactersIgnoringModifiers")).to_lowercase()
        } else {
            rust_string(send!(Id; event, c"characters"))
        }
    };
    if command && text == "v" {
        // SAFETY: main thread; the pasteboard returns an autoreleased NSString or nil.
        let pasted = unsafe {
            let board = send!(Id; class(c"NSPasteboard"), c"generalPasteboard");
            rust_string(send!(Id; board, c"stringForType:", NSPasteboardTypeString => Id))
        };
        emit(Event::Paste(pasted));
        return;
    }
    for ch in text.chars().filter(|c| !c.is_control()) {
        emit(Event::Key(KeyEvent {
            key: Key::Char(ch),
            command,
            shift,
            option,
        }));
    }
}

extern "C" fn window_changed(_this: Id, _sel: Sel, _notification: Id) {
    let (window, view) = (WINDOW.load(Ordering::Acquire), VIEW.load(Ordering::Acquire));
    if window.is_null() || view.is_null() {
        return;
    }
    // SAFETY: main thread; window and view are live.
    let (rect, scale) = unsafe { (bounds(view), send!(f64; window, c"backingScaleFactor")) };
    emit(Event::Resize {
        width: rect.width,
        height: rect.height,
        scale,
    });
}

unsafe fn make_class(
    superclass: &CStr,
    name: &CStr,
    methods: &[(&CStr, *const c_void, &CStr)],
) -> Id {
    let cls = objc_allocateClassPair(class(superclass), name.as_ptr(), 0);
    if cls.is_null() {
        // Already registered (run_app called twice in one process).
        return class(name);
    }
    for &(selector, imp, types) in methods {
        class_addMethod(cls, sel(selector), imp, types.as_ptr());
    }
    objc_registerClassPair(cls);
    cls
}

/// Create the application and one window, start `work` on the engine thread with the input
/// channel, and run the AppKit event loop on this (the main) thread until the user quits.
pub fn run_app(
    title: &str,
    width: f64,
    height: f64,
    work: impl FnOnce(Window, Receiver<Event>) + Send + 'static,
) -> ! {
    const REGULAR_APP: i64 = 0;
    const TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE: u64 = 1 | 2 | 4 | 8;
    const BACKING_BUFFERED: u64 = 2;

    let (tx, rx) = channel();
    let _ = EVENTS.set(tx);

    // SAFETY: this runs on the main thread before the run loop starts, and every send! below
    // matches the documented signature of its method.
    unsafe {
        let pool = send!(Id; send!(Id; class(c"NSAutoreleasePool"), c"alloc"), c"init");
        let app = send!(Id; class(c"NSApplication"), c"sharedApplication");
        send!(bool; app, c"setActivationPolicy:", REGULAR_APP => i64);

        let app_delegate_class = make_class(
            c"NSObject",
            c"BroxeeAppDelegate",
            &[(
                c"applicationShouldTerminateAfterLastWindowClosed:",
                should_terminate_after_last_window as *const c_void,
                c"B@:@",
            )],
        );
        let app_delegate = send!(Id; send!(Id; app_delegate_class, c"alloc"), c"init");
        send!((); app, c"setDelegate:", app_delegate => Id);

        // Menu bar with Quit (Cmd+Q).
        let menu_bar = send!(Id; send!(Id; class(c"NSMenu"), c"alloc"), c"init");
        let app_item = send!(Id; send!(Id; class(c"NSMenuItem"), c"alloc"), c"init");
        send!((); menu_bar, c"addItem:", app_item => Id);
        send!((); app, c"setMainMenu:", menu_bar => Id);
        let app_menu = send!(Id; send!(Id; class(c"NSMenu"), c"alloc"), c"init");
        let quit = send!(Id; send!(Id; class(c"NSMenuItem"), c"alloc"),
            c"initWithTitle:action:keyEquivalent:",
            ns_string("Quit Broxee") => Id, sel(c"terminate:") => Sel, ns_string("q") => Id);
        send!((); app_menu, c"addItem:", quit => Id);
        send!((); app_item, c"setSubmenu:", app_menu => Id);

        let rect = CGRect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        };
        let window = send!(Id; send!(Id; class(c"NSWindow"), c"alloc"),
            c"initWithContentRect:styleMask:backing:defer:",
            rect => CGRect, TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE => u64, BACKING_BUFFERED => u64, false => bool);
        send!((); window, c"setTitle:", ns_string(title) => Id);
        send!((); window, c"setBackgroundColor:", send!(Id; class(c"NSColor"), c"whiteColor") => Id);

        let window_delegate_class = make_class(
            c"NSObject",
            c"BroxeeWindowDelegate",
            &[
                (
                    c"windowDidResize:",
                    window_changed as *const c_void,
                    c"v@:@",
                ),
                (
                    c"windowDidChangeBackingProperties:",
                    window_changed as *const c_void,
                    c"v@:@",
                ),
            ],
        );
        let window_delegate = send!(Id; send!(Id; window_delegate_class, c"alloc"), c"init");
        send!((); window, c"setDelegate:", window_delegate => Id);

        // Our own content view, so scroll, click and key events reach Broxee.
        let view_class = make_class(
            c"NSView",
            c"BroxeeView",
            &[
                (
                    c"acceptsFirstResponder",
                    returns_yes as *const c_void,
                    c"B@:",
                ),
                (c"scrollWheel:", scroll_wheel as *const c_void, c"v@:@"),
                (c"mouseDown:", mouse_down as *const c_void, c"v@:@"),
                (c"mouseDragged:", mouse_dragged as *const c_void, c"v@:@"),
                (c"mouseUp:", mouse_up as *const c_void, c"v@:@"),
                (c"mouseMoved:", mouse_moved as *const c_void, c"v@:@"),
                (c"keyDown:", key_down as *const c_void, c"v@:@"),
            ],
        );
        let view = send!(Id; send!(Id; view_class, c"alloc"), c"initWithFrame:", rect => CGRect);
        send!((); window, c"setContentView:", view => Id);

        // Fit the window on screen now. macOS would shrink it later anyway, and the resize
        // would rebuild every frame surface and lay the page out again right after loading.
        let screen = send!(Id; class(c"NSScreen"), c"mainScreen");
        if !screen.is_null() {
            let visible = rect_of(screen, c"visibleFrame");
            let frame = rect_of(window, c"frame");
            let content = bounds(view);
            let fitted = CGSize {
                width: content
                    .width
                    .min(visible.width - (frame.width - content.width)),
                height: content
                    .height
                    .min(visible.height - (frame.height - content.height)),
            };
            if fitted.width < content.width || fitted.height < content.height {
                send!((); window, c"setContentSize:", fitted => CGSize);
            }
        }
        send!((); window, c"center");
        send!((); view, c"setWantsLayer:", true => bool);
        let layer = send!(Id; view, c"layer");
        let scale = send!(f64; window, c"backingScaleFactor");
        send!((); layer, c"setContentsScale:", scale => f64);
        send!((); layer, c"setContentsGravity:", kCAGravityTopLeft => Id);
        // The header's own layer, hidden until the header changes on its own.
        let header = send!(Id; send!(Id; class(c"CALayer"), c"alloc"), c"init");
        send!((); header, c"setContentsScale:", scale => f64);
        send!((); header, c"setContentsGravity:", kCAGravityTopLeft => Id);
        send!((); header, c"setHidden:", true => bool);
        send!((); layer, c"addSublayer:", header => Id);

        let appearance = send!(Id; app, c"effectiveAppearance");
        let dark =
            !appearance.is_null() && rust_string(send!(Id; appearance, c"name")).contains("Dark");

        WINDOW.store(window, Ordering::Release);
        VIEW.store(view, Ordering::Release);
        LAYER.store(layer, Ordering::Release);
        HEADER_LAYER.store(header, Ordering::Release);

        send!((); window, c"makeKeyAndOrderFront:", std::ptr::null_mut::<c_void>() => Id);
        send!(bool; window, c"makeFirstResponder:", view => Id);
        // Pointer moves without a button reach the view (tooltips).
        send!((); window, c"setAcceptsMouseMovedEvents:", true => bool);
        send!((); app, c"activateIgnoringOtherApps:", true => bool);

        // The size the window really got, so the first frames already match it.
        let shown = bounds(view);
        let handle = Window {
            scale,
            width: shown.width,
            height: shown.height,
            dark,
            pool: RefCell::new(Pool::default()),
            header_pool: RefCell::new(Pool::default()),
        };
        std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || work(handle, rx))
            .expect("spawn the engine thread");

        send!((); app, c"run");
        send!((); pool, c"drain");
    }
    std::process::exit(0)
}
