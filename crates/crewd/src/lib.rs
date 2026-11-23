//! The Crew daemon: runs agent processes and serves clients.

mod approvals;
pub mod runner;
pub mod server;
pub mod service;

pub use server::{ServeOptions, serve, serve_until};
