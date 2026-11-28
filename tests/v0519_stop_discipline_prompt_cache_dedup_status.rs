use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::corrector::{
    is_dedup_passed_verify_output, is_same_or_broader_verify_cmd, is_verification_shell_cmd,
    ToolDedupCache, VerificationTracker,
};
use selencode::agent::{run_turn_ui, UiEvent};
use selencode::config::ToolBudgetCfg;
use selencode::providers::anthropic::AnthropicStreamParser;
use selencode::providers::gemini::GeminiStreamParser;
use selencode::providers::openai::{
    apply_openai_envelope_cache_control, parse_openai_usage,
    should_apply_openai_envelope_cache_control,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx, Registry, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0519-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

struct FakeVerifyShellTool {
    exec_count: Arc<AtomicUsize>,
    fail_next: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for FakeVerifyShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Mock run_shell untuk uji deterministik v0.5.19".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &Ctx) -> Result<String> {
        self.exec_count.fetch_add(1, Ordering::SeqCst);
        let cmd = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if self.fail_next.load(Ordering::SeqCst) > 0 {
            self.fail_next.fetch_sub(1, Ordering::SeqCst);
            return Ok(format!("exit code: 1\nFAILED: test_saldo ({cmd})\n"));
        }
        Ok(format!("exit code: 0\n✓ 15 passed ({cmd})\n"))
    }
}

fn registry_with_fake_shell(
    exec_count: Arc<AtomicUsize>,
    fail_next: Arc<AtomicUsize>,
) -> Registry {
    default_registry().register(Arc::new(FakeVerifyShellTool {
        exec_count,
        fail_next,
    }))
}

// =========================================================================
// MASALAH 1: Disiplin berhenti + verifikasi inkremental sesudah build+tes hijau
// =========================================================================

#[test]
fn m1_compound_svelte_verify_and_incremental_verify_after_small_edit() {
    let full_cmd = "npm run check && npm test && npm run build";
    assert!(is_verification_shell_cmd(&json!({"command": full_cmd})));
    assert!(is_verification_shell_cmd(&json!({"command": "npx svelte-check"})));
    assert!(is_verification_shell_cmd(
        &json!({"command": "npx vitest run src/lib/db.test.ts"})
    ));

    // Sub-perintah dari perintah majemuk diterima sebagai keluarga yang sah,
    // penyempitan ke 1 file tes sebelum lulus penuh ditolak (menjaga pagar v0.5.15),
    // dan probe `python -c` / `node -e` TETAP ditolak.
    assert!(is_same_or_broader_verify_cmd(full_cmd, "npm run check"));
    assert!(is_same_or_broader_verify_cmd(full_cmd, "npm test"));
    assert!(is_same_or_broader_verify_cmd(full_cmd, "npx vitest run"));
    assert!(is_same_or_broader_verify_cmd(full_cmd, "npm run build"));
    assert!(!is_same_or_broader_verify_cmd(
        full_cmd,
        "npx vitest run src/lib/db.test.ts"
    ));
    assert!(!is_same_or_broader_verify_cmd(
        full_cmd,
        "python -c \"assert 1 == 1\""
    ));
    assert!(!is_same_or_broader_verify_cmd(
        full_cmd,
        "node -e \"console.log('ok')\""
    ));

    // Alur VerificationTracker:
    // 1. Tulis aplikasi & jalankan suite penuh -> HIJAU
    let mut sc = VerificationTracker::new(None).with_strict_known_verify_only(true);
    sc.observe_tool_at_step(
        "write_file",
        &json!({"path": "src/routes/+page.svelte"}),
        "ok",
        false,
        1,
    );
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": full_cmd}),
        "exit code: 0\nsvelte-check found 0 errors\n15 passed\nbuilt in 1.2s",
        false,
        2,
    );
    assert!(sc.has_passed_green_verify);
    assert_eq!(sc.last_verify_passed, Some(true));
    assert_eq!(sc.edits_since_last_green_verify, 0);
    assert!(sc.honest_unverified_closing_note("Selesai.").is_none());

    // 2. Edit kecil (1 berkas) sesudah hijau -> status berubah menjadi belum diverifikasi ulang
    sc.observe_tool_at_step(
        "edit_file",
        &json!({"path": "src/lib/format.ts"}),
        "ok",
        false,
        3,
    );
    assert_eq!(sc.edits_since_last_green_verify, 1);
    assert!(!sc.verified_after_last_edit);
    assert!(
        sc.honest_unverified_closing_note("Selesai.").is_some(),
        "edit baru sesudah hijau harus tercatat belum diverifikasi pada catatan penutup"
    );

    // 3. Probe `node -e` tetap DITOLAK sebagai verifikasi
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": "node -e \"console.log(42)\""}),
        "exit code: 0\n42",
        false,
        4,
    );
    assert!(
        !sc.verified_after_last_edit,
        "probe node -e tidak boleh dianggap verifikasi"
    );

    // 4. Jalankan verifikasi inkremental terarah (`npx vitest run src/lib/format.test.ts`) -> diterima & HIJAU!
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": "npx vitest run src/lib/format.test.ts"}),
        "exit code: 0\n✓ 3 passed",
        false,
        5,
    );
    assert!(sc.verified_after_last_edit);
    assert_eq!(sc.last_verify_passed, Some(true));
    assert_eq!(sc.edits_since_last_green_verify, 0);
    assert!(sc.honest_unverified_closing_note("Selesai.").is_none());
}

