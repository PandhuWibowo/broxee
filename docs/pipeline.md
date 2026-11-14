# Pipeline and concurrency

How a page moves from a URL to pixels, and which stages run in sequence, in parallel, or on which thread.
The rule of thumb: a stage runs in parallel only when its pieces share no mutable state; anything that
depends on document order stays sequential.

## Threads

| Thread | Owns | Why |
|---|---|---|
| **Main (UI)** | the AppKit event loop, the window, presenting frames | macOS only allows UI calls on the main thread. It never does engine work, so the window stays responsive while a page loads. |
| **Engine** | the browser state, input handling, painting, the visible page | Keeps every page-level data structure on one thread at a time, so none of them need locks. It never waits on the network: it polls a running load once per frame (16 ms) and otherwise sleeps until an event arrives. |
| **Loader** (one per page load) | one load: fetch, parse, subresources, cascade, layout | A load builds its `Loaded` alone and sends it to the engine thread whole. At most one runs at a time for memory; dropping its channel cancels it at the next stage boundary (a fetch already waiting finishes first, then the thread frees what it built). |
| **Fetch pool** (scoped, up to 6) | one subresource request each | Network waits dominate load time. Six concurrent requests per page matches what browsers allow per host. Finished connections go to a shared keep-alive pool (`net`, up to 8 idle for 30 s), so later requests to the same host skip the TCP and TLS handshakes. |
| **Raster bands** (scoped, one per core) | one horizontal band of the framebuffer each | Bands never overlap, so each thread writes its own rows with no locking. |
| **Font loader** | reading the default page font at startup | Independent of the page, so it overlaps with the HTML request. Figtree and the symbol font are decoded later, on the thread that first draws with them. |

There is no async runtime. Blocking I/O on a small, bounded set of threads is enough for dozens of requests
and keeps the binary small; an event-driven (kqueue/epoll) network layer can replace the fetch pool later
without changing the stages around it. The places that are event-driven by nature are the UI run loop (main
thread) and, later, the JavaScript event loop (single-threaded, as in every browser).

## Stages

| # | Stage | Needs | Mode | Thread | Status |
|---|---|---|---|---|---|
| 1 | Load fonts | nothing | **parallel** with 2 to 4 | font loader | done |
| 2 | Fetch HTML | URL | sequential (one request) | engine | done |
| 3 | Parse HTML into the DOM | whole body | sequential (streaming later) | engine | done |
| 3b | Prepare form controls, run scripts | DOM | sequential: external scripts fetched in document order, then run in order on a scoped thread with a 64 MB reserved stack (`script::on_script_stack`), then `DOMContentLoaded` and `load` | loader + scoped | done |
| 4 | Discover stylesheets and images | DOM | sequential scan, document order recorded; images among the parser's nodes only | engine | done |
| 5 | Fetch stylesheets, then images | URLs from 4 | **parallel**, bounded to 6, stylesheets queued first; images keep compressed bytes and their header size only | fetch pool | done |
| 6 | Parse each stylesheet | its own body + the DOM's tags, ids and classes | **parallel**, inside its fetch thread; rules that cannot match the document are skipped | fetch pool | done |
| 7 | Join | all of 6 | **barrier**: sheets back in document order | engine | done |
| 8 | Cascade | DOM + every sheet, in order | sequential (parallel per subtree is possible later: style sharing, the matched-rule cache and the bloom filter are per-thread state) | engine | done |
| 8b | Find and fetch web fonts | styles + `@font-face` rules | the text's characters are matched to faces (sequential), then the needed faces are fetched and decoded **in parallel**, up to 6; pages without `@font-face` skip it | loader + fetch pool | done |
| 9 | Layout | styles + fonts (1 joined here) | sequential: a block's position depends on its previous siblings; flex items need their siblings' sizes. Each box is laid out once | engine | done |
| 10 | Build display list | layout | sequential, linear and cheap | engine | done |
| 11 | Rasterize glyphs | unique (glyph, size) pairs from 10 | sequential into a shared cache, read-only afterwards | engine | done |
| 11b | Decode visible images | images in 10 not yet cached | **parallel** across images (scoped threads), each decoded and scaled down in one streaming pass to its drawn size; written to the cache before compositing | engine + scoped | done |
| 12 | Composite | display list + glyph and image caches | **parallel** by horizontal band | raster bands | done |
| 13 | Present | pixels | sequential, **main thread only**, handed over with `dispatch_async_f` | main | done |

## Input and frames in the window

| Step | Mode | Thread |
|---|---|---|
| AppKit delivers scroll, click, key, paste and resize events | event-driven | main |
| Events cross to the engine over a channel | async hand-off | main to engine |
| The engine handles every queued event, then paints once (30 scroll events cost one frame) | coalesced, sequential | engine |
| Navigation runs stages 2 to 9 on the loader thread; after each stage it reports progress and the engine thread repaints the loading line on its next frame | sequential, off the engine thread | loader, then engine |
| Frames are painted straight into one of two `IOSurface`s; the one on screen is never written | double-buffered | engine, then main |
| A change to the header alone (typing in the address field, the loading line) paints only the header, into the header layer's own two small surfaces; a full frame includes the header and hides that layer | double-buffered | engine, then main |
| A DOM change (a key in a field, a script's handler or timer, `:hover`/`:focus` that a rule sees) runs `engine::update` once for everything that task changed: the cascade again unless only text changed and no rule reads text, then layout inside the smallest anchor around the changes (a full layout when none fits) | sequential, coalesced per task | engine |
| Switching tab: the visible page is reduced to its sources, written to disk, then dropped; the selected tab is read back and rebuilt with stages 2 to 9 (no fetches) | sequential | engine |
| The session file is rewritten (temporary file, then rename) after each tab or navigation change | sequential, a few KB | engine |

If the window server still holds the spare surface, the engine waits up to 50 ms for it rather than
allocating a third 16 MB buffer; past that it reuses it and accepts one possibly torn frame.

## Ordering rules that must not break

- **Stylesheet order is document order**, regardless of which fetch finishes first. Results are stored by
  index and joined at stage 7.
- **The glyph cache is written before compositing starts and only read during it.** Compositing threads never
  insert, so they share it without a lock.
- **Frames cross threads by reference counting, never by sharing a buffer being written.** The engine paints a
  surface that is not on screen, then sends the main thread its own reference; the main thread gives it to the
  layer and drops that reference. The surface on screen is never written.
