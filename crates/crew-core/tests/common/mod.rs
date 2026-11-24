//! Helpers shared by the git test binaries.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crew_core::worktree;

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A repo on `main` with one commit holding `a.txt` ("one\ntwo\n") and `b.txt`.
pub fn repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    fs::write(dir.join("b.txt"), "bee\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Root repo plus a fresh worktree for `crew/t`. Returns (root, dest, base).
#[allow(dead_code)]
pub fn setup(tmp: &Path) -> (PathBuf, PathBuf, String) {
    let root = tmp.join("root");
    repo(&root);
    let dest = tmp.join("worktrees/demo/t");
    let base = worktree::create(&root, "main", &dest, "crew/t").unwrap();
    (root, dest, base)
}
