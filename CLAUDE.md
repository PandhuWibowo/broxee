# Broxee: Claude Code context

## What this is

Broxee is a web browser with its own engine, written in Rust (edition 2021). Networking, HTML, CSS, layout and
painting are written in this repo. The only borrowed piece is TLS (`rustls` with the `ring` backend), because
hand-rolled crypto is unsafe.

The product goal is a daily browser that is small, fast and easy to organise: separate work and personal profiles,
organisable tabs, session restore, split view up to four panes, and bookmarks that are quick to find.

Today it opens a macOS window that loads, lays out and paints pages, with scrolling, links, history and an
address field; the same binary is a CLI that prints the DOM, styles or layout with timings and peak RAM.

## The 60 MB budget (non-negotiable)

Everything stays under **60 MB**: the release binary on disk, and peak RAM on the stress page in
`scripts/budget.sh`. `./scripts/budget.sh` fails the build when either reaches the limit, and CI runs it on Linux
and macOS.

- Run `./scripts/budget.sh` after any change that touches parsing, layout, painting or dependencies.
- Add a row to the "Budget log" table in `README.md` when a milestone lands.
- A feature that cannot fit the budget is redesigned, not shipped with the limit raised.

## Module map

```
crates/
  flate/   DEFLATE, zlib and gzip decoding, CRC-32, Adler-32 (HTTP bodies now, PNG images next)
  net/     URL parse + join, HTTP/1.1 over TCP, TLS (rustls), keep-alive pool, chunked bodies, gzip,
           redirects, file://
  html/    tokenizer.rs (byte-oriented, raw-text elements), entities.rs, lib.rs (tree builder + arena DOM,
           fast hasher shared with css, layout and paint)
  css/     parser.rs (rules, declarations, shorthands, @media), media.rs (query evaluation), grid.rs,
           selector.rs, values.rs (specified + computed values), cascade.rs, ua.css
  font/    TrueType/OpenType metrics (advances, ascent/descent, cmap 4/12, .ttc), TextMetrics trait,
           Approximate fallback used by tests
  layout/  block + inline layout into BoxFragment / TextFragment lists, one shared text buffer;
           flex.rs (flex containers, intrinsic widths), grid.rs (grid containers)
  image/   PNG and baseline JPEG decoders that scale while decoding, header probes (GIF, WebP sizes)
  paint/   display list, glyph rasterizer + cache, image cache + parallel decode, band compositing,
           streaming PNG writer
  platform/ macOS window via the Objective-C runtime, IOSurface frames (empty module on other OSes)
  shell/   the `broxee` binary: main.rs (CLI), engine.rs (page pipeline, FontStore), browser.rs (history,
           navigation, input, Screen trait), chrome.rs (header from DESIGN.md), pool.rs (fetch pool)
scripts/
  budget.sh  60 MB check: binary size + peak RAM on a generated 2.6 MB page with a <style> block
```

Concurrency (what is sequential, parallel, main-thread only) is defined in `docs/pipeline.md`. Read it before
moving work between threads. See the roadmap in `README.md`; CSS and layout support and gaps are listed there.

### Written from scratch

The owner wants everything written in this repository. The only exception is TLS (`rustls`), because
hand-written cryptography is unsafe. OS interfaces are declared by hand (`extern "C"` blocks, `objc_msgSend`
casts), not taken from binding crates. Ask before proposing any dependency.

### Paint and platform invariants

- **The glyph and image caches are written only before compositing starts**; band threads read them
  without locks.
- **Images are never decoded at full size.** Decoders stream rows into `image::Scaler`, which outputs the
  drawn size; only visible images are decoded, and the pixel cache stays under `IMAGE_CACHE_BYTES`. A new
  decoder (progressive JPEG, GIF, WebP) must keep that: memory proportional to the output, not the file.
- **Every `send!` states the method's exact C signature.** A wrong type is undefined behaviour, not an error.
- **AppKit calls happen on the main thread only.** The engine thread hands work over with `dispatch_async_f`.
- **Paint into the window's `IOSurface`, never into a separate buffer that is then copied.** At most two
  surfaces per window.

