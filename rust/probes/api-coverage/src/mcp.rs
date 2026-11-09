//! rmcp covering `src/api/mcp_server.py`: FastMCP `name='fav'`, `instructions=...`,
//! `stateless_http=True`, `json_response=True`, DNS-rebinding protection off,
//! mounted at `/mcp`, 18 tools whose input schemas must match Python's byte for byte.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

/// One tool as the Python server lists it; the schema is taken verbatim (a golden in WP4).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct FavMcp {
    pub instructions: &'static str,
    pub tools: Arc<Vec<ToolSpec>>,
}

impl ServerHandler for FavMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fav", "1.30.0"))
            .with_instructions(self.instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .tools
            .iter()
            .map(|spec| {
                let schema: JsonObject = spec.input_schema.as_object().cloned().unwrap_or_default();
                Tool::new(spec.name, spec.description, Arc::new(schema))
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        if !self.tools.iter().any(|tool| tool.name == request.name) {
            // FastMCP reports an unknown tool as a tool error, not a protocol error.
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!("Unknown tool: {}", request.name))]).into());
        }
        let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let payload = serde_json::json!({ "tool": request.name, "arguments": arguments });
        let mut result = CallToolResult::structured(payload.clone());
        result.content = vec![ContentBlock::text(payload.to_string())];
        Ok(result.into())
    }
}

/// `FastMCP(stateless_http=True, json_response=True, transport_security=... off)`.
pub fn service(server: FavMcp) -> StreamableHttpService<FavMcp, NeverSessionManager> {
    let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    config.legacy_session_mode = false;
    config.json_response = true;
    StreamableHttpService::new(move || Ok(server.clone()), Arc::new(NeverSessionManager::default()), config)
}

/// Mounted on the API's axum router, as `app.mount(MCP_PATH, mcp.streamable_http_app())`.
pub fn router(server: FavMcp) -> axum::Router {
    axum::Router::new().nest_service("/mcp", service(server))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn server() -> FavMcp {
        FavMcp {
            instructions: "fav crawls content sources on a schedule.",
            tools: Arc::new(vec![ToolSpec {
                name: "get_run",
                description: "One run with its error and log excerpt.",
                // Shape and key order as FastMCP emits them, including `title`.
                input_schema: serde_json::from_str(
                    r#"{"properties":{"run_id":{"title":"Run Id","type":"integer"}},"required":["run_id"],"title":"get_runArguments","type":"object"}"#,
                )
                .unwrap(),
            }]),
        }
    }

    async fn post(body: serde_json::Value, host: &str) -> (http::StatusCode, String, serde_json::Value) {
        let request = http::Request::post("/mcp")
            .header("host", host)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(server()).oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, content_type, json)
    }

    #[tokio::test]
    async fn stateless_json_initialize_from_any_host() {
        let (status, content_type, body) = post(
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"litellm","version":"1"}}}),
            "fav.media.svc.cluster.local:8000",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(content_type.starts_with("application/json"), "{content_type}");
        assert_eq!(body["result"]["serverInfo"]["name"], "fav");
        assert_eq!(body["result"]["instructions"], "fav crawls content sources on a schedule.");
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn tools_list_passes_the_python_schema_through_unchanged() {
        let (status, _, body) = post(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}), "fav.s117.me").await;
        assert_eq!(status, 200, "{body}");
        let schema = &body["result"]["tools"][0]["inputSchema"];
        assert_eq!(
            serde_json::to_string(schema).unwrap(),
            r#"{"properties":{"run_id":{"title":"Run Id","type":"integer"}},"required":["run_id"],"title":"get_runArguments","type":"object"}"#
        );
    }

    #[tokio::test]
    async fn tools_call_returns_structured_and_text_content() {
        let (status, _, body) = post(
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_run","arguments":{"run_id":7}}}),
            "fav.s117.me",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["structuredContent"]["arguments"]["run_id"], 7);
        assert_eq!(body["result"]["content"][0]["type"], "text");
        assert_eq!(body["result"]["isError"], serde_json::Value::Bool(false));
    }
}
