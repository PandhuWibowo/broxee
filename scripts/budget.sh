#!/usr/bin/env bash
# Fails when Broxee breaks its 60 MB budget: release binary size, or peak RAM on a stress page
# laid out and painted at 2x (a Retina frame), the way a window shows it.
set -euo pipefail

LIMIT_MB=60
cd "$(dirname "$0")/.."

cargo build --release --quiet
bin=target/release/broxee
bytes=$(wc -c < "$bin" | tr -d ' ')
size_mb=$(awk -v b="$bytes" 'BEGIN { printf "%.1f", b / 1048576 }')

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
page="$tmp/stress.html"
awk 'BEGIN {
  print "<!doctype html><html><head><title>Budget stress page</title><style>.row h2 { font-size: 20px } div > p a { color: #0369a1 } ul li { margin: 2px 0 } .r7 span { font-weight: bold } @media (min-width: 600px) { .row { padding: 4px } }</style></head><body>"
  for (i = 1; i <= 20000; i++)
    printf "<div class=\"row r%d\"><h2>Item %d</h2><p>Text &amp; more <a href=\"/p/%d\">link</a> <span>x</span></p><ul><li>a<li>b<li>c</ul></div>\n", i, i, i
  print "</body></html>"
}' > "$page"
page_kb=$(( $(wc -c < "$page" | tr -d ' ') / 1024 ))

ram_mb=$("$bin" "$page" --paint --scale 2 | awk '/^peak RAM/ { print $3 }')

echo "binary    ${size_mb} MB (limit ${LIMIT_MB} MB)"
echo "peak RAM  ${ram_mb} MB on a ${page_kb} KB stress page, painted at 2x (limit ${LIMIT_MB} MB)"
awk -v s="$size_mb" -v r="$ram_mb" -v l="$LIMIT_MB" 'BEGIN {
  if (r == "") { print "FAIL: could not read peak RAM"; exit 1 }
  if (s + 0 >= l || r + 0 >= l) { print "FAIL: over budget"; exit 1 }
  print "PASS: within budget"
}'
