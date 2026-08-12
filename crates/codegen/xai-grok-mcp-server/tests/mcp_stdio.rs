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
    assert_eq!(expected.len(), 1);

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
    assert_eq!(listed.tools.len(), 1);
    let tool = &listed.tools[0];
    let definition = &expected[0].function;
    assert_eq!(tool.name.as_ref(), definition.name);
    assert_eq!(
        tool.description.as_deref(),
        definition.description.as_deref()
    );
    assert_eq!(
        tool.input_schema.as_ref(),
        definition.parameters.as_object().unwrap()
    );

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

    assert_eq!(result.is_error, Some(false));
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
