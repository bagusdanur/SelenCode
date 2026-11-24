use async_trait::async_trait;
use selencode::agent::run_turn;
use selencode::providers::{AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Provider bohongan scripted:
/// mode "tool": panggilan 1 -> tool_call read_file, panggilan 2 -> teks final.
/// mode "text": langsung teks.
struct MockProvider {
    calls: AtomicUsize,
    mode: String,
    target: String,
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.mode == "tool" && n == 0 {
            let _ = tx.send(StreamEvent::Token("saya baca dulu. ".into())).await;
            return Ok(AssistantReply {
                text: "saya baca dulu.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: json!({"path": self.target}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            });
        }
        let _ = tx.send(StreamEvent::Token("selesai!".into())).await;
        Ok(AssistantReply {
            text: "selesai!".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        })
    }
}

fn temp_cwd(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("selencode-loop-{}-{}", tag, uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test]
async fn loop_runs_two_rounds_with_tool() {
    let cwd = temp_cwd("tool");
    std::fs::write(cwd.join("catatan.txt"), "isi rahasia proyek").unwrap();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
        mode: "tool".into(),
        target: "catatan.txt".into(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let mut messages: Vec<Message> = Vec::new();

    let final_text = run_turn(provider, &reg, &ctx, &mut messages, "baca catatan", 25)
        .await
        .unwrap();
    assert_eq!(final_text, "selesai!");

    // Ada ToolResult berisi isi file di riwayat
    let found = messages.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, Block::ToolResult { content, is_error, .. }
            if content.contains("isi rahasia proyek") && !is_error))
    });
    assert!(found, "ToolResult berisi isi file harus ada: {messages:?}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn loop_stops_on_plain_text() {
    let cwd = temp_cwd("text");
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
        mode: "text".into(),
        target: String::new(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let mut messages: Vec<Message> = Vec::new();
    let final_text = run_turn(provider, &reg, &ctx, &mut messages, "halo", 25)
        .await
        .unwrap();
    assert_eq!(final_text, "selesai!");
    // system + user + assistant saja
    assert_eq!(messages.len(), 3, "{messages:?}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn loop_tool_error_becomes_tool_result_not_crash() {
    let cwd = temp_cwd("err");
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
        mode: "tool".into(),
        target: "tidak-ada.txt".into(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let mut messages: Vec<Message> = Vec::new();
    let final_text = run_turn(provider, &reg, &ctx, &mut messages, "baca", 25)
        .await
        .unwrap();
    assert_eq!(final_text, "selesai!");
    let found_err = messages.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, Block::ToolResult { is_error, .. } if *is_error))
    });
    assert!(found_err, "error tool harus jadi ToolResult is_error: {messages:?}");
    std::fs::remove_dir_all(&cwd).ok();
}
