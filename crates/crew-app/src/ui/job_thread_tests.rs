//! A scheduled job's thread: its header, quiet runs folded, the found
//! reply's tag, the job panel, the composer's hint, the notification for
//! a found run and the clock on job threads. Runs crewd would only make
//! over hours are handed to the window's state as crewd would send them.

use chrono::{Days, Local, NaiveTime, TimeZone as _};
use crew_core::event::RunStatus;
use crew_core::ids::{JobId, MessageId, RunId, ThreadId};
use crew_core::schedule::Repeat;
use crew_core::store::{Author, Missed, NewJob, OnAsk, StoredJob, StoredMessage, StoredRun, Tell};
use crew_proto::{Request, ServerMsg};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, px};

use super::palette::DEBOUNCE;
use super::scheduled_tests::{job, job_msg, new_thread, open_world, run, run_msg, send};
use super::test_ui::{
    bounds_of, click, has, label_of, notifications, open_sized, press, shows, type_text, wait_until,
};
use super::thread_tests::{World, world};
use super::workspace::Workspace;
use crate::client::ClientEvent;
use crate::test_support::{Daemon, short_home};

/// Unix ms of `h:m` local time, `days_ago` days before today.
fn at(days_ago: u64, h: u32, m: u32) -> i64 {
    let day = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(days_ago))
        .unwrap();
    let time = day.and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap());
    Local
        .from_local_datetime(&time)
        .earliest()
        .unwrap()
        .timestamp_millis()
}

/// A run of `job` that started at `started` and took `secs`.
fn run_at(job: &StoredJob, found: Option<bool>, started: i64, secs: i64) -> StoredRun {
    let mut r = run(job, RunStatus::Done, found, 0);
    r.started_at = started;
    r.ended_at = Some(started + secs * 1000);
    r
}

fn message_at(
    thread: ThreadId,
    author: Author,
    body: &str,
    run: Option<RunId>,
    created_at: i64,
) -> StoredMessage {
    StoredMessage {
        id: MessageId::new(),
        thread,
        author,
        body: body.into(),
        created_at,
        run,
        approval: None,
        quiet: false,
        job_message: false,
    }
}

fn msg(message: StoredMessage) -> ServerMsg {
    ServerMsg::Message {
        thread: message.thread,
        message,
    }
}

/// A run of `job` and its reply, quiet unless it found something.
fn reply(job: &StoredJob, found: bool, started: i64, body: &str) -> (StoredRun, StoredMessage) {
    let r = run_at(job, Some(found), started, 48);
    let mut m = message_at(
        job.thread,
        Author::Agent(job.agent),
        body,
        Some(r.id),
        started + 48_000,
    );
    m.quiet = !found;
    (r, m)
}

fn open_thread(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    thread: ThreadId,
) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_thread(thread, window, cx));
    })
    .unwrap();
    wait_until(cx, "the thread to open", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.open_thread_id() == Some(thread) && ws.state().is_watched(thread)
        })
    });
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: impl Into<String>) -> String {
    let id = id.into();
    label_of(cx, window, id.clone()).unwrap_or_else(|| panic!("{id} isn't shown"))
}

/// A fake job on a thread of its own, its setup message set at
/// 21 Nov-like `setup`, the thread open.
struct Scene {
    w: World,
    window: AnyWindowHandle,
    ws: Entity<Workspace>,
    job: StoredJob,
    setup: StoredMessage,
}

fn scene(cx: &mut TestAppContext, home: &std::path::Path, tmp: &std::path::Path) -> Scene {
    let w = world(home, &tmp.join("repo"), &["Scout"]);
    let thread = new_thread(&w, "OVH VPS stock");
    let (window, ws) = open_world(cx, home, &w);
    let mut stock = job(
        "OVH VPS stock",
        "@Scout Check ovhcloud.com for a VPS-2.",
        w.agents[0],
        &w,
        thread,
    );
    stock.next_due = Some(at(0, 16, 0));
    let mut setup = message_at(thread, Author::You, &stock.body, None, at(6, 9, 0));
    setup.job_message = true;
    open_thread(cx, window, &ws, thread);
    take_in(cx, window, &ws, vec![job_msg(&stock), msg(setup.clone())]);
    Scene {
        w,
        window,
        ws,
        job: stock,
        setup,
    }
}

