#!/usr/bin/env python3
"""Paired hooks-on/off benchmark for workmap's Claude Code hooks.

Each case is asked twice in fresh, non-persisted `claude -p` sessions that
differ only in whether workmap's SessionStart and UserPromptSubmit hooks run.
Both arms get the same model, cwd, tools, permission rules, system-prompt
suffix, setting sources and environment (including WORKMAP_STATE_DIR, so the
skill and CLI read the same index snapshot in both arms). Stop/SessionEnd
hooks are never installed: they would ingest benchmark sessions.

Subcommands:
  settings   write on.json / off.json and the hook wrapper into --out
  retrieval  run `workmap hook ... --explain` offline for every case (adapter
             benchmark: payload replay, not native delivery)
  run        run the paired native sessions (bounded parallel, resumable)
  score      deterministic checks and a blind adjudication sheet (A/B labels)
  report     unblind manual adjudication and print paired aggregates

Keep the dataset, ground-truth notes and blind key outside every directory the
agents can read, and not next to the index snapshot: the hook prints the state
directory in its follow-up commands, so agents will look there. Check
`tools` in each run.json for reads of private paths afterwards.

Dataset (JSON, keep it private): {"cases": [{"id", "cwd", "prompt", "class":
"current|stale|negative", "facts": [{"key", "regex", "source_ref",
"source_kind"}], "stale_claims": [{"key", "regex"}], "notes"}]}
Regexes are Python, case-insensitive, matched against the final answer text.
"""
import argparse
import hashlib
import json
import os
import random
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ARMS = ("on", "off")
READ_TOOLS = [
    "Read", "Glob", "Grep",
    "Bash(git log:*)", "Bash(git show:*)", "Bash(git status:*)", "Bash(git diff:*)",
    "Bash(git branch:*)", "Bash(git grep:*)", "Bash(git ls-files:*)", "Bash(git rev-parse:*)",
    "Bash(ls:*)", "Bash(cat:*)", "Bash(head:*)", "Bash(tail:*)", "Bash(wc:*)", "Bash(du:*)",
    "Bash(df:*)", "Bash(grep:*)", "Bash(rg:*)", "Bash(find:*)", "Bash(stat:*)", "Bash(file:*)",
    "Bash(sqlite3 -readonly:*)", "Bash(workmap:*)",
]
DENY_TOOLS = [
    "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch", "Agent", "Task",
    "Bash(workmap scan:*)", "Bash(workmap ingest:*)", "Bash(workmap hooks:*)",
    "Bash(workmap install:*)", "Bash(workmap uninstall:*)", "Bash(find:*-delete*)",
    "Bash(find:*-exec*)",
]
SYSTEM_SUFFIX = (
    "Benchmark constraints: this is a read-only question about the project in the current "
    "directory. Do not modify, create or delete files, do not commit, do not start services or "
    "long jobs. Investigate with read-only tools as needed, then answer concisely. Cite the "
    "specific sources (file paths with line numbers, commit hashes, or conversation references) "
    "that support each claim. If the evidence is not available, say so instead of guessing."
)


