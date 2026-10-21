//! Locations of state, persisted roots and provider sources.
//!
//! Everything derives from one home directory so tests can point `WORKMAP_HOME`
//! at a temporary fake home and never touch the real one.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Env {
    /// Home directory used for default scan, provider sources and install targets.
    pub home: PathBuf,
    /// True when `WORKMAP_HOME` overrides the real home (tests, demos).
    pub overridden: bool,
    pub state_dir: PathBuf,
    /// Explicit provider store locations (`WORKMAP_CLAUDE_DIR` etc.), captured once.
    pub provider_dirs: Vec<(&'static str, PathBuf)>,
}

pub const PROVIDER_VARS: &[&str] = &["WORKMAP_CLAUDE_DIR", "WORKMAP_CODEX_DIR", "WORKMAP_OPENCODE_DIR"];

impl Env {
    pub fn from_process() -> Env {
        let (home, overridden) = match std::env::var_os("WORKMAP_HOME") {
            Some(h) => (PathBuf::from(h), true),
            None => (std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")), false),
        };
        let state_dir = std::env::var_os("WORKMAP_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state/workmap"));
        let provider_dirs = PROVIDER_VARS.iter().filter_map(|k| std::env::var_os(k).map(|v| (*k, PathBuf::from(v)))).collect();
        Env { home, overridden, state_dir, provider_dirs }
    }

    #[cfg(test)]
    pub fn with_home(home: &Path) -> Env {
        Env { home: home.to_path_buf(), overridden: true, state_dir: home.join(".local/state/workmap"), provider_dirs: vec![] }
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("index.db")
    }

    pub fn roots_file(&self) -> PathBuf {
        self.state_dir.join("roots.txt")
    }

    fn provider(&self, var: &str) -> Option<PathBuf> {
        self.provider_dirs.iter().find(|(k, _)| *k == var).map(|(_, v)| v.clone())
    }

    pub fn claude_projects(&self) -> PathBuf {
        self.provider("WORKMAP_CLAUDE_DIR").unwrap_or_else(|| self.home.join(".claude/projects"))
    }

    pub fn codex_home(&self) -> PathBuf {
        self.provider("WORKMAP_CODEX_DIR").unwrap_or_else(|| self.home.join(".codex"))
    }

    pub fn opencode_dir(&self) -> PathBuf {
        self.provider("WORKMAP_OPENCODE_DIR").unwrap_or_else(|| self.home.join(".local/share/opencode"))
    }

    /// Variables generated commands/units must carry so they see the same
    /// index, roots and sources as the process that wrote them.
    pub fn exports(&self) -> Vec<(String, String)> {
        let mut v = vec![("WORKMAP_STATE_DIR".to_string(), self.state_dir.display().to_string())];
        if self.overridden {
            v.push(("WORKMAP_HOME".into(), self.home.display().to_string()));
        }
        for (k, p) in &self.provider_dirs {
            v.push((k.to_string(), p.display().to_string()));
        }
        v
    }

    /// Persisted extra roots, one absolute path per line.
    pub fn load_roots(&self) -> Vec<PathBuf> {
        std::fs::read_to_string(self.roots_file()).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(PathBuf::from).collect()
    }

    pub fn add_root(&self, root: &Path) -> std::io::Result<bool> {
        let canon = std::fs::canonicalize(root)?;
        let mut roots = self.load_roots();
        if roots.contains(&canon) {
            return Ok(false);
        }
        roots.push(canon);
        std::fs::create_dir_all(&self.state_dir)?;
        let body: String = roots.iter().map(|r| format!("{}\n", r.display())).collect();
        std::fs::write(self.roots_file(), body)?;
        Ok(true)
    }
}
