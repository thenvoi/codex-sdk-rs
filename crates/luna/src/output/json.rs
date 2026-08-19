use std::io::{self, Write};

use codex_app_server_sdk::api::{StreamedTurn, ThreadEvent, ThreadItem, UserMessageContentItem};
use serde_json::Value;

use crate::error::LunaError;

pub(crate) async fn stream_json_events(streamed: &mut StreamedTurn) -> Result<(), LunaError> {
    let mut stdout = io::stdout();

    while let Some(next) = streamed.next_event().await {
        let event = next?;
        let json_event = thread_event_to_json(&event);

        let line = serde_json::to_string(&json_event)
            .map_err(|err| LunaError::Protocol(format!("failed to serialize event: {err}")))?;
        writeln!(stdout, "{line}")?;

        match event {
            ThreadEvent::TurnCompleted { .. } => break,
            ThreadEvent::TurnFailed { error } => {
                return Err(LunaError::Turn(format!("turn failed: {}", error.message)));
            }
            ThreadEvent::Error { message } => {
                return Err(LunaError::Turn(format!("stream error: {message}")));
            }
            _ => {}
        }
    }

    Ok(())
}

pub(crate) fn thread_event_to_json(event: &ThreadEvent) -> Value {
    match event {
        ThreadEvent::ThreadStarted { thread_id } => {
            serde_json::json!({ "type": "thread.started", "threadId": thread_id })
        }
        ThreadEvent::TurnStarted => serde_json::json!({ "type": "turn.started" }),
        ThreadEvent::TurnCompleted {
            usage,
            terminal_status,
        } => {
            let mut obj = serde_json::json!({ "type": "turn.completed" });
            if let Some(usage) = usage {
                obj["usage"] = serde_json::json!({
                    "inputTokens": usage.input_tokens,
                    "cachedInputTokens": usage.cached_input_tokens,
                    "outputTokens": usage.output_tokens,
                });
            }
            if let Some(terminal_status) = terminal_status {
                obj["terminalStatus"] = serde_json::json!(terminal_status);
            }
            obj
        }
        ThreadEvent::TurnFailed { error } => {
            serde_json::json!({ "type": "turn.failed", "error": { "message": error.message } })
        }
        ThreadEvent::ItemStarted { item } => {
            serde_json::json!({ "type": "item.started", "item": thread_item_to_json(item) })
        }
        ThreadEvent::ItemUpdated { item } => {
            serde_json::json!({ "type": "item.updated", "item": thread_item_to_json(item) })
        }
        ThreadEvent::ItemCompleted { item } => {
            serde_json::json!({ "type": "item.completed", "item": thread_item_to_json(item) })
        }
        ThreadEvent::Error { message } => {
            serde_json::json!({ "type": "error", "message": message })
        }
    }
}

