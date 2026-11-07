# Changelog

## Unreleased

### Added
- `net`: URL parsing and relative resolution, HTTP/1.1 over TCP with TLS (rustls), chunked bodies, redirects, `file://`.
- `html`: tokenizer, entity decoding, tree builder with implied end tags and raw-text elements, arena DOM.
- `shell`: `broxee <url-or-file> [--tree] [--depth N]`, printing timings and peak RAM.
- `scripts/budget.sh`: fails the build when the binary or peak RAM reaches 60 MB.
- Project files: `CLAUDE.md`, `AGENT.md`, `.claude/`, CI (fmt, clippy, tests, budget on Linux and macOS), `Makefile`.

- `css`: stylesheet and `style=""` parsing with shorthand expansion, `@media` evaluated against the
  viewport, selector matching (type, class, id, attribute, `:root`, descendant, child), cascade with
  specificity, `!important`, inline styles and inheritance, and a user-agent stylesheet.
- `shell`: loads `<style>` and `<link rel=stylesheet>` (up to 32), runs the cascade, `--styles` prints
  computed styles, `--no-css-fetch` skips external sheets.
- `net`: a GET whose connection is reset is retried once.
- `css`: `min-width`, `max-width`, `min-height`, `max-height`; `vw`, `vh`, `vmin`, `vmax` units; rules whose
  subject is only an attribute selector get their own index bucket.
- `font`: TrueType/OpenType reader for advances and vertical metrics (cmap formats 4 and 12, `.ttc`).
- `layout`: block layout (sizes, auto margins, min/max, percentages, sibling margin collapsing), inline layout
  (line breaking, white-space modes, `<br>`, text-align, half-leading baselines), `<img>` from its size,
  scrollable height including overflow.
- `shell`: `--layout` prints every box and line, `--width` sets the viewport; text is measured with Arial or
  DejaVu Sans when present.
- `font`: glyph outlines from `glyf`/`loca`, including composite glyphs and their transforms.
- `paint`: display list, signed-area anti-aliased rasterizer, glyph cache, compositing in parallel bands,
  painting into caller-owned strided BGRA memory, PNG writer (CRC-32, Adler-32, stored deflate).
- `platform`: macOS window through the Objective-C runtime, Quit menu, quit on last window close, engine thread
  with `dispatch_async_f` hand-off, frames shown through `IOSurface` without copies.
- `shell`: `--window`, `--png`, `--paint`, `--scale`, `--height`; stylesheets fetched and parsed in parallel
  (bounded to 6, joined in document order); fonts load in parallel with the page.
- `css`: `:link` and `:any-link`.
- `html`: `ChunkVec`, a chunked list that grows without copying.
- `docs/pipeline.md`: which stages are sequential, parallel, or main-thread only.

- Browsing in the window: scrolling (trackpad, wheel, keys), following links, back/forward/reload with scroll
  positions restored, an address field (⌘L, typing, paste, Enter/Esc), Tab focus through the header with focus
  rings, error and message pages rendered by the engine, re-layout on resize, dark appearance from macOS.
- `platform`: input events from a custom `NSView` (scroll, click, keys, paste), window delegate for resize and
  display changes, dark appearance detection, two alternating `IOSurface`s per window.
- `paint`: `overlay` for interface drawing (fills, text, masks, horizontal clip), glyph cache trimming.
- `shell`: `engine.rs` (shared pipeline, fonts loaded once and shared), `browser.rs` behind a `Screen` trait so
  scenario tests run on an in-memory screen, `chrome.rs` for the header.

- `css`: `+` and `~` combinators, `:first-child`, `:last-child`, `:only-child`, `:nth-child()`,
  `:nth-last-child()`, `:not()` with simple arguments, `:checked`, `:disabled`, `:enabled`, `:empty`, attribute
  operators (`~=`, `|=`, `^=`, `$=`, `*=`, `i`). Style sharing turns off when a sibling-dependent rule exists.
- `css`: `position`, `top`/`right`/`bottom`/`left`/`inset`, `visibility`, `overflow`, `opacity: 0`.
- `css`: custom properties and `var()` with fallbacks, nesting and cycle detection, resolved lazily on the
  declaring element.
- `layout`: relative offsets, absolute and fixed boxes in their own paint layers (containing block, insets,
  shrink-to-fit), overflow clip ranges.
