//! Lightweight standalone CLI for the Grok Build MCP gateway.
//!
//! Supports headless stdio/HTTP mode (`--features cli`) and an optional
//! minimal ratatui observer UI (`--features ui`).

#[cfg(feature = "ui")]
mod observer_ui;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use rmcp::ServiceExt;
use xai_grok_mcp_server::{
    GatewayServer, GatewaySession, HttpGateway, TerminalApprovalBroker, gateway_api_key_from_env,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum GatewayTransport {
    Stdio,
    Http,
}

/// Standalone Grok Build MCP gateway server.
#[derive(Debug, Parser, Clone)]
#[command(name = "xai-grok-mcp", version, about)]
struct GatewayCliArgs {
    /// Workspace root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
    /// Native Grok MCP configuration file for downstream servers.
    #[arg(long, value_name = "FILE")]
    mcp_config: Option<PathBuf>,
    /// Gateway transport.
    #[arg(long, value_enum, default_value_t = GatewayTransport::Http)]
    transport: GatewayTransport,
    /// Interface to bind for HTTP. Defaults to loopback.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// HTTP port.
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Disable MCP HTTP sessions for clients that probe before `initialize`.
    #[arg(long)]
    stateless: bool,
    /// Run without the observer approval UI. Required for stdio.
    #[arg(long)]
    headless: bool,
    /// Use the terminal approval prompt fallback instead of the observer UI.
    #[arg(long)]
    terminal_approval: bool,
    /// Auto-approve promptable calls; hard policy denials still apply.
    #[arg(
        long = "always-approve",
        alias = "yolo",
        alias = "dangerously-skip-permissions"
    )]
    always_approve: bool,
    /// Disable API key authentication for the HTTP gateway.
    #[arg(long = "no-auth")]
    no_auth: bool,
    /// Additional allowed paths outside the workspace (repeatable).
    #[arg(long = "allow-path", value_name = "PATH")]
    allow_path: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let args = GatewayCliArgs::parse();
    run_gateway_server(args).await
}

async fn run_gateway_server(args: GatewayCliArgs) -> Result<()> {
    if !args.headless && args.transport != GatewayTransport::Http {
        bail!("the observer UI requires --transport http; pass --headless for stdio");
    }
    if args.terminal_approval && args.transport != GatewayTransport::Http {
        bail!("--terminal-approval is only supported with --transport http");
    }
    if args.stateless && args.transport != GatewayTransport::Http {
        bail!("--stateless is only supported with --transport http");
    }

    let api_key = (args.transport == GatewayTransport::Http && !args.no_auth)
        .then(gateway_api_key_from_env)
        .transpose()?;
    let workspace = dunce::canonicalize(args.workspace.unwrap_or(std::env::current_dir()?))?;
    let session = match args.mcp_config {
        Some(path) => GatewaySession::with_native_mcp_config(&workspace, path).await?,
        None => GatewaySession::new(&workspace)?,
    };
    let session = if args.always_approve {
        session.with_always_approve()
    } else {
        session
    };
    let session = if !args.allow_path.is_empty() {
        session.with_allowed_paths(args.allow_path)
    } else {
        session
    };

    if !args.headless && !args.terminal_approval {
        return run_observer_mode(session, workspace, args.host, args.port, !args.stateless, api_key)
            .await;
    }

    let session = if args.terminal_approval {
        session.with_approval_broker(Arc::new(TerminalApprovalBroker))
    } else {
        session
    };
    let downstream_servers = session.downstream_server_count().await;
    let exposed_tools = session.toolset().tool_definitions().len();
    let server = GatewayServer::new(session);
    match args.transport {
        GatewayTransport::Stdio => {
            eprintln!(
                "grok mcp server stdio workspace={} downstream_servers={} exposed_tools={}",
                workspace.display(),
                downstream_servers,
                exposed_tools
            );
            server
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
        }
        GatewayTransport::Http => {
            let http = HttpGateway::bind_with_stateful_and_api_key(
                server,
                SocketAddr::new(args.host, args.port),
                !args.stateless,
                api_key,
            )
            .await?;
            eprintln!(
                "grok mcp server HTTP bound={} workspace={} endpoint={} downstream_servers={} exposed_tools={}",
                http.address(),
                workspace.display(),
                http.endpoint(),
                downstream_servers,
                exposed_tools
            );
            tokio::signal::ctrl_c().await?;
            http.shutdown().await?;
        }
    }
    Ok(())
}

#[cfg(feature = "ui")]
async fn run_observer_mode(
    session: GatewaySession,
    workspace: PathBuf,
    host: IpAddr,
    port: u16,
    stateful: bool,
    api_key: Option<String>,
) -> Result<()> {
    let (broker, approvals) = xai_grok_mcp_server::ChannelApprovalBroker::new(32);
    let session = session.with_approval_broker(broker);
    let server = GatewayServer::new(session);
    let session = Arc::clone(server.session());
    let bridge =
        xai_grok_mcp_server::LocalObserverBridge::spawn(session.event_bus(), approvals);
    let http = HttpGateway::bind_with_stateful_and_api_key(
        server,
        SocketAddr::new(host, port),
        stateful,
        api_key,
    )
    .await?;
    let status = observer_ui::ObserverStatus {
        workspace: workspace.display().to_string(),
        endpoint: http.endpoint(),
        downstream_servers: session.downstream_server_count().await,
        exposed_tools: session.toolset().tool_definitions().len(),
    };
    let result = observer_ui::run(bridge, status).await;
    http.shutdown().await?;
    result
}

#[cfg(not(feature = "ui"))]
async fn run_observer_mode(
    _session: GatewaySession,
    _workspace: PathBuf,
    _host: IpAddr,
    _port: u16,
    _stateful: bool,
    _api_key: Option<String>,
) -> Result<()> {
    bail!(
        "observer UI is not compiled in (built without `--features ui`); pass --headless or --terminal-approval"
    )
}
