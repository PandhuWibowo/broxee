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
  flate/   DEFLATE, zlib, gzip and Brotli decoding (brotli.rs; RFC 7932 dictionary and tables as
           data, CRC-checked in tests), CRC-32, Adler-32
  net/     URL parse + join, HTTP/1.1 over TCP, TLS (rustls), keep-alive pool, chunked bodies, gzip,
           redirects, file://, cookies.rs (per-profile jars), psl.rs (Public Suffix List, as data)
  html/    tokenizer.rs (byte-oriented, raw-text elements), entities.rs, lib.rs (tree builder + arena DOM,
           fast hasher shared with css, layout and paint)
  css/     parser.rs (rules, declarations, shorthands, @media), media.rs (query evaluation), grid.rs,
           hints.rs (presentational attributes), vocabulary.rs (rules that cannot match), quirks.css,
           selector.rs, values.rs (specified + computed values), cascade.rs, ua.css,
           fontface.rs (`font-family` lists, `@font-face` rules, `unicode-range`)
  font/    TrueType/OpenType metrics (advances, ascent/descent, cmap 4/12, .ttc), TextMetrics trait,
           Approximate fallback used by tests; woff.rs (WOFF, WOFF2 with the glyf/loca/hmtx
           transforms), faces.rs (FontKey, matching by family/weight/style, per-character fallback),
           figtree/ (the interface font, OFL, as data)
  layout/  block + inline layout into BoxFragment / TextFragment lists, one shared text buffer;
           flex.rs (flex containers, intrinsic widths), grid.rs (grid containers), table.rs (tables),
           incremental.rs (laying out one box again after DOM changes)
  image/   PNG, JPEG, GIF, WebP (vp8.rs lossy, vp8l.rs lossless, vp8_tables.rs from RFC 6386) decoders
           that scale while decoding; svg.rs (XML subset, flattened paths, scanline rasterizer)
  js/      JavaScript, written here: lexer.rs, parser.rs (syntax tree), interp.rs (arena heap, scopes,
           evaluation, limits), builtins.rs (globals, DOM bindings, events), regex.rs; the DOM through `Host`
  paint/   display list, glyph rasterizer + cache, image cache + parallel decode, band compositing,
           streaming PNG writer; shape.rs (rounded rectangles, gradients, shadows, evaluated per pixel)
  platform/ macOS window via the Objective-C runtime, IOSurface frames (empty module on other OSes)
  shell/   the `broxee` binary: main.rs (CLI), engine.rs (page pipeline, FontStore, snapshot/restore),
           browser.rs (profiles, tabs, history, navigation, input, session file, Screen trait),
           forms.rs (form controls: injected parts, state, submission),
           script.rs (which scripts run, `js::Host` over the DOM, dispatch and timers),
           hibernate.rs (sleeping tabs on disk), palette.rs (⌘K results and ranking),
           chrome.rs (sidebar, header and palette drawing from DESIGN.md),
           pool.rs (fetch pool), webfonts.rs (`@font-face`: which faces the text uses, fetching)
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
- **A group draws its content over its backdrop, then blends back** (`draw_group`): exactly group opacity
  over an opaque backdrop, and the rounded clip. Backdrops are copied 32 rows at a time within the group's
  bounds; groups that need none are dropped while building (`fade`, rounded clips whose content stays off the
  corners). `emit_range` never reopens a group it is inside: a group cut by a layer recursed forever.
