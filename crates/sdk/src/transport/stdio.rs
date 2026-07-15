use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::{RawFrame, TransportHandle, read_json_frames, transport_channels, write_json_frames};
use crate::error::ClientError;

pub async fn spawn_stdio_transport(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<TransportHandle, ClientError> {
    let mut command = stdio_command(binary, args, env, None);
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
    current_dir: Option<&Path>,
) -> Result<OwnedStdioTransport, ClientError> {
    let mut command = stdio_command(binary, args, env, current_dir);
    command.stderr(Stdio::piped()).kill_on_drop(true);
    spawn_stdio_command(command).await
}

fn stdio_command(
    binary: &str,
    args: &[String],
    env: &HashMap<String, String>,
    current_dir: Option<&Path>,
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
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
