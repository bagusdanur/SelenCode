use async_trait::async_trait;
use selencode::providers::anthropic::{build_request as anthropic_request, AnthropicStreamParser};
use selencode::providers::gemini::{build_request as gemini_request, GeminiStreamParser};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::repl::{parse_slash_command, SlashCommand};
use selencode::store::Store;
use selencode::tools::web::check_url_allowed;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

fn sample_tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "read_file".into(),
        description: "baca file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    }]
}

fn sample_msgs() -> Vec<Message> {
    vec![
        Message::text(Role::System, "kamu Selena"),
        Message::text(Role::User, "baca a.txt"),
        Message {
            role: Role::Assistant,
            content: vec![
                Block::Text("baik".into()),
                Block::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "a.txt"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "call_1".into(),
                content: "isi file".into(),
                is_error: false,
            }],
        },
    ]
}

// ---------- Translasi format: Anthropic ----------

#[test]
fn anthropic_request_separates_system_and_tools() {
    let body = anthropic_request(&sample_msgs(), &sample_tools(), "claude-x", 4096);
    // system TIDAK jadi pesan, tapi field `system`
    assert_eq!(body["system"], json!("kamu Selena"));
    let msgs = body["messages"].as_array().unwrap();
    assert!(msgs.iter().all(|m| m["role"] != "system"));
    // user pertama = teks biasa dalam block text
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"][0]["type"], "text");
    // assistant membawa tool_use dengan input sebagai objek
    assert_eq!(msgs[1]["role"], "assistant");
    let tu = msgs[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use ada");
    assert_eq!(tu["name"], "read_file");
    assert_eq!(tu["input"]["path"], "a.txt");
    // tool_result terbungkus sebagai pesan user
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(msgs[2]["content"][0]["content"], "isi file");
    // tools memakai input_schema
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["path"]["type"],
        "string"
    );
    assert_eq!(body["stream"], true);
}

// ---------- Translasi format: Gemini ----------

#[test]
fn gemini_request_uses_system_instruction_and_function_declarations() {
    let body = gemini_request(&sample_msgs(), &sample_tools());
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "kamu Selena");
    let decls = &body["tools"][0]["functionDeclarations"];
    assert_eq!(decls[0]["name"], "read_file");
    assert_eq!(decls[0]["parameters"]["properties"]["path"]["type"], "string");

    let contents = body["contents"].as_array().unwrap();
    // user text
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "baca a.txt");
    // model functionCall
    assert_eq!(contents[1]["role"], "model");
    let fc = contents[1]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .expect("functionCall ada");
    assert_eq!(fc["functionCall"]["name"], "read_file");
    assert_eq!(fc["functionCall"]["args"]["path"], "a.txt");
    // functionResponse: id internal untuk gemini = nama fungsi
    let msgs2 = vec![
        Message::text(Role::User, "lanjut"),
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "read_file".into(),
                content: "isi file".into(),
                is_error: false,
            }],
        },
    ];
    let body2 = gemini_request(&msgs2, &[]);
    let contents2 = body2["contents"].as_array().unwrap();
    let fr = &contents2[1]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "read_file");
    assert_eq!(fr["response"]["result"], "isi file");
}

// ---------- Parsing stream: Anthropic ----------

#[test]
fn anthropic_parser_collects_text_tool_and_usage() {
    let mut p = AnthropicStreamParser::new();
    let events = [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 42, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "halo "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "dunia"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": " \"a.txt\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 17}}),
        json!({"type": "message_stop"}),
    ];
    let mut tokens = String::new();
    for e in &events {
        if let Some(t) = p.handle_event(e) {
            tokens.push_str(&t);
        }
    }
    let reply = p.finish();
    assert_eq!(tokens, "halo dunia");
    assert_eq!(reply.text, "halo dunia");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "toolu_1");
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].args, json!({"path": "a.txt"}));
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_use"));
    let u = reply.usage.expect("usage ada");
    assert_eq!(u.input_tokens, 42);
    assert_eq!(u.output_tokens, 17);
}

// ---------- Parsing stream: Gemini ----------

