//! What crewd does with each request a `codex app-server` sends: answer
//! it at once, refuse it, or put it to you as a Crew permission approval.
//! Pure, so the mapping is tested on its own.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crew_core::codex::proto::{
    ApprovalDecision, ApprovalResponse, ElicitationParams, ElicitationResponse, FileChange,
    ServerRequest,
};
use serde_json::{Value, json};

/// The MCP server crew-mcp is registered as.
pub(crate) const CREW_SERVER: &str = "crew";

/// What to do with one server request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Mapped {
    /// Answer at once with this result.
    Reply(Value),
    /// Answer with a "method not found" error carrying this message.
    Refuse(String),
    /// Ask you first.
    Ask(AskFor),
    /// Answer at once with this result and tell the thread `note`.
    Declined { reply: Value, note: String },
}

/// A request that becomes a Crew permission approval.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AskFor {
    pub(crate) kind: ReplyKind,
    pub(crate) tool: String,
    pub(crate) input: Value,
    /// The Codex item it is about.
    pub(crate) item: Option<String>,
}

/// How the answer to a request is shaped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReplyKind {
    /// `{"decision": ...}` for a command or file change.
    Approval,
    /// `{"action": ...}` for an MCP elicitation.
    Elicitation,
    /// A grant of the permissions, or of none. Holds what may be granted:
    /// what was asked, cut down to the thread's folder.
    Permissions(Value),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Answer {
    Yes,
    No,
    /// The run is stopping.
    Cancel,
}

impl ReplyKind {
    pub(crate) fn reply(&self, answer: Answer) -> Value {
        let value = match self {
            ReplyKind::Approval => serde_json::to_value(ApprovalResponse::new(match answer {
                Answer::Yes => ApprovalDecision::Accept,
                Answer::No => ApprovalDecision::Decline,
                Answer::Cancel => ApprovalDecision::Cancel,
            })),
            ReplyKind::Elicitation => serde_json::to_value(match answer {
                Answer::Yes => ElicitationResponse::accept(),
                Answer::No => ElicitationResponse::decline(),
                Answer::Cancel => ElicitationResponse::cancel(),
            }),
            // There is no "cancel" here: granting nothing is the refusal.
            // The grant lasts for the turn only.
            ReplyKind::Permissions(grant) => {
                let granted = match answer {
                    Answer::Yes => grant.clone(),
                    Answer::No | Answer::Cancel => json!({}),
                };
                Ok(json!({"permissions": granted, "scope": "turn"}))
            }
        };
        value.expect("replies serialize")
    }
}

/// Maps a request. `files` holds the changes of started file change
/// items; `crew_wired` says whether the `crew` MCP server is Crew's own;
/// `cwd` is the thread's folder, the only place access can be granted.
pub(crate) fn map_request(
    method: &str,
    params: &Value,
    files: &HashMap<String, Vec<FileChange>>,
    crew_wired: bool,
    cwd: &Path,
) -> Mapped {
    match ServerRequest::parse(method, params) {
        ServerRequest::CommandApproval(p) => {
            let command = p.command.unwrap_or_default();
            let mut input = json!({"command": command, "cwd": p.cwd.unwrap_or_default()});
            // `command` is the full shell wrapper; the actions read better.
            let actions: Vec<&str> = p
                .command_actions
                .iter()
                .map(|a| a.command.as_str())
                .filter(|c| !c.trim().is_empty())
                .collect();
            if !actions.is_empty() {
                input["label"] = json!(actions.join(" && "));
            }
            ask(ReplyKind::Approval, "Bash", input, Some(p.item_id))
        }
        ServerRequest::FileChangeApproval(p) => {
            let changes = files.get(&p.item_id).map_or(&[][..], Vec::as_slice);
            let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
            let listed: Vec<Value> = changes
                .iter()
                .map(|c| json!({"path": c.path, "diff": c.diff}))
                .collect();
            let input = json!({"path": paths.join(", "), "changes": listed});
            ask(ReplyKind::Approval, "Edit", input, Some(p.item_id))
        }
        ServerRequest::PermissionsApproval(p) => {
            let cwd = real_cwd(cwd);
            let granted = clamp(&p.permissions, &cwd);
            if granted.as_object().is_none_or(|g| g.is_empty()) {
                return Mapped::Declined {
                    reply: ReplyKind::Permissions(granted).reply(Answer::No),
                    note: OUTSIDE_WORKTREE.into(),
                };
            }
            // What is really asked, never the model's reason alone.
            let label = describe(&p.permissions, p.reason.as_deref(), &cwd);
            let input = json!({
                "label": label,
                "cwd": p.cwd.unwrap_or_default(),
                "permissions": p.permissions,
            });
            ask(
                ReplyKind::Permissions(granted),
                "Permissions",
                input,
                Some(p.item_id),
            )
        }
        ServerRequest::McpElicitation(p) if crew_wired && p.server_name == CREW_SERVER => {
            Mapped::Reply(ReplyKind::Elicitation.reply(Answer::Yes))
        }
        ServerRequest::McpElicitation(p) => {
            let input = p
                .meta
                .as_ref()
                .and_then(|m| m.get("tool_params"))
                .cloned()
                .unwrap_or_else(|| json!({"message": p.message}));
            ask(ReplyKind::Elicitation, &elicited_tool(&p), input, None)
        }
        // Includes the legacy v1 approvals, which a v2 client never gets,
        // and requests about sign-in, which Crew never answers.
        ServerRequest::Raw(_) => Mapped::Refuse(format!("Crew doesn't handle {method}.")),
    }
}

