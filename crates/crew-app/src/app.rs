//! App setup: fonts, theme, the shortcut table, menus and the window.

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeConfig};
use gpui_kit::*;

use crate::connection::ConnectionConfig;
use crate::ui::workspace::Workspace;
use crate::{fonts, theme};

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
    ]
);

pub const WINDOW_MIN_WIDTH: f32 = 960.;
pub const WINDOW_MIN_HEIGHT: f32 = 600.;

/// One shortcut: its binding and what the shortcuts list calls it.
pub struct Shortcut {
    pub binding: KeyBinding,
    // Read by the shortcuts list in settings.
    #[cfg_attr(not(test), allow(dead_code))]
    pub label: &'static str,
}

fn shortcut<A: Action>(keys: &'static str, action: A, label: &'static str) -> Shortcut {
    Shortcut {
        binding: KeyBinding::new(keys, action, None),
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

/// Fonts, theme, key bindings, menus and Quit. Call after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    let (ui, mono) = fonts::load(cx);
    match serde_json::from_value::<ThemeConfig>(theme::theme_config(ui, mono)) {
        Ok(config) => Theme::update(cx, |t| t.apply_config(&Rc::new(config))),
        Err(e) => eprintln!("crew: theme does not parse: {e}"),
    }
    cx.bind_keys(bindings().into_iter().map(|s| s.binding));
    cx.bind_keys(crate::ui::composer::bindings());
    cx.set_menus(menus());
    cx.on_action(|_: &Quit, cx| cx.quit());
}

pub fn run() {
    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        init(cx);
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Crew".into()),
                appears_transparent: true,
                // Centres the traffic lights in the 48px title bar.
                traffic_light_position: Some(point(px(16.), px(17.))),
            }),
            app_owns_titlebar_drag: true,
            window_min_size: Some(size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT))),
            ..Default::default()
        };
        let conn = ConnectionConfig::from_env();
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
                "cmd-2"
            ]
        );
        let labels: Vec<&str> = bindings().iter().map(|s| s.label).collect();
        assert!(labels.contains(&"Stop all runs"), "{labels:?}");
    }
}
