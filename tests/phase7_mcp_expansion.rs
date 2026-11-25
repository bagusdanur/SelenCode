//! Tes Fase 7 — MCP Compatibility Expansion (resources, prompts, schema validation, ping, & auto-reconnect).

use selencode::config::McpServerCfg;
use selencode::mcp::client::{
    validate_tool_args, McpClient, SUPPORTED_MCP_FEATURES, UNSUPPORTED_MCP_FEATURES,
};
use selencode::mcp::connect_server;
use selencode::tools::{Ctx, Tool};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn full_fixture_path() -> String {
    format!(
        "{}/tests/fixtures/mcp_full_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn mcp_schema_validation_catches_missing_required_and_wrong_types() {
    let schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "count": { "type": "integer" },
            "verbose": { "type": "boolean" }
        },
        "required": ["name"]
    });

    assert!(validate_tool_args(&schema, &json!({"name": "Selena", "count": 3, "verbose": true})).is_ok());
    assert!(validate_tool_args(&schema, &json!({})).is_err());
    assert!(validate_tool_args(&schema, &json!({"name": 123})).is_err());
    assert!(validate_tool_args(&schema, &json!({"name": "ok", "count": "bukan_angka"})).is_err());
    assert!(!SUPPORTED_MCP_FEATURES.is_empty());
    assert!(!UNSUPPORTED_MCP_FEATURES.is_empty());
}

#[tokio::test]
async fn mcp_resources_prompts_ping_and_auto_reconnect_end_to_end() {
    if !python3_available() {
        eprintln!("python3 tidak ada — tes MCP expansion dilewati");
        return;
    }
    let cfg = McpServerCfg {
        name: "full".into(),
        command: "python3".into(),
        args: vec![full_fixture_path()],
        env: HashMap::new(),
        trust: true,
        timeout_secs: 10,
    };

    let (client, tools) = McpClient::start(&cfg).await.unwrap();
    assert_eq!(tools.len(), 2);

    let caps = client.capabilities().await;
    assert!(caps.resources);
    assert!(caps.prompts);
    assert_eq!(caps.server_name, "selen-full-fixture");

    // 1) Ping
    client.ping().await.unwrap();

    // 2) Resources list & read
    let resources = client.list_resources().await.unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///docs/arch.md");
    let content = client.read_resource("file:///docs/arch.md").await.unwrap();
    assert!(content.contains("Arsitektur SelenCode"));

    // 3) Prompts list & get
    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "code_review");
    let ptext = client
        .get_prompt("code_review", json!({"target": "src/sandbox.rs"}))
        .await
        .unwrap();
    assert!(ptext.contains("src/sandbox.rs"));

    // 4) Auto-reconnect setelah server mati mendadak!
    let _ = client.call_tool("die_once", json!({})).await;
    // Tunggu sebentar agar reader task melihat EOF dan menandai dead=true
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    // Panggilan berikutnya wajib otomatis reconnect & berhasil!
    let recovered = client.call_tool("greet", json!({"name": "Selena"})).await.unwrap();
    assert_eq!(recovered, "Halo Selena!");

    // 5) connect_server mengekspor tools + mcp__full__resources + mcp__full__prompts
    let exposed = connect_server(&cfg).await.unwrap();
    let names: Vec<String> = exposed.iter().map(|t| t.spec().name).collect();
    assert!(names.contains(&"mcp__full__greet".to_string()));
    assert!(names.contains(&"mcp__full__resources".to_string()));
    assert!(names.contains(&"mcp__full__prompts".to_string()));

    // Schema validation via McpTool menolak argumen wajib yang hilang sebelum kirim ke server!
    let greet_tool = exposed
        .iter()
        .find(|t| t.spec().name == "mcp__full__greet")
        .unwrap();
    let ctx = Ctx::new(PathBuf::from("."), true);
    assert!(greet_tool.run(json!({}), &ctx).await.is_err());
    assert_eq!(
        greet_tool.run(json!({"name": "Rust"}), &ctx).await.unwrap(),
        "Halo Rust!"
    );
}
