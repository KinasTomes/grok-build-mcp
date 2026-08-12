# Plan: Expose Grok Build Tool Runtime as an MCP Server for ChatGPT Web

## Goal

Add a new MCP server mode to Grok Build that exposes Grok Build's existing local coding tools and configured downstream MCP tools to an external MCP client such as ChatGPT Web.

The important architectural requirement is:

```text
ChatGPT Web
    ↓ MCP
Grok Build MCP Server
    ↓
Grok Tool Runtime / Tool Registry
    ↓
Local filesystem / shell / git / workspace / downstream MCP servers
```

Do **not** route requests through Grok's own LLM, model provider, agent reasoning loop, or prompt loop.

ChatGPT Web must remain the agent/model performing reasoning and deciding which tools to call.

Grok Build should only act as a local tool execution runtime and MCP gateway.

---

## Desired User Experience

Eventually I want to be able to run something similar to:

```bash
grok mcp-server --workspace ~/projects/my-project
```

or, if a separate binary is architecturally cleaner:

```bash
grok-mcp-server --workspace ~/projects/my-project
```

The server should expose an MCP endpoint that ChatGPT can connect to.

Once connected, ChatGPT should be able to:

```text
list/search files
read files
edit/apply patches
create files
run shell commands
run tests
inspect git status/diff
use any appropriate existing Grok coding tools
use tools provided by MCP servers configured in Grok
```

without invoking a Grok-hosted LLM.

---

# Phase 1 — Repository Architecture Investigation

Before making significant changes, inspect the repository and document the relevant execution path.

Focus especially on these existing areas:

```text
xai-grok-tools
xai-grok-workspace
xai-grok-shell
tool registry
tool runtime
managed MCP
MCP extensions
session tool configuration
sandbox / approvals / permissions
```

Known potentially relevant files/modules include:

```text
crates/codegen/xai-grok-tools/src/registry/
crates/codegen/xai-grok-shell/src/session/managed_mcp.rs
crates/codegen/xai-grok-shell/src/extensions/mcp.rs
crates/codegen/xai-grok-workspace/src/session/tool_config.rs
crates/common/xai-tool-runtime/
```

Do not assume these are necessarily the final integration points. Follow the actual current code.

Determine:

1. Where built-in tools are registered.
2. What common representation/interface/trait is used for a tool.
3. How tool schemas are represented.
4. How a tool invocation is executed.
5. What execution context a tool requires.
6. How workspace state is provided.
7. How shell execution is sandboxed or approved.
8. How configured MCP servers are started and managed.
9. How tools from downstream MCP servers are represented.
10. Whether built-in tools and MCP tools already share a common registry.
11. Whether MCP tools are registered dynamically after startup.
12. Whether there is already reusable MCP server-side protocol code in the repository/dependencies.

Before implementation, produce a short architecture note summarizing:

```text
tool registration path
tool execution path
MCP client path
workspace/session dependencies
permission/sandbox dependencies
best MCP-server integration point
```

Then proceed with implementation without waiting for additional approval unless a major blocker exists.

---

# Phase 2 — Choose the Smallest Integration Boundary

Prefer adding a new independent crate rather than deeply modifying the TUI or agent runtime.

Ideal shape:

```text
crates/codegen/xai-grok-mcp-server/
├── Cargo.toml
└── src/
    ├── main.rs
    ├── server.rs
    ├── adapter.rs
    ├── context.rs
    └── error.rs
```

The exact structure may differ if existing repository conventions suggest something better.

The new server should depend on existing Grok crates rather than duplicate their functionality.

Reuse:

```text
tool registry
tool implementations
workspace handling
MCP client/runtime
permission system
sandboxing
output handling
```

Do not reimplement tools such as filesystem read, shell execution, grep, patching, etc.

---

# Phase 3 — Implement MCP Tool Adapter

Create an adapter between Grok's internal tool representation and MCP.

Conceptually:

```text
Grok Tool
   ↓
MCP Tool Definition
```

For MCP `tools/list`, expose:

```text
name
description
inputSchema
```

using Grok's existing tool metadata/schema wherever possible.

For MCP `tools/call`:

```text
MCP request
    ↓
resolve Grok tool by name
    ↓
validate arguments
    ↓
construct required execution context
    ↓
execute through existing Grok runtime
    ↓
convert result to MCP response
```

Do not bypass existing execution abstractions by calling filesystem or shell APIs directly.

---

# Phase 4 — Built-in Coding Tools

Expose useful coding tools from the existing Grok registry.

Do not create a manually duplicated registry if the existing runtime can enumerate them.

The server should ideally automatically inherit newly added Grok tools in the future.

At minimum, verify that the resulting MCP tool set supports the equivalent of:

