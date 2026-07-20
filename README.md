# codex-app-server-sdk

Tokio Rust SDK for Codex App Server JSON-RPC over JSONL.

## Status

- `0.5.1`
- Focused on deterministic automation: explicit timeouts and no implicit retries.
- Typed v2 request methods with raw JSON fallback for protocol drift.

## Features

- `stdio`: spawn `codex app-server` locally.
- `ws` (always enabled): websocket transport with explicit startup and connection APIs.
  - Use `connect_ws` to connect directly to `ws://` or `wss://` endpoints without any process management (useful for existing public or loopback URLs).
  - Use `start_ws_daemon` to reuse or start `codex app-server --listen ...` with separate `listen_url` and `connect_url`.
  - Use `start_ws_blocking` when the SDK should own the child process lifecycle instead of leaving a daemon running.
  - `start_and_connect_ws` remains the loopback convenience wrapper for `ws://127.0.0.1:*`, `ws://[::1]:*`, and `ws://localhost:*`; `wss://` URLs are connect-only and are never auto-started.
  - Daemon logs are written to `/tmp/codex-app-server-sdk/*.log`.

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
    .skip_git_repo_check(true) // matches CLI flag: --skip-git-repo-check
    .build();
let mut thread = codex.start_thread(thread_options);
let turn_options = TurnOptions::builder().build();

let turn = thread
    .run("Summarize this repository in two bullet points.", turn_options)
    .await?;

println!("thread: {}", thread.id().unwrap_or("<unknown>"));
println!("response: {}", turn.final_response);
# Ok(())
# }
```

Use `run_streamed(...)` when you need incremental item and lifecycle events.
`StreamedTurn::turn_id()` returns the exact provider turn id from the successful
`turn/start` response for durable correlation and replay-safe observation keys.

`AgentMessageItem.phase` mirrors the app-server’s optional `agentMessage.phase` field (`commentary` or `final_answer`). Use `message.is_final_answer()` to identify the final turn message from `ItemCompleted`; `Turn.final_response` and `ask(...)` already prefer the `final_answer` item when the server provides it and otherwise fall back to the last completed agent message.

`TurnOptionsBuilder` supports raw JSON schemas (`.output_schema(...)`) and typed schema generation (`.output_schema_for::<T>()`) for `output_schema`, plus per-turn overrides for `cwd`, `model`, `model_provider`, reasoning, personality, approval/sandbox, collaboration mode, and raw extra fields.

High-level thread lifecycle helpers now include `set_name(...)`, `read(...)`, `archive()`, `unarchive()`, `rollback(...)`, `compact_start()`, `steer(...)`, and `interrupt(...)`.

Use `ask(...)` or `ask_with_options(...)` when you only need the final response string:

```rust
# use codex_app_server_sdk::api::{Codex, ThreadOptions, TurnOptions};
# use codex_app_server_sdk::StdioConfig;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let final_response = codex
    .ask_with_options(
        "Summarize this repository in one sentence.",
        ThreadOptions::default(),
        TurnOptions::default(),
    )
    .await?;
println!("response: {final_response}");
# Ok(())
# }
```

Resume helpers are also available in the high-level API. `ResumeThread` gives you a typed resume target, and the narrower `resume_thread_by_id(...)` / `resume_latest_thread(...)` helpers remain available.

```rust
# use codex_app_server_sdk::api::{Codex, ResumeThread, ThreadOptions};
# use codex_app_server_sdk::StdioConfig;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
let _by_id = codex.resume_thread("thread_123", ThreadOptions::default());
let _latest = codex.resume_thread(
    ResumeThread::Latest,
    ThreadOptions::builder()
        .working_directory("/path/to/project")
        .build(),
);
# Ok(())
# }
```

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

`ThreadOptionsBuilder` also exposes protocol-level options that were previously missing, including:
- `model_provider`
- `model_reasoning_summary`
- `personality`
- `sandbox_policy`
- `base_instructions`
- `developer_instructions`
- `ephemeral`
- `collaboration_mode`
- `config` overrides and dynamic tools
- `experimental_raw_events` and `persist_extended_history`

## Quickstart (ws, explicit startup + high-level api)

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

The same `start_thread(...)`, `run(...)`, and `run_streamed(...)` flow works for stdio, `ws://`, and `wss://` transports.

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

## Auth support

Typed methods include:

- `account/read`
- `account/login/start`
- `account/login/cancel`
- `account/logout`
- `account/rateLimits/read`

Server-initiated `account/chatgptAuthTokens/refresh` is surfaced as typed events, with optional automatic handler registration.

## Raw fallback

Use:

- `send_raw_request(method, params, timeout)`
- `send_raw_notification(method, params)`

for newly added methods or fields not yet wrapped in typed helpers.

## Full Typed RPC Coverage

`Codex` now forwards the full typed client RPC surface after handshake initialization, including thread lifecycle operations (with resume overrides), turn controls, auth/config methods, skills/MCP/review APIs, collaboration mode listing, background-terminal cleanup, windows sandbox setup start, fuzzy file search session controls, and typed null-parameter methods.

## Examples

- `crates/sdk/examples/turn_start_stream.rs`
- `crates/sdk/examples/auth_api_key.rs`
- `crates/sdk/examples/raw_fallback.rs`
- `crates/sdk/examples/ws_persistent.rs`
- `crates/sdk/examples/high_level_run.rs`
- `crates/sdk/examples/high_level_streamed.rs`
- `crates/sdk/examples/high_level_resume.rs`
- `crates/sdk/examples/high_level_output_schema.rs`

