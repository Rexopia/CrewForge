use crate::auth::AuthService;
use crate::auth::openai_oauth::extract_account_id_from_tokens;
use crate::provider::ProviderRuntimeOptions;
use crate::provider::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, ToolCall, ToolSpec,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

const DEFAULT_CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_RESPONSES_URL_ENV: &str = "CREWFORGE_CODEX_RESPONSES_URL";
const CODEX_BASE_URL_ENV: &str = "CREWFORGE_CODEX_BASE_URL";
const DEFAULT_CODEX_INSTRUCTIONS: &str =
    "You are CrewForge, a concise and helpful coding assistant.";

pub struct OpenAiCodexProvider {
    auth: AuthService,
    auth_profile_override: Option<String>,
    responses_url: String,
    client: Client,
}

// ── Request types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    /// Polymorphic input: role-based messages, function_call echoes, function_call_output items.
    input: Vec<Value>,
    instructions: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    store: bool,
    stream: bool,
    text: ResponsesTextOptions,
    reasoning: ResponsesReasoningOptions,
    include: Vec<String>,
    tool_choice: String,
    parallel_tool_calls: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesTextOptions {
    verbosity: String,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoningOptions {
    effort: String,
    summary: String,
}

// ── Response types ──────────────────────────────────────────────────────────

// ── Constructor ─────────────────────────────────────────────────────────────

impl OpenAiCodexProvider {
    pub fn new(options: &ProviderRuntimeOptions) -> anyhow::Result<Self> {
        let state_dir = options
            .crewforge_dir
            .clone()
            .unwrap_or_else(default_crewforge_dir);
        let auth = AuthService::new(&state_dir, options.secrets_encrypt);
        let responses_url = resolve_responses_url(options)?;

        Ok(Self {
            auth,
            auth_profile_override: options.auth_profile_override.clone(),
            responses_url,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        })
    }
}

// ── URL resolution ──────────────────────────────────────────────────────────

fn default_crewforge_dir() -> PathBuf {
    directories::UserDirs::new().map_or_else(
        || PathBuf::from(".crewforge"),
        |dirs| dirs.home_dir().join(".crewforge"),
    )
}

fn build_responses_url(base_or_endpoint: &str) -> anyhow::Result<String> {
    let candidate = base_or_endpoint.trim();
    if candidate.is_empty() {
        anyhow::bail!("OpenAI Codex endpoint override cannot be empty");
    }

    let mut parsed = reqwest::Url::parse(candidate)
        .map_err(|_| anyhow::anyhow!("OpenAI Codex endpoint override must be a valid URL"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        _ => anyhow::bail!("OpenAI Codex endpoint override must use http:// or https://"),
    }

    let path = parsed.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        let with_suffix = if path.is_empty() || path == "/" {
            "/responses".to_string()
        } else {
            format!("{path}/responses")
        };
        parsed.set_path(&with_suffix);
    }

    parsed.set_query(None);
    parsed.set_fragment(None);

    Ok(parsed.to_string())
}

fn resolve_responses_url(options: &ProviderRuntimeOptions) -> anyhow::Result<String> {
    if let Some(endpoint) = std::env::var(CODEX_RESPONSES_URL_ENV)
        .ok()
        .and_then(|value| first_nonempty(Some(&value)))
    {
        return build_responses_url(&endpoint);
    }

    if let Some(base_url) = std::env::var(CODEX_BASE_URL_ENV)
        .ok()
        .and_then(|value| first_nonempty(Some(&value)))
    {
        return build_responses_url(&base_url);
    }

    if let Some(api_url) = options
        .provider_api_url
        .as_deref()
        .and_then(|value| first_nonempty(Some(value)))
    {
        return build_responses_url(&api_url);
    }

    Ok(DEFAULT_CODEX_RESPONSES_URL.to_string())
}

fn first_nonempty(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

// ── Model / reasoning helpers ───────────────────────────────────────────────

#[allow(dead_code)]
fn resolve_instructions(system_prompt: Option<&str>) -> String {
    first_nonempty(system_prompt).unwrap_or_else(|| DEFAULT_CODEX_INSTRUCTIONS.to_string())
}

fn normalize_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn clamp_reasoning_effort(model: &str, effort: &str) -> String {
    let id = normalize_model_id(model);
    if id == "gpt-5-codex" {
        return match effort {
            "low" | "medium" | "high" => effort.to_string(),
            "minimal" => "low".to_string(),
            "xhigh" => "high".to_string(),
            _ => "high".to_string(),
        };
    }
    if (id.starts_with("gpt-5.2") || id.starts_with("gpt-5.3")) && effort == "minimal" {
        return "low".to_string();
    }
    if id.starts_with("gpt-5-codex") && effort == "xhigh" {
        return "high".to_string();
    }
    if id == "gpt-5.1" && effort == "xhigh" {
        return "high".to_string();
    }
    if id == "gpt-5.1-codex-mini" {
        return if effort == "high" || effort == "xhigh" {
            "high".to_string()
        } else {
            "medium".to_string()
        };
    }
    effort.to_string()
}

fn resolve_reasoning_effort(model_id: &str) -> String {
    let raw = std::env::var("CREWFORGE_CODEX_REASONING_EFFORT")
        .ok()
        .and_then(|value| first_nonempty(Some(&value)))
        .unwrap_or_else(|| "xhigh".to_string())
        .to_ascii_lowercase();
    clamp_reasoning_effort(model_id, &raw)
}

// ── Input building ──────────────────────────────────────────────────────────

/// Convert ChatMessage history to Responses API input items.
///
/// Returns (instructions, input_items) where:
/// - `instructions` is the joined system prompt content
/// - `input_items` are polymorphic JSON values (user/assistant messages, tool results)
fn build_responses_input(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => system_parts.push(&msg.content),
            "user" => {
                input.push(serde_json::json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": msg.content}]
                }));
            }
            "assistant" => {
                // Check if this is a structured tool-call message (JSON-encoded)
                if let Ok(parsed) = serde_json::from_str::<Value>(&msg.content) {
                    // If it has tool_calls, emit both text and function_call items
                    if let Some(tool_calls) = parsed.get("tool_calls").and_then(Value::as_array) {
                        // Emit assistant text if present
                        if let Some(text) = parsed.get("content").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}]
                            }));
                        }
                        // Emit function_call items (echoed back for context)
                        for tc in tool_calls {
                            let call_id = tc
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let name =
                                tc.get("name").and_then(Value::as_str).unwrap_or("unknown");
                            let arguments = tc
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}");
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": arguments,
                                "call_id": call_id,
                                "id": format!("fc_{call_id}")
                            }));
                        }
                    } else {
                        // Plain assistant message
                        let text = parsed
                            .get("content")
                            .and_then(Value::as_str)
                            .map(|s| s.to_string())
                            .unwrap_or(msg.content.clone());
                        input.push(serde_json::json!({
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}]
                        }));
                    }
                } else {
                    input.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": msg.content}]
                    }));
                }
            }
            "tool" => {
                // Each tool result is a separate ChatMessage with JSON content:
                // {"tool_call_id": "...", "content": "..."}
                if let Ok(result) = serde_json::from_str::<Value>(&msg.content) {
                    let call_id = result
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let content = result
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": content
                    }));
                }
            }
            _ => {}
        }
    }

    let instructions = if system_parts.is_empty() {
        DEFAULT_CODEX_INSTRUCTIONS.to_string()
    } else {
        system_parts.join("\n\n")
    };

    (instructions, input)
}

