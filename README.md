# Broxee

Broxee is a web browser with its own engine, written in Rust. Hard budget: **under 60 MB** for the app on disk and for RAM on simple pages.

## Run

```bash
cargo run --release -- https://example.com --window    # open it in a window (macOS)
cargo run --release -- https://example.com --png out.png --scale 2   # render to an image
cargo run --release -- https://example.com --tree      # fetch, parse, print the DOM tree
cargo run --release -- https://example.com --styles    # the tree with computed styles
cargo run --release -- https://example.com --layout    # every box and line with its position
cargo run --release -- page.html --width 400           # local files, any viewport width
cargo test
```

Every run prints fetch, parse, CSS, cascade, layout and paint timings plus peak RAM, so budget regressions show
up immediately. `--no-css-fetch` skips external stylesheets; `--paint` paints without writing a file.

In the window:

| Action | Keys or mouse |
|---|---|
| Scroll | trackpad or wheel, ↑ ↓, Space / Shift+Space, Page Up / Page Down, Home / End |
| Follow a link | click it |
| Back / forward / reload | the header buttons, or ⌘[ / ⌘] / ⌘R |
| Address field | ⌘L or click the host; type or paste (⌘V), Enter to open, Esc to cancel |
| Move between header controls | Tab / Shift+Tab, Enter or Space to activate |
| Quit | ⌘Q or close the window |

## Written from scratch

Everything is written in this repository against the operating system's own interfaces, with one
exception: **TLS** uses `rustls` (and its `ring` crypto backend), because hand-written cryptography puts users
at risk. That is the only dependency; `libc` appears in the build only as a dependency of `ring`.

The window layer calls AppKit through the Objective-C runtime (`objc_msgSend`) and shows frames through
`IOSurface`; process memory is read with a hand-declared `getrusage`.

## Crates

| Crate | Role | Status |
|---|---|---|
| `flate` | DEFLATE, zlib and gzip decoding with checksums and an output cap; CRC-32, Adler-32 | ✅ |
| `net` | URL parsing/joining, HTTP/1.1 over TCP, keep-alive connection pool, chunked and gzip bodies, redirects, `file://`, one retry on a reset or stale connection | ✅ |
| `html` | Tokenizer, entity decoding, tree builder with implied end tags, chunked arena DOM of 28-byte nodes | ✅ |
| `css` | Parser, `@media`, selector matching, cascade, computed styles, custom properties, grid values | ✅ |
| `font` | TrueType/OpenType reader: advances, vertical metrics, cmap formats 4 and 12, `.ttc`, glyph outlines (simple and composite) | ✅ |
| `layout` | Block, inline, inline-block, flex and grid layout, positioning, line breaking, white-space, text-align, baselines | ✅ |
| `paint` | Display list, anti-aliased glyph rasterizer, glyph cache, image cache with parallel decoding, parallel band compositing, streaming PNG writer | ✅ |
| `image` | PNG (all color types and depths, `tRNS`, Adam7) and baseline JPEG (any subsampling, restarts, Adobe RGB), decoded straight to the drawn size; header sizes for PNG, JPEG, GIF, WebP | ✅ |
| `platform` | macOS window, menu, app delegate, main-thread hand-off, zero-copy frames through `IOSurface` | ✅ (macOS) |
| `shell` | The `broxee` binary: CLI, the page pipeline (`engine.rs`), the browser (`browser.rs`: history, navigation, scrolling, links, address field) and its header (`chrome.rs`) | ✅ |

How the stages run (sequential, parallel, which thread) is documented in [`docs/pipeline.md`](docs/pipeline.md).

Text is measured and drawn with Arial (macOS) or DejaVu Sans (Linux) until Figtree is bundled (see
`DESIGN.md`); without either, layout uses a fixed half-em approximation and text is not drawn.

## Layout support