- **Transforms are resolved before compositing** (`resolve_transforms`, after the glyph and image caches are
  written): a move shifts the items (whole pixels, so glyph phases hold), anything else renders offscreen,
  over black and over white for alpha, within `MAX_LAYER_PIXELS`. Inside a transformed element only the
  clips it owns move with it (`ClipTracker::floor`); its ancestors' clips apply where it is drawn. Groups
  on the same range: a rounded clip (the parent's) is outer to a child's transform or opacity.
- **Every band walks every item, so `Item` stays small**: rounded shapes, gradients and shadows are one
  boxed `Item::Effect`. Plain boxes keep the `Fill` path; only boxes with radii, shadows or gradients
  pay for per-pixel work, and only at their corners and edges (`Edges`: interiors are filled or
  skipped, straight edges get one coverage per row). Measure GitHub at 2x after changing it.
- **Images are never decoded at full size.** Decoders stream rows into `image::Scaler`, which outputs the
  drawn size; only visible images are decoded, and the pixel cache stays under `IMAGE_CACHE_BYTES`. A new
  decoder must keep that: memory proportional to the output, not the file. Progressive JPEG keeps
  only the k x k coefficients the drawn size needs plus a non-zero mask per block, one decode at a time.
- **Every `send!` states the method's exact C signature.** A wrong type is undefined behaviour, not an error.
- **AppKit calls happen on the main thread only.** The engine thread hands work over with `dispatch_async_f`.
- **Paint into the window's `IOSurface`, never into a separate buffer that is then copied.** At most two
  surfaces per window; the spare one is released when idle (`Window::release_spare`).
- **Window memory is measured with `footprint <pid>` on a real window** (idle and `phys_footprint_peak`), not
  only with the CLI: frame surfaces (16 MB each at Retina) and AppKit are on top of the page. Measure a
  sequence of heavy pages too, not only one page: freed memory left behind by earlier pages is what pushed the
  peak past 60 MB in #1.
- **`main` re-executes itself with `MallocSpaceEfficient=1` on macOS** (`space_efficient_malloc`). Without it
  macOS malloc keeps 10+ MB of freed memory dirty after a heavy page. Nothing may run before that call
  (threads, fonts, sockets): the exec replaces the process. If window memory jumps after a macOS update,
  check this first.

### Tab invariants

- **Only the visible tab is a live page** (`Browser::page`). Every other tab is a file in `TabStore` or just
  its history. Never keep a second `Loaded` around: a heavy page is 10 to 20 MB live.
- **CSS background images are kept by URL** in the page's `ImageStore` (beside `<img>` files by `NodeId`,
  within the same budget), keyed by the URL as the computed style holds it: stylesheets' URLs are made absolute
  after parsing (`Stylesheet::resolve_urls`), `style` attributes' stay relative and are fetched against the page.
  They are found after the cascade, for displayed elements only, and stored with a sleeping tab.
- **A snapshot holds only what the page was built from**, as it arrived (`net::Wire`): rebuilding must give the
  same DOM, so `NodeId`s in the stored `ImageStore` stay valid. Do not store anything derived (styles, layout).
- **Image ids in the painter are per page.** `Painter::forget_images` runs whenever the page changes (show,
  sleep); skipping it shows another page's pixels.
- **Switching profile calls `net::forget_sessions`.** Nothing that could link profiles (connections, TLS
  tickets, and cookies once they exist) may be shared between them.
- **Split view keeps one live page.** Only the focused pane has a `Loaded`; the others are pixels copied from
  the previous frame surface (`Screen::render` hands it over), so they cost no memory. Giving a pane focus
  sleeps the focused tab and wakes the pane's tab at the pane's size. Events over the focused pane get
  pane-relative y (`Browser::route`); anything that draws in the pane uses its rows of the frame.
- **One page load at a time.** `Browser::navigate` stops the running load before starting another, and every
  tab or profile change stops it too. A finished load always belongs to the visible tab, because nothing can
  change the visible tab without stopping it first. Keep both true when adding background work.
- **Cookies belong to a profile.** Every fetch for a page passes that profile's jar (`engine::Jar`);
  subresources use `net::Context::subresource`, which blocks other sites' cookies. Never log or print cookie
  values, never write session cookies to disk, and keep cookie and session files 0600 (`write_private`).
- **Form controls draw with nodes the browser adds** (`forms::Forms::prepare`, before anything reads the
  DOM): they go after the page's own nodes so no id changes, and rebuilding from the sources gives the same
  DOM. Typed values live in `Loaded::forms` only; passwords are never logged or written to disk.
- **Session and tab files are data, never trusted**: every length and count is checked against what is left,
  and damage gives a reload, not a panic.

### Testing the browser

- **Never send synthetic input to the real desktop** (osascript keystrokes, clicks). The owner works on this
  machine; keystrokes can land in another app. Drive `Browser` through `FakeScreen` in `browser.rs` tests with
  local files, and render PNGs from it when a visual check is needed.

### Change invariants

- **Every DOM mutation records a `html::Change`** (text, an element's attributes or tag, a node's children),
  read once per task by `engine::update`. A new mutating `Dom` method must record one; boxes the cascade
  generates (`show_generated`) do not. Past `MAX_CHANGES` the log gives up and the page is rebuilt whole.
- **`engine::update` handles a whole task's changes at once** (a handler, a timer run, a keystroke): one
  cascade at most, compared with the old styles by value (`StyleMap::changed_since`), then one relayout.

