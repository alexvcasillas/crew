//! The `crewd` binary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use crewd::{ServeOptions, serve};

fn non_empty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|v| !v.is_empty())
}

fn options() -> Result<(PathBuf, ServeOptions), String> {
    let home = match non_empty(std::env::var_os("CREW_HOME")) {
        Some(home) => PathBuf::from(home),
        None => match non_empty(std::env::var_os("HOME")) {
            Some(h) => PathBuf::from(h).join(".crew"),
            None => return Err("Set CREW_HOME or HOME.".into()),
        },
    };
    let mut opts = ServeOptions::default();
    if let Some(v) = non_empty(std::env::var_os("CREW_MAX_RUNS")) {
        opts.max_runs = v
            .to_str()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("CREW_MAX_RUNS must be a number above 0, got {v:?}."))?;
    }
    opts.claude_bin = non_empty(std::env::var_os("CREW_CLAUDE_BIN")).map(PathBuf::from);
    opts.codex_bin = non_empty(std::env::var_os("CREW_CODEX_BIN")).map(PathBuf::from);
    Ok((home, opts))
}

#[tokio::main]
async fn main() -> ExitCode {
    let (home, opts) = match options() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("crewd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match serve(&home, opts).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crewd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
