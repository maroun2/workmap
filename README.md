# workmap

**A local, offline map of everything you and your coding agents have already
done: Git repositories, worktrees, docs, commits, and Claude Code / Codex /
OpenCode conversations, linked by evidence.**

You open a project and an agent starts from zero. Yet somewhere on your disk is
the README line that names the file you are about to change, a sibling worktree
where the same file already looks different, the commit that last touched it,
the conversation from two weeks ago that read it and found the bug, and the
library next door that this project depends on by path. workmap finds those
and shows each one with the exact file, line and timestamp it came from.

- **Deterministic.** No LLM indexing, no embeddings, no summaries. Every line of
  output is a fact with a source (`path:line`, transcript line, commit).
- **Local and offline.** One SQLite file on your machine. The only network
  access is optional: listing repositories you can reach through an
  already-authenticated `gh` or `glab`, and you can turn that off.
- **Agent-friendly.** Short, copy-pasteable references (`:src/lib.rs`,
  `:commit:5ae2bbc7`, `:message:5f2c9a10:l5`) and a bounded `--budget` mode for
  feeding context to an agent.

## A walk through it

The output below is real, from [`examples/demo.sh`](examples/demo.sh), which
builds three synthetic projects (`inkwell`, a CLI; `inkcore`, its library;
`notes`, a docs repo), one Claude Code conversation and a fake `gh`. The full
run is in [`examples/demo-output.txt`](examples/demo-output.txt).

**1. What exists?** (excerpts below are trimmed; the full output is in the file above)

```
$ workmap overview
workmap: 3 projects, 1 agent conversations; last scan 2026-10-21 06:26Z
- inkcore — 1 worktree(s), 0 conversation(s), last commit 2026-09-01
- inkwell — 2 worktree(s), 1 conversation(s), last commit 2026-09-01, last conversation 2026-09-10
- notes — 1 worktree(s), 0 conversation(s), last commit 2026-09-01
Remote-only, not cloned (1; metadata from authenticated gh/glab; expand remote:<name>):
- remote:github.com/acme/inkwell-vscode — public — Editor extension for inkwell
```

**2. One project at a glance.** A quoted purpose line with its source, then
a typed, directed map grouped by source node: each conversation, doc, commit or
project printed once with its name and reference, followed by short target
lists. Only confirmed edges appear; mentions and requested/failed accesses are
counted and listed with `--weak`. `--detailed` prints the longer sections view.

```
$ workmap context inkwell
# inkwell — map · main @ $DEMO/home/src/inkwell · HEAD 36d628cc · 0 modified · last commit 2026-09-01
Purpose (:README.md l.3): "Command-line Markdown checker built on https://github.com/acme/inkcore."
Scope: export WORKMAP_PROJECT=inkwell WORKMAP_STATE_DIR=$DEMO/state WORKMAP_HOME=$DEMO/home — `:` refs are relative to it; other worktrees/projects are fully qualified
Worktrees (2): main*, timeout-fix
Selected (confirmed edges only): docs 2/2 · files 3/3 · conversations 1/1 · commits 1/1 · projects 2/2 · weak edges hidden: 0
Types (A type → B: stored edge from A to B; ← from B to A; +N: more targets, see expand A): read/write = tool result recorded; ref = doc names the path/repository; changes = commit changed the file; history = commit indexed on this branch; dependency = manifest/submodule declares it; co-use = a conversation used files of both projects (not a dependency)

inkwell (this project)
  ref, dependency, co-use → :related:inkcore
  ref ← :related:inkcore, :related:notes
worktree main
  history → :commit:36d628cc
"Parser timeout on huge files" :session:5f2c9a10 (claude@main 2026-09-10)
  read → :src/check.rs
  write → inkcore:src/retry.rs
:README.md
  ref → :docs/exit-codes.md, :src/check.rs
:commit:36d628cc "check command and exit codes" (2026-09-01)
  changes → :README.md, :docs/exit-codes.md, :src/check.rs, :src/main.rs, :Cargo.toml

Next: workmap expand REF (evidence, details) · workmap search 'words' · workmap context --detailed (sections view)
```

Every reference expands to its details and evidence:

