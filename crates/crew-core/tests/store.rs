use std::path::{Path, PathBuf};

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{
    ApprovalKind, ApprovalStatus, Author, RunFilter, Store, StoreError, ThreadWorktree,
};

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: "dev".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: "be brief".into(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec!["Read".into()],
        handoff_policy: None,
        handoff_budget: None,
        max_run_minutes: 20,
    }
}

#[test]
fn round_trips_project_channel_thread_messages() {
    let store = Store::open_in_memory().unwrap();
    let p = store
        .create_project("demo", Path::new("/work/demo"), "main")
        .unwrap();
    let c = store.create_channel(p, "general").unwrap();
    let t = store.create_thread(c, "first").unwrap();
    assert_eq!(store.thread_cwd(t).unwrap(), Path::new("/work/demo"));
    let a = store.create_agent(&spec("Ada")).unwrap();

    store.add_message(t, Author::You, "hello", None).unwrap();
    store.add_message(t, Author::Agent(a), "hi", None).unwrap();
    store.add_message(t, Author::System, "note", None).unwrap();
    let msgs = store.messages(t).unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].author, Author::You);
    assert_eq!(msgs[0].body, "hello");
    assert_eq!(msgs[1].author, Author::Agent(a));
    assert_eq!(msgs[2].author, Author::System);
    assert!(msgs.iter().all(|m| m.thread == t && m.created_at > 0));

    let agents = store.agents().unwrap();
    assert_eq!(agents, vec![(a, spec("Ada"))]);
    assert!(matches!(
        store.thread_cwd(crew_core::ids::ThreadId::new()),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn agent_names_are_unique_ignoring_case() {
    let store = Store::open_in_memory().unwrap();
    store.create_agent(&spec("Ada")).unwrap();
    let err = store.create_agent(&spec("aDA")).unwrap_err();
    assert!(matches!(err, StoreError::DuplicateAgentName(n) if n == "aDA"));
}

fn setup_run(store: &Store) -> (crew_core::ids::ThreadId, crew_core::ids::AgentId) {
    let p = store
        .create_project("demo", Path::new("/work/demo"), "main")
        .unwrap();
    let c = store.create_channel(p, "general").unwrap();
    let t = store.create_thread(c, "t").unwrap();
    let a = store.create_agent(&spec("Ada")).unwrap();
    (t, a)
}

#[test]
fn events_keep_order_and_resume_from_seq() {
    let store = Store::open_in_memory().unwrap();
    let (t, a) = setup_run(&store);
    let r = store.create_run(t, a).unwrap();
    let r2 = store.create_run(t, a).unwrap();
    for i in 0..5 {
        let seq = store
            .append_event(
                r,
                &Event::Message {
                    text: format!("m{i}"),
                },
            )
            .unwrap();
        assert_eq!(seq, i);
    }
    assert_eq!(
        store
            .append_event(r2, &Event::Raw { line: "x".into() })
            .unwrap(),
        0
    );
    let all = store.events(r, 0).unwrap();
    assert_eq!(all.len(), 5);
    let tail = store.events(r, 3).unwrap();
    assert_eq!(
        tail,
        vec![
            (3, Event::Message { text: "m3".into() }),
            (4, Event::Message { text: "m4".into() }),
        ]
    );
}

#[test]
fn runs_start_queued_then_get_a_session() {
    let store = Store::open_in_memory().unwrap();
    let (t, a) = setup_run(&store);
    let r = store.create_run(t, a).unwrap();
    let queued = store.run(r).unwrap();
    assert_eq!(queued.status, RunStatus::Queued);
    assert_eq!(queued.session_id, "");
    assert!(queued.ended_at.is_none());

    store.start_run(r, "sess-1").unwrap();
    let started = store.run(r).unwrap();
    assert_eq!(started.status, RunStatus::Working);
    assert_eq!(started.session_id, "sess-1");
    assert!(started.started_at >= queued.started_at);
    assert!(started.ended_at.is_none());

    assert!(matches!(
        store.start_run(crew_core::ids::RunId::new(), "x"),
        Err(StoreError::NotFound("run"))
    ));
}

#[test]
fn unfinished_runs_become_interrupted_on_startup() {
    let store = Store::open_in_memory().unwrap();
    let (t, a) = setup_run(&store);
    let r1 = store.create_run(t, a).unwrap();
    let r2 = store.create_run(t, a).unwrap();
    let r3 = store.create_run(t, a).unwrap();
    store.start_run(r1, "s1").unwrap();
    assert_eq!(store.run(r1).unwrap().status, RunStatus::Working);
    assert_eq!(store.run(r2).unwrap().status, RunStatus::Queued);
    store.set_run_status(r3, RunStatus::Done).unwrap();
    let done = store.run(r3).unwrap();
    assert!(done.ended_at.is_some());

    assert_eq!(store.mark_unfinished_runs_interrupted().unwrap(), 2);
    for r in [r1, r2] {
        let run = store.run(r).unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert!(run.ended_at.is_some());
    }
    assert_eq!(store.run(r3).unwrap(), done);

    let interrupted = store
        .runs(RunFilter {
            status: Some(RunStatus::Interrupted),
            thread: None,
        })
        .unwrap();
    assert_eq!(interrupted.len(), 2);
    let by_thread = store
        .runs(RunFilter {
            status: None,
            thread: Some(t),
        })
        .unwrap();
    assert_eq!(by_thread.len(), 3);
}

#[test]
fn reopening_file_db_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crew.db");
    let t = {
        let store = Store::open(&path).unwrap();
        let (t, a) = setup_run(&store);
        store.add_message(t, Author::You, "kept", None).unwrap();
        let r = store.create_run(t, a).unwrap();
        store
            .append_event(r, &Event::Raw { line: "x".into() })
            .unwrap();
        t
    };
    let store = Store::open(&path).unwrap();
    assert_eq!(store.messages(t).unwrap()[0].body, "kept");
    assert_eq!(store.agents().unwrap().len(), 1);
    assert_eq!(store.runs(RunFilter::default()).unwrap().len(), 1);
}

#[test]
fn newer_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crew.db");
    drop(Store::open(&path).unwrap());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let err = Store::open(&path).err().expect("newer schema must fail");
    assert!(matches!(err, StoreError::NewerSchema));
    assert_eq!(
        err.to_string(),
        "This database was written by a newer Crew."
    );
}

