//! Crew: a macOS app where coding-agent CLIs work as chat members.

pub mod client;
pub mod connection;
pub mod model;

#[cfg(test)]
mod test_support;

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod fonts;
#[cfg(target_os = "macos")]
mod theme;
#[cfg(target_os = "macos")]
pub mod ui;