### Testing the browser

- **Never send synthetic input to the real desktop** (osascript keystrokes, clicks). The owner works on this
  machine; keystrokes can land in another app. Drive `Browser` through `FakeScreen` in `browser.rs` tests with
  local files, and render PNGs from it when a visual check is needed.

### Layout invariants

- **Layout tests use `font::Approximate`** (every glyph 0.5em, space 0.25em) so results never depend on the
  fonts installed on a machine.
- **Fragments stay small**: `TextFragment` is 24 bytes and keeps no vertical extent; derive it from the
  baseline and the node's style. The stress page has 160k of them.
- **`Engine::bottom` must see every box and line** so the scrollable height includes overflow.
- **Positioned boxes cost nothing per fragment.** Paint order is `Layout::layers` (index ranges) and clipping is
  `Layout::clips` (nested ranges); a fragment's position in the lists decides both. Anything that reorders or
  inserts fragments after layout must keep those ranges valid. Fixed layers never add to the scroll height.
- **Every box is laid out once.** Flex items and inline blocks are sized from intrinsic widths
  (`intrinsic_width`, memoized, no layout), laid out at their final size, then moved with `shift_range` or
  grown with `stretch`. Do not add a measure-by-layout pass: nested containers would multiply it.
- **Positions that must survive later moves are stored as box indices or offsets from a box**
  (`Engine::containing`, `Deferred::anchor`), never as absolute coordinates.
- **Inline layout reuses `Engine::items`/`words`.** Anything that lays out a box while a line is being collected
  (`collect_inline_block`) or measures one (`measure_inline`) must save and restore them.

### CSS invariants

- **Unsupported selectors are dropped, never approximated.** A selector Broxee cannot evaluate must not match;
  a wrong match is worse than a missing one.
- **Style sharing is off when any rule looks at siblings or position** (`RuleIndex::sibling_dependent`).
  A new selector of that kind must set it through `Selector::depends_on_siblings`.
- **Custom property sets borrow from the stylesheets** and are resolved lazily on the declaring element.
  Inline `style` declarations are freed per element: never cache by their address.
- **The ancestor bloom filter must stay in step with the traversal**: every visited element is pushed, and
  `pop_to(parent)` runs before matching. Breaking that gives false "no match" results.
- **Computed values are integers** (`Px` is 1/64 px) so `ComputedStyle` can be hashed and deduplicated.
  Values that are lists or names (grid templates, placements) live in interned tables in `StyleMap`; the style
  holds their index.

## Key conventions

- **Zero warnings.** `cargo build` and `cargo clippy --all-targets -- -D warnings` must be clean.
- **`cargo fmt`** before every commit; CI runs `cargo fmt --check`.
- **Tests live next to the code** in a bottom `#[cfg(test)] mod tests`.
- **No new dependencies without the owner's approval.** Every crate costs binary size and is counted in the budget.
  Engine work (parsing, layout, painting) is written here, not pulled in.
- **Arena DOM.** Nodes live in `Dom::nodes` (a `ChunkVec`) and refer to each other by `NodeId` (an index). A node
  is 28 bytes and owns no allocation: text, comments and attribute values live in shared buffers, and
  `NodeKind` / `Element` are borrowed views. Do not introduce per-node `Box`, `String` or `Rc<RefCell<…>>`.
- **Slices on ASCII boundaries.** The tokenizer slices `&str` only at ASCII delimiters, which keeps UTF-8 slicing
  valid without per-char decoding. Keep that invariant when extending it.
- **Never panic on bad input.** Real pages are malformed. Truncated tags, stray end tags and unknown entities must
  parse to something sensible (see `survives_truncated_input`).

## Build and test

```bash
make run URL=https://example.com   # fetch, parse, print the tree
make test                          # cargo test
make check                         # fmt + clippy + test + budget, same as CI
```

## Design

UI work (the browser window, tabs, profiles, split view, bookmarks) follows `DESIGN.md`. Treat `DESIGN.md` as design
data, then apply the `antislop` skills as the filter. If code and `DESIGN.md` disagree, fix one of them.
