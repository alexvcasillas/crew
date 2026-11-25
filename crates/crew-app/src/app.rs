//! App setup: fonts, theme, the shortcut table, menus and the window.

use std::path::Path;

use gpui_kit::*;

use crate::connection::ConnectionConfig;
use crate::fonts;
use crate::theme::scale;
use crate::ui::workspace::Workspace;

actions!(
    crew,
    [
        /// Quit Crew (⌘Q).
        Quit,
        /// Search threads, agents and files (⌘K).
        Search,
        /// Start a new thread in the current channel (⌘N).
        NewThread,
        /// Stop every run in every project (⇧⌘.).
        StopAll,
        /// Open settings (⌘,).
        OpenSettings,
        /// Show the threads (⌘1).
        ShowThreads,
        /// Show what is running now (⌘2).
        ShowRunning,
        /// Approve the newest request in the open thread (⌘↵).
        ApproveLatest,
        /// Rename the selected thread card (⌘R).
        RenameThread,
        /// Archive the selected thread card (⌘⌫).
        ArchiveThread,
        /// Zoom the window in a step (⌘+).
        ZoomIn,
        /// Zoom the window out a step (⌘−).
        ZoomOut,
        /// Zoom the window back to 100% (⌘0).
        ResetZoom,
    ]
);

/// The traffic lights' left inset and their buttons' height.
const TRAFFIC_LIGHTS_X: f32 = 16.;
const TRAFFIC_LIGHT_SIZE: f32 = 14.;

/// Where the traffic lights go: centred in the title bar, which grows
/// with text size and zoom. They themselves don't scale.
pub fn traffic_lights() -> Point<Pixels> {
    let title = crate::ui::title_bar_height();
    point(px(TRAFFIC_LIGHTS_X), (title - px(TRAFFIC_LIGHT_SIZE)) / 2.)
}

pub const WINDOW_MIN_WIDTH: f32 = 960.;
pub const WINDOW_MIN_HEIGHT: f32 = 600.;

/// One shortcut: its binding and what the shortcuts list calls it.
pub struct Shortcut {
    pub binding: KeyBinding,
    pub label: &'static str,
}

fn shortcut<A: Action>(keys: &'static str, action: A, label: &'static str) -> Shortcut {
    Shortcut {
        binding: KeyBinding::new(keys, action, None),
        label,
    }
}

/// A shortcut that stays out of the way while you type: text fields keep
/// their own meaning for the keys (⌘⌫ deletes to the start of the line).
fn outside_text<A: Action>(keys: &'static str, action: A, label: &'static str) -> Shortcut {
    Shortcut {
        binding: KeyBinding::new(keys, action, Some("!Input")),
        label,
    }
}

/// Every shortcut, the one list the keymap and any shortcuts page read.
pub fn bindings() -> Vec<Shortcut> {
    vec![
        shortcut("cmd-q", Quit, "Quit"),
        shortcut("cmd-k", Search, "Search"),
        shortcut("cmd-n", NewThread, "New thread"),
        shortcut("cmd-shift-.", StopAll, "Stop all runs"),
        shortcut("cmd-,", OpenSettings, "Settings"),
        shortcut("cmd-1", ShowThreads, "Threads"),
        shortcut("cmd-2", ShowRunning, "Running now"),
        shortcut("cmd-shift-r", ShowRunning, "Running now"),
        shortcut("cmd-enter", ApproveLatest, "Approve the newest request"),
        outside_text("cmd-r", RenameThread, "Rename thread"),
        outside_text("cmd-backspace", ArchiveThread, "Archive thread"),
        outside_text("cmd-=", ZoomIn, "Bigger"),
        outside_text("cmd--", ZoomOut, "Smaller"),
        outside_text("cmd-0", ResetZoom, "Reset zoom"),
    ]
}

fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Crew").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit Crew", Quit),
        ]),
        Menu::new("File").items([MenuItem::action("New thread", NewThread)]),
        Menu::new("View").items([
            MenuItem::action("Threads", ShowThreads),
            MenuItem::action("Running now", ShowRunning),
            MenuItem::separator(),
            MenuItem::action("Search…", Search),
        ]),
        Menu::new("Runs").items([MenuItem::action("Stop all runs", StopAll)]),
    ]
}

/// Fonts, the appearance saved in `crew_home`, theme, key bindings, menus
/// and Quit. Call after `gpui_kit::init`.
pub fn init(crew_home: &Path, cx: &mut App) {
    fonts::load(cx);
    scale::load(crew_home.to_path_buf(), cx);
    cx.bind_keys(bindings().into_iter().map(|s| s.binding));
    // ⌘+ too, which the shortcuts list shows as one row with ⌘=.
    cx.bind_keys([KeyBinding::new("cmd-+", ZoomIn, Some("!Input"))]);
    cx.bind_keys(crate::ui::composer::bindings());
    cx.bind_keys(crate::ui::diff_view::bindings());
    cx.bind_keys(crate::ui::running_view::bindings());
    cx.bind_keys(crate::ui::settings::bindings());
    cx.bind_keys(crate::ui::palette::bindings());
    cx.bind_keys(crate::ui::sidebar::bindings());
    cx.set_menus(menus());
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ZoomIn, cx| scale::step_zoom(1, cx));
    cx.on_action(|_: &ZoomOut, cx| scale::step_zoom(-1, cx));
    cx.on_action(|_: &ResetZoom, cx| scale::step_zoom(0, cx));
}

pub fn run() {
    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        let conn = ConnectionConfig::from_env();
        gpui_kit::init(cx);
        init(&conn.crew_home, cx);
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Crew".into()),
                appears_transparent: true,
                traffic_light_position: Some(traffic_lights()),
            }),
            app_owns_titlebar_drag: true,
            window_min_size: Some(size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT))),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(conn, window, cx))
        })
        .expect("failed to open the Crew window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::bindings;

    #[test]
    fn keymap_has_the_crew_shortcuts() {
        let keys: Vec<String> = bindings()
            .iter()
            .map(|s| {
                s.binding
                    .keystrokes()
                    .iter()
                    .map(|k| k.unparse(gpui_kit::OperatingSystem::Mac))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(
            keys,
            [
                "cmd-q",
                "cmd-k",
                "cmd-n",
                "cmd-shift-.",
                "cmd-,",
                "cmd-1",
                "cmd-2",
                "cmd-shift-r",
                "cmd-enter",
                "cmd-r",
                "cmd-backspace",
                "cmd-=",
                "cmd--",
                "cmd-0",
            ]
        );
        let labels: Vec<&str> = bindings().iter().map(|s| s.label).collect();
        assert!(labels.contains(&"Stop all runs"), "{labels:?}");
    }
}
