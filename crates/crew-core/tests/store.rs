use std::path::Path;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::event::{Event, RunStatus};
use crew_core::store::{Author, RunFilter, Store, StoreError};

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        role: "dev".into(),
        runtime: Runtime::ClaudeCode,
        model: "claude-haiku-4-5".into(),
        instructions: "be brief".into(),
        permission_mode: PermissionMode::ReadOnly,
        allowed_tools: vec!["Read".into()],
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

    store.add_message(t, Author::You, "hello").unwrap();
    store.add_message(t, Author::Agent(a), "hi").unwrap();
    store.add_message(t, Author::System, "note").unwrap();
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
        store.add_message(t, Author::You, "kept").unwrap();
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
