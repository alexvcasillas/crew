//! Parsing of Claude Code output and building of its command line.

pub mod command;
pub mod parse;

pub use command::{
    APPROVE_TOOL, ClaudeInvocation, HANDOFF_TOOL, McpWiring, SessionArg, build_claude,
    claude_bin_from,
};
pub use parse::{StreamParser, parse_line};
