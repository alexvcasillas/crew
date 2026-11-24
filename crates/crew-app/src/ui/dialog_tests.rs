//! End to end: the empty channel, the new thread composer and the thread
//! and channel dialogs, with a real crewd on a temp home and fake claude
//! scripts.

use std::path::Path;

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ThreadId};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::diff_tests::repo_with_readme;
use super::inspector_tests::run_of;
use super::sidebar::row_probe::row_of;
use super::test_ui::{
    bounds_of, click, has, label_of, open, press, type_text, wait_to_show, wait_until,
};
use super::thread_tests::{World, agent, ask, go_to, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, fake_claude, fixture, short_home};

fn go_to_channel(cx: &mut TestAppContext, window: AnyWindowHandle, channel: ChannelId) {
    wait_until(cx, "the channel in the sidebar", |cx| {
        has(cx, window, row_of(channel))
    });
    click(cx, window, row_of(channel));
    wait_until(cx, "the channel view", |cx| has(cx, window, "channel-view"));
}

fn card(thread: ThreadId) -> String {
    format!("thread-{thread}")
}

fn composer_text(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> String {
    ws.read_with(cx, |ws, cx| ws.composer.read(cx).text(cx))
}

fn wait_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str, want: &str) {
    let mut last = None;
    wait_until(cx, &format!("{id} to read {want:?}"), |cx| {
        last = label_of(cx, window, id.to_string());
        last.as_deref() == Some(want)
    });
}

/// Opens the thread, then goes back to its channel, so its card is the
/// selected one.
fn select_card(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    w: &World,
    thread: ThreadId,
) {
    go_to(cx, window, ws, w.channel, thread);
    go_to_channel(cx, window, w.channel);
    wait_until(cx, "the card", |cx| has(cx, window, card(thread)));
    ws.read_with(cx, |ws, _| {
        assert_eq!(ws.channel_view.selected, Some(thread));
    });
}

fn dialog_shown(cx: &mut TestAppContext, window: AnyWindowHandle) -> bool {
    has(cx, window, "dialog-confirm")
}

fn wait_dialog_closed(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) {
    wait_until(cx, "the dialog to close", |cx| {
        ws.read_with(cx, |ws, _| ws.dialog.is_none()) && !dialog_shown(cx, window)
    });
}

/// Replaces the text of the focused field.
fn retype(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, text);
}

#[gpui_kit::test]
fn an_empty_channel_offers_agents_and_starts_a_thread(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge", "Prism"]);
    let design: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "design".into(),
    });
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, design);

    wait_to_show(cx, window, "No threads yet");
    assert!(
        !has(cx, window, "channel-stat-threads"),
        "nothing to sum up yet"
    );
    assert_eq!(
        label_of(cx, window, "empty-composer-base").as_deref(),
        Some("Starts in a new worktree off main")
    );
    let forge = w.agents[0];
    let chip = format!("bring-in-{forge}");
    assert_eq!(
        label_of(cx, window, chip.clone()).as_deref(),
        Some("@Forge")
    );
    assert!(has(cx, window, format!("bring-in-{}", w.agents[1])));

    click(cx, window, chip);
    assert_eq!(composer_text(cx, &ws), "@Forge ");
    type_text(cx, window, "tidy the docs");
    press(cx, window, "enter");

    let mut created = None;
    wait_until(cx, "the new thread to open", |cx| {
        created = ws.read_with(cx, |ws, _| ws.open_thread_id());
        created.is_some_and(|t| ws.read_with(cx, |ws, _| !ws.state().messages(t).is_empty()))
    });
    ws.read_with(cx, |ws, _| {
        let state = ws.state();
        let thread = state.thread(created.unwrap()).unwrap();
        assert_eq!(thread.channel, design);
        assert_eq!(thread.title, "@Forge tidy the docs");
        assert_eq!(state.messages(thread.id)[0].body, "@Forge tidy the docs");
    });
}

