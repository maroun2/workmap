//! SQLite derived index. Stores references, metadata and evidence pointers
//! (path + line), never full source files or transcripts.

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
CREATE TABLE IF NOT EXISTS scans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started INTEGER NOT NULL,
  finished INTEGER,
  stats TEXT NOT NULL DEFAULT '{}'
);
"#;

/// Bump when session parsers change what they extract.
pub const PARSER_VERSION: i64 = 2;

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
        c.execute("DELETE FROM source_files WHERE status IN ('parsed','error')", [])?;
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

pub fn clear_origin(c: &Connection, origin: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM edges WHERE origin=?1", [origin])?;
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
