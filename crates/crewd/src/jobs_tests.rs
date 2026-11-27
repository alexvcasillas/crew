use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveTime, TimeZone};
use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, JobId, RunId, ThreadId};
use crew_core::schedule::Repeat;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, JobPatch, Missed, NewJob, OnAsk, Store, StoredJob,
    StoredRun, Tell,
};
use crew_proto::{Request, ServerMsg};
use tempfile::TempDir;

use crate::jobs::job_prompt;
use crate::service::{CliBins, Service};

const WAIT: Duration = Duration::from_secs(10);

/// A fake claude. It logs its arguments to `args.log` (each argument ends
/// with byte 0x1e, each call with 0x1f). A prompt with `WAIT` in it waits
/// until the file `go` exists.
fn fake_claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude-fake.sh");
    let script = format!(
        r#"#!/bin/sh
for last in "$@"; do :; done
for a in "$@"; do printf '%s\036' "$a" >> '{dir}/args.log'; done
printf '\037' >> '{dir}/args.log'
printf '{{"type":"system","subtype":"init","cwd":"/work/demo","session_id":"s","model":"m","tools":[]}}\n'
case "$last" in
  *WAIT*) while [ ! -e '{dir}/go' ]; do sleep 0.02; done ;;
esac
printf '{{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}}\n'
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The arguments of each call, in order.
fn logged_calls(dir: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(dir.join("args.log")).unwrap_or_default();
    text.split_terminator('\u{1f}')
        .map(|call| call.split_terminator('\u{1e}').map(String::from).collect())
        .collect()
}

/// The prompt: the argument after `--`.
fn prompt_of(call: &[String]) -> &str {
    let n = call.len();
    assert_eq!(call[n - 2], "--", "prompt is not last: {call:?}");
    &call[n - 1]
}

fn flag<'a>(call: &'a [String], flag: &str) -> Option<&'a str> {
    call.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

/// Today at `h:m`, local time.
fn at(h: u32, m: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 25, h, m, 0).unwrap()
}

fn every_15() -> Repeat {
    Repeat::Every {
        minutes: 15,
        days: None,
        window: None,
    }
}

struct Setup {
    _tmp: TempDir,
    dir: PathBuf,
    svc: Service,
    channel: ChannelId,
    agent: AgentId,
}

fn service(dir: &Path, store: Store) -> Service {
    let svc = Service::new(
        store,
        4,
        CliBins {
            claude: Some(dir.join("claude-fake.sh")),
            no_title_runs: true,
            ..Default::default()
        },
        dir.join("worktrees"),
        None,
        Duration::from_secs(60),
        256,
    );
    svc.clock().set(at(12, 0));
    svc
}

fn setup_with(open: impl FnOnce(&Path) -> Store) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    fake_claude(&dir);
    let root = dir.join("demo");
    std::fs::create_dir(&root).unwrap();
    let svc = service(&dir, open(&dir));
    let (channel, agent) = {
        let db = svc.db();
        let p = db.store.create_project("demo", &root, "main").unwrap();
        let c = db.store.create_channel(p, "general").unwrap();
        let a = db
            .store
            .create_agent(&AgentSpec {
                name: "Forge".into(),
                role: String::new(),
                runtime: Runtime::ClaudeCode,
                model: "sonnet".into(),
                instructions: String::new(),
                permission_mode: PermissionMode::ReadOnly,
                allowed_tools: vec![],
                handoff_policy: None,
                handoff_budget: None,
                max_run_minutes: 0,
            })
            .unwrap();
        (c, a)
    };
    Setup {
        _tmp: tmp,
        dir,
        svc,
        channel,
        agent,
    }
}

fn setup() -> Setup {
    setup_with(|_| Store::open_in_memory().unwrap())
}

impl Setup {
    fn new_job(&self, body: &str) -> NewJob {
        NewJob {
            name: "Nightly check".into(),
            body: body.into(),
            skill: None,
            model_override: None,
            agent: self.agent,
            channel: self.channel,
            repeat: every_15(),
            tell: Tell::Changes,
            on_ask: OnAsk::Wait,
            missed: Missed::OnceOnWake,
            enabled: true,
        }
    }

    async fn create(&self, job: NewJob) -> JobId {
        let id = self.svc.handle(Request::CreateJob { job }).await.unwrap();
        serde_json::from_value(id).unwrap()
    }

    fn job(&self, id: JobId) -> StoredJob {
        self.svc.db().store.job(id).unwrap()
    }

