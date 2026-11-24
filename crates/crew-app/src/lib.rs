//! Crew: a macOS app where coding-agent CLIs work as chat members.

pub mod checks;
pub mod client;
pub mod comments;
pub mod connection;
pub mod diff_parse;
pub mod editor;
pub mod markdown;
pub mod model;
pub mod prefs;
pub mod recents;

#[cfg(test)]
mod test_support;

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod fonts;
#[cfg(target_os = "macos")]
pub mod launchd;
#[cfg(target_os = "macos")]
mod theme;
#[cfg(target_os = "macos")]
pub mod ui;
