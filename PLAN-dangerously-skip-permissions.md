# Plan: Always-Approve Mode for the MCP Gateway

## Goal

Allow unattended MCP gateway sessions to run tools that normally require an
approval prompt.

The gateway command should accept the same names as the existing Grok CLI:

```text
--always-approve
--yolo
--dangerously-skip-permissions
```

`--always-approve` is the canonical name; the other two are aliases.

## Safety Contract

This mode converts permission decisions from `Ask` to `Allow`. It does not
convert `Deny` to `Allow`.

Therefore it must not bypass:

- workspace path containment;
- destructive-command denials;
- unavailable or unregistered tool rejection;
- downstream-tool registration checks;
- HTTP `GROK_MCP_API_KEY` authentication;
- cancellation or tool-runtime errors.

The flag removes interactive approval, not the gateway's hard safety policy.

## Smallest Implementation

### 1. Add the CLI flag

In `crates/codegen/xai-grok-pager/src/mcp_cmd.rs`, extend
`GatewayServerArgs` with one boolean field:

```rust
#[arg(
    long = "always-approve",
    alias = "yolo",
    alias = "dangerously-skip-permissions"
)]
pub always_approve: bool,
```

Do not add a new permission-mode enum or configuration file setting until a
second gateway permission mode actually exists.

### 2. Carry the mode into `GatewaySession`

Add a builder-style method such as
`GatewaySession::with_always_approve()` and call it once in
`run_gateway_server` after either `GatewaySession::new` or
`GatewaySession::with_native_mcp_config` has completed.

This keeps stdio, stateful HTTP, stateless HTTP, observer UI, and downstream
MCP configuration on the same session path.

### 3. Apply it at the shared permission boundary

Add one `always_approve: bool` field to `GatewayPermission`, defaulting to
`false`.

`GatewayPermission::evaluate` should first compute its existing decision, then
map only:

```text
Ask + always_approve=true -> Allow
```

Existing `Allow` and `Deny` decisions remain unchanged. Do not special-case
individual tools in `GatewayServer::call_tool`; all transports and future
tools should continue through the shared permission policy.

### 4. Keep approval infrastructure unchanged

Do not add a second approval broker or event type. In always-approve mode no
`ApprovalRequested` event is produced because the shared decision is already
`Allow`. Existing observer and terminal approval paths remain unchanged when
the flag is absent.

### 5. Document the dangerous mode

Update the built-in gateway section in
`crates/codegen/xai-grok-pager/docs/user-guide/07-mcp-servers.md` with:

```bash
grok mcp server --headless --stateless --always-approve
```

State explicitly that hard denials and workspace containment still apply.

## Tests

Add the smallest security-focused regression coverage:

1. CLI parsing accepts all three names and resolves them to the same boolean.
2. An ordinary shell command currently classified as `Ask` executes without
   an approval broker when always-approve is enabled.
3. A destructive command classified as `Deny` remains denied.
4. A path outside the workspace remains denied.
5. Existing default-mode approval tests remain unchanged and pass.

The existing HTTP tests already prove HTTP and stdio share `GatewayServer` and
the same catalog/policy path; do not duplicate the entire matrix for this
single boolean.

## Verification

```bash
cargo fmt --check --package xai-grok-mcp-server
cargo test -p xai-grok-mcp-server --test mcp_stdio
cargo test -p xai-grok-pager mcp_cmd
```

Manual smoke test:

```bash
export GROK_MCP_API_KEY="<random-secret-with-at-least-32-characters>"
grok mcp server \
  --workspace /path/to/test-workspace \
  --headless \
  --stateless \
  --always-approve
```

Verify that a normal build/test command runs without prompting, while an
outside-workspace file request and a destructive shell command are rejected.

## Definition of Done

- [x] All three CLI spellings enable the same mode.
- [x] `Ask` decisions run without an approval broker.
- [x] `Deny` decisions remain denied.
- [x] Default behavior is unchanged when the flag is absent.
- [x] API-key authentication remains required for HTTP mode.
- [x] Documentation includes the warning and launch example.
- [ ] Focused tests and existing MCP gateway integration tests pass.
