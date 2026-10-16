# Repository Guidelines

## Project Structure & Module Organization
- Workspace root uses a virtual manifest (`Cargo.toml`) with members:
- `crates/sdk`: SDK library crate (`codex-app-server-sdk`).
- `crates/luna`: Luna CLI crate (`luna`) that depends on the SDK.
- `codex-app-server-sdk-macros`: proc-macro crate used by the SDK.
- `crates/sdk/src/lib.rs`: SDK crate exports.
- `crates/sdk/src/client/mod.rs`: async client, RPC lifecycle, handshake/readiness.
- `crates/sdk/src/api.rs`: high-level typed `Codex`/`Thread` convenience API.
- `crates/sdk/src/transport/`: `stdio` and `ws` transports (both always enabled).
- `crates/sdk/src/protocol/`: typed request/response/notification/server-request models.
- `crates/sdk/src/events/mod.rs`: event parsing and enum mapping.
- `crates/sdk/src/error.rs`: SDK/client error types.
- `crates/sdk/examples/`: runnable SDK samples.
- `crates/sdk/tests/`: SDK protocol tests + live integration tests (run by default).
- `crates/luna/src/main.rs`: Luna CLI entrypoint.
- `crates/luna/tests/`: Luna CLI integration tests.
- `adrs/`: architecture decision records.

## Build, Test, and Development Commands
- `cargo fmt --all`: format all Rust code.
- `cargo check --workspace`: compile validation for all workspace members.
- `cargo test --workspace -- --nocapture`: full workspace test suite (including integration tests).
- `cargo test -p codex-app-server-sdk --test integration_stdio -- --nocapture`: real `codex app-server` SDK tests.
- `cargo test -p codex-app-server-sdk --test integration_api_stdio -- --nocapture`: real high-level API tests.
- `cargo test -p codex-app-server-sdk --test integration_ws -- --nocapture`: real websocket transport tests.
- `cargo test -p luna --test integration_luna -- --nocapture`: real `luna` CLI tests (resume/continue flows).
- `cargo run -p codex-app-server-sdk --example raw_fallback`: raw RPC smoke test.
- `cargo run -p codex-app-server-sdk --example turn_start_stream`: live turn streaming test.
- `cargo run -p luna -- exec "..."` (or `cargo run -p luna -- x "..."`): one-shot run (streamed by default) with defaults `gpt-5.6-luna` + `max` (override via flags).
- `cargo run -p luna -- exec --stdio "..."`: one-shot run over stdio transport instead of default websocket.
- `cargo run -p luna -- exec --cwd <path> "..."`: one-shot run with explicit Codex working directory.
- `cargo run -p luna -- exec --final-response "..."`: one-shot run that only prints final response content.
- `cargo run -p luna -- exec --continue "..."`: continue the most recent recorded session.
- `cargo run -p luna -- exec --resume <session_id> "..."`: resume a specific session id.
- `cargo run -p luna -- chat`: open the interactive multi-turn Ratatui interface (accepts the same flags as `exec`).
- `cargo run -p luna -- start`: ensure the default websocket daemon is running and exit.

## CI Merge Gate (Source of Truth)
- Required for merge:
- `cargo fmt --all`
- `cargo check --workspace`
- `cargo test --workspace -- --nocapture`
- Also required when touching protocol parsing, lifecycle, or transport:
- `cargo test -p codex-app-server-sdk --test integration_stdio -- --nocapture`
- `cargo test -p codex-app-server-sdk --test integration_api_stdio -- --nocapture`
- `cargo test -p codex-app-server-sdk --test integration_ws -- --nocapture`
- `cargo test -p luna --test integration_luna -- --nocapture`
- If live tests fail, treat the failure as actionable and fix the underlying cause (do not dismiss as environmental).

## Agent Workflow
1. Preflight: `rustc --version`, `cargo --version`, `codex --version`.
2. Implement minimal typed changes first; preserve raw fallback APIs for protocol drift.
3. Validate in order: `fmt`, `check --workspace`, `test --workspace`, `test -p codex-app-server-sdk --test integration_stdio`, `test -p codex-app-server-sdk --test integration_api_stdio`, `test -p codex-app-server-sdk --test integration_ws`, `test -p luna --test integration_luna`.
4. If behavior changes, update `crates/sdk/examples/` and `README.md` in the same PR.

## Agent Communication & Verification
- Always run relevant tests/checks after code changes without waiting for user request; report results or why not run.
- Do not invent execution rules; if unsure, re-read `AGENTS.md`/`README.md`/CI docs before stating constraints.
- Avoid interim status narration during research; deliver one consolidated update with findings unless the user asks for step-by-step updates.

