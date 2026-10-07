# Grok Build MCP

A fork of [xai-org/grok-build](https://github.com/xai-org/grok-build) that exposes Grok Build's local coding tools through an MCP gateway. Connect your MCP client to a workspace to read and edit files, search code, and run terminal commands.

The gateway runs without a Grok account or an xAI model API key. Your MCP client supplies the model; the gateway executes tools. The original Grok CLI and TUI are included and retain their own authentication requirements.

## Features

- Streamable HTTP and local stdio transports.
- Stateful HTTP for session-aware clients and stateless HTTP for clients that probe before initialization.
- Local observer TUI for tool activity and approval requests.
- Headless execution, terminal approval, and automatic approval.
- Static bearer-token authentication and an explicit `--no-auth` mode.
- Downstream MCP servers loaded from native Grok TOML configuration, with namespaced tools and catalog refresh.

## Windows binary

Open the [Windows Build workflow](https://github.com/KinasTomes/grok-build-mcp/actions/workflows/windows.yml), select a successful run, and download the `grok-build-mcp-windows-x64` artifact. Extract it to get `grok.exe`.

The workflow runs on pushes to `main` and can be started manually. Each artifact contains the code from that run; features on another branch require a build of that branch.

```powershell
.\grok.exe --version
.\grok.exe mcp server --help
```

If the executable is on `PATH`, use `grok` in place of `.\grok.exe`. The official xAI installer installs upstream Grok Build rather than this fork.

## Quick start

### Windows PowerShell

Generate a random bearer token and start the gateway for an existing workspace:

```powershell
$keyBytes = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($keyBytes)
$rng.Dispose()
$env:GROK_MCP_API_KEY = [Convert]::ToBase64String($keyBytes)

.\grok.exe mcp server --workspace "D:\Code\my-project"
```

### Linux / macOS

```bash
export GROK_MCP_API_KEY="$(openssl rand -hex 32)"
grok mcp server --workspace /path/to/my-project
```

The default endpoint is `http://127.0.0.1:8765/mcp`. HTTP starts with the observer TUI, where you can inspect activity and respond to approval requests. The workspace defaults to the current directory when `--workspace` is omitted.

Configure your client with:

| Setting | Value |
| --- | --- |
| Transport | Streamable HTTP |
| URL | `http://127.0.0.1:8765/mcp` |
| Header | `Authorization: Bearer <your GROK_MCP_API_KEY value>` |

`GROK_MCP_API_KEY` must contain at least 32 characters. It is a static token supplied by you, not generated automatically by the gateway. Set it in the environment of the process launching the server; a `.env` file alone does not load it into that process.

## Run modes

### Headless with automatic approval

For unattended tool execution:

```bash
grok mcp server --workspace /path/to/my-project --headless --always-approve
```

`--always-approve` accepts calls that would otherwise require approval. Its aliases are `--yolo` and `--dangerously-skip-permissions`. Workspace file-path checks and explicit destructive-command denials still apply. These checks do not make arbitrary shell commands safe; use this mode with clients you trust.

Without an approval UI, terminal approval broker, or automatic approval, calls requiring approval are denied.

### Terminal approval

```bash
grok mcp server --workspace /path/to/my-project --terminal-approval
```

This replaces the full-screen observer with terminal approval prompts. It is available only for HTTP transport.

### Stateless HTTP and remote access

For clients that perform discovery before `initialize`, such as ChatGPT's `openai-mcp` client:

```bash
grok mcp server --workspace /path/to/my-project --headless --stateless --always-approve
```

Default HTTP mode uses MCP sessions and `Mcp-Session-Id`. `--stateless` makes requests independent and does not return that header.

To connect remotely, forward `http://127.0.0.1:8765` through an HTTPS reverse proxy or tunnel. Configure the client with `https://<your-domain>/mcp` and the bearer token. An existing Cloudflare Tunnel can route that hostname to the local port.

### HTTP without authentication

For local testing or a deployment where a proxy handles authentication:

```bash
grok mcp server --workspace /path/to/my-project --headless --no-auth
```

`--no-auth` removes the HTTP bearer-token requirement. Anyone who can reach the endpoint can call exposed tools, subject to gateway policy. Authentication and approval are separate; add `--always-approve` if you also want unattended execution.

### Local stdio

```bash
grok mcp server --transport stdio --workspace /path/to/my-project --headless --always-approve
```

Stdio requires `--headless` and does not require `GROK_MCP_API_KEY`. Clients using a `mcpServers` JSON configuration can launch it directly:

```json
{
  "mcpServers": {
    "grok-build": {
      "command": "grok",
      "args": [
        "mcp", "server",
        "--transport", "stdio",
        "--workspace", "/path/to/my-project",
        "--headless",
        "--always-approve"
      ]
    }
  }
}
```

On Windows, use the full executable path if needed and escape backslashes in JSON paths.

## Downstream MCP servers

Create a native Grok configuration file, for example `gateway.toml`:

```toml
[mcp_servers.my_tools]
command = "/path/to/my-mcp-server"
args = []

[mcp_servers.remote_tools]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${REMOTE_MCP_TOKEN}" }
```

Load it explicitly:

```bash
grok mcp server --workspace /path/to/my-project --mcp-config gateway.toml
```

Downstream tools have names such as `my_tools__tool_name`. Catalogs refresh when connected servers report changes. Pass `--mcp-config` to load downstream servers; the gateway does not automatically load your normal Grok MCP configuration.

See the [MCP guide](crates/codegen/xai-grok-pager/docs/user-guide/07-mcp-servers.md) for configuration syntax and the separate `grok mcp add`, `list`, and `doctor` commands.

## Build from source

Requirements:

- Rust through `rustup`; the toolchain is pinned in [rust-toolchain.toml](rust-toolchain.toml).
- A native C/C++ build toolchain. On Windows, use Visual Studio Build Tools with the C++ workload.
- Protocol Buffers compiler (`protoc`). On Unix, bundled `bin/protoc` uses DotSlash; install `dotslash` or supply a native compiler through `PROTOC` / `PATH`.
- Enough disk space for the workspace and build cache. Target individual packages rather than the entire workspace.

```bash
git clone https://github.com/KinasTomes/grok-build-mcp.git
cd grok-build-mcp
cargo build --release -p xai-grok-pager-bin
```

The artifact is `target/release/xai-grok-pager` on Unix or `target/release/xai-grok-pager.exe` on Windows. Rename it to `grok` / `grok.exe`, or invoke it by its original name.

Windows setup with Chocolatey, matching the build workflow:

```powershell
choco install protoc -y
$env:PROTOC = (Get-Command protoc).Source
cargo build --release -p xai-grok-pager-bin
.\target\release\xai-grok-pager.exe mcp server --help
```

To run from source, set `GROK_MCP_API_KEY` and execute:

```bash
cargo run -p xai-grok-pager-bin -- mcp server --workspace /path/to/my-project
```

## Development

```bash
cargo check -p xai-grok-mcp-server
cargo test -p xai-grok-mcp-server --test mcp_stdio
cargo test -p xai-grok-pager --lib mcp_cmd
cargo fmt --check --package xai-grok-mcp-server --package xai-grok-pager
```

The integration suite covers discovery, tool calls, permissions, approvals, downstream catalog changes, and HTTP/stdio behavior. The root [Cargo.toml](Cargo.toml) is generated upstream; prefer per-crate manifests when changing dependencies.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| API key must contain at least 32 characters | Set `GROK_MCP_API_KEY` in the server process environment, or use the intended `--no-auth` mode. |
| HTTP returns `401` | Send the matching `Authorization: Bearer <token>` header. |
| Discovery fails before initialization | Try `--stateless` if the client probes before `initialize`. |
| A headless terminal call is denied | Use `--terminal-approval` for interactive HTTP approval or `--always-approve` for unattended execution. Hard denials still apply. |
| An outside-workspace file request is denied | Set the correct `--workspace`; direct file tools are scoped to that directory. |
| Windows cannot execute `bin/protoc` | Install native `protoc` and set `PROTOC` to its executable path. |
| Linker reports `No space left on device` | Free build storage before retrying; the pager has a large dependency tree. |

## Repository layout

| Path | Purpose |
| --- | --- |
| `crates/codegen/xai-grok-mcp-server` | Gateway runtime, transport, policy, approvals, events, and tests |
| `crates/codegen/xai-grok-pager/src/mcp_cmd.rs` | MCP CLI and gateway startup |
| `crates/codegen/xai-grok-pager/src/gateway_observer.rs` | Gateway observer TUI |
| `crates/codegen/xai-grok-pager-bin` | CLI executable package |
| `crates/codegen/xai-grok-tools` | Existing coding-tool implementations |
| `.github/workflows/windows.yml` | Windows release build and packaging |

The repository also retains the upstream agent, TUI, workspace runtime, and shared crates. [SOURCE_REV](SOURCE_REV) records the upstream revision.

## Documentation and license

- [Bundled user guide](crates/codegen/xai-grok-pager/docs/user-guide/README.md)
- [MCP configuration](crates/codegen/xai-grok-pager/docs/user-guide/07-mcp-servers.md)
- [Gateway implementation plan](PLAN.md)

First-party code is licensed under [Apache-2.0](LICENSE). This fork derives from xAI's Grok Build; third-party components retain their original licenses. See [THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES), [tool notices](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md), and [vendored notices](third_party/NOTICE).
