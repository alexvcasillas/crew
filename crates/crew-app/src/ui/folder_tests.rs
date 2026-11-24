//! End to end: threads and reviews in folder projects (a folder of repos
//! with a copy per thread, and folders worked on in place), with a real
//! crewd on a temp home and a fake claude that writes files.

use std::path::{Path, PathBuf};

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ThreadId};
use crew_core::project_kind::WorkMode;
use crew_proto::Request;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};

use super::inspector_tests::run_of;
use super::test_ui::{click, has, label_of, notifications, press, shows, wait_to_show, wait_until};
use super::thread_tests::{World, agent, ask, go_to, open_thread, world_named};
use super::title_bar::home_relative;
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, short_home};

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

/// A claude that writes each `path|content` line of `edits` in its
/// folder with a Write call, then replies "Fixed.". It starts its
/// session in the folder it runs in. Quill first waits for
/// the file `go`.
fn writing_claude(dir: &Path) -> PathBuf {
    let d = dir.display();
    let init = format!(
        r#"{{"type":"system","subtype":"init","cwd":"%s","session_id":"{SESSION}","model":"claude-haiku-4-5","tools":["Write"]}}\n"#
    );
    let result = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"Fixed.","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"{SESSION}"}}"#
    );
    let call = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_%s","name":"Write","input":{{"file_path":"%s/%s","content":"%s"}}}}]}},"session_id":"{SESSION}"}}\n"#
    );
    let done = format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_%s","content":"ok"}}]}},"session_id":"{SESSION}"}}\n"#
    );
    let run = format!(
        r#"D='{d}'
printf '{init}' "$PWD"
case "$*" in
  *"You are Quill"*) while [ ! -e "$D/go" ]; do sleep 0.02; done;;
esac
n=0
if [ -f "$D/edits" ]; then
  while IFS='|' read -r path content; do
    [ -z "$path" ] && continue
    n=$((n+1))
    mkdir -p "$(dirname "$path")"
    printf '%s\n' "$content" > "$path"
    printf '{call}' "$n" "$PWD" "$path" "$content"
    printf '{done}' "$n"
  done < "$D/edits"
fi
printf '%s\n' '{result}'"#
    );
    fake_claude(dir, &run)
}

/// Sets what the fake claude writes on its next runs.
fn set_edits(dir: &Path, edits: &[(&str, &str)]) {
    let lines: Vec<String> = edits.iter().map(|(p, c)| format!("{p}|{c}")).collect();
    std::fs::write(dir.join("edits"), lines.join("\n") + "\n").unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
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

/// A repo on `main` whose one commit holds `file` saying `text`.
fn repo(dir: &Path, file: &str, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

/// A writer agent.
fn writer(w: &World, name: &str) -> AgentId {
    ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::EditFiles,
                ..agent(name, "Builds")
            },
        },
    )
}

struct Folder {
    tmp: tempfile::TempDir,
    home: tempfile::TempDir,
    _daemon: Daemon,
    root: PathBuf,
    w: World,
}

/// crewd with the writing claude and `root` (made by `make` inside a
/// temp folder) added as `name`.
fn folder(name: &str, mode: Option<WorkMode>, make: impl FnOnce(&Path)) -> Folder {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(name);
    make(&root);
    let claude = writing_claude(tmp.path());
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world_named(home.path(), &root, name, mode, &[]);
    Folder {
        tmp,
        home,
        _daemon: daemon,
        root,
        w,
    }
}

/// `platform/` with the repos `api`, `web` and `infra`, a README and a
/// compose file.
fn platform(root: &Path) {
    repo(&root.join("api"), "a.txt", "one\n");
    repo(&root.join("web"), "b.txt", "bee\n");
    repo(&root.join("infra"), "c.txt", "sea\n");
    std::fs::write(root.join("README.md"), "read me\n").unwrap();
    std::fs::write(root.join("docker-compose.yml"), "services: {}\n").unwrap();
}

