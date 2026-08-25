use super::*;
use serde_json::{Value, json};

fn assert_omits(params: &Value, fields: &[&str]) {
    for field in fields {
        assert!(
            params.get(*field).is_none(),
            "request unexpectedly includes {field}: {params}"
        );
    }
}

#[test]
fn serializes_thread_defaults_and_turn_overrides_on_their_owned_requests() {
    let thread_options = ThreadOptions::builder()
        .model("gpt-thread")
        .model_provider("provider-thread")
        .working_directory("/workspace")
        .approval_policy(ApprovalMode::OnRequest)
        .sandbox_mode(SandboxMode::WorkspaceWrite)
        .base_instructions("base instructions")
        .developer_instructions("developer instructions")
        .model_reasoning_effort(ModelReasoningEffort::High)
        .web_search_enabled(true)
        .insert_config("profile", json!("retained"))
        .insert_config("model_reasoning_effort", json!("low"))
        .insert_config("web_search", json!("disabled"))
        .dynamic_tools(vec![DynamicToolSpec::new(
            "demo_tool",
            "Demo dynamic tool",
            json!({"type": "object"}),
        )])
        .build();

    let start = serde_json::to_value(build_thread_start_params(&thread_options))
        .expect("serialize thread/start");
    assert_eq!(
        start,
        json!({
            "model": "gpt-thread",
            "modelProvider": "provider-thread",
            "cwd": "/workspace",
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "config": {
                "profile": "retained",
                "model_reasoning_effort": "high",
                "web_search": "live"
            },
            "baseInstructions": "base instructions",
            "developerInstructions": "developer instructions",
            "dynamicTools": [{
                "name": "demo_tool",
                "description": "Demo dynamic tool",
                "inputSchema": {"type": "object"}
            }]
        })
    );
    assert_omits(&start, &["effort", "webSearchEnabled", "webSearchMode"]);

    let resume = serde_json::to_value(build_thread_resume_params("thread-1", &thread_options))
        .expect("serialize thread/resume");
    assert_eq!(
        resume,
        json!({
            "threadId": "thread-1",
            "model": "gpt-thread",
            "modelProvider": "provider-thread",
            "cwd": "/workspace",
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "config": {
                "profile": "retained",
                "model_reasoning_effort": "high",
                "web_search": "live"
            },
            "baseInstructions": "base instructions",
            "developerInstructions": "developer instructions"
        })
    );
    assert_omits(
        &resume,
        &[
            "dynamicTools",
            "effort",
            "webSearchEnabled",
            "webSearchMode",
        ],
    );

    let turn_options = TurnOptions::builder()
        .model("gpt-turn")
        .model_reasoning_effort(ModelReasoningEffort::Low)
        .approval_policy(ApprovalMode::Never)
        .sandbox_policy(json!({"type": "dangerFullAccess"}))
        .insert_extra("customTurnFlag", json!(true))
        .insert_extra("modelProvider", json!("provider-turn"))
        .insert_extra("config", json!({"model_reasoning_effort": "xhigh"}))
        .insert_extra("webSearchEnabled", json!(true))
        .insert_extra("webSearchMode", json!("live"))
        .insert_extra("web_search", json!("live"))
        .build();
    let turn = serde_json::to_value(build_turn_start_params(
        "thread-1",
        Input::text("hello"),
        &thread_options,
        &turn_options,
    ))
    .expect("serialize turn/start");
    assert_eq!(
        turn,
        json!({
            "threadId": "thread-1",
            "input": [{"type": "text", "text": "hello"}],
            "cwd": "/workspace",
            "model": "gpt-turn",
            "effort": "low",
            "approvalPolicy": "never",
            "sandboxPolicy": {"type": "dangerFullAccess"},
            "customTurnFlag": true
        })
    );
    assert_omits(
        &turn,
        &[
            "modelProvider",
            "config",
            "webSearch",
            "webSearchEnabled",
            "webSearchMode",
            "web_search",
        ],
    );
}

#[test]
fn boolean_thread_web_search_serializes_to_provider_modes_or_omits() {
    for (enabled, expected) in [(true, "live"), (false, "disabled")] {
        let options = ThreadOptions::builder().web_search_enabled(enabled).build();
        let start = serde_json::to_value(build_thread_start_params(&options))
            .expect("serialize thread/start");
        let resume = serde_json::to_value(build_thread_resume_params("thread-1", &options))
            .expect("serialize thread/resume");

        assert_eq!(start.pointer("/config/web_search"), Some(&json!(expected)));
        assert_eq!(resume.pointer("/config/web_search"), Some(&json!(expected)));
        assert_omits(&start, &["webSearchEnabled", "webSearchMode"]);
        assert_omits(&resume, &["webSearchEnabled", "webSearchMode"]);
    }

    let options = ThreadOptions::default();
    let start =
        serde_json::to_value(build_thread_start_params(&options)).expect("serialize thread/start");
    let resume = serde_json::to_value(build_thread_resume_params("thread-1", &options))
        .expect("serialize thread/resume");
    assert!(start.get("config").is_none());
    assert!(resume.get("config").is_none());
}

#[test]
fn explicit_web_search_mode_overrides_boolean_and_generic_config() {
    let options = ThreadOptions::builder()
        .web_search_enabled(true)
        .web_search_mode(WebSearchMode::Cached)
        .insert_config("web_search", json!("disabled"))
        .build();

    let start =
        serde_json::to_value(build_thread_start_params(&options)).expect("serialize thread/start");
    let resume = serde_json::to_value(build_thread_resume_params("thread-1", &options))
        .expect("serialize thread/resume");
    assert_eq!(start.pointer("/config/web_search"), Some(&json!("cached")));
    assert_eq!(resume.pointer("/config/web_search"), Some(&json!("cached")));
}

#[test]
fn turn_start_does_not_inherit_thread_default_effort() {
    let thread_options = ThreadOptions::builder()
        .model_reasoning_effort(ModelReasoningEffort::High)
        .build();
    let turn = serde_json::to_value(build_turn_start_params(
        "thread-1",
        Input::text("hello"),
        &thread_options,
        &TurnOptions::default(),
    ))
    .expect("serialize turn/start");

    assert_omits(&turn, &["effort"]);
}
