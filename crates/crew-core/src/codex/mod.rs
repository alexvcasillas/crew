//! Talking to `codex app-server` over stdio.
//!
//! The server speaks JSON-RPC style messages, one JSON object per line, with no
//! `Content-Length` headers and no `"jsonrpc"` field. [`proto`] models the few
//! messages Crew uses; [`client`] runs the connection.

pub mod client;
pub mod events;
pub mod proto;

pub use client::{CodexConn, CodexError, Incoming, RequestId};
pub use events::CodexTranslator;
