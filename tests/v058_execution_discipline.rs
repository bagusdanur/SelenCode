//! Suite uji penerimaan terpadu SelenCode v0.5.8:
//! - Perbaikan 1: Tumpukan 3 lapis SelenCode (Lapis A prompt disiplin + opencode guard, Lapis A2 `folder_map`
//!   tree-sitter <=300 token & kosong di folder hampa, Lapis B deskripsi skill investigasi, Lapis C
//!   aturan ulang/resume job, serta riset ter-batch paralel tidak dihukum anti-spin).
//! - Perbaikan 2: Ringkasan penutup wajib saat menabrak `max_steps` (via model & fallback templat saat
//!   model gagal; pesan terakhir DB selalu Assistant, bukan ToolResult; giliran normal tanpa duplikat)
//!   + daftar periksa penutup tugas coding ala SWE-agent `review_on_submit`.
//! - Perbaikan 3: Tampilan aksi tool terstruktur (`prepare_tool_preview`, `format_tool_event`, redaksi
//!   argumen rahasia di progres & blok akhir, penggabungan pengulangan berturut `×N`, batas <=10 baris
//!   dengan seluruh baris gagal tetap tampil beralasan, giliran <=3 aksi tanpa penggabungan paksa).

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use selencode::agent::context::{build_folder_map, build_system_prompt};
use selencode::agent::corrector::SelfCorrectState;
use selencode::agent::run_turn_counted;
use selencode::config::ContextCfg;
use selencode::providers::{
    AssistantReply, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::skills::{scan_skills_in_dirs, seed_builtin_skills_to, SKILL_INDEX_DESC_MAX_CHARS};
use selencode::store::Store;
use selencode::telegram::format_telegram_progress_html;
use selencode::tool_display::{
    build_action_summary_lines, build_template_closing_summary, format_tool_event,
    prepare_tool_preview, redact_tool_args_for_display, short_error_reason, ToolActionEvent,
    DEFAULT_ACTION_BLOCK_MAX_LINES, DEFAULT_TOOL_PREVIEW_LENGTH,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v058-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------- PERBAIKAN 1: Tumpukan 3 lapis SelenCode + folder_map Aider/SWE-agent ----------

#[test]
fn perbaikan1_three_layer_prompt_folder_map_and_skill_trigger() {
    let dir = temp_dir("p1-map");
    // Folder kosong -> folder_map wajib kosong (0 token pada sesi trivial)
    assert_eq!(build_folder_map(&dir, 300), "");

    // Tambah berkas proyek nyata + berkas rahasia yang harus diabaikan
    std::fs::write(dir.join("README.md"), "# Queue Worker\n").unwrap();
    std::fs::write(dir.join(".env"), "SECRET_KEY=supersecret\n").unwrap();
    std::fs::write(
        dir.join("queue_worker.py"),
        "def process_job(job_id, resume=True):\n    return True\n\ndef requeue_job(job_id):\n    return job_id\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("job_queue.py"),
        "def show_job(job_id):\n    pass\n\ndef enqueue(title):\n    pass\n",
    )
    .unwrap();
    for i in 0..30 {
        std::fs::write(dir.join(format!("extra_{i:02}.txt")), format!("data {i}\n")).unwrap();
    }

    let fmap = build_folder_map(&dir, 300);
    assert!(fmap.contains("Peta folder kerja:"));
    assert!(fmap.contains("queue_worker.py"));
    assert!(fmap.contains("process_job"));
    assert!(fmap.contains("job_queue.py"));
    assert!(!fmap.contains(".env"), "file rahasia tidak boleh masuk folder_map");
    let est_tok = fmap.chars().count().div_ceil(3);
    assert!(est_tok <= 330, "folder_map ({est_tok} tok) harus mematuhi pagu ketat ~300 token");
    assert_eq!(build_folder_map(&dir, 0), "");

    // Lapis A + B + C hadir di build_system_prompt
    let sys = build_system_prompt(&dir, &[]);
    assert!(sys.contains("SATU giliran paralel"));
    assert!(sys.contains("--help"));
    assert!(sys.contains("A successful tool call is not a successful task"));
    assert!(sys.contains("kerjaan user"));
    assert!(sys.contains("systematic-debugging"));
    assert!(sys.contains("requeue"));

    // Lapis B: deskripsi indeks `systematic-debugging` cocok dengan kasus job gagal/ulang & <=60 char
    let skills_root = dir.join("skills");
    seed_builtin_skills_to(&skills_root);
    let scan = scan_skills_in_dirs(&dir.join("__no_proj__"), Some(&skills_root));
    let dbg = scan
        .skills
        .iter()
        .find(|s| s.name == "systematic-debugging")
        .unwrap();
    assert!(dbg.summary.chars().count() <= SKILL_INDEX_DESC_MAX_CHARS);
    assert!(dbg.summary.contains("job gagal"));

    let _ = std::fs::remove_dir_all(&dir);
}

struct BatchedReadProv {
    step: AtomicUsize,
    saw_anti_spin: Arc<Mutex<bool>>,
}

#[async_trait]
impl Provider for BatchedReadProv {
    fn model_name(&self) -> &str {
        "batch-mock"
    }
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let joined: String = msgs.iter().map(|m| m.plain_text()).collect::<Vec<_>>().join("\n");
        if joined.contains("[GUARD ANTI-BERPUTAR]") {
            *self.saw_anti_spin.lock().unwrap() = true;
        }
        let n = self.step.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // 1 giliran assistant melakukan 5 pencarian/list paralel ter-batch sekaligus!
            Ok(AssistantReply {
                text: String::new(),
                tool_calls: (0..5)
                    .map(|i| ToolCall {
                        id: format!("b{i}"),
                        name: "search_text".into(),
                        args: json!({"query": format!("kunci_{i}")}),
                    })
                    .collect(),
                finish_reason: Some("tool_calls".into()),
                usage: Some(Usage { input_tokens: 300, output_tokens: 40, ..Default::default() }),
            })
        } else {
            Ok(AssistantReply {
                text: "Riset ter-batch selesai.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage { input_tokens: 350, output_tokens: 30, ..Default::default() }),
            })
        }
    }
}

