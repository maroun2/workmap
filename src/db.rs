//! SQLite derived index. Stores references, metadata and evidence pointers
//! (path + line), plus bounded, redacted searchable passages (doc sections,
//! commit subjects, natural-language conversation messages). It never stores
//! whole source files, tool payloads or credentials.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
CREATE TABLE IF NOT EXISTS nodes (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,          -- repo | checkout | file | commit | session | remote | branch
  label TEXT NOT NULL,
  path TEXT,                   -- absolute path when the node is a filesystem object
  repo_id TEXT,
  checkout_id TEXT,
  ts INTEGER,                  -- most relevant timestamp (unix seconds)
  meta TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS nodes_kind ON nodes(kind);
CREATE INDEX IF NOT EXISTS nodes_path ON nodes(path);
CREATE INDEX IF NOT EXISTS nodes_checkout ON nodes(checkout_id);
CREATE TABLE IF NOT EXISTS edges (
  src TEXT NOT NULL,
  dst TEXT NOT NULL,
  kind TEXT NOT NULL,          -- worktree_of | references | entrypoint | touched | read | write | mentioned | ran_in | remote | branch
  strength TEXT NOT NULL,      -- structural | observed | requested | mentioned | inferred
  source_path TEXT NOT NULL DEFAULT '',
  source_line INTEGER NOT NULL DEFAULT 0,
  ts INTEGER,
  note TEXT NOT NULL DEFAULT '',
  origin TEXT NOT NULL,        -- what produced the edge (checkout id or source file); replaced as a unit
  PRIMARY KEY (src, dst, kind, strength, source_path, source_line)
);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE INDEX IF NOT EXISTS edges_origin ON edges(origin);
CREATE TABLE IF NOT EXISTS excerpts (
  node_id TEXT NOT NULL,
  line INTEGER NOT NULL,
  heading TEXT NOT NULL,
  text TEXT NOT NULL,
  PRIMARY KEY (node_id, line)
);
CREATE TABLE IF NOT EXISTS source_files (
  path TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  cwd TEXT,
  status TEXT NOT NULL,        -- parsed | unrelated | missing | error
  session_id TEXT,
  warnings INTEGER NOT NULL DEFAULT 0
);
-- Bounded searchable text. `origin` matches the edge origin that produced it,
-- so re-indexing a checkout or re-parsing a transcript replaces it as a unit.
CREATE TABLE IF NOT EXISTS passages (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,          -- project | doc | commit | message | remote
  node_id TEXT NOT NULL,       -- repo / doc / commit / session / remote node
  repo_id TEXT,
  checkout_id TEXT,
  source_path TEXT NOT NULL DEFAULT '',
  line INTEGER NOT NULL DEFAULT 0,
  loc TEXT NOT NULL DEFAULT '', -- message locator: l<transcript line> or p<part ordinal>
  part INTEGER NOT NULL DEFAULT 0,
  speaker TEXT NOT NULL DEFAULT '',
  ts INTEGER,
  title TEXT NOT NULL DEFAULT '',
  heading TEXT NOT NULL DEFAULT '',
  text TEXT NOT NULL,
  origin TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS passages_origin ON passages(origin);
CREATE INDEX IF NOT EXISTS passages_node ON passages(node_id, loc);
CREATE INDEX IF NOT EXISTS passages_repo ON passages(repo_id);
CREATE VIRTUAL TABLE IF NOT EXISTS passages_fts USING fts5(title, text, content='passages', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS passages_ai AFTER INSERT ON passages BEGIN
  INSERT INTO passages_fts(rowid, title, text) VALUES (new.id, new.title, new.text);
END;
CREATE TRIGGER IF NOT EXISTS passages_ad AFTER DELETE ON passages BEGIN
  INSERT INTO passages_fts(passages_fts, rowid, title, text) VALUES ('delete', old.id, old.title, old.text);
END;
CREATE TRIGGER IF NOT EXISTS passages_au AFTER UPDATE ON passages BEGIN
  INSERT INTO passages_fts(passages_fts, rowid, title, text) VALUES ('delete', old.id, old.title, old.text);
  INSERT INTO passages_fts(rowid, title, text) VALUES (new.id, new.title, new.text);
END;
-- Raw cross-project link candidates found while indexing a checkout; resolved
-- against the full project set by the relation pass on every scan.
CREATE TABLE IF NOT EXISTS raw_links (
  origin TEXT NOT NULL,
  repo_id TEXT NOT NULL,
  checkout_id TEXT NOT NULL,
  kind TEXT NOT NULL,          -- cargo_path | cargo_git | npm_path | npm_git | py_path | py_git | submodule | symlink | doc_url | doc_path | dep_name | pkg_name
  raw TEXT NOT NULL,
  resolved TEXT NOT NULL DEFAULT '',
  source_path TEXT NOT NULL,
  line INTEGER NOT NULL,
  note TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS raw_links_origin ON raw_links(origin);
-- Git blob ids of substantive tracked files (identical-content detection).
CREATE TABLE IF NOT EXISTS file_hashes (
  checkout_id TEXT NOT NULL,
  repo_id TEXT NOT NULL,
  rel TEXT NOT NULL,
  blob TEXT NOT NULL,
  size INTEGER NOT NULL,
  PRIMARY KEY (checkout_id, rel)
);
CREATE INDEX IF NOT EXISTS file_hashes_blob ON file_hashes(blob);
-- Remote repository catalog from authenticated gh/glab (metadata only, local).
CREATE TABLE IF NOT EXISTS remote_repos (
  provider TEXT NOT NULL,
  host TEXT NOT NULL,
  identity TEXT NOT NULL,      -- host/namespace/name, lowercase
  namespace TEXT NOT NULL,
  name TEXT NOT NULL,
  url TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  visibility TEXT NOT NULL DEFAULT '',
  archived INTEGER NOT NULL DEFAULT 0,
  default_branch TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL DEFAULT '',
  fetched_at INTEGER NOT NULL,
  PRIMARY KEY (provider, host, identity)
);
CREATE TABLE IF NOT EXISTS remote_hosts (
  provider TEXT NOT NULL,
  host TEXT NOT NULL,
  status TEXT NOT NULL,        -- ok | cached | failed | unauthenticated
  fetched_at INTEGER NOT NULL DEFAULT 0,
  checked_at INTEGER NOT NULL DEFAULT 0,
  count INTEGER NOT NULL DEFAULT 0,
  message TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (provider, host)
);
-- Opt-in shallow snapshots of remote-only repositories: what was indexed, at
-- which commit, by which indexer version. A failed refresh keeps the old data.
CREATE TABLE IF NOT EXISTS remote_snapshots (
  identity TEXT PRIMARY KEY,
  branch TEXT NOT NULL,
  sha TEXT NOT NULL DEFAULT '',          -- commit whose data is stored ('' = none yet)
  version INTEGER NOT NULL DEFAULT 0,
  indexed_at INTEGER NOT NULL DEFAULT 0,
  checked_at INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL,                  -- ok | failed
  error TEXT NOT NULL DEFAULT '',
  files INTEGER NOT NULL DEFAULT 0,
  docs INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS scans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started INTEGER NOT NULL,
  finished INTEGER,
  stats TEXT NOT NULL DEFAULT '{}'
);
"#;

/// Bump when session parsers or derived passages change what they extract.
pub const PARSER_VERSION: i64 = 4;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    c.execute_batch(SCHEMA)?;
    // Parser changes invalidate cached parse results (sources get re-read once).
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v != PARSER_VERSION {
        // Keep the rows (so a source deleted meanwhile is still flagged missing),
        // but force a re-parse; and force checkouts to rebuild docs/commit passages.
        c.execute("UPDATE source_files SET mtime=-1 WHERE status IN ('parsed','error')", [])?;
        c.execute("UPDATE nodes SET meta=json_remove(meta,'$.sig') WHERE kind='checkout'", [])?;
        c.execute_batch(&format!("PRAGMA user_version={PARSER_VERSION};"))?;
    }
    repair_trailing_slash_files(&c)?;
    Ok(c)
}

/// Earlier builds could store session file paths with a trailing '/' (path alias
/// resolution joined an empty tail). Fold such nodes into the real file node.
fn repair_trailing_slash_files(c: &Connection) -> rusqlite::Result<()> {
    let bad: Vec<(String, String, String)> = {
        let mut st = c.prepare("SELECT id, path, label FROM nodes WHERE kind='file' AND path LIKE '%/'")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (old, path, label) in bad {
        let fixed = path.trim_end_matches('/').to_string();
        let new = crate::util::stable_id("file", &fixed);
        c.execute(
            "INSERT OR IGNORE INTO nodes(id,kind,label,path,repo_id,checkout_id,ts,meta) SELECT ?2,kind,?3,?4,repo_id,checkout_id,ts,meta FROM nodes WHERE id=?1",
            params![old, new, label.trim_end_matches('/'), fixed],
        )?;
        c.execute("UPDATE OR REPLACE edges SET dst=?2 WHERE dst=?1", params![old, new])?;
        c.execute("DELETE FROM edges WHERE dst=?1", [&old])?;
        c.execute("DELETE FROM nodes WHERE id=?1", [&old])?;
    }
    Ok(())
}

pub struct NodeRow<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub label: &'a str,
    pub path: Option<&'a str>,
    pub repo_id: Option<&'a str>,
    pub checkout_id: Option<&'a str>,
    pub ts: Option<i64>,
    pub meta: &'a serde_json::Value,
}

pub fn upsert_node(c: &Connection, n: &NodeRow) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO nodes(id,kind,label,path,repo_id,checkout_id,ts,meta) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,label=excluded.label,path=excluded.path,
           repo_id=coalesce(excluded.repo_id,nodes.repo_id),checkout_id=coalesce(excluded.checkout_id,nodes.checkout_id),
           ts=max(coalesce(excluded.ts,0),coalesce(nodes.ts,0)),meta=excluded.meta",
        params![n.id, n.kind, n.label, n.path, n.repo_id, n.checkout_id, n.ts, n.meta.to_string()],
    )?;
    Ok(())
}

/// File node keyed by absolute path; does not overwrite richer metadata.
pub fn ensure_file_node(c: &Connection, path: &str, repo_id: Option<&str>, checkout_id: Option<&str>, rel: &str) -> rusqlite::Result<String> {
    let id = crate::util::stable_id("file", path);
    c.execute(
        "INSERT INTO nodes(id,kind,label,path,repo_id,checkout_id,ts,meta) VALUES(?1,'file',?2,?3,?4,?5,NULL,'{}')
         ON CONFLICT(id) DO UPDATE SET repo_id=coalesce(nodes.repo_id,excluded.repo_id),checkout_id=coalesce(nodes.checkout_id,excluded.checkout_id)",
        params![id, rel, path, repo_id, checkout_id],
    )?;
    Ok(id)
}

pub struct EdgeRow<'a> {
    pub src: &'a str,
    pub dst: &'a str,
    pub kind: &'a str,
    pub strength: &'a str,
    pub source_path: &'a str,
    pub source_line: i64,
    pub ts: Option<i64>,
    pub note: &'a str,
    pub origin: &'a str,
}