- `paint`: layers in order, fixed layers ignore scroll, nested clips per item, hidden content skipped.
- `shell`: link hit testing follows layers, fixed boxes and visibility.
- `css`: flex properties (`flex-direction`, `flex-wrap`, `flex-flow`, `justify-content`, `align-items`,
  `align-self`, `flex`, `flex-grow`, `flex-shrink`, `flex-basis`, `gap`, `row-gap`, `column-gap`, `order`),
  `inline-flex`, `box-sizing`.
- `layout`: flex containers (`flex.rs`): one layout per item, sizes from memoized intrinsic widths, the 9.7
  resolution loop, auto margins, justify and align, wrapping rows, columns. `inline-block`/`inline-flex` as atomic
  inlines. `box-sizing: border-box`.
- `css`: grid properties (`grid.rs`): track lists, `repeat()` with `auto-fill`/`auto-fit`, `minmax()`,
  `fit-content()`, template areas, line placement and the `grid-template`, `grid-area`, `grid-row`, `grid-column`
  shorthands. Templates and placements are interned in `StyleMap`; a style carries two ids.
- `layout`: grid containers (`grid.rs`): placement by area, line and span, sparse auto-placement, track sizing with
  content contributions, maximize, `fr` and auto stretch, alignment; every item laid out once.
- `flate`: DEFLATE decoding (stored, fixed and dynamic Huffman blocks, 9-bit fast lookup), zlib and gzip
  wrappers with checksum checks, an output cap. CRC-32 and Adler-32 moved here from the PNG writer.
- `net`: keep-alive connection pool, `Accept-Encoding: gzip, deflate`, responses framed by length or chunks
  and read straight into the body, 64 MB body cap, connect and write timeouts.
- `image`: PNG decoder (every color type and bit depth, palettes, `tRNS`, Adam7) and baseline JPEG decoder
  (Huffman with 9-bit lookup, float IDCT with a DC-only path, any subsampling, restart markers, Adobe RGB),
  both scaling rows down as they decode; header probes for PNG, JPEG, GIF and WebP.
- `flate`: streaming zlib output with a 32 KiB window, incremental CRC-32 and Adler-32.
- `net`: `data:` URLs.
- Images on pages: fetched with stylesheets, sized by layout as replaced elements, decoded in parallel when
  visible at their drawn size, cached up to 8 MB of pixels (on-screen images are always kept).
- Tables (`layout/table.rs`): automatic column widths, spans, row groups, anonymous rows, spacing, row
  backgrounds, vertical alignment, `inline-table`. `css`: table display values, `vertical-align`,
  `border-spacing`, `border-collapse`.
- Presentational hints (`css/hints.rs`), quirks-mode table reset (`quirks.css`), `text-align: -webkit-center`
  for `<center>`. `html`: a new `tbody`/`thead`/`tfoot`/`caption` closes open rows and cells.
- Inline elements with margins, padding, borders or backgrounds: they take space on the line and get one box
  per line they span (`<code>` on Wikipedia, the Hacker News header gap).
