//! The palette's catalog of commands and settings: every field a settings
//! page draws has an entry, the commands are the shortcuts list, and a
//! setting's current value reads from the page's state.

use std::collections::BTreeSet;

use crew_core::agent::{AgentSpec, PermissionMode};
use crew_core::ids::AgentId;
use crew_proto::Request;
use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{AnyWindowHandle, AppContext as _, ElementId, TestAppContext};

use super::palette::catalog::{
    Item, Needs, Scope, SettingEntry, commands, current_value, settings_entries,
};
use super::settings::{Page, pretty_keys};
use super::test_ui::{click, open_tall, press, wait_until};
use super::thread_tests::{agent, world};
use crate::app::bindings;
use crate::test_support::{Daemon, fake_claude, short_home};

/// The ids of the `field-` rows on screen.
fn field_ids(cx: &mut TestAppContext, window: AnyWindowHandle) -> BTreeSet<String> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .elements()
            .iter()
            .filter_map(|e| match e.path().last() {
                Some(ElementId::Name(name)) if name.starts_with("field-") => Some(name.to_string()),
                _ => None,
            })
            .collect()
    })
    .unwrap()
}

fn entry(field: &str) -> &'static SettingEntry {
    settings_entries()
        .iter()
        .find(|e| e.field == field)
        .unwrap_or_else(|| panic!("no entry {field}"))
}

#[gpui_kit::test]
fn every_settings_field_has_a_catalog_entry(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let (window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the runtimes, agent and project", |cx| {
        ws.read_with(cx, |ws, _| {
            !ws.runtimes.is_empty()
                && ws.state().agent(w.agents[0]).is_some()
                && ws.state().project(w.project).is_some()
        })
    });
    press(cx, window, "cmd-,");

    let pages = [
        (Page::General, "general"),
        (Page::Appearance, "appearance"),
        (Page::Skills, "skills"),
        (Page::Agents, "agents"),
        (Page::Runtimes, "runtimes"),
        (Page::Limits, "limits"),
        (Page::Projects, "projects"),
    ];
    for (page, nav) in pages {
        click(cx, window, format!("settings-nav-{nav}"));
        let expected: BTreeSet<String> = settings_entries()
            .iter()
            .filter(|e| e.page == page)
            .map(|e| e.field.to_string())
            .collect();
        assert!(!expected.is_empty(), "{nav} has entries");
        assert_eq!(field_ids(cx, window), expected, "the {nav} page");
    }
}

#[test]
fn every_field_id_is_unique_and_a_field_id() {
    let ids: Vec<&str> = settings_entries().iter().map(|e| e.field).collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "no id twice");
    assert!(ids.iter().all(|id| id.starts_with("field-")));
    assert!(
        settings_entries()
            .iter()
            .filter(|e| e.field.starts_with("field-agent-"))
            .all(|e| e.scope == Scope::Agent)
    );
}

#[test]
fn commands_come_from_the_shortcuts_list() {
    let all = commands();
    let shortcuts = bindings();
    for shortcut in &shortcuts {
        let same: Vec<_> = all.iter().filter(|c| c.label == shortcut.label).collect();
        assert_eq!(same.len(), 1, "{} once", shortcut.label);
        assert!(same[0].action.partial_eq(shortcut.binding.action()));
    }
    // Two shortcuts open Running now; the list shows ⌘⇧R.
    let running = all.iter().find(|c| c.label == "Running now").unwrap();
    assert_eq!(
        running.keys.as_deref(),
        Some(pretty_keys("cmd-shift-r").as_str())
    );
    let stop = all.iter().find(|c| c.label == "Stop all runs").unwrap();
    assert_eq!(stop.keys.as_deref(), Some("⌘⇧."));
    assert_eq!(stop.needs, Needs::ActiveRun);
    let rename = all.iter().find(|c| c.label == "Rename thread").unwrap();
    assert_eq!(rename.needs, Needs::OpenThread);
    let approve = all
        .iter()
        .find(|c| c.label == "Approve the newest request")
        .unwrap();
    assert_eq!(approve.needs, Needs::PendingRequest);
    let search = all.iter().find(|c| c.label == "Search").unwrap();
    assert_eq!(search.needs, Needs::Nothing);

    let add = all.iter().find(|c| c.label == "Add project…").unwrap();
    assert_eq!(add.keys, None);
    assert_eq!(add.needs, Needs::Nothing);
    let labels: BTreeSet<&str> = all.iter().map(|c| c.label).collect();
    assert_eq!(labels.len(), all.len(), "no command twice");
}

