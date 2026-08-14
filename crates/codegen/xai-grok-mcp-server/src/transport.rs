use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use axum::{extract::Request, middleware::Next, response::Response};
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{GatewayServer, GatewaySession};

/// Stable Streamable HTTP endpoint path.
pub const MCP_HTTP_ENDPOINT: &str = "/mcp";

/// A bound Streamable HTTP gateway and its coordinated shutdown handle.
pub struct HttpGateway {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
    session: Arc<GatewaySession>,
}

impl HttpGateway {
    /// Bind the shared gateway session at [`MCP_HTTP_ENDPOINT`].
    pub async fn bind(server: GatewayServer, address: SocketAddr) -> anyhow::Result<Self> {
        Self::bind_with_stateful(server, address, true).await
    }

    /// Bind the shared gateway session at [`MCP_HTTP_ENDPOINT`].
    ///
    /// Stateless mode is useful for MCP clients which issue discovery requests
    /// before their `initialize` request and therefore have no session ID yet.
    pub async fn bind_with_stateful(
        server: GatewayServer,
        address: SocketAddr,
        stateful: bool,
    ) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("failed to bind MCP HTTP listener at {address}"))?;
        let address = listener.local_addr()?;
        let shutdown = CancellationToken::new();
        let session = Arc::clone(server.session());
        let service_session = Arc::clone(&session);
        let config = if address.ip().is_loopback() {
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts(address))
        } else {
            // A non-loopback bind is an explicit CLI choice. The public host
            // name is normally supplied by a reverse proxy and cannot be
            // inferred from a bound IP, so that deployment owns Host checks.
            StreamableHttpServerConfig::default().disable_allowed_hosts()
        }
        .with_stateful_mode(stateful)
        .with_cancellation_token(shutdown.child_token());
        let service = StreamableHttpService::new(
            move || {
                Ok(GatewayServer::from_shared_session(Arc::clone(
                    &service_session,
                )))
            },
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let app = axum::Router::new()
            .nest_service(MCP_HTTP_ENDPOINT, service)
            // Keep this deliberately header-only: MCP requests often contain
            // source code and tool arguments, which must not enter logs.
            .layer(axum::middleware::from_fn(log_mcp_http_request));
        let server_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled_owned().await })
                .await
        });

        Ok(Self {
            address,
            shutdown,
            task,
            session,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}{}", self.address, MCP_HTTP_ENDPOINT)
    }

    /// Stop HTTP sessions, downstream MCP transports, and terminal processes.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        self.task.await.context("MCP HTTP server task failed")??;
        self.session.shutdown().await;
        Ok(())
    }
}

/// Emit request metadata needed to diagnose remote MCP client compatibility.
///
/// This is intentionally at `info` level so it is available with the CLI's
/// normal tracing setup. Do not add request/response bodies here: they can
/// contain workspace data or tool arguments.
async fn log_mcp_http_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let headers = request.headers();
    let accept = header_value(headers, "accept");
    let content_type = header_value(headers, "content-type");
    let host = header_value(headers, "host");
    let origin = header_value(headers, "origin");
    let user_agent = header_value(headers, "user-agent");
    let mcp_session_id = header_value(headers, "mcp-session-id");

    let response = next.run(request).await;
    tracing::info!(
        method = %method,
        path,
        status = %response.status(),
        elapsed_ms = started.elapsed().as_millis(),
        accept = ?accept,
        content_type = ?content_type,
        host = ?host,
        origin = ?origin,
        user_agent = ?user_agent,
        mcp_session_id = ?mcp_session_id,
        "MCP HTTP request"
    );
    response
}

fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn allowed_hosts(address: SocketAddr) -> Vec<String> {
    let mut hosts = vec![address.ip().to_string()];
    if address.ip().is_loopback() {
        hosts.extend([
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ]);
    }
    hosts
}
