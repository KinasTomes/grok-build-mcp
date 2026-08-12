//! Standalone MCP gateway for a deliberately small Grok Build tool runtime.
//!
//! This first vertical slice exposes only the read-only `read_file` tool. It
//! deliberately owns an [`Arc<FinalizedToolset>`] directly rather than an
//! agent, sampler, ACP session, or prompt loop.

mod policy;
mod server;

pub use policy::{GatewayPermission, GatewayPermissionDecision};
pub use server::GatewayServer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use xai_grok_tools::registry::types::{
    FinalizedToolset, SessionContext, ToolConfig, ToolRegistryBuilder, ToolServerConfig,
};

/// Client-facing name of the sole tool exposed by this vertical slice.
pub const READ_FILE_TOOL: &str = "read_file";

static NEXT_GATEWAY_SESSION: AtomicU64 = AtomicU64::new(1);

/// Long-lived runtime state for one gateway process/workspace.
///
/// The terminal backend is retained by the finalized toolset's resources. The
/// factory creates it once here, before finalization, so future toolset swaps
/// can preserve the same session-lifetime backend.
pub struct GatewaySession {
    workspace: PathBuf,
    toolset: Arc<FinalizedToolset>,
    permission: GatewayPermission,
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
        let context = SessionContext {
            backend: Arc::new(xai_grok_tools::computer::local::LocalTerminalBackend::new()),
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
            tools: vec![ToolConfig::for_tool::<
                xai_grok_tools::implementations::grok_build::ReadFileTool,
            >()],
            behavior_preset: None,
        };
        let toolset = ToolRegistryBuilder::new()
            .finalize(config, context)
            .map_err(|errors| {
                anyhow::anyhow!(
                    "failed to finalize the read-only gateway toolset: {}",
                    errors
                        .iter()
                        .map(|error| error.summary())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?;

        Ok(Self {
            workspace,
            toolset: Arc::new(toolset),
            permission: GatewayPermission::read_only(READ_FILE_TOOL),
        })
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
}