## Coding Style & Naming Conventions
- Follow `rustfmt` output (4-space indentation, trailing commas where applicable).
- Naming: `snake_case` for functions/modules/files, `UpperCamelCase` for types/enums, `UPPER_SNAKE_CASE` for constants.
- Keep protocol structs explicit and forward-compatible: preserve unknown fields via `extra` maps and `Unknown` variants.

## Testing Guidelines
- Use `#[test]` for pure protocol/unit behavior and `#[tokio::test]` for async flows.
- Name tests as behavior statements (for example, `model_list_typed_matches_raw`).
- Live integration tests run by default and must not be marked `#[ignore]`.
- For protocol changes, add serialization + event-path coverage.
- For lifecycle changes, test handshake invariant: `initialize()` then `initialized()` before normal RPC.

## Integration Test Prerequisites
- `codex` CLI must be installed and executable from `PATH`.
- `codex app-server` must start successfully on local machine.
- User must be authenticated (`chatgpt` or API key mode) for account/model/turn flows.
- Network access must be available for upstream model calls.
- Live SDK integration tests inherit host `HOME`/`CODEX_HOME` auth context by default; set `CODEX_SDK_TEST_ISOLATE_HOME=1` to opt into isolated home mode when debugging config-induced failures.
- If running `crates/sdk/examples/auth_api_key.rs`, set `OPENAI_API_KEY`.
- Known non-fatal runtime logs from app-server can appear; treat test assertions, not stderr noise, as the pass/fail signal.

## Commit & Pull Request Guidelines
- Git history currently has no commits; no repository-specific commit pattern exists yet.
- Use concise imperative subjects (recommended: Conventional Commits, e.g., `feat: add turn interrupt integration test`).
- PRs should include what changed, why, validation commands/results, protocol/API impact, and docs/example updates.

## Pull Request Definition of Done
- Code compiles and all required checks pass (see CI Merge Gate).
- New behavior has tests (unit and/or integration) and existing tests are updated.
- Public behavior changes are reflected in `README.md` and relevant `crates/sdk/examples/`.
- Protocol/wire implications are documented when changing parsing or serde behavior.
- No secrets are added to code, tests, examples, or logs.

## Release and Versioning Policy
- Version intent:
- `patch`: bug fix, no public API break.
- `minor`: additive public API or behavior-compatible expansion.
- `major` (or pre-1.0 designated breaking bump): removal/rename/semantic break.
- When changing protocol/runtime expectations, update:
- `README.md` behavior text.
- `AGENTS.md` runtime expectations.
- PR evidence with full live integration test run results.
- New typed protocol features must preserve raw fallback unless a replacement path is documented.

## Public API Change Rules
- Prefer additive changes over breaking changes.
- Do not remove or rename public items without a migration note.
- Preserve forward-compatibility behavior:
- unknown fields remain preserved via `extra`.
- unknown notifications/server requests map to `Unknown` variants.
- request envelopes continue omitting `jsonrpc`.
- Handshake invariants are API-level behavior and must remain stable unless intentionally versioned.

## Security & Configuration Tips
- Do not hardcode secrets; use environment variables (for example, `OPENAI_API_KEY`).
- Keep auth/token handling in runtime config only.
- Run live tests only in trusted environments because they execute a real local app-server process.

## Protocol Invariants (Do Not Break)
- JSON-RPC messages must omit the `jsonrpc` field.
- Maintain readiness gating: non-init calls are invalid before `initialized()`.
- Do not remove raw fallback APIs; they are required for protocol drift handling.
- Parse unknown notifications/server requests into explicit `Unknown` variants instead of failing hard.