```text
read
search / grep
directory listing / glob
write/create
edit/apply patch
shell execution
git-related operations if provided
workspace inspection
```

The actual MCP names should preferably match existing Grok tool names unless namespacing is required.

---

# Phase 5 — Downstream MCP Aggregation

This is an important requirement.

If the user's Grok configuration contains other MCP servers, for example:

```text
GitHub MCP
browser MCP
database MCP
custom MCP servers
```

their tools should also become available through this MCP server where practical.

Desired architecture:

```text
ChatGPT
   ↓
Grok MCP Server
   ├── Grok built-in tools
   └── configured downstream MCP tools
            ├── GitHub MCP
            ├── Browser MCP
            └── ...
```

Reuse Grok's existing managed MCP subsystem.

Do not launch a completely separate MCP configuration system unless necessary.

If downstream MCP tools and built-in Grok tools already share a common tool abstraction, expose that shared registry directly.

If they do not, create a thin aggregation layer.

---

# Phase 6 — Tool Namespacing and Collisions

Handle duplicate tool names safely.

For example, two providers may both expose:

```text
search
read
query
```

Prefer a deterministic namespace strategy such as:

```text
grok.read
grok.shell
github.search
browser.search
postgres.query
```

However, avoid adding namespaces unnecessarily if the existing Grok MCP subsystem already guarantees globally unique names.

The mapping must remain stable between server restarts.

---

# Phase 7 — Workspace Isolation

The MCP server must operate within an explicitly selected workspace.

Example:

```bash
grok mcp-server --workspace /home/user/projects/foo
```

All existing Grok workspace restrictions should remain active.

Do not accidentally give unrestricted filesystem access simply because the caller is MCP.

Where the existing Grok runtime already protects workspace boundaries, reuse it.

Ensure path traversal such as:

```text
../../
```

cannot bypass the workspace/sandbox rules.

---

# Phase 8 — Shell and Dangerous Operations

Do not weaken Grok's existing safety model.

The MCP server should reuse the current:

```text
permission system
approval system
sandbox
command execution policy
workspace restrictions
```

If the existing design expects interactive approval from the TUI, abstract that interaction rather than disabling the security layer.

For the initial MVP, it is acceptable to implement a configurable policy such as:

```text
deny
allow
require approval
```

for dangerous actions.

The default should not silently grant more permissions than the normal Grok CLI.

---

# Phase 9 — No Grok LLM Calls

This requirement must have tests or clear verification.

Running the MCP server must not initialize or call:

```text
Grok API
LLM provider
agent reasoning loop
prompt generation
model inference
```

unless some unrelated existing subsystem absolutely requires initialization.

A tool call such as:

```text
read_file
```

must conceptually be:

```text
ChatGPT
→ MCP
→ Grok tool implementation
→ filesystem
```

not:

```text
ChatGPT
→ MCP
→ Grok agent
→ Grok model
→ tool
```

The server should work even when no Grok API key is configured, provided the requested local tools themselves do not require one.

---

# Phase 10 — Transport

Use a transport compatible with modern MCP clients and ChatGPT.

Prefer the MCP transport already used or supported by Grok Build's dependencies if possible.

Primary target:

```text
Streamable HTTP
```

Optionally support stdio if inexpensive:

```bash
grok mcp-server --transport stdio
```

Example HTTP configuration:

```bash
grok mcp-server \
  --workspace ~/projects/foo \
  --host 127.0.0.1 \
  --port 8765
```

Do not expose externally by default.

Default binding should be localhost unless explicitly configured otherwise.

---

# Phase 11 — Dynamic MCP Tool Changes

Investigate how Grok handles MCP tool changes/reconnections.

If downstream MCP servers can dynamically add/remove tools, update the exposed MCP registry accordingly.

If MCP supports tool-list change notifications in the current stack, forward or generate them where appropriate.

For MVP, startup-time discovery is acceptable if dynamic updates would significantly increase complexity, but document the limitation.

---

# Phase 12 — Tool Result Conversion

Preserve useful structured information.

Handle:

```text
text output
structured JSON
errors
stdout
stderr
exit code
attachments/resources if applicable
```

Avoid flattening everything into an unreadable string when Grok already provides structured results.

Large tool responses should use Grok's existing truncation/output mechanisms if available.

---

# Phase 13 — Logging

Add useful logs for:

```text
server startup
workspace
registered built-in tools
connected downstream MCP servers
registered MCP tools
tool invocation
tool execution duration
tool failure
downstream MCP reconnect/failure
```

Do not log secrets, environment variables, tokens, or sensitive tool arguments unnecessarily.

---

# Phase 14 — CLI Integration

Add an ergonomic command.

Preferred:

```bash
grok mcp-server
```

