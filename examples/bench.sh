#!/usr/bin/env bash
# Synthetic scale check: REPOS git repos with FILES files each and SESSIONS
# Claude-style transcripts of MSGS messages. Prints first/repeat scan times,
# index size and search latency. Usage:
#   REPOS=40 FILES=60 SESSIONS=200 MSGS=100 examples/bench.sh target/release/workmap
set -euo pipefail
BIN=$(realpath "${1:-target/release/workmap}")
DIR=${2:-/tmp/workmap-bench}
REPOS=${REPOS:-40} FILES=${FILES:-60} SESSIONS=${SESSIONS:-200} MSGS=${MSGS:-100}
rm -rf "$DIR"; mkdir -p "$DIR"
H=$DIR/home
export WORKMAP_HOME=$H WORKMAP_STATE_DIR=$DIR/state
export GIT_AUTHOR_DATE=2026-09-01T12:00:00Z GIT_COMMITTER_DATE=2026-09-01T12:00:00Z
for r in $(seq 1 "$REPOS"); do
  P=$H/src/proj$r; mkdir -p "$P/src" "$P/docs"
  printf '[package]\nname = "proj%s"\n\n[dependencies]\nproj%s = { path = "../proj%s" }\n' "$r" $(( r % REPOS + 1 )) $(( r % REPOS + 1 )) > "$P/Cargo.toml"
  printf '# proj%s\n\nService number %s. See `src/f1.rs` and docs/guide.md.\n\n## Usage\nRun it.\n' "$r" "$r" > "$P/README.md"
  printf '# Guide\n\n## Deploy\nDeploy proj%s with the blue-green checklist.\n' "$r" > "$P/docs/guide.md"
  for f in $(seq 1 "$FILES"); do printf 'pub fn f%s() -> u32 { %s }\n' "$f" "$r$f" > "$P/src/f$f.rs"; done
  git -C "$P" -c init.defaultBranch=main init -q
  git -C "$P" add -A
  git -C "$P" -c user.name=B -c user.email=b@example.invalid commit -qm "proj$r initial"
done
for s in $(seq 1 "$SESSIONS"); do
  r=$(( s % REPOS + 1 )); P=$(realpath "$H/src/proj$r")
  D=$H/.claude/projects/$(printf '%s' "$P" | sed 's/[^A-Za-z0-9]/-/g'); mkdir -p "$D"
  F=$D/sess-$s.jsonl
  {
    printf '{"type":"summary","summary":"Session %s on proj%s"}\n' "$s" "$r"
    for m in $(seq 1 "$MSGS"); do
      printf '{"type":"user","cwd":"%s","sessionId":"sess-%s","timestamp":"2026-09-02T10:%02d:00Z","message":{"content":"question %s about module f%s and the deploy checklist"}}\n' "$P" "$s" $(( m % 60 )) "$m" "$m"
      printf '{"type":"assistant","cwd":"%s","sessionId":"sess-%s","timestamp":"2026-09-02T10:%02d:01Z","message":{"content":[{"type":"text","text":"answer %s: f%s returns a constant; token%s%s appears only here."},{"type":"tool_use","id":"t%s","name":"Read","input":{"file_path":"%s/src/f1.rs"}}]}}\n' "$P" "$s" $(( m % 60 )) "$m" "$m" "$s" "$m" "$m" "$P"
    done
  } > "$F"
done
ms() { date +%s%3N; }
t0=$(ms); "$BIN" scan --quiet --no-remote --no-refresh; t1=$(ms)
"$BIN" scan --quiet --no-remote --no-refresh; t2=$(ms)
"$BIN" search token77 --limit 5 >/dev/null; t3=$(ms)
"$BIN" search deploy checklist --project proj3 --limit 10 >/dev/null; t4=$(ms)
"$BIN" context proj3 --task 'deploy checklist' --file src/f1.rs >/dev/null; t5=$(ms)
"$BIN" related proj3 --depth 3 >/dev/null; t6=$(ms)
size=$(du -b "$DIR/state/index.db" | cut -f1)
msgs=$("$BIN" status | awk '/^  message:/{print $2}')
src=$(du -sb "$H/.claude" | cut -f1)
echo "repos=$REPOS files/repo=$FILES sessions=$SESSIONS messages/session=$((MSGS*2)) transcript_bytes=$src"
echo "first scan: $((t1-t0)) ms; repeat scan: $((t2-t1)) ms; index: $size bytes; message passages: $msgs"
echo "search global: $((t3-t2)) ms; search scoped: $((t4-t3)) ms; context --task --file: $((t5-t4)) ms; related --depth 3: $((t6-t5)) ms"