### Script invariants

- **JavaScript is written here** (`crates/js`), not taken from an engine crate: the owner's rule, and it keeps
  memory in our hands. Decided for #12; recorded in README.
- **A page's heap is an arena dropped with the page.** No collector: objects have a ceiling (`OBJECT_LIMIT`),
  scopes of calls that made no closure are popped, every entry (script, handler, timer) has a step limit.
  Past a limit the page's scripts stop for good; the page keeps working without them.
- **Script work runs on a large-stack thread** (`script::on_script_stack`, `js::STACK_SIZE`): the interpreter
  recurses. The runtime is `Send` (`Arc`, not `Rc`) so a `Loaded` can still cross from the loader thread.
- **Scripts run before styles**, after form controls are prepared; later DOM changes (events, timers) restyle
  and lay out the page again. Image ids come from parser nodes only, so a waking tab finds them again even if
  its scripts behave differently the second time.
- **Accessors and classes live in the same arena.** A getter/setter pair is a `Kind::Accessor` object in the
  property slot; `get` calls the getter, and `put` looks up the prototype chain for a setter only once the page
  has made an accessor (`Interp::accessors`). A class is its constructor function plus a `ClassRec` (fields,
  parent); `super` finds its home through the hidden `%super`/`%class` bindings of the class scopes.
- **`async` and generator bodies run on a statement machine** (`interp/asyncs.rs`); `yield` is a pause
  point like `await`, and a generator object holds its paused call in `Interp::coros` (`Slot::Running`
  while it runs, so a call it makes never takes its place). `for…of` pulls one value at a time
  (`Interp::for_source`): never collect an iterator whole there, a generator may not end.
  The machine itself (`async` too): frames for blocks, loops, `try` and
  `switch`, so a call can pause between statements and resume from the microtask queue. Before a statement
  runs, the `await`s it always evaluates are settled one by one into `Interp::slots` (keyed by the node's
  address; the AST is shared through `Arc`, so addresses stay put); statements without `await` run whole on
  the recursive evaluator. A paused call's scopes are pinned. The microtask queue drains at the end of every
  task (`Runtime::checkpoint`), bounded by `MICROTASK_LIMIT`.
- **Scripts see no cookies, cannot fetch or navigate, and store nothing on disk.** Widen this only on purpose.

### Compression invariants

- **Measure before adding a format or a cache.** README's "Compression" table records each decision with its
  numbers (why Brotli and not zstd, why decoded pixels are not compressed, why `opt-level = 3`).
- **The bit reader refills 8 bytes at a time** and keeps bits read ahead. Anything that moves `Bits::pos`
  directly (raw/stored bytes) must call `discard_ahead` first.
- **`unsafe` in `flate` is only the ARMv8 CRC32 intrinsics**, behind a runtime feature check, with the table
  version as fallback and a test that the two agree.

### Font invariants

- **Text is measured and drawn with the same `FontKey`** (`layout::font_key`): family list, weight, style.
  Layout and paint both go through `TextMetrics`, so a glyph is drawn by the face that measured it.
- **Only faces some styled text uses are fetched** (`webfonts::needed`, after the cascade, `::before`/`::after`
  text included). Font files are page sources (`Sources::fonts`): a sleeping tab wakes with them, no network.
  `data:` fonts are not stored twice; their stylesheet holds them.
- **Face ids from `font::WEB_FACES` are per page.** `Painter::forget_images` drops their glyphs; the process's
  own faces (below it) stay cached.
- **The process's extra faces load lazily** (`SystemFace::Lazy`): Figtree when the interface first draws
  text, the symbol font when a character needs it. Decoding them up front cost 0.5 MB of peak RAM on every page.
- **`Fonts::head` must inline**: it runs per word and per line. As a call it cost 9% of the stress page's layout.

### Layout invariants

- **Layout tests use `font::Approximate`** (every glyph 0.5em, space 0.25em) so results never depend on the
  fonts installed on a machine.
- **Fragments stay small**: `TextFragment` is 24 bytes and keeps no vertical extent; derive it from the
  baseline and the node's style. The stress page has 160k of them.