pub fn add_edge(c: &Connection, e: &EdgeRow) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO edges(src,dst,kind,strength,source_path,source_line,ts,note,origin) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![e.src, e.dst, e.kind, e.strength, e.source_path, e.source_line, e.ts, e.note, e.origin],
    )?;
    Ok(())
}

/// Drop everything one producer (a checkout id or `src:<transcript>`) wrote.
pub fn clear_origin(c: &Connection, origin: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM edges WHERE origin=?1", [origin])?;
    c.execute("DELETE FROM passages WHERE origin=?1", [origin])?;
    c.execute("DELETE FROM raw_links WHERE origin=?1", [origin])?;
    c.execute("DELETE FROM file_hashes WHERE checkout_id=?1", [origin])?;
    Ok(())
}

pub struct Passage<'a> {
    pub kind: &'a str,
    pub node_id: &'a str,
    pub repo_id: Option<&'a str>,
    pub checkout_id: Option<&'a str>,
    pub source_path: &'a str,
    pub line: i64,
    pub loc: &'a str,
    pub part: i64,
    pub speaker: &'a str,
    pub ts: Option<i64>,
    pub title: &'a str,
    pub heading: &'a str,
    pub text: &'a str,
    pub origin: &'a str,
}

pub fn add_passage(c: &Connection, p: &Passage) -> rusqlite::Result<()> {
    if p.text.trim().is_empty() {
        return Ok(());
    }
    c.execute(
        "INSERT INTO passages(kind,node_id,repo_id,checkout_id,source_path,line,loc,part,speaker,ts,title,heading,text,origin) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![p.kind, p.node_id, p.repo_id, p.checkout_id, p.source_path, p.line, p.loc, p.part, p.speaker, p.ts, p.title, p.heading, p.text, p.origin],
    )?;
    Ok(())
}