#[test]
fn author_serialises_snake_case_with_kind() {
    let a = crew_core::ids::AgentId::new();
    assert_eq!(
        serde_json::to_value(Author::You).unwrap(),
        serde_json::json!({"kind": "you"})
    );
    assert_eq!(
        serde_json::to_value(Author::System).unwrap(),
        serde_json::json!({"kind": "system"})
    );
    let agent = serde_json::to_value(Author::Agent(a)).unwrap();
    assert_eq!(
        agent,
        serde_json::json!({"kind": "agent", "id": a.to_string()})
    );
    assert_eq!(
        serde_json::from_value::<Author>(agent).unwrap(),
        Author::Agent(a)
    );
}

#[test]
fn agent_spec_enums_serialise_snake_case() {
    let json = serde_json::to_value(spec("Ada")).unwrap();
    assert_eq!(json["runtime"], "claude_code");
    assert_eq!(json["permission_mode"], "read_only");
    let mut edit = spec("Bo");
    edit.permission_mode = PermissionMode::EditFiles;
    let json = serde_json::to_value(&edit).unwrap();
    assert_eq!(json["permission_mode"], "edit_files");
    assert_eq!(serde_json::from_value::<AgentSpec>(json).unwrap(), edit);
}

const SCHEMA_V1: &str = include_str!("../src/store/schema_v1.sql");

/// A table name and its columns as (name, type, not null).
type TableShape = (String, Vec<(String, String, bool)>);

/// Columns of every table, in order, as `PRAGMA table_info` reports them.
fn table_shapes(conn: &rusqlite::Connection) -> Vec<TableShape> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|t| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT name, type, \"notnull\" FROM pragma_table_info('{t}')"
                ))
                .unwrap();
            let cols = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (t, cols)
        })
        .collect()
}

