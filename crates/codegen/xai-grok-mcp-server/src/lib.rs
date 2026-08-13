//! Standalone MCP gateway for a deliberately small Grok Build tool runtime.
//!
//! It deliberately owns an [`Arc<FinalizedToolset>`] directly rather than an
//! agent, sampler, ACP session, or prompt loop.

mod approval;
mod events;
mod policy;
mod server;
mod transport;

pub use policy::{GatewayPermission, GatewayPermissionDecision, ShellPolicy};
pub use server::GatewayServer;
pub use transport::{HttpGateway, MCP_HTTP_ENDPOINT};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use xai_grok_tools::registry::types::{
    FinalizedToolset, SessionContext, ToolConfig, ToolRegistryBuilder, ToolServerConfig,
};

/// Client-facing name of the primary workspace read tool.
pub const READ_FILE_TOOL: &str = "read_file";

static NEXT_GATEWAY_SESSION: AtomicU64 = AtomicU64::new(1);

/// Long-lived runtime state for one gateway process/workspace.
///
/// The terminal backend is retained by the finalized toolset's resources.
pub struct GatewaySession {
    workspace: PathBuf,
    toolset: Arc<FinalizedToolset>,
    permission: GatewayPermission,
    events: GatewayEventBus,
    approval_broker: Option<Arc<dyn ApprovalBroker>>,
    approval_timeout: std::time::Duration,
    terminal_backend: Arc<xai_grok_tools::computer::local::LocalTerminalBackend>,
    /// Retains downstream clients for as long as dynamic `McpErasedTool`s can run.
    _mcp_state: Option<Arc<Mutex<xai_grok_mcp::servers::McpState>>>,
}