```
$ workmap expand inkwell:session:5f2c9a10
# ':conversation:Parser timeout' — claude conversation in inkwell @main
title "Parser timeout on huge files"
session 5f2c9a10-demo · 2026-09-10 09:00Z → 2026-09-10 09:05Z · cwd $DEMO/home/src/inkwell
transcript $DEMO/home/.claude/projects/-tmp-workmap-demo-home-src-inkwell/5f2c9a10-demo.jsonl
first user input 2026-09-10 09:00Z l.2: "inkwell check hangs on a 40 MB file, why?"
latest user input (as typed; not a decision record) 2026-09-10 09:05Z l.7: "ship it after the release checklist"
access: r1 w1
coverage: 4 searchable message(s)
messages: :message:5f2c9a10:l2 (user), :message:5f2c9a10:l3 (assistant), :message:5f2c9a10:l5 (assistant) … (search inside: search 'words' --kind message)
…
```

**3. Search what was said, not just what was committed.** Individual messages
from agent transcripts are searchable, with speaker, time and transcript line:

```
$ workmap search retry deadline --project inkwell --kind message
1. :message:5f2c9a10:l5 — claude assistant message · 2026-09-10 09:01Z · conversation "Parser timeout on huge files"
   "The hang is the [retry] helper in inkcore: it retries a timed-out read…"

$ workmap expand inkwell:message:5f2c9a10:l5
# :message:5f2c9a10:l5 — assistant message in claude conversation ':conversation:Parser timeout'
2026-09-10 09:01Z · transcript …/5f2c9a10-demo.jsonl line 5 · historical text, not a decision record
The hang is the retry helper in inkcore: it retries a timed-out read three times without a deadline. I capped the total wait.
Previous: :message:5f2c9a10:l3 assistant: "Let me read the check command first."
Next: :message:5f2c9a10:l7 user: "ship it after the release checklist"
```

**4. Why are two projects connected?**

```
$ workmap why notes inkcore
1. notes → inkwell (1 piece(s) of evidence)
   → doc names its URL or path [documented] at …/notes/README.md:4: "See https://github.com/acme/inkwell/blob/main/README.md before tagging."
2. inkwell → inkcore (4 piece(s) of evidence)
   → Cargo path dependency [declared] at …/inkwell/Cargo.toml:6: "inkcore = { path = "../inkcore" }"
```

**5. Context for a task, bounded.** Give the task and the files you will touch;
get a ranked list where every item says why it is there, cut at a byte budget:

```
$ workmap context inkwell --task 'parser timeout on huge files' --file src/check.rs --budget 2500
- ':conversation:Parser timeout' [90] — read src/check.rs (observed, Read) at transcript l.3, 2026-09-10
- :README.md#commands [40] — names src/check.rs at README.md:6
- :commit:36d628cc [30] — changed src/check.rs (A) on 2026-09-01
- :docs/exit-codes.md#codes [19] — text matches 'parser' 'huge' in doc docs/exit-codes.md § Codes l.3
- :related:inkcore [12] — related project: → Cargo path dependency [declared] at …/Cargo.toml:6
```

