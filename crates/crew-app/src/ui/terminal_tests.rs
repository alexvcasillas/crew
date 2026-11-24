//! End to end: Claude Code sessions run in a terminal, shown read-only,
//! with a real crewd reading a synthetic claude home. Never the real one.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crew_core::ids::{ChannelId, ProjectId};
use crew_proto::Request;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext};
use tempfile::TempDir;

use super::terminal_view::TerminalRow;
use super::test_ui::{click, has, label_of, open, shows, shows_within, wait_until};
use super::thread_tests::ask;
use super::workspace::Workspace;
use crate::client::{self, ClientEvent, ClientHandle};
use crate::connection::ConnectionConfig;
use crate::test_support::{Daemon, recording_proxy, short_home};

/// A crewd reading `claude_home`, and a project `demo` at `root`.
struct Setup {
    daemon: Option<Daemon>,
    home: PathBuf,
    _dirs: TempDir,
    claude_home: PathBuf,
    root: PathBuf,
    project: ProjectId,
    channel: ChannelId,
    _client: ClientHandle,
    _events: UnboundedReceiver<ClientEvent>,
}

impl Setup {
    fn new(home: &Path) -> Setup {
        let dirs = tempfile::tempdir().unwrap();
        // Claude Code writes the real path, as `getcwd` gives it.
        let base = std::fs::canonicalize(dirs.path()).unwrap();
        let claude_home = base.join("claude");
        let root = base.join("demo");
        // A registry folder, so only its entries make a session live.
        for dir in [
            &claude_home.join("projects"),
            &claude_home.join("sessions"),
            &root,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let daemon = start(home, &claude_home);
        let (client, events) = client::spawn(ConnectionConfig {
            crew_home: home.to_path_buf(),
        });
        let project: ProjectId = ask(
            &client,
            Request::CreateProject {
                name: "demo".into(),
                root: root.to_string_lossy().into_owned(),
                main_branch: "main".into(),
            },
        );
        let channel: ChannelId = ask(
            &client,
            Request::CreateChannel {
                project,
                name: "general".into(),
            },
        );
        Setup {
            daemon: Some(daemon),
            home: home.to_path_buf(),
            _dirs: dirs,
            claude_home,
            root,
            project,
            channel,
            _client: client,
            _events: events,
        }
    }

    /// Stops crewd, runs `meanwhile`, then starts it again on the same
    /// homes.
    fn restart(&mut self, meanwhile: impl FnOnce(&Self)) {
        drop(self.daemon.take());
        meanwhile(self);
        self.daemon = Some(start(&self.home, &self.claude_home));
    }

    /// Writes session `id` whole: a prompt and one reply.
    fn write(&self, id: &str, prompt: &str, answer: &str) -> PathBuf {
        let path = self.transcript(id);
        let lines = [user(&self.root, 1, prompt), reply(&self.root, 2, answer)];
        // Renamed into place, so crewd never reads half a file.
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, lines.join("\n") + "\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        path
    }

    /// The file Claude Code writes for session `id` started in the
    /// project's root.
    fn transcript(&self, id: &str) -> PathBuf {
        let folder: String = self
            .root
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = self.claude_home.join("projects").join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{id}.jsonl"))
    }

    /// A session with one prompt and one reply.
    fn session(&self, id: &str, prompt: &str) -> PathBuf {
        let path = self.transcript(id);
        let lines = [user(&self.root, 1, prompt), reply(&self.root, 2, "On it.")];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// Claude Code's registry entry for session `id`, run by this test
    /// process so its pid is alive.
    fn register(&self, id: &str, status: &str) {
        let entry = serde_json::json!({
            "pid": std::process::id(),
            "sessionId": id,
            "cwd": self.root,
            "kind": "interactive",
            "entrypoint": "cli",
            "status": status,
        });
        let path = self.claude_home.join("sessions").join("entry.json");
        // Written whole, so crewd never reads half an entry.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, entry.to_string()).unwrap();
        std::fs::rename(tmp, path).unwrap();
    }
}

/// crewd on `home`, reading `claude_home`.
fn start(home: &Path, claude_home: &Path) -> Daemon {
    Daemon::start_with_options(
        home.to_path_buf(),
        crewd::ServeOptions {
            claude_bin: Some(PathBuf::from("/nonexistent/claude")),
            claude_home: Some(claude_home.to_path_buf()),
            ..Default::default()
        },
    )
}

fn line(root: &Path, kind: &str, second: u32, message: serde_json::Value) -> String {
    serde_json::json!({
        "type": kind,
        "message": message,
        "uuid": format!("{kind}{second}"),
        "sessionId": "x",
        "timestamp": format!("2026-11-20T10:00:{second:02}.000Z"),
        "cwd": root,
        "entrypoint": "cli",
        "isSidechain": false,
    })
    .to_string()
}

fn user(root: &Path, second: u32, text: &str) -> String {
    line(
        root,
        "user",
        second,
        serde_json::json!({"role": "user", "content": text}),
    )
}

fn reply(root: &Path, second: u32, text: &str) -> String {
    assistant(
        root,
        second,
        serde_json::json!({"type": "text", "text": text}),
    )
}

fn assistant(root: &Path, second: u32, block: serde_json::Value) -> String {
    line(
        root,
        "assistant",
        second,
        serde_json::json!({"id": format!("msg_{second}"), "role": "assistant", "content": [block]}),
    )
}

fn tool_result(root: &Path, second: u32, id: &str) -> String {
    line(
        root,
        "user",
        second,
        serde_json::json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "ok", "is_error": false}
        ]}),
    )
}