#[gpui_kit::test]
fn cmd_n_opens_the_composer_and_previews_the_branch(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, w.channel);
    wait_until(cx, "the card", |cx| has(cx, window, card(w.thread)));

    press(cx, window, "cmd-n");
    assert!(has(cx, window, "new-thread-composer"));
    assert_eq!(
        label_of(cx, window, "new-thread-composer").as_deref(),
        Some("New thread")
    );
    assert!(has(cx, window, card(w.thread)), "the cards stay below");
    assert!(!has(cx, window, "channel-stat-threads"));
    type_text(cx, window, "Add a retry flag");
    wait_label(
        cx,
        window,
        "new-thread-branch",
        "Starts in a new worktree on branch crew/add-a-retry-flag off main",
    );
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, "@Forge add a --retry-jitter flag");
    wait_label(
        cx,
        window,
        "new-thread-branch",
        "Starts in a new worktree on branch crew/forge-add-a-retry-jitter-flag off main",
    );
    assert_eq!(
        label_of(cx, window, "new-thread-with").as_deref(),
        Some("With Forge")
    );

    // esc cancels and drops what you wrote.
    press(cx, window, "escape");
    wait_until(cx, "the composer to close", |cx| {
        !has(cx, window, "new-thread-composer")
    });
    assert!(has(cx, window, "channel-stat-threads"));
    press(cx, window, "cmd-n");
    assert_eq!(composer_text(cx, &ws), "");

    // The @ menu opens below: there is no room above.
    type_text(cx, window, "@");
    let menu = bounds_of(cx, window, "mention-menu").expect("the @ menu");
    let composer = bounds_of(cx, window, "new-thread-composer").unwrap();
    assert!(menu.origin.y > composer.origin.y, "{menu:?} {composer:?}");
    press(cx, window, "escape backspace");

    // ↵ starts the thread and opens it.
    type_text(cx, window, "Write the changelog");
    press(cx, window, "enter");
    let mut created = None;
    wait_until(cx, "the new thread to open", |cx| {
        created = ws.read_with(cx, |ws, _| ws.open_thread_id());
        created.is_some()
    });
    ws.read_with(cx, |ws, _| {
        let thread = ws.state().thread(created.unwrap()).unwrap();
        assert_eq!(thread.title, "Write the changelog");
        assert_eq!(thread.channel, w.channel);
    });
}

#[gpui_kit::test]
fn the_rename_dialog_renames_the_thread(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    select_card(cx, window, &ws, &w, w.thread);

    press(cx, window, "cmd-r");
    wait_to_show(cx, window, "Rename thread");
    assert!(!has(cx, window, "rename-branch"), "no branch yet");
    retype(cx, window, "  ");
    click(cx, window, "dialog-confirm");
    wait_label(cx, window, "dialog-error", "Give the thread a title.");

    retype(cx, window, "Fix the login on Safari");
    press(cx, window, "enter");
    wait_dialog_closed(cx, window, &ws);
    wait_label(
        cx,
        window,
        &format!("thread-title-{}", w.thread),
        "Fix the login on Safari",
    );
}

#[gpui_kit::test]
fn archive_moves_the_card_and_unarchive_brings_it_back(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    select_card(cx, window, &ws, &w, w.thread);

    press(cx, window, "cmd-backspace");
    wait_label(
        cx,
        window,
        "archive-heading",
        "Archive this thread? “fix the login” moves to Archived in #general. You can bring it back any time.",
    );
    assert!(!has(cx, window, "archive-note"), "no worktree");
    click(cx, window, "dialog-confirm");
    wait_dialog_closed(cx, window, &ws);
    wait_until(cx, "the card to go", |cx| !has(cx, window, card(w.thread)));
    wait_label(cx, window, "channel-filter-archived", "Archived 1");

    click(cx, window, "channel-filter-archived");
    assert!(has(cx, window, card(w.thread)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(card(w.thread), cx);
    })
    .unwrap();
    click(cx, window, format!("thread-more-{}", w.thread));
    // Rename, then Unarchive: no branch to copy, no worktree to discard.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    })
    .unwrap();
    wait_until(cx, "the card to leave Archived", |cx| {
        !has(cx, window, card(w.thread))
    });
    click(cx, window, "channel-filter-all");
    assert!(has(cx, window, card(w.thread)));
    ws.read_with(cx, |ws, _| {
        assert!(ws.state().thread(w.thread).unwrap().archived_at.is_none());
    });
}

