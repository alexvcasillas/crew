//! Wire protocol between crewd and its clients.
//!
//! Each frame is a big-endian `u32` length followed by that many bytes of JSON.

use std::io;

use crew_core::agent::AgentSpec;
use crew_core::event::{Event, RunStatus};
use crew_core::ids::{ChannelId, ProjectId, RunId, ThreadId};
use crew_core::store::StoredMessage;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Version both sides must agree on in `Hello`.
pub const PROTO_VERSION: u32 = 1;
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
