use anyhow::{anyhow, Result};
use async_trait::async_trait;
use selencode::agent::context::estimate_tokens;
use selencode::agent::{
    has_truncated_tool_call, is_output_length_truncated, run_turn_ui, UiEvent,
    OUTPUT_TRUNCATION_HEADER, STALE_STREAM_GIVEUP_LIMIT,
};
use selencode::config::{Config, ProviderTimeoutsCfg, ToolBudgetCfg};
use selencode::providers::router::{DynamicRouterProvider, ModelRoute, RouterTelemetry};
use selencode::providers::{
    AssistantReply, Message, Provider, ProviderTimeouts, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0518-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

// =========================================================================
// MASALAH 1: Fallback chain menyala untuk kelas mati-diam (0 token maupun
// macet di tengah stream), pemberitahuan terlihat, & breaker jadi jaring akhir
// =========================================================================

#[tokio::test]
async fn m1_dead_silent_and_midstream_primary_falls_back_with_notice_and_breaker_when_all_dead() {
    struct AlwaysStalePrimary {
        hits: Arc<AtomicUsize>,
        emit_partial: bool,
    }
    #[async_trait]
    impl Provider for AlwaysStalePrimary {
        fn model_name(&self) -> &str {
            "primary-dead"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            if self.emit_partial {
                let _ = tx.send(StreamEvent::Token("potongan awal ".into())).await;
            }
            Err(anyhow!(
                "timeout ketidakaktifan stream provider (tidak ada data selama 180 detik) — koneksi dihentikan"
            ))
        }
    }

    struct HealthyFallback {
        hits: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for HealthyFallback {
        fn model_name(&self) -> &str {
            "fallback-sehat"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let _ = tx
                .send(StreamEvent::Token("Jawaban tuntas dari rute fallback.".into()))
                .await;
            Ok(AssistantReply {
                text: "Jawaban tuntas dari rute fallback.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 300,
                    output_tokens: 20,
                    ..Default::default()
                }),
            })
        }
    }

    let cwd = temp_dir("m1-fallback");
    let reg = default_registry();

    // Kasus 1A: primary macet DI TENGAH STREAM (sudah kirim token parsial!) + fallback sehat ->
    // fallback WAJIB menerima request & menyelesaikan giliran dengan pemberitahuan terlihat!
    let p_hits_a = Arc::new(AtomicUsize::new(0));
    let fb_hits_a = Arc::new(AtomicUsize::new(0));
    let tel_a = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router_a: Arc<dyn Provider> = Arc::new(
        DynamicRouterProvider::new(
            vec![
                ModelRoute::new(
                    "router",
                    "primary-dead",
                    Arc::new(AlwaysStalePrimary {
                        hits: p_hits_a.clone(),
                        emit_partial: true,
                    }),
                    None,
                    None,
                    None,
                    true,
                ),
                ModelRoute::new(
                    "backup",
                    "fallback-sehat",
                    Arc::new(HealthyFallback {
                        hits: fb_hits_a.clone(),
                    }),
                    None,
                    None,
                    None,
                    false,
                ),
            ],
            false,
            HashMap::new(),
            1,
            tel_a.clone(),
        )
        .with_non_sticky_manual(true)
        .with_backoff_ms(0),
    );
    let ctx_a = Ctx::new(cwd.clone(), true)
        .with_router_telemetry(tel_a)
        .with_api_retry(3, 5);
    let mut msgs_a = Vec::new();
    let (ui_tx_a, mut ui_rx_a) = mpsc::channel(64);
    let out_a = run_turn_ui(router_a, &reg, &ctx_a, &mut msgs_a, "cek proyek", 0, ui_tx_a)
        .await
        .expect("fallback sehat wajib menyelamatkan primary yang macet di tengah stream");
    assert!(out_a.contains("Jawaban tuntas dari rute fallback."), "{out_a}");
    assert_eq!(p_hits_a.load(Ordering::SeqCst), 1);
    assert_eq!(
        fb_hits_a.load(Ordering::SeqCst),
        1,
        "rute fallback wajib menerima tepat 1 request saat primary macet di tengah stream"
    );
    let mut infos_a = Vec::new();
    while let Ok(ev) = ui_rx_a.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos_a.push(s);
        }
    }
    assert!(
        infos_a
            .iter()
            .any(|s| s.contains("Model utama gagal") && s.contains("fallback-sehat")),
        "pemberitahuan pengalihan fallback wajib terlihat: {infos_a:?}"
    );

    // Kasus 1B: primary mati-diam (0 token) + fallback juga mati-diam ->
    // keduanya dicoba lalu circuit breaker menghentikan giliran secara bersih.
    let p_hits_b = Arc::new(AtomicUsize::new(0));
    let fb_hits_b = Arc::new(AtomicUsize::new(0));
    let tel_b = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router_b: Arc<dyn Provider> = Arc::new(
        DynamicRouterProvider::new(
            vec![
                ModelRoute::new(
                    "router",
                    "primary-dead",
                    Arc::new(AlwaysStalePrimary {
                        hits: p_hits_b.clone(),
                        emit_partial: false,
                    }),
                    None,
                    None,
                    None,
                    true,
                ),
                ModelRoute::new(
                    "backup",
                    "fallback-dead",
                    Arc::new(AlwaysStalePrimary {
                        hits: fb_hits_b.clone(),
                        emit_partial: false,
                    }),
                    None,
                    None,
                    None,
                    false,
                ),
            ],
            false,
            HashMap::new(),
            1,
            tel_b.clone(),
        )
        .with_non_sticky_manual(true)
        .with_backoff_ms(0),
    );
    let ctx_b = Ctx::new(cwd.clone(), true)
        .with_router_telemetry(tel_b)
        .with_api_retry(3, 5);
    let mut msgs_b = Vec::new();
    let (ui_tx_b, _ui_rx_b) = mpsc::channel(64);
    let err_b = run_turn_ui(router_b, &reg, &ctx_b, &mut msgs_b, "halo", 0, ui_tx_b)
        .await
        .unwrap_err();
    assert_eq!(p_hits_b.load(Ordering::SeqCst), STALE_STREAM_GIVEUP_LIMIT);
    assert_eq!(fb_hits_b.load(Ordering::SeqCst), STALE_STREAM_GIVEUP_LIMIT);
    assert!(
        err_b.to_string().contains("3x beruntun"),
        "circuit breaker wajib menjadi jaring terakhir bila fallback juga mati: {err_b}"
    );

    std::fs::remove_dir_all(&cwd).ok();
}

