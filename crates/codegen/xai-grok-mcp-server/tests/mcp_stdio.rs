use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ServiceExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::sync::Notify;
use xai_grok_mcp_server::{
    ApprovalDecision, ChannelApprovalBroker, GatewayEvent, GatewayPermissionDecision,
    GatewayServer, GatewaySession, HttpGateway, READ_FILE_TOOL,
};

#[derive(Clone, Default)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[derive(Clone, Default)]
struct ListChangedClient {
    notifications: Arc<AtomicUsize>,
    changed: Arc<Notify>,
}

impl ClientHandler for ListChangedClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.notifications.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        std::future::ready(())
    }
}

async fn start_gateway(
    workspace: &TempDir,
) -> rmcp::service::RunningService<rmcp::RoleClient, TestClient> {
    let session = GatewaySession::new(workspace.path()).expect("gateway session without API key");
    start_gateway_session(session).await
}

async fn start_gateway_session(
    session: GatewaySession,
) -> rmcp::service::RunningService<rmcp::RoleClient, TestClient> {
    let server = GatewayServer::new(session);
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .expect("server should start")
            .waiting()
            .await;
    });
    TestClient
        .serve(client_transport)
        .await
        .expect("MCP initialize should succeed")
}

async fn start_gateway_with_events(
    session: GatewaySession,
) -> (
    std::sync::Arc<GatewaySession>,
    rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
) {
    let server = GatewayServer::new(session);
    let session = std::sync::Arc::clone(server.session());
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });
    let client = TestClient.serve(client_transport).await.unwrap();
    (session, client)
}

async fn start_http_gateway_session(
    session: GatewaySession,
) -> (
    HttpGateway,
    rmcp::service::RunningService<rmcp::RoleClient, TestClient>,
) {
    let gateway = HttpGateway::bind(GatewayServer::new(session), "127.0.0.1:0".parse().unwrap())
        .await
        .expect("HTTP gateway should bind");
    let client = TestClient
        .serve(StreamableHttpClientTransport::from_uri(gateway.endpoint()))
        .await
        .expect("HTTP MCP initialize should succeed");
    (gateway, client)
}

