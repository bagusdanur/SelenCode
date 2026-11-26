//! Uji penerimaan v0.5.11:
//! - Perbaikan 1: Lantai resume ramping di SELURUH panggilan giliran resume + aktivasi tool malas tetap jalan
//! - Perbaikan 2: Gerbang skill di level loop harness (bukan sekadar kalimat prompt) + muat-ulang `skill_view` konsisten
//! - Perbaikan 3: Dokumentasi `config.example.toml` + peringatan kunci config tak dikenal / salah tempat
//! - Perbaikan 4: Pembatalan provider diam tidak membayar 2× response timeout; fallback rute berbeda tetap bekerja

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::{
    build_resume_snapshot, build_resume_system_prompt, build_system_prompt,
};
use selencode::agent::{classify_required_skill_for_turn, run_turn_counted};
use selencode::config::{validate_config_str, Config};
use selencode::providers::openai::OpenAiProvider;
use selencode::providers::router::{
    is_timeout_error, DynamicRouterProvider, ModelRoute, RouterTelemetry,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use selencode::skills::{seed_builtin_skills_to, SkillInfo};
use selencode::store::Store;
use selencode::tools::skill::SkillView;
use selencode::tools::{default_registry, Ctx, Tool};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0511_{tag}_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// =========================================================================
// PERBAIKAN 1: Lantai resume ramping di semua panggilan giliran resume
// =========================================================================

#[tokio::test]
async fn p1_resume_followup_calls_stay_lean_and_lazy_toolsets_work() {
    let dir = temp_dir("p1-resume");
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());

    // 1. Prompt resume jauh lebih ringkas daripada prompt sesi baru sambil menjaga semua aturan inti.
    let full_sys = build_system_prompt(&dir, &[]);
    let res_sys = build_resume_system_prompt(&dir, &[]);
    assert!(
        res_sys.chars().count() < full_sys.chars().count() / 2,
        "prompt resume ({}) wajib jauh lebih ringkas daripada prompt sesi baru ({})",
        res_sys.chars().count(),
        full_sys.chars().count()
    );
    assert!(res_sys.contains("activate_toolset"));
    assert!(res_sys.contains("watch_job"));
    assert!(res_sys.contains("belum bisa kucek sekarang"));

    // 2. Seed 14 skill bawaan di workspace agar terlihat apakah resume menyaring katalog 14 skill penuh.
    seed_builtin_skills_to(&dir.join(".selencode").join("skills"));
    std::fs::write(dir.join("kata.txt"), "sandi=BULAN-511\nkonteks=audit gudang\n").unwrap();
    // Buat file besar untuk memastikan prune di panggilan lanjutan resume memotong output tool besar!
    let big_log = (0..120)
        .map(|i| format!("baris log panjang nomor {i}: detail transaksi gudang selatan 0123456789"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join("big.log"), &big_log).unwrap();

    // Seed sesi lama untuk uji `session_search` (tool malas di toolset `history`).
    let old_sid = store
        .create_session(&dir.to_string_lossy(), "9router", "mock-model")
        .unwrap();
    store
        .save_message(
            &old_sid,
            &Message::text(Role::User, "Catatan penting: frasa rahasia ARSIP-V0511-OK."),
        )
        .unwrap();
    let cur_sid = store
        .create_session(&dir.to_string_lossy(), "9router", "mock-model")
        .unwrap();

    struct InspectResumeProv {
        step: AtomicUsize,
        seen_sys_lens: Arc<Mutex<Vec<usize>>>,
        seen_tool_counts: Arc<Mutex<Vec<usize>>>,
        seen_big_log_pruned_on_step3: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Provider for InspectResumeProv {
        fn model_name(&self) -> &str {
            "mock-resume-v0511"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let sys_len = msgs
                .first()
                .map(|m| m.plain_text().chars().count())
                .unwrap_or(0);
            self.seen_sys_lens.lock().unwrap().push(sys_len);
            self.seen_tool_counts.lock().unwrap().push(tools.len());

            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n == 2 {
                // Pada step 3 (setelah 2 tool call), isi `big.log` dari step 0 WAJIB sudah ter-prune!
                let pruned = msgs.iter().any(|m| {
                    m.role == Role::Tool
                        && m.plain_text().contains("diringkas — arsip:")
                });
                *self.seen_big_log_pruned_on_step3.lock().unwrap() = pruned;
            }

            let call = match n {
                0 => Some(ToolCall {
                    id: "r1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "big.log"}),
                }),
                1 => Some(ToolCall {
                    id: "r2".into(),
                    name: "activate_toolset".into(),
                    args: json!({"toolsets": ["history"]}),
                }),
                2 => Some(ToolCall {
                    id: "r3".into(),
                    name: "session_search".into(),
                    args: json!({"query": "ARSIP-V0511-OK"}),
                }),
                _ => None,
            };
            if let Some(tc) = call {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![tc],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 2800,
                        output_tokens: 30,
                    }),
                })
            } else {
                Ok(AssistantReply {
                    text: "Ditemukan ARSIP-V0511-OK dan sandi BULAN-511.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 2900,
                        output_tokens: 35,
                    }),
                })
            }
        }
    }

    let seen_sys = Arc::new(Mutex::new(Vec::new()));
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let big_pruned = Arc::new(Mutex::new(false));
    let prov: Arc<dyn Provider> = Arc::new(InspectResumeProv {
        step: AtomicUsize::new(0),
        seen_sys_lens: seen_sys.clone(),
        seen_tool_counts: seen_tools.clone(),
        seen_big_log_pruned_on_step3: big_pruned.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true)
        .with_resumed_session(true)
        .with_store(store, cur_sid);

    // Simulasikan sesi pra-resume dengan 8 pesan (memicu pemadatan checkpoint pra-resume!)
    let mut msgs = vec![
        Message::text(Role::System, "sys sesi lama super panjang"),
        Message::text(Role::User, "Giliran 1 lama"),
        Message::text(Role::Assistant, "Jawaban 1 lama"),
        Message::text(Role::User, "Giliran 2 lama"),
        Message::text(Role::Assistant, "Jawaban 2 lama"),
        Message::text(Role::User, "Giliran 3 lama"),
        Message::text(Role::Assistant, "Jawaban 3 lama"),
        Message::text(Role::User, "Baca kata.txt"),
        Message::text(Role::Assistant, "Sudah dibaca: BULAN-511."),
    ];

    // Verifikasi `build_resume_snapshot` memadatkan pesan pra-resume panjang
    let snap_check = build_resume_snapshot(&msgs, Some(&res_sys));
    assert!(
        snap_check
            .iter()
            .any(|m| m.plain_text().contains("[Ringkasan checkpoint pra-resume]")),
        "riwayat pra-resume > 6 pesan wajib diringkas ke checkpoint"
    );

    let (ans, _) = run_turn_counted(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Baca big.log lalu cari frasa ARSIP-V0511-OK dari sesi lain",
        8,
    )
    .await
    .unwrap();

    assert!(ans.contains("ARSIP-V0511-OK"));
    let sys_lens = seen_sys.lock().unwrap().clone();
    assert!(
        sys_lens.iter().all(|&l| l < 1800),
        "SEMUA panggilan di giliran resume wajib memakai system prompt + snapshot ramping (<1800 char, tanpa dump 14 skill): {:?}",
        sys_lens
    );
    let tcounts = seen_tools.lock().unwrap().clone();
    assert_eq!(tcounts[0], 6, "call 1 resume memakai 6 tool inti");
    assert_eq!(
        tcounts[2], 7,
        "setelah activate_toolset(['history']), session_search langsung aktif di giliran yang sama"
    );
    assert!(
        *big_pruned.lock().unwrap(),
        "hasil tool besar pada langkah awal giliran resume wajib ter-prune pada langkah lanjutan"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Gerbang skill di level loop + muat-ulang `skill_view` konsisten
// =========================================================================

#[tokio::test]
async fn p2_loop_skill_gate_enforces_bug_and_tdd_and_consistent_reload() {
    let dir = temp_dir("p2-skill-gate");
    let skills_root = dir.join(".selencode").join("skills");
    seed_builtin_skills_to(&skills_root);
    let installed: Vec<SkillInfo> = selencode::skills::list_skills(&dir);

    // 1. Klasifikasi jenis tugas deterministik (positif & negatif tanpa spam):
    assert_eq!(
        classify_required_skill_for_turn(
            "Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Perbaiki.",
            &installed
        )
        .as_deref(),
        Some("systematic-debugging")
    );
    assert_eq!(
        classify_required_skill_for_turn(
            "Di pajak.js hasil hitung seharusnya 34965, bukan 35350. Koreksi kodenya.",
            &installed
        )
        .as_deref(),
        Some("systematic-debugging")
    );
    assert_eq!(
        classify_required_skill_for_turn(
            "Ada bug di hitung_total.js, temukan akar penyebabnya.",
            &installed
        )
        .as_deref(),
        Some("systematic-debugging")
    );
    assert_eq!(
        classify_required_skill_for_turn(
            "Tulis fitur pemformat angka rupiah lengkap dengan tesnya.",
            &installed
        )
        .as_deref(),
        Some("test-driven-development")
    );
    // Kontrol negatif: Proyek A, Proyek B ("cetak error ke stderr"), & trivial TIDAK boleh memicu skill!
    assert_eq!(
        classify_required_skill_for_turn(
            "Buat skrip rekap.py yang membaca data.csv lalu mencetak total dan rata-rata kolom nilai.",
            &installed
        ),
        None
    );
    assert_eq!(
        classify_required_skill_for_turn(
            "Buat kalkulator CLI Node.js calc.js mendukung add, sub, mul, div (div dengan 0 cetak error ke stderr dan exit code 1).",
            &installed
        ),
        None
    );
    assert_eq!(
        classify_required_skill_for_turn("Balas ok saja.", &installed),
        None
    );

    // 2. Uji Gerbang Pre-Mutate di loop: model langsung mencoba `edit_file` pada prompt kanonik
    //    tanpa memanggil `skill_view` -> harness WAJIB menahan edit, menjalankan `skill_view`,
    //    merekam `skill_view` di DB SEBELUM `edit_file`, lalu baru mengizinkan `edit_file`!
    std::fs::write(
        dir.join("hitung_total.js"),
        "const total = 35000 - 10;\nconsole.log(total);\n",
    )
    .unwrap();
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());
    let sid = store
        .create_session(&dir.to_string_lossy(), "9router", "mock-gate")
        .unwrap();

    struct SkipSkillEditProv {
        step: AtomicUsize,
        saw_skill_before_second_try: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Provider for SkipSkillEditProv {
        fn model_name(&self) -> &str {
            "mock-skip-skill"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Model nakal: langsung coba edit_file tanpa panggil skill_view dulu!
                return Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "e_early".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "hitung_total.js",
                            "old_text": "35000 - 10",
                            "new_text": "35000 * 0.9"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 400,
                        output_tokens: 30,
                    }),
                });
            }
            if n == 1 {
                let has_sv = msgs.iter().any(|m| {
                    m.role == Role::Tool
                        && m.plain_text().contains("Systematic Debugging")
                        && m.plain_text().contains("Gerbang Skill Harness")
                });
                *self.saw_skill_before_second_try.lock().unwrap() = has_sv;
                return Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "e_after".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "hitung_total.js",
                            "old_text": "35000 - 10",
                            "new_text": "35000 * 0.9"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 550,
                        output_tokens: 30,
                    }),
                });
            }
            Ok(AssistantReply {
                text: "Sudah diperbaiki menjadi 31500.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 520,
                    output_tokens: 20,
                }),
            })
        }
    }

    let saw_sv = Arc::new(Mutex::new(false));
    let prov: Arc<dyn Provider> = Arc::new(SkipSkillEditProv {
        step: AtomicUsize::new(0),
        saw_skill_before_second_try: saw_sv.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true).with_store(store.clone(), sid.clone());
    let mut msgs = Vec::new();
    let (ans, _) = run_turn_counted(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Perbaiki.",
        6,
    )
    .await
    .unwrap();
    assert!(ans.contains("31500"));
    assert!(
        *saw_sv.lock().unwrap(),
        "gerbang loop wajib menyuntikkan skill_view sebelum edit_file dieksekusi"
    );
    // Verifikasi urutan tool_use di SQLite DB: `skill_view` HARUS ketat SEBELUM `edit_file`!
    let saved = store.load_messages(&sid).unwrap();
    let mut tool_seq: Vec<String> = Vec::new();
    for m in &saved {
        for b in &m.content {
            if let Block::ToolUse { name, .. } = b {
                tool_seq.push(name.clone());
            }
        }
    }
    let idx_sv = tool_seq
        .iter()
        .position(|n| n == "skill_view")
        .expect("skill_view wajib tercatat di DB");
    let idx_edit = tool_seq
        .iter()
        .position(|n| n == "edit_file")
        .expect("edit_file wajib tercatat di DB");
    assert!(
        idx_sv < idx_edit,
        "skill_view ({idx_sv}) wajib muncul SEBELUM edit_file ({idx_edit}) di DB: {tool_seq:?}"
    );
    assert!(std::fs::read_to_string(dir.join("hitung_total.js"))
        .unwrap()
        .contains("35000 * 0.9"));

    // 3. Cacat B: muat-ulang `skill_view(name)` tanpa `full=true` HARUS tetap mengembalikan
    //    salinan kerja ringkas (±1.500–2.200 char, BUKAN 14.600 char!), sedangkan `full=true` penuh.
    let sv_first = SkillView
        .run(json!({"name": "systematic-debugging"}), &ctx)
        .await
        .unwrap();
    let sv_reload = SkillView
        .run(json!({"name": "systematic-debugging"}), &ctx)
        .await
        .unwrap();
    let sv_full = SkillView
        .run(json!({"name": "systematic-debugging", "full": true}), &ctx)
        .await
        .unwrap();
    assert_eq!(sv_first, sv_reload, "muat-ulang default wajib konsisten dengan muatan pertama");
    assert!(
        sv_reload.chars().count() <= 2_500,
        "muat-ulang default harus tetap salinan kerja ringkas (<=2500 char), dapat {}",
        sv_reload.chars().count()
    );
    assert!(
        sv_full.chars().count() > 10_000,
        "permintaan eksplisit full=true harus mengembalikan teks penuh"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 3: Dokumentasi `config.example.toml` + peringatan kunci tak dikenal / salah tempat
// =========================================================================

#[test]
fn p3_config_example_and_unknown_or_misplaced_key_warnings() {
    // 1. `config.example.toml` memuat `[timeouts]` top-level + ketiga knob `[telegram]` & 0 peringatan!
    let example = include_str!("../config.example.toml");
    assert!(example.contains("[timeouts]"));
    assert!(example.contains("connect_secs = 20"));
    assert!(example.contains("response_secs = 60"));
    assert!(example.contains("stream_idle_secs = 90"));
    assert!(example.contains("total_secs = 300"));
    assert!(example.contains("TOP-LEVEL"));
    assert!(example.contains("turn_timeout_secs = 900"));
    assert!(example.contains("stall_notice_secs = 180"));
    assert!(example.contains("poll_watchdog_secs = 300"));
    let ex_warnings = validate_config_str(example);
    assert!(
        ex_warnings.is_empty(),
        "config.example.toml tidak boleh punya kunci tak dikenal: {ex_warnings:?}"
    );

    // 2. Typo kunci (`turn_timeot_secs`) memicu peringatan bernama kunci, namun config tetap valid saat parse!
    let typo_toml = r#"
default_provider = "router"

[providers.router]
kind = "openai"
base_url = "https://router.ryukomik.com/v1"
model = "ag/gemini-4-neon-medium"
api_key = "dummy"

[telegram]
turn_timeot_secs = 600
"#;
    let w_typo = validate_config_str(typo_toml);
    assert_eq!(w_typo.len(), 1);
    assert_eq!(w_typo[0].key_path, "telegram.turn_timeot_secs");
    assert!(w_typo[0].message.contains("turn_timeot_secs"));
    // Tidak boleh gagal start!
    let parsed_typo: Config = toml::from_str(typo_toml).unwrap();
    assert_eq!(parsed_typo.telegram.turn_timeout_secs, 900);

    // 3. Penempatan salah `[providers.router.timeouts]` memicu peringatan khusus top-level & diabaikan!
    let misplaced_toml = r#"
default_provider = "router"

[providers.router]
kind = "openai"
base_url = "https://router.ryukomik.com/v1"
model = "ag/gemini-4-neon-medium"
api_key = "dummy"

[providers.router.timeouts]
response_secs = 5
"#;
    let w_mis = validate_config_str(misplaced_toml);
    assert_eq!(w_mis.len(), 1);
    assert_eq!(w_mis[0].key_path, "providers.router.timeouts");
    assert!(
        w_mis[0].message.contains("top-level") && w_mis[0].message.contains("diabaikan"),
        "pesan salah tempat wajib jelas: {}",
        w_mis[0].message
    );
    let parsed_mis: Config = toml::from_str(misplaced_toml).unwrap();
    assert_eq!(
        parsed_mis.timeouts.response_secs, 60,
        "timeouts di bawah provider tidak boleh dipakai diam-diam"
    );
}

// =========================================================================
// PERBAIKAN 4: Pembatalan provider diam ≈1× response timeout (bukan 2×)
// =========================================================================

#[tokio::test]
async fn p4_silent_provider_skips_same_route_retry_but_keeps_different_fallback() {
    assert!(is_timeout_error(
        "timeout 6 detik menunggu respons awal provider — provider tidak merespons"
    ));

    // Server TCP diam total yang menghitung berapa kali koneksi diterima.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_count = Arc::new(AtomicUsize::new(0));
    let accept_count_srv = accept_count.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            accept_count_srv.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    let timeouts = ProviderTimeouts {
        connect: Duration::from_secs(2),
        response: Duration::from_secs(1),
        stream_idle: Duration::from_secs(1),
        total: Duration::from_secs(10),
    };
    let silent: Arc<dyn Provider> = Arc::new(OpenAiProvider::with_timeouts(
        format!("http://{addr}/v1"),
        "test-key".into(),
        "silent-only".into(),
        512,
        timeouts,
    ));

    // Kasus 1: config bawaan `max_retries = 1` (sebelumnya membayar 2× response_timeout!),
    // tanpa rute fallback berbeda -> WAJIB batal dalam ~1× response_timeout (~1 dtk, bukan ~2+ dtk)
    // dan hanya menyentuh server 1 kali!
    let tel1 = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router_single = DynamicRouterProvider::new(
        vec![ModelRoute::new(
            "silent",
            "silent-only",
            silent.clone(),
            None,
            None,
            None,
            true,
        )],
        false,
        HashMap::new(),
        1, // default_routing_retries() = 1!
        tel1,
    );
    let t0 = Instant::now();
    let (tx1, _rx1) = mpsc::channel(8);
    let err = router_single
        .stream_chat(&[Message::text(Role::User, "halo")], &[], tx1)
        .await
        .unwrap_err();
    let elapsed1 = t0.elapsed();
    assert!(
        elapsed1 < Duration::from_millis(1800),
        "pembatalan provider diam dengan max_retries=1 harus ~1× response_timeout (1s), bukan 2×: {elapsed1:?}"
    );
    assert_eq!(
        accept_count.load(Ordering::SeqCst),
        1,
        "rute yang sama yang baru saja timeout TIDAK boleh dicoba ulang!"
    );
    assert!(err.to_string().contains("tidak merespons"));

    // Kasus 2: provider diam + rute fallback BERBEDA yang sehat (max_retries = 1) ->
    // primary dicoba 1× (~1s), langsung pindah ke fallback berbeda dan berhasil!
    struct HealthyFb(Arc<AtomicUsize>);
    #[async_trait]
    impl Provider for HealthyFb {
        fn model_name(&self) -> &str {
            "healthy-fb"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(StreamEvent::Token("Fallback berbeda sukses!".into())).await;
            Ok(AssistantReply {
                text: "Fallback berbeda sukses!".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 90,
                    output_tokens: 10,
                }),
            })
        }
    }

    let fb_hits = Arc::new(AtomicUsize::new(0));
    let tel2 = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router_fb = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("silent", "silent-only", silent, None, None, None, true),
            ModelRoute::new(
                "backup",
                "healthy-fb",
                Arc::new(HealthyFb(fb_hits.clone())),
                None,
                None,
                None,
                false,
            ),
        ],
        false,
        HashMap::new(),
        1,
        tel2,
    );
    let t1 = Instant::now();
    let (tx2, _rx2) = mpsc::channel(8);
    let reply = router_fb
        .stream_chat(&[Message::text(Role::User, "halo")], &[], tx2)
        .await
        .unwrap();
    assert!(reply.text.contains("Fallback berbeda sukses!"));
    assert_eq!(fb_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        accept_count.load(Ordering::SeqCst),
        2,
        "primary hanya dicoba 1× lagi pada panggilan kedua, lalu langsung fallback"
    );
    assert!(t1.elapsed() < Duration::from_millis(2000));

    server.abort();
}