/// A project on a real repo with Forge, who can edit, and a claude that
/// runs `script`.
fn editing_world(home: &Path, tmp: &Path, script: &str) -> (Daemon, World, AgentId) {
    let root = tmp.join("repo");
    repo_with_readme(&root);
    let claude = fake_claude(tmp, script);
    let daemon = Daemon::start_with(home.to_path_buf(), claude);
    let w = world(home, &root, &[]);
    let forge: AgentId = ask(
        &w.client,
        Request::CreateAgent {
            spec: crew_core::agent::AgentSpec {
                permission_mode: PermissionMode::EditFiles,
                ..agent("Forge", "Builds")
            },
        },
    );
    (daemon, w, forge)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[gpui_kit::test]
fn discard_with_delete_branch_removes_the_worktree_and_branch(cx: &mut TestAppContext) {
    let home = short_home();
    let tmp = tempfile::tempdir().unwrap();
    let (_daemon, w, forge) = editing_world(
        home.path(),
        tmp.path(),
        &format!(
            "printf 'hello\\n' > README.md\nprintf 'a\\n' > a.txt\nprintf 'b\\n' > b.txt\ncat '{}'",
            fixture("edit_and_test.jsonl").display()
        ),
    );
    let (window, ws) = open(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Done);
    let (path, branch) = ws.read_with(cx, |ws, _| {
        let wt = ws
            .state()
            .thread(w.thread)
            .unwrap()
            .worktree
            .clone()
            .unwrap();
        (wt.path, wt.branch)
    });
    assert!(path.exists());
    select_card(cx, window, &ws, &w, w.thread);

    // The archive dialog says the files stay.
    wait_label(
        cx,
        window,
        &format!("thread-worktree-{}", w.thread),
        "3 uncommitted",
    );
    press(cx, window, "cmd-backspace");
    wait_label(
        cx,
        window,
        "archive-note",
        "The worktree and its 3 uncommitted files stay on disk. Discard it separately if you don't need them.",
    );
    click(cx, window, "dialog-cancel");
    wait_dialog_closed(cx, window, &ws);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(card(w.thread), cx);
    })
    .unwrap();
    click(cx, window, format!("thread-more-{}", w.thread));
    // Rename, Copy branch name, Open worktree in terminal, Archive, a
    // separator, Discard worktree.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(5usize, cx);
    })
    .unwrap();
    wait_label(
        cx,
        window,
        "discard-heading",
        &format!(
            "Discard the worktree? This deletes the folder for {branch}. Uncommitted changes can't be recovered."
        ),
    );
    wait_until(cx, "the files", |cx| has(cx, window, "discard-more"));
    assert_eq!(
        label_of(cx, window, "discard-more").as_deref(),
        Some("and 1 more file")
    );
    assert_eq!(
        label_of(cx, window, "dialog-confirm").as_deref(),
        Some("Discard 3 files")
    );
    click(cx, window, "discard-delete-branch");
    click(cx, window, "dialog-confirm");
    wait_dialog_closed(cx, window, &ws);
    wait_until(cx, "the worktree to go", |cx| {
        !path.exists()
            && ws.read_with(cx, |ws, _| {
                ws.state().thread(w.thread).unwrap().worktree.is_none()
            })
    });
    assert_eq!(
        git(&tmp.path().join("repo"), &["branch", "--list", &branch]),
        "",
        "the branch went too"
    );
}

#[gpui_kit::test]
fn discard_while_an_agent_works_is_refused(cx: &mut TestAppContext) {
    let home = short_home();
    let tmp = tempfile::tempdir().unwrap();
    let (_daemon, w, forge) = editing_world(
        home.path(),
        tmp.path(),
        &format!(
            "printf 'hello\\n' > README.md\nhead -n 1 '{}'\nexec sleep 60",
            fixture("read_file.jsonl").display()
        ),
    );
    let (window, ws) = open(cx, home.path());
    w.post("@Forge fix it");
    run_of(cx, &ws, w.thread, forge, RunStatus::Working);
    let mut path = None;
    wait_until(cx, "the worktree", |cx| {
        path = ws.read_with(cx, |ws, _| {
            ws.state()
                .thread(w.thread)
                .unwrap()
                .worktree
                .clone()
                .map(|wt| wt.path)
        });
        path.is_some()
    });
    let path = path.unwrap();
    go_to_channel(cx, window, w.channel);
    wait_until(cx, "the card", |cx| has(cx, window, card(w.thread)));
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_discard_dialog(w.thread, window, cx));
    })
    .unwrap();
    wait_until(cx, "the dialog", |cx| has(cx, window, "discard-heading"));
    click(cx, window, "discard-delete-branch");
    click(cx, window, "dialog-confirm");
    wait_label(cx, window, "dialog-error", "Stop Forge first.");
    assert!(path.exists(), "nothing was deleted");
    assert!(dialog_shown(cx, window));
    let () = w.ask(Request::StopThread { thread: w.thread });
}

