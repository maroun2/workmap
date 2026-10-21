//! How much to trust what the index says: freshness of the last scan, the
//! bounded commit history, and per-project conversation coverage (truncated,
//! capped, missing or partly unparseable transcripts).

use crate::util::{fmt_ts, now_unix};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

pub struct Freshness {
    pub last: Option<i64>,
    pub commits: i64,
    pub sessions_indexed: bool,
}

pub fn stale_secs() -> i64 {
    std::env::var("WORKMAP_STALE_HOURS").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(48) * 3600
}

pub fn freshness(c: &Connection) -> Freshness {
    let row: Option<(i64, String)> = c.query_row("SELECT finished, stats FROM scans WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten();
    let stats: Value = row.as_ref().and_then(|r| serde_json::from_str(&r.1).ok()).unwrap_or(Value::Null);
    Freshness {
        last: row.map(|r| r.0),
        commits: stats.get("commits_per_worktree").and_then(|x| x.as_i64()).unwrap_or(30),
        sessions_indexed: stats.get("sessions_indexed").and_then(|x| x.as_bool()).unwrap_or(true),
    }
}

pub fn age(secs: i64) -> String {
    match secs {
        s if s < 3600 => format!("{}m ago", s.max(0) / 60),
        s if s < 48 * 3600 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// `Index: last scan … · history: newest N commits per worktree`, plus a
/// warning line when the index is older than the stale threshold.
pub fn index_line(c: &Connection) -> String {
    let f = freshness(c);
    match f.last {
        None => "Index: never scanned — run `workmap scan` first; everything below is empty or partial\n".into(),
        Some(t) => {
            let a = now_unix() - t;
            let mut s = format!(
                "Index: last scan {} ({}) · history: newest {} commits per worktree{}\n",
                fmt_ts(t),
                age(a),
                f.commits,
                if f.sessions_indexed { "" } else { " · conversations NOT refreshed (--no-sessions)" }
            );
            if a > stale_secs() {
                s.push_str(&format!("WARNING: index is stale ({} > {}h); run `workmap scan` — recent commits, docs and conversations may be missing\n", age(a), stale_secs() / 3600));
            }
            s
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct Coverage {
    pub sessions: i64,
    pub truncated: i64,
    pub capped: i64,
    pub missing: i64,
    pub with_warnings: i64,
    pub messages: i64,
}

/// Conversation coverage for one project (repo id) or the whole index.
pub fn coverage(c: &Connection, repo: Option<&str>) -> Coverage {
    let filter = if repo.is_some() { "kind='session' AND repo_id=?1" } else { "kind='session' AND ?1 IS NULL" };
    let q = |extra: &str| -> i64 { c.query_row(&format!("SELECT count(*) FROM nodes WHERE {filter}{extra}"), [repo], |r| r.get(0)).unwrap_or(0) };
    Coverage {
        sessions: q(""),
        truncated: q(" AND json_extract(meta,'$.truncated')"),
        capped: q(" AND json_extract(meta,'$.messages_capped')"),
        missing: q(" AND json_extract(meta,'$.source_missing')"),
        with_warnings: q(" AND json_extract(meta,'$.warnings') > 0"),
        messages: c
            .query_row(&format!("SELECT count(DISTINCT node_id || ' ' || loc) FROM passages WHERE kind='message' AND {}", if repo.is_some() { "repo_id=?1" } else { "?1 IS NULL" }), [repo], |r| {
                r.get(0)
            })
            .unwrap_or(0),
    }
}

pub fn coverage_line(cov: &Coverage) -> String {
    let mut gaps = Vec::new();
    if cov.truncated > 0 {
        gaps.push(format!("{} truncated at the size cap", cov.truncated));
    }
    if cov.capped > 0 {
        gaps.push(format!("{} with message text capped", cov.capped));
    }
    if cov.missing > 0 {
        gaps.push(format!("{} source transcript missing (evidence kept)", cov.missing));
    }
    if cov.with_warnings > 0 {
        gaps.push(format!("{} with unparseable lines", cov.with_warnings));
    }
    format!(
        "Coverage: {} conversation(s), {} searchable message(s){}\n",
        cov.sessions,
        cov.messages,
        if gaps.is_empty() { "; no truncation, missing sources or parse warnings".into() } else { format!("; {}", gaps.join(", ")) }
    )
}

/// Source files per provider and status (for `status`).
pub fn sources(c: &Connection) -> Vec<(String, String, i64)> {
    let Ok(mut st) = c.prepare("SELECT provider, status, count(*) FROM source_files GROUP BY 1,2 ORDER BY 1,2") else { return vec![] };
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|r| r.flatten().collect()).unwrap_or_default()
}
