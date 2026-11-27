use anyhow::{anyhow, Result};
use async_trait::async_trait;
use selencode::agent::context::{
    estimate_tokens, proactive_prune_context_with_archive,
};
use selencode::agent::corrector::ToolDedupCache;
use selencode::agent::{
    classify_turn_api_error, jittered_backoff_ms, run_turn_ui, TurnApiErrorKind, UiEvent,
    STALE_STREAM_GIVEUP_LIMIT,
};
use selencode::config::{
    load_from, parse_config_str_with_policy, save_to, validate_config_str, MaxStepsSource,
    ProviderCfg,
};
use selencode::doctor::run_doctor;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use selencode::tools::file::truncate_lines_to_char_budget;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0517-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

// =========================================================================
// MASALAH 1: Skalasi stale-stream timeout + pemulihan retry & circuit breaker
// (desain internal SelenCode)
// =========================================================================

#[test]
fn m1_context_scaled_stream_idle_and_backoff_tiers() {
    let base = ProviderTimeouts::default();
    assert_eq!(base.effective_stream_idle(4_000), Duration::from_secs(180));
    assert_eq!(base.effective_stream_idle(20_000), Duration::from_secs(180));
    assert_eq!(base.effective_stream_idle(60_000), Duration::from_secs(240));
    assert_eq!(base.effective_stream_idle(120_000), Duration::from_secs(300));
    assert!(
        base.effective_total(120_000) >= Duration::from_secs(600),
        "total timeout operasional harus memberi ruang di atas effective_stream_idle"
    );

    // Timeout uji pendek (<30s) TIDAK boleh di-override agar unit test timeout tetap cepat.
    let short = ProviderTimeouts {
        connect: Duration::from_secs(2),
        response: Duration::from_secs(1),
        stream_idle: Duration::from_secs(1),
        total: Duration::from_secs(10),
    };
    assert_eq!(short.effective_stream_idle(120_000), Duration::from_secs(1));
    assert_eq!(short.effective_total(120_000), Duration::from_secs(10));

    // Klasifikasi error panggilan API
    assert_eq!(
        classify_turn_api_error(
            "timeout ketidakaktifan stream provider (tidak ada data selama 90 detik) — koneksi dihentikan"
        ),
        TurnApiErrorKind::StreamStaleOrDrop
    );
    assert_eq!(
        classify_turn_api_error("stream terputus di tengah jawaban (router): unexpected eof"),
        TurnApiErrorKind::StreamStaleOrDrop
    );
    assert_eq!(
        classify_turn_api_error("provider membalas 429 Too Many Requests"),
        TurnApiErrorKind::TransientServerOrRateLimit
    );
    assert_eq!(
        classify_turn_api_error("provider membalas 413 Payload Too Large"),
        TurnApiErrorKind::ContextOverflow
    );
    assert_eq!(
        classify_turn_api_error("provider membalas 401 Unauthorized"),
        TurnApiErrorKind::NonRetryable
    );

    // Jittered exponential backoff berada pada [delay, delay + delay/2]
    for attempt in 1..=3 {
        let d = jittered_backoff_ms(attempt, 1000, 30_000);
        let base_delay = 1000u64 << (attempt - 1);
        assert!(
            d >= base_delay && d <= base_delay + base_delay / 2,
            "attempt {attempt}: backoff {d} di luar [{base_delay}, {}]",
            base_delay + base_delay / 2
        );
    }
}