- **`Engine::bottom` must see every box and line** so the scrollable height includes overflow.
- **Positioned boxes cost nothing per fragment.** Paint order is `Layout::layers` (index ranges, sorted by
  `Layer::z` for painting; a relative box with a `z-index` splits the flow into its own layer), clipping is
  `Layout::clips` and opacity / rounded clips are `Layout::groups` (nested ranges like clips); a fragment's
  position in the lists decides all three. Hit testing walks `texts_top_down` / `boxes_top_down`, never
  index order. Anything that reorders or
  inserts fragments after layout must keep those ranges valid. Fixed layers never add to the scroll height.
- **`box_extents` / `text_extents` are computed at the end of `layout()`** (`Layout::index`). Anything that moves
  fragments after that must recompute them, or painting skips what is on screen.
- **Every box is laid out once.** Flex items and inline blocks are sized from intrinsic widths
  (`intrinsic_width`, memoized, no layout), laid out at their final size, then moved with `shift_range` or
  grown with `stretch`. Do not add a measure-by-layout pass: nested containers would multiply it.
- **Positions that must survive later moves are stored as box indices or offsets from a box**
  (`Engine::containing`, `Deferred::anchor`), never as absolute coordinates.
- **Floats live in their block formatting context** (`Engine::floats`, page coordinates). A box that starts a
  context (`starts_context`) sets the outer list aside while its content is laid out and drops its own after,
  so moving the box later moves its floats with it. A float is laid out once, in place (`place_float`); the
  check costs nothing on pages without floats, keep it that way.
- **Inline layout reuses `Engine::items`/`words`.** Anything that lays out a box while a line is being collected
  (`collect_inline_block`) or measures one (`measure_inline`) must save and restore them.
- **Incremental layout rebuilds one anchor** (`Layout::relayout`, `incremental.rs`): a box that clips its
  overflow, is static and not floated, built where it is (`Place::Flow`) or moved once onto its line
  (`Place::Line`), with only block containers and inline boxes in normal flow above it. It is rebuilt from the
  arguments recorded when it was built and kept only if its size and baseline come out the same. Anything that
  moves or resizes a box after building it must go through `shift_range` or call `Engine::moved`, which drops
  the anchors in that range. The randomized test in `incremental.rs` checks the result equals a full pass,
  fragment for fragment; run it after touching layout.
- **Clipped content does not reach the scrollable height**: a clipping box restores `Engine::bottom` after its
  content (except the root element and `<body>`, whose overflow belongs to the viewport). Incremental layout
  depends on it: nothing inside an anchor can change the page's height.

### CSS invariants

- **Stylesheets are parsed against the document's vocabulary** (`css::Vocabulary`): rules needing a tag, id,
  class, attribute or exact attribute value (`[data-theme=dark]`) the DOM lacks are dropped. Correct only while
  the DOM cannot gain names: a page with scripts parses its sheets with `Vocabulary::everything()`, since its
  scripts can add names at any time.
- **`Vocabulary::text_could_match` may only reject what `could_match` rejects.** It skips anything inside
  parentheses, brackets or strings and any escaped name. After changing either, run
  `CHECK=1 cargo run --release -p css --example parse_bench -- page.html sheets...` on real sites.
- **Presentational hints** (`hints.rs`) apply between UA and author rules; elements with hints skip the
  matched-rule cache because the result depends on attributes.
- **Unsupported selectors are dropped, never approximated.** A selector Broxee cannot evaluate must not match;
  a wrong match is worse than a missing one.
- **Style sharing is off when any rule looks at siblings or position** (`RuleIndex::sibling_dependent`).
  A new selector of that kind must set it through `Selector::depends_on_siblings`.
- **Custom property sets borrow from the stylesheets** and are resolved lazily on the declaring element.
  Inline `style` declarations are freed per element: never cache by their address.
- **The ancestor bloom filter must stay in step with the traversal**: every visited element is pushed, and
  `pop_to(parent)` runs before matching. Breaking that gives false "no match" results.
- **Generated boxes (`::before`/`::after`) are nodes outside the tree** (`Dom::show_generated`): their parent is
  the element, but they are not among its children, so selectors, scripts and `childNodes` never see them.
  Layout adds them around the children (`layout::kids`, plain DOM walks when an element has none). Script events
  that land on one go to its element. `content` rules alone are bucketed; rules that only style a generated
  box are matched once a box exists (`RuleIndex::pseudo_styling`).
- **A state change restyles only when a rule can see it** (`StyleMap::state_change_matters`). Pages without
  state rules never hit-test the pointer for styles. Elements in a state never share a sibling's style.
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