#[tokio::test]
async fn m1_repeated_green_verify_converges_cleanly_without_stagnation_loop() {
    struct StubbornVerifyProvider {
        calls: Arc<AtomicUsize>,
        saw_stop_nudge: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for StubbornVerifyProvider {
        fn model_name(&self) -> &str {
            "mock-gemini"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            for m in msgs {
                for b in &m.content {
                    match b {
                        Block::Text(t) | Block::ToolResult { content: t, .. } => {
                            if t.contains("[DISIPLIN BERHENTI]") {
                                *self.saw_stop_nudge.lock().unwrap() = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if tools.is_empty() {
                let txt = "Aplikasi selesai dibuat pada `app.js` dan seluruh verifikasi sudah lulus."
                    .to_string();
                let _ = tx.send(StreamEvent::Token(txt.clone())).await;
                return Ok(AssistantReply {
                    text: txt,
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 1000,
                        output_tokens: 40,
                        cache_read_tokens: 800,
                        cache_write_tokens: 0,
                    }),
                });
            }
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "app.js", "content": "export const ok = true;\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 800,
                        output_tokens: 30,
                        ..Default::default()
                    }),
                }),
                // Langkah 1: verifikasi pertama kali -> benar-benar jalan dan LULUS.
                // Langkah 2, 3, 4: model membandel mengulang perintah verifikasi identik tanpa edit baru!
                1..=4 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("v{n}"),
                        name: "run_shell".into(),
                        args: json!({"command": "npm run check && npm test && npm run build"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1000,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                }),
                _ => Ok(AssistantReply {
                    text: "Selesai.".into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 1000,
                        output_tokens: 15,
                        ..Default::default()
                    }),
                }),
            }
        }
    }

    let cwd = temp_dir("m1-converge");
    let shell_execs = Arc::new(AtomicUsize::new(0));
    let reg = registry_with_fake_shell(shell_execs.clone(), Arc::new(AtomicUsize::new(0)));
    let mut ctx = Ctx::new(cwd, true);
    ctx.self_correct = true;
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_stop_nudge = Arc::new(Mutex::new(false));
    let provider: Arc<dyn Provider> = Arc::new(StubbornVerifyProvider {
        calls: calls.clone(),
        saw_stop_nudge: saw_stop_nudge.clone(),
    });
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(128);
    let mut messages = Vec::new();
    let final_text = run_turn_ui(
        provider,
        &reg,
        &ctx,
        &mut messages,
        "Buat berkas app.js lalu verifikasi proyek.",
        0,
        ui_tx,
    )
    .await
    .unwrap();

    let mut infos = Vec::new();
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos.push(s);
        }
    }

    assert_eq!(
        shell_execs.load(Ordering::SeqCst),
        1,
        "perintah verifikasi identik tanpa edit baru tidak boleh dijalankan lebih dari 1x"
    );
    assert!(
        *saw_stop_nudge.lock().unwrap(),
        "harus menyuntikkan [DISIPLIN BERHENTI]"
    );
    assert!(
        calls.load(Ordering::SeqCst) <= 6,
        "giliran harus berhenti cepat, bukan berputar panjang (actual={})",
        calls.load(Ordering::SeqCst)
    );
    assert!(
        !infos.iter().any(|s| s.contains("Loop/stagnasi terdeteksi")),
        "dedup verifikasi hijau tidak boleh memicu alarm stagnasi palsu: {infos:?}"
    );
    assert!(
        final_text.contains("Verifikasi:") && final_text.contains("(LULUS)"),
        "penutupan harus menyertakan status verifikasi LULUS, dapat: {final_text}"
    );
    assert!(
        !final_text.contains("GAGAL"),
        "penutupan terverifikasi hijau tidak boleh pernah melabeli GAGAL: {final_text}"
    );
}