#[tokio::test]
async fn m1_mid_turn_stale_stream_recovers_and_circuit_breaker_stops_infinite_hang() {
    struct FlakyStaleProvider {
        calls: Arc<AtomicUsize>,
        always_stale: bool,
    }

    #[async_trait]
    impl Provider for FlakyStaleProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-neon-medium"
        }

        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.always_stale {
                return Err(anyhow!(
                    "timeout ketidakaktifan stream provider (tidak ada data selama 90 detik) — koneksi dihentikan"
                ));
            }
            match n {
                1 => Err(anyhow!(
                    "timeout ketidakaktifan stream provider (tidak ada data selama 90 detik) — koneksi dihentikan"
                )),
                2 => Err(anyhow!("stream terputus di tengah jawaban (router): unexpected eof")),
                _ => {
                    let _ = tx.send(StreamEvent::Token("Pulih dan selesai!".into())).await;
                    Ok(AssistantReply {
                        text: "Pulih dan selesai!".into(),
                        tool_calls: Vec::new(),
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 450,
                            output_tokens: 20,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let cwd = temp_dir("m1-recover");
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true).with_api_retry(3, 5);

    // Kasus 1: 2x stream macet/putus -> pulih otomatis pada percobaan ke-3 tanpa membunuh giliran!
    let calls1 = Arc::new(AtomicUsize::new(0));
    let prov1: Arc<dyn Provider> = Arc::new(FlakyStaleProvider {
        calls: calls1.clone(),
        always_stale: false,
    });
    let mut msgs1 = Vec::new();
    let (ui_tx, mut ui_rx) = mpsc::channel(64);
    let reply = run_turn_ui(prov1, &reg, &ctx, &mut msgs1, "halo cek proyek", 0, ui_tx)
        .await
        .expect("giliran harus pulih dari stream macet sementara");
    assert!(reply.contains("Pulih dan selesai!"), "{reply}");
    assert_eq!(calls1.load(Ordering::SeqCst), 3);

    let mut infos = Vec::new();
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos.push(s);
        }
    }
    assert!(
        infos.iter().any(|s| s.contains("mencoba ulang otomatis")),
        "harus ada pemberitahuan retry otomatis yang terlihat: {infos:?}"
    );

    // Kasus 2: provider macet terus -> circuit breaker berhenti bersih tepat di STALE_STREAM_GIVEUP_LIMIT (3)
    let calls2 = Arc::new(AtomicUsize::new(0));
    let prov2: Arc<dyn Provider> = Arc::new(FlakyStaleProvider {
        calls: calls2.clone(),
        always_stale: true,
    });
    let mut msgs2 = Vec::new();
    let (ui_tx2, _ui_rx2) = mpsc::channel(64);
    let err = run_turn_ui(prov2, &reg, &ctx, &mut msgs2, "halo", 0, ui_tx2)
        .await
        .unwrap_err();
    assert_eq!(calls2.load(Ordering::SeqCst), STALE_STREAM_GIVEUP_LIMIT);
    assert!(
        err.to_string().contains("3x beruntun"),
        "error circuit breaker harus jelas: {err}"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

// =========================================================================
// MASALAH 2: Batas ganda read_file di sumber + proactive 3-pass context prune
// (desain internal SelenCode)
// =========================================================================

#[tokio::test]
async fn m2_read_file_dual_cap_pagination_and_per_line_clamp() {
    let cwd = temp_dir("m2-readfile");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();
    let read_tool = reg.get("read_file").unwrap();

    // 1a. Berkas Svelte besar (~850 baris panjang, ~55KB < 100KB): dipotong rapi oleh pagu karakter 16.000 char!
    let mut svelte = String::new();
    for i in 1..=850 {
        svelte.push_str(&format!(
            "const baris_{i:04} = 'komponen transaksi svelte panjang nomor {i}';\n"
        ));
    }
    assert!(svelte.len() < 100 * 1024);
    std::fs::write(cwd.join("+page.svelte"), &svelte).unwrap();

    let out = read_tool
        .run(json!({"path": "+page.svelte"}), &ctx)
        .await
        .unwrap();
    assert!(
        out.chars().count() <= 17_000,
        "hasil read_file harus terkendali di pagu karakter/baris: {} char",
        out.chars().count()
    );
    assert!(
        out.contains("pagu 16000 char") && out.contains("dari 850") && out.contains("start="),
        "harus memotong pada pagu 16000 char + petunjuk pagination start/end: {out}"
    );

    // 1b. Berkas 850 baris pendek (~10KB): dipotong rapi oleh pagu baris bawaan 250 baris!
    let mut short_many = String::new();
    for i in 1..=850 {
        short_many.push_str(&format!("let v{i} = {i};\n"));
    }
    std::fs::write(cwd.join("short_many.svelte"), &short_many).unwrap();
    let out_lines = read_tool
        .run(json!({"path": "short_many.svelte"}), &ctx)
        .await
        .unwrap();
    assert!(
        out_lines.contains("baris 1-250 dari 850") && out_lines.contains("start=251"),
        "harus memotong pada pagu 250 baris + start=251: {out_lines}"
    );

    // 2. Baris super panjang (minified CSS/SVG 5.000 char) dipotong per baris (2.000 char)
    let long_line = format!(".cls {{ content: '{}'; }}\n", "x".repeat(5000));
    std::fs::write(cwd.join("minified.css"), &long_line).unwrap();
    let out_css = read_tool
        .run(json!({"path": "minified.css"}), &ctx)
        .await
        .unwrap();
    assert!(out_css.contains("... [baris dipotong]"), "{out_css}");
    assert!(out_css.chars().count() < 2_500);

    // 3. Helper `truncate_lines_to_char_budget` berhenti pada baris utuh terakhir
    let lines = vec!["12345".into(), "67890".into(), "abcde".into()];
    let (kept, count, trunc) = truncate_lines_to_char_budget(&lines, 11);
    assert!(trunc);
    assert_eq!(count, 2);
    assert_eq!(kept, "12345\n67890");

    // 4. Berkas kecil tetap utuh 100% tanpa footer tambahan
    std::fs::write(cwd.join("small.txt"), "satu\ndua\ntiga\n").unwrap();
    let out_small = read_tool
        .run(json!({"path": "small.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out_small, "satu\ndua\ntiga");

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn m2_proactive_prune_controls_context_explosion_archives_and_resets_dedup() {
    let cwd = temp_dir("m2-prune");
    let archive_dir = cwd.join("pruned_archive");

    // Simulasi persis insiden Termux HP: < 12 pesan (jauh di bawah keep_tail=20 lama!),
    // berisi bacaan 3 berkas besar + 1 write_file 16.358 byte.
    let big_read_1 = "export function transaksi() { return 42; }\n".repeat(350); // ~15KB
    let big_read_2 = "/* app.css styling */\n.card { padding: 12px; }\n".repeat(320); // ~15KB
    let big_write_body = "<div>".to_string() + &"konten svelte ".repeat(1168) + "</div>"; // ~16.360 byte
    assert!(big_write_body.len() >= 16_000);

    let mut msgs = vec![
        Message::text(Role::System, "Kamu adalah Selena."),
        Message::text(Role::User, "Perbaiki halaman transaksi Svelte."),
        Message {
            role: Role::Assistant,
            content: vec![
                Block::ToolUse {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "+page.svelte"}),
                },
                Block::ToolUse {
                    id: "c2".into(),
                    name: "read_file".into(),
                    args: json!({"path": "app.css"}),
                },
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![
                Block::ToolResult {
                    id: "c1".into(),
                    content: big_read_1.clone(),
                    is_error: false,
                },
                Block::ToolResult {
                    id: "c2".into(),
                    content: big_read_2.clone(),
                    is_error: false,
                },
            ],
        },
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c3".into(),
                name: "write_file".into(),
                args: json!({"path": "+page.svelte", "content": big_write_body}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c3".into(),
                content: "OK: +page.svelte ditulis (16360 byte).".into(),
                is_error: false,
            }],
        },
        // Putaran tool terbaru (ekor terlindungi)
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c4".into(),
                name: "read_file".into(),
                args: json!({"path": "app.html"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c4".into(),
                content: "<!doctype html><html><body>%sveltekit.body%</body></html>".into(),
                is_error: false,
            }],
        },
    ];

    let before_toks = estimate_tokens(&msgs);
    assert!(before_toks > 10_000, "estimasi awal harus besar: {before_toks}");

    // 1. Gerbang min_reclaim: jika threshold tidak tercapai, TIDAK menyentuh `msgs`!
    let noop = proactive_prune_context_with_archive(
        &mut msgs,
        &archive_dir,
        10_000,
        18_000,
        2_000,
        2,
        false,
    );
    assert_eq!(noop.pruned_count, 0);
    assert!(!archive_dir.exists());

    // 2. Ketika tekanan token melewati ambang (mis. 20.000 >= 18.000), proactive prune WAJIB menyala!
    let res = proactive_prune_context_with_archive(
        &mut msgs,
        &archive_dir,
        20_000,
        18_000,
        2_000,
        2,
        false,
    );
    assert!(res.pruned_count >= 3, "harus memangkas ToolResult lama + write_file args: {res:?}");
    assert!(res.pruned_tool_results >= 2);
    assert_eq!(res.pruned_tool_args, 1, "argumen write_file 16KB harus ikut diringkas");
    assert!(res.reclaimed_tokens >= 8_000, "harus membebaskan ribuan token: {res:?}");
    let after_toks = estimate_tokens(&msgs);
    assert!(
        after_toks < before_toks / 4,
        "konteks sesudah prune harus jauh lebih ramping ({before_toks} -> {after_toks})"
    );
    // Pastikan ekor terbaru (`app.html`) tetap 100% utuh!
    let last_tool_txt = match &msgs.last().unwrap().content[0] {
        Block::ToolResult { content, .. } => content.as_str(),
        _ => "",
    };
    assert!(last_tool_txt.contains("%sveltekit.body%"), "{last_tool_txt}");
    // Pastikan arsip byte-exact tersimpan di disk!
    let archived_files: Vec<_> = std::fs::read_dir(&archive_dir)
        .unwrap()
        .flatten()
        .collect();
    assert!(archived_files.len() >= 3);

    // 3. Reset ToolDedupCache sesudah prune memungkinkan baca ulang berkas yang sama!
    std::fs::write(cwd.join("app.css"), &big_read_2).unwrap();
    let mut dedup = ToolDedupCache::new();
    let read_args = json!({"path": "app.css"});
    dedup.record_success("read_file", &read_args, &big_read_2, &cwd);
    assert!(dedup.check_hit("read_file", &read_args, &cwd).is_some());
    dedup.clear_file_reads();
    assert!(
        dedup.check_hit("read_file", &read_args, &cwd).is_none(),
        "sesudah clear_file_reads, read_file harus diizinkan baca ulang"
    );

    // 4. End-to-end di dalam `run_turn_ui`: pastikan `call_token_metrics.pruned_count > 0` tercatat!
    struct MultiBigReadProvider {
        step: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for MultiBigReadProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-neon-medium"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let s = self.step.fetch_add(1, Ordering::SeqCst);
            match s {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "r1".into(),
                            name: "read_file".into(),
                            args: json!({"path": "f1.txt"}),
                        },
                        ToolCall {
                            id: "r2".into(),
                            name: "read_file".into(),
                            args: json!({"path": "f2.txt"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 4_500,
                        output_tokens: 60,
                        ..Default::default()
                    }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "r3".into(),
                        name: "read_file".into(),
                        args: json!({"path": "f3.txt"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 22_000,
                        output_tokens: 50,
                        ..Default::default()
                    }),
                }),
                2 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "r4".into(),
                        name: "read_file".into(),
                        args: json!({"path": "f4.txt"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 24_000,
                        output_tokens: 40,
                        ..Default::default()
                    }),
                }),
                _ => {
                    let _ = tx.send(StreamEvent::Token("Semua file diperiksa.".into())).await;
                    Ok(AssistantReply {
                        text: "Semua file diperiksa.".into(),
                        tool_calls: Vec::new(),
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 8_000,
                            output_tokens: 30,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    for i in 1..=4 {
        std::fs::write(
            cwd.join(format!("f{i}.txt")),
            format!("baris isi berkas besar nomor {i} untuk uji prune proaktif\n").repeat(220),
        )
        .unwrap();
    }
    let db_path = cwd.join("test.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());
    let sid = store
        .create_session(&cwd.to_string_lossy(), "router", "ag/gemini-4-neon-medium", "uji prune")
        .unwrap();
    let mut tb = selencode::config::ToolBudgetCfg::default();
    tb.proactive_prune_tokens = 12_000;
    tb.proactive_prune_min_reclaim_tokens = 1_500;
    let ctx_e2e = Ctx::new(cwd.clone(), true)
        .with_context_limit(1_048_576) // 1M seperti Gemini di HP pemilik!
        .with_tool_budget(tb)
        .with_store(store.clone(), sid.clone());
    let reg = default_registry();
    let prov_e2e: Arc<dyn Provider> = Arc::new(MultiBigReadProvider {
        step: Arc::new(AtomicUsize::new(0)),
    });
    let mut turn_msgs = Vec::new();
    let (ui_tx, _ui_rx) = mpsc::channel(64);
    let out = run_turn_ui(prov_e2e, &reg, &ctx_e2e, &mut turn_msgs, "periksa 4 file", 0, ui_tx)
        .await
        .unwrap();
    assert!(out.contains("Semua file diperiksa."));
    let metrics = store.session_token_metrics(&sid).unwrap();
    assert!(
        metrics.total_pruned > 0,
        "pruned_count di call_token_metrics WAJIB > 0 pada jendela 1M saat konteks meledak: {metrics:?}"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

// =========================================================================
// MASALAH 3: Pagu warisan `max_steps = 25` dibebaskan otomatis + selen doctor
// =========================================================================

#[test]
fn m3_legacy_max_steps_25_liberated_explicit_respected_and_doctor_reports() {
    let cwd = temp_dir("m3-legacy-steps");
    let db_path = cwd.join("doc.db");
    let store = Store::open_at(&db_path).unwrap();
    let sandbox = SandboxController::default();
    let pcfg = ProviderCfg {
        kind: "openai".into(),
        base_url: Some("https://router.ryukomik.com/v1".into()),
        model: "ag/gemini-4-neon-medium".into(),
        models: Vec::new(),
        api_key_env: String::new(),
        api_key: Some("dummy".into()),
        price_in: None,
        price_out: None,
        context_limit: None,
        fallback: Vec::new(),
    };

    // 1. Config lama memuat `max_steps = 25` -> diabaikan (selalu 0) + pemberitahuan terlihat + tidak ditulis ulang saat save!
    let legacy_path = cwd.join("legacy-config.toml");
    let legacy_toml = r#"
default_provider = "router"

[providers.router]
kind = "openai"
base_url = "https://router.ryukomik.com/v1"
model = "ag/gemini-4-neon-medium"
api_key = "dummy"

[agent]
max_steps = 25
"#;
    std::fs::write(&legacy_path, legacy_toml).unwrap();
    let legacy_cfg = load_from(&legacy_path).unwrap();
    assert_eq!(
        legacy_cfg.agent.max_steps, 0,
        "max_steps = 25 dari config lama wajib diabaikan menjadi tanpa batas (0)"
    );
    assert!(matches!(
        legacy_cfg.max_steps_source,
        MaxStepsSource::DeprecatedIgnored {
            legacy_value: 25,
            ..
        }
    ));
    assert!(
        !legacy_cfg.startup_notices.is_empty()
            && legacy_cfg.startup_notices[0].contains("25")
            && legacy_cfg.startup_notices[0].contains("tanpa batas"),
        "harus ada pemberitahuan terlihat tentang diabaikannya max_steps lama: {:?}",
        legacy_cfg.startup_notices
    );
    let saved_path = cwd.join("saved-clean.toml");
    save_to(&legacy_cfg, &saved_path).unwrap();
    let saved_txt = std::fs::read_to_string(&saved_path).unwrap();
    assert!(
        !saved_txt.contains("max_steps"),
        "config tersimpan baru tidak boleh menulis max_steps lagi: {saved_txt}"
    );
    let rep_legacy = run_doctor(&legacy_cfg, &cwd, "router", &pcfg, &store, &sandbox);
    let step_chk_legacy = rep_legacy
        .checks
        .iter()
        .find(|c| c.name == "Batas Langkah")
        .expect("doctor wajib melaporkan pemeriksaan Batas Langkah");
    assert!(
        step_chk_legacy.detail.contains("tanpa batas")
            && step_chk_legacy.detail.contains("max_steps = 25")
            && step_chk_legacy.detail.contains("diabaikan"),
        "doctor harus melaporkan tanpa batas & catatan max_steps=25 diabaikan: {}",
        step_chk_legacy.detail
    );

    // 2. Nilai max_steps lain (mis. 40) juga diabaikan pada giliran utama (Opsi A — terstruktur)!
    let other_toml = r#"
default_provider = "router"

[providers.router]
kind = "openai"
model = "ag/gemini-4-neon-medium"
api_key = "dummy"

[agent]
max_steps = 40
"#;
    let other_cfg = parse_config_str_with_policy(other_toml, "/tmp/custom.toml").unwrap();
    assert_eq!(other_cfg.agent.max_steps, 0);
    assert_eq!(
        other_cfg.max_steps_source,
        MaxStepsSource::DeprecatedIgnored {
            legacy_value: 40,
            config_path: "/tmp/custom.toml".into()
        }
    );
    assert!(!other_cfg.startup_notices.is_empty());
    let rep_other = run_doctor(&other_cfg, &cwd, "router", &pcfg, &store, &sandbox);
    let step_chk_other = rep_other
        .checks
        .iter()
        .find(|c| c.name == "Batas Langkah")
        .unwrap();
    assert!(
        step_chk_other.detail.contains("tanpa batas")
            && step_chk_other.detail.contains("max_steps = 40")
            && step_chk_other.detail.contains("/tmp/custom.toml"),
        "doctor harus melaporkan tanpa batas & catatan max_steps=40 diabaikan: {}",
        step_chk_other.detail
    );

    // 3. Config bersih tanpa `max_steps` + config.example.toml tidak memuat kunci `max_steps`
    let example = include_str!("../config.example.toml");
    assert!(
        !example
            .lines()
            .any(|l| l.trim_start().starts_with("max_steps")),
        "config.example.toml tidak boleh memuat kunci max_steps lagi"
    );
    let default_toml = r#"
default_provider = "router"

[providers.router]
kind = "openai"
model = "ag/gemini-4-neon-medium"
api_key = "dummy"

[tool_budget]
file_read_max_chars = 16000
file_read_default_lines = 250
proactive_prune_tokens = 18000
proactive_prune_min_reclaim_tokens = 2000
"#;
    assert!(
        validate_config_str(default_toml).is_empty(),
        "4 kunci baru [tool_budget] wajib terdaftar di skema resmi"
    );
    let default_cfg = parse_config_str_with_policy(default_toml, "/tmp/default.toml").unwrap();
    assert_eq!(default_cfg.agent.max_steps, 0);
    assert_eq!(default_cfg.max_steps_source, MaxStepsSource::DefaultUnbounded);
    assert!(default_cfg.startup_notices.is_empty());
    let rep_default = run_doctor(&default_cfg, &cwd, "router", &pcfg, &store, &sandbox);
    let step_chk_default = rep_default
        .checks
        .iter()
        .find(|c| c.name == "Batas Langkah")
        .unwrap();
    assert!(
        step_chk_default.detail.contains("tanpa batas langkah")
            && step_chk_default.detail.contains("LoopGuard"),
        "{}",
        step_chk_default.detail
    );

    std::fs::remove_dir_all(&cwd).ok();
}
