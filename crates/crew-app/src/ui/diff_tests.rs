//! End to end: reviewing, committing and discarding an agent's changes,
//! with a real crewd on a temp home and a fake claude that edits files in
//! its worktree.

use std::path::{Path, PathBuf};

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ThreadId};
use crew_proto::Request;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::inspector_tests::{git_repo, run_of};
use super::test_ui::{
    click, has, label_of, notifications, press, shows, type_text, wait_to_show, wait_until,
};
use super::thread_tests::{World, agent, ask, open_thread, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

const DISCARD_ASK: &str =
    "Discard Forge's changes? The worktree is removed. A branch with commits is kept.";

/// A git repo at `root` whose first commit holds `README.md`.
fn repo_with_readme(root: &Path) {
    git_repo(root);
    std::fs::write(root.join("README.md"), "hello\nworld\n").unwrap();
    for args in [
        &["add", "README.md"][..],
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "readme",
        ][..],
    ] {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }
}

/// crewd with a claude that runs `script` in its worktree, then replays
/// an edit and a test run ending in "Fixed.". The project has Forge, who
/// edits files.
struct Setup {
    _tmp: tempfile::TempDir,
    _home: tempfile::TempDir,
    _daemon: Daemon,
    w: World,
    forge: AgentId,
}

fn setup(script: &str) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let claude = fake_claude(
        tmp.path(),
        &format!(
            "{script}\ncat '{}'",
            fixture("edit_and_test.jsonl").display()
        ),
    );
    let home = short_home();
    let daemon = Daemon::start_with(home.path().to_path_buf(), claude);
    let w = world(home.path(), &root, &[]);
    let forge: AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::EditFiles,
                ..agent("Forge", "Builds")
            },
        },
    );
    Setup {
        _tmp: tmp,
        _home: home,
        _daemon: daemon,
        w,
        forge,
    }
}

fn worktree(cx: &mut TestAppContext, ws: &Entity<Workspace>, thread: ThreadId) -> Option<PathBuf> {
    ws.read_with(cx, |ws, _| {
        ws.state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.path.clone())
    })
}

/// Waits until the app knows the thread has `n` changed files.
fn wait_for_changes(cx: &mut TestAppContext, ws: &Entity<Workspace>, n: usize) {
    wait_until(cx, "the thread's changes", |cx| {
        ws.read_with(cx, |ws, _| ws.thread_change_count() == Some(n))
    });
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap()
}

/// Opens the review from the inspector's Changes tab.
fn review_from_inspector(cx: &mut TestAppContext, window: AnyWindowHandle) {
    click(cx, window, "inspector-tab-changes");
    click(cx, window, "changes-review");
    wait_to_show(cx, window, "Forge's changes");
}

#[gpui_kit::test]
fn diff_shows_files_and_lines_from_the_worktree(cx: &mut TestAppContext) {
    let s = setup(
        "printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt\nseq 1 2500 > big.txt",
    );
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    let run = run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done).id;
    wait_for_changes(cx, &ws, 3);

    click(cx, window, format!("review-{run}"));
    wait_to_show(cx, window, "Forge's changes");
    assert!(shows(cx, window, "fix the login · #general"));
    assert!(
        !has(cx, window, "thread-view"),
        "the review replaces the body"
    );
    assert!(!has(cx, window, "inspector"));
    wait_until(cx, "the files", |cx| {
        label_of(cx, window, "review-summary").as_deref() == Some("3 files changed · +2503 −1")
    });
    let file = |cx: &mut TestAppContext, ix: usize| label_of(cx, window, ("review-file", ix));
    assert_eq!(file(cx, 0).as_deref(), Some("M README.md +1 −1"));
    assert_eq!(file(cx, 1).as_deref(), Some("A big.txt +2500 −0"));
    assert_eq!(file(cx, 2).as_deref(), Some("A new.txt +2 −0"));

    // Lines with their numbers: old, new, sign and text.
    assert!(shows(cx, window, "1 1   hello"));
    assert!(shows(cx, window, "2 · − world"));
    assert!(shows(cx, window, "· 2 + there"));
    assert!(shows(cx, window, "@@ -1,2 +1,2 @@"));
    // The big file waits until you ask for it.
    assert!(shows(cx, window, "big.txt changes 2500 lines."));
    assert!(!shows(cx, window, "· 1 + 1"));
    assert!(shows(cx, window, "· 2 + two"));
    click(cx, window, ("diff-show", 1usize));
    assert!(shows(cx, window, "· 1 + 1"));
    assert!(!shows(cx, window, "· 2500 + 2500"), "far rows aren't drawn");
    click(cx, window, ("review-file", 2usize));
    assert!(
        shows(cx, window, "· 2 + two"),
        "the file list scrolls to it"
    );

    // The commit message is drafted from Forge's reply.
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("Fixed.")
    );
    assert_eq!(
        label_of(cx, window, "review-commit").as_deref(),
        Some("Commit 3 files")
    );
    assert_eq!(label_of(cx, window, "review-into").as_deref(), Some("main"));
    assert_eq!(
        label_of(cx, window, "review-branch").as_deref(),
        Some("crew/fix-the-login")
    );

    // Viewed is kept per thread while the app runs.
    click(cx, window, ("review-file", 0usize));
    assert!(shows(cx, window, "· 2 + there"), "back at the top");
    click(cx, window, ("review-viewed", 0usize));
    press(cx, window, "escape");
    wait_to_show(cx, window, "fix the login");
    assert!(has(cx, window, "thread-view"));
    assert!(!shows(cx, window, "Forge's changes"));
    review_from_inspector(cx, window);
    let viewed = ws.read_with(cx, |ws, _| ws.viewed_files(s.w.thread));
    assert_eq!(viewed, ["README.md"]);

    // Asking for changes goes back to the thread with Forge mentioned.
    click(cx, window, "review-ask");
    assert!(has(cx, window, "thread-view"));
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx)),
        "@Forge "
    );
    type_text(cx, window, "rename it");
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx)),
        "@Forge rename it",
        "the composer has focus"
    );
}

