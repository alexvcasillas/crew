//! Parsing of Claude Code output and building of its command line.

pub mod command;
pub mod parse;

pub use command::{ClaudeInvocation, SessionArg, build_claude, claude_bin_from};
pub use parse::parse_line;