fn ask(kind: ReplyKind, tool: &str, input: Value, item: Option<String>) -> Mapped {
    Mapped::Ask(AskFor {
        kind,
        tool: tool.to_string(),
        input,
        item,
    })
}

/// `mcp__<server>__<tool>` for an MCP tool call ask, read from its
/// message (`... run tool "name"?`). Just the server if it names none.
fn elicited_tool(p: &ElicitationParams) -> String {
    let tool = p
        .message
        .split_once("tool \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty());
    match tool {
        Some(tool) => format!("mcp__{}__{tool}", p.server_name),
        None => format!("mcp__{}", p.server_name),
    }
}

/// Said in the thread when an access ask is declined without asking you.
const OUTSIDE_WORKTREE: &str = "Declined access outside the worktree.";
/// Paths shown in an access ask before "+N more".
const SHOWN_PATHS: usize = 3;
/// Longest path shown in an access ask; longer ones are cut in the middle.
const SHOWN_PATH_CHARS: usize = 40;
/// Longest reason shown in an access ask, which always comes last.
const SHOWN_REASON_CHARS: usize = 80;

/// The real folder of a run: links resolved, if it exists.
fn real_cwd(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical(cwd, ""))
}

/// `path` against `cwd` if relative, with `.` and `..` worked out on the
/// text alone.
fn lexical(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in cwd.join(path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Where `path` really is: the deepest part of it that exists with its
/// links resolved, then the rest. `None` for a link that can't be
/// resolved, such as one to a missing place, as it could point anywhere.
fn resolve(path: &str, cwd: &Path) -> Option<PathBuf> {
    let abs = lexical(cwd, path);
    let existing = abs.ancestors().find(|a| a.symlink_metadata().is_ok())?;
    let real = std::fs::canonicalize(existing).ok()?;
    let rest = abs.strip_prefix(existing).ok()?;
    Some(real.join(rest))
}

/// `path` resolved, if that is inside `cwd`, a real folder.
fn inside(path: &str, cwd: &Path) -> Option<PathBuf> {
    resolve(path, cwd).filter(|real| real.starts_with(cwd))
}

/// Text from the agent, safe to show on one line: no control characters
/// and no marks that flip the reading direction.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// `text` cut to `max` characters, keeping its start and end.
fn middle_cut(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(3);
    let head: String = chars[..keep / 2].iter().collect();
    let tail: String = chars[chars.len() - (keep - keep / 2)..].iter().collect();
    format!("{head}...{tail}")
}

/// A path as shown: where it really is, relative to `cwd` when inside it,
/// cleaned and cut to length.
fn shown(path: &str, cwd: &Path) -> String {
    let real = resolve(path, cwd).unwrap_or_else(|| lexical(cwd, path));
    let text = match real.strip_prefix(cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => real.to_string_lossy().into_owned(),
    };
    middle_cut(&clean(&text), SHOWN_PATH_CHARS)
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A sandbox entry's path as shown: a path, a glob or a special place.
fn entry_shown(entry: &Value, cwd: &Path) -> String {
    let path = &entry["path"];
    match path["type"].as_str() {
        Some("path") => shown(path["path"].as_str().unwrap_or_default(), cwd),
        Some("glob_pattern") => middle_cut(
            &clean(path["pattern"].as_str().unwrap_or_default()),
            SHOWN_PATH_CHARS,
        ),
        Some("special") => match path["value"]["kind"].as_str() {
            Some("root") => "/".into(),
            Some(kind) => clean(kind),
            None => "?".into(),
        },
        _ => "?".into(),
    }
}

fn network_asked(asked: &Value) -> bool {
    asked["network"]["enabled"] == json!(true)
}

/// What an access ask wants, such as `network, write gen, /`, then the
/// agent's reason. Network and writes come first and paths are short, so
/// nothing that matters is ever cut off.
fn describe(asked: &Value, reason: Option<&str>, cwd: &Path) -> String {
    let fs = &asked["fileSystem"];
    let mut write: Vec<String> = strings(&fs["write"])
        .iter()
        .map(|p| shown(p, cwd))
        .collect();
    let mut read: Vec<String> = strings(&fs["read"]).iter().map(|p| shown(p, cwd)).collect();
    for entry in fs["entries"].as_array().into_iter().flatten() {
        match entry["access"].as_str() {
            Some("write") => write.push(entry_shown(entry, cwd)),
            Some("read") => read.push(entry_shown(entry, cwd)),
            _ => {}
        }
    }
    let list = |verb: &str, paths: Vec<String>| {
        if paths.is_empty() {
            return None;
        }
        let more = paths.len().saturating_sub(SHOWN_PATHS);
        let mut text = format!(
            "{verb} {}",
            paths[..paths.len().min(SHOWN_PATHS)].join(", ")
        );
        if more > 0 {
            text.push_str(&format!(" +{more} more"));
        }
        Some(text)
    };
    let parts: Vec<String> = [
        network_asked(asked).then(|| "network".to_string()),
        list("write", write),
        list("read", read),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut label = if parts.is_empty() {
        "more access".into()
    } else {
        parts.join(", ")
    };
    let reason = reason.map(clean).filter(|r| !r.trim().is_empty());
    if let Some(reason) = reason {
        let reason: String = reason.trim().chars().collect();
        let cut: String = reason.chars().take(SHOWN_REASON_CHARS).collect();
        let more = if cut.len() < reason.len() { "..." } else { "" };
        label = format!("{label} · {cut}{more}");
    }
    label
}

/// What may be granted of `asked`: write and read access only to paths
/// that really are inside `cwd` (links resolved), as those resolved
/// absolute paths, and network only if asked for. Reads are held to the
/// same folder: the sandbox reads it already, and anything else would need
/// its own ruling. Globs and special places can't be checked, so they are
/// never granted. Empty if nothing remains.
fn clamp(asked: &Value, cwd: &Path) -> Value {
    let fs = &asked["fileSystem"];
    let keep = |list: &Value| -> Vec<String> {
        strings(list)
            .into_iter()
            .filter_map(|p| inside(p, cwd))
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    };
    let mut granted_fs = serde_json::Map::new();
    for key in ["read", "write"] {
        let paths = keep(&fs[key]);
        if !paths.is_empty() {
            granted_fs.insert(key.into(), json!(paths));
        }
    }
    let entries: Vec<Value> = fs["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| matches!(e["access"].as_str(), Some("read" | "write")))
        .filter(|e| e["path"]["type"] == "path")
        .filter_map(|e| {
            let real = inside(e["path"]["path"].as_str()?, cwd)?;
            Some(json!({
                "path": {"type": "path", "path": real.to_string_lossy()},
                "access": e["access"],
            }))
        })
        .collect();
    if !entries.is_empty() {
        granted_fs.insert("entries".into(), json!(entries));
    }
    let mut granted = serde_json::Map::new();
    if network_asked(asked) {
        granted.insert("network".into(), json!({"enabled": true}));
    }
    if !granted_fs.is_empty() {
        granted.insert("fileSystem".into(), Value::Object(granted_fs));
    }
    Value::Object(granted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(method: &str, params: Value, wired: bool) -> Mapped {
        map_request(
            method,
            &params,
            &HashMap::new(),
            wired,
            Path::new("/work/demo"),
        )
    }

    fn asked(m: Mapped) -> AskFor {
        match m {
            Mapped::Ask(a) => a,
            other => panic!("not an ask: {other:?}"),
        }
    }

    #[test]
    fn commands_ask_with_a_short_label() {
        let a = asked(map(
            "item/commandExecution/requestApproval",
            json!({"itemId": "exec-1", "command": "/bin/zsh -lc 'mkdir sub'", "cwd": "/work/demo",
                   "commandActions": [{"type": "unknown", "command": "mkdir sub"}]}),
            true,
        ));
        assert_eq!(a.tool, "Bash");
        assert_eq!(a.item.as_deref(), Some("exec-1"));
        assert_eq!(
            a.input,
            json!({"command": "/bin/zsh -lc 'mkdir sub'", "cwd": "/work/demo", "label": "mkdir sub"})
        );
        assert_eq!(a.kind.reply(Answer::Yes), json!({"decision": "accept"}));
        assert_eq!(a.kind.reply(Answer::No), json!({"decision": "decline"}));
        assert_eq!(a.kind.reply(Answer::Cancel), json!({"decision": "cancel"}));
    }

    #[test]
    fn file_changes_list_the_started_items_changes() {
        let mut files = HashMap::new();
        files.insert(
            "exec-9".to_string(),
            vec![
                FileChange {
                    path: "/work/demo/a.md".into(),
                    kind: json!({"type": "add"}),
                    diff: "a\n".into(),
                },
                FileChange {
                    path: "/work/demo/b.md".into(),
                    kind: json!({"type": "update"}),
                    diff: "-b\n+c\n".into(),
                },
            ],
        );
        let m = map_request(
            "item/fileChange/requestApproval",
            &json!({"itemId": "exec-9"}),
            &files,
            true,
            Path::new("/work/demo"),
        );
        let a = asked(m);
        assert_eq!(a.tool, "Edit");
        assert_eq!(a.input["path"], "/work/demo/a.md, /work/demo/b.md");
        assert_eq!(a.input["changes"][1]["diff"], "-b\n+c\n");
    }

    #[test]
    fn crew_elicitations_pass_only_when_crew_is_wired() {
        let params = json!({"serverName": "crew", "mode": "form",
                            "message": "Allow the crew MCP server to run tool \"handoff\"?"});
        assert_eq!(
            map("mcpServer/elicitation/request", params.clone(), true),
            Mapped::Reply(json!({"action": "accept", "content": {}, "_meta": null}))
        );
        // Without wiring, a server named crew is the user's own.
        let a = asked(map("mcpServer/elicitation/request", params, false));
        assert_eq!(a.tool, "mcp__crew__handoff");
        assert_eq!(
            a.input,
            json!({"message": "Allow the crew MCP server to run tool \"handoff\"?"})
        );
        assert_eq!(
            a.kind.reply(Answer::No),
            json!({"action": "decline", "content": null, "_meta": null})
        );
    }

    #[test]
    fn elicited_tools_are_read_from_the_message() {
        let p = |server: &str, message: &str| ElicitationParams {
            server_name: server.into(),
            message: message.into(),
            ..Default::default()
        };
        assert_eq!(
            elicited_tool(&p(
                "github",
                "Allow the github MCP server to run tool \"create_issue\"?"
            )),
            "mcp__github__create_issue"
        );
        assert_eq!(elicited_tool(&p("docs", "Pick a file")), "mcp__docs");
    }

    fn permissions(permissions: Value, reason: Option<&str>) -> Mapped {
        map(
            "item/permissions/requestApproval",
            json!({"itemId": "i1", "cwd": "/work/demo", "reason": reason, "permissions": permissions}),
            true,
        )
    }

    #[test]
    fn permission_asks_show_what_is_asked_and_grant_only_the_worktree() {
        let a = asked(permissions(
            json!({"network": {"enabled": true},
                   "fileSystem": {"read": null, "write": ["/work/demo/gen", "/", "/work/demo/../etc"]}}),
            Some("build output"),
        ));
        assert_eq!(a.tool, "Permissions");
        assert_eq!(
            a.input["label"],
            "network, write gen, /, /work/etc · build output"
        );
        assert_eq!(
            a.kind.reply(Answer::Yes),
            json!({"permissions": {"network": {"enabled": true},
                                   "fileSystem": {"write": ["/work/demo/gen"]}},
                   "scope": "turn"})
        );
        for no in [Answer::No, Answer::Cancel] {
            assert_eq!(
                a.kind.reply(no),
                json!({"permissions": {}, "scope": "turn"})
            );
        }
    }

    #[test]
    fn long_path_lists_are_cut_and_relative_paths_are_in_the_worktree() {
        let a = asked(permissions(
            json!({"fileSystem": {"write": ["a", "b", "/work/demo/c", "d", "e"]}}),
            None,
        ));
        assert_eq!(a.input["label"], "write a, b, c +2 more");
        let granted = a.kind.reply(Answer::Yes);
        assert_eq!(
            granted["permissions"]["fileSystem"]["write"],
            json!([
                "/work/demo/a",
                "/work/demo/b",
                "/work/demo/c",
                "/work/demo/d",
                "/work/demo/e"
            ])
        );
    }

    #[test]
    fn asks_for_nothing_inside_the_worktree_are_declined_at_once() {
        for asked_for in [
            json!({"fileSystem": {"write": ["/etc"]}, "network": null}),
            json!({"fileSystem": {"entries": [{"path": {"type": "special", "value": {"kind": "root"}}, "access": "write"}]}}),
            json!({"network": {"enabled": false}}),
        ] {
            assert_eq!(
                permissions(asked_for, Some("please")),
                Mapped::Declined {
                    reply: json!({"permissions": {}, "scope": "turn"}),
                    note: "Declined access outside the worktree.".into(),
                }
            );
        }
    }

    #[test]
    fn sandbox_entries_inside_the_worktree_are_kept() {
        let a = asked(permissions(
            json!({"fileSystem": {"entries": [
                {"path": {"type": "path", "path": "/work/demo/out"}, "access": "write"},
                {"path": {"type": "glob_pattern", "pattern": "**/*.rs"}, "access": "write"},
                {"path": {"type": "special", "value": {"kind": "tmpdir"}}, "access": "read"}
            ]}}),
            None,
        ));
        assert_eq!(a.input["label"], "write out, **/*.rs, read tmpdir");
        assert_eq!(
            a.kind.reply(Answer::Yes)["permissions"],
            json!({"fileSystem": {"entries": [
                {"path": {"type": "path", "path": "/work/demo/out"}, "access": "write"}
            ]}})
        );
        // Entries are granted as clean absolute paths, like the lists.
        let a = asked(permissions(
            json!({"fileSystem": {"entries": [
                {"path": {"type": "path", "path": "sub/../out2"}, "access": "write"}
            ]}}),
            None,
        ));
        assert_eq!(
            a.kind.reply(Answer::Yes)["permissions"]["fileSystem"]["entries"],
            json!([{"path": {"type": "path", "path": "/work/demo/out2"}, "access": "write"}])
        );
    }

    #[test]
    fn labels_drop_control_characters_and_keep_network_first() {
        let a = asked(permissions(
            json!({"network": {"enabled": true}, "fileSystem": {"write": ["gen\n\u{202e}x"]}}),
            Some("line one\nline two\t\u{2066}end"),
        ));
        assert_eq!(
            a.input["label"],
            "network, write genx · line oneline twoend"
        );
        // Each path is cut in the middle, the list never as a whole.
        let long = format!("/work/demo/{}/end", "d".repeat(100));
        let a = asked(permissions(
            json!({"network": {"enabled": true}, "fileSystem": {"write": [long, long, long, long]}}),
            Some(&"why ".repeat(50)),
        ));
        let label = a.input["label"].as_str().unwrap();
        assert!(label.starts_with("network, write "), "{label}");
        assert!(label.contains(" +1 more"), "{label}");
        assert!(label.contains("..."), "{label}");
        assert!(label.matches("/end").count() == 3, "{label}");
        assert!(label.len() < 260, "{} {label}", label.len());
    }

    #[test]
    fn links_out_of_the_worktree_are_not_granted() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(cwd.join("gen")).unwrap();
        std::os::unix::fs::symlink("/", cwd.join("escape")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/crew-test", cwd.join("dangling")).unwrap();
        let ask = |write: Value| {
            map_request(
                "item/permissions/requestApproval",
                &json!({"itemId": "i", "permissions": {"fileSystem": {"write": write}}}),
                &HashMap::new(),
                true,
                &cwd,
            )
        };
        for escaping in [
            json!(["escape"]),
            json!(["escape/etc"]),
            json!(["dangling/x"]),
        ] {
            assert!(
                matches!(ask(escaping.clone()), Mapped::Declined { .. }),
                "{escaping}"
            );
        }
        let a = asked(ask(json!(["gen/out", "escape/crew-none"])));
        assert_eq!(a.input["label"], "write gen/out, /crew-none");
        assert_eq!(
            a.kind.reply(Answer::Yes)["permissions"]["fileSystem"]["write"],
            json!([cwd.join("gen/out")])
        );
    }

    #[test]
    fn other_requests_are_refused() {
        assert_eq!(
            map("account/chatgptAuthTokens/refresh", json!({}), true),
            Mapped::Refuse("Crew doesn't handle account/chatgptAuthTokens/refresh.".into())
        );
    }
}