| Supported | Not yet |
|---|---|
| Block boxes: width, height, min/max, `margin: auto`, percentages, padding, borders, `box-sizing` | floats, tables |
| Grid: track lists (lengths, %, `fr`, `auto`, `min-content`, `max-content`, `minmax()`, `fit-content()`, `repeat()` with `auto-fill`/`auto-fit`), `grid-template-areas` and `grid-area`, line numbers and `span`, sparse auto-placement, implicit tracks, gaps, `align-self`, `order` | column auto-flow and `dense`, named lines, `justify-items`/`-self`, `align-content`/`justify-content`, subgrid, intrinsic size of a grid container |
| Flex: rows and columns (and reverses), wrapping rows, grow/shrink/basis with min/max and automatic minimums, auto margins, `justify-content`, `align-items`/`align-self`, `gap`, `order`, text as anonymous items | `align-content`, wrapping columns, baseline alignment (as start), cross-axis auto margins |
| `inline-block` and `inline-flex`: shrink-to-fit, unbreakable on the line, aligned on their last baseline | `vertical-align` |
| `position: relative` (shifts the box and its content), `absolute` (against the nearest positioned ancestor, insets, shrink-to-fit width, static position when no inset), `fixed` (stays put while scrolling, no scroll height) | `z-index` (positioned boxes paint in document order, over normal flow), `sticky` (treated as relative) |
| `overflow: hidden`/`clip` clips descendants to the padding box; `visibility: hidden`; `opacity: 0` | scrolling inside boxes (`auto`/`scroll` show everything), partial opacity (drawn opaque) |
| Sibling margin collapsing | collapsing through parents and empty boxes |
| Anonymous blocks around inline runs; an inline holding a block becomes a block | |
| Line breaking, collapsed white space, `pre`, `nowrap`, `pre-wrap`, `<br>` | `justify`, hyphenation, bidi, `word-break`/`overflow-wrap` |
| Half-leading baselines, `line-height` normal/number/length | `vertical-align` |
| `<img>` as a replaced element: CSS size, `width`/`height` attributes, the file's natural size and ratio, `max-width` keeping proportions, in inline, block, flex and grid | `object-fit`, `<picture>`, `srcset` density selection (first candidate is used), `loading=lazy` |
| Scrollable height includes overflow | padding, borders and margins on inline elements |

TLS uses `rustls` (ring backend). Everything else is written here; crypto is the one thing we do not hand-roll.

## CSS support

| Area | Supported | Not yet |
|---|---|---|
| Selectors | type, `.class`, `#id`, `*`, attributes (`=`, `~=`, `\|=`, `^=`, `$=`, `*=`, `i` flag), `:root`, descendant, `>`, `+`, `~`, `:first-child`/`:last-child`/`:only-child`, `:nth-child()`/`:nth-last-child()`, `:not()` with simple arguments, `:checked`, `:disabled`/`:enabled`, `:empty`, `:link` | `:is()`/`:where()`/`:has()`, complex `:not()` arguments; `:hover`/`:focus`/`:visited` and `::before`/`::after` never match (dropped) |
| At-rules | `@media` (type, min/max width and height, range syntax, `calc(a ± b)`, `prefers-color-scheme`, orientation, reduced motion), `@supports` and `@layer` contents | `@container`, `@import`, `@font-face`, `@keyframes` (skipped) |
| Properties | display (incl. `flex`, `inline-flex`), color, background(-color), font, font-size/weight/style, line-height, text-align, white-space, text-decoration, width, height, min/max, `box-sizing`, margin, padding, border (+ sides, width/style/color), position, inset (`top`/`right`/`bottom`/`left`), visibility, overflow, opacity (0 or visible), flex-direction/-wrap/-flow, justify-content, align-items/-self, flex/-grow/-shrink/-basis, gap/row-gap/column-gap, order, `vw`/`vh`/`vmin`/`vmax` | everything else, `light-dark()`, `calc()` in property values |
| Grid | `display: grid`/`inline-grid`, `grid-template-columns`/`-rows`/`-areas`, `grid-template`, `grid` (template forms), `grid-auto-rows`/`-columns`/`-flow`, `grid-row`/`-column` (+ `-start`/`-end`), `grid-area` | `grid: auto-flow ...`, `span <name>`, `<name> <n>` |
| Custom properties | `--name` declarations (inherited), `var(--name, fallback)` in any property including shorthands, nested `var()`, cycle detection, inline `style` variables | `@property` |
| Cascade | UA → author → inline, specificity, order, `!important`, inheritance, `inherit`/`initial` | cascade layers order, `unset` treated as `initial` |

