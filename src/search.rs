//! Full-text search over indexed passages (SQLite FTS5, bm25 ranking with
//! deterministic tie-breaking). Searches project metadata, doc sections,
//! commit subjects, remote catalog entries and individual conversation
//! messages. Code search stays with your editor or `rg`.
//!
//! Recall is widened in fixed, inspectable tiers; every hit carries the tier
//! that found it:
//!
//! 1. `exact`     all words, as tokens (`passages_fts`, unicode61)
//! 2. `prefix`    all words, words of ≥4 characters as prefixes (`deadlock*`)
//! 3. `stem`      all words under the Porter stemmer (`passages_stem`):
//!    charges ~ charged ~ charging
//! 4. `any`       any word (stemmed), only with `--any`
//! 5. `substring` identifier-like queries (sha, path, CamelCase, snake_case,
//!    ERROR_CODE): every word as a case-insensitive substring of the passage
//!    text/title/commit sha, plus file nodes whose path contains it
//!
//! Tiers fill up to the limit in that order (duplicates keep the earliest
//! tier); `--exact` runs tier 1 only. No synonyms, no embeddings.

use rusqlite::{types::Value as SqlValue, Connection};

pub const KINDS: &[&str] = &["project", "doc", "commit", "message", "remote"];
/// `--kind` values accepted by `search`: passages plus file nodes (substring tier only).
pub const SEARCH_KINDS: &[&str] = &["project", "doc", "commit", "message", "remote", "file"];

#[derive(Default, Clone, Debug)]
pub struct Query {
    pub text: String,
    pub kinds: Vec<String>,
    pub provider: Option<String>,
    pub speaker: Option<String>,
    pub since: Option<i64>,
    pub limit: usize,
    /// Match any word instead of all words.
    pub any: bool,
    /// Exact tokens only: no prefix/stem/substring fallback tiers.
    pub exact: bool,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub kind: String,
    pub node_id: String,
    pub repo_id: Option<String>,
    pub checkout_id: Option<String>,
    pub source_path: String,
    pub line: i64,
    pub loc: String,
    pub speaker: String,
    pub ts: i64,
    pub title: String,
    pub heading: String,
    pub snippet: String,
    /// Identical hits collapsed into this one (e.g. the same doc in other worktrees).
    pub dups: usize,
    /// Which tier found it: exact | prefix | stem | any | substring.
    pub tier: &'static str,
}

/// One term per query word; words joined by `._-/:` inside an identifier
/// (`retry.ts`, `idempotency_key`) and `"quoted phrases"` stay one phrase.
fn terms(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let split = |s: &str| -> Vec<String> { s.split(|c: char| !(c.is_alphanumeric() || c == '_')).flat_map(|w| w.split('_')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect() };
    for (i, seg) in text.split('"').enumerate() {
        if i % 2 == 1 {
            let words = split(seg);
            if !words.is_empty() {
                out.push(words);
            }
            continue;
        }
        for raw in seg.split_whitespace() {
            // Inside one whitespace word, `a.b` / `a_b` / `a/b` / `a-b` / `a:b` is one identifier...
            let mut cur = String::new();
            let chars: Vec<char> = raw.chars().collect();
            let mut groups: Vec<String> = Vec::new();
            for (k, &ch) in chars.iter().enumerate() {
                let joiner = "._-/:".contains(ch) && k > 0 && chars[k - 1].is_alphanumeric() && chars.get(k + 1).map(|n| n.is_alphanumeric()).unwrap_or(false);
                if ch.is_alphanumeric() || ch == '_' || joiner {
                    cur.push(ch);
                } else if !cur.is_empty() {
                    groups.push(std::mem::take(&mut cur));
                }
            }
            if !cur.is_empty() {
                groups.push(cur);
            }
            // ...searched as a phrase of its tokens.
            for g in groups {
                let words = split(&g);
                if !words.is_empty() {
                    out.push(words);
                }
            }
        }
    }
    out
}

fn term_sql(t: &[String], prefix: bool) -> String {
    let star = prefix && t.iter().map(|w| w.chars().count()).sum::<usize>() >= 4;
    format!("\"{}\"{}", t.join(" "), if star { "*" } else { "" })
}

/// Words of the query as quoted FTS5 terms; `"exact phrases"` and identifiers stay phrases.
pub fn fts_query(text: &str, any: bool) -> Option<String> {
    let t = terms(text);
    if t.is_empty() {
        return None;
    }
    Some(t.iter().map(|x| term_sql(x, false)).collect::<Vec<_>>().join(if any { " OR " } else { " " }))
}

/// Whitespace words that look like identifiers: a commit sha (≥7 hex with a
/// digit), a path (`/` or `.ext`), snake_case / ERROR_CODE / kebab-case, CamelCase.
pub fn identifier_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '/' || c == '.')).trim_end_matches('.'))
        .filter(|w| w.chars().count() >= 3 && looks_like_identifier(w))
        .map(str::to_string)
        .collect()
}