/// A real job, made by crewd with its thread and setup message.
fn real_job(cx: &mut TestAppContext, w: &World, ws: &Entity<Workspace>) -> StoredJob {
    let id: JobId = w.ask(Request::CreateJob {
        job: NewJob {
            name: "OVH VPS stock".into(),
            body: "Check ovhcloud.com".into(),
            skill: None,
            model_override: None,
            agent: w.agents[0],
            channel: w.channel,
            repeat: Repeat::Every {
                minutes: 60,
                days: None,
                window: None,
            },
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::OnceOnWake,
            enabled: true,
        },
    });
    wait_until(cx, "the job and its thread", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state()
                .job(id)
                .is_some_and(|j| ws.state().thread(j.thread).is_some())
        })
    });
    ws.read_with(cx, |ws, _| ws.state().job(id).unwrap().clone())
}

#[gpui_kit::test]
fn header_shows_repeat_and_next(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());

    assert_eq!(
        label(cx, s.window, "thread-job-tag"),
        "Every hour · next 16:00"
    );
    assert_eq!(label(cx, s.window, "thread-run-now"), "Run now");
    // The setup message says it is the job, when it was set up and how
    // it repeats.
    let id = s.setup.id;
    assert_eq!(
        label(cx, s.window, format!("message-tag-{id}")),
        "Scheduled job"
    );
    let day = Local
        .timestamp_millis_opt(s.setup.created_at)
        .unwrap()
        .format("%-d %b");
    assert_eq!(
        label(cx, s.window, format!("message-time-{id}")),
        format!("Set up {day} · every hour")
    );

    // A later job message is an edit.
    let mut edit = message_at(s.job.thread, Author::You, "Check it", None, at(0, 9, 0));
    edit.job_message = true;
    send(cx, &s.ws, vec![msg(edit.clone())]);
    assert_eq!(
        label(cx, s.window, format!("message-tag-{}", edit.id)),
        "Scheduled job · edited"
    );

    // Paused, it says so.
    let mut paused = s.job.clone();
    paused.enabled = false;
    paused.next_due = None;
    paused.paused_at = Some(at(1, 9, 0));
    send(cx, &s.ws, vec![job_msg(&paused)]);
    assert_eq!(label(cx, s.window, "thread-job-tag"), "Every hour · paused");
    let day = Local
        .timestamp_millis_opt(at(1, 9, 0))
        .unwrap()
        .format("%-d %b");
    assert_eq!(
        label(cx, s.window, "job-panel-next"),
        format!("Paused by you · {day}")
    );
    paused.paused_reason = Some("Agent deleted".into());
    send(cx, &s.ws, vec![job_msg(&paused)]);
    assert_eq!(
        label(cx, s.window, "job-panel-next"),
        "Paused until you pick another agent"
    );
    // A thread that isn't a job's has no tag.
    open_thread(cx, s.window, &s.ws, s.w.thread);
    assert!(!has(cx, s.window, "thread-job-tag"));
    assert!(!has(cx, s.window, "thread-run-now"));
}

