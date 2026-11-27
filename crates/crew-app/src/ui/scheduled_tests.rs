//! The Scheduled page, its sidebar row and its ⌘K commands, with a real
//! crewd on a temp home. Jobs and their runs that crewd would only make
//! over hours are handed to the window's state as crewd would send them.

use chrono::{NaiveTime, Weekday};
use crew_core::agent::Runtime;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ApprovalId, ChannelId, JobId, MessageId, RunId, ThreadId};
use crew_core::schedule::{Repeat, WeekdaySet};
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, Missed, NewJob, OnAsk, StoredApproval, StoredJob,
    StoredMessage, StoredRun, Tell,
};
use crew_proto::{Request, ServerMsg};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::common::{clock, now_ms};
use super::scheduled_view::{JobPrefill, next_lines, run_time, summary_line};
use super::sidebar::row_probe::status_of;
use super::test_ui::{bounds_of, click, has, label_of, open, press, shows, type_text, wait_until};
use super::thread_tests::{World, go_to, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, short_home};

const MIN: i64 = 60_000;

fn every_30_weekdays() -> Repeat {
    Repeat::Every {
        minutes: 30,
        days: Some(WeekdaySet::from_days(&[
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
        ])),
        window: Some((t(9, 0), t(19, 0))),
    }
}

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

/// A job as crewd would send it, in `thread`.
fn job(name: &str, body: &str, agent: AgentId, w: &World, thread: ThreadId) -> StoredJob {
    StoredJob {
        id: JobId::new(),
        name: name.into(),
        body: body.into(),
        skill: None,
        model_override: None,
        agent,
        channel: w.channel,
        thread,
        repeat: Repeat::Every {
            minutes: 60,
            days: None,
            window: None,
        },
        tell: Tell::Changes,
        on_ask: OnAsk::Wait,
        missed: Missed::OnceOnWake,
        enabled: true,
        paused_reason: None,
        next_due: Some(now_ms() + 30 * MIN),
        skipped: 0,
        created_at: now_ms(),
    }
}

/// A run of `job` that started `ago` ms ago, ended unless live.
fn run(job: &StoredJob, status: RunStatus, found: Option<bool>, ago: i64) -> StoredRun {
    let started_at = now_ms() - ago;
    let live = matches!(status, RunStatus::Working | RunStatus::Queued);
    StoredRun {
        id: RunId::new(),
        thread: job.thread,
        agent: job.agent,
        session_id: String::new(),
        status,
        started_at,
        ended_at: (!live).then_some(started_at + 1000),
        runtime: Runtime::ClaudeCode,
        waiting_for: None,
        skill: None,
        model_override: None,
        message: None,
        job: Some(job.id),
        found,
        summary: None,
        missed: 0,
    }
}

/// Hands `msgs` to the window's state, as if crewd had sent them.
fn send(cx: &mut TestAppContext, ws: &Entity<Workspace>, msgs: Vec<ServerMsg>) {
    ws.update(cx, |ws, cx| {
        for msg in msgs {
            ws.state_mut().apply(msg);
        }
        cx.notify();
    });
    cx.run_until_parked();
}

fn job_msg(job: &StoredJob) -> ServerMsg {
    ServerMsg::JobChanged {
        job: Box::new(job.clone()),
    }
}

fn run_msg(run: &StoredRun) -> ServerMsg {
    ServerMsg::RunStatus {
        thread: run.thread,
        run: run.id,
        status: run.status.clone(),
        details: Box::new(run.clone()),
    }
}

fn message(
    thread: ThreadId,
    author: Author,
    body: &str,
    run: Option<RunId>,
    quiet: bool,
) -> ServerMsg {
    ServerMsg::Message {
        thread,
        message: StoredMessage {
            id: MessageId::new(),
            thread,
            author,
            body: body.into(),
            created_at: now_ms(),
            run,
            approval: None,
            quiet,
            job_message: false,
        },
    }
}

fn ask(run: &StoredRun, command: &str) -> ServerMsg {
    ServerMsg::Approval {
        thread: run.thread,
        approval: StoredApproval {
            id: ApprovalId::new(),
            thread: run.thread,
            run: run.id,
            kind: ApprovalKind::Permission {
                tool: "Bash".into(),
                input: serde_json::json!({ "command": command }),
                tool_use_id: None,
            },
            status: ApprovalStatus::Pending,
            created_at: now_ms(),
            decided_at: None,
        },
    }
}