## Known Runtime Expectations
- Live integration tests run by default and require local `codex app-server` plus active auth.
- If local `~/.codex/config.toml` or `$CODEX_HOME/config.toml` contains unsupported keys, integration tests can fail with config-derivation errors; prefer isolating `HOME`/`CODEX_HOME` in test runtime env when validating SDK behavior independent of user config.
- `crates/sdk/examples/auth_api_key.rs` requires `OPENAI_API_KEY`.
- Loopback websocket startup can manage a persistent local daemon (`codex app-server --listen ...`) and writes restrictive, rotating logs below `std::env::temp_dir()/codex-app-server-sdk/`; managed daemon startup is explicitly unsupported on Windows, where callers must use stdio or a separately managed URL.
- `CodexClient` provides high-level API entrypoints (`start_thread`, `resume_thread`, `as_api`) so stdio and ws clients can both use the same typed `run`/`run_streamed` thread flow.
- Use `connect_ws` to attach to a running websocket server, `start_ws_daemon` or `start_ws_blocking` for explicit startup, and `start_and_connect_ws` only as the loopback convenience wrapper. Optional `WsConfig.auth_token` / `with_auth_token(...)` sends `Authorization: Bearer` on the websocket upgrade (and on managed-loopback readiness probes).
- `Codex` forwards the full typed RPC surface (thread lifecycle, turn controls, auth/config, skills, MCP, review, and raw fallback) after ensuring handshake readiness.
- High-level API includes final-response shortcuts: `Thread::ask(...)`, `Codex::ask(...)`, and `Codex::ask_with_options(...)`, which return only the final agent message text.
- `StreamedTurn::turn_id()` exposes the active server turn ID so interactive consumers can call `Thread::interrupt(...)` and continue draining terminal events.
- Use `ThreadOptions::builder()` for API-level thread defaults; it now covers protocol-oriented fields beyond CLI parity (for example `model_provider`, `personality`, `sandbox_policy`, collaboration mode payload, and config/dynamic tool extras).
- Use `TurnOptions::builder()` for per-turn `cwd`, model, explicit reasoning effort, personality, approval, sandbox, collaboration, schemas, and extras.
- Use `ServiceTier::{Default, Fast}` with `ThreadOptions::builder().service_tier(...)` or `TurnOptions::builder().service_tier(...)`; the SDK sends app-server `serviceTier` consistently on thread start/resume and turn start.
- Typed schema generation is provided by `OpenAiSerializable` + `openai_json_schema_for::<T>()` (backed by `schemars`); derived schemas strip `$schema` metadata for OpenAI/Codex structured output compatibility.
- The `luna` binary exposes explicit commands: `exec` (also available as `x`) for one-shot runs, `chat` for a multi-turn Ratatui interface, `start` to explicitly ensure a websocket daemon is running, `sessions` for recorded-session listing, and `doctor` for offline/redacted or opt-in live diagnostics. `chat` and `exec` share one exact Clap flag contract; chat retains one thread, supports an optional initial prompt, requires interactive stdin/stdout, and provides host commands/autocomplete for compaction, effort, and enabled skills (no `/model` picker). Both turn commands support session continuation (`--continue` for latest, `--resume <session_id>` for explicit ids), default to websocket transport at `ws://127.0.0.1:4222` (with `--stdio` override), and automatically reuse or start that implicit default endpoint. Websocket URL precedence is `--ws-url`, `CODEX_APP_SERVER_WS_URL`, legacy `CODEX_WEB_SERVER_URL`, then the default; user-supplied endpoints are connect-only. Websocket auth uses `--ws-auth-token` / `--ws-auth-token-file` (`Authorization: Bearer`, `wss://` or loopback `ws://` only). Luna sets Codex `cwd` to the invocation directory by default (override with `--cwd <path>`), defaults model + reasoning to (`gpt-5.6-luna`, `max`), and allows optional overrides (`--model`, `--reasoning-effort`, and related config flags). `CODEX_BINARY` overrides OS-neutral PATH discovery.
- For websocket v2 compatibility, route search/sandbox workspace-write tuning through config overrides (`web_search`, `sandbox_workspace_write.network_access`, `sandbox_workspace_write.writable_roots`) and avoid relying on legacy extra fields like `webSearchEnabled`, `networkAccessEnabled`, `additionalDirectories`, or `skipGitRepoCheck` because app-server thread/turn params ignore them.
- `luna exec --agent <name>` resolves `~/.codex/config.toml` under `[agents.<name>]`, reads `config_file` (relative to the declaring config file), and maps role config instructions into thread `developer_instructions` with precedence: `developer_instructions` -> `model_instructions_file` contents -> role `description`.
- Use `cargo run -p luna -- ...` or `./target/release/luna ...` to run the repository binary explicitly.

## Critical Paths and Review Focus
- High-risk paths:
- `crates/sdk/src/client/mod.rs`: state machine, request correlation, timeout/error semantics.
- `crates/sdk/src/protocol/*`: wire compatibility and serde mapping.
- `crates/sdk/src/events/mod.rs`: event decoding and unknown fallback paths.
- `crates/sdk/tests/integration_stdio.rs`: live behavior contract.
- For edits in these files, add explicit before/after behavior notes in the PR description.
