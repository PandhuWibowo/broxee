# AGENTS.md: guide for AI coding agents working on Broxee

Read `CLAUDE.md` first for the module map and conventions. This file lists the rules and the routine.
`CLAUDE.md` is the full, authoritative list of invariants; when the two disagree, `CLAUDE.md` wins and this file
gets fixed.

## What Broxee is

A web browser with its own engine, written in Rust (edition 2021): networking, HTML, CSS, layout and painting are
all in this repository. The only borrowed piece is TLS (`rustls` with the `ring` backend). Today it opens a macOS
window that loads, lays out and paints pages; the same `broxee` binary is a CLI that prints the DOM, computed
styles or layout with timings and peak RAM. On Linux only the CLI exists (no window yet).

## Non-negotiable rules

1. **60 MB budget.** The release binary and peak RAM on the budget stress page stay under 60 MB.
   `./scripts/budget.sh` must print `PASS`.
2. **No new dependencies** without explicit approval from the owner. Say what it adds to the binary size.
3. **Everything is written here, from scratch.** The one exception is TLS (`rustls`): no hand-rolled crypto.
   OS interfaces are declared by hand, not pulled from binding crates.
4. **Respect `docs/pipeline.md`.** Only parallelise stages that share no mutable state; keep AppKit on the main
   thread; keep stylesheet order equal to document order.
5. **Zero warnings**, `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
6. **Never panic on malformed input.** Add a regression test for every parsing bug you fix.
7. **Never drive the real desktop from tests or checks.** No synthetic keystrokes or clicks: test the browser on
   the in-memory `FakeScreen` instead.
8. **UI follows `DESIGN.md`**, filtered through `antislop`. Do not invent a logo, icons or brand colors that
   `DESIGN.md` does not define; ask, or use an honest placeholder.
9. **A feature that cannot fit the budget is redesigned**, never shipped with the limit raised.
10. **Never log or print cookie values**, never write session cookies to disk, keep cookie and session files 0600.

## Repository layout

| Path | What lives there |
|---|---|
| `crates/flate` | DEFLATE, zlib, gzip, Brotli, CRC-32, Adler-32 |
| `crates/net` | URLs, HTTP/1.1, TLS, keep-alive pool, redirects, `file://`, cookies, Public Suffix List |
| `crates/html` | tokenizer, entities, tree builder, arena DOM, the shared fast hasher |
| `crates/css` | parser, `@media`, selectors, cascade, computed values, grid templates, UA and quirks sheets |
| `crates/font` | TrueType/OpenType metrics and outlines, `font::Approximate` for tests |
| `crates/layout` | block, inline, flex, grid and table layout into fragment lists |
| `crates/image` | PNG and JPEG decoders that scale while decoding, GIF/WebP size probes |
| `crates/paint` | display list, glyph rasterizer and cache, image cache, band compositing, PNG writer |
| `crates/platform` | macOS window via the Objective-C runtime and `IOSurface` (empty on other OSes) |
| `crates/shell` | the `broxee` binary: CLI, page pipeline, browser (tabs, profiles, session), chrome, palette |
| `scripts/budget.sh` | the 60 MB check |
| `docs/pipeline.md` | what runs sequentially, in parallel, or on the main thread only |
| `DESIGN.md` | design data for every UI change |
| `CHANGELOG.md`, `README.md` | changes per release; roadmap, CSS/layout support, budget log, compression decisions |

## Invariants by area

Before touching a crate, read its section in `CLAUDE.md`. The ones that are easiest to break:

| You touch | Read in `CLAUDE.md` | Easy to break |
|---|---|---|
| `html` | Key conventions | 28-byte nodes with no per-node `Box`/`String`/`Rc`; slice `&str` only at ASCII delimiters |
| `css` | CSS invariants | vocabulary pruning, unsupported selectors must not match, bloom filter in step with traversal |
| `layout` | Layout invariants | every box laid out once (no measure-by-layout); `TextFragment` stays 24 bytes; `Layout::index` after moves |
| `paint`, `image` | Paint and platform invariants | caches written only before compositing; images never decoded at full size |
| `platform` | Paint and platform invariants | exact C signature on every `send!`; AppKit on the main thread; paint straight into the `IOSurface` |
| `shell` (tabs) | Tab invariants | only the visible tab is live; snapshots hold only sources; one page load at a time |
| `net` | Tab invariants | cookies per profile; `net::forget_sessions` on profile switch |
| `flate` | Compression invariants | `discard_ahead` before moving `Bits::pos`; `unsafe` only for the ARMv8 CRC32 intrinsics |
| `shell/main.rs` | Paint and platform invariants | nothing may run before `space_efficient_malloc` on macOS |

