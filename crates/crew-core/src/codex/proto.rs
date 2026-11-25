//! Message types for the parts of the `codex app-server` protocol Crew uses.
//!
//! Incoming structs default every field, so new or missing fields never break
//! parsing. Messages Crew does not model, or cannot read, stay as raw JSON.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Method names.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const MODEL_LIST: &str = "model/list";
    pub const SKILLS_LIST: &str = "skills/list";
    pub const THREAD_START: &str = "thread/start";
    pub const THREAD_RESUME: &str = "thread/resume";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";

    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const TURN_DIFF_UPDATED: &str = "turn/diff/updated";
    pub const TURN_COMPLETED: &str = "turn/completed";

    pub const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
    pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
    pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
    pub const MCP_ELICITATION: &str = "mcpServer/elicitation/request";
}

// ---- Client requests ----

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: Option<Capabilities>,
}

impl InitializeParams {
    /// The params Crew sends: experimental API on, attestation off.
    pub fn crew(version: &str) -> Self {
        InitializeParams {
            client_info: ClientInfo {
                name: "crew".into(),
                title: Some("Crew".into()),
                version: version.into(),
            },
            capabilities: Some(Capabilities {
                experimental_api: true,
                request_attestation: false,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub experimental_api: bool,
    pub request_attestation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    /// Free-form map of config.toml keys, applied to this thread only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_turns: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Sandbox for this turn and later ones, in object form, such as
    /// `{"type":"workspaceWrite","writableRoots":[...],...}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    Text {
        text: String,
        text_elements: Vec<Value>,
    },
    /// Runs a skill, whose file is at `path`.
    Skill { name: String, path: String },
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

// ---- Responses ----

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InitializeResponse {
    pub user_agent: String,
    pub codex_home: String,
    pub platform_family: String,
    pub platform_os: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelListResponse {
    pub data: Vec<Model>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub hidden: bool,
    pub is_default: bool,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
    pub description: String,
}

/// Reply to both `thread/start` and `thread/resume`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread: Thread,
    pub model: String,
}

pub type ThreadResumeResponse = ThreadStartResponse;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    /// The rollout file Codex keeps for this thread.
    pub path: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub items: Vec<ThreadItem>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TurnError {
    pub message: String,
}

// ---- Thread items ----

/// An item in a turn. Types Crew does not model stay as raw JSON in `Other`.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadItem {
    AgentMessage(AgentMessageItem),
    CommandExecution(CommandExecutionItem),
    FileChange(FileChangeItem),
    McpToolCall(McpToolCallItem),
    Other(Value),
}

impl<'de> Deserialize<'de> for ThreadItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let item = match v.get("type").and_then(Value::as_str) {
            Some("agentMessage") => typed(&v).map(ThreadItem::AgentMessage),
            Some("commandExecution") => typed(&v).map(ThreadItem::CommandExecution),
            Some("fileChange") => typed(&v).map(ThreadItem::FileChange),
            Some("mcpToolCall") => typed(&v).map(ThreadItem::McpToolCall),
            _ => None,
        };
        Ok(item.unwrap_or(ThreadItem::Other(v)))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentMessageItem {
    pub id: String,
    pub text: String,
    /// `commentary` for progress notes, `final_answer` for the reply.
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CommandExecutionItem {
    pub id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub status: ItemStatus,
    pub command_actions: Vec<CommandAction>,
    pub aggregated_output: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CommandAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FileChangeItem {
    pub id: String,
    pub changes: Vec<FileChange>,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    /// `{"type":"add"}` and friends, kept raw.
    pub kind: Value,
    pub diff: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpToolCallItem {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub status: ItemStatus,
    pub arguments: Value,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    #[default]
    #[serde(other)]
    Unknown,
}

// ---- Server notifications ----

/// A notification from the server. Anything not modelled, or not readable,
/// is `Raw` and holds the whole message.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    ItemStarted(ItemNotification),
    ItemCompleted(ItemNotification),
    AgentMessageDelta(AgentMessageDelta),
    TurnDiffUpdated(TurnDiffUpdated),
    TurnCompleted(TurnCompleted),
    Raw(Value),
}

impl ServerNotification {
    /// Reads a whole notification message (`{"method", "params", ...}`).
    pub fn parse(msg: Value) -> Self {
        let params = msg.get("params").unwrap_or(&Value::Null);
        let parsed = match msg.get("method").and_then(Value::as_str) {
            Some(method::ITEM_STARTED) => typed(params).map(ServerNotification::ItemStarted),
            Some(method::ITEM_COMPLETED) => typed(params).map(ServerNotification::ItemCompleted),
            Some(method::AGENT_MESSAGE_DELTA) => {
                typed(params).map(ServerNotification::AgentMessageDelta)
            }
            Some(method::TURN_DIFF_UPDATED) => {
                typed(params).map(ServerNotification::TurnDiffUpdated)
            }
            Some(method::TURN_COMPLETED) => typed(params).map(ServerNotification::TurnCompleted),
            _ => None,
        };
        parsed.unwrap_or(ServerNotification::Raw(msg))
    }
}

/// Params of `item/started` and `item/completed`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub item: ThreadItem,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentMessageDelta {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

/// The cumulative unified diff of the turn so far.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TurnDiffUpdated {
    pub thread_id: String,
    pub turn_id: String,
    pub diff: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TurnCompleted {
    pub thread_id: String,
    pub turn: Turn,
}

// ---- Server requests ----

/// A request from the server that needs a reply. Unmodelled or unreadable
/// requests are `Raw` and hold the params.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerRequest {
    CommandApproval(CommandApprovalParams),
    FileChangeApproval(FileChangeApprovalParams),
    PermissionsApproval(PermissionsApprovalParams),
    McpElicitation(ElicitationParams),
    Raw(Value),
}

impl ServerRequest {
    pub fn parse(method: &str, params: &Value) -> Self {
        let parsed = match method {
            method::COMMAND_APPROVAL => typed(params).map(ServerRequest::CommandApproval),
            method::FILE_CHANGE_APPROVAL => typed(params).map(ServerRequest::FileChangeApproval),
            method::PERMISSIONS_APPROVAL => typed(params).map(ServerRequest::PermissionsApproval),
            method::MCP_ELICITATION => typed(params).map(ServerRequest::McpElicitation),
            _ => None,
        };
        parsed.unwrap_or_else(|| ServerRequest::Raw(params.clone()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CommandApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    /// Set when several approvals belong to one item.
    pub approval_id: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub command_actions: Vec<CommandAction>,
}

/// The diff is not in the request; it is on the item's `item/started`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub reason: Option<String>,
    pub grant_root: Option<String>,
}

/// The agent asks for more than its sandbox allows, such as network
/// access or more writable folders, for the turn or the session.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PermissionsApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    /// `{"network": ..., "fileSystem": ...}`, each null when not asked for.
    pub permissions: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ElicitationParams {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub server_name: String,
    /// `form`, `url` or a newer mode.
    pub mode: String,
    pub message: String,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
    pub requested_schema: Option<Value>,
    pub url: Option<String>,
}

// ---- Replies to server requests ----

/// Reply to a command or file change approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ApprovalResponse {
    pub decision: ApprovalDecision,
}

impl ApprovalResponse {
    pub fn new(decision: ApprovalDecision) -> Self {
        ApprovalResponse { decision }
    }
}

/// `Decline` lets the turn go on; `Cancel` also ends the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

/// Reply to an MCP elicitation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<Value>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

impl ElicitationResponse {
    pub fn accept() -> Self {
        ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(Value::Object(Default::default())),
            meta: None,
        }
    }

    pub fn decline() -> Self {
        Self::without_content(ElicitationAction::Decline)
    }

    pub fn cancel() -> Self {
        Self::without_content(ElicitationAction::Cancel)
    }

    fn without_content(action: ElicitationAction) -> Self {
        ElicitationResponse {
            action,
            content: None,
            meta: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

fn typed<T: DeserializeOwned>(v: &Value) -> Option<T> {
    T::deserialize(v).ok()
}