Speed tricks, the same ones production engines use: rules bucketed by id/class/tag, a counting bloom filter of
ancestor tags/ids/classes that rejects most descendant selectors without walking the tree, style sharing between
identical siblings, and deduplicated computed styles (each node stores a 4-byte style index). `:root`-only rules
have their own bucket so other elements never test them. A computed style depends only on the parent's style,
the matched rules and the `style` attribute, so elements without the attribute that match the same rules under
the same parent style reuse the result instead of computing and hashing a new one.

Custom properties cost nothing on pages without them. Sites that use them (GitHub declares about 20,000) are
kept cheap three ways: variable sets form a chain that borrows the strings from the stylesheet, a set is shared
by every element that matched the same rules under the same parent, and a variable is resolved only the first
time something reads it (most theme variables never are). A `var()` value is substituted and parsed once per
(declaration, variable set) and then reused.

## Roadmap

1. ✅ Networking
2. ✅ HTML → DOM
3. ✅ CSS parser + selector matching + cascade
4. ✅ Layout: block, inline, text wrapping
5. ✅ Painting to a window: the first visible page
6. ✅ Scrolling, links, back/forward, address bar
7. JavaScript (Boa) + a subset of the DOM API
8. Then the product features: profiles, tab organisation, session restore, split view, bookmarks

## Budget log

`./scripts/budget.sh` enforces it; `make check` runs it with fmt, clippy and tests.

| Date | Change | Binary | Stress page (2.6 MB, 360k nodes) | Wikipedia article (407 KB, 7,680 nodes) |
|---|---|---|---|---|
| 2026-11-06 | First DOM (Vec children, owned strings) | 1.3 MB | 55.8 MB peak, 24 ms parse | 12.9 MB peak, 4.6 ms parse |
| 2026-11-06 | Linked children, u32 ids, interned names, borrowed tokens | 1.3 MB | 27.0 MB peak, 19 ms parse | 11.7 MB peak, 4.6 ms parse |
| 2026-11-06 | CSS: parser, `@media`, cascade with bloom filter and style sharing | 1.4 MB | 27.3 MB peak, 45 ms cascade (with a `<style>` block) | 13.6 MB peak, 476 rules, 5.7 ms cascade |
| 2026-11-06 | Layout: blocks, lines, fonts; 24-byte text runs, reserved box list | 1.4 MB | 37.0 MB peak (47 MB before slimming fragments) | 14.3 MB peak, 1.2 ms layout, 11,294 px tall |
| 2026-11-07 | Paint, window, parallel CSS fetch; budget now paints at 2x; chunked DOM and text lists | 1.5 MB | 58.3 MB peak painted at 2x (42.4 MB without paint) | n/a |
| 2026-11-07 | 28-byte DOM nodes; text and attribute values in shared buffers | 1.5 MB | 48.7 MB peak painted at 2x (32.8 MB without paint), parse 16 ms | 14.7 MB peak |
| 2026-11-07 | Browsing: scroll, links, history, address field, header, double-buffered frames | 1.5 MB | 48.8 MB peak painted at 2x | n/a |
| 2026-11-07 | CSS: sibling combinators, `:nth-*`, `:not`, states, attribute operators, positioning, overflow clips, visibility, custom properties | 1.6 MB | 48.8 MB peak painted at 2x | 29.4 MB peak, 8.7 ms cascade, 716 rules |
| 2026-11-07 | Flex, inline-block, box-sizing; matched-rule style cache, word-at-a-time hasher, allocation-free child walks | 1.6 MB | 48.6 MB peak painted at 2x; cascade 48 → 16 ms, layout 18 → 12 ms | 29.2 MB peak, 6.8 ms cascade |
| 2026-11-07 | Grid; fetched stylesheets parsed in their own buffer with comments blanked in place | 1.6 MB | 48.6 MB peak painted at 2x | 15.3 MB peak unpainted, 1.3 to 2.3 ms layout |
| 2026-11-07 | `flate` (inflate, gzip, zlib); keep-alive connections, gzip bodies, framed reads without copies | 1.7 MB | 48.6 MB peak painted at 2x | CSS 207 → 69 ms wall |
| 2026-11-07 | Images: `image` crate, decode-at-drawn-size, 8 MB pixel cache, streaming PNG writer | 1.7 MB | 48.8 MB peak painted at 2x | Cat article: 57 images (906 KB), 9 decoded at 2x, 39 MB peak |

