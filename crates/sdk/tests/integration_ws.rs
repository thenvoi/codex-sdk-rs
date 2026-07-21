use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use codex_app_server_sdk::api::{ThreadEvent, ThreadOptions, TurnOptions};
use codex_app_server_sdk::protocol::requests::{ClientInfo, InitializeParams};
use codex_app_server_sdk::{ClientOptions, CodexClient, WsConfig, WsStartConfig, WsStartMode};

const STREAM_EVENT_TIMEOUT: Duration = Duration::from_secs(10);

fn reserve_local_ws_url() -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);

    Ok(format!("ws://127.0.0.1:{}", addr.port()))
}

fn isolated_codex_home() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-sdk-rs-ws-integration-home-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(path.join(".codex")).expect("create isolated codex home");
    path
}

fn isolate_home_enabled() -> bool {
    std::env::var("CODEX_SDK_TEST_ISOLATE_HOME")
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "1" || normalized == "true" || normalized == "yes"
        })
        .unwrap_or(false)
}

fn isolated_ws_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if isolate_home_enabled() {
        let isolated_home = isolated_codex_home();
        env.insert(
            "HOME".to_string(),
            isolated_home.to_string_lossy().to_string(),
        );
        env.insert(
            "CODEX_HOME".to_string(),
            isolated_home.to_string_lossy().to_string(),
        );
    }
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        env.insert("OPENAI_API_KEY".to_string(), api_key);
    }
    env
}

fn ws_client_config(url: &str) -> WsConfig {
    WsConfig {
        url: url.to_string(),
        env: isolated_ws_env(),
        options: ClientOptions::default(),
    }
}

fn ws_start_config(listen_url: &str, connect_url: &str) -> WsStartConfig {
    WsStartConfig::new(
        listen_url.to_string(),
        connect_url.to_string(),
        isolated_ws_env(),
    )
}

async fn initialize_client(client: &CodexClient) -> Result<(), Box<dyn std::error::Error>> {
    client
        .initialize(InitializeParams::new(ClientInfo::new(
            "integration_ws_test",
            "Integration WS Test",
            env!("CARGO_PKG_VERSION"),
        )))
        .await?;
    client.initialized().await?;
    Ok(())
}

async fn connect_initialized_ws_client(
    url: &str,
) -> Result<CodexClient, Box<dyn std::error::Error>> {
    let client = CodexClient::start_and_connect_ws(ws_client_config(url)).await?;
    initialize_client(&client).await?;

    Ok(client)
}

#[tokio::test]
async fn auto_starts_and_reuses_persistent_loopback_websocket_server()
-> Result<(), Box<dyn std::error::Error>> {
    let url = reserve_local_ws_url()?;

    let first_client = connect_initialized_ws_client(&url).await?;
    drop(first_client);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let second_client = connect_initialized_ws_client(&url).await?;
    drop(second_client);

    Ok(())
}

#[tokio::test]
async fn ws_client_start_thread_runs_and_streams() -> Result<(), Box<dyn std::error::Error>> {
    let url = reserve_local_ws_url()?;
    let client = connect_initialized_ws_client(&url).await?;

    let mut thread = client.start_thread(ThreadOptions::default());
    let turn = thread
        .run("Reply with exactly: ok", TurnOptions::default())
        .await?;

    assert!(thread.id().is_some(), "thread id should be populated");
    assert!(
        !turn.final_response.trim().is_empty(),
        "final response should not be empty"
    );

    let mut streamed = thread
        .run_streamed("Reply with exactly: ok", TurnOptions::default())
        .await?;
    let mut saw_terminal = false;

    while let Some(next) = tokio::time::timeout(STREAM_EVENT_TIMEOUT, streamed.next_event()).await?
    {
        let event = next?;
        match event {
            ThreadEvent::TurnCompleted { .. } => {
                saw_terminal = true;
                break;
            }
            ThreadEvent::TurnFailed { error } => {
                return Err(format!("turn failed unexpectedly: {}", error.message).into());
            }
            ThreadEvent::ThreadStarted { .. }
            | ThreadEvent::TurnStarted
            | ThreadEvent::ItemStarted { .. }
            | ThreadEvent::ItemUpdated { .. }
            | ThreadEvent::ItemCompleted { .. }
            | ThreadEvent::Error { .. } => {}
        }
    }

    assert!(saw_terminal, "expected streamed turn completion event");

    Ok(())
}