pub struct RawLink<'a> {
    pub origin: &'a str,
    pub repo_id: &'a str,
    pub checkout_id: &'a str,
    pub kind: &'a str,
    pub raw: &'a str,
    pub resolved: &'a str,
    pub source_path: &'a str,
    pub line: i64,
    pub note: &'a str,
}

pub fn add_raw_link(c: &Connection, l: &RawLink) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO raw_links(origin,repo_id,checkout_id,kind,raw,resolved,source_path,line,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![l.origin, l.repo_id, l.checkout_id, l.kind, l.raw, l.resolved, l.source_path, l.line, l.note],
    )?;
    Ok(())
}

pub fn source_state(c: &Connection, path: &str) -> rusqlite::Result<Option<(i64, i64, String, Option<String>)>> {
    c.query_row("SELECT size,mtime,status,cwd FROM source_files WHERE path=?1", [path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()
}

#[allow(clippy::too_many_arguments)]
pub fn set_source(c: &Connection, path: &str, provider: &str, size: i64, mtime: i64, cwd: Option<&str>, status: &str, session_id: Option<&str>, warnings: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO source_files(path,provider,size,mtime,cwd,status,session_id,warnings) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![path, provider, size, mtime, cwd, status, session_id, warnings],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn fts_passages_follow_inserts_and_deletes() {
        let t = crate::testutil::TempDir::new("fts");
        let c = super::open(&t.path().join("i.db")).unwrap();
        let p = super::Passage {
            kind: "doc",
            node_id: "n",
            repo_id: None,
            checkout_id: None,
            source_path: "",
            line: 1,
            loc: "",
            part: 0,
            speaker: "",
            ts: None,
            title: "T",
            heading: "",
            text: "the parser timeout bug",
            origin: "o",
        };
        super::add_passage(&c, &p).unwrap();
        let n = |c: &rusqlite::Connection| c.query_row("SELECT count(*) FROM passages_fts WHERE passages_fts MATCH 'timeout'", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(n(&c), 1);
        super::clear_origin(&c, "o").unwrap();
        assert_eq!(n(&c), 0, "deleted passages leave the FTS index");
    }
}