fn wait_label(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    id: impl Into<gpui_kit::ElementId> + Clone,
    want: &str,
) {
    let started = std::time::Instant::now();
    loop {
        cx.run_until_parked();
        let last = label_of(cx, window, id.clone());
        if last.as_deref() == Some(want) {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "timed out waiting for {want:?}, last {last:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn thread_root(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> PathBuf {
    ws.read_with(cx, |ws, _| {
        ws.state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.path.clone())
            .unwrap()
    })
}

#[gpui_kit::test]
fn a_folder_of_repos_shows_a_worktree_per_repo_and_commits_one_repo(cx: &mut TestAppContext) {
    let f = folder("platform", Some(WorkMode::OwnCopy), platform);
    let forge = writer(&f.w, "Forge");
    set_edits(
        f.tmp.path(),
        &[
            ("api/a.txt", "two"),
            ("api/new.txt", "new"),
            ("web/b.txt", "web"),
        ],
    );
    let (window, ws) = open_thread(cx, f.home.path(), &f.w);
    f.w.post("@Forge fix it");
    run_of(cx, &ws, f.w.thread, forge, RunStatus::Done);

    // The header says the thread has a worktree per repo.
    wait_label(cx, window, "thread-mode-chip", "3 worktrees");

    // The Thread tab lists each repo's worktree and what changed there.
    click(cx, window, "inspector-tab-thread");
    wait_label(
        cx,
        window,
        "working-folder-hint",
        "Thread folder platform · crew/fix-the-login",
    );
    wait_to_show(cx, window, "api crew/fix-the-login 2 changed");
    assert!(shows(cx, window, "web crew/fix-the-login 1 changed"));
    assert!(shows(cx, window, "infra crew/fix-the-login clean"));
    wait_label(
        cx,
        window,
        "working-not-copied",
        "Plain files at the top of platform/ stay out of the thread folder: README.md, docker-compose.yml. Agents can read them from the original folder but not change them.",
    );

    // Review groups the files by repo, with a commit per repo.
    click(cx, window, "inspector-tab-changes");
    click(cx, window, "changes-review");
    wait_to_show(cx, window, "Forge's changes");
    wait_label(cx, window, "review-summary", "3 files in 2 repos · +3 −2");
    assert_eq!(
        label_of(cx, window, ("review-repo", 0usize)).as_deref(),
        Some("api · 2 files")
    );
    assert_eq!(
        label_of(cx, window, ("review-repo", 1usize)).as_deref(),
        Some("web · 1 file")
    );
    assert_eq!(
        label_of(cx, window, ("review-file", 0usize)).as_deref(),
        Some("M a.txt +1 −1")
    );
    assert_eq!(
        label_of(cx, window, ("review-repo-head", 0usize)).as_deref(),
        Some("api on crew/fix-the-login · 2 files")
    );
    assert_eq!(
        label_of(cx, window, ("review-commit-repo", 0usize)).as_deref(),
        Some("Commit 2 files in api")
    );
    assert_eq!(
        label_of(cx, window, ("review-commit-repo", 1usize)).as_deref(),
        Some("Commit 1 file in web")
    );
    assert_eq!(
        label_of(cx, window, "review-discard").as_deref(),
        Some("Discard changes in api and web")
    );
    assert!(!has(cx, window, "review-mode-chip"), "not in place");

    // Committing web leaves api to review.
    click(cx, window, ("review-commit-repo", 1usize));
    wait_until(cx, "web's group to go", |cx| {
        !has(cx, window, ("review-repo", 1usize))
    });
    let web = thread_root(cx, &ws, f.w.thread).join("web");
    assert_eq!(git(&web, &["log", "-1", "--format=%s"]), "Fixed.");
    assert_eq!(git(&web, &["status", "--porcelain"]), "");
    assert_eq!(
        label_of(cx, window, ("review-repo", 0usize)).as_deref(),
        Some("api · 2 files")
    );
    assert_eq!(notifications(cx, window), 1, "the commit's notice only");
}

#[gpui_kit::test]
fn in_place_a_second_writer_waits_its_turn(cx: &mut TestAppContext) {
    let f = folder("notes", None, |root| {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("index.md"), "old\n").unwrap();
    });
    let quill = writer(&f.w, "Quill");
    let forge = writer(&f.w, "Forge");
    set_edits(f.tmp.path(), &[("index.md", "new")]);
    let other: ThreadId = f.w.ask(Request::CreateThread {
        channel: f.w.channel,
        title: "write the notes".into(),
    });
    let (window, ws) = open_thread(cx, f.home.path(), &f.w);
    let _: crew_core::ids::MessageId = f.w.ask(Request::PostMessage {
        thread: other,
        body: "@Quill write".into(),
    });
    run_of(cx, &ws, other, quill, RunStatus::Working);
    f.w.post("@Forge tidy up");
    wait_until(cx, "Forge to wait for Quill", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .thread_runs(f.w.thread)
                .iter()
                .any(|r| r.agent == forge && r.waiting_for.is_some())
        })
    });

    wait_label(cx, window, "thread-mode-chip", "in place");
    wait_label(
        cx,
        window,
        "composer-hint",
        "Forge will start when Quill finishes writing.",
    );

    click(cx, window, "inspector-tab-thread");
    let root = ws.read_with(cx, |ws, _| {
        ws.state().project(f.w.project).unwrap().root.clone()
    });
    wait_label(
        cx,
        window,
        "working-in-place",
        &format!("Edits go straight to {}", home_relative(&root)),
    );
    wait_label(cx, window, ("working-writer", 0usize), "Quill is writing");
    wait_label(
        cx,
        window,
        ("working-waiting", 0usize),
        "Forge waits to write · 1st in queue",
    );
    assert!(shows(
        cx,
        window,
        "Agents edit your files directly. There's no undo outside git."
    ));

    // Cancel takes Forge out of the queue.
    click(cx, window, ("working-cancel", 0usize));
    run_of(cx, &ws, f.w.thread, forge, RunStatus::Stopped);
    wait_until(cx, "the queue to empty", |cx| {
        !has(cx, window, ("working-waiting", 0usize))
    });
    assert_ne!(
        label_of(cx, window, "composer-hint").as_deref(),
        Some("Forge will start when Quill finishes writing.")
    );

    // Quill's thread lists what it changed so far.
    std::fs::write(f.tmp.path().join("go"), "").unwrap();
    run_of(cx, &ws, other, quill, RunStatus::Done);
    go_to(cx, window, &ws, f.w.channel, other);
    click(cx, window, "inspector-tab-thread");
    wait_label(cx, window, ("working-changed", 0usize), "index.md +1");
    assert!(!has(cx, window, ("working-writer", 0usize)));
}