    /// The job's runs, oldest first.
    fn runs(&self, id: JobId) -> Vec<StoredRun> {
        let mut runs = self.svc.db().store.job_runs(id, 100).unwrap();
        runs.reverse();
        runs
    }

    async fn until_ended(&self, run: RunId) {
        let started = Instant::now();
        loop {
            let status = self.svc.db().store.run(run).unwrap().status;
            if !matches!(status, RunStatus::Queued | RunStatus::Working) {
                return;
            }
            assert!(started.elapsed() < WAIT, "run {run} never ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn go(&self) {
        std::fs::write(self.dir.join("go"), "").unwrap();
    }
}

fn due(at: DateTime<Local>) -> Option<i64> {
    Some(at.timestamp_millis())
}

#[tokio::test]
async fn a_due_job_starts_a_fresh_run_in_its_thread() {
    let s = setup();
    let id = s.create(s.new_job("Check the build")).await;
    let thread = s.job(id).thread;
    assert_eq!(s.job(id).next_due, due(at(12, 15)));

    // A reply of yours starts a session in the thread.
    s.svc
        .post_message(thread, "@Forge wave hello", None, None)
        .unwrap();
    let mine = s.svc.db().store.runs(Default::default()).unwrap()[0].id;
    s.until_ended(mine).await;
    // An earlier run of the job found something.
    {
        let db = s.svc.db();
        let old = db.store.create_job_run(thread, s.agent, id, 0).unwrap();
        db.store
            .set_run_report(old, true, Some("Build broke on main"))
            .unwrap();
        db.store.set_run_status(old, RunStatus::Done).unwrap();
    }
    let found = s.svc.db().store.recent_found(id, 3).unwrap();

    s.svc.fire_due(at(12, 15));
    let runs = s.runs(id);
    assert_eq!(runs.len(), 2);
    let run = &runs[1];
    assert_eq!(run.job, Some(id));
    assert_eq!(run.thread, thread);
    s.until_ended(run.id).await;

    let calls = logged_calls(&s.dir);
    assert_eq!(calls.len(), 2);
    let (first, job_call) = (&calls[0], &calls[1]);
    assert_eq!(flag(job_call, "--resume"), None, "a job run never resumes");
    let session = flag(job_call, "--session-id").expect("a new session");
    assert_ne!(Some(session), flag(first, "--session-id"));
    let prompt = prompt_of(job_call);
    assert_eq!(prompt, job_prompt(&s.job(id), &found));
    assert!(prompt.starts_with("Check the build\n\n"), "{prompt}");
    assert!(prompt.contains("Build broke on main"), "{prompt}");
    assert!(
        !prompt.contains("wave hello"),
        "no thread context: {prompt}"
    );
    assert!(prompt.ends_with("Otherwise found=false."), "{prompt}");
    // No message of yours is added for a run.
    let yours = s
        .svc
        .db()
        .store
        .messages(thread)
        .unwrap()
        .into_iter()
        .filter(|m| m.author == Author::You)
        .count();
    assert_eq!(yours, 2);
    assert_eq!(s.job(id).next_due, due(at(12, 30)));
}

#[test]
fn the_prompt_lists_earlier_reports_newest_first() {
    let job = StoredJob {
        id: JobId::new(),
        name: "n".into(),
        body: "Look for new issues.".into(),
        skill: None,
        model_override: None,
        agent: AgentId::new(),
        channel: ChannelId::new(),
        thread: ThreadId::new(),
        repeat: every_15(),
        tell: Tell::Changes,
        on_ask: OnAsk::Wait,
        missed: Missed::OnceOnWake,
        enabled: true,
        paused_reason: None,
        next_due: None,
        skipped: 0,
        created_at: 0,
    };
    let report = "When you're done, call the `report` tool once: found=true only if there is something new the user hasn't been told about above, with a one-line summary. Otherwise found=false.";
    assert_eq!(
        job_prompt(&job, &[]),
        format!("Look for new issues.\n\n{report}")
    );
    let found = [
        (at(9, 5).timestamp_millis(), "Two new issues".to_string()),
        (at(8, 0).timestamp_millis(), "One new issue".to_string()),
    ];
    assert_eq!(
        job_prompt(&job, &found),
        format!(
            "Look for new issues.\n\nEarlier reports from this job, newest first:\n- Wed 25 Nov 09:05: Two new issues\n- Wed 25 Nov 08:00: One new issue\n\n{report}"
        )
    );
}

#[tokio::test]
async fn overlap_skips_and_counts() {
    let s = setup();
    let id = s.create(s.new_job("Watch the queue WAIT")).await;
    s.svc.fire_due(at(12, 15));
    assert_eq!(s.runs(id).len(), 1);

    s.svc.fire_due(at(12, 30));
    let job = s.job(id);
    assert_eq!(s.runs(id).len(), 1, "still going, so skipped");
    assert_eq!(job.skipped, 1);
    assert_eq!(job.next_due, due(at(12, 45)));

    s.go();
    s.until_ended(s.runs(id)[0].id).await;
    s.svc.fire_due(at(12, 45));
    assert_eq!(s.runs(id).len(), 2);
    assert_eq!(s.job(id).skipped, 0, "a run that starts resets the count");
}

#[tokio::test]
async fn waiting_on_you_skips_until_answered() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].clone();
    s.until_ended(run.id).await;
    let ask = s
        .svc
        .db()
        .store
        .create_approval(
            run.thread,
            run.id,
            &ApprovalKind::Permission {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                tool_use_id: None,
            },
        )
        .unwrap();

