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
pub(super) fn repo_with_readme(root: &Path) {
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
    setup_with(script, "edit_and_test.jsonl")
}

fn setup_with(script: &str, replay: &str) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    repo_with_readme(&root);
    let claude = fake_claude(
        tmp.path(),
        &format!("{script}\ncat '{}'", fixture(replay).display()),
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

#[gpui_kit::test]
fn a_read_only_reply_after_the_edits_keeps_the_editor_on_the_review(cx: &mut TestAppContext) {
    // Read-only runs reply "Done thinking." and change nothing.
    let s = setup(&format!(
        "case \"$*\" in *'--permission-mode plan'*) cat '{}'; exit 0;; esac\nprintf 'one\\n' > new.txt",
        fixture("thinking.jsonl").display()
    ));
    let scout: AgentId = ask(
        &s.w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::ReadOnly,
                ..agent("Scout", "Reviews")
            },
        },
    );
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    s.w.post("@Scout look it over");
    run_of(cx, &ws, s.w.thread, scout, RunStatus::Done);
    wait_until(cx, "Scout's reply", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .messages(s.w.thread)
                .iter()
                .any(|m| m.body == "Done thinking.")
        })
    });

    review_from_inspector(cx, window);
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.review_message(cx)).as_deref(),
        Some("Fixed."),
        "drafted from Forge's reply, not Scout's"
    );
    assert!(shows(cx, window, "Commit message · drafted by Forge"));
    click(cx, window, "review-discard");
    wait_to_show(cx, window, DISCARD_ASK);
    click(cx, window, "discard-cancel");
    wait_until(cx, "the dialog to close", |cx| {
        !shows(cx, window, DISCARD_ASK)
    });

    click(cx, window, "review-ask");
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx)),
        "@Forge "
    );
}

#[gpui_kit::test]
fn checks_the_agent_ran_are_listed_and_open_the_run(cx: &mut TestAppContext) {
    let s = setup_with("printf 'one\\n' > new.txt", "edit_and_test_ok.jsonl");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    let run = run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done).id;
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    // Hidden before, the inspector comes back for the command.
    ws.update(cx, |ws, _| ws.inspector_open = Some(false));

    wait_to_show(cx, window, "Checks Forge ran");
    assert_eq!(
        label_of(cx, window, ("review-check", 0usize)).as_deref(),
        Some("Passed cargo test")
    );
    assert!(!has(cx, window, ("review-check", 1usize)));

    click(cx, window, ("review-check", 0usize));
    assert!(has(cx, window, "thread-view"), "back at the thread");
    assert!(!shows(cx, window, "Forge's changes"));
    assert!(has(cx, window, "inspector"));
    wait_to_show(cx, window, "$ cargo test");
    let shown = ws.read_with(cx, |ws, _| ws.shown_run(s.w.thread).map(|r| r.id));
    assert_eq!(shown, Some(run));
}

#[gpui_kit::test]
fn a_failed_check_shows_as_failed(cx: &mut TestAppContext) {
    let s = setup("printf 'one\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "Failed cargo test");
}

#[gpui_kit::test]
fn a_running_check_shows_running_then_passes(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let go = tmp.path().join("go");
    let sid = r#""session_id":"00000000-0000-4000-8000-000000000001""#;
    let call = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_09","name":"Bash","input":{{"command":"npm test"}}}}]}},{sid}}}"#
    );
    let done = format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_09","content":"ok"}}]}},{sid}}}"#
    );
    let s = setup_with(
        &format!(
            "printf 'one\\n' > new.txt\ncat <<'EOF'\n{call}\nEOF\nwhile [ ! -e '{}' ]; do sleep 0.02; done\ncat <<'EOF'\n{done}\nEOF",
            go.display()
        ),
        "edit_and_test_ok.jsonl",
    );
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
    wait_to_show(cx, window, "Running npm test");

    std::fs::write(&go, "").unwrap();
    wait_to_show(cx, window, "Passed npm test");
    assert!(!shows(cx, window, "Running npm test"));
}

/// A fake claude: read-only runs reply with `reply` (JSON-escaped, and
/// `%s` stands for the run's folder), others edit as `script` says.
fn reviewer_script(script: &str, reply: &str) -> String {
    let sid = r#""session_id":"00000000-0000-4000-8000-000000000001""#;
    let text = format!(
        r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{reply}"}}]}},{sid}}}"#
    );
    let result = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"{reply}","duration_ms":5,"num_turns":1,"permission_denials":[],{sid}}}"#
    );
    // printf reads backslashes, so JSON's `\n` goes in as `\\n`.
    let (text, result) = (text.replace('\\', "\\\\"), result.replace('\\', "\\\\"));
    format!(
        "case \"$*\" in *'--permission-mode plan'*) d=$(pwd); printf '%s\\n' '{init}'; printf '{text}\\n' \"$d\"; printf '{result}\\n' \"$d\"; exit 0;; esac\n{script}",
        init = r#"{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"00000000-0000-4000-8000-000000000001","model":"claude-haiku-4-5","tools":["Read"]}"#,
    )
}

