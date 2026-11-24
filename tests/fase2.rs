//! Tes Fase 2: redaksi rahasia, compaction konteks, dan redaksi terpusat di loop.
use async_trait::async_trait;
use selencode::agent::context::{compact_messages, estimate_tokens, needs_compaction};
use selencode::agent::run_turn;
use selencode::providers::{AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec};
use selencode::security::redact_secrets;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

// ---------- redaksi ----------

#[test]
fn redact_common_secret_shapes() {
    let input = concat!(
        "key1 sk-abcdefgh12345678Z selesai\n",
        "Authorization: Bearer abcdefghijklmnop1234\n",
        "API_KEY=supersecret99\n",
        "db_password: \"kata-sandi-panjang\"\n",
        "aws AKIAIOSFODNN7EXAMPLE ok\n",
        "gh ghp_1234567890abcdefghij1234\n",
    );
    let out = redact_secrets(input);
    assert!(out.contains("[REDACTED]"), "{out}");
    for secret in [
        "sk-abcdefgh12345678Z",
        "abcdefghijklmnop1234",
        "supersecret99",
        "kata-sandi-panjang",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_1234567890abcdefghij1234",
    ] {
        assert!(!out.contains(secret), "rahasia '{secret}' bocor: {out}");
    }
}

#[test]
fn redact_leaves_normal_text_alone() {
    let normal = "halo dunia, ini teks biasa tanpa rahasia. token ekonomi dibahas, password pendek aman.";
    assert_eq!(redact_secrets(normal), normal);
}

// ---------- compaction ----------

fn long_history() -> Vec<Message> {
    let mut v = vec![
        Message::text(Role::System, "system prompt"),
        Message::text(Role::User, "perintah pertama user"),
    ];
    for i in 0..5 {
        v.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: format!("c{i}"),
                name: "read_file".into(),
                args: json!({"path": format!("f{i}.rs")}),
            }],
        });
        v.push(Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: format!("c{i}"),
                content: format!("isi file ke-{i} ").repeat(40),
                is_error: false,
            }],
        });
    }
    v.push(Message::text(Role::User, "perintah terakhir user"));
    v.push(Message::text(Role::Assistant, "jawaban terakhir"));
    v
}

#[test]
fn compaction_shrinks_and_keeps_edges() {
    let msgs = long_history();
    let before = msgs.len();
    let out = compact_messages(&msgs);
    assert!(out.len() < before, "{} !< {}", out.len(), before);
    // Kepala utuh: system + user pertama
    assert_eq!(out[0].plain_text(), "system prompt");
    assert_eq!(out[1].plain_text(), "perintah pertama user");
    // Ada pesan ringkasan berisi penanda hasil lama + daftar tool
    let summary = out
        .iter()
        .find(|m| m.plain_text().contains("[hasil lama diringkas]"))
        .expect("pesan ringkasan harus ada");
    assert!(summary.plain_text().contains("read_file"), "{summary:?}");
    // Ekor utuh: pesan terakhir tidak berubah
    assert_eq!(out.last().unwrap().plain_text(), "jawaban terakhir");
    assert_eq!(
        out[out.len() - 2].plain_text(),
        "perintah terakhir user"
    );
    // Tidak ada ToolResult yatim di awal ekor (pasangan tool utuh):
    // pesan tepat sebelum ekor adalah ringkasan System.
    let tail_start = out.len() - 6;
    assert!(out[tail_start].role != Role::Tool || tail_start == 0);
}

#[test]
fn compaction_trigger_threshold() {
    let msgs = long_history();
    assert!(estimate_tokens(&msgs) > 0);
    assert!(!needs_compaction(&msgs, 1_000_000));
    assert!(needs_compaction(&msgs, 10));
    // Riwayat pendek tidak menyusut
    let short = vec![Message::text(Role::User, "halo")];
    assert_eq!(compact_messages(&short).len(), 1);
}

// ---------- redaksi terpusat di loop ----------

struct ReadOnceProvider {
    calls: AtomicUsize,
    target: String,
}

#[async_trait]
impl Provider for ReadOnceProvider {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: json!({"path": self.target}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            });
        }
        let _ = tx.send(StreamEvent::Token("beres".into())).await;
        Ok(AssistantReply {
            text: "beres".into(),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn loop_redacts_tool_output_before_provider() {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "selencode-redact-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(
        dir.join("catatan.txt"),
        "konfigurasi: API_KEY=rahasia12345678 jangan bocor",
    )
    .unwrap();

    let provider: Arc<dyn Provider> = Arc::new(ReadOnceProvider {
        calls: AtomicUsize::new(0),
        target: "catatan.txt".into(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true);
    let mut messages: Vec<Message> = Vec::new();
    run_turn(provider, &reg, &ctx, &mut messages, "baca catatan", 25)
        .await
        .unwrap();

    let result = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("harus ada ToolResult");
    assert!(result.contains("[REDACTED]"), "{result}");
    assert!(!result.contains("rahasia12345678"), "rahasia bocor: {result}");
    std::fs::remove_dir_all(&dir).ok();
}
