//! Search fallbacks (prefix/stem/substring), labels, no-match diagnostics.
//! Synthetic fixtures only: temp homes, fake CLIs, no network, no real home.

#[allow(unused_imports)]
use super::*;
use crate::search;
use crate::testutil::{commit_all, ledger, write, Ledger};
use crate::view::more::{self, SearchOpts, SearchReq};

/// The shared ledger fixture plus: a runbook doc (deadlock wording, an embedded
/// CamelCase identifier) and a Claude conversation in the main checkout.
fn fx() -> (Ledger, Connection, String) {
    let fx = ledger();
    write(&fx.api.join("docs/runbook.md"), "# Runbook\n\n## Worker stalls\nThe worker deadlocked on the ledger row lock when two retries raced.\nConfigured by LedgerRetryPolicy in settings.\nA second post of the same event fails with ERR_DUP_CHARGE.\n");
    commit_all(&fx.api, "Runbook for worker stalls");
    let m = fx.api.display().to_string();
    let conv = fx.home.join(".claude/projects").join(crate::util::claude_project_dir_name(&m)).join("cccc3333-lock.jsonl");
    write(
        &conv,
        &crate::testutil::jsonl(&[
            json!({"type":"summary","summary":"Worker stall investigation"}),
            json!({"type":"user","cwd":m,"sessionId":"cccc3333-lock","gitBranch":"main","timestamp":"2026-09-04T08:00:00Z","message":{"role":"user","content":"Why does the reconciliation worker hang at night?"}}),
            json!({"type":"assistant","cwd":m,"sessionId":"cccc3333-lock","timestamp":"2026-09-04T08:00:05Z","message":{"content":[{"type":"text","text":"It deadlocked: two retries took the row locks in opposite order."}]}}),
        ]),
    );
    // A transcript whose cwd is outside every indexed checkout: never indexed.
    let stray = fx.home.join("scratch");
    std::fs::create_dir_all(&stray).unwrap();
    let st = stray.display().to_string();
    write(
        &fx.home.join(".claude/projects").join(crate::util::claude_project_dir_name(&st)).join("dddd4444-stray.jsonl"),
        &crate::testutil::jsonl(&[
            json!({"type":"user","cwd":st,"sessionId":"dddd4444-stray","timestamp":"2026-09-05T08:00:00Z","message":{"role":"user","content":"deadlock notes from a scratch dir"}}),
        ]),
    );
    let env = Env::with_home(&fx.home);
    let (c, _) = scan_once(&env);
    let handle = view::resolve_project(&c, &fx.api.to_string_lossy()).unwrap().handle;
    (fx, c, handle)
}

fn qy(text: &str) -> search::Query {
    search::Query { text: text.into(), limit: 10, ..Default::default() }
}

fn global(c: &Connection, q: search::Query) -> String {
    more::search_view(&v(c), &view::Req::default(), &SearchReq { query: q, this_worktree: false }).unwrap()
}

fn scoped_search(c: &Connection, handle: &str, q: search::Query, here: bool) -> String {
    more::search_view_opts(&v(c), &scoped(handle), &SearchReq { query: q, this_worktree: here }, &SearchOpts::default()).unwrap()
}

/// Hit lines (`N. ref — description [tier] [provenance]`) of a search output.
fn hit_lines(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.split_once(". ").map(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).unwrap_or(false)).collect()
}

/// Every `workmap search …` command printed in an output (after `→ ` or `: `).
fn printed_commands(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.find("workmap search ").map(|i| l[i..].to_string())).collect()
}

/// Run a printed command through the real CLI argument handling.
fn run_printed(c: &Connection, cmd: &str) -> String {
    let w = shell_split(cmd);
    assert_eq!((w[0].as_str(), w[1].as_str()), ("workmap", "search"), "{cmd}");
    crate::search_cmd(&v(c), &crate::Args::parse(w[1..].to_vec())).unwrap_or_else(|e| panic!("{cmd}: {e}"))
}

fn cli(c: &Connection, args: &[&str]) -> String {
    let mut a = vec!["search".to_string()];
    a.extend(args.iter().map(|s| s.to_string()));
    crate::search_cmd(&v(c), &crate::Args::parse(a)).unwrap()
}