fn append(path: &Path, lines: &[String]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

/// Waits for the project's "In terminal" row and opens it.
fn expand(cx: &mut TestAppContext, window: AnyWindowHandle, s: &Setup, label: &str) {
    let group = format!("terminal-group-{}", s.project);
    wait_until(cx, label, |cx| {
        label_of(cx, window, group.clone()).as_deref() == Some(label)
    });
    click(cx, window, group);
}

/// Opens a window on `home` and the session from its sidebar.
fn open_session(
    cx: &mut TestAppContext,
    home: &Path,
    s: &Setup,
    id: &str,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, ws) = open(cx, home);
    expand(cx, window, s, "In terminal · 1");
    click(cx, window, format!("terminal-session-{id}"));
    wait_until(cx, "the session view", |cx| {
        has(cx, window, "terminal-view")
    });
    (window, ws)
}

#[gpui_kit::test]
fn a_terminal_session_is_listed_under_its_project(cx: &mut TestAppContext) {
    let home = short_home();
    let s = Setup::new(home.path());
    s.session("s-one", "Fix the flaky test\nIt fails on CI");
    let (window, ws) = open(cx, home.path());
    let ticking = |cx: &mut TestAppContext| ws.read_with(cx, |ws, _| ws.terminal_ticking());
    let group = format!("terminal-group-{}", s.project);
    wait_until(cx, "the terminal row", |cx| has(cx, window, group.clone()));
    assert!(!ticking(cx), "nothing listed, nothing to refresh");
    expand(cx, window, &s, "In terminal · 1");
    assert_eq!(
        label_of(cx, window, "terminal-session-s-one").as_deref(),
        Some("Fix the flaky test")
    );
    assert!(!has(cx, window, "terminal-live-s-one"), "not live");
    assert!(ticking(cx), "times refresh while listed");

    // Collapsed again, the sessions are hidden.
    click(cx, window, group);
    assert!(!has(cx, window, "terminal-session-s-one"));
    assert!(!ticking(cx));
}

#[gpui_kit::test]
fn a_project_without_terminal_sessions_has_no_terminal_row(cx: &mut TestAppContext) {
    let home = short_home();
    let s = Setup::new(home.path());
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the project", |cx| {
        has(cx, window, format!("project-{}", s.project))
    });
    ws.read_with(cx, |ws, _| {
        assert!(ws.state().terminal_sessions(s.project).is_empty())
    });
    assert!(!has(cx, window, format!("terminal-group-{}", s.project)));
}

