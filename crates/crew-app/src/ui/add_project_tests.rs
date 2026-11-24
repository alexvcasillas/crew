//! End to end: the Add project dialog, with a real crewd on a temp home,
//! temp folders and a folder picker that answers a fixed path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use crew_core::project_kind::{NEEDS_REPO, ProjectKind, WorkMode};
use crew_core::store::StoredProject;
use crew_proto::FolderInfo;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, ExternalPaths, FileDropEvent, InputEvent as _,
    Modifiers, MouseMoveEvent, Pixels, Point, Task, TestAppContext, point, px,
};

use super::dialogs::NOT_A_FOLDER;
use super::test_ui::{
    bounds_of, click, has, label_of, open_tall, press, shows, shows_within, type_text,
    wait_to_show, wait_until,
};
use super::thread_tests::world;
use super::workspace::Workspace;
use crate::recents::Recents;
use crate::test_support::{Daemon, short_home};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A git repository at `dir` on branch `main`.
fn git_repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    dir.canonicalize().unwrap()
}

/// The next folder (or file) picked is `path`.
fn picks(cx: &mut TestAppContext, ws: &Entity<Workspace>, path: &Path) {
    let path = path.to_path_buf();
    ws.update(cx, |ws, _| {
        ws.set_folder_picker(Rc::new(move |_| Task::ready(Some(path.clone()))));
    });
}

fn picks_file(cx: &mut TestAppContext, ws: &Entity<Workspace>, path: &Path) {
    let path = path.to_path_buf();
    ws.update(cx, |ws, _| {
        ws.set_file_picker(Rc::new(move |_| Task::ready(Some(path.clone()))));
    });
}

fn dialog_open(cx: &mut TestAppContext, window: AnyWindowHandle) -> bool {
    has(cx, window, "add-project")
}

fn wait_closed(cx: &mut TestAppContext, window: AnyWindowHandle, ws: &Entity<Workspace>) {
    wait_until(cx, "the dialog to close", |cx| {
        ws.read_with(cx, |ws, _| ws.dialog.is_none()) && !dialog_open(cx, window)
    });
}

/// Opens Add project from the sidebar's +.
fn open_dialog(cx: &mut TestAppContext, window: AnyWindowHandle) {
    click(cx, window, "sidebar-projects-add");
    wait_until(cx, "the dialog", |cx| dialog_open(cx, window));
}

fn project_at(
    cx: &mut TestAppContext,
    ws: &Entity<Workspace>,
    root: &Path,
) -> Option<StoredProject> {
    ws.read_with(cx, |ws, _| {
        ws.state()
            .projects()
            .into_iter()
            .find(|p| p.root == root)
            .cloned()
    })
}

/// Waits for the project at `root` and for #general to be open in it.
fn wait_in_general(cx: &mut TestAppContext, ws: &Entity<Workspace>, root: &Path) -> StoredProject {
    wait_until(cx, "#general of the new project", |cx| {
        ws.read_with(cx, |ws, _| {
            let state = ws.state();
            let Some(project) = state.projects().into_iter().find(|p| p.root == root) else {
                return false;
            };
            ws.nav
                .channel
                .and_then(|c| state.channel(c))
                .is_some_and(|c| c.project == project.id && c.name == "general")
                && ws.settings.is_none()
        })
    });
    project_at(cx, ws, root).unwrap()
}

/// The app with one project, so the first launch is skipped.
fn app_with_a_project(
    cx: &mut TestAppContext,
    home: &Path,
    tmp: &Path,
) -> (AnyWindowHandle, Entity<Workspace>, PathBuf) {
    let demo = tmp.join("demo");
    let w = world(home, &demo, &[]);
    let (window, ws) = open_tall(cx, home);
    wait_until(cx, "the project", |cx| {
        ws.read_with(cx, |ws, _| ws.state().project(w.project).is_some())
    });
    // The root as the project has it.
    (window, ws, demo)
}

