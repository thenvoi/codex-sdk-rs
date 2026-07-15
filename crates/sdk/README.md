# codex-app-server-sdk

Tokio Rust SDK for Codex App Server JSON-RPC over JSONL.

## Status

- `0.5.1`
- Focused on deterministic automation: explicit timeouts and no implicit retries.
- Typed v2 request methods with raw JSON fallback for protocol drift.

## Features

- `stdio`: spawn `codex app-server` locally.
- `ws`: websocket transport with explicit startup and connection APIs.
  - Use `connect_ws` to connect directly to `ws://` or `wss://` endpoints without any process management.
  - Use `start_ws_daemon` to reuse or start `codex app-server --listen ...` with separate `listen_url` and `connect_url`.
  - Use `start_ws_blocking` when the SDK should own the child process lifecycle instead of leaving a daemon running.
  - `start_and_connect_ws` remains the loopback convenience wrapper for `ws://127.0.0.1:*`, `ws://[::1]:*`, and `ws://localhost:*`; `wss://` URLs are connect-only and are never auto-started.
  - Daemon logs are written to `/tmp/codex-app-server-sdk/*.log`.
- High-level typed thread helpers:
  - `Codex::ask(...)`
  - `Codex::ask_with_options(...)`
  - `ResumeThread` enum for typed resume targets
  - `Codex::resume_thread_by_id(...)`
  - `Codex::resume_latest_thread(...)`
  - `Thread::run(...)`
  - `Thread::run_streamed(...)`
- Typed schema generation via `OpenAiSerializable` and `openai_json_schema_for::<T>()`.

## Requirements

- `codex` CLI installed and available on `PATH`.
- `codex app-server` must start locally for live flows.
- Active Codex authentication for account/model/turn requests.

## Quickstart (stdio)

```rust
use codex_app_server_sdk::{CodexClient, StdioConfig};
use codex_app_server_sdk::requests::{ClientInfo, InitializeParams, ThreadStartParams, TurnStartParams};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let client = CodexClient::spawn_stdio(StdioConfig::default()).await?;

let init = InitializeParams::new(ClientInfo::new("my_client", "My Client", "0.1.0"));
let _ = client.initialize(init).await?;
client.initialized().await?;

let thread = client.thread_start(ThreadStartParams::default()).await?;
let thread_id = thread.thread.id;

let turn = client
    .turn_start(TurnStartParams::text(thread_id, "Summarize this repository."))
    .await?;

println!("turn: {}", turn.turn.id);
# Ok(())
# }
```

## Quickstart (high-level typed API)

```rust
use codex_app_server_sdk::api::{
    Codex, ModelReasoningEffort, SandboxMode, ThreadOptions, TurnOptions, WebSearchMode,
};
use codex_app_server_sdk::StdioConfig;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let thread_options = ThreadOptions::builder()
    .sandbox_mode(SandboxMode::WorkspaceWrite)
    .model_reasoning_effort(ModelReasoningEffort::Medium)
    .web_search_mode(WebSearchMode::Live)
    .skip_git_repo_check(true)
    .build();
let mut thread = codex.start_thread(thread_options);

let turn = thread
    .run(
        "Summarize this repository in two bullet points.",
        TurnOptions::default(),
    )
    .await?;

println!("thread: {}", thread.id().unwrap_or("<unknown>"));
println!("response: {}", turn.final_response);
# Ok(())
# }
```

Use `run_streamed(...)` when you need incremental item and lifecycle events.

Resume a recorded thread with a typed resume target:

```rust
# use codex_app_server_sdk::api::{Codex, ResumeThread, ThreadOptions};
# use codex_app_server_sdk::StdioConfig;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let mut thread = codex.resume_thread(ResumeThread::ById("thread_123".to_string()), ThreadOptions::default());
# Ok(())
# }
```

Resume the latest recorded thread for a workspace:

```rust
# use codex_app_server_sdk::api::{Codex, ResumeThread, ThreadOptions};
# use codex_app_server_sdk::StdioConfig;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let mut thread = codex.resume_thread(
    ResumeThread::Latest,
    ThreadOptions::builder()
        .working_directory("/path/to/project")
        .build(),
);
# Ok(())
# }
```

The explicit helpers remain available: use `resume_thread_by_id(...)` and `resume_latest_thread(...)` when you prefer the narrower API surface.

