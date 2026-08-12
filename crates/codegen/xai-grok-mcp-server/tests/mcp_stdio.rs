use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::{ClientHandler, ServiceExt};
use tempfile::TempDir;
use xai_grok_mcp_server::{GatewayServer, GatewaySession, READ_FILE_TOOL};

#[derive(Clone, Default)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
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
    let client = start_gateway_session(session).await;
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
    client.cancel().await.unwrap();
}
