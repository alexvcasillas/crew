//! The Crew window, built with GPUI-kit.

mod common;
pub mod composer;
mod connect;
#[cfg(debug_assertions)]
mod dev_hooks;
mod mention_menu;
mod run_card;
mod service_control;
mod sidebar;
mod status_bar;
mod thread_list;
mod thread_view;
mod title_bar;
pub mod workspace;

#[cfg(test)]
mod test_ui;
#[cfg(test)]
mod thread_tests;

pub use crate::app::run;
