use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::TransportHandle;
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

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Value>(256);
    let (inbound_tx, inbound_rx) = mpsc::channel::<Result<Value, ClientError>>(1024);

    let inbound_for_writer = inbound_tx.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            match serde_json::to_string(&message) {
                Ok(line) => {
                    if let Err(err) = stdin.write_all(line.as_bytes()).await {
                        let _ = inbound_for_writer.send(Err(ClientError::Io(err))).await;
                        break;
                    }
                    if let Err(err) = stdin.write_all(b"\n").await {
                        let _ = inbound_for_writer.send(Err(ClientError::Io(err))).await;
                        break;
                    }
                }
                Err(err) => {
                    let _ = inbound_for_writer
                        .send(Err(ClientError::Serialization(err)))
                        .await;
                    break;
                }
            }
        }
    });

    let inbound_for_reader = inbound_tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        if inbound_for_reader.send(Ok(value)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        if inbound_for_reader
                            .send(Err(ClientError::InvalidMessage(format!(
                                "failed to parse JSONL frame: {err}"
                            ))))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                },
                Ok(None) => {
                    let _ = inbound_for_reader
                        .send(Err(ClientError::TransportClosed))
                        .await;
                    break;
                }
                Err(err) => {
                    let _ = inbound_for_reader.send(Err(ClientError::Io(err))).await;
                    break;
                }
            }
        }
    });

    Ok(OwnedStdioTransport {
        handle: TransportHandle {
            outbound: outbound_tx,
            inbound: inbound_rx,
        },
        child,
        writer_task,
    })
}