#[gpui_kit::test]
fn opening_a_terminal_session_streams_its_lines_read_only(cx: &mut TestAppContext) {
    let crewd_home = short_home();
    let s = Setup::new(crewd_home.path());
    let path = s.session("s-one", "Fix the flaky test");
    let home = short_home();
    let seen = recording_proxy(
        &home.path().join("crewd.sock"),
        crewd_home.path().join("crewd.sock"),
    );
    let (window, ws) = open_session(cx, home.path(), &s, "s-one");

    assert_eq!(
        label_of(cx, window, "terminal-title").as_deref(),
        Some("Fix the flaky test")
    );
    assert!(shows(cx, window, "terminal"), "the chip");
    assert!(shows(cx, window, "Read only. Reply in your terminal."));
    assert_eq!(
        label_of(cx, window, "terminal-cwd").as_deref(),
        Some(s.root.to_string_lossy().as_ref())
    );
    wait_until(cx, "the backlog", |cx| {
        shows_within(cx, window, "terminal-list", "On it.")
    });
    assert!(shows_within(
        cx,
        window,
        "terminal-list",
        "Fix the flaky test"
    ));
    assert!(!has(cx, window, "composer"), "no composer");
    assert!(!has(cx, window, "inspector"), "no inspector");

    append(
        &path,
        &[
            assistant(
                &s.root,
                3,
                serde_json::json!({"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test"}}),
            ),
            tool_result(&s.root, 4, "t1"),
            assistant(
                &s.root,
                5,
                serde_json::json!({"type": "thinking", "thinking": "Maybe the timeout."}),
            ),
            reply(&s.root, 6, "Tests pass now."),
        ],
    );
    wait_until(cx, "the new lines", |cx| {
        shows_within(cx, window, "terminal-list", "Tests pass now.")
    });
    assert!(shows_within(cx, window, "terminal-list", "Ran cargo test"));
    assert!(shows_within(cx, window, "terminal-list", "Thinking"));
    assert!(
        !shows_within(cx, window, "terminal-list", "Maybe the timeout."),
        "thinking starts collapsed"
    );
    let thinking = ws.read_with(cx, |ws, _| {
        let rows = &ws.terminal.as_ref().unwrap().rows;
        rows.iter()
            .position(|r| matches!(r, TerminalRow::Thinking { .. }))
            .unwrap()
    });
    click(cx, window, ("terminal-thinking", thinking));
    assert!(shows_within(
        cx,
        window,
        "terminal-list",
        "Maybe the timeout."
    ));

    // Leaving unsubscribes.
    click(cx, window, format!("channel-{}", s.channel));
    assert!(!has(cx, window, "terminal-view"));
    ws.read_with(cx, |ws, _| {
        assert!(!ws.state().is_terminal_watched("s-one"));
    });
    wait_until(cx, "the unsubscribe", |_| {
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, Request::UnsubscribeTerminal { session } if session == "s-one"))
    });
}

#[gpui_kit::test]
fn a_replaced_terminal_session_fills_again(cx: &mut TestAppContext) {
    let home = short_home();
    let s = Setup::new(home.path());
    s.write("s-one", "Fix the flaky test", "**Old** reply.");
    let (window, _ws) = open_session(cx, home.path(), &s, "s-one");
    wait_until(cx, "the backlog", |cx| {
        shows_within(cx, window, "terminal-list", "bold Old")
    });

    // A new file in its place: crewd resets the session, the app asks
    // for its backlog again.
    s.write("s-one", "Fix the flaky test", "**New** reply.");
    wait_until(cx, "the new backlog", |cx| {
        shows_within(cx, window, "terminal-list", "bold New")
    });
    assert!(!shows_within(cx, window, "terminal-list", "bold Old"));
}

/// crewd numbers a session's items from 0 again after a restart, so the
/// same seq can hold other text than before.
#[gpui_kit::test]
fn a_session_changed_while_crewd_was_away_shows_its_new_text(cx: &mut TestAppContext) {
    let home = short_home();
    let mut s = Setup::new(home.path());
    s.write("s-one", "Fix the flaky test", "**Old** reply.");
    let (window, ws) = open_session(cx, home.path(), &s, "s-one");
    let said_row = |cx: &mut TestAppContext| {
        ws.read_with(cx, |ws, _| {
            ws.terminal.as_ref().and_then(|t| {
                t.rows
                    .iter()
                    .position(|r| matches!(r, TerminalRow::Said { .. }))
            })
        })
    };
    wait_until(cx, "the backlog", |cx| {
        shows_within(cx, window, "terminal-list", "bold Old")
    });
    let said = said_row(cx).unwrap();

    s.restart(|s| {
        s.write("s-one", "Fix the flaky test", "**New** reply.");
    });
    wait_until(cx, "the new text", |cx| {
        shows_within(cx, window, "terminal-list", "bold New")
    });
    assert!(!shows_within(cx, window, "terminal-list", "bold Old"));
    assert_eq!(said_row(cx), Some(said), "the same row");
    assert!(has(cx, window, "terminal-view"), "still open");
}

