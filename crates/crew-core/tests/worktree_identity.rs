//! Kept in its own binary: it changes the process environment.

mod common;

use std::fs;
use std::path::Path;

use common::{git, setup};
use crew_core::worktree;

#[test]
fn commit_without_identity_uses_crew() {
    let tmp = tempfile::tempdir().unwrap();
    let (_root, dest, _base) = setup(tmp.path());
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    git(&dest, &["config", "--unset", "user.name"]);
    git(&dest, &["config", "--unset", "user.email"]);
    fs::write(dest.join("a.txt"), "changed\n").unwrap();

    // Hide any global or system identity for this test's git calls.
    let sha = temp_env(&home, || worktree::commit(&dest, "chore: anon")).unwrap();

    assert_eq!(
        git(&dest, &["log", "-1", "--format=%an <%ae>", &sha]),
        "Crew <crew@localhost>"
    );
}

/// Runs `f` with git's global and system config hidden.
fn temp_env<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    // SAFETY: this is the only test in this binary, so no other thread
    // reads the environment meanwhile.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", home.join("gitconfig"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    }
    let out = f();
    unsafe {
        std::env::remove_var("GIT_CONFIG_GLOBAL");
        std::env::remove_var("GIT_CONFIG_NOSYSTEM");
    }
    out
}