#[test]
fn migrates_v1_db_to_v2_keeping_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crew.db");
    let (p, c, t, a, r) = (
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
        "00000000-0000-4000-8000-000000000003",
        "00000000-0000-4000-8000-000000000004",
        "00000000-0000-4000-8000-000000000005",
    );
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        // A spec written before the handoff and time limit fields existed.
        let old_spec = r#"{"name":"Ada","role":"dev","runtime":"claude_code","model":"m","instructions":"","permission_mode":"edit_files","allowed_tools":[]}"#;
        conn.execute_batch(&format!(
            "INSERT INTO projects VALUES ('{p}', 'demo', '/work/demo', 'main', 1);
             INSERT INTO channels VALUES ('{c}', '{p}', 'general', 2);
             INSERT INTO agents VALUES ('{a}', 'Ada', '{old_spec}', 3);
             INSERT INTO threads VALUES ('{t}', '{c}', 'first', 4);
             INSERT INTO messages VALUES ('00000000-0000-4000-8000-000000000006', '{t}', 'you', NULL, 'hello', 5, 0);
             INSERT INTO runs VALUES ('{r}', '{t}', '{a}', 'sess', 'done', 6, 7, 0);
             INSERT INTO events VALUES ('{r}', 0, '{{\"type\":\"raw\",\"line\":\"x\"}}');"
        ))
        .unwrap();
    }

    let store = Store::open(&path).unwrap();

    let projects = store.projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "demo");
    assert_eq!(projects[0].root, PathBuf::from("/work/demo"));
    assert_eq!(projects[0].setup_command, None);
    assert!(projects[0].copy_files.is_empty());
    let channels = store.channels(projects[0].id).unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].name, "general");
    assert_eq!(channels[0].handoff_policy, None);
    let threads = store.threads(channels[0].id).unwrap();
    assert_eq!(threads.len(), 1);
    let thread = &threads[0];
    assert_eq!(thread.title, "first");
    assert_eq!(thread.worktree, None);
    assert_eq!(thread.handoffs_used, 0);
    assert_eq!(store.project_of_thread(thread.id).unwrap(), projects[0]);

    let msgs = store.messages(thread.id).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "hello");
    assert_eq!(msgs[0].run, None);

    let agents = store.agents().unwrap();
    assert_eq!(agents[0].1.name, "Ada");
    assert_eq!(agents[0].1.max_run_minutes, 20);
    assert_eq!(agents[0].1.handoff_policy, None);

    let runs = store.runs(RunFilter::default()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Done);
    assert_eq!(
        store.events(runs[0].id, 0).unwrap(),
        vec![(0, Event::Raw { line: "x".into() })]
    );
    assert!(store.pending_approvals(None).unwrap().is_empty());
    drop(store);

    // The migrated file has the same shape as a fresh one and is at version 2.
    let migrated = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = migrated
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let fresh_path = dir.path().join("fresh.db");
    drop(Store::open(&fresh_path).unwrap());
    let fresh = rusqlite::Connection::open(&fresh_path).unwrap();
    assert_eq!(table_shapes(&migrated), table_shapes(&fresh));

    // Reopening a v2 file does not migrate again.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.projects().unwrap().len(), 1);
}

