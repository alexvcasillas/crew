//! `crew-mcp`: the MCP server crewd passes to each Claude run.
//!
//! Speaks newline-delimited JSON-RPC on stdio and serves two tools,
//! `handoff` and `approve`. Each call opens its own connection to crewd.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use crew_core::ids::RunId;
use crew_proto::{
    ClientMsg, PROTO_VERSION, PermissionAnswer, Request, ServerMsg, read_frame, write_frame,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use uuid::Uuid;

/// How long reaching crewd and getting past `Hello` may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Protocol version used when the client does not name one.
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const UNREACHABLE: &str = "Crew isn't reachable right now.";
/// Tools of this server, which `approve` lets through without asking.
const OWN_TOOLS: &str = "mcp__crew__";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;

#[derive(Parser)]
#[command(about = "MCP server that connects a Claude run to crewd")]
struct Args {
    /// The run this server belongs to.
    #[arg(long)]
    run: Uuid,
}

#[derive(Clone)]
struct Ctx {
    home: PathBuf,
    run: RunId,
}

fn non_empty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|v| !v.is_empty())
}

fn crew_home() -> Option<PathBuf> {
    non_empty(std::env::var_os("CREW_HOME"))
        .map(PathBuf::from)
        .or_else(|| non_empty(std::env::var_os("HOME")).map(|h| PathBuf::from(h).join(".crew")))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let Some(home) = crew_home() else {
        eprintln!("crew-mcp: Set CREW_HOME or HOME.");
        return ExitCode::FAILURE;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("crew-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(serve(Ctx {
        home,
        run: RunId(args.run),
    })) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crew-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Reads requests from stdin until it closes. Tool calls run in their own
/// tasks, since `approve` can wait a long time for a person.
async fn serve(ctx: Ctx) -> std::io::Result<()> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let _ = out_tx.send(error(Value::Null, PARSE_ERROR, &e.to_string()));
                continue;
            }
        };
        // Without an id it is a notification, which gets no reply.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL);
                let _ = out_tx.send(result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "crew", "version": env!("CARGO_PKG_VERSION")},
                    }),
                ));
            }
            "ping" => {
                let _ = out_tx.send(result(id, json!({})));
            }
            "tools/list" => {
                let _ = out_tx.send(result(id, json!({ "tools": tools() })));
            }
            "tools/call" => {
                let ctx = ctx.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let _ = out_tx.send(call_tool(&ctx, id, &params).await);
                });
            }
            other => {
                let _ = out_tx.send(error(id, METHOD_NOT_FOUND, &format!("No method {other}.")));
            }
        }
    }
    // stdin closed: Claude is gone, so pending calls don't matter.
    writer.abort();
    Ok(())
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn text(id: Value, text: &str, is_error: bool) -> Value {
    result(
        id,
        json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
    )
}

fn tools() -> Value {
    json!([
        {
            "name": "handoff",
            "description": "Hand work to another agent in this thread. Name the agent and give it a short brief of what to do.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Name of the agent to hand off to."},
                    "brief": {"type": "string", "description": "What the agent should do."},
                },
                "required": ["agent", "brief"],
            },
        },
        {
            "name": "approve",
            "description": "Asks the people in Crew whether a tool may run. Used by Claude Code as the permission prompt tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool_name": {"type": "string"},
                    "input": {"type": "object"},
                    "tool_use_id": {"type": "string"},
                },
                "required": ["tool_name", "input"],
            },
        },
    ])
}

async fn call_tool(ctx: &Ctx, id: Value, params: &Value) -> Value {
    let args = &params["arguments"];
    let str_arg = |name: &str| args.get(name).and_then(Value::as_str).map(String::from);
    match params.get("name").and_then(Value::as_str) {
        Some("handoff") => {
            let (Some(agent), Some(brief)) = (str_arg("agent"), str_arg("brief")) else {
                return text(id, "handoff needs an agent and a brief.", true);
            };
            let req = Request::Handoff {
                run: ctx.run,
                agent,
                brief,
            };
            match ask(&ctx.home, req).await {
                Ok(Value::String(s)) => text(id, &s, false),
                Ok(other) => text(id, &other.to_string(), false),
                Err(e) => text(id, &e, true),
            }
        }
        Some("approve") => {
            // Always a decision, never a tool error, so Claude gets a clean
            // answer even when Crew can't give one.
            let decision = approve(ctx, args).await;
            text(id, &decision.to_string(), false)
        }
        Some(other) => error(id, INVALID_PARAMS, &format!("No tool {other}.")),
        None => error(id, INVALID_PARAMS, "tools/call needs a name."),
    }
}

fn deny(message: &str) -> Value {
    json!({"behavior": "deny", "message": message})
}

/// Decides one permission ask. Crew's own tools are allowed without asking.
async fn approve(ctx: &Ctx, args: &Value) -> Value {
    let Some(tool) = args.get("tool_name").and_then(Value::as_str) else {
        return deny("approve needs a tool_name.");
    };
    let input = args.get("input").cloned().unwrap_or_else(|| json!({}));
    if tool.starts_with(OWN_TOOLS) {
        return json!({"behavior": "allow", "updatedInput": input});
    }
    let req = Request::PermissionAsk {
        run: ctx.run,
        tool: tool.to_string(),
        input: input.clone(),
    };
    let answer = match ask(&ctx.home, req).await {
        Ok(v) => v,
        Err(e) => return deny(&e),
    };
    match serde_json::from_value::<PermissionAnswer>(answer) {
        Ok(PermissionAnswer { allow: true, .. }) => {
            json!({"behavior": "allow", "updatedInput": input})
        }
        Ok(PermissionAnswer {
            allow: false,
            message,
        }) => deny(message.as_deref().unwrap_or("Denied in Crew.")),
        Err(_) => deny("Crew sent an answer I can't read."),
    }
}

/// Sends one request to crewd on a fresh connection and waits for the
/// answer. Only reaching crewd has a time limit.
async fn ask(home: &Path, req: Request) -> Result<Value, String> {
    let connect = async {
        let mut stream = UnixStream::connect(home.join("crewd.sock")).await.ok()?;
        let hello = Request::Hello {
            version: PROTO_VERSION,
        };
        match exchange(&mut stream, 1, hello).await? {
            Ok(_) => Some(Ok(stream)),
            // A version mismatch: crewd says what to do about it.
            Err(e) => Some(Err(e)),
        }
    };
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Some(Ok(stream))) => stream,
        Ok(Some(Err(e))) => return Err(e),
        Ok(None) | Err(_) => return Err(UNREACHABLE.into()),
    };
    exchange(&mut stream, 2, req)
        .await
        .unwrap_or_else(|| Err(UNREACHABLE.into()))
}

/// Writes a request and reads until its response. `None` if the connection
/// fails.
async fn exchange(stream: &mut UnixStream, id: u64, req: Request) -> Option<Result<Value, String>> {
    write_frame(stream, &ClientMsg::Request { id, req })
        .await
        .ok()?;
    loop {
        match read_frame::<_, ServerMsg>(stream).await.ok()?? {
            ServerMsg::Response { id: got, result } if got == id => return Some(result),
            _ => {}
        }
    }
}
