#!/usr/bin/env bash
# Site regression suite: renders every saved page in testdata/sites and compares each frame
# with the stored tiles, then reports timings and peak RAM per page.
#
#   ./scripts/sites.sh                      check every page (what CI runs)
#   ./scripts/sites.sh --only NAME          check one page
#   ./scripts/sites.sh --update             store the current renders as the reference
#   ./scripts/sites.sh --base REV           also render at REV and write pixel diffs of the
#                                           pages that changed (target/sites/diff)
#   ./scripts/sites.sh --record NAME URL LICENSE
#                                           save a live page and everything it loads
#
# A page is a folder testdata/sites/NAME with `site` (url=, license=), `archive/` (the saved
# responses, see crates/net/src/archive.rs) and `1x.tiles` / `2x.tiles`. A `url=` without a
# scheme is a file in the folder (pages written for the suite). Pages replay with no network,
# the bundled font and a frozen clock, so a render is exact: any changed tile is a change.
# macOS (arm64) and Linux (x86_64) give the same bytes, so one reference serves both; if they
# ever part, CI says so. Renders: 1x at 1280x2400 (most of the page) and 2x at 1280x800
# (the first screen).
set -euo pipefail
cd "$(dirname "$0")/.."

LIMIT_MB=60
SITES=testdata/sites
OUT=target/sites

mode=check
only=
base=
case "${1:-}" in
  --update) mode=update ;;
  --only) only=${2:?--only NAME}; ;;
  --base) base=${2:?--base REV} ;;
  --record)
    name=${2:?--record NAME URL LICENSE}; url=${3:?URL}; license=${4:?LICENSE}
    cargo build --release --quiet
    dir="$SITES/$name"
    rm -rf "$dir/archive"
    mkdir -p "$dir"
    ./target/release/broxee "$url" --record "$dir/archive" --paint > /dev/null
    printf 'url=%s\nlicense=%s\n' "$url" "$license" > "$dir/site"
    echo "saved $url: $(($(wc -l < "$dir/archive/index.tsv") - 1)) responses, $(du -sh "$dir/archive" | cut -f1)"
    echo "now run: ./scripts/sites.sh --update --only $name"
    exit 0
    ;;
  "") ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
if [ "${2:-}" = --only ] && [ $mode = update ]; then only=${3:?--only NAME}; fi

cargo build --release --quiet
bin=$PWD/target/release/broxee
rm -rf "$OUT"
mkdir -p "$OUT/new" "$OUT/png"

# Field from broxee's summary: `field <output> <label> <awk field>`.
field() { awk -v l="$2" -v f="$3" '$1 == l { print $f; exit }' <<< "$1"; }

# Render one page at one scale; prints broxee's summary. `render NAME DIR BIN SCALE [ARGS...]`
render() {
  local name=$1 dir=$2 binary=$3 scale=$4; shift 4
  local url
  url=$(sed -n 's/^url=//p' "$dir/site")
  case "$url" in
    *://*) ;;
    *) url="file://$PWD/$dir/$url" ;;
  esac
  local height=2400
  [ "$scale" = 2 ] && height=800
  "$binary" "$url" --replay "$dir/archive" --width 1280 --height "$height" --scale "$scale" "$@"
}

report="$OUT/report.md"
{
  echo "| Page | 1x | 2x | Parse | Cascade | Layout | Composite 2x | Peak RAM |"
  echo "|---|---|---|---|---|---|---|---|"
} > "$report"

