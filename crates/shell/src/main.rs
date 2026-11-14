mod backgrounds;
mod engine;
mod forms;
#[cfg(any(target_os = "macos", test))]
mod hibernate;
mod images;
mod pool;
mod script;
mod scriptnet;
// Used by the window (browser.rs), which only macOS builds; its tests run everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod selection;
mod webfonts;

#[cfg(any(target_os = "macos", test))]
mod browser;
#[cfg(any(target_os = "macos", test))]
mod chrome;
#[cfg(any(target_os = "macos", test))]
mod palette;

use std::process::ExitCode;

use css::{ComputedStyle, Display, StyleMap};
use html::{Dom, NodeId, NodeKind};
use layout::Layout;
use paint::{Painter, Viewport};

use engine::{FontStore, Settings};

const USAGE: &str =
    "usage: broxee [<url-or-file>...] [--window | --png FILE | --paint] [--tree | --styles | --layout] \
                     [--depth N] [--width PX] [--height PX] [--scale N] [--no-css-fetch] [--panes 1|2|4]";

#[derive(Clone)]
struct Options {
    target: String,
    /// More addresses for the window, each opened in its own tab after `target`.
    more: Vec<String>,
    tree: bool,
    styles: bool,
    layout: bool,
    png: Option<String>,
    paint: bool,
    window: bool,
    depth: usize,
    width: f32,
    height: f32,
    scale: f32,
    fetch_css: bool,
    /// Panes the window opens with (1, 2 or 4), showing the addresses given.
    panes: usize,
}

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    space_efficient_malloc();
    let mut targets: Vec<String> = Vec::new();
    let mut opts = Options {
        target: String::new(),
        more: Vec::new(),
        tree: false,
        styles: false,
        layout: false,
        png: None,
        paint: false,
        window: false,
        depth: 8,
        width: 1280.0,
        height: 800.0,
        scale: 1.0,
        fetch_css: true,
        panes: 1,
    };

    let mut args = std::env::args().skip(1);
    let number = |args: &mut std::iter::Skip<std::env::Args>, min: f32, max: f32, default: f32| {
        args.next()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| (min..=max).contains(v))
            .unwrap_or(default)
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tree" => opts.tree = true,
            "--styles" => opts.styles = true,
            "--layout" => opts.layout = true,
            "--paint" => opts.paint = true,
            "--window" => opts.window = true,
            "--no-css-fetch" => opts.fetch_css = false,
            "--png" => opts.png = args.next(),
            "--width" => opts.width = number(&mut args, 100.0, 8000.0, opts.width),
            "--height" => opts.height = number(&mut args, 100.0, 8000.0, opts.height),
            "--scale" => opts.scale = number(&mut args, 0.5, 4.0, opts.scale),
            "--depth" => opts.depth = number(&mut args, 0.0, 1000.0, opts.depth as f32) as usize,
            "--panes" => {
                opts.panes = match number(&mut args, 1.0, 4.0, 1.0) as usize {
                    n @ (1 | 2 | 4) => n,
                    _ => 1,
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            // Finder adds a process serial number (`-psn_0_12345`) when it launches an app on
            // older macOS releases. It is not an address.
            _ if arg.starts_with("-psn_") => {}
            _ if !arg.starts_with("--") => targets.push(arg),
            _ => {
                eprintln!("unexpected argument: {arg}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    // Opened from Finder (double-clicked, or Broxee.app from the .dmg) there are no arguments:
    // open the window instead of printing the usage and exiting at once. Anything that asks
    // for CLI output keeps the CLI.
    let cli_output = opts.png.is_some() || opts.paint || opts.tree || opts.styles || opts.layout;
    if cfg!(target_os = "macos") && targets.is_empty() && !cli_output {
        opts.window = true;
    }
    if opts.window {
        // Without an address the window brings back the last session; with several, each
        // gets a tab and the last one is shown.
        let mut targets = targets.into_iter();
        opts.target = targets.next().unwrap_or_default();
        opts.more = targets.collect();
        return run_window(opts);
    }
    if targets.len() > 1 {
        eprintln!("unexpected argument: {}\n{USAGE}", targets[1]);
        return ExitCode::from(2);
    }
    let Some(target) = targets.pop() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    opts.target = target;
    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Restart once with `MallocSpaceEfficient=1`. macOS malloc otherwise keeps freed memory dirty
/// in its per-CPU caches: after GitHub then example.com the window idled at 43 MB with 6.8 MB
/// live, and peaked at 59 MB. With it: 29 MB idle, 48 MB peak, same timings (README,
/// "Window memory"). libmalloc reads the variable only at process start, hence the exec. It is
/// undocumented: if a macOS release ignores it, memory goes back up and nothing breaks.
/// A value already set (including by the user, to turn it off with 0) is left alone.
#[cfg(target_os = "macos")]
fn space_efficient_malloc() {
    use std::os::unix::process::CommandExt;
    const VAR: &str = "MallocSpaceEfficient";
    if std::env::var_os(VAR).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // `exec` only returns on failure; then carry on in this process with the default malloc.
    let _ = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(VAR, "1")
        .exec();
}

#[cfg(target_os = "macos")]
fn run_window(opts: Options) -> ExitCode {
    browser::run(opts)
}

#[cfg(not(target_os = "macos"))]
fn run_window(_opts: Options) -> ExitCode {
    eprintln!("--window is only available on macOS for now; use --png to render to a file");
    ExitCode::from(2)
}

fn run(opts: &Options) -> Result<(), String> {
    // Fonts start loading now and overlap the page request (stage 1 of docs/pipeline.md).
    let fonts = FontStore::start();
    let url = engine::to_url(&opts.target)?;
    let settings = Settings {
        width: opts.width,
        height: opts.height,
        dark: false,
        fetch_css: opts.fetch_css,
    };
    let page = engine::load(&url, settings, &fonts, None, &mut |_| true)?;

    if opts.layout {
        let mut out = String::new();
        dump_layout(&page.dom, &page.layout, opts.depth, &mut out);
        println!("{out}");
    } else if opts.styles {
        let mut out = String::new();
        dump_styled(&page.dom, &page.styles, Dom::ROOT, 0, opts.depth, &mut out);
        println!("{out}");
    } else if opts.tree {
        println!("{}", page.dom.dump(opts.depth));
    }

    let painted = (opts.paint || opts.png.is_some()).then(|| {
        let metrics = page.metrics(&fonts);
        let view = engine::page_view(&page, &metrics);
        let viewport = Viewport {
            width: opts.width,
            height: opts.height,
            scale: opts.scale,
            scroll_y: 0.0,
        };
        Painter::new().paint(&view, viewport)
    });
    if let (Some(path), Some((frame, _))) = (&opts.png, &painted) {
        let png = paint::png::encode(frame.width, frame.height, &frame.pixels);
        std::fs::write(path, png).map_err(|e| format!("{path}: {e}"))?;
    }

    let css = &page.css;
    let rules: usize = css.sheets.iter().map(css::Stylesheet::len).sum();
    let skipped: usize = css.sheets.iter().map(|s| s.skipped).sum();
    println!("url       {}", page.url);
    println!("status    {}", page.status);
    println!(
        "title     {}",
        page.dom.title().unwrap_or_else(|| "(none)".into())
    );
    println!("size      {:.1} KB", page.bytes as f64 / 1024.0);
    println!(
        "nodes     {} ({} elements)",
        page.dom.len(),
        page.dom.element_count()
    );
    println!("fetch     {:.1} ms", page.fetch_ms);
    println!("parse     {:.2} ms", page.parse_ms);
    println!(
        "css       {} sheets ({} fetched, {} failed), {rules} rules ({skipped} skipped: no match possible), {:.1} ms wall ({:.1} ms parsing across threads)",
        css.sheets.len(),
        css.fetched,
        css.failed,
        css.wall_ms,
        css.parse_ms
    );
    println!(
        "images    {} loaded, {:.0} KB compressed",
        page.images.len(),
        page.images.bytes() as f64 / 1024.0
    );
    println!(
        "cascade   {:.2} ms, {} unique styles",
        page.cascade_ms,
        page.styles.unique_styles()
    );
    println!(
        "fonts     {}, waited {:.2} ms after css",
        fonts.name(),
        page.fonts_wait_ms
    );
    let web = &page.webfonts;
    if web.declared > 0 {
        println!(
            "webfonts  {} of {} faces loaded ({} failed), {:.0} KB in memory, {:.1} ms",
            web.faces.len(),
            web.declared,
            web.failed,
            web.bytes as f64 / 1024.0,
            web.ms
        );
    }
    println!(
        "layout    {:.2} ms, {} boxes, {} text runs, {:.0}x{:.0} px",
        page.layout_ms,
        page.layout.boxes.len(),
        page.layout.texts.len(),
        page.layout.width,
        page.layout.height
    );
    if let Some((frame, stats)) = &painted {
        println!(
            "paint     {}x{} px at {}x, {} items, {} glyphs ({:.0} KB), list {:.2} ms, glyphs {:.2} ms, composite {:.2} ms on {} bands",
            frame.width,
            frame.height,
            opts.scale,
            stats.items,
            stats.glyphs_rasterized,
            stats.glyph_cache_bytes as f64 / 1024.0,
            stats.display_list_ms,
            stats.glyph_ms,
            stats.composite_ms,
            stats.bands
        );
    }
    if let Some((_, stats)) = &painted {
        println!(
            "images    {} decoded in {:.2} ms, {:.0} KB of pixels",
            stats.images_decoded,
            stats.image_ms,
            stats.image_cache_bytes as f64 / 1024.0
        );
    }
    if let Some(path) = &opts.png {
        println!("png       {path}");
    }
    println!("peak RAM  {:.1} MB", peak_rss_mb());
    Ok(())
}

/// Every element's box and every line of text, indented by tree depth, in document order.
fn dump_layout(dom: &Dom, laid_out: &Layout, max_depth: usize, out: &mut String) {
    let mut box_of: Vec<Option<usize>> = vec![None; dom.len()];
    for (i, b) in laid_out.boxes.iter().enumerate() {
        box_of[b.node as usize].get_or_insert(i);
    }
    let mut texts_of: Vec<Vec<usize>> = vec![Vec::new(); dom.len()];
    for (i, t) in laid_out.texts.iter().enumerate() {
        texts_of[t.node as usize].push(i);
    }
    let mut depth_of = vec![0usize; dom.len()];
    for id in dom.descendants(Dom::ROOT).skip(1) {
        let parent = dom.parent(id).unwrap_or(Dom::ROOT);
        depth_of[id as usize] = depth_of[parent as usize] + 1;
        let depth = depth_of[id as usize];
        if depth > max_depth {
            continue;
        }
        let indent = "  ".repeat(depth);
        if let Some(i) = box_of[id as usize] {
            let r = laid_out.boxes[i].rect;
            let tag = dom.tag_name(id).unwrap_or_default();
            out.push_str(&format!(
                "{indent}<{tag}> {:.0},{:.0} {:.0}x{:.0}\n",
                r.x, r.y, r.width, r.height
            ));
        }
        for &i in &texts_of[id as usize] {
            let t = &laid_out.texts[i];
            let text: String = laid_out.text(t).chars().take(50).collect();
            out.push_str(&format!(
                "{indent}\"{text}\" x{:.0} baseline{:.0} w{:.0}\n",
                t.x, t.baseline, t.width
            ));
        }
    }
}

/// The DOM tree annotated with each element's computed display, font and spacing.
fn dump_styled(
    dom: &Dom,
    styles: &StyleMap,
    id: NodeId,
    depth: usize,
    max_depth: usize,
    out: &mut String,
) {
    let line = match dom.kind(id) {
        NodeKind::Document => "#document".to_string(),
        NodeKind::Element(_) => {
            let tag = dom.tag_name(id).unwrap_or_default();
            let s = styles.style(id);
            if s.display == Display::None {
                out.push_str(&format!("{}<{tag}> hidden\n", "  ".repeat(depth)));
                return;
            }
            format!("<{tag}> {}", describe(s))
        }
        NodeKind::Text(t) => {
            let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if t.is_empty() {
                return;
            }
            let short: String = t.chars().take(60).collect();
            let ellipsis = if short.len() < t.len() { "…" } else { "" };
            format!("\"{short}{ellipsis}\"")
        }
        NodeKind::Doctype(_) | NodeKind::Comment(_) => return,
    };
    out.push_str(&"  ".repeat(depth));
    out.push_str(&line);
    out.push('\n');
    if depth >= max_depth {
        return;
    }
    for child in dom.children(id) {
        dump_styled(dom, styles, child, depth + 1, max_depth, out);
    }
}

fn describe(s: &ComputedStyle) -> String {
    let mut parts = vec![
        format!("{:?}", s.display).to_lowercase(),
        format!("{} {}", s.font_size, s.font_weight),
        s.color.to_string(),
    ];
    if s.background_color.a > 0 {
        parts.push(format!("bg {}", s.background_color));
    }
    if s.margin.iter().any(|m| *m != css::Length::ZERO) {
        parts.push(format!("margin {}", sides(&s.margin)));
    }
    if s.padding.iter().any(|p| *p != css::Length::ZERO) {
        parts.push(format!("padding {}", sides(&s.padding)));
    }
    if s.text_align != css::TextAlign::Left {
        parts.push(format!("{:?}", s.text_align).to_lowercase());
    }
    parts.join(" · ")
}

fn sides(v: &[css::Length; 4]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `struct rusage` from `<sys/resource.h>`: two `timeval`s (16 bytes each on 64-bit macOS and Linux)
/// followed by fourteen `long`s, of which `ru_maxrss` is the first.
#[repr(C)]
struct RUsage {
    user_time: [i64; 2],
    system_time: [i64; 2],
    max_rss: i64,
    rest: [i64; 13],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

const RUSAGE_SELF: i32 = 0;

/// Peak resident memory of this process, in MB. Tracked from day one to hold the 60 MB budget.
fn peak_rss_mb() -> f64 {
    let mut usage = RUsage {
        user_time: [0; 2],
        system_time: [0; 2],
        max_rss: 0,
        rest: [0; 13],
    };
    // SAFETY: getrusage writes one `struct rusage` into the pointer, and RUsage matches its layout.
    if unsafe { getrusage(RUSAGE_SELF, &mut usage) } != 0 {
        return 0.0;
    }
    let max = usage.max_rss as f64;
    // macOS reports bytes, Linux reports kilobytes.
    if cfg!(target_os = "macos") {
        max / 1_048_576.0
    } else {
        max / 1024.0
    }
}
