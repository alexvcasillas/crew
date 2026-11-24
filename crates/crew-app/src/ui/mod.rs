//! The Crew window, built with GPUI-kit.

mod activity;
mod agent_view;
mod approval_card;
mod channel_view;
mod common;
pub mod composer;
mod connect;
#[cfg(debug_assertions)]
mod dev_hooks;
mod dialogs;
pub mod diff_view;
mod first_launch;
mod folder;
mod inspector;
mod markdown_view;
mod mention_menu;
pub mod palette;
mod project_view;
mod run_card;
pub mod running_view;
mod service_control;
pub mod settings;
pub(crate) mod sidebar;
mod status_bar;
mod terminal_tab;
mod terminal_view;
mod thread_settings;
mod thread_view;
mod title_bar;
pub mod workspace;

#[cfg(test)]
mod add_project_tests;
#[cfg(test)]
mod appearance_tests;
#[cfg(test)]
mod auto_tests;
#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod codex_tests;
#[cfg(test)]
mod dialog_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod folder_tests;
#[cfg(test)]
mod inspector_tests;
#[cfg(test)]
mod motion_tests;
#[cfg(test)]
mod pages_tests;
#[cfg(test)]
mod palette_tests;
#[cfg(test)]
mod running_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod sidebar_tests;
#[cfg(test)]
mod terminal_tab_tests;
#[cfg(test)]
mod terminal_tests;
#[cfg(test)]
mod test_ui;
#[cfg(test)]
mod thread_tests;
#[cfg(test)]
mod threads_sidebar_tests;

pub use crate::app::run;

/// The title bar's height with the appearance settings in use.
pub fn title_bar_height() -> gpui_kit::Pixels {
    crate::theme::scale::chrome(title_bar::TITLE_BAR_HEIGHT)
}