    s.svc.fire_due(at(12, 30));
    assert_eq!(s.runs(id).len(), 1);
    assert_eq!(s.job(id).skipped, 1);
    s.svc.fire_due(at(12, 45));
    assert_eq!(s.job(id).skipped, 2);

    s.svc
        .db()
        .store
        .decide_approval(ask, ApprovalStatus::Declined, None)
        .unwrap();
    s.svc.fire_due(at(13, 0));
    assert_eq!(s.runs(id).len(), 2);
    assert_eq!(s.job(id).skipped, 0);
}

#[tokio::test]
async fn sleep_gap_runs_once_with_missed_count() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    // Asleep from before 12:15 until 13:15.
    s.svc.fire_due(at(13, 15));
    let runs = s.runs(id);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].missed, 4, "12:30, 12:45, 13:00 and 13:15");
    assert_eq!(s.job(id).next_due, due(at(13, 30)));
}

#[tokio::test]
async fn skip_policy_just_moves_on() {
    let s = setup();
    let mut new = s.new_job("Look around");
    new.missed = Missed::Skip;
    let id = s.create(new).await;
    s.svc.fire_due(at(13, 15));
    assert!(s.runs(id).is_empty());
    assert_eq!(s.job(id).next_due, due(at(13, 30)));

    s.svc.fire_due(at(13, 30));
    assert_eq!(s.runs(id).len(), 1, "a slot on time still runs");
}

#[tokio::test]
async fn run_now_doesnt_move_the_schedule() {
    let s = setup();
    let id = s.create(s.new_job("Look around WAIT")).await;
    s.svc.handle(Request::RunJobNow { job: id }).await.unwrap();
    let runs = s.runs(id);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].missed, 0);
    assert_eq!(s.job(id).next_due, due(at(12, 15)));

    let again = s.svc.handle(Request::RunJobNow { job: id }).await;
    assert_eq!(again, Err("This job is already running.".into()));
    s.go();
    s.until_ended(runs[0].id).await;
}

#[tokio::test]
async fn edit_recomputes_next_due() {
    let s = setup();
    let mut new = s.new_job("Look around");
    new.repeat = Repeat::Daily {
        at: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
    };
    let id = s.create(new).await;
    let tomorrow_9 = Local.with_ymd_and_hms(2026, 11, 26, 9, 0, 0).unwrap();
    assert_eq!(s.job(id).next_due, due(tomorrow_9));

    let mut notices = s.svc.notices();
    let patch = JobPatch {
        repeat: Some(Repeat::Daily {
            at: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
        }),
        ..Default::default()
    };
    s.svc
        .handle(Request::UpdateJob { job: id, patch })
        .await
        .unwrap();
    assert_eq!(s.job(id).next_due, due(at(18, 0)));
    let changed = std::iter::from_fn(|| notices.try_recv().ok()).any(|n| {
        matches!(&n.msg, ServerMsg::JobChanged { job } if job.id == id && job.next_due == due(at(18, 0)))
    });
    assert!(changed, "clients hear of the new time");

    s.svc
        .handle(Request::SetJobEnabled {
            job: id,
            enabled: false,
        })
        .await
        .unwrap();
    assert_eq!(s.job(id).next_due, None);
    s.svc
        .handle(Request::SetJobEnabled {
            job: id,
            enabled: true,
        })
        .await
        .unwrap();
    assert_eq!(s.job(id).next_due, due(at(18, 0)));
}

