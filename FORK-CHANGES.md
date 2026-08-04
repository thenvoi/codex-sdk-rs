# thenvoi/codex-sdk-rs fork changes

Rev-pinned fork consumed by [thenvoi/tjam](https://github.com/thenvoi/tjam).
The fork changes only the Rust client library linked into Jam; users continue
installing and running the stock OpenAI Codex CLI and app-server.

## 1. Owned stdio lifecycle and child working directory

`CodexClient::spawn_stdio_owned` returns the normal protocol client together
with a `StdioProcess` handle. The handle exposes PID/status observation, bounded
stderr capture, stdin close, graceful wait, and forced termination after a
caller-provided timeout. The additive API also accepts `current_dir` without
changing the existing `StdioConfig` struct-literal contract.

This is required by long-running embedders that must prove app-server teardown
and run the same JSONL protocol through wrappers such as `sbx exec -i`. The
existing detached `spawn_stdio` behavior remains unchanged.

Upstream PR: [thehumanworks/codex-sdk-rs#8](https://github.com/thehumanworks/codex-sdk-rs/pull/8).

## 2. Weekly upstream replay

`.github/workflows/jam-rebase-owned-stdio.yml` runs every Monday and on demand.
It derives every fork-only commit from the default maintenance branch, replays
that patch set onto current `thehumanworks/codex-sdk-rs` `main`, compares the
resulting tree with `jam-owned-stdio-latest`, validates the workspace plus SDK
unit tests, and force-with-lease updates the generated branch only when either
upstream or the maintained fork patch set changes.

A red run means upstream drifted under the patch and requires a manual rebase.
When the staged branch advances, tjam's `fork-freshness` workflow files an issue
until `Cargo.toml` and `Cargo.lock` pin the new revision.

### Manual conflict recovery

1. Record the failing workflow's upstream target and fork-only commit list.
2. Start a candidate branch at the latest `upstream/main` and replay those
   commits in order.
3. If upstream now provides a fork capability, omit that obsolete patch only
   after verifying the equivalent public API and its integration coverage.
4. Resolve remaining conflicts by preserving current upstream behavior and the
   smallest still-required fork delta, then run the workflow validation commands.
5. After review, update both `jam-owned-stdio` and
   `jam-owned-stdio-latest` to the same validated linear history with
   `--force-with-lease`, and dispatch this workflow once. Do not merge a recovery
   branch into the maintenance branch: the merge commit would itself become a
   fork-only replay input.

The July 2026 streamed-turn identity patch is intentionally absent from the
current fork delta because upstream now exposes `StreamedTurn::turn_id()` and
tests that the successful `turn/start` provider ID remains available.

## Upgrade procedure

1. Inspect the latest successful `jam-rebase-owned-stdio` run and its staged SHA.
2. Point tjam's `codex-app-server-sdk` git `rev` at that SHA.
3. Run `cargo update -p codex-app-server-sdk` and the full tjam verification gate.
4. Update tjam's fork comment and changelog if upstream behavior changed.
5. Remove the fork pin once a released upstream crate contains the owned-stdio API.