#[gpui_kit::test]
fn current_value_reads_the_page_state(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start_with(home.path().to_path_buf(), fake_claude(tmp.path(), "exit 0"));
    let w = world(home.path(), &tmp.path().join("repo"), &["Forge"]);
    let auto: AgentId = w.ask(Request::CreateAgent {
        spec: AgentSpec {
            model: "sonnet".into(),
            permission_mode: PermissionMode::Auto,
            ..agent("Anvil", "builds")
        },
    });
    let (_window, ws) = open_tall(cx, home.path());
    wait_until(cx, "the agents and project", |cx| {
        ws.read_with(cx, |ws, _| {
            ws.state().agent(auto).is_some() && ws.state().project(w.project).is_some()
        })
    });
    let value = |cx: &mut TestAppContext, field: &str, item: Option<Item>| {
        ws.read_with(cx, |ws, cx| current_value(ws, entry(field), item, cx))
    };

    let anvil = Some(Item::Agent(auto));
    assert_eq!(
        value(cx, "field-agent-permission", anvil).as_deref(),
        Some("Auto")
    );
    assert_eq!(
        value(cx, "field-agent-model", anvil).as_deref(),
        Some("Sonnet")
    );
    assert_eq!(
        value(cx, "field-agent-runtime", anvil).as_deref(),
        Some("Claude Code")
    );
    assert_eq!(
        value(cx, "field-agent-handoffs", anvil).as_deref(),
        Some("Channel default")
    );
    let forge = Some(Item::Agent(w.agents[0]));
    assert_eq!(
        value(cx, "field-agent-permission", forge).as_deref(),
        Some("Read only")
    );

    // Free text has no value to show.
    assert_eq!(
        value(cx, "field-project-setup", Some(Item::Project(w.project))),
        None
    );
    assert_eq!(value(cx, "field-agent-instructions", anvil), None);
    // A scoped field without its item has nothing to read.
    assert_eq!(value(cx, "field-agent-permission", None), None);

    assert_eq!(
        value(cx, "field-general-thread-titles", None).as_deref(),
        Some("Generated")
    );
    // Finder, or the editor this Mac has.
    assert!(value(cx, "field-general-editor", None).is_some());
    assert_eq!(
        value(cx, "field-appearance-zoom", None).as_deref(),
        Some("100%")
    );
    assert_eq!(
        value(cx, "field-appearance-rendering", None).as_deref(),
        Some("Standard")
    );
    let limits = ws.read_with(cx, |ws, _| ws.state().limits().max_runs);
    assert_eq!(
        value(cx, "field-limits-max-runs", None),
        Some(limits.to_string())
    );
    assert_eq!(
        value(cx, "field-limits-new-runs", None).as_deref(),
        Some("Running")
    );
}

#[test]
fn number_options_are_the_pages_own() {
    let names = |ns: &[String]| ns.to_vec();
    let options = |field: &str| -> Vec<String> {
        entry(field).options.iter().map(|o| o.to_string()).collect()
    };
    let ui: Vec<String> = crate::prefs::UI_SIZES
        .iter()
        .map(|n| n.to_string())
        .collect();
    let code: Vec<String> = crate::prefs::CODE_SIZES
        .iter()
        .map(|n| n.to_string())
        .collect();
    let at_once: Vec<String> = super::settings::limits::AGENTS_AT_ONCE
        .iter()
        .map(|n| n.to_string())
        .collect();
    let budgets: Vec<String> = super::settings::limits::BUDGETS
        .iter()
        .map(|n| n.to_string())
        .collect();
    assert_eq!(options("field-appearance-ui-size"), names(&ui));
    assert_eq!(options("field-appearance-code-size"), names(&code));
    assert_eq!(options("field-limits-max-runs"), names(&at_once));
    assert_eq!(options("field-limits-budget"), names(&budgets));
}
