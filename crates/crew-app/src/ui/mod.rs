//! The Crew window, built with GPUI-kit.

mod activity;
mod approval_card;
mod common;
pub mod composer;
mod connect;
#[cfg(debug_assertions)]
mod dev_hooks;
pub mod diff_view;
mod first_launch;
mod inspector;
mod markdown_view;
mod mention_menu;
mod run_card;
pub mod running_view;
mod service_control;
pub mod settings;
mod sidebar;
mod status_bar;
mod terminal_tab;
mod terminal_view;
mod thread_list;
mod thread_settings;
mod thread_view;
mod title_bar;
pub mod workspace;

#[cfg(test)]
mod auto_tests;
#[cfg(test)]
mod codex_tests;
#[cfg(test)]
mod diff_tests;
#[cfg(test)]
mod inspector_tests;
#[cfg(test)]
mod motion_tests;
#[cfg(test)]
mod running_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod terminal_tab_tests;
#[cfg(test)]
mod terminal_tests;
#[cfg(test)]
mod test_ui;
#[cfg(test)]
mod thread_tests;

pub use crate::app::run;