#[gpui_kit::test]
fn commit_empties_the_diff(cx: &mut TestAppContext) {
    let s = setup("printf 'one\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    wait_until(cx, "the commit button", |cx| {
        label_of(cx, window, "review-commit").as_deref() == Some("Commit 1 file")
    });

    click(cx, window, ("review-viewed", 0usize));
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.viewed_files(s.w.thread)),
        ["new.txt"]
    );
    click(cx, window, "review-commit");
    wait_to_show(cx, window, "No changes to review.");
    assert!(
        ws.read_with(cx, |ws, _| ws.viewed_files(s.w.thread))
            .is_empty(),
        "Viewed marks go with the commit"
    );
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("")
    );
    let path = worktree(cx, &ws, s.w.thread).unwrap();
    assert_eq!(git(&path, &["log", "-1", "--format=%s"]).trim(), "Fixed.");
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    assert_eq!(
        label_of(cx, window, "review-commit").as_deref(),
        Some("Commit 0 files")
    );
    assert!(!ws.read_with(cx, |ws, cx| ws.review_can_commit(cx)));
    // The thread knows there is nothing left to review.
    wait_for_changes(cx, &ws, 0);
}

#[gpui_kit::test]
fn discard_asks_then_removes_the_worktree(cx: &mut TestAppContext) {
    let s = setup("printf 'one\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    let path = worktree(cx, &ws, s.w.thread).unwrap();
    review_from_inspector(cx, window);

    click(cx, window, "review-discard");
    wait_to_show(cx, window, DISCARD_ASK);
    // Cancel keeps everything.
    click(cx, window, "discard-cancel");
    wait_until(cx, "the dialog to close", |cx| {
        !shows(cx, window, DISCARD_ASK)
    });
    assert!(path.exists());

    click(cx, window, "review-discard");
    wait_to_show(cx, window, DISCARD_ASK);
    click(cx, window, "discard-confirm");
    wait_until(cx, "the worktree to go", |cx| {
        worktree(cx, &ws, s.w.thread).is_none() && !path.exists()
    });
    wait_to_show(cx, window, "No changes to review.");
    assert_eq!(notifications(cx, window), 0, "no error");
}

#[gpui_kit::test]
fn commit_while_running_shows_the_wait_message(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let go = tmp.path().join("go");
    let s = setup(&format!(
        "printf 'one\\n' > new.txt\nwhile [ ! -e '{}' ]; do sleep 0.02; done",
        go.display()
    ));
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Working);
    wait_until(cx, "the worktree", |cx| {
        worktree(cx, &ws, s.w.thread).is_some_and(|p| p.join("new.txt").exists())
    });
    let thread = s.w.thread;
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_review(thread, window, cx));
    })
    .unwrap();
    wait_to_show(cx, window, "Forge's changes");
    wait_to_show(cx, window, "· 1 + one");

    // No reply yet, so you write the message.
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("")
    );
    assert!(!ws.read_with(cx, |ws, cx| ws.review_can_commit(cx)));
    click(cx, window, "review-message");
    type_text(cx, window, "Add one");
    assert!(ws.read_with(cx, |ws, cx| ws.review_can_commit(cx)));
    click(cx, window, "review-commit");
    wait_until(cx, "the error", |cx| notifications(cx, window) == 1);
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.last_error.clone()).as_deref(),
        Some("Wait for Forge to finish, or stop it.")
    );
    assert!(shows(cx, window, "· 1 + one"), "nothing was committed");

    std::fs::write(&go, "").unwrap();
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
}

/// Opens the review straight away, as the debug hook does.
fn open_review_now(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    thread: ThreadId,
) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_review(thread, window, cx));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_review_follows_the_run_and_drafts_from_its_reply(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let go = tmp.path().join("go");
    let s = setup(&format!(
        "printf 'one\\n' > new.txt\nwhile [ ! -e '{}' ]; do sleep 0.02; done\nprintf 'later\\n' > later.txt",
        go.display()
    ));
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Working);
    wait_until(cx, "the worktree", |cx| {
        worktree(cx, &ws, s.w.thread).is_some_and(|p| p.join("new.txt").exists())
    });
    open_review_now(cx, window, &ws, s.w.thread);
    wait_to_show(cx, window, "· 1 + one");
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("")
    );

    // The run ends: its new file and its reply show up without a refresh.
    std::fs::write(&go, "").unwrap();
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_to_show(cx, window, "· 1 + later");
    wait_until(cx, "the drafted message", |cx| {
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref() == Some("Fixed.")
    });
    assert!(shows(cx, window, "Commit message · drafted by Forge"));
}

#[gpui_kit::test]
fn esc_in_the_message_keeps_the_review_and_the_text(cx: &mut TestAppContext) {
    let s = setup("printf 'one\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("Fixed."),
        "drafted from Forge's reply"
    );

    click(cx, window, "review-message");
    type_text(cx, window, " Trims the email.");
    press(cx, window, "escape");
    assert!(
        shows(cx, window, "Forge's changes"),
        "Esc leaves the field only"
    );
    let typed = "Fixed. Trims the email.";
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some(typed)
    );

    // A second Esc leaves the review; coming back keeps the text.
    press(cx, window, "escape");
    wait_until(cx, "the thread", |cx| has(cx, window, "thread-view"));
    review_from_inspector(cx, window);
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some(typed)
    );
}