#[gpui_kit::test]
fn quiet_runs_fold_into_one_line_with_show(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    // 18 quiet runs, yesterday 20:00 to today 13:00, then one that found
    // something.
    let mut msgs = Vec::new();
    let mut quiet = Vec::new();
    for h in 20..24 {
        quiet.push(reply(&s.job, false, at(1, h, 0), "Nothing new."));
    }
    for h in 0..14 {
        quiet.push(reply(&s.job, false, at(0, h, 0), "Nothing new."));
    }
    for (r, m) in &quiet {
        msgs.push(run_msg(r));
        msgs.push(msg(m.clone()));
    }
    let (found, found_reply) = reply(&s.job, true, at(0, 14, 0), "A VPS-2 is in stock.");
    msgs.push(run_msg(&found));
    msgs.push(msg(found_reply));
    send(cx, &s.ws, msgs);

    // The quiet replies' time is when they ended, 48 s after each start.
    let text = "18 runs with nothing new · yesterday 20:00 to today 13:00";
    assert!(shows(cx, s.window, text), "the folded line");
    let first = quiet[0].1.id;
    assert!(!has(cx, s.window, format!("message-{first}")));
    assert!(!has(cx, s.window, format!("quiet-{first}")));
    assert_eq!(
        label(cx, s.window, format!("job-fold-show-{first}")),
        "Show"
    );

    click(cx, s.window, format!("job-fold-show-{first}"));
    assert!(has(cx, s.window, format!("quiet-{first}")), "shown");
    assert_eq!(
        label(cx, s.window, format!("job-fold-show-{first}")),
        "Hide"
    );
    click(cx, s.window, format!("job-fold-show-{first}"));
    assert!(!has(cx, s.window, format!("quiet-{first}")), "folded again");
}

#[gpui_kit::test]
fn last_quiet_after_found_is_a_one_liner(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    let (found, found_reply) = reply(&s.job, true, at(0, 14, 0), "A VPS-2 is in stock.");
    let (quiet, quiet_reply) = reply(
        &s.job,
        false,
        at(0, 15, 0),
        "Nothing new. The Gravelines VPS-2 is still in stock at the same price.",
    );
    send(
        cx,
        &s.ws,
        vec![
            run_msg(&found),
            msg(found_reply.clone()),
            run_msg(&quiet),
            msg(quiet_reply.clone()),
        ],
    );
    let id = quiet_reply.id;
    assert_eq!(
        label(cx, s.window, format!("quiet-{id}")),
        "Scout · 15:00 · Nothing new. The Gravelines VPS-2 is still in stock at the same price."
    );
    assert!(
        !has(cx, s.window, format!("message-{id}")),
        "not a full message"
    );
    assert!(has(cx, s.window, format!("message-{}", found_reply.id)));
}

#[gpui_kit::test]
fn found_reply_has_the_tag(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    let (found, found_reply) = reply(&s.job, true, at(0, 14, 0), "A VPS-2 is in stock.");
    // A run that failed says so in a note, with no tag.
    let mut failed = run_at(&s.job, None, at(0, 15, 0), 3);
    failed.status = RunStatus::Failed;
    let note = message_at(
        s.job.thread,
        Author::System,
        "Scout failed: no CLI",
        Some(failed.id),
        at(0, 15, 1),
    );
    send(
        cx,
        &s.ws,
        vec![
            run_msg(&found),
            msg(found_reply.clone()),
            run_msg(&failed),
            msg(note.clone()),
        ],
    );
    assert_eq!(
        label(cx, s.window, format!("message-tag-{}", found_reply.id)),
        "Found something"
    );
    assert!(has(cx, s.window, format!("message-{}", note.id)));
    assert!(!has(cx, s.window, format!("message-tag-{}", note.id)));
}

