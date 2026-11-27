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

- [`hooks_ab.py`](hooks_ab.py) runs a paired hooks-on/off comparison on your
  own questions: each question is asked in two fresh, non-persisted
  `claude -p` sessions that differ only in the workmap SessionStart and
  UserPromptSubmit hooks (Stop/SessionEnd are never installed, so nothing is
  ingested). Point `--state-dir` at a copy of the index, keep the dataset
  private, then run `settings`, `retrieval`, `run`, `score` and `report`
  (`examples/hooks_ab.py -h`).

`demo.sh` and `bench.sh` set `WORKMAP_HOME` and `WORKMAP_STATE_DIR`, so your
real home and index are never read or written.