fn new_thread(w: &World, title: &str) -> ThreadId {
    w.ask(Request::CreateThread {
        channel: w.channel,
        title: title.into(),
        first_line: false,
    })
}

/// Opens the window and waits for the world's channel in the sidebar.
fn open_world(
    cx: &mut TestAppContext,
    home: &std::path::Path,
    w: &World,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, ws) = open(cx, home);
    let channel = w.channel;
    wait_until(cx, "the snapshot", |cx| {
        ws.read_with(cx, |ws, _| ws.state().channel(channel).is_some())
    });
    (window, ws)
}

fn show_scheduled(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.show_scheduled(window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) -> String {
    label_of(cx, window, id.clone()).unwrap_or_else(|| panic!("{id} isn't shown"))
}

fn hover(cx: &mut TestAppContext, window: AnyWindowHandle, id: String) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.hover(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn sidebar_row_counts_found_and_waiting(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(
        home.path(),
        &tmp.path().join("repo"),
        &["Scout", "Sentinel"],
    );
    let other = new_thread(&w, "OVH VPS stock");
    let (window, ws) = open_world(cx, home.path(), &w);

    assert_eq!(label(cx, window, "sidebar-scheduled".into()), "Scheduled");
    assert_eq!(
        label(cx, window, "sidebar-scheduled-count".into()),
        "Nothing new"
    );

    // A run that found something counts green until you open its thread.
    let stock = job(
        "OVH VPS stock",
        "Check ovhcloud.com",
        w.agents[0],
        &w,
        other,
    );
    let found = run(&stock, RunStatus::Done, Some(true), MIN);
    send(cx, &ws, vec![job_msg(&stock), run_msg(&found)]);
    assert_eq!(
        label(cx, window, "sidebar-scheduled-count".into()),
        "1 job found something"
    );

    // A job waiting on you shows amber, over green.
    let prs = job(
        "PRs waiting for my review",
        "Check GitHub",
        w.agents[1],
        &w,
        w.thread,
    );
    let waiting = run(&prs, RunStatus::Working, None, MIN);
    send(
        cx,
        &ws,
        vec![
            job_msg(&prs),
            run_msg(&waiting),
            ask(&waiting, "glab mr list"),
        ],
    );
    assert_eq!(
        label(cx, window, "sidebar-scheduled-count".into()),
        "1 job waiting on you"
    );

    // Clicking the row opens the page; opening the job's thread reads it.
    click(cx, window, "sidebar-scheduled");
    assert!(has(cx, window, "scheduled"));
    assert!(has(cx, window, "sidebar"), "the sidebar stays");
    go_to(cx, window, &ws, w.channel, other);
    assert!(!has(cx, window, "scheduled"), "a thread leaves the page");
    assert_eq!(
        label(cx, window, "sidebar-scheduled-count".into()),
        "1 job waiting on you"
    );
    ws.read_with(cx, |ws, _| {
        let seen = |t| ws.thread_seen(t);
        assert_eq!(ws.state().scheduled_counts(seen), (0, 1));
    });
}

#[gpui_kit::test]
fn page_lists_jobs_with_repeats_last_and_next(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(
        home.path(),
        &tmp.path().join("repo"),
        &["Scout", "Sentinel", "Forge", "Quill"],
    );
    let (window, ws) = open_world(cx, home.path(), &w);
    let now = now_ms();

    let mut prs = job(
        "PRs waiting for my review",
        "Check GitHub and GitLab for PRs assigned to me that I haven't reviewed yet.\nList the new ones.",
        w.agents[1],
        &w,
        w.thread,
    );
    prs.repeat = every_30_weekdays();
    prs.next_due = Some(now + 20 * MIN);
    let mut prs_quiet = run(&prs, RunStatus::Done, Some(false), 40 * MIN);
    prs_quiet.summary = Some("Nothing new".into());
    let mut prs_found = run(&prs, RunStatus::Done, Some(true), 10 * MIN);
    prs_found.summary = Some("2 new PRs".into());

    let mut stock = job(
        "OVH VPS stock",
        "Check ovhcloud.com",
        w.agents[0],
        &w,
        w.thread,
    );
    stock.next_due = Some(now + 50 * MIN);
    stock.created_at += 1;
    let stock_quiet = run(&stock, RunStatus::Done, Some(false), 5 * MIN);

    let mut audit = job(
        "Dependency audit",
        "Run cargo audit",
        w.agents[2],
        &w,
        w.thread,
    );
    audit.repeat = Repeat::Daily { at: t(14, 45) };
    audit.created_at += 2;

    let mut weekly = job(
        "Weekly work summary",
        "Summarize my week",
        w.agents[3],
        &w,
        w.thread,
    );
    weekly.repeat = Repeat::Weekly {
        days: WeekdaySet::from_days(&[Weekday::Mon]),
        at: t(9, 0),
    };
    weekly.tell = Tell::EveryRun;
    weekly.created_at += 3;
    let weekly_done = run(&weekly, RunStatus::Done, None, 3 * MIN);

    send(
        cx,
        &ws,
        vec![
            job_msg(&prs),
            job_msg(&stock),
            job_msg(&audit),
            job_msg(&weekly),
            run_msg(&prs_quiet),
            run_msg(&prs_found),
            run_msg(&stock_quiet),
            run_msg(&weekly_done),
        ],
    );
    show_scheduled(cx, window, &ws);

    assert_eq!(label(cx, window, "scheduled-title".into()), "Scheduled");
    assert_eq!(
        label(cx, window, "scheduled-summary".into()),
        summary_line(4, prs.next_due, now)
    );
    assert!(summary_line(4, prs.next_due, now).starts_with("4 jobs on · next run "));
    assert_eq!(label(cx, window, "title-scheduled".into()), "Scheduled");
    for head in ["Job", "Agent", "Repeats", "Last runs", "Next"] {
        assert!(shows(cx, window, head), "{head}");
    }
    assert!(has(cx, window, "scheduled-new"), "New job");

    let p = prs.id;
    assert_eq!(
        label(cx, window, format!("job-name-{p}")),
        "PRs waiting for my review"
    );
    assert!(has(cx, window, format!("job-new-{p}")), "found and unread");
    assert_eq!(
        label(cx, window, format!("job-desc-{p}")),
        "Check GitHub and GitLab for PRs assigned to me that I haven't reviewed yet."
    );
    assert_eq!(
        label(cx, window, format!("job-agent-{p}")),
        "Sentinel · claude"
    );
    assert_eq!(
        label(cx, window, format!("job-repeats-{p}")),
        "Every 30 min · Weekdays, 9:00 to 19:00"
    );
    assert_eq!(label(cx, window, format!("job-strip-{p}")), "quiet found");
    assert_eq!(
        label(cx, window, format!("job-last-{p}")),
        format!("2 new PRs · {}", run_time(prs_found.ended_at.unwrap(), now))
    );
    let next = match next_lines(now + 20 * MIN, now) {
        (day, time) if time.is_empty() => day,
        (day, time) => format!("{day} {time}"),
    };
    assert_eq!(label(cx, window, format!("job-next-{p}")), next);
    assert_eq!(label(cx, window, format!("job-toggle-{p}")), "On");

    let s = stock.id;
    assert!(!has(cx, window, format!("job-new-{s}")));
    assert_eq!(
        label(cx, window, format!("job-repeats-{s}")),
        "Every hour · Any time"
    );
    assert_eq!(
        label(cx, window, format!("job-last-{s}")),
        format!("Nothing new · {}", clock(stock_quiet.ended_at.unwrap()))
    );
    assert_eq!(
        label(cx, window, format!("job-repeats-{}", audit.id)),
        "Every day at 14:45 · demo"
    );
    assert_eq!(
        label(cx, window, format!("job-repeats-{}", weekly.id)),
        "Mondays at 9:00 · Always tells you"
    );
    assert_eq!(
        label(cx, window, format!("job-strip-{}", weekly.id)),
        "found"
    );

    // Rows are in the order the jobs were made.
    let top = |cx: &mut TestAppContext, id: JobId| {
        bounds_of(cx, window, format!("job-{id}")).unwrap().origin.y
    };
    assert!(top(cx, p) < top(cx, s));
    assert!(top(cx, s) < top(cx, audit.id));

    for legend in ["Nothing new", "Found something", "Waiting on you", "Failed"] {
        assert!(shows(cx, window, legend), "{legend}");
    }
    assert!(
        label(cx, window, "scheduled-note".into()).starts_with("Jobs run while your Mac is awake.")
    );

    // A click on the row opens the job's thread.
    click(cx, window, format!("job-{s}"));
    wait_until(cx, "the job's thread", |cx| {
        ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(w.thread))
    });
    assert!(!has(cx, window, "scheduled"));

    // ⌘K finds the page and a new job.
    press(cx, window, "cmd-k");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
    type_text(cx, window, "sched");
    wait_until(cx, "the commands", |cx| {
        shows(cx, window, "New scheduled job…")
    });
    let rows: Vec<(String, String)> = (0..20usize)
        .map_while(|i| {
            let label = label_of(cx, window, ("palette-row", i))?;
            let sub =
                super::test_ui::description_of(cx, window, ("palette-row", i)).unwrap_or_default();
            Some((label, sub))
        })
        .collect();
    assert!(
        rows.contains(&("Scheduled".into(), "4 on".into())),
        "{rows:?}"
    );
    assert!(rows.contains(&("New scheduled job…".into(), String::new())));
}

#[gpui_kit::test]
fn paused_jobs_have_their_own_section(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Quill"]);
    let (window, ws) = open_world(cx, home.path(), &w);

    let mut notes = job(
        "Release notes draft",
        "Draft release notes",
        w.agents[0],
        &w,
        w.thread,
    );
    notes.enabled = false;
    notes.next_due = None;
    notes.repeat = Repeat::Weekly {
        days: WeekdaySet::from_days(&[Weekday::Fri]),
        at: t(17, 0),
    };
    let mut on = job(
        "OVH VPS stock",
        "Check ovhcloud.com",
        w.agents[0],
        &w,
        w.thread,
    );
    on.created_at += 1;
    send(cx, &ws, vec![job_msg(&notes), job_msg(&on)]);
    show_scheduled(cx, window, &ws);

    assert_eq!(label(cx, window, "scheduled-paused".into()), "Paused · 1");
    let y = |cx: &mut TestAppContext, id: String| bounds_of(cx, window, id).unwrap().origin.y;
    let header = y(cx, "scheduled-paused".into());
    assert!(y(cx, format!("job-{}", on.id)) < header, "on jobs first");
    assert!(
        header < y(cx, format!("job-{}", notes.id)),
        "paused under it"
    );
    let n = notes.id;
    assert_eq!(label(cx, window, format!("job-last-{n}")), "Paused by you");
    assert_eq!(label(cx, window, format!("job-next-{n}")), "—");
    assert_eq!(label(cx, window, format!("job-toggle-{n}")), "Off");
    assert_eq!(
        label(cx, window, "scheduled-summary".into()),
        summary_line(1, on.next_due, now_ms())
    );
}

#[gpui_kit::test]
fn hover_shows_run_now_and_menu(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let id: JobId = w.ask(Request::CreateJob {
        job: NewJob {
            name: "OVH VPS stock".into(),
            body: "Check ovhcloud.com".into(),
            skill: None,
            model_override: None,
            agent: w.agents[0],
            channel: w.channel,
            repeat: Repeat::Weekly {
                days: WeekdaySet::from_days(&[Weekday::Sun]),
                at: t(3, 0),
            },
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::Skip,
            enabled: true,
        },
    });
    let (window, ws) = open_world(cx, home.path(), &w);
    wait_until(cx, "the job", |cx| {
        ws.read_with(cx, |ws, _| ws.state().job(id).is_some())
    });
    show_scheduled(cx, window, &ws);

    assert!(has(cx, window, format!("job-next-{id}")));
    assert!(!has(cx, window, format!("job-run-now-{id}")));
    hover(cx, window, format!("job-{id}"));
    assert!(has(cx, window, format!("job-run-now-{id}")));
    assert!(has(cx, window, format!("job-more-{id}")));
    assert!(
        !has(cx, window, format!("job-next-{id}")),
        "in place of Next"
    );

    click(cx, window, format!("job-more-{id}"));
    assert!(has(cx, window, "popup-menu"), "the overflow menu");
    for item in ["Edit", "Duplicate", "Pause", "Delete"] {
        assert!(shows(cx, window, item), "{item}");
    }
    assert!(
        has(cx, window, "scheduled"),
        "the menu doesn't open the thread"
    );
    press(cx, window, "escape");

    hover(cx, window, format!("job-{id}"));
    click(cx, window, format!("job-run-now-{id}"));
    wait_until(cx, "a run of the job", |cx| {
        ws.read_with(cx, |ws, _| !ws.state().job_runs(id).is_empty())
    });
    assert!(has(cx, window, "scheduled"), "Run now stays on the page");
}

#[gpui_kit::test]
fn toggle_sends_set_job_enabled(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let id: JobId = w.ask(Request::CreateJob {
        job: NewJob {
            name: "OVH VPS stock".into(),
            body: "Check ovhcloud.com".into(),
            skill: None,
            model_override: None,
            agent: w.agents[0],
            channel: w.channel,
            repeat: Repeat::Weekly {
                days: WeekdaySet::from_days(&[Weekday::Sun]),
                at: t(3, 0),
            },
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::Skip,
            enabled: true,
        },
    });
    let (window, ws) = open_world(cx, home.path(), &w);
    wait_until(cx, "the job", |cx| {
        ws.read_with(cx, |ws, _| ws.state().job(id).is_some())
    });
    show_scheduled(cx, window, &ws);
    let enabled =
        |cx: &mut TestAppContext| ws.read_with(cx, |ws, _| ws.state().job(id).map(|j| j.enabled));

    click(cx, window, format!("job-toggle-{id}"));
    wait_until(cx, "the job off", |cx| enabled(cx) == Some(false));
    assert_eq!(label(cx, window, "scheduled-paused".into()), "Paused · 1");
    assert_eq!(label(cx, window, format!("job-toggle-{id}")), "Off");
    assert!(
        has(cx, window, "scheduled"),
        "the toggle doesn't open the thread"
    );

    click(cx, window, format!("job-toggle-{id}"));
    wait_until(cx, "the job on", |cx| enabled(cx) == Some(true));
    assert!(!has(cx, window, "scheduled-paused"));
}

#[gpui_kit::test]
fn row_states(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(
        home.path(),
        &tmp.path().join("repo"),
        &["Forge", "Sentinel", "Quill", "Scout"],
    );
    let (window, ws) = open_world(cx, home.path(), &w);
    let threads: Vec<ThreadId> = (0..5)
        .map(|i| new_thread(&w, &format!("job {i}")))
        .collect();

    let audit = job(
        "Dependency audit",
        "Run cargo audit",
        w.agents[0],
        &w,
        threads[0],
    );
    let running = run(&audit, RunStatus::Working, None, 72_000);

    let mut prs = job(
        "PRs waiting for my review",
        "Check GitHub",
        w.agents[1],
        &w,
        threads[1],
    );
    prs.skipped = 2;
    let waiting = run(&prs, RunStatus::Working, None, MIN);

    let weekly = job(
        "Weekly work summary",
        "Summarize",
        w.agents[2],
        &w,
        threads[2],
    );
    let failed = run(&weekly, RunStatus::Failed, None, 2 * MIN);
    let reason = "Codex isn't signed in. Run `codex login`.";

    let stock = job(
        "OVH VPS stock",
        "Check ovhcloud.com",
        w.agents[3],
        &w,
        threads[3],
    );
    let mut woke = run(&stock, RunStatus::Done, Some(false), 3 * MIN);
    woke.missed = 6;

    let mut notes = job("Release notes draft", "Draft", w.agents[2], &w, threads[4]);
    notes.enabled = false;
    notes.next_due = None;
    notes.paused_reason = Some("Agent deleted".into());

    send(
        cx,
        &ws,
        vec![
            job_msg(&audit),
            job_msg(&prs),
            job_msg(&weekly),
            job_msg(&stock),
            job_msg(&notes),
            run_msg(&running),
            run_msg(&waiting),
            ask(&waiting, "glab mr list"),
            run_msg(&failed),
            message(threads[2], Author::System, reason, Some(failed.id), false),
            run_msg(&woke),
        ],
    );
    show_scheduled(cx, window, &ws);

    let a = audit.id;
    let state = label(cx, window, format!("job-state-{a}"));
    assert!(state.starts_with("Running · 1m 1"), "{state}");
    assert_eq!(
        label(cx, window, format!("job-detail-{a}")),
        format!("Started {}", clock(running.started_at))
    );

    let p = prs.id;
    assert_eq!(
        label(cx, window, format!("job-state-{p}")),
        "Waiting on you"
    );
    assert_eq!(
        label(cx, window, format!("job-detail-{p}")),
        "Wants to run glab mr list · 2 runs skipped"
    );

    let f = weekly.id;
    assert_eq!(
        label(cx, window, format!("job-state-{f}")),
        format!("Failed · {}", clock(failed.ended_at.unwrap()))
    );
    assert_eq!(
        label(cx, window, format!("job-detail-{f}")),
        "Codex isn't signed in. Run codex login."
    );

    let s = stock.id;
    assert_eq!(
        label(cx, window, format!("job-state-{s}")),
        format!("Nothing new · {}", clock(woke.ended_at.unwrap()))
    );
    assert_eq!(
        label(cx, window, format!("job-detail-{s}")),
        "Missed 6 runs while the Mac slept, ran once on wake"
    );

    let n = notes.id;
    assert_eq!(label(cx, window, format!("job-state-{n}")), "Agent deleted");
    assert_eq!(
        label(cx, window, format!("job-detail-{n}")),
        "Paused until you pick another agent"
    );

    // Hovered, each state offers what it needs.
    hover(cx, window, format!("job-{a}"));
    assert!(has(cx, window, format!("job-stop-{a}")));
    hover(cx, window, format!("job-{p}"));
    assert!(has(cx, window, format!("job-open-{p}")));
    hover(cx, window, format!("job-{n}"));
    assert!(has(cx, window, format!("job-edit-{n}")));
    hover(cx, window, format!("job-{f}"));
    assert!(has(cx, window, format!("job-run-now-{f}")));
}

#[gpui_kit::test]
fn empty_page_suggestions(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    show_scheduled(cx, window, &ws);

    assert!(shows(cx, window, "No scheduled jobs yet"));
    assert!(shows(
        cx,
        window,
        "An agent can check something on a schedule and tell you when it changes."
    ));
    let asked = |cx: &mut TestAppContext| ws.read_with(cx, |ws, _| ws.job_form_asked);
    for (id, text, prefill) in [
        (
            "scheduled-suggest-page",
            "Check a page every hour",
            JobPrefill::PageCheck,
        ),
        (
            "scheduled-suggest-prs",
            "Review my PRs each morning",
            JobPrefill::PrMorning,
        ),
        (
            "scheduled-suggest-weekly",
            "Weekly summary of my work",
            JobPrefill::WeeklySummary,
        ),
    ] {
        assert_eq!(label(cx, window, id.into()), text);
        click(cx, window, id);
        assert_eq!(asked(cx), Some((None, Some(prefill))), "{text}");
    }
    click(cx, window, "scheduled-empty-new");
    assert_eq!(asked(cx), Some((None, None)));
    assert!(!has(cx, window, "scheduled-paused"));
}

#[gpui_kit::test]
fn quiet_replies_dont_count_unread(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let watchers: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "watchers".into(),
    });
    let thread: ThreadId = w.ask(Request::CreateThread {
        channel: watchers,
        title: "OVH VPS stock".into(),
        first_line: false,
    });
    let (window, ws) = open_world(cx, home.path(), &w);
    wait_until(cx, "#watchers", |cx| has(cx, window, status_of(watchers)));
    let scout = Author::Agent(w.agents[0]);

    send(
        cx,
        &ws,
        vec![message(thread, scout.clone(), "Nothing new.", None, true)],
    );
    assert_eq!(label(cx, window, status_of(watchers)), "");
    send(
        cx,
        &ws,
        vec![message(thread, scout, "A VPS-2 is in stock.", None, false)],
    );
    assert_eq!(label(cx, window, status_of(watchers)), "1 unread");
}
