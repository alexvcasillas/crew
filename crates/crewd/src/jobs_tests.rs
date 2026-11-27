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
    StoredMessage, StoredRun, Tell,
};
use crew_proto::{PermissionAnswer, Request, ServerMsg};
use tempfile::TempDir;

use crate::jobs::job_prompt;
use crate::service::{CliBins, Service};

const WAIT: Duration = Duration::from_secs(10);

/// A fake claude. It logs its arguments to `args.log` (each argument ends
/// with byte 0x1e, each call with 0x1f). A prompt with `WAIT` in it waits
/// until the file `go` exists. One with `FAIL` in it exits with an error
/// before it answers.
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
case "$last" in
  *FAIL*) exit 1 ;;
esac
printf '{{"type":"result","subtype":"success","is_error":false,"result":"ok","duration_ms":1,"num_turns":1,"permission_denials":[],"session_id":"s"}}\n'
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A `codex` that is signed in and runs `scenario-<n>.json` for its n-th
/// app-server, logging what it reads to `messages-<n>.log`.
fn fake_codex(dir: &Path) {
    let path = dir.join("codex");
    let fake = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crew-core/tests/fake/codex_app_server.py");
    let d = dir.display();
    let body = format!(
        "#!/bin/sh\n\
         case \"$1\" in\n\
         --version) echo 'codex-cli 0.165.0' ;;\n\
         login) exit 0 ;;\n\
         app-server)\n\
           n=$(cat '{d}/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{d}/count'\n\
           exec python3 '{fake}' '{d}/scenario-'$n'.json' ;;\n\
         *) exit 2 ;;\n\
         esac\n",
        fake = fake.display(),
    );
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Writes the n-th app-server's scenario.
fn scenario(dir: &Path, n: u32, steps: Vec<serde_json::Value>) {
    let log = dir.join(format!("messages-{n}.log"));
    let body = serde_json::json!({"log": log, "steps": steps});
    std::fs::write(dir.join(format!("scenario-{n}.json")), body.to_string()).unwrap();
}

/// What the n-th app-server read.
fn codex_messages(dir: &Path, n: u32) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join(format!("messages-{n}.log")))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A skill listing with one `lint` skill.
fn lint_listing() -> Vec<serde_json::Value> {
    use serde_json::json;
    vec![
        json!({"expect": "initialize", "as": "init"}),
        json!({"respond": "init", "result": {}}),
        json!({"expect": "initialized"}),
        json!({"expect": "skills/list", "as": "list"}),
        json!({"respond": "list", "result": {"data": [
            {"cwd": "/work/demo", "errors": [], "skills": [
                {"name": "lint", "description": "", "path": "/s/lint/SKILL.md",
                 "enabled": true, "scope": "repo"}
            ]}
        ]}}),
    ]
}

/// A Codex run that answers once.
fn codex_run() -> Vec<serde_json::Value> {
    use serde_json::json;
    const TID: &str = "00000000-0000-7000-8000-000000000001";
    let send = |method: &str, params: serde_json::Value| json!({"send": {"method": method, "params": params}});
    vec![
        json!({"expect": "initialize", "as": "init"}),
        json!({"respond": "init", "result": {"userAgent": "fake", "codexHome": "/work/codex", "platformFamily": "unix", "platformOs": "macos"}}),
        json!({"expect": "initialized"}),
        json!({"expect": "thread/start", "as": "open"}),
        json!({"respond": "open", "result": {"thread": {"id": TID, "cwd": "/work/demo", "path": null}, "model": "gpt-test"}}),
        send(
            "thread/started",
            json!({"thread": {"id": TID, "model": "gpt-test", "cwd": "/work/demo"}}),
        ),
        json!({"expect": "turn/start", "as": "turn", "match": {"threadId": TID}}),
        json!({"respond": "turn", "result": {"turn": {"id": "turn-1", "status": "inProgress"}}}),
        send(
            "item/completed",
            json!({"item": {"type": "agentMessage", "id": "msg-1", "text": "Linted.", "phase": "final_answer"}, "threadId": TID, "turnId": "turn-1"}),
        ),
        send(
            "turn/completed",
            json!({"threadId": TID, "turn": {"id": "turn-1", "status": "completed", "items": [], "durationMs": 5}}),
        ),
    ]
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
    service_wired(dir, store, None)
}

