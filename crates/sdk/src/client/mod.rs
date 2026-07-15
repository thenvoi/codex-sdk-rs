use std::collections::HashMap;
use std::future::Future;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Child;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::{Child as TokioChild, ChildStderr};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::api::{Codex, ResumeThread, Thread, ThreadOptions};
use crate::error::{ClientError, IncomingClassified, RpcError, classify_incoming};
use crate::events::{
    ServerEvent, ServerNotification, ServerRequestEvent, parse_notification, parse_server_request,
};
use crate::protocol::requests;
use crate::protocol::responses;
use crate::protocol::server_requests;
use crate::protocol::shared::{EmptyObject, RequestId};
use crate::transport::TransportHandle;
use crate::transport::stdio::{spawn_owned_stdio_transport, spawn_stdio_transport};
use crate::transport::ws::connect_ws_transport;
use crate::transport::ws_daemon::{ensure_local_ws_app_server, start_ws_server};

type PendingMap = HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>;
type RefreshFuture = Pin<
    Box<
        dyn Future<Output = Result<server_requests::ChatgptAuthTokensRefreshResponse, ClientError>>
            + Send,
    >,
>;
type RefreshHandler =
    Arc<dyn Fn(server_requests::ChatgptAuthTokensRefreshParams) -> RefreshFuture + Send + Sync>;
type ApplyPatchApprovalFuture = Pin<
    Box<
        dyn Future<Output = Result<server_requests::ApplyPatchApprovalResponse, ClientError>>
            + Send,
    >,
>;
type ApplyPatchApprovalHandler = Arc<
    dyn Fn(server_requests::ApplyPatchApprovalParams) -> ApplyPatchApprovalFuture + Send + Sync,
>;
type ExecCommandApprovalFuture = Pin<
    Box<
        dyn Future<Output = Result<server_requests::ExecCommandApprovalResponse, ClientError>>
            + Send,
    >,
>;
type ExecCommandApprovalHandler = Arc<
    dyn Fn(server_requests::ExecCommandApprovalParams) -> ExecCommandApprovalFuture + Send + Sync,
>;
type CommandExecutionRequestApprovalFuture = Pin<
    Box<
        dyn Future<
                Output = Result<
                    server_requests::CommandExecutionRequestApprovalResponse,
                    ClientError,
                >,
            > + Send,
    >,
>;
type CommandExecutionRequestApprovalHandler = Arc<
    dyn Fn(
            server_requests::CommandExecutionRequestApprovalParams,
        ) -> CommandExecutionRequestApprovalFuture
        + Send
        + Sync,
>;
type FileChangeRequestApprovalFuture = Pin<
    Box<
        dyn Future<Output = Result<server_requests::FileChangeRequestApprovalResponse, ClientError>>
            + Send,
    >,
>;
type FileChangeRequestApprovalHandler = Arc<
    dyn Fn(server_requests::FileChangeRequestApprovalParams) -> FileChangeRequestApprovalFuture
        + Send
        + Sync,
>;
type ToolRequestUserInputFuture = Pin<
    Box<
        dyn Future<Output = Result<server_requests::ToolRequestUserInputResponse, ClientError>>
            + Send,
    >,
>;
type ToolRequestUserInputHandler = Arc<
    dyn Fn(server_requests::ToolRequestUserInputParams) -> ToolRequestUserInputFuture + Send + Sync,
>;
type DynamicToolCallFuture = Pin<
    Box<dyn Future<Output = Result<server_requests::DynamicToolCallResponse, ClientError>> + Send>,
>;
type DynamicToolCallHandler =
    Arc<dyn Fn(server_requests::DynamicToolCallParams) -> DynamicToolCallFuture + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub default_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StdioConfig {
    pub codex_binary: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub options: ClientOptions,
}

impl Default for StdioConfig {
    fn default() -> Self {
        Self {
            codex_binary: "codex".to_string(),
            args: vec!["app-server".to_string()],
            env: HashMap::new(),
            options: ClientOptions::default(),
        }
    }
}

/// A stdio client together with exclusive ownership of its app-server child.
///
/// Use this when the embedding application, rather than the SDK, must observe
/// and deterministically stop the child process. Dropping [`StdioProcess`]
/// requests termination; callers should prefer [`StdioProcess::shutdown`] so
/// they can observe the exit status.
pub struct SpawnedStdio {
    pub client: CodexClient,
    pub process: StdioProcess,
}

