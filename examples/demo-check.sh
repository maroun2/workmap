#!/usr/bin/env bash
# Run demo.sh and compare its output with demo-output.txt, ignoring only what
# changes on every run: scan/fetch times, scan duration and index size.
# Usage: examples/demo-check.sh [path/to/workmap] [--update]
# The demo runs in /tmp/workmap-demo (demo.sh's default): budget lines depend on
# the length of that path, so the golden output is only valid there.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=target/release/workmap UPDATE=
for a in "$@"; do
  case $a in
    --update) UPDATE=1 ;;
    *) BIN=$a ;;
  esac
done
DEMO=/tmp/workmap-demo
OUT=$(mktemp)
trap 'rm -rf "$OUT" "$DEMO"' EXIT
bash examples/demo.sh "$BIN" "$DEMO" > "$OUT"
if [ -n "$UPDATE" ]; then
  cp "$OUT" examples/demo-output.txt
  echo "updated examples/demo-output.txt"
  exit 0
fi
TS='[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}Z'
norm() {
  sed -E \
    -e "s/last scan $TS/last scan <T>/g" \
    -e "s/$TS \([0-9]+[mhd] ago\)/<T> (<N> ago)/g" \
    -e "s/repositories from $TS; last check $TS/repositories from <T>; last check <T>/g" \
    -e "s/fetched $TS/fetched <T>/g" \
    -e "s/indexed $TS from/indexed <T> from/g" \
    -e 's/"ms":[0-9]+/"ms":<N>/g' \
    -e 's/index\.db \([0-9]+ bytes\)/index.db (<N> bytes)/g' \
    "$1"
}
if diff -u <(norm examples/demo-output.txt) <(norm "$OUT"); then
  echo "demo output matches examples/demo-output.txt"
else
  echo "demo output changed; if intended, run: examples/demo-check.sh $BIN --update" >&2
  exit 1
fi