async fn write_mutable_downstream(workspace: &TempDir) -> std::path::PathBuf {
    let script = workspace.path().join("mutable-downstream.py");
    std::fs::write(&script, r#"import json, sys
catalog = 0
def tools():
    if catalog == 0:
        return [
            {"name":"switch","description":"switch the fixture catalog","inputSchema":{"type":"object","properties":{}}},
            {"name":"legacy","description":"legacy tool","inputSchema":{"type":"object","properties":{}}},
            {"name":"mutable","description":"mutable v1","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}},
        ]
    return [
        {"name":"switch","description":"switch the fixture catalog","inputSchema":{"type":"object","properties":{}}},
        {"name":"mutable","description":"mutable v2","inputSchema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}},
        {"name":"added","description":"newly added tool","inputSchema":{"type":"object","properties":{}}},
    ]
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    changed = False
    if method == "initialize":
        result = {"protocolVersion":"2025-03-26","capabilities":{"tools":{"listChanged":True}},"serverInfo":{"name":"mutable","version":"1"}}
    elif method == "tools/list":
        result = {"tools":tools()}
    elif method == "tools/call":
        name = request["params"]["name"]
        if name == "switch":
            catalog = 1
            changed = True
        result = {"content":[{"type":"text","text":"called:" + name}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
    if changed:
        print(json.dumps({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}), flush=True)
"#).unwrap();
    let config = workspace.path().join("mutable-mcp.toml");
    std::fs::write(
        &config,
        format!(
            "[mcp_servers.fixture]\ncommand = \"python3\"\nargs = [\"{}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    config
}

async fn assert_dynamic_catalog_refresh(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ListChangedClient>,
    observer: &ListChangedClient,
) {
    let before = client.list_tools(None).await.unwrap();
    assert!(
        before
            .tools
            .iter()
            .any(|tool| tool.name.as_ref() == "fixture__legacy")
    );
    let changed = observer.changed.notified();
    let switch = client
        .call_tool(CallToolRequestParams::new("fixture__switch"))
        .await
        .unwrap();
    assert_eq!(switch.is_error, Some(false));
    tokio::time::timeout(std::time::Duration::from_secs(3), changed)
        .await
        .expect("outer gateway must forward tools/list_changed");
    let after = client.list_tools(None).await.unwrap();
    assert!(
        !after
            .tools
            .iter()
            .any(|tool| tool.name.as_ref() == "fixture__legacy")
    );
    assert!(
        after
            .tools
            .iter()
            .any(|tool| tool.name.as_ref() == "fixture__added")
    );
    let mutable = after
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "fixture__mutable")
        .unwrap();
    assert_eq!(mutable.description.as_deref(), Some("mutable v2"));
    assert_eq!(
        mutable.input_schema["properties"]["count"]["type"],
        "integer"
    );
    let updated_call = client
        .call_tool(
            CallToolRequestParams::new("fixture__mutable")
                .with_arguments(serde_json::json!({"count": 1}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(updated_call.is_error, Some(false));
    let disappeared = client
        .call_tool(CallToolRequestParams::new("fixture__legacy"))
        .await
        .unwrap();
    assert_eq!(disappeared.is_error, Some(true));
    // A repeated notification must not duplicate the retained registration.
    client
        .call_tool(CallToolRequestParams::new("fixture__switch"))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let repeated = client.list_tools(None).await.unwrap();
    assert_eq!(
        repeated
            .tools
            .iter()
            .filter(|tool| tool.name.as_ref() == "fixture__mutable")
            .count(),
        1
    );
    assert_eq!(observer.notifications.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn downstream_list_changed_refreshes_stdio_and_notifies_outer_client() {
    let workspace = TempDir::new().unwrap();
    let config = write_mutable_downstream(&workspace).await;
    let session = GatewaySession::with_native_mcp_config(workspace.path(), config)
        .await
        .unwrap();
    let server = GatewayServer::new(session);
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });
    let observer = ListChangedClient::default();
    let client = observer.clone().serve(client_transport).await.unwrap();
    assert_dynamic_catalog_refresh(&client, &observer).await;
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn downstream_list_changed_refreshes_http_and_notifies_outer_client() {
    let workspace = TempDir::new().unwrap();
    let config = write_mutable_downstream(&workspace).await;
    let session = GatewaySession::with_native_mcp_config(workspace.path(), config)
        .await
        .unwrap();
    let gateway = HttpGateway::bind(GatewayServer::new(session), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let observer = ListChangedClient::default();
    let client = observer
        .clone()
        .serve(StreamableHttpClientTransport::from_uri(gateway.endpoint()))
        .await
        .unwrap();
    assert_dynamic_catalog_refresh(&client, &observer).await;
    client.cancel().await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn initialize_and_tools_list_use_finalized_grok_definition_without_an_api_key() {
    let workspace = TempDir::new().unwrap();
    let expected_session = GatewaySession::new(workspace.path()).unwrap();
    let expected = expected_session.toolset().tool_definitions();
    assert!(expected.len() >= 6);

    let client = start_gateway(&workspace).await;
    assert_eq!(
        client
            .peer_info()
            .expect("initialize response")
            .server_info
            .name,
        "xai-grok-mcp-server"
    );

    let listed = client.list_tools(None).await.unwrap();
    assert_eq!(listed.tools.len(), expected.len());
    for definition in expected {
        let tool = listed
            .tools
            .iter()
            .find(|tool| tool.name.as_ref() == definition.function.name)
            .expect("each finalized definition must be listed");
        assert_eq!(
            tool.description.as_deref(),
            definition.function.description.as_deref()
        );
        assert_eq!(
            tool.input_schema.as_ref(),
            definition.function.parameters.as_object().unwrap()
        );
    }

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn tools_call_reads_a_file_through_the_finalized_toolset() {
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "hello from workspace\n").unwrap();
    let client = start_gateway(&workspace).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new(READ_FILE_TOOL).with_arguments(
                serde_json::json!({"target_file": "hello.txt"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false), "result: {result:?}");
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(text.contains("hello from workspace"), "tool output: {text}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn unexposed_tool_is_denied() {
    let workspace = TempDir::new().unwrap();
    let client = start_gateway(&workspace).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("bash").with_arguments(
                serde_json::json!({"command": "echo must-not-run"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("not available")
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn workspace_edit_safe_shell_and_path_policy_are_enforced() {
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("edit.txt"), "before\n").unwrap();
    let client = start_gateway(&workspace).await;

    let edit = client
        .call_tool(
            CallToolRequestParams::new("search_replace").with_arguments(
                serde_json::json!({"file_path":"edit.txt","old_string":"before","new_string":"after","replace_all":false})
                    .as_object().unwrap().clone(),
            ),
        ).await.unwrap();
    assert_eq!(edit.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("edit.txt")).unwrap(),
        "after\n"
    );

    let create = client
        .call_tool(
            CallToolRequestParams::new("search_replace").with_arguments(
                serde_json::json!({"file_path":"created.txt","old_string":"","new_string":"created\n","replace_all":false})
                    .as_object().unwrap().clone(),
            ),
        ).await.unwrap();
    assert_eq!(create.is_error, Some(false));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("created.txt")).unwrap(),
        "created\n"
    );

    let shell = client
        .call_tool(
            CallToolRequestParams::new("run_terminal_cmd").with_arguments(
                serde_json::json!({"command":"pwd","description":"show workspace","is_background":false})
                    .as_object().unwrap().clone(),
            ),
        ).await.unwrap();
    assert_eq!(shell.is_error, Some(false));

    let denied_shell = client
        .call_tool(
            CallToolRequestParams::new("run_terminal_cmd").with_arguments(
                serde_json::json!({"command":"rm -rf /","description":"unsafe","is_background":false})
                    .as_object().unwrap().clone(),
            ),
        ).await.unwrap();
    assert_eq!(denied_shell.is_error, Some(true));

    let outside = client
        .call_tool(
            CallToolRequestParams::new(READ_FILE_TOOL).with_arguments(
                serde_json::json!({"target_file":"../outside.txt"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(outside.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn configured_downstream_tool_is_registered_and_called_via_finalized_toolset() {
    let workspace = TempDir::new().unwrap();
    let script = workspace.path().join("downstream.py");
    std::fs::write(&script, r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"echo","description":"fixture echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"downstream:" + request["params"]["arguments"]["value"]}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
"#).unwrap();
    let config = workspace.path().join("mcp.toml");
    std::fs::write(
        &config,
        format!(
            "[mcp_servers.fixture]\ncommand = \"python3\"\nargs = [\"{}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    let session = GatewaySession::with_native_mcp_config(workspace.path(), &config)
        .await
        .unwrap();
    let expected = session
        .toolset()
        .tool_definitions()
        .into_iter()
        .find(|definition| definition.function.name == "fixture__echo")
        .unwrap();
    let (gateway_session, client) = start_gateway_with_events(session).await;
    let mut events = gateway_session.event_bus().subscribe();
    let listed = client.list_tools(None).await.unwrap();
    let tool = listed
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "fixture__echo")
        .unwrap();
    assert_eq!(
        tool.description.as_deref(),
        expected.function.description.as_deref()
    );
    assert_eq!(
        tool.input_schema.as_ref(),
        expected.function.parameters.as_object().unwrap()
    );
    let result = client
        .call_tool(
            CallToolRequestParams::new("fixture__echo").with_arguments(
                serde_json::json!({"value":"ok"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "result: {result:?}");
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("downstream:ok")
    );
    let mut saw_started = false;
    let mut saw_finished = false;
    for _ in 0..3 {
        match events.recv().await.unwrap() {
            GatewayEvent::ToolCallStarted { tool_name, .. } if tool_name == "fixture__echo" => {
                saw_started = true
            }
            GatewayEvent::ToolCallFinished { tool_name, .. } if tool_name == "fixture__echo" => {
                saw_finished = true
            }
            _ => {}
        }
        if saw_started && saw_finished {
            break;
        }
    }
    assert!(
        saw_started && saw_finished,
        "downstream call must use outer lifecycle events"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn streamable_http_uses_the_same_catalog_and_preserves_gateway_policy() {
    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "hello over HTTP\n").unwrap();
    let expected = GatewaySession::new(workspace.path())
        .unwrap()
        .toolset()
        .tool_definitions();
    let stdio = start_gateway(&workspace).await;
    let stdio_catalog: Vec<_> = stdio
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.map(|value| value.to_string()),
                (*tool.input_schema).clone(),
            )
        })
        .collect();
    let (gateway, client) =
        start_http_gateway_session(GatewaySession::new(workspace.path()).unwrap()).await;

    assert_eq!(
        client
            .peer_info()
            .expect("initialize response")
            .server_info
            .name,
        "xai-grok-mcp-server"
    );
    let listed = client.list_tools(None).await.unwrap();
    let http_catalog: Vec<_> = listed
        .tools
        .iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_ref().map(ToString::to_string),
                (*tool.input_schema).clone(),
            )
        })
        .collect();
    assert_eq!(http_catalog, stdio_catalog);
    assert_eq!(listed.tools.len(), expected.len());
    for definition in expected {
        let tool = listed
            .tools
            .iter()
            .find(|tool| tool.name.as_ref() == definition.function.name)
            .unwrap();
        assert_eq!(
            tool.description.as_deref(),
            definition.function.description.as_deref()
        );
        assert_eq!(
            tool.input_schema.as_ref(),
            definition.function.parameters.as_object().unwrap()
        );
    }

    let read = client
        .call_tool(
            CallToolRequestParams::new(READ_FILE_TOOL).with_arguments(
                serde_json::json!({"target_file":"hello.txt"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(read.is_error, Some(false));
    assert!(
        read.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("hello over HTTP")
    );

    let dangerous = client
        .call_tool(
            CallToolRequestParams::new("run_terminal_cmd").with_arguments(
                serde_json::json!({"command":"rm -rf /","description":"unsafe","is_background":false})
                    .as_object().unwrap().clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(dangerous.is_error, Some(true));
    let outside = client
        .call_tool(
            CallToolRequestParams::new(READ_FILE_TOOL).with_arguments(
                serde_json::json!({"target_file":"../outside.txt"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(outside.is_error, Some(true));
    stdio.cancel().await.unwrap();
    client.cancel().await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn streamable_http_discovers_and_calls_downstream_tools_without_an_api_key() {
    let workspace = TempDir::new().unwrap();
    let script = workspace.path().join("downstream-http.py");
    std::fs::write(&script, r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"echo","description":"fixture HTTP echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"downstream-http:" + request["params"]["arguments"]["value"]}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
"#).unwrap();
    let config = workspace.path().join("mcp.toml");
    std::fs::write(
        &config,
        format!(
            "[mcp_servers.fixture]\ncommand = \"python3\"\nargs = [\"{}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    let session = GatewaySession::with_native_mcp_config(workspace.path(), &config)
        .await
        .unwrap();
    assert_eq!(session.downstream_server_count().await, 1);
    let (gateway, client) = start_http_gateway_session(session).await;
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .any(|tool| tool.name.as_ref() == "fixture__echo")
    );
    let result = client
        .call_tool(
            CallToolRequestParams::new("fixture__echo").with_arguments(
                serde_json::json!({"value":"ok"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("downstream-http:ok")
    );
    client.cancel().await.unwrap();
    gateway.shutdown().await.unwrap();
}

#[tokio::test]
async fn ask_allow_once_executes_and_events_share_one_call_id() {
    let workspace = TempDir::new().unwrap();
    let (broker, mut approvals) = ChannelApprovalBroker::new(2);
    let session = GatewaySession::new(workspace.path())
        .unwrap()
        .with_approval_broker(broker);
    let (session, client) = start_gateway_with_events(session).await;
    let mut events = session.event_bus().subscribe();
    {
        let call = client.call_tool(
            CallToolRequestParams::new("run_terminal_cmd").with_arguments(
                serde_json::json!({"command":"cargo --version","description":"version","is_background":false})
                    .as_object().unwrap().clone(),
            ),
        );
        tokio::pin!(call);
        let pending = tokio::select! {
            pending = approvals.recv() => pending.expect("Ask should reach broker"),
            result = call.as_mut() => panic!("Ask completed before approval: {result:?}"),
        };
        assert_eq!(pending.request.tool_name, "run_terminal_cmd");
        pending.resolve(ApprovalDecision::AllowOnce);
        let result = call.as_mut().await.unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    let mut started = None;
    let mut requested = None;
    let mut resolved = None;
    let mut finished = None;
    for _ in 0..5 {
        match events.recv().await.unwrap() {
            GatewayEvent::ToolCallStarted { call_id, .. } => started = Some(call_id),
            GatewayEvent::ApprovalRequested { call_id, .. } => requested = Some(call_id),
            GatewayEvent::ApprovalResolved {
                call_id, allowed, ..
            } => {
                assert!(allowed);
                resolved = Some(call_id);
            }
            GatewayEvent::ToolCallFinished { call_id, .. } => finished = Some(call_id),
            _ => {}
        }
        if finished.is_some() {
            break;
        }
    }
    assert_eq!(started, requested);
    assert_eq!(started, resolved);
    assert_eq!(started, finished);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn ask_deny_timeout_and_missing_broker_fail_closed() {
    let workspace = TempDir::new().unwrap();
    let (broker, mut approvals) = ChannelApprovalBroker::new(2);
    let session = GatewaySession::new(workspace.path())
        .unwrap()
        .with_approval_broker(broker)
        .with_approval_timeout(std::time::Duration::from_millis(20));
    let (_, client) = start_gateway_with_events(session).await;
    let params =
        || {
            CallToolRequestParams::new("run_terminal_cmd").with_arguments(
        serde_json::json!({"command":"cargo test","description":"test","is_background":false})
            .as_object().unwrap().clone(),
    )
        };
    {
        let call = client.call_tool(params());
        tokio::pin!(call);
        let pending = tokio::select! {
            pending = approvals.recv() => pending.unwrap(),
            result = call.as_mut() => panic!("Ask completed before approval: {result:?}"),
        };
        pending.resolve(ApprovalDecision::Deny);
        assert_eq!(call.as_mut().await.unwrap().is_error, Some(true));
    }

    // The late resolution is deliberately ignored after the timeout; no tool
    // can execute after this call has already returned a denial.
    let timed_out = client.call_tool(params()).await.unwrap();
    assert_eq!(timed_out.is_error, Some(true));
    approvals
        .recv()
        .await
        .unwrap()
        .resolve(ApprovalDecision::AllowOnce);
    client.cancel().await.unwrap();

    let missing = start_gateway(&workspace).await;
    let result = missing.call_tool(params()).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    missing.cancel().await.unwrap();
}

#[tokio::test]
async fn shell_permission_keeps_safe_ask_and_deny_distinct() {
    let workspace = TempDir::new().unwrap();
    let session = GatewaySession::new(workspace.path()).unwrap();
    for (command, expected) in [
        ("pwd", GatewayPermissionDecision::Allow),
        ("cargo test", GatewayPermissionDecision::Ask),
        ("rm -rf /", GatewayPermissionDecision::Deny),
    ] {
        let input = session
            .toolset()
            .try_parse(
                "run_terminal_cmd",
                &serde_json::json!({"command":command,"description":"test","is_background":false}),
            )
            .await
            .unwrap();
        assert_eq!(
            session.permission().evaluate("run_terminal_cmd", &input),
            expected
        );
    }
}

#[tokio::test]
async fn execution_failure_emits_failed_lifecycle_event() {
    let workspace = TempDir::new().unwrap();
    let (session, client) =
        start_gateway_with_events(GatewaySession::new(workspace.path()).unwrap()).await;
    let mut events = session.event_bus().subscribe();
    let result = client
        .call_tool(
            CallToolRequestParams::new("get_terminal_command_output").with_arguments(
                serde_json::json!({"task_ids":[]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let mut started = None;
    let mut failed = None;
    for _ in 0..3 {
        match events.recv().await.unwrap() {
            GatewayEvent::ToolCallStarted { call_id, .. } => started = Some(call_id),
            GatewayEvent::ToolCallFailed { call_id, .. } => failed = Some(call_id),
            _ => {}
        }
        if failed.is_some() {
            break;
        }
    }
    assert_eq!(started, failed);
    client.cancel().await.unwrap();
}