#[gpui_kit::test]
fn picking_a_repo_fills_the_dialog_and_add_opens_general(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let api = git_repo(&tmp.path().join("api"));
    std::fs::write(api.join(".gitignore"), ".env\n.env.production\nlocal/\n").unwrap();
    std::fs::write(api.join(".env"), "KEY=1\n").unwrap();
    std::fs::write(api.join(".env.production"), "KEY=live\n").unwrap();
    std::fs::create_dir_all(api.join("local")).unwrap();
    std::fs::write(api.join("local/dev.toml"), "x = 1\n").unwrap();
    git(
        &api,
        &["remote", "add", "origin", "git@github.com:owner/api.git"],
    );
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    open_dialog(cx, window);
    for label in [
        "Add project",
        "Pick a folder. A git repo, a folder of repos and a plain folder all work.",
        "Choose a folder",
        "or drag one here",
    ] {
        assert!(shows_within(cx, window, "add-project", label), "{label}");
    }
    assert!(
        !shows_within(cx, window, "add-project", "Recent folders"),
        "nothing picked yet"
    );
    // Add project stays off until a folder is picked.
    click(cx, window, "dialog-confirm");
    assert!(dialog_open(cx, window));

    picks(cx, &ws, &api);
    click(cx, window, "add-project-choose");
    wait_to_show(cx, window, "Git repo on main · origin github.com/owner/api");
    assert_eq!(
        label_of(cx, window, "add-project-folder").as_deref(),
        Some(api.to_string_lossy().as_ref())
    );
    assert!(has(cx, window, "add-project-change"));
    assert!(!shows_within(cx, window, "add-project", "Choose a folder"));
    let name = ws.read_with(cx, |ws, cx| ws.add_project_name(cx));
    assert_eq!(name.as_deref(), Some("api"));
    assert!(shows(
        cx,
        window,
        "Own worktree per thread (recommended) (selected)"
    ));
    assert!(shows(
        cx,
        window,
        "Each thread gets its own branch and folder, so threads never collide."
    ));
    assert!(shows(cx, window, "In the folder itself"));
    assert!(shows(
        cx,
        window,
        "Agents edit your checkout. One writing agent at a time."
    ));
    assert!(!has(cx, window, "add-project-warning"));
    assert!(shows(cx, window, "Setup files to copy into each worktree"));
    assert!(shows(cx, window, ".env 6 B (checked)"));
    assert!(shows(cx, window, ".env.production live keys, left out"));
    assert!(shows(cx, window, "local/ 1 file (checked)"));
    assert!(shows(cx, window, "Found in the folder and ignored by git."));
    assert!(shows(cx, window, "Setup command · optional"));
    assert!(shows(
        cx,
        window,
        "Runs in each new worktree before the agent starts."
    ));
    assert!(shows(cx, window, "Crew creates #general for you."));

    // Leave local/ out, add a file by hand, and give a setup command.
    click(cx, window, ("add-project-file", 2usize));
    assert!(shows(cx, window, "local/ 1 file"));
    std::fs::write(api.join("seed.sql"), "select 1;\n").unwrap();
    picks_file(cx, &ws, &api.join("seed.sql"));
    click(cx, window, "add-project-add-file");
    wait_to_show(cx, window, "seed.sql 10 B (checked)");
    // A file outside the folder is refused.
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, "x").unwrap();
    picks_file(cx, &ws, &outside);
    click(cx, window, "add-project-add-file");
    wait_to_show(cx, window, "Pick a file inside api.");

    click(cx, window, "add-project-setup");
    type_text(cx, window, "pnpm install");
    click(cx, window, "add-project-name");
    press(cx, window, "cmd-a backspace");
    type_text(cx, window, "Api");

    click(cx, window, "dialog-confirm");
    let project = wait_in_general(cx, &ws, &api);
    wait_closed(cx, window, &ws);
    assert_eq!(project.name, "Api");
    assert_eq!(project.kind, ProjectKind::Repo);
    assert_eq!(project.work_mode, WorkMode::OwnCopy);
    assert_eq!(project.main_branch, "main");
    assert_eq!(project.copy_files, [".env", "seed.sql"]);
    assert_eq!(project.setup_command.as_deref(), Some("pnpm install"));

    // It is a recent folder now, kept in the prefs file.
    wait_until(cx, "the recent folder on disk", |_| {
        Recents::load(home.path())
            .folders
            .first()
            .is_some_and(|f| f.path == api && f.kind == ProjectKind::Repo)
    });
}

