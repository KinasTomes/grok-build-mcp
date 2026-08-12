use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
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
        let app = axum::Router::new().nest_service(MCP_HTTP_ENDPOINT, service);
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
