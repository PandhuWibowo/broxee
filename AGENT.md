# AGENT.md: guide for AI coding agents working on Broxee

Read `CLAUDE.md` first for the module map and conventions. This file lists the rules and the routine.

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

## Commands to run after any change

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
./scripts/budget.sh
```

`make check` runs all four.

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