#[gpui_kit::test]
fn a_plain_folder_works_in_place_only(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let notes = tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("index.md"), "# Notes\n").unwrap();
    let notes = notes.canonicalize().unwrap();
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    open_dialog(cx, window);
    picks(cx, &ws, &notes);
    click(cx, window, "add-project-choose");
    wait_to_show(cx, window, "Plain folder, no git");
    assert!(shows(cx, window, "In the folder itself (selected)"));
    assert!(shows(
        cx,
        window,
        "Agents edit files in place. One writing agent at a time; others wait."
    ));
    assert!(shows(cx, window, "Own copy per thread (disabled)"));
    assert!(shows(cx, window, NEEDS_REPO));
    // In the folder comes first.
    let in_place = bounds_of(cx, window, "add-project-mode-in-place").unwrap();
    let own_copy = bounds_of(cx, window, "add-project-mode-own-copy").unwrap();
    assert!(in_place.origin.y < own_copy.origin.y);
    assert_eq!(
        label_of(cx, window, "add-project-warning").as_deref(),
        Some("Agents edit your files directly. There's no undo outside git.")
    );
    assert!(!shows(cx, window, "Setup files to copy into each worktree"));
    assert!(!has(cx, window, "add-project-setup"));
    assert!(shows(cx, window, "Crew creates #general for you."));

    click(cx, window, "add-project-mode-own-copy");
    assert!(
        shows(cx, window, "In the folder itself (selected)"),
        "it stays"
    );

    click(cx, window, "dialog-confirm");
    let project = wait_in_general(cx, &ws, &notes);
    assert_eq!(project.kind, ProjectKind::Plain);
    assert_eq!(project.work_mode, WorkMode::InPlace);
    assert_eq!(project.name, "notes");
}

#[gpui_kit::test]
fn a_folder_of_repos_lists_them_and_can_work_in_place(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let platform = tmp.path().join("platform");
    for repo in ["api", "web", "infra"] {
        git_repo(&platform.join(repo));
    }
    std::fs::write(platform.join("README.md"), "hi\n").unwrap();
    let platform = platform.canonicalize().unwrap();
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    open_dialog(cx, window);
    picks(cx, &ws, &platform);
    click(cx, window, "add-project-choose");
    wait_to_show(
        cx,
        window,
        "Folder with 3 git repos inside: api, infra, web",
    );
    assert!(shows(
        cx,
        window,
        "Own copy per thread (recommended) (selected)"
    ));
    assert!(shows(
        cx,
        window,
        "Crew makes a thread folder with a worktree of each repo, so threads don't collide. Plain files outside the repos are not copied."
    ));
    assert!(shows(
        cx,
        window,
        "Agents edit files in place and all threads share the folder. One writing agent at a time; others wait."
    ));
    assert!(!has(cx, window, "add-project-warning"));
    assert!(has(cx, window, "add-project-setup"));

    // In place: the warning shows and the worktree fields go.
    click(cx, window, "add-project-mode-in-place");
    assert!(shows(cx, window, "In the folder itself (selected)"));
    assert!(has(cx, window, "add-project-warning"));
    assert!(!has(cx, window, "add-project-setup"));

    click(cx, window, "dialog-confirm");
    let project = wait_in_general(cx, &ws, &platform);
    assert_eq!(project.work_mode, WorkMode::InPlace);
    let ProjectKind::MultiRepo { mut repos } = project.kind else {
        panic!("a folder of repos");
    };
    repos.sort();
    assert_eq!(repos, ["api", "infra", "web"]);
}

