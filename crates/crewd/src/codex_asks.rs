//! What crewd does with each request a `codex app-server` sends: answer
//! it at once, refuse it, or put it to you as a Crew permission approval.
//! Pure, so the mapping is tested on its own.

use std::collections::HashMap;

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
    /// A grant of the asked permissions, or of none. Holds what was asked.
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
            ReplyKind::Permissions(asked) => {
                let granted = match answer {
                    Answer::Yes => without_nulls(asked),
                    Answer::No | Answer::Cancel => json!({}),
                };
                Ok(json!({"permissions": granted, "scope": "turn"}))
            }
        };
        value.expect("replies serialize")
    }
}

/// Maps a request. `files` holds the changes of started file change
/// items; `crew_wired` says whether the `crew` MCP server is Crew's own.
pub(crate) fn map_request(
    method: &str,
    params: &Value,
    files: &HashMap<String, Vec<FileChange>>,
    crew_wired: bool,
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
            let label = p
                .reason
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "more access than its sandbox gives".into());
            let input = json!({
                "label": label,
                "cwd": p.cwd.unwrap_or_default(),
                "permissions": p.permissions,
            });
            let kind = ReplyKind::Permissions(p.permissions);
            ask(kind, "Permissions", input, Some(p.item_id))
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

/// The object without its null fields, as a grant leaves out what wasn't
/// asked for.
fn without_nulls(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(method: &str, params: Value, wired: bool) -> Mapped {
        map_request(method, &params, &HashMap::new(), wired)
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

    #[test]
    fn permission_asks_grant_what_was_asked_or_nothing() {
        let asked_for = json!({"network": {"enabled": true}, "fileSystem": null});
        let a = asked(map(
            "item/permissions/requestApproval",
            json!({"itemId": "i1", "cwd": "/work/demo", "reason": "fetch the docs", "permissions": asked_for}),
            true,
        ));
        assert_eq!(a.tool, "Permissions");
        assert_eq!(a.input["label"], "fetch the docs");
        assert_eq!(
            a.kind.reply(Answer::Yes),
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
        );
        for no in [Answer::No, Answer::Cancel] {
            assert_eq!(
                a.kind.reply(no),
                json!({"permissions": {}, "scope": "turn"})
            );
        }
    }

    #[test]
    fn other_requests_are_refused() {
        assert_eq!(
            map("account/chatgptAuthTokens/refresh", json!({}), true),
            Mapped::Refuse("Crew doesn't handle account/chatgptAuthTokens/refresh.".into())
        );
    }
}
