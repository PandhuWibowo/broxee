//! Measure stylesheet parsing: time, allocations and peak heap, against a page's vocabulary.
//!
//! ```text
//! cargo run --release -p css --example parse_bench -- page.html sheet1.css sheet2.css ...
//! ```
//!
//! Identical sheets are parsed once. Each run parses everything five times and reports the
//! fastest, so numbers are comparable before and after a change.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Counts allocations and tracks the heap's high-water mark.
struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: same layout the caller gave us.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((page, sheets)) = args.split_first() else {
        eprintln!("usage: parse_bench page.html sheet.css...");
        std::process::exit(2);
    };
    let html = std::fs::read_to_string(page).expect("read the page");
    let dom = html::parse(&html);
    let vocabulary = css::Vocabulary::of(&dom);
    let mut texts: Vec<String> = Vec::new();
    for path in sheets {
        let text = std::fs::read_to_string(path).expect("read a stylesheet");
        if !texts.contains(&text) {
            texts.push(text);
        }
    }
    let bytes: usize = texts.iter().map(String::len).sum();

    // The rules kept must be exactly those the full selector check keeps: parse everything,
    // drop rules no selector of which could match, and compare.
    if std::env::var_os("CHECK").is_some() {
        for text in &texts {
            let fast = css::parse_stylesheet_for(text.clone(), &vocabulary);
            let all = css::parse_stylesheet(text);
            let slow: Vec<String> = all
                .rules
                .iter()
                .filter_map(|rule| {
                    let kept: Vec<_> = rule
                        .selectors
                        .iter()
                        .filter(|s| vocabulary.could_match(s))
                        .collect();
                    (!kept.is_empty())
                        .then(|| format!("{kept:?} {:?} {:?}", rule.decls, rule.condition))
                })
                .collect();
            let fast: Vec<String> = fast
                .rules
                .iter()
                .map(|r| {
                    format!(
                        "{:?} {:?} {:?}",
                        r.selectors.iter().collect::<Vec<_>>(),
                        r.decls,
                        r.condition
                    )
                })
                .collect();
            assert_eq!(fast, slow, "a sheet of {} bytes differs", text.len());
        }
        println!("check: every sheet keeps exactly the rules the full check keeps");
    }

    let mut best = f64::MAX;
    let (mut allocations, mut peak, mut kept, mut skipped) = (0, 0, 0, 0);
    for _ in 0..5 {
        let inputs = texts.clone();
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        let parsed: Vec<css::Stylesheet> = inputs
            .into_iter()
            .map(|text| css::parse_stylesheet_for(text, &vocabulary))
            .collect();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        peak = PEAK.load(Ordering::Relaxed) - base;
        kept = parsed.iter().map(|s| s.rules.len()).sum::<usize>();
        skipped = parsed.iter().map(|s| s.skipped).sum::<usize>();
        best = best.min(ms);
        drop(parsed);
    }
    println!(
        "{} sheets, {:.1} MB: {best:.1} ms, {allocations} allocations, {:.1} MB peak heap while \
         parsing (beyond the input text), {kept} rules kept, {skipped} skipped",
        texts.len(),
        bytes as f64 / 1e6,
        peak as f64 / 1e6
    );
}
