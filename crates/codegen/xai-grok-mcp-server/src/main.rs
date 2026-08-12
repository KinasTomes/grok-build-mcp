use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;
use rmcp::ServiceExt;
use xai_grok_mcp_server::{GatewayServer, GatewaySession, HttpGateway};
use xai_grok_sandbox::{ProfileName, SandboxManager};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Transport {
    Stdio,
    Http,
}

struct Args {
    workspace: PathBuf,
    mcp_config: Option<PathBuf>,
    transport: Transport,
    host: IpAddr,
    port: u16,
}

const USAGE: &str = "usage: xai-grok-mcp-server --workspace <path> [--mcp-config <native-grok-config.toml>] [--transport stdio|http] [--host 127.0.0.1] [--port 8765]";

fn parse_args() -> anyhow::Result<Args> {
    let mut args = std::env::args_os().skip(1);
    let mut workspace = None;
    let mut mcp_config = None;
    let mut transport = Transport::Stdio;
    let mut host: IpAddr = "127.0.0.1".parse().expect("valid loopback address");
    let mut port = 8765;
    while let Some(flag) = args.next() {
        match flag.to_string_lossy().as_ref() {
            "--workspace" => workspace = args.next().map(PathBuf::from),
            "--mcp-config" => mcp_config = args.next().map(PathBuf::from),
            "--transport" => match args.next().as_deref().and_then(|v| v.to_str()) {
                Some("stdio") => transport = Transport::Stdio,
                Some("http") => transport = Transport::Http,
                _ => anyhow::bail!("{USAGE}"),
            },
            "--host" => {
                host = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--host must be an IP address; {USAGE}"))?;
            }
            "--port" => {
                port = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--port must be a valid TCP port; {USAGE}"))?;
            }
            _ => anyhow::bail!("{USAGE}"),
        }
    }
    Ok(Args {
        workspace: workspace.ok_or_else(|| anyhow::anyhow!(USAGE))?,
        mcp_config,
        transport,
        host,
        port,
    })
}

fn initialize_sandbox(workspace: &std::path::Path) -> anyhow::Result<()> {
    // This coding gateway applies Grok's existing workspace profile before it
    // constructs a toolset or accepts tool execution. Sandbox application is
    // process-wide and irreversible by design.
    xai_grok_sandbox::set_configured_profile(ProfileName::Workspace.to_string());
    let mut sandbox = SandboxManager::new(ProfileName::Workspace, workspace);
    sandbox.apply(workspace)?;
    sandbox.install();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let workspace = std::fs::canonicalize(&args.workspace)
        .with_context(|| format!("invalid workspace: {}", args.workspace.display()))?;
    initialize_sandbox(&workspace)?;

    let session = match args.mcp_config {
        Some(config) => GatewaySession::with_native_mcp_config(&workspace, config).await?,
        None => GatewaySession::new(&workspace)?,
    };
    let downstream_count = session.downstream_server_count().await;
    let tool_count = session.toolset().tool_definitions().len();
    let server = GatewayServer::new(session);
    match args.transport {
        Transport::Stdio => {
            eprintln!(
                "xai-grok-mcp-server stdio workspace={} downstream_servers={} exposed_tools={}",
                workspace.display(),
                downstream_count,
                tool_count
            );
            server
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
        }
        Transport::Http => {
            let http = HttpGateway::bind(server, SocketAddr::new(args.host, args.port)).await?;
            eprintln!(
                "xai-grok-mcp-server HTTP bound={} workspace={} endpoint={} downstream_servers={} exposed_tools={}",
                http.address(),
                workspace.display(),
                http.endpoint(),
                downstream_count,
                tool_count
            );
            tokio::signal::ctrl_c().await?;
            http.shutdown().await?;
        }
    }
    Ok(())
}
