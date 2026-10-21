//! Embeds the source revision for `workmap --version` (see src/buildinfo.rs).
//! Never fails: without Git or a `.git` (Nix source, crate tarball) it falls
//! back to `WORKMAP_BUILD_REV`, then `unknown`.

#[path = "src/buildinfo.rs"]
mod buildinfo;

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).env("GIT_OPTIONAL_LOCKS", "0").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    for f in ["build.rs", "src", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={f}");
    }
    println!("cargo:rerun-if-env-changed=WORKMAP_BUILD_REV");
    // Only trust Git when the manifest dir is the top of its own checkout, not a
    // directory inside some unrelated repository (vendored or unpacked crate).
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let top = git(&dir, &["rev-parse", "--show-toplevel"]).map(|t| PathBuf::from(t.trim()));
    let ours = top.as_deref().map(|t| canon(t) == canon(&dir)).unwrap_or(false);
    let (rev, dirty) = if ours {
        if let Some(paths) = git(&dir, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"]) {
            let mut it = paths.lines();
            let (gd, common) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string());
            for p in [format!("{gd}/HEAD"), format!("{gd}/index"), format!("{common}/refs"), format!("{common}/packed-refs")] {
                if Path::new(&p).exists() {
                    println!("cargo:rerun-if-changed={p}");
                }
            }
        }
        let rev = git(&dir, &["rev-parse", "--short=12", "HEAD"]);
        let dirty = git(&dir, &["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.trim().is_empty()).unwrap_or(false);
        (rev, dirty)
    } else {
        (None, false)
    };
    let env_rev = std::env::var("WORKMAP_BUILD_REV").ok();
    let rev = buildinfo::revision(rev.as_deref(), dirty, env_rev.as_deref());
    println!("cargo:rustc-env=WORKMAP_BUILD_REV={rev}");
    println!("cargo:rustc-env=WORKMAP_BUILD_PROFILE={}", std::env::var("PROFILE").unwrap_or_default());
    println!("cargo:rustc-env=WORKMAP_BUILD_TARGET={}", std::env::var("TARGET").unwrap_or_default());
}
