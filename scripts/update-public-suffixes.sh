#!/usr/bin/env bash
# Rebuild crates/net/src/public_suffix_list.gz from publicsuffix.org (Mozilla Public License 2.0):
# rules only, one per line, internationalised labels in punycode, sorted, gzip -9.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sSfL -o "$tmp/list.dat" https://publicsuffix.org/list/public_suffix_list.dat
python3 -I - "$tmp/list.dat" "$tmp/rules.txt" <<'EOF'
import sys
rules = set()
for line in open(sys.argv[1], encoding="utf-8"):
    rule = line.split()[0] if line.split() else ""
    if not rule or rule.startswith("//"):
        continue
    bang = "!" if rule.startswith("!") else ""
    labels = []
    for label in rule.lstrip("!").split("."):
        if label == "*" or label.isascii():
            labels.append(label.lower())
        else:
            labels.append("xn--" + label.encode("punycode").decode("ascii"))
    rules.add(bang + ".".join(labels))
open(sys.argv[2], "w").write("\n".join(sorted(rules)) + "\n")
print(len(rules), "rules")
EOF
gzip -9 -n -c "$tmp/rules.txt" > crates/net/src/public_suffix_list.gz
ls -l crates/net/src/public_suffix_list.gz
