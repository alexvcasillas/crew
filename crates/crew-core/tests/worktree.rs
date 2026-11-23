mod common;

use std::fs;

use common::{git, repo, setup};
use crew_core::slug::slugify;
use crew_core::worktree::{self, FileChange};

#[test]
fn slug_dedupes_and_truncates() {
    let none = |_: &str| false;
    assert_eq!(slugify("Fix the Login bug!", &none), "fix-the-login-bug");
    assert_eq!(slugify("  --Hello__World--  ", &none), "hello-world");
    assert_eq!(slugify("Café au lait", &none), "caf-au-lait");
    assert_eq!(slugify("!!!", &none), "untitled");

    let long = "a".repeat(30) + " " + &"b".repeat(30);
    let s = slugify(&long, &none);
    assert_eq!(s.len(), 48);
    assert_eq!(s, format!("{}-{}", "a".repeat(30), "b".repeat(17)));
    // Truncation never leaves a trailing dash.
    let s = slugify(&format!("{} tail", "x".repeat(47)), &none);
    assert_eq!(s, "x".repeat(47));

    let taken = |s: &str| ["fix-it", "fix-it-2"].contains(&s);
    assert_eq!(slugify("Fix it", &taken), "fix-it-3");
    assert_eq!(slugify("Other", &taken), "other");

    let full = "z".repeat(48);
    let long_taken = |s: &str| s == full;
    let s = slugify(&"z".repeat(60), &long_taken);
    assert_eq!(s, format!("{}-2", "z".repeat(46)));
    assert!(
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
}

#[test]
fn worktree_from_main_ignores_dirty_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    repo(&root);
    fs::write(root.join("a.txt"), "dirty\n").unwrap();
    fs::write(root.join("new.txt"), "untracked\n").unwrap();
    let dest = tmp.path().join("worktrees/demo/t");

    let base = worktree::create(&root, "main", &dest, "crew/t").unwrap();

    assert_eq!(base, git(&root, &["rev-parse", "main"]));
    assert_eq!(
        fs::read_to_string(dest.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(!dest.join("new.txt").exists());
    assert_eq!(git(&dest, &["branch", "--show-current"]), "crew/t");
    // The root keeps its edit and its branch.
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "dirty\n");
    assert_eq!(git(&root, &["branch", "--show-current"]), "main");

    let err = worktree::create(&root, "nope", &tmp.path().join("x"), "crew/x").unwrap_err();
    match err {
        worktree::WorktreeError::Git { args, stderr } => {
            assert_eq!(args[0], "worktree");
            assert!(!stderr.is_empty());
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn diff_lists_modified_added_and_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    let (_root, dest, base) = setup(tmp.path());
    fs::write(dest.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    fs::remove_file(dest.join("b.txt")).unwrap();
    fs::write(dest.join("staged.txt"), "s\n").unwrap();
    git(&dest, &["add", "staged.txt"]);
    fs::create_dir_all(dest.join("src")).unwrap();
    fs::write(dest.join("src/new.rs"), "fn main() {}\n").unwrap();
    fs::write(dest.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(dest.join("ignored.txt"), "nope\n").unwrap();
    let status_before = git(&dest, &["status", "--porcelain"]);

    let diff = worktree::diff(&dest, &base, &[]).unwrap();

    let change = |path: &str, status: char, added: u32, removed: u32| FileChange {
        path: path.into(),
        status,
        added,
        removed,
    };
    assert_eq!(
        diff.files,
        vec![
            change(".gitignore", 'A', 1, 0),
            change("a.txt", 'M', 2, 1),
            change("b.txt", 'D', 0, 1),
            change("src/new.rs", 'A', 1, 0),
            change("staged.txt", 'A', 1, 0),
        ]
    );
    assert!(diff.patch.contains("+fn main() {}"));
    assert!(diff.patch.contains("-two"));
    assert!(!diff.patch.contains("b/ignored.txt"));
    // Looking at the diff leaves the index alone.
    assert_eq!(git(&dest, &["status", "--porcelain"]), status_before);
}

#[test]
fn commit_then_diff_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (_root, dest, base) = setup(tmp.path());
    fs::write(dest.join("a.txt"), "changed\n").unwrap();
    fs::write(dest.join("c.txt"), "new\n").unwrap();

    let sha = worktree::commit(&dest, "feat: change a", &[]).unwrap();

    assert_eq!(sha, git(&dest, &["rev-parse", "HEAD"]));
    assert_eq!(git(&dest, &["rev-parse", "HEAD^"]), base);
    assert_eq!(git(&dest, &["log", "-1", "--format=%s"]), "feat: change a");
    assert_eq!(
        git(&dest, &["log", "-1", "--format=%an <%ae>"]),
        "Test <test@example.com>"
    );
    assert_eq!(git(&dest, &["status", "--porcelain"]), "");
    let diff = worktree::diff(&dest, &sha, &[]).unwrap();
    assert!(diff.files.is_empty());
    assert_eq!(diff.patch, "");
    // Against the thread's base the committed work still shows.
    assert_eq!(worktree::diff(&dest, &base, &[]).unwrap().files.len(), 2);
}

#[test]
fn remove_deletes_dir_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, dest, _base) = setup(tmp.path());
    fs::write(dest.join("a.txt"), "unsaved\n").unwrap();
    assert!(worktree::branch_exists(&root, "crew/t").unwrap());

    worktree::remove(&root, &dest, "crew/t", true).unwrap();

    assert!(!dest.exists());
    assert_eq!(git(&root, &["branch", "--list", "crew/t"]), "");
    assert!(!worktree::branch_exists(&root, "crew/t").unwrap());
    assert!(!git(&root, &["worktree", "list"]).contains("worktrees/demo/t "));

    // Keeping the branch, and a directory that is already gone.
    let dest2 = tmp.path().join("worktrees/demo/t2");
    worktree::create(&root, "main", &dest2, "crew/t2").unwrap();
    fs::remove_dir_all(&dest2).unwrap();
    worktree::remove(&root, &dest2, "crew/t2", false).unwrap();
    assert_eq!(git(&root, &["branch", "--list", "crew/t2"]), "crew/t2");
    assert!(!git(&root, &["worktree", "list"]).contains("worktrees/demo/t2"));
}

#[test]
fn copy_files_skips_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, dest, _base) = setup(tmp.path());
    fs::write(root.join(".env"), "KEY=1\n").unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::write(root.join("config/local.toml"), "x = 1\n").unwrap();

    let skipped = worktree::copy_files(
        &root,
        &dest,
        &[
            ".env".into(),
            "missing.env".into(),
            "config/local.toml".into(),
        ],
    )
    .unwrap();

    assert_eq!(skipped, vec!["missing.env".to_string()]);
    assert_eq!(fs::read_to_string(dest.join(".env")).unwrap(), "KEY=1\n");
    assert_eq!(
        fs::read_to_string(dest.join("config/local.toml")).unwrap(),
        "x = 1\n"
    );

    for bad in ["../outside", "/etc/hosts"] {
        assert!(matches!(
            worktree::copy_files(&root, &dest, &[bad.into()]),
            Err(worktree::WorktreeError::BadPath(_))
        ));
    }
}

