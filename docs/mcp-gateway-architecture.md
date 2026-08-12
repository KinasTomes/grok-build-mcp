# MCP gateway architecture (Phase 1)

## Conclusion

The Grok Build tool runtime can be exposed without starting Grok's LLM, prompt, or agent loop.  The narrow reusable boundary is an `Arc<FinalizedToolset>` from `xai-grok-tools`: it lists configured tool definitions, invokes a tool directly, retains per-session resources, and accepts dynamically registered downstream MCP tools.

A standalone `xai-grok-mcp-server` crate is therefore viable.  It should own a gateway session (workspace path, toolset, terminal backend, downstream MCP state, permission policy, and lifecycle), then adapt that session to MCP `tools/list` and `tools/call`.  It must not use `AcpSession` or the sampler/agent turn loop.

There are two important caveats:

1. Permission prompting and plan-mode gating live in the shell agent's tool-call loop, **outside** `FinalizedToolset::call`. A gateway that calls the toolset directly must install an explicit non-agent permission policy/approval mechanism before dispatch; otherwise it bypasses those interactive checks.
2. Process sandboxing is initialized once for the whole process. The gateway must apply the selected `xai-grok-sandbox` profile before it starts tool execution; it is not a per-toolset feature.

## Built-in tool registration and execution

### Registration

`xai-grok-tools::registry::types::ToolRegistryBuilder::new()` is the built-in catalog. It registers Grok Build, Codex-compatible, OpenCode-compatible, memory, search, and other tools in its constructor, then invokes process-global tool packs registered via `register_tool_pack`. Each built-in is registered under a fully qualified ID such as `GrokBuild:read_file`.

The common implementation contract is `xai_tool_runtime::Tool`:

- `Args` is deserializable JSON and `schemars::JsonSchema`.
- `Output` is serializable and implements the runtime output contract.
- `id()`, `description()`, optional capabilities/listing predicate, and `run()` or streaming `execute()` define a tool.
- Grok-specific metadata (`ToolMetadata`) adds namespace, kind, requirements, and templated description metadata.

At registration the builder type-erases each tool into a `ToolEntry`. It retains the argument JSON schema (generated with `schemars`, root title/description removed), metadata, JSON input parser, output converter, configuration-param handlers, and local-runtime registration closure. `ToolServerConfig` selects the enabled entries, may rename tools or parameters, and supplies parameter/version/description overrides.

`ToolRegistryBuilder::finalize[_with_trunc_config]` validates that config and produces `FinalizedToolset`. It builds tool definitions (name, description, JSON Schema) after applying configuration, registers enabled tools into `xai_computer_hub_sdk::LocalRegistry`, and creates the resource container used by implementations. The result is session-scoped; it is not merely a static registry.

### Direct execution path

The direct path—independent of a model—is:

```text
MCP tools/call adapter
  -> FinalizedToolset::call(name, JSON args, call_id, cwd_override)
  -> call_streaming_with_cancellation / call_raw
  -> resolve client-facing name and reverse parameter aliases
  -> LocalRegistry::find(ToolId)
  -> erased Tool::execute(ToolCallContext, JSON args)
  -> progress frames + terminal output
  -> typed output conversion, reminders, resource-state persistence
  -> ToolRunResult
```

The object-safe interface below concrete tool implementations is `xai_tool_runtime::ToolDispatch`; it accepts a `ToolId`, JSON arguments, and `ToolCallContext`, and returns a stream with zero or more progress frames followed by exactly one terminal result. `FinalizedToolset` also supplies its own inherent `call`/streaming methods for client-facing names. Those methods are the best gateway adapter target because they preserve name remapping, output conversion, reminders, and persistence.

The shell's normal agent path is different only before execution: `SessionActor` parses a model tool call, performs hooks/plan checks/permission resolution, then calls `WorkspaceOps::call_tool`; local workspace mode finally invokes this same `FinalizedToolset::call` path. The sampler is not required for the last portion.

## Session and workspace dependencies

`FinalizedToolset` is created with `SessionContext`, not a bare cwd. The minimum useful gateway context needs:

- `cwd`: workspace root/default relative-path base, also available in each call context.
- `AsyncFileSystem`: normally `LocalFs` for reads/writes/search-replace.
- `TerminalBackend`: normally one session-lifetime `LocalTerminalBackend`, required for bash/background-task/kill/output tools.
- `session_folder`, `state_path`, and an owner session id: logs/artifacts, persisted resource state, and process ownership.
- `session_env`: environment injected into shell execution.
- `ToolNotificationHandle`: a no-op handle is acceptable initially; an MCP gateway may later map progress/background notifications to MCP notifications.

Optional resources select feature-dependent tools: LSP backend, skills, memory, web/API clients, image/video clients, auth provider, scheduler/subagent state, etc. The first gateway should expose only an explicitly selected coding toolset and build the smallest valid `SessionContext`; it should not accidentally advertise tools whose optional runtime resource was omitted.