failed=0
changed_pages=()
for dir in "$SITES"/*/; do
  dir=${dir%/}
  name=$(basename "$dir")
  [ -n "$only" ] && [ "$name" != "$only" ] && continue
  [ -f "$dir/site" ] || continue
  row="| $name"
  peak=0
  page_changed=0
  for scale in 1 2; do
    tiles="$OUT/new/$name/${scale}x.tiles"
    mkdir -p "$OUT/new/$name"
    if ! summary=$(render "$name" "$dir" "$bin" "$scale" --tiles "$tiles" 2>"$OUT/$name-${scale}x.log"); then
      row="$row | error"
      echo "FAIL $name ${scale}x: broxee failed, see $OUT/$name-${scale}x.log"
      failed=1
      continue
    fi
    ram=$(field "$summary" peak 3)
    peak=$(awk -v a="$peak" -v b="$ram" 'BEGIN { print (b > a ? b : a) }')
    if [ "$scale" = 1 ]; then
      parse=$(field "$summary" parse 2)
      cascade=$(field "$summary" cascade 2)
      layout=$(field "$summary" layout 2)
    else
      composite=$(awk '$1 == "paint" { for (i = 1; i < NF; i++) if ($i == "composite") print $(i + 1) }' <<< "$summary")
    fi
    reference="$dir/${scale}x.tiles"
    if [ $mode = update ]; then
      cp "$tiles" "$reference"
      row="$row | stored"
    elif [ ! -f "$reference" ]; then
      row="$row | no reference"
      echo "FAIL $name ${scale}x: no reference; run ./scripts/sites.sh --update --only $name"
      failed=1
    elif cmp -s "$tiles" "$reference"; then
      row="$row | same"
    else
      n=$(awk 'FNR == 1 { next } NR == FNR { a[FNR] = $0; next }
               { split(a[FNR], x, " "); m = split($0, y, " "); for (i = 1; i <= m; i++) if (x[i] != y[i]) c++ }
               END { print c + 0 }' "$reference" "$tiles")
      [ "$(head -1 "$reference")" != "$(head -1 "$tiles")" ] && n="size"
      row="$row | **$n tiles changed**"
      echo "FAIL $name ${scale}x: $n tiles differ from $reference"
      render "$name" "$dir" "$bin" "$scale" --png "$OUT/png/$name-${scale}x.png" > /dev/null 2>&1 || true
      failed=1
      page_changed=1
    fi
  done
  [ $page_changed = 1 ] && changed_pages+=("$name")
  if awk -v r="$peak" -v l="$LIMIT_MB" 'BEGIN { exit !(r + 0 >= l) }'; then
    echo "FAIL $name: peak RAM $peak MB (limit $LIMIT_MB MB)"
    failed=1
  fi
  echo "$row | ${parse:-} ms | ${cascade:-} ms | ${layout:-} ms | ${composite:-} ms | $peak MB |" >> "$report"
done

if [ -n "$base" ]; then
  # Render the same pages with the build at REV and draw where they differ.
  tree="$OUT/base-tree"
  git worktree remove --force "$tree" 2> /dev/null || true
  git worktree add --detach "$tree" "$base" > /dev/null
  (cd "$tree" && cargo build --release --quiet)
  cargo build --release --quiet -p paint --example framediff
  mkdir -p "$OUT/diff"
  pages=("${changed_pages[@]}")
  [ ${#pages[@]} -eq 0 ] && for dir in "$SITES"/*/; do pages+=("$(basename "$dir")"); done
  for name in "${pages[@]}"; do
    [ -n "$only" ] && [ "$name" != "$only" ] && continue
    for scale in 1 2; do
      old="$OUT/diff/$name-${scale}x-base.png"
      new="$OUT/diff/$name-${scale}x.png"
      render "$name" "$SITES/$name" "$tree/target/release/broxee" "$scale" --png "$old" > /dev/null 2>&1 || continue
      render "$name" "$SITES/$name" "$bin" "$scale" --png "$new" > /dev/null 2>&1 || continue
      printf '%s %sx: ' "$name" "$scale"
      ./target/release/examples/framediff "$old" "$new" "$OUT/diff/$name-${scale}x-diff.png" || true
    done
  done
  git worktree remove --force "$tree"
fi

cat "$report"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "### Site regressions ($(uname -sm))"; echo; cat "$report"; } >> "$GITHUB_STEP_SUMMARY"
fi
if [ $failed = 1 ]; then
  echo
  echo "FAIL. Renders of changed pages: $OUT/png. This build's tiles: $OUT/new."
  echo "If the change is intended: ./scripts/sites.sh --update, and commit the tiles."
  echo "To see what moved: ./scripts/sites.sh --base main"
  exit 1
fi
echo "PASS: every page renders as stored"
