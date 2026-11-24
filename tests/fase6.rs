//! Tes Fase 6: plan mode, extract_plan, update_plan, delegate/subagent,
//! parser /plan, dan reducer TUI PlanUpdated/PlanReady.
//! Semua provider bohongan — tanpa jaringan, tanpa API key asli.

use async_trait::async_trait;
use selencode::agent::plan::{extract_plan, TurnMode};
use selencode::agent::{run_turn_mode, UiEvent};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec,
};
use selencode::repl::{parse_slash_command, SlashCommand};
use selencode::tools::delegate::Delegate;
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use selencode::tui::app::{App, LineKind};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_cwd(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-fase6-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn reply_text(t: &str) -> AssistantReply {
    AssistantReply {
        text: t.into(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
    }
}

// ---------- Plan mode ----------

/// Provider bohongan plan: panggilan 1 memanggil write_file (menulis),
/// panggilan 2 menutup dengan rencana. Mencatat nama tool yang diterima.
struct PlanMock {
    calls: AtomicUsize,
    seen_tools: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl Provider for PlanMock {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        self.seen_tools
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let _ = tx.send(StreamEvent::Token("saya coba tulis. ".into())).await;
            return Ok(AssistantReply {
                text: "saya coba tulis.".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    args: json!({"path": "baru.txt", "content": "halo"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            });
        }
        let _ = tx.send(StreamEvent::Token("## Rencana".into())).await;
        Ok(reply_text(
            "Sudah kujelajahi.\n\n## Rencana\n1. Baca catatan.txt\n2. Edit isinya\n3. Jalankan tes",
        ))
    }
}

#[tokio::test]
async fn plan_mode_filters_specs_and_blocks_writes() {
    let cwd = temp_cwd("plan");
    let mock = Arc::new(PlanMock {
        calls: AtomicUsize::new(0),
        seen_tools: Mutex::new(Vec::new()),
    });
    let provider: Arc<dyn Provider> = mock.clone();
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let mut messages: Vec<Message> = Vec::new();

    let final_text = run_turn_mode(
        provider,
        &reg,
        &ctx,
        &mut messages,
        "buatkan file baru",
        25,
        TurnMode::Plan,
    )
    .await
    .unwrap();
    assert!(final_text.contains("## Rencana"), "{final_text}");

    // (a) Specs yang diterima provider TIDAK memuat tool menulis.
    let seen = mock.seen_tools.lock().unwrap();
    assert!(!seen.is_empty());
    for names in seen.iter() {
        for banned in ["write_file", "edit_file", "run_shell", "git_commit", "update_plan", "delegate"] {
            assert!(
                !names.iter().any(|n| n == banned),
                "tool {banned} tidak boleh ditawarkan di plan mode: {names:?}"
            );
        }
        for allowed in ["read_file", "list_files", "search_text", "outline"] {
            assert!(
                names.iter().any(|n| n == allowed),
                "tool baca-saja {allowed} harus ditawarkan: {names:?}"
            );
        }
    }
    drop(seen);

    // (b) write_file yang dipanggil model ditolak & file TIDAK terbuat.
    assert!(!cwd.join("baru.txt").exists(), "file tidak boleh terbuat di plan mode");
    let blocked = messages.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, Block::ToolResult { content, is_error, .. }
            if *is_error && content.contains("tidak tersedia di plan mode")))
    });
    assert!(blocked, "ToolResult penolakan plan mode harus ada: {messages:?}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn extract_plan_rules() {
    // Dengan penanda: ambil dari penanda sampai akhir.
    let p = extract_plan("Pembuka dulu.\n\n## Rencana\n1. Satu\n2. Dua").unwrap();
    assert!(p.starts_with("## Rencana"), "{p}");
    assert!(!p.contains("Pembuka"), "{p}");
    // Tanpa penanda: seluruh jawaban adalah rencana.
    let p2 = extract_plan("1. Satu\n2. Dua").unwrap();
    assert_eq!(p2, "1. Satu\n2. Dua");
    // Kosong -> None.
    assert_eq!(extract_plan("   "), None);
    assert_eq!(extract_plan(""), None);
}

// ---------- update_plan ----------