pub fn looks_like_identifier(w: &str) -> bool {
    let sha = w.len() >= 7 && w.chars().all(|c| c.is_ascii_hexdigit()) && w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic());
    let path = w.contains('/') || w.rsplit_once('.').map(|(a, e)| !a.is_empty() && (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false);
    let inner = |c: char| w.trim_matches(c).contains(c);
    let joined = inner('_') || (inner('-') && w.chars().any(|c| c.is_ascii_alphabetic()));
    let b: Vec<char> = w.chars().collect();
    let camel = b.windows(2).any(|p| p[0].is_lowercase() && p[1].is_uppercase());
    sha || path || joined || camel
}

pub fn date_to_ts(d: &str) -> Option<i64> {
    crate::util::parse_iso(&format!("{d}T00:00:00Z"))
}

/// One planned tier: its label and what is sent to SQLite (an FTS5 MATCH
/// expression for `table`, or the lowercased substrings for `substring`).
#[derive(Clone, Debug)]
pub struct Tier {
    pub label: &'static str,
    pub table: &'static str,
    pub expr: String,
    pub needles: Vec<String>,
}

impl Tier {
    /// Human-readable form of what the tier sends.
    pub fn shown(&self) -> String {
        if self.label == "substring" {
            format!("contains {}", self.needles.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(" AND "))
        } else {
            format!("{} MATCH '{}'", self.table, self.expr)
        }
    }
}

/// The tiers a query runs, in order. Tiers identical to an earlier one are skipped.
pub fn plan(q: &Query) -> Vec<Tier> {
    let t = terms(&q.text);
    let mut out = Vec::new();
    if t.is_empty() {
        return out;
    }
    let exact = t.iter().map(|x| term_sql(x, false)).collect::<Vec<_>>().join(" ");
    let fts = |label, table, expr: String| Tier { label, table, expr, needles: vec![] };
    out.push(fts("exact", "passages_fts", exact.clone()));
    if q.exact {
        return out;
    }
    let prefix = t.iter().map(|x| term_sql(x, true)).collect::<Vec<_>>().join(" ");
    if prefix != exact {
        out.push(fts("prefix", "passages_fts", prefix));
    }
    out.push(fts("stem", "passages_stem", exact));
    if q.any && t.len() > 1 {
        out.push(fts("any", "passages_stem", t.iter().map(|x| term_sql(x, false)).collect::<Vec<_>>().join(" OR ")));
    }
    if !identifier_words(&q.text).is_empty() {
        let needles: Vec<String> = q
            .text
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '/' || c == '.')).trim_end_matches('.').to_lowercase())
            .filter(|w| w.chars().count() >= 2)
            .collect();
        if !needles.is_empty() {
            out.push(Tier { label: "substring", table: "", expr: String::new(), needles });
        }
    }
    out
}

/// Where to search: one project (repo id), optionally one checkout; `prefer`
/// ranks that checkout's hits first and wins among identical passages.
#[derive(Default, Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub repo: Option<&'a str>,
    pub checkout: Option<&'a str>,
    pub prefer: Option<&'a str>,
}

