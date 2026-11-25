//! Tes Wave 4 — Multimodal Vision (`Block::Image`), Custom Hooks (`pre_tool`/`post_tool`),
//! MCP Streamable HTTP/SSE parsing, & Worktree isolated verification.

use selencode::config::{HookCfg, McpServerCfg};
use selencode::hooks::{matcher_hits, HookEngine};
use selencode::mcp::client::{expand_env_header, parse_http_or_sse_jsonrpc};
use selencode::providers::{anthropic, detect_image_mime, encode_base64, gemini, Block, Message, Role};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use serde_json::json;
use std::path::{Path, PathBuf};

#[test]
fn multimodal_image_encoding_providers_and_sqlite_roundtrip() {
    assert_eq!(encode_base64(b"hi"), "aGk=");
    assert_eq!(encode_base64(b"Selen"), "U2VsZW4=");
    assert_eq!(detect_image_mime(Path::new("shot.PNG")), Some("image/png"));
    assert_eq!(detect_image_mime(Path::new("err.jpg")), Some("image/jpeg"));
    assert_eq!(detect_image_mime(Path::new("code.rs")), None);

    let msg = Message {
        role: Role::User,
        content: vec![
            Block::Text("Analisis screenshot ini".into()),
            Block::Image {
                media_type: "image/png".into(),
                data_base64: "aGk=".into(),
            },
        ],
    };

    // Anthropic request membawa image source base64
    let anth = anthropic::build_request(&[msg.clone()], &[], "claude", 1024);
    assert_eq!(anth["messages"][0]["content"][1]["type"], "image");
    assert_eq!(
        anth["messages"][0]["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(anth["messages"][0]["content"][1]["source"]["data"], "aGk=");

    // Gemini request membawa inlineData
    let gem = gemini::build_request(&[msg.clone()], &[]);
    assert_eq!(gem["contents"][0]["parts"][1]["inlineData"]["mimeType"], "image/png");
    assert_eq!(gem["contents"][0]["parts"][1]["inlineData"]["data"], "aGk=");

    // SQLite roundtrip untuk Block::Image
    let db: PathBuf = std::env::temp_dir().join(format!(
        "selencode-w4-img-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&db).unwrap();
    let sid = store.create_session("/tmp", "r", "m", "img").unwrap();
    store.save_message(&sid, &msg).unwrap();
    let loaded = store.load_messages(&sid).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(matches!(
        &loaded[0].content[1],
        Block::Image { media_type, data_base64 } if media_type == "image/png" && data_base64 == "aGk="
    ));
    let _ = std::fs::remove_file(&db);
}

#[tokio::test]
async fn custom_hooks_matcher_pre_block_and_post_notes() {
    assert!(matcher_hits("*", "edit_file"));
    assert!(matcher_hits("edit_file,write_file", "write_file"));
    assert!(matcher_hits("mcp__*", "mcp__github__list"));
    assert!(!matcher_hits("edit_file", "read_file"));

    let sandbox = SandboxController::default();
    let cwd = std::env::temp_dir();

    // Hook post_tool yang berhasil mengembalikan catatan
    let ok_hook = HookCfg {
        event: "post_tool".into(),
        matcher: "edit_file".into(),
        command: "echo hook_berjalan".into(),
        timeout_secs: 5,
        block_on_error: false,
    };
    let engine = HookEngine::from_config(&[ok_hook]);
    let notes = engine
        .run_post_tool("edit_file", "{}", false, &cwd, &sandbox)
        .await;
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("hook_berjalan"));

    // Hook pre_tool dengan block_on_error=true dan exit non-zero memblokir tool!
    let block_hook = HookCfg {
        event: "pre_tool".into(),
        matcher: "run_shell".into(),
        command: "exit 1".into(),
        timeout_secs: 5,
        block_on_error: true,
    };
    let blocker = HookEngine::from_config(&[block_hook]);
    let res = blocker
        .run_pre_tool("run_shell", "{}", &cwd, &sandbox)
        .await;
    assert!(res.is_err());
}

#[test]
fn mcp_http_sse_parser_and_header_env_expansion() {
    std::env::set_var("SELENCODE_TEST_MCP_HDR", "rahasia123");
    assert_eq!(
        expand_env_header("Bearer ${SELENCODE_TEST_MCP_HDR}"),
        "Bearer rahasia123"
    );

    let direct = parse_http_or_sse_jsonrpc(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#)
        .unwrap();
    assert_eq!(direct["result"], json!({"tools": []}));

    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n";
    let parsed_sse = parse_http_or_sse_jsonrpc(sse).unwrap();
    assert_eq!(parsed_sse["result"]["ok"], json!(true));

    // Config TOML remote HTTP MCP parse mulus tanpa `command`
    let srv: McpServerCfg = toml::from_str(
        r#"
        name = "remote_gh"
        url = "https://mcp.example.com/rpc"
        trust = false
        [headers]
        Authorization = "Bearer ${GH_TOKEN}"
        "#,
    )
    .unwrap();
    assert_eq!(srv.url.as_deref(), Some("https://mcp.example.com/rpc"));
    assert_eq!(srv.headers.get("Authorization").map(String::as_str), Some("Bearer ${GH_TOKEN}"));
}