#[tokio::test]
async fn test_connect_ws_does_not_start_daemon() {
    let url = "ws://127.0.0.1:4234"; // Random unused port
    let config = WsConfig {
        url: url.to_string(),
        env: HashMap::new(),
        options: ClientOptions::default(),
    };
    let result = CodexClient::connect_ws(config).await;
    assert!(
        result.is_err(),
        "Expected connect_ws to fail without start_and_connect_ws"
    );
}

#[tokio::test]
async fn start_ws_daemon_supports_separate_listen_and_connect_urls()
-> Result<(), Box<dyn std::error::Error>> {
    let listen_url = reserve_local_ws_url()?;
    let connect_url = format!("{listen_url}/");

    let server = CodexClient::start_ws_daemon(ws_start_config(&listen_url, &connect_url)).await?;
    assert_eq!(server.mode(), WsStartMode::Daemon);
    assert_eq!(server.listen_url(), listen_url);
    assert_eq!(server.connect_url(), connect_url);
    assert!(server.started_new_process(), "expected a new daemon");

    let client = CodexClient::connect_ws(server.connect_config(ClientOptions::default())).await?;
    initialize_client(&client).await?;

    Ok(())
}

#[tokio::test]
async fn start_ws_blocking_returns_owned_handle_and_can_shutdown()
-> Result<(), Box<dyn std::error::Error>> {
    let url = reserve_local_ws_url()?;
    let mut server = CodexClient::start_ws_blocking(ws_start_config(&url, &url)).await?;
    assert_eq!(server.mode(), WsStartMode::Blocking);
    assert!(server.owns_process(), "blocking mode should own the child");
    assert!(
        server.started_new_process(),
        "blocking mode should start a child"
    );

    let client = CodexClient::connect_ws(server.connect_config(ClientOptions::default())).await?;
    initialize_client(&client).await?;
    drop(client);

    server.shutdown()?;
    let port = url::Url::parse(&url)?
        .port()
        .ok_or("url should include port")?;
    let mut rebound = false;
    for _ in 0..10 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            rebound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(rebound, "expected blocking server port to become free");

    Ok(())
}

#[tokio::test]
async fn start_ws_daemon_errors_when_reuse_existing_is_disabled()
-> Result<(), Box<dyn std::error::Error>> {
    let url = reserve_local_ws_url()?;
    let first = CodexClient::start_ws_daemon(ws_start_config(&url, &url)).await?;
    assert!(
        first.started_new_process(),
        "first startup should spawn daemon"
    );

    let result =
        CodexClient::start_ws_daemon(ws_start_config(&url, &url).with_reuse_existing(false)).await;

    match result {
        Err(codex_app_server_sdk::ClientError::TransportSend(message)) => {
            assert!(
                message.contains("reuse_existing is disabled"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected reuse_existing failure, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn start_ws_daemon_fails_when_port_is_occupied_by_non_websocket_service()
-> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!("ws://127.0.0.1:{}", addr.port());

    let server_task = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut request = vec![0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        }
    });

    let result = CodexClient::start_ws_daemon(ws_start_config(&url, &url)).await;
    server_task.await?;

    match result {
        Err(codex_app_server_sdk::ClientError::TransportSend(message)) => {
            assert!(
                message.contains("websocket startup conflict"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected startup conflict, got {other:?}"),
    }

    Ok(())
}
