//! The Crew daemon: runs agent processes and serves clients.

pub mod runner;
pub mod server;
pub mod service;

pub use server::{ServeOptions, serve};
