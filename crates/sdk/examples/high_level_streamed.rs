use codex_app_server_sdk::StdioConfig;
use codex_app_server_sdk::ThreadEventRenderer;
use codex_app_server_sdk::api::{Codex, ThreadEvent, ThreadOptions, TurnOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let codex = Codex::spawn_stdio(StdioConfig::default()).await?;
    let mut thread = codex.start_thread(ThreadOptions::default());

    let mut streamed = thread
        .run_streamed("Reply with exactly: ok", TurnOptions::default())
        .await?;

    let mut renderer = ThreadEventRenderer::new();
    while let Some(event) = streamed.next_event().await {
        let event = event?;
        if let Some(fragment) = renderer.render(&event) {
            println!("{}", fragment.markdown);
        }
        match event {
            ThreadEvent::ThreadStarted { thread_id } => {
                println!("thread started: {thread_id}");
            }
            ThreadEvent::TurnStarted => {
                println!("turn started");
            }
            ThreadEvent::ItemUpdated { item }
            | ThreadEvent::ItemStarted { item }
            | ThreadEvent::ItemCompleted { item } => {
                let _ = item;
            }
            ThreadEvent::TurnCompleted { usage, .. } => {
                println!("turn completed: usage={usage:?}");
                break;
            }
            ThreadEvent::TurnFailed { error } => {
                println!("turn failed: {}", error.message);
                break;
            }
            ThreadEvent::Error { message } => {
                println!("stream error: {message}");
            }
        }
    }

    Ok(())
}
