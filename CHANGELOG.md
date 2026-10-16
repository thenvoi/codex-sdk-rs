# Changelog

## codex-app-server-sdk 0.6.0 — UNRELEASED

> **This release contains breaking changes.** They are intentional
> complexity reductions from the audit in `docs/complexity-assessment.md`;
> no deprecation shims were kept. Migration notes are inline below.

### Breaking changes

- **Removed `CodexClient::start_ws` and `Codex::start_ws`.** Both were pure
  aliases. Migrate: call `start_ws_daemon` (identical behavior).
- **Removed `CodexClient::{start_thread, resume_thread, resume_thread_by_id,
  resume_latest_thread}`.** These duplicated the `Codex` API. Migrate:
  `client.as_api().start_thread(...)`, or construct a `Codex` directly.
- **Removed `TurnOptions::{with_output_schema, with_output_schema_for,
  with_model, with_working_directory}`.** They were a third configuration
  style beside public fields and the builder. Migrate:
  `TurnOptions::builder().output_schema(...).build()` etc.
- **Removed `pub type RunResult`.** Migrate: use `api::Turn` directly.
- **Removed `protocol::shared::{JsonRpcNotification, JsonRpcResponse}`.**
  Never constructed by the SDK; the wire is built from typed requests.
- **Removed `TurnStartParams::collaboration_mode` (`Option<String>`).** The
  field was always `None` and its type contradicted the structured object
  actually sent. Collaboration mode travels via `TurnOptions` /
  `ThreadOptions` (see bug fix below).
- **Removed per-turn model-provider and web-search options.** Codex owns
  `modelProvider` and web-search defaults on thread start and resume, not
  `turn/start`. Migrate those calls to `ThreadOptions::builder()`.
- **Replaced `ThreadStartParams::effort` with `config` and removed
  `TurnStartParams::model_provider`.** Thread reasoning defaults now use
  `config.model_reasoning_effort`. Explicit turn effort remains available
  through `TurnOptions::builder().model_reasoning_effort(...)`.
- **`ClientError` gains `Config(String)` and `Startup { message, log_path }`
  variants** (breaking for exhaustive matches). Invalid URLs/configuration
  now surface as `Config`; daemon spawn/readiness/port-conflict failures as
  `Startup`, carrying the daemon log path when one exists. `next_event` on
  a closed event channel is now `TransportClosed` (was a mislabeled
  `TransportSend`). The JSON-RPC codes are named:
  `error::RPC_ERROR_CODE_HANDLER_FAILED` (-32001) and
  `error::RPC_ERROR_CODE_TRANSPORT_FAILURE` (-32098).
- **`WsServerHandle::shutdown` is now `async`.** `Drop` is best-effort only
  (SIGTERM + `try_wait`, no blocking sleeps on the runtime). Process-group
  termination is unix-only; Windows shutdown is best-effort `child.kill()`.
- **`WsConfig` no longer has `env`/`with_env`;
  `start_and_connect_ws(config, env)` takes the daemon environment
  explicitly** (on `CodexClient` and `Codex`). Previously `WsConfig.env`
  was silently ignored by `connect_ws` — the field only exists where a
  process can actually be spawned. `WsStartConfig` keeps its `env`.
- **`WsConfig` gains `auth_token: Option<String>`** (breaking for struct
  literals). When set, connect and managed-loopback readiness probes send
  `Authorization: Bearer <token>`. Use `with_auth_token(...)` or set the
  field explicitly (`None` for unauthenticated servers).
- **`WsServerHandle`/`WsStartMode` moved to `transport::ws_daemon`**
  (re-exports from `client` and the crate root are preserved, so most
  imports keep working).
- **Removed `schema::{serialize_openai_value, deserialize_openai_value}`.**
  They were aliases of `serde_json::{to_value, from_value}`. The trait
  conveniences `to_openai_value` / `from_openai_value` remain.
- **Removed the `serde_yaml` dependency** (declared but unused; the crate is
  deprecated upstream).

### Bug fixes