#[gpui_kit::test]
fn a_reviewer_reply_pins_its_comments_to_the_diff(cx: &mut TestAppContext) {
    let reply = "Two notes.\\n\\n- ./README.md:2: Say who.\\n- `%s/new.txt:1` - Check it.\\nnowhere.rs:4: Gone.";
    let s = setup(&reviewer_script(
        "printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt",
        reply,
    ));
    let scout: AgentId = ask(
        &s.w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::ReadOnly,
                ..agent("Scout", "Reviews")
            },
        },
    );
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 2);
    review_from_inspector(cx, window);
    // No reviewer yet: no comments, no callout.
    wait_to_show(cx, window, "· 2 + there");
    assert!(!has(cx, window, "review-callout"));
    assert!(!shows(cx, window, "Comments outside the diff"));

    // Scout replies while the review is open.
    s.w.post("@Scout look it over");
    run_of(cx, &ws, s.w.thread, scout, RunStatus::Done);
    wait_to_show(cx, window, "Scout left 3 comments.");
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.pinned_comments()),
        [
            (
                "README.md".to_string(),
                Some(2),
                "Scout · README.md:2".to_string()
            ),
            (
                "new.txt".to_string(),
                Some(1),
                "Scout · new.txt:1".to_string()
            ),
        ]
    );
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.outside_comments()),
        ["Scout · nowhere.rs:4"]
    );
    assert!(shows(cx, window, "Comments outside the diff"));
    assert!(shows(cx, window, "Scout · README.md:2"));
    assert!(shows(cx, window, "Say who."));
    assert!(shows(cx, window, "Scout · nowhere.rs:4"));

    // Forge changes things again: Scout's comments were about older work.
    s.w.post("@Forge once more");
    wait_until(cx, "Forge's second run", |cx| {
        ws.read_with(cx, |ws, _| {
            let runs = ws.state().thread_runs(s.w.thread);
            runs.len() == 3 && runs[2].status == RunStatus::Done
        })
    });
    wait_until(cx, "the comments to go", |cx| {
        !shows(cx, window, "Comments outside the diff")
    });
    assert!(ws.read_with(cx, |ws, _| ws.pinned_comments()).is_empty());
    assert!(!has(cx, window, "review-callout"));
}

#[gpui_kit::test]
fn split_shows_two_columns_and_is_remembered(cx: &mut TestAppContext) {
    let s = setup("printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 2);
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "· 2 + there");
    assert!(!shows(cx, window, "2 − world | 2 + there"));

    click(cx, window, "review-split");
    // The paired change reads as one row with a gutter on each side.
    assert!(shows(cx, window, "2 − world | 2 + there"));
    assert!(shows(cx, window, "2 − world"));
    assert!(shows(cx, window, "2 + there"));
    assert!(shows(cx, window, "1   hello | 1   hello"));
    // A new file has nothing on the left.
    assert!(shows(cx, window, " | 1 + one"));
    assert!(!shows(cx, window, "· 2 + there"));

    // The choice is kept when the review closes and opens again.
    press(cx, window, "escape");
    wait_to_show(cx, window, "fix the login");
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "2 − world | 2 + there");

    click(cx, window, "review-unified");
    assert!(shows(cx, window, "· 2 + there"));
    assert!(!shows(cx, window, "2 − world | 2 + there"));
}

#[gpui_kit::test]
fn split_of_a_deleted_file_has_an_empty_right_side(cx: &mut TestAppContext) {
    let s = setup("rm README.md");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "1 · − hello");
    click(cx, window, "review-split");
    assert!(shows(cx, window, "1 − hello | "));
    assert!(shows(cx, window, "2 − world | "));
}

#[gpui_kit::test]
fn the_list_keeps_its_place_when_the_rows_change(cx: &mut TestAppContext) {
    let s = setup("seq 1 100 > README.md");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "· 1 + 1");
    // Unified: file, hunk, two deletions, then the additions.
    ws.read_with(cx, |ws, _| ws.scroll_review_to(50));
    cx.run_until_parked();
    assert_eq!(ws.read_with(cx, |ws, _| ws.review_top()), Some(50));

    // The same line is two rows higher once the deletions pair up.
    click(cx, window, "review-split");
    assert_eq!(ws.read_with(cx, |ws, _| ws.review_top()), Some(48));
    click(cx, window, "review-unified");
    assert_eq!(ws.read_with(cx, |ws, _| ws.review_top()), Some(50));

    // Reading the changes again keeps the place too.
    click(cx, window, "review-refresh");
    wait_until(cx, "the list to settle", |cx| {
        ws.read_with(cx, |ws, _| ws.review_top()) == Some(50)
    });
}

/// Launch arguments `open` would have got, in order.
type Launches = std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>;

/// Swaps in these editors and an opener that records its arguments, or
/// fails when `fail` is set. Nothing is launched.
fn fake_editors(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    detected: &[&str],
    fail: bool,
) -> Launches {
    let launches = Launches::default();
    let seen = launches.clone();
    let opener: crate::editor::Opener = std::sync::Arc::new(move |args| {
        seen.lock().unwrap().push(
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        );
        if fail { Err("no".into()) } else { Ok(()) }
    });
    let detected = detected.iter().map(|d| d.to_string()).collect();
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.set_editor_env(detected, opener, window, cx));
    })
    .unwrap();
    launches
}