pub(crate) fn thread_item_to_json(item: &ThreadItem) -> Value {
    match item {
        ThreadItem::AgentMessage(msg) => {
            let mut value = serde_json::json!({
                "type": "agentMessage",
                "id": msg.id,
                "text": msg.text,
            });
            if let Some(phase) = msg.phase {
                value["phase"] = Value::String(phase.as_str().to_string());
            }
            value
        }
        ThreadItem::UserMessage(msg) => {
            let content: Vec<Value> = msg
                .content
                .iter()
                .map(|entry| match entry {
                    UserMessageContentItem::Text { text } => {
                        serde_json::json!({ "type": "text", "text": text })
                    }
                    UserMessageContentItem::Image { url } => {
                        serde_json::json!({ "type": "image", "url": url })
                    }
                    UserMessageContentItem::LocalImage { path } => {
                        serde_json::json!({ "type": "localImage", "path": path })
                    }
                    UserMessageContentItem::Unknown(raw) => raw.clone(),
                })
                .collect();
            serde_json::json!({
                "type": "userMessage",
                "id": msg.id,
                "content": content,
            })
        }
        ThreadItem::Plan(plan) => serde_json::json!({
            "type": "plan",
            "id": plan.id,
            "text": plan.text,
        }),
        ThreadItem::Reasoning(reasoning) => serde_json::json!({
            "type": "reasoning",
            "id": reasoning.id,
            "text": reasoning.text,
        }),
        ThreadItem::CommandExecution(command) => serde_json::json!({
            "type": "commandExecution",
            "id": command.id,
            "command": command.command,
            "aggregatedOutput": command.aggregated_output,
            "exitCode": command.exit_code,
            "status": command.status.as_str(),
        }),
        ThreadItem::FileChange(file_change) => {
            let changes: Vec<Value> = file_change
                .changes
                .iter()
                .map(|change| {
                    serde_json::json!({
                        "path": change.path,
                        "kind": change.kind.as_str(),
                    })
                })
                .collect();
            serde_json::json!({
                "type": "fileChange",
                "id": file_change.id,
                "changes": changes,
                "status": file_change.status.as_str(),
            })
        }
        ThreadItem::McpToolCall(tool) => serde_json::json!({
            "type": "mcpToolCall",
            "id": tool.id,
            "server": tool.server,
            "tool": tool.tool,
            "arguments": tool.arguments,
            "result": tool.result,
            "error": tool.error.as_ref().map(|error| &error.message),
            "status": tool.status.as_str(),
        }),
        ThreadItem::DynamicToolCall(tool) => serde_json::json!({
            "type": "dynamicToolCall",
            "id": tool.id,
            "tool": tool.tool,
            "arguments": tool.arguments,
            "status": tool.status,
            "contentItems": tool.content_items,
            "success": tool.success,
            "durationMs": tool.duration_ms,
        }),
        ThreadItem::CollabToolCall(tool) => serde_json::json!({
            "type": "collabToolCall",
            "id": tool.id,
            "tool": tool.tool,
            "status": tool.status,
            "senderThreadId": tool.sender_thread_id,
            "receiverThreadId": tool.receiver_thread_id,
            "newThreadId": tool.new_thread_id,
            "prompt": tool.prompt,
            "agentStatus": tool.agent_status,
        }),
        ThreadItem::WebSearch(search) => serde_json::json!({
            "type": "webSearch",
            "id": search.id,
            "query": search.query,
        }),
        ThreadItem::ImageView(image) => serde_json::json!({
            "type": "imageView",
            "id": image.id,
            "path": image.path,
        }),
        ThreadItem::EnteredReviewMode(review) => serde_json::json!({
            "type": "enteredReviewMode",
            "id": review.id,
            "review": review.review,
        }),
        ThreadItem::ExitedReviewMode(review) => serde_json::json!({
            "type": "exitedReviewMode",
            "id": review.id,
            "review": review.review,
        }),
        ThreadItem::ContextCompaction(item) => serde_json::json!({
            "type": "contextCompaction",
            "id": item.id,
        }),
        ThreadItem::TodoList(todo) => {
            let items: Vec<Value> = todo
                .items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "text": item.text,
                        "completed": item.completed,
                    })
                })
                .collect();
            serde_json::json!({
                "type": "todoList",
                "id": todo.id,
                "items": items,
            })
        }
        ThreadItem::Error(error) => serde_json::json!({
            "type": "error",
            "id": error.id,
            "message": error.message,
        }),
        ThreadItem::Unknown(unknown) => serde_json::json!({
            "type": "unknown",
            "id": unknown.id,
            "itemType": unknown.item_type,
            "raw": unknown.raw,
        }),
    }
}

#[cfg(test)]
mod tests {
    use codex_app_server_sdk::{AgentMessageItem, AgentMessagePhase};

    use super::*;

    #[test]
    fn agent_message_json_shape_includes_phase_without_other_changes() {
        let value = thread_item_to_json(&ThreadItem::AgentMessage(AgentMessageItem {
            id: "msg_1".to_string(),
            text: "done".to_string(),
            phase: Some(AgentMessagePhase::FinalAnswer),
        }));
        assert_eq!(
            value,
            serde_json::json!({
                "type": "agentMessage",
                "id": "msg_1",
                "text": "done",
                "phase": "final_answer",
            })
        );
    }
}
