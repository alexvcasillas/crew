//! Generated thread titles.
//!
//! After a thread's first message, one short run of a cheap model names
//! the thread. It is not a run of an agent: nothing about it is stored,
//! shown or counted. It has no tools, no session and no MCP servers, and
//! it runs in the temp folder, away from any project.
//!
//! Claude Code runs it on Haiku or Sonnet. When Claude Code isn't
//! installed or signed in, or when asked for, Codex runs it on the
//! cheapest model it lists.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crew_core::store::TitleModel;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::process::Command;

use crate::runner::signal_group;
use crate::runtimes;
use crate::service::{CliBins, log};

/// How long a title run may take, sign-in check and model list included.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);
/// Longest title, in characters, as for a first line.
pub(crate) const TITLE_MAX: usize = 80;
/// Most characters of the message the model is shown.
const MESSAGE_MAX: usize = 2000;

/// What the model is asked, with the message after it.
const PROMPT: &str = "Write a short title for a chat thread that starts with the message \
below. Use at most six words, in sentence case. Reply with the title only, with no quotes \
and no full stop.\n\nMessage:\n";

/// A title for `message`, or `None` if the run failed, took too long or
/// gave nothing usable.
pub(crate) async fn generate(bins: &CliBins, model: TitleModel, message: &str) -> Option<String> {
    let prompt = prompt(message);
    let raw = tokio::time::timeout(TIMEOUT, async {
        match claude_model(bins, model).await {
            Some((bin, model)) => run(claude_command(&bin, model, &prompt), true).await,
            None => {
                let (bin, model) = codex_model(bins).await?;
                run(codex_command(&bin, &model, &prompt), false).await
            }
        }
    })
    .await;
    match raw {
        Ok(raw) => clean(&raw?),
        Err(_) => {
            log(format!(
                "the title run didn't answer within {} s",
                TIMEOUT.as_secs()
            ));
            None
        }
    }
}

/// The `claude` binary and model alias to use, or `None` when Codex runs
/// it instead.
async fn claude_model(bins: &CliBins, model: TitleModel) -> Option<(PathBuf, &'static str)> {
    let alias = match model {
        TitleModel::Haiku => "haiku",
        TitleModel::Sonnet => "sonnet",
        TitleModel::CodexCheapest => return None,
    };
    let bin = runtimes::find_bin(&runtimes::claude_bin(bins))?;
    // Unknown counts as signed in: the run says soon enough.
    match runtimes::claude_signed_in(&bin).await {
        Some(false) => None,
        _ => Some((bin, alias)),
    }
}

/// The `codex` binary and its cheapest model, if it is installed, signed
/// in and lists one.
async fn codex_model(bins: &CliBins) -> Option<(PathBuf, String)> {
    let bin = runtimes::find_bin(&runtimes::codex_bin(bins))?;
    if runtimes::codex_signed_in(&bin).await.ok().flatten() != Some(true) {
        return None;
    }
    let models = runtimes::codex_models(&bin).await;
    let model = crew_proto::default_codex_model(&models)?.id.clone();
    Some((bin, model))
}

fn prompt(message: &str) -> String {
    let message: String = message.trim().chars().take(MESSAGE_MAX).collect();
    format!("{PROMPT}{message}")
}

/// `claude -p` with no tools, slash commands, MCP servers or saved
/// session, answering in JSON. The prompt goes last, after `--`.
fn claude_command(bin: &Path, model: &str, prompt: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-p",
        "--model",
        model,
        "--output-format",
        "json",
        "--tools",
        "",
        "--disable-slash-commands",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--",
        prompt,
    ]);
    cmd
}

/// `codex exec` in a read-only sandbox with no saved session. It prints
/// the final answer on stdout.
fn codex_command(bin: &Path, model: &str, prompt: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args([
        "exec",
        "--model",
        model,
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "--ephemeral",
        "--color",
        "never",
        "--",
        prompt,
    ]);
    cmd
}

/// Runs the command in the temp folder, in its own process group, and
/// returns what it printed: for `claude`, the `result` of its JSON answer.
/// `None` if it failed. The group is killed when it ends or is dropped.
async fn run(mut cmd: Command, claude: bool) -> Option<String> {
    cmd.current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            log(format!("couldn't start the title run: {e}"));
            return None;
        }
    };
    let group = Group(child.id().map(|p| Pid::from_raw(p as i32)));
    let out = child.wait_with_output().await.ok()?;
    drop(group);
    if !out.status.success() {
        log(format!("the title run failed: {}", out.status));
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !claude {
        return Some(stdout);
    }
    result_text(&stdout)
}

/// Kills a process group when dropped, so a title run that timed out
/// leaves nothing behind.
struct Group(Option<Pid>);

impl Drop for Group {
    fn drop(&mut self) {
        signal_group(self.0, Signal::SIGKILL);
    }
}

/// The text of `claude -p --output-format json`'s answer, unless it is an
/// error.
fn result_text(stdout: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if json.get("type")? != "result" || json.get("is_error") == Some(&true.into()) {
        return None;
    }
    Some(json.get("result")?.as_str()?.to_string())
}

/// The title in a model's answer: its first line that isn't blank,
/// trimmed, without quotes or a full stop around it, at most
/// [`TITLE_MAX`] characters and ending in "…" when cut. `None` if nothing
/// is left.
pub(crate) fn clean(raw: &str) -> Option<String> {
    const QUOTES: &[char] = &['"', '\'', '`', '“', '”', '‘', '’', '«', '»', '*'];
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line);
    let line = line
        .trim_start_matches('#')
        .trim()
        .trim_matches(QUOTES)
        .trim()
        .trim_end_matches('.')
        .trim_matches(QUOTES)
        .trim();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= TITLE_MAX {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(TITLE_MAX - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::{TITLE_MAX, clean, result_text};

    #[test]
    fn a_title_is_one_trimmed_line_without_quotes() {
        assert_eq!(
            clean("  \"Flaky parallel runner on Linux.\"  \n").as_deref(),
            Some("Flaky parallel runner on Linux")
        );
        assert_eq!(
            clean("\n\nTitle: “Fix the parser”\nsecond line").as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(clean("`**Ship it**`").as_deref(), Some("Ship it"));
        assert_eq!(clean("## Port it to C#").as_deref(), Some("Port it to C#"));
    }

    #[test]
    fn nothing_left_means_no_title() {
        assert_eq!(clean(""), None);
        assert_eq!(clean("  \n \"\" \n"), None);
        assert_eq!(clean("..."), None);
    }

    #[test]
    fn a_long_title_is_cut_like_a_first_line() {
        let long = "word ".repeat(40);
        let cut = clean(&long).unwrap();
        assert_eq!(cut.chars().count(), TITLE_MAX);
        assert!(cut.ends_with("word…"), "{cut}");
    }

    #[test]
    fn only_a_claude_answer_that_isnt_an_error_counts() {
        assert_eq!(
            result_text(r#"{"type":"result","is_error":false,"result":"Fix it"}"#).as_deref(),
            Some("Fix it")
        );
        assert_eq!(
            result_text(r#"{"type":"result","is_error":true,"result":"Not logged in"}"#),
            None
        );
        // Stream lines are not an answer.
        assert_eq!(
            result_text("{\"type\":\"system\"}\n{\"type\":\"result\",\"result\":\"x\"}"),
            None
        );
        assert_eq!(result_text("Fix it"), None);
    }
}
