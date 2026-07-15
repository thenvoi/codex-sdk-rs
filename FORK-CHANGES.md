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

Upstream PR: recorded here once opened.

## 2. Weekly upstream replay

`.github/workflows/jam-rebase-owned-stdio.yml` runs every Monday and on demand.
It derives every fork-only commit from the default maintenance branch, replays
that patch set onto current `thehumanworks/codex-sdk-rs` `main`, validates the
workspace plus SDK unit tests, and force-with-lease updates
`jam-owned-stdio-latest` only when upstream advances.

A red run means upstream drifted under the patch and requires a manual rebase.
When the staged branch advances, tjam's `fork-freshness` workflow files an issue
until `Cargo.toml` and `Cargo.lock` pin the new revision.

## Upgrade procedure

1. Inspect the latest successful `jam-rebase-owned-stdio` run and its staged SHA.
2. Point tjam's `codex-app-server-sdk` git `rev` at that SHA.
3. Run `cargo update -p codex-app-server-sdk` and the full tjam verification gate.
4. Update tjam's fork comment and changelog if upstream behavior changed.
5. Remove the fork pin once a released upstream crate contains the owned-stdio API.