/// WHERE clauses for scope and filters (shared by every tier, so a fallback
/// tier can never escape `--project/--worktree/--kind/--speaker/--since`).
fn filters(q: &Query, s: &Scope) -> (Vec<String>, Vec<SqlValue>) {
    let mut wh: Vec<String> = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if let Some(r) = s.repo {
        wh.push("p.repo_id = ?".into());
        args.push(SqlValue::Text(r.into()));
    }
    if let Some(co) = s.checkout {
        wh.push("(p.checkout_id = ? OR p.checkout_id IS NULL)".into());
        args.push(SqlValue::Text(co.into()));
    }
    let kinds: Vec<&String> = q.kinds.iter().filter(|k| k.as_str() != "file").collect();
    if !q.kinds.is_empty() {
        if kinds.is_empty() {
            wh.push("0".into());
        } else {
            wh.push(format!("p.kind IN ({})", kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")));
            args.extend(kinds.iter().map(|k| SqlValue::Text(k.to_string())));
        }
    }
    if let Some(sp) = &q.speaker {
        wh.push("p.speaker = ?".into());
        args.push(SqlValue::Text(sp.clone()));
    }
    if let Some(p) = &q.provider {
        wh.push("p.heading LIKE ? AND p.kind='message'".into());
        args.push(SqlValue::Text(format!("{p}%")));
    }
    if let Some(t) = q.since {
        wh.push("coalesce(p.ts,0) >= ?".into());
        args.push(SqlValue::Integer(t));
    }
    (wh, args)
}

/// Predicate selecting passages matched by one tier (no bm25 needed).
fn tier_pred(t: &Tier, args: &mut Vec<SqlValue>) -> String {
    if t.label == "substring" {
        let mut parts = Vec::new();
        for n in &t.needles {
            parts.push("instr(lower(p.title || ' ' || p.text || ' ' || p.loc), ?) > 0".to_string());
            args.push(SqlValue::Text(n.clone()));
        }
        format!("({})", parts.join(" AND "))
    } else {
        args.push(SqlValue::Text(t.expr.clone()));
        format!("p.id IN (SELECT rowid FROM {tb} WHERE {tb} MATCH ?)", tb = t.table)
    }
}

/// Whether file nodes take part (path-like substring queries without
/// message-only filters).
fn files_apply(q: &Query, t: &Tier) -> bool {
    t.label == "substring" && q.speaker.is_none() && q.provider.is_none() && (q.kinds.is_empty() || q.kinds.iter().any(|k| k == "file")) && t.needles.iter().any(|n| n.contains('/') || n.contains('.'))
}

fn file_filters(q: &Query, t: &Tier, s: &Scope) -> (String, Vec<SqlValue>) {
    let mut wh = vec!["n.kind='file'".to_string(), "n.repo_id IS NOT NULL".to_string()];
    let mut args = Vec::new();
    for n in &t.needles {
        wh.push("instr(lower(n.label), ?) > 0".into());
        args.push(SqlValue::Text(n.clone()));
    }
    if let Some(r) = s.repo {
        wh.push("n.repo_id = ?".into());
        args.push(SqlValue::Text(r.into()));
    }
    if let Some(co) = s.checkout {
        wh.push("n.checkout_id = ?".into());
        args.push(SqlValue::Text(co.into()));
    }
    if let Some(ts) = q.since {
        wh.push("coalesce(n.ts,0) >= ?".into());
        args.push(SqlValue::Integer(ts));
    }
    (wh.join(" AND "), args)
}

/// Distinct passages (+ file nodes) matched by any of `tiers` within the scope.
pub fn count(c: &Connection, q: &Query, s: &Scope, tiers: &[Tier]) -> rusqlite::Result<i64> {
    Ok(groups(c, q, s, tiers)?.iter().map(|g| g.2).sum())
}

/// Like [`count`], grouped by (repo id, checkout id).
pub fn groups(c: &Connection, q: &Query, s: &Scope, tiers: &[Tier]) -> rusqlite::Result<Vec<(Option<String>, Option<String>, i64)>> {
    if tiers.is_empty() {
        return Ok(vec![]);
    }
    let (mut wh, mut args) = filters(q, s);
    let mut ors = Vec::new();
    for t in tiers {
        ors.push(tier_pred(t, &mut args));
    }
    wh.push(format!("({})", ors.join(" OR ")));
    let sql = format!("SELECT p.repo_id, p.checkout_id, count(*) FROM passages p WHERE {} GROUP BY 1,2 ORDER BY 3 DESC, 1, 2", wh.join(" AND "));
    let mut st = c.prepare(&sql)?;
    let mut out: Vec<(Option<String>, Option<String>, i64)> = st.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    for t in tiers.iter().filter(|t| files_apply(q, t)) {
        let (fw, fa) = file_filters(q, t, s);
        let mut st = c.prepare(&format!("SELECT n.repo_id, n.checkout_id, count(*) FROM nodes n WHERE {fw} GROUP BY 1,2"))?;
        for g in st.query_map(rusqlite::params_from_iter(fa.iter()), |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)))? {
            let g = g?;
            match out.iter_mut().find(|o| o.0 == g.0 && o.1 == g.1) {
                Some(o) => o.2 += g.2,
                None => out.push(g),
            }
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1)));
    Ok(out)
}