## Commands to run after any change

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
./scripts/budget.sh
```

`make check` runs all four.

Other useful commands:

```bash
make run URL=https://example.com                     # fetch, parse, print the tree
cargo run --release -- page.html --layout --width 800  # every box and line
cargo run --release -- page.html --png out.png        # render to a PNG (works on Linux too)
```

## Toolchain

- CI and the release workflow use the **latest stable Rust** (`dtolnay/rust-toolchain@stable`). New stable
  releases add clippy lints, and with `-D warnings` they turn CI red. Run `rustup update stable` before running
  clippy, so you see what CI sees.
- Code that compiles only on macOS (`platform`, the window, the ARMv8 CRC path in `flate`) is not built by a
  Linux `cargo clippy`. Items used only from macOS-only modules need
  `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]`, or Linux CI fails on dead code.
- `cargo clippy --target aarch64-apple-darwin` works on Linux for every crate that does not depend on `ring`
  (`-p flate -p html -p css -p font -p layout -p image -p paint -p platform`).

## Testing

- Tests live next to the code in a bottom `#[cfg(test)] mod tests`.
- Layout tests use `font::Approximate` (every glyph 0.5em, space 0.25em), never the fonts installed on a machine.
- Browser behaviour is tested through `FakeScreen` in `crates/shell/src/browser.rs` with local files; render a PNG
  from it when a visual check is needed.
- Every parsing bug fix gets a regression test; malformed input must parse to something sensible.

## Adding an engine crate (css, layout, paint)

1. Create `crates/<name>/` with `version.workspace = true` and `edition.workspace = true`.
2. Add it to `members` in the root `Cargo.toml`.
3. Keep data in arenas or flat `Vec`s indexed by id, like `html::Dom`.
4. Wire it into `crates/shell` and print its timing next to `fetch` and `parse`.
5. Run the budget script and record the result in the README budget log.

## Performance habits

- Measure before optimising: `broxee <page>` prints fetch time, parse time and peak RAM.
- Prefer one pass over the input. The tokenizer never backtracks except to find a raw-text end tag.
- Reuse buffers instead of allocating per node where it is easy; merge adjacent text nodes (the tree builder does).
- Release profile uses LTO, one codegen unit, `strip` and `panic = "abort"`. Do not loosen it to speed up builds.
- Measure before adding a format or a cache, and record the decision with its numbers in README's
  "Compression" table.
- Window memory is measured with `footprint <pid>` on a real macOS window, over a sequence of heavy pages, not
  only with the CLI.

## Commits and pull requests

- Commit subjects follow `type(scope): summary` in lower case, e.g. `feat(paint): place glyphs on quarter pixels`,
  `fix(url): ...`, `perf(css): ...`, `test(net): ...`. The scope is a crate or a feature area.
- Add user-visible changes to `CHANGELOG.md` under `## Unreleased`.
- When a milestone lands, add a row to the "Budget log" table in `README.md` with the binary size and peak RAM.
- `cargo fmt` before every commit; CI runs `cargo fmt --check`, clippy, tests and the budget on Linux and macOS.

## Releasing

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed. It builds macOS arm64, macOS
x86_64 and Linux x86_64, runs clippy, tests and the budget, and publishes a GitHub prerelease with `.tar.gz`
archives and `.sha256` checksums.

1. Set `version` under `[workspace.package]` in the root `Cargo.toml` (all crates inherit it).
2. In `CHANGELOG.md`, move `## Unreleased` into `## X.Y.Z (YYYY-MM-DD)` and leave an empty `## Unreleased` above.
3. Merge to `main` and wait for CI to be green.
4. Tag the commit on `main` and push the tag:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The workflow refuses a tag that does not match the `Cargo.toml` version. It can also be run by hand
(`workflow_dispatch`) with a tag name: an existing tag is rebuilt, and a new one is created on the latest commit of
the default branch when the release is published. Use that when tags cannot be pushed (agent sessions that may only
push branches).
