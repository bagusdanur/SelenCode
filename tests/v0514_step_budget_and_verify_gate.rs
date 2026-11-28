//! Uji penerimaan v0.5.14:
//! - Perbaikan 1: Kesadaran anggaran langkah (checkpoint rasio bawaan 0.7 terstruktur,
//!   pemicu konvergensi belum-ada-edit, akhir pagu pola opencode + label kegagalan eksplisit
//!   bila nol edit, penjaga pengulangan identik 3x berturut-turut dengan pengecualian poller,
//!   serta knob config `[guard] budget_warning_enabled` / `budget_warning_ratio`).
//! - Perbaikan 2: Gerbang verifikasi berbasis keadaan tercatat di loop (pola SWE-agent
//!   `review_on_submit` + refleksi keluaran gagal mentah ala Aider, batas <= 3 putaran,
//!   penutupan jujur sesudah batas, kontrol hijau 0 suntikan, kontrol tanpa verifikasi diam total).

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::corrector::{is_verification_shell_cmd, LoopGuard, LoopWarning};
use selencode::agent::{prompt_requests_file_change, run_turn_ui, UiEvent};
use selencode::config::{validate_config_str, GuardCfg};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0514_{tag}_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn all_tool_result_texts(msgs: &[Message]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs {
        if m.role == Role::Tool {
            for b in &m.content {
                if let Block::ToolResult { content, .. } = b {
                    out.push(content.clone());
                }
            }
        }
    }
    out
}

// =========================================================================
// PERBAIKAN 1: Kesadaran anggaran langkah + konvergensi + akhir pagu + 3x stall
// =========================================================================

