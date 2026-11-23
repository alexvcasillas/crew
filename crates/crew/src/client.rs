//! A small blocking-style client for the crewd socket.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

/// The socket is missing or refuses the connection.
#[derive(Debug)]
pub struct NotRunning;

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("crewd isn't running. Start it with `crewd`.")
    }
}

impl std::error::Error for NotRunning {}

/// One connection to crewd, already past the `Hello` handshake.
pub struct Client {
    stream: UnixStream,
    next_id: u64,
    /// Notifications that arrived while waiting for a response.
    pending: Vec<ServerMsg>,
}

impl Client {
    /// Connects to `<home>/crewd.sock` and says hello.
    pub async fn connect(home: &Path) -> Result<Client> {
        let stream = match UnixStream::connect(home.join("crewd.sock")).await {
            Ok(stream) => stream,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                return Err(NotRunning.into());
            }
            Err(e) => return Err(anyhow::Error::new(e).context("Can't connect to crewd")),
        };
        let mut client = Client {
            stream,
            next_id: 1,
            pending: Vec::new(),
        };
        client
            .call(Request::Hello {
                version: PROTO_VERSION,
            })
            .await?;
        Ok(client)
    }

    /// Sends a request and waits for its response.
    pub async fn call(&mut self, req: Request) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, req }).await?;
        loop {
            match read_frame::<_, ServerMsg>(&mut self.stream).await? {
                Some(ServerMsg::Response { id: got, result }) if got == id => {
                    return result.map_err(|e| anyhow!(e));
                }
                Some(other) => self.pending.push(other),
                None => bail!("crewd closed the connection."),
            }
        }
    }

    /// Like `call`, decoding the answer.
    pub async fn ask<T: DeserializeOwned>(&mut self, req: Request) -> Result<T> {
        Ok(serde_json::from_value(self.call(req).await?)?)
    }

    /// How many notifications are waiting unread. Crewd writes a thread's
    /// whole history before it answers the next request, so asking anything
    /// after `Subscribe` leaves exactly the history here.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// The next notification, or `None` when crewd hangs up.
    pub async fn next(&mut self) -> Result<Option<ServerMsg>> {
        if !self.pending.is_empty() {
            return Ok(Some(self.pending.remove(0)));
        }
        Ok(read_frame(&mut self.stream).await?)
    }
}
