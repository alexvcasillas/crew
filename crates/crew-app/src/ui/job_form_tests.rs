//! The new and edit job form, with a real crewd on a temp home. No
//! agent CLI is there, so a test run fails, but its job and thread are
//! made.

use chrono::{FixedOffset, NaiveTime, TimeZone as _, Weekday};
use crew_core::ids::{AgentId, ChannelId, JobId};
use crew_core::schedule::{Repeat, WeekdaySet};
use crew_core::store::{JobPatch, Missed, NewJob, OnAsk, StoredJob, Tell};
use crew_proto::Request;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::dialogs::{ROW_PAD, TITLE_GAP, TITLE_LINE, next_times};
use super::scheduled_view::JobPrefill;
use super::sidebar::row_probe::row_of;
use super::test_ui::{bounds_of, click, has, label_of, open_tall, press, type_text, wait_until};
use super::thread_tests::{World, world};
use super::workspace::Workspace;
use crate::test_support::{Daemon, short_home};

const WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
];

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

fn hourly() -> Repeat {
    Repeat::Every {
        minutes: 60,
        days: None,
        window: None,
    }
}

fn open_world(
    cx: &mut TestAppContext,
    home: &std::path::Path,
    w: &World,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let (window, ws) = open_tall(cx, home);
    let channel = w.channel;
    let agents = w.agents.len();
    wait_until(cx, "the snapshot", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().channel(channel).is_some() && ws.state().agents().len() == agents
        })
    });
    (window, ws)
}

