use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use xai_grok_tools::types::ToolInput;

/// Non-interactive, fail-closed gateway permission policy.
///
/// It classifies the parsed Grok `ToolInput` before dispatch. The shell's ACP
/// permission prompt is intentionally not imported: an `Ask` decision has no
/// safe resolution on an MCP stdio connection, so it is always a denial here.
#[derive(Debug, Clone)]
pub struct GatewayPermission {
    workspace: PathBuf,
    shell_policy: ShellPolicy,
    downstream_tools: Arc<RwLock<HashSet<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayPermissionDecision {
    Allow,
    Deny,
}

/// Shell policy is intentionally narrow by default. It admits useful local
/// inspection/Git commands without granting a general terminal escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellPolicy {
    #[default]
    SafeCommands,
    DenyAll,
}

impl GatewayPermission {
    pub fn coding_default(workspace: PathBuf) -> Self {
        Self::coding(workspace, ShellPolicy::SafeCommands)
    }

    pub fn coding(workspace: PathBuf, shell_policy: ShellPolicy) -> Self {
        Self {
            workspace,
            shell_policy,
            downstream_tools: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn allow_downstream_tool(&self, name: String) {
        self.downstream_tools
            .write()
            .expect("permission lock poisoned")
            .insert(name);
    }

    pub fn evaluate(&self, requested_name: &str, input: &ToolInput) -> GatewayPermissionDecision {
        let allowed = match input {
            ToolInput::ReadFile(read) => self.workspace_path(&read.path),
            ToolInput::ListDir(list) => self.workspace_path(&list.target_directory),
            ToolInput::Grep(grep) => grep
                .path
                .as_deref()
                .is_none_or(|path| self.workspace_path(path)),
            ToolInput::SearchReplace(edit) => self.workspace_path(&edit.file_path),
            ToolInput::Bash(bash) => self.shell_allowed(&bash.command),
            // Background output is read-only and can only observe commands
            // created by an admitted `run_terminal_cmd` in this toolset.
            ToolInput::TaskOutput(_) => true,
            // Dynamically registered MCP tools parse as Dynamic. Only tools
            // discovered from an explicitly configured downstream server are
            // admitted; arbitrary/unclassified dynamic calls fail closed.
            ToolInput::Dynamic(_) | ToolInput::MCPTool(_) => self
                .downstream_tools
                .read()
                .expect("permission lock poisoned")
                .contains(requested_name),
            _ => false,
        };
        allowed
            .then_some(GatewayPermissionDecision::Allow)
            .unwrap_or(GatewayPermissionDecision::Deny)
    }

    fn workspace_path(&self, candidate: &str) -> bool {
        let candidate = Path::new(candidate);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace.join(candidate)
        };
        // Lexical containment rejects `../` before the tool reaches the OS.
        // Existing sandbox enforcement remains the second, process-level layer.
        normalize_lexically(&resolved).starts_with(&self.workspace)
    }

    fn shell_allowed(&self, command: &str) -> bool {
        if self.shell_policy == ShellPolicy::DenyAll
            || command.contains([';', '|', '&', '>', '<', '`', '\n'])
        {
            return false;
        }
        let mut words = command.split_whitespace();
        match words.next() {
            Some("pwd") => words.next().is_none(),
            Some("ls") | Some("rg") => true,
            Some("git") => matches!(words.next(), Some("status" | "diff" | "log" | "show")),
            _ => false,
        }
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