#[tokio::test]
async fn p1_forced_small_budget_checkpoint_convergence_and_explicit_zero_edit_failure() {
    // 0. Config knobs default & validasi tanpa peringatan
    let def_guard = GuardCfg::default();
    assert!(def_guard.enabled);
    assert!(def_guard.budget_warning_enabled);
    assert!((def_guard.budget_warning_ratio - 0.7).abs() < 1e-6);

    let cfg_toml = r#"
default_provider = "router"
[providers.router]
kind = "openai"
base_url = "https://example.com/v1"
model = "test"
api_key_env = "TEST_KEY"
[guard]
enabled = true
budget_warning_enabled = true
budget_warning_ratio = 0.7
"#;
    let warns = validate_config_str(cfg_toml);
    assert!(
        warns.is_empty(),
        "kunci config [guard] baru tidak boleh memicu peringatan: {warns:?}"
    );
    assert!(prompt_requests_file_change(
        "Fix bug in pylint where invalid option crashes with traceback."
    ));
    assert!(!prompt_requests_file_change("Jelaskan apa itu borrow checker."));

    let dir = temp_dir("p1-budget");
    std::fs::write(dir.join("app.py"), "def f(x):\n    return x - 1\n").unwrap();
    let reg = default_registry();

    // 1A. Skenario paksa pagu kecil (max_steps = 6): model eksplorasi sampai checkpoint langkah 4/6,
    //     melihat pemberitahuan anggaran langkah, lalu langsung melakukan edit sebelum pagu habis!
    struct EditAfterCheckpointProv {
        step: AtomicUsize,
        saw_checkpoint: Arc<Mutex<bool>>,
        checkpoint_count: Arc<Mutex<usize>>,
    }
    #[async_trait]
    impl Provider for EditAfterCheckpointProv {
        fn model_name(&self) -> &str {
            "mock-budget-edit"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let tool_texts = all_tool_result_texts(msgs);
            let cp_hits = tool_texts
                .iter()
                .filter(|t| t.contains("[PEMBERITAHUAN ANGGARAN LANGKAH — checkpoint]"))
                .count();
            *self.checkpoint_count.lock().unwrap() = cp_hits;
            if tool_texts
                .iter()
                .any(|t| t.contains("sudah memakai 4 dari 6 langkah"))
            {
                *self.saw_checkpoint.lock().unwrap() = true;
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                0..=3 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("explore_{n}"),
                        name: "search_text".into(),
                        args: json!({"query": format!("def_{n}")}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                4 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "do_edit".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "app.py",
                            "old": "return x - 1",
                            "new": "return x + 1"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1050,
                        output_tokens: 30,
                        ..Default::default()
                    }),
                }),
                _ => Ok(AssistantReply {
                    text: "Perbaikan diterapkan di app.py sebelum pagu habis.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 1100,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                }),
            }
        }
    }

    let saw_cp = Arc::new(Mutex::new(false));
    let cp_cnt = Arc::new(Mutex::new(0));
    let prov_a: Arc<dyn Provider> = Arc::new(EditAfterCheckpointProv {
        step: AtomicUsize::new(0),
        saw_checkpoint: saw_cp.clone(),
        checkpoint_count: cp_cnt.clone(),
    });
    let ctx_a = Ctx::new(dir.clone(), true);
    let (tx_a, mut rx_a) = mpsc::channel::<UiEvent>(64);
    let mut msgs_a = Vec::new();
    let ans_a = run_turn_ui(
        prov_a,
        &reg,
        &ctx_a,
        &mut msgs_a,
        "Perbaiki bug di app.py agar mengembalikan x + 1.",
        6,
        tx_a,
    )
    .await
    .unwrap();
    assert!(*saw_cp.lock().unwrap(), "model wajib melihat checkpoint '4 dari 6 langkah'");
    assert_eq!(*cp_cnt.lock().unwrap(), 1, "checkpoint hanya boleh menembak 1 kali");
    assert!(std::fs::read_to_string(dir.join("app.py")).unwrap().contains("return x + 1"));
    assert!(ans_a.contains("app.py"));
    let mut infos_a = Vec::new();
    while let Ok(ev) = rx_a.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos_a.push(s);
        }
    }
    assert!(infos_a.iter().any(|s| s.contains("[anggaran-langkah] checkpoint: 4/6")));

    // 1B. Skenario paksa pagu kecil (max_steps = 6) dengan 0 edit sampai pagu habis:
    //     checkpoint (4/6) + konvergensi nol-edit (5/6) terlihat model, DAN akhir pagu
    //     WAJIB dibuka pernyataan kegagalan eksplisit (tidak boleh terdengar netral-sukses)!
    struct EndlessExploreProv {
        step: AtomicUsize,
        saw_convergence: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for EndlessExploreProv {
        fn model_name(&self) -> &str {
            "mock-zero-edit-maxsteps"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let tool_texts = all_tool_result_texts(msgs);
            if tool_texts
                .iter()
                .any(|t| t.contains("[PERINTAH KONVERGENSI — BELUM ADA EDIT]"))
            {
                *self.saw_convergence.lock().unwrap() = true;
            }
            if tools.is_empty() {
                // Simulasikan model mencoba menjawab wrap-up netral -> harness tetap WAJIB menempel label gagal!
                return Ok(AssistantReply {
                    text: "Sudah saya pelajari kodenya; langkah berikutnya tinggal edit app.py.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                });
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("scan_{n}"),
                    name: "search_text".into(),
                    args: json!({"query": format!("sym_{n}")}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            })
        }
    }

    let saw_conv = Arc::new(Mutex::new(false));
    let prov_b: Arc<dyn Provider> = Arc::new(EndlessExploreProv {
        step: AtomicUsize::new(0),
        saw_convergence: saw_conv.clone(),
    });
    let (tx_b, _rx_b) = mpsc::channel::<UiEvent>(64);
    let mut msgs_b = Vec::new();
    let ans_b = run_turn_ui(
        prov_b,
        &reg,
        &ctx_a,
        &mut msgs_b,
        "Perbaiki bug crash di app.py.",
        6,
        tx_b,
    )
    .await
    .unwrap();
    assert!(
        *saw_conv.lock().unwrap(),
        "pemicu konvergensi belum-ada-edit wajib terlihat model sebelum pagu habis"
    );
    assert!(
        ans_b.starts_with("Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat."),
        "akhir pagu nol-edit wajib dibuka pernyataan kegagalan eksplisit, dapat: {ans_b}"
    );

    // 1C. Knob mematikan budget_warning_enabled -> tidak menyuntikkan checkpoint
    let mut no_warn_guard = GuardCfg::default();
    no_warn_guard.budget_warning_enabled = false;
    let ctx_off = Ctx::new(dir.clone(), true).with_guard(no_warn_guard);
    let saw_cp_off = Arc::new(Mutex::new(false));
    let cp_cnt_off = Arc::new(Mutex::new(0));
    let prov_off: Arc<dyn Provider> = Arc::new(EditAfterCheckpointProv {
        step: AtomicUsize::new(0),
        saw_checkpoint: saw_cp_off.clone(),
        checkpoint_count: cp_cnt_off.clone(),
    });
    let (tx_off, _rx_off) = mpsc::channel::<UiEvent>(64);
    let mut msgs_off = Vec::new();
    let _ = run_turn_ui(
        prov_off,
        &reg,
        &ctx_off,
        &mut msgs_off,
        "Perbaiki bug di app.py.",
        6,
        tx_off,
    )
    .await
    .unwrap();
    assert_eq!(
        *cp_cnt_off.lock().unwrap(),
        0,
        "budget_warning_enabled=false harus mematikan checkpoint"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn p1_identical_repeat_notice_fires_on_3rd_consecutive_and_exempts_polling() {
    // Unit check: poller (`watch_job` status/wait) dikecualikan bawaan
    let mut lg = LoopGuard::with_default_guards();
    for _ in 0..5 {
        assert_eq!(
            lg.record_with_result(
                "watch_job",
                &json!({"action": "status", "id": "job1"}),
                "RUNNING (50%)",
                false
            ),
            LoopWarning::None
        );
    }

    // End-to-end di loop: 3 panggilan identik berturut-turut dengan hasil sama -> notice tepat di ke-3!
    let dir = temp_dir("p1-repeat3");
    std::fs::write(dir.join("note.txt"), "isi tetap\n").unwrap();
    let reg = default_registry();

    struct RepeatThreeProv {
        step: AtomicUsize,
        results_seen_per_step: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Provider for RepeatThreeProv {
        fn model_name(&self) -> &str {
            "mock-repeat3"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if let Some(last_tr) = all_tool_result_texts(msgs).last() {
                self.results_seen_per_step.lock().unwrap().push(last_tr.clone());
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("lf_{n}"),
                        name: "list_files".into(),
                        args: json!({}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                Ok(AssistantReply {
                    text: "Berhenti berputar dan menutup.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let prov: Arc<dyn Provider> = Arc::new(RepeatThreeProv {
        step: AtomicUsize::new(0),
        results_seen_per_step: seen.clone(),
    });
    let ctx = Ctx::new(dir.clone(), true);
    let (tx, _rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let _ = run_turn_ui(prov, &reg, &ctx, &mut msgs, "Daftar isi folder.", 10, tx)
        .await
        .unwrap();

    let per_step = seen.lock().unwrap().clone();
    assert_eq!(per_step.len(), 3);
    assert!(
        !per_step[0].contains("PERINGATAN PENGULANGAN IDENTIK"),
        "panggilan ke-1 tidak boleh kena notice: {}",
        per_step[0]
    );
    assert!(
        !per_step[1].contains("PERINGATAN PENGULANGAN IDENTIK"),
        "panggilan ke-2 tidak boleh kena notice: {}",
        per_step[1]
    );
    assert!(
        per_step[2].contains("[PERINGATAN PENGULANGAN IDENTIK — 3x beruntun]"),
        "notice wajib menempel tepat pada pengulangan identik ke-3: {}",
        per_step[2]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Gerbang verifikasi sebelum menutup (3 skenario + batas <=3 jujur)
// =========================================================================

#[tokio::test]
async fn p2_verification_gate_holds_red_feeds_raw_failure_and_stays_silent_on_green_or_no_verify() {
    // Pastikan detektor perintah verifikasi menangkap runner/repro nyata (termasuk SymPy `bin/test` & `node ...`)
    assert!(is_verification_shell_cmd(&json!({"command": "pytest tests/test_a.py"})));
    assert!(is_verification_shell_cmd(&json!({"command": "./bin/test sympy/physics/units/tests/test_prefixes.py"})));
    assert!(is_verification_shell_cmd(&json!({"command": "node test_hitung.js"})));
    assert!(is_verification_shell_cmd(&json!({"command": "python -c \"assert 1 == 2\""})));
    assert!(!is_verification_shell_cmd(&json!({"command": "git status"})));
    assert!(!is_verification_shell_cmd(&json!({"command": "node --version"})));

    let dir = temp_dir("p2-verify-gate");
    let reg = default_registry();

    // Helper berkas bug + skrip tes lintas-platform (Node.js)
    let buggy_js = "function hitungTotal(h, d) { return h - d; }\nmodule.exports = { hitungTotal };\n";
    let test_js = "const { hitungTotal } = require('./hitung_total.js');\nconst got = hitungTotal(35000, 10);\nif (got !== 31500) { console.error('ASSERT_FAIL: expected 31500 got ' + got); process.exit(1); }\nconsole.log('TEST_OK: 31500');\n";

    // ---------------------------------------------------------------------
    // SKENARIO 2A: Perbaikan pertama SENGAJA salah arah -> ditahan gerbang ->
    //              verifikasi ulang gagal -> keluaran gagal MENTAH dikembalikan ->
    //              perbaikan benar -> hijau -> penutupan diterima!
    // ---------------------------------------------------------------------
    let ws_fix = dir.join("ws_fix");
    std::fs::create_dir_all(&ws_fix).unwrap();
    std::fs::write(ws_fix.join("hitung_total.js"), buggy_js).unwrap();
    std::fs::write(ws_fix.join("test_hitung.js"), test_js).unwrap();

    struct WrongThenRightProv {
        step: AtomicUsize,
        gate_msgs_seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Provider for WrongThenRightProv {
        fn model_name(&self) -> &str {
            "mock-wrong-then-right"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if let Some(last) = msgs.last() {
                if last.role == Role::User && last.plain_text().contains("[Self-Correction Gate") {
                    self.gate_msgs_seen.lock().unwrap().push(last.plain_text());
                }
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                // 0: jalankan tes -> merah (34990 != 31500)
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "t0".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "node test_hitung.js"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 1: perbaikan pertama SENGAJA salah arah (h - d * 2 = 34980)
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "e1".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "hitung_total.js",
                            "old": "return h - d;",
                            "new": "return h - d * 2;"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 2: mencoba langsung menutup TANPA verifikasi ulang -> GERBANG 1/3 MENAHAN!
                2 => Ok(AssistantReply {
                    text: "Sudah saya perbaiki di hitung_total.js.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
                // 3: jalankan ulang tes sesuai perintah gerbang -> MASIH MERAH (got 34980)
                3 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "t1".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "node test_hitung.js"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 4: mencoba menutup padahal tes masih gagal -> GERBANG 2/3 MENAHAN + UMPAN KELUARAN MENTAH!
                4 => Ok(AssistantReply {
                    text: "Perbaikan selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
                // 5: perbaiki dengan benar (h - (h * d) / 100) + jalankan ulang tes -> HIJAU!
                5 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e2".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "hitung_total.js",
                                "old": "return h - d * 2;",
                                "new": "return h - (h * d) / 100;"
                            }),
                        },
                        ToolCall {
                            id: "t2".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node test_hitung.js"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 6: tutup sesudah tes hijau -> DITERIMA!
                _ => Ok(AssistantReply {
                    text: "Bug di hitung_total.js diperbaiki dan `node test_hitung.js` lulus (31500).".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
            }
        }
    }

    let gates_a = Arc::new(Mutex::new(Vec::new()));
    let prov_fix: Arc<dyn Provider> = Arc::new(WrongThenRightProv {
        step: AtomicUsize::new(0),
        gate_msgs_seen: gates_a.clone(),
    });
    let ctx_fix = Ctx::new(ws_fix.clone(), true).with_self_correct(true);
    let (tx_fix, _rx_fix) = mpsc::channel::<UiEvent>(64);
    let mut msgs_fix = Vec::new();
    let ans_fix = run_turn_ui(
        prov_fix,
        &reg,
        &ctx_fix,
        &mut msgs_fix,
        "Perbaiki bug diskon di hitung_total.js sampai node test_hitung.js lulus.",
        15,
        tx_fix,
    )
    .await
    .unwrap();
    let seen_gates = gates_a.lock().unwrap().clone();
    assert!(
        seen_gates.is_empty(),
        "v0.5.24 menghapus loop refleksi penutup (0 suntikan [Self-Correction Gate]): {seen_gates:?}"
    );
    assert!(
        ans_fix.contains("[Status Verifikasi Akhir — GAGAL]")
            && ans_fix.contains("node test_hitung.js"),
        "ketika model berhenti tanpa verifikasi ulang setelah edit, catatan penutup deterministik wajib jujur menyatakan belum terverifikasi/gagal: {ans_fix}"
    );

    // ---------------------------------------------------------------------
    // SKENARIO 2B: Kontrol HIJAU (edit benar + verifikasi lulus sesudah edit terakhir)
    //              -> penutupan diterima TANPA SATU PUN suntikan gerbang!
    // ---------------------------------------------------------------------
    let ws_green = dir.join("ws_green");
    std::fs::create_dir_all(&ws_green).unwrap();
    std::fs::write(ws_green.join("hitung_total.js"), buggy_js).unwrap();
    std::fs::write(ws_green.join("test_hitung.js"), test_js).unwrap();

    struct GreenControlProv {
        step: AtomicUsize,
        saw_any_gate: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for GreenControlProv {
        fn model_name(&self) -> &str {
            "mock-green-control"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if msgs.iter().any(|m| {
                m.plain_text().contains("[Self-Correction Gate")
                    || m.plain_text().contains("[Daftar Periksa Penutup]")
            }) {
                *self.saw_any_gate.lock().unwrap() = true;
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "eg".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "hitung_total.js",
                                "old": "return h - d;",
                                "new": "return h - (h * d) / 100;"
                            }),
                        },
                        ToolCall {
                            id: "tg".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node test_hitung.js"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                _ => Ok(AssistantReply {
                    text: "Sudah diperbaiki dan lulus 31500.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
            }
        }
    }

    let green_gate_flag = Arc::new(Mutex::new(false));
    let prov_green: Arc<dyn Provider> = Arc::new(GreenControlProv {
        step: AtomicUsize::new(0),
        saw_any_gate: green_gate_flag.clone(),
    });
    let ctx_green = Ctx::new(ws_green.clone(), true).with_self_correct(true);
    let (tx_g, _rx_g) = mpsc::channel::<UiEvent>(64);
    let mut msgs_g = Vec::new();
    let _ = run_turn_ui(
        prov_green,
        &reg,
        &ctx_green,
        &mut msgs_g,
        "Perbaiki hitung_total.js dan verifikasi.",
        10,
        tx_g,
    )
    .await
    .unwrap();
    assert!(
        !*green_gate_flag.lock().unwrap(),
        "kontrol hijau TIDAK boleh menerima satu pun suntikan gerbang!"
    );

    // ---------------------------------------------------------------------
    // SKENARIO 2C: Kontrol TANPA verifikasi (tugas coding tanpa perintah tes/repro)
    //              -> gerbang DIAM TOTAL (0 suntikan, 0 panggilan ekstra)!
    // ---------------------------------------------------------------------
    let ws_nover = dir.join("ws_nover");
    std::fs::create_dir_all(&ws_nover).unwrap();

    struct NoVerifyControlProv {
        calls: Arc<AtomicUsize>,
        saw_any_gate: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for NoVerifyControlProv {
        fn model_name(&self) -> &str {
            "mock-no-verify"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if msgs.iter().any(|m| {
                m.plain_text().contains("[Self-Correction Gate")
                    || m.plain_text().contains("[Daftar Periksa Penutup]")
            }) {
                *self.saw_any_gate.lock().unwrap() = true;
            }
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "helper.py", "content": "def add(a, b):\n    return a + b\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                Ok(AssistantReply {
                    text: "Berkas helper.py sudah dibuat.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let nover_calls = Arc::new(AtomicUsize::new(0));
    let nover_gate = Arc::new(Mutex::new(false));
    let prov_nover: Arc<dyn Provider> = Arc::new(NoVerifyControlProv {
        calls: nover_calls.clone(),
        saw_any_gate: nover_gate.clone(),
    });
    let ctx_nover = Ctx::new(ws_nover.clone(), true).with_self_correct(true);
    let (tx_nv, _rx_nv) = mpsc::channel::<UiEvent>(64);
    let mut msgs_nv = Vec::new();
    let _ = run_turn_ui(
        prov_nover,
        &reg,
        &ctx_nover,
        &mut msgs_nv,
        "Buat berkas helper.py dengan fungsi add(a, b).",
        10,
        tx_nv,
    )
    .await
    .unwrap();
    assert!(
        !*nover_gate.lock().unwrap(),
        "kontrol tanpa verifikasi wajib diam total (0 suntikan gerbang)!"
    );
    assert_eq!(
        nover_calls.load(Ordering::SeqCst),
        2,
        "kontrol tanpa verifikasi hanya boleh 2 panggilan (write_file + tutup), 0 panggilan gerbang!"
    );

    // ---------------------------------------------------------------------
    // SKENARIO 2D: Batas <= 3 putaran refleksi + penutupan jujur saat masih gagal
    // ---------------------------------------------------------------------
    let ws_cap = dir.join("ws_cap");
    std::fs::create_dir_all(&ws_cap).unwrap();
    std::fs::write(ws_cap.join("hitung_total.js"), buggy_js).unwrap();
    std::fs::write(ws_cap.join("test_hitung.js"), test_js).unwrap();

    struct AlwaysRedProv {
        step: AtomicUsize,
        gate_hits: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for AlwaysRedProv {
        fn model_name(&self) -> &str {
            "mock-always-red"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if let Some(last) = msgs.last() {
                if last.role == Role::User && last.plain_text().contains("[Self-Correction Gate") {
                    self.gate_hits.fetch_add(1, Ordering::SeqCst);
                }
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e0".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "hitung_total.js",
                                "old": "return h - d;",
                                "new": "return h - d - 1;"
                            }),
                        },
                        ToolCall {
                            id: "t0".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node test_hitung.js"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                // Terus mencoba menutup tanpa memperbaiki -> gerbang menahan maks 3 kali, lalu tutup JUJUR!
                Ok(AssistantReply {
                    text: "Sudah saya ubah hitung_total.js.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let cap_hits = Arc::new(AtomicUsize::new(0));
    let prov_cap: Arc<dyn Provider> = Arc::new(AlwaysRedProv {
        step: AtomicUsize::new(0),
        gate_hits: cap_hits.clone(),
    });
    let ctx_cap = Ctx::new(ws_cap.clone(), true).with_self_correct(true);
    let (tx_cap, _rx_cap) = mpsc::channel::<UiEvent>(64);
    let mut msgs_cap = Vec::new();
    let ans_cap = run_turn_ui(
        prov_cap,
        &reg,
        &ctx_cap,
        &mut msgs_cap,
        "Perbaiki hitung_total.js sampai node test_hitung.js lulus.",
        15,
        tx_cap,
    )
    .await
    .unwrap();
    assert_eq!(
        cap_hits.load(Ordering::SeqCst),
        0,
        "v0.5.24 menghapus loop refleksi penutup (0 putaran self-correction)"
    );
    assert!(
        ans_cap.contains("[Status Verifikasi Akhir — GAGAL]")
            && ans_cap.contains("node test_hitung.js"),
        "penutupan langsung diterima 1x tetapi WAJIB menyatakan status gagal jujur secara deterministik: {ans_cap}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
