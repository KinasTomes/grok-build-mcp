use std::path::PathBuf;

use anyhow::Context;
use rmcp::ServiceExt;
use xai_grok_mcp_server::{GatewayServer, GatewaySession};
use xai_grok_sandbox::{ProfileName, SandboxManager};

fn parse_workspace() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (Some(flag), Some(path)) if flag == "--workspace" => Ok(PathBuf::from(path)),
        _ => anyhow::bail!("usage: xai-grok-mcp-server --workspace <path>"),
    }
}

fn initialize_sandbox(workspace: &std::path::Path) -> anyhow::Result<()> {
    // This read-only server applies the existing read-only profile before it
    // constructs a toolset or accepts tool execution. Sandbox application is
    // process-wide and irreversible by design.
    xai_grok_sandbox::set_configured_profile(ProfileName::ReadOnly.to_string());
    let mut sandbox = SandboxManager::new(ProfileName::ReadOnly, workspace);
    sandbox.apply(workspace)?;
    sandbox.install();
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = parse_workspace()?;
    let workspace = std::fs::canonicalize(&workspace)
        .with_context(|| format!("invalid workspace: {}", workspace.display()))?;
    initialize_sandbox(&workspace)?;

    let server = GatewayServer::new(GatewaySession::new(&workspace)?);
    server
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
