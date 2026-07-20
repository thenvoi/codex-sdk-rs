use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use codex_app_server_sdk::api::{
    Codex, ResumeThread, ThreadEvent, ThreadItem, ThreadOptions, TurnOptions,
};
use codex_app_server_sdk::{CodexClient, JsonSchema, OpenAiSerializable, StdioConfig};
use serde::{Deserialize, Serialize};

const TEST_TIMEOUT: Duration = Duration::from_secs(90);

fn isolated_codex_home() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-sdk-rs-api-integration-home-{}-{stamp}",
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

fn isolated_stdio_config() -> StdioConfig {
    let mut config = StdioConfig::default();
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
    config.env = env;
    config
}

fn unique_token(label: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    format!("{label}-{}-{stamp}", std::process::id())
}

fn unique_working_directory(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-sdk-rs-api-{label}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create working directory");
    path
}

async fn seed_session(
    codex: &Codex,
    token: &str,
    options: ThreadOptions,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut thread = codex.start_thread(options);
    let prompt = format!(
        "Remember this exact sentinel token for future turns in this session: {token}. Reply with exactly STORED."
    );
    let response = thread.ask(prompt, TurnOptions::default()).await?;
    assert!(
        !response.trim().is_empty(),
        "seed response should not be empty"
    );
    Ok(thread
        .id()
        .ok_or("thread id should be populated after seeding")?
        .to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, OpenAiSerializable)]
struct SchemaConstrainedResponse {
    answer: String,
}

#[tokio::test]
async fn run_collects_typed_items_and_response() -> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(isolated_stdio_config()).await?;
    let mut thread = codex.start_thread(ThreadOptions::default());

    let result = thread
        .run("Reply with exactly: ok", TurnOptions::default())
        .await?;

    assert!(thread.id().is_some(), "thread id should be populated");
    assert!(
        !result.items.is_empty(),
        "turn should contain completed items"
    );
    assert!(
        !result.final_response.trim().is_empty(),
        "final response should not be empty"
    );
    let final_answer_item = result.items.iter().find_map(|item| match item {
        ThreadItem::AgentMessage(message) if message.is_final_answer() => Some(message),
        _ => None,
    });
    if let Some(message) = final_answer_item {
        assert_eq!(
            result.final_response, message.text,
            "final_response should match the final_answer agent message when provided"
        );
    }

    Ok(())
}

#[tokio::test]
async fn ask_returns_final_response_only() -> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
    let mut thread = codex.start_thread(ThreadOptions::default());

    let final_response = thread
        .ask("Reply with exactly: ok", TurnOptions::default())
        .await?;

    assert!(thread.id().is_some(), "thread id should be populated");
    assert!(
        !final_response.trim().is_empty(),
        "final response should not be empty"
    );

    Ok(())
}

#[tokio::test]
async fn codex_ask_with_options_returns_final_response_only()
-> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(StdioConfig::default()).await?;

    let final_response = codex
        .ask_with_options(
            "Reply with exactly: ok",
            ThreadOptions::default(),
            TurnOptions::default(),
        )
        .await?;

    assert!(
        !final_response.trim().is_empty(),
        "final response should not be empty"
    );

    Ok(())
}

#[tokio::test]
async fn run_streamed_emits_turn_lifecycle_events() -> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(isolated_stdio_config()).await?;
    let mut thread = codex.start_thread(ThreadOptions::default());

    let mut streamed = thread
        .run_streamed("Reply with exactly: ok", TurnOptions::default())
        .await?;
    assert!(
        !streamed.turn_id().is_empty(),
        "streamed turn must expose the provider turn id returned by turn/start"
    );

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut saw_started = false;
    let mut saw_completed = false;

    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_secs(2), streamed.next_event()).await;
        let Some(event) = (match next {
            Ok(event) => event,
            Err(_) => continue,
        }) else {
            break;
        };

        let event = event?;
        match event {
            ThreadEvent::ThreadStarted { .. } | ThreadEvent::TurnStarted => {
                saw_started = true;
            }
            ThreadEvent::TurnCompleted { .. } => {
                saw_completed = true;
                break;
            }
            ThreadEvent::TurnFailed { error } => {
                return Err(format!("turn failed unexpectedly: {}", error.message).into());
            }
            ThreadEvent::ItemStarted { .. }
            | ThreadEvent::ItemUpdated { .. }
            | ThreadEvent::ItemCompleted { .. }
            | ThreadEvent::Error { .. } => {}
        }
    }

    assert!(saw_started, "expected started events");
    assert!(saw_completed, "expected turn completion event");

    Ok(())
}