/// Lifecycle handle for a stdio app-server child.
pub struct StdioProcess {
    child: TokioChild,
    writer_task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for StdioProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioProcess")
            .field("id", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl StdioProcess {
    /// Host process id, when the platform exposes one.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Takes the child's stderr stream for bounded diagnostic capture.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Non-blocking child status check.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Waits for the child to exit without forcing termination.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Closes the protocol stdin stream. This is idempotent.
    pub fn close_stdin(&mut self) {
        if let Some(task) = self.writer_task.take() {
            task.abort();
        }
    }

    /// Closes stdin and waits for a graceful exit, then kills on timeout.
    ///
    /// Returns the final status and whether forced termination was required.
    pub async fn shutdown(
        &mut self,
        graceful_timeout: Duration,
    ) -> std::io::Result<(ExitStatus, bool)> {
        self.close_stdin();
        match tokio::time::timeout(graceful_timeout, self.child.wait()).await {
            Ok(status) => status.map(|status| (status, false)),
            Err(_) => {
                self.child.start_kill()?;
                self.child.wait().await.map(|status| (status, true))
            }
        }
    }
}

impl Drop for StdioProcess {
    fn drop(&mut self) {
        self.close_stdin();
        let _ = self.child.start_kill();
    }
}

#[derive(Debug, Clone)]
pub struct WsConfig {
    pub url: String,
    pub env: HashMap<String, String>,
    pub options: ClientOptions,
}

impl WsConfig {
    pub fn new(
        url: impl Into<String>,
        env: HashMap<String, String>,
        options: ClientOptions,
    ) -> Self {
        Self {
            url: url.into(),
            env,
            options,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            url: String::from("ws://127.0.0.1:4222"),
            env: HashMap::new(),
            options: ClientOptions::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WsStartConfig {
    pub listen_url: String,
    pub connect_url: String,
    pub env: HashMap<String, String>,
    pub reuse_existing: bool,
}

impl WsStartConfig {
    pub fn new(
        listen_url: impl Into<String>,
        connect_url: impl Into<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            listen_url: listen_url.into(),
            connect_url: connect_url.into(),
            env,
            reuse_existing: true,
        }
    }

    pub fn with_listen_url(mut self, listen_url: impl Into<String>) -> Self {
        self.listen_url = listen_url.into();
        self
    }

    pub fn with_connect_url(mut self, connect_url: impl Into<String>) -> Self {
        self.connect_url = connect_url.into();
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_reuse_existing(mut self, reuse_existing: bool) -> Self {
        self.reuse_existing = reuse_existing;
        self
    }
}

impl Default for WsStartConfig {
    fn default() -> Self {
        Self {
            listen_url: String::from("ws://127.0.0.1:4222"),
            connect_url: String::from("ws://127.0.0.1:4222"),
            env: HashMap::new(),
            reuse_existing: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsStartMode {
    Daemon,
    Blocking,
}

#[derive(Debug)]
pub struct WsServerHandle {
    listen_url: String,
    connect_url: String,
    mode: WsStartMode,
    reused_existing: bool,
    log_path: Option<PathBuf>,
    process_group_id: Option<u32>,
    child: Option<Child>,
}

impl WsServerHandle {
    pub fn listen_url(&self) -> &str {
        &self.listen_url
    }

    pub fn connect_url(&self) -> &str {
        &self.connect_url
    }

    pub fn mode(&self) -> WsStartMode {
        self.mode
    }

    pub fn reused_existing(&self) -> bool {
        self.reused_existing
    }

    pub fn started_new_process(&self) -> bool {
        !self.reused_existing
    }

    pub fn owns_process(&self) -> bool {
        self.child.is_some()
    }

    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    pub fn connect_config(&self, options: ClientOptions) -> WsConfig {
        WsConfig::new(self.connect_url.clone(), HashMap::new(), options)
    }

    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        let process_group_id = self.process_group_id.take();
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };

        let bind_target = websocket_bind_target(&self.listen_url)
            .or_else(|| websocket_bind_target(&self.connect_url));

        if let Some(process_group_id) = process_group_id {
            let _ = terminate_process_group(process_group_id);
        }

        let mut child_exited = false;
        for attempt in 0..20 {
            if !child_exited && child.try_wait()?.is_some() {
                child_exited = true;
            }
            if child_exited && bind_target.as_ref().is_none_or(websocket_port_is_free) {
                return Ok(());
            }
            if attempt == 5
                && let Some(process_group_id) = process_group_id
            {
                let _ = terminate_process_group(process_group_id);
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        if let Some(process_group_id) = process_group_id {
            let _ = kill_process_group(process_group_id);
        }
        if !child_exited {
            let _ = child.kill();
            let _ = child.wait()?;
        }

        for _ in 0..20 {
            if bind_target.as_ref().is_none_or(websocket_port_is_free) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let target = bind_target
            .map(|(host, port)| format!("{host}:{port}"))
            .unwrap_or_else(|| self.connect_url.clone());
        Err(ClientError::TransportSend(format!(
            "websocket app-server did not release `{target}` during shutdown"
        )))
    }

    pub(crate) fn from_reused_existing(
        listen_url: String,
        connect_url: String,
        mode: WsStartMode,
        log_path: Option<PathBuf>,
    ) -> Self {
        Self {
            listen_url,
            connect_url,
            mode,
            reused_existing: true,
            log_path,
            process_group_id: None,
            child: None,
        }
    }

    pub(crate) fn daemon_started(
        listen_url: String,
        connect_url: String,
        log_path: PathBuf,
    ) -> Self {
        Self {
            listen_url,
            connect_url,
            mode: WsStartMode::Daemon,
            reused_existing: false,
            log_path: Some(log_path),
            process_group_id: None,
            child: None,
        }
    }

    pub(crate) fn blocking_started(listen_url: String, connect_url: String, child: Child) -> Self {
        let process_group_id = Some(child.id());
        Self {
            listen_url,
            connect_url,
            mode: WsStartMode::Blocking,
            reused_existing: false,
            log_path: None,
            process_group_id,
            child: Some(child),
        }
    }
}

impl Drop for WsServerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn websocket_bind_target(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port()?;
    Some((host, port))
}

fn websocket_port_is_free((host, port): &(String, u16)) -> bool {
    TcpListener::bind((host.as_str(), *port)).is_ok()
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn signal_process_group(process_group_id: u32, signal: i32) -> std::io::Result<()> {
    let process_group_id = i32::try_from(process_group_id)
        .map_err(|_| std::io::Error::other("process group id is too large"))?;
    let result = unsafe { kill(-process_group_id, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn terminate_process_group(process_group_id: u32) -> std::io::Result<()> {
    signal_process_group(process_group_id, 15)
}

#[cfg(unix)]
fn kill_process_group(process_group_id: u32) -> std::io::Result<()> {
    signal_process_group(process_group_id, 9)
}

#[cfg(not(unix))]
fn terminate_process_group(_process_group_id: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn kill_process_group(_process_group_id: u32) -> std::io::Result<()> {
    Ok(())
}

struct Inner {
    outbound: mpsc::Sender<Value>,
    pending: Mutex<PendingMap>,
    default_timeout: Duration,
    initialized: AtomicBool,
    ready: AtomicBool,
    next_id: AtomicI64,
    event_tx: broadcast::Sender<ServerEvent>,
    event_rx: Mutex<broadcast::Receiver<ServerEvent>>,
    refresh_handler: RwLock<Option<RefreshHandler>>,
    apply_patch_approval_handler: RwLock<Option<ApplyPatchApprovalHandler>>,
    exec_command_approval_handler: RwLock<Option<ExecCommandApprovalHandler>>,
    command_execution_request_approval_handler:
        RwLock<Option<CommandExecutionRequestApprovalHandler>>,
    file_change_request_approval_handler: RwLock<Option<FileChangeRequestApprovalHandler>>,
    tool_request_user_input_handler: RwLock<Option<ToolRequestUserInputHandler>>,
    dynamic_tool_call_handler: RwLock<Option<DynamicToolCallHandler>>,
}

#[derive(Clone)]
pub struct CodexClient {
    inner: Arc<Inner>,
}

macro_rules! typed_method {
    ($fn_name:ident, $method:literal, $params_ty:ty, $result_ty:ty) => {
        pub async fn $fn_name(&self, params: $params_ty) -> Result<$result_ty, ClientError> {
            self.request_typed_internal($method, params, None, true)
                .await
        }
    };
}

macro_rules! typed_null_method {
    ($fn_name:ident, $method:literal, $result_ty:ty) => {
        pub async fn $fn_name(&self) -> Result<$result_ty, ClientError> {
            self.request_typed_value_internal($method, Value::Null, None, true)
                .await
        }
    };
}

impl CodexClient {
    pub async fn spawn_stdio(config: StdioConfig) -> Result<Self, ClientError> {
        let handle = spawn_stdio_transport(&config.codex_binary, &config.args, &config.env).await?;
        Ok(Self::from_transport(handle, config.options.default_timeout))
    }

    /// Spawns a stdio app-server while returning exclusive process ownership.
    ///
    /// `current_dir` configures the child process working directory without
    /// changing the existing [`StdioConfig`] struct-literal API. The caller is
    /// responsible for invoking [`StdioProcess::shutdown`] and may take stderr
    /// for bounded diagnostics.
    pub async fn spawn_stdio_owned(
        config: StdioConfig,
        current_dir: Option<&Path>,
    ) -> Result<SpawnedStdio, ClientError> {
        let spawned = spawn_owned_stdio_transport(
            &config.codex_binary,
            &config.args,
            &config.env,
            current_dir,
        )
        .await?;
        let client = Self::from_transport(spawned.handle, config.options.default_timeout);
        Ok(SpawnedStdio {
            client,
            process: StdioProcess {
                child: spawned.child,
                writer_task: Some(spawned.writer_task),
            },
        })
    }

    pub async fn connect_ws(config: WsConfig) -> Result<Self, ClientError> {
        let handle = connect_ws_transport(&config.url).await?;
        Ok(Self::from_transport(handle, config.options.default_timeout))
    }

    pub async fn start_ws(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        Self::start_ws_daemon(config).await
    }

    pub async fn start_ws_daemon(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        start_ws_server(&config, WsStartMode::Daemon).await
    }

    pub async fn start_ws_blocking(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        start_ws_server(&config, WsStartMode::Blocking).await
    }

    pub async fn start_and_connect_ws(config: WsConfig) -> Result<Self, ClientError> {
        ensure_local_ws_app_server(&config.url, &config.env).await?;

        let handle = connect_ws_transport(&config.url).await?;
        Ok(Self::from_transport(handle, config.options.default_timeout))
    }

    fn from_transport(handle: TransportHandle, default_timeout: Duration) -> Self {
        let (event_tx, event_rx) = broadcast::channel(1024);
        let inner = Arc::new(Inner {
            outbound: handle.outbound,
            pending: Mutex::new(HashMap::new()),
            default_timeout,
            initialized: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            next_id: AtomicI64::new(1),
            event_tx,
            event_rx: Mutex::new(event_rx),
            refresh_handler: RwLock::new(None),
            apply_patch_approval_handler: RwLock::new(None),
            exec_command_approval_handler: RwLock::new(None),
            command_execution_request_approval_handler: RwLock::new(None),
            file_change_request_approval_handler: RwLock::new(None),
            tool_request_user_input_handler: RwLock::new(None),
            dynamic_tool_call_handler: RwLock::new(None),
        });

        tokio::spawn(run_inbound_loop(handle.inbound, inner.clone()));
        Self { inner }
    }

    pub fn as_api(&self) -> Codex {
        Codex::from_client(self.clone())
    }

    pub fn start_thread(&self, options: ThreadOptions) -> Thread {
        self.as_api().start_thread(options)
    }

    pub fn resume_thread(&self, target: impl Into<ResumeThread>, options: ThreadOptions) -> Thread {
        self.as_api().resume_thread(target, options)
    }

    pub fn resume_thread_by_id(&self, id: impl Into<String>, options: ThreadOptions) -> Thread {
        self.as_api().resume_thread_by_id(id, options)
    }

    pub fn resume_latest_thread(&self, options: ThreadOptions) -> Thread {
        self.as_api().resume_latest_thread(options)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.inner.event_tx.subscribe()
    }

    pub async fn next_event(&self) -> Result<ServerEvent, ClientError> {
        let mut rx = self.inner.event_rx.lock().await;
        rx.recv().await.map_err(|err| {
            ClientError::TransportSend(format!("event channel receive failed: {err}"))
        })
    }

    pub async fn set_chatgpt_auth_tokens_refresh_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::ChatgptAuthTokensRefreshParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::ChatgptAuthTokensRefreshResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: RefreshHandler = Arc::new(move |params| Box::pin(handler(params)));
        *self.inner.refresh_handler.write().await = Some(wrapped);
    }

    pub async fn clear_chatgpt_auth_tokens_refresh_handler(&self) {
        *self.inner.refresh_handler.write().await = None;
    }

    pub async fn set_apply_patch_approval_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::ApplyPatchApprovalParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::ApplyPatchApprovalResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: ApplyPatchApprovalHandler = Arc::new(move |params| Box::pin(handler(params)));
        *self.inner.apply_patch_approval_handler.write().await = Some(wrapped);
    }

    pub async fn clear_apply_patch_approval_handler(&self) {
        *self.inner.apply_patch_approval_handler.write().await = None;
    }

    pub async fn set_exec_command_approval_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::ExecCommandApprovalParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::ExecCommandApprovalResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: ExecCommandApprovalHandler = Arc::new(move |params| Box::pin(handler(params)));
        *self.inner.exec_command_approval_handler.write().await = Some(wrapped);
    }

    pub async fn clear_exec_command_approval_handler(&self) {
        *self.inner.exec_command_approval_handler.write().await = None;
    }

    pub async fn set_command_execution_request_approval_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::CommandExecutionRequestApprovalParams) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: Future<
                Output = Result<
                    server_requests::CommandExecutionRequestApprovalResponse,
                    ClientError,
                >,
            > + Send
            + 'static,
    {
        let wrapped: CommandExecutionRequestApprovalHandler =
            Arc::new(move |params| Box::pin(handler(params)));
        *self
            .inner
            .command_execution_request_approval_handler
            .write()
            .await = Some(wrapped);
    }

    pub async fn clear_command_execution_request_approval_handler(&self) {
        *self
            .inner
            .command_execution_request_approval_handler
            .write()
            .await = None;
    }

    pub async fn set_file_change_request_approval_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::FileChangeRequestApprovalParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::FileChangeRequestApprovalResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: FileChangeRequestApprovalHandler =
            Arc::new(move |params| Box::pin(handler(params)));
        *self
            .inner
            .file_change_request_approval_handler
            .write()
            .await = Some(wrapped);
    }

    pub async fn clear_file_change_request_approval_handler(&self) {
        *self
            .inner
            .file_change_request_approval_handler
            .write()
            .await = None;
    }

    pub async fn set_tool_request_user_input_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::ToolRequestUserInputParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::ToolRequestUserInputResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: ToolRequestUserInputHandler =
            Arc::new(move |params| Box::pin(handler(params)));
        *self.inner.tool_request_user_input_handler.write().await = Some(wrapped);
    }

    pub async fn clear_tool_request_user_input_handler(&self) {
        *self.inner.tool_request_user_input_handler.write().await = None;
    }

    pub async fn set_dynamic_tool_call_handler<F, Fut>(&self, handler: F)
    where
        F: Fn(server_requests::DynamicToolCallParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<server_requests::DynamicToolCallResponse, ClientError>>
            + Send
            + 'static,
    {
        let wrapped: DynamicToolCallHandler = Arc::new(move |params| Box::pin(handler(params)));
        *self.inner.dynamic_tool_call_handler.write().await = Some(wrapped);
    }

    pub async fn clear_dynamic_tool_call_handler(&self) {
        *self.inner.dynamic_tool_call_handler.write().await = None;
    }

    pub async fn initialize(
        &self,
        params: requests::InitializeParams,
    ) -> Result<responses::InitializeResult, ClientError> {
        if self.inner.initialized.load(Ordering::SeqCst) {
            return Err(ClientError::AlreadyInitialized);
        }

        let result: responses::InitializeResult = self
            .request_typed_internal("initialize", params, None, false)
            .await?;

        self.inner.initialized.store(true, Ordering::SeqCst);
        Ok(result)
    }

    pub async fn initialized(&self) -> Result<(), ClientError> {
        if !self.inner.initialized.load(Ordering::SeqCst) {
            return Err(ClientError::NotInitialized {
                method: "initialized".to_string(),
            });
        }
        self.send_notification("initialized", EmptyObject::default(), false)
            .await?;
        self.inner.ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn send_raw_request(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ClientError> {
        let method = method.into();
        let requires_ready = method != "initialize";
        self.request_value_internal(&method, params, timeout, requires_ready)
            .await
    }

    pub async fn send_raw_notification(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ClientError> {
        let method = method.into();
        let requires_ready = method != "initialized";
        self.send_notification(&method, params, requires_ready)
            .await
    }

    pub async fn respond_server_request<R: Serialize>(
        &self,
        id: RequestId,
        result: R,
    ) -> Result<(), ClientError> {
        let result = serde_json::to_value(result)?;
        self.send_message(json!({ "id": id, "result": result }))
            .await
    }

    pub async fn respond_server_request_error(
        &self,
        id: RequestId,
        error: RpcError,
    ) -> Result<(), ClientError> {
        self.send_message(json!({ "id": id, "error": error })).await
    }

    pub async fn respond_chatgpt_auth_tokens_refresh(
        &self,
        id: RequestId,
        response: server_requests::ChatgptAuthTokensRefreshResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_apply_patch_approval(
        &self,
        id: RequestId,
        response: server_requests::ApplyPatchApprovalResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_exec_command_approval(
        &self,
        id: RequestId,
        response: server_requests::ExecCommandApprovalResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_command_execution_request_approval(
        &self,
        id: RequestId,
        response: server_requests::CommandExecutionRequestApprovalResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_file_change_request_approval(
        &self,
        id: RequestId,
        response: server_requests::FileChangeRequestApprovalResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_tool_request_user_input(
        &self,
        id: RequestId,
        response: server_requests::ToolRequestUserInputResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    pub async fn respond_dynamic_tool_call(
        &self,
        id: RequestId,
        response: server_requests::DynamicToolCallResponse,
    ) -> Result<(), ClientError> {
        self.respond_server_request(id, response).await
    }

    typed_method!(
        thread_start,
        "thread/start",
        requests::ThreadStartParams,
        responses::ThreadResult
    );
    typed_method!(
        thread_resume,
        "thread/resume",
        requests::ThreadResumeParams,
        responses::ThreadResult
    );
    typed_method!(
        thread_fork,
        "thread/fork",
        requests::ThreadForkParams,
        responses::ThreadResult
    );
    typed_method!(
        thread_archive,
        "thread/archive",
        requests::ThreadArchiveParams,
        responses::ThreadArchiveResult
    );
    typed_method!(
        thread_name_set,
        "thread/name/set",
        requests::ThreadSetNameParams,
        responses::ThreadSetNameResult
    );
    typed_method!(
        thread_unarchive,
        "thread/unarchive",
        requests::ThreadUnarchiveParams,
        responses::ThreadUnarchiveResult
    );
    typed_method!(
        thread_compact_start,
        "thread/compact/start",
        requests::ThreadCompactStartParams,
        responses::ThreadCompactStartResult
    );
    typed_method!(
        thread_background_terminals_clean,
        "thread/backgroundTerminals/clean",
        requests::ThreadBackgroundTerminalsCleanParams,
        responses::ThreadBackgroundTerminalsCleanResult
    );
    typed_method!(
        thread_rollback,
        "thread/rollback",
        requests::ThreadRollbackParams,
        responses::ThreadRollbackResult
    );
    typed_method!(
        thread_list,
        "thread/list",
        requests::ThreadListParams,
        responses::ThreadListResult
    );
    typed_method!(
        thread_loaded_list,
        "thread/loaded/list",
        requests::ThreadLoadedListParams,
        responses::ThreadLoadedListResult
    );
    typed_method!(
        thread_read,
        "thread/read",
        requests::ThreadReadParams,
        responses::ThreadReadResult
    );
    typed_method!(
        skills_list,
        "skills/list",
        requests::SkillsListParams,
        responses::SkillsListResult
    );
    typed_method!(
        skills_remote_list,
        "skills/remote/list",
        requests::SkillsRemoteReadParams,
        responses::SkillsRemoteReadResult
    );
    typed_method!(
        skills_remote_export,
        "skills/remote/export",
        requests::SkillsRemoteWriteParams,
        responses::SkillsRemoteWriteResult
    );
    typed_method!(
        app_list,
        "app/list",
        requests::AppsListParams,
        responses::AppsListResult
    );
    typed_method!(
        skills_config_write,
        "skills/config/write",
        requests::SkillsConfigWriteParams,
        responses::SkillsConfigWriteResult
    );
    typed_method!(
        turn_start,
        "turn/start",
        requests::TurnStartParams,
        responses::TurnResult
    );
    typed_method!(
        turn_steer,
        "turn/steer",
        requests::TurnSteerParams,
        responses::TurnSteerResult
    );
    typed_method!(
        turn_interrupt,
        "turn/interrupt",
        requests::TurnInterruptParams,
        EmptyObject
    );
    typed_method!(
        review_start,
        "review/start",
        requests::ReviewStartParams,
        responses::ReviewStartResult
    );
    typed_method!(
        model_list,
        "model/list",
        requests::ModelListParams,
        responses::ModelListResult
    );
    typed_method!(
        experimental_feature_list,
        "experimentalFeature/list",
        requests::ExperimentalFeatureListParams,
        responses::ExperimentalFeatureListResult
    );
    typed_method!(
        collaboration_mode_list,
        "collaborationMode/list",
        requests::CollaborationModeListParams,
        responses::CollaborationModeListResult
    );
    typed_method!(
        mock_experimental_method,
        "mock/experimentalMethod",
        requests::MockExperimentalMethodParams,
        responses::MockExperimentalMethodResult
    );
    typed_method!(
        mcp_server_oauth_login,
        "mcpServer/oauth/login",
        requests::McpServerOauthLoginParams,
        responses::McpServerOauthLoginResult
    );
    typed_method!(
        mcp_server_status_list,
        "mcpServerStatus/list",
        requests::ListMcpServerStatusParams,
        responses::McpServerStatusListResult
    );
    typed_method!(
        windows_sandbox_setup_start,
        "windowsSandbox/setupStart",
        requests::WindowsSandboxSetupStartParams,
        responses::WindowsSandboxSetupStartResult
    );
    typed_method!(
        account_login_start,
        "account/login/start",
        requests::LoginAccountParams,
        responses::LoginAccountResult
    );
    typed_method!(
        account_login_cancel,
        "account/login/cancel",
        requests::CancelLoginAccountParams,
        EmptyObject
    );
    typed_method!(
        feedback_upload,
        "feedback/upload",
        requests::FeedbackUploadParams,
        responses::FeedbackUploadResult
    );
    typed_method!(
        command_exec,
        "command/exec",
        requests::CommandExecParams,
        responses::CommandExecResult
    );
    typed_method!(
        config_read,
        "config/read",
        requests::ConfigReadParams,
        responses::ConfigReadResult
    );
    typed_method!(
        config_value_write,
        "config/value/write",
        requests::ConfigValueWriteParams,
        responses::ConfigValueWriteResult
    );
    typed_method!(
        config_batch_write,
        "config/batchWrite",
        requests::ConfigBatchWriteParams,
        responses::ConfigBatchWriteResult
    );
    typed_method!(
        account_read,
        "account/read",
        requests::GetAccountParams,
        responses::GetAccountResult
    );
    typed_method!(
        fuzzy_file_search_session_start,
        "fuzzyFileSearch/sessionStart",
        requests::FuzzyFileSearchSessionStartParams,
        responses::FuzzyFileSearchSessionStartResult
    );
    typed_method!(
        fuzzy_file_search_session_update,
        "fuzzyFileSearch/sessionUpdate",
        requests::FuzzyFileSearchSessionUpdateParams,
        responses::FuzzyFileSearchSessionUpdateResult
    );
    typed_method!(
        fuzzy_file_search_session_stop,
        "fuzzyFileSearch/sessionStop",
        requests::FuzzyFileSearchSessionStopParams,
        responses::FuzzyFileSearchSessionStopResult
    );

    // Backward-compatible aliases for previous method names.
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

    typed_null_method!(
        config_mcp_server_reload,
        "config/mcpServer/reload",
        EmptyObject
    );
    typed_null_method!(account_logout, "account/logout", EmptyObject);
    typed_null_method!(
        account_rate_limits_read,
        "account/rateLimits/read",
        responses::AccountRateLimitsReadResult
    );
    typed_null_method!(
        config_requirements_read,
        "configRequirements/read",
        responses::ConfigRequirementsReadResult
    );

    async fn send_notification<P: Serialize>(
        &self,
        method: &str,
        params: P,
        requires_ready: bool,
    ) -> Result<(), ClientError> {
        if requires_ready && !self.inner.ready.load(Ordering::SeqCst) {
            return Err(ClientError::NotReady {
                method: method.to_string(),
            });
        }

        let value = serde_json::to_value(params)?;
        self.send_message(json!({ "method": method, "params": value }))
            .await
    }

    async fn request_typed_internal<P, R>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
        requires_ready: bool,
    ) -> Result<R, ClientError>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let value = serde_json::to_value(params)?;
        self.request_typed_value_internal(method, value, timeout, requires_ready)
            .await
    }

    async fn request_typed_value_internal<R>(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        requires_ready: bool,
    ) -> Result<R, ClientError>
    where
        R: serde::de::DeserializeOwned,
    {
        let raw = self
            .request_value_internal(method, params, timeout, requires_ready)
            .await?;

        serde_json::from_value(raw).map_err(|source| ClientError::UnexpectedResult {
            method: method.to_string(),
            source,
        })
    }

    async fn request_value_internal(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        requires_ready: bool,
    ) -> Result<Value, ClientError> {
        if requires_ready && !self.inner.ready.load(Ordering::SeqCst) {
            return Err(ClientError::NotReady {
                method: method.to_string(),
            });
        }

        if method == "initialize" && self.inner.initialized.load(Ordering::SeqCst) {
            return Err(ClientError::AlreadyInitialized);
        }

        let id_num = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let id = RequestId::Integer(id_num);

        let request = json!({
            "method": method,
            "id": id,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.clone(), tx);

        if let Err(err) = self.send_message(request).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(err);
        }

        let timeout = timeout.unwrap_or(self.inner.default_timeout);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(ClientError::Rpc { error }),
            Ok(Err(_)) => Err(ClientError::TransportClosed),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(ClientError::Timeout {
                    method: method.to_string(),
                    timeout_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    async fn send_message(&self, value: Value) -> Result<(), ClientError> {
        self.inner.outbound.send(value).await.map_err(|err| {
            ClientError::TransportSend(format!("failed to send outbound frame: {err}"))
        })
    }
}

async fn run_inbound_loop(
    mut inbound: mpsc::Receiver<Result<Value, ClientError>>,
    inner: Arc<Inner>,
) {
    while let Some(frame) = inbound.recv().await {
        match frame {
            Ok(value) => {
                if let Err(err) = process_incoming_value(value, &inner).await {
                    fail_all_pending(&inner, &format!("processing inbound frame failed: {err}"))
                        .await;
                    let _ = inner.event_tx.send(ServerEvent::TransportClosed);
                    break;
                }
            }
            Err(err) => {
                fail_all_pending(&inner, &format!("transport error: {err}")).await;
                let _ = inner.event_tx.send(ServerEvent::TransportClosed);
                break;
            }
        }
    }
}

async fn process_incoming_value(value: Value, inner: &Arc<Inner>) -> Result<(), ClientError> {
    match classify_incoming(value)? {
        IncomingClassified::Response { id, result } => {
            if let Some(sender) = inner.pending.lock().await.remove(&id) {
                let _ = sender.send(result);
            }
        }
        IncomingClassified::Notification {
            method,
            params,
            raw: _,
        } => {
            let parsed = parse_notification(method.clone(), params.clone())
                .unwrap_or(ServerNotification::Unknown { method, params });
            let _ = inner.event_tx.send(ServerEvent::Notification(parsed));
        }
        IncomingClassified::ServerRequest {
            id,
            method,
            params,
            raw: _,
        } => {
            let parsed = parse_server_request(id.clone(), method.clone(), params.clone())
                .unwrap_or(ServerRequestEvent::Unknown { id, method, params });
            if !try_auto_handle_server_request(inner, &parsed).await {
                let _ = inner.event_tx.send(ServerEvent::ServerRequest(parsed));
            }
        }
    }
    Ok(())
}

async fn try_auto_handle_server_request(inner: &Arc<Inner>, request: &ServerRequestEvent) -> bool {
    match request {
        ServerRequestEvent::ChatgptAuthTokensRefresh { id, params } => {
            let handler = inner.refresh_handler.read().await.clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(inner, id, response, "chatgptAuthTokens refresh")
                .await
        }
        ServerRequestEvent::ApplyPatchApproval { id, params } => {
            let handler = inner.apply_patch_approval_handler.read().await.clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(inner, id, response, "applyPatchApproval").await
        }
        ServerRequestEvent::ExecCommandApproval { id, params } => {
            let handler = inner.exec_command_approval_handler.read().await.clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(inner, id, response, "execCommandApproval").await
        }
        ServerRequestEvent::CommandExecutionRequestApproval { id, params } => {
            let handler = inner
                .command_execution_request_approval_handler
                .read()
                .await
                .clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(
                inner,
                id,
                response,
                "item/commandExecution/requestApproval",
            )
            .await
        }
        ServerRequestEvent::FileChangeRequestApproval { id, params } => {
            let handler = inner
                .file_change_request_approval_handler
                .read()
                .await
                .clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(
                inner,
                id,
                response,
                "item/fileChange/requestApproval",
            )
            .await
        }
        ServerRequestEvent::ToolRequestUserInput { id, params } => {
            let handler = inner.tool_request_user_input_handler.read().await.clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(inner, id, response, "item/tool/requestUserInput")
                .await
        }
        ServerRequestEvent::DynamicToolCall { id, params } => {
            let handler = inner.dynamic_tool_call_handler.read().await.clone();
            let Some(handler) = handler else {
                return false;
            };

            let response = handler(params.clone()).await;
            send_server_request_handler_result(inner, id, response, "item/tool/call").await
        }
        _ => false,
    }
}

async fn send_server_request_handler_result<R: Serialize>(
    inner: &Arc<Inner>,
    id: &RequestId,
    response: Result<R, ClientError>,
    context: &str,
) -> bool {
    let payload = match response {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(err) => json!({
            "id": id,
            "error": {
                "code": -32001,
                "message": format!("{context} handler failed: {err}")
            }
        }),
    };

    if inner.outbound.send(payload).await.is_err() {
        let _ = inner.event_tx.send(ServerEvent::TransportClosed);
    }

    true
}

async fn fail_all_pending(inner: &Arc<Inner>, message: &str) {
    let mut pending = inner.pending.lock().await;
    let entries = std::mem::take(&mut *pending);
    drop(pending);

    for (_, sender) in entries {
        let _ = sender.send(Err(RpcError {
            code: -32098,
            message: message.to_string(),
            data: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    fn test_client() -> (
        CodexClient,
        mpsc::Sender<Result<Value, ClientError>>,
        mpsc::Receiver<Value>,
    ) {
        let (transport_outbound_tx, transport_outbound_rx) = mpsc::channel::<Value>(32);
        let (transport_inbound_tx, transport_inbound_rx) =
            mpsc::channel::<Result<Value, ClientError>>(32);
        let client = CodexClient::from_transport(
            TransportHandle {
                outbound: transport_outbound_tx,
                inbound: transport_inbound_rx,
            },
            Duration::from_secs(5),
        );
        (client, transport_inbound_tx, transport_outbound_rx)
    }

    #[tokio::test]
    async fn auto_handles_apply_patch_approval_when_handler_registered() {
        let (client, inbound_tx, mut outbound_rx) = test_client();

        client
            .set_apply_patch_approval_handler(|_| async {
                let mut response = server_requests::ApplyPatchApprovalResponse::default();
                response
                    .extra
                    .insert("decision".to_string(), Value::String("approve".to_string()));
                Ok(response)
            })
            .await;

        inbound_tx
            .send(Ok(json!({
                "id": 42,
                "method": "applyPatchApproval",
                "params": {}
            })))
            .await
            .expect("send inbound server request");

        let outbound = timeout(Duration::from_secs(2), outbound_rx.recv())
            .await
            .expect("timed out waiting for outbound response")
            .expect("expected outbound response frame");
        assert_eq!(outbound.get("id"), Some(&json!(42)));
        assert_eq!(
            outbound.pointer("/result/decision"),
            Some(&Value::String("approve".to_string()))
        );
    }

    #[tokio::test]
    async fn unhandled_server_request_is_published_as_event() {
        let (client, inbound_tx, mut outbound_rx) = test_client();

        inbound_tx
            .send(Ok(json!({
                "id": 7,
                "method": "applyPatchApproval",
                "params": {}
            })))
            .await
            .expect("send inbound server request");

        let event = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("timed out waiting for event")
            .expect("event receive");

        match event {
            ServerEvent::ServerRequest(ServerRequestEvent::ApplyPatchApproval { id, .. }) => {
                assert_eq!(id, RequestId::Integer(7));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(
            timeout(Duration::from_millis(200), outbound_rx.recv())
                .await
                .is_err(),
            "did not expect auto-response when handler is absent"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_stdio_sets_cwd_and_closes_stdin_before_forced_kill() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!(
            "codex-sdk-owned-stdio-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cwd).expect("create test cwd");
        let marker = cwd.join("cwd.txt");

        let mut config = StdioConfig {
            codex_binary: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "pwd > \"$CODEX_SDK_CWD_MARKER\"; while IFS= read -r _; do :; done".to_string(),
            ],
            ..StdioConfig::default()
        };
        config.env.insert(
            "CODEX_SDK_CWD_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        );

        let mut spawned = CodexClient::spawn_stdio_owned(config, Some(&cwd))
            .await
            .expect("spawn owned stdio");
        assert!(spawned.process.id().is_some());

        let marker_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !marker.exists() && tokio::time::Instant::now() < marker_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let observed = std::fs::read_to_string(&marker).expect("read cwd marker");
        assert_eq!(
            std::fs::canonicalize(observed.trim()).expect("canonical observed cwd"),
            std::fs::canonicalize(&cwd).expect("canonical expected cwd")
        );

        let (status, forced) = spawned
            .process
            .shutdown(Duration::from_secs(2))
            .await
            .expect("shutdown owned stdio");
        assert!(status.success());
        assert!(!forced, "stdin EOF should permit graceful shutdown");

        std::fs::remove_dir_all(cwd).expect("remove test cwd");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_stdio_forces_termination_after_grace_period() {
        let config = StdioConfig {
            codex_binary: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "while :; do IFS= read -r _ || :; done".to_string(),
            ],
            ..StdioConfig::default()
        };
        let mut spawned = CodexClient::spawn_stdio_owned(config, None)
            .await
            .expect("spawn owned stdio");

        let (_status, forced) = spawned
            .process
            .shutdown(Duration::from_millis(20))
            .await
            .expect("force shutdown owned stdio");
        assert!(forced, "non-exiting child must be killed after timeout");
    }
}
