use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::CodexClient;
use crate::client::StdioConfig;
use crate::client::WsConfig;
use crate::client::{WsServerHandle, WsStartConfig};
use crate::error::ClientError;
use crate::events::{ServerEvent, ServerNotification};
use crate::protocol::methods::codex_rpc_table;
use crate::protocol::{requests, responses};
use crate::schema::OpenAiSerializable;

const THREAD_LIST_PAGE_LIMIT: u32 = 100;
const MAX_THREAD_LIST_PAGES: usize = 100;

/// Declares a wire-facing string enum from a single variant table.
///
/// For each enum this emits the type itself (with per-variant serde renames
/// matching the wire spellings exactly), `as_str`, `Display`, `FromStr`
/// (rejecting anything that is not a wire spelling), and a `VARIANTS` list of
/// all accepted wire spellings.
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $variant:ident => $wire:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $( #[serde(rename = $wire)] $variant, )+
        }

        impl $name {
            /// All accepted wire spellings, in declaration order.
            pub const VARIANTS: &'static [&'static str] = &[$($wire),+];

            /// Returns the wire spelling for this value.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $wire, )+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                match raw {
                    $( $wire => Ok(Self::$variant), )+
                    _ => Err(format!(
                        concat!(
                            "invalid ",
                            stringify!($name),
                            " value '{}'; expected one of: {}"
                        ),
                        raw,
                        Self::VARIANTS.join(", "),
                    )),
                }
            }
        }
    };
}

wire_enum! {
    pub enum ApprovalMode {
        Never => "never",
        OnRequest => "on-request",
        OnFailure => "on-failure",
        Untrusted => "untrusted",
    }
}

wire_enum! {
    pub enum SandboxMode {
        ReadOnly => "read-only",
        WorkspaceWrite => "workspace-write",
        DangerFullAccess => "danger-full-access",
    }
}

wire_enum! {
    pub enum ModelReasoningEffort {
        None => "none",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        XHigh => "xhigh",
        Max => "max",
        Ultra => "ultra",
    }
}

wire_enum! {
    pub enum ModelReasoningSummary {
        None => "none",
        Auto => "auto",
        Concise => "concise",
        Detailed => "detailed",
    }
}

wire_enum! {
    pub enum ModelVerbosity {
        Low => "low",
        Medium => "medium",
        High => "high",
    }
}

wire_enum! {
    /// App-server service tier for thread and turn requests.
    pub enum ServiceTier {
        Default => "default",
        Fast => "fast",
    }
}

wire_enum! {
    pub enum Personality {
        None => "none",
        Friendly => "friendly",
        Pragmatic => "pragmatic",
    }
}

wire_enum! {
    pub enum WebSearchMode {
        Disabled => "disabled",
        Cached => "cached",
        Live => "live",
    }
}

