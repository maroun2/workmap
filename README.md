# workmap

A local, offline map of your Git repositories, worktrees, docs, commits and
coding-agent conversations (Claude Code, Codex, OpenCode).

Point it at a project and it lists what you would otherwise have to remember:
the README line that names a file, the sibling worktree with the same file,
the commit that last touched it, and the earlier agent conversation that
already read or wrote it. Every line of output carries its evidence (path,
line number, timestamp). workmap never summarises or interprets.

> Status: 0.1 baseline. See the repository history for the feature roadmap.

## Build

```sh
cargo build --release          # needs a C compiler (SQLite is bundled) and git at runtime
cargo test
```

With Nix: `nix-build -I nixpkgs=flake:nixpkgs` (see `default.nix`).

## Use

```sh
workmap scan                        # index $HOME (plus `workmap roots add DIR` roots)
workmap overview                    # every indexed project by name
workmap context myproject           # docs, key files, commits, conversations
workmap expand myproject:src/lib.rs # who read/wrote it, docs naming it, commits
workmap ask 'myproject:conversation:Fix the parser' 'What did you change?' --dry-run
```

`workmap --help` lists every command.

## Data and privacy

Everything stays on your machine. The index lives in
`~/.local/state/workmap/index.db` (override with `WORKMAP_STATE_DIR`). It holds
paths, metadata, short doc excerpts and evidence pointers, with
secret-looking tokens redacted. Delete that directory to remove it.

## License

MIT, see [LICENSE](LICENSE).
