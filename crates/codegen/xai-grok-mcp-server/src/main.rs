use std::path::PathBuf;

use anyhow::Context;
use rmcp::ServiceExt;
use xai_grok_mcp_server::{GatewayServer, GatewaySession};
use xai_grok_sandbox::{ProfileName, SandboxManager};

fn parse_args() -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    let mut args = std::env::args_os().skip(1);
    let (Some(flag), Some(workspace)) = (args.next(), args.next()) else {
        anyhow::bail!(
            "usage: xai-grok-mcp-server --workspace <path> [--mcp-config <native-grok-config.toml>]"
        );
    };
    if flag != "--workspace" {
        anyhow::bail!(
            "usage: xai-grok-mcp-server --workspace <path> [--mcp-config <native-grok-config.toml>]"
        );
    }
    let mcp_config = match (args.next(), args.next()) {
        (None, None) => None,
        (Some(flag), Some(path)) if flag == "--mcp-config" => Some(PathBuf::from(path)),
        _ => anyhow::bail!(
            "usage: xai-grok-mcp-server --workspace <path> [--mcp-config <native-grok-config.toml>]"
        ),
    };
    Ok((PathBuf::from(workspace), mcp_config))
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
    let (workspace, mcp_config) = parse_args()?;
    let workspace = std::fs::canonicalize(&workspace)
        .with_context(|| format!("invalid workspace: {}", workspace.display()))?;
    initialize_sandbox(&workspace)?;

    let session = match mcp_config {
        Some(config) => GatewaySession::with_native_mcp_config(&workspace, config).await?,
        None => GatewaySession::new(&workspace)?,
    };
    let server = GatewayServer::new(session);
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