**6. Ask the conversation that already did the work** (read-only; see
[ask](#ask-read-only-follow-up-questions)):

```
$ workmap ask --project inkwell ':conversation:Parser timeout' 'Which deadline did you pick?' --dry-run
```

## Quick start

```sh
git clone https://github.com/maroun2/workmap && cd workmap
cargo build --release                 # Rust 1.80+, a C compiler (SQLite is bundled)
./target/release/workmap scan         # index $HOME; first run on a large home takes minutes
./target/release/workmap overview
```

Projects outside `$HOME`, or under skipped directories such as `~/.config` or
`tmp`: `workmap roots add /path/to/projects && workmap scan`.

Try it without touching your own data: `examples/demo.sh target/release/workmap`
(writes only under `/tmp/workmap-demo`).

### Nix / NixOS

```sh
nix-build -I nixpkgs=flake:nixpkgs      # result/bin/workmap; runs the test suite in the sandbox
nix-build -I nixpkgs=flake:nixpkgs --argstr rev "$(git rev-parse --short=12 HEAD)"   # same, with the revision in --version
```

`workmap --version` prints `<version> (rev <git revision>[-dirty]; <profile>; <target>)`. Cargo builds
from a checkout read the revision from Git; the Nix source has no `.git`, so it comes from the
`rev` argument (`WORKMAP_BUILD_REV`), else `unknown`.

In a NixOS or home-manager configuration, `import /path/to/workmap { inherit pkgs; }`
gives you the package (`default.nix` is a plain `rustPlatform.buildRustPackage`),
e.g. `home.packages = [ (import ./workmap { inherit pkgs; }) ];`.

### Install for agents (optional)

```sh
workmap install          # copies the binary, writes skills and a daily systemd --user timer
workmap uninstall        # removes them again; the index is kept until you delete it
```

`install` copies the binary to `~/.local/share/workmap/bin/workmap` and writes a
`workmap` skill (binary + `SKILL.md`) into `~/.claude/skills`,
`~/.codex/skills` and `~/.config/opencode/skills`. The skill text is generated
from your index: commands, registered roots, and a bounded project overview
(≈24 KiB) that includes remote-only repositories by name. Every `workmap scan`
refreshes skill files that workmap generated (marked with an HTML comment) and
nothing else. It refuses to write through a symlinked skills directory or into
a Git checkout, and never uses sudo. The timer runs `workmap scan --quiet`
daily (`Persistent=true`). It is enabled only for your real home with a user
session bus. `--no-activate` writes the units without enabling them.

## Commands

| Command | What it shows |
|---|---|
| `workmap overview` | Every project by name, plus remote-only repositories |
| `workmap context [P] [--budget BYTES] [--weak]` | Compact map: a quoted purpose line (doc path + line), then typed, directed connections grouped by source node: conversations (title + `:session:` ref), docs, commits, worktree history, related projects. Confirmed edges only; weak ones are counted, listed with `--weak`. Whole groups of lines within `--budget` (default 4000 bytes); the header and the `Next:` line always print |
| `workmap context [P] --detailed` | The sections view: worktrees, docs, key files with reasons, commits, conversations with access codes, related projects, index coverage (also `workmap expand P`) |
| `workmap context [P] --task '…' --file PATH… [--budget BYTES]` | Ranked, budget-bounded context; every item has a reason |
| `workmap search WORDS… [--project P [--worktree W] [--here]] [--kind K] [--speaker user\|assistant] [--provider claude\|codex\|opencode] [--since DATE] [--any] [--exact] [--explain]` | Full-text search over project metadata, doc sections, commits, conversation messages and remote catalog/snapshot entries. Tiers, each hit labelled: `[exact]` tokens, then `[prefix]`, then `[stem]` (Porter), `[substring]` for sha/path/identifier-shaped queries; `[any]` only with `--any`. No synonyms. Zero/few hits print what was searched, coverage and runnable commands that widen the scope |
| `workmap expand REF` | One file, doc section, commit, conversation, message or relation, with everything linked to it |
| `workmap related [P] [--depth N]` | Related projects (breadth-first, bounded, cycle-safe) with evidence |
| `workmap why P OTHER [--depth N]` | Shortest evidence chain between two projects, each step quoted |
| `workmap ask REF 'question' [--dry-run]` | Read-only follow-up in a stored Claude Code or Codex conversation |
| `workmap resume REF [--cwd DIR] [--dry-run]` | Reopen the original conversation interactively in its own CLI and recorded directory |
| `workmap hooks status\|install\|uninstall [--harness claude\|codex] [--scope user\|project] [--dry-run]` | Opt-in host hooks: context at session start and per prompt, ingest when a session stops |
| `workmap scan [--root DIR] [--full] [--no-sessions] [--no-remote] [--refresh-remote]` | Incremental re-index |
| `workmap ingest --transcript FILE… [--checkout DIR…] [--quiet]` | Index only these transcripts/checkouts now, in one transaction (milliseconds; no home walk, no pruning, no remote sync). OpenCode: `--transcript '<opencode.db>#session=<id>'`. The daily timer's full scan stays the backstop |
| `workmap roots add DIR` / `roots list` | Extra roots to scan, persisted |
| `workmap remote status\|list\|refresh\|enable\|disable` | The gh/glab catalog |
| `workmap remote snapshots status\|enable\|disable\|set KEY VALUE` | Opt-in shallow snapshots of remote-only repositories |
| `workmap status` | Counts, freshness, transcript coverage per provider, remote host status |
| `workmap hook EVENT --harness claude\|codex` | Used by installed hooks: reads the host's event JSON on stdin, prints bounded context or nothing, always exits 0 |
| `workmap --version` | Version, git revision (or `WORKMAP_BUILD_REV`, else `unknown`) and build profile |

### References

A reference is `<project>:<rest>`, or `:<rest>` once a scope is set with
`--project P` or `export WORKMAP_PROJECT=P` (`WORKMAP_WORKTREE` / `--worktree`
picks a non-main worktree). There is no saved "current project": concurrent
shells and agents never affect each other.

| `<rest>` | Names |
|---|---|
| `src/lib.rs` | a file or doc |
| `README.md#install` | one doc section (GitHub-style heading slug) |
| `commit:5ae2bbc7` | a commit (≥ 4 hex chars) |
| `conversation:Fix the parser` | a conversation by exact title or unique title prefix |
| `session:019ff26f` | a conversation by the provider's session id prefix |
| `message:5f2c9a10:l5` | one message: session id prefix + transcript line (`l…`) or part number (`p…`, OpenCode) |
| `related:inkcore` | all relation evidence between the scope project and another |

`remote:github.com/acme/tool` names a repository you can access but have not
cloned; `remote:github.com/acme/tool:README.md#usage` names a doc section of
its [snapshot](#remote-snapshots-opt-in) at the indexed commit.

A project whose name is not unique (e.g. two clones of one remote) gets a
stable compact handle `<name>~<hex>` derived from its git directory, e.g.
`ledger-api~67ba`; its checkout path works too. Bare ambiguous names fail with
the handles listed; nothing is fuzzy-picked.

References that point into another worktree or project are printed fully
qualified, `<handle>[@<worktree>]:<rest>` (e.g.
`ledger-api~67ba@fix/webhook-retry:ledger/webhooks.py`), and resolve without
any scope or environment.

## What gets indexed

| Source | What is kept |
|---|---|
| Git checkouts under `$HOME` and your roots | repository identity (Git common dir), all worktrees, remotes (credentials stripped), branches, the newest 30 commits per worktree (`scan --commits N`) with changed files |
| Docs: `README*`, `CLAUDE.md`, `AGENTS.md`, `docs/*.md`, top-level `*.md`, Claude Code project memory | headings, short excerpts with line numbers, links to files, doc sections as searchable text (≤ 80 passages per doc) |
| Manifests: `Cargo.toml`, `package.json`, `pyproject.toml`, `.gitmodules` | local path and Git dependencies, declared package names |
| Claude Code `~/.claude/projects/*/*.jsonl` | file reads/writes from structured tool calls with their results, user and assistant text messages; subagent transcripts (`<session>/subagents/agent-*.jsonl`) as child conversations linked to their parent |
| Codex `~/.codex/sessions/**/rollout-*.jsonl` | `apply_patch` writes, shell commands Codex itself parsed as read/search/list (`parsed_cmd`, with the command's exit status), shell mentions, user and assistant messages (both rollout formats) |
| OpenCode `opencode.db` (read-only) and legacy JSON storage | file tools and patches, user and assistant text parts |
| `gh` / `glab` (optional) | names and metadata of repositories you can access |

Tool calls, tool output, reasoning blocks and harness-injected context
(`<system-reminder>`, `<environment_context>`, AGENTS.md preambles) are never
indexed as text. A message is cut at 4,800 characters, a conversation at 5,000
messages or 2 MiB of text, and a transcript is read up to 512 MiB; each cut is
reported as coverage, not hidden.

## Evidence, not interpretation

Every relation keeps its kind, its strength class and where it came from.
They are never merged into a vague "related" score.

| Relation | Class | Evidence |
|---|---|---|
| Cargo / package.json / pyproject path or Git dependency, Git submodule | `declared` | manifest file and line |
| Symlink pointing into another project | `structural` | the symlink |
| A doc names another project's URL or path | `documented` | doc file and line |
| Separate clones of the same remote (`host/namespace/repo`) | `identity` | the remotes |
| A conversation running in A read or wrote files of B | `observed` | transcript line; co-use, **not** a dependency |
| Identical substantive files (Git blob id, 1 KiB – 4 MiB, licences, lockfiles, build output and vendored trees excluded, blobs shared by > 5 projects ignored) | `hash` | example file; shared content, **not** lineage |
| A registry dependency has the same name as a package declared in another project | `candidate` | shown separately as UNVERIFIED, never used in `why` chains |

Worktrees of one repository (same Git common dir) are the same project, not
related projects. Remote and local copies are matched by normalized
`host/namespace/repo`, so a repository you cloned is not listed again as
remote-only.

The same discipline applies elsewhere:

- File access counts only from structured tool calls. A path inside a shell
  command is a *mention* (`sh`), shown but never ranked. A tool call without a
  recorded result is *requested*, not observed.
- A bare filename in a doc that happens to match one file is an unverified
  candidate, not a link.
- Conversation text is historical. The latest user input of a conversation is
  shown as "as typed; not a decision record". `ask` prints what changed on disk
  since the conversation ended.

## Freshness and coverage

Views that make claims print where those claims stop:

```
Index: last scan 2026-10-21 06:26Z (3h ago) · history: newest 30 commits per worktree
Coverage: 12 conversation(s), 840 searchable message(s); 1 truncated at the size cap, 2 source transcript missing (evidence kept)
WARNING: index is stale (5d ago > 48h); run `workmap scan` — recent commits, docs and conversations may be missing
```

The stale threshold is 48 hours (`WORKMAP_STALE_HOURS`). Transcripts deleted
after indexing keep their evidence and text, flagged as missing. Checkouts
deleted from disk are pruned on the next scan, but only when their parent
directory still exists, so an unmounted drive or a missing root never empties
the index. `workmap status` breaks coverage
down per provider (parsed, unrelated, missing, error).

## Remote discovery (gh / glab)

When `gh` or `glab` is already logged in, `scan` lists the repositories you can
access on each configured host, including self-hosted GitHub Enterprise and
GitLab hosts from their config files (`~/.config/gh/hosts.yml`,
`~/.config/glab-cli/config.yml`):

- **Scope.** GitHub: `user/repos?affiliation=owner,collaborator,organization_member`.
  GitLab: `projects?membership=true`. Repositories you own or are a member of,
  not every public repository.
- **Metadata only by default.** Host, namespace, name, URL, description,
  visibility, archived flag, default branch, last update. Nothing is cloned or
  fetched unless you opt in to [remote snapshots](#remote-snapshots-opt-in).
- **No prompts, no secrets.** workmap runs `gh auth status --hostname H` and
  `gh api --hostname H --paginate …` (same for `glab`) with prompts disabled and
  stdin closed. It never asks for a token, never runs `gh auth token`, never
  reads or stores the CLIs' stderr, and reports failures as fixed messages
  (`exit code 1`, `timed out after 180s`).
- **Failure isolation and caching.** Each host is refreshed at most once per
  24 hours (`WORKMAP_REMOTE_TTL_HOURS`). A host that fails (offline, rate
  limit, logged out) keeps its previous entries, marked stale. Other hosts are
  unaffected. No configured or authenticated host means remote discovery is
  skipped silently.
- **Opt out.** `workmap remote disable` (also deletes cached remote
  metadata), `WORKMAP_REMOTE=0`, or `scan --no-remote` for one run.
  `workmap remote refresh` forces a refresh.

Remote metadata and snapshot text, private repositories included, stay in
your local index and your generated skill files. Nothing is sent anywhere.

The daily systemd timer runs with the unit's `PATH`, which may not contain
`gh`/`glab`; it then reports them as "not installed" and keeps the cached
catalog. Set `WORKMAP_GH_BIN` / `WORKMAP_GLAB_BIN` before `workmap install` to
pin the paths into the unit. Under `WORKMAP_HOME` the CLIs get the fake
home's config dirs and no `GH_TOKEN`/`GITLAB_TOKEN`-style variables, so a test
or demo run never reaches your real accounts.

## Remote snapshots (opt-in)

Metadata says a repository exists; a snapshot says what is in it. With
snapshots enabled, `scan` takes a temporary shallow clone of each in-scope
remote-only repository, indexes it like a local main checkout, and deletes
the clone:

```sh
workmap remote snapshots enable                         # persisted in ~/.local/state/workmap/config
workmap remote snapshots set snapshot_include 'acme/*'  # optional scope
workmap scan                                            # or one run: scan --snapshots / --no-snapshots
workmap search language server
workmap expand remote:github.com/acme/inkwell-vscode:README.md#diagnostics
```

```
1. remote:github.com/acme/inkwell-vscode:README.md#diagnostics — doc README.md § Diagnostics l.3 in REMOTE SNAPSHOT at 3873a6aa1e82 (not on local disk)
```

- **Cached per commit.** workmap resolves the branch HEAD with `gh api` /
  `glab api` (falling back to `git ls-remote`) and stores one row per
  repository: branch, commit, indexer version. Unchanged HEAD: nothing is
  cloned. Moved HEAD or a newer workmap indexer: re-snapshot. Only one
  branch is indexed (the default branch, or `snapshot_branch`), so work on
  other branches is not seen.
- **What is kept.** Doc sections, the head commit's subject, URL-based
  manifest dependencies (Git deps, submodule URLs), declared package names,
  doc links to other repositories, and Git blob ids for identical-file
  detection. All of it is stored under virtual references tied to the commit
  (`remote:<host/ns/repo>:<path>#<section>`), and `expand` returns the cached
  text plus a browser link at that commit. No temp path and no working tree is
  stored. Local `path = "../x"` dependencies and symlinks of a remote checkout
  say nothing about your disk and are ignored.
- **Passive.** `git clone --depth 1 --single-branch --no-checkout --no-tags
  --no-recurse-submodules --template= --filter=blob:limit=512k` with
  `core.hooksPath=/dev/null`; files are read with `git cat-file`, so nothing
  from the repository is executed, installed or checked out, and no symlink is
  followed. Lazy blob fetches are disabled (`GIT_NO_LAZY_FETCH=1`).
- **Credentials.** The clone uses an `https://host/ns/repo.git` URL without
  credentials. Authentication comes from your Git credential helpers, plus
  `gh auth git-credential` for gh hosts. Tokens never appear in URLs,
  arguments, logs or the index. For glab hosts, configure Git credentials
  (for example via glab's own setup) or only public repositories can be read.
- **Bounds and failures.** Per clone: `snapshot_max_kb` (default 200 MiB; the
  clone is killed when its directory grows past it) and
  `snapshot_timeout_secs` (default 120). At most `snapshot_max_repos` (default
  10) new or changed snapshots per scan; the rest are deferred. A failed
  snapshot records its error (`workmap remote snapshots status`) and keeps the
  previously indexed commit's data. Temp clones live in
  `<state>/snapshots-tmp/` and are removed on success, failure, or on the next
  scan after an interrupted run.
- **Off again.** `workmap remote snapshots disable` stops them and deletes
  snapshot data; `workmap remote disable` turns off all remote access.

Snapshots contain no conversations: `ask` only resumes conversations that
exist as native transcripts on this machine.

## `ask`: read-only follow-up questions

`ask` resumes a stored conversation with the provider's own CLI, restricted to
reading. Before anything runs, workmap checks that the installed CLI's `--help`
documents every restricting flag, and refuses otherwise. `--dry-run` prints the
exact command, working directory and why each flag is there.

- **Claude Code:** `claude -p --resume ID --fork-session --no-session-persistence --tools Read,Grep,Glob --disallowedTools Bash,Edit,Write,… --permission-mode dontAsk --strict-mcp-config` with an empty MCP config and hooks disabled. The original transcript is not appended to.
- **Codex:** `codex exec resume --ignore-user-config --ephemeral -c sandbox_mode="read-only" -c approval_policy="never" -c mcp_servers={} ID`.
- **OpenCode:** refused. `opencode run --session` has no read-only mode workmap
  can verify, so it fails closed.

The resumed agent remembers its transcript, not today's files. `ask` prints
how many commits landed since the conversation ended, how many of the files it
wrote have changed or vanished since, and how many tracked files are modified
now.

## `resume`: continue the original session

`resume` is the writable counterpart of `ask`, and only you start it. It
reopens the recorded session interactively with the provider's own resume
command, in the directory the session ran in. That appends to the original
transcript. The syntax is checked against the installed CLI's `--help` before
anything runs, and the command is executed as argv, never through a shell.
`--dry-run` prints the exact command.

- **Claude Code:** `claude --resume ID`. Claude Code ties sessions to the
  directory they ran in, so this runs in the recorded cwd.
- **Codex:** `codex resume ID`.
- **OpenCode:** `opencode DIR --session ID`. Conversations indexed from
  OpenCode's legacy JSON storage are refused, because their ids can't be
  verified against the current database.
- **Subagent transcripts** are refused, and the error names the parent
  session to resume instead.

If the recorded directory is gone, `resume` refuses. It lists the indexed
worktrees still on the recorded branch (`--cwd <path>`) and the `git worktree
add <old path> <branch>` command that recreates the original. If `--cwd`
differs from the recorded directory, it first prints the recorded and current
cwd, branch and HEAD side by side.

## Host hooks (opt-in)

`workmap hooks install` adds four hooks to a host's settings. They run only
local, indexed SQLite queries: no scan, network or model. They always exit 0,
print nothing when they have nothing relevant, and keep output within
`WORKMAP_HOOK_BUDGET` bytes (default 1500). Output is fully qualified
references with source excerpts, and never a summary.

- **SessionStart:** if the session's cwd is inside an indexed checkout, the
  hook adds the project handle, worktree and branch, the index age, recent
  conversations in this checkout, and sibling worktrees with their latest
  activity, all as `workmap expand` references. In an unrelated directory it
  adds nothing.
- **UserPromptSubmit:** searches the prompt's words (stop-words removed)
  within that project. It adds only passages that contain at least two
  distinct prompt words (more for long prompts), or a file whose path the
  prompt names, along with the matched words. A reference already sent in a
  session is not sent again; the per-session state lives in
  `<state>/hook-seen/`, is pruned after 7 days, and is capped at 200 sessions.
- **Stop, SessionEnd:** start a detached `workmap ingest --transcript <path>
  --quiet`, so the next session sees this one without waiting for a full scan.

Installed entries are marked with `WORKMAP_HOOK=1` in the command.
`install` is idempotent. It keeps every other key and every foreign hook,
copies the previous file to `<file>.workmap-backup`, replaces the file
atomically, and refuses invalid JSON or a symlinked settings file.
`hooks uninstall` and `workmap uninstall` remove only workmap's own entries.
Rewritten files keep their content, but JSON key order and whitespace are
normalized. `--scope project` writes `.claude/settings.local.json` (personal;
Claude Code keeps it out of git) or `.codex/hooks.json` in the current
directory.

| Host | Version checked | Status | Settings |
|---|---|---|---|
| Claude Code | 2.1.302 | supported ([hooks docs](https://code.claude.com/docs/en/hooks)) | `~/.claude/settings.json`, `.claude/settings.local.json` |
| Codex CLI | 0.163.0 (`hooks` feature stable, on by default) | supported, same events and JSON output ([hooks docs](https://learn.chatgpt.com/docs/hooks)); hooks run only after you trust them with `/hooks` in Codex | `~/.codex/hooks.json`, `.codex/hooks.json` |
| OpenCode | 1.19.3 | unsupported: the installed package documents no hook or context-injection interface | — |

## Data and privacy

- The index lives in `~/.local/state/workmap/index.db` (`WORKMAP_STATE_DIR`).
  Delete the directory to remove everything.
- It contains paths, metadata, doc excerpts and sections, commit subjects, and
  the natural-language text of your agent conversations (bounded, with
  secret-looking tokens such as `ghp_…`, `sk-…`, `glpat-…` and long random
  strings redacted). Treat it like the transcripts themselves.
- workmap only reads your repositories and transcript stores. Git runs with
  `GIT_OPTIONAL_LOCKS=0`, and the OpenCode and Codex databases are opened
  read-only.
- `WORKMAP_HOME` replaces `$HOME` for scanning, provider stores, gh/glab config
  and install targets (tests and the demo use a fake home).
  `WORKMAP_CLAUDE_DIR`, `WORKMAP_CODEX_DIR` and `WORKMAP_OPENCODE_DIR` point at
  other stores. `WORKMAP_GH_BIN` and `WORKMAP_GLAB_BIN` select the CLIs.

## Scale and limits

Measured with [`examples/bench.sh`](examples/bench.sh) on a synthetic home: 40
repositories × 60 files, 200 Claude-style transcripts × 200 messages
(10.9 MB of JSONL), on a 3-vCPU VM (Xeon E5-2690 v2, ext4):

| | |
|---|---|
| first scan | 15.3 s |
| repeat scan, nothing changed | 4.6 s (mostly `git remote`/`branch`/`worktree` calls per repository) |
| index size | 26.7 MB, 40,000 message passages |
| `search` (global / one project) | 31 ms / 214 ms |
| `context --task --file` | 0.4 s |
| `related --depth 3` | 31 ms |

These numbers describe that synthetic layout on that machine, not a benchmark.
A real home is dominated by other costs: directory walk size, `git log` on slow
or network storage, and reading the first 64 KiB of every Claude transcript on
the first scan. Repeat scans only `stat` unchanged transcripts and skip
unchanged checkouts.

Known limits:

- Codex has no structured read tool. Reads come from Codex's own best-effort
  parse of shell commands (`parsed_cmd`): code `cr`, only when the command
  exited 0, never shown as a confirmed full-file read and ranked below a
  provider `Read` result. Searches and listings are kept apart (`srch`, `ls`);
  older rollouts without `parsed_cmd` give shell mentions only.
- A subagent transcript cannot be resumed on its own (it shares its parent's
  session id); `ask` refuses and names the parent.
- A checkout deleted or moved after conversations ran in it stays in the index
  as missing, with its recorded path and branch. A new location is named only
  with identity evidence (same git worktree admin dir, or same HEAD and
  branch); several candidates are listed, never chosen. A moved main checkout
  becomes a different repository.
- Only HEAD history of each worktree is indexed (default 30 commits, 40 files
  each). Other branches appear by name.
- Relations come from each repository's main checkout. A dependency added only
  on a feature branch is not seen until it is merged there.
- Manifests are parsed line by line, not with a full TOML parser. Unusual
  layouts (multi-line inline tables) can be missed. Python support covers PEP
  621 `dependencies`, Poetry dependency tables and `tool.uv.sources`.
- The compact map selects per category by fixed rules (docs and files by
  confirmed-edge count and rank, conversations with confirmed file access by
  recency, commits by how many selected files they changed) and shows at most
  6 targets per connection type. Its size depends on the graph; it is bounded by
  the limits and `--budget`, not guaranteed smaller than `--detailed`.
- Search is word-based (SQLite FTS5, `unicode61`, diacritics folded). It has
  no stemming and no fuzzy matching, and does not search code: use `rg` or your
  editor for that.
- Remote snapshots cover one branch per repository and only docs,
  manifests and blob ids, not code.
- `ask` enforcement is verified from each CLI's documented flags. OpenCode is
  unsupported.
- Host hooks match prompts lexically, without stemming. A prompt that words a
  problem differently from the transcripts gets no context.
- Identical-file detection stats at most 30,000 tracked files and records at
  most 20,000 per main checkout; very large repositories are only partly
  compared.
- The scan skips dependency, cache and build directories (`node_modules`,
  `target`, `.cache`, `.config`, `.local`, `tmp`, … see `discover::SKIP_DIRS`)
  and follows symlinked directories only within two levels of a root.

## Upgrading to 0.4

`workmap context` now prints the compact map; the previous output is
`workmap context --detailed` (unchanged, also `workmap expand <project>`).
Commit file lists keep non-ASCII paths verbatim (`core.quotePath=false`);
a checkout drops its old quoted duplicates when it is re-indexed (new HEAD,
index or doc change, or `scan --full`).
No re-parse is needed.

## Upgrading to 0.3

0.3 re-parses every transcript once (subagent transcripts, Codex `parsed_cmd`
reads and recorded checkout snapshots are new) and builds the stemmed search
table from existing passages on first open. Projects with duplicate names are
now printed as `<name>~<hex>` instead of their checkout path; the path still
resolves. Hooks are never installed implicitly: run `workmap hooks install`.

## Upgrading from 0.1

0.2 adds searchable passages and relation data. The first `workmap scan` after
upgrading re-reads every transcript and re-indexes every checkout once (about
as long as the very first scan); later scans are incremental again. Existing
evidence for transcripts deleted in the meantime is kept and flagged missing.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests build throwaway Git repositories, worktrees and synthetic Claude Code,
Codex and OpenCode stores in a temp dir, and fake `gh`/`glab` scripts. They
never touch your home or the network. CI runs the same on every push.

## License

MIT, see [LICENSE](LICENSE).