/// A service whose runs get crew-mcp, if `crew_mcp` names it.
fn service_wired(dir: &Path, store: Store, crew_mcp: Option<(PathBuf, PathBuf)>) -> Service {
    let svc = Service::new(
        store,
        4,
        CliBins {
            claude: Some(dir.join("claude-fake.sh")),
            codex: Some(dir.join("codex")),
            no_title_runs: true,
            ..Default::default()
        },
        dir.join("worktrees"),
        crew_mcp,
        Duration::from_secs(60),
        256,
    );
    svc.clock().set(at(12, 0));
    svc
}

fn setup_with(open: impl FnOnce(&Path) -> Store) -> Setup {
    setup_full(open, false)
}

/// A setup whose runs get crew-mcp.
fn setup_wired() -> Setup {
    setup_full(|_| Store::open_in_memory().unwrap(), true)
}

fn setup_full(open: impl FnOnce(&Path) -> Store, wired: bool) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    fake_claude(&dir);
    fake_codex(&dir);
    let root = dir.join("demo");
    std::fs::create_dir(&root).unwrap();
    let mcp = wired.then(|| (dir.join("crew-mcp"), dir.join("home")));
    let svc = service_wired(&dir, open(&dir), mcp);
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

    /// A Codex agent and a job of its that runs `/skill`, turned off.
    async fn codex_job(&self, skill: &str) -> JobId {
        let agent = self
            .svc
            .db()
            .store
            .create_agent(&AgentSpec {
                name: "Cody".into(),
                role: String::new(),
                runtime: Runtime::Codex,
                model: "gpt-test".into(),
                instructions: String::new(),
                permission_mode: PermissionMode::ReadOnly,
                allowed_tools: vec![],
                handoff_policy: None,
                handoff_budget: None,
                max_run_minutes: 0,
            })
            .unwrap();
        let mut new = self.new_job("everything");
        new.agent = agent;
        new.skill = Some(skill.into());
        new.enabled = false;
        self.create(new).await
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

    /// Waits until the run's process is up.
    async fn until_live(&self, run: RunId) {
        let started = Instant::now();
        while !self.svc.live_run_pids().iter().any(|(_, r)| *r == run) {
            assert!(started.elapsed() < WAIT, "run {run} never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The messages of the job's thread made by `run`.
    fn run_messages(&self, id: JobId, run: RunId) -> Vec<StoredMessage> {
        let thread = self.job(id).thread;
        let messages = self.svc.db().store.messages(thread).unwrap();
        messages
            .into_iter()
            .filter(|m| m.run == Some(run))
            .collect()
    }

    /// The agent's reply of `run`.
    fn reply(&self, id: JobId, run: RunId) -> StoredMessage {
        self.run_messages(id, run)
            .into_iter()
            .find(|m| m.author == Author::Agent(self.agent))
            .expect("a reply")
    }

    async fn report(&self, run: RunId, found: bool, summary: Option<&str>) -> Result<(), String> {
        self.svc
            .handle(Request::Report {
                run,
                found,
                summary: summary.map(String::from),
            })
            .await
            .map(|_| ())
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

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_skill_job_finds_its_skill_on_a_cold_start() {
    let s = setup();
    let id = s.codex_job("lint").await;
    scenario(&s.dir, 1, lint_listing());
    scenario(&s.dir, 2, codex_run());
    let run: RunId =
        serde_json::from_value(s.svc.handle(Request::RunJobNow { job: id }).await.unwrap())
            .unwrap();
    s.until_ended(run).await;
    let asked = codex_messages(&s.dir, 2);
    let turn = asked
        .iter()
        .find(|m| m["method"] == "turn/start")
        .expect("the run started a turn");
    assert_eq!(
        turn["params"]["input"][1],
        serde_json::json!({"type": "skill", "name": "lint", "path": "/s/lint/SKILL.md"})
    );
    assert_eq!(s.svc.db().store.run(run).unwrap().status, RunStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_skill_job_whose_skill_is_gone_fails_the_slot() {
    let s = setup();
    let id = s.codex_job("gone").await;
    scenario(&s.dir, 1, lint_listing());
    let run: RunId =
        serde_json::from_value(s.svc.handle(Request::RunJobNow { job: id }).await.unwrap())
            .unwrap();
    let stored = s.svc.db().store.run(run).unwrap();
    assert_eq!(stored.status, RunStatus::Failed);
    let thread = s.job(id).thread;
    let note = s
        .svc
        .db()
        .store
        .messages(thread)
        .unwrap()
        .into_iter()
        .find(|m| m.author == Author::System)
        .expect("a note");
    assert_eq!(
        note.body,
        "Couldn't find the skill /gone. Edit the job to pick another."
    );
    assert_eq!(note.run, Some(run));
    // Only the listing ran, no Codex run.
    assert_eq!(
        std::fs::read_to_string(s.dir.join("count")).unwrap().trim(),
        "1"
    );

    // On schedule, the slot fails the same way and the schedule goes on.
    s.svc
        .handle(Request::SetJobEnabled {
            job: id,
            enabled: true,
        })
        .await
        .unwrap();
    s.svc.fire_due(at(12, 15));
    let runs = s.runs(id);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].status, RunStatus::Failed);
    assert_eq!(s.job(id).next_due, due(at(12, 30)));
}

#[tokio::test]
async fn report_found_keeps_the_reply_loud() {
    let s = setup();
    let id = s.create(s.new_job("Look for new issues WAIT")).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_live(run).await;
    s.report(run, true, Some("  Two new issues  "))
        .await
        .unwrap();
    s.go();
    s.until_ended(run).await;

    let stored = s.svc.db().store.run(run).unwrap();
    assert_eq!(stored.status, RunStatus::Done);
    assert_eq!(stored.found, Some(true));
    assert_eq!(stored.summary.as_deref(), Some("Two new issues"));
    assert!(!s.reply(id, run).quiet);
}

#[tokio::test]
async fn report_is_refused_outside_a_live_job_run() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    let thread = s.job(id).thread;
    s.svc.post_message(thread, "@Forge hi", None, None).unwrap();
    let mine = s.svc.db().store.runs(Default::default()).unwrap()[0].id;
    s.until_ended(mine).await;
    assert!(s.report(mine, true, None).await.is_err(), "not a job run");

    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_ended(run).await;
    assert!(s.report(run, true, None).await.is_err(), "already ended");
    assert_eq!(s.svc.db().store.run(run).unwrap().found, None);
}

#[tokio::test]
async fn no_report_is_quiet() {
    let s = setup();
    let id = s.create(s.new_job("Look for new issues")).await;
    let mut notices = s.svc.notices();
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_ended(run).await;
    let reply = s.reply(id, run);
    assert!(reply.quiet, "nothing reported, so quiet");

    // Clients only ever see it quiet, and hear the job changed after.
    let mut seen = Vec::new();
    while let Ok(n) = notices.try_recv() {
        seen.push(n.msg);
    }
    let published: Vec<&StoredMessage> = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Message { message, .. } if message.id == reply.id => Some(message),
            _ => None,
        })
        .collect();
    assert!(!published.is_empty());
    assert!(published.iter().all(|m| m.quiet), "{published:?}");
    let at_reply = seen
        .iter()
        .position(|m| matches!(m, ServerMsg::Message { message, .. } if message.id == reply.id))
        .unwrap();
    assert!(
        seen[at_reply..]
            .iter()
            .any(|m| matches!(m, ServerMsg::JobChanged { job } if job.id == id)),
        "the job page hears the run ended"
    );

    // Reporting nothing found is quiet too.
    let id = s.create(s.new_job("Look again WAIT")).await;
    s.svc.run_job_now(id).await.unwrap();
    let run = s.runs(id)[0].id;
    s.until_live(run).await;
    s.report(run, false, None).await.unwrap();
    s.go();
    s.until_ended(run).await;
    assert_eq!(s.svc.db().store.run(run).unwrap().found, Some(false));
    assert!(s.reply(id, run).quiet);
}

#[tokio::test]
async fn every_run_is_never_quiet_when_done() {
    let s = setup();
    let mut new = s.new_job("Summarize the day");
    new.tell = Tell::EveryRun;
    let id = s.create(new).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_ended(run).await;
    assert_eq!(s.svc.db().store.run(run).unwrap().status, RunStatus::Done);
    assert!(!s.reply(id, run).quiet);
}

#[tokio::test]
async fn failed_is_never_quiet() {
    let s = setup();
    let id = s.create(s.new_job("Look around FAIL")).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_ended(run).await;
    assert_eq!(s.svc.db().store.run(run).unwrap().status, RunStatus::Failed);
    let messages = s.run_messages(id, run);
    assert!(!messages.is_empty(), "the failure is noted");
    assert!(messages.iter().all(|m| !m.quiet), "{messages:?}");
}

const JOB_DENIED: &str = "Scheduled job: denied while you're away.";

#[tokio::test]
async fn deny_answers_without_a_card() {
    let s = setup();
    let mut new = s.new_job("Tidy up WAIT");
    new.on_ask = OnAsk::Deny;
    let id = s.create(new).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_live(run).await;

    let wait = s
        .svc
        .ask_permission(
            run,
            "Bash",
            serde_json::json!({"command": "rm -rf x"}),
            None,
        )
        .unwrap();
    assert_eq!(wait.approval, None);
    assert_eq!(
        wait.answer.await.unwrap(),
        PermissionAnswer {
            allow: false,
            message: Some(JOB_DENIED.into()),
        }
    );
    let thread = s.job(id).thread;
    assert!(s.svc.db().store.approvals(thread).unwrap().is_empty());
    assert!(
        s.svc
            .db()
            .store
            .messages(thread)
            .unwrap()
            .iter()
            .all(|m| m.approval.is_none())
    );
    s.go();
    s.until_ended(run).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_answers_without_a_card_for_codex() {
    use serde_json::json;
    let s = setup();
    let id = s.codex_job("lint").await;
    let patch = JobPatch {
        on_ask: Some(OnAsk::Deny),
        ..Default::default()
    };
    s.svc
        .handle(Request::UpdateJob { job: id, patch })
        .await
        .unwrap();
    scenario(&s.dir, 1, lint_listing());
    // The run asks to run a command before it answers; a decline must
    // come back at once.
    let mut steps = codex_run();
    let ask = json!({"request": "item/commandExecution/requestApproval", "id": 0,
        "params": {"threadId": "00000000-0000-7000-8000-000000000001", "turnId": "turn-1",
                   "itemId": "exec-1", "command": "rm -rf x", "cwd": "/work/demo", "commandActions": []},
        "reply": {"decision": "decline"}});
    let at = steps.len() - 2;
    steps.insert(at, ask);
    scenario(&s.dir, 2, steps);
    let run: RunId =
        serde_json::from_value(s.svc.handle(Request::RunJobNow { job: id }).await.unwrap())
            .unwrap();
    s.until_ended(run).await;
    assert_eq!(s.svc.db().store.run(run).unwrap().status, RunStatus::Done);
    let thread = s.job(id).thread;
    assert!(s.svc.db().store.approvals(thread).unwrap().is_empty());
}

#[tokio::test]
async fn report_is_only_served_to_job_runs() {
    let s = setup_wired();
    let id = s.create(s.new_job("Look around")).await;
    let thread = s.job(id).thread;
    s.svc.post_message(thread, "@Forge hi", None, None).unwrap();
    let mine = s.svc.db().store.runs(Default::default()).unwrap()[0].id;
    s.until_ended(mine).await;
    s.svc.fire_due(at(12, 15));
    s.until_ended(s.runs(id)[0].id).await;

    let calls = logged_calls(&s.dir);
    assert_eq!(calls.len(), 2);
    let config = |call: &[String]| flag(call, "--mcp-config").unwrap().to_string();
    let allowed = |call: &[String]| flag(call, "--allowedTools").unwrap_or("").to_string();
    assert!(
        !config(&calls[0]).contains("report"),
        "{}",
        config(&calls[0])
    );
    assert!(!allowed(&calls[0]).contains("report"));
    assert!(
        config(&calls[1]).contains(r#""--tools","handoff,approve,report""#),
        "{}",
        config(&calls[1])
    );
    assert!(
        allowed(&calls[1]).contains("mcp__crew_run__report"),
        "{}",
        allowed(&calls[1])
    );
}

#[tokio::test]
async fn prune_drops_old_quiet_replies() {
    let s = setup();
    let id = s.create(s.new_job("Look around")).await;
    s.svc.fire_due(at(12, 15));
    let run = s.runs(id)[0].id;
    s.until_ended(run).await;
    assert!(s.reply(id, run).quiet);

    let now = Local::now();
    s.svc.clock().set(now + chrono::Duration::days(6));
    s.svc.prune_quiet_replies();
    assert!(s.reply(id, run).quiet, "kept for a week");

    s.svc.clock().set(now + chrono::Duration::days(8));
    s.svc.prune_quiet_replies();
    assert!(
        s.run_messages(id, run)
            .iter()
            .all(|m| m.author != Author::Agent(s.agent)),
        "the quiet reply is gone"
    );
    assert_eq!(s.runs(id).len(), 1, "the run stays");
}