#[gpui_kit::test]
fn panel_shows_settings_next_three_and_recent_runs(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    let mut msgs = Vec::new();
    msgs.push(run_msg(&run_at(&s.job, Some(false), at(0, 7, 0), 50)));
    let mut woke = run_at(&s.job, Some(false), at(0, 8, 14), 55);
    woke.missed = 6;
    msgs.push(run_msg(&woke));
    msgs.push(run_msg(&run_at(&s.job, Some(false), at(0, 13, 0), 51)));
    msgs.push(run_msg(&run_at(&s.job, Some(true), at(0, 14, 0), 64)));
    msgs.push(run_msg(&run_at(&s.job, Some(false), at(0, 15, 0), 48)));
    send(cx, &s.ws, msgs);

    assert!(has(cx, s.window, "job-panel"));
    assert!(!has(cx, s.window, "inspector"), "in place of the activity");
    assert_eq!(label(cx, s.window, "job-panel-title"), "Scheduled job");
    assert_eq!(label(cx, s.window, "job-panel-switch"), "Job on");
    assert_eq!(
        label(cx, s.window, "job-panel-repeats"),
        "Every hour, any time"
    );
    assert_eq!(
        label(cx, s.window, "job-panel-agent"),
        "Scout · claude · haiku"
    );
    assert_eq!(
        label(cx, s.window, "job-panel-tell"),
        "When something changes"
    );
    assert_eq!(
        label(cx, s.window, "job-panel-missed"),
        "Runs once when the Mac wakes"
    );
    assert_eq!(
        label(cx, s.window, "job-panel-next"),
        "16:00 · 17:00 · 18:00"
    );
    assert_eq!(label(cx, s.window, "job-panel-strip-title"), "Last 24 runs");
    let ticks: Vec<String> = (0..5)
        .map(|i| label(cx, s.window, format!("job-panel-tick-{i}")))
        .collect();
    assert_eq!(ticks, ["quiet", "quiet", "quiet", "found", "quiet"]);
    assert!(!has(cx, s.window, "job-panel-tick-5"));
    // Newest first.
    let runs: Vec<String> = (0..4)
        .map(|i| label(cx, s.window, format!("job-panel-run-{i}")))
        .collect();
    assert_eq!(
        runs,
        [
            "15:00 · Nothing new · 48s",
            "14:00 · Found something · 1m 4s",
            "13:00 · Nothing new · 51s",
            "08:14 · Nothing new · ran on wake, missed 6 · 55s",
        ]
    );
    // The board lists the latest four.
    assert!(!has(cx, s.window, "job-panel-run-4"));
}

#[gpui_kit::test]
fn panel_edit_pause_delete(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    let stock = real_job(cx, &w, &ws);
    open_thread(cx, window, &ws, stock.thread);
    let id = stock.id;

    click(cx, window, "job-panel-edit");
    assert!(has(cx, window, "job-form"), "the edit form");
    assert!(ws.read_with(cx, |ws, _| {
        ws.job_form().is_some_and(|f| f.editing() == Some(id))
    }));
    press(cx, window, "escape");
    wait_until(cx, "the form to close", |cx| !has(cx, window, "job-form"));

    assert_eq!(label(cx, window, "job-panel-pause"), "Pause");
    click(cx, window, "job-panel-pause");
    wait_until(cx, "the job paused", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().job(id).unwrap().enabled)
    });
    assert_eq!(label(cx, window, "job-panel-pause"), "Resume");
    assert_eq!(label(cx, window, "job-panel-switch"), "Job off");
    click(cx, window, "job-panel-switch");
    wait_until(cx, "the job on again", |cx| {
        ws.read_with(cx, |ws, _| ws.state().job(id).unwrap().enabled)
    });

    // Run now in the header starts a run of the job.
    click(cx, window, "thread-run-now");
    wait_until(cx, "the job's run", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().job_runs(id).is_empty())
    });

    click(cx, window, "job-panel-delete");
    wait_until(cx, "the job gone", |cx| {
        ws.read_with(cx, |ws, _| ws.state().job(id).is_none())
    });
    assert!(!has(cx, window, "job-panel"));
}

#[gpui_kit::test]
fn composer_hint_in_job_threads(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    assert_eq!(
        label(cx, s.window, "composer-hint"),
        "A reply runs once. To change every run, edit the job."
    );
    open_thread(cx, s.window, &s.ws, s.w.thread);
    assert_ne!(
        label_of(cx, s.window, "composer-hint").as_deref(),
        Some("A reply runs once. To change every run, edit the job.")
    );
}

/// Hands `msgs` to the workspace as the client would, so it reacts.
fn take_in(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    msgs: Vec<ServerMsg>,
) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.take_in(msgs.into_iter().map(ClientEvent::Msg).collect(), window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
}

