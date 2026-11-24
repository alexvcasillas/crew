mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{git, repo};
use crew_core::project_kind::{
    NEEDS_REPO, ProjectKind, WorkMode, detect_kind, is_live_keys, looks_secret, relative_to,
    repo_of,
};

fn multi(repos: &[&str]) -> ProjectKind {
    ProjectKind::MultiRepo {
        repos: repos.iter().map(|r| r.to_string()).collect(),
    }
}

#[test]
fn a_repo_is_a_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("api");
    repo(&root);
    assert_eq!(detect_kind(&root), ProjectKind::Repo);
    // A folder inside a checkout works in that checkout.
    fs::create_dir_all(root.join("src")).unwrap();
    assert_eq!(detect_kind(&root.join("src")), ProjectKind::Repo);
}

#[test]
fn a_folder_of_repos_lists_them() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("platform");
    repo(&root.join("web"));
    repo(&root.join("api"));
    repo(&root.join("services/infra"));
    fs::write(root.join("README.md"), "hi\n").unwrap();
    assert_eq!(detect_kind(&root), multi(&["api", "services/infra", "web"]));
}

#[test]
fn a_plain_folder_has_no_git() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("notes");
    fs::create_dir_all(root.join("2026")).unwrap();
    fs::write(root.join("index.md"), "hi\n").unwrap();
    assert_eq!(detect_kind(&root), ProjectKind::Plain);
    assert_eq!(detect_kind(&tmp.path().join("missing")), ProjectKind::Plain);
}

#[test]
fn nested_repos_and_submodules_count_with_their_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("platform");
    repo(&root.join("api"));
    // A repo inside a repo.
    repo(&root.join("api/vendor/lib"));
    // A submodule of web.
    let lib = tmp.path().join("lib");
    repo(&lib);
    repo(&root.join("web"));
    git(
        &root.join("web"),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib.to_str().unwrap(),
            "deps/lib",
        ],
    );
    assert!(root.join("web/deps/lib/.git").exists());
    assert_eq!(detect_kind(&root), multi(&["api", "web"]));
}

#[test]
fn hidden_and_build_folders_and_deep_repos_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("work");
    repo(&root.join("node_modules/pkg"));
    repo(&root.join("target/thing"));
    repo(&root.join(".cache/repo"));
    repo(&root.join("a/b/c/too-deep"));
    repo(&root.join("a/b/deep-enough"));
    assert_eq!(detect_kind(&root), multi(&["a/b/deep-enough"]));
}

#[test]
fn thirty_repos_are_found_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("many");
    for i in 0..30 {
        let dir = root.join(format!("repo-{i:02}"));
        fs::create_dir_all(dir.join(".git")).unwrap();
        // Lots of files and folders inside each repo, never read.
        for j in 0..50 {
            fs::create_dir_all(dir.join(format!("src/mod{j}/deep"))).unwrap();
        }
    }
    // A big tree that isn't a repo still stops at the depth limit.
    for j in 0..200 {
        fs::create_dir_all(root.join(format!("docs/part{j}/a/b/c/d"))).unwrap();
    }
    let started = Instant::now();
    let kind = detect_kind(&root);
    let took = started.elapsed();
    let ProjectKind::MultiRepo { repos } = kind else {
        panic!("not a folder of repos: {kind:?}");
    };
    assert_eq!(repos.len(), 30);
    assert_eq!(repos[0], "repo-00");
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[test]
fn modes_follow_the_kind() {
    assert_eq!(ProjectKind::Repo.default_mode(), WorkMode::OwnCopy);
    assert_eq!(multi(&["api"]).default_mode(), WorkMode::OwnCopy);
    assert_eq!(ProjectKind::Plain.default_mode(), WorkMode::InPlace);
    assert_eq!(
        ProjectKind::Plain.allows(WorkMode::OwnCopy),
        Err(NEEDS_REPO.to_string())
    );
    assert!(ProjectKind::Plain.allows(WorkMode::InPlace).is_ok());
    assert!(multi(&["api"]).allows(WorkMode::OwnCopy).is_ok());
    assert_eq!(ProjectKind::Repo.repos(), vec![String::new()]);
    assert!(ProjectKind::Plain.repos().is_empty());

    let json = serde_json::to_value(multi(&["api"])).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"kind": "multi_repo", "repos": ["api"]})
    );
    assert_eq!(
        serde_json::to_value(WorkMode::InPlace).unwrap(),
        serde_json::json!("in_place")
    );
}

#[test]
fn paths_map_to_their_repo() {
    let repos = vec!["api".to_string(), "api/sub".to_string(), "web".into()];
    assert_eq!(repo_of(&repos, "api/src/a.rs"), Some(("api", "src/a.rs")));
    assert_eq!(repo_of(&repos, "api/sub/x"), Some(("api/sub", "x")));
    assert_eq!(repo_of(&repos, "apix/a"), None);
    assert_eq!(repo_of(&repos, "README.md"), None);
    let one = vec![String::new()];
    assert_eq!(repo_of(&one, "src/a.rs"), Some(("", "src/a.rs")));

    let root = std::path::Path::new("/work/demo");
    assert_eq!(
        relative_to(root, "/work/demo/src/./a.rs").as_deref(),
        Some("src/a.rs")
    );
    assert_eq!(relative_to(root, "src/a.rs").as_deref(), Some("src/a.rs"));
    assert_eq!(relative_to(root, "/work/other/a.rs"), None);
    assert_eq!(relative_to(root, "../a.rs"), None);
}

#[test]
fn secret_looking_setup_files() {
    assert!(looks_secret(".env.production"));
    assert!(is_live_keys(".env.production"));
    assert!(looks_secret("certs/server.pem"));
    assert!(looks_secret("deploy.key"));
    assert!(!is_live_keys("deploy.key"));
    assert!(looks_secret(".env.prod"));
    assert!(!looks_secret(".env"));
    assert!(!looks_secret(".env.test"));
    assert!(!looks_secret("certs/"));
    assert!(!looks_secret("keyboard.json"));
}