#[tokio::test]
async fn update_plan_renders_and_updates_state() {
    let cwd = temp_cwd("upd");
    let reg = default_registry();
    let tool = reg.get("update_plan").expect("update_plan terdaftar");
    assert!(!tool.needs_approval(&json!({})));
    let ctx = Ctx::new(cwd.clone(), true);

    let out = tool
        .run(
            json!({"steps": [
                {"text": "Baca kode", "status": "done"},
                {"text": "Tulis fitur", "status": "in_progress"},
                {"text": "Tes"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.contains("[x] Baca kode"), "{out}");
    assert!(out.contains("[>] Tulis fitur"), "{out}");
    assert!(out.contains("[ ] Tes"), "{out}");
    assert_eq!(ctx.plan_state.lock().unwrap().len(), 3);

    // Panggilan kedua mengganti daftar & status.
    let out2 = tool
        .run(
            json!({"steps": [{"text": "Tulis fitur", "status": "done"}]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out2.contains("[x] Tulis fitur"), "{out2}");
    let state = ctx.plan_state.lock().unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].status, "done");
    drop(state);
    std::fs::remove_dir_all(&cwd).ok();
}

// ---------- delegate / subagent ----------

/// Provider bohongan anak: tanpa ToolResult -> panggil read_file catatan.txt
/// (atau write_file bila attempt_write). Dengan ToolResult -> teks final
/// yang mencerminkan berhasil/gagalnya tool. Stateless per riwayat pesan,
/// jadi aman dipakai banyak loop anak bersamaan.
struct ChildMock {
    attempt_write: bool,
}

#[async_trait]
impl Provider for ChildMock {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let mut saw_ok: Option<String> = None;
        let mut saw_err = false;
        for m in msgs {
            for b in &m.content {
                if let Block::ToolResult {
                    content, is_error, ..
                } = b
                {
                    if *is_error {
                        saw_err = true;
                    } else {
                        saw_ok = Some(content.clone());
                    }
                }
            }
        }
        if saw_err {
            let _ = tx.send(StreamEvent::Token("gagal".into())).await;
            return Ok(reply_text("tidak bisa menulis: tool ditolak"));
        }
        if let Some(content) = saw_ok {
            let _ = tx.send(StreamEvent::Token("temuan".into())).await;
            return Ok(reply_text(&format!("temuan: {content}")));
        }
        let (name, args) = if self.attempt_write {
            (
                "write_file",
                json!({"path": "anak-nulis.txt", "content": "dari anak"}),
            )
        } else {
            ("read_file", json!({"path": "catatan.txt"}))
        };
        Ok(AssistantReply {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "a1".into(),
                name: name.into(),
                args,
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn delegate_runs_parallel_in_input_order() {
    let cwd = temp_cwd("del");
    std::fs::write(cwd.join("catatan.txt"), "isi-target").unwrap();
    let provider: Arc<dyn Provider> = Arc::new(ChildMock { attempt_write: false });
    let tool = Delegate::new(provider, Arc::new(read_only_registry()), 3);
    let ctx = Ctx::new(cwd.clone(), true);

    let out = tool
        .run(
            json!({"tasks": [
                {"prompt": "periksa alpha"},
                {"prompt": "periksa beta"},
                {"prompt": "periksa gamma"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();
    let i1 = out.find("### Tugas 1: periksa alpha").expect(&out);
    let i2 = out.find("### Tugas 2: periksa beta").expect(&out);
    let i3 = out.find("### Tugas 3: periksa gamma").expect(&out);
    assert!(i1 < i2 && i2 < i3, "urutan harus sesuai input: {out}");
    assert_eq!(out.matches("temuan: isi-target").count(), 3, "{out}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn delegate_children_are_read_only() {
    let cwd = temp_cwd("delro");
    let provider: Arc<dyn Provider> = Arc::new(ChildMock { attempt_write: true });
    let tool = Delegate::new(provider, Arc::new(read_only_registry()), 3);
    let ctx = Ctx::new(cwd.clone(), true);
    let out = tool
        .run(json!({"tasks": [{"prompt": "tulis file"}]}), &ctx)
        .await
        .unwrap();
    assert!(out.contains("tidak bisa menulis"), "{out}");
    assert!(
        !cwd.join("anak-nulis.txt").exists(),
        "subagent tidak boleh bisa menulis file"
    );
    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn delegate_rejects_bad_task_counts() {
    let cwd = temp_cwd("delcnt");
    let provider: Arc<dyn Provider> = Arc::new(ChildMock { attempt_write: false });
    let tool = Delegate::new(provider, Arc::new(read_only_registry()), 3);
    let ctx = Ctx::new(cwd.clone(), true);
    assert!(tool.run(json!({"tasks": []}), &ctx).await.is_err());
    let six = json!({"tasks": [
        {"prompt": "1"}, {"prompt": "2"}, {"prompt": "3"},
        {"prompt": "4"}, {"prompt": "5"}, {"prompt": "6"}
    ]});
    let err = tool.run(six, &ctx).await.unwrap_err().to_string();
    assert!(err.contains("maks 5"), "{err}");
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn read_only_registry_matches_plan_list() {
    let names = read_only_registry().names();
    assert_eq!(names.len(), selencode::agent::plan::READ_ONLY_TOOLS.len());
    for n in selencode::agent::plan::READ_ONLY_TOOLS {
        assert!(names.iter().any(|x| x == n), "{n} hilang dari registry baca-saja");
    }
    for banned in ["write_file", "run_shell", "delegate", "update_plan"] {
        assert!(!names.iter().any(|x| x == banned));
    }
}

// ---------- parser & TUI ----------

#[test]
fn slash_plan_parses() {
    assert_eq!(parse_slash_command("/plan"), SlashCommand::Plan);
    assert_eq!(parse_slash_command("  /plan  "), SlashCommand::Plan);
    assert_eq!(parse_slash_command("/planx"), SlashCommand::Unknown("planx".into()));
}

#[test]
fn tui_reducer_plan_events() {
    let mut app = App::new("/tmp/proyek".into(), "router".into(), "model-x".into(), 25, 0, 0);
    app.apply_event(UiEvent::PlanUpdated(
        "Rencana:\n[x] Baca kode\n[ ] Tes".into(),
    ));
    let line = app.lines.last().expect("blok plan harus ada");
    assert_eq!(line.kind, LineKind::Plan);
    assert!(line.text.contains("[x] Baca kode"), "{}", line.text);

    app.apply_event(UiEvent::PlanReady("## Rencana\n1. Satu".into()));
    assert_eq!(app.pending_plan.as_deref(), Some("## Rencana\n1. Satu"));
    assert!(!app.busy);
}
