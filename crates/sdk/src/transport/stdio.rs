use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::{RawFrame, TransportHandle, read_json_frames, transport_channels, write_json_frames};
use crate::error::ClientError;

#[cfg(windows)]
fn no_console_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_command: &mut Command) {}

pub async fn spawn_stdio_transport(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<TransportHandle, ClientError> {
    let mut command = stdio_command(binary, args, env, &[], None);
    command.stderr(Stdio::inherit());
    let spawned = spawn_stdio_command(command).await?;
    tokio::spawn(async move {
        let mut child = spawned.child;
        let _ = child.wait().await;
    });
    Ok(spawned.handle)
}

pub(crate) struct OwnedStdioTransport {
    pub handle: TransportHandle,
    pub child: Child,
    pub writer_task: JoinHandle<()>,
}

pub(crate) async fn spawn_owned_stdio_transport(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
    env_remove: &[String],
    current_dir: Option<&Path>,
) -> Result<OwnedStdioTransport, ClientError> {
    let mut command = stdio_command(binary, args, env, env_remove, current_dir);
    command.stderr(Stdio::piped()).kill_on_drop(true);
    spawn_stdio_command(command).await
}

/// The app-server command: the parent's environment without `env_remove`,
/// then `env`, so a variable `env` sets reaches the child even when
/// `env_remove` names it.
fn stdio_command(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
    env_remove: &[String],
    current_dir: Option<&Path>,
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    for key in env_remove {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    no_console_window(&mut command);
    command
}

async fn spawn_stdio_command(mut command: Command) -> Result<OwnedStdioTransport, ClientError> {
    let mut child = command.spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ClientError::TransportSend("missing child stdin".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ClientError::TransportSend("missing child stdout".to_string()))?;

    let (outbound_tx, outbound_rx, inbound_tx, inbound_rx) = transport_channels();

    let writer_task = tokio::spawn(write_json_frames(
        outbound_rx,
        inbound_tx.clone(),
        async move |payload: String| {
            stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(ClientError::Io)?;
            stdin.write_all(b"\n").await.map_err(ClientError::Io)
        },
    ));

    let mut lines = BufReader::new(stdout).lines();
    tokio::spawn(read_json_frames(
        inbound_tx,
        "JSONL frame",
        async move || match lines.next_line().await {
            Ok(Some(line)) => RawFrame::Payload(line.into_bytes()),
            Ok(None) => RawFrame::Closed(ClientError::TransportClosed),
            Err(err) => RawFrame::Closed(ClientError::Io(err)),
        },
    ));

    Ok(OwnedStdioTransport {
        handle: TransportHandle {
            outbound: outbound_tx,
            inbound: inbound_rx,
        },
        child,
        writer_task,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;

    use super::stdio_command;

    /// The names of the variables a child printed with `env`.
    async fn child_env(env: &HashMap<String, String>, env_remove: &[String]) -> Vec<String> {
        let mut command = stdio_command("/usr/bin/env", &[], env, env_remove, None);
        let output = command.output().await.expect("run env");
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn a_removed_variable_does_not_reach_the_child() {
        let inherited = child_env(&HashMap::new(), &[]).await;
        assert!(inherited.contains(&"HOME".to_owned()), "{inherited:?}");

        let env = HashMap::from([("KEPT".to_owned(), "set".to_owned())]);
        let names = child_env(&env, &["HOME".to_owned()]).await;
        assert!(!names.contains(&"HOME".to_owned()), "{names:?}");
        assert!(names.contains(&"KEPT".to_owned()), "{names:?}");
        assert!(names.contains(&"PATH".to_owned()), "{names:?}");
    }

    #[tokio::test]
    async fn a_variable_env_sets_reaches_the_child_even_when_removed() {
        let env = HashMap::from([("HOME".to_owned(), "/set/on/purpose".to_owned())]);
        let names = child_env(&env, &["HOME".to_owned()]).await;
        assert!(names.contains(&"HOME".to_owned()), "{names:?}");
    }
}