#[test]
fn excluded_paths_stay_out_of_diff_and_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, dest, base) = setup(tmp.path());
    let root_exclude = root.join(".git/info/exclude");
    let before = fs::read(&root_exclude).ok();
    fs::write(dest.join(".env.local"), "SECRET=1\n").unwrap();
    fs::create_dir_all(dest.join("config")).unwrap();
    fs::write(dest.join("config/dev.toml"), "x = 1\n").unwrap();
    // A tracked file that was copied over is left out too.
    fs::write(dest.join("b.txt"), "copied\n").unwrap();
    fs::write(dest.join("new.txt"), "new\n").unwrap();
    let exclude = vec![
        ".env.local".to_string(),
        "config/dev.toml".to_string(),
        "b.txt".to_string(),
        "not-there.txt".to_string(),
    ];

    let diff = worktree::diff(&dest, &base, &exclude).unwrap();
    let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["new.txt"]);
    assert!(!diff.patch.contains(".env.local"));
    assert!(!diff.patch.contains("b.txt"));
    assert!(diff.patch.contains("new.txt"));

    worktree::commit(&dest, "Add new", &exclude).unwrap();
    assert_eq!(
        git(&dest, &["show", "--name-only", "--format=", "HEAD"]),
        "new.txt"
    );
    assert!(dest.join(".env.local").is_file());
    assert_eq!(fs::read_to_string(dest.join("b.txt")).unwrap(), "copied\n");
    // The excluded paths are still there, unstaged.
    let paths: Vec<String> = worktree::diff(&dest, "HEAD", &[])
        .unwrap()
        .files
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(paths, vec![".env.local", "b.txt", "config/dev.toml"]);
    // The repo's own exclude file is never touched.
    assert_eq!(fs::read(&root_exclude).ok(), before);
}