/// Convert ToolSpec list to Responses API tool definitions.
fn build_tools_json(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "strict": false
            })
        })
        .collect()
}

// ── Response parsing ────────────────────────────────────────────────────────

fn nonempty_preserve(text: Option<&str>) -> Option<String> {
    text.and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// Extract function_call tool calls from a parsed Responses API response.
fn extract_tool_calls(output: &[Value]) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    for item in output {
        let item_type = item.get("type").and_then(Value::as_str);
        if item_type == Some("function_call") {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }
    calls
}

// ── SSE streaming parser ────────────────────────────────────────────────────

fn extract_stream_event(event: &Value, saw_delta: bool) -> StreamChunk {
    let event_type = event.get("type").and_then(Value::as_str);
    match event_type {
        Some("response.output_text.delta") => StreamChunk::TextDelta(
            nonempty_preserve(event.get("delta").and_then(Value::as_str)).unwrap_or_default(),
        ),
        Some("response.output_text.done") if !saw_delta => StreamChunk::TextDone(
            nonempty_preserve(event.get("text").and_then(Value::as_str)),
        ),
        Some("response.function_call_arguments.done") => {
            let call_id = event
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let name = event
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let arguments = event
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            StreamChunk::FunctionCall { call_id, name, arguments }
        }
        Some("response.completed" | "response.done") => {
            if let Some(response) = event.get("response") {
                let output = response
                    .get("output")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let output_text = response
                    .get("output_text")
                    .and_then(Value::as_str)
                    .and_then(|t| if t.is_empty() { None } else { Some(t.to_string()) });
                StreamChunk::Completed { output, output_text }
            } else {
                StreamChunk::None
            }
        }
        _ => StreamChunk::None,
    }
}

enum StreamChunk {
    TextDelta(String),
    TextDone(Option<String>),
    FunctionCall { call_id: String, name: String, arguments: String },
    Completed { output: Vec<Value>, output_text: Option<String> },
    None,
}

fn extract_stream_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);

    if event_type == Some("error") {
        return first_nonempty(
            event
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| event.get("code").and_then(Value::as_str))
                .or_else(|| {
                    event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                }),
        );
    }

    if event_type == Some("response.failed") {
        return first_nonempty(
            event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
        );
    }

    None
}

