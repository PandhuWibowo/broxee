#!/usr/bin/env bash
# Fails when Broxee starts too slowly: time from the process start to the first frame, and to
# an interactive page, for a cold and a warm launch (README, "Startup").
#
# Each launch runs the window's own startup drawn into memory (`--offscreen`), 1280x800 points
# at 2x (a Retina frame), and prints its milestones (`BROXEE_STARTUP`, crates/shell/src/startup.rs).
# The clock starts just before the binary is executed, so loading it, and on macOS its restart
# with `MallocSpaceEfficient`, are counted.
#
#   cold  the first launch ever of a fresh copy of the binary, in an empty HOME (no session,
#         tab files, cookies or cache), opening a local page given as its address. The OS file
#         cache is not flushed (that needs root): the copy was just written, so it is in memory.
#   warm  a launch with no address in the HOME the cold one left, after one more launch added two
#         tabs: the session (three tabs, two profiles) is restored and its visible tab, the same
#         page, is loaded again. The median of WARM_RUNS launches.
#
# main: the first line of `main` (after the restart on macOS).
# first-frame: the window's first frame is painted (sidebar and header).
# interactive: the visible tab's page is laid out and painted, answering input.
set -euo pipefail

# Warm launches: what a person waits for every day. GitHub's macOS runner took 56 ms to the first
# frame and 75 ms to interactive (17 to 28 and 26 to 44 ms on an Apple Silicon laptop): the limits
# leave it about 3.5x and 5x, so a noisy run does not fail and a real regression still does.
FIRST_FRAME_LIMIT_MS=200
INTERACTIVE_LIMIT_MS=400
# The cold launch is mostly the system checking a binary it has never run (200 to 460 ms on
# macOS before `main` here), which varies from run to run and machine to machine: a loose limit.
COLD_LIMIT_MS=1500
WARM_RUNS=5
cd "$(dirname "$0")/.."

cargo build --release --quiet

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
bin="$tmp/broxee"
cp target/release/broxee "$bin"
home="$tmp/home"
mkdir "$home"

# A long article: headings, paragraphs with links, lists, and a <style> block.
page="$tmp/page.html"
awk 'BEGIN {
  print "<!doctype html><html><head><title>Startup page</title><style>body { font: 16px/1.5 sans-serif; max-width: 720px; margin: 0 auto } .row h2 { font-size: 20px } div > p a { color: #0369a1 } ul li { margin: 2px 0 } .r7 span { font-weight: bold }</style></head><body>"
  for (i = 1; i <= 1000; i++)
    printf "<div class=\"row r%d\"><h2>Section %d</h2><p>Some text &amp; more about it, with <a href=\"/p/%d\">a link</a> and <span>a word</span> or two.</p><ul><li>one<li>two<li>three</ul></div>\n", i, i, i
  print "</body></html>"
}' > "$page"
for extra in a b; do
  printf '<!doctype html><title>Tab %s</title><p>Another tab.</p>\n' "$extra" > "$tmp/$extra.html"
done
page_kb=$(( $(wc -c < "$page" | tr -d ' ') / 1024 ))

# One launch: prints "main first-frame interactive" in ms. The clock starts in the process that
# then becomes broxee (perl's exec keeps the process), so nothing but the exec is between.
launch() {
  local out
  out=$(HOME="$home" perl -MTime::HiRes=time -e \
    '$ENV{BROXEE_STARTUP} = sprintf("%.0f", time() * 1e6); exec @ARGV or exit 127' \
    -- "$bin" --offscreen --width 1280 --height 800 --scale 2 "$@" 2>&1 >/dev/null)
  awk '$1 == "startup" { t[$2] = $3 } END {
    if (t["main"] == "" || t["first-frame"] == "" || t["interactive"] == "") exit 1
    print t["main"], t["first-frame"], t["interactive"]
  }' <<< "$out" || { echo "FAIL: no startup milestones from broxee:" >&2; echo "$out" >&2; exit 1; }
}

cold=$(launch "$page")
launch "$tmp/a.html" "$tmp/b.html" "$page" > /dev/null
warm_runs=()
for _ in $(seq "$WARM_RUNS"); do
  warm_runs+=("$(launch)")
done
# Median of one column of the warm launches.
median() {
  printf '%s\n' "${warm_runs[@]}" | awk -v c="$1" '{ print $c }' | sort -n |
    awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'
}
warm="$(median 1) $(median 2) $(median 3)"

read -r cold_main cold_frame cold_ready <<< "$cold"
read -r warm_main warm_frame warm_ready <<< "$warm"
echo "page         ${page_kb} KB local file, 1280x800 at 2x"
echo "cold         main ${cold_main} ms, first frame ${cold_frame} ms, interactive ${cold_ready} ms (limit ${COLD_LIMIT_MS} ms)"
echo "warm         main ${warm_main} ms, first frame ${warm_frame} ms (limit ${FIRST_FRAME_LIMIT_MS} ms), interactive ${warm_ready} ms (limit ${INTERACTIVE_LIMIT_MS} ms), median of ${WARM_RUNS}"
awk -v cr="$cold_ready" -v wf="$warm_frame" -v wr="$warm_ready" -v lc="$COLD_LIMIT_MS" \
    -v lf="$FIRST_FRAME_LIMIT_MS" -v lr="$INTERACTIVE_LIMIT_MS" 'BEGIN {
  if (cr + 0 >= lc) { print "FAIL: cold launch too slow"; exit 1 }
  if (wf + 0 >= lf) { print "FAIL: warm first frame too slow"; exit 1 }
  if (wr + 0 >= lr) { print "FAIL: warm launch too slow to an interactive page"; exit 1 }
  print "PASS: within the startup budget"
}'
