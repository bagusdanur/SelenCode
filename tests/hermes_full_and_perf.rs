//! Tes menyeluruh Paket Hemat Token ala Hermes (Tahap 0–6) + Bagian A (Performa TUI Laptop Kentang).

use async_trait::async_trait;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use selencode::agent::context::{
    cap_dynamic_injection, compact_messages_hermes, compression_summary_budget, head_tail_truncate,
    prune_old_tool_results_with_archive, spillover_if_needed, spillover_with_preview,
    AuxCooldownState, HERMES_SUMMARY_MARKER,
};
use selencode::agent::corrector::{
    canonical_tool_fingerprint, LoopGuard, SelfCorrectState, ToolDedupCache,
};
use selencode::agent::plan::TurnMode;
use selencode::agent::review::{run_self_improvement_review, AUTO_REVIEW_MIN_TOOLS};
use selencode::providers::{
    anthropic, AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::store::{CallTokenMetric, Store};
use selencode::tools::toolsets::{
    active_tool_specs, active_tool_specs_with_mcp, build_mcp_compact_index,
    select_initial_toolsets, LOAD_MCP_TOOLS_NAME,
};
use selencode::tools::{default_registry, Ctx, Tool};
use selencode::tui::app::{App, LineKind, TranscriptLine};
use selencode::tui::draw;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

#[test]
fn tahap0_token_metrics_instrumentation_roundtrip() {
    let db = std::env::temp_dir().join(format!(
        "selen-t0-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&db).unwrap();
    let sid = store.create_session("/tmp", "router", "m", "t0").unwrap();
    store
        .record_call_token_metric(&CallTokenMetric {
            session_id: sid.clone(),
            model: "m".into(),
            est_system_tokens: 400,
            est_tools_tokens: 700,
            est_history_tokens: 300,
            est_tool_results_tokens: 200,
            actual_input_tokens: 1650,
            actual_output_tokens: 120,
            active_tools_count: 9,
            pruned_count: 2,
            deduped_count: 1,
        })
        .unwrap();
    let rep = store.session_token_metrics(&sid).unwrap();
    assert_eq!(rep.calls, 1);
    assert_eq!(rep.avg_system_tokens, 400);
    assert_eq!(rep.avg_tools_tokens, 700);
    assert_eq!(rep.total_actual_input, 1650);
    assert_eq!(rep.total_pruned, 2);
    assert_eq!(rep.total_deduped, 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn tahap1_prune_archives_to_file_protects_last_20_and_is_idempotent() {
    let arch_dir = std::env::temp_dir().join(format!(
        "selen-t1-arch-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut msgs = vec![
        Message::text(Role::System, "sys prompt"),
        Message::text(Role::User, "baca file besar"),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "call_old_1".into(),
                name: "read_file".into(),
                args: json!({"path": "big.rs"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "call_old_1".into(),
                content: format!("Baris pertama file big.rs\n{}", "A".repeat(800)),
                is_error: false,
            }],
        },
    ];
    // Tambahkan 20 pesan baru agar pesan tool di atas berada tepat di luar 20 pesan terakhir!
    for i in 0..20 {
        msgs.push(Message::text(Role::User, format!("pesan baru {i}")));
    }
    // Juga pasang 1 hasil tool besar di dalam 20 pesan terakhir (HARUS dilindungi!).
    msgs[22] = Message {
        role: Role::Tool,
        content: vec![Block::ToolResult {
            id: "call_recent".into(),
            content: "R".repeat(800),
            is_error: false,
        }],
    };

    let pruned1 = prune_old_tool_results_with_archive(&mut msgs, &arch_dir, 20);
    assert_eq!(pruned1, 1);

    // Stub berisi nama tool, path arsip, dan cuplikan awal; file arsip berisi teks utuh!
    let stub = match &msgs[3].content[0] {
        Block::ToolResult { content, .. } => content.clone(),
        _ => panic!("harus ToolResult"),
    };
    assert!(stub.starts_with("[Hasil read_file diringkas — arsip: "), "{stub}");
    assert!(stub.contains("Baris pertama file big.rs"), "{stub}");
    // Hasil tool di dalam 20 pesan terakhir TIDAK tersentuh!
    let recent_len = match &msgs[22].content[0] {
        Block::ToolResult { content, .. } => content.len(),
        _ => 0,
    };
    assert_eq!(recent_len, 800);

    // Pindaian kedua WAJIB idempoten (0 pruned lagi!).
    let pruned2 = prune_old_tool_results_with_archive(&mut msgs, &arch_dir, 20);
    assert_eq!(pruned2, 0);

    // File arsip benar-benar tertulis di disk.
    let files: Vec<_> = std::fs::read_dir(&arch_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1);
    let archived = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(archived.contains(&"A".repeat(400)));

    let _ = std::fs::remove_dir_all(&arch_dir);
}

#[test]
fn tahap2_canonical_fingerprint_dedup_and_self_correct_rem() {
    // JSON dengan urutan kunci berbeda menghasilkan fingerprint identik.
    let fp1 = canonical_tool_fingerprint("read_file", &json!({"path": "a.rs", "start_line": 1}));
    let fp2 = canonical_tool_fingerprint("read_file", &json!({"start_line": 1, "path": "a.rs"}));
    assert_eq!(fp1, fp2);

    let dir = std::env::temp_dir().join(format!(
        "selen-t2-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();

    let mut dedup = ToolDedupCache::new();
    let rf_args = json!({"path": "a.rs"});
    dedup.record_success("read_file", &rf_args, "fn a() {}", &dir);
    assert!(dedup.check_hit("read_file", &rf_args, &dir).is_some());

    // Begitu file diubah di disk -> dedup invalid dan izinkan baca ulang!
    std::fs::write(dir.join("a.rs"), "fn a() { println!(\"berubah\"); }\n").unwrap();
    assert!(dedup.check_hit("read_file", &rf_args, &dir).is_none());

    // Dedup run_shell yang sukses -> panggilan identik langsung di-dedup; invalidasi saat file diedit.
    let sh_args = json!({"command": "cargo test"});
    dedup.record_success("run_shell", &sh_args, "exit code: 0\nok", &dir);
    assert!(dedup.check_hit("run_shell", &sh_args, &dir).is_some());
    dedup.invalidate_on_edit(Some("a.rs"));
    assert!(dedup.check_hit("run_shell", &sh_args, &dir).is_none());

    // Rem Self-correction: maks 1 putaran!
    let mut sc = SelfCorrectState::new(None).with_max_attempts(1);
    sc.observe_tool("edit_file", &json!({"path": "a.rs"}), "ok", false);
    assert!(sc.completion_gate(5).is_some()); // putaran 1/1
    assert!(sc.completion_gate(5).is_none()); // putaran ke-2 ditahan!

    // LoopGuard: 2x gagal identik -> peringatan, dan 3x gagal tool sama (arg berbeda) pada guard Hermes!
    let mut lg = LoopGuard::with_hermes_guards();
    let _ = lg.record("run_shell", &sh_args, true);
    assert!(lg.record("run_shell", &sh_args, true).hint_message().is_some());
    let mut lg_same_tool = LoopGuard::with_hermes_guards();
    assert!(lg_same_tool.record("run_shell", &json!({"command": "e1"}), true).hint_message().is_none());
    assert!(lg_same_tool.record("run_shell", &json!({"command": "e2"}), true).hint_message().is_none());
    assert!(lg_same_tool.record("run_shell", &json!({"command": "e3"}), true).hint_message().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tahap3_and_tahap4_anthropic_cache_breakpoints_and_shell_head_tail_spillover() {
    let msgs = vec![
        Message::text(Role::System, "Kamu Selena"),
        Message::text(Role::User, "p1"),
        Message::text(Role::Assistant, "a1"),
        Message::text(Role::User, "p2"),
        Message::text(Role::Assistant, "a2"),
    ];
    let mut body = anthropic::build_request(&msgs, &[], "claude", 1024);
    anthropic::apply_prompt_caching(&mut body);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    // 3 pesan non-system terakhir mendapat breakpoint cache_control (system_and_3).
    assert_eq!(body["messages"][1]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][2]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][3]["content"][0]["cache_control"]["type"], "ephemeral");

    // Tahap 4: 40% kepala + 60% ekor menjaga pesan error di akhir output shell!
    let shell_out = format!("START_MARKER\n{}\nEND_ERROR_MARKER", "M".repeat(5000));
    let cut = head_tail_truncate(&shell_out, 1000);
    assert!(cut.contains("START_MARKER"));
    assert!(cut.contains("END_ERROR_MARKER"));
    assert!(cut.contains("40% kepala / 60% ekor"));

    // Spillover menulis arsip utuh dan mengembalikan path.
    let big = format!("HEAD_X\n{}\nTAIL_Y", "Z".repeat(6000));
    let spilled = spillover_if_needed("sess-t4", "c1", "read_file", &big, 2000).unwrap();
    assert!(spilled.contains("[Spillover `read_file`"));
    assert!(spilled.contains("HEAD_X"));
    assert!(spilled.contains("TAIL_Y"));
}

#[test]
fn tahap5_and_tahap6_hermes_compression_template_aux_cooldown_and_mcp_index() {
    assert_eq!(compression_summary_budget(20_000), 4_000);
    assert_eq!(compression_summary_budget(1_000), 2_000);
    assert_eq!(compression_summary_budget(200_000), 12_000);

    let mut cd = AuxCooldownState::default();
    assert_eq!(cd.record_failure(), 60);
    assert_eq!(cd.record_failure(), 300);
    assert_eq!(cd.record_failure(), 900);
    assert!(cd.is_in_cooldown());
    cd.record_success();
    assert!(!cd.is_in_cooldown());

    // Buat 28 pesan -> compact_messages_hermes lindungi 3 awal + 20 akhir & pakai template tetap!
    let mut msgs = vec![Message::text(Role::System, "sys")];
    for i in 0..27 {
        msgs.push(Message::text(
            if i % 2 == 0 { Role::User } else { Role::Assistant },
            format!("pesan ke-{i}"),
        ));
    }
    let compacted = compact_messages_hermes(&msgs);
    assert_eq!(compacted.len(), 24); // 3 head + 1 summary + 20 tail
    let sum_text = compacted[3].plain_text();
    for section in [
        HERMES_SUMMARY_MARKER,
        "## Goal",
        "## Constraints & Preferences",
        "## Progress",
        "## Key Decisions",
        "## Relevant Files",
        "## Next Steps",
        "## Critical Context",
    ] {
        assert!(sum_text.contains(section), "section {section} hilang");
    }

    // Tahap 6: cap injeksi dinamis <= 1.500 token & indeks on-demand MCP saat >15 tool MCP!
    let big_dyn = "D".repeat(10_000);
    let capped = cap_dynamic_injection(&big_dyn, 1_500);
    assert!(capped.chars().count() <= 6_100);

    struct DummyMcp(String);
    #[async_trait]
    impl Tool for DummyMcp {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.clone(),
                description: "Deskripsi panjang tool MCP untuk pengujian indeks ringkas 60 karakter".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &Ctx) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }
    let mut reg = default_registry();
    for i in 0..18 {
        reg = reg.register(Arc::new(DummyMcp(format!("mcp__srv__tool_{i}"))));
    }
    let idx = build_mcp_compact_index(&reg).expect("indeks MCP aktif untuk >15 tool");
    assert!(idx.contains("18 tool"));
    let specs = active_tool_specs_with_mcp(&reg, &HashSet::new(), &HashSet::new(), TurnMode::Execute);
    assert!(specs.iter().any(|s| s.name == LOAD_MCP_TOOLS_NAME));
    assert!(!specs.iter().any(|s| s.name.starts_with("mcp__")));
}

#[test]
fn bagian_a_tui_per_message_render_cache_handles_150_messages_fast() {
    let mut app = App::new("/tmp".into(), "router".into(), "m".into(), 10, 0, 0);
    for i in 0..150 {
        app.lines.push(TranscriptLine::simple(
            if i % 2 == 0 {
                LineKind::User
            } else {
                LineKind::Assistant
            },
            format!("Pesan ke-{i} dengan **markdown** dan `kode_{i}` untuk uji cache render."),
        ));
    }
    let backend = TestBackend::new(100, 30);
    let mut term = Terminal::new(backend).unwrap();
    // Frame 1: mengisi cache semua baris.
    term.draw(|f| draw(f, &mut app)).unwrap();
    assert!(app.lines.iter().all(|l| l.cached_width == 100));

    // Frame 2 (simulasi ketikan di input/setup): 100% cache hit, sangat cepat!
    let t0 = std::time::Instant::now();
    for _ in 0..20 {
        term.draw(|f| draw(f, &mut app)).unwrap();
    }
    let avg_ms = t0.elapsed().as_secs_f64() * 1000.0 / 20.0;
    assert!(
        avg_ms < 50.0,
        "redraw cache 150 pesan harus sangat ringan (<50ms di CI, aktual {avg_ms:.2}ms)"
    );
}

#[tokio::test]
async fn regression_fixes_shell_spillover_lean_toolsets_and_review_metrics() {
    // 1. Spillover run_shell 66.000 char menyimpan arsip UTUH (termasuk baris tengah & baris 2000)
    //    dan pratinjau menyertakan kepala + ekor sejati + petunjuk search_text/read_file!
    let mut big_shell = String::new();
    for i in 1..=2000 {
        if i == 750 {
            big_shell.push_str("BARIS-0750: FAKTA-TENGAH-RAHASIA\n");
        } else {
            big_shell.push_str(&format!("BARIS-{i:04}: isi log panjang untuk uji spillover shell\n"));
        }
    }
    assert!(big_shell.chars().count() > 60_000);
    let spilled = spillover_with_preview("sess-reg-sh", "sh1", "run_shell", &big_shell, 50_000, 2_400)
        .expect("run_shell >50k harus spillover");
    assert!(spilled.contains("[Spillover `run_shell`"));
    assert!(spilled.contains("BARIS-0001"));
    assert!(spilled.contains("BARIS-2000"), "ekor sejati baris 2000 harus ada: {spilled}");
    assert!(spilled.contains("search_text"));

    // 2. Prompt panjang 220 char & prompt resume di folder polos TIDAK boleh membengkak jadi 11-13 tool!
    let plain_dir = std::env::temp_dir().join(format!(
        "selen-plain-ts-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&plain_dir).unwrap();
    let reg = default_registry();
    let long_task = "Buat program Python rekap.py yang membaca file contoh.csv berisi kolom nama dan nilai untuk 5 siswa, hitung rata-rata nilai dengan 2 desimal, tampilkan siswa dengan nilai tertinggi, lalu jalankan skripnya untuk memastikan hasilnya tepat.";
    assert!(long_task.chars().count() > 200);
    let ts_long = select_initial_toolsets(long_task, &plain_dir, false, TurnMode::Execute);
    let specs_long = active_tool_specs(&reg, &ts_long, TurnMode::Execute);
    assert!(
        specs_long.len() <= 7,
        "tugas panjang di folder polos harus tetap <=7 tool (bukan 13!), dapat {}: {:?}",
        specs_long.len(),
        specs_long.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );
    let resume_prompt = "apa kata sandi yang kita simpan di kata.txt dan apa pekerjaan terakhir yang baru kita selesaikan?";
    let ts_res = select_initial_toolsets(resume_prompt, &plain_dir, false, TurnMode::Execute);
    let specs_res = active_tool_specs(&reg, &ts_res, TurnMode::Execute);
    assert!(
        specs_res.len() <= 7,
        "resume di folder polos harus <=7 tool (bukan 11!), dapat {}",
        specs_res.len()
    );
    // Sementara folder dengan .git tetap otomatis mengaktifkan git_status & git_diff!
    std::fs::create_dir_all(plain_dir.join(".git")).unwrap();
    let ts_git = select_initial_toolsets("periksa perubahan", &plain_dir, false, TurnMode::Execute);
    let specs_git = active_tool_specs(&reg, &ts_git, TurnMode::Execute);
    assert!(specs_git.iter().any(|s| s.name == "git_status"));
    assert!(specs_git.iter().any(|s| s.name == "git_diff"));
    let _ = std::fs::remove_dir_all(&plain_dir);

    // 3. Auto-review threshold >= 6 & run_self_improvement_review wajib mengisi call_token_metrics 1:1 dengan usage!
    assert!(AUTO_REVIEW_MIN_TOOLS >= 6);
    struct NoopReviewProv;
    #[async_trait]
    impl Provider for NoopReviewProv {
        fn model_name(&self) -> &str {
            "mock-review"
        }
        async fn stream_chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> anyhow::Result<AssistantReply> {
            Ok(AssistantReply {
                text: "NONE".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 1500,
                    output_tokens: 1,
                }),
            })
        }
    }
    let db = std::env::temp_dir().join(format!(
        "selen-rev-m-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Arc::new(Store::open_at(&db).unwrap());
    let sid = store.create_session("/tmp", "mock", "mock-review", "rev").unwrap();
    let ctx = Ctx::new(std::env::temp_dir(), true).with_store(store.clone(), sid.clone());
    let hist = vec![
        Message::text(Role::User, "tolong buat file"),
        Message::text(Role::Assistant, "selesai"),
    ];
    let res = run_self_improvement_review(Arc::new(NoopReviewProv), &ctx, &hist)
        .await
        .unwrap();
    assert!(res.is_none());
    let rep = store.session_token_metrics(&sid).unwrap();
    assert_eq!(rep.calls, 1, "review call wajib tercatat di call_token_metrics");
    assert_eq!(rep.total_actual_input, 1500);
    let _ = std::fs::remove_file(&db);
}