/// Parse SSE stream body into a ChatResponse.
fn parse_sse_to_chat_response(body: &str) -> anyhow::Result<ChatResponse> {
    let mut saw_delta = false;
    let mut delta_accumulator = String::new();
    let mut fallback_text: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut completed_response: Option<(Vec<Value>, Option<String>)> = None;

    let process_chunk = |chunk: &str,
                         saw_delta: &mut bool,
                         delta_accumulator: &mut String,
                         fallback_text: &mut Option<String>,
                         tool_calls: &mut Vec<ToolCall>,
                         completed_response: &mut Option<(Vec<Value>, Option<String>)>|
     -> anyhow::Result<()> {
        let data_lines: Vec<String> = chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.trim().to_string())
            .collect();
        if data_lines.is_empty() {
            return Ok(());
        }

        for line in &data_lines {
            let line = line.trim();
            if line.is_empty() || line == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            if let Some(message) = extract_stream_error_message(&event) {
                return Err(anyhow::anyhow!("OpenAI Codex stream error: {message}"));
            }

            match extract_stream_event(&event, *saw_delta) {
                StreamChunk::TextDelta(text) => {
                    *saw_delta = true;
                    delta_accumulator.push_str(&text);
                }
                StreamChunk::TextDone(text) => {
                    if fallback_text.is_none() {
                        *fallback_text = text;
                    }
                }
                StreamChunk::FunctionCall { call_id, name, arguments } => {
                    tool_calls.push(ToolCall {
                        id: call_id,
                        name,
                        arguments,
                    });
                }
                StreamChunk::Completed { output, output_text } => {
                    *completed_response = Some((output, output_text));
                }
                StreamChunk::None => {}
            }
        }

        Ok(())
    };

    let mut buffer = body.to_string();
    loop {
        let Some(idx) = buffer.find("\n\n") else {
            break;
        };
        let chunk = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        process_chunk(
            &chunk,
            &mut saw_delta,
            &mut delta_accumulator,
            &mut fallback_text,
            &mut tool_calls,
            &mut completed_response,
        )?;
    }
    if !buffer.trim().is_empty() {
        process_chunk(
            &buffer,
            &mut saw_delta,
            &mut delta_accumulator,
            &mut fallback_text,
            &mut tool_calls,
            &mut completed_response,
        )?;
    }

    // If we got a completed response, use it as the authoritative source for tool calls.
    if let Some((output, output_text)) = completed_response {
        let completed_tool_calls = extract_tool_calls(&output);
        if !completed_tool_calls.is_empty() {
            // Prefer completed response's tool calls over streaming deltas
            tool_calls = completed_tool_calls;
        }
        let text = if saw_delta {
            nonempty_preserve(Some(&delta_accumulator))
        } else {
            output_text.or(fallback_text)
        };
        return Ok(ChatResponse {
            text,
            tool_calls,
            usage: None,
            reasoning_content: None,
        });
    }

    // Fallback: assemble from streaming events
    let text = if saw_delta {
        nonempty_preserve(Some(&delta_accumulator))
    } else {
        fallback_text
    };

    Ok(ChatResponse {
        text,
        tool_calls,
        usage: None,
        reasoning_content: None,
    })
}

// ── HTTP request execution ──────────────────────────────────────────────────