fn wait_for_launches(cx: &mut TestAppContext, launches: &Launches, n: usize) -> Vec<Vec<String>> {
    wait_until(cx, "the editor to be launched", |_| {
        launches.lock().unwrap().len() >= n
    });
    launches.lock().unwrap().clone()
}

#[gpui_kit::test]
fn open_in_editor_launches_the_chosen_editor_or_finder(cx: &mut TestAppContext) {
    let s = setup("printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    let launches = fake_editors(cx, window, &ws, &["Cursor", "Zed"], false);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 2);
    let wt = worktree(cx, &ws, s.w.thread).unwrap().display().to_string();
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "· 2 + there");

    // Nothing chosen: the first detected editor, on the folder.
    click(cx, window, "review-open-editor");
    assert_eq!(
        wait_for_launches(cx, &launches, 1)[0],
        vec!["-a".to_string(), "Cursor".into(), wt.clone()]
    );

    // A picked file goes along for an editor.
    click(cx, window, ("review-file", 1usize));
    click(cx, window, "review-open-editor");
    let sent = wait_for_launches(cx, &launches, 2);
    assert_eq!(sent[1][..3], ["-a", "Cursor", &wt]);
    assert_eq!(sent[1].len(), 4);
    assert!(sent[1][3].starts_with(&format!("{wt}/")), "{:?}", sent[1]);

    // Choosing another editor sticks, and is remembered.
    ws.update(cx, |ws, cx| ws.choose_editor("Zed".into(), cx));
    click(cx, window, "review-open-editor");
    assert_eq!(wait_for_launches(cx, &launches, 3)[2][..2], ["-a", "Zed"]);
    assert_eq!(
        crate::editor::load_choice(s._home.path()),
        Some("Zed".into())
    );

    // Finder opens the folder only, even with a file picked.
    ws.update(cx, |ws, cx| ws.choose_editor("Finder".into(), cx));
    click(cx, window, "review-open-editor");
    assert_eq!(wait_for_launches(cx, &launches, 4)[3], vec![wt]);
    assert!(!has(cx, window, "review-open-error"));
}

#[gpui_kit::test]
fn the_picked_file_stays_the_same_file_after_the_diff_reloads(cx: &mut TestAppContext) {
    let s = setup("printf 'hello\\nthere\\n' > README.md\nprintf 'one\\ntwo\\n' > new.txt");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    let launches = fake_editors(cx, window, &ws, &["Zed"], false);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 2);
    let wt = worktree(cx, &ws, s.w.thread).unwrap();
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "· 2 + there");

    // Pick new.txt, the second file, then a file sorting first appears.
    click(cx, window, ("review-file", 1usize));
    std::fs::write(wt.join("aaa.txt"), "a\n").unwrap();
    click(cx, window, "review-refresh");
    wait_to_show(cx, window, "aaa.txt");

    click(cx, window, "review-open-editor");
    let sent = wait_for_launches(cx, &launches, 1);
    assert_eq!(
        sent[0].last().unwrap(),
        &wt.join("new.txt").display().to_string()
    );
}

#[gpui_kit::test]
fn a_failed_launch_says_so_under_the_review_title(cx: &mut TestAppContext) {
    let s = setup("printf 'hello\\nthere\\n' > README.md");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    let launches = fake_editors(cx, window, &ws, &["Zed"], true);
    s.w.post("@Forge fix it");
    run_of(cx, &ws, s.w.thread, s.forge, RunStatus::Done);
    wait_for_changes(cx, &ws, 1);
    review_from_inspector(cx, window);
    wait_to_show(cx, window, "· 2 + there");

    click(cx, window, "review-open-editor");
    wait_for_launches(cx, &launches, 1);
    wait_to_show(cx, window, "Zed didn't open. Check that it is installed.");
    assert!(has(cx, window, "review-open-error"));
}

#[gpui_kit::test]
fn general_offers_finder_and_the_detected_editors(cx: &mut TestAppContext) {
    let s = setup("exit 0");
    let (window, ws) = open_thread(cx, s._home.path(), &s.w);
    fake_editors(cx, window, &ws, &["Cursor", "Zed"], false);
    click(cx, window, "title-settings");
    click(cx, window, "settings-nav-general");
    wait_until(cx, "the Editor row", |cx| has(cx, window, "general-editor"));
    assert!(!has(cx, window, "general-max-runs"));
    // The default is the first detected editor.
    let shown = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, cx| {
            ws.editor_select
                .read(cx)
                .selected_value()
                .map(|v| v.to_string())
        })
    };
    assert_eq!(shown(cx), Some("Cursor".to_string()));
    ws.update(cx, |ws, cx| ws.choose_editor("Finder".into(), cx));
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.editor_app()),
        None,
        "Finder means no app"
    );
}