// =========================================================================
// MASALAH 2: Akhir jujur & tersimpan saat generasi terpotong max_output_tokens
// di tengah tool call raksasa (tidak diam & tidak ada penerapan sebagian)
// =========================================================================

#[tokio::test]
async fn m2_max_output_tokens_truncation_is_visible_persisted_and_cancels_partial_tool_call() {
    assert!(is_output_length_truncated(Some("length")));
    assert!(is_output_length_truncated(Some("max_tokens")));
    assert!(is_output_length_truncated(Some("MAX_TOKENS")));
    assert!(!is_output_length_truncated(Some("stop")));

    // Kasus 2A: Langkah 0 menulis `ok.txt`, langkah 1 terpotong `length` di tengah `write_file` raksasa (`_raw`).
    struct TruncatedToolProvider {
        step: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for TruncatedToolProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-neon-medium"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let s = self.step.fetch_add(1, Ordering::SeqCst);
            if s == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "ok.txt", "content": "aman\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 500,
                        output_tokens: 80,
                        ..Default::default()
                    }),
                })
            } else {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c2".into(),
                        name: "write_file".into(),
                        args: json!({"_raw": "{\"path\":\"big.svelte\",\"content\":\"<div>terpotong..."}),
                    }],
                    finish_reason: Some("length".into()),
                    usage: Some(Usage {
                        input_tokens: 1_200,
                        output_tokens: 8_188,
                        ..Default::default()
                    }),
                })
            }
        }
    }

    let sample_trunc = AssistantReply {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "x".into(),
            name: "write_file".into(),
            args: json!({"_raw": "{\"path\":\"a"}),
        }],
        finish_reason: Some("length".into()),
        usage: None,
    };
    assert!(has_truncated_tool_call(&sample_trunc));

    let cwd = temp_dir("m2-trunc");
    let store = Arc::new(Store::open_at(&cwd.join("m2.db")).unwrap());
    let sid_a = store
        .create_session(&cwd.to_string_lossy(), "router", "ag/gemini-4-neon-medium", "uji trunc A")
        .unwrap();
    let ctx_a = Ctx::new(cwd.clone(), true).with_store(store.clone(), sid_a.clone());
    let reg = default_registry();
    let prov_a: Arc<dyn Provider> = Arc::new(TruncatedToolProvider {
        step: Arc::new(AtomicUsize::new(0)),
    });
    let mut msgs_a = Vec::new();
    let (ui_tx_a, mut ui_rx_a) = mpsc::channel(64);
    let out_a = run_turn_ui(prov_a, &reg, &ctx_a, &mut msgs_a, "tulis berkas", 0, ui_tx_a)
        .await
        .unwrap();

    // 1. Tidak ada penulisan sebagian untuk tool call yang terpotong!
    assert!(cwd.join("ok.txt").exists(), "edit langkah sebelumnya tetap tersimpan");
    assert!(
        !cwd.join("big.svelte").exists(),
        "tool call terpotong TIDAK boleh menulis sebagian ke disk"
    );
    // 2. Pesan penutup terlihat, jujur, menyebut yang sudah & belum diterapkan!
    assert!(out_a.contains(OUTPUT_TRUNCATION_HEADER), "{out_a}");
    assert!(out_a.contains("ok.txt"), "harus menyebut yang sudah diterapkan: {out_a}");
    assert!(out_a.contains("write_file"), "harus menyebut tool terpotong yang dibatalkan: {out_a}");
    // 3. Terkirim ke UI & tersimpan di SQLite!
    let mut streamed_a = String::new();
    while let Ok(ev) = ui_rx_a.try_recv() {
        if let UiEvent::Token(t) = ev {
            streamed_a.push_str(&t);
        }
    }
    assert!(streamed_a.contains(OUTPUT_TRUNCATION_HEADER), "{streamed_a}");
    let saved_a = store.load_messages(&sid_a).unwrap();
    assert!(
        saved_a
            .last()
            .map(|m| m.plain_text().contains(OUTPUT_TRUNCATION_HEADER))
            .unwrap_or(false),
        "pesan penutup pemotongan WAJIB tersimpan di DB"
    );

    // Kasus 2B: `finish_reason = "length"`, teks kosong & tool_calls kosong (8.188 ≈ 8.192 token) ->
    // TIDAK boleh berakhir diam dengan string kosong!
    struct EmptyLengthProvider;
    #[async_trait]
    impl Provider for EmptyLengthProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-neon-medium"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            Ok(AssistantReply {
                text: String::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("length".into()),
                usage: Some(Usage {
                    input_tokens: 8_000,
                    output_tokens: 8_188,
                    ..Default::default()
                }),
            })
        }
    }
    let sid_b = store
        .create_session(&cwd.to_string_lossy(), "router", "ag/gemini-4-neon-medium", "uji trunc B")
        .unwrap();
    let ctx_b = Ctx::new(cwd.clone(), true).with_store(store.clone(), sid_b.clone());
    let mut msgs_b = Vec::new();
    let (ui_tx_b, _ui_rx_b) = mpsc::channel(64);
    let out_b = run_turn_ui(
        Arc::new(EmptyLengthProvider),
        &reg,
        &ctx_b,
        &mut msgs_b,
        "tulis ulang semua",
        0,
        ui_tx_b,
    )
    .await
    .unwrap();
    assert!(out_b.contains(OUTPUT_TRUNCATION_HEADER), "{out_b}");
    assert!(out_b.contains("belum ada perubahan berkas"), "{out_b}");
    let saved_b = store.load_messages(&sid_b).unwrap();
    assert!(
        saved_b
            .last()
            .map(|m| m.plain_text().contains(OUTPUT_TRUNCATION_HEADER))
            .unwrap_or(false)
    );

    std::fs::remove_dir_all(&cwd).ok();
}