#[test]
fn threads_keep_worktree_and_handoff_settings() {
    let store = Store::open_in_memory().unwrap();
    let p = store
        .create_project("demo", Path::new("/work/demo"), "main")
        .unwrap();
    let c = store.create_channel(p, "general").unwrap();
    let t = store.create_thread(c, "first").unwrap();
    let fresh = store.thread(t).unwrap();
    assert_eq!(fresh.id, t);
    assert_eq!(fresh.channel, c);
    assert_eq!(fresh.worktree, None);
    assert_eq!(fresh.handoff_policy, None);
    assert_eq!(fresh.handoff_budget, None);
    assert_eq!(fresh.handoffs_used, 0);

    let wt = ThreadWorktree {
        path: PathBuf::from("/work/crew/worktrees/demo/first"),
        branch: "crew/first".into(),
        base: "abc123".into(),
    };
    let root = store.thread_cwd(t).unwrap();
    store.set_thread_worktree(t, Some(&wt)).unwrap();
    assert_eq!(store.thread_cwd(t).unwrap(), wt.path);
    store
        .set_thread_handoff(t, Some(HandoffPolicy::Budget), Some(5))
        .unwrap();
    assert_eq!(store.bump_handoffs_used(t).unwrap(), 1);
    assert_eq!(store.bump_handoffs_used(t).unwrap(), 2);
    let got = store.thread(t).unwrap();
    assert_eq!(got.worktree, Some(wt));
    assert_eq!(got.handoff_policy, Some(HandoffPolicy::Budget));
    assert_eq!(got.handoff_budget, Some(5));
    assert_eq!(got.handoffs_used, 2);
    assert_eq!(store.threads(c).unwrap(), vec![got]);

    store.set_thread_worktree(t, None).unwrap();
    assert_eq!(store.thread(t).unwrap().worktree, None);
    assert_eq!(store.thread_cwd(t).unwrap(), root);

    store
        .set_project_setup(p, Some("make setup"), &[".env".into()])
        .unwrap();
    let project = store.project_of_thread(t).unwrap();
    assert_eq!(project.setup_command.as_deref(), Some("make setup"));
    assert_eq!(project.copy_files, vec![".env".to_string()]);
    assert_eq!(project.main_branch, "main");

    let missing = crew_core::ids::ThreadId::new();
    assert!(matches!(
        store.thread(missing),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.bump_handoffs_used(missing),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn messages_carry_the_run_that_wrote_them() {
    let store = Store::open_in_memory().unwrap();
    let (t, a) = setup_run(&store);
    let r = store.create_run(t, a).unwrap();
    store
        .add_message(t, Author::Agent(a), "done", Some(r))
        .unwrap();
    let msgs = store.messages(t).unwrap();
    assert_eq!(msgs[0].run, Some(r));
    let json = serde_json::to_value(&msgs[0]).unwrap();
    assert_eq!(json["run"], r.to_string());
}

#[test]
fn approvals_are_created_decided_and_cancelled() {
    let store = Store::open_in_memory().unwrap();
    let (t, a) = setup_run(&store);
    let b = store.create_agent(&spec("Bo")).unwrap();
    let r = store.create_run(t, a).unwrap();
    let r2 = store.create_run(t, a).unwrap();

    let handoff = store
        .create_approval(
            t,
            r,
            &ApprovalKind::Handoff {
                from: a,
                to: b,
                brief: "check it".into(),
            },
        )
        .unwrap();
    let ask = store
        .create_approval(
            t,
            r2,
            &ApprovalKind::Permission {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "touch x"}),
            },
        )
        .unwrap();
    let pending = store.pending_approvals(Some(t)).unwrap();
    assert_eq!(
        pending.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![handoff, ask]
    );
    assert!(
        pending.iter().all(|p| p.status == ApprovalStatus::Pending
            && p.decided_at.is_none()
            && p.thread == t)
    );
    assert_eq!(pending[0].run, r);
    assert!(
        store
            .pending_approvals(Some(crew_core::ids::ThreadId::new()))
            .unwrap()
            .is_empty()
    );

    let decided = store
        .decide_approval(
            handoff,
            ApprovalStatus::Approved,
            Some("check it well".into()),
        )
        .unwrap();
    assert_eq!(decided.status, ApprovalStatus::Approved);
    assert!(decided.decided_at.is_some());
    assert_eq!(
        decided.kind,
        ApprovalKind::Handoff {
            from: a,
            to: b,
            brief: "check it well".into(),
        }
    );
    // A decided approval cannot be decided again.
    assert!(matches!(
        store.decide_approval(handoff, ApprovalStatus::Declined, None),
        Err(StoreError::NotFound(_))
    ));

    assert_eq!(store.cancel_approvals_for_run(r).unwrap(), 0);
    assert_eq!(store.cancel_approvals_for_run(r2).unwrap(), 1);
    assert!(store.pending_approvals(None).unwrap().is_empty());

    assert_eq!(
        serde_json::to_value(&decided.kind).unwrap(),
        serde_json::json!({"kind": "handoff", "from": a.to_string(), "to": b.to_string(), "brief": "check it well"})
    );
    assert_eq!(
        serde_json::to_value(ApprovalStatus::Cancelled).unwrap(),
        "cancelled"
    );
}

#[test]
fn agent_spec_defaults_new_fields() {
    let json = serde_json::json!({
        "name": "Ada", "role": "dev", "runtime": "claude_code", "model": "m",
        "instructions": "", "permission_mode": "read_only", "allowed_tools": []
    });
    let spec: AgentSpec = serde_json::from_value(json).unwrap();
    assert_eq!(spec.max_run_minutes, 20);
    assert_eq!(spec.handoff_policy, None);
    assert_eq!(spec.handoff_budget, None);
    let mut per_agent = spec.clone();
    per_agent.handoff_policy = Some(HandoffPolicy::PerAgent);
    assert_eq!(
        serde_json::to_value(&per_agent).unwrap()["handoff_policy"],
        "per_agent"
    );
}
