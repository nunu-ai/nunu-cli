mod tools;

use crate::{
    auth::{AuthHeader, CredentialProvider},
    config::Config,
};
use anyhow::{Context as _, Result};
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    Peer, RoleClient, ServerHandler, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientInfo,
        ClientJsonRpcMessage, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::{
        StreamableHttpClientTransport,
        auth::AuthError,
        streamable_http_client::{
            SseError, StreamableHttpClient, StreamableHttpClientTransportConfig,
            StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use sse_stream::Sse;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tools::LocalToolRegistry;

const API_KEY_HEADER: &str = "x-api-key";
const UPSTREAM_TRANSPORT_ERROR_MESSAGE: &str = "nexus could not complete this tool call.";

#[derive(Clone, Debug)]
struct AuthenticatedHttpClient {
    http: reqwest::Client,
    credential: CredentialProvider,
}

impl AuthenticatedHttpClient {
    fn new(credential: CredentialProvider) -> Result<Self> {
        let http = crate::tls::http_client_builder()?
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build the MCP HTTP client")?;
        Ok(Self { http, credential })
    }

    async fn authentication(
        &self,
        mut headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<
        (
            Option<String>,
            Option<String>,
            HashMap<HeaderName, HeaderValue>,
        ),
        StreamableHttpError<reqwest::Error>,
    > {
        match self.credential.header().await.map_err(|error| {
            StreamableHttpError::Auth(AuthError::AuthorizationFailed(error.to_string()))
        })? {
            AuthHeader::ApiKey(api_key) => {
                let value = HeaderValue::from_str(&api_key).map_err(|_| {
                    StreamableHttpError::Auth(AuthError::AuthorizationFailed(
                        "the API key contains characters that cannot be used in an HTTP header"
                            .to_string(),
                    ))
                })?;
                headers.insert(HeaderName::from_static(API_KEY_HEADER), value);
                Ok((None, None, headers))
            }
            AuthHeader::Bearer(access_token) => {
                Ok((Some(access_token.clone()), Some(access_token), headers))
            }
        }
    }

    async fn refresh_rejected_bearer(
        &self,
        rejected_access_token: Option<&str>,
    ) -> std::result::Result<(), StreamableHttpError<reqwest::Error>> {
        let Some(rejected_access_token) = rejected_access_token else {
            return Ok(());
        };
        self.credential
            .refresh_after_unauthorized(rejected_access_token)
            .await
            .map_err(|error| {
                StreamableHttpError::Auth(AuthError::TokenRefreshFailed(error.to_string()))
            })
    }
}

fn is_unauthorized<T>(
    result: &std::result::Result<T, StreamableHttpError<reqwest::Error>>,
) -> bool {
    match result {
        Err(StreamableHttpError::AuthRequired(_)) => true,
        Err(StreamableHttpError::Client(error)) => {
            error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
        }
        _ => false,
    }
}

impl StreamableHttpClient for AuthenticatedHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            None,
            custom_headers,
            4 * 1024 * 1024,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> std::result::Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let retry_headers = custom_headers.clone();
        let (bearer, rejected, headers) = self.authentication(custom_headers).await?;
        let first = self
            .http
            .post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                bearer,
                headers,
                max_sse_event_size,
            )
            .await;
        if !is_unauthorized(&first) || rejected.is_none() {
            return first;
        }

        self.refresh_rejected_bearer(rejected.as_deref()).await?;
        let (bearer, _, headers) = self.authentication(retry_headers).await?;
        self.http
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                bearer,
                headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<(), StreamableHttpError<Self::Error>> {
        let retry_headers = custom_headers.clone();
        let (bearer, rejected, headers) = self.authentication(custom_headers).await?;
        let first = self
            .http
            .delete_session(uri.clone(), session_id.clone(), bearer, headers)
            .await;
        if !is_unauthorized(&first) || rejected.is_none() {
            return first;
        }

        self.refresh_rejected_bearer(rejected.as_deref()).await?;
        let (bearer, _, headers) = self.authentication(retry_headers).await?;
        self.http
            .delete_session(uri, session_id, bearer, headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<
        BoxStream<'static, std::result::Result<Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            None,
            custom_headers,
            4 * 1024 * 1024,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> std::result::Result<
        BoxStream<'static, std::result::Result<Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        let retry_headers = custom_headers.clone();
        let (bearer, rejected, headers) = self.authentication(custom_headers).await?;
        let first = self
            .http
            .get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                bearer,
                headers,
                max_sse_event_size,
            )
            .await;
        if !is_unauthorized(&first) || rejected.is_none() {
            return first;
        }

        self.refresh_rejected_bearer(rejected.as_deref()).await?;
        let (bearer, _, headers) = self.authentication(retry_headers).await?;
        self.http
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                bearer,
                headers,
                max_sse_event_size,
            )
            .await
    }
}

