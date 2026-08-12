/// A deliberately non-interactive gateway policy.
///
/// It checks Grok's parsed `ToolInput`, but deliberately does not depend on
/// the workspace shell's ACP prompt transport. Any future `Ask` decision must
/// therefore fail closed until the gateway gains its own approval transport.
#[derive(Debug, Clone)]
pub struct GatewayPermission {
    allowed_read_tool: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayPermissionDecision {
    Allow,
    Deny,
}

impl GatewayPermission {
    pub fn read_only(tool_name: impl Into<String>) -> Self {
        Self {
            allowed_read_tool: tool_name.into(),
        }
    }

    pub fn evaluate(
        &self,
        requested_name: &str,
        input: &xai_grok_tools::types::ToolInput,
    ) -> GatewayPermissionDecision {
        if requested_name == self.allowed_read_tool
            && matches!(input, xai_grok_tools::types::ToolInput::ReadFile(_))
        {
            GatewayPermissionDecision::Allow
        } else {
            GatewayPermissionDecision::Deny
        }
    }
}