#[gpui_kit::test]
fn the_live_dot_clears_when_the_session_goes_idle(cx: &mut TestAppContext) {
    let home = short_home();
    let s = Setup::new(home.path());
    s.register("s-one", "busy");
    s.session("s-one", "Fix the flaky test");
    let (window, ws) = open(cx, home.path());
    expand(cx, window, &s, "In terminal · 1");
    wait_until(cx, "the live dot", |cx| {
        has(cx, window, "terminal-live-s-one")
    });
    assert_eq!(
        label_of(cx, window, "terminal-live-s-one").as_deref(),
        Some("live")
    );
    assert_eq!(
        label_of(cx, window, "status-terminal").as_deref(),
        Some("1 in terminal")
    );
    ws.read_with(cx, |ws, _| {
        assert_eq!(
            ws.run_counts(),
            (0, 0),
            "terminal sessions aren't Crew runs"
        );
    });
    assert!(!has(cx, window, "title-running"));

    s.register("s-one", "idle");
    wait_until(cx, "the dot to clear", |cx| {
        !has(cx, window, "terminal-live-s-one")
    });
    assert!(has(cx, window, "terminal-session-s-one"), "still listed");
    assert!(!has(cx, window, "status-terminal"));
}

#[gpui_kit::test]
fn running_now_lists_live_terminal_sessions(cx: &mut TestAppContext) {
    use gpui_kit::{Action as _, AppContext as _};
    let home = short_home();
    let s = Setup::new(home.path());
    s.register("s-one", "busy");
    s.session("s-one", "Fix the flaky test");
    s.session("s-two", "Idle one");
    let (window, ws) = open(cx, home.path());
    wait_until(cx, "the live session", |cx| {
        ws.read_with(cx, |ws, _| ws.state().live_terminal_sessions().len() == 1)
    });
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(crate::app::ShowRunning.boxed_clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        label_of(cx, window, "running-group-terminal").as_deref(),
        Some("IN TERMINAL · 1")
    );
    assert!(has(cx, window, "running-terminal-s-one"));
    assert!(!has(cx, window, "running-terminal-s-two"), "only live ones");
    assert_eq!(
        label_of(cx, window, "running-terminal-state-s-one").as_deref(),
        Some("live")
    );
    assert_eq!(
        label_of(cx, window, "running-summary").as_deref(),
        Some("Nothing is running."),
        "Crew runs only"
    );

    click(cx, window, "running-terminal-open-s-one");
    assert!(!has(cx, window, "running"));
    assert!(has(cx, window, "terminal-view"));
    ws.read_with(cx, |ws, _| {
        assert!(ws.state().is_terminal_watched("s-one"));
    });
}

#[gpui_kit::test]
fn a_deleted_terminal_session_goes_away(cx: &mut TestAppContext) {
    let home = short_home();
    let s = Setup::new(home.path());
    let path = s.session("s-one", "Fix the flaky test");
    let (window, ws) = open_session(cx, home.path(), &s, "s-one");

    std::fs::remove_file(&path).unwrap();
    wait_until(cx, "the session to go", |cx| {
        !has(cx, window, "terminal-session-s-one")
    });
    assert!(!has(cx, window, "terminal-view"), "its view closes");
    assert!(!has(cx, window, format!("terminal-group-{}", s.project)));
    ws.read_with(cx, |ws, _| {
        assert!(!ws.state().is_terminal_watched("s-one"));
    });
}

/// Inline code keeps the words and spaces around it, in a list item and
/// in a paragraph, and the spaces next to it are drawn wide enough to see.
#[gpui_kit::test]
fn inline_code_keeps_the_spaces_around_it(cx: &mut TestAppContext) {
    use super::markdown_view::drawn::parts;
    let home = short_home();
    let s = Setup::new(home.path());
    s.write(
        "s-code",
        "Explain",
        "- Returns `(total, percent)` to `cart.py`.\n\nIt reads `a` from `b`.",
    );
    let (window, _ws) = open_session(cx, home.path(), &s, "s-code");
    let item = "Returns (total, percent) to cart.py.";
    let paragraph = "It reads a from b.";
    wait_until(cx, "the reply", |_| {
        parts(item).is_some() && parts(paragraph).is_some()
    });
    // The list item reads whole.
    assert!(shows(cx, window, item));
    let gap = |t: &str| (t.to_string(), true, false);
    let words = |t: &str| (t.to_string(), false, false);
    let code = |t: &str| (t.to_string(), true, true);
    assert_eq!(
        parts(item).unwrap(),
        vec![
            words("Returns"),
            gap(" "),
            code("(total, percent)"),
            gap(" "),
            words("to"),
            gap(" "),
            code("cart.py"),
            words("."),
        ]
    );
    assert_eq!(
        parts(paragraph).unwrap(),
        vec![
            words("It reads"),
            gap(" "),
            code("a"),
            gap(" "),
            words("from"),
            gap(" "),
            code("b"),
            words("."),
        ]
    );
}