impl OpenAiCodexProvider {
    async fn send_chat_request(
        &self,
        input: Vec<Value>,
        instructions: String,
        tools: Vec<Value>,
        model: &str,
    ) -> anyhow::Result<ChatResponse> {
        let profile = self
            .auth
            .get_profile("openai-codex", self.auth_profile_override.as_deref())
            .await?;

        let access_token = self
            .auth
            .get_valid_openai_access_token(self.auth_profile_override.as_deref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI Codex OAuth credentials not found. Run `crewforge auth login --provider openai-codex`."
                )
            })?;

        let id_token_str = profile
            .as_ref()
            .and_then(|p| p.token_set.as_ref())
            .and_then(|ts| ts.id_token.clone());
        let account_id = profile
            .and_then(|p| p.account_id)
            .or_else(|| {
                extract_account_id_from_tokens(id_token_str.as_deref(), &access_token)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI Codex account id not found in auth profile/token. Run `crewforge auth login --provider openai-codex` again."
                )
            })?;

        let normalized_model = normalize_model_id(model);

        let request = ResponsesRequest {
            model: normalized_model.to_string(),
            input,
            instructions,
            tools,
            store: false,
            stream: true,
            text: ResponsesTextOptions {
                verbosity: "medium".to_string(),
            },
            reasoning: ResponsesReasoningOptions {
                effort: resolve_reasoning_effort(normalized_model),
                summary: "auto".to_string(),
            },
            include: vec!["reasoning.encrypted_content".to_string()],
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
        };

        let request_builder = self
            .client
            .post(&self.responses_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("Content-Type", "application/json")
            .header("chatgpt-account-id", &account_id);

        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            return Err(super::api_error("OpenAI Codex", response).await);
        }

        let body = response.text().await?;
        parse_sse_to_chat_response(&body)
    }
}

// ── Provider trait implementation ───────────────────────────────────────────

