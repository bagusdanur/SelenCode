//! Tes Tahap 1 — Fondasi Keandalan:
//! 1. Transactional Coding, Checkpoint, & Conflict-Safe Rollback
//! 2. Deteksi Toolchain Proyek & Parser Diagnostik Error Terstruktur
//! 3. Self-Correction Engine, LoopGuard, & Automatic Verification Gate end-to-end

use async_trait::async_trait;
use selencode::agent::corrector::{LoopGuard, LoopWarning, VerificationTracker};
use selencode::agent::run_turn;
use selencode::providers::{
    AssistantReply, Message, Provider, StreamEvent, ToolCall, ToolSpec,
};
use selencode::repl::{parse_slash_command, SlashCommand};
use selencode::tools::undo::{FileSnapshot, UndoStack};
use selencode::tools::verify::{detect_project_plan, parse_diagnostics, ProjectKind};
use selencode::tools::{default_registry, Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-s1-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

// ---------- 1. Transactional Checkpoints & Conflict-Safe Rollback ----------

#[test]
fn checkpoint_rollback_pulihkan_hanya_perubahan_agent_dan_hormati_edit_user() {
    let cwd = temp_dir("rollback");
    let a = cwd.join("a.rs");
    let b = cwd.join("b.rs");
    let c = cwd.join("baru.rs");
    std::fs::write(&a, "fn a() { 1 }\n").unwrap();
    std::fs::write(&b, "fn b() { 1 }\n").unwrap();

    let mut stack = UndoStack::default();
    let cp1 = stack.begin_checkpoint("sebelum refactor");

    // Agent mengedit a.rs, b.rs, dan membuat baru.rs.
    std::fs::write(&a, "fn a() { 2 }\n").unwrap();
    stack.record(FileSnapshot {
        path: a.clone(),
        rel: "a.rs".into(),
        before: Some(b"fn a() { 1 }\n".to_vec()),
        after: b"fn a() { 2 }\n".to_vec(),
    });
    std::fs::write(&b, "fn b() { 2 }\n").unwrap();
    stack.record(FileSnapshot {
        path: b.clone(),
        rel: "b.rs".into(),
        before: Some(b"fn b() { 1 }\n".to_vec()),
        after: b"fn b() { 2 }\n".to_vec(),
    });
    std::fs::write(&c, "fn baru() {}\n").unwrap();
    stack.record(FileSnapshot {
        path: c.clone(),
        rel: "baru.rs".into(),
        before: None,
        after: b"fn baru() {}\n".to_vec(),
    });

    assert_eq!(stack.files_changed_since(cp1), vec!["a.rs", "b.rs", "baru.rs"]);

    // User secara manual mengedit b.rs di luar SelenCode!
    std::fs::write(&b, "fn b() { 999 /* kerjaan manual user */ }\n").unwrap();

    let report = stack.rollback_to(Some(cp1)).unwrap();
    assert_eq!(report.checkpoint_id, cp1);
    // a.rs dipulihkan, baru.rs dihapus kembali, b.rs TIDAK ditimpa karena konflik eksternal!
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn a() { 1 }\n");
    assert!(!c.exists(), "file baru dari agent harus dihapus saat rollback");
    assert_eq!(
        std::fs::read_to_string(&b).unwrap(),
        "fn b() { 999 /* kerjaan manual user */ }\n",
        "pekerjaan manual user di b.rs TIDAK boleh ditimpa!"
    );
    assert_eq!(report.restored.len(), 2);
    assert_eq!(report.skipped_conflicts.len(), 1);
    assert!(report.skipped_conflicts[0].contains("b.rs"));

    std::fs::remove_dir_all(&cwd).ok();
}

// ---------- 2. Project Detection & Structured Diagnostics Parser ----------

#[test]
fn detect_project_plan_rust_node_python_go() {
    // Rust
    let rust_dir = temp_dir("det-rust");
    std::fs::write(rust_dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    let plan = detect_project_plan(&rust_dir, "all");
    assert_eq!(plan.kind, ProjectKind::Rust);
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps[0].command.contains("cargo check"));
    assert!(plan.steps[1].command.contains("cargo test"));
    std::fs::remove_dir_all(&rust_dir).ok();

    // Node + TypeScript
    let node_dir = temp_dir("det-node");
    std::fs::write(
        node_dir.join("package.json"),
        r#"{"scripts":{"test":"vitest","lint":"eslint ."}}"#,
    )
    .unwrap();
    std::fs::write(node_dir.join("tsconfig.json"), "{}").unwrap();
    let plan = detect_project_plan(&node_dir, "all");
    assert_eq!(plan.kind, ProjectKind::Node);
    assert!(plan.steps.iter().any(|s| s.command.contains("tsc --noEmit")));
    assert!(plan.steps.iter().any(|s| s.command.contains("npm test")));
    std::fs::remove_dir_all(&node_dir).ok();

    // Python
    let py_dir = temp_dir("det-py");
    std::fs::write(py_dir.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
    let plan = detect_project_plan(&py_dir, "test");
    assert_eq!(plan.kind, ProjectKind::Python);
    assert!(plan.steps[0].command.contains("pytest"));
    std::fs::remove_dir_all(&py_dir).ok();

    // Go
    let go_dir = temp_dir("det-go");
    std::fs::write(go_dir.join("go.mod"), "module example.com/x\n").unwrap();
    let plan = detect_project_plan(&go_dir, "all");
    assert_eq!(plan.kind, ProjectKind::Go);
    assert!(plan.steps.iter().any(|s| s.command.contains("go test")));
    std::fs::remove_dir_all(&go_dir).ok();
}

#[test]
fn parse_diagnostics_ekstrak_rust_ts_python_go_dan_tolak_secret_snippet() {
    let cwd = temp_dir("diag");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/demo.rs"),
        "fn main() {\n    let x: u32 = \"salah\";\n    println!(\"{x}\");\n}\n",
    )
    .unwrap();
    std::fs::write(cwd.join(".env"), "API_KEY=rahasia_super_penting\n").unwrap();

    let raw = "\
error[E0308]: mismatched types
 --> src/demo.rs:2:18
  |
2 |     let x: u32 = \"salah\";
  |                  ^^^^^^^ expected `u32`, found `&str`

src/app.ts(10,4): error TS2322: Type 'string' is not assignable to type 'number'.
  File \".env\", line 1, in <module>
    API_KEY=rahasia_super_penting
SyntaxError: invalid syntax
main.go:18:9: undefined: helperFn
";

    let diags = parse_diagnostics(&cwd, raw);
    assert_eq!(diags.len(), 4, "harus mengekstrak 4 diagnostik: {diags:?}");

    // Rust diagnostic + snippet dari src/demo.rs
    assert_eq!(diags[0].file, "src/demo.rs");
    assert_eq!(diags[0].line, 2);
    assert_eq!(diags[0].code.as_deref(), Some("E0308"));
    let sn = diags[0].snippet.as_deref().expect("snippet src/demo.rs ada");
    assert!(sn.contains("let x: u32"), "{sn}");

    // TypeScript
    assert_eq!(diags[1].file, "src/app.ts");
    assert_eq!(diags[1].code.as_deref(), Some("TS2322"));

    // Python menunjuk .env -> snippet WAJIB None agar secret tidak pernah dibaca!
    assert_eq!(diags[2].file, ".env");
    assert!(diags[2].snippet.is_none(), "snippet .env harus ditolak");

    // Go
    assert_eq!(diags[3].file, "main.go");
    assert_eq!(diags[3].line, 18);

    std::fs::remove_dir_all(&cwd).ok();
}

// ---------- 3. LoopGuard, SelfCorrectState, & End-to-End Verification Gate ----------

#[test]
fn loop_guard_dan_self_correct_gate_berjalan_sesuai_batas() {
    let mut lg = LoopGuard::new();
    let args = json!({"path": "src/a.rs"});
    assert_eq!(lg.record("read_file", &args, false), LoopWarning::None);
    assert_eq!(lg.record("read_file", &args, false), LoopWarning::None);
    assert!(matches!(
        lg.record("read_file", &args, false),
        LoopWarning::RepeatedCall { count: 3, .. }
    ));

    // Stagnasi error beruntun
    let mut lg2 = LoopGuard::new();
    for i in 0..3 {
        assert_eq!(
            lg2.record("run_shell", &json!({"command": format!("cmd{i}")}), true),
            LoopWarning::None
        );
    }
    assert!(matches!(
        lg2.record("run_shell", &json!({"command": "cmd3"}), true),
        LoopWarning::Stagnation {
            consecutive_errors: 4
        }
    ));

    // VerificationTracker deterministik melacak edit & status verifikasi (0 panggilan LLM tambahan)
    let mut sc = VerificationTracker::new(Some(1));
    sc.observe_tool("edit_file", &json!({"path": "src/lib.rs"}), "OK: src/lib.rs diedit.", false);
    assert!(sc.edited_code_file);
    assert!(!sc.verified_after_last_edit);
    assert!(sc
        .closing_verification_summary_line(true)
        .unwrap()
        .contains("belum diverifikasi"));

    // Setelah verify_project LULUS -> tidak ada catatan peringatan penutup
    let mut sc2 = VerificationTracker::new(Some(2));
    sc2.observe_tool("write_file", &json!({"path": "src/new.rs"}), "OK: ditulis.", false);
    sc2.observe_tool("verify_project", &json!({"scope": "check"}), "Verifikasi proyek [Rust]: LULUS ✅", false);
    assert!(sc2.verified_after_last_edit);
    assert!(sc2.honest_unverified_closing_note("Selesai.").is_none());

    // Slash commands Tahap 1
    assert_eq!(parse_slash_command("/checkpoint"), SlashCommand::Checkpoint(None));
    assert_eq!(
        parse_slash_command("/cp sebelum-edit"),
        SlashCommand::Checkpoint(Some("sebelum-edit".into()))
    );
    assert_eq!(
        parse_slash_command("/rollback 2"),
        SlashCommand::Rollback(Some("2".into()))
    );
    assert_eq!(
        parse_slash_command("/verify check"),
        SlashCommand::Verify(Some("check".into()))
    );
}

/// Mock provider untuk menguji pelacakan verifikasi penutup deterministik (v0.5.24):
/// - Panggilan 0: edit file `app.rs`
/// - Panggilan 1: menjawab selesai tanpa verifikasi -> TIDAK ada loop refleksi tambahan,
///   jawaban akhir dikirim tepat 1x disertai catatan penutup jujur + checkpoint otomatis tersimpan!
struct SelfCorrectMock {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for SelfCorrectMock {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        // v0.5.24: tidak boleh pernah menyuntikkan [Self-Correction Gate ...]
        assert!(
            !msgs.iter().any(|m| m.plain_text().contains("[Self-Correction Gate")),
            "v0.5.24 menghapus loop refleksi Self-Correction Gate"
        );
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "e1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "app.rs", "content": "fn main() {}\n"}),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            }),
            _ => {
                let _ = tx.send(StreamEvent::Token("Selesai menulis app.rs.".into())).await;
                Ok(AssistantReply {
                    text: "Selesai menulis app.rs.".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }
}

#[tokio::test]
async fn self_correction_gate_end_to_end_menahan_klaim_selesai_sebelum_verifikasi() {
    let cwd = temp_dir("e2e-sc");
    let reg = default_registry();
    assert!(reg.get("verify_project").is_some(), "verify_project terdaftar di registry");
    // Pastikan verify_project tidak masuk read_only_registry.
    assert!(selencode::tools::read_only_registry().get("verify_project").is_none());

    let ctx = Ctx::new(cwd.clone(), true).with_self_correct(true);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(SelfCorrectMock {
        calls: calls.clone(),
    });
    let mut messages = Vec::new();
    let out = run_turn(provider, &reg, &ctx, &mut messages, "buat app.rs", 10)
        .await
        .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "harus tepat 2 panggilan (1 tool + 1 penutup), 0 putaran refleksi tambahan"
    );
    assert!(
        out.starts_with("Selesai menulis app.rs."),
        "jawaban akhir harus dikirim sekali apa adanya + catatan deterministik: {out}"
    );
    assert!(
        out.contains("belum diverifikasi"),
        "ringkasan penutup deterministik wajib mencatat status belum diverifikasi: {out}"
    );
    // Pastikan checkpoint otomatis giliran tersimpan karena app.rs diubah!
    let cps = ctx.undo_stack.lock().unwrap().list_checkpoints();
    assert_eq!(cps.len(), 1, "checkpoint giliran otomatis harus tersimpan");
    assert_eq!(cps[0].1, 1, "1 file diubah sejak checkpoint");

    // Pastikan tool verify_project tersedia & punya schema valid.
    let vp = reg.get("verify_project").unwrap();
    assert_eq!(vp.spec().name, "verify_project");

    std::fs::remove_dir_all(&cwd).ok();
}