fn open_form(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ws: &Entity<Workspace>,
    job: Option<JobId>,
    prefill: Option<JobPrefill>,
) {
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.open_job_form(job, prefill, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    assert!(has(cx, window, "job-form"), "the job form");
}

/// The job as the open form has it.
fn drafted(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Result<NewJob, String> {
    ws.read_with(cx, |ws, cx| {
        ws.job_form().expect("the job form is open").new_job(cx)
    })
}

fn ready(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, cx| ws.job_form().is_some_and(|f| f.ready(cx)))
}

fn first_agent(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> AgentId {
    ws.read_with(cx, |ws, _| ws.state().agents()[0].id)
}

fn jobs(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> Vec<StoredJob> {
    ws.read_with(cx, |ws, _| {
        ws.state().jobs().into_iter().cloned().collect::<Vec<_>>()
    })
}

fn dialog_open(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, _| ws.dialog.is_some())
}

fn busy(cx: &mut TestAppContext, ws: &Entity<Workspace>) -> bool {
    ws.read_with(cx, |ws, _| ws.dialog.as_ref().is_some_and(|d| d.busy))
}

/// Clicks the field `id` and types `text` into it.
fn fill(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str, text: &str) {
    click(cx, window, id);
    press(cx, window, "cmd-a backspace");
    if !text.is_empty() {
        type_text(cx, window, text);
    }
}

/// Opens the menu of the button `id` and picks its item `ix`.
fn pick(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str, ix: usize) {
    click(cx, window, id);
    assert!(has(cx, window, "popup-menu"), "the {id} menu");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &str) -> String {
    label_of(cx, window, id.to_string()).unwrap_or_else(|| panic!("{id} isn't shown"))
}

#[gpui_kit::test]
fn create_sends_the_job(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout", "Forge"]);
    let watchers: ChannelId = w.ask(Request::CreateChannel {
        project: w.project,
        name: "watchers".into(),
    });
    let (window, ws) = open_world(cx, home.path(), &w);
    wait_until(cx, "#watchers", |cx| has(cx, window, row_of(watchers)));
    // The channel you are in is where the job goes.
    click(cx, window, row_of(watchers));
    open_form(cx, window, &ws, None, None);

    assert_eq!(label(cx, window, "job-title"), "New scheduled job");
    assert_eq!(label(cx, window, "dialog-confirm"), "Create job");
    assert!(has(cx, window, "job-test-run"));
    let agent = first_agent(cx, &ws);
    // The defaults: every hour, any time, tell on changes, wait for me,
    // run once on wake.
    fill(cx, window, "job-name", "OVH VPS stock");
    fill(cx, window, "job-what", "Check ovhcloud.com for a VPS-2.");
    let draft = drafted(cx, &ws).unwrap();
    assert_eq!(
        draft,
        NewJob {
            name: "OVH VPS stock".into(),
            body: "Check ovhcloud.com for a VPS-2.".into(),
            skill: None,
            model_override: None,
            agent,
            channel: watchers,
            repeat: hourly(),
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::OnceOnWake,
            enabled: true,
        }
    );

    // Every 30 min, told after every run, denied asks, missed runs
    // skipped, by the other agent.
    assert_eq!(label(cx, window, "job-interval-value"), "hour");
    pick(cx, window, "job-interval", 1);
    assert_eq!(label(cx, window, "job-interval-value"), "30 min");
    let other = w.agents.iter().copied().find(|a| *a != agent).unwrap();
    let other_ix = ws.read_with(cx, |ws, _| {
        ws.state()
            .agents()
            .iter()
            .position(|a| a.id == other)
            .unwrap()
    });
    pick(cx, window, "job-agent", other_ix);
    click(cx, window, "job-tell-every-run");
    click(cx, window, "job-ask-deny");
    click(cx, window, "job-missed-skip");
    assert_eq!(
        label(cx, window, "job-tell-every-run"),
        "After every run (selected)"
    );

    click(cx, window, "dialog-confirm");
    wait_until(cx, "the job", |cx| jobs(cx, &ws).len() == 1);
    let job = jobs(cx, &ws).remove(0);
    assert_eq!(job.name, "OVH VPS stock");
    assert_eq!(job.body, "Check ovhcloud.com for a VPS-2.");
    assert_eq!(job.skill, None);
    assert_eq!(job.agent, other);
    assert_eq!(job.channel, watchers);
    assert_eq!(
        job.repeat,
        Repeat::Every {
            minutes: 30,
            days: None,
            window: None,
        }
    );
    assert_eq!(job.tell, Tell::EveryRun);
    assert_eq!(job.on_ask, OnAsk::Deny);
    assert_eq!(job.missed, Missed::Skip);
    assert!(job.enabled);
    wait_until(cx, "the dialog to close", |cx| !dialog_open(cx, &ws));
}

#[test]
fn next_times_preview() {
    let tz = FixedOffset::east_opt(3600).unwrap();
    // Wednesday 25 November 2026.
    let at = |d: u32, h: u32, m: u32| tz.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
    assert_eq!(
        next_times(&hourly(), at(25, 15, 30)),
        "today 16:00, 17:00, 18:00"
    );
    assert_eq!(
        next_times(&hourly(), at(25, 22, 10)),
        "today 23:00, tomorrow 00:00, 01:00"
    );
    let weekdays = Repeat::Weekly {
        days: WeekdaySet::from_days(&WEEKDAYS),
        at: t(9, 0),
    };
    // Friday after 9: Monday is three days on.
    assert_eq!(
        next_times(&weekdays, at(27, 10, 0)),
        "Mon 09:00, Tue 09:00, Wed 09:00"
    );
    let daily = Repeat::Daily { at: t(9, 30) };
    assert_eq!(
        next_times(&daily, at(25, 8, 0)),
        "today 09:30, tomorrow 09:30, Fri 09:30"
    );
    let windowed = Repeat::Every {
        minutes: 120,
        days: Some(WeekdaySet::from_days(&WEEKDAYS)),
        window: Some((t(9, 0), t(19, 0))),
    };
    assert_eq!(
        next_times(&windowed, at(25, 16, 0)),
        "today 18:00, tomorrow 10:00, 12:00"
    );
}

#[gpui_kit::test]
fn next_times_show_in_the_form(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    open_form(cx, window, &ws, None, None);

    let expected = |repeat: &Repeat| format!("Next: {}", next_times(repeat, chrono::Local::now()));
    let before = expected(&hourly());
    let shown = label(cx, window, "job-next");
    assert!(
        shown == before || shown == expected(&hourly()),
        "{shown:?} isn't {before:?}"
    );
    click(cx, window, "job-repeat-daily");
    fill(cx, window, "job-at", "07:15");
    let daily = Repeat::Daily { at: t(7, 15) };
    assert_eq!(label(cx, window, "job-next"), expected(&daily));
    // A time it can't read shows no preview.
    fill(cx, window, "job-at", "7 in the morning");
    assert!(!has(cx, window, "job-next"));
}

#[gpui_kit::test]
fn days_and_window_only_apply_when_on(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    open_form(cx, window, &ws, None, None);
    fill(cx, window, "job-name", "Page check");
    fill(cx, window, "job-what", "Look");
    let repeat = |cx: &mut TestAppContext| drafted(cx, &ws).unwrap().repeat;

    // Off, the days and hours show their defaults but don't apply, and
    // can't be changed.
    assert_eq!(repeat(cx), hourly());
    assert_eq!(label(cx, window, "job-day-0"), "Monday (on)");
    assert_eq!(label(cx, window, "job-day-5"), "Saturday");
    click(cx, window, "job-day-5");
    assert_eq!(label(cx, window, "job-day-5"), "Saturday");
    assert_eq!(repeat(cx), hourly());

    click(cx, window, "job-only-on");
    assert_eq!(label(cx, window, "job-only-on"), "Only on (on)");
    assert_eq!(
        repeat(cx),
        Repeat::Every {
            minutes: 60,
            days: Some(WeekdaySet::from_days(&WEEKDAYS)),
            window: Some((t(9, 0), t(19, 0))),
        }
    );
    click(cx, window, "job-day-5");
    click(cx, window, "job-day-0");
    fill(cx, window, "job-from", "08:30");
    fill(cx, window, "job-to", "20:00");
    let on = Repeat::Every {
        minutes: 60,
        days: Some(WeekdaySet::from_days(&[
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
        ])),
        window: Some((t(8, 30), t(20, 0))),
    };
    assert_eq!(repeat(cx), on);

    // Off again: neither applies.
    click(cx, window, "job-only-on");
    assert_eq!(repeat(cx), hourly());
    // On again, it keeps what you picked, and that is saved.
    click(cx, window, "job-only-on");
    assert_eq!(repeat(cx), on);

    // A time it can't read is said in the dialog.
    fill(cx, window, "job-to", "late");
    assert_eq!(
        drafted(cx, &ws).unwrap_err(),
        "Use a time like 09:00 for the hours."
    );
    click(cx, window, "dialog-confirm");
    assert_eq!(
        label(cx, window, "dialog-error"),
        "Use a time like 09:00 for the hours."
    );
    fill(cx, window, "job-to", "20:00");

    click(cx, window, "dialog-confirm");
    wait_until(cx, "the job", |cx| jobs(cx, &ws).len() == 1);
    assert_eq!(jobs(cx, &ws)[0].repeat, on);
}

#[gpui_kit::test]
fn edit_prefills_and_saves(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout", "Forge"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    let agent = w.agents[1];
    let original = NewJob {
        name: "Lint the docs".into(),
        body: "only the changed ones".into(),
        skill: Some("unslop".into()),
        model_override: Some("claude-sonnet-4-5".into()),
        agent,
        channel: w.channel,
        repeat: Repeat::Weekly {
            days: WeekdaySet::from_days(&[Weekday::Mon, Weekday::Thu]),
            at: t(8, 45),
        },
        tell: Tell::EveryRun,
        on_ask: OnAsk::Deny,
        missed: Missed::Skip,
        enabled: true,
    };
    let id: JobId = w.ask(Request::CreateJob {
        job: original.clone(),
    });
    wait_until(cx, "the job", |cx| jobs(cx, &ws).len() == 1);
    open_form(cx, window, &ws, Some(id), None);

    assert_eq!(label(cx, window, "job-title"), "Edit scheduled job");
    assert_eq!(label(cx, window, "dialog-confirm"), "Save");
    assert!(
        !has(cx, window, "job-test-run"),
        "a test run makes a new job"
    );
    // The channel can't change: it is shown, not offered.
    assert!(!has(cx, window, "job-channel"));
    assert_eq!(label(cx, window, "job-channel-fixed"), "demo / #general");
    assert_eq!(label(cx, window, "job-skill"), "/unslop");
    assert_eq!(label(cx, window, "job-repeat-weekly"), "Weekly (selected)");
    assert_eq!(label(cx, window, "job-day-3"), "Thursday (on)");
    assert_eq!(
        label(cx, window, "job-tell-every-run"),
        "After every run (selected)"
    );
    assert_eq!(drafted(cx, &ws).unwrap(), original);
    // Nothing changed, nothing to send.
    let patch =
        |cx: &mut TestAppContext| ws.read_with(cx, |ws, cx| ws.job_form().unwrap().request(cx));
    assert_eq!(
        patch(cx),
        Ok(Request::UpdateJob {
            job: id,
            patch: JobPatch::default(),
        })
    );

    fill(cx, window, "job-name", "Lint all docs");
    fill(cx, window, "job-what", "/unslop every file");
    click(cx, window, "job-day-0");
    let repeat = Repeat::Weekly {
        days: WeekdaySet::from_days(&[Weekday::Thu]),
        at: t(8, 45),
    };
    // Only what changed is sent.
    assert_eq!(
        patch(cx),
        Ok(Request::UpdateJob {
            job: id,
            patch: JobPatch {
                name: Some("Lint all docs".into()),
                body: Some("every file".into()),
                repeat: Some(repeat),
                ..JobPatch::default()
            },
        })
    );
    click(cx, window, "dialog-confirm");
    wait_until(cx, "the saved job", |cx| {
        jobs(cx, &ws)[0].name == "Lint all docs"
    });
    let job = jobs(cx, &ws).remove(0);
    assert_eq!(job.body, "every file");
    assert_eq!(job.skill.as_deref(), Some("unslop"));
    assert_eq!(job.model_override.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(job.repeat, repeat);
    assert_eq!(job.agent, agent);
    assert_eq!(job.tell, Tell::EveryRun);
    wait_until(cx, "the dialog to close", |cx| !dialog_open(cx, &ws));

    // Without the skill, it is cleared.
    open_form(cx, window, &ws, Some(id), None);
    fill(cx, window, "job-what", "Just look");
    assert_eq!(
        patch(cx),
        Ok(Request::UpdateJob {
            job: id,
            patch: JobPatch {
                body: Some("Just look".into()),
                skill: Some(None),
                ..JobPatch::default()
            },
        })
    );
}

#[gpui_kit::test]
fn test_run_creates_off_and_opens_thread(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    open_form(cx, window, &ws, None, None);
    fill(cx, window, "job-name", "Try it");
    fill(cx, window, "job-what", "Look around");

    click(cx, window, "job-test-run");
    wait_until(cx, "the job's thread to open", |cx| {
        let job = jobs(cx, &ws).first().cloned();
        job.is_some_and(|job| ws.read_with(cx, |ws, _| ws.open_thread_id() == Some(job.thread)))
    });
    let job = jobs(cx, &ws).remove(0);
    assert!(!job.enabled, "a test run's job starts off");
    assert_eq!(job.name, "Try it");
    assert!(!dialog_open(cx, &ws));
    assert!(ws.read_with(cx, |ws, _| ws.scheduled.is_none()));
}

#[gpui_kit::test]
fn empty_name_or_message_blocks_create(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    open_form(cx, window, &ws, None, None);

    assert!(!ready(cx, &ws));
    fill(cx, window, "job-what", "Look around");
    assert!(!ready(cx, &ws), "no name");
    click(cx, window, "dialog-confirm");
    assert!(!busy(cx, &ws), "Create is off");
    click(cx, window, "job-test-run");
    assert!(!busy(cx, &ws), "so is the test run");

    fill(cx, window, "job-name", "Look");
    fill(cx, window, "job-what", "   ");
    assert!(!ready(cx, &ws), "no message");
    click(cx, window, "dialog-confirm");
    assert!(!busy(cx, &ws));

    // A skill alone is enough.
    fill(cx, window, "job-what", "/unslop");
    assert!(ready(cx, &ws));
    let draft = drafted(cx, &ws).unwrap();
    assert_eq!(draft.skill.as_deref(), Some("unslop"));
    assert_eq!(draft.body, "");
    assert_eq!(label(cx, window, "job-skill"), "/unslop");
    assert!(jobs(cx, &ws).is_empty());
}

#[gpui_kit::test]
fn suggestion_prefills(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout", "Forge"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.show_scheduled(window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    let agent = first_agent(cx, &ws);
    let weekly = |days: &[Weekday]| Repeat::Weekly {
        days: WeekdaySet::from_days(days),
        at: t(9, 0),
    };
    for (id, name, repeat, tell, body) in [
        (
            "scheduled-suggest-page",
            "Page check",
            hourly(),
            Tell::Changes,
            "Check <page> and tell me only when it changes.",
        ),
        (
            "scheduled-suggest-prs",
            "PRs waiting for my review",
            weekly(&WEEKDAYS),
            Tell::Changes,
            "Check GitHub and GitLab for PRs assigned to me that I haven't reviewed yet. List the new ones.",
        ),
        (
            "scheduled-suggest-weekly",
            "Weekly work summary",
            weekly(&[Weekday::Mon]),
            Tell::EveryRun,
            "Summarize what I merged, reviewed and opened on GitHub and GitLab last week, grouped by repo.",
        ),
    ] {
        click(cx, window, id);
        assert!(has(cx, window, "job-form"), "{name}");
        assert_eq!(
            drafted(cx, &ws).unwrap(),
            NewJob {
                name: name.into(),
                body: body.into(),
                skill: None,
                model_override: None,
                agent,
                channel: w.channel,
                repeat,
                tell,
                on_ask: OnAsk::Wait,
                missed: Missed::OnceOnWake,
                enabled: true,
            },
            "{name}"
        );
        click(cx, window, "dialog-cancel");
        wait_until(cx, "the form to close", |cx| {
            !dialog_open(cx, &ws) && !has(cx, window, "job-form")
        });
    }
}

#[gpui_kit::test]
fn the_title_sits_close_to_the_first_row(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    open_form(cx, window, &ws, None, None);
    let title = bounds_of(cx, window, "job-title").unwrap();
    let name = bounds_of(cx, window, "job-name").unwrap();
    // The title's line, then 8 under it and the row's 14: 22 in all,
    // as the board has it.
    let gap = f32::from(name.origin.y - title.origin.y) - TITLE_LINE;
    assert_eq!(gap, TITLE_GAP + ROW_PAD);
}

#[gpui_kit::test]
fn edit_keeps_a_repeat_with_only_days_or_only_hours(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let w = world(home.path(), &tmp.path().join("repo"), &["Scout"]);
    let (window, ws) = open_world(cx, home.path(), &w);
    let agent = first_agent(cx, &ws);
    let weekdays = Some(WeekdaySet::from_days(&WEEKDAYS));
    let hours = Some((t(8, 0), t(18, 0)));
    for (days, window_) in [(weekdays, None), (None, hours)] {
        let id: JobId = w.ask(Request::CreateJob {
            job: NewJob {
                name: "Half".into(),
                body: "Look".into(),
                skill: None,
                model_override: None,
                agent,
                channel: w.channel,
                repeat: Repeat::Every {
                    minutes: 60,
                    days,
                    window: window_,
                },
                tell: Tell::Changes,
                on_ask: OnAsk::Wait,
                missed: Missed::OnceOnWake,
                enabled: true,
            },
        });
        wait_until(cx, "the job", |cx| jobs(cx, &ws).iter().any(|j| j.id == id));
        open_form(cx, window, &ws, Some(id), None);
        assert_eq!(label(cx, window, "job-only-on"), "Only on (on)");
        let patch =
            |cx: &mut TestAppContext| ws.read_with(cx, |ws, cx| ws.job_form().unwrap().request(cx));

        // Only the name changed: the repeat isn't sent.
        fill(cx, window, "job-name", "Renamed");
        assert_eq!(
            patch(cx),
            Ok(Request::UpdateJob {
                job: id,
                patch: JobPatch {
                    name: Some("Renamed".into()),
                    ..JobPatch::default()
                },
            })
        );

        // A new interval keeps the missing half missing.
        pick(cx, window, "job-interval", 1);
        let half = Repeat::Every {
            minutes: 30,
            days,
            window: window_,
        };
        assert_eq!(drafted(cx, &ws).unwrap().repeat, half);
        click(cx, window, "dialog-confirm");
        wait_until(cx, "the saved job", |cx| {
            jobs(cx, &ws).iter().any(|j| j.id == id && j.repeat == half)
        });
        wait_until(cx, "the dialog to close", |cx| !dialog_open(cx, &ws));

        // Turned off and on again, both apply.
        open_form(cx, window, &ws, Some(id), None);
        click(cx, window, "job-only-on");
        click(cx, window, "job-only-on");
        let both = drafted(cx, &ws).unwrap().repeat;
        let Repeat::Every {
            days: on_days,
            window: on_hours,
            ..
        } = both
        else {
            panic!("{both:?}");
        };
        assert!(on_days.is_some() && on_hours.is_some(), "{both:?}");
        click(cx, window, "dialog-cancel");
        wait_until(cx, "the dialog to close", |cx| !dialog_open(cx, &ws));
    }
}