def sh(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def load(p):
    return json.loads(Path(p).read_text())


def dump(p, v):
    Path(p).write_text(json.dumps(v, indent=1, ensure_ascii=False) + "\n")


# ---------------------------------------------------------------- settings

WRAP = r"""#!/bin/sh
# Runs one workmap hook event and records payload, output and latency in $WMB_RUN_DIR.
ev="$1"; d="${WMB_RUN_DIR:-}"
in=$(cat)
t0=$(date +%s%N)
out=$(printf '%s' "$in" | env WORKMAP_HOOK=1 WORKMAP_STATE_DIR=__STATE__ __BIN__ hook "$ev" --harness claude)
t1=$(date +%s%N)
if [ -n "$d" ] && [ -d "$d" ]; then
  printf '%s\n' "$in" >> "$d/hook-$ev.in.jsonl"
  printf '{"event":"%s","ms":%s,"bytes":%s}\n' "$ev" $(( (t1 - t0) / 1000000 )) "${#out}" >> "$d/hook-timing.jsonl"
  [ -n "$out" ] && printf '%s\n' "$out" >> "$d/hook-$ev.out.jsonl"
fi
[ -n "$out" ] && printf '%s\n' "$out"
exit 0
"""


def cmd_settings(a):
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    wrap = out / "hook-wrap.sh"
    wrap.write_text(WRAP.replace("__STATE__", shlex.quote(a.state_dir)).replace("__BIN__", shlex.quote(a.workmap_bin)))
    wrap.chmod(0o700)
    env = {"WORKMAP_STATE_DIR": a.state_dir}
    hook = lambda ev, t: [{"hooks": [{"type": "command", "command": f"{shlex.quote(str(wrap))} {ev}", "timeout": t}]}]
    dump(out / "on.json", {"env": env, "hooks": {"SessionStart": hook("SessionStart", 10), "UserPromptSubmit": hook("UserPromptSubmit", 5)}})
    dump(out / "off.json", {"env": env})
    print(f"wrote {out}/on.json {out}/off.json {wrap}")


# ---------------------------------------------------------------- retrieval (payload replay)

def cmd_retrieval(a):
    ds = load(a.dataset)
    out = Path(a.out) / "retrieval"
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, WORKMAP_STATE_DIR=a.state_dir, WORKMAP_HOOK="1")
    rows = []
    for c in ds["cases"]:
        rec = {"id": c["id"]}
        for ev in ("SessionStart", "UserPromptSubmit"):
            payload = {"hook_event_name": ev, "session_id": f"replay-{c['id']}", "cwd": c["cwd"], "prompt": c["prompt"], "source": "startup"}
            t0 = time.monotonic()
            r = sh([a.workmap_bin, "hook", ev, "--harness", "claude", "--explain"], input=json.dumps(payload), env=env)
            ms = round((time.monotonic() - t0) * 1000)
            ctx = ""
            if r.stdout.strip():
                ctx = json.loads(r.stdout)["hookSpecificOutput"]["additionalContext"]
            (out / f"{c['id']}.{ev}.explain.txt").write_text(r.stderr)
            rec[ev] = {"ms": ms, "bytes": len(ctx), "context": ctx,
                       "kept": [l.split("keep ", 1)[1].split(":", 3)[:3] for l in r.stderr.splitlines() if l.startswith("explain: keep ")]}
        rows.append(rec)
    dump(out / "retrieval.json", rows)
    print(f"{len(rows)} case(s) -> {out}/retrieval.json")


# ---------------------------------------------------------------- native runs