#[test]
fn prefix_and_stem_tiers_recover_inflected_words_and_label_every_hit() {
    let (_fx, c, h) = fx();
    // Review finding: `deadlock` found nothing while `deadlocked` found the text.
    let exact = global(&c, qy("deadlocked"));
    assert!(hit_lines(&exact).iter().all(|l| l.contains("[exact]")) && hit_lines(&exact).len() == 2, "{exact}");
    let out = global(&c, qy("deadlock"));
    let lines = hit_lines(&out);
    assert!(out.contains("Tiers: exact 0 · prefix 2 · stem 2"), "{out}");
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines.iter().all(|l| l.ends_with("[prefix]")), "{out}");
    assert!(out.contains(":message:cccc3333:l3") && out.contains(":docs/runbook.md#worker-stalls") && out.contains("[deadlocked]"), "{out}");
    assert!(!out.contains("scratch dir"), "the unrelated transcript is not indexed");
    // `--exact` keeps today's strict behaviour and says what the fallbacks would add.
    let strict = global(&c, search::Query { exact: true, ..qy("deadlock") });
    assert!(strict.contains("no matches") && strict.contains("Tiers: exact 0 (") && !strict.contains("stricter:"), "{strict}");
    let looser = strict.lines().find(|l| l.starts_with("Looser: without --exact")).unwrap_or_else(|| panic!("{strict}"));
    let cmd = &printed_commands(looser)[0];
    assert!(hit_lines(&run_printed(&c, cmd)).len() == 2, "{cmd}");
    // Stems: charging ~ charged ~ charges ~ charge, only via the Porter tier.
    let res = search::search(&c, &qy("charging"), &search::Scope::default()).unwrap();
    assert!(res.tiers.iter().map(|t| (t.0.label, t.1)).collect::<Vec<_>>().starts_with(&[("exact", 0), ("prefix", 0)]), "{:?}", res.tiers);
    assert!(res.hits.len() >= 3 && res.hits.iter().all(|h| h.tier == "stem"), "{:?}", res.hits);
    let kinds: std::collections::BTreeSet<&str> = res.hits.iter().map(|h| h.kind.as_str()).collect();
    assert!(kinds.contains("message") && kinds.contains("doc") && kinds.contains("commit"), "{kinds:?}");
    let out = global(&c, qy("charging"));
    assert!(out.contains("[charged]") || out.contains("[charge]"), "{out}");
    for o in [&exact, &out] {
        assert_refs_roundtrip(&c, &h, o);
    }
    let sc = scoped_search(&c, &h, qy("deadlock"), false);
    assert!(assert_refs_roundtrip(&c, &h, &sc) >= 2, "{sc}");
}

#[test]
fn any_word_is_opt_in_and_no_match_output_names_the_missing_word() {
    let (_fx, c, h) = fx();
    // Synonyms are not guessed: `duplicate` never matches `double`.
    let out = global(&c, qy("duplicate charges"));
    assert!(out.contains("no matches") && out.contains("Tiers: exact 0 · prefix 0 · stem 0"), "{out}");
    assert!(out.contains("Per word in this scope (prefix+stem): duplicate 0 · charges "), "{out}");
    assert!(!out.contains("[any]"), "OR is never run implicitly");
    let line = out.lines().find(|l| l.starts_with("Looser: any word (--any)")).unwrap_or_else(|| panic!("{out}"));
    let cmd = &printed_commands(line)[0];
    assert!(cmd.ends_with("--any"), "{cmd}");
    let any = run_printed(&c, cmd);
    let lines = hit_lines(&any);
    assert!(!lines.is_empty() && lines.iter().all(|l| l.contains("[any]")), "{any}");
    let n: usize = line.split(") ").nth(1).and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse().ok()).unwrap();
    assert!(any.contains(&format!("of {n} matching passage(s)")), "the printed count is what the command finds:\n{line}\n{any}");
    // Exact AND hits come first and keep their own label under --any.
    let mixed = global(&c, search::Query { any: true, ..qy("worker deadlocked") });
    let l = hit_lines(&mixed);
    assert!(l[0].contains("[exact]") && l.iter().any(|x| x.contains("[any]")), "{mixed}");
    let first_any = l.iter().position(|x| x.contains("[any]")).unwrap();
    assert!(l[first_any..].iter().all(|x| x.contains("[any]")), "tiers stay in order:\n{mixed}");
    assert_refs_roundtrip(&c, &h, &mixed);
}