// =========================================================================
// MASALAH 2: Prompt cache (parsing & SQLite usage, kebijakan cache_control,
// stabilitas prefiks tanpa rolling len-20 prune, dan histeresis rearm)
// =========================================================================

#[test]
fn m2_provider_cache_token_parsing_and_sqlite_persistence() {
    // 1. OpenAI / Gemini-via-9Router (`prompt_tokens_details.cached_tokens`)
    let u_oai = parse_openai_usage(&json!({
        "prompt_tokens": 32_000,
        "completion_tokens": 450,
        "prompt_tokens_details": {
            "cached_tokens": 27_400,
            "cache_write_tokens": 1_200
        }
    }));
    assert_eq!(u_oai.input_tokens, 32_000);
    assert_eq!(u_oai.output_tokens, 450);
    assert_eq!(u_oai.cache_read_tokens, 27_400);
    assert_eq!(u_oai.cache_write_tokens, 1_200);

    // 2. Anthropic-compatible OpenAI-wire (`cache_read_input_tokens` / `cache_creation_input_tokens`)
    let u_anth_wire = parse_openai_usage(&json!({
        "prompt_tokens": 20_000,
        "completion_tokens": 300,
        "cache_read_input_tokens": 16_000,
        "cache_creation_input_tokens": 2_000
    }));
    assert_eq!(u_anth_wire.cache_read_tokens, 16_000);
    assert_eq!(u_anth_wire.cache_write_tokens, 2_000);

    // 3. DeepSeek (`prompt_cache_hit_tokens`) & Kimi (`cached_tokens`)
    let u_ds = parse_openai_usage(&json!({
        "prompt_tokens": 15_000,
        "completion_tokens": 200,
        "prompt_cache_hit_tokens": 12_500
    }));
    assert_eq!(u_ds.cache_read_tokens, 12_500);
    let u_kimi = parse_openai_usage(&json!({
        "prompt_tokens": 10_000,
        "completion_tokens": 100,
        "cached_tokens": 8_200
    }));
    assert_eq!(u_kimi.cache_read_tokens, 8_200);

    // 4. Native AnthropicStreamParser
    let mut ap = AnthropicStreamParser::new();
    ap.handle_event(&json!({
        "type": "message_start",
        "message": {
            "usage": {
                "input_tokens": 14_000,
                "output_tokens": 0,
                "cache_read_input_tokens": 11_500,
                "cache_creation_input_tokens": 900
            }
        }
    }));
    ap.handle_event(&json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"output_tokens": 250}
    }));
    let a_usage = ap.finish().usage.expect("anthropic usage");
    assert_eq!(a_usage.cache_read_tokens, 11_500);
    assert_eq!(a_usage.cache_write_tokens, 900);

    // 5. Native GeminiStreamParser (`cachedContentTokenCount`)
    let mut gp = GeminiStreamParser::new();
    gp.handle_chunk(&json!({
        "candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 40_000,
            "candidatesTokenCount": 500,
            "cachedContentTokenCount": 34_200
        }
    }));
    let g_usage = gp.finish().usage.expect("gemini usage");
    assert_eq!(g_usage.cache_read_tokens, 34_200);
    assert_eq!(g_usage.input_tokens, 40_000);

    // 6. SQLite Store: record_usage_full + total_cache_usage + usage_report
    let dir = temp_dir("m2-store");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(
            &dir.to_string_lossy(),
            "openai",
            "ag/gemini-4-hydrogen-medium",
            "uji cache",
        )
        .unwrap();
    store
        .record_usage_full(&sid, "ag/gemini-4-hydrogen-medium", u_oai, "main")
        .unwrap();
    assert_eq!(store.total_usage(&sid).unwrap(), (32_000, 450));
    assert_eq!(store.total_cache_usage(&sid).unwrap(), (27_400, 1_200));
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let rep = store.usage_report(&sid, &today).unwrap();
    assert_eq!(rep.session_rows.len(), 1);
    assert_eq!(rep.session_rows[0].cache_read_tokens, 27_400);
    assert_eq!(rep.session_rows[0].cache_write_tokens, 1_200);
}