#[test]
fn gemini_parser_collects_text_function_call_and_usage() {
    let mut p = GeminiStreamParser::new();
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"text": "siap, "}]}}]}),
        json!({"candidates": [{"content": {"parts": [
            {"text": "kubaca"},
            {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}
        ]}}], "usageMetadata": {"promptTokenCount": 30, "candidatesTokenCount": 9}}),
    ];
    let mut tokens = String::new();
    for c in &chunks {
        if let Some(t) = p.handle_chunk(c) {
            tokens.push_str(&t);
        }
    }
    let reply = p.finish();
    assert_eq!(tokens, "siap, kubaca");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].id, "read_file"); // id gemini = nama fungsi
    assert_eq!(reply.tool_calls[0].args, json!({"path": "a.txt"}));
    let u = reply.usage.expect("usage ada");
    assert_eq!((u.input_tokens, u.output_tokens), (30, 9));
}

// ---------- Store SQLite ----------

fn temp_store(tag: &str) -> (Store, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "selencode-fase3-{tag}-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&path).unwrap();
    (store, path)
}

#[test]
fn store_roundtrip_messages_usage_and_cwd_isolation() {
    let (store, path) = temp_store("roundtrip");
    let sid = store
        .create_session("/proyek/a", "router", "model-x", "judul sesi")
        .unwrap();
    let msgs = vec![
        Message::text(Role::System, "sys"),
        Message::text(Role::User, "halo"),
        Message {
            role: Role::Assistant,
            content: vec![
                Block::Text("baik".into()),
                Block::ToolUse {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "a.txt"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c1".into(),
                content: "isi".into(),
                is_error: false,
            }],
        },
    ];
    store.save_messages(&sid, &msgs).unwrap();
    let loaded = store.load_messages(&sid).unwrap();
    assert_eq!(loaded.len(), msgs.len());
    for (a, b) in msgs.iter().zip(loaded.iter()) {
        assert_eq!(a.role, b.role);
        assert_eq!(format!("{:?}", a.content), format!("{:?}", b.content));
    }

    store.record_usage(&sid, "model-x", 100, 25).unwrap();
    store.record_usage(&sid, "model-x", 50, 5).unwrap();
    assert_eq!(store.total_usage(&sid).unwrap(), (150, 30));

    // Sesi cwd lain tidak tercampur.
    let other = store
        .create_session("/proyek/b", "router", "model-x", "sesi b")
        .unwrap();
    let list_a = store.list_sessions("/proyek/a", 10).unwrap();
    assert_eq!(list_a.len(), 1);
    assert_eq!(list_a[0].id, sid);
    let latest_b = store.latest_session("/proyek/b").unwrap().unwrap();
    assert_eq!(latest_b.id, other);
    assert!(store.latest_session("/tidak/ada").unwrap().is_none());

    std::fs::remove_file(&path).ok();
}

#[test]
fn db_path_env_override() {
    std::env::set_var("SELENCODE_DB", "/tmp/selencode-override-test.db");
    assert_eq!(
        selencode::store::db_path(),
        PathBuf::from("/tmp/selencode-override-test.db")
    );
    std::env::remove_var("SELENCODE_DB");
}

// ---------- fetch_url: penolakan SSRF (tanpa jaringan keluar) ----------

#[tokio::test]
async fn fetch_url_rejects_private_and_bad_schemes() {
    for bad in [
        "http://127.0.0.1/",
        "http://127.0.0.1:8080/admin",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://10.0.0.5/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data",
        "file:///etc/passwd",
        "ftp://example.com/x",
    ] {
        assert!(
            check_url_allowed(bad).await.is_err(),
            "{bad} harus ditolak"
        );
    }
    // localhost beresolusi ke loopback -> ditolak lewat jalur DNS.
    assert!(check_url_allowed("http://localhost/").await.is_err());
    assert!(check_url_allowed("http://localhost:3000/x").await.is_err());
}

// ---------- Parsing perintah garis miring ----------