/// A live run of `job`, then its end with its reply.
fn finish(job: &StoredJob, found: bool, body: &str) -> (Vec<ServerMsg>, Vec<ServerMsg>) {
    let mut live = run(job, RunStatus::Working, None, 60_000);
    let start = vec![run_msg(&live)];
    live.status = RunStatus::Done;
    live.ended_at = Some(live.started_at + 30_000);
    live.found = Some(found);
    let mut reply = message_at(
        job.thread,
        Author::Agent(job.agent),
        body,
        Some(live.id),
        live.started_at + 30_000,
    );
    // As crewd marks it: quiet when the job tells only of changes.
    reply.quiet = !found && job.tell == Tell::Changes;
    (start, vec![msg(reply), run_msg(&live)])
}

#[gpui_kit::test]
fn found_run_notifies_quiet_doesnt(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let thread = new_thread(&w, "OVH VPS stock");
    let (window, ws) = open_world(cx, home.path(), &w);
    let stock = job("OVH VPS stock", "Check", w.agents[0], &w, thread);
    take_in(cx, window, &ws, vec![job_msg(&stock)]);
    assert_eq!(notifications(cx, window), 0);

    let (start, end) = finish(&stock, false, "Nothing new.");
    take_in(cx, window, &ws, start);
    take_in(cx, window, &ws, end);
    assert_eq!(notifications(cx, window), 0, "a quiet run");

    let (start, end) = finish(
        &stock,
        true,
        "A VPS-2 is in stock in Gravelines at €6.49 a month.\n\nThe link.",
    );
    take_in(cx, window, &ws, start);
    take_in(cx, window, &ws, end.clone());
    assert_eq!(notifications(cx, window), 1, "a found run");
    assert_eq!(label(cx, window, "job-notice-title"), "OVH VPS stock");
    assert_eq!(
        label(cx, window, "job-notice-text"),
        "Scout: A VPS-2 is in stock in Gravelines at €6.49 a month."
    );
    // crewd sending it again doesn't notify again.
    take_in(cx, window, &ws, end);
    assert_eq!(notifications(cx, window), 1);

    // The toast slides in before it can be clicked.
    wait_until(cx, "the toast in place", |cx| {
        bounds_of(cx, window, "job-notice-open").is_some_and(|b| b.origin.y > px(0.))
    });
    click(cx, window, "job-notice-open");
    wait_until(cx, "the job's thread", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(thread))
    });

    // Open closes the toast; it fades out on the test clock.
    wait_until(cx, "the toast gone", |cx| {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        !has(cx, window, "job-notice-title")
    });

    // While you look at the thread, it doesn't.
    let (start, end) = finish(&stock, true, "Another one.");
    take_in(cx, window, &ws, start);
    take_in(cx, window, &ws, end);
    assert!(!has(cx, window, "job-notice-title"));
}

#[gpui_kit::test]
fn job_threads_show_the_clock_icon(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    let stock = real_job(cx, &w, &ws);
    let job_thread = stock.thread;

    // Cards in the channel.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.select_channel(w.channel, window, cx));
    })
    .unwrap();
    wait_until(cx, "the job's card", |cx| {
        has(cx, window, format!("thread-{job_thread}"))
    });
    assert!(has(cx, window, format!("thread-clock-{job_thread}")));
    assert!(!has(cx, window, format!("thread-clock-{}", w.thread)));

    // ⌘K.
    press(cx, window, "cmd-k");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
    type_text(cx, window, "OVH");
    cx.executor()
        .advance_clock(DEBOUNCE + std::time::Duration::from_millis(30));
    // Threads are found by crewd, after the commands.
    let mut row = None;
    wait_until(cx, "the job's thread row", |cx| {
        row = (0..8_usize).find(|i| {
            label_of(cx, window, ("palette-row", *i)).as_deref() == Some("OVH VPS stock")
        });
        row.is_some()
    });
    assert!(has(cx, window, ("palette-clock", row.unwrap())));
    press(cx, window, "escape");

    // The threads sidebar.
    click(cx, window, "sidebar-switch");
    wait_until(cx, "the job's row", |cx| {
        has(cx, window, format!("thread-row-{job_thread}"))
    });
    assert!(has(cx, window, format!("thread-row-clock-{job_thread}")));
    assert!(!has(cx, window, format!("thread-row-clock-{}", w.thread)));
}