fn pref_sql(prefer: Option<&str>) -> String {
    prefer.map(|p| format!("'{}'", p.replace('\'', "''"))).unwrap_or_else(|| "NULL".into())
}

fn row_hit(r: &rusqlite::Row, tier: &'static str) -> rusqlite::Result<Hit> {
    Ok(Hit {
        kind: r.get(0)?,
        node_id: r.get(1)?,
        repo_id: r.get(2)?,
        checkout_id: r.get(3)?,
        source_path: r.get(4)?,
        line: r.get(5)?,
        loc: r.get(6)?,
        speaker: r.get(7)?,
        ts: r.get(8)?,
        title: r.get(9)?,
        heading: r.get(10)?,
        snippet: r.get(11)?,
        dups: 0,
        tier,
    })
}

/// Rows of one tier in rank order: bm25 (substring: none), then the preferred
/// checkout, then a main checkout, then newest, then stable ids.
fn fetch(c: &Connection, q: &Query, s: &Scope, t: &Tier, n: usize) -> rusqlite::Result<Vec<Hit>> {
    let (mut wh, mut args) = filters(q, s);
    let cols = "p.kind, p.node_id, p.repo_id, p.checkout_id, p.source_path, p.line, p.loc, p.speaker, coalesce(p.ts,0), p.title, p.heading";
    let rank = format!(
        "CASE WHEN p.checkout_id = {pref} THEN 0 WHEN (SELECT json_extract(meta,'$.main') FROM nodes WHERE id=p.checkout_id) THEN 1 ELSE 2 END, coalesce(p.ts,0) DESC, p.node_id, p.loc, p.part, p.id",
        pref = pref_sql(s.prefer)
    );
    let sql = if t.label == "substring" {
        let pred = tier_pred(t, &mut args);
        wh.push(pred);
        format!("SELECT {cols}, substr(p.text, 1, 200) FROM passages p WHERE {} ORDER BY {rank} LIMIT {n}", wh.join(" AND "))
    } else {
        wh.insert(0, format!("{tb} MATCH ?", tb = t.table));
        args.insert(0, SqlValue::Text(t.expr.clone()));
        format!(
            "SELECT {cols}, snippet({tb}, -1, '[', ']', '…', 14), round(bm25({tb}, 3.0, 1.0), 6) AS score
             FROM {tb} JOIN passages p ON p.id={tb}.rowid WHERE {} ORDER BY score, {rank} LIMIT {n}",
            wh.join(" AND "),
            tb = t.table
        )
    };
    let mut st = c.prepare(&sql)?;
    let mut out: Vec<Hit> = st.query_map(rusqlite::params_from_iter(args.iter()), |r| row_hit(r, t.label))?.collect::<Result<_, _>>()?;
    if t.label == "substring" {
        for h in &mut out {
            h.snippet = mark_substring(&h.snippet, &h.loc, &t.needles);
        }
        if files_apply(q, t) {
            let (fw, fa) = file_filters(q, t, s);
            let sql = format!(
                "SELECT 'file', n.id, n.repo_id, n.checkout_id, coalesce(n.path,''), 0, '', '', coalesce(n.ts,0), n.label, '', n.label FROM nodes n WHERE {fw}
                 ORDER BY CASE WHEN n.checkout_id = {pref} THEN 0 WHEN (SELECT json_extract(meta,'$.main') FROM nodes WHERE id=n.checkout_id) THEN 1 ELSE 2 END, n.label, n.id LIMIT {n}",
                pref = pref_sql(s.prefer)
            );
            let mut st = c.prepare(&sql)?;
            let files: Vec<Hit> = st.query_map(rusqlite::params_from_iter(fa.iter()), |r| row_hit(r, "substring"))?.collect::<Result<_, _>>()?;
            out.extend(files);
        }
    }
    Ok(out)
}