`xai-grok-workspace::SessionContextFactory` and `WorkspaceSessionContextFactory` already provide a reusable production-shaped constructor for this context and session-lifetime terminal backend. `WorkspaceOps` is not necessary for a standalone server: its local call path simply locates a workspace session and delegates to `session.toolset().call(...)`. A new crate can construct and retain the `FinalizedToolset` itself, or use a `WorkspaceHandle` only if it needs workspace services such as checkpoints, hot toolset swaps, or hub integration.

## Permissions and sandboxing

### Permission policy

The permission system is `xai-grok-workspace::permission::PermissionHandle`. It classifies `AccessKind` values (read/edit/bash/grep/MCP/web), applies managed and project policy, stored grants, safe-command logic, optional auto classification, and can issue interactive ACP prompts through its prompter.

However, `FinalizedToolset` and individual local tools do not invoke `PermissionHandle`. In the CLI flow, `xai-grok-shell/src/session/acp_session_impl/tool_calls.rs` derives `AccessKind` from parsed `ToolInput`, runs plan-mode and hook checks, requests permission, and only then dispatches. Thus an MCP gateway must choose and implement one of these explicit policies:

- non-interactive fail-closed (recommended initial default): allow only preconfigured/managed policy and return an MCP tool error for `Ask`;
- non-interactive allow-all, only behind an explicit dangerous CLI option; or
- a gateway-specific approval channel that can turn an `Ask` into a user-visible decision.

The existing shell `AcpPrompter` depends on an ACP client session and is not suitable by itself for ChatGPT Web MCP. Reusing permission *evaluation* is possible, but the prompt transport needs a new gateway adapter.

Plan-mode restrictions and pre/post tool hooks are likewise shell-agent orchestration rather than runtime enforcement. They are out of scope for an initial standalone gateway unless intentionally reintroduced as gateway middleware.

### OS sandbox

`xai-grok-sandbox::SandboxManager` applies an irreversible process-wide profile at startup. It restricts in-process filesystem access (where supported) and coordinates child-process network restrictions; local filesystem errors also log sandbox violations. The gateway must configure/apply/install it before creating its session/toolset. The active profile—not `FinalizedToolset`—enforces filesystem/process boundaries, so direct dispatch continues to use the same protections once correctly initialized.

## Downstream MCP discovery and execution

### Discovery/startup

The shell merges MCP server definitions from Grok TOML, plugins, Claude/Cursor compatibility files, project `.mcp.json`, and client-supplied entries in `session/managed_mcp.rs`. It applies folder-trust filtering, configured disables, and managed allow/deny policy before spawning. This merge helper is shell-private today, so a standalone crate cannot reuse it without extracting/publicizing a focused configuration API.

`xai-grok-mcp::servers` is the reusable MCP client lifecycle layer. It supports stdio and HTTP transports, initialization, OAuth/credential handling, timeouts, retries/recovery, liveness, and `tools/list`. `McpClient::get_tool_registrations` initializes a server, pages through `tools/list`, normalizes each schema to an object schema, and creates an `McpToolRegistration` with a qualified name:

```text
<server name>__<downstream tool name>
```

Invalid/ambiguous names are rejected. Dynamic registration is supported after startup: `FinalizedToolset::register_tool(name, tool, input_schema_override)` adds a runtime tool behind its `RwLock` and into the shared local registry. The shell uses this through its `ToolBridge` when each `McpToolRegistration` is model-visible. This means built-ins and downstream MCP tools converge on the same finalized dispatch surface, although they originate differently: built-ins are finalized from `ToolRegistryBuilder`; MCP tools are appended dynamically.

### Invocation

Each `McpErasedTool` implements the same `xai_tool_runtime::Tool` trait with `Args = serde_json::Value`. On execution it looks up its server in shared `McpState`, calls `ensure_initialized`, sends downstream `tools/call`, applies configured timeout/recovery/auth retry behavior, and translates response content into Grok `ToolOutput::MCP`. Consequently, after registration the outer gateway can invoke a downstream tool through `FinalizedToolset::call` exactly as it invokes a built-in.

Tool list changes are already anticipated: `FinalizedToolset` uses an `RwLock` specifically for rare dynamic MCP registration/removal, and downstream MCP clients observe `tools/list_changed`. A gateway should refresh its server registrations and emit MCP `notifications/tools/list_changed` (or require the client to refresh) whenever the outward catalog changes.

Downstream MCP calls currently get permission classification only when the shell agent loop runs (`AccessKind::MCPTool`). Direct gateway calls therefore need the same middleware noted above, including policy treatment for qualified downstream names.

## Server-side protocol support

The repository has mature MCP **client** support in `xai-grok-mcp`; its `rmcp` dependency is deliberately quarantined there. Its enabled features include client and client transports, not an implemented outbound server. `xai-computer-hub-mcp-adapter` is also an inbound/downstream bridge (MCP server -> Computer Hub), not an MCP server exposing Grok tools.

