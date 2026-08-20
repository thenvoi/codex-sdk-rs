use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::{Parser, ValueEnum};
use codex_app_server_sdk::{
    AgentMessageItem, ApprovalMode, CodexClient, CommandExecutionStatus, ModelReasoningEffort,
    ModelReasoningSummary, ModelVerbosity, PatchApplyStatus, PatchChangeKind, Personality,
    ReasoningItem, SandboxMode, ThreadEvent, ThreadItem, ThreadOptions, TurnOptions, Usage,
    WebSearchMode, WsConfig,
};
use owo_colors::{OwoColorize, Stream::Stdout};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_REASONING_EFFORT: ModelReasoningEffort = ModelReasoningEffort::Low;
const DEV_INSTRUCTIONS_PREVIEW_CHARS: usize = 220;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum ConnectionMode {
    Remote,
    Local,
}

#[derive(Parser)]
#[command(name = "agent", about = "Agent CLI using codex-app-server-sdk")]
struct Cli {
    /// The prompt to send to the model
    prompt: String,

    /// The websocket URL to connect to
    #[arg(long)]
    ws_url: Option<String>,

    /// Working directory for the thread (defaults to the invocation directory)
    #[arg(long)]
    cwd: Option<PathBuf>,

    #[arg(long)]
    agent: Option<String>,

    /// Restrict agent lookup to project-local or user-global config
    #[arg(long, value_enum, requires = "agent")]
    scope: Option<AgentScope>,

    /// Show token usage and thread ID in output
    #[arg(long)]
    verbose: bool,

