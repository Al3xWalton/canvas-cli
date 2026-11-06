//! Records which git commit this build came from, and where its source
//! lives, so `--version` and the update check can name it exactly.
//!
//! Sets `CANVAS_COMMIT` (full hash, or empty outside a git checkout),
//! `CANVAS_COMMIT_DATE` (`YYYY-MM-DD`), `CANVAS_DIRTY` (`1` if the tree had
//! uncommitted changes) and `CANVAS_SOURCE_DIR` (the workspace root).

use std::path::PathBuf;
use std::process::Command;

fn git(root: &PathBuf, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .map_or_else(|| manifest.clone(), PathBuf::from);
    let git_dir = root.join(".git");
    // Rebuild the stamp after every commit, pull or checkout.
    for watched in ["HEAD", "logs/HEAD", "index"] {
        println!("cargo:rerun-if-changed={}", git_dir.join(watched).display());
    }
    let commit = git(&root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let date = git(&root, &["log", "-1", "--format=%cs"]).unwrap_or_default();
    let dirty = git(&root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=CANVAS_COMMIT={commit}");
    println!("cargo:rustc-env=CANVAS_COMMIT_DATE={date}");
    println!(
        "cargo:rustc-env=CANVAS_DIRTY={}",
        if dirty { "1" } else { "" }
    );
    println!("cargo:rustc-env=CANVAS_SOURCE_DIR={}", root.display());
}
