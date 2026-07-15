mod server_requests;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::{Child as TokioChild, ChildStderr};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::api::Codex;
use crate::error::{
    ClientError, IncomingClassified, RPC_ERROR_CODE_TRANSPORT_FAILURE, RpcError, classify_incoming,
};
use crate::events::{
    ServerEvent, ServerNotification, ServerRequestEvent, parse_notification, parse_server_request,
};
use crate::protocol::methods::codex_rpc_table;
use crate::protocol::requests;
use crate::protocol::responses;
use crate::protocol::shared::{EmptyObject, RequestId};
use crate::transport::TransportHandle;
use crate::transport::stdio::{spawn_owned_stdio_transport, spawn_stdio_transport};
use crate::transport::ws::connect_ws_transport;
use crate::transport::ws_daemon::{ensure_local_ws_app_server, start_ws_server};

pub use crate::transport::ws_daemon::{WsServerHandle, WsStartMode};

type PendingMap = HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>;

/// Wire method name of the `initialize` handshake request — the only request
/// allowed before the connection is ready.
pub(crate) const METHOD_INITIALIZE: &str = "initialize";
/// Wire method name of the `initialized` handshake notification — the only
/// notification allowed before the connection is ready.
pub(crate) const METHOD_INITIALIZED: &str = "initialized";

/// Handshake state machine (stored in an `AtomicU8`): monotonically advances
/// `New -> Initializing -> Initialized -> Ready`, except that a failed
/// `initialize` inside [`CodexClient::ensure_ready`] resets to `New`.
const HANDSHAKE_NEW: u8 = 0;
const HANDSHAKE_INITIALIZING: u8 = 1;
const HANDSHAKE_INITIALIZED: u8 = 2;
const HANDSHAKE_READY: u8 = 3;

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