#[test]
fn m2_openai_envelope_cache_control_policy_and_markers() {
    // Gemini / OpenAI / localhost -> FALSE (andalkan automatic implicit prefix caching; hindari HTTP 400)
    assert!(!should_apply_openai_envelope_cache_control(
        "http://127.0.0.1:20129/v1",
        "ag/gemini-4-hydrogen-medium"
    ));
    assert!(!should_apply_openai_envelope_cache_control(
        "https://router.example.com/v1",
        "ag/gemini-4-hydrogen-medium"
    ));
    assert!(!should_apply_openai_envelope_cache_control(
        "https://api.openai.com/v1",
        "gpt-4o"
    ));
    assert!(!should_apply_openai_envelope_cache_control(
        "https://api.openai.com/v1",
        "claude-sonnet-4"
    ));

    // Claude / Kimi / Qwen pada gateway pendukung -> TRUE
    assert!(should_apply_openai_envelope_cache_control(
        "https://openrouter.ai/api/v1",
        "anthropic/claude-sonnet-4"
    ));
    assert!(should_apply_openai_envelope_cache_control(
        "https://router.example.com/v1",
        "ag/claude-sonnet-4-6"
    ));
    assert!(should_apply_openai_envelope_cache_control(
        "https://openrouter.ai/api/v1",
        "moonshotai/kimi-k2"
    ));
    assert!(should_apply_openai_envelope_cache_control(
        "https://openrouter.ai/api/v1",
        "qwen/qwen3-coder"
    ));

    // Terapkan marker maks 4 breakpoint (system + maks 3 pesan non-system/non-tool terbaru)
    let mut wire = vec![
        json!({"role": "system", "content": "Sistem SelenCode"}),
        json!({"role": "user", "content": "Pesan 1"}),
        json!({"role": "assistant", "content": "Balasan 1"}),
        json!({"role": "tool", "tool_call_id": "t1", "content": "hasil tool"}),
        json!({"role": "user", "content": "Pesan 2"}),
        json!({"role": "assistant", "content": "Balasan 2"}),
        json!({"role": "user", "content": "Pesan 3"}),
    ];
    apply_openai_envelope_cache_control(&mut wire);

    let count_markers = |arr: &[Value]| -> usize {
        arr.iter()
            .filter_map(|m| m.get("content").and_then(|c| c.as_array()))
            .flatten()
            .filter(|p| p.get("cache_control").is_some())
            .count()
    };
    assert_eq!(count_markers(&wire), 4, "maks 4 breakpoint terstruktur");
    assert!(
        wire[0]["content"][0]["cache_control"]["type"] == "ephemeral",
        "pesan system wajib ditandai ephemeral"
    );
    assert!(
        wire[3]["content"].is_string(),
        "pesan role=tool tidak boleh diubah menjadi array cache_control"
    );
    assert!(
        wire[6]["content"][0]["cache_control"]["type"] == "ephemeral",
        "pesan user terbaru wajib ditandai ephemeral"
    );
}