#[gpui_kit::test]
fn in_place_review_spans_repos_and_files_outside_git(cx: &mut TestAppContext) {
    let f = folder("platform", Some(WorkMode::InPlace), |root| {
        repo(&root.join("api"), "a.txt", "one\n");
        repo(&root.join("web"), "b.txt", "bee\n");
        // Yours, changed before the agent ran.
        std::fs::write(root.join("web/user.txt"), "mine\n").unwrap();
    });
    let forge = writer(&f.w, "Forge");
    set_edits(
        f.tmp.path(),
        &[
            ("api/a.txt", "two"),
            ("api/new.txt", "new"),
            ("web/b.txt", "web"),
            ("docker-compose.yml", "services"),
        ],
    );
    let (window, ws) = open_thread(cx, f.home.path(), &f.w);
    f.w.post("@Forge fix it");
    run_of(cx, &ws, f.w.thread, forge, RunStatus::Done);
    wait_until(cx, "the changes", |cx| {
        ws.read_with(cx, |ws, _| ws.thread_change_count() == Some(4))
    });
    click(cx, window, "inspector-tab-changes");
    click(cx, window, "changes-review");
    wait_to_show(cx, window, "Forge's changes");

    // The title says the thread works in place.
    assert_eq!(
        label_of(cx, window, "review-mode-chip").as_deref(),
        Some("in place")
    );
    assert_eq!(
        label_of(cx, window, "review-where").as_deref(),
        Some("fix the login · platform · #general")
    );

    // Files by repo, yours marked, and the file git doesn't have.
    wait_label(cx, window, "review-summary", "5 files in 2 repos · +4 −2");
    let label = |cx: &mut TestAppContext, id: (&'static str, usize)| label_of(cx, window, id);
    assert_eq!(
        label(cx, ("review-repo", 0)).as_deref(),
        Some("api · 2 files")
    );
    assert_eq!(
        label(cx, ("review-repo", 1)).as_deref(),
        Some("web · 2 files")
    );
    assert_eq!(
        label(cx, ("review-file", 3)).as_deref(),
        Some("A user.txt +1 −0 · changed outside this thread")
    );
    assert_eq!(
        label_of(cx, window, "review-not-in-git").as_deref(),
        Some("Not in git · 1 file")
    );
    assert_eq!(
        label(cx, ("review-outside", 0)).as_deref(),
        Some("docker-compose.yml edited")
    );
    assert!(shows(cx, window, "Not in git, can't review or undo."));

    // What next: a commit per repo, of the thread's files only.
    assert_eq!(
        label(cx, ("review-repo-head", 0)).as_deref(),
        Some("api on main · 2 files")
    );
    assert_eq!(
        label(cx, ("review-repo-head", 1)).as_deref(),
        Some("web on main · 1 file")
    );
    assert_eq!(
        label(cx, ("review-commit-repo", 0)).as_deref(),
        Some("Commit 2 files in api")
    );
    assert_eq!(
        label(cx, ("review-commit-repo", 1)).as_deref(),
        Some("Commit 1 file in web")
    );
    assert!(shows(
        cx,
        window,
        "docker-compose.yml is not in git. It's already changed on disk and Crew can't undo it."
    ));
    assert_eq!(
        label_of(cx, window, "review-discard").as_deref(),
        Some("Discard changes in api and web")
    );
    assert!(shows(
        cx,
        window,
        "In-place threads commit on each repo's current branch. Nothing is pushed."
    ));
    assert_eq!(
        label(cx, ("review-repo-caption", 0)).as_deref(),
        Some("Commit message · drafted by Forge")
    );

    // The footer counts viewed files; j and k step through the changes.
    assert_eq!(
        label_of(cx, window, "review-viewed-count").as_deref(),
        Some("Viewed 0 of 4 files")
    );
    assert!(shows(cx, window, "j k next / previous change"));
    click(cx, window, ("review-viewed", 0usize));
    assert_eq!(
        label_of(cx, window, "review-viewed-count").as_deref(),
        Some("Viewed 1 of 4 files")
    );
    let change = |cx: &mut TestAppContext| ws.read_with(cx, |ws, _| ws.review_change());
    click(cx, window, "review-summary");
    assert_eq!(change(cx), None);
    press(cx, window, "j");
    let first = change(cx).expect("j goes to the first change");
    press(cx, window, "j");
    let second = change(cx).unwrap();
    assert!(second > first, "then the next");
    press(cx, window, "k");
    assert_eq!(change(cx), Some(first), "k goes back");
    // In a commit message j and k are letters.
    click(cx, window, ("review-repo-message", 1usize));
    press(cx, window, "j k");
    assert_eq!(change(cx), Some(first));
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_repo_message(1, cx))
            .as_deref(),
        Some("Fixed.jk")
    );

    // Committing api takes only its files, on its own branch.
    let api = f.root.join("api");
    click(cx, window, ("review-commit-repo", 0usize));
    wait_until(cx, "api to be committed", |cx| {
        label_of(cx, window, ("review-repo", 0usize)).as_deref() == Some("web · 2 files")
    });
    assert_eq!(git(&api, &["log", "-1", "--format=%s"]), "Fixed.");
    assert_eq!(git(&api, &["status", "--porcelain"]), "");
    assert_eq!(git(&api, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    // Discard asks with the repos named, then puts back only the
    // thread's tracked file.
    click(cx, window, "review-discard");
    let ask_text = "Discard Forge's changes in web? Files this thread changed go back to what git has. Files not in git stay as they are.";
    wait_to_show(cx, window, ask_text);
    click(cx, window, "discard-confirm");
    let web = f.root.join("web");
    wait_until(cx, "web/b.txt to be put back", |_| {
        std::fs::read_to_string(web.join("b.txt")).unwrap_or_default() == "bee\n"
    });
    assert_eq!(
        std::fs::read_to_string(web.join("user.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("docker-compose.yml")).unwrap(),
        "services\n"
    );
    wait_until(cx, "nothing left to commit", |cx| {
        !has(cx, window, ("review-commit-repo", 0usize))
    });
}
