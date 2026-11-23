//! Finds the agent CLIs on this machine.
//!
//! Only runs each CLI's `--version` and its own sign-in status command.
//! Never reads a CLI's credentials or config.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crew_core::agent::Runtime;
use crew_core::claude::claude_bin_from;
use crew_proto::RuntimeInfo;
use tokio::process::Command;

use crate::service::CliBins;

/// How long one CLI command may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Claude Code, then Codex. The `claude` binary is the one runs use.
pub async fn detect(bins: &CliBins) -> Vec<RuntimeInfo> {
    let claude = bins
        .claude
        .clone()
        .unwrap_or_else(|| claude_bin_from(std::env::var_os("CREW_CLAUDE_BIN")));
    let codex = bins.codex.clone().unwrap_or_else(|| PathBuf::from("codex"));
    let (claude, codex) = tokio::join!(
        probe(Runtime::ClaudeCode, &claude, Some(&["auth", "status"])),
        // No adapter yet, so its sign-in state doesn't matter.
        probe(Runtime::Codex, &codex, None),
    );
    vec![claude, codex]
}

/// Output of a command that finished in time.
struct Ran {
    ok: bool,
    stdout: String,
}

/// Runs `bin args` with a time limit. With `keep_success` false, the
/// output of a command that succeeds is dropped unread.
async fn run(bin: &Path, args: &[&str], keep_success: bool) -> Option<Ran> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(TIMEOUT, child).await.ok()?.ok()?;
    let ok = out.status.success();
    let stdout = if ok && !keep_success {
        String::new()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    Some(Ran { ok, stdout })
}

async fn probe(runtime: Runtime, bin: &Path, status: Option<&[&str]>) -> RuntimeInfo {
    let missing = RuntimeInfo {
        runtime: runtime.clone(),
        installed: false,
        version: None,
        signed_in: None,
        path: None,
    };
    let Some(path) = find(bin) else {
        return missing;
    };
    // Both at once, so a slow CLI takes one time limit, not two.
    let (version, signed_in) = tokio::join!(run(&path, &["--version"], true), async {
        match status {
            Some(args) => run(&path, args, false).await.and_then(signed_in),
            None => None,
        }
    });
    let version = match version {
        Some(ran) if ran.ok => parse_version(&ran.stdout),
        _ => return missing,
    };
    RuntimeInfo {
        runtime,
        installed: true,
        version,
        signed_in,
        path: Some(path.to_string_lossy().into_owned()),
    }
}

/// `claude auth status` exits 0 when signed in. When signed out it exits
/// 1 and prints `"loggedIn": false`. Anything else, such as a CLI without
/// the command, is unknown.
fn signed_in(ran: Ran) -> Option<bool> {
    if ran.ok {
        return Some(true);
    }
    let json: serde_json::Value = serde_json::from_str(&ran.stdout).ok()?;
    (json.get("loggedIn")? == false).then_some(false)
}

/// The first word that starts with a digit, as in `2.1.0 (Claude Code)`
/// or `codex-cli 0.1.0`. Else the whole first line.
fn parse_version(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    let word = line
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()));
    Some(word.unwrap_or(line).to_string()).filter(|v| !v.is_empty())
}

/// The binary a command name runs: itself if it is a path, else the first
/// match on `PATH`.
fn find(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return bin.is_file().then(|| bin.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_parsed() {
        assert_eq!(
            parse_version("2.1.330 (Claude Code)\n").as_deref(),
            Some("2.1.330")
        );
        assert_eq!(
            parse_version("codex-cli 0.165.0\n").as_deref(),
            Some("0.165.0")
        );
        assert_eq!(parse_version("dev build\n").as_deref(), Some("dev build"));
        assert_eq!(parse_version(""), None);
    }

    #[tokio::test]
    async fn output_of_a_success_is_kept_only_when_asked() {
        let echo = Path::new("/bin/echo");
        let kept = run(echo, &["hi"], true).await.unwrap();
        assert!(kept.ok);
        assert_eq!(kept.stdout, "hi\n");
        let dropped = run(echo, &["hi"], false).await.unwrap();
        assert!(dropped.ok);
        assert_eq!(dropped.stdout, "");
        let failed = run(Path::new("/bin/sh"), &["-c", "echo no; exit 1"], false)
            .await
            .unwrap();
        assert!(!failed.ok);
        assert_eq!(failed.stdout, "no\n");
    }

    #[test]
    fn sign_in_comes_from_the_exit_code() {
        let ran = |ok: bool, stdout: &str| Ran {
            ok,
            stdout: stdout.into(),
        };
        assert_eq!(signed_in(ran(true, "")), Some(true));
        assert_eq!(signed_in(ran(false, r#"{"loggedIn":false}"#)), Some(false));
        assert_eq!(signed_in(ran(false, "error: unknown command 'auth'")), None);
        assert_eq!(signed_in(ran(false, r#"{"loggedIn":true}"#)), None);
    }
}