#[gpui_kit::test]
fn a_folder_already_added_offers_to_open_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let other = git_repo(&tmp.path().join("other"));
    let (window, ws, demo) = app_with_a_project(cx, home.path(), tmp.path());

    // From Settings › Projects, which keeps no add form of its own.
    press(cx, window, "cmd-,");
    click(cx, window, "settings-nav-projects");
    click(cx, window, "settings-add-project");
    wait_until(cx, "the dialog", |cx| dialog_open(cx, window));
    picks(cx, &ws, &demo);
    click(cx, window, "add-project-choose");
    wait_to_show(cx, window, "Already added as demo.");
    assert!(has(cx, window, "add-project-open"));
    assert!(!shows(cx, window, "How agents work here"));
    click(cx, window, "dialog-confirm");
    assert!(dialog_open(cx, window), "Add project is off");

    // Change to another folder, then back through Recent folders.
    picks(cx, &ws, &other);
    click(cx, window, "add-project-change");
    wait_to_show(cx, window, "Git repo on main");
    assert!(!shows(cx, window, "Already added as demo."));
    click(cx, window, "dialog-cancel");
    wait_closed(cx, window, &ws);

    click(cx, window, "settings-add-project");
    wait_until(cx, "the dialog", |cx| dialog_open(cx, window));
    assert!(shows_within(cx, window, "add-project", "Recent folders"));
    let rows = ws.read_with(cx, |ws, _| ws.recents.folders.len());
    assert_eq!(rows, 2, "both picked folders");
    assert!(shows(cx, window, "other git"));
    click(cx, window, ("add-project-recent", 1usize));
    wait_to_show(cx, window, "Already added as demo.");

    click(cx, window, "add-project-open");
    wait_closed(cx, window, &ws);
    wait_in_general(cx, &ws, &demo);
}

/// Drags `path` in from outside the window and drops it at `position`.
fn drop_path(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    path: &Path,
    position: Point<Pixels>,
) {
    let path = path.to_path_buf();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // The pointer comes in from outside: the dialog may have been
        // reached by keys.
        window.dispatch_event(
            MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers: Modifiers::default(),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            FileDropEvent::Entered {
                position,
                paths: ExternalPaths([path].into_iter().collect()),
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(FileDropEvent::Pending { position }.to_platform_input(), cx);
        window.render_frame(cx);
        window.dispatch_event(FileDropEvent::Submit { position }.to_platform_input(), cx);
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_kit::test]
fn dropping_a_folder_on_the_dialog_picks_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let shop = git_repo(&tmp.path().join("shop"));
    let notes = tmp.path().join("notes.md");
    std::fs::write(&notes, "# Notes\n").unwrap();
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    // From the palette's Add project…, the last row.
    press(cx, window, "cmd-k");
    wait_until(cx, "the palette", |cx| has(cx, window, "palette"));
    press(cx, window, "up enter");
    wait_until(cx, "the dialog", |cx| dialog_open(cx, window));

    // A file is not a folder: the error shows and Add stays off.
    let zone = bounds_of(cx, window, "add-project-drop").unwrap();
    drop_path(cx, window, &notes, zone.center());
    wait_to_show(cx, window, NOT_A_FOLDER);
    click(cx, window, "dialog-confirm");
    assert!(dialog_open(cx, window), "Add project is off");
    assert!(!ws.read_with(cx, |ws, _| ws.add_project_ready()));

    // The whole dialog takes a drop, the footer beside Cancel too.
    let cancel = bounds_of(cx, window, "dialog-cancel").unwrap();
    let footer = point(cancel.origin.x - px(60.), cancel.center().y);
    drop_path(cx, window, &shop, footer);
    wait_to_show(cx, window, "Git repo on main");
    assert!(!shows(cx, window, NOT_A_FOLDER), "the error is gone");
    assert_eq!(
        label_of(cx, window, "add-project-folder").as_deref(),
        Some(shop.to_string_lossy().as_ref())
    );
    press(cx, window, "enter");
    wait_in_general(cx, &ws, &shop);
}

#[gpui_kit::test]
fn a_folder_inside_a_repo_adds_the_whole_repo(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let api = git_repo(&tmp.path().join("api"));
    std::fs::create_dir_all(api.join("src/parse")).unwrap();
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    open_dialog(cx, window);
    picks(cx, &ws, &api.join("src/parse"));
    click(cx, window, "add-project-choose");
    wait_to_show(
        cx,
        window,
        "Inside the api repo, so Crew adds the whole repo.",
    );
    assert_eq!(
        label_of(cx, window, "add-project-folder").as_deref(),
        Some(api.to_string_lossy().as_ref())
    );
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.add_project_name(cx))
            .as_deref(),
        Some("api")
    );
    click(cx, window, "dialog-confirm");
    let project = wait_in_general(cx, &ws, &api);
    assert_eq!(project.kind, ProjectKind::Repo);

    // Another folder inside it is the same repo, added already.
    open_dialog(cx, window);
    picks(cx, &ws, &api.join("src"));
    click(cx, window, "add-project-choose");
    wait_to_show(cx, window, "Already added as api.");
}

