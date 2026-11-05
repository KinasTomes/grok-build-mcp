use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use xai_grok_tools::types::ToolInput;

/// Gateway permission policy over parsed Grok tool input.
///
/// It classifies the parsed Grok `ToolInput` before dispatch. The shell's ACP
/// permission prompt is intentionally not imported. A host may resolve `Ask`
/// through its own local approval broker; without one the server denies it.
#[derive(Debug, Clone)]
pub struct GatewayPermission {
    workspace: PathBuf,
    allowed_paths: Vec<PathBuf>,
    shell_policy: ShellPolicy,
    always_approve: bool,
    downstream_tools: Arc<RwLock<HashSet<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayPermissionDecision {
    Allow,
    Ask,
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
            allowed_paths: Vec::new(),
            shell_policy,
            always_approve: false,
            downstream_tools: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn with_allowed_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.allowed_paths = paths
            .into_iter()
            .map(|p| normalize_lexically(&p.canonicalize().unwrap_or(p)))
            .collect();
        self
    }

    pub fn with_always_approve(mut self) -> Self {
        self.always_approve = true;
        self
    }

    pub fn replace_downstream_tools(&self, names: HashSet<String>) {
        self.downstream_tools
            .write()
            .expect("permission lock poisoned")
            .clone_from(&names);
    }

    pub fn evaluate(&self, requested_name: &str, input: &ToolInput) -> GatewayPermissionDecision {
        let decision = match input {
            ToolInput::ReadFile(read) => self.path_decision(&read.path),
            ToolInput::ListDir(list) => self.path_decision(&list.target_directory),
            ToolInput::Grep(grep) => grep
                .path
                .as_deref()
                .map_or(GatewayPermissionDecision::Allow, |path| {
                    self.path_decision(path)
                }),
            ToolInput::SearchReplace(edit) => self.path_decision(&edit.file_path),
            ToolInput::Bash(bash) => self.shell_decision(&bash.command),
            // Background output is read-only and can only observe commands
            // created by an admitted `run_terminal_cmd` in this toolset.
            ToolInput::TaskOutput(_) => GatewayPermissionDecision::Allow,
            // Dynamically registered MCP tools parse as Dynamic. Only tools
            // discovered from an explicitly configured downstream server are
            // admitted; arbitrary/unclassified dynamic calls fail closed.
            ToolInput::Dynamic(_) | ToolInput::MCPTool(_)
                if self
                    .downstream_tools
                    .read()
                    .expect("permission lock poisoned")
                    .contains(requested_name) =>
            {
                GatewayPermissionDecision::Allow
            }
            _ => GatewayPermissionDecision::Deny,
        };
        if self.always_approve && decision == GatewayPermissionDecision::Ask {
            GatewayPermissionDecision::Allow
        } else {
            decision
        }
    }

    fn path_decision(&self, candidate: &str) -> GatewayPermissionDecision {
        self.workspace_path(candidate)
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
        let normalized = normalize_lexically(&resolved);
        normalized.starts_with(&self.workspace)
            || self
                .allowed_paths
                .iter()
                .any(|allowed| normalized.starts_with(allowed))
            || is_git_worktree_of(&normalized, &self.workspace)
    }

    fn shell_decision(&self, command: &str) -> GatewayPermissionDecision {
        if self.shell_policy == ShellPolicy::DenyAll || destructive_command(command) {
            return GatewayPermissionDecision::Deny;
        }
        if !self.always_approve && command.contains([';', '|', '&', '>', '<', '`', '\n']) {
            return GatewayPermissionDecision::Deny;
        }
        let mut words = command.split_whitespace();
        let safe = match words.next() {
            Some("pwd") => words.next().is_none(),
            Some("ls") | Some("rg") => true,
            Some("git") => matches!(words.next(), Some("status" | "diff" | "log" | "show")),
            _ => false,
        };
        if safe {
            GatewayPermissionDecision::Allow
        } else {
            GatewayPermissionDecision::Ask
        }
    }
}

fn destructive_command(command: &str) -> bool {
    let words: Vec<_> = command.split_whitespace().collect();
    match words.as_slice() {
        ["rm", ..]
        | ["sudo", ..]
        | ["dd", ..]
        | ["mkfs", ..]
        | ["shutdown", ..]
        | ["reboot", ..]
        | ["kill", ..]
        | ["pkill", ..]
        | ["chmod", ..]
        | ["chown", ..]
        | ["curl", ..]
        | ["wget", ..] => true,
        ["git", "reset", ..] | ["git", "clean", ..] | ["git", "checkout", ..] => true,
        _ => false,
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

// shortcut: only inspects .git file pointers of main repo workspace; upgrade if server starts inside a worktree
fn is_git_worktree_of(path: &Path, workspace: &Path) -> bool {
    let target_git = workspace.join(".git");
    let target_git = target_git.canonicalize().unwrap_or(target_git);
    for dir in path.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_file() {
            if let Ok(content) = std::fs::read_to_string(&dot_git) {
                if let Some(gitdir) = content.trim().strip_prefix("gitdir:") {
                    let gitdir_path = Path::new(gitdir.trim());
                    let resolved = if gitdir_path.is_absolute() {
                        gitdir_path.to_path_buf()
                    } else {
                        dir.join(gitdir_path)
                    };
                    let resolved = resolved.canonicalize().unwrap_or(resolved);
                    return resolved.starts_with(&target_git);
                }
            }
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_git_worktree_outside_workspace_is_allowed() {
        let ws = TempDir::new().unwrap();
        let main_git = ws.path().join(".git");
        std::fs::create_dir_all(main_git.join("worktrees/wt1")).unwrap();

        let wt = TempDir::new().unwrap();
        let wt_file = wt.path().join("sub/file.rs");
        std::fs::create_dir_all(wt.path().join("sub")).unwrap();
        std::fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", main_git.join("worktrees/wt1").display()),
        )
        .unwrap();

        let policy = GatewayPermission::coding_default(ws.path().to_path_buf());
        assert!(policy.workspace_path(wt_file.to_str().unwrap()));
        assert!(!policy.workspace_path("/unrelated/outside/path"));

        let extra_dir = TempDir::new().unwrap();
        let extra_file = extra_dir.path().join("state.json");
        let policy_with_extra = policy.with_allowed_paths(vec![extra_dir.path().to_path_buf()]);
        assert!(policy_with_extra.workspace_path(extra_file.to_str().unwrap()));
    }
}