#[tokio::test]
async fn deleting_the_agent_pauses_its_jobs() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    s.svc
        .handle(Request::DeleteAgent { agent: s.agent })
        .await
        .unwrap();
    let job = s.job(id);
    assert!(!job.enabled);
    assert_eq!(job.paused_reason.as_deref(), Some("Agent deleted"));
    assert_eq!(job.next_due, None);
    s.svc.fire_due(at(13, 0));
    assert!(s.runs(id).is_empty());
}

#[tokio::test]
async fn archiving_the_thread_deletes_the_job() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    let thread = s.job(id).thread;
    let mut notices = s.svc.notices();
    s.svc
        .handle(Request::ArchiveThread {
            thread,
            archived: true,
        })
        .await
        .unwrap();
    assert!(s.svc.db().store.job(id).is_err());
    let removed = std::iter::from_fn(|| notices.try_recv().ok())
        .any(|n| matches!(n.msg, ServerMsg::JobRemoved { job } if job == id));
    assert!(removed);
}

#[tokio::test]
async fn restart_doesnt_fire_a_slot_twice() {
    let s = setup_with(|dir| Store::open(&dir.join("crew.db")).unwrap());
    let id = s.create(s.new_job("Look around")).await;
    s.svc.fire_due(at(12, 15));
    s.until_ended(s.runs(id)[0].id).await;

    let again = service(&s.dir, Store::open(&s.dir.join("crew.db")).unwrap());
    again.fire_due(at(12, 15));
    again.fire_due(at(12, 20));
    assert_eq!(s.runs(id).len(), 1);
}

#[tokio::test]
async fn editing_the_message_posts_an_edited_message() {
    let s = setup();
    let id = s.create(s.new_job("Check the build")).await;
    let thread = s.job(id).thread;
    let job_messages = || {
        s.svc
            .db()
            .store
            .messages(thread)
            .unwrap()
            .into_iter()
            .filter(|m| m.job_message && m.author == Author::You)
            .collect::<Vec<_>>()
    };
    assert_eq!(job_messages().len(), 1);

    let patch = JobPatch {
        body: Some("Check the docs".into()),
        ..Default::default()
    };
    s.svc
        .handle(Request::UpdateJob { job: id, patch })
        .await
        .unwrap();
    let messages = job_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].body, "Check the docs");

    let patch = JobPatch {
        tell: Some(Tell::EveryRun),
        ..Default::default()
    };
    s.svc
        .handle(Request::UpdateJob { job: id, patch })
        .await
        .unwrap();
    assert_eq!(job_messages().len(), 2, "settings alone post nothing");

    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].clone();
    assert_eq!(run.message, Some(messages[1].id));
    s.until_ended(run.id).await;
    let calls = logged_calls(&s.dir);
    assert!(prompt_of(&calls[0]).starts_with("Check the docs\n\n"));
}

#[tokio::test]
async fn bad_jobs_are_refused() {
    let s = setup();
    let mut odd = s.new_job("Look around");
    odd.repeat = Repeat::Every {
        minutes: 7,
        days: None,
        window: None,
    };
    let mut nameless = s.new_job("Look around");
    nameless.name = "  ".into();
    let empty = s.new_job(" ");
    let mut no_days = s.new_job("Look around");
    no_days.repeat = Repeat::Weekly {
        days: crew_core::schedule::WeekdaySet::from_days(&[]),
        at: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
    };
    let mut flat = s.new_job("Look around");
    let nine = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    flat.repeat = Repeat::Every {
        minutes: 30,
        days: None,
        window: Some((nine, nine)),
    };
    let mut nobody = s.new_job("Look around");
    nobody.agent = AgentId::new();
    for job in [odd, nameless, empty, no_days, flat, nobody] {
        let reply = s.svc.handle(Request::CreateJob { job: job.clone() }).await;
        assert!(reply.is_err(), "{job:?} was let through");
    }
    assert!(s.svc.db().store.jobs().unwrap().is_empty());
}

#[tokio::test]
async fn a_test_run_creates_the_job_off_and_runs_it() {
    let s = setup();
    let reply = s
        .svc
        .handle(Request::TestJob {
            job: s.new_job("Look around"),
        })
        .await
        .unwrap();
    let id: JobId = serde_json::from_value(reply).unwrap();
    let job = s.job(id);
    assert!(!job.enabled);
    assert_eq!(job.next_due, None);
    let runs = s.runs(id);
    assert_eq!(runs.len(), 1);
    s.until_ended(runs[0].id).await;
}