#[tokio::test]
async fn m2_prefix_stability_across_long_session_and_rearm_hysteresis() {
    // 1. Cek default terbaru selaras landasan SelenCode (96k pemicu di v0.5.20 / 4.096 min reclaim)
    let tb_def = ToolBudgetCfg::default();
    assert_eq!(tb_def.proactive_prune_tokens, 96_000);
    assert_eq!(tb_def.proactive_prune_min_reclaim_tokens, 4_096);

    // 2. Simulasi sesi 14 langkah (>26 pesan, melampaui batas lama len-20!)
    //    Pastikan prefiks awal TETAP 100% byte-identik di antara langkah saat di bawah ambang 28k,
    //    DAN token cache read/write dari provider tersimpan end-to-end ke tabel `usage` SQLite!
    struct LongPrefixAuditProvider {
        step: Arc<AtomicUsize>,
        snapshots: Arc<Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait]
    impl Provider for LongPrefixAuditProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-hydrogen-medium"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let serialized: Vec<String> = msgs
                .iter()
                .map(|m| {
                    let parts: Vec<String> = m
                        .content
                        .iter()
                        .map(|b| match b {
                            Block::Text(t) => format!("T:{t}"),
                            Block::ToolUse { id, name, args } => format!("U:{id}:{name}:{args}"),
                            Block::ToolResult {
                                id,
                                content,
                                is_error,
                            } => format!("R:{id}:{is_error}:{content}"),
                            Block::Image { media_type, .. } => format!("I:{media_type}"),
                        })
                        .collect();
                    format!("{:?}|{}", m.role, parts.join("|"))
                })
                .collect();
            self.snapshots.lock().unwrap().push(serialized);

            let cache_read = if n == 0 { 0 } else { 4_000 + (n as u64) * 300 };
            if n < 13 {
                Ok(AssistantReply {
                    text: format!("Langkah {n}"),
                    tool_calls: vec![ToolCall {
                        id: format!("t{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": format!("file_{n}.txt"),
                            "content": format!("isi berkas ke-{n} {}", "x".repeat(320))
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 5_000 + (n as u64) * 350,
                        output_tokens: 60,
                        cache_read_tokens: cache_read,
                        cache_write_tokens: if n == 0 { 4_000 } else { 0 },
                    }),
                })
            } else {
                let done = "Semua 13 berkas selesai dibuat.".to_string();
                let _ = tx.send(StreamEvent::Token(done.clone())).await;
                Ok(AssistantReply {
                    text: done,
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 9_600,
                        output_tokens: 50,
                        cache_read_tokens: 8_200,
                        cache_write_tokens: 0,
                    }),
                })
            }
        }
    }

    let cwd = temp_dir("m2-prefix");
    let store = Arc::new(Store::open_at(&cwd.join("selen.db")).unwrap());
    let sid = store
        .create_session(
            &cwd.to_string_lossy(),
            "openai",
            "ag/gemini-4-hydrogen-medium",
            "uji prefix",
        )
        .unwrap();
    let reg = default_registry();
    let mut ctx = Ctx::new(cwd, true);
    ctx.store = Some(store.clone());
    ctx.session_id = Some(sid.clone());
    ctx.self_correct = false;

    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(LongPrefixAuditProvider {
        step: Arc::new(AtomicUsize::new(0)),
        snapshots: snapshots.clone(),
    });
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let mut messages = Vec::new();
    let _ = run_turn_ui(
        provider,
        &reg,
        &ctx,
        &mut messages,
        "Buat 13 berkas catatan kecil bertahap.",
        0,
        ui_tx,
    )
    .await
    .unwrap();

    let snaps = snapshots.lock().unwrap();
    assert_eq!(snaps.len(), 14);
    assert!(snaps.last().unwrap().len() > 26);
    for i in 1..snaps.len() {
        let prev = &snaps[i - 1];
        let curr = &snaps[i];
        assert!(curr.len() >= prev.len());
        assert_eq!(
            &curr[..prev.len()],
            &prev[..],
            "prefiks pesan pada panggilan #{i} bermutasi secara tidak perlu — akan merusak prompt cache!"
        );
    }

    let (cr_db, cw_db) = store.total_cache_usage(&sid).unwrap();
    assert!(
        cr_db > 50_000,
        "cache_read_tokens harus tercatat di SQLite usage, dapat: {cr_db}"
    );
    assert_eq!(cw_db, 4_000, "cache_write_tokens harus tercatat di SQLite usage");

    let mut done_usage = None;
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::TurnDone { usage } = ev {
            done_usage = usage;
        }
    }
    let du = done_usage.expect("TurnDone harus membawa Usage");
    assert_eq!(du.cache_read_tokens, cr_db);
    assert_eq!(du.cache_write_tokens, cw_db);

    // 3. Uji histeresis rearm: setelah prune proaktif menyala 1× di ambang rendah,
    //    panggilan berikutnya yang hanya menambah sedikit token TIDAK boleh memicu prune lagi!
    struct HysteresisProvider {
        step: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for HysteresisProvider {
        fn model_name(&self) -> &str {
            "mock-hyst"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let actual_in = selencode::agent::context::estimate_tokens(msgs) as u64;
            match n {
                0..=2 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("h{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": format!("big_{n}.txt"),
                            "content": "A".repeat(12_000)
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: actual_in,
                        output_tokens: 30,
                        ..Default::default()
                    }),
                }),
                3..=6 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("s{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": format!("small_{n}.txt"),
                            "content": "kecil"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: actual_in,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                _ => {
                    let _ = tx.send(StreamEvent::Token("Tuntas.".into())).await;
                    Ok(AssistantReply {
                        text: "Tuntas.".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: actual_in,
                            output_tokens: 15,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let cwd_h = temp_dir("m2-hyst");
    let mut ctx_h = Ctx::new(cwd_h, true);
    ctx_h.self_correct = false;
    ctx_h.tool_budget.proactive_prune_tokens = 6_000;
    ctx_h.tool_budget.proactive_prune_min_reclaim_tokens = 1_000;
    let (htx, mut hrx) = mpsc::channel::<UiEvent>(128);
    let mut hmsgs = Vec::new();
    let _ = run_turn_ui(
        Arc::new(HysteresisProvider {
            step: Arc::new(AtomicUsize::new(0)),
        }),
        &reg,
        &ctx_h,
        &mut hmsgs,
        "Uji histeresis rearm prune proaktif.",
        0,
        htx,
    )
    .await
    .unwrap();

    let mut prune_events = 0usize;
    while let Ok(ev) = hrx.try_recv() {
        if let UiEvent::Info(s) = ev {
            if s.contains("Prune proaktif") {
                prune_events += 1;
            }
        }
    }
    assert!(
        (1..=2).contains(&prune_events),
        "histeresis rearm harus mencegah thrashing prune di setiap panggilan (terjadi {prune_events}x)"
    );
}

// =========================================================================
// MASALAH 3: Dedup pada verifikasi yang sudah LULUS tidak boleh melabeli GAGAL
// (dan kegagalan nyata TETAP dilaporkan jujur sebagai GAGAL)
// =========================================================================

#[tokio::test]
async fn m3_dedup_on_passed_verify_preserves_lulus_status_and_never_reports_gagal() {
    let cwd = temp_dir("m3-dedup-status");
    // 1. Unit level: ToolDedupCache + VerificationTracker
    let mut dedup = ToolDedupCache::new();
    let mut sc = VerificationTracker::new(None).with_strict_known_verify_only(true);
    let verify_args = json!({"command": "npm test"});
    let pass_out = "exit code: 0\n15 passed";

    sc.observe_tool_at_step("edit_file", &json!({"path": "src/app.ts"}), "ok", false, 1);
    dedup.invalidate_on_edit(Some("src/app.ts"));
    sc.observe_tool_at_step("run_shell", &verify_args, pass_out, false, 2);
    dedup.record_success("run_shell", &verify_args, pass_out, &cwd);
    assert_eq!(sc.last_verify_passed, Some(true));

    // Model mengulang `npm test` identik -> kena dedup
    let dedup_stub = dedup
        .check_hit("run_shell", &verify_args, &cwd)
        .expect("harus kena dedup");
    assert!(is_dedup_passed_verify_output(&dedup_stub));

    sc.observe_tool_at_step("run_shell", &verify_args, &dedup_stub, false, 3);
    assert_eq!(
        sc.last_verify_passed,
        Some(true),
        "dedup verifikasi yang sudah lulus WAJIB mempertahankan status LULUS"
    );
    assert!(sc.verified_after_last_edit);
    assert!(sc
        .honest_unverified_closing_note("Selesai memperbaiki src/app.ts.")
        .is_none());
    let summary_line = sc
        .closing_verification_summary_line(true)
        .expect("harus ada baris ringkasan verifikasi");
    assert!(
        summary_line.contains("LULUS"),
        "harus LULUS, dapat: {summary_line}"
    );
    assert!(
        !summary_line.contains("GAGAL"),
        "tidak boleh GAGAL, dapat: {summary_line}"
    );

    // 2. End-to-end `run_turn_ui`: edit -> verify LULUS -> ulang verify (kena dedup) -> tutup.
    //    Harus langsung tutup dengan status LULUS dan 0 putaran self-correction!
    struct DedupThenCloseProvider {
        step: Arc<AtomicUsize>,
        saw_self_correct_gate: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for DedupThenCloseProvider {
        fn model_name(&self) -> &str {
            "mock-m3"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            for m in msgs {
                if m.plain_text().contains("[Self-Correction Gate") {
                    *self.saw_self_correct_gate.lock().unwrap() = true;
                }
            }
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "big.ts", "content": "export const v = 1;\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 400,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "t1".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "npm test"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 500,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                2 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "t2".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "npm test"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 550,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                _ => {
                    let reply = "Perubahan pada `big.ts` sudah diverifikasi dan lulus.".to_string();
                    let _ = tx.send(StreamEvent::Token(reply.clone())).await;
                    Ok(AssistantReply {
                        text: reply,
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 600,
                            output_tokens: 25,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let execs = Arc::new(AtomicUsize::new(0));
    let reg = registry_with_fake_shell(execs.clone(), Arc::new(AtomicUsize::new(0)));
    let mut ctx = Ctx::new(cwd.clone(), true);
    ctx.self_correct = true;
    let saw_gate = Arc::new(Mutex::new(false));
    let (utx, _urx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let out = run_turn_ui(
        Arc::new(DedupThenCloseProvider {
            step: Arc::new(AtomicUsize::new(0)),
            saw_self_correct_gate: saw_gate.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        "Perbarui big.ts lalu verifikasi proyek.",
        0,
        utx,
    )
    .await
    .unwrap();

    assert_eq!(execs.load(Ordering::SeqCst), 1);
    assert!(
        !*saw_gate.lock().unwrap(),
        "dedup pada verifikasi yang sudah lulus tidak boleh memicu Self-Correction Gate"
    );
    assert!(
        out.contains("Verifikasi:") && out.contains("(LULUS)"),
        "output akhir harus LULUS: {out}"
    );
    assert!(
        !out.contains("GAGAL"),
        "output akhir tidak boleh memuat GAGAL: {out}"
    );

    // 3. Pagar non-regresi: kegagalan verifikasi NYATA tetap dilaporkan jujur sebagai GAGAL!
    let mut sc_fail = VerificationTracker::new(None).with_strict_known_verify_only(true);
    sc_fail.observe_tool_at_step("edit_file", &json!({"path": "src/bug.ts"}), "ok", false, 1);
    sc_fail.observe_tool_at_step(
        "run_shell",
        &json!({"command": "npm test"}),
        "exit code: 1\nFAIL src/bug.test.ts",
        false,
        2,
    );
    assert_eq!(sc_fail.last_verify_passed, Some(false));
    assert!(
        sc_fail.honest_unverified_closing_note("Selesai.").is_some(),
        "verifikasi gagal nyata WAJIB memicu catatan penutup jujur"
    );
    let fail_line = sc_fail.closing_verification_summary_line(true).unwrap();
    assert!(
        fail_line.contains("GAGAL"),
        "kegagalan nyata wajib tetap tertulis GAGAL: {fail_line}"
    );
}
