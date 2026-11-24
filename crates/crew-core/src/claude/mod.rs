//! Parsing of Claude Code output and session files, and building of its
//! command line.

pub mod command;
pub mod parse;
pub mod transcript;

pub use command::{
    APPROVE_TOOL, ClaudeInvocation, CrewContext, HANDOFF_TOOL, McpWiring, SessionArg, Teammate,
    build_claude, claude_bin_from,
};
pub use parse::{StreamParser, parse_line};
