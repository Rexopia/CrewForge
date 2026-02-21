use std::borrow::Cow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

use crate::hub::RoomHub;
use crate::kernel::MessageRole;

#[derive(Clone)]
struct HubToolServer {
    room_hub: Arc<RoomHub>,
    agent_id: String,
    tools: Arc<Vec<Tool>>,
}

impl HubToolServer {
    fn new(room_hub: Arc<RoomHub>, agent_id: String) -> Self {
        let tools = vec![hub_get_tool(), hub_ack_tool(), hub_post_tool()];
        Self {
            room_hub,
            agent_id,
            tools: Arc::new(tools),
        }
    }
}

impl ServerHandler for HubToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..ServerInfo::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = std::result::Result<ListToolsResult, McpError>> + Send + '_
    {
        let tools = self.tools.clone();
        async move {
            Ok(ListToolsResult {
                tools: (*tools).clone(),
                next_cursor: None,
                meta: None,
            })
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "hub_get_unread" => {
                let unread_result = self
                    .room_hub
                    .get_unread(&self.agent_id)
                    .await
                    .map_err(internal_error)?;

                let text = if unread_result.unread.is_empty() {
                    "(no unread events)".to_string()
                } else {
                    unread_result
                        .unread
                        .iter()
                        .map(|event| {
                            format!("#{} [{}] {}", event.event_seq, event.speaker, event.text)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                Ok(CallToolResult {
                    content: vec![Content::text(text)],
                    structured_content: Some(json!({
                        "count": unread_result.unread.len(),
                        "uptoEventSeq": unread_result.upto_event_seq,
                        "unread": unread_result.unread.iter().map(|event| {
                            json!({
                                "eventSeq": event.event_seq,
                                "role": match event.role { MessageRole::Human => "human", MessageRole::Agent => "agent" },
                                "speaker": event.speaker,
                                "text": event.text,
                                "ts": event.ts,
                            })
                        }).collect::<Vec<_>>(),
                    })),
                    is_error: Some(false),
                    meta: None,
                })
            }
            "hub_ack" => {
                let arguments = request.arguments.ok_or_else(|| {
                    McpError::invalid_params("missing arguments for hub_ack", None)
                })?;
                let upto_event_seq =
                    as_positive_u64(arguments.get("uptoEventSeq")).ok_or_else(|| {
                        McpError::invalid_params("uptoEventSeq must be positive integer", None)
                    })?;

                let acked = self
                    .room_hub
                    .ack(&self.agent_id, upto_event_seq)
                    .await
                    .map_err(internal_error)?;

                Ok(CallToolResult {
                    content: vec![Content::text(format!("acked through event #{acked}"))],
                    structured_content: Some(json!({ "ackedEventSeq": acked })),
                    is_error: Some(false),
                    meta: None,
                })
            }
            "hub_post" => {
                let arguments = request.arguments.ok_or_else(|| {
                    McpError::invalid_params("missing arguments for hub_post", None)
                })?;

                let text = arguments
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| McpError::invalid_params("text is required", None))?;

                let ack_event_seq = as_positive_u64(arguments.get("ackEventSeq"));

                let result = self
                    .room_hub
                    .post(&self.agent_id, &text, ack_event_seq)
                    .await
                    .map_err(internal_error)?;

                let body = if result.posted {
                    format!(
                        "posted event #{}",
                        result
                            .message
                            .as_ref()
                            .map(|m| m.event_seq)
                            .unwrap_or_default()
                    )
                } else {
                    format!("not posted: {}", result.reason)
                };

                Ok(CallToolResult {
                    content: vec![Content::text(body)],
                    structured_content: Some(json!({
                        "posted": result.posted,
                        "reason": result.reason,
                        "eventSeq": result.message.as_ref().map(|msg| msg.event_seq),
                    })),
                    is_error: Some(false),
                    meta: None,
                })
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

pub struct RoomHubMcpServer {
    host: String,
    bound_addr: Option<SocketAddr>,
    agent_token_by_id: HashMap<String, String>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<tokio::task::JoinHandle<()>>,
}

impl RoomHubMcpServer {
    pub fn new(host: &str, agent_token_by_id: Vec<(String, String)>) -> Self {
        Self {
            host: host.to_string(),
            bound_addr: None,
            agent_token_by_id: agent_token_by_id.into_iter().collect(),
            shutdown_tx: None,
            join_handle: None,
        }
    }

    pub fn base_url(&self) -> Result<String> {
        let addr = self
            .bound_addr
            .ok_or_else(|| anyhow!("RoomHubMcpServer is not started"))?;
        Ok(format!("http://{}:{}", self.host, addr.port()))
    }

    pub fn get_mcp_url_for_agent(&self, agent_id: &str) -> Result<String> {
        let token = self
            .agent_token_by_id
            .get(agent_id)
            .ok_or_else(|| anyhow!("missing MCP token for agent: {agent_id}"))?;
        Ok(format!(
            "{}/mcp?token={}",
            self.base_url()?,
            urlencoding::encode(token)
        ))
    }

    pub async fn start(&mut self, room_hub: Arc<RoomHub>) -> Result<()> {
        if self.join_handle.is_some() {
            return Ok(());
        }

        let mut services_by_token = HashMap::new();
        for (agent_id, token) in &self.agent_token_by_id {
            let hub = room_hub.clone();
            let id = agent_id.clone();
            let service = StreamableHttpService::new(
                move || Ok(HubToolServer::new(hub.clone(), id.clone())),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            );
            services_by_token.insert(token.clone(), service);
        }

        let state = McpRouteState {
            services_by_token: Arc::new(services_by_token),
        };
        let router = Router::new()
            .route("/mcp", any(handle_mcp_request))
            .with_state(state);

        let bind_addr = format!("{}:0", self.host);
        let listener = TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("failed to bind MCP server at {bind_addr}"))?;
        let local_addr = listener
            .local_addr()
            .context("failed to read mcp server local addr")?;

        let (tx, rx) = oneshot::channel::<()>();
        let server = axum::serve(listener, router).with_graceful_shutdown(async {
            let _ = rx.await;
        });

        let handle = tokio::spawn(async move {
            if let Err(error) = server.await {
                eprintln!("mcp server stopped with error: {error}");
            }
        });

        self.bound_addr = Some(local_addr);
        self.shutdown_tx = Some(tx);
        self.join_handle = Some(handle);
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }

        self.bound_addr = None;
        Ok(())
    }
}

#[derive(Clone)]
struct McpRouteState {
    services_by_token:
        Arc<HashMap<String, StreamableHttpService<HubToolServer, LocalSessionManager>>>,
}

#[derive(Debug, serde::Deserialize)]
struct McpQuery {
    token: Option<String>,
}

async fn handle_mcp_request(
    State(state): State<McpRouteState>,
    Query(query): Query<McpQuery>,
    req: Request<Body>,
) -> impl IntoResponse {
    let token = match query.token {
        Some(token) if !token.trim().is_empty() => token,
        _ => {
            return (StatusCode::UNAUTHORIZED, "missing MCP token").into_response();
        }
    };

    let Some(service) = state.services_by_token.get(&token).cloned() else {
        return (StatusCode::UNAUTHORIZED, "invalid MCP token").into_response();
    };

    match service.oneshot(req).await {
        Ok(response) => response.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mcp service error: {error}"),
        )
            .into_response(),
    }
}

fn internal_error(error: anyhow::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn as_positive_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    let val = value?;
    if let Some(v) = val.as_u64() {
        if v > 0 {
            return Some(v);
        }
    }
    if let Some(v) = val.as_i64() {
        if v > 0 {
            return Some(v as u64);
        }
    }
    None
}

fn hub_get_tool() -> Tool {
    let schema: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
    .expect("hub_get schema should deserialize");

    Tool::new(
        Cow::Borrowed("hub_get_unread"),
        Cow::Borrowed("Get unread room events for the connected agent."),
        Arc::new(schema),
    )
}

fn hub_ack_tool() -> Tool {
    let schema: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "properties": {
            "uptoEventSeq": { "type": "integer", "minimum": 1 }
        },
        "required": ["uptoEventSeq"],
        "additionalProperties": false
    }))
    .expect("hub_ack schema should deserialize");

    Tool::new(
        Cow::Borrowed("hub_ack"),
        Cow::Borrowed("Advance read cursor for the connected agent."),
        Arc::new(schema),
    )
}