/// Snippet around the first needle, `[marked]` like FTS snippets.
fn mark_substring(text: &str, loc: &str, needles: &[String]) -> String {
    let lower = text.to_lowercase();
    for n in needles {
        if lower.len() == text.len() {
            if let Some(i) = lower.find(n.as_str()) {
                let start = text[..i].char_indices().rev().nth(60).map(|x| x.0).unwrap_or(0);
                let end = i + n.len();
                let tail: String = text[end..].chars().take(80).collect();
                return format!("{}{}[{}]{tail}", if start > 0 { "…" } else { "" }, &text[start..i], &text[i..end]);
            }
        }
        if loc.to_lowercase().starts_with(n.as_str()) {
            return format!("[{loc}] {text}");
        }
    }
    text.to_string()
}

fn same_hit(o: &Hit, h: &Hit) -> bool {
    o.kind == h.kind && ((o.node_id == h.node_id && o.loc == h.loc) || ((h.kind == "doc" || h.kind == "file") && o.repo_id == h.repo_id && o.title == h.title && o.loc == h.loc))
}

/// Result of a tiered search.
pub struct Outcome {
    pub hits: Vec<Hit>,
    /// Distinct matching passages (+ file nodes) over all planned tiers.
    pub total: i64,
    /// Planned tiers with their own totals in scope.
    pub tiers: Vec<(Tier, i64)>,
}

/// Tiered search: tiers fill up to the limit in plan order; each hit keeps
/// the first tier that found it. With `prefer`, hits of that checkout (and
/// project-level hits) come first within each tier.
pub fn search(c: &Connection, q: &Query, s: &Scope) -> rusqlite::Result<Outcome> {
    let plan = plan(q);
    let limit = q.limit.max(1);
    let mut tiers = Vec::new();
    for t in &plan {
        let n = count(c, q, s, std::slice::from_ref(t))?;
        tiers.push((t.clone(), n));
    }
    let total = count(c, q, s, &plan)?;
    let mut out: Vec<Hit> = Vec::new();
    for (t, n) in &tiers {
        if out.len() >= limit {
            break;
        }
        if *n == 0 {
            continue;
        }
        let mut found: Vec<Hit> = Vec::new();
        for h in fetch(c, q, s, t, limit * 6 + 50)? {
            if let Some(o) = out.iter_mut().chain(found.iter_mut()).find(|o| same_hit(o, &h)) {
                if o.node_id != h.node_id && o.tier == h.tier {
                    o.dups += 1;
                }
                continue;
            }
            found.push(h);
        }
        if let Some(p) = s.prefer {
            // Stable: current worktree (and project-level) first, rank order kept within each group.
            found.sort_by_key(|h| h.checkout_id.as_deref().map(|co| co != p).unwrap_or(false));
        }
        out.extend(found);
    }
    out.truncate(limit);
    Ok(Outcome { hits: out, total, tiers })
}