def parse_stream(path):
    """Summarize one stream-json transcript: init, tool calls, denials, result."""
    init, result, tools, errors = {}, {}, [], 0
    for line in Path(path).read_text(errors="replace").splitlines():
        try:
            e = json.loads(line)
        except ValueError:
            continue
        t = e.get("type")
        if t == "system" and e.get("subtype") == "init":
            init = e
        elif t == "assistant":
            for b in e.get("message", {}).get("content", []) or []:
                if b.get("type") == "tool_use":
                    inp = b.get("input", {})
                    tools.append({"name": b.get("name"), "arg": str(inp.get("command") or inp.get("file_path") or inp.get("pattern") or inp.get("path") or "")[:300]})
        elif t == "user":
            for b in (e.get("message", {}).get("content") or []):
                if isinstance(b, dict) and b.get("type") == "tool_result" and b.get("is_error"):
                    errors += 1
        elif t == "result":
            result = e
    u = result.get("usage", {}) or {}
    return {
        "model": init.get("model"),
        "init_tools": len(init.get("tools", []) or []),
        "skills_visible": sorted(init.get("skills", []) or []),
        "slash_commands": len(init.get("slash_commands", []) or []),
        "workmap_skill_visible": any(s.split(":")[-1] == "workmap" for s in (init.get("skills") or []) + (init.get("slash_commands") or [])),
        "mcp_servers": [m.get("name") for m in init.get("mcp_servers", []) or []],
        "tool_calls": len(tools),
        "tools": tools,
        "workmap_cli_calls": sum(1 for x in tools if x["name"] == "Bash" and re.search(r"(^|[\s/;&|])workmap\b", x["arg"])),
        "skill_calls": sum(1 for x in tools if x["name"] == "Skill"),
        "tool_errors": errors,
        "permission_denials": len(result.get("permission_denials", []) or []),
        "is_error": result.get("is_error"),
        "subtype": result.get("subtype"),
        "num_turns": result.get("num_turns"),
        "duration_ms": result.get("duration_ms"),
        "duration_api_ms": result.get("duration_api_ms"),
        "input_tokens": u.get("input_tokens"),
        "cache_creation_input_tokens": u.get("cache_creation_input_tokens"),
        "cache_read_input_tokens": u.get("cache_read_input_tokens"),
        "output_tokens": u.get("output_tokens"),
        "total_cost_usd": result.get("total_cost_usd"),
        "model_usage": result.get("modelUsage"),
        "answer": result.get("result", ""),
    }


