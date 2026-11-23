//! Crew: a macOS app where coding-agent CLIs work as chat members.

#[cfg(target_os = "macos")]
fn main() {
    crew_app::ui::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Crew runs on macOS only.");
    std::process::exit(1);
}