#[tokio::test]
async fn codex_client_start_thread_runs_typed_api() -> Result<(), Box<dyn std::error::Error>> {
    let client = CodexClient::spawn_stdio(isolated_stdio_config()).await?;
    let mut thread = client.start_thread(ThreadOptions::default());

    let result = thread
        .run("Reply with exactly: ok", TurnOptions::default())
        .await?;

    assert!(thread.id().is_some(), "thread id should be populated");
    assert!(
        !result.final_response.trim().is_empty(),
        "final response should not be empty"
    );

    Ok(())
}

#[tokio::test]
async fn codex_resume_thread_by_id_reuses_existing_thread() -> Result<(), Box<dyn std::error::Error>>
{
    let codex = Codex::spawn_stdio(isolated_stdio_config()).await?;
    let token = unique_token("resume-by-id");
    let thread_id = seed_session(&codex, &token, ThreadOptions::default()).await?;

    let mut thread = codex.resume_thread(thread_id.clone(), ThreadOptions::default());
    let response = thread
        .ask(
            "Return only the sentinel token from earlier in this same session. Do not add any other text.",
            TurnOptions::default(),
        )
        .await?;

    assert_eq!(thread.id(), Some(thread_id.as_str()));
    assert!(
        response.contains(&token),
        "resume by id should return the seeded sentinel token"
    );

    Ok(())
}

#[tokio::test]
async fn codex_resume_latest_thread_uses_most_recent_matching_working_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(isolated_stdio_config()).await?;
    let target_cwd = unique_working_directory("resume-latest-target");
    let other_cwd = unique_working_directory("resume-latest-other");
    let target_cwd = target_cwd.to_string_lossy().to_string();
    let other_cwd = other_cwd.to_string_lossy().to_string();

    let older_token = unique_token("resume-latest-old");
    let other_token = unique_token("resume-latest-other");
    let newer_token = unique_token("resume-latest-new");

    let _older_id = seed_session(
        &codex,
        &older_token,
        ThreadOptions::builder()
            .working_directory(target_cwd.clone())
            .build(),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(25)).await;

    let _other_id = seed_session(
        &codex,
        &other_token,
        ThreadOptions::builder()
            .working_directory(other_cwd.clone())
            .build(),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(25)).await;

    let newer_id = seed_session(
        &codex,
        &newer_token,
        ThreadOptions::builder()
            .working_directory(target_cwd.clone())
            .build(),
    )
    .await?;

    let mut thread = codex.resume_thread(
        ResumeThread::Latest,
        ThreadOptions::builder()
            .working_directory(target_cwd.clone())
            .build(),
    );
    let response = thread
        .ask(
            "Return only the sentinel token from earlier in this same session. Do not add any other text.",
            TurnOptions::default(),
        )
        .await?;

    assert_eq!(thread.id(), Some(newer_id.as_str()));
    assert!(
        response.contains(&newer_token),
        "resume latest should return the newest sentinel token in the matching working directory"
    );
    assert!(
        !response.contains(&older_token),
        "resume latest should not return the older matching thread token"
    );
    assert!(
        !response.contains(&other_token),
        "resume latest should not return a token from a different working directory"
    );

    Ok(())
}

#[tokio::test]
async fn run_respects_output_schema_for_typed_deserialization()
-> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(isolated_stdio_config()).await?;
    let mut thread = codex.start_thread(ThreadOptions::default());

    let turn_options = TurnOptions::builder()
        .output_schema_for::<SchemaConstrainedResponse>()
        .build();

    let result = thread
        .run(
            "Respond with strict JSON only using one field named answer with value ok.",
            turn_options,
        )
        .await?;

    let value: serde_json::Value = serde_json::from_str(&result.final_response)?;
    let parsed = SchemaConstrainedResponse::from_openai_value(value)?;
    assert!(
        !parsed.answer.trim().is_empty(),
        "expected non-empty answer in schema constrained output"
    );

    Ok(())
}