/// Run a query, optionally limited to one project (repo id) or one checkout.
/// Returns (hits after collapsing duplicates, total matching passages).
/// Single FTS pass (exact tokens; any word with `q.any`), no fallback tiers.
pub fn run(c: &Connection, q: &Query, repo: Option<&str>, checkout: Option<&str>) -> rusqlite::Result<(Vec<Hit>, i64)> {
    run_pref(c, q, repo, checkout, None)
}

/// [`run`] where, among identical passages (the same doc in several
/// worktrees), the one in `prefer` wins, then the one in a main checkout.
pub fn run_pref(c: &Connection, q: &Query, repo: Option<&str>, checkout: Option<&str>, prefer: Option<&str>) -> rusqlite::Result<(Vec<Hit>, i64)> {
    run_pref_in(c, q, repo, checkout, prefer, false)
}

/// [`run_pref`]; with `text_only`, a passage's title (a message's conversation
/// label, a doc's path) is not searched, only its text.
pub fn run_pref_in(c: &Connection, q: &Query, repo: Option<&str>, checkout: Option<&str>, prefer: Option<&str>, text_only: bool) -> rusqlite::Result<(Vec<Hit>, i64)> {
    let Some(m) = fts_query(&q.text, q.any) else { return Ok((vec![], 0)) };
    let m = if text_only { format!("text : ({m})") } else { m };
    let t = Tier { label: if q.any { "any" } else { "exact" }, table: "passages_fts", expr: m, needles: vec![] };
    let s = Scope { repo, checkout, prefer };
    let total = count(c, q, &s, std::slice::from_ref(&t))?;
    let mut out: Vec<Hit> = Vec::new();
    for h in fetch(c, q, &s, &t, q.limit.max(1) * 6 + 50)? {
        // One hit per message/section/commit; the same doc section in several
        // worktrees (same repo, path, heading) collapses into the first.
        match out.iter_mut().find(|o| same_hit(o, &h)) {
            Some(o) => {
                if o.node_id != h.node_id {
                    o.dups += 1;
                }
            }
            None => out.push(h),
        }
    }
    out.truncate(q.limit.max(1));
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_quoted_terms() {
        assert_eq!(fts_query("parser timeout", false).as_deref(), Some("\"parser\" \"timeout\""));
        assert_eq!(fts_query("a b", true).as_deref(), Some("\"a\" OR \"b\""));
        assert_eq!(fts_query("say \"exact phrase\" now", false).as_deref(), Some("\"say\" \"exact phrase\" \"now\""));
        assert_eq!(fts_query("NEAR(x) * ^", false).as_deref(), Some("\"near\" \"x\""));
        assert!(fts_query("  ** ", false).is_none());
        // Identifiers stay one phrase of their tokens.
        assert_eq!(fts_query("src/retry.ts ERR_DUP_CHARGE", false).as_deref(), Some("\"src retry ts\" \"err dup charge\""));
    }

    #[test]
    fn plan_tiers_and_identifier_shapes() {
        let q = |t: &str| Query { text: t.into(), limit: 5, ..Default::default() };
        let labels = |q: &Query| plan(q).iter().map(|t| t.label).collect::<Vec<_>>();
        assert_eq!(labels(&q("deadlock")), ["exact", "prefix", "stem"]);
        assert_eq!(plan(&q("deadlock"))[1].expr, "\"deadlock\"*");
        assert_eq!(labels(&q("fix it")), ["exact", "stem"], "short words: prefix tier would equal exact");
        assert_eq!(labels(&Query { any: true, ..q("double charge") }), ["exact", "prefix", "stem", "any"]);
        assert_eq!(labels(&Query { exact: true, ..q("retry.ts") }), ["exact"]);
        assert_eq!(labels(&q("retry.ts")).last(), Some(&"substring"));
        for w in ["d6b3d89", "src/retry.ts", "retry.ts", "ERR_DUP_CHARGE", "idempotency_key", "retryBackoff", "dup-charge"] {
            assert!(looks_like_identifier(w), "{w}");
        }
        for w in ["deadlock", "charges", "decade", "1234567", "Deadlock"] {
            assert!(!looks_like_identifier(w), "{w}");
        }
    }
}
