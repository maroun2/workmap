#!/usr/bin/env bash
# Build a synthetic home with three small projects, agent transcripts and a
# fake `gh`, then run workmap against it. Nothing outside $DEMO is read or
# written. Usage: examples/demo.sh [path/to/workmap] [demo-dir]
set -euo pipefail
BIN=$(realpath "${1:-target/release/workmap}")
DEMO=${2:-/tmp/workmap-demo}
rm -rf "$DEMO"; mkdir -p "$DEMO"
H=$DEMO/home
export WORKMAP_HOME=$H WORKMAP_STATE_DIR=$DEMO/state WORKMAP_CMD=workmap
export GIT_AUTHOR_DATE=2026-09-01T12:00:00Z GIT_COMMITTER_DATE=2026-09-01T12:00:00Z
g() { git -C "$1" -c user.name=Demo -c user.email=demo@example.invalid -c init.defaultBranch=main -c commit.gpgsign=false "${@:2}"; }
mk() { mkdir -p "$(dirname "$1")"; cat > "$1"; }

# inkcore: a parsing library
P=$H/src/inkcore
mk $P/Cargo.toml <<'EOF'
[package]
name = "inkcore"
version = "0.3.0"
EOF
mk $P/README.md <<'EOF'
# inkcore

Markdown tokenizer shared by the inkwell tools.

## Layout
- `src/lexer.rs` turns text into tokens.
- `src/retry.rs` wraps flaky file reads.

Used by ../inkwell/README.md.
EOF
mk $P/src/lexer.rs <<'EOF'
pub fn lex(s: &str) -> Vec<&str> { s.split_whitespace().collect() }
EOF
mk $P/src/retry.rs <<'EOF'
pub fn retry<T>(f: impl Fn() -> Option<T>) -> Option<T> { (0..3).find_map(|_| f()) }
EOF
g $P init -q; g $P remote add origin git@github.com:acme/inkcore.git; g $P add -A; g $P commit -qm "Lexer and retry helper"

# inkwell: a CLI depending on inkcore by path, with a feature worktree
P=$H/src/inkwell
mk $P/Cargo.toml <<'EOF'
[package]
name = "inkwell"
version = "1.0.0"

[dependencies]
inkcore = { path = "../inkcore" }
clap = "4"
EOF
mk $P/README.md <<'EOF'
# inkwell

Command-line Markdown checker built on https://github.com/acme/inkcore.

## Commands
- `inkwell check` runs `src/check.rs` over every file.
- Exit codes are documented in docs/exit-codes.md.
EOF
mk $P/docs/exit-codes.md <<'EOF'
# Exit codes

## Codes
0 ok, 1 findings, 2 the parser timed out on a huge file.
EOF
mk $P/src/main.rs <<'EOF'
mod check;
fn main() { check::run(); }
EOF
mk $P/src/check.rs <<'EOF'
pub fn run() { println!("ok"); }
EOF
g $P init -q; g $P remote add origin https://github.com/acme/inkwell.git; g $P add -A; g $P commit -qm "check command and exit codes"
g $P worktree add -q -b timeout-fix "$H/src/inkwell-timeout"

# notes: a docs-only repo that links both
P=$H/src/notes
mk $P/README.md <<'EOF'
# notes

Release checklist for /home/demo/src/inkwell and inkcore.
See https://github.com/acme/inkwell/blob/main/README.md before tagging.
EOF
g $P init -q; g $P add -A; g $P commit -qm "release notes"

# A Claude Code conversation in inkwell that read check.rs and edited inkcore's retry.rs
IW=$(realpath "$H/src/inkwell"); IC=$(realpath "$H/src/inkcore")
D=$H/.claude/projects/$(printf '%s' "$IW" | sed 's/[^A-Za-z0-9]/-/g')
mk $D/5f2c9a10-demo.jsonl <<EOF
{"type":"summary","summary":"Parser timeout on huge files"}
{"type":"user","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:00:00Z","message":{"content":"inkwell check hangs on a 40 MB file, why?"}}
{"type":"assistant","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:00:05Z","message":{"content":[{"type":"text","text":"Let me read the check command first."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"$IW/src/check.rs"}}]}}
{"type":"user","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:00:06Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"pub fn run() ..."}]}}
{"type":"assistant","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:01:00Z","message":{"content":[{"type":"text","text":"The hang is the retry helper in inkcore: it retries a timed-out read three times without a deadline. I capped the total wait."},{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"$IC/src/retry.rs"}}]}}
{"type":"user","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:01:02Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}
{"type":"user","cwd":"$IW","sessionId":"5f2c9a10-demo","timestamp":"2026-09-10T09:05:00Z","message":{"content":"ship it after the release checklist"}}
EOF

# A fake, already-authenticated gh that lists one cloned and one not-cloned repository.
mk $H/.config/gh/hosts.yml <<'EOF'
github.com:
    user: demo
EOF
mk $DEMO/bin/gh <<'EOF'
#!/bin/sh
case "$1" in
  auth) exit 0;;
  api) echo '[{"full_name":"acme/inkcore","html_url":"https://github.com/acme/inkcore","private":false,"archived":false,"default_branch":"main","updated_at":"2026-09-02T00:00:00Z","description":"Markdown tokenizer"}]'
       echo '[{"full_name":"acme/inkwell-vscode","html_url":"https://github.com/acme/inkwell-vscode","private":false,"archived":false,"default_branch":"main","updated_at":"2026-08-20T00:00:00Z","description":"Editor extension for inkwell"}]';;
esac
EOF
chmod +x $DEMO/bin/gh
export WORKMAP_GH_BIN=$DEMO/bin/gh

# The not-cloned acme/inkwell-vscode, served from a local bare repo through a
# demo-only Git config (GIT_CONFIG_GLOBAL), for the opt-in snapshot step.
R=$DEMO/remotes/acme/inkwell-vscode.git; W=$DEMO/work-vscode
mk $W/README.md <<'EOF'
# inkwell-vscode

## Diagnostics
Shows inkwell findings inline; the language server shells out to `inkwell check --json`.
EOF
mk $W/package.json <<'EOF'
{ "name": "inkwell-vscode", "dependencies": { "inkcore": "github:acme/inkcore" } }
EOF
g $W init -q; g $W add -A; g $W commit -qm "diagnostics"
git init -q --bare -b main "$R"; g $W push -q "$R" HEAD:main
printf '[url "file://%s/remotes/"]\n\tinsteadOf = https://github.com/\n' "$DEMO" > $DEMO/gitconfig
export GIT_CONFIG_GLOBAL=$DEMO/gitconfig

run() { echo "\$ workmap $(printf '%q ' "$@")"; "$BIN" "$@" | sed "s|$DEMO|\$DEMO|g"; echo; }
run scan --quiet
run overview
run context inkwell
run expand inkwell:session:5f2c9a10
run context inkwell --detailed
run search timeout
run search retry deadline --project inkwell --kind message
run expand inkwell:message:5f2c9a10:l5
run related inkwell
run why notes inkcore
run remote snapshots enable
run scan --quiet
run search language server
run expand remote:github.com/acme/inkwell-vscode:README.md#diagnostics
run context inkwell --task 'parser timeout on huge files' --file src/check.rs --budget 2500
run status
