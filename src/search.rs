//! Full-text search over indexed passages (SQLite FTS5, bm25 ranking with
//! deterministic tie-breaking). Searches project metadata, doc sections,
//! commit subjects, remote catalog entries and individual conversation
//! messages. Code search stays with your editor or `rg`.

use rusqlite::{types::Value as SqlValue, Connection};

pub const KINDS: &[&str] = &["project", "doc", "commit", "message", "remote"];

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
}

/// Words of the query as quoted FTS5 terms; `"exact phrases"` stay phrases.
pub fn fts_query(text: &str, any: bool) -> Option<String> {
    let mut terms = Vec::new();
    for (i, seg) in text.split('"').enumerate() {
        let words: Vec<String> = seg.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect();
        if i % 2 == 1 && words.len() > 1 {
            terms.push(format!("\"{}\"", words.join(" ")));
        } else {
            terms.extend(words.into_iter().map(|w| format!("\"{w}\"")));
        }
    }
    if terms.is_empty() {
        return None;
    }
    Some(terms.join(if any { " OR " } else { " " }))
}

pub fn date_to_ts(d: &str) -> Option<i64> {
    crate::util::parse_iso(&format!("{d}T00:00:00Z"))
}

/// Run a query, optionally limited to one project (repo id) or one checkout.
/// Returns (hits after collapsing duplicates, total matching passages).
pub fn run(c: &Connection, q: &Query, repo: Option<&str>, checkout: Option<&str>) -> rusqlite::Result<(Vec<Hit>, i64)> {
    let Some(m) = fts_query(&q.text, q.any) else { return Ok((vec![], 0)) };
    let mut wh = vec!["passages_fts MATCH ?".to_string()];
    let mut args: Vec<SqlValue> = vec![SqlValue::Text(m)];
    if let Some(r) = repo {
        wh.push("p.repo_id = ?".into());
        args.push(SqlValue::Text(r.into()));
    }
    if let Some(co) = checkout {
        wh.push("(p.checkout_id = ? OR p.checkout_id IS NULL)".into());
        args.push(SqlValue::Text(co.into()));
    }
    if !q.kinds.is_empty() {
        wh.push(format!("p.kind IN ({})", q.kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",")));
        args.extend(q.kinds.iter().map(|k| SqlValue::Text(k.clone())));
    }
    if let Some(s) = &q.speaker {
        wh.push("p.speaker = ?".into());
        args.push(SqlValue::Text(s.clone()));
    }
    if let Some(p) = &q.provider {
        wh.push("p.heading LIKE ? AND p.kind='message'".into());
        args.push(SqlValue::Text(format!("{p}%")));
    }
    if let Some(t) = q.since {
        wh.push("coalesce(p.ts,0) >= ?".into());
        args.push(SqlValue::Integer(t));
    }
    let wsql = wh.join(" AND ");
    let total: i64 = c.query_row(&format!("SELECT count(*) FROM passages_fts JOIN passages p ON p.id=passages_fts.rowid WHERE {wsql}"), rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
    let fetch = (q.limit.max(1) * 6 + 50) as i64;
    let sql = format!(
        "SELECT p.kind, p.node_id, p.repo_id, p.checkout_id, p.source_path, p.line, p.loc, p.speaker, coalesce(p.ts,0), p.title, p.heading,
                snippet(passages_fts, 1, '[', ']', '…', 14), round(bm25(passages_fts, 3.0, 1.0), 6) AS score, p.id
         FROM passages_fts JOIN passages p ON p.id=passages_fts.rowid WHERE {wsql}
         ORDER BY score, coalesce(p.ts,0) DESC, p.node_id, p.loc, p.part LIMIT {fetch}"
    );
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
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
        })
    })?;
    let mut out: Vec<Hit> = Vec::new();
    for h in rows {
        let h = h?;
        // One hit per message/section/commit; the same doc section in several
        // worktrees (same repo, path, heading) collapses into the first.
        let dup = out.iter_mut().find(|o| o.kind == h.kind && ((o.node_id == h.node_id && o.loc == h.loc) || (h.kind == "doc" && o.repo_id == h.repo_id && o.title == h.title && o.loc == h.loc)));
        match dup {
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
    }
}