`AgentMessageItem.phase` mirrors the app-server's optional `agentMessage.phase` field (`commentary` or `final_answer`). Use `message.is_final_answer()` to identify the final turn message from `ItemCompleted`; `Turn.final_response` and `ask(...)` already prefer the `final_answer` item when the server provides it and otherwise fall back to the last completed agent message.

## Typed output schema

```rust
use codex_app_server_sdk::api::{Codex, ThreadOptions, TurnOptions};
use codex_app_server_sdk::{JsonSchema, OpenAiSerializable, StdioConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema, OpenAiSerializable)]
struct Reply {
    answer: String,
}

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let mut thread = codex.start_thread(ThreadOptions::default());
let turn_options = TurnOptions::builder().output_schema_for::<Reply>().build();
let turn = thread
    .run("Respond with JSON only and include the `answer` field.", turn_options)
    .await?;

let value: serde_json::Value = serde_json::from_str(&turn.final_response)?;
let reply = Reply::from_openai_value(value)?;
println!("{}", reply.answer);
# Ok(())
# }
```

Use `codex_app_server_sdk::JsonSchema` instead of adding a separate `schemars` dependency unless you deliberately need a different version elsewhere in your application. That keeps the derive macro and `OpenAiSerializable` on the same trait version.

If you want the SDK to wire the derives and crate paths for you, use the convenience attribute:

```rust
#[codex_app_server_sdk::openai_type]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    #[serde(rename = "final_answer")]
    answer: String,
}
```

Embedders that must own the app-server lifecycle can request an owned process
handle and set the child working directory without changing the executable that
users install:

```rust
use codex_app_server_sdk::{CodexClient, StdioConfig};
use std::path::Path;
use std::time::Duration;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let mut spawned = CodexClient::spawn_stdio_owned(
    StdioConfig::default(),
    Some(Path::new("/path/to/project")),
).await?;

let client = spawned.client.clone();
// Initialize and use `client` as usual.
let (_status, forced) = spawned.process.shutdown(Duration::from_secs(3)).await?;
println!("forced shutdown: {forced}");
# Ok(())
# }
```

`StdioProcess::take_stderr` supports bounded diagnostic capture. Dropping the
handle requests child termination, but callers should use `shutdown` to close
stdin first and observe whether a forced kill was needed.

## Websocket flow

```rust
use codex_app_server_sdk::api::{ThreadOptions, TurnOptions};
use codex_app_server_sdk::{CodexClient, WsConfig, WsStartConfig};
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let _server = CodexClient::start_ws_daemon(WsStartConfig::default()).await?;
let client = CodexClient::connect_ws(WsConfig::default()).await?;

let mut thread = client.start_thread(ThreadOptions::default());
let turn = thread
    .run("Reply with exactly: ok", TurnOptions::default())
    .await?;

println!("response: {}", turn.final_response);
# Ok(())
# }
```

For an exposed bind, use separate URLs:

```rust
use codex_app_server_sdk::{CodexClient, WsStartConfig};
use std::collections::HashMap;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let _server = CodexClient::start_ws_daemon(WsStartConfig {
    listen_url: "ws://0.0.0.0:4222".to_string(),
    connect_url: "ws://127.0.0.1:4222".to_string(),
    env: HashMap::new(),
    reuse_existing: true,
}).await?;
# Ok(())
# }
```

## Reliability model

- No automatic retries for any RPC method.
- Every request has a timeout (`ClientOptions::default_timeout`) with per-call override available through raw request APIs.
- Requests are blocked client-side until you complete both steps: `initialize()` then `initialized()`.
- Unknown events and fields are preserved through `Unknown` variants and `extra` maps.

## Raw fallback

Use:

- `send_raw_request(method, params, timeout)`
- `send_raw_notification(method, params)`

for newly added methods or fields not yet wrapped in typed helpers.

## Examples

- `cargo run -p codex-app-server-sdk --example turn_start_stream`
- `cargo run -p codex-app-server-sdk --example raw_fallback`
- `cargo run -p codex-app-server-sdk --example high_level_run`
- `cargo run -p codex-app-server-sdk --example high_level_streamed`
- `cargo run -p codex-app-server-sdk --example high_level_resume`
- `cargo run -p codex-app-server-sdk --example high_level_output_schema`
- `cargo run -p codex-app-server-sdk --example ws_persistent`
- `cargo run -p codex-app-server-sdk --example web_search_agent`