#[gpui_kit::test]
fn archiving_a_job_thread_warns(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    let stock = real_job(cx, &w, &ws);
    let open_archive = |cx: &mut TestAppContext, thread: ThreadId| {
        cx.update_window(window, |_, window, cx| {
            ws.update(cx, |ws, cx| ws.open_archive_dialog(thread, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
    };
    open_archive(cx, stock.thread);
    assert_eq!(
        label(cx, window, "archive-job-note"),
        "Its scheduled job, OVH VPS stock, will be deleted too."
    );
    press(cx, window, "escape");
    wait_until(cx, "the dialog to close", |cx| {
        !has(cx, window, "archive-heading")
    });
    open_archive(cx, w.thread);
    assert!(has(cx, window, "archive-heading"));
    assert!(!has(cx, window, "archive-job-note"));
}

#[gpui_kit::test]
fn panel_shows_even_after_activity_was_hidden(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    // You hid the activity in another thread.
    open_thread(cx, s.window, &s.ws, s.w.thread);
    click(cx, s.window, "toggle-activity");
    assert!(!has(cx, s.window, "inspector"));
    open_thread(cx, s.window, &s.ws, s.job.thread);
    assert!(has(cx, s.window, "job-panel"), "wide, the panel shows");
    assert!(!has(cx, s.window, "toggle-activity"), "as on the board");
}

#[gpui_kit::test]
fn narrow_job_threads_toggle_the_panel(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_sized(cx, home.path(), 1000.);
    let stock = real_job(cx, &w, &ws);
    open_thread(cx, window, &ws, stock.thread);
    assert!(!has(cx, window, "job-panel"));
    click(cx, window, "toggle-activity");
    assert!(has(cx, window, "job-panel"));
    click(cx, window, "toggle-activity");
    assert!(!has(cx, window, "job-panel"));
}

#[gpui_kit::test]
fn a_recent_run_opens_its_activity_and_back_returns(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    let (found, found_reply) = reply(&s.job, true, at(0, 14, 0), "A VPS-2 is in stock.");
    send(cx, &s.ws, vec![run_msg(&found), msg(found_reply)]);

    click(cx, s.window, "job-panel-run-0");
    assert!(!has(cx, s.window, "job-panel"));
    assert!(has(cx, s.window, "inspector"), "the run's activity");
    assert_eq!(
        s.ws.read_with(cx, |ws, _| ws.shown_run(s.job.thread).map(|r| r.id)),
        Some(found.id)
    );
    assert_eq!(
        label(cx, s.window, "inspector-back-to-job"),
        "Scheduled job"
    );
    click(cx, s.window, "inspector-back-to-job");
    assert!(has(cx, s.window, "job-panel"));
    assert!(!has(cx, s.window, "inspector-back-to-job"));
}

#[gpui_kit::test]
fn every_run_jobs_notify_and_read_done(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let s = scene(cx, home.path(), tmp.path());
    let mut every = s.job.clone();
    every.tell = Tell::EveryRun;
    take_in(cx, s.window, &s.ws, vec![job_msg(&every)]);
    // Looking elsewhere, a reply that found nothing still tells you.
    open_thread(cx, s.window, &s.ws, s.w.thread);
    let (start, end) = finish(&every, false, "Here is the weekly summary.");
    take_in(cx, s.window, &s.ws, start);
    take_in(cx, s.window, &s.ws, end);
    assert_eq!(notifications(cx, s.window), 1);
    assert_eq!(
        label(cx, s.window, "job-notice-text"),
        "Scout: Here is the weekly summary."
    );
    // Its run reads Done, not Found something.
    open_thread(cx, s.window, &s.ws, every.thread);
    let run = label(cx, s.window, "job-panel-run-0");
    assert!(run.contains(" · Done · "), "{run}");
}
