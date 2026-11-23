//! Crew: a macOS app where coding-agent CLIs work as chat members.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod connection;

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod fonts;
#[cfg(target_os = "macos")]
mod theme;
#[cfg(target_os = "macos")]
mod ui;

#[cfg(target_os = "macos")]
fn main() {
    ui::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Crew runs on macOS only.");
    std::process::exit(1);
}