#[test]
fn identifier_queries_find_substrings_inside_tokens_with_their_own_label() {
    let (fx, c, h) = fx();
    // CamelCase inside a longer token: FTS has one token `ledgerretrypolicy`.
    let out = global(&c, qy("RetryPolicy"));
    assert!(out.contains("Tiers: exact 0 · prefix 0 · stem 0 · substring 1"), "{out}");
    let l = hit_lines(&out);
    assert!(l.len() == 1 && l[0].contains(":docs/runbook.md#worker-stalls") && l[0].ends_with("[substring]") && out.contains("Ledger[RetryPolicy]"), "{out}");
    // Error codes stay one phrase (exact), and a code prefix still finds it.
    let code = global(&c, qy("ERR_DUP_CHARGE"));
    assert!(hit_lines(&code).len() == 1 && hit_lines(&code)[0].contains("[exact]"), "{code}");
    let pre = global(&c, qy("ERR_DUP_CHARG"));
    assert!(hit_lines(&pre).first().map(|l| l.contains("runbook.md") && l.ends_with("[prefix]")).unwrap_or(false), "{pre}");
    assert!(!global(&c, search::Query { exact: true, ..qy("RetryPolicy") }).contains("[substring]"), "--exact disables the substring tier");
    // A path: the commit naming it (exact phrase) and the file node itself (substring).
    let path = global(&c, qy("retry.ts"));
    let l = hit_lines(&path);
    assert!(l.iter().any(|x| x.contains("ledger-sdk:commit:") && x.ends_with("[exact]")), "{path}");
    let file = l.iter().find(|x| x.contains("ledger-sdk:src/retry.ts — file")).unwrap_or_else(|| panic!("{path}"));
    assert!(file.ends_with("[substring]"), "{file}");
    let ex = view::expand(&v(&c), "ledger-sdk:src/retry.ts", &view::Req::default()).unwrap();
    assert!(ex.starts_with("# :src/retry.ts — file in ledger-sdk") && ex.contains("SDK skeleton with retry backoff"), "file ref expands to the file view:\n{ex}");
    // A sha prefix longer than the 8-char token in commit titles: only the stored sha has it.
    let sha = String::from_utf8(std::process::Command::new("git").arg("-C").arg(&fx.api_fix).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    let pre12 = &sha.trim()[..12];
    let s = scoped_search(&c, &h, qy(pre12), false);
    let l = hit_lines(&s);
    assert!(l.len() == 1 && l[0].starts_with(&format!("1. :commit:{} — commit", &pre12[..8])) && l[0].contains("[substring] [other worktree fix/webhook-retry]"), "{s}");
    assert!(s.contains(&format!("[{}", &pre12[..8])), "snippet marks the sha:\n{s}");
    assert!(assert_refs_roundtrip(&c, &h, &s) >= 1);
    assert!(expand_ref(&c, &h, &format!(":commit:{}", &pre12[..8])).unwrap().contains("Reuse provider idempotency key"));
    // The file kind is selectable and still scoped.
    let only_files = cli(&c, &["retry.ts", "--kind", "file"]);
    assert!(hit_lines(&only_files).len() == 1 && only_files.contains("ledger-sdk:src/retry.ts"), "{only_files}");
    let api_only = scoped_search(&c, &h, qy("retry.ts"), false);
    assert!(!api_only.lines().any(|l| l.starts_with("1. ") && l.contains("ledger-sdk")), "{api_only}");
}

#[test]
fn filters_and_scope_hold_in_every_fallback_tier() {
    let (fx, c, h) = fx();
    // --kind in the stem tier.
    let docs = search::search(&c, &search::Query { kinds: vec!["doc".into()], ..qy("charging") }, &search::Scope::default()).unwrap();
    assert!(!docs.hits.is_empty() && docs.hits.iter().all(|x| x.kind == "doc" && x.tier == "stem"), "{:?}", docs.hits);
    // --since in the stem tier (commits are dated 2026-01-02, messages 2026-09).
    let all = search::search(&c, &qy("charging"), &search::Scope::default()).unwrap();
    assert!(all.hits.iter().any(|x| x.kind == "commit"), "{:?}", all.hits);
    let since = search::date_to_ts("2026-09-02").unwrap();
    let recent = search::search(&c, &search::Query { since: Some(since), ..qy("charging") }, &search::Scope::default()).unwrap();
    assert!(recent.hits.iter().any(|x| x.kind == "message") && recent.hits.iter().all(|x| x.ts >= since && x.kind != "commit"), "{:?}", recent.hits);
    // --speaker in the prefix tier; the output says what the filter removed and how to see it.
    let out = cli(&c, &["deadlock", "--speaker", "user", "--project", &h]);
    assert!(out.contains("no matches"), "{out}");
    let line = out.lines().find(|l| l.starts_with("Filtered out: ")).unwrap_or_else(|| panic!("{out}"));
    assert!(line.starts_with("Filtered out: 2 more in this scope"), "{line}");
    assert_eq!(hit_lines(&run_printed(&c, &printed_commands(line)[0])).len(), 2);
    // --kind and --this-worktree in the substring tier.
    assert!(global(&c, search::Query { kinds: vec!["message".into()], ..qy("RetryPolicy") }).contains("no matches"));
    let fixp = view::resolve_project(&c, &fx.api_fix.to_string_lossy()).unwrap();
    let mut req = scoped(&h);
    req.worktree = Some(view::worktree_name(&fixp, &fixp.focus));
    let s = more::search_view_opts(&v(&c), &req, &SearchReq { query: qy("RetryPolicy"), this_worktree: true }, &SearchOpts::default()).unwrap();
    assert!(s.contains("no matches") && s.contains("1 in other worktree main → "), "runbook lives in main only:\n{s}");
    // Provider filter in the prefix tier.
    assert!(hit_lines(&cli(&c, &["deadlock", "--provider", "codex"])).is_empty());
    assert_eq!(hit_lines(&cli(&c, &["deadlock", "--provider", "claude"])).len(), 1);
}

#[test]
fn ranking_and_output_are_deterministic() {
    let (_fx, c, h) = fx();
    for t in ["charging", "deadlock", "retry", "retry.ts", "worker deadlocked"] {
        for any in [false, true] {
            let q = search::Query { any, ..qy(t) };
            let a = global(&c, q.clone());
            let b = global(&c, q.clone());
            assert_eq!(a, b, "{t}");
            let s1 = scoped_search(&c, &h, q.clone(), false);
            assert_eq!(s1, scoped_search(&c, &h, q.clone(), false), "{t}");
            assert_refs_roundtrip(&c, &h, &s1);
        }
    }
}

#[test]
fn no_match_diagnostics_point_at_hits_outside_the_scope_with_working_commands() {
    let (_fx, c, h) = fx();
    // The fix lives only in the fix/webhook-retry worktree.
    let out = cli(&c, &["idempotency", "--project", &h, "--here"]);
    assert!(out.contains("no matches") && out.contains("@main only"), "{out}");
    for want in [
        "Searched: project ",
        "worktree main only",
        "Indexed there: ",
        "Freshness: worktree main: indexed at its current HEAD",
        "Outside this scope: 2 matching passage(s)",
        "2 in other worktree fix/webhook-retry → ",
        "Not indexed: 1 transcript(s) anywhere whose cwd is outside every indexed checkout",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    // Sample of the outside hits: fully qualified refs tagged with where they live.
    assert!(out.lines().filter(|l| l.starts_with("  - ") && l.ends_with("[exact] [other worktree fix/webhook-retry]")).count() == 2, "{out}");
    assert!(assert_refs_roundtrip(&c, &h, &out) >= 2, "{out}");
    let cmds = printed_commands(&out);
    assert_eq!(cmds.len(), 2, "{cmds:?}");
    for cmd in &cmds {
        let r = run_printed(&c, cmd);
        assert_eq!(hit_lines(&r).len(), 2, "{cmd}:\n{r}");
        assert_refs_roundtrip(&c, &h, &r);
    }
    assert!(cmds[0].contains("--worktree fix/webhook-retry --here") && !cmds[1].contains("--here"), "{cmds:?}");
    // In another project: per-project count and both widening commands work.
    let out = cli(&c, &["TypeScript", "--project", &h, "--here"]);
    assert!(out.contains("1 in other project ledger-sdk → ") && out.contains("[other project ledger-sdk]"), "{out}");
    let cmds = printed_commands(&out);
    assert_eq!(cmds.len(), 3, "{out}");
    let got: Vec<usize> = cmds.iter().map(|cmd| hit_lines(&run_printed(&c, cmd)).len()).collect();
    assert_eq!(got, [1, 0, 1], "per-project and drop --project find it; dropping only --here does not:\n{cmds:?}");
    // Nothing anywhere: say so, no commands that lead nowhere.
    let none = cli(&c, &["zyzzogeton", "--project", &h]);
    assert!(none.contains("Outside this scope: no matches anywhere else in the index either") && printed_commands(&none).is_empty(), "{none}");
    // Few hits get the compact report; the limit cut gets a working `--limit`.
    let cut = cli(&c, &["charging", "--limit", "1"]);
    let more = cut.lines().find(|l| l.starts_with("More: ")).unwrap_or_else(|| panic!("{cut}"));
    assert!(more.contains(" more matching passage(s) not shown") && more.contains("--limit 3"), "{more}");
    assert_eq!(hit_lines(&run_printed(&c, &printed_commands(more)[0])).len(), 3);
}

#[test]
fn scoped_search_ranks_this_worktree_first_and_tags_the_rest() {
    let (fx, c, h) = fx();
    // Scope = the fix worktree (by path), all worktrees searched.
    let fix_path = fx.api_fix.to_string_lossy().into_owned();
    let out = more::search_view_opts(&v(&c), &scoped(&fix_path), &SearchReq { query: qy("retry"), this_worktree: false }, &SearchOpts::default()).unwrap();
    let lines = hit_lines(&out);
    assert!(lines.iter().any(|l| l.ends_with("[other worktree main]")) && lines.iter().any(|l| !l.contains("[other")), "{out}");
    for tier in ["[exact]", "[prefix]", "[stem]"] {
        let t: Vec<&&str> = lines.iter().filter(|l| l.contains(tier)).collect();
        if let Some(first_other) = t.iter().position(|l| l.contains("[other worktree")) {
            assert!(t[first_other..].iter().all(|l| l.contains("[other worktree")), "this worktree first within {tier}:\n{out}");
        }
    }
    assert!(lines.iter().filter(|l| l.contains("[other worktree main]")).all(|l| l.contains(" — ")), "{out}");
    // The narrowing hint runs and drops the other worktree.
    let hint = out.lines().find(|l| l.contains("hit(s) above are from other worktrees")).unwrap_or_else(|| panic!("{out}"));
    assert!(hint.contains("Only @fix/webhook-retry: "), "{hint}");
    let only = run_printed(&c, &printed_commands(hint)[0]);
    assert!(!only.contains("[other worktree") && !hit_lines(&only).is_empty(), "{only}");
    assert!(assert_refs_roundtrip(&c, &h, &out) >= 2);
    assert_refs_roundtrip(&c, &h, &only);
    // `--here` is `--this-worktree`.
    let a = cli(&c, &["retry", "--project", &fix_path, "--here"]);
    let b = cli(&c, &["retry", "--project", &fix_path, "--this-worktree"]);
    assert_eq!(a, b);
}

#[test]
fn explain_prints_each_tier_query_and_where_matches_went() {
    let (_fx, c, h) = fx();
    let out = cli(&c, &["deadlock", "--project", &h, "--kind", "commit", "--explain"]);
    for want in [
        "## Explain",
        "[exact] passages_fts MATCH '\"deadlock\"' → 0 in scope (0 shown)",
        "[prefix] passages_fts MATCH '\"deadlock\"*' → 0 in scope (0 shown) · 0 in the whole index with the same filters · 2 in scope without kind/speaker/provider/since filters",
        "[stem] passages_stem MATCH '\"deadlock\"' → 0 in scope",
        "Filtered out: 2 more in this scope without --kind",
        "Indexed there: ",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    let sub = cli(&c, &["RetryPolicy", "--explain"]);
    assert!(sub.contains("[substring] contains 'retrypolicy' → 1 in scope (1 shown)"), "{sub}");
}
