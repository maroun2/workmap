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
    /// `gh` / `glab` executables for remote discovery (`WORKMAP_GH_BIN`, `WORKMAP_GLAB_BIN`).
    pub gh_bin: PathBuf,
    pub glab_bin: PathBuf,
    /// Config dirs the CLIs are told to use; derived from home under `WORKMAP_HOME`.
    pub gh_config: PathBuf,
    pub glab_config: PathBuf,
    /// Hosts with a token in the environment (presence only; values never read).
    pub env_token_hosts: Vec<(&'static str, String)>,
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
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let config = match (overridden, var("XDG_CONFIG_HOME")) {
            (false, Some(x)) => PathBuf::from(x),
            _ => home.join(".config"),
        };
        let gh_config = var("GH_CONFIG_DIR").filter(|_| !overridden).map(PathBuf::from).unwrap_or_else(|| config.join("gh"));
        let glab_config = var("GLAB_CONFIG_DIR").filter(|_| !overridden).map(PathBuf::from).unwrap_or_else(|| config.join("glab-cli"));
        let mut env_token_hosts = Vec::new();
        if !overridden {
            let host = |k: &str, d: &str| std::env::var(k).ok().filter(|h| !h.is_empty()).unwrap_or_else(|| d.to_string());
            if var("GH_TOKEN").or_else(|| var("GITHUB_TOKEN")).is_some() {
                env_token_hosts.push(("gh", "github.com".to_string()));
            }
            if var("GH_ENTERPRISE_TOKEN").or_else(|| var("GITHUB_ENTERPRISE_TOKEN")).is_some() && var("GH_HOST").is_some() {
                env_token_hosts.push(("gh", host("GH_HOST", "github.com")));
            }
            if var("GITLAB_TOKEN").or_else(|| var("GITLAB_ACCESS_TOKEN")).is_some() {
                env_token_hosts.push(("glab", host("GITLAB_HOST", "gitlab.com")));
            }
        }
        Env {
            home,
            overridden,
            state_dir,
            provider_dirs,
            gh_bin: var("WORKMAP_GH_BIN").map(PathBuf::from).unwrap_or_else(|| "gh".into()),
            glab_bin: var("WORKMAP_GLAB_BIN").map(PathBuf::from).unwrap_or_else(|| "glab".into()),
            gh_config,
            glab_config,
            env_token_hosts,
        }
    }

    #[cfg(test)]
    pub fn with_home(home: &Path) -> Env {
        Env {
            home: home.to_path_buf(),
            overridden: true,
            state_dir: home.join(".local/state/workmap"),
            provider_dirs: vec![],
            // Tests never reach a real CLI unless they install a fake one here.
            gh_bin: home.join("no-such-gh"),
            glab_bin: home.join("no-such-glab"),
            gh_config: home.join(".config/gh"),
            glab_config: home.join(".config/glab-cli"),
            env_token_hosts: vec![],
        }
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
        for (k, p) in [("WORKMAP_GH_BIN", &self.gh_bin), ("WORKMAP_GLAB_BIN", &self.glab_bin)] {
            if std::env::var_os(k).is_some() {
                v.push((k.to_string(), p.display().to_string()));
            }
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
