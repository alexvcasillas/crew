//! Wire protocol between crewd and its clients.
//!
//! Each frame is a big-endian `u32` length followed by that many bytes of JSON.

use std::io;

use crew_core::agent::AgentSpec;
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{ApprovalId, ChannelId, ProjectId, RunId, ThreadId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::{StoredApproval, StoredMessage};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Version both sides must agree on in `Hello`.
pub const PROTO_VERSION: u32 = 2;
/// Largest frame body accepted, in bytes.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// A message from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Request { id: u64, req: Request },
}

/// What a client asks crewd to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
    },
    CreateProject {
        name: String,
        root: String,
        main_branch: String,
    },
    CreateChannel {
        project: ProjectId,
        name: String,
    },
    CreateAgent {
        spec: AgentSpec,
    },
    CreateThread {
        channel: ChannelId,
        title: String,
    },
    PostMessage {
        thread: ThreadId,
        body: String,
    },
    Subscribe {
        thread: ThreadId,
    },
    StopRun {
        run: RunId,
    },
    StopThread {
        thread: ThreadId,
    },
    StopAll,
    ListRuns {
        status: Option<RunStatus>,
    },
    /// Answered with `Vec<(AgentId, AgentSpec)>`.
    ListAgents,
    /// Answered with `Vec<StoredProject>`.
    ListProjects,
    /// Answered with `Vec<StoredChannel>`.
    ListChannels {
        project: ProjectId,
    },
    /// Answered with `Vec<StoredThread>`.
    ListThreads {
        channel: ChannelId,
    },
    /// Sets what a new worktree of the project gets before its first run.
    UpdateProject {
        project: ProjectId,
        setup_command: Option<String>,
        copy_files: Vec<String>,
    },
    /// Answered with a `Diff` of the uncommitted changes in the thread's
    /// worktree. Empty if the thread has no worktree.
    ThreadDiff {
        thread: ThreadId,
    },
    /// Commits everything in the thread's worktree. Answered with the sha.
    Commit {
        thread: ThreadId,
        message: String,
    },
    /// Removes the thread's worktree, changes included, and its branch
    /// unless the branch has commits beyond the worktree's base.
    Discard {
        thread: ThreadId,
    },
    /// From crew-mcp: the agent of `run` hands work to `agent`. Answered
    /// with a text for the agent.
    Handoff {
        run: RunId,
        agent: String,
        brief: String,
    },
    /// From crew-mcp: the agent of `run` wants to use `tool`. Answered with
    /// a [`PermissionAnswer`] once someone decides.
    PermissionAsk {
        run: RunId,
        tool: String,
        input: serde_json::Value,
        /// Claude's id for the tool call, if it sent one.
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    /// Answered with the pending `Vec<StoredApproval>`, for one thread or
    /// all of them.
    ListApprovals {
        thread: Option<ThreadId>,
    },
    /// Approves or declines a pending approval. `brief` replaces a
    /// handoff's brief.
    Decide {
        approval: ApprovalId,
        approve: bool,
        brief: Option<String>,
    },
    /// Lets `n` more handoffs run in the thread on their own.
    AllowMoreHandoffs {
        thread: ThreadId,
        n: u32,
    },
    /// Sets the thread's own handoff policy and budget. `None` falls back
    /// to the agent or channel.
    SetThreadHandoff {
        thread: ThreadId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    },
    /// Sets the channel's handoff policy and budget. `None` falls back to
    /// the global default.
    SetChannelHandoff {
        channel: ChannelId,
        policy: Option<HandoffPolicy>,
        budget: Option<u32>,
    },
}

/// The answer to [`Request::PermissionAsk`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionAnswer {
    pub allow: bool,
    /// Why it was denied, shown to the agent.
    pub message: Option<String>,
}

/// A message from crewd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Answer to the request with the same id.
    Response {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    /// Something a run did, with its sequence number in that run.
    Event {
        thread: ThreadId,
        run: RunId,
        seq: u64,
        event: Event,
    },
    /// A run changed state.
    RunStatus {
        thread: ThreadId,
        run: RunId,
        status: RunStatus,
    },
    /// A message was added to a thread.
    Message {
        thread: ThreadId,
        message: StoredMessage,
    },
    /// An approval was asked for or decided.
    Approval {
        thread: ThreadId,
        approval: StoredApproval,
    },
    /// This connection fell behind and missed notifications. crewd closes
    /// it next; connect and subscribe again to catch up.
    Resync,
}

/// Writes one frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let body = serde_json::to_vec(v)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Reads one frame. Returns `None` on a clean end of input before a frame.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        let n = r.read(&mut len[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        got += n;
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes is over the {MAX_FRAME} byte limit"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let msgs = vec![
            ClientMsg::Request {
                id: 1,
                req: Request::Hello {
                    version: PROTO_VERSION,
                },
            },
            ClientMsg::Request {
                id: 2,
                req: Request::StopAll,
            },
            ClientMsg::Request {
                id: 3,
                req: Request::ListRuns {
                    status: Some(RunStatus::Working),
                },
            },
            ClientMsg::Request {
                id: 4,
                req: Request::Handoff {
                    run: RunId::new(),
                    agent: "Sentinel".into(),
                    brief: "check the edit".into(),
                },
            },
            ClientMsg::Request {
                id: 5,
                req: Request::PermissionAsk {
                    run: RunId::new(),
                    tool: "Bash".into(),
                    input: serde_json::json!({ "command": "touch x.txt" }),
                    tool_use_id: Some("toolu_1".into()),
                },
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m).await.unwrap();
        }
        let first_len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        let first: serde_json::Value = serde_json::from_slice(&buf[4..4 + first_len]).unwrap();
        assert_eq!(first["type"], "request");

        let mut r = buf.as_slice();
        for m in &msgs {
            let got: ClientMsg = read_frame(&mut r).await.unwrap().unwrap();
            assert_eq!(&got, m);
        }
        assert!(read_frame::<_, ClientMsg>(&mut r).await.unwrap().is_none());

        let reply = ServerMsg::Response {
            id: 9,
            result: Err("nope".into()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &reply).await.unwrap();
        let got: ServerMsg = read_frame(&mut buf.as_slice()).await.unwrap().unwrap();
        assert_eq!(got, reply);
    }

    #[test]
    fn permission_answer_json_shape() {
        let a: PermissionAnswer =
            serde_json::from_str(r#"{"allow":false,"message":"no"}"#).unwrap();
        assert_eq!(
            a,
            PermissionAnswer {
                allow: false,
                message: Some("no".into())
            }
        );
        let a: PermissionAnswer = serde_json::from_str(r#"{"allow":true,"message":null}"#).unwrap();
        assert!(a.allow);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let mut buf = (MAX_FRAME + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let err = read_frame::<_, serde_json::Value>(&mut buf.as_slice())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let empty: &[u8] = &[];
        let got = read_frame::<_, serde_json::Value>(&mut &*empty)
            .await
            .unwrap();
        assert!(got.is_none());

        // A cut-off length is an error, not a clean end.
        let partial: &[u8] = &[0, 0];
        let err = read_frame::<_, serde_json::Value>(&mut &*partial)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