- Tables: non-table content goes in anonymous cells (Wikipedia's `figure { display: table }` lost its image);
  `caption-side: bottom`.
- `platform`: `Window::release_spare`, `release_free_memory`; the window is fitted to the screen before showing.
- `image`: progressive JPEG (spectral selection, successive approximation, end-of-band runs, restarts; partial
  files show what arrived), keeping only the frequencies the drawn size needs.
- `flate`: Brotli decoding (RFC 7932: prefix codes, context modeling, block switching, static dictionary and
  transforms), verified byte for byte against the reference encoder and live GitHub, Cloudflare and MDN streams.
  `net` asks for `br` first.

- Tabs and profiles: a 240-point sidebar with the profile switcher, the tab list and "+ New tab"; the
  profile's 2px edge over the sidebar and the pane; the pane header names the profile ("github.com · Work").
  ⌘T, ⌘W, ⌘⇧T, ⌘1 … ⌘9, ⌘⇧P, ⌘\\. Personal (emerald) and Work (amber) keep their own tabs, history and
  closed tabs.
- Sleeping tabs (`hibernate.rs`): only the visible tab is a live page; the others are written to a private file
  as the sources they arrived as (`net::Response::wire`, `engine::Sources`) plus their compressed images, and
  rebuilt from it without the network (`engine::snapshot`, `engine::restore`).
- Session restore: tabs and history are saved after each change and come back asleep on start.
  `broxee --window` without an address restores the session; several addresses open one tab each.
- `net::forget_sessions`: switching profile closes idle connections and drops TLS session tickets.
- `paint::Target::left`: painting into a pane that starts right of the sidebar.

### Changed
- gzip decoding twice as fast (237 → 478 MB/s): CRC-32 with ARMv8 instructions or slicing-by-8, output allocated
  from the gzip size trailer, 8-byte match copies, 8-byte bit refills.
- Every JPEG drawn smaller than its size decodes with a k-point inverse DCT (libjpeg's scaling): a 12 MP baseline
  photo at 400 px went from 172 to 21 ms.
- Rule pruning also checks attributes and exact attribute values, dropping unused themes.
- Window: GitHub peak 74 → 58 MB, idle 43 MB; a resize that changes nothing no longer restyles and repaints.
- Stylesheets are parsed against the document's tags, ids and classes; rules that cannot match are skipped.
  GitHub: 28,170 of 30,309 rules, peak 43.7 → 25.3 MB unpainted and 60.2 → 40.5 MB painted at 2x.
- Layout: `<center>` checks once per container, image checks by atom.
- The PNG writer streams rows into the file: a 1280x3000 render peaks at 48 MB instead of 69 MB.
- GitHub's stylesheets load in 0.23 s instead of 0.72 s, Wikipedia's in 69 ms instead of 207 ms.
- Fetched stylesheets are parsed in the buffer they arrived in, and comments are blanked in place, instead of two
  copies per sheet. Flex and grid share one box-height helper.
- Cascade 3x faster on the stress page (48 to 16 ms): computed styles are cached by (parent style, matched rules);
  the shared hasher mixes 8 bytes per step instead of 1 (`Fnv` is now `FastHasher`); candidate rules are no
  longer sorted twice.
- Layout 18 to 12 ms on the stress page: children are walked in place and inline runs are sibling ranges, with no
  allocation per box or inline element.
- Fixed: absolutely positioned boxes used containing-block and static positions recorded before an ancestor was
  shifted (`position: relative`, flex alignment). Both are now read from the ancestor's box when needed.
- DOM memory halved: children are a linked list instead of a Vec per node, ids are u32, tag and
  attribute names are interned, text is stored at exact size. Stress page peak RAM 55.8 MB to 27.0 MB.
- DOM nodes shrank from 48 to 28 bytes: text, comments and attribute values live in shared buffers instead of
  one allocation each, and `NodeKind` / `Element` are now views borrowed from the DOM. Stress page painted at
  2x: 58.3 MB to 48.7 MB; parse 20 ms to 16 ms.
- The `libc` crate is no longer used; `getrusage` is declared by hand.
- `scripts/budget.sh` measures peak RAM with the page painted at 2x.
- The raw HTML is freed right after parsing.
- Tokenizer borrows from the source and allocates only for lowercased names or decoded entities.
  Stress page parse 24 ms to 19 ms.
- Fixed: a new page could show the previous page's image. The painter cached decoded images by their index
  within the page, so another page's image 0 drawn at the same size reused the old pixels; the cache is now
  cleared when the page changes, which also frees the old page's pixels.
- macOS: `broxee` restarts itself once with `MallocSpaceEfficient=1`, so malloc returns freed memory instead
  of keeping it dirty. Window peak: GitHub 58 → 48 MB, four tabs 61 → 46 MB, twelve heavy pages in a row
  47 MB; idle about 29 MB. Timings unchanged (#1).
- Pages load on their own thread (#2): the window keeps scrolling, following links, switching tabs and
  profiles and editing the address while a page loads. Esc or the stop button (× in place of reload) stops a
  load; opening another page or changing tab stops it too. Back, forward and reload move the tab only once
  the page arrives.
- URLs follow the URL standard for whitespace (#3): leading and trailing spaces and controls are dropped, tabs
  and newlines removed, and spaces, unsafe characters and non-ASCII in paths and queries percent-encoded; a
  host with a space or another forbidden character is an error, an empty port means the default, and
  `user:pass@` is dropped (never sent). Several addresses pasted as one, on the command line or in the
  address field, are refused with a clear message. `file://` paths are decoded before reading, so files with
  spaces in their names open.
- `net`: a test for the one-time retry of a GET whose connection closes before any response (#4). The
  occasional reset from example.com could not be reproduced: 100 fresh connections and connections reused
  after 2 to 28 seconds idle all answered.
- ⌘K palette (#5): open tabs in every profile (labelled with the profile), this profile's history (most recent
  first, with page titles now kept per history entry), every command with its shortcut, and a typed address.
  Fuzzy matching ranks substrings, word starts and letters found apart. Keyboard (↑ ↓, Tab, Page Up/Down,
  Enter, Esc, ⌘K to close) and mouse; it fits the window's height and scrolls its results. A new tab opens
  with the palette, and the sidebar has the "Search or go to ⌘K" field. Radius 10, a hairline and one soft
  shadow; ⌘ ⇧ ⌥ drawn as icons; long titles end with "…".
- Cookies, one jar per profile (#6): RFC 6265bis parsing and matching (cookie dates, `Domain`, `Path`,
  `Expires`, `Max-Age`, `Secure`, `HttpOnly`, `SameSite`, name prefixes), sent and stored on every redirect
  hop, third-party cookies blocked, public suffixes refused using the built-in Public Suffix List
  (`net::site`), persistent cookies saved per profile with mode 0600, session cookies kept in memory only,
  and "Clear this profile's cookies" in the palette. The session file is now written 0600 too.
- Keyboard focus for links in the page (#20): Tab and Shift+Tab continue from the header into the page's
  visible `<a href>` links in document order and wrap; the page scrolls to the focused link, a 2px `accent`
  ring with a 2px gap marks it, Enter follows it, Esc leaves it. The focus is kept per tab and comes back
  when a sleeping tab wakes. Fixed: Tab in the address field restarted the order from the first button.
- Bookmarks per profile (#8): ⌘D saves the page with a name and folder (the last folder preselected) or edits
  its bookmark; ⌘⌥B shows them by folder in place of the tab list, with keyboard and mouse; every bookmark
  is a palette result; "Remove this page's bookmark" is a palette command; saved per profile (0600) and
  read defensively. The sidebar is now a list of rows (labels, items, an empty state) with long titles
  ending in "…". Key events carry the Option modifier.
- Sidebar (#9): pinned tabs and tab groups, shown under section labels and kept in that order (so ⌘1 … ⌘9
  match the screen); folding a group (its label, or the palette) hides all but its active tab; dragging a row
  reorders tabs, with a 2px drop line, and a tab joins the section it lands in; the list scrolls under the
  profile row and search field, and follows the active tab; the right edge resizes the sidebar from 200 to
  320 points, laying the page out once when released; pins, groups, folds and the width are saved in the
  session. Labels and titles end with "…" when long. The window reports drags, releases and where a scroll
  happened.
- Profiles can be added, renamed, recolored and removed (#11), from the palette: new profiles take the least
  used color; removing asks for the profile's exact name and deletes its folder (tabs, history, cookies,
  bookmarks); the last profile cannot go; up to 12. Each profile keeps a fixed folder id, validated when read
  (letters, digits, `-`, `_`), so a damaged session cannot point outside `profiles/`. Two new colors, rose
  and violet, checked to 4.5:1 on `canvas` in both themes and recorded in DESIGN.md. The palette lists
  "Switch to …" for every other profile.
- ⋯ menu and tooltips (#10): the header's ⋯ button (also a Tab stop) opens a menu to copy the address, open the
  page in a new tab of another profile, bookmark it, reload or close the tab, by mouse or keyboard; tooltips
  appear after 0.6 s on the header's buttons (with their shortcuts), the profile switcher, the close button
  and sleeping tabs ("Asleep to save memory. Select to wake."), and hide on any other input. The window
  reports pointer moves and can write to the clipboard. A flaky 50 ms timing check in a test became a 2 s
  guard.
- CSS parsing (#16): rules whose selector text names a class or id the page lacks are dropped before their
  selectors are built. GitHub's 30 sheets (3.4 MB): 15.0 → 11.7 ms and 155,109 → 34,167 allocations, with
  the same 820 rules kept; checked rule by rule against the full selector check on GitHub, Wikipedia, MDN
  and Hacker News. `cargo run --release -p css --example parse_bench -- page.html sheets...` measures it
  (`CHECK=1` runs the comparison). The old "about 125 ms" note was stale.
- Display list (#17): layout records the vertical extent of every run of 256 boxes and 256 text runs
  (`Layout::box_extents`, `text_extents`, a few KB), and painting visits only runs that reach the view. On the
  140,000-run stress page the display list takes 0.01 ms a frame instead of 0.65 ms, wherever the page is
  scrolled; indexing adds about 0.8 ms to each layout. A test paints a long page with fixed and positioned
  layers at scroll positions on run edges, with and without the index, and compares every pixel.