    /// Print only the final agent response text
    #[arg(long)]
    last_response_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum AgentScope {
    Project,
    User,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum AgentApprovalPolicy {
    Mode(ApprovalMode),
    Raw(Value),
}

impl AgentApprovalPolicy {
    fn mode(&self) -> Option<ApprovalMode> {
        match self {
            Self::Mode(mode) => Some(*mode),
            Self::Raw(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AgentConfig {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
    #[serde(default)]
    developer_instructions: Option<String>,
    #[serde(default)]
    model_instructions_file: Option<String>,
    #[serde(default)]
    model_reasoning_effort: Option<ModelReasoningEffort>,
    #[serde(default)]
    model_reasoning_summary: Option<ModelReasoningSummary>,
    #[serde(default)]
    model_verbosity: Option<ModelVerbosity>,
    #[serde(default)]
    approval_policy: Option<AgentApprovalPolicy>,
    #[serde(default)]
    sandbox_mode: Option<SandboxMode>,
    #[serde(default)]
    sandbox_policy: Option<Value>,
    #[serde(default)]
    personality: Option<Personality>,
    #[serde(default)]
    base_instructions: Option<String>,
    #[serde(default)]
    web_search: Option<WebSearchMode>,
    #[serde(flatten)]
    config: toml::Table,
}

#[derive(Debug, Clone)]
struct LoadedAgent {
    name: String,
    config_path: PathBuf,
    model: Option<String>,
    model_provider: Option<String>,
    model_reasoning_effort: Option<ModelReasoningEffort>,
    model_reasoning_summary: Option<ModelReasoningSummary>,
    approval_policy: Option<ApprovalMode>,
    sandbox_mode: Option<SandboxMode>,
    sandbox_policy: Option<Value>,
    personality: Option<Personality>,
    base_instructions: Option<String>,
    web_search: Option<WebSearchMode>,
    developer_instructions: String,
    config: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct TurnStartInfo {
    agent_name: String,
    config_path: PathBuf,
    model: Option<String>,
    developer_instructions_preview: String,
}

impl From<&LoadedAgent> for TurnStartInfo {
    fn from(agent: &LoadedAgent) -> Self {
        Self {
            agent_name: agent.name.clone(),
            config_path: agent.config_path.clone(),
            model: agent.model.clone(),
            developer_instructions_preview: clipped_text(
                &agent.developer_instructions,
                DEV_INSTRUCTIONS_PREVIEW_CHARS,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedAgentConfig {
    scope: AgentScope,
    path: PathBuf,
}

fn load_agent(
    agent_name: &str,
    scope: Option<AgentScope>,
    cwd: &Path,
) -> anyhow::Result<LoadedAgent> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let resolved = resolve_agent_config_path(agent_name, scope, cwd, home.as_deref())?;
    let path = resolved.path;
    let file = fs::read_to_string(&path)
        .with_context(|| format!("failed to read agent config {}", path.display()))?;
    load_agent_from_str(agent_name, &path, &file)
}

fn load_agent_from_str(agent_name: &str, path: &Path, file: &str) -> anyhow::Result<LoadedAgent> {
    let config: AgentConfig = toml::from_str(&file)
        .with_context(|| format!("failed to parse agent config {}", path.display()))?;
    let developer_instructions = resolve_developer_instructions(agent_name, path, &config)?;
    let mut config_map = config_map_from_agent(&config)
        .with_context(|| format!("failed to map agent config {}", path.display()))?;

    Ok(LoadedAgent {
        name: non_empty(config.name).unwrap_or_else(|| agent_name.to_string()),
        config_path: path.to_path_buf(),
        model: non_empty(config.model),
        model_provider: non_empty(config.model_provider),
        model_reasoning_effort: config.model_reasoning_effort,
        model_reasoning_summary: config.model_reasoning_summary,
        approval_policy: config
            .approval_policy
            .as_ref()
            .and_then(AgentApprovalPolicy::mode),
        sandbox_mode: config.sandbox_mode,
        sandbox_policy: config.sandbox_policy,
        personality: config.personality,
        base_instructions: non_empty(config.base_instructions),
        web_search: config.web_search,
        developer_instructions,
        config: std::mem::take(&mut config_map),
    })
}

fn resolve_developer_instructions(
    agent_name: &str,
    path: &Path,
    config: &AgentConfig,
) -> anyhow::Result<String> {
    if let Some(instructions) = config
        .developer_instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(instructions.to_string());
    }

    if let Some(model_instructions_file) = config
        .model_instructions_file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let instructions_path = resolve_path_from_file(path, model_instructions_file);
        let instructions = fs::read_to_string(&instructions_path).with_context(|| {
            format!(
                "failed to read model_instructions_file for '{}' at {}",
                agent_name,
                instructions_path.display()
            )
        })?;
        let trimmed = instructions.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    if let Some(description) = config
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(description.to_string());
    }

    anyhow::bail!(
        "agent '{}' config {} must set `developer_instructions`, `model_instructions_file`, or `description`",
        agent_name,
        path.display()
    )
}

fn resolve_path_from_file(file_path: &Path, raw_path: &str) -> PathBuf {
    let candidate = PathBuf::from(raw_path);
    if candidate.is_absolute() {
        candidate
    } else {
        file_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(candidate)
    }
}

fn config_map_from_agent(config: &AgentConfig) -> anyhow::Result<Map<String, Value>> {
    let mut map = Map::new();

    if let Some(verbosity) = config.model_verbosity {
        map.insert(
            "model_verbosity".to_string(),
            Value::String(verbosity.as_str().to_string()),
        );
    }
    if let Some(web_search) = config.web_search {
        map.insert(
            "web_search".to_string(),
            Value::String(web_search.as_str().to_string()),
        );
    }
    if let Some(AgentApprovalPolicy::Raw(value)) = &config.approval_policy {
        map.insert("approval_policy".to_string(), value.clone());
    }

    for (key, value) in &config.config {
        insert_config_value(&mut map, key, toml_value_to_json(value.clone())?);
    }

    Ok(map)
}

fn insert_config_value(map: &mut Map<String, Value>, key: &str, value: Value) {
    match value {
        Value::Object(object) => {
            for (child_key, child_value) in object {
                insert_config_value(map, &format!("{key}.{child_key}"), child_value);
            }
        }
        value => {
            map.insert(key.to_string(), value);
        }
    }
}

fn toml_value_to_json(value: toml::Value) -> anyhow::Result<Value> {
    serde_json::to_value(value).context("failed to convert TOML value to JSON")
}

fn resolve_agent_config_path(
    agent_name: &str,
    scope: Option<AgentScope>,
    cwd: &Path,
    home: Option<&Path>,
) -> anyhow::Result<ResolvedAgentConfig> {
    match scope {
        Some(AgentScope::Project) => resolve_project_agent_config_path(agent_name, cwd, home),
        Some(AgentScope::User) => resolve_user_agent_config_path(agent_name, home),
        None => resolve_project_agent_config_path(agent_name, cwd, home)
            .or_else(|_| resolve_user_agent_config_path(agent_name, home))
            .with_context(|| missing_agent_message(agent_name, cwd, home)),
    }
}

fn resolve_project_agent_config_path(
    agent_name: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> anyhow::Result<ResolvedAgentConfig> {
    let candidates = project_agent_config_candidates(agent_name, cwd, home);
    for path in &candidates {
        if path.is_file() {
            return Ok(ResolvedAgentConfig {
                scope: AgentScope::Project,
                path: path.clone(),
            });
        }
    }

    anyhow::bail!(
        "agent '{}' not found in project scope; searched project paths: {}",
        agent_name,
        format_paths(&candidates)
    )
}

fn resolve_user_agent_config_path(
    agent_name: &str,
    home: Option<&Path>,
) -> anyhow::Result<ResolvedAgentConfig> {
    let path = user_agent_config_path(agent_name, home)?;
    if path.is_file() {
        return Ok(ResolvedAgentConfig {
            scope: AgentScope::User,
            path,
        });
    }

    anyhow::bail!(
        "agent '{}' not found in user scope at {}",
        agent_name,
        path.display()
    )
}

fn missing_agent_message(agent_name: &str, cwd: &Path, home: Option<&Path>) -> String {
    let project_paths = project_agent_config_candidates(agent_name, cwd, home);
    match user_agent_config_path(agent_name, home) {
        Ok(user_path) => format!(
            "agent '{}' not found; searched project scope paths: {}; searched user scope path: {}",
            agent_name,
            format_paths(&project_paths),
            user_path.display()
        ),
        Err(error) => format!(
            "agent '{}' not found; searched project scope paths: {}; user scope unavailable: {error:#}",
            agent_name,
            format_paths(&project_paths)
        ),
    }
}

fn project_agent_config_candidates(
    agent_name: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let agent_file = format!("{agent_name}.toml");
    let mut candidates = Vec::new();
    let mut current = cwd;

    loop {
        if home.is_some_and(|home| current == home) {
            break;
        }

        candidates.push(
            current
                .join(".codex")
                .join("agents")
                .join(agent_file.as_str()),
        );

        if current.join(".git").exists() {
            break;
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    candidates
}

fn user_agent_config_path(agent_name: &str, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let home = home.context("HOME is not set; cannot locate user agent config")?;
    Ok(home
        .join(".codex")
        .join("agents")
        .join(format!("{agent_name}.toml")))
}

fn format_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "(none)".to_string();
    }

    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn clipped_text(text: &str, max_chars: usize) -> String {
    if text.trim().is_empty() {
        return "(empty)".to_string();
    }
    if max_chars == 0 {
        return "...".to_string();
    }

    let mut preview = String::with_capacity(max_chars.saturating_add(3).min(text.len()));
    let mut chars = 0usize;

    for word in text.split_whitespace() {
        let separator = usize::from(!preview.is_empty());
        let remaining = max_chars.saturating_sub(chars);

        if remaining <= separator {
            preview.push_str("...");
            return preview;
        }

        if separator == 1 {
            preview.push(' ');
            chars += 1;
        }

        for ch in word.chars() {
            if chars == max_chars {
                preview.push_str("...");
                return preview;
            }
            preview.push(ch);
            chars += 1;
        }
    }

    preview
}

fn turn_start_message(info: &TurnStartInfo) -> String {
    match &info.model {
        Some(model) => format!(
            "Agent `{}` - {} | model: {} | config: {}",
            info.agent_name,
            info.developer_instructions_preview,
            model,
            info.config_path.display()
        ),
        None => format!(
            "Agent `{}` - {} | config: {}",
            info.agent_name,
            info.developer_instructions_preview,
            info.config_path.display()
        ),
    }
}

fn print_turn_start_info(info: &TurnStartInfo) {
    println!(
        "{} {}",
        "info:".if_supports_color(Stdout, |t| t.dimmed().to_string()),
        turn_start_message(info).if_supports_color(Stdout, |t| t.dimmed().to_string())
    );
}

fn print_usage(usage: &Usage) {
    let input_tokens = usage.input_tokens;
    let cached_input_tokens = usage.cached_input_tokens;
    let output_tokens = usage.output_tokens;

    println!(
        "\n\n{}",
        format!("---\nUsage:\n- Input tokens: {input_tokens}\n- Cached input tokens: {cached_input_tokens}\nOutput tokens: {output_tokens}")
            .if_supports_color(Stdout, |t| t.dimmed().to_string())
    );
}

fn print_reasoning_text(text: &str) {
    if text.is_empty() {
        println!(
            "{}\n",
            "Thinking...".if_supports_color(Stdout, |t| t.dimmed().italic().to_string())
        );
    } else {
        println!(
            "{}\n\t{}\n",
            "Thinking...".if_supports_color(Stdout, |t| t.dimmed().italic().to_string()),
            text.if_supports_color(Stdout, |t| t.dimmed().to_string())
        );
    }
}

fn print_agent_message_text(text: &str) -> anyhow::Result<()> {
    print_chunk(
        &text
            .if_supports_color(Stdout, |t| t.bold().bright_white().to_string())
            .to_string(),
    )
}

fn print_chunk(chunk: &str) -> anyhow::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "{chunk}")?;
    stdout.flush()?;
    Ok(())
}

fn ensure_stdout_newline(printed_any: bool, ended_with_newline: &mut bool) -> anyhow::Result<()> {
    if printed_any && !*ended_with_newline {
        let mut stdout = io::stdout();
        writeln!(stdout)?;
        stdout.flush()?;
        *ended_with_newline = true;
    }
    Ok(())
}

#[derive(Debug, Default)]
struct StreamRenderState {
    streamed_agent_message_ids: HashSet<String>,
    streamed_agent_message_without_id: bool,
    agent_message_ended_with_newline: bool,
}

impl StreamRenderState {
    fn note_agent_message_delta(&mut self, message: &AgentMessageItem) {
        if message.id.is_empty() {
            self.streamed_agent_message_without_id = true;
        } else {
            self.streamed_agent_message_ids.insert(message.id.clone());
        }
        self.agent_message_ended_with_newline = message.text.ends_with('\n');
    }

    fn take_agent_message_was_streamed(&mut self, message: &AgentMessageItem) -> bool {
        if message.id.is_empty() {
            let was_streamed = self.streamed_agent_message_without_id;
            self.streamed_agent_message_without_id = false;
            was_streamed
        } else {
            self.streamed_agent_message_ids.remove(&message.id)
        }
    }
}

fn append_reasoning_delta(
    reasoning_text_by_id: &mut HashMap<String, String>,
    reasoning: ReasoningItem,
) {
    if !reasoning.text.is_empty() {
        reasoning_text_by_id
            .entry(reasoning.id)
            .or_default()
            .push_str(&reasoning.text);
    }
}

fn completed_reasoning_text<'a>(
    reasoning: &'a ReasoningItem,
    reasoning_text_by_id: &'a HashMap<String, String>,
) -> &'a str {
    if reasoning.text.is_empty() {
        reasoning_text_by_id
            .get(&reasoning.id)
            .map(String::as_str)
            .unwrap_or_default()
    } else {
        reasoning.text.as_str()
    }
}

fn build_thread_config(
    active_agent: Option<LoadedAgent>,
    working_directory: &Path,
) -> ThreadOptions {
    let mut config_map = Map::new();
    config_map.insert("service_tier".to_string(), "fast".into());
    let mut builder = ThreadOptions::builder()
        .model(DEFAULT_MODEL)
        .model_reasoning_effort(DEFAULT_REASONING_EFFORT)
        .ephemeral(true)
        .working_directory(working_directory.display().to_string())
        .skip_git_repo_check(true);

    if let Some(agent) = active_agent {
        if let Some(model) = agent.model {
            builder = builder.model(model);
        }
        if let Some(model_provider) = agent.model_provider {
            builder = builder.model_provider(model_provider);
        }
        if let Some(model_reasoning_effort) = agent.model_reasoning_effort {
            builder = builder.model_reasoning_effort(model_reasoning_effort);
        }
        if let Some(model_reasoning_summary) = agent.model_reasoning_summary {
            builder = builder.model_reasoning_summary(model_reasoning_summary);
        }
        if let Some(approval_policy) = agent.approval_policy {
            builder = builder.approval_policy(approval_policy);
        }
        if let Some(sandbox_mode) = agent.sandbox_mode {
            builder = builder.sandbox_mode(sandbox_mode);
        }
        if let Some(sandbox_policy) = agent.sandbox_policy {
            builder = builder.sandbox_policy(sandbox_policy);
        }
        if let Some(personality) = agent.personality {
            builder = builder.personality(personality);
        }
        if let Some(base_instructions) = agent.base_instructions {
            builder = builder.base_instructions(base_instructions);
        }
        if let Some(web_search) = agent.web_search {
            builder = builder.web_search_mode(web_search);
        }
        for (key, value) in agent.config {
            config_map.insert(key, value);
        }
        if !config_map.is_empty() {
            builder = builder.config(config_map);
        }
        builder = builder.developer_instructions(agent.developer_instructions);
    } else {
        builder = builder.config(config_map);
    }

    builder.build()
}

fn connect_failure_message(
    ws_url: &str,
    connect_error: &impl std::fmt::Display,
    start_error: &impl std::fmt::Display,
) -> String {
    format!(
        "failed to connect to websocket at `{ws_url}` ({connect_error}); \
         starting a local app-server as a fallback also failed: {start_error}"
    )
}

/// Credentials forwarded to any `codex app-server` daemon the SDK spawns on
/// our behalf, so a fresh daemon inherits the caller's auth.
fn daemon_env() -> std::collections::HashMap<String, String> {
    [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CODEX_ID_TOKEN",
        "CODEX_ACCESS_TOKEN",
    ]
    .iter()
    .filter_map(|name| env::var(name).ok().map(|value| (name.to_string(), value)))
    .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let invocation_dir =
        env::current_dir().context("failed to resolve current working directory")?;
    let working_directory = cli.cwd.clone().unwrap_or_else(|| invocation_dir.clone());
    let active_agent = cli
        .agent
        .as_deref()
        .map(|agent_name| load_agent(agent_name, cli.scope, &invocation_dir))
        .transpose()?;
    let turn_start_info = active_agent.as_ref().map(TurnStartInfo::from);
    let ws_config = match cli.ws_url {
        Some(ref ws_url) => WsConfig::default().with_url(ws_url),
        None => WsConfig::default(),
    };
    let ws_url = ws_config.url.clone();

    if !cli.last_response_only
        && let Some(ref info) = turn_start_info
    {
        print_turn_start_info(info);
    }

    let client = match CodexClient::connect_ws(ws_config.clone()).await {
        Ok(client) => client,
        Err(connect_error) => {
            // Fall back to starting a local app-server. The SDK only starts
            // servers for managed (loopback) targets and fails cleanly
            // otherwise, so no loopback detection is needed here.
            CodexClient::start_and_connect_ws(ws_config, daemon_env())
                .await
                .map_err(|start_error| {
                    anyhow::anyhow!(connect_failure_message(
                        &ws_url,
                        &connect_error,
                        &start_error
                    ))
                })?
        }
    };
    let mut thread = client
        .as_api()
        .start_thread(build_thread_config(active_agent, &working_directory));

    if cli.last_response_only {
        let final_response = thread
            .ask(cli.prompt.as_str(), TurnOptions::default())
            .await?;
        let mut ended_with_newline = final_response.ends_with('\n');
        if !final_response.is_empty() {
            print_agent_message_text(&final_response)?;
        }
        ensure_stdout_newline(!final_response.is_empty(), &mut ended_with_newline)?;
        return Ok(());
    }

    let mut streamed = thread
        .run_streamed(cli.prompt.as_str(), TurnOptions::default())
        .await?;
    let mut printed_reasoning_items = HashSet::new();
    let mut reasoning_text_by_id = HashMap::new();
    let mut render_state = StreamRenderState::default();

    while let Some(next) = streamed.next_event().await {
        match next? {
            ThreadEvent::TurnCompleted { usage, .. } => {
                if cli.verbose
                    && let Some(usage) = usage
                {
                    print_usage(&usage);
                }
                break;
            }
            ThreadEvent::TurnFailed { error } => {
                eprintln!(
                    "{}",
                    format!("streamed turn failed: {}", error.message)
                        .if_supports_color(Stdout, |t| t.bold().red().to_string())
                );
                break;
            }
            ThreadEvent::ThreadStarted { thread_id } => {
                if cli.verbose {
                    println!(
                        "{}",
                        format!("Thread ID: {thread_id}\n---")
                            .if_supports_color(Stdout, |t| t.dimmed().to_string())
                    );
                }
            }
            ThreadEvent::Error { message } => {
                eprintln!(
                    "{}",
                    format!("error: {message}")
                        .if_supports_color(Stdout, |t| t.bold().red().to_string())
                );
            }
            ThreadEvent::TurnStarted => {}
            ThreadEvent::ItemStarted { .. } => {}
            ThreadEvent::ItemUpdated {
                item: ThreadItem::AgentMessage(message),
            } => {
                if !message.text.is_empty() {
                    print_agent_message_text(&message.text)?;
                    render_state.note_agent_message_delta(&message);
                }
            }
            ThreadEvent::ItemUpdated {
                item: ThreadItem::Reasoning(reasoning),
            } => {
                let id = reasoning.id.clone();
                let delta_text = reasoning.text.clone();
                let has_text = !delta_text.is_empty();
                append_reasoning_delta(&mut reasoning_text_by_id, reasoning);
                if has_text {
                    print_reasoning_text(&delta_text);
                    printed_reasoning_items.insert(id);
                }
            }
            ThreadEvent::ItemUpdated { .. } => {}
            ThreadEvent::ItemCompleted { item } => match item {
                ThreadItem::AgentMessage(message) => {
                    if render_state.take_agent_message_was_streamed(&message) {
                        ensure_stdout_newline(
                            true,
                            &mut render_state.agent_message_ended_with_newline,
                        )?;
                    } else if !message.text.is_empty() {
                        print_agent_message_text(&message.text)?;
                        render_state.agent_message_ended_with_newline =
                            message.text.ends_with('\n');
                        ensure_stdout_newline(
                            true,
                            &mut render_state.agent_message_ended_with_newline,
                        )?;
                    }
                }
                ThreadItem::Reasoning(reasoning) => {
                    if !printed_reasoning_items.contains(&reasoning.id) {
                        print_reasoning_text(completed_reasoning_text(
                            &reasoning,
                            &reasoning_text_by_id,
                        ));
                        printed_reasoning_items.insert(reasoning.id);
                    }
                }
                ThreadItem::WebSearch(search) => {
                    println!(
                        "{} {}\n",
                        "⊙ Searching:".if_supports_color(Stdout, |t| t.cyan().to_string()),
                        search
                            .query
                            .if_supports_color(Stdout, |t| t.cyan().to_string())
                    );
                }
                ThreadItem::Plan(plan) => {
                    println!(
                        "{} {}\n",
                        "Plan:".if_supports_color(Stdout, |t| t.yellow().bold().to_string()),
                        plan.text
                            .if_supports_color(Stdout, |t| t.yellow().to_string())
                    );
                }
                ThreadItem::CommandExecution(cmd) => {
                    let failed = cmd.status == CommandExecutionStatus::Failed
                        || cmd.exit_code.is_some_and(|c| c != 0);

                    println!(
                        "{} {}",
                        ">".if_supports_color(Stdout, |t| if failed {
                            t.red().to_string()
                        } else {
                            t.green().to_string()
                        }),
                        cmd.command.if_supports_color(Stdout, |t| if failed {
                            t.red().to_string()
                        } else {
                            t.green().to_string()
                        }),
                    );
                    if !cmd.aggregated_output.is_empty() {
                        println!(
                            "{}",
                            cmd.aggregated_output
                                .if_supports_color(Stdout, |t| t.dimmed().to_string())
                        );
                    }
                    if let Some(code) = cmd.exit_code {
                        if code != 0 {
                            println!(
                                "{}",
                                format!("exit code: {code}")
                                    .if_supports_color(Stdout, |t| t.red().to_string())
                            );
                        }
                    }
                }
                ThreadItem::FileChange(fc) => {
                    for change in &fc.changes {
                        match change.kind {
                            PatchChangeKind::Add => println!(
                                "\t{} {}",
                                "+".if_supports_color(Stdout, |t| t.green().to_string()),
                                change
                                    .path
                                    .if_supports_color(Stdout, |t| t.green().to_string()),
                            ),
                            PatchChangeKind::Delete => println!(
                                "\t{} {}",
                                "-".if_supports_color(Stdout, |t| t.red().to_string()),
                                change
                                    .path
                                    .if_supports_color(Stdout, |t| t.red().to_string()),
                            ),
                            PatchChangeKind::Update | PatchChangeKind::Unknown => println!(
                                "\t{} {}",
                                "~".if_supports_color(Stdout, |t| t.magenta().to_string()),
                                change
                                    .path
                                    .if_supports_color(Stdout, |t| t.magenta().to_string()),
                            ),
                        }
                    }
                    if fc.status == PatchApplyStatus::Failed {
                        println!(
                            "\t{}",
                            "patch failed"
                                .if_supports_color(Stdout, |t| t.bold().red().to_string())
                        );
                    }
                }
                ThreadItem::McpToolCall(mcp) => {
                    print!(
                        "{} {}",
                        "MCP:".if_supports_color(Stdout, |t| t.blue().bold().to_string()),
                        format!("{}::{}", mcp.server, mcp.tool)
                            .if_supports_color(Stdout, |t| t.blue().to_string()),
                    );
                    if let Some(ref err) = mcp.error {
                        println!(
                            " {}",
                            format!("error: {}", err.message)
                                .if_supports_color(Stdout, |t| t.red().to_string())
                        );
                    } else {
                        println!();
                    }
                }
                ThreadItem::DynamicToolCall(dtc) => {
                    println!(
                        "{} {} ({})",
                        "Tool:".if_supports_color(Stdout, |t| t.blue().bold().to_string()),
                        dtc.tool.if_supports_color(Stdout, |t| t.blue().to_string()),
                        dtc.status
                            .if_supports_color(Stdout, |t| t.dimmed().to_string()),
                    );
                }
                ThreadItem::CollabToolCall(collab) => {
                    println!(
                        "{} {} ({})",
                        "Collab:".if_supports_color(Stdout, |t| t.blue().bold().to_string()),
                        collab
                            .tool
                            .if_supports_color(Stdout, |t| t.blue().to_string()),
                        collab
                            .status
                            .if_supports_color(Stdout, |t| t.dimmed().to_string()),
                    );
                }
                ThreadItem::TodoList(todo) => {
                    for item in &todo.items {
                        if item.completed {
                            println!(
                                "\t{} {}",
                                "✓".if_supports_color(Stdout, |t| t.green().to_string()),
                                item.text
                                    .if_supports_color(Stdout, |t| t.dimmed().to_string()),
                            );
                        } else {
                            println!(
                                "\t{} {}",
                                "○".if_supports_color(Stdout, |t| t.dimmed().to_string()),
                                item.text,
                            );
                        }
                    }
                }
                ThreadItem::Error(err) => {
                    eprintln!(
                        "{}",
                        format!("error: {}", err.message)
                            .if_supports_color(Stdout, |t| t.bold().red().to_string())
                    );
                }
                ThreadItem::ImageView(img) => {
                    println!(
                        "\t{}",
                        format!("Image: {}", img.path)
                            .if_supports_color(Stdout, |t| t.dimmed().to_string())
                    );
                }
                ThreadItem::EnteredReviewMode(review) => {
                    println!(
                        "{}",
                        format!("Entered review mode: {}", review.review)
                            .if_supports_color(Stdout, |t| t.yellow().italic().to_string())
                    );
                }
                ThreadItem::ExitedReviewMode(_) => {
                    println!(
                        "{}",
                        "Exited review mode"
                            .if_supports_color(Stdout, |t| t.yellow().italic().to_string())
                    );
                }
                ThreadItem::ContextCompaction(_) => {
                    println!(
                        "{}",
                        "Context compacted".if_supports_color(Stdout, |t| t.dimmed().to_string())
                    );
                }
                ThreadItem::UserMessage(_) => {}
                ThreadItem::Unknown(unk) => {
                    if let Some(ref ty) = unk.item_type {
                        println!(
                            "{}",
                            format!("Unknown item: {ty}")
                                .if_supports_color(Stdout, |t| t.dimmed().to_string())
                        );
                    }
                }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use serde_json::json;

    const DOCUMENTED_CUSTOM_AGENT_FIELDS: &[&str] = &[
        "name",
        "description",
        "developer_instructions",
        "model",
        "model_reasoning_effort",
        "sandbox_mode",
        "mcp_servers",
        "skills.config",
    ];

    #[test]
    fn cli_accepts_agent_scope_values() {
        let project = Cli::try_parse_from([
            "agx",
            "review this",
            "--agent",
            "reviewer",
            "--scope",
            "project",
        ])
        .expect("parse project scope");
        let user = Cli::try_parse_from([
            "agx",
            "review this",
            "--agent",
            "reviewer",
            "--scope",
            "user",
        ])
        .expect("parse user scope");

        assert_eq!(project.scope, Some(AgentScope::Project));
        assert_eq!(user.scope, Some(AgentScope::User));
    }

    #[test]
    fn cwd_flag_reaches_thread_options_working_directory() {
        let cli = Cli::try_parse_from(["agx", "review this", "--cwd", "/tmp/project"])
            .expect("parse cwd flag");
        let cwd = cli.cwd.expect("cwd flag parsed");

        let thread_options = build_thread_config(None, &cwd);

        assert_eq!(
            thread_options.working_directory.as_deref(),
            Some("/tmp/project")
        );
    }

    #[test]
    fn cwd_flag_defaults_to_none_so_invocation_directory_is_used() {
        let cli = Cli::try_parse_from(["agx", "review this"]).expect("parse without cwd flag");

        assert_eq!(cli.cwd, None);
    }

    #[test]
    fn connect_failure_message_includes_url_and_both_errors() {
        let message = connect_failure_message(
            "ws://203.0.113.10:4222",
            &"connection refused",
            &"invalid websocket URL",
        );

        assert!(message.contains("ws://203.0.113.10:4222"));
        assert!(message.contains("connection refused"));
        assert!(message.contains("invalid websocket URL"));
    }

    #[test]
    fn unknown_agent_config_keys_are_tolerated() {
        let agent = load_agent_from_str(
            "reviewer",
            Path::new("/tmp/reviewer.toml"),
            r#"
name = "reviewer"
developer_instructions = "Review code"
nickname_candidates = ["Atlas", "Delta"]
some_future_key = "value"
"#,
        )
        .expect("unknown keys must not fail agent loading");

        assert_eq!(
            agent.config.get("nickname_candidates"),
            Some(&json!(["Atlas", "Delta"]))
        );
        assert_eq!(agent.config.get("some_future_key"), Some(&json!("value")));
    }

    #[test]
    fn cli_accepts_last_response_only_flag() {
        let parsed = Cli::try_parse_from(["agx", "review this", "--last-response-only"])
            .expect("parse last response flag");

        assert!(parsed.last_response_only);
        assert_eq!(parsed.prompt, "review this");
    }

    #[test]
    fn documented_custom_agent_fields_are_loaded_and_applied() {
        let agent_toml = r#"
name = "reviewer"
description = "PR reviewer focused on correctness, security, and missing tests."
developer_instructions = "Review code like an owner."
model = "gpt-5.3-codex"
model_provider = "openai"
model_reasoning_effort = "xhigh"
model_reasoning_summary = "detailed"
model_verbosity = "high"
approval_policy = "never"
sandbox_mode = "read-only"
sandbox_policy = { mode = "readOnly" }
personality = "pragmatic"
base_instructions = "Base rules"
web_search = "live"
service_tier = "flex"

[mcp_servers.docs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-docs"]
enabled = true

[[skills.config]]
path = "skills/reviewer"
enabled = true
"#;

        for field in DOCUMENTED_CUSTOM_AGENT_FIELDS {
            let needle = field.rsplit('.').next().expect("field suffix");
            assert!(
                agent_toml.contains(needle),
                "fixture must cover documented custom agent field {field}"
            );
        }

        let path = Path::new("/tmp/reviewer.toml");
        let raw_config: AgentConfig = toml::from_str(agent_toml).expect("parse raw agent config");
        assert_eq!(
            raw_config.description.as_deref(),
            Some("PR reviewer focused on correctness, security, and missing tests.")
        );
        assert_eq!(raw_config.model_verbosity, Some(ModelVerbosity::High));

        let agent = load_agent_from_str("reviewer", path, agent_toml).expect("load agent");
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(agent.model_provider.as_deref(), Some("openai"));
        assert_eq!(
            agent.model_reasoning_effort,
            Some(ModelReasoningEffort::XHigh)
        );
        assert_eq!(
            agent.model_reasoning_summary,
            Some(ModelReasoningSummary::Detailed)
        );
        assert_eq!(agent.approval_policy, Some(ApprovalMode::Never));
        assert_eq!(agent.sandbox_mode, Some(SandboxMode::ReadOnly));
        assert_eq!(agent.sandbox_policy, Some(json!({"mode": "readOnly"})));
        assert_eq!(agent.personality, Some(Personality::Pragmatic));
        assert_eq!(agent.base_instructions.as_deref(), Some("Base rules"));
        assert_eq!(agent.web_search, Some(WebSearchMode::Live));
        assert_eq!(agent.developer_instructions, "Review code like an owner.");
        assert_eq!(
            agent.config.get("mcp_servers.docs.command"),
            Some(&json!("npx"))
        );
        assert_eq!(
            agent.config.get("mcp_servers.docs.args"),
            Some(&json!(["-y", "@modelcontextprotocol/server-docs"]))
        );
        assert_eq!(
            agent.config.get("skills.config"),
            Some(&json!([{"path": "skills/reviewer", "enabled": true}]))
        );

        let thread_options = build_thread_config(Some(agent), Path::new("/tmp/workspace"));
        assert_eq!(
            thread_options.working_directory.as_deref(),
            Some("/tmp/workspace")
        );
        assert_eq!(thread_options.model.as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(thread_options.model_provider.as_deref(), Some("openai"));
        assert_eq!(
            thread_options.model_reasoning_effort,
            Some(ModelReasoningEffort::XHigh)
        );
        assert_eq!(
            thread_options.model_reasoning_summary,
            Some(ModelReasoningSummary::Detailed)
        );
        assert_eq!(thread_options.approval_policy, Some(ApprovalMode::Never));
        assert_eq!(thread_options.sandbox_mode, Some(SandboxMode::ReadOnly));
        assert_eq!(thread_options.personality, Some(Personality::Pragmatic));
        assert_eq!(
            thread_options.developer_instructions.as_deref(),
            Some("Review code like an owner.")
        );
        let config = thread_options.config.expect("thread config");
        assert_eq!(config.get("service_tier"), Some(&json!("flex")));
        assert_eq!(config.get("model_verbosity"), Some(&json!("high")));
        assert_eq!(config.get("web_search"), Some(&json!("live")));
        assert_eq!(config.get("mcp_servers.docs.enabled"), Some(&json!(true)));
        assert_eq!(
            config.get("skills.config"),
            Some(&json!([{"path": "skills/reviewer", "enabled": true}]))
        );
    }

    #[test]
    fn agent_omitted_options_use_cli_defaults() {
        let thread_options = build_thread_config(None, Path::new("/tmp/workspace"));

        assert_eq!(
            thread_options.working_directory.as_deref(),
            Some("/tmp/workspace")
        );
        assert_eq!(thread_options.model.as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(
            thread_options.model_reasoning_effort,
            Some(DEFAULT_REASONING_EFFORT)
        );
        assert_eq!(thread_options.ephemeral, Some(true));
        assert_eq!(thread_options.skip_git_repo_check, Some(true));
        assert_eq!(
            thread_options
                .config
                .as_ref()
                .and_then(|config| config.get("service_tier")),
            Some(&json!("fast"))
        );
    }

    #[test]
    fn approval_policy_object_is_preserved_as_config_override() {
        let agent = load_agent_from_str(
            "reviewer",
            Path::new("/tmp/reviewer.toml"),
            r#"
name = "reviewer"
description = "Review code"
developer_instructions = "Review code"

[approval_policy.reject]
rules = true
sandbox_approval = false
mcp_elicitations = true
"#,
        )
        .expect("load agent");

        assert_eq!(agent.approval_policy, None);
        assert_eq!(
            agent.config.get("approval_policy"),
            Some(&json!({"reject": {
                "rules": true,
                "sandbox_approval": false,
                "mcp_elicitations": true
            }}))
        );
    }

    #[test]
    fn model_instructions_file_is_supported_as_instructions_fallback() {
        let dir = make_temp_dir();
        let agent_path = dir.join("reviewer.toml");
        let instructions_path = dir.join("reviewer.md");
        fs::write(&instructions_path, "\nPrefer small, tested changes.\n")
            .expect("write instructions");

        let agent = load_agent_from_str(
            "reviewer",
            &agent_path,
            r#"
name = "reviewer"
description = "fallback description"
model_instructions_file = "reviewer.md"
"#,
        )
        .expect("load agent");

        assert_eq!(
            agent.developer_instructions,
            "Prefer small, tested changes."
        );

        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn default_lookup_resolves_project_agent_from_current_project() {
        let fixture = AgentLookupFixture::new();
        fixture.write_project_agent("reviewer");
        let resolved =
            resolve_agent_config_path("reviewer", None, &fixture.cwd, Some(fixture.home.as_path()))
                .expect("resolve project agent");

        assert_eq!(resolved.scope, AgentScope::Project);
        assert_eq!(resolved.path, fixture.project_agent_path("reviewer"));
        fixture.cleanup();
    }

    #[test]
    fn default_lookup_falls_back_to_user_agent() {
        let fixture = AgentLookupFixture::new();
        fixture.write_user_agent("reviewer");
        let resolved =
            resolve_agent_config_path("reviewer", None, &fixture.cwd, Some(fixture.home.as_path()))
                .expect("resolve user agent");

        assert_eq!(resolved.scope, AgentScope::User);
        assert_eq!(resolved.path, fixture.user_agent_path("reviewer"));
        fixture.cleanup();
    }

    #[test]
    fn project_scope_fails_when_only_user_agent_exists() {
        let fixture = AgentLookupFixture::new();
        fixture.write_user_agent("reviewer");
        let error = resolve_agent_config_path(
            "reviewer",
            Some(AgentScope::Project),
            &fixture.cwd,
            Some(fixture.home.as_path()),
        )
        .expect_err("project scope should ignore user agent")
        .to_string();

        assert!(error.contains("project scope"));
        assert!(error.contains(&fixture.project_agent_path("reviewer").display().to_string()));
        assert!(!error.contains(&fixture.user_agent_path("reviewer").display().to_string()));
        fixture.cleanup();
    }

    #[test]
    fn user_scope_ignores_project_agent() {
        let fixture = AgentLookupFixture::new();
        fixture.write_project_agent("reviewer");
        fixture.write_user_agent("reviewer");
        let resolved = resolve_agent_config_path(
            "reviewer",
            Some(AgentScope::User),
            &fixture.cwd,
            Some(fixture.home.as_path()),
        )
        .expect("resolve user agent");

        assert_eq!(resolved.scope, AgentScope::User);
        assert_eq!(resolved.path, fixture.user_agent_path("reviewer"));
        fixture.cleanup();
    }

    #[test]
    fn default_lookup_prefers_project_agent_when_both_exist() {
        let fixture = AgentLookupFixture::new();
        fixture.write_project_agent("reviewer");
        fixture.write_user_agent("reviewer");
        let resolved =
            resolve_agent_config_path("reviewer", None, &fixture.cwd, Some(fixture.home.as_path()))
                .expect("resolve project agent");

        assert_eq!(resolved.scope, AgentScope::Project);
        assert_eq!(resolved.path, fixture.project_agent_path("reviewer"));
        fixture.cleanup();
    }

    #[test]
    fn missing_agent_error_identifies_project_and_user_paths() {
        let fixture = AgentLookupFixture::new();
        let error =
            resolve_agent_config_path("reviewer", None, &fixture.cwd, Some(fixture.home.as_path()))
                .expect_err("missing agent should fail");
        let message = format!("{error:#}");

        assert!(message.contains("project scope paths"));
        assert!(message.contains(&fixture.project_agent_path("reviewer").display().to_string()));
        assert!(message.contains("user scope path"));
        assert!(message.contains(&fixture.user_agent_path("reviewer").display().to_string()));
        fixture.cleanup();
    }

    #[test]
    fn verbose_turn_start_message_includes_resolved_agent_config_path() {
        let path = Path::new("/tmp/reviewer.toml");
        let agent = load_agent_from_str(
            "reviewer",
            path,
            r#"
name = "reviewer"
developer_instructions = "Review code"
model = "gpt-5.5"
"#,
        )
        .expect("load agent");

        let message = turn_start_message(&TurnStartInfo::from(&agent));

        assert!(message.starts_with("Agent `reviewer` - Review code"));
        assert!(message.contains("config: /tmp/reviewer.toml"));
    }

    #[test]
    fn clipped_text_normalizes_whitespace_without_clipping() {
        assert_eq!(
            clipped_text("  keep\nthese\tinstructions readable  ", 64),
            "keep these instructions readable"
        );
    }

    #[test]
    fn clipped_text_truncates_on_char_boundary() {
        assert_eq!(clipped_text("abcdef ghij", 8), "abcdef g...");
    }

    #[test]
    fn completed_reasoning_text_uses_accumulated_delta_when_final_item_is_empty() {
        let mut reasoning_text_by_id = HashMap::new();
        append_reasoning_delta(
            &mut reasoning_text_by_id,
            ReasoningItem {
                id: "reason_1".to_string(),
                text: "checking ".to_string(),
            },
        );
        append_reasoning_delta(
            &mut reasoning_text_by_id,
            ReasoningItem {
                id: "reason_1".to_string(),
                text: "docs".to_string(),
            },
        );

        let completed = ReasoningItem {
            id: "reason_1".to_string(),
            text: String::new(),
        };

        assert_eq!(
            completed_reasoning_text(&completed, &reasoning_text_by_id),
            "checking docs"
        );
    }

    #[test]
    fn completed_reasoning_text_prefers_authoritative_final_item_text() {
        let mut reasoning_text_by_id = HashMap::new();
        append_reasoning_delta(
            &mut reasoning_text_by_id,
            ReasoningItem {
                id: "reason_1".to_string(),
                text: "partial".to_string(),
            },
        );

        let completed = ReasoningItem {
            id: "reason_1".to_string(),
            text: "complete summary".to_string(),
        };

        assert_eq!(
            completed_reasoning_text(&completed, &reasoning_text_by_id),
            "complete summary"
        );
    }

    #[test]
    fn streamed_agent_message_tracking_prevents_completion_duplicate() {
        let mut state = StreamRenderState::default();
        let delta = AgentMessageItem {
            id: "msg_1".to_string(),
            text: "hel".to_string(),
            phase: None,
        };
        let completed = AgentMessageItem {
            id: "msg_1".to_string(),
            text: "hello".to_string(),
            phase: None,
        };

        state.note_agent_message_delta(&delta);

        assert!(state.take_agent_message_was_streamed(&completed));
        assert!(!state.take_agent_message_was_streamed(&completed));
    }

    fn make_temp_dir() -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = env::temp_dir().join(format!("agx-test-{}-{now}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    struct AgentLookupFixture {
        root: PathBuf,
        home: PathBuf,
        repo: PathBuf,
        cwd: PathBuf,
    }

    impl AgentLookupFixture {
        fn new() -> Self {
            let root = make_temp_dir();
            let home = root.join("home");
            let repo = root.join("repo");
            let cwd = repo.join("crates").join("agx");

            fs::create_dir_all(&home).expect("create home");
            fs::create_dir_all(&cwd).expect("create cwd");
            fs::create_dir_all(repo.join(".git")).expect("create git dir");

            Self {
                root,
                home,
                repo,
                cwd,
            }
        }

        fn project_agent_path(&self, agent_name: &str) -> PathBuf {
            self.repo
                .join(".codex")
                .join("agents")
                .join(format!("{agent_name}.toml"))
        }

        fn user_agent_path(&self, agent_name: &str) -> PathBuf {
            self.home
                .join(".codex")
                .join("agents")
                .join(format!("{agent_name}.toml"))
        }

        fn write_project_agent(&self, agent_name: &str) {
            write_agent_config(&self.project_agent_path(agent_name), "project instructions");
        }

        fn write_user_agent(&self, agent_name: &str) {
            write_agent_config(&self.user_agent_path(agent_name), "user instructions");
        }

        fn cleanup(self) {
            fs::remove_dir_all(self.root).expect("cleanup");
        }
    }

    fn write_agent_config(path: &Path, developer_instructions: &str) {
        fs::create_dir_all(path.parent().expect("agent path parent")).expect("create agent dir");
        fs::write(
            path,
            format!(
                r#"
name = "reviewer"
developer_instructions = "{developer_instructions}"
"#
            ),
        )
        .expect("write agent config");
    }
}