There is no reusable Grok MCP-server host/adapter found in the current tree. A new crate must implement the server transport and `initialize`, `tools/list`, and `tools/call` protocol handlers (likely by enabling the appropriate `rmcp` server/stdio or streamable-HTTP features in that crate, or by using another MCP server library). It can still reuse the existing `xai-grok-mcp` client code for downstream aggregation.

## Smallest integration point and proposed crate boundary

Create `crates/codegen/xai-grok-mcp-server` as an executable that depends on the runtime/tool/workspace/MCP client crates but not `xai-grok-agent` or `xai-grok-shell`'s `AcpSession`/sampler loop.

```text
ChatGPT Web MCP client
  -> xai-grok-mcp-server protocol handler
  -> GatewaySession middleware
       - workspace/session context + FinalizedToolset
       - gateway permission policy
       - downstream McpState/McpClient lifecycle
  -> FinalizedToolset::definitions / ::call
       -> LocalRegistry built-in implementations
       -> dynamically registered McpErasedTool implementations
```

Recommended responsibilities:

- `context`: construct `SessionContext`, one session-lifetime terminal backend, state/session directories, sandbox setup, and selected `ToolServerConfig`.
- `adapter`: translate finalized definitions to MCP definitions; preserve tool name, description, and JSON Schema; call `FinalizedToolset::call_streaming`/`call`; translate terminal output/errors to MCP result content.
- `downstream`: load/start permitted configured MCP servers, retrieve `McpToolRegistration`s, and register/unregister them in the same `FinalizedToolset`.
- `policy`: convert/parse the resolved internal `ToolInput` to `AccessKind` and resolve an explicit gateway approval policy before the adapter dispatches.
- `server`: own stdio/HTTP MCP transport and notify clients when dynamic tools change.

The preferred initial public integration API is a small extraction from shell-private MCP merge/startup code, not a dependency on the shell crate. It should accept `cwd`, configuration sources, trust/policy inputs, and session-owned `McpState`, then return permitted server definitions/clients or registrations. Until that extraction exists, the new crate can use the public `xai-grok-mcp::servers` lifecycle directly with a deliberately narrower configuration source (for example Grok TOML only); duplicating the shell's compatibility merge logic would be a poor long-term boundary.

## Phase 2 implementation constraints

- Do not enumerate `ToolRegistryBuilder::new()` blindly: finalize an explicit coding-tool configuration so unsupported API-backed or agent-only tools are not advertised.
- Reuse `FinalizedToolset` dispatch rather than calling filesystem or shell APIs directly.
- Generate initial `tools/list` from finalized definitions and use its client-facing names; downstream names must remain qualified to avoid collisions.
- Start downstream MCP before first list, then dynamically update registrations after a successful refresh.
- Make approval behavior explicit in CLI/config and default to fail-closed for unresolved prompts.
- Apply the sandbox before executing any tool, and tear down the session terminal backend and MCP clients on server shutdown.

## Phase 3 implementation

`xai-grok-mcp-server` now finalizes this explicit local coding set: `read_file`,
`list_dir`, `grep`, `search_replace`, `run_terminal_cmd`,
`get_terminal_command_output`, and `kill_task`. `search_replace` supplies both
workspace edits and file creation. `kill_task` is present only because Grok's
background-command runtime requires it; the gateway policy denies it directly.
No API-backed, image/video, web, memory, agent/subagent, scheduler, or UI tools
are finalized.

The executable applies the existing `Workspace` sandbox profile before it
constructs the session. This keeps filesystem writes limited to the workspace
while allowing the explicitly selected local editing tools. Gateway permission
middleware classifies parsed `ToolInput`: workspace-contained reads/searches and
edits are allowed; paths escaping with `..` are denied; shell is a configurable,
default-safe allowlist (`pwd`, `ls`, `rg`, and read-only Git inspection); direct
task-kill and all unknown inputs are denied. Only a dynamically registered tool
that was discovered from the configured native MCP file is allowed downstream.
There is no ACP prompting path, so an unresolved/Ask outcome is fail-closed.

### Native downstream configuration

The public leaf API `xai_grok_config_types::native_mcp_servers_from_toml`
extracts the narrow native `[mcp_servers.<name>]` parser from shell-private
configuration handling. It resolves enabled stdio/HTTP/SSE entries, simple
environment substitutions, and OAuth metadata; malformed, disabled, and
setup-incomplete entries are skipped independently. The gateway currently takes
an explicit `--mcp-config <file>` containing this native format. It starts these
servers before serving stdio, asks each `McpClient` for registrations, retains
the shared `McpState`, and appends every `McpErasedTool` via
`FinalizedToolset::register_tool`. Qualified `<server>__<tool>` names and source
schemas/descriptions are retained without a gateway copy.

Unsupported in this phase: automatic global/project TOML merge, `.mcp.json`,
Claude/Cursor compatibility imports, plugins/managed client-supplied entries,
setup preferences, and runtime `tools/list_changed` refresh/outer notification.
Those need a follow-up extraction that also carries folder trust and managed
policy semantics rather than copying shell merge code.
