//! Finds the agent CLIs on this machine.
//!
//! Only runs each CLI's `--version`, its own sign-in status command and,
//! for Codex, `model/list` on a short-lived `codex app-server`. Never reads
//! a CLI's credentials or config.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crew_core::agent::Runtime;
use crew_core::claude::claude_bin_from;
use crew_core::codex::CodexConn;
use crew_core::codex::proto::{InitializeParams, ModelListResponse, method};
use crew_proto::{ModelInfo, RuntimeInfo};
use tokio::process::Command;

use crate::service::{CliBins, log};

/// How long one CLI command may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The models a Claude Code agent can pick. Claude Code takes these
/// aliases and maps them to the latest model of each family.
const CLAUDE_MODELS: [(&str, &str); 3] =
    [("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];

/// How Crew asks a CLI whether it is signed in.
#[derive(Clone, Copy)]
enum SignIn {
    /// `claude auth status`.
    Claude,
    /// `codex login status`.
    Codex,
}

/// Claude Code, then Codex. The `claude` binary is the one runs use. A
/// binary that is found counts as installed, even if `--version` fails.
pub async fn detect(bins: &CliBins) -> Vec<RuntimeInfo> {
    let (claude, codex) = names(bins);
    let (claude, codex) = tokio::join!(
        probe(Runtime::ClaudeCode, &claude, SignIn::Claude),
        probe(Runtime::Codex, &codex, SignIn::Codex),
    );
    vec![claude, codex]
}

/// The `claude` and `codex` commands to look for.
fn names(bins: &CliBins) -> (PathBuf, PathBuf) {
    let claude = bins
        .claude
        .clone()
        .unwrap_or_else(|| claude_bin_from(std::env::var_os("CREW_CLAUDE_BIN")));
    (claude, codex_bin(bins))
}

/// The `codex` command runs and checks use.
pub(crate) fn codex_bin(bins: &CliBins) -> PathBuf {
    bins.codex.clone().unwrap_or_else(|| PathBuf::from("codex"))
}

fn claude_models() -> Vec<ModelInfo> {
    CLAUDE_MODELS
        .iter()
        .map(|(id, label)| ModelInfo {
            id: id.to_string(),
            label: label.to_string(),
        })
        .collect()
}

/// Which CLIs are on `PATH`, without running them: no version, sign-in
/// or Codex models.
pub fn found(bins: &CliBins) -> Vec<RuntimeInfo> {
    let (claude, codex) = names(bins);
    [(Runtime::ClaudeCode, claude), (Runtime::Codex, codex)]
        .into_iter()
        .map(|(runtime, bin)| {
            let path = find(&bin);
            let models = match runtime {
                Runtime::ClaudeCode => claude_models(),
                Runtime::Codex => vec![],
            };
            RuntimeInfo {
                runtime,
                installed: path.is_some(),
                version: None,
                signed_in: None,
                path: path.map(|p| p.to_string_lossy().into_owned()),
                models,
            }
        })
        .collect()
}

/// Whether Codex is signed in, from the exit code of `codex login status`.
/// Its output is never read. `None` if it didn't answer in time; an error
/// if it couldn't start.
pub(crate) async fn codex_signed_in(bin: &Path) -> io::Result<Option<bool>> {
    let status = Command::new(bin)
        .args(["login", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(TIMEOUT, status).await {
        Ok(status) => Ok(Some(status?.success())),
        Err(_) => Ok(None),
    }
}

/// The models a short-lived `codex app-server` lists, minus hidden ones.
/// Empty if it fails or takes too long.
async fn codex_models(bin: &Path) -> Vec<ModelInfo> {
    let list = async {
        let mut cmd = Command::new(bin);
        cmd.arg("app-server").stderr(Stdio::null());
        // Dropping the connection ends the process.
        let (conn, _incoming) = CodexConn::spawn(cmd)?;
        conn.initialize(&InitializeParams::crew(env!("CARGO_PKG_VERSION")))
            .await?;
        conn.call::<_, ModelListResponse>(method::MODEL_LIST, &serde_json::json!({}))
            .await
    };
    match tokio::time::timeout(TIMEOUT, list).await {
        Ok(Ok(list)) => list
            .data
            .into_iter()
            .filter(|m| !m.hidden && !m.id.is_empty())
            .map(|m| ModelInfo {
                label: if m.display_name.trim().is_empty() {
                    m.id.clone()
                } else {
                    m.display_name
                },
                id: m.id,
            })
            .collect(),
        Ok(Err(e)) => {
            log(format!("couldn't list the Codex models: {e}"));
            vec![]
        }
        Err(_) => {
            log(format!(
                "Codex didn't list its models within {} s",
                TIMEOUT.as_secs()
            ));
            vec![]
        }
    }
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

async fn probe(runtime: Runtime, bin: &Path, sign_in: SignIn) -> RuntimeInfo {
    let missing = RuntimeInfo {
        runtime: runtime.clone(),
        installed: false,
        version: None,
        signed_in: None,
        path: None,
        // Claude Code's are known without asking it.
        models: match sign_in {
            SignIn::Claude => claude_models(),
            SignIn::Codex => vec![],
        },
    };
    let Some(path) = find(bin) else {
        return missing;
    };
    // All at once, so a slow CLI takes one time limit, not several.
    let (version, signed_in, models) = tokio::join!(
        run(&path, &["--version"], true),
        async {
            match sign_in {
                SignIn::Claude => run(&path, &["auth", "status"], false)
                    .await
                    .and_then(signed_in),
                SignIn::Codex => codex_signed_in(&path).await.ok().flatten(),
            }
        },
        async {
            match sign_in {
                SignIn::Claude => claude_models(),
                SignIn::Codex => codex_models(&path).await,
            }
        },
    );
    // The binary is there even if it won't say its version, as a stuck or
    // stubbed CLI may not.
    let version = match version {
        Some(ran) if ran.ok => parse_version(&ran.stdout),
        Some(_) => {
            log(format!("`{} --version` failed", path.display()));
            None
        }
        None => {
            log(format!(
                "`{} --version` didn't answer within {} s",
                path.display(),
                TIMEOUT.as_secs()
            ));
            None
        }
    };
    RuntimeInfo {
        runtime,
        installed: true,
        version,
        signed_in,
        path: Some(path.to_string_lossy().into_owned()),
        models,
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