/// Where and how to connect to a websocket app-server.
///
/// Environment variables for a daemon the SDK may spawn are *not* part of
/// this config: pass them to [`CodexClient::start_and_connect_ws`] (or use
/// [`WsStartConfig`] with the explicit start APIs), since a plain
/// [`CodexClient::connect_ws`] never spawns anything.
///
/// When `auth_token` is set, the websocket HTTP upgrade includes
/// `Authorization: Bearer <token>` (capability token or pre-signed JWT).
/// Auth tokens are only accepted for `wss://` or loopback `ws://` URLs.
#[derive(Clone)]
pub struct WsConfig {
    pub url: String,
    pub options: ClientOptions,
    /// Optional bearer credential for app-server websocket auth.
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for WsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConfig")
            .field("url", &self.url)
            .field("options", &self.options)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl WsConfig {
    pub fn new(url: impl Into<String>, options: ClientOptions) -> Self {
        Self {
            url: url.into(),
            options,
            auth_token: None,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    pub fn with_auth_token(mut self, auth_token: impl Into<String>) -> Self {
        self.auth_token = Some(auth_token.into());
        self
    }
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            url: String::from("ws://127.0.0.1:4222"),
            options: ClientOptions::default(),
            auth_token: None,
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

struct Inner {
    outbound: mpsc::Sender<Value>,
    pending: Mutex<PendingMap>,
    default_timeout: Duration,
    /// One of the `HANDSHAKE_*` states.
    handshake_state: AtomicU8,
    /// Serializes the `ensure_ready` handshake critical section.
    handshake_lock: Mutex<()>,
    next_id: AtomicI64,
    event_tx: broadcast::Sender<ServerEvent>,
    event_rx: Mutex<broadcast::Receiver<ServerEvent>>,
    server_request_handlers: server_requests::ServerRequestHandlers,
}

impl Inner {
    fn handshake_state(&self) -> u8 {
        self.handshake_state.load(Ordering::SeqCst)
    }
}

/// The `initialize` params used when the caller has not supplied their own
/// (e.g. [`CodexClient::ensure_ready`]).
pub(crate) fn default_initialize_params() -> requests::InitializeParams {
    requests::InitializeParams::new(requests::ClientInfo::new(
        "codex_sdk_rs",
        "Codex Rust SDK",
        env!("CARGO_PKG_VERSION"),
    ))
}

#[derive(Clone)]
pub struct CodexClient {
    inner: Arc<Inner>,
}

/// Expands the shared RPC method table ([`codex_rpc_table!`]) into
/// `CodexClient`'s typed request methods. One recursive arm per row kind:
/// `typed` (params + result), `null` (null params), and `alias`
/// (delegates to another generated method).
macro_rules! define_client_rpc_methods {
    () => {};
    (
        $(#[$doc:meta])*
        typed $fn_name:ident, $method:literal, $params_ty:ty, $result_ty:ty;
        $($rest:tt)*
    ) => {
        $(#[$doc])*
        pub async fn $fn_name(&self, params: $params_ty) -> Result<$result_ty, ClientError> {
            self.request_typed_internal($method, params, None, true)
                .await
        }

        define_client_rpc_methods! { $($rest)* }
    };
    (
        $(#[$doc:meta])*
        null $fn_name:ident, $method:literal, $result_ty:ty;
        $($rest:tt)*
    ) => {
        $(#[$doc])*
        pub async fn $fn_name(&self) -> Result<$result_ty, ClientError> {
            self.request_typed_value_internal($method, Value::Null, None, true)
                .await
        }

        define_client_rpc_methods! { $($rest)* }
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

        define_client_rpc_methods! { $($rest)* }
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
        let handle = connect_ws_transport(&config.url, config.auth_token.as_deref()).await?;
        Ok(Self::from_transport(handle, config.options.default_timeout))
    }

    pub async fn start_ws_daemon(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        start_ws_server(&config, WsStartMode::Daemon).await
    }

    pub async fn start_ws_blocking(config: WsStartConfig) -> Result<WsServerHandle, ClientError> {
        start_ws_server(&config, WsStartMode::Blocking).await
    }

    /// Connects to `config.url`, first ensuring a local app-server daemon is
    /// running when the URL is a managed loopback target. `env` is passed to
    /// any daemon this call spawns (it is not used when connecting to an
    /// already-running server).
    ///
    /// When `config.auth_token` is set, readiness probes and the final connect
    /// both send `Authorization: Bearer <token>`.
    pub async fn start_and_connect_ws(
        config: WsConfig,
        env: HashMap<String, String>,
    ) -> Result<Self, ClientError> {
        ensure_local_ws_app_server(&config.url, &env, config.auth_token.as_deref()).await?;

        let handle = connect_ws_transport(&config.url, config.auth_token.as_deref()).await?;
        Ok(Self::from_transport(handle, config.options.default_timeout))
    }

    fn from_transport(handle: TransportHandle, default_timeout: Duration) -> Self {
        let (event_tx, event_rx) = broadcast::channel(1024);
        let inner = Arc::new(Inner {
            outbound: handle.outbound,
            pending: Mutex::new(HashMap::new()),
            default_timeout,
            handshake_state: AtomicU8::new(HANDSHAKE_NEW),
            handshake_lock: Mutex::new(()),
            next_id: AtomicI64::new(1),
            event_tx,
            event_rx: Mutex::new(event_rx),
            server_request_handlers: server_requests::ServerRequestHandlers::default(),
        });

        tokio::spawn(run_inbound_loop(handle.inbound, inner.clone()));
        Self { inner }
    }

    pub fn as_api(&self) -> Codex {
        Codex::from_client(self.clone())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.inner.event_tx.subscribe()
    }

    pub async fn next_event(&self) -> Result<ServerEvent, ClientError> {
        let mut rx = self.inner.event_rx.lock().await;
        loop {
            match rx.recv().await {
                Ok(event) => return Ok(event),
                // Skip over dropped events instead of failing: the channel is
                // still alive and later events remain deliverable.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(ClientError::TransportClosed);
                }
            }
        }
    }

    /// Sends the `initialize` handshake request. Errors with
    /// [`ClientError::AlreadyInitialized`] when the handshake request has
    /// already been performed (explicitly or via [`Self::ensure_ready`]).
    pub async fn initialize(
        &self,
        params: requests::InitializeParams,
    ) -> Result<responses::InitializeResult, ClientError> {
        let result: responses::InitializeResult = self
            .request_typed_internal(METHOD_INITIALIZE, params, None, false)
            .await?;

        self.inner
            .handshake_state
            .fetch_max(HANDSHAKE_INITIALIZED, Ordering::SeqCst);
        Ok(result)
    }

    /// Sends the `initialized` handshake notification, marking the connection
    /// ready. Errors with [`ClientError::NotInitialized`] when `initialize`
    /// has not completed yet.
    pub async fn initialized(&self) -> Result<(), ClientError> {
        if self.inner.handshake_state() < HANDSHAKE_INITIALIZED {
            return Err(ClientError::NotInitialized {
                method: METHOD_INITIALIZED.to_string(),
            });
        }
        self.send_notification(METHOD_INITIALIZED, EmptyObject::default(), false)
            .await?;
        self.inner
            .handshake_state
            .fetch_max(HANDSHAKE_READY, Ordering::SeqCst);
        Ok(())
    }

    /// Idempotently completes the `initialize`/`initialized` handshake with
    /// default client info: exactly one caller performs each handshake step,
    /// even under concurrent invocation; every other caller waits and then
    /// returns `Ok(())`.
    pub async fn ensure_ready(&self) -> Result<(), ClientError> {
        self.ensure_ready_with(&default_initialize_params()).await
    }

    /// [`Self::ensure_ready`] with explicit `initialize` params, used when
    /// this caller ends up performing the handshake.
    pub(crate) async fn ensure_ready_with(
        &self,
        params: &requests::InitializeParams,
    ) -> Result<(), ClientError> {
        if self.inner.handshake_state() == HANDSHAKE_READY {
            return Ok(());
        }

        let _guard = self.inner.handshake_lock.lock().await;
        if self.inner.handshake_state() == HANDSHAKE_READY {
            return Ok(());
        }

        if self.inner.handshake_state() < HANDSHAKE_INITIALIZED {
            self.inner
                .handshake_state
                .store(HANDSHAKE_INITIALIZING, Ordering::SeqCst);
            let initialize_result: Result<responses::InitializeResult, ClientError> = self
                .request_typed_internal(METHOD_INITIALIZE, params.clone(), None, false)
                .await;
            match initialize_result {
                Ok(_) => {
                    self.inner
                        .handshake_state
                        .store(HANDSHAKE_INITIALIZED, Ordering::SeqCst);
                }
                Err(err) => {
                    self.inner
                        .handshake_state
                        .store(HANDSHAKE_NEW, Ordering::SeqCst);
                    return Err(err);
                }
            }
        }

        self.send_notification(METHOD_INITIALIZED, EmptyObject::default(), false)
            .await?;
        self.inner
            .handshake_state
            .store(HANDSHAKE_READY, Ordering::SeqCst);
        Ok(())
    }

    pub async fn send_raw_request(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ClientError> {
        let method = method.into();
        let requires_ready = method != METHOD_INITIALIZE;
        self.request_value_internal(&method, params, timeout, requires_ready)
            .await
    }

    pub async fn send_raw_notification(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ClientError> {
        let method = method.into();
        let requires_ready = method != METHOD_INITIALIZED;
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

    codex_rpc_table!(define_client_rpc_methods);

    async fn send_notification<P: Serialize>(
        &self,
        method: &str,
        params: P,
        requires_ready: bool,
    ) -> Result<(), ClientError> {
        if requires_ready && self.inner.handshake_state() < HANDSHAKE_READY {
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
        if requires_ready && self.inner.handshake_state() < HANDSHAKE_READY {
            return Err(ClientError::NotReady {
                method: method.to_string(),
            });
        }

        // The only request exempt from the ready gate is the `initialize`
        // handshake itself; re-sending it after the handshake completed is an
        // explicit re-initialization error.
        if !requires_ready && self.inner.handshake_state() >= HANDSHAKE_INITIALIZED {
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
            if !server_requests::try_auto_handle_server_request(inner, &parsed).await {
                let _ = inner.event_tx.send(ServerEvent::ServerRequest(parsed));
            }
        }
    }
    Ok(())
}

async fn fail_all_pending(inner: &Arc<Inner>, message: &str) {
    let mut pending = inner.pending.lock().await;
    let entries = std::mem::take(&mut *pending);
    drop(pending);

    for (_, sender) in entries {
        let _ = sender.send(Err(RpcError {
            code: RPC_ERROR_CODE_TRANSPORT_FAILURE,
            message: message.to_string(),
            data: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::server_requests as sr;
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
                let mut response = sr::ApplyPatchApprovalResponse::default();
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

    #[tokio::test]
    async fn auto_handles_dynamic_tool_call_when_handler_registered() {
        let (client, inbound_tx, mut outbound_rx) = test_client();

        client
            .set_dynamic_tool_call_handler(|_| async {
                let mut response = sr::DynamicToolCallResponse::default();
                response
                    .extra
                    .insert("output".to_string(), Value::String("done".to_string()));
                Ok(response)
            })
            .await;

        inbound_tx
            .send(Ok(json!({
                "id": 43,
                "method": "item/tool/call",
                "params": {}
            })))
            .await
            .expect("send inbound server request");

        let outbound = timeout(Duration::from_secs(2), outbound_rx.recv())
            .await
            .expect("timed out waiting for outbound response")
            .expect("expected outbound response frame");
        assert_eq!(outbound.get("id"), Some(&json!(43)));
        assert_eq!(
            outbound.pointer("/result/output"),
            Some(&Value::String("done".to_string()))
        );
    }

    #[tokio::test]
    async fn handler_error_is_answered_with_error_response() {
        let (client, inbound_tx, mut outbound_rx) = test_client();

        client
            .set_apply_patch_approval_handler(|_| async {
                Err::<sr::ApplyPatchApprovalResponse, _>(ClientError::TransportSend(
                    "boom".to_string(),
                ))
            })
            .await;

        inbound_tx
            .send(Ok(json!({
                "id": 44,
                "method": "applyPatchApproval",
                "params": {}
            })))
            .await
            .expect("send inbound server request");

        let outbound = timeout(Duration::from_secs(2), outbound_rx.recv())
            .await
            .expect("timed out waiting for outbound response")
            .expect("expected outbound response frame");
        assert_eq!(outbound.get("id"), Some(&json!(44)));
        assert_eq!(
            outbound.pointer("/error/code"),
            Some(&json!(crate::error::RPC_ERROR_CODE_HANDLER_FAILED))
        );
        let message = outbound
            .pointer("/error/message")
            .and_then(Value::as_str)
            .expect("error message");
        assert!(
            message.contains("applyPatchApproval handler failed"),
            "unexpected error message: {message}"
        );
        assert!(
            message.contains("boom"),
            "unexpected error message: {message}"
        );
    }

    #[tokio::test]
    async fn concurrent_ensure_ready_sends_exactly_one_initialize() {
        let (client, inbound_tx, mut outbound_rx) = test_client();

        let first = client.clone();
        let second = client.clone();
        let first_task = tokio::spawn(async move { first.ensure_ready().await });
        let second_task = tokio::spawn(async move { second.ensure_ready().await });

        // Exactly one `initialize` request must appear on the wire; answer it.
        let request = timeout(Duration::from_secs(2), outbound_rx.recv())
            .await
            .expect("timed out waiting for initialize request")
            .expect("expected initialize request frame");
        assert_eq!(
            request.get("method").and_then(Value::as_str),
            Some(METHOD_INITIALIZE)
        );
        let id = request.get("id").cloned().expect("initialize request id");
        inbound_tx
            .send(Ok(json!({ "id": id, "result": {} })))
            .await
            .expect("send initialize response");

        // Followed by exactly one `initialized` notification.
        let notification = timeout(Duration::from_secs(2), outbound_rx.recv())
            .await
            .expect("timed out waiting for initialized notification")
            .expect("expected initialized notification frame");
        assert_eq!(
            notification.get("method").and_then(Value::as_str),
            Some(METHOD_INITIALIZED)
        );
        assert!(
            notification.get("id").is_none(),
            "initialized must be a notification"
        );

        // Both racing callers complete successfully.
        first_task
            .await
            .expect("join first ensure_ready")
            .expect("first ensure_ready should succeed");
        second_task
            .await
            .expect("join second ensure_ready")
            .expect("second ensure_ready should succeed");

        // No second handshake frame (in particular no second initialize).
        assert!(
            timeout(Duration::from_millis(200), outbound_rx.recv())
                .await
                .is_err(),
            "expected no further outbound frames after the handshake"
        );

        // The handshake is done: an explicit re-initialize is rejected and
        // another ensure_ready is a no-op.
        assert!(matches!(
            client.initialize(default_initialize_params()).await,
            Err(ClientError::AlreadyInitialized)
        ));
        client
            .ensure_ready()
            .await
            .expect("ensure_ready should stay idempotent");
    }

    /// Accumulates every wire method string in the shared RPC table into a
    /// slice. `alias` rows carry no wire string and are skipped.
    macro_rules! collect_rpc_wire_methods {
        (@row [$($acc:tt)*]) => { &[$($acc)*] };
        (
            @row [$($acc:tt)*]
            $(#[$doc:meta])*
            typed $fn_name:ident, $method:literal, $params_ty:ty, $result_ty:ty;
            $($rest:tt)*
        ) => {
            collect_rpc_wire_methods!(@row [$($acc)* $method,] $($rest)*)
        };
        (
            @row [$($acc:tt)*]
            $(#[$doc:meta])*
            null $fn_name:ident, $method:literal, $result_ty:ty;
            $($rest:tt)*
        ) => {
            collect_rpc_wire_methods!(@row [$($acc)* $method,] $($rest)*)
        };
        (
            @row [$($acc:tt)*]
            $(#[$doc:meta])*
            alias $fn_name:ident => $target:ident, $params_ty:ty, $result_ty:ty;
            $($rest:tt)*
        ) => {
            collect_rpc_wire_methods!(@row [$($acc)*] $($rest)*)
        };
        ($($rows:tt)*) => { collect_rpc_wire_methods!(@row [] $($rows)*) };
    }

    #[test]
    fn rpc_table_wire_methods_are_wellformed_and_unique() {
        const WIRE_METHODS: &[&str] = codex_rpc_table!(collect_rpc_wire_methods);

        assert_eq!(WIRE_METHODS.len(), 43, "unexpected RPC table size");

        let mut seen = std::collections::HashSet::new();
        for method in WIRE_METHODS {
            assert!(!method.is_empty(), "empty wire method string");
            assert!(seen.insert(*method), "duplicate wire method: {method}");

            let segments: Vec<&str> = method.split('/').collect();
            assert!(
                segments.len() >= 2,
                "wire method `{method}` does not have a `domain/action` shape"
            );
            for segment in segments {
                assert!(
                    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphanumeric()),
                    "wire method `{method}` has a malformed segment `{segment}`"
                );
            }
        }
    }

    /// Compile-time proof that `Codex` and `CodexClient` expose the same RPC
    /// surface: referencing every table row's method on both types fails to
    /// compile if either side is missing one.
    #[test]
    fn codex_and_codex_client_expose_every_rpc_table_method() {
        macro_rules! assert_rpc_parity {
            () => {};
            (
                $(#[$doc:meta])*
                typed $fn_name:ident, $method:literal, $params_ty:ty, $result_ty:ty;
                $($rest:tt)*
            ) => {
                let _ = CodexClient::$fn_name;
                let _ = Codex::$fn_name;
                assert_rpc_parity! { $($rest)* }
            };
            (
                $(#[$doc:meta])*
                null $fn_name:ident, $method:literal, $result_ty:ty;
                $($rest:tt)*
            ) => {
                let _ = CodexClient::$fn_name;
                let _ = Codex::$fn_name;
                assert_rpc_parity! { $($rest)* }
            };
            (
                $(#[$doc:meta])*
                alias $fn_name:ident => $target:ident, $params_ty:ty, $result_ty:ty;
                $($rest:tt)*
            ) => {
                let _ = CodexClient::$fn_name;
                let _ = Codex::$fn_name;
                assert_rpc_parity! { $($rest)* }
            };
        }

        codex_rpc_table!(assert_rpc_parity);
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