impl GatewaySession {
    /// Build the no-auth, production-shaped runtime for `workspace`.
    pub fn new(workspace: impl AsRef<Path>) -> anyhow::Result<Self> {
        let workspace = std::fs::canonicalize(workspace.as_ref())?;
        let session_id = format!(
            "mcp-gateway-{}",
            NEXT_GATEWAY_SESSION.fetch_add(1, Ordering::Relaxed)
        );
        let session_folder = std::env::temp_dir()
            .join("xai-grok-mcp-server")
            .join(std::process::id().to_string())
            .join(&session_id);
        std::fs::create_dir_all(&session_folder)?;

        // This is the same SessionContext boundary used by workspace sessions,
        // constructed directly so this executable has no dependency on the
        // workspace's agent/sampler orchestration crate. It deliberately has
        // no auth provider, API key provider, or enabled API-backed clients.
        let terminal_backend =
            Arc::new(xai_grok_tools::computer::local::LocalTerminalBackend::new());
        let context = SessionContext {
            backend: terminal_backend.clone(),
            fs: Arc::new(xai_grok_tools::computer::local::LocalFs),
            cwd: workspace.clone(),
            session_folder: session_folder.clone(),
            session_env: Arc::new(HashMap::new()),
            notification_handle: xai_grok_tools::notification::ToolNotificationHandle::noop(),
            owner_session_id: Some(session_id),
            subagent: None,
            parent_scheduler_handle: None,
            skills: Vec::new(),
            state_path: session_folder.join("tool_state.json"),
            memory_backend: None,
            web_search_config: Default::default(),
            web_fetch_config: Default::default(),
            lsp: None,
            image_gen_config: Default::default(),
            video_gen_config: Default::default(),
            app_builder_deployer_config: Default::default(),
            api_key_provider: None,
            auth_provider: None,
            attribution_callback: None,
            system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
        };
        let config = ToolServerConfig {
            // Deliberately local coding tools only. `search_replace` also
            // creates new files when its old string is empty.
            tools: vec![
                ToolConfig::for_tool::<xai_grok_tools::implementations::grok_build::ReadFileTool>(),
                ToolConfig::for_tool::<xai_grok_tools::implementations::grok_build::ListDirTool>(),
                ToolConfig::for_tool::<xai_grok_tools::implementations::grok_build::GrepTool>(),
                ToolConfig::for_tool::<
                    xai_grok_tools::implementations::grok_build::SearchReplaceTool,
                >(),
                ToolConfig::for_tool::<xai_grok_tools::implementations::grok_build::BashTool>(),
                ToolConfig::for_tool::<
                    xai_grok_tools::implementations::grok_build::GetTerminalCommandOutputTool,
                >(),
                // Required by Grok's background-command invariant. The
                // gateway policy intentionally denies direct kill requests.
                ToolConfig::for_tool::<xai_grok_tools::implementations::grok_build::KillTaskTool>(),
            ],
            behavior_preset: None,
        };
        let toolset = ToolRegistryBuilder::new()
            .finalize(config, context)
            .map_err(|errors| {
                anyhow::anyhow!(
                    "failed to finalize the coding gateway toolset: {}",
                    errors
                        .iter()
                        .map(|error| error.summary())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?;

        Ok(Self {
            workspace: workspace.clone(),
            toolset: Arc::new(toolset),
            permission: GatewayPermission::coding_default(workspace.clone()),
            events: GatewayEventBus::default(),
            approval_broker: None,
            approval_timeout: std::time::Duration::from_secs(60),
            terminal_backend,
            _mcp_state: None,
        })
    }

    /// Start native-configured MCP servers and register their discovered tools
    /// into this session's existing finalized toolset.
    pub async fn with_native_mcp_config(
        workspace: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
    ) -> anyhow::Result<Self> {
        let mut session = Self::new(workspace)?;
        let config_text = std::fs::read_to_string(config_path.as_ref())?;
        let root: toml::Value = toml::from_str(&config_text)?;
        let native = xai_grok_config_types::native_mcp_servers_from_toml(&root);
        session.register_downstream(native).await?;
        Ok(session)
    }

    async fn register_downstream(
        &mut self,
        native: xai_grok_config_types::NativeMcpServers,
    ) -> anyhow::Result<()> {
        if native.servers.is_empty() {
            return Ok(());
        }
        let state = Arc::new(Mutex::new(xai_grok_mcp::servers::McpState::new(
            native.servers.clone(),
        )));
        let event_writer = xai_file_utils::events::EventWriter::noop();
        let ctx = xai_grok_mcp::servers::McpSpawnCtx::session_less(&event_writer);
        let started = xai_grok_mcp::servers::start_mcp_servers(
            native.servers,
            &Default::default(),
            &Default::default(),
            &native.oauth,
            &ctx,
        )
        .await;
        for client in started {
            let client = Arc::new(client.map_err(|error| anyhow::anyhow!(error.to_string()))?);
            let server_name = client.server_name().to_string();
            state
                .lock()
                .await
                .owned_clients
                .insert(server_name, Arc::clone(&client));
            for registration in client.get_tool_registrations(Arc::clone(&state)).await? {
                self.permission
                    .allow_downstream_tool(registration.name.clone());
                self.toolset.register_tool(
                    registration.name,
                    registration.tool,
                    Some(registration.input_schema),
                )?;
            }
        }
        self._mcp_state = Some(state);
        Ok(())
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn toolset(&self) -> &Arc<FinalizedToolset> {
        &self.toolset
    }

    pub fn permission(&self) -> &GatewayPermission {
        &self.permission
    }

    pub fn event_bus(&self) -> &GatewayEventBus {
        &self.events
    }

    /// Attach a local approval consumer. Without one, Ask remains deny.
    pub fn with_approval_broker(mut self, broker: Arc<dyn ApprovalBroker>) -> Self {
        self.approval_broker = Some(broker);
        self
    }

    pub fn with_approval_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.approval_timeout = timeout;
        self
    }

    pub(crate) async fn request_approval(
        &self,
        request: ApprovalRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ApprovalDecision {
        let Some(broker) = &self.approval_broker else {
            return ApprovalDecision::Deny;
        };
        tokio::select! {
            _ = cancellation.cancelled() => ApprovalDecision::Deny,
            result = tokio::time::timeout(self.approval_timeout, broker.request(request)) => result.unwrap_or(ApprovalDecision::Deny),
        }
    }

    /// Number of configured downstream MCP servers that completed startup.
    pub async fn downstream_server_count(&self) -> usize {
        match &self._mcp_state {
            Some(state) => state.lock().await.owned_clients.len(),
            None => 0,
        }
    }

    /// Stop session-owned terminal work and request downstream MCP transport shutdown.
    pub async fn shutdown(&self) {
        self.terminal_backend.cancel();
        if let Some(state) = &self._mcp_state {
            let clients: Vec<_> = state.lock().await.owned_clients.values().cloned().collect();
            for client in clients {
                client.shutdown().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_approval_wait_fails_closed_before_a_late_response() {
        let workspace = tempfile::TempDir::new().unwrap();
        let (broker, mut pending) = ChannelApprovalBroker::new(1);
        let session = GatewaySession::new(workspace.path())
            .unwrap()
            .with_approval_broker(broker)
            .with_approval_timeout(std::time::Duration::from_secs(1));
        let cancellation = tokio_util::sync::CancellationToken::new();
        let wait = session.request_approval(
            ApprovalRequest {
                call_id: "call".into(),
                tool_name: "run_terminal_cmd".into(),
                summary: "cargo test".into(),
            },
            cancellation.clone(),
        );
        tokio::pin!(wait);
        let request = tokio::select! {
            request = pending.recv() => request.unwrap(),
            _ = &mut wait => panic!("approval resolved before a consumer responded"),
        };
        cancellation.cancel();
        assert_eq!(wait.as_mut().await, ApprovalDecision::Deny);
        request.resolve(ApprovalDecision::AllowOnce);
    }
}
pub use approval::{
    ApprovalBroker, ApprovalDecision, ApprovalRequest, ChannelApprovalBroker, PendingApproval,
    TerminalApprovalBroker,
};
pub use events::{GatewayEvent, GatewayEventBus};