#[gpui_kit::test]
fn a_late_answer_for_an_earlier_folder_is_dropped(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let first = git_repo(&tmp.path().join("first"));
    let second = tmp.path().join("second");
    std::fs::create_dir_all(&second).unwrap();
    let second = second.canonicalize().unwrap();
    let (window, ws, _) = app_with_a_project(cx, home.path(), tmp.path());

    open_dialog(cx, window);
    picks(cx, &ws, &first);
    click(cx, window, "add-project-choose");
    wait_to_show(cx, window, "Git repo on main");

    // A second ask clears what showed: Add is off while crewd reads.
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.add_project_inspect(second.clone(), window, cx)
        })
    })
    .unwrap();
    assert!(shows(cx, window, "Reading folder…"));
    assert!(!shows(cx, window, "Git repo on main"));
    assert!(!ws.read_with(cx, |ws, _| ws.add_project_ready()));
    wait_to_show(cx, window, "Plain folder, no git");
    assert!(!shows(cx, window, "Reading folder…"));

    // An answer for the first ask, landing late, changes nothing.
    let stale = ws.read_with(cx, |ws, _| ws.add_project_seq()) - 1;
    let info = FolderInfo {
        exists: true,
        root: first.to_string_lossy().into_owned(),
        name_suggestion: "first".into(),
        ..FolderInfo::default()
    };
    cx.update_window(window, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.add_project_answer(stale, first.clone(), Ok(info), window, cx)
        })
    })
    .unwrap();
    assert!(shows(cx, window, "Plain folder, no git"));
    assert_eq!(
        label_of(cx, window, "add-project-folder").as_deref(),
        Some(second.to_string_lossy().as_ref())
    );
}

#[gpui_kit::test]
fn the_first_launch_adds_its_project_through_the_dialog(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap();
    let home = short_home();
    let _daemon = Daemon::start(home.path().to_path_buf());
    let notes = tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let notes = notes.canonicalize().unwrap();
    let (window, ws) = open_tall(cx, home.path());
    wait_to_show(cx, window, "Welcome to Crew");

    // The picker opens with the dialog.
    picks(cx, &ws, &notes);
    click(cx, window, "first-launch-pick");
    wait_to_show(cx, window, "Plain folder, no git");
    click(cx, window, "dialog-confirm");
    wait_closed(cx, window, &ws);
    wait_to_show(cx, window, "notes added");
    assert!(!has(cx, window, "first-launch-pick"), "step 1 is done");
    ws.read_with(cx, |ws, _| {
        assert!(ws.first_launch.is_some(), "the first launch goes on");
        let projects = ws.state().projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].work_mode, WorkMode::InPlace);
    });
}