fn hub_post_tool() -> Tool {
    let schema: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "minLength": 1 },
            "ackEventSeq": { "type": "integer", "minimum": 1 }
        },
        "required": ["text"],
        "additionalProperties": false
    }))
    .expect("hub_post schema should deserialize");

    Tool::new(
        Cow::Borrowed("hub_post"),
        Cow::Borrowed("Post one room message as the connected agent."),
        Arc::new(schema),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, AgentTools, RateLimitConfig};
    use crate::kernel::{MessageRole, SessionKernel};

    #[tokio::test]
    async fn mcp_server_starts_and_generates_agent_url() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let agents = vec![
            AgentConfig {
                id: "codex".into(),
                name: "Codex".into(),
                model: "m".into(),
                context_dir: ".room/agents/codex".into(),
                tools: AgentTools::default(),
            },
            AgentConfig {
                id: "kimi".into(),
                name: "Kimi".into(),
                model: "m".into(),
                context_dir: ".room/agents/kimi".into(),
                tools: AgentTools::default(),
            },
        ];
        kernel
            .append_message(MessageRole::Human, "Rex".into(), "hello".into(), None)
            .await
            .expect("append");

        let hub = Arc::new(RoomHub::new(
            kernel,
            &agents,
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        ));

        let mut server =
            RoomHubMcpServer::new("127.0.0.1", vec![("codex".into(), "token1".into())]);
        server.start(hub).await.expect("start server");
        let url = server
            .get_mcp_url_for_agent("codex")
            .expect("url should exist");
        assert!(url.contains("/mcp?token=token1"));
        server.stop().await.expect("stop server");
    }
}