## `spark` CLI

The repository includes a `spark` binary with explicit `exec`, `start`, and `sessions` commands.
By default `spark exec` connects over websocket to `ws://127.0.0.1:4222` and reuses/auto-starts the loopback app-server daemon:

```bash
cargo run -p spark -- exec "Summarize this repository in one sentence."
```

Use `spark start` to ensure the websocket daemon is running and exit:

```bash
cargo run -p spark -- start
```

By default, Spark sets Codex `cwd` to the current shell working directory where `spark` is invoked. Use `--cwd` to override:

```bash
cargo run -p spark -- exec --cwd /path/to/project "Summarize this repository in one sentence."
```

`spark exec` resolves its websocket URL in this order: `--ws-url`, `CODEX_WEB_SERVER_URL`, then the default `ws://127.0.0.1:4222`.

```bash
CODEX_WEB_SERVER_URL=ws://127.0.0.1:5222 cargo run -p spark -- exec "Summarize this repository in one sentence."
```

Use `--no-daemon` with `spark exec` to connect to a websocket URL without spawning a local daemon process:

```bash
cargo run -p spark -- exec --no-daemon "Summarize this repository in one sentence."
```

Use `--stdio` with `spark exec` to force app-server stdio transport instead:

```bash
cargo run -p spark -- exec --stdio "Summarize this repository in one sentence."
```

Use `--final-response` with `spark exec` to print only the final message content:

```bash
cargo run -p spark -- exec --final-response "Summarize this repository in one sentence."
```

Resume the most recent session (`codex resume --last` equivalent):

```bash
cargo run -p spark -- exec --continue "Follow up on the previous answer."
```

Resume a specific session id:

```bash
cargo run -p spark -- exec --resume thread_123 "Continue from that session."
```

`spark` defaults to:

- model: `gpt-5.3-codex-spark` (override: `--model`)
- reasoning effort: `xhigh` (override: `--reasoning-effort`)
- websocket transport (`ws://127.0.0.1:4222`) unless `spark exec --stdio` is provided
- Codex `cwd` defaults to the invocation directory, unless `--cwd <path>` is provided
- each completed `agentMessage` is newline-terminated so consecutive messages do not run together
- `spark exec --continue` and `spark exec --resume <session_id>` are mutually exclusive
- transport initialization is started early and overlapped with prompt/config resolution to reduce first-message latency

Additional optional config flags:

- transport/session: `--ws-url`, `--stdio`, `--no-daemon`, `--continue`, `--resume`, env `CODEX_WEB_SERVER_URL`
- model/reasoning: `--model`, `--model-provider`, `--reasoning-effort`, `--reasoning-summary`, `--model-verbosity`
- policy/sandbox: `--approval-policy`, `--sandbox`, `--sandbox-policy-json`, `--sandbox-network-access-enabled|--sandbox-network-access-disabled`, `--sandbox-writable-root`, `--ephemeral`
- network/search: `--web-search-mode`
- instructions/personality: `--agent`, `--base-instructions`, `--developer-instructions`, `--personality`
- config/schema: `--config`, `--config-json`, `--config-profile`, `--output-schema-json`, `--output-schema-file`, `--turn-extra-json`
- app-server extras: `--dynamic-tools-json`

Example with explicit overrides:

```bash
cargo run -p spark -- \
  exec \
  --model gpt-5.3-codex \
  --reasoning-effort high \
  --approval-policy on-request \
  --sandbox workspace-write \
  --sandbox-network-access-enabled \
  --sandbox-writable-root /tmp/reports \
  --web-search-mode live \
  --output-schema-json '{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}' \
  "Return JSON with an answer field."
```

Agent profiles are optional and are loaded via `--agent <name>` from `~/.codex/config.toml` under `[agents.<name>]`.
When `--stdio` is used, `spark` resolves the Codex CLI path with `which codex` and exits early if no path is returned.
If startup fails with a codex lookup error, run `which codex` and ensure your shell PATH includes the desired Codex CLI install.
`[agents.<name>].config_file` points to a role TOML file (relative paths resolve from the config file directory).
`spark` maps the role to thread `developer_instructions` in this order:
- `developer_instructions` from the role config file
- `model_instructions_file` (file contents, path resolved relative to the role config file)
- `[agents.<name>].description` as a fallback

See `docs/spark-session-resumption.md` for additional details.

## `agx` CLI

`agx` is an agent-focused CLI that loads Codex custom agent TOML files directly.

`agx --agent <name>` first searches for `.codex/agents/<name>.toml` from the invocation directory upward through the current repo, then falls back to `~/.codex/agents/<name>.toml`. If both files exist, the project-local file is selected; agent files are not merged.

Use `--scope project` to search only project-local agent paths, or `--scope user` to search only `~/.codex/agents`. When an agent is selected, `agx` prints the active agent name, instructions preview, model when set, and resolved config path before starting the turn.

By default, `agx` streams mapped turn events as they arrive. Use `--last-response-only` to suppress the startup banner and streamed events and print only the final agent response text.

## Integration tests

These tests execute against a real local `codex app-server` process:

```bash
cargo test -p codex-app-server-sdk --test integration_stdio -- --nocapture
cargo test -p codex-app-server-sdk --test integration_api_stdio -- --nocapture
cargo test -p codex-app-server-sdk --test integration_ws -- --nocapture
cargo test -p spark --test integration_spark -- --nocapture
```

## License

MIT