#[async_trait]
impl Provider for OpenAiCodexProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ChatMessage::system(sys));
        }
        messages.push(ChatMessage::user(message));

        let (instructions, input) = build_responses_input(&messages);
        let response = self
            .send_chat_request(input, instructions, vec![], model)
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let (instructions, input) = build_responses_input(messages);
        let response = self
            .send_chat_request(input, instructions, vec![], model)
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (instructions, input) = build_responses_input(request.messages);
        let tools = request
            .tools
            .map(build_tools_json)
            .unwrap_or_default();
        self.send_chat_request(input, instructions, tools, model)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            match value {
                // SAFETY: tests run single-threaded (cfg(test)); no concurrent env access.
                Some(next) => unsafe { std::env::set_var(key, next) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.as_deref() {
                // SAFETY: tests run single-threaded (cfg(test)); no concurrent env access.
                unsafe { std::env::set_var(self.key, original) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn default_state_dir_is_non_empty() {
        let path = default_crewforge_dir();
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn build_responses_url_appends_suffix_for_base_url() {
        assert_eq!(
            build_responses_url("https://api.tonsof.blue/v1").unwrap(),
            "https://api.tonsof.blue/v1/responses"
        );
    }

    #[test]
    fn build_responses_url_keeps_existing_responses_endpoint() {
        assert_eq!(
            build_responses_url("https://api.tonsof.blue/v1/responses").unwrap(),
            "https://api.tonsof.blue/v1/responses"
        );
    }

    #[test]
    fn resolve_responses_url_prefers_explicit_endpoint_env() {
        let _endpoint_guard = EnvGuard::set(
            CODEX_RESPONSES_URL_ENV,
            Some("https://env.example.com/v1/responses"),
        );
        let _base_guard = EnvGuard::set(CODEX_BASE_URL_ENV, Some("https://base.example.com/v1"));

        let options = ProviderRuntimeOptions::default();
        assert_eq!(
            resolve_responses_url(&options).unwrap(),
            "https://env.example.com/v1/responses"
        );
    }

    #[test]
    fn resolve_responses_url_uses_provider_api_url_override() {
        let _endpoint_guard = EnvGuard::set(CODEX_RESPONSES_URL_ENV, None);
        let _base_guard = EnvGuard::set(CODEX_BASE_URL_ENV, None);

        let options = ProviderRuntimeOptions {
            provider_api_url: Some("https://proxy.example.com/v1".to_string()),
            ..ProviderRuntimeOptions::default()
        };

        assert_eq!(
            resolve_responses_url(&options).unwrap(),
            "https://proxy.example.com/v1/responses"
        );
    }

    #[test]
    fn constructor_with_custom_endpoint() {
        let options = ProviderRuntimeOptions {
            provider_api_url: Some("https://api.tonsof.blue/v1".to_string()),
            ..ProviderRuntimeOptions::default()
        };

        let provider = OpenAiCodexProvider::new(&options).unwrap();
        assert!(provider.responses_url.contains("tonsof.blue"));
    }

    #[test]
    fn resolve_instructions_uses_default_when_missing() {
        assert_eq!(
            resolve_instructions(None),
            DEFAULT_CODEX_INSTRUCTIONS.to_string()
        );
    }

    #[test]
    fn resolve_instructions_uses_default_when_blank() {
        assert_eq!(
            resolve_instructions(Some("   ")),
            DEFAULT_CODEX_INSTRUCTIONS.to_string()
        );
    }

    #[test]
    fn resolve_instructions_uses_system_prompt_when_present() {
        assert_eq!(
            resolve_instructions(Some("Be strict")),
            "Be strict".to_string()
        );
    }

    #[test]
    fn clamp_reasoning_effort_adjusts_known_models() {
        assert_eq!(
            clamp_reasoning_effort("gpt-5-codex", "xhigh"),
            "high".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5-codex", "minimal"),
            "low".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5-codex", "medium"),
            "medium".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.3-codex", "minimal"),
            "low".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1", "xhigh"),
            "high".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5-codex", "xhigh"),
            "high".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "low"),
            "medium".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "xhigh"),
            "high".to_string()
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.3-codex", "xhigh"),
            "xhigh".to_string()
        );
    }

    #[test]
    fn parse_sse_reads_text_deltas() {
        let payload = r#"data: {"type":"response.created","response":{"id":"resp_123"}}

data: {"type":"response.output_text.delta","delta":"Hello"}
data: {"type":"response.output_text.delta","delta":" world"}
data: {"type":"response.completed","response":{"output":[],"output_text":"Hello world"}}
data: [DONE]
"#;

        let response = parse_sse_to_chat_response(payload).unwrap();
        assert_eq!(response.text.as_deref(), Some("Hello world"));
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn parse_sse_reads_function_calls() {
        let payload = r#"data: {"type":"response.function_call_arguments.done","call_id":"call_123","name":"file_read","arguments":"{\"path\":\"a.txt\"}"}

data: {"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call_123","name":"file_read","arguments":"{\"path\":\"a.txt\"}"}],"output_text":null}}
data: [DONE]
"#;

        let response = parse_sse_to_chat_response(payload).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "file_read");
        assert_eq!(response.tool_calls[0].id, "call_123");
    }

    #[test]
    fn parse_sse_falls_back_to_completed_response() {
        let payload = r#"data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"Done"}]}],"output_text":"Done"}}
data: [DONE]
"#;

        let response = parse_sse_to_chat_response(payload).unwrap();
        assert_eq!(response.text.as_deref(), Some("Done"));
    }

    #[test]
    fn build_responses_input_maps_content_types_by_role() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hi"),
            ChatMessage::assistant("Hello!"),
            ChatMessage::user("Thanks"),
        ];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, "You are helpful.");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["role"], "user");
    }

    #[test]
    fn build_responses_input_uses_default_instructions_without_system() {
        let messages = vec![ChatMessage::user("Hello")];
        let (instructions, input) = build_responses_input(&messages);
        assert_eq!(instructions, DEFAULT_CODEX_INSTRUCTIONS);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn build_tools_json_creates_function_tools() {
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "Run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let json = build_tools_json(&tools);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["type"], "function");
        assert_eq!(json[0]["name"], "shell");
    }

    #[test]
    fn extract_tool_calls_from_output() {
        let output = vec![
            serde_json::json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "file_read",
                "arguments": "{\"path\":\"a.txt\"}"
            }),
            serde_json::json!({
                "type": "message",
                "content": [{"type": "output_text", "text": "hello"}]
            }),
        ];
        let calls = extract_tool_calls(&output);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
    }

    #[test]
    fn capabilities_reports_native_tools() {
        let options = ProviderRuntimeOptions::default();
        let provider = OpenAiCodexProvider::new(&options).expect("provider should initialize");
        let caps = provider.capabilities();

        assert!(caps.native_tool_calling);
        assert!(!caps.vision);
    }
}