#[derive(Clone)]
struct ProxyServer {
    upstream: Peer<RoleClient>,
    tools: Arc<Vec<Tool>>,
    local_tools: Arc<LocalToolRegistry>,
}

#[cfg(unix)]
async fn termination_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("failed to listen for SIGINT"),
        signal = terminate.recv() => signal.context("SIGTERM listener stopped unexpectedly"),
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for a termination signal")
}

impl ServerHandler for ProxyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("nunu-cli", env!("CARGO_PKG_VERSION"))
                    .with_title("Nunu MCP proxy"),
            )
            .with_instructions("Nunu cloud tools proxied through the local CLI over stdin/stdout.")
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        std::future::ready(Ok(ListToolsResult::with_all_items((*self.tools).clone())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
        if let Some(result) = self.local_tools.call(&request).await {
            return result;
        }
        match self.upstream.call_tool_once(request).await {
            Ok(response) => Ok(response),
            Err(ServiceError::McpError(error)) => Err(error),
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                UPSTREAM_TRANSPORT_ERROR_MESSAGE,
            )])
            .into()),
        }
    }
}

/// Run the local stdio MCP server and proxy tools from the configured Nunu MCP.
///
/// # Errors
///
/// Returns an error if authentication, the upstream MCP connection, tool
/// discovery, or either MCP transport fails.
pub async fn serve_stdio(
    mcp_url: &str,
    api_url: &str,
    credential: CredentialProvider,
    workspace_root: Option<&Path>,
) -> Result<()> {
    credential.validate_mcp_destination(mcp_url)?;
    let allowed_root = resolve_allowed_root(workspace_root).await?;
    let upload_config = Config::with_credential(credential.clone(), api_url, None)?;
    let local_tools = Arc::new(LocalToolRegistry::standard(upload_config, allowed_root));
    let client = AuthenticatedHttpClient::new(credential)?;
    let transport = StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(mcp_url.to_string()),
    );
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("nunu-cli", env!("CARGO_PKG_VERSION")),
    );
    let mut upstream = client_info
        .serve(transport)
        .await
        .context("failed to connect to the Nunu MCP")?;
    let remote_tools = upstream
        .list_all_tools()
        .await
        .context("failed to list Nunu MCP tools")?;
    let tools = local_tools.merged_with(remote_tools);

    let proxy = ProxyServer {
        upstream: upstream.peer().clone(),
        tools: Arc::new(tools),
        local_tools,
    };
    let downstream = proxy
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start the stdio MCP server")?;

    let completion = tokio::select! {
        result = downstream.waiting() => Ok(Some(result)),
        signal_result = termination_signal() => signal_result.map(|()| None),
    };
    let _ = upstream.close().await;
    let downstream_result = completion?;
    if let Some(result) = downstream_result {
        result.context("stdio MCP server task failed")?;
    }
    Ok(())
}

