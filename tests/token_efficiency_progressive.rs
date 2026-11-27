//! Tes terstruktur Progressive Disclosure & Diet Token:
//! Membuktikan penghematan token terukur pada system prompt + schema tool + aktivasi on-demand.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::{analyze_token_breakdown, build_system_prompt};
use selencode::agent::plan::TurnMode;
use selencode::agent::run_turn;
use selencode::providers::{
    AssistantReply, Message, Provider, StreamEvent, ToolCall, ToolSpec,
};
use selencode::tools::toolsets::{
    active_tool_specs, select_initial_toolsets, ACTIVATE_TOOLSET_NAME,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[test]
fn progressive_toolsets_cut_initial_token_overhead_drastically() {
    let reg = default_registry();
    let all_specs = reg.specs();
    let cwd = std::env::temp_dir();

    // 1. Tugas ringan ("buat file hello.txt") -> hanya Core Toolset + activate_toolset (~9 tool).
    let simple_ts = select_initial_toolsets(
        "buat file hello.txt berisi Halo Dunia",
        &cwd,
        false,
        TurnMode::Execute,
    );
    let simple_specs = active_tool_specs(&reg, &simple_ts, TurnMode::Execute);
    assert!(
        simple_specs.len() <= 10,
        "tugas ringan harus <= 10 tool aktif, dapat {}",
        simple_specs.len()
    );
    assert!(all_specs.len() >= 20, "registry penuh memiliki >= 20 tool");

    let sys_simple = build_system_prompt(&cwd, &simple_specs);
    let msgs = vec![
        Message::text(selencode::providers::Role::System, sys_simple),
        Message::text(selencode::providers::Role::User, "buat file hello.txt berisi Halo Dunia"),
    ];
    let bd_lean = analyze_token_breakdown(&msgs, &simple_specs, all_specs.len());
    let bd_full = analyze_token_breakdown(&msgs, &all_specs, all_specs.len());

    // Penghematan token schema tool WAJIB signifikan (> 60%) dan total awal < 2.000 token!
    assert!(
        bd_lean.tools_tokens * 100 < bd_full.tools_tokens * 40,
        "schema tool harus turun >60%: lean={} vs full={}",
        bd_lean.tools_tokens,
        bd_full.tools_tokens
    );
    assert!(
        bd_lean.total_tokens() < 2000,
        "total awal tugas ringan harus < 2000 token (target Muse <= 4000), dapat {}",
        bd_lean.total_tokens()
    );

    // 2. Prompt yang menyebut LSP / commit / url otomatis mengaktifkan toolset terkait tanpa langkah ekstra!
    let lsp_ts = select_initial_toolsets(
        "cari referensi simbol lewat lsp lalu commit perubahan ke https://example.com",
        &cwd,
        false,
        TurnMode::Execute,
    );
    assert!(lsp_ts.contains("lsp"));
    assert!(lsp_ts.contains("git_write"));
    assert!(lsp_ts.contains("web"));
}

struct ActivateToolsetMock {
    calls: AtomicUsize,
    seen_tool_counts: Mutex<Vec<usize>>,
}

#[async_trait]
impl Provider for ActivateToolsetMock {
    fn model_name(&self) -> &str {
        "mock-SelenCode"
    }
    async fn stream_chat(
        &self,
        _messages: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_tool_counts.lock().unwrap().push(tools.len());
        match n {
            0 => Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "act1".into(),
                    name: ACTIVATE_TOOLSET_NAME.into(),
                    args: json!({"toolsets": ["lsp", "bg"]}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            }),
            _ => {
                // Setelah activate_toolset(["lsp", "bg"]), tool lsp_diagnostics & bg_start HARUS muncul di `tools`!
                assert!(tools.iter().any(|t| t.name == "lsp_diagnostics"));
                assert!(tools.iter().any(|t| t.name == "bg_start"));
                let _ = tx.send(StreamEvent::Token("Toolset aktif!".into())).await;
                Ok(AssistantReply {
                    text: "Toolset aktif!".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }
}

#[tokio::test]
async fn activate_toolset_dynamically_expands_tools_mid_turn() {
    let cwd: PathBuf = std::env::temp_dir().join(format!(
        "selen-SelenCode-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let mock = Arc::new(ActivateToolsetMock {
        calls: AtomicUsize::new(0),
        seen_tool_counts: Mutex::new(Vec::new()),
    });
    let prov: Arc<dyn Provider> = mock.clone();
    let mut messages = Vec::new();
    let out = run_turn(prov, &reg, &ctx, &mut messages, "halo", 5)
        .await
        .unwrap();
    assert_eq!(out, "Toolset aktif!");
    let counts = mock.seen_tool_counts.lock().unwrap().clone();
    assert_eq!(counts.len(), 2);
    assert!(counts[1] > counts[0] + 6, "jumlah tool harus bertambah setelah aktivasi: {counts:?}");

    // Pastikan skill bawaan `save-token` bisa dipasang & dibaca via Tier-2 load_skill!
    let p = selencode::skills::install_token_efficiency_skill(&cwd).unwrap();
    assert!(p.is_file());
    let (info, body) = selencode::skills::load_skill(&cwd, "save-token").unwrap();
    assert_eq!(info.name, "save-token");
    assert!(body.contains("Progressive Disclosure"));

    let _ = std::fs::remove_dir_all(&cwd);
}
