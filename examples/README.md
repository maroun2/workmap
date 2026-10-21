# Examples

Everything here is synthetic and reproducible. No real projects or transcripts.

- [`demo.sh`](demo.sh) builds a fake home under `/tmp/workmap-demo` with three
  small Git projects (`inkwell`, `inkcore`, `notes`), one Claude Code
  transcript and a fake authenticated `gh`, then runs the main commands.
  `examples/demo.sh target/release/workmap`
- [`demo-output.txt`](demo-output.txt) is the output of that script
  (`$DEMO` stands for the demo directory).
- [`bench.sh`](bench.sh) generates a larger synthetic home (repositories ×
  files, transcripts × messages; sizes via `REPOS`, `FILES`, `SESSIONS`,
  `MSGS`) and prints scan, search and context timings plus the index size.

Both scripts set `WORKMAP_HOME` and `WORKMAP_STATE_DIR`, so your real home and
index are never read or written.