def run_one(a, c, arm, rdir):
    rdir.mkdir(parents=True, exist_ok=True)
    meta_p = rdir / "run.json"
    if meta_p.exists():
        return load(meta_p)
    settings = str(Path(a.out) / f"{arm}.json")
    argv = ["claude", "-p", c["prompt"], "--model", a.model,
            "--setting-sources", a.setting_sources, "--settings", settings,
            "--strict-mcp-config", "--no-session-persistence",
            "--permission-mode", "dontAsk",
            "--allowedTools", *READ_TOOLS, "--disallowedTools", *DENY_TOOLS,
            "--append-system-prompt", SYSTEM_SUFFIX,
            *[x for d in a.plugin_dir for x in ("--plugin-dir", d)],
            "--output-format", "stream-json", "--verbose"]
    env = dict(os.environ, WMB_RUN_DIR=str(rdir), WORKMAP_STATE_DIR=a.state_dir)
    if a.claude_tmpdir:
        env["CLAUDE_CODE_TMPDIR"] = a.claude_tmpdir
    t0 = time.monotonic()
    with open(rdir / "stream.jsonl", "w") as so, open(rdir / "stderr.txt", "w") as se:
        p = subprocess.Popen(argv, cwd=c["cwd"], env=env, stdout=so, stderr=se, stdin=subprocess.DEVNULL, start_new_session=True)
        try:
            rc = p.wait(timeout=a.timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            rc, timed_out = p.wait(), True
    wall_ms = round((time.monotonic() - t0) * 1000)
    s = parse_stream(rdir / "stream.jsonl")
    hook_bytes, hook_ms = 0, 0
    tp = rdir / "hook-timing.jsonl"
    if tp.exists():
        for l in tp.read_text().splitlines():
            h = json.loads(l)
            hook_bytes += h["bytes"]
            hook_ms += h["ms"]
    m = {"case": c["id"], "arm": arm, "rc": rc, "timed_out": timed_out, "wall_ms": wall_ms,
         "hook_bytes": hook_bytes, "hook_ms": hook_ms, "argv_flags": argv[3:4] + argv[5:], **s}
    dump(meta_p, m)
    return m


def cmd_run(a):
    ds = load(a.dataset)
    cases = [c for c in ds["cases"] if not a.cases or c["id"] in a.cases.split(",")]
    rng = random.Random(a.seed)
    order = list(range(len(cases)))
    rng.shuffle(order)
    plan = []
    for i in order:
        arms = list(ARMS)
        rng.shuffle(arms)
        plan.append((cases[i], arms))
    dump(Path(a.out) / "plan.json", [{"case": c["id"], "order": arms} for c, arms in plan])
    budget = {"left": a.max_runs}
    lock = threading.Lock()
    prog = Path(a.out) / "progress.jsonl"

    def pair(item):
        c, arms = item
        res = []
        for pos, arm in enumerate(arms):
            rdir = Path(a.out) / "runs" / c["id"] / arm
            fresh = not (rdir / "run.json").exists()
            with lock:
                if fresh and budget["left"] <= 0:
                    return res
                if fresh:
                    budget["left"] -= 1
            m = run_one(a, c, arm, rdir)
            with lock, open(prog, "a") as f:
                f.write(json.dumps({"t": int(time.time()), "case": c["id"], "arm": arm, "pos": pos, "rc": m["rc"], "timed_out": m["timed_out"], "wall_ms": m["wall_ms"], "hook_bytes": m["hook_bytes"], "cost": m["total_cost_usd"]}) + "\n")
            res.append(m)
        return res

    with ThreadPoolExecutor(a.jobs) as ex:
        list(ex.map(pair, plan))
    print(f"done; progress in {prog}")


# ---------------------------------------------------------------- scoring

CITE_PATH = re.compile(r"(?<![\w.])((?:/|\.{1,2}/|[\w.-]+/)[\w./@+-]*\.[A-Za-z0-9]{1,8}|/[\w./@+-]{3,})(?::(\d+))?")
CITE_SHA = re.compile(r"\b[0-9a-f]{7,40}\b")


def citations(ans, cwd):
    paths, shas = {}, {}
    for m in CITE_PATH.finditer(ans):
        p = m.group(1).rstrip(".,)")
        full = Path(p) if p.startswith("/") else Path(cwd) / p
        paths[p] = full.exists()
    for s in set(CITE_SHA.findall(ans)):
        if re.fullmatch(r"\d+", s):
            continue
        shas[s] = sh(["git", "-C", cwd, "cat-file", "-e", s + "^{commit}"]).returncode == 0
    return paths, shas


def cmd_score(a):
    ds = load(a.dataset)
    out = Path(a.out)
    rng = random.Random(a.seed + 1)
    rows, sheet, key = [], [], {}
    for c in ds["cases"]:
        for arm in ARMS:
            p = out / "runs" / c["id"] / arm / "run.json"
            if not p.exists():
                continue
            m = load(p)
            ans = m.get("answer") or ""
            facts = {f["key"]: (bool(re.search(f["regex"], ans, re.I | re.S)) if f.get("regex") else None) for f in c.get("facts", [])}
            stale = {s["key"]: bool(re.search(s["regex"], ans, re.I | re.S)) for s in c.get("stale_claims", [])}
            paths, shas = citations(ans, c["cwd"])
            rows.append({"case": c["id"], "class": c["class"], "arm": arm, "facts_exact": facts, "stale_hits": stale,
                         "cited_paths": len(paths), "cited_paths_valid": sum(paths.values()),
                         "cited_shas": len(shas), "cited_shas_valid": sum(shas.values()),
                         **{k: m.get(k) for k in ("timed_out", "is_error", "wall_ms", "duration_ms", "duration_api_ms", "num_turns", "tool_calls", "workmap_cli_calls", "skill_calls", "tool_errors", "permission_denials", "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens", "total_cost_usd", "hook_bytes", "hook_ms")}})
        answers = [(arm, (load(out / "runs" / c["id"] / arm / "run.json").get("answer") or "")) for arm in ARMS if (out / "runs" / c["id"] / arm / "run.json").exists()]
        rng.shuffle(answers)
        labels = "AB"
        key[c["id"]] = {labels[i]: arm for i, (arm, _) in enumerate(answers)}
        sheet.append({"case": c["id"], "prompt": c["prompt"], "class": c["class"], "facts": c.get("facts", []), "stale_claims": c.get("stale_claims", []), "notes": c.get("notes", ""),
                      "answers": {labels[i]: ans for i, (_, ans) in enumerate(answers)}})
    dump(out / "score-exact.json", rows)
    dump(out / "blind-sheet.json", sheet)
    dump(out / "blind-key.json", key)
    print(f"{len(rows)} run(s) scored -> score-exact.json; blind-sheet.json for manual adjudication (blind-key.json kept separate)")


def sign_test(wins, losses):
    """Two-sided exact binomial p for wins vs losses (ties dropped)."""
    from math import comb
    n, k = wins + losses, min(wins, losses)
    if n == 0:
        return 1.0
    return min(1.0, 2 * sum(comb(n, i) for i in range(k + 1)) / 2 ** n)


def cmd_report(a):
    """Unblind manual adjudication (adj-*.json + blind-key.json) and compute paired metrics."""
    ds = {c["id"]: c for c in load(a.dataset)["cases"]}
    out = Path(a.out)
    key = load(a.key)
    exact = {(r["case"], r["arm"]): r for r in load(out / "score-exact.json")}
    adj = {}
    for f in a.adjudication:
        for s in load(f)["scores"]:
            adj[(s["case"], key[s["case"]][s["label"]])] = s
    excluded = set(filter(None, a.exclude.split(",")))
    table = []
    for cid, c in ds.items():
        row = {"case": cid, "class": c["class"], "excluded": cid in excluded, "facts": len(c.get("facts", []))}
        for arm in ARMS:
            e, j = exact.get((cid, arm), {}), adj.get((cid, arm), {})
            tok_in = sum((e.get(k) or 0) for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))
            fl = [x["label"] for x in j.get("facts", [])]
            row[arm] = {
                "fact_score": sum(x["score"] for x in j.get("facts", [])), "fact_max": 2 * len(c.get("facts", [])),
                "facts_correct": fl.count("correct"), "facts_wrong": fl.count("wrong"), "facts_unavailable": fl.count("unavailable"), "facts_omitted": fl.count("omitted"), "facts_partial": fl.count("partial"),
                "exact_hits": sum(1 for v in e.get("facts_exact", {}).values() if v), "exact_checked": sum(1 for v in e.get("facts_exact", {}).values() if v is not None),
                "stale_regex": sum(e.get("stale_hits", {}).values()), "stale_manual": sum(1 for v in j.get("stale", {}).values() if v),
                "usefulness": j.get("usefulness"), "fabricated": len(j.get("fabricated", [])),
                "cit_checked": j.get("citations", {}).get("checked", 0), "cit_supported": j.get("citations", {}).get("supported", 0),
                "tokens_in_total": tok_in, "cache_read": e.get("cache_read_input_tokens"), "cache_creation": e.get("cache_creation_input_tokens"),
                "input_uncached": e.get("input_tokens"), "output_tokens": e.get("output_tokens"), "cost_usd": e.get("total_cost_usd"),
                "duration_ms": e.get("duration_ms"), "wall_ms": e.get("wall_ms"), "timed_out": e.get("timed_out"),
                "tool_calls": e.get("tool_calls"), "workmap_cli_calls": e.get("workmap_cli_calls"), "skill_calls": e.get("skill_calls"),
                "permission_denials": e.get("permission_denials"), "hook_bytes": e.get("hook_bytes"), "hook_ms": e.get("hook_ms")}
        table.append(row)
    rows = [r for r in table if not r["excluded"] and r["on"]["usefulness"] is not None and r["off"]["usefulness"] is not None]

    def paired(metric, higher_better=True):
        d = [r["on"][metric] - r["off"][metric] for r in rows if r["on"][metric] is not None and r["off"][metric] is not None]
        w = sum(1 for x in d if (x > 0) == higher_better and x != 0)
        l = sum(1 for x in d if (x < 0) == higher_better and x != 0)
        s = sorted(d)
        tot = lambda arm: sum(r[arm][metric] or 0 for r in rows)
        return {"n": len(d), "sum_on": tot("on"), "sum_off": tot("off"), "mean_diff_on_minus_off": round(sum(d) / len(d), 3) if d else None,
                "median_diff": s[len(s) // 2] if s else None, "on_better": w, "off_better": l, "ties": len(d) - w - l, "sign_test_p": round(sign_test(w, l), 4)}

    agg = {m: paired(m, hb) for m, hb in [("fact_score", True), ("facts_correct", True), ("facts_wrong", False), ("facts_unavailable", False), ("usefulness", True),
                                          ("stale_manual", False), ("stale_regex", False), ("fabricated", False), ("exact_hits", True), ("cit_supported", True),
                                          ("tokens_in_total", False), ("output_tokens", False), ("cost_usd", False), ("duration_ms", False), ("tool_calls", False)]}
    fmax = sum(r["facts"] for r in rows)
    agg["source_coverage"] = {"denominator_facts": fmax, "on_correct": sum(r["on"]["facts_correct"] for r in rows), "off_correct": sum(r["off"]["facts_correct"] for r in rows)}
    by_class = {}
    for cl in sorted({r["class"] for r in rows}):
        rs = [r for r in rows if r["class"] == cl]
        by_class[cl] = {"n": len(rs), **{f"{arm}_fact_score": sum(r[arm]["fact_score"] for r in rs) for arm in ARMS}, "fact_max": sum(r["on"]["fact_max"] for r in rs),
                        **{f"{arm}_stale_manual": sum(r[arm]["stale_manual"] for r in rs) for arm in ARMS}}
    agg["by_class"] = by_class
    dump(out / "report.json", {"excluded": sorted(excluded), "n_pairs": len(rows), "aggregate": agg, "per_case": table})
    print(json.dumps({"n_pairs": len(rows), "aggregate": agg}, indent=1))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sp = ap.add_subparsers(dest="cmd", required=True)
    for name in ("settings", "retrieval", "run", "score", "report"):
        p = sp.add_parser(name)
        p.add_argument("--out", required=True, help="private artifact directory")
        p.add_argument("--state-dir", default=os.environ.get("WORKMAP_STATE_DIR", ""), help="index snapshot dir (never the live one)")
        p.add_argument("--workmap-bin", default="workmap")
        p.add_argument("--dataset")
        p.add_argument("--seed", type=int, default=20261027)
    r = sp.choices["run"]
    r.add_argument("--model", default="claude-haiku-5-5")
    r.add_argument("--jobs", type=int, default=3)
    r.add_argument("--timeout", type=int, default=300, help="per-run wall clock limit, seconds")
    r.add_argument("--max-runs", type=int, default=60)
    r.add_argument("--cases", default="", help="comma-separated subset")
    r.add_argument("--setting-sources", default="project,local", help="user settings are excluded so live hooks never run")
    r.add_argument("--claude-tmpdir", default="")
    r.add_argument("--plugin-dir", action="append", default=[],
                   help="plugin loaded in BOTH arms, e.g. one carrying the workmap skill, since user skills are excluded with user settings")
    rp = sp.choices["report"]
    rp.add_argument("--key", required=True, help="blind-key.json (move it away from adjudicators first)")
    rp.add_argument("--adjudication", action="append", required=True, help="adjudication JSON file(s), repeatable")
    rp.add_argument("--exclude", default="", help="comma-separated case ids excluded from paired aggregates (e.g. contaminated)")
    a = ap.parse_args()
    if a.cmd != "settings" and not a.dataset:
        ap.error("--dataset is required")
    if not a.state_dir:
        ap.error("--state-dir is required")
    {"settings": cmd_settings, "retrieval": cmd_retrieval, "run": cmd_run, "score": cmd_score, "report": cmd_report}[a.cmd](a)


if __name__ == "__main__":
    main()