Other pages after the layout milestone: GitHub home 22.7 MB peak (8,170 rules); Hacker News 11.5 MB.

GitHub after custom properties: 39 MB peak, 24 ms cascade, 24,822 rules. Rules that only declare or use
variables used to be dropped as empty; keeping them grew the rule count 2.5x. The first version took 84 ms and
49 MB. Lazy resolution, sets shared per matched-rule list, a `:root` bucket, unsorted candidate lists and
trimming spare `Vec` capacity in parsed rules (6 MB alone) brought it to the numbers above.

Window mode, measured with `footprint` (what Activity Monitor shows): Hacker News 48 MB with double buffering,
of which 31 MB are the two 2560x1600 Retina frames. A third buffer appeared during loading (63 MB) until the
pool was capped at two. Showing frames as a `CGImage` first cost 63 MB for a near-empty page because the
frame existed three times; painting straight into an `IOSurface` removed both copies.

Images never exist at full size: a decoder hands rows to a box filter that shrinks them to the size they
are drawn at, PNG data streams through a 32 KiB inflate window and JPEG is decoded one row of blocks at a
time. Only images in view are decoded (in parallel); decoded pixels live in a 8 MB cache that evicts what
has not been drawn recently, and the page keeps only compressed bytes (12 MB cap). The debugging PNG writer
used to hold three copies of a frame (69 MB for a 1280x3000 render); it now writes rows straight into the
file (48 MB).

Parallel stylesheet loading: GitHub's 28 stylesheets went from 3.5 s (one after another) to about 0.75 s wall.
Reusing connections and accepting gzip then took it to 0.23 s: the 28 requests no longer pay 28 TLS handshakes,
and 3.3 MB of CSS arrives as about a sixth of that.

Timings taken right after network waits are inflated: on Apple Silicon the CPU clocks down while idle, so
GitHub's cascade measures 10 ms just after its stylesheets download and 2.8 ms when warm (same code, same
rules). Compare warm numbers when hunting regressions.

## Known issues

- `example.com` occasionally resets the first connection (seen three times, never on a retry). `net::fetch` now
  retries a reset GET once, as browsers do. Cause not found.
- No table layout and no presentational attributes (`bgcolor`, `align`): Hacker News renders centred without its
  orange bar.
- Tab reaches the header controls but not links inside the page yet; there is no visible focus for page links.
- Reload cannot stop a load in progress (the load runs on the engine thread to completion).
- No cookies, so nothing stays logged in; profiles (DESIGN.md) wait for cookie storage.
- The header omits the profile name, profile edge and the ⋯ menu from DESIGN.md until profiles and menu
  actions exist, rather than showing controls that do nothing.
- Progressive JPEG, GIF, WebP and SVG are sized from their headers but not drawn yet (progressive JPEG is the
  common one on photo sites). Failed images draw nothing.
- Tables lay out as blocks: Wikipedia infoboxes and Hacker News stack their cells.
- Building the display list walks every fragment each frame (0.7 ms on the 140k-fragment stress page). An index
  by vertical position would make scrolling cost independent of page length.
- Selectors keep each class, tag and id name as its own allocation. On GitHub (28,000 rules, 3.3 MB of CSS) the
  CSS stage takes about 25 MB and the page peaks at 42 to 44 MB unpainted; interning names per stylesheet is the
  next memory step.
- Glyphs are placed on whole device pixels, so spacing is slightly uneven at 1x; subpixel positioning later.
- Parsing GitHub's CSS (8,170 rules) takes about 125 ms; worth profiling.
- Painting will add a window framebuffer (about 16 MB at 2x for 1280x800). The stress page sits at 37 MB, so
  paint must stay within roughly 20 MB including glyph caches.