// =========================================================================
// MASALAH 3: Kesabaran dasar sesi kecil = 180 detik (sama dengan dasar Hermes)
// =========================================================================

#[test]
fn m3_base_stream_idle_is_180s_and_tiers_scale_correctly() {
    let pt = ProviderTimeouts::default();
    assert_eq!(pt.stream_idle, Duration::from_secs(180));
    assert_eq!(pt.total, Duration::from_secs(600));
    assert_eq!(pt.effective_stream_idle(1_000), Duration::from_secs(180));
    assert_eq!(pt.effective_stream_idle(16_000), Duration::from_secs(180));
    assert_eq!(pt.effective_stream_idle(50_001), Duration::from_secs(240));
    assert_eq!(pt.effective_stream_idle(100_001), Duration::from_secs(300));

    let cfg_t = ProviderTimeoutsCfg::default();
    assert_eq!(cfg_t.stream_idle_secs, 180);
    assert_eq!(cfg_t.total_secs, 600);
    assert_eq!(Config::empty().timeouts.stream_idle_secs, 180);
    assert_eq!(Config::empty().timeouts.total_secs, 600);
}

// =========================================================================
// MASALAH 4: Gerbang prune pasca-tool menyala DI GILIRAN PENELANAN ITU SENDIRI
// sebelum panggilan model berikutnya & mencatat pruned_count > 0 langsung
// =========================================================================

