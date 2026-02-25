mod generated;

use s3s::S3;

use std::future::Future;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

/// An MCP server that exposes S3 operations as MCP tools.
pub struct McpServer {
    s3: Arc<dyn S3>,
    tools: Vec<Tool>,
}

impl McpServer {
    /// Create a new MCP server wrapping the given S3 implementation.
    pub fn new(s3: impl S3) -> Self {
        Self {
            s3: Arc::new(s3),
            tools: generated::tool_definitions(),
        }
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("S3-compatible storage service exposed via MCP".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: self.tools.clone(),
            meta: None,
            next_cursor: None,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let name = request.name.into_owned();
        async move { generated::dispatch(&*self.s3, &name).await }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }
}
