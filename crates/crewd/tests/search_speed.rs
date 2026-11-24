//! Search stays quick with thousands of threads and large worktrees.
//!
//! The fixture is built in a temp database: 5,000 threads with two
//! messages each, a project root with 3,000 tracked files and 2,000
//! ignored ones under `node_modules`, and two thread worktrees. Only the
//! query path is timed: the first search fills the file index, the
//! timed ones reuse it, as typing in the palette does.
//!
//! The bound is 200 ms. On CI (`CI` set) it is 1 s, since shared runners
//! are slow and run tests in debug builds side by side; a regression
//! that walks ignored folders or rereads git per query still fails there.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crew_core::ids::ProjectId;
use crew_core::store::{Author, Store, ThreadWorktree};
use crew_proto::{Request, SearchResults};
use crewd::service::{CliBins, Service};

fn git(dir: &Path, args: &[&str]) -> String {
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

fn big_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
    for dir in 0..60 {
        let d = root.join(format!("src/module_{dir}"));
        std::fs::create_dir_all(&d).unwrap();
        for f in 0..50 {
            std::fs::write(d.join(format!("file_{f}.rs")), "x").unwrap();
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);
    for dir in 0..40 {
        let d = root.join(format!("node_modules/pkg_{dir}"));
        std::fs::create_dir_all(&d).unwrap();
        for f in 0..50 {
            std::fs::write(d.join(format!("file_{f}.rs")), "x").unwrap();
        }
    }
}

/// The fixture's store and the project in it.
fn fixture(tmp: &Path) -> (Store, ProjectId) {
    let root = tmp.join("root");
    big_repo(&root);
    let store = Store::open_in_memory().unwrap();
    let p = store.create_project("big", &root, "main").unwrap();
    let channels: Vec<_> = (0..20)
        .map(|i| store.create_channel(p, &format!("channel-{i}")).unwrap())
        .collect();
    let words = ["parser", "login", "cache", "render", "deploy", "query"];
    for i in 0..5000 {
        let channel = channels[i % channels.len()];
        let word = words[i % words.len()];
        let t = store
            .create_thread(channel, &format!("Fix the {word} bug number {i}"))
            .unwrap();
        store
            .add_message(t, Author::You, &format!("The {word} breaks when {i}"), None)
            .unwrap();
        store
            .add_message(t, Author::System, "Stopped before starting.", None)
            .unwrap();
        if i < 2 {
            let path = tmp.join(format!("worktrees/big/t{i}"));
            let branch = format!("crew/t{i}");
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &branch,
                    &path.to_string_lossy(),
                    "main",
                ],
            );
            std::fs::write(path.join("src/module_1/file_1.rs"), "changed").unwrap();
            let base = git(&path, &["rev-parse", "HEAD"]);
            let wt = ThreadWorktree {
                path,
                branch,
                base,
                ready: true,
            };
            store.set_thread_worktree(t, Some(&wt)).unwrap();
        }
    }
    (store, p)
}

#[tokio::test(flavor = "multi_thread")]
async fn search_over_five_thousand_threads_is_quick() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, project) = fixture(tmp.path());
    let svc = Service::new(
        store,
        4,
        CliBins::default(),
        tmp.path().join("crew-worktrees"),
        None,
        Duration::from_secs(60),
        64,
    );
    let search = |query: &str| {
        let req = Request::Search {
            query: query.into(),
            project: Some(project),
            limit: 50,
        };
        let svc = svc.clone();
        async move {
            let value = svc.handle(req).await.unwrap();
            serde_json::from_value::<SearchResults>(value).unwrap()
        }
    };

    // Fills the file index of the root and both worktrees.
    let first = search("file_1.rs").await;
    assert!(
        first
            .files
            .iter()
            .all(|f| !f.path.starts_with("node_modules")),
        "ignored folders stay out"
    );
    let changed: Vec<_> = first.files.iter().filter(|f| f.changed).collect();
    assert_eq!(changed.len(), 2, "the edit in each worktree");

    let bound = if std::env::var_os("CI").is_some() {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(200)
    };
    for query in [
        "parser",
        "bug number 4999",
        "file_4",
        "module_59/file",
        "zzz",
    ] {
        let started = Instant::now();
        let r = search(query).await;
        let took = started.elapsed();
        assert!(took < bound, "searching {query:?} took {took:?}");
        if query == "parser" {
            assert_eq!(r.threads.len(), 50);
            assert_eq!(r.totals.threads, 834);
        }
        if query == "bug number 4999" {
            assert_eq!(r.totals.threads, 1);
        }
    }
}