#[tokio::test]
async fn m4_post_tool_prune_fires_in_same_ingestion_turn_before_next_call() {
    struct BatchIngestionProvider {
        step: Arc<AtomicUsize>,
        step1_msg_tokens: Arc<Mutex<usize>>,
    }
    #[async_trait]
    impl Provider for BatchIngestionProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-neon-medium"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let s = self.step.fetch_add(1, Ordering::SeqCst);
            if s == 0 {
                // Satu batch penelanan massal: 4 bacaan berkas besar sekaligus!
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "b1".into(),
                            name: "read_file".into(),
                            args: json!({"path": "a.svelte"}),
                        },
                        ToolCall {
                            id: "b2".into(),
                            name: "read_file".into(),
                            args: json!({"path": "b.svelte"}),
                        },
                        ToolCall {
                            id: "b3".into(),
                            name: "read_file".into(),
                            args: json!({"path": "c.css"}),
                        },
                        ToolCall {
                            id: "b4".into(),
                            name: "read_file".into(),
                            args: json!({"path": "d.html"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 4_350,
                        output_tokens: 90,
                        ..Default::default()
                    }),
                })
            } else {
                *self.step1_msg_tokens.lock().unwrap() = estimate_tokens(msgs);
                let _ = tx.send(StreamEvent::Token("Selesai memeriksa batch.".into())).await;
                Ok(AssistantReply {
                    text: "Selesai memeriksa batch.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 6_500,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                })
            }
        }
    }

    let cwd = temp_dir("m4-posttool");
    for fname in ["a.svelte", "b.svelte", "c.css", "d.html"] {
        let body = format!("/* {fname} konten besar untuk uji penelanan massal */\nconst row = '0123456789abcdef';\n")
            .repeat(180); // ~14.5KB per file -> 4 file = ~58KB (~19k+ token dalam 1 batch!)
        std::fs::write(cwd.join(fname), body).unwrap();
    }

    let store = Arc::new(Store::open_at(&cwd.join("m4.db")).unwrap());
    let sid = store
        .create_session(&cwd.to_string_lossy(), "router", "ag/gemini-4-neon-medium", "uji m4")
        .unwrap();
    let mut tb = ToolBudgetCfg::default();
    tb.proactive_prune_tokens = 12_000;
    tb.proactive_prune_min_reclaim_tokens = 1_500;
    let ctx = Ctx::new(cwd.clone(), true)
        .with_context_limit(1_048_576) // jendela 1M
        .with_tool_budget(tb)
        .with_store(store.clone(), sid.clone());
    let reg = default_registry();
    let step1_tokens = Arc::new(Mutex::new(0usize));
    let prov: Arc<dyn Provider> = Arc::new(BatchIngestionProvider {
        step: Arc::new(AtomicUsize::new(0)),
        step1_msg_tokens: step1_tokens.clone(),
    });
    let mut msgs = Vec::new();
    let (ui_tx, mut ui_rx) = mpsc::channel(64);
    let out = run_turn_ui(prov, &reg, &ctx, &mut msgs, "baca 4 berkas sekaligus", 0, ui_tx)
        .await
        .unwrap();
    assert!(out.contains("Selesai memeriksa batch."));

    // 1. Panggilan model ke-2 (langkah 1) menerima konteks YANG SUDAH DIPANGKAS di akhir langkah 0!
    let seen_at_step1 = *step1_tokens.lock().unwrap();
    assert!(
        seen_at_step1 < 9_000,
        "konteks yang masuk ke panggilan langkah 1 wajib sudah dipangkas pasca-tool (<9k, bukan ~19k+): {seen_at_step1}"
    );
    // 2. Baris metrik `call_token_metrics` giliran penelanan ITU SENDIRI mencatat `pruned_count > 0`!
    let summary = store.session_token_metrics(&sid).unwrap();
    assert!(
        summary.total_pruned >= 4,
        "pruned_count pada giliran penelanan massal wajib >= 4 (4 berkas batch dipangkas pasca-tool): {summary:?}"
    );
    // 3. Pemberitahuan pasca-tool muncul di UI!
    let mut infos = Vec::new();
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos.push(s);
        }
    }
    assert!(
        infos.iter().any(|s| s.contains("Prune proaktif pasca-tool")),
        "harus ada pemberitahuan prune pasca-tool: {infos:?}"
    );

    std::fs::remove_dir_all(&cwd).ok();
}