#[gpui_kit::test]
fn channel_settings_saves_a_description_and_rejects_bad_names(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let _: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "bugs".into(),
    });
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, w.channel);

    // No description yet: the link opens settings with it to fill in.
    click(cx, window, "channel-add-description");
    wait_until(cx, "the dialog", |cx| {
        has(cx, window, "channel-settings-heading")
    });
    type_text(cx, window, "Small fixes and chores");
    click(cx, window, "dialog-confirm");
    wait_dialog_closed(cx, window, &ws);
    wait_label(cx, window, "channel-description", "Small fixes and chores");

    // A bad name is caught before crewd, a taken one by crewd.
    click(cx, window, "channel-settings");
    wait_until(cx, "the dialog", |cx| {
        has(cx, window, "channel-settings-heading")
    });
    retype(cx, window, "Two Words");
    press(cx, window, "enter");
    wait_label(
        cx,
        window,
        "dialog-error",
        "Use lowercase letters, numbers and dashes.",
    );
    retype(cx, window, "bugs");
    press(cx, window, "enter");
    wait_label(cx, window, "dialog-error", "#bugs already exists in demo.");
    retype(cx, window, "#chores");
    press(cx, window, "enter");
    wait_dialog_closed(cx, window, &ws);
    wait_label(cx, window, "channel-title", "#chores");
}

/// A channel named before the rules keeps its name and still saves.
#[gpui_kit::test]
fn a_channel_with_an_old_name_still_saves_its_description(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let legacy: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "Bugs".into(),
    });
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, legacy);
    click(cx, window, "channel-add-description");
    wait_until(cx, "the dialog", |cx| {
        has(cx, window, "channel-settings-heading")
    });
    type_text(cx, window, "Things that broke");
    press(cx, window, "enter");
    wait_dialog_closed(cx, window, &ws);
    wait_label(cx, window, "channel-description", "Things that broke");
    wait_label(cx, window, "channel-title", "#Bugs");
}

#[gpui_kit::test]
fn shortcuts_act_only_on_cards_in_view_and_unarchive_archived_ones(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    select_card(cx, window, &ws, &w, w.thread);

    // A search hides the selected card: ⌘R leaves it alone.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.channel_view
                .search
                .update(cx, |search, cx| search.set_value("zzz", window, cx));
        });
    })
    .unwrap();
    wait_until(cx, "the card to hide", |cx| {
        !has(cx, window, card(w.thread))
    });
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.focus_window(window, cx));
    })
    .unwrap();
    press(cx, window, "cmd-r");
    assert!(!dialog_shown(cx, window), "no dialog for a hidden card");

    // Archived, under Archived: ⌘⌫ brings it back.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.channel_view
                .search
                .update(cx, |search, cx| search.set_value("", window, cx));
        });
    })
    .unwrap();
    let () = w.ask(Request::ArchiveThread {
        thread: w.thread,
        archived: true,
    });
    wait_label(cx, window, "channel-filter-archived", "Archived 1");
    click(cx, window, "channel-filter-archived");
    wait_until(cx, "the archived card", |cx| {
        has(cx, window, card(w.thread))
    });
    press(cx, window, "cmd-backspace");
    assert!(!dialog_shown(cx, window), "unarchiving doesn't ask");
    wait_until(cx, "the card to come back", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().thread(w.thread).unwrap().archived_at.is_none()
        })
    });
}

#[gpui_kit::test]
fn text_fields_keep_their_delete_shortcuts(cx: &mut TestAppContext) {
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let tmp = tempfile::tempdir().unwrap();
    let w = world(home.path(), &tmp.path().join("repo"), &[]);
    let (window, ws) = open(cx, home.path());
    go_to_channel(cx, window, w.channel);
    select_card(cx, window, &ws, &w, w.thread);

    press(cx, window, "cmd-n");
    type_text(cx, window, "hello brave world");
    press(cx, window, "alt-backspace");
    wait_label(
        cx,
        window,
        "new-thread-branch",
        "Starts in a new worktree on branch crew/hello-brave off main",
    );
    press(cx, window, "cmd-backspace");
    type_text(cx, window, "x");
    wait_label(
        cx,
        window,
        "new-thread-branch",
        "Starts in a new worktree on branch crew/x off main",
    );
    assert!(
        !dialog_shown(cx, window),
        "⌘⌫ while typing must not archive the selected thread"
    );
}
