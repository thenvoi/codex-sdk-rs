use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::CodexClient;
use crate::client::StdioConfig;
use crate::client::WsConfig;
use crate::client::{WsServerHandle, WsStartConfig};
use crate::error::ClientError;
use crate::events::{ServerEvent, ServerNotification};
use crate::protocol::shared::EmptyObject;
use crate::protocol::{requests, responses};
use crate::schema::OpenAiSerializable;

const THREAD_LIST_PAGE_LIMIT: u32 = 100;
const MAX_THREAD_LIST_PAGES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Never,
    OnRequest,
    OnFailure,
    Untrusted,
}

impl ApprovalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnRequest => "on-request",
            Self::OnFailure => "on-failure",
            Self::Untrusted => "untrusted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ModelReasoningEffort {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelReasoningSummary {
    None,
    Auto,
    Concise,
    Detailed,
}

impl ModelReasoningSummary {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Personality {
    None,
    Friendly,
    Pragmatic,
}

impl Personality {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Friendly => "friendly",
            Self::Pragmatic => "pragmatic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebSearchMode {
    Disabled,
    Cached,
    Live,
}

impl WebSearchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Cached => "cached",
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollaborationModeKind {
    Plan,
    Default,
}

impl CollaborationModeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollaborationModeSettings {
    pub model: String,
    pub reasoning_effort: Option<ModelReasoningEffort>,
    pub developer_instructions: Option<String>,
}

impl CollaborationModeSettings {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            reasoning_effort: None,
            developer_instructions: None,
        }
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: ModelReasoningEffort) -> Self {
        self.reasoning_effort = Some(reasoning_effort);
        self
    }

    pub fn with_developer_instructions(
        mut self,
        developer_instructions: impl Into<String>,
    ) -> Self {
        self.developer_instructions = Some(developer_instructions.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollaborationMode {
    pub mode: CollaborationModeKind,
    pub settings: CollaborationModeSettings,
}

impl CollaborationMode {
    pub fn new(mode: CollaborationModeKind, settings: CollaborationModeSettings) -> Self {
        Self { mode, settings }
    }

    fn as_value(&self) -> Value {
        let mut settings = Map::new();
        settings.insert(
            "model".to_string(),
            Value::String(self.settings.model.clone()),
        );
        if let Some(reasoning_effort) = self.settings.reasoning_effort {
            settings.insert(
                "reasoning_effort".to_string(),
                Value::String(reasoning_effort.as_str().to_string()),
            );
        }
        if let Some(instructions) = &self.settings.developer_instructions {
            settings.insert(
                "developer_instructions".to_string(),
                Value::String(instructions.clone()),
            );
        }

        let mut value = Map::new();
        value.insert(
            "mode".to_string(),
            Value::String(self.mode.as_str().to_string()),
        );
        value.insert("settings".to_string(), Value::Object(settings));
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl DynamicToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    fn as_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("name".to_string(), Value::String(self.name.clone()));
        value.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        value.insert("inputSchema".to_string(), self.input_schema.clone());
        Value::Object(value)
    }
}

/// SDK-level resume target for selecting either the latest recorded thread or a specific thread id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeThread {
    Latest,
    ById(String),
}

impl From<String> for ResumeThread {
    fn from(value: String) -> Self {
        Self::ById(value)
    }
}

impl From<&str> for ResumeThread {
    fn from(value: &str) -> Self {
        Self::ById(value.to_string())
    }
}

impl From<&String> for ResumeThread {
    fn from(value: &String) -> Self {
        Self::ById(value.clone())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadOptions {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub sandbox_mode: Option<SandboxMode>,
    pub sandbox_policy: Option<Value>,
    pub working_directory: Option<String>,
    pub skip_git_repo_check: Option<bool>,
    pub model_reasoning_effort: Option<ModelReasoningEffort>,
    pub model_reasoning_summary: Option<ModelReasoningSummary>,
    pub network_access_enabled: Option<bool>,
    pub web_search_mode: Option<WebSearchMode>,
    pub web_search_enabled: Option<bool>,
    pub approval_policy: Option<ApprovalMode>,
    pub additional_directories: Option<Vec<String>>,
    pub personality: Option<Personality>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub ephemeral: Option<bool>,
    pub collaboration_mode: Option<CollaborationMode>,
    pub config: Option<Map<String, Value>>,
    pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
    pub experimental_raw_events: Option<bool>,
    pub persist_extended_history: Option<bool>,
}

impl ThreadOptions {
    pub fn builder() -> ThreadOptionsBuilder {
        ThreadOptionsBuilder::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadOptionsBuilder {
    options: ThreadOptions,
}

impl ThreadOptionsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build(self) -> ThreadOptions {
        self.options
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.options.model = Some(model.into());
        self
    }

    pub fn model_provider(mut self, model_provider: impl Into<String>) -> Self {
        self.options.model_provider = Some(model_provider.into());
        self
    }

    pub fn sandbox_mode(mut self, sandbox_mode: SandboxMode) -> Self {
        self.options.sandbox_mode = Some(sandbox_mode);
        self
    }

    pub fn sandbox_policy(mut self, sandbox_policy: Value) -> Self {
        self.options.sandbox_policy = Some(sandbox_policy);
        self
    }

    pub fn working_directory(mut self, working_directory: impl Into<String>) -> Self {
        self.options.working_directory = Some(working_directory.into());
        self
    }

    pub fn skip_git_repo_check(mut self, enabled: bool) -> Self {
        self.options.skip_git_repo_check = Some(enabled);
        self
    }

    pub fn model_reasoning_effort(mut self, model_reasoning_effort: ModelReasoningEffort) -> Self {
        self.options.model_reasoning_effort = Some(model_reasoning_effort);
        self
    }

    pub fn model_reasoning_summary(
        mut self,
        model_reasoning_summary: ModelReasoningSummary,
    ) -> Self {
        self.options.model_reasoning_summary = Some(model_reasoning_summary);
        self
    }

    pub fn network_access_enabled(mut self, enabled: bool) -> Self {
        self.options.network_access_enabled = Some(enabled);
        self
    }

    pub fn web_search_mode(mut self, web_search_mode: WebSearchMode) -> Self {
        self.options.web_search_mode = Some(web_search_mode);
        self
    }

    pub fn web_search_enabled(mut self, enabled: bool) -> Self {
        self.options.web_search_enabled = Some(enabled);
        self
    }

    pub fn approval_policy(mut self, approval_policy: ApprovalMode) -> Self {
        self.options.approval_policy = Some(approval_policy);
        self
    }

    pub fn additional_directories(mut self, additional_directories: Vec<String>) -> Self {
        self.options.additional_directories = Some(additional_directories);
        self
    }

    pub fn add_directory(mut self, directory: impl Into<String>) -> Self {
        self.options
            .additional_directories
            .get_or_insert_with(Vec::new)
            .push(directory.into());
        self
    }

    pub fn personality(mut self, personality: Personality) -> Self {
        self.options.personality = Some(personality);
        self
    }

    pub fn base_instructions(mut self, base_instructions: impl Into<String>) -> Self {
        self.options.base_instructions = Some(base_instructions.into());
        self
    }

    pub fn developer_instructions(mut self, developer_instructions: impl Into<String>) -> Self {
        self.options.developer_instructions = Some(developer_instructions.into());
        self
    }

    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.options.ephemeral = Some(ephemeral);
        self
    }

    pub fn collaboration_mode(mut self, collaboration_mode: CollaborationMode) -> Self {
        self.options.collaboration_mode = Some(collaboration_mode);
        self
    }

    pub fn config(mut self, config: Map<String, Value>) -> Self {
        self.options.config = Some(config);
        self
    }

    pub fn insert_config(mut self, key: impl Into<String>, value: Value) -> Self {
        self.options
            .config
            .get_or_insert_with(Map::new)
            .insert(key.into(), value);
        self
    }

    pub fn dynamic_tools(mut self, dynamic_tools: Vec<DynamicToolSpec>) -> Self {
        self.options.dynamic_tools = Some(dynamic_tools);
        self
    }

    pub fn experimental_raw_events(mut self, enabled: bool) -> Self {
        self.options.experimental_raw_events = Some(enabled);
        self
    }

    pub fn persist_extended_history(mut self, enabled: bool) -> Self {
        self.options.persist_extended_history = Some(enabled);
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    pub output_schema: Option<Value>,
    pub working_directory: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub model_reasoning_effort: Option<ModelReasoningEffort>,
    pub model_reasoning_summary: Option<ModelReasoningSummary>,
    pub personality: Option<Personality>,
    pub approval_policy: Option<ApprovalMode>,
    pub sandbox_policy: Option<Value>,
    pub collaboration_mode: Option<CollaborationMode>,
    pub skip_git_repo_check: Option<bool>,
    pub web_search_mode: Option<WebSearchMode>,
    pub web_search_enabled: Option<bool>,
    pub network_access_enabled: Option<bool>,
    pub additional_directories: Option<Vec<String>>,
    pub extra: Option<Map<String, Value>>,
}

impl TurnOptions {
    pub fn builder() -> TurnOptionsBuilder {
        TurnOptionsBuilder::new()
    }

    pub fn with_output_schema(mut self, output_schema: Value) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    pub fn with_output_schema_for<T: OpenAiSerializable>(mut self) -> Self {
        self.output_schema = Some(T::openai_output_schema());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_working_directory(mut self, working_directory: impl Into<String>) -> Self {
        self.working_directory = Some(working_directory.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct TurnOptionsBuilder {
    options: TurnOptions,
}

impl TurnOptionsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn build(self) -> TurnOptions {
        self.options
    }

    pub fn output_schema(mut self, output_schema: Value) -> Self {
        self.options.output_schema = Some(output_schema);
        self
    }

    pub fn output_schema_for<T: OpenAiSerializable>(mut self) -> Self {
        self.options.output_schema = Some(T::openai_output_schema());
        self
    }

    pub fn clear_output_schema(mut self) -> Self {
        self.options.output_schema = None;
        self
    }

    pub fn working_directory(mut self, working_directory: impl Into<String>) -> Self {
        self.options.working_directory = Some(working_directory.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.options.model = Some(model.into());
        self
    }

    pub fn model_provider(mut self, model_provider: impl Into<String>) -> Self {
        self.options.model_provider = Some(model_provider.into());
        self
    }

    pub fn model_reasoning_effort(mut self, model_reasoning_effort: ModelReasoningEffort) -> Self {
        self.options.model_reasoning_effort = Some(model_reasoning_effort);
        self
    }

    pub fn model_reasoning_summary(
        mut self,
        model_reasoning_summary: ModelReasoningSummary,
    ) -> Self {
        self.options.model_reasoning_summary = Some(model_reasoning_summary);
        self
    }

    pub fn personality(mut self, personality: Personality) -> Self {
        self.options.personality = Some(personality);
        self
    }

    pub fn approval_policy(mut self, approval_policy: ApprovalMode) -> Self {
        self.options.approval_policy = Some(approval_policy);
        self
    }

    pub fn sandbox_policy(mut self, sandbox_policy: Value) -> Self {
        self.options.sandbox_policy = Some(sandbox_policy);
        self
    }

    pub fn collaboration_mode(mut self, collaboration_mode: CollaborationMode) -> Self {
        self.options.collaboration_mode = Some(collaboration_mode);
        self
    }

    pub fn skip_git_repo_check(mut self, enabled: bool) -> Self {
        self.options.skip_git_repo_check = Some(enabled);
        self
    }

    pub fn web_search_mode(mut self, web_search_mode: WebSearchMode) -> Self {
        self.options.web_search_mode = Some(web_search_mode);
        self
    }

    pub fn web_search_enabled(mut self, enabled: bool) -> Self {
        self.options.web_search_enabled = Some(enabled);
        self
    }

    pub fn network_access_enabled(mut self, enabled: bool) -> Self {
        self.options.network_access_enabled = Some(enabled);
        self
    }

    pub fn additional_directories(mut self, additional_directories: Vec<String>) -> Self {
        self.options.additional_directories = Some(additional_directories);
        self
    }

    pub fn add_directory(mut self, directory: impl Into<String>) -> Self {
        self.options
            .additional_directories
            .get_or_insert_with(Vec::new)
            .push(directory.into());
        self
    }

    pub fn extra(mut self, extra: Map<String, Value>) -> Self {
        self.options.extra = Some(extra);
        self
    }

    pub fn insert_extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.options
            .extra
            .get_or_insert_with(Map::new)
            .insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadError {
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ThreadRunError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("{message}")]
    TurnFailed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInput {
    Text { text: String },
    LocalImage { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Text(String),
    Items(Vec<UserInput>),
}

impl Input {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn items(items: Vec<UserInput>) -> Self {
        Self::Items(items)
    }
}

impl From<String> for Input {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Input {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<Vec<UserInput>> for Input {
    fn from(value: Vec<UserInput>) -> Self {
        Self::Items(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub items: Vec<ThreadItem>,
    pub final_response: String,
    pub usage: Option<Usage>,
}

pub type RunResult = Turn;

pub struct StreamedTurn {
    turn_id: String,
    receiver: mpsc::Receiver<Result<ThreadEvent, ClientError>>,
    task: JoinHandle<()>,
}

impl StreamedTurn {
    /// Provider turn id returned by the successful `turn/start` response.
    ///
    /// This remains available for the handle's lifetime so callers can key
    /// durable, replay-safe observations without parsing notification payloads.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub async fn next_event(&mut self) -> Option<Result<ThreadEvent, ClientError>> {
        self.receiver.recv().await
    }
}

impl Drop for StreamedTurn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadEvent {
    ThreadStarted { thread_id: String },
    TurnStarted,
    TurnCompleted { usage: Option<Usage> },
    TurnFailed { error: ThreadError },
    ItemStarted { item: ThreadItem },
    ItemUpdated { item: ThreadItem },
    ItemCompleted { item: ThreadItem },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessagePhase {
    Commentary,
    FinalAnswer,
    Unknown,
}

impl AgentMessagePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageItem {
    pub id: String,
    pub text: String,
    pub phase: Option<AgentMessagePhase>,
}

impl AgentMessageItem {
    pub fn is_final_answer(&self) -> bool {
        matches!(self.phase, Some(AgentMessagePhase::FinalAnswer))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UserMessageContentItem {
    Text { text: String },
    Image { url: String },
    LocalImage { path: String },
    Unknown(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserMessageItem {
    pub id: String,
    pub content: Vec<UserMessageContentItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecutionItem {
    pub id: String,
    pub command: String,
    pub aggregated_output: String,
    pub exit_code: Option<i32>,
    pub status: CommandExecutionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeItem {
    pub id: String,
    pub changes: Vec<FileUpdateChange>,
    pub status: PatchApplyStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolCallStatus {
    InProgress,
    Completed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCallItem {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub arguments: Value,
    pub result: Option<Value>,
    pub error: Option<ThreadError>,
    pub status: McpToolCallStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynamicToolCallItem {
    pub id: String,
    pub tool: String,
    pub arguments: Value,
    pub status: String,
    pub content_items: Vec<Value>,
    pub success: Option<bool>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabToolCallItem {
    pub id: String,
    pub tool: String,
    pub status: String,
    pub sender_thread_id: String,
    pub receiver_thread_id: Option<String>,
    pub new_thread_id: Option<String>,
    pub prompt: Option<String>,
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchItem {
    pub id: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageViewItem {
    pub id: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewModeItem {
    pub id: String,
    pub review: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompactionItem {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub text: String,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoListItem {
    pub id: String,
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnknownItem {
    pub id: Option<String>,
    pub item_type: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadItem {
    AgentMessage(AgentMessageItem),
    UserMessage(UserMessageItem),
    Plan(PlanItem),
    Reasoning(ReasoningItem),
    CommandExecution(CommandExecutionItem),
    FileChange(FileChangeItem),
    McpToolCall(McpToolCallItem),
    DynamicToolCall(DynamicToolCallItem),
    CollabToolCall(CollabToolCallItem),
    WebSearch(WebSearchItem),
    ImageView(ImageViewItem),
    EnteredReviewMode(ReviewModeItem),
    ExitedReviewMode(ReviewModeItem),
    ContextCompaction(ContextCompactionItem),
    TodoList(TodoListItem),
    Error(ErrorItem),
    Unknown(UnknownItem),
}

macro_rules! codex_forward_typed_method {
    ($method:ident, $params_ty:ty, $result_ty:ty) => {
        pub async fn $method(&self, params: $params_ty) -> Result<$result_ty, ClientError> {
            self.ensure_initialized().await?;
            self.inner.client.$method(params).await
        }
    };
}

macro_rules! codex_forward_null_method {
    ($method:ident, $result_ty:ty) => {
        pub async fn $method(&self) -> Result<$result_ty, ClientError> {
            self.ensure_initialized().await?;
            self.inner.client.$method().await
        }
    };
}

#[derive(Clone)]
pub struct Codex {
    inner: Arc<CodexInner>,
}

struct CodexInner {
    client: CodexClient,
    initialize_params: requests::InitializeParams,
    initialized: AtomicBool,
    initialize_lock: Mutex<()>,
}

impl Codex {
    pub fn with_initialize_params(
        client: CodexClient,
        initialize_params: requests::InitializeParams,
    ) -> Self {
        Self {
            inner: Arc::new(CodexInner {
                client,
                initialize_params,
                initialized: AtomicBool::new(false),
                initialize_lock: Mutex::new(()),
            }),
        }
    }

    pub fn from_client(client: CodexClient) -> Self {
        let initialize_params = requests::InitializeParams::new(requests::ClientInfo::new(
            "codex_sdk_rs",
            "Codex Rust SDK",
            env!("CARGO_PKG_VERSION"),
        ));
        Self::with_initialize_params(client, initialize_params)
    }

    pub async fn spawn_stdio(config: StdioConfig) -> Result<Self, ClientError> {
        let client = CodexClient::spawn_stdio(config).await?;
        Ok(Self::from_client(client))
    }

    pub async fn connect_ws(config: WsConfig) -> Result<Self, ClientError> {
        let client = CodexClient::connect_ws(config).await?;
        Ok(Self::from_client(client))
    }

    pub async fn start_ws(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        CodexClient::start_ws(config).await
    }

    pub async fn start_ws_daemon(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        CodexClient::start_ws_daemon(config).await
    }

    pub async fn start_ws_blocking(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        CodexClient::start_ws_blocking(config).await
    }

    pub async fn start_and_connect_ws(config: WsConfig) -> Result<Self, ClientError> {
        let client = CodexClient::start_and_connect_ws(config).await?;
        Ok(Self::from_client(client))
    }

    pub fn start_thread(&self, options: ThreadOptions) -> Thread {
        Thread {
            codex: self.clone(),
            id: None,
            pending_resume: None,
            last_turn_id: None,
            options,
        }
    }

    pub fn resume_thread(&self, target: impl Into<ResumeThread>, options: ThreadOptions) -> Thread {
        let target = target.into();
        let id = match &target {
            ResumeThread::Latest => None,
            ResumeThread::ById(thread_id) => Some(thread_id.clone()),
        };

        Thread {
            codex: self.clone(),
            id,
            pending_resume: Some(target),
            last_turn_id: None,
            options,
        }
    }

    pub fn resume_thread_by_id(&self, id: impl Into<String>, options: ThreadOptions) -> Thread {
        self.resume_thread(ResumeThread::ById(id.into()), options)
    }

    pub fn resume_latest_thread(&self, options: ThreadOptions) -> Thread {
        self.resume_thread(ResumeThread::Latest, options)
    }

    /// Runs a one-shot turn on a new thread and returns only the final agent response text.
    pub async fn ask(&self, input: impl Into<Input>) -> Result<String, ThreadRunError> {
        self.ask_with_options(input, ThreadOptions::default(), TurnOptions::default())
            .await
    }

    /// Runs a one-shot turn on a new thread and returns only the final agent response text.
    pub async fn ask_with_options(
        &self,
        input: impl Into<Input>,
        thread_options: ThreadOptions,
        turn_options: TurnOptions,
    ) -> Result<String, ThreadRunError> {
        let mut thread = self.start_thread(thread_options);
        thread.ask(input, turn_options).await
    }

    /// Lists recorded threads after ensuring the app-server handshake is complete.
    pub async fn thread_list(
        &self,
        params: requests::ThreadListParams,
    ) -> Result<responses::ThreadListResult, ClientError> {
        self.ensure_initialized().await?;
        self.inner.client.thread_list(params).await
    }

    codex_forward_typed_method!(
        thread_start,
        requests::ThreadStartParams,
        responses::ThreadResult
    );
    codex_forward_typed_method!(
        thread_resume,
        requests::ThreadResumeParams,
        responses::ThreadResult
    );
    codex_forward_typed_method!(
        thread_fork,
        requests::ThreadForkParams,
        responses::ThreadResult
    );
    codex_forward_typed_method!(
        thread_archive,
        requests::ThreadArchiveParams,
        responses::ThreadArchiveResult
    );
    codex_forward_typed_method!(
        thread_name_set,
        requests::ThreadSetNameParams,
        responses::ThreadSetNameResult
    );
    codex_forward_typed_method!(
        thread_unarchive,
        requests::ThreadUnarchiveParams,
        responses::ThreadUnarchiveResult
    );
    codex_forward_typed_method!(
        thread_compact_start,
        requests::ThreadCompactStartParams,
        responses::ThreadCompactStartResult
    );
    codex_forward_typed_method!(
        thread_background_terminals_clean,
        requests::ThreadBackgroundTerminalsCleanParams,
        responses::ThreadBackgroundTerminalsCleanResult
    );
    codex_forward_typed_method!(
        thread_rollback,
        requests::ThreadRollbackParams,
        responses::ThreadRollbackResult
    );
    codex_forward_typed_method!(
        thread_loaded_list,
        requests::ThreadLoadedListParams,
        responses::ThreadLoadedListResult
    );
    codex_forward_typed_method!(
        thread_read,
        requests::ThreadReadParams,
        responses::ThreadReadResult
    );
    codex_forward_typed_method!(
        skills_list,
        requests::SkillsListParams,
        responses::SkillsListResult
    );
    codex_forward_typed_method!(
        skills_remote_list,
        requests::SkillsRemoteReadParams,
        responses::SkillsRemoteReadResult
    );
    codex_forward_typed_method!(
        skills_remote_export,
        requests::SkillsRemoteWriteParams,
        responses::SkillsRemoteWriteResult
    );
    codex_forward_typed_method!(
        app_list,
        requests::AppsListParams,
        responses::AppsListResult
    );
    codex_forward_typed_method!(
        skills_config_write,
        requests::SkillsConfigWriteParams,
        responses::SkillsConfigWriteResult
    );
    codex_forward_typed_method!(turn_start, requests::TurnStartParams, responses::TurnResult);
    codex_forward_typed_method!(
        turn_steer,
        requests::TurnSteerParams,
        responses::TurnSteerResult
    );
    codex_forward_typed_method!(turn_interrupt, requests::TurnInterruptParams, EmptyObject);
    codex_forward_typed_method!(
        review_start,
        requests::ReviewStartParams,
        responses::ReviewStartResult
    );
    codex_forward_typed_method!(
        model_list,
        requests::ModelListParams,
        responses::ModelListResult
    );
    codex_forward_typed_method!(
        experimental_feature_list,
        requests::ExperimentalFeatureListParams,
        responses::ExperimentalFeatureListResult
    );
    codex_forward_typed_method!(
        collaboration_mode_list,
        requests::CollaborationModeListParams,
        responses::CollaborationModeListResult
    );
    codex_forward_typed_method!(
        mock_experimental_method,
        requests::MockExperimentalMethodParams,
        responses::MockExperimentalMethodResult
    );
    codex_forward_typed_method!(
        mcp_server_oauth_login,
        requests::McpServerOauthLoginParams,
        responses::McpServerOauthLoginResult
    );
    codex_forward_typed_method!(
        mcp_server_status_list,
        requests::ListMcpServerStatusParams,
        responses::McpServerStatusListResult
    );
    codex_forward_typed_method!(
        windows_sandbox_setup_start,
        requests::WindowsSandboxSetupStartParams,
        responses::WindowsSandboxSetupStartResult
    );
    codex_forward_typed_method!(
        account_login_start,
        requests::LoginAccountParams,
        responses::LoginAccountResult
    );
    codex_forward_typed_method!(
        account_login_cancel,
        requests::CancelLoginAccountParams,
        EmptyObject
    );
    codex_forward_typed_method!(
        feedback_upload,
        requests::FeedbackUploadParams,
        responses::FeedbackUploadResult
    );
    codex_forward_typed_method!(
        command_exec,
        requests::CommandExecParams,
        responses::CommandExecResult
    );
    codex_forward_typed_method!(
        config_read,
        requests::ConfigReadParams,
        responses::ConfigReadResult
    );
    codex_forward_typed_method!(
        config_value_write,
        requests::ConfigValueWriteParams,
        responses::ConfigValueWriteResult
    );
    codex_forward_typed_method!(
        config_batch_write,
        requests::ConfigBatchWriteParams,
        responses::ConfigBatchWriteResult
    );
    codex_forward_typed_method!(
        account_read,
        requests::GetAccountParams,
        responses::GetAccountResult
    );
    codex_forward_typed_method!(
        fuzzy_file_search_session_start,
        requests::FuzzyFileSearchSessionStartParams,
        responses::FuzzyFileSearchSessionStartResult
    );
    codex_forward_typed_method!(
        fuzzy_file_search_session_update,
        requests::FuzzyFileSearchSessionUpdateParams,
        responses::FuzzyFileSearchSessionUpdateResult
    );
    codex_forward_typed_method!(
        fuzzy_file_search_session_stop,
        requests::FuzzyFileSearchSessionStopParams,
        responses::FuzzyFileSearchSessionStopResult
    );

    pub async fn skills_remote_read(
        &self,
        params: requests::SkillsRemoteReadParams,
    ) -> Result<responses::SkillsRemoteReadResult, ClientError> {
        self.skills_remote_list(params).await
    }

    pub async fn skills_remote_write(
        &self,
        params: requests::SkillsRemoteWriteParams,
    ) -> Result<responses::SkillsRemoteWriteResult, ClientError> {
        self.skills_remote_export(params).await
    }

    codex_forward_null_method!(config_mcp_server_reload, EmptyObject);
    codex_forward_null_method!(account_logout, EmptyObject);
    codex_forward_null_method!(
        account_rate_limits_read,
        responses::AccountRateLimitsReadResult
    );
    codex_forward_null_method!(
        config_requirements_read,
        responses::ConfigRequirementsReadResult
    );

    pub async fn send_raw_request(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<Value, ClientError> {
        self.ensure_initialized().await?;
        self.inner
            .client
            .send_raw_request(method, params, timeout)
            .await
    }

    pub async fn send_raw_notification(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ClientError> {
        self.ensure_initialized().await?;
        self.inner
            .client
            .send_raw_notification(method, params)
            .await
    }

    pub fn client(&self) -> CodexClient {
        self.inner.client.clone()
    }

    async fn ensure_initialized(&self) -> Result<(), ClientError> {
        if self.inner.initialized.load(Ordering::SeqCst) {
            return Ok(());
        }

        let _guard = self.inner.initialize_lock.lock().await;
        if self.inner.initialized.load(Ordering::SeqCst) {
            return Ok(());
        }

        match self
            .inner
            .client
            .initialize(self.inner.initialize_params.clone())
            .await
        {
            Ok(_) | Err(ClientError::AlreadyInitialized) => {}
            Err(err) => return Err(err),
        }

        self.inner.client.initialized().await?;
        self.inner.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }
}

pub struct Thread {
    codex: Codex,
    id: Option<String>,
    pending_resume: Option<ResumeThread>,
    last_turn_id: Option<String>,
    options: ThreadOptions,
}

impl Thread {
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    async fn ensure_thread_started_or_resumed(
        &mut self,
    ) -> Result<(String, Option<String>), ClientError> {
        self.codex.ensure_initialized().await?;

        let mut emit_thread_started = None;
        if let Some(pending_resume) = self.pending_resume.clone() {
            let thread_id = match pending_resume {
                ResumeThread::ById(thread_id) => thread_id,
                ResumeThread::Latest => self.resolve_latest_thread_id().await?,
            };
            let resume_params = build_thread_resume_params(&thread_id, &self.options);
            let resumed = self.codex.inner.client.thread_resume(resume_params).await?;
            self.id = Some(resumed.thread.id);
            self.pending_resume = None;
            self.last_turn_id = None;
        } else if self.id.is_none() {
            let thread = self
                .codex
                .inner
                .client
                .thread_start(build_thread_start_params(&self.options))
                .await?;
            self.id = Some(thread.thread.id.clone());
            emit_thread_started = Some(thread.thread.id);
            self.last_turn_id = None;
        }

        let thread_id = self.id.clone().ok_or_else(|| {
            ClientError::TransportSend("thread id unavailable after start/resume".to_string())
        })?;

        Ok((thread_id, emit_thread_started))
    }

    async fn resolve_latest_thread_id(&self) -> Result<String, ClientError> {
        let mut cursor: Option<String> = None;
        let mut pages_scanned = 0usize;
        let mut threads = Vec::new();

        loop {
            pages_scanned += 1;
            if pages_scanned > MAX_THREAD_LIST_PAGES {
                return Err(ClientError::TransportSend(format!(
                    "could not resolve latest thread after scanning {MAX_THREAD_LIST_PAGES} pages"
                )));
            }

            let result = self
                .codex
                .inner
                .client
                .thread_list(requests::ThreadListParams {
                    limit: Some(THREAD_LIST_PAGE_LIMIT),
                    cursor: cursor.clone(),
                    ..Default::default()
                })
                .await?;

            threads.extend(result.data);

            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        select_latest_thread_id(&threads, self.options.working_directory.as_deref()).ok_or_else(
            || match self.options.working_directory.as_deref() {
                Some(working_directory) => ClientError::TransportSend(format!(
                    "no recorded thread found for working directory `{working_directory}`"
                )),
                None => ClientError::TransportSend("no recorded thread found".to_string()),
            },
        )
    }

    pub async fn set_name(
        &mut self,
        name: impl Into<String>,
    ) -> Result<responses::ThreadSetNameResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_name_set(requests::ThreadSetNameParams {
                thread_id,
                name: name.into(),
                extra: Map::new(),
            })
            .await
    }

    pub async fn read(
        &mut self,
        include_turns: Option<bool>,
    ) -> Result<responses::ThreadReadResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_read(requests::ThreadReadParams {
                thread_id,
                include_turns,
                extra: Map::new(),
            })
            .await
    }

    pub async fn archive(&mut self) -> Result<responses::ThreadArchiveResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_archive(requests::ThreadArchiveParams {
                thread_id,
                extra: Map::new(),
            })
            .await
    }

    pub async fn unarchive(&mut self) -> Result<responses::ThreadUnarchiveResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_unarchive(requests::ThreadUnarchiveParams {
                thread_id,
                extra: Map::new(),
            })
            .await
    }

    pub async fn rollback(
        &mut self,
        count: u32,
    ) -> Result<responses::ThreadRollbackResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_rollback(requests::ThreadRollbackParams {
                thread_id,
                count,
                extra: Map::new(),
            })
            .await
    }

    pub async fn compact_start(
        &mut self,
    ) -> Result<responses::ThreadCompactStartResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .thread_compact_start(requests::ThreadCompactStartParams {
                thread_id,
                extra: Map::new(),
            })
            .await
    }

    pub async fn steer(
        &mut self,
        input: impl Into<Input>,
        expected_turn_id: Option<String>,
    ) -> Result<responses::TurnSteerResult, ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        let expected_turn_id = expected_turn_id.or_else(|| self.last_turn_id.clone());
        let expected_turn_id = expected_turn_id.ok_or_else(|| {
            ClientError::TransportSend(
                "turn/steer requires expected_turn_id or a previously started turn".to_string(),
            )
        })?;
        self.codex
            .inner
            .client
            .turn_steer(requests::TurnSteerParams {
                thread_id,
                input: normalize_input(input.into()),
                expected_turn_id: Some(expected_turn_id),
                extra: Map::new(),
            })
            .await
    }

    pub async fn interrupt(&mut self, turn_id: impl Into<String>) -> Result<(), ClientError> {
        let (thread_id, _) = self.ensure_thread_started_or_resumed().await?;
        self.codex
            .inner
            .client
            .turn_interrupt(requests::TurnInterruptParams {
                thread_id,
                turn_id: turn_id.into(),
                extra: Map::new(),
            })
            .await?;
        Ok(())
    }

    pub async fn run_streamed(
        &mut self,
        input: impl Into<Input>,
        turn_options: TurnOptions,
    ) -> Result<StreamedTurn, ClientError> {
        let (thread_id, emit_thread_started) = self.ensure_thread_started_or_resumed().await?;

        let server_events = self.codex.inner.client.subscribe();

        let turn_response = self
            .codex
            .inner
            .client
            .turn_start(build_turn_start_params(
                &thread_id,
                input.into(),
                &self.options,
                &turn_options,
            ))
            .await?;
        let turn_id = turn_response.turn.id;
        self.last_turn_id = Some(turn_id.clone());

        let (tx, rx) = mpsc::channel(256);

        if let Some(started_thread_id) = emit_thread_started
            && tx
                .send(Ok(ThreadEvent::ThreadStarted {
                    thread_id: started_thread_id,
                }))
                .await
                .is_err()
        {
            return Err(ClientError::TransportClosed);
        }

        if tx.send(Ok(ThreadEvent::TurnStarted)).await.is_err() {
            return Err(ClientError::TransportClosed);
        }

        let streamed_turn_id = turn_id.clone();
        let task = tokio::spawn(async move {
            pump_turn_events(server_events, tx, thread_id, turn_id).await;
        });

        Ok(StreamedTurn {
            turn_id: streamed_turn_id,
            receiver: rx,
            task,
        })
    }

    pub async fn run(
        &mut self,
        input: impl Into<Input>,
        turn_options: TurnOptions,
    ) -> Result<Turn, ThreadRunError> {
        let mut streamed = self.run_streamed(input, turn_options).await?;
        let mut items = Vec::new();
        let mut final_answer = None;
        let mut fallback_response = None;
        let mut usage = None;
        let mut saw_terminal = false;

        while let Some(next) = streamed.next_event().await {
            let event = next.map_err(ThreadRunError::Client)?;
            match event {
                ThreadEvent::ItemCompleted { item } => {
                    update_final_response_candidates(
                        &item,
                        &mut final_answer,
                        &mut fallback_response,
                    );
                    items.push(item);
                }
                ThreadEvent::TurnCompleted { usage: completed } => {
                    usage = completed;
                    saw_terminal = true;
                    break;
                }
                ThreadEvent::TurnFailed { error } => {
                    return Err(ThreadRunError::TurnFailed {
                        message: error.message,
                    });
                }
                ThreadEvent::Error { message } => {
                    return Err(ThreadRunError::TurnFailed { message });
                }
                ThreadEvent::ThreadStarted { .. }
                | ThreadEvent::TurnStarted
                | ThreadEvent::ItemStarted { .. }
                | ThreadEvent::ItemUpdated { .. } => {}
            }
        }

        if !saw_terminal {
            return Err(ThreadRunError::Client(ClientError::TransportClosed));
        }

        Ok(Turn {
            items,
            final_response: final_answer.or(fallback_response).unwrap_or_default(),
            usage,
        })
    }

    /// Runs one turn on this thread and returns only the final agent response text.
    pub async fn ask(
        &mut self,
        input: impl Into<Input>,
        turn_options: TurnOptions,
    ) -> Result<String, ThreadRunError> {
        let turn = self.run(input, turn_options).await?;
        Ok(turn.final_response)
    }
}

async fn pump_turn_events(
    mut server_events: tokio::sync::broadcast::Receiver<ServerEvent>,
    tx: mpsc::Sender<Result<ThreadEvent, ClientError>>,
    thread_id: String,
    turn_id: String,
) {
    let mut latest_usage: Option<Usage> = None;

    loop {
        let next = server_events.recv().await;
        let server_event = match next {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let _ = tx.send(Err(ClientError::TransportClosed)).await;
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                continue;
            }
        };

        match server_event {
            ServerEvent::Notification(notification) => match notification {
                ServerNotification::ItemStarted(payload)
                    if matches_target_from_extra(&payload.extra, &thread_id, Some(&turn_id)) =>
                {
                    if tx
                        .send(Ok(ThreadEvent::ItemStarted {
                            item: parse_thread_item(payload.item),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ServerNotification::ItemCompleted(payload)
                    if matches_target_from_extra(&payload.extra, &thread_id, Some(&turn_id)) =>
                {
                    if tx
                        .send(Ok(ThreadEvent::ItemCompleted {
                            item: parse_thread_item(payload.item),
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ServerNotification::ItemAgentMessageDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let text = delta.delta.or(delta.text).unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    let item = ThreadItem::AgentMessage(AgentMessageItem {
                        id: delta.item_id.unwrap_or_default(),
                        text,
                        phase: None,
                    });
                    if tx
                        .send(Ok(ThreadEvent::ItemUpdated { item }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ServerNotification::ItemPlanDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let text = delta.delta.or(delta.text).unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    let item = ThreadItem::Plan(PlanItem {
                        id: delta.item_id.unwrap_or_default(),
                        text,
                    });
                    if tx
                        .send(Ok(ThreadEvent::ItemUpdated { item }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ServerNotification::ItemReasoningSummaryTextDelta(delta)
                | ServerNotification::ItemReasoningTextDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let text = delta.delta.or(delta.text).unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    let item = ThreadItem::Reasoning(ReasoningItem {
                        id: delta.item_id.unwrap_or_default(),
                        text,
                    });
                    if tx
                        .send(Ok(ThreadEvent::ItemUpdated { item }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                ServerNotification::TurnCompleted(payload)
                    if payload.turn.id == turn_id
                        && matches_target_from_extra(
                            &payload.turn.extra,
                            &thread_id,
                            Some(&turn_id),
                        ) =>
                {
                    let status = payload.turn.status.unwrap_or_default().to_ascii_lowercase();
                    if status == "failed" {
                        let message = payload
                            .turn
                            .error
                            .map(|error| error.message)
                            .unwrap_or_else(|| "turn failed".to_string());
                        let _ = tx
                            .send(Ok(ThreadEvent::TurnFailed {
                                error: ThreadError { message },
                            }))
                            .await;
                        break;
                    }

                    let usage = parse_usage_from_turn_extra(&payload.turn.extra)
                        .or_else(|| latest_usage.clone());
                    let _ = tx.send(Ok(ThreadEvent::TurnCompleted { usage })).await;
                    break;
                }
                ServerNotification::ThreadTokenUsageUpdated(payload)
                    if payload
                        .thread_id
                        .as_deref()
                        .is_none_or(|incoming| incoming == thread_id) =>
                {
                    if !matches_target_from_extra(&payload.extra, &thread_id, Some(&turn_id)) {
                        continue;
                    }
                    latest_usage = payload
                        .usage
                        .as_ref()
                        .and_then(parse_usage_from_value)
                        .or_else(|| {
                            payload
                                .extra
                                .get("tokenUsage")
                                .and_then(parse_usage_from_value)
                        });
                }
                ServerNotification::Error(payload)
                    if matches_target_from_extra(&payload.extra, &thread_id, Some(&turn_id)) =>
                {
                    let _ = tx
                        .send(Ok(ThreadEvent::Error {
                            message: payload.error.message,
                        }))
                        .await;
                }
                _ => {}
            },
            ServerEvent::TransportClosed => {
                let _ = tx.send(Err(ClientError::TransportClosed)).await;
                break;
            }
            ServerEvent::ServerRequest(_) => {}
        }
    }
}

fn update_final_response_candidates(
    item: &ThreadItem,
    final_answer: &mut Option<String>,
    fallback_response: &mut Option<String>,
) {
    let ThreadItem::AgentMessage(agent_message) = item else {
        return;
    };

    if agent_message.is_final_answer() {
        *final_answer = Some(agent_message.text.clone());
    } else {
        *fallback_response = Some(agent_message.text.clone());
    }
}

fn select_latest_thread_id(
    threads: &[responses::ThreadSummary],
    working_directory: Option<&str>,
) -> Option<String> {
    let mut selected: Option<(Option<i64>, String)> = None;

    for thread in threads {
        if !thread_matches_working_directory(thread, working_directory) {
            continue;
        }

        let candidate = (thread_recency_score(thread), thread.id.clone());
        match &selected {
            None => selected = Some(candidate),
            // `thread/list` is already emitted newest-first. If either entry lacks
            // recency metadata, keep the earlier-listed match instead of letting a
            // later older summary override it just because it has a timestamp.
            Some((Some(best_score), _)) if candidate.0.is_some_and(|score| score > *best_score) => {
                selected = Some(candidate)
            }
            Some(_) => {}
        }
    }

    selected.map(|(_, thread_id)| thread_id)
}

fn thread_matches_working_directory(
    thread: &responses::ThreadSummary,
    working_directory: Option<&str>,
) -> bool {
    let Some(working_directory) = working_directory else {
        return true;
    };

    thread.extra.get("cwd").and_then(Value::as_str) == Some(working_directory)
}

fn thread_recency_score(thread: &responses::ThreadSummary) -> Option<i64> {
    parse_timestamp(thread.extra.get("updatedAt"))
        .or_else(|| parse_timestamp(thread.extra.get("createdAt")))
}

fn parse_timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|raw| i64::try_from(raw).ok())),
        Value::String(raw) => raw.parse::<i64>().ok(),
        _ => None,
    }
}

fn build_thread_start_params(options: &ThreadOptions) -> requests::ThreadStartParams {
    let mut extra = Map::new();
    if let Some(skip) = options.skip_git_repo_check {
        extra.insert("skipGitRepoCheck".to_string(), Value::Bool(skip));
    }
    if let Some(mode) = options.web_search_mode {
        extra.insert(
            "webSearchMode".to_string(),
            Value::String(mode.as_str().to_string()),
        );
    }
    if let Some(enabled) = options.web_search_enabled {
        extra.insert("webSearchEnabled".to_string(), Value::Bool(enabled));
    }
    if let Some(network) = options.network_access_enabled {
        extra.insert("networkAccessEnabled".to_string(), Value::Bool(network));
    }
    if let Some(additional) = &options.additional_directories {
        extra.insert(
            "additionalDirectories".to_string(),
            Value::Array(
                additional
                    .iter()
                    .map(|entry| Value::String(entry.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(config) = &options.config {
        extra.insert("config".to_string(), Value::Object(config.clone()));
    }
    if let Some(dynamic_tools) = &options.dynamic_tools {
        extra.insert(
            "dynamicTools".to_string(),
            Value::Array(
                dynamic_tools
                    .iter()
                    .map(DynamicToolSpec::as_value)
                    .collect(),
            ),
        );
    }
    if let Some(enabled) = options.experimental_raw_events {
        extra.insert("experimentalRawEvents".to_string(), Value::Bool(enabled));
    }
    if let Some(enabled) = options.persist_extended_history {
        extra.insert("persistExtendedHistory".to_string(), Value::Bool(enabled));
    }

    requests::ThreadStartParams {
        model: options.model.clone(),
        model_provider: options.model_provider.clone(),
        cwd: options.working_directory.clone(),
        approval_policy: options
            .approval_policy
            .map(|mode| mode.as_str().to_string()),
        sandbox: options.sandbox_mode.map(|mode| mode.as_str().to_string()),
        sandbox_policy: options.sandbox_policy.clone(),
        effort: options
            .model_reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        summary: options
            .model_reasoning_summary
            .map(|summary| summary.as_str().to_string()),
        personality: options.personality.map(|value| value.as_str().to_string()),
        ephemeral: options.ephemeral,
        base_instructions: options.base_instructions.clone(),
        developer_instructions: options.developer_instructions.clone(),
        extra,
    }
}

fn build_thread_resume_params(
    thread_id: &str,
    options: &ThreadOptions,
) -> requests::ThreadResumeParams {
    let mut extra = Map::new();
    if let Some(skip) = options.skip_git_repo_check {
        extra.insert("skipGitRepoCheck".to_string(), Value::Bool(skip));
    }
    if let Some(mode) = options.web_search_mode {
        extra.insert(
            "webSearchMode".to_string(),
            Value::String(mode.as_str().to_string()),
        );
    }
    if let Some(enabled) = options.web_search_enabled {
        extra.insert("webSearchEnabled".to_string(), Value::Bool(enabled));
    }
    if let Some(network) = options.network_access_enabled {
        extra.insert("networkAccessEnabled".to_string(), Value::Bool(network));
    }
    if let Some(additional) = &options.additional_directories {
        extra.insert(
            "additionalDirectories".to_string(),
            Value::Array(
                additional
                    .iter()
                    .map(|entry| Value::String(entry.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(policy) = &options.sandbox_policy {
        extra.insert("sandboxPolicy".to_string(), policy.clone());
    }
    if let Some(effort) = options.model_reasoning_effort {
        extra.insert(
            "effort".to_string(),
            Value::String(effort.as_str().to_string()),
        );
    }
    if let Some(summary) = options.model_reasoning_summary {
        extra.insert(
            "summary".to_string(),
            Value::String(summary.as_str().to_string()),
        );
    }
    if let Some(ephemeral) = options.ephemeral {
        extra.insert("ephemeral".to_string(), Value::Bool(ephemeral));
    }
    if let Some(collaboration_mode) = &options.collaboration_mode {
        extra.insert(
            "collaborationMode".to_string(),
            collaboration_mode.as_value(),
        );
    }
    if let Some(dynamic_tools) = &options.dynamic_tools {
        extra.insert(
            "dynamicTools".to_string(),
            Value::Array(
                dynamic_tools
                    .iter()
                    .map(DynamicToolSpec::as_value)
                    .collect(),
            ),
        );
    }
    if let Some(enabled) = options.experimental_raw_events {
        extra.insert("experimentalRawEvents".to_string(), Value::Bool(enabled));
    }

    requests::ThreadResumeParams {
        thread_id: thread_id.to_string(),
        history: None,
        path: None,
        model: options.model.clone(),
        model_provider: options.model_provider.clone(),
        cwd: options.working_directory.clone(),
        approval_policy: options
            .approval_policy
            .map(|mode| mode.as_str().to_string()),
        sandbox: options.sandbox_mode.map(|mode| mode.as_str().to_string()),
        config: options.config.clone(),
        base_instructions: options.base_instructions.clone(),
        developer_instructions: options.developer_instructions.clone(),
        personality: options.personality.map(|value| value.as_str().to_string()),
        persist_extended_history: options.persist_extended_history,
        extra,
    }
}

fn build_turn_start_params(
    thread_id: &str,
    input: Input,
    options: &ThreadOptions,
    turn_options: &TurnOptions,
) -> requests::TurnStartParams {
    let mut extra = Map::new();
    if let Some(skip) = turn_options
        .skip_git_repo_check
        .or(options.skip_git_repo_check)
    {
        extra.insert("skipGitRepoCheck".to_string(), Value::Bool(skip));
    }
    if let Some(mode) = turn_options.web_search_mode.or(options.web_search_mode) {
        extra.insert(
            "webSearchMode".to_string(),
            Value::String(mode.as_str().to_string()),
        );
    }
    if let Some(enabled) = turn_options
        .web_search_enabled
        .or(options.web_search_enabled)
    {
        extra.insert("webSearchEnabled".to_string(), Value::Bool(enabled));
    }
    if let Some(network) = turn_options
        .network_access_enabled
        .or(options.network_access_enabled)
    {
        extra.insert("networkAccessEnabled".to_string(), Value::Bool(network));
    }
    if let Some(additional) = turn_options
        .additional_directories
        .as_ref()
        .or(options.additional_directories.as_ref())
    {
        extra.insert(
            "additionalDirectories".to_string(),
            Value::Array(
                additional
                    .iter()
                    .map(|entry| Value::String(entry.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(collaboration_mode) = turn_options
        .collaboration_mode
        .as_ref()
        .or(options.collaboration_mode.as_ref())
    {
        extra.insert(
            "collaborationMode".to_string(),
            collaboration_mode.as_value(),
        );
    }
    if let Some(extra_overrides) = &turn_options.extra {
        for (key, value) in extra_overrides {
            extra.insert(key.clone(), value.clone());
        }
    }

    requests::TurnStartParams {
        thread_id: thread_id.to_string(),
        input: normalize_input(input),
        cwd: turn_options
            .working_directory
            .clone()
            .or_else(|| options.working_directory.clone()),
        model: turn_options.model.clone().or_else(|| options.model.clone()),
        model_provider: turn_options
            .model_provider
            .clone()
            .or_else(|| options.model_provider.clone()),
        effort: turn_options
            .model_reasoning_effort
            .or(options.model_reasoning_effort)
            .map(|effort| effort.as_str().to_string()),
        summary: turn_options
            .model_reasoning_summary
            .or(options.model_reasoning_summary)
            .map(|summary| summary.as_str().to_string()),
        personality: turn_options
            .personality
            .or(options.personality)
            .map(|value| value.as_str().to_string()),
        output_schema: turn_options.output_schema.clone(),
        approval_policy: turn_options
            .approval_policy
            .or(options.approval_policy)
            .map(|mode| mode.as_str().to_string()),
        sandbox_policy: turn_options
            .sandbox_policy
            .clone()
            .or_else(|| options.sandbox_policy.clone()),
        collaboration_mode: None,
        extra,
    }
}

fn normalize_input(input: Input) -> Vec<requests::TurnInputItem> {
    match input {
        Input::Text(text) => vec![requests::TurnInputItem::Text { text }],
        Input::Items(items) => {
            let mut text_parts = Vec::new();
            let mut normalized = Vec::new();

            for item in items {
                match item {
                    UserInput::Text { text } => text_parts.push(text),
                    UserInput::LocalImage { path } => {
                        normalized.push(requests::TurnInputItem::LocalImage { path });
                    }
                }
            }

            if !text_parts.is_empty() {
                normalized.insert(
                    0,
                    requests::TurnInputItem::Text {
                        text: text_parts.join("\n\n"),
                    },
                );
            }

            normalized
        }
    }
}

fn matches_target_from_extra(
    extra: &Map<String, Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> bool {
    let thread_matches = extra
        .get("threadId")
        .and_then(Value::as_str)
        .map(|incoming| incoming == thread_id)
        .unwrap_or(true);

    let turn_matches = match turn_id {
        Some(target_turn_id) => extra
            .get("turnId")
            .and_then(Value::as_str)
            .map(|incoming| incoming == target_turn_id)
            .unwrap_or(true),
        None => true,
    };

    thread_matches && turn_matches
}

fn parse_usage_from_turn_extra(extra: &Map<String, Value>) -> Option<Usage> {
    extra
        .get("usage")
        .and_then(parse_usage_from_value)
        .or_else(|| extra.get("tokenUsage").and_then(parse_usage_from_value))
}

fn parse_usage_from_value(value: &Value) -> Option<Usage> {
    let object = value.as_object()?;
    if let Some(last) = object.get("last") {
        return parse_usage_from_value(last);
    }

    let input_tokens = object.get("inputTokens").and_then(Value::as_i64)?;
    let cached_input_tokens = object
        .get("cachedInputTokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output_tokens = object.get("outputTokens").and_then(Value::as_i64)?;

    Some(Usage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
    })
}

fn parse_thread_item(item: Value) -> ThreadItem {
    let object = match item.as_object() {
        Some(object) => object,
        None => {
            return ThreadItem::Unknown(UnknownItem {
                id: None,
                item_type: None,
                raw: item,
            });
        }
    };

    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value.to_string());
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .map(|value| value.to_string());

    match item_type.as_deref() {
        Some("agentMessage") => ThreadItem::AgentMessage(AgentMessageItem {
            id: id.unwrap_or_default(),
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            phase: object
                .get("phase")
                .and_then(Value::as_str)
                .map(parse_agent_message_phase),
        }),
        Some("userMessage") => ThreadItem::UserMessage(UserMessageItem {
            id: id.unwrap_or_default(),
            content: object
                .get("content")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .map(parse_user_message_content_item)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        Some("plan") => ThreadItem::Plan(PlanItem {
            id: id.unwrap_or_default(),
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("reasoning") => ThreadItem::Reasoning(ReasoningItem {
            id: id.unwrap_or_default(),
            text: parse_reasoning_text(object),
        }),
        Some("commandExecution") => ThreadItem::CommandExecution(CommandExecutionItem {
            id: id.unwrap_or_default(),
            command: object
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            aggregated_output: object
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            exit_code: object
                .get("exitCode")
                .and_then(Value::as_i64)
                .map(|value| value as i32),
            status: parse_command_execution_status(
                object
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
        }),
        Some("fileChange") => ThreadItem::FileChange(FileChangeItem {
            id: id.unwrap_or_default(),
            changes: object
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(parse_file_update_change)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            status: parse_patch_apply_status(
                object
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
        }),
        Some("mcpToolCall") => {
            let error = object
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(|message| ThreadError {
                    message: message.to_string(),
                });

            ThreadItem::McpToolCall(McpToolCallItem {
                id: id.unwrap_or_default(),
                server: object
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                tool: object
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: object.get("arguments").cloned().unwrap_or(Value::Null),
                result: object.get("result").cloned(),
                error,
                status: parse_mcp_tool_call_status(
                    object
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
            })
        }
        Some("dynamicToolCall") => ThreadItem::DynamicToolCall(DynamicToolCallItem {
            id: id.unwrap_or_default(),
            tool: object
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: object.get("arguments").cloned().unwrap_or(Value::Null),
            status: object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content_items: object
                .get("contentItems")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            success: object.get("success").and_then(Value::as_bool),
            duration_ms: object.get("durationMs").and_then(Value::as_u64),
        }),
        Some("collabToolCall") => ThreadItem::CollabToolCall(CollabToolCallItem {
            id: id.unwrap_or_default(),
            tool: object
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status: object
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            sender_thread_id: object
                .get("senderThreadId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            receiver_thread_id: object
                .get("receiverThreadId")
                .and_then(Value::as_str)
                .map(str::to_string),
            new_thread_id: object
                .get("newThreadId")
                .and_then(Value::as_str)
                .map(str::to_string),
            prompt: object
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::to_string),
            agent_status: object
                .get("agentStatus")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        Some("webSearch") => ThreadItem::WebSearch(WebSearchItem {
            id: id.unwrap_or_default(),
            query: object
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("imageView") => ThreadItem::ImageView(ImageViewItem {
            id: id.unwrap_or_default(),
            path: object
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("enteredReviewMode") => ThreadItem::EnteredReviewMode(ReviewModeItem {
            id: id.unwrap_or_default(),
            review: object
                .get("review")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("exitedReviewMode") => ThreadItem::ExitedReviewMode(ReviewModeItem {
            id: id.unwrap_or_default(),
            review: object
                .get("review")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        Some("contextCompaction") => ThreadItem::ContextCompaction(ContextCompactionItem {
            id: id.unwrap_or_default(),
        }),
        Some("todoList") => ThreadItem::TodoList(TodoListItem {
            id: id.unwrap_or_default(),
            items: object
                .get("items")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_todo_item).collect())
                .unwrap_or_default(),
        }),
        Some("error") => ThreadItem::Error(ErrorItem {
            id: id.unwrap_or_default(),
            message: object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => ThreadItem::Unknown(UnknownItem {
            id,
            item_type,
            raw: item,
        }),
    }
}

fn parse_agent_message_phase(value: &str) -> AgentMessagePhase {
    match value {
        "commentary" => AgentMessagePhase::Commentary,
        "final_answer" => AgentMessagePhase::FinalAnswer,
        _ => AgentMessagePhase::Unknown,
    }
}

fn parse_user_message_content_item(value: &Value) -> UserMessageContentItem {
    let Some(object) = value.as_object() else {
        return UserMessageContentItem::Unknown(value.clone());
    };

    match object.get("type").and_then(Value::as_str) {
        Some("text") => UserMessageContentItem::Text {
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("image") => UserMessageContentItem::Image {
            url: object
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("localImage") => UserMessageContentItem::LocalImage {
            path: object
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        _ => UserMessageContentItem::Unknown(value.clone()),
    }
}

fn parse_reasoning_text(object: &Map<String, Value>) -> String {
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        return text.to_string();
    }

    let summary = object
        .get("summary")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let content = object
        .get("content")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if summary.is_empty() {
        content
    } else if content.is_empty() {
        summary
    } else {
        format!("{summary}\n{content}")
    }
}

fn parse_file_update_change(change: &Value) -> Option<FileUpdateChange> {
    let object = change.as_object()?;
    let kind = match object.get("kind") {
        Some(Value::String(kind)) => parse_patch_change_kind(kind),
        Some(Value::Object(kind_object)) => kind_object
            .get("type")
            .and_then(Value::as_str)
            .map(parse_patch_change_kind)
            .unwrap_or(PatchChangeKind::Unknown),
        _ => PatchChangeKind::Unknown,
    };

    Some(FileUpdateChange {
        path: object
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind,
    })
}

fn parse_todo_item(value: &Value) -> Option<TodoItem> {
    let object = value.as_object()?;
    Some(TodoItem {
        text: object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        completed: object
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_command_execution_status(status: &str) -> CommandExecutionStatus {
    match status {
        "inProgress" | "in_progress" => CommandExecutionStatus::InProgress,
        "completed" => CommandExecutionStatus::Completed,
        "failed" => CommandExecutionStatus::Failed,
        "declined" => CommandExecutionStatus::Declined,
        _ => CommandExecutionStatus::Unknown,
    }
}

fn parse_patch_change_kind(kind: &str) -> PatchChangeKind {
    match kind {
        "add" => PatchChangeKind::Add,
        "delete" => PatchChangeKind::Delete,
        "update" => PatchChangeKind::Update,
        _ => PatchChangeKind::Unknown,
    }
}

fn parse_patch_apply_status(status: &str) -> PatchApplyStatus {
    match status {
        "inProgress" | "in_progress" => PatchApplyStatus::InProgress,
        "completed" => PatchApplyStatus::Completed,
        "failed" => PatchApplyStatus::Failed,
        "declined" => PatchApplyStatus::Declined,
        _ => PatchApplyStatus::Unknown,
    }
}

fn parse_mcp_tool_call_status(status: &str) -> McpToolCallStatus {
    match status {
        "inProgress" | "in_progress" => McpToolCallStatus::InProgress,
        "completed" => McpToolCallStatus::Completed,
        "failed" => McpToolCallStatus::Failed,
        _ => McpToolCallStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Serialize,
        Deserialize,
        JsonSchema,
        codex_app_server_sdk::OpenAiSerializable,
    )]
    struct StructuredReply {
        answer: String,
    }

    #[test]
    fn public_string_enums_serde_as_codex_config_values() {
        #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
        struct ConfigValues {
            approval_policy: ApprovalMode,
            sandbox_mode: SandboxMode,
            model_reasoning_effort: ModelReasoningEffort,
            model_reasoning_summary: ModelReasoningSummary,
            personality: Personality,
            web_search: WebSearchMode,
            collaboration_mode: CollaborationModeKind,
        }

        let parsed: ConfigValues = toml::from_str(
            r#"
approval_policy = "on-request"
sandbox_mode = "danger-full-access"
model_reasoning_effort = "xhigh"
model_reasoning_summary = "detailed"
personality = "pragmatic"
web_search = "live"
collaboration_mode = "plan"
"#,
        )
        .expect("deserialize TOML config values");

        assert_eq!(
            parsed,
            ConfigValues {
                approval_policy: ApprovalMode::OnRequest,
                sandbox_mode: SandboxMode::DangerFullAccess,
                model_reasoning_effort: ModelReasoningEffort::XHigh,
                model_reasoning_summary: ModelReasoningSummary::Detailed,
                personality: Personality::Pragmatic,
                web_search: WebSearchMode::Live,
                collaboration_mode: CollaborationModeKind::Plan,
            }
        );

        let value = serde_json::to_value(&parsed).expect("serialize config values");
        assert_eq!(
            value,
            json!({
                "approval_policy": "on-request",
                "sandbox_mode": "danger-full-access",
                "model_reasoning_effort": "xhigh",
                "model_reasoning_summary": "detailed",
                "personality": "pragmatic",
                "web_search": "live",
                "collaboration_mode": "plan",
            })
        );
    }

    fn thread_summary(
        id: &str,
        cwd: Option<&str>,
        updated_at: Option<i64>,
        created_at: Option<i64>,
    ) -> responses::ThreadSummary {
        let mut extra = Map::new();
        if let Some(cwd) = cwd {
            extra.insert("cwd".to_string(), Value::String(cwd.to_string()));
        }
        if let Some(updated_at) = updated_at {
            extra.insert("updatedAt".to_string(), Value::from(updated_at));
        }
        if let Some(created_at) = created_at {
            extra.insert("createdAt".to_string(), Value::from(created_at));
        }

        responses::ThreadSummary {
            id: id.to_string(),
            extra,
            ..Default::default()
        }
    }

    #[test]
    fn normalize_input_combines_text_and_images() {
        let normalized = normalize_input(Input::Items(vec![
            UserInput::Text {
                text: "first".to_string(),
            },
            UserInput::Text {
                text: "second".to_string(),
            },
            UserInput::LocalImage {
                path: "/tmp/one.png".to_string(),
            },
            UserInput::LocalImage {
                path: "/tmp/two.png".to_string(),
            },
        ]));

        assert_eq!(normalized.len(), 3);
        match &normalized[0] {
            requests::TurnInputItem::Text { text } => assert_eq!(text, "first\n\nsecond"),
            other => panic!("expected text input item, got {other:?}"),
        }
        match &normalized[1] {
            requests::TurnInputItem::LocalImage { path } => assert_eq!(path, "/tmp/one.png"),
            other => panic!("expected local image input item, got {other:?}"),
        }
        match &normalized[2] {
            requests::TurnInputItem::LocalImage { path } => assert_eq!(path, "/tmp/two.png"),
            other => panic!("expected local image input item, got {other:?}"),
        }
    }

    #[test]
    fn parse_agent_message_item() {
        let item = parse_thread_item(json!({
            "id": "item_1",
            "type": "agentMessage",
            "text": "hello",
            "phase": "final_answer"
        }));

        assert_eq!(
            item,
            ThreadItem::AgentMessage(AgentMessageItem {
                id: "item_1".to_string(),
                text: "hello".to_string(),
                phase: Some(AgentMessagePhase::FinalAnswer),
            })
        );
    }

    #[test]
    fn parse_missing_documented_thread_item_variants() {
        let cases = vec![
            (
                json!({
                    "id": "user_1",
                    "type": "userMessage",
                    "content": [
                        { "type": "text", "text": "hello" },
                        { "type": "localImage", "path": "/tmp/example.png" }
                    ]
                }),
                ThreadItem::UserMessage(UserMessageItem {
                    id: "user_1".to_string(),
                    content: vec![
                        UserMessageContentItem::Text {
                            text: "hello".to_string(),
                        },
                        UserMessageContentItem::LocalImage {
                            path: "/tmp/example.png".to_string(),
                        },
                    ],
                }),
            ),
            (
                json!({
                    "id": "plan_1",
                    "type": "plan",
                    "text": "1. inspect\n2. patch"
                }),
                ThreadItem::Plan(PlanItem {
                    id: "plan_1".to_string(),
                    text: "1. inspect\n2. patch".to_string(),
                }),
            ),
            (
                json!({
                    "id": "reason_1",
                    "type": "reasoning",
                    "summary": ["checking docs"],
                    "content": ["raw chain"]
                }),
                ThreadItem::Reasoning(ReasoningItem {
                    id: "reason_1".to_string(),
                    text: "checking docs\nraw chain".to_string(),
                }),
            ),
            (
                json!({
                    "id": "tool_1",
                    "type": "dynamicToolCall",
                    "tool": "tool/search",
                    "arguments": { "q": "rust" },
                    "status": "completed",
                    "contentItems": [{ "type": "text", "text": "done" }],
                    "success": true,
                    "durationMs": 12
                }),
                ThreadItem::DynamicToolCall(DynamicToolCallItem {
                    id: "tool_1".to_string(),
                    tool: "tool/search".to_string(),
                    arguments: json!({ "q": "rust" }),
                    status: "completed".to_string(),
                    content_items: vec![json!({ "type": "text", "text": "done" })],
                    success: Some(true),
                    duration_ms: Some(12),
                }),
            ),
            (
                json!({
                    "id": "collab_1",
                    "type": "collabToolCall",
                    "tool": "delegate",
                    "status": "completed",
                    "senderThreadId": "thr_a",
                    "receiverThreadId": "thr_b",
                    "newThreadId": "thr_c",
                    "prompt": "review this",
                    "agentStatus": "idle"
                }),
                ThreadItem::CollabToolCall(CollabToolCallItem {
                    id: "collab_1".to_string(),
                    tool: "delegate".to_string(),
                    status: "completed".to_string(),
                    sender_thread_id: "thr_a".to_string(),
                    receiver_thread_id: Some("thr_b".to_string()),
                    new_thread_id: Some("thr_c".to_string()),
                    prompt: Some("review this".to_string()),
                    agent_status: Some("idle".to_string()),
                }),
            ),
            (
                json!({
                    "id": "image_1",
                    "type": "imageView",
                    "path": "/tmp/example.jpg"
                }),
                ThreadItem::ImageView(ImageViewItem {
                    id: "image_1".to_string(),
                    path: "/tmp/example.jpg".to_string(),
                }),
            ),
            (
                json!({
                    "id": "review_1",
                    "type": "enteredReviewMode",
                    "review": "current changes"
                }),
                ThreadItem::EnteredReviewMode(ReviewModeItem {
                    id: "review_1".to_string(),
                    review: "current changes".to_string(),
                }),
            ),
            (
                json!({
                    "id": "review_2",
                    "type": "exitedReviewMode",
                    "review": "looks good"
                }),
                ThreadItem::ExitedReviewMode(ReviewModeItem {
                    id: "review_2".to_string(),
                    review: "looks good".to_string(),
                }),
            ),
            (
                json!({
                    "id": "compact_1",
                    "type": "contextCompaction"
                }),
                ThreadItem::ContextCompaction(ContextCompactionItem {
                    id: "compact_1".to_string(),
                }),
            ),
        ];

        for (raw, expected) in cases {
            assert_eq!(parse_thread_item(raw), expected);
        }
    }

    #[tokio::test]
    async fn pump_turn_events_emits_reasoning_text_deltas() {
        let (server_tx, server_rx) = tokio::sync::broadcast::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let pump = tokio::spawn(pump_turn_events(
            server_rx,
            event_tx,
            "thread_1".to_string(),
            "turn_1".to_string(),
        ));

        let mut extra = Map::new();
        extra.insert(
            "threadId".to_string(),
            Value::String("thread_1".to_string()),
        );
        extra.insert("turnId".to_string(), Value::String("turn_1".to_string()));

        server_tx
            .send(ServerEvent::Notification(
                ServerNotification::ItemReasoningSummaryTextDelta(
                    crate::protocol::notifications::DeltaNotification {
                        item_id: Some("reason_1".to_string()),
                        delta: Some("checking docs".to_string()),
                        text: None,
                        summary_index: Some(0),
                        extra: extra.clone(),
                    },
                ),
            ))
            .expect("send summary reasoning delta");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("reasoning summary event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::ItemUpdated {
                item: ThreadItem::Reasoning(ReasoningItem {
                    id: "reason_1".to_string(),
                    text: "checking docs".to_string(),
                }),
            }
        );

        server_tx
            .send(ServerEvent::Notification(
                ServerNotification::ItemReasoningTextDelta(
                    crate::protocol::notifications::DeltaNotification {
                        item_id: Some("reason_1".to_string()),
                        delta: Some("raw reasoning".to_string()),
                        text: None,
                        summary_index: None,
                        extra,
                    },
                ),
            ))
            .expect("send raw reasoning delta");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("reasoning text event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::ItemUpdated {
                item: ThreadItem::Reasoning(ReasoningItem {
                    id: "reason_1".to_string(),
                    text: "raw reasoning".to_string(),
                }),
            }
        );

        pump.abort();
    }

    #[test]
    fn final_response_prefers_final_answer_phase() {
        let mut final_answer = None;
        let mut fallback_response = None;

        update_final_response_candidates(
            &ThreadItem::AgentMessage(AgentMessageItem {
                id: "msg_1".to_string(),
                text: "thinking".to_string(),
                phase: Some(AgentMessagePhase::Commentary),
            }),
            &mut final_answer,
            &mut fallback_response,
        );
        update_final_response_candidates(
            &ThreadItem::AgentMessage(AgentMessageItem {
                id: "msg_2".to_string(),
                text: "final".to_string(),
                phase: Some(AgentMessagePhase::FinalAnswer),
            }),
            &mut final_answer,
            &mut fallback_response,
        );

        assert_eq!(fallback_response.as_deref(), Some("thinking"));
        assert_eq!(final_answer.as_deref(), Some("final"));
        assert_eq!(
            final_answer.or(fallback_response),
            Some("final".to_string())
        );
    }

    #[test]
    fn parse_usage_from_token_usage_payload() {
        let usage = parse_usage_from_value(&json!({
            "last": {
                "inputTokens": 10,
                "cachedInputTokens": 2,
                "outputTokens": 7
            }
        }));

        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
                output_tokens: 7,
            })
        );
    }

    #[test]
    fn parse_unknown_item_preserves_payload() {
        let raw = json!({
            "id": "x",
            "type": "newItemType",
            "payload": {"a": 1}
        });
        let parsed = parse_thread_item(raw.clone());

        match parsed {
            ThreadItem::Unknown(unknown) => {
                assert_eq!(unknown.id.as_deref(), Some("x"));
                assert_eq!(unknown.item_type.as_deref(), Some("newItemType"));
                assert_eq!(unknown.raw, raw);
            }
            other => panic!("expected unknown item, got {other:?}"),
        }
    }

    #[test]
    fn select_latest_thread_id_prefers_newest_matching_working_directory() {
        let threads = vec![
            thread_summary("thread_other", Some("/tmp/other"), Some(300), None),
            thread_summary("thread_old", Some("/tmp/workspace"), None, Some(100)),
            thread_summary("thread_new", Some("/tmp/workspace"), Some(200), None),
        ];

        assert_eq!(
            select_latest_thread_id(&threads, Some("/tmp/workspace")),
            Some("thread_new".to_string())
        );
    }

    #[test]
    fn select_latest_thread_id_falls_back_to_first_matching_thread_without_timestamps() {
        let threads = vec![
            thread_summary("thread_other", Some("/tmp/other"), None, None),
            thread_summary("thread_match", Some("/tmp/workspace"), None, None),
            thread_summary("thread_match_two", Some("/tmp/workspace"), None, None),
        ];

        assert_eq!(
            select_latest_thread_id(&threads, Some("/tmp/workspace")),
            Some("thread_match".to_string())
        );
    }

    #[test]
    fn select_latest_thread_id_keeps_newest_listed_match_when_later_entry_only_has_timestamp() {
        let threads = vec![
            thread_summary("thread_newest", Some("/tmp/workspace"), None, None),
            thread_summary("thread_older", Some("/tmp/workspace"), Some(100), None),
        ];

        assert_eq!(
            select_latest_thread_id(&threads, Some("/tmp/workspace")),
            Some("thread_newest".to_string())
        );
    }

    #[test]
    fn thread_options_builder_maps_extended_protocol_fields() {
        let collaboration_mode = CollaborationMode::new(
            CollaborationModeKind::Default,
            CollaborationModeSettings::new("gpt-5.2-codex")
                .with_reasoning_effort(ModelReasoningEffort::High),
        );

        let options = ThreadOptions::builder()
            .model("gpt-5.2-codex")
            .model_provider("mock_provider")
            .sandbox_mode(SandboxMode::WorkspaceWrite)
            .sandbox_policy(json!({"type": "dangerFullAccess"}))
            .working_directory("/tmp/workspace")
            .skip_git_repo_check(true)
            .model_reasoning_effort(ModelReasoningEffort::None)
            .model_reasoning_summary(ModelReasoningSummary::Auto)
            .network_access_enabled(true)
            .web_search_mode(WebSearchMode::Live)
            .web_search_enabled(false)
            .approval_policy(ApprovalMode::OnRequest)
            .add_directory("/tmp/one")
            .add_directory("/tmp/two")
            .personality(Personality::Pragmatic)
            .base_instructions("base instructions")
            .developer_instructions("developer instructions")
            .ephemeral(true)
            .insert_config("sandbox_workspace_write.network_access", Value::Bool(true))
            .dynamic_tools(vec![DynamicToolSpec::new(
                "demo_tool",
                "Demo dynamic tool",
                json!({"type": "object"}),
            )])
            .experimental_raw_events(true)
            .persist_extended_history(true)
            .collaboration_mode(collaboration_mode)
            .build();

        let thread_params = build_thread_start_params(&options);
        assert_eq!(thread_params.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(
            thread_params.model_provider.as_deref(),
            Some("mock_provider")
        );
        assert_eq!(thread_params.cwd.as_deref(), Some("/tmp/workspace"));
        assert_eq!(thread_params.approval_policy.as_deref(), Some("on-request"));
        assert_eq!(thread_params.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(
            thread_params.sandbox_policy,
            Some(json!({"type": "dangerFullAccess"}))
        );
        assert_eq!(thread_params.effort.as_deref(), Some("none"));
        assert_eq!(thread_params.summary.as_deref(), Some("auto"));
        assert_eq!(thread_params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(thread_params.ephemeral, Some(true));
        assert_eq!(
            thread_params.base_instructions.as_deref(),
            Some("base instructions")
        );
        assert_eq!(
            thread_params.developer_instructions.as_deref(),
            Some("developer instructions")
        );
        assert_eq!(
            thread_params.extra.get("skipGitRepoCheck"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            thread_params.extra.get("webSearchMode"),
            Some(&Value::String("live".to_string()))
        );
        assert_eq!(
            thread_params.extra.get("webSearchEnabled"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            thread_params.extra.get("networkAccessEnabled"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            thread_params.extra.get("additionalDirectories"),
            Some(&json!(["/tmp/one", "/tmp/two"]))
        );
        assert_eq!(
            thread_params.extra.get("config"),
            Some(&json!({"sandbox_workspace_write.network_access": true}))
        );
        assert_eq!(
            thread_params.extra.get("dynamicTools"),
            Some(&json!([{
                "name": "demo_tool",
                "description": "Demo dynamic tool",
                "inputSchema": {"type": "object"}
            }]))
        );
        assert_eq!(
            thread_params.extra.get("experimentalRawEvents"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            thread_params.extra.get("persistExtendedHistory"),
            Some(&Value::Bool(true))
        );

        let resume_params = build_thread_resume_params("thread_123", &options);
        assert_eq!(resume_params.thread_id, "thread_123");
        assert_eq!(resume_params.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(
            resume_params.model_provider.as_deref(),
            Some("mock_provider")
        );
        assert_eq!(resume_params.cwd.as_deref(), Some("/tmp/workspace"));
        assert_eq!(resume_params.approval_policy.as_deref(), Some("on-request"));
        assert_eq!(resume_params.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(resume_params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(
            resume_params
                .config
                .as_ref()
                .and_then(|config| config.get("sandbox_workspace_write.network_access")),
            Some(&Value::Bool(true))
        );
        assert_eq!(resume_params.persist_extended_history, Some(true));
        assert_eq!(
            resume_params.extra.get("sandboxPolicy"),
            Some(&json!({"type": "dangerFullAccess"}))
        );
        assert_eq!(
            resume_params.extra.get("effort"),
            Some(&Value::String("none".to_string()))
        );
        assert_eq!(
            resume_params.extra.get("summary"),
            Some(&Value::String("auto".to_string()))
        );
        assert_eq!(
            resume_params.extra.get("experimentalRawEvents"),
            Some(&Value::Bool(true))
        );

        let turn_params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &options,
            &TurnOptions::default(),
        );
        assert_eq!(turn_params.model_provider.as_deref(), Some("mock_provider"));
        assert_eq!(turn_params.effort.as_deref(), Some("none"));
        assert_eq!(turn_params.summary.as_deref(), Some("auto"));
        assert_eq!(turn_params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(
            turn_params.sandbox_policy,
            Some(json!({"type": "dangerFullAccess"}))
        );
        assert_eq!(
            turn_params.extra.get("collaborationMode"),
            Some(&json!({
                "mode": "default",
                "settings": {
                    "model": "gpt-5.2-codex",
                    "reasoning_effort": "high"
                }
            }))
        );
    }

    #[test]
    fn thread_options_builder_skip_git_repo_check_matches_cli_flag_semantics() {
        let enabled = ThreadOptions::builder().skip_git_repo_check(true).build();
        let enabled_params = build_thread_start_params(&enabled);
        assert_eq!(
            enabled_params.extra.get("skipGitRepoCheck"),
            Some(&Value::Bool(true))
        );

        let disabled = ThreadOptions::builder().skip_git_repo_check(false).build();
        let disabled_params = build_thread_start_params(&disabled);
        assert_eq!(
            disabled_params.extra.get("skipGitRepoCheck"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn turn_options_builder_sets_typed_output_schema() {
        let turn_options = TurnOptions::builder()
            .output_schema_for::<StructuredReply>()
            .build();
        let turn_params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &ThreadOptions::default(),
            &turn_options,
        );

        assert_eq!(
            turn_params.output_schema,
            Some(StructuredReply::openai_output_schema())
        );
    }

    #[test]
    fn turn_options_builder_clear_output_schema_overrides_previous_value() {
        let turn_options = TurnOptions::builder()
            .output_schema(json!({"type": "object"}))
            .clear_output_schema()
            .build();
        let turn_params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &ThreadOptions::default(),
            &turn_options,
        );

        assert_eq!(turn_params.output_schema, None);
    }

    #[test]
    fn turn_options_value_helpers_set_raw_and_typed_schemas() {
        let raw = TurnOptions::default().with_output_schema(json!({"type": "object"}));
        assert_eq!(raw.output_schema, Some(json!({"type": "object"})));

        let typed = TurnOptions::default().with_output_schema_for::<StructuredReply>();
        assert_eq!(
            typed.output_schema,
            Some(StructuredReply::openai_output_schema())
        );
    }

    #[test]
    fn turn_options_builder_overrides_thread_defaults() {
        let thread_options = ThreadOptions::builder()
            .model("gpt-5-thread-default")
            .model_provider("provider-thread")
            .working_directory("/tmp/thread")
            .model_reasoning_effort(ModelReasoningEffort::Low)
            .model_reasoning_summary(ModelReasoningSummary::Auto)
            .personality(Personality::Friendly)
            .approval_policy(ApprovalMode::OnRequest)
            .sandbox_policy(json!({"thread": true}))
            .skip_git_repo_check(false)
            .network_access_enabled(false)
            .web_search_mode(WebSearchMode::Cached)
            .web_search_enabled(false)
            .add_directory("/tmp/thread-dir")
            .build();

        let turn_options = TurnOptions::builder()
            .model("gpt-5-turn-override")
            .model_provider("provider-turn")
            .working_directory("/tmp/turn")
            .model_reasoning_effort(ModelReasoningEffort::High)
            .model_reasoning_summary(ModelReasoningSummary::Detailed)
            .personality(Personality::Pragmatic)
            .approval_policy(ApprovalMode::Never)
            .sandbox_policy(json!({"turn": true}))
            .skip_git_repo_check(true)
            .network_access_enabled(true)
            .web_search_mode(WebSearchMode::Live)
            .web_search_enabled(true)
            .add_directory("/tmp/turn-dir")
            .insert_extra("customTurnFlag", Value::Bool(true))
            .build();

        let params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &thread_options,
            &turn_options,
        );

        assert_eq!(params.cwd.as_deref(), Some("/tmp/turn"));
        assert_eq!(params.model.as_deref(), Some("gpt-5-turn-override"));
        assert_eq!(params.model_provider.as_deref(), Some("provider-turn"));
        assert_eq!(params.effort.as_deref(), Some("high"));
        assert_eq!(params.summary.as_deref(), Some("detailed"));
        assert_eq!(params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(params.approval_policy.as_deref(), Some("never"));
        assert_eq!(params.sandbox_policy, Some(json!({"turn": true})));
        assert_eq!(
            params.extra.get("skipGitRepoCheck"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            params.extra.get("networkAccessEnabled"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            params.extra.get("webSearchMode"),
            Some(&Value::String("live".to_string()))
        );
        assert_eq!(
            params.extra.get("webSearchEnabled"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            params.extra.get("additionalDirectories"),
            Some(&json!(["/tmp/turn-dir"]))
        );
        assert_eq!(params.extra.get("customTurnFlag"), Some(&Value::Bool(true)));
    }
}