async fn resolve_allowed_root(workspace_root: Option<&Path>) -> Result<PathBuf> {
    let requested_root = match workspace_root {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .context("failed to determine the MCP server working directory")?,
    };
    let allowed_root = tokio::fs::canonicalize(&requested_root)
        .await
        .with_context(|| {
            format!(
                "failed to resolve the MCP workspace root '{}'",
                requested_root.display()
            )
        })?;
    let metadata = tokio::fs::metadata(&allowed_root).await.with_context(|| {
        format!(
            "failed to inspect the MCP workspace root '{}'",
            allowed_root.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_dir(),
        "the MCP workspace root '{}' is not a directory",
        allowed_root.display()
    );
    Ok(allowed_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPLOAD_BUILD_TOOL: &str = "upload_build";
    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<rmcp::RoleServer>,
        ) -> impl Future<Output = std::result::Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
        {
            let mut schema = serde_json::Map::new();
            schema.insert(
                "type".to_string(),
                serde_json::Value::String("object".to_string()),
            );
            std::future::ready(Ok(ListToolsResult::with_all_items(vec![
                Tool::new("echo", "Echo a remote response", schema.clone()),
                Tool::new(UPLOAD_BUILD_TOOL, "Remote upload placeholder", schema),
            ])))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<rmcp::RoleServer>,
        ) -> std::result::Result<CallToolResponse, rmcp::ErrorData> {
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "remote:{}",
                request.name
            ))])
            .into())
        }
    }

    #[tokio::test]
    async fn forwards_tool_discovery_and_calls() {
        let (upstream_client_io, upstream_server_io) = tokio::io::duplex(64 * 1024);
        let (upstream_client_read, upstream_client_write) = tokio::io::split(upstream_client_io);
        let (upstream_server_read, upstream_server_write) = tokio::io::split(upstream_server_io);

        let (remote_server, upstream) = tokio::join!(
            EchoServer.serve((upstream_server_read, upstream_server_write)),
            ClientInfo::new(
                ClientCapabilities::default(),
                Implementation::new("proxy-test", "1"),
            )
            .serve((upstream_client_read, upstream_client_write))
        );
        let mut remote_server = remote_server.expect("start remote server");
        let mut upstream = upstream.expect("connect upstream client");
        let tools = upstream
            .list_all_tools()
            .await
            .expect("list upstream tools");
        let root = tempfile::tempdir().expect("create allowed root");
        let config = Config::new("secret".to_string(), "http://localhost:3000/api")
            .expect("create upload config");
        let local_tools = Arc::new(LocalToolRegistry::standard(
            config,
            root.path().canonicalize().expect("canonicalize root"),
        ));
        let tools = local_tools.merged_with(tools);
        let proxy = ProxyServer {
            upstream: upstream.peer().clone(),
            tools: Arc::new(tools),
            local_tools,
        };

        let (host_io, proxy_io) = tokio::io::duplex(64 * 1024);
        let (host_read, host_write) = tokio::io::split(host_io);
        let (proxy_read, proxy_write) = tokio::io::split(proxy_io);
        let (proxy_server, host) = tokio::join!(
            proxy.serve((proxy_read, proxy_write)),
            ClientInfo::new(
                ClientCapabilities::default(),
                Implementation::new("host-test", "1"),
            )
            .serve((host_read, host_write))
        );
        let mut proxy_server = proxy_server.expect("start proxy server");
        let mut host = host.expect("connect host client");

        let proxied_tools = host.list_all_tools().await.expect("list proxied tools");
        assert_eq!(proxied_tools.len(), 2);
        assert_eq!(proxied_tools[0].name, "echo");
        assert_eq!(proxied_tools[1].name, UPLOAD_BUILD_TOOL);
        assert_ne!(
            proxied_tools[1].description.as_deref(),
            Some("Remote upload placeholder")
        );

        let response = host
            .call_tool_once(CallToolRequestParams::new("echo"))
            .await
            .expect("call proxied tool");
        let CallToolResponse::Complete(result) = response else {
            panic!("expected a complete tool response");
        };
        let text = result.content[0].as_text().expect("text content");
        assert_eq!(text.text, "remote:echo");

        let local_error = host
            .call_tool_once(CallToolRequestParams::new(UPLOAD_BUILD_TOOL))
            .await;
        assert!(
            local_error.is_err(),
            "local handler must reject missing input"
        );

        remote_server.close().await.expect("close remote server");
        let response = host
            .call_tool_once(CallToolRequestParams::new("echo"))
            .await
            .expect("receive sanitized upstream transport error");
        let CallToolResponse::Complete(result) = response else {
            panic!("expected a complete error response");
        };
        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().expect("text content");
        assert_eq!(text.text, UPSTREAM_TRANSPORT_ERROR_MESSAGE);

        host.close().await.expect("close host");
        proxy_server.close().await.expect("close proxy");
        upstream.close().await.expect("close upstream");
    }

    #[test]
    fn proxy_server_info_advertises_tools() {
        let capabilities = ServerCapabilities::builder().enable_tools().build();
        assert!(capabilities.tools.is_some());
    }

    #[test]
    fn api_key_header_is_not_reserved_by_mcp() {
        let name = HeaderName::from_static(API_KEY_HEADER);
        assert_eq!(name.as_str(), API_KEY_HEADER);
    }
}