#[test]
fn slash_command_parsing() {
    assert_eq!(parse_slash_command("/exit"), SlashCommand::Exit);
    assert_eq!(parse_slash_command("/quit"), SlashCommand::Exit);
    assert_eq!(parse_slash_command("/model"), SlashCommand::Model(None));
    assert_eq!(
        parse_slash_command("/model claude"),
        SlashCommand::Model(Some("claude".into()))
    );
    assert_eq!(parse_slash_command("/sessions"), SlashCommand::Sessions);
    assert_eq!(parse_slash_command("/models"), SlashCommand::Models(None));
    assert_eq!(
        parse_slash_command("/models claude"),
        SlashCommand::Models(Some("claude".into()))
    );
    assert_eq!(
        parse_slash_command("/provider add claude anthropic claude-sonnet-4 ANTHROPIC_API_KEY"),
        SlashCommand::Provider(Some(
            "add claude anthropic claude-sonnet-4 ANTHROPIC_API_KEY".into()
        ))
    );
    assert_eq!(parse_slash_command("/new"), SlashCommand::New);
    assert_eq!(parse_slash_command("/clear"), SlashCommand::New);
    assert_eq!(parse_slash_command("/compact"), SlashCommand::Compact);
    assert_eq!(parse_slash_command("/help"), SlashCommand::Help);
    assert_eq!(parse_slash_command("/?"), SlashCommand::Help);
    assert_eq!(
        parse_slash_command("/ngawur"),
        SlashCommand::Unknown("ngawur".into())
    );
    assert_eq!(
        parse_slash_command("perbaiki bug ini"),
        SlashCommand::NotCommand
    );
    assert_eq!(parse_slash_command("  /model  "), SlashCommand::Model(None));
}

// ---------- Loop + store: persistensi end-to-end (provider bohongan) ----------

struct UsageMock;

#[async_trait]
impl Provider for UsageMock {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let _ = tx.send(StreamEvent::Token("ok".into())).await;
        Ok(AssistantReply {
            text: "ok".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 7,
            }),
        })
    }
}

#[tokio::test]
async fn loop_persists_messages_and_usage_to_store() {
    use selencode::agent::run_turn;
    use selencode::tools::{default_registry, Ctx};

    let (store, path) = temp_store("loop");
    let store = Arc::new(store);
    let cwd = std::env::temp_dir();
    let sid = store
        .create_session(&cwd.to_string_lossy(), "mock", "mock-model", "tes")
        .unwrap();
    let ctx = Ctx::new(cwd.clone(), true).with_store(store.clone(), sid.clone());
    let provider: Arc<dyn Provider> = Arc::new(UsageMock);
    let reg = default_registry();
    let mut messages: Vec<Message> = Vec::new();
    run_turn(provider, &reg, &ctx, &mut messages, "halo", 25)
        .await
        .unwrap();

    let saved = store.load_messages(&sid).unwrap();
    assert_eq!(saved.len(), messages.len());
    assert!(saved.len() >= 3, "system+user+assistant tersimpan: {saved:?}");
    assert_eq!(store.total_usage(&sid).unwrap(), (11, 7));
    std::fs::remove_file(&path).ok();
}

#[test]
fn config_save_load_and_direct_api_key_fallback() {
    use selencode::config::{load_from, save_to, AgentCfg, Config, ProviderCfg, ShellCfg};
    use std::collections::HashMap;

    let path = std::env::temp_dir().join(format!(
        "selencode-cfg-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    let mut providers = HashMap::new();
    providers.insert(
        "or".to_string(),
        ProviderCfg {
            kind: "openai".to_string(),
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            model: "anthropic/claude-3.7-sonnet".to_string(),
            models: vec![
                "anthropic/claude-3.7-sonnet".to_string(),
                "openai/gpt-4.1".to_string(),
            ],
            api_key_env: String::new(),
            api_key: Some("sk-direct-test-key".to_string()),
            price_in: None,
            price_out: None,
            context_limit: None,
        },
    );
    let cfg = Config {
        default_provider: "or".to_string(),
        providers,
        agent: AgentCfg::default(),
        shell: ShellCfg::default(),
        mcp_servers: Vec::new(),
        telegram: Default::default(),
        routing: Default::default(),
        sandbox: Default::default(),
        hooks: Vec::new(),
    };
    save_to(&cfg, &path).unwrap();
    let loaded = load_from(&path).unwrap();
    let (name, pcfg, key) = loaded.resolve_provider(None).unwrap();
    assert_eq!(name, "or");
    assert_eq!(pcfg.model, "anthropic/claude-3.7-sonnet");
    assert_eq!(pcfg.effective_models().len(), 2);
    assert_eq!(key, "sk-direct-test-key");
    std::fs::remove_file(&path).ok();
}