- **`thread/start` now sends `collaborationMode`.** Previously a
  `collaboration_mode` set in `ThreadOptions` was silently dropped on
  `thread/start` (while `thread/resume` and `turn/start` sent it). Payload
  snapshot tests now pin the exact key sets of all three requests.

### Improvements

- **Wire enums are single-sourced.** `ApprovalMode`, `SandboxMode`,
  `ModelReasoningEffort`, `ModelReasoningSummary`, `Personality`,
  `WebSearchMode`, and `CollaborationModeKind` are generated from one table
  each (`wire_enum!`), and now expose public `as_str()`, `Display`,
  `FromStr` (accepting the wire spellings), and a `VARIANTS` const.
  New `ModelVerbosity` enum (previously duplicated in each CLI) with the
  same surface. `api::UnknownItem` is now exported from the crate root.
- **Typed service-tier selection.** `ServiceTier::{Default, Fast}` can be set
  in `ThreadOptions` or `TurnOptions` and is encoded as app-server
  `serviceTier` on thread start/resume and turn start.
- **One RPC method table.** The full 43-method surface (plus the two
  `skills_remote_*` aliases) is defined once in `protocol::methods` and
  expanded into both `CodexClient` and `Codex`; a compile-time test proves
  the two types expose the same set, so the surfaces can no longer drift.
- **Table-driven server-request handling and notification parsing.** The
  seven approval/tool server requests and the 40 typed notifications are
  each declared in one table that generates the enums, parsers, handler
  storage, `set_*`/`clear_*`/`respond_*` methods, and dispatch.
  `ServerNotification::method_name()` is new.
- **Shared transport plumbing.** stdio and websocket transports share one
  reader/writer implementation with named channel capacities; websocket
  text and binary frames go through a single path.
- **One handshake state machine.** The initialize/ready flags previously
  tracked in two layers (with an error-swallowing workaround) are now a
  single state with an idempotent, race-safe `CodexClient::ensure_ready()`.
- **Daemon lifecycle hardening.** One `WsTarget` URL parse/format path,
  `spawn_blocking` instead of a hand-rolled launcher thread, startup lock
  keyed by `(host, port)` so distinct targets don't serialize, and one
  websocket-handshake liveness probe shared by startup and shutdown.
- **Streamed turns expose their active ID.** `StreamedTurn::turn_id()` lets
  interactive consumers request `Thread::interrupt(...)` while continuing to
  drain the stream through its terminal event.
- **Item status enums expose `as_str()`** (`CommandExecutionStatus`,
  `PatchApplyStatus`, `McpToolCallStatus`, `PatchChangeKind`) with
  canonical wire spellings.
- **Options builders are generated** from one field table; adding an option
  is now a one-row change. Thread/turn request encoding shares one
  extras inserter over an explicit, unit-tested merge of turn-over-thread
  precedence.

## luna 0.3.0 — UNRELEASED

- New `--fast` flag selects app-server `serviceTier: "fast"`; without it Luna
  explicitly selects `default`. The behavior is transport-independent.
- **Fix:** `--json` output now uses canonical wire casing for command,
  file-change, and tool-call statuses (`inProgress`, `completed`, …);
  previously Rust `Debug` names (`InProgress`) leaked into the JSON.
- Hand-rolled enum parsers and the local `ModelVerbosity` deleted in favor
  of the SDK's `FromStr`/`VARIANTS` (error messages unchanged in shape,
  now generated).
- **Fix:** auth env vars (`OPENAI_API_KEY`, `CODEX_API_KEY`,
  `CODEX_ID_TOKEN`, `CODEX_ACCESS_TOKEN`) are now forwarded to any
  `codex app-server` daemon the SDK spawns; previously a freshly spawned
  daemon started unauthenticated.

## agx 0.2.0 — UNRELEASED

- New `--cwd <path>` flag (defaults to the invocation directory); the
  thread working directory was previously never set.
- Removed the vestigial `nickname_candidates` agent-config field (parsed
  and validated but never used; unknown keys in agent files remain
  tolerated).
- Loopback detection is no longer string-matched in agx; the SDK's
  managed-target validation decides daemon startability, and connect
  errors are preserved in the fallback failure message.
- Same daemon auth-env forwarding fix as luna.