wire_enum! {
    pub enum CollaborationModeKind {
        Plan => "plan",
        Default => "default",
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

/// Declares an options struct and its builder from a single
/// `(setter-kind, field, type)` table.
///
/// Setter kinds:
/// - `into_string`: setter takes `impl Into<String>`
/// - `copy`: setter takes the field type by value (for `Copy` types)
/// - `value`: setter takes the field type by value (for owned types)
/// - `bool`: setter takes `bool`
/// - `vec_push <method>`: whole-`Vec` setter plus a per-entry push method
/// - `map_insert <method>`: whole-`Map` setter plus a per-key insert method
macro_rules! options_builder {
    (
        $options:ident, $builder:ident, {
            $( [$($kind:tt)+] $field:ident: $ty:ty; )+
        }
    ) => {
        #[derive(Debug, Clone, Default)]
        pub struct $options {
            $( pub $field: Option<$ty>, )+
        }

        impl $options {
            pub fn builder() -> $builder {
                $builder::new()
            }
        }

        #[derive(Debug, Clone, Default)]
        pub struct $builder {
            options: $options,
        }

        impl $builder {
            pub fn new() -> Self {
                Self::default()
            }

            pub fn build(self) -> $options {
                self.options
            }

            $( options_builder!(@setter [$($kind)+] $field: $ty); )+
        }
    };
    (@setter [into_string] $field:ident: $ty:ty) => {
        pub fn $field(mut self, $field: impl Into<String>) -> Self {
            self.options.$field = Some($field.into());
            self
        }
    };
    (@setter [copy] $field:ident: $ty:ty) => {
        options_builder!(@setter [value] $field: $ty);
    };
    (@setter [value] $field:ident: $ty:ty) => {
        pub fn $field(mut self, $field: $ty) -> Self {
            self.options.$field = Some($field);
            self
        }
    };
    (@setter [bool] $field:ident: $ty:ty) => {
        pub fn $field(mut self, enabled: bool) -> Self {
            self.options.$field = Some(enabled);
            self
        }
    };
    (@setter [vec_push $push_method:ident] $field:ident: $ty:ty) => {
        options_builder!(@setter [value] $field: $ty);

        pub fn $push_method(mut self, entry: impl Into<String>) -> Self {
            self.options
                .$field
                .get_or_insert_with(Vec::new)
                .push(entry.into());
            self
        }
    };
    (@setter [map_insert $insert_method:ident] $field:ident: $ty:ty) => {
        options_builder!(@setter [value] $field: $ty);

        pub fn $insert_method(mut self, key: impl Into<String>, value: Value) -> Self {
            self.options
                .$field
                .get_or_insert_with(Map::new)
                .insert(key.into(), value);
            self
        }
    };
}

options_builder!(ThreadOptions, ThreadOptionsBuilder, {
    [into_string] model: String;
    [into_string] model_provider: String;
    [copy] sandbox_mode: SandboxMode;
    [value] sandbox_policy: Value;
    [into_string] working_directory: String;
    [bool] skip_git_repo_check: bool;
    [copy] model_reasoning_effort: ModelReasoningEffort;
    [copy] model_reasoning_summary: ModelReasoningSummary;
    [copy] service_tier: ServiceTier;
    [bool] network_access_enabled: bool;
    [copy] web_search_mode: WebSearchMode;
    [bool] web_search_enabled: bool;
    [copy] approval_policy: ApprovalMode;
    [vec_push add_directory] additional_directories: Vec<String>;
    [copy] personality: Personality;
    [into_string] base_instructions: String;
    [into_string] developer_instructions: String;
    [bool] ephemeral: bool;
    [value] collaboration_mode: CollaborationMode;
    [map_insert insert_config] config: Map<String, Value>;
    [value] dynamic_tools: Vec<DynamicToolSpec>;
    [bool] experimental_raw_events: bool;
    [bool] persist_extended_history: bool;
});

options_builder!(TurnOptions, TurnOptionsBuilder, {
    [value] output_schema: Value;
    [into_string] working_directory: String;
    [into_string] model: String;
    [copy] model_reasoning_effort: ModelReasoningEffort;
    [copy] model_reasoning_summary: ModelReasoningSummary;
    [copy] service_tier: ServiceTier;
    [copy] personality: Personality;
    [copy] approval_policy: ApprovalMode;
    [value] sandbox_policy: Value;
    [value] collaboration_mode: CollaborationMode;
    [bool] skip_git_repo_check: bool;
    [bool] network_access_enabled: bool;
    [vec_push add_directory] additional_directories: Vec<String>;
    [map_insert insert_extra] extra: Map<String, Value>;
});

impl TurnOptionsBuilder {
    pub fn output_schema_for<T: OpenAiSerializable>(mut self) -> Self {
        self.options.output_schema = Some(T::openai_output_schema());
        self
    }

    pub fn clear_output_schema(mut self) -> Self {
        self.options.output_schema = None;
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

pub struct StreamedTurn {
    receiver: mpsc::Receiver<Result<ThreadEvent, ClientError>>,
    task: JoinHandle<()>,
    turn_id: String,
}

impl StreamedTurn {
    /// Returns the app-server turn ID for this stream.
    ///
    /// Callers can pass this ID to [`Thread::interrupt`] while continuing to
    /// drain the stream until its terminal event arrives.
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
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted,
    TurnCompleted {
        usage: Option<Usage>,
        /// Native app-server terminal status. Older servers can omit this field.
        terminal_status: Option<String>,
    },
    TurnFailed {
        error: ThreadError,
    },
    ItemStarted {
        item: ThreadItem,
    },
    ItemUpdated {
        item: ThreadItem,
    },
    ItemCompleted {
        item: ThreadItem,
    },
    Error {
        message: String,
    },
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

/// Expands the shared RPC method table
/// ([`codex_rpc_table!`](crate::protocol::methods)) into `Codex`'s forwards:
/// each generated method completes the app-server handshake via
/// `ensure_initialized` and then delegates to the `CodexClient` method of
/// the same name. One recursive arm per row kind (`typed`, `null`, `alias`);
/// `alias` rows delegate to another `Codex` method, which already ensures
/// initialization.
macro_rules! define_codex_forwards {
    () => {};
    (
        $(#[$doc:meta])*
        typed $fn_name:ident, $method:literal, $params_ty:ty, $result_ty:ty;
        $($rest:tt)*
    ) => {
        $(#[$doc])*
        pub async fn $fn_name(&self, params: $params_ty) -> Result<$result_ty, ClientError> {
            self.ensure_initialized().await?;
            self.inner.client.$fn_name(params).await
        }

        define_codex_forwards! { $($rest)* }
    };
    (
        $(#[$doc:meta])*
        null $fn_name:ident, $method:literal, $result_ty:ty;
        $($rest:tt)*
    ) => {
        $(#[$doc])*
        pub async fn $fn_name(&self) -> Result<$result_ty, ClientError> {
            self.ensure_initialized().await?;
            self.inner.client.$fn_name().await
        }

        define_codex_forwards! { $($rest)* }
    };
    (
        $(#[$doc:meta])*
        alias $fn_name:ident => $target:ident, $params_ty:ty, $result_ty:ty;
        $($rest:tt)*
    ) => {
        $(#[$doc])*
        pub async fn $fn_name(&self, params: $params_ty) -> Result<$result_ty, ClientError> {
            self.$target(params).await
        }

        define_codex_forwards! { $($rest)* }
    };
}

#[derive(Clone)]
pub struct Codex {
    inner: Arc<CodexInner>,
}

struct CodexInner {
    client: CodexClient,
    initialize_params: requests::InitializeParams,
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
            }),
        }
    }

    pub fn from_client(client: CodexClient) -> Self {
        Self::with_initialize_params(client, crate::client::default_initialize_params())
    }

    pub async fn spawn_stdio(config: StdioConfig) -> Result<Self, ClientError> {
        let client = CodexClient::spawn_stdio(config).await?;
        Ok(Self::from_client(client))
    }

    pub async fn connect_ws(config: WsConfig) -> Result<Self, ClientError> {
        let client = CodexClient::connect_ws(config).await?;
        Ok(Self::from_client(client))
    }

    pub async fn start_ws_daemon(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        CodexClient::start_ws_daemon(config).await
    }

    pub async fn start_ws_blocking(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        CodexClient::start_ws_blocking(config).await
    }

    /// See [`CodexClient::start_and_connect_ws`]: `env` is passed to any
    /// daemon this call spawns for a managed loopback URL.
    pub async fn start_and_connect_ws(
        config: WsConfig,
        env: HashMap<String, String>,
    ) -> Result<Self, ClientError> {
        let client = CodexClient::start_and_connect_ws(config, env).await?;
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

    codex_rpc_table!(define_codex_forwards);

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
        self.inner
            .client
            .ensure_ready_with(&self.inner.initialize_params)
            .await
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

        // `rx` is held locally until `StreamedTurn` is returned, so these
        // sends cannot fail.
        if let Some(started_thread_id) = emit_thread_started {
            tx.send(Ok(ThreadEvent::ThreadStarted {
                thread_id: started_thread_id,
            }))
            .await
            .expect("receiver held locally");
        }
        tx.send(Ok(ThreadEvent::TurnStarted))
            .await
            .expect("receiver held locally");

        let stream_turn_id = turn_id.clone();
        let task = tokio::spawn(async move {
            pump_turn_events(server_events, tx, thread_id, stream_turn_id).await;
        });

        Ok(StreamedTurn {
            receiver: rx,
            task,
            turn_id,
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
                ThreadEvent::TurnCompleted {
                    usage: completed, ..
                } => {
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

/// Converts a streaming delta notification into an `ItemUpdated` event, or
/// `None` when the delta carries no text.
fn delta_item_updated(
    delta: crate::protocol::notifications::DeltaNotification,
    build_item: fn(id: String, text: String) -> ThreadItem,
) -> Option<ThreadEvent> {
    let text = delta.delta.or(delta.text).unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    Some(ThreadEvent::ItemUpdated {
        item: build_item(delta.item_id.unwrap_or_default(), text),
    })
}

async fn pump_turn_events(
    mut server_events: tokio::sync::broadcast::Receiver<ServerEvent>,
    tx: mpsc::Sender<Result<ThreadEvent, ClientError>>,
    thread_id: String,
    turn_id: String,
) {
    let mut latest_usage: Option<Usage> = None;

    // Sends the event to the consumer, breaking out of the pump loop when the
    // consumer is gone.
    macro_rules! send_or_break {
        ($event:expr) => {
            if tx.send($event).await.is_err() {
                break;
            }
        };
    }

    loop {
        let next = server_events.recv().await;
        let server_event = match next {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                send_or_break!(Err(ClientError::TransportClosed));
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
                    send_or_break!(Ok(ThreadEvent::ItemStarted {
                        item: parse_thread_item(payload.item),
                    }));
                }
                ServerNotification::ItemCompleted(payload)
                    if matches_target_from_extra(&payload.extra, &thread_id, Some(&turn_id)) =>
                {
                    send_or_break!(Ok(ThreadEvent::ItemCompleted {
                        item: parse_thread_item(payload.item),
                    }));
                }
                ServerNotification::ItemAgentMessageDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let Some(event) = delta_item_updated(delta, |id, text| {
                        ThreadItem::AgentMessage(AgentMessageItem {
                            id,
                            text,
                            phase: None,
                        })
                    }) else {
                        continue;
                    };
                    send_or_break!(Ok(event));
                }
                ServerNotification::ItemPlanDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let Some(event) = delta_item_updated(delta, |id, text| {
                        ThreadItem::Plan(PlanItem { id, text })
                    }) else {
                        continue;
                    };
                    send_or_break!(Ok(event));
                }
                ServerNotification::ItemReasoningSummaryTextDelta(delta)
                | ServerNotification::ItemReasoningTextDelta(delta)
                    if matches_target_from_extra(&delta.extra, &thread_id, Some(&turn_id)) =>
                {
                    let Some(event) = delta_item_updated(delta, |id, text| {
                        ThreadItem::Reasoning(ReasoningItem { id, text })
                    }) else {
                        continue;
                    };
                    send_or_break!(Ok(event));
                }
                ServerNotification::TurnCompleted(payload)
                    if payload.turn.id == turn_id
                        && matches_target_from_extra(
                            &payload.turn.extra,
                            &thread_id,
                            Some(&turn_id),
                        ) =>
                {
                    let terminal_status =
                        payload.turn.status.map(|status| status.trim().to_owned());
                    let status = terminal_status
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if status == "failed" {
                        let message = payload
                            .turn
                            .error
                            .map(|error| error.message)
                            .unwrap_or_else(|| "turn failed".to_string());
                        send_or_break!(Ok(ThreadEvent::TurnFailed {
                            error: ThreadError { message },
                        }));
                        break;
                    }

                    let usage = parse_usage_from_turn_extra(&payload.turn.extra)
                        .or_else(|| latest_usage.clone());
                    send_or_break!(Ok(ThreadEvent::TurnCompleted {
                        usage,
                        terminal_status,
                    }));
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
                    send_or_break!(Ok(ThreadEvent::Error {
                        message: payload.error.message,
                    }));
                }
                _ => {}
            },
            ServerEvent::TransportClosed => {
                send_or_break!(Err(ClientError::TransportClosed));
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

/// Options shared by thread/start, thread/resume, and turn/start wire
/// encoding, after per-turn overrides (if any) have been merged over the
/// thread-level defaults.
#[derive(Debug, Clone, Default, PartialEq)]
struct ResolvedOptions {
    model: Option<String>,
    working_directory: Option<String>,
    model_reasoning_summary: Option<ModelReasoningSummary>,
    service_tier: Option<ServiceTier>,
    personality: Option<Personality>,
    approval_policy: Option<ApprovalMode>,
    sandbox_policy: Option<Value>,
    skip_git_repo_check: Option<bool>,
    network_access_enabled: Option<bool>,
    additional_directories: Option<Vec<String>>,
    collaboration_mode: Option<CollaborationMode>,
}

impl ResolvedOptions {
    fn from_thread(options: &ThreadOptions) -> Self {
        Self {
            model: options.model.clone(),
            working_directory: options.working_directory.clone(),
            model_reasoning_summary: options.model_reasoning_summary,
            service_tier: options.service_tier,
            personality: options.personality,
            approval_policy: options.approval_policy,
            sandbox_policy: options.sandbox_policy.clone(),
            skip_git_repo_check: options.skip_git_repo_check,
            network_access_enabled: options.network_access_enabled,
            additional_directories: options.additional_directories.clone(),
            collaboration_mode: options.collaboration_mode.clone(),
        }
    }

    /// Merges per-turn overrides over thread-level defaults: any field set on
    /// `turn_options` wins; otherwise the thread-level value applies.
    fn merge(options: &ThreadOptions, turn_options: &TurnOptions) -> Self {
        Self {
            model: turn_options.model.clone().or_else(|| options.model.clone()),
            working_directory: turn_options
                .working_directory
                .clone()
                .or_else(|| options.working_directory.clone()),
            model_reasoning_summary: turn_options
                .model_reasoning_summary
                .or(options.model_reasoning_summary),
            service_tier: turn_options.service_tier.or(options.service_tier),
            personality: turn_options.personality.or(options.personality),
            approval_policy: turn_options.approval_policy.or(options.approval_policy),
            sandbox_policy: turn_options
                .sandbox_policy
                .clone()
                .or_else(|| options.sandbox_policy.clone()),
            skip_git_repo_check: turn_options
                .skip_git_repo_check
                .or(options.skip_git_repo_check),
            network_access_enabled: turn_options
                .network_access_enabled
                .or(options.network_access_enabled),
            additional_directories: turn_options
                .additional_directories
                .clone()
                .or_else(|| options.additional_directories.clone()),
            collaboration_mode: turn_options
                .collaboration_mode
                .clone()
                .or_else(|| options.collaboration_mode.clone()),
        }
    }
}

/// Inserts the extra-map keys shared by thread/start, thread/resume, and
/// turn/start. Per-method extras (e.g. `sandboxPolicy` on resume) are inserted
/// by the individual builders.
fn insert_common_extras(extra: &mut Map<String, Value>, resolved: &ResolvedOptions) {
    if let Some(service_tier) = resolved.service_tier {
        extra.insert(
            "serviceTier".to_string(),
            Value::String(service_tier.as_str().to_string()),
        );
    }
    if let Some(skip) = resolved.skip_git_repo_check {
        extra.insert("skipGitRepoCheck".to_string(), Value::Bool(skip));
    }
    if let Some(network) = resolved.network_access_enabled {
        extra.insert("networkAccessEnabled".to_string(), Value::Bool(network));
    }
    if let Some(additional) = &resolved.additional_directories {
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
    if let Some(collaboration_mode) = &resolved.collaboration_mode {
        extra.insert(
            "collaborationMode".to_string(),
            collaboration_mode.as_value(),
        );
    }
}

/// Builds thread configuration without discarding generic config keys.
/// Typed SDK settings win over a duplicate generic key. An explicit
/// `web_search_mode` wins over the boolean compatibility setting.
fn thread_config(options: &ThreadOptions) -> Option<Map<String, Value>> {
    let mut config = options.config.clone().unwrap_or_default();

    if let Some(effort) = options.model_reasoning_effort {
        config.insert(
            "model_reasoning_effort".to_string(),
            Value::String(effort.as_str().to_string()),
        );
    }
    if let Some(web_search) = options.web_search_mode.or_else(|| {
        options.web_search_enabled.map(|enabled| {
            if enabled {
                WebSearchMode::Live
            } else {
                WebSearchMode::Disabled
            }
        })
    }) {
        config.insert(
            "web_search".to_string(),
            Value::String(web_search.as_str().to_string()),
        );
    }

    (!config.is_empty()).then_some(config)
}

fn dynamic_tools_value(dynamic_tools: &[DynamicToolSpec]) -> Value {
    Value::Array(
        dynamic_tools
            .iter()
            .map(DynamicToolSpec::as_value)
            .collect(),
    )
}

fn build_thread_start_params(options: &ThreadOptions) -> requests::ThreadStartParams {
    let mut extra = Map::new();
    insert_common_extras(&mut extra, &ResolvedOptions::from_thread(options));
    if let Some(dynamic_tools) = &options.dynamic_tools {
        extra.insert(
            "dynamicTools".to_string(),
            dynamic_tools_value(dynamic_tools),
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
        config: thread_config(options),
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
    insert_common_extras(&mut extra, &ResolvedOptions::from_thread(options));
    if let Some(policy) = &options.sandbox_policy {
        extra.insert("sandboxPolicy".to_string(), policy.clone());
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
        config: thread_config(options),
        base_instructions: options.base_instructions.clone(),
        developer_instructions: options.developer_instructions.clone(),
        personality: options.personality.map(|value| value.as_str().to_string()),
        persist_extended_history: options.persist_extended_history,
        extra,
    }
}

const TURN_START_TYPED_OR_FORBIDDEN_EXTRA_KEYS: &[&str] = &[
    "threadId",
    "input",
    "cwd",
    "model",
    "modelProvider",
    "effort",
    "summary",
    "personality",
    "outputSchema",
    "approvalPolicy",
    "sandboxPolicy",
    "config",
    "webSearch",
    "webSearchEnabled",
    "webSearchMode",
    "web_search",
];

fn build_turn_start_params(
    thread_id: &str,
    input: Input,
    options: &ThreadOptions,
    turn_options: &TurnOptions,
) -> requests::TurnStartParams {
    let resolved = ResolvedOptions::merge(options, turn_options);

    let mut extra = Map::new();
    insert_common_extras(&mut extra, &resolved);
    if let Some(extra_overrides) = &turn_options.extra {
        for (key, value) in extra_overrides {
            if !TURN_START_TYPED_OR_FORBIDDEN_EXTRA_KEYS.contains(&key.as_str()) {
                extra.insert(key.clone(), value.clone());
            }
        }
    }

    requests::TurnStartParams {
        thread_id: thread_id.to_string(),
        input: normalize_input(input),
        cwd: resolved.working_directory,
        model: resolved.model,
        effort: turn_options
            .model_reasoning_effort
            .map(|effort| effort.as_str().to_string()),
        summary: resolved
            .model_reasoning_summary
            .map(|summary| summary.as_str().to_string()),
        personality: resolved.personality.map(|value| value.as_str().to_string()),
        output_schema: turn_options.output_schema.clone(),
        approval_policy: resolved
            .approval_policy
            .map(|mode| mode.as_str().to_string()),
        sandbox_policy: resolved.sandbox_policy,
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

impl CommandExecutionStatus {
    /// Canonical wire spelling; `Unknown` renders as `"unknown"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InProgress => "inProgress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Unknown => "unknown",
        }
    }
}

impl PatchChangeKind {
    /// Canonical wire spelling; `Unknown` renders as `"unknown"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Delete => "delete",
            Self::Update => "update",
            Self::Unknown => "unknown",
        }
    }
}

impl PatchApplyStatus {
    /// Canonical wire spelling; `Unknown` renders as `"unknown"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InProgress => "inProgress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Declined => "declined",
            Self::Unknown => "unknown",
        }
    }
}

impl McpToolCallStatus {
    /// Canonical wire spelling; `Unknown` renders as `"unknown"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InProgress => "inProgress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
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
mod request_serialization_tests;

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

    macro_rules! wire_enum_round_trip_test {
        ($test_name:ident, $ty:ident, [$($variant:ident),+ $(,)?], [$($wire:literal),+ $(,)?]) => {
            #[test]
            fn $test_name() {
                assert_eq!($ty::VARIANTS, [$($wire),+]);

                let values = [$($ty::$variant),+];
                assert_eq!(values.len(), $ty::VARIANTS.len());
                for value in values {
                    assert_eq!(value.as_str().parse::<$ty>(), Ok(value));
                    assert_eq!(
                        serde_json::to_value(value)
                            .expect("serialize wire enum")
                            .as_str(),
                        Some(value.as_str())
                    );
                    assert_eq!(value.to_string(), value.as_str());
                }

                let error = "not-a-real-value"
                    .parse::<$ty>()
                    .expect_err("unknown wire value must be rejected");
                for wire in $ty::VARIANTS {
                    assert!(error.contains(wire), "error {error:?} must list {wire}");
                }
            }
        };
    }

    wire_enum_round_trip_test!(
        approval_mode_round_trips,
        ApprovalMode,
        [Never, OnRequest, OnFailure, Untrusted],
        ["never", "on-request", "on-failure", "untrusted"]
    );
    wire_enum_round_trip_test!(
        sandbox_mode_round_trips,
        SandboxMode,
        [ReadOnly, WorkspaceWrite, DangerFullAccess],
        ["read-only", "workspace-write", "danger-full-access"]
    );
    wire_enum_round_trip_test!(
        model_reasoning_effort_round_trips,
        ModelReasoningEffort,
        [None, Minimal, Low, Medium, High, XHigh, Max, Ultra],
        [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
        ]
    );
    wire_enum_round_trip_test!(
        model_reasoning_summary_round_trips,
        ModelReasoningSummary,
        [None, Auto, Concise, Detailed],
        ["none", "auto", "concise", "detailed"]
    );
    wire_enum_round_trip_test!(
        model_verbosity_round_trips,
        ModelVerbosity,
        [Low, Medium, High],
        ["low", "medium", "high"]
    );
    wire_enum_round_trip_test!(
        service_tier_round_trips,
        ServiceTier,
        [Default, Fast],
        ["default", "fast"]
    );
    wire_enum_round_trip_test!(
        personality_round_trips,
        Personality,
        [None, Friendly, Pragmatic],
        ["none", "friendly", "pragmatic"]
    );
    wire_enum_round_trip_test!(
        web_search_mode_round_trips,
        WebSearchMode,
        [Disabled, Cached, Live],
        ["disabled", "cached", "live"]
    );
    wire_enum_round_trip_test!(
        collaboration_mode_kind_round_trips,
        CollaborationModeKind,
        [Plan, Default],
        ["plan", "default"]
    );

    #[test]
    fn public_string_enums_serde_as_codex_config_values() {
        #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
        struct ConfigValues {
            approval_policy: ApprovalMode,
            sandbox_mode: SandboxMode,
            model_reasoning_effort: ModelReasoningEffort,
            model_reasoning_summary: ModelReasoningSummary,
            service_tier: ServiceTier,
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
service_tier = "fast"
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
                service_tier: ServiceTier::Fast,
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
                "service_tier": "fast",
                "personality": "pragmatic",
                "web_search": "live",
                "collaboration_mode": "plan",
            })
        );
    }

    #[test]
    fn max_and_ultra_reasoning_efforts_serde_as_codex_values() {
        for (effort, expected) in [
            (ModelReasoningEffort::Max, "max"),
            (ModelReasoningEffort::Ultra, "ultra"),
        ] {
            assert_eq!(
                serde_json::to_value(effort).expect("serialize reasoning effort"),
                json!(expected)
            );
            assert_eq!(
                serde_json::from_value::<ModelReasoningEffort>(json!(expected))
                    .expect("deserialize reasoning effort"),
                effort
            );
        }
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

    #[tokio::test]
    async fn pump_turn_events_emits_agent_message_and_plan_deltas() {
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
                ServerNotification::ItemAgentMessageDelta(
                    crate::protocol::notifications::DeltaNotification {
                        item_id: Some("msg_1".to_string()),
                        delta: Some("hello".to_string()),
                        text: None,
                        summary_index: None,
                        extra: extra.clone(),
                    },
                ),
            ))
            .expect("send agent message delta");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("agent message event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::ItemUpdated {
                item: ThreadItem::AgentMessage(AgentMessageItem {
                    id: "msg_1".to_string(),
                    text: "hello".to_string(),
                    phase: None,
                }),
            }
        );

        server_tx
            .send(ServerEvent::Notification(
                ServerNotification::ItemPlanDelta(
                    crate::protocol::notifications::DeltaNotification {
                        item_id: Some("plan_1".to_string()),
                        delta: None,
                        text: Some("1. inspect".to_string()),
                        summary_index: None,
                        extra,
                    },
                ),
            ))
            .expect("send plan delta");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("plan event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::ItemUpdated {
                item: ThreadItem::Plan(PlanItem {
                    id: "plan_1".to_string(),
                    text: "1. inspect".to_string(),
                }),
            }
        );

        pump.abort();
    }

    async fn assert_completed_terminal_status(
        wire_status: Option<&str>,
        expected_status: Option<&str>,
    ) {
        let (server_tx, server_rx) = tokio::sync::broadcast::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let pump = tokio::spawn(pump_turn_events(
            server_rx,
            event_tx,
            "thread_1".to_string(),
            "turn_1".to_string(),
        ));

        let mut turn_extra = Map::new();
        turn_extra.insert(
            "threadId".to_string(),
            Value::String("thread_1".to_string()),
        );
        turn_extra.insert("turnId".to_string(), Value::String("turn_1".to_string()));
        server_tx
            .send(ServerEvent::Notification(
                ServerNotification::TurnCompleted(
                    crate::protocol::notifications::TurnCompletedNotification {
                        turn: responses::Turn {
                            id: "turn_1".to_string(),
                            status: wire_status.map(str::to_owned),
                            extra: turn_extra,
                            ..Default::default()
                        },
                        extra: Map::new(),
                    },
                ),
            ))
            .expect("send turn completion");

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("turn completed event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::TurnCompleted {
                usage: None,
                terminal_status: expected_status.map(str::to_owned),
            }
        );

        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .expect("pump task ends")
            .expect("pump task join");
    }

    #[tokio::test]
    async fn pump_turn_events_preserves_completed_terminal_status() {
        assert_completed_terminal_status(Some("completed"), Some("completed")).await;
    }

    #[tokio::test]
    async fn pump_turn_events_preserves_interrupted_terminal_status() {
        assert_completed_terminal_status(Some("interrupted"), Some("interrupted")).await;
    }

    #[tokio::test]
    async fn pump_turn_events_preserves_absent_terminal_status() {
        assert_completed_terminal_status(None, None).await;
    }

    #[tokio::test]
    async fn pump_turn_events_emits_turn_failed_and_terminates() {
        let (server_tx, server_rx) = tokio::sync::broadcast::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let pump = tokio::spawn(pump_turn_events(
            server_rx,
            event_tx,
            "thread_1".to_string(),
            "turn_1".to_string(),
        ));

        let mut turn_extra = Map::new();
        turn_extra.insert(
            "threadId".to_string(),
            Value::String("thread_1".to_string()),
        );
        turn_extra.insert("turnId".to_string(), Value::String("turn_1".to_string()));

        server_tx
            .send(ServerEvent::Notification(
                ServerNotification::TurnCompleted(
                    crate::protocol::notifications::TurnCompletedNotification {
                        turn: responses::Turn {
                            id: "turn_1".to_string(),
                            status: Some("failed".to_string()),
                            error: Some(responses::TurnError {
                                message: "boom".to_string(),
                                ..Default::default()
                            }),
                            extra: turn_extra,
                            ..Default::default()
                        },
                        extra: Map::new(),
                    },
                ),
            ))
            .expect("send failed turn completion");

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("turn failed event")
                .expect("event channel open")
                .expect("thread event ok"),
            ThreadEvent::TurnFailed {
                error: ThreadError {
                    message: "boom".to_string(),
                },
            }
        );

        // The pump terminates after the terminal event, closing the channel.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("channel close")
                .is_none()
        );
        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .expect("pump task ends")
            .expect("pump task join");
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
            .service_tier(ServiceTier::Fast)
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
        assert_eq!(thread_params.summary.as_deref(), Some("auto"));
        assert_eq!(thread_params.extra.get("serviceTier"), Some(&json!("fast")));
        assert_eq!(thread_params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(thread_params.ephemeral, Some(true));
        assert_eq!(
            thread_params
                .config
                .as_ref()
                .and_then(|config| config.get("model_reasoning_effort")),
            Some(&json!("none"))
        );
        assert_eq!(
            thread_params
                .config
                .as_ref()
                .and_then(|config| config.get("web_search")),
            Some(&json!("live"))
        );
        assert!(thread_params.extra.get("webSearchMode").is_none());
        assert!(thread_params.extra.get("webSearchEnabled").is_none());
        assert_eq!(
            thread_params.extra.get("dynamicTools"),
            Some(&json!([{
                "name": "demo_tool",
                "description": "Demo dynamic tool",
                "inputSchema": {"type": "object"}
            }]))
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
        assert_eq!(
            resume_params
                .config
                .as_ref()
                .and_then(|config| config.get("model_reasoning_effort")),
            Some(&json!("none"))
        );
        assert_eq!(
            resume_params
                .config
                .as_ref()
                .and_then(|config| config.get("web_search")),
            Some(&json!("live"))
        );
        assert!(resume_params.extra.get("effort").is_none());
        assert!(resume_params.extra.get("dynamicTools").is_none());

        let turn_params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &options,
            &TurnOptions::default(),
        );
        assert!(turn_params.effort.is_none());
        assert!(turn_params.extra.get("webSearchMode").is_none());
        assert!(turn_params.extra.get("webSearchEnabled").is_none());
        assert_eq!(turn_params.summary.as_deref(), Some("auto"));
        assert_eq!(turn_params.personality.as_deref(), Some("pragmatic"));
        assert_eq!(turn_params.extra.get("serviceTier"), Some(&json!("fast")));
        assert_eq!(
            turn_params.sandbox_policy,
            Some(json!({"type": "dangerFullAccess"}))
        );
    }

    fn extra_affecting_thread_options() -> ThreadOptions {
        ThreadOptions::builder()
            .skip_git_repo_check(true)
            .web_search_mode(WebSearchMode::Live)
            .web_search_enabled(false)
            .network_access_enabled(true)
            .add_directory("/tmp/one")
            .sandbox_policy(json!({"type": "dangerFullAccess"}))
            .model_reasoning_effort(ModelReasoningEffort::Low)
            .model_reasoning_summary(ModelReasoningSummary::Auto)
            .service_tier(ServiceTier::Fast)
            .ephemeral(true)
            .collaboration_mode(CollaborationMode::new(
                CollaborationModeKind::Plan,
                CollaborationModeSettings::new("gpt-5.2-codex"),
            ))
            .insert_config("profile", Value::String("test".to_string()))
            .dynamic_tools(vec![DynamicToolSpec::new(
                "demo_tool",
                "Demo dynamic tool",
                json!({"type": "object"}),
            )])
            .experimental_raw_events(true)
            .persist_extended_history(true)
            .build()
    }

    fn sorted_extra_keys(extra: &Map<String, Value>) -> Vec<&str> {
        let mut keys: Vec<&str> = extra.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn thread_start_params_extra_encodes_collaboration_mode_and_pins_key_set() {
        let options = extra_affecting_thread_options();
        let params = build_thread_start_params(&options);

        assert_eq!(
            params.extra.get("collaborationMode"),
            Some(&json!({
                "mode": "plan",
                "settings": { "model": "gpt-5.2-codex" }
            }))
        );
        // Thread defaults belong in `config`; legacy search extras do not.
        assert_eq!(
            sorted_extra_keys(&params.extra),
            vec![
                "additionalDirectories",
                "collaborationMode",
                "dynamicTools",
                "experimentalRawEvents",
                "networkAccessEnabled",
                "persistExtendedHistory",
                "serviceTier",
                "skipGitRepoCheck",
            ]
        );
    }

    #[test]
    fn thread_resume_params_extra_pins_key_set() {
        let options = extra_affecting_thread_options();
        let params = build_thread_resume_params("thread_123", &options);

        assert_eq!(
            params.extra.get("collaborationMode"),
            Some(&json!({
                "mode": "plan",
                "settings": { "model": "gpt-5.2-codex" }
            }))
        );
        // `dynamicTools` and legacy thread-default fields are absent on resume.
        assert_eq!(
            sorted_extra_keys(&params.extra),
            vec![
                "additionalDirectories",
                "collaborationMode",
                "ephemeral",
                "experimentalRawEvents",
                "networkAccessEnabled",
                "sandboxPolicy",
                "serviceTier",
                "skipGitRepoCheck",
                "summary",
            ]
        );
    }

    #[test]
    fn turn_start_params_extra_pins_key_set() {
        let options = extra_affecting_thread_options();
        let params = build_turn_start_params(
            "thread_123",
            Input::text("hello"),
            &options,
            &TurnOptions::default(),
        );

        assert_eq!(
            params.extra.get("collaborationMode"),
            Some(&json!({
                "mode": "plan",
                "settings": { "model": "gpt-5.2-codex" }
            }))
        );
        assert_eq!(
            sorted_extra_keys(&params.extra),
            vec![
                "additionalDirectories",
                "collaborationMode",
                "networkAccessEnabled",
                "serviceTier",
                "skipGitRepoCheck",
            ]
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
    fn service_tier_encodes_for_thread_start_resume_and_turn_start() {
        for service_tier in [ServiceTier::Default, ServiceTier::Fast] {
            let options = ThreadOptions::builder().service_tier(service_tier).build();
            let expected = json!(service_tier.as_str());

            let start = build_thread_start_params(&options);
            assert_eq!(start.extra.get("serviceTier"), Some(&expected));

            let resume = build_thread_resume_params("thread_123", &options);
            assert_eq!(resume.extra.get("serviceTier"), Some(&expected));

            let turn = build_turn_start_params(
                "thread_123",
                Input::text("hello"),
                &options,
                &TurnOptions::default(),
            );
            assert_eq!(turn.extra.get("serviceTier"), Some(&expected));
        }
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
        let raw = TurnOptions::builder()
            .output_schema(json!({"type": "object"}))
            .build();
        assert_eq!(raw.output_schema, Some(json!({"type": "object"})));

        let typed = TurnOptions::builder()
            .output_schema_for::<StructuredReply>()
            .build();
        assert_eq!(
            typed.output_schema,
            Some(StructuredReply::openai_output_schema())
        );
    }

    #[test]
    fn resolved_options_merge_prefers_turn_values_and_falls_back_to_thread() {
        let thread_options = ThreadOptions::builder()
            .model("thread-model")
            .model_provider("thread-provider")
            .working_directory("/tmp/thread")
            .model_reasoning_effort(ModelReasoningEffort::Low)
            .service_tier(ServiceTier::Default)
            .personality(Personality::Friendly)
            .approval_policy(ApprovalMode::OnRequest)
            .sandbox_policy(json!({"thread": true}))
            .skip_git_repo_check(false)
            .web_search_mode(WebSearchMode::Cached)
            .web_search_enabled(false)
            .network_access_enabled(false)
            .add_directory("/tmp/thread-dir")
            .collaboration_mode(CollaborationMode::new(
                CollaborationModeKind::Plan,
                CollaborationModeSettings::new("thread-collab"),
            ))
            .model_reasoning_summary(ModelReasoningSummary::Auto)
            .build();

        // Fields set on the turn win over thread-level defaults.
        let turn_options = TurnOptions::builder()
            .model("turn-model")
            .model_reasoning_effort(ModelReasoningEffort::High)
            .service_tier(ServiceTier::Fast)
            .sandbox_policy(json!({"turn": true}))
            .skip_git_repo_check(true)
            .add_directory("/tmp/turn-dir")
            .build();

        let merged = ResolvedOptions::merge(&thread_options, &turn_options);
        assert_eq!(merged.model.as_deref(), Some("turn-model"));
        assert_eq!(merged.service_tier, Some(ServiceTier::Fast));
        assert_eq!(merged.sandbox_policy, Some(json!({"turn": true})));
        assert_eq!(merged.skip_git_repo_check, Some(true));
        assert_eq!(
            merged.additional_directories,
            Some(vec!["/tmp/turn-dir".to_string()])
        );
        // Fields not set on the turn fall back to the thread-level values.
        assert_eq!(merged.working_directory.as_deref(), Some("/tmp/thread"));
        assert_eq!(merged.personality, Some(Personality::Friendly));
        assert_eq!(merged.approval_policy, Some(ApprovalMode::OnRequest));
        assert_eq!(merged.network_access_enabled, Some(false));
        assert_eq!(
            merged.model_reasoning_summary,
            Some(ModelReasoningSummary::Auto)
        );
        assert_eq!(merged.collaboration_mode, thread_options.collaboration_mode);

        // With no turn overrides at all, the merge equals the thread defaults.
        assert_eq!(
            ResolvedOptions::merge(&thread_options, &TurnOptions::default()),
            ResolvedOptions::from_thread(&thread_options)
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
            .service_tier(ServiceTier::Default)
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
            .working_directory("/tmp/turn")
            .model_reasoning_effort(ModelReasoningEffort::High)
            .model_reasoning_summary(ModelReasoningSummary::Detailed)
            .service_tier(ServiceTier::Fast)
            .personality(Personality::Pragmatic)
            .approval_policy(ApprovalMode::Never)
            .sandbox_policy(json!({"turn": true}))
            .skip_git_repo_check(true)
            .network_access_enabled(true)
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
        assert_eq!(params.effort.as_deref(), Some("high"));
        assert_eq!(params.summary.as_deref(), Some("detailed"));
        assert_eq!(params.extra.get("serviceTier"), Some(&json!("fast")));
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
            params.extra.get("additionalDirectories"),
            Some(&json!(["/tmp/turn-dir"]))
        );
        assert_eq!(params.extra.get("customTurnFlag"), Some(&Value::Bool(true)));
    }
}