#[tokio::test]
async fn perbaikan1_batched_reads_not_penalized_by_antispin() {
    let dir = temp_dir("p1-batch");
    std::fs::write(dir.join("a.txt"), "kunci_0\nkunci_1\n").unwrap();
    let saw_spin = Arc::new(Mutex::new(false));
    let prov: Arc<dyn Provider> = Arc::new(BatchedReadProv {
        step: AtomicUsize::new(0),
        saw_anti_spin: saw_spin.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true).with_context_cfg(ContextCfg {
        folder_map: false,
        folder_map_max_tokens: 300,
    });
    let mut msgs = Vec::new();
    let (ans, _) = run_turn_counted(prov, &reg, &ctx, &mut msgs, "Teliti semua kunci", 6)
        .await
        .unwrap();
    assert!(ans.contains("selesai"));
    assert!(
        !*saw_spin.lock().unwrap(),
        "5 pencarian paralel dalam 1 giliran ter-batch TIDAK boleh langsung memicu guard anti-berputar!"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 2: Ringkasan penutup wajib + Daftar periksa SWE-agent ----------

struct TruncatingProv {
    fail_closing: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for TruncatingProv {
    fn model_name(&self) -> &str {
        "trunc-mock"
    }
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        // Panggilan penutup khusus dikirim dengan `tools = []`.
        if tools.is_empty() {
            if self.fail_closing {
                return Err(anyhow!("provider unavailable on closing"));
            }
            return Ok(AssistantReply {
                text: "Ringkasan penutup: 2 aksi list_files selesai; langkah berikutnya lanjutkan inspeksi."
                    .into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage { input_tokens: 200, output_tokens: 40, ..Default::default() }),
            });
        }
        Ok(AssistantReply {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("t{n}"),
                name: "list_files".into(),
                args: json!({}),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: Some(Usage { input_tokens: 220, output_tokens: 25, ..Default::default() }),
        })
    }
}

#[tokio::test]
async fn perbaikan2_max_steps_closing_summary_model_and_template_fallback() {
    let dir = temp_dir("p2-closing");
    std::fs::write(dir.join("note.txt"), "halo").unwrap();
    let reg = default_registry();

    // Kasus A: Model masih bisa dipanggil untuk ringkasan penutup (tools = [])
    let store_a = Arc::new(Store::open_at(&dir.join("a.db")).unwrap());
    let sid_a = store_a
        .create_session(&dir.to_string_lossy(), "mock", "trunc-mock", "uji max_steps A")
        .unwrap();
    let ctx_a = Ctx::new(dir.clone(), true).with_store(store_a.clone(), sid_a.clone());
    let prov_a: Arc<dyn Provider> = Arc::new(TruncatingProv {
        fail_closing: false,
        calls: AtomicUsize::new(0),
    });
    let mut msgs_a = Vec::new();
    let (ans_a, _) = run_turn_counted(prov_a, &reg, &ctx_a, &mut msgs_a, "Kerjakan banyak langkah", 2)
        .await
        .unwrap();
    assert!(ans_a.contains("Ringkasan penutup"));
    let saved_a = store_a.load_messages(&sid_a).unwrap();
    assert_eq!(
        saved_a.last().unwrap().role,
        Role::Assistant,
        "pesan terakhir di DB wajib Assistant (ringkasan penutup), bukan ToolResult!"
    );

    // Kasus B: Model GAGAL saat ringkasan penutup -> fallback templat deterministik tetap tiba!
    let store_b = Arc::new(Store::open_at(&dir.join("b.db")).unwrap());
    let sid_b = store_b
        .create_session(&dir.to_string_lossy(), "mock", "trunc-mock", "uji max_steps B")
        .unwrap();
    let ctx_b = Ctx::new(dir.clone(), true).with_store(store_b.clone(), sid_b.clone());
    let prov_b: Arc<dyn Provider> = Arc::new(TruncatingProv {
        fail_closing: true,
        calls: AtomicUsize::new(0),
    });
    let mut msgs_b = Vec::new();
    let (ans_b, _) = run_turn_counted(prov_b, &reg, &ctx_b, &mut msgs_b, "Kerjakan banyak langkah", 2)
        .await
        .unwrap();
    assert!(ans_b.contains("Ringkasan penutup"));
    assert!(ans_b.contains("2 aksi tool"));
    let saved_b = store_b.load_messages(&sid_b).unwrap();
    assert_eq!(saved_b.last().unwrap().role, Role::Assistant);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn perbaikan2_swe_agent_coding_closing_checklist() {
    let dir = temp_dir("p2-swe");
    std::fs::write(dir.join("repro_bug.js"), "console.log('repro');").unwrap();
    let mut st = SelfCorrectState::new(None).with_max_attempts(1);

    // Edit file kode + buat skrip repro sementara
    st.observe_tool("edit_file", &json!({"path": "pricing.js"}), "OK: pricing.js diedit.", false);
    st.observe_tool("write_file", &json!({"path": "repro_bug.js"}), "OK: repro_bug.js ditulis.", false);

    // (a) Belum diverifikasi sesudah edit terakhir -> gate menolak selesai
    let g1 = st.completion_gate_with_checklist(4, "Selesai.", &dir);
    assert!(g1.as_deref().unwrap().contains("belum menjalankan verifikasi"));

    // Jalankan verifikasi sesudah edit terakhir
    st.observe_tool("run_shell", &json!({"command": "node --check pricing.js"}), "exit code: 0\nOK", false);

    // (b) & (c) Sudah terverifikasi, tetapi repro_bug.js masih ada & jawaban belum menyebut pricing.js -> checklist memicu 1x
    let g2 = st.completion_gate_with_checklist(4, "Sudah beres semua.", &dir);
    let msg2 = g2.expect("checklist penutup harus mengingatkan berkas repro & daftar file diubah");
    assert!(msg2.contains("repro_bug.js"));
    assert!(msg2.contains("pricing.js"));

    // Sesudah checklist dicek sekali -> tidak mengulang lagi (anti-loop)
    assert!(st.completion_gate_with_checklist(4, "Perubahan pada pricing.js lulus; repro_bug.js dihapus.", &dir).is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 3: Tampilan aksi tool terstruktur + blok ringkasan <=10 baris ----------

#[test]
fn perbaikan3_tool_preview_redaction_progress_and_collapsed_action_block() {
    // 1. Peta argumen utama & pemotongan 40 char
    assert_eq!(
        format_tool_event("read_file", &json!({"path": "src/main.rs"}), DEFAULT_TOOL_PREVIEW_LENGTH),
        "read_file — src/main.rs"
    );
    assert_eq!(
        format_tool_event("search_text", &json!({"query": "resume=True"}), DEFAULT_TOOL_PREVIEW_LENGTH),
        "search_text — resume=True"
    );
    assert_eq!(
        format_tool_event("skill_view", &json!({"name": "systematic-debugging"}), DEFAULT_TOOL_PREVIEW_LENGTH),
        "skill_view — systematic-debugging"
    );
    assert_eq!(
        format_tool_event("delegate", &json!({"tasks": [{"prompt": "Periksa modul auth"}]}), DEFAULT_TOOL_PREVIEW_LENGTH),
        "delegate — Periksa modul auth"
    );
    assert_eq!(
        format_tool_event("list_files", &json!({}), DEFAULT_TOOL_PREVIEW_LENGTH),
        "list_files"
    );
    let long_cmd = "python scripts/job_queue.py show 6c3321a9-9988-7766-5544-33221100aabb --verbose";
    let prev = prepare_tool_preview("run_shell", &json!({"command": long_cmd}), 40);
    assert!(prev.chars().count() <= 40);
    assert!(prev.ends_with('…'));

    // 2. Redaksi rahasia pada pratinjau & gelembung progres Telegram
    let secret_cmd = "curl -H 'Authorization: Bearer sk-live1234567890abcdef' --token mySuperSecret99 https://api.test";
    let red_prev = prepare_tool_preview("run_shell", &json!({"command": secret_cmd}), 120);
    assert!(!red_prev.contains("sk-live1234567890abcdef"));
    assert!(!red_prev.contains("mySuperSecret99"));
    assert!(redact_tool_args_for_display("API_KEY=rahasia123").contains("[REDACTED]"));

    let prog_html = format_telegram_progress_html(
        3,
        25,
        Some(&format!("run_shell — {red_prev}")),
        4,
        "",
    );
    assert!(prog_html.contains("<code>run_shell</code> — <code>"));
    assert!(!prog_html.contains("mySuperSecret99"));

    // 3. Giliran <= 3 aksi: tampil apa adanya tanpa penggabungan paksa
    let small = vec![
        ToolActionEvent { name: "read_file".into(), preview: "a.py".into(), is_error: false, error_reason: None },
        ToolActionEvent { name: "read_file".into(), preview: "a.py".into(), is_error: false, error_reason: None },
        ToolActionEvent { name: "edit_file".into(), preview: "a.py".into(), is_error: false, error_reason: None },
    ];
    let (tot_s, lines_s) = build_action_summary_lines(&small, DEFAULT_ACTION_BLOCK_MAX_LINES);
    assert_eq!(tot_s, 3);
    assert_eq!(lines_s.len(), 3);
    assert!(!lines_s[0].contains('×'));

    // 4. Giliran 31 aksi mayoritas `run_shell` serupa + 2 kegagalan:
    //    judul = 31, baris <= 10, pengulangan tergabung `×N`, 2 baris gagal WAJIB tampil beralasan!
    let mut big = Vec::new();
    big.push(ToolActionEvent { name: "skill_view".into(), preview: "systematic-debugging".into(), is_error: false, error_reason: None });
    big.push(ToolActionEvent { name: "read_file".into(), preview: "queue_worker.py".into(), is_error: false, error_reason: None });
    for _ in 0..8 {
        big.push(ToolActionEvent {
            name: "run_shell".into(),
            preview: "python job_queue.py show 6c3321a9".into(),
            is_error: false,
            error_reason: None,
        });
    }
    big.push(ToolActionEvent {
        name: "run_shell".into(),
        preview: "python ocr_v5.py --page 5".into(),
        is_error: true,
        error_reason: Some(short_error_reason("ERROR: V5 quality check failed on page 5")),
    });
    for i in 0..10 {
        big.push(ToolActionEvent {
            name: "read_file".into(),
            preview: format!("metadata/page-{i:02}/pipeline-status.json"),
            is_error: false,
            error_reason: None,
        });
    }
    for _ in 0..7 {
        big.push(ToolActionEvent {
            name: "run_shell".into(),
            preview: "python job_queue.py requeue 6c3321a9".into(),
            is_error: false,
            error_reason: None,
        });
    }
    big.push(ToolActionEvent {
        name: "run_shell".into(),
        preview: "python verify_output.py --strict".into(),
        is_error: true,
        error_reason: Some(short_error_reason("Error: missing output zip")),
    });
    big.push(ToolActionEvent {
        name: "run_shell".into(),
        preview: "python job_queue.py show 6c3321a9".into(),
        is_error: false,
        error_reason: None,
    });
    big.push(ToolActionEvent {
        name: "watch_job".into(),
        preview: "Comic-auto Hold On".into(),
        is_error: false,
        error_reason: None,
    });
    assert_eq!(big.len(), 31);

    let (tot_b, lines_b) = build_action_summary_lines(&big, DEFAULT_ACTION_BLOCK_MAX_LINES);
    assert_eq!(tot_b, 31);
    assert!(
        lines_b.len() <= 10,
        "blok aksi ({}) wajib <= 10 baris:\n{}",
        lines_b.len(),
        lines_b.join("\n")
    );
    let joined_b = lines_b.join("\n");
    assert!(joined_b.contains("run_shell ×8 — python job_queue.py show 6c3321a9"));
    assert!(joined_b.contains("V5 quality check failed on page 5"));
    assert!(joined_b.contains("missing output zip"));
    assert!(joined_b.contains("aksi sukses lainnya"));

    // Template closing summary tidak pernah kosong
    let tpl = build_template_closing_summary(&big, "batas langkah (25) tercapai");
    assert!(tpl.contains("31 aksi tool (29 berhasil, 2 gagal)"));
    assert!(tpl.contains("missing output zip"));
}