Options should include where appropriate:

```text
--workspace
--host
--port
--transport
--config
```

Reuse Grok's existing config discovery behavior.

Running inside a project may default workspace to the current directory if that matches existing Grok CLI behavior.

---

# Phase 15 — Tests

Add tests covering at least:

## Tool discovery

Start server and verify:

```text
tools/list
```

contains expected Grok tools.

## Local tool execution

Call a harmless tool such as file read against a temporary workspace.

Verify the returned contents.

## Write/edit

Modify a temporary file and verify the result.

## Shell

Run a harmless command such as:

```bash
printf hello
```

and verify output.

## Workspace isolation

Attempt to access a file outside the workspace.

Verify it is rejected according to existing Grok policy.

## Downstream MCP

Create or reuse a small test MCP server exposing:

```text
echo
```

Configure it through Grok.

Verify:

```text
ChatGPT-side MCP
→ Grok MCP gateway
→ downstream MCP echo
```

works.

## No LLM dependency

Run MCP integration tests without a Grok API key.

Verify local tool discovery and execution still work.

---

# Phase 16 — Manual End-to-End Test

Provide instructions for running something similar to:

```bash
cargo run -p xai-grok-mcp-server -- \
  --workspace /tmp/test-project \
  --port 8765
```

Then use an MCP inspector/client to:

```text
initialize
tools/list
tools/call
```

Test this sequence:

```text
list project
read source file
modify source file
run tests
inspect resulting changes
```

Also test at least one configured downstream MCP tool.

---

# Phase 17 — Keep the Patch Maintainable

The upstream Grok Build repository is periodically synchronized from another codebase, so minimize invasive changes.

Prefer:

```text
new crate
small public interfaces
small exports
adapter layer
```

over restructuring existing Grok internals.

Avoid large refactors unrelated to MCP server support.

When existing types/functions need to become public, expose the smallest surface necessary.

Add comments explaining why the public API is needed.

---

# Non-Goals

Do not implement:

```text
a new coding agent
a new LLM abstraction
another prompt loop
a ChatGPT API client
a replacement for Grok's tool implementations
a replacement for Grok's MCP client system
a new plugin system
a custom shell implementation
```

Do not make Grok decide how to solve coding tasks.

The external MCP client is responsible for reasoning.

---

# Architecture Target

Final architecture should look approximately like this:

```text
                         ChatGPT Web
                              │
                              │ MCP
                              ▼
                    ┌──────────────────┐
                    │ Grok MCP Server  │
                    └────────┬─────────┘
                             │
                       Tool Adapter
                             │
                       Tool Registry
                             │
            ┌────────────────┴─────────────────┐
            │                                  │
            ▼                                  ▼
      Grok Built-ins                    Managed MCP
            │                                  │
    ┌───────┼────────┐              ┌──────────┼──────────┐
    ▼       ▼        ▼              ▼          ▼          ▼
 filesystem shell   patch         GitHub     Browser   custom
            │
            ▼
        Workspace
```

There must be no LLM/model layer between the MCP server and tool registry.

---

# Definition of Done

The work is complete when all of the following are true:

* [ ] Grok Build can be launched as an MCP server.
* [ ] The MCP server can run without invoking the Grok model.
* [ ] Existing Grok coding tools are discoverable using `tools/list`.
* [ ] Existing Grok coding tools can be executed using `tools/call`.
* [ ] Tool execution uses existing Grok workspace/runtime abstractions.
* [ ] Existing sandbox/permission protections remain active.
* [ ] Configured downstream MCP tools can be exposed through the same server.
* [ ] Tool naming collisions are handled safely.
* [ ] A workspace can be selected from the CLI.
* [ ] The server defaults to local-only networking.
* [ ] Integration tests cover read, modification, shell execution, and workspace isolation.
* [ ] At least one downstream MCP tool is tested end-to-end.
* [ ] The implementation does not require a Grok API key for purely local tools.
* [ ] Documentation includes build, run, configuration, and test instructions.
* [ ] Changes to existing Grok crates are kept as small as practical.

---

# Implementation Strategy

Work incrementally.

First get this working:

```text
MCP client
→ Grok MCP server
→ one existing Grok tool
```

Then:

```text
→ all suitable built-in tools
```

Then:

```text
→ configured downstream MCP tools
```

Then finish:

```text
permissions
namespacing
tests
CLI
documentation
```

Do not attempt a large rewrite before proving a single existing Grok tool can successfully execute through the MCP protocol.

At the end, provide:

1. Architecture summary.
2. Files added.
3. Existing files modified.
4. Important design decisions.
5. Commands to build and run.
6. Commands/tests used to verify functionality.
7. Known limitations.
8. A short example showing how an external MCP client would connect.
