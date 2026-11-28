//! Uji unit & integrasi SelenCode v0.5.15 (4 Butir):
//! - Perbaikan 1: Bawaan `max_steps = 0` (tanpa batas terstruktur), rumus konvergensi berpagu
//!   tepat spesifikasi (sisa <= 5 ATAU >= 85%: pagu 15 -> langkah 10, pagu 25 -> langkah 20),
//!   notice konvergensi 20-langkah di mode tanpa pagu (tekanan bukan penghentian),
//!   penjaga pengulangan 3x tetap aktif, & tag langkah Telegram menyesuaikan (`langkah N` vs `langkah N/M`).
//! - Perbaikan 2: Kalimat kegagalan nol-edit ditegakkan PASCA-GENERASI secara programatis
//!   di depan teks model/templat apa pun (tidak bisa ditimpa model).
//! - Perbaikan 3: Integritas keadaan verifikasi terikat pada perintah mapan (probe `node -e`/`python3 -c`
//!   atau suite tak terkait tidak menimpa keadaan), plus kutipan hasil perintah mapan / `belum diverifikasi`
//!   pada ringkasan penutup.
//! - Perbaikan 4: Deteksi mutasi berkas tes lintas bahasa + penanda transkrip `[berkas-tes] diubah: ...`
//!   + baris wajib `Catatan berkas tes diubah: ...` di ringkasan penutup.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::corrector::{is_same_or_broader_verify_cmd, is_test_file_path};
use selencode::agent::{
    compute_convergence_step, enforce_zero_edit_failure_header, run_turn_ui, UiEvent,
    ZERO_EDIT_FAILURE_HEADER,
};
use selencode::config::{validate_config_str, AgentCfg, GuardCfg};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec,
};
use selencode::telegram::format_telegram_progress_html;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0515_{tag}_{}_{}",
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

fn find_python() -> String {
    for cand in ["python3", "python"] {
        if let Ok(out) = Command::new(cand).arg("--version").output() {
            let txt = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if out.status.success() && txt.contains("Python 3") {
                return cand.to_string();
            }
        }
    }
    panic!("python3/python 3 tidak ditemukan di PATH");
}

// =========================================================================
// PERBAIKAN 1: Tanpa pagu bawaan, titik tembak konvergensi 15 & 25, & tag Telegram
// =========================================================================

#[tokio::test]
async fn p1_unbounded_default_convergence_formula_and_telegram_step_tag() {
    // 1. Default config: tanpa batas (0) & ambang konvergensi tanpa-pagu = 20
    assert_eq!(AgentCfg::default().max_steps, 0);
    assert_eq!(GuardCfg::default().no_edit_convergence_steps, 20);
    let warns = validate_config_str(
        r#"
default_provider = "router"
[providers.router]
kind = "openai"
base_url = "https://example.com/v1"
model = "test"
api_key_env = "TEST_KEY"
[agent]
max_steps = 0
[guard]
no_edit_convergence_steps = 20
"#,
    );
    assert!(warns.is_empty(), "config baru tidak boleh memicu peringatan: {warns:?}");

    // 2. Rumus konvergensi mode berpagu disamakan tepat ke spesifikasi (sisa <= 5 ATAU >= 85%, tercapai dulu):
    assert_eq!(compute_convergence_step(15), 10, "pagu 15 harus menembak di langkah 10 (sisa 5)");
    assert_eq!(compute_convergence_step(25), 20, "pagu 25 harus menembak di langkah 20 (sisa 5)");
    assert_eq!(compute_convergence_step(100), 85, "pagu 100 harus menembak di langkah 85 (85%)");

    // 3. Tag langkah Telegram menyesuaikan: berpagu -> `langkah N/M`, tanpa pagu (0) -> `langkah N`
    let bounded_tag = format_telegram_progress_html(3, 25, Some("read_file"), 1, "");
    assert!(bounded_tag.contains("langkah 3/25"), "got: {bounded_tag}");
    let unbounded_tag = format_telegram_progress_html(7, 0, Some("read_file"), 2, "");
    assert!(unbounded_tag.contains("langkah 7"), "got: {unbounded_tag}");
    assert!(!unbounded_tag.contains("7/0"), "tanpa pagu tidak boleh tampil /0: {unbounded_tag}");
    let unbounded_stream = format_telegram_progress_html(4, 0, None, 1, "Menyusun jawaban");
    assert!(unbounded_stream.contains("langkah 4"), "got: {unbounded_stream}");
    assert!(!unbounded_stream.contains("4/0"));

    // 4. Mode berpagu (max_steps = 15): konvergensi benar-benar menembak di langkah 10 (BUKAN 12!)
    let dir = temp_dir("p1-conv15");
    std::fs::write(dir.join("mod.py"), "x = 1\n").unwrap();
    let reg = default_registry();

    struct ProbeStepConvProv {
        step: AtomicUsize,
        conv_step_seen: Arc<Mutex<Option<usize>>>,
        cp_count: Arc<Mutex<usize>>,
        stop_after: usize,
    }
    #[async_trait]
    impl Provider for ProbeStepConvProv {
        fn model_name(&self) -> &str {
            "mock-conv-probe"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let texts = all_tool_result_texts(msgs);
            *self.cp_count.lock().unwrap() = texts
                .iter()
                .filter(|t| t.contains("[PEMBERITAHUAN ANGGARAN LANGKAH — checkpoint]"))
                .count();
            if self.conv_step_seen.lock().unwrap().is_none()
                && texts
                    .iter()
                    .any(|t| t.contains("[PERINTAH KONVERGENSI — BELUM ADA EDIT]"))
            {
                // Notice ditempel di akhir step (n - 1), yakni langkah 1-based = n
                *self.conv_step_seen.lock().unwrap() = Some(n);
            }
            if n < self.stop_after {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("s_{n}"),
                        name: "search_text".into(),
                        args: json!({"query": format!("q_{n}")}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                Ok(AssistantReply {
                    text: "Selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let seen_at_15 = Arc::new(Mutex::new(None));
    let cp_15 = Arc::new(Mutex::new(0));
    let prov15: Arc<dyn Provider> = Arc::new(ProbeStepConvProv {
        step: AtomicUsize::new(0),
        conv_step_seen: seen_at_15.clone(),
        cp_count: cp_15.clone(),
        stop_after: 12,
    });
    let ctx15 = Ctx::new(dir.clone(), true);
    let (tx15, _rx15) = mpsc::channel::<UiEvent>(64);
    let mut msgs15 = Vec::new();
    let _ = run_turn_ui(prov15, &reg, &ctx15, &mut msgs15, "Perbaiki bug di mod.py.", 15, tx15)
        .await
        .unwrap();
    assert_eq!(
        *seen_at_15.lock().unwrap(),
        Some(10),
        "pagu 15 WAJIB menembakkan konvergensi tepat di langkah 10 (sisa 5), bukan langkah 12!"
    );

    // 5. Mode TANPA PAGU (max_steps = 0) dengan ambang no_edit_convergence_steps = 4:
    //    menembakkan 1 notice konvergensi di langkah 4, TIDAK berhenti (lanjut sampai langkah 6), & 0 checkpoint pagu!
    let mut g_unbounded = GuardCfg::default();
    g_unbounded.no_edit_convergence_steps = 4;
    let ctx_unb = Ctx::new(dir.clone(), true).with_guard(g_unbounded);
    let seen_unb = Arc::new(Mutex::new(None));
    let cp_unb = Arc::new(Mutex::new(0));
    let prov_unb: Arc<dyn Provider> = Arc::new(ProbeStepConvProv {
        step: AtomicUsize::new(0),
        conv_step_seen: seen_unb.clone(),
        cp_count: cp_unb.clone(),
        stop_after: 6,
    });
    let (tx_u, _rx_u) = mpsc::channel::<UiEvent>(64);
    let mut msgs_u = Vec::new();
    let ans_u = run_turn_ui(prov_unb, &reg, &ctx_unb, &mut msgs_u, "Perbaiki bug di mod.py.", 0, tx_u)
        .await
        .unwrap();
    assert_eq!(*seen_unb.lock().unwrap(), Some(4), "mode tanpa pagu wajib menyuntik konvergensi di ambang langkah 4");
    assert_eq!(*cp_unb.lock().unwrap(), 0, "mode tanpa pagu tidak boleh menyuntik checkpoint rasio pagu");
    assert!(ans_u.contains("Selesai."), "notice tanpa pagu adalah tekanan, bukan penghentian (lanjut sampai selesai)");

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Penegakan PASCA-GENERASI kalimat kegagalan nol-edit
// =========================================================================

#[tokio::test]
async fn p2_zero_edit_failure_header_enforced_post_generation_unconditionally() {
    // 1. Unit check langsung pada helper pasca-generasi:
    let neutral = "Pekerjaan dihentikan sementara karena batas langkah tercapai.";
    let enforced = enforce_zero_edit_failure_header(neutral, true);
    assert!(enforced.starts_with(ZERO_EDIT_FAILURE_HEADER));
    assert!(enforced.contains(neutral));

    // Bahkan bila model memakai kalimat "Gagal: ..." versi sendiri -> tetap wajib ditempel kalimat baku!
    let custom_gagal = "Gagal menyelesaikan tepat waktu; langkah berikutnya edit foo.py.";
    let enforced2 = enforce_zero_edit_failure_header(custom_gagal, true);
    assert!(enforced2.starts_with(ZERO_EDIT_FAILURE_HEADER));
    assert!(enforced2.contains(custom_gagal));

    // Di luar kondisi nol-edit (ada mutasi / bukan tugas perubahan) -> TIDAK ada penempelan apa pun!
    assert_eq!(enforce_zero_edit_failure_header(neutral, false), neutral);

    // 2. End-to-end loop: tugas perubahan berakhir di pagu nol-edit vs tugas baca-saja berakhir di pagu
    let dir = temp_dir("p2-postgen");
    std::fs::write(dir.join("foo.py"), "x = 1\n").unwrap();
    let reg = default_registry();

    struct NeutralWrapupProv {
        step: AtomicUsize,
    }
    #[async_trait]
    impl Provider for NeutralWrapupProv {
        fn model_name(&self) -> &str {
            "mock-neutral-wrapup"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if tools.is_empty() {
                return Ok(AssistantReply {
                    text: "Proses dihentikan sementara karena batas langkah; analisis sudah selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                });
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("r_{n}"),
                    name: "search_text".into(),
                    args: json!({"query": format!("k_{n}")}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: None,
            })
        }
    }

    let ctx = Ctx::new(dir.clone(), true);
    let (tx1, _rx1) = mpsc::channel::<UiEvent>(64);
    let mut msgs1 = Vec::new();
    let ans_edit = run_turn_ui(
        Arc::new(NeutralWrapupProv { step: AtomicUsize::new(0) }),
        &reg,
        &ctx,
        &mut msgs1,
        "Perbaiki bug di foo.py.",
        3,
        tx1,
    )
    .await
    .unwrap();
    assert!(
        ans_edit.starts_with(ZERO_EDIT_FAILURE_HEADER),
        "harus ditempel pasca-generasi di depan teks model: {ans_edit}"
    );

    // Non-edit task: TIDAK boleh ditempel header kegagalan nol-edit!
    let (tx2, _rx2) = mpsc::channel::<UiEvent>(64);
    let mut msgs2 = Vec::new();
    let ans_ro = run_turn_ui(
        Arc::new(NeutralWrapupProv { step: AtomicUsize::new(0) }),
        &reg,
        &ctx,
        &mut msgs2,
        "Jelaskan isi folder ini tanpa mengubah apa pun.",
        3,
        tx2,
    )
    .await
    .unwrap();
    assert!(
        !ans_ro.contains(ZERO_EDIT_FAILURE_HEADER),
        "tugas baca-saja tidak boleh ditempel header kegagalan nol-edit: {ans_ro}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 3: Integritas keadaan verifikasi (probe tak menimpa) + kutipan penutup
// =========================================================================

#[tokio::test]
async fn p3_verification_state_bound_to_established_cmd_not_overwritten_by_probes() {
    // 1. Unit checks pada `is_same_or_broader_verify_cmd`:
    assert!(is_same_or_broader_verify_cmd("python3 uji.py", "python uji.py"));
    assert!(!is_same_or_broader_verify_cmd(
        "python3 uji.py",
        "python3 -c \"print('ok')\""
    ));
    assert!(!is_same_or_broader_verify_cmd(
        "pytest tests/test_prefixes.py",
        "pytest tests/test_other.py"
    ));
    assert!(is_same_or_broader_verify_cmd(
        "pytest tests/test_prefixes.py::test_mul",
        "pytest tests/test_prefixes.py"
    ));
    assert!(is_same_or_broader_verify_cmd(
        "pytest tests/test_prefixes.py",
        "pytest tests/"
    ));
    assert!(is_same_or_broader_verify_cmd(
        "pytest tests/test_prefixes.py",
        "pytest"
    ));

    // 2. End-to-end: `node uji.js` gagal (exit 1) -> edit salah -> probe `node -e "console.log('ok')"` (exit 0) ->
    //    coba tutup -> GERBANG TETAP MENAHAN dengan nama `node uji.js` (probe TIDAK menimpa!) ->
    //    perbaiki + jalankan ulang `node uji.js` (exit 0) -> tutup diterima & penutup mengutip `node uji.js`!
    let dir = temp_dir("p3-integrity");
    let ws = dir.join("ws_probe");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        ws.join("calc.js"),
        "exports.calc = (h, d) => h - d;\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("uji.js"),
        "const { calc } = require('./calc.js');\nconst v = calc(35000, 10);\nif (v !== 31500) { console.error('FAIL_UJI: got ' + v); process.exit(1); }\nconsole.log('PASS_UJI: 31500');\n",
    )
    .unwrap();

    struct ProbeOverwriteProv {
        step: AtomicUsize,
        gate_msgs: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Provider for ProbeOverwriteProv {
        fn model_name(&self) -> &str {
            "mock-probe-integrity"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if let Some(last) = msgs.last() {
                if last.role == Role::User && last.plain_text().contains("[Self-Correction Gate") {
                    self.gate_msgs.lock().unwrap().push(last.plain_text());
                }
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                // 0: jalankan perintah verifikasi mapan `node uji.js` -> GAGAL (exit 1)
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "u0".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "node uji.js"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 1: edit masih keliru + jalankan probe ad-hoc `node -e "console.log('probe ok')"` (exit 0!)
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e1".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "calc.js",
                                "old": "exports.calc = (h, d) => h - d;",
                                "new": "exports.calc = (h, d) => h - d * 2;"
                            }),
                        },
                        ToolCall {
                            id: "probe1".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node -e \"console.log('probe ok')\""}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                // 2: coba tutup seolah-olah probe exit 0 sudah meluluskan -> GERBANG WAJIB MENAHAN!
                2 => Ok(AssistantReply {
                    text: "Sudah saya perbaiki calc.js, probe ok.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
                // 3: perbaiki benar + jalankan ulang perintah mapan `node uji.js` -> LULUS!
                3 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e2".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "calc.js",
                                "old": "exports.calc = (h, d) => h - d * 2;",
                                "new": "exports.calc = (h, d) => h - (h * d) / 100;"
                            }),
                        },
                        ToolCall {
                            id: "u2".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node uji.js"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                }),
                _ => Ok(AssistantReply {
                    text: "Perbaikan pada calc.js selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
            }
        }
    }

    let reg = default_registry();
    let gates = Arc::new(Mutex::new(Vec::new()));
    let ctx = Ctx::new(ws.clone(), true).with_self_correct(true);
    let (tx, _rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let ans = run_turn_ui(
        Arc::new(ProbeOverwriteProv {
            step: AtomicUsize::new(0),
            gate_msgs: gates.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        "Perbaiki calc.js sampai node uji.js lulus.",
        0,
        tx,
    )
    .await
    .unwrap();

    let seen_gates = gates.lock().unwrap().clone();
    assert!(
        seen_gates.is_empty(),
        "v0.5.24 menghapus loop refleksi penutup (0 suntikan [Self-Correction Gate]): {seen_gates:?}"
    );
    assert!(
        ans.contains("[Status Verifikasi Akhir — GAGAL]") && ans.contains("node uji.js"),
        "probe `node -e` exit 0 TIDAK boleh menimpa keadaan gagal `node uji.js` pada ringkasan penutup deterministik: {ans}"
    );
    assert!(
        !ans.contains("Verifikasi: `node -e"),
        "probe `node -e` tidak boleh tercatat sebagai perintah verifikasi utama: {ans}"
    );

    // 3. Tugas perubahan TANPA verifikasi: gerbang diam, tapi ringkasan menyatakan "belum diverifikasi"
    let ws_nv = dir.join("ws_nover");
    std::fs::create_dir_all(&ws_nv).unwrap();
    struct NoVerProv(AtomicUsize);
    #[async_trait]
    impl Provider for NoVerProv {
        fn model_name(&self) -> &str {
            "mock-nover"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "util.py", "content": "def ok(): return 1\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                Ok(AssistantReply {
                    text: "Berkas util.py sudah dibuat.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }
    let ctx_nv = Ctx::new(ws_nv.clone(), true).with_self_correct(true);
    let (tx_nv, _rx_nv) = mpsc::channel::<UiEvent>(64);
    let mut msgs_nv = Vec::new();
    let ans_nv = run_turn_ui(
        Arc::new(NoVerProv(AtomicUsize::new(0))),
        &reg,
        &ctx_nv,
        &mut msgs_nv,
        "Buat util.py berisi fungsi ok().",
        0,
        tx_nv,
    )
    .await
    .unwrap();
    assert!(
        ans_nv.contains("belum diverifikasi"),
        "tugas perubahan tanpa verifikasi harus menyatakan 'belum diverifikasi': {ans_nv}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 4: Visibilitas mutasi berkas tes + cek mandiri laporan v0.5.15
// =========================================================================

#[tokio::test]
async fn p4_test_file_mutations_visible_in_transcript_and_closing_and_v0515_report_passes() {
    // 1. Unit checks pola lintas bahasa `is_test_file_path`:
    for yes in [
        "test_discount.py",
        "pkg/tests/test_lint.py",
        "src/__tests__/app.js",
        "calc.spec.ts",
        "calc.test.tsx",
        "util_test.go",
        "foo_test.rs",
        "tests/v0515.rs",
        "src/test/java/AppTest.java",
    ] {
        assert!(is_test_file_path(yes), "harus dikenali sebagai berkas tes: {yes}");
    }
    for no in ["src/main.rs", "calc.js", "pylint/config.py", "README.md"] {
        assert!(!is_test_file_path(no), "bukan berkas tes: {no}");
    }

    // 2. End-to-end: ubah berkas sumber + berkas tes -> transkrip & ringkasan penutup wajib menandai!
    let dir = temp_dir("p4-test-vis");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("app.py"), "x = 1\n").unwrap();
    std::fs::write(dir.join("tests/test_app.py"), "assert True\n").unwrap();

    struct EditTestFileProv(AtomicUsize);
    #[async_trait]
    impl Provider for EditTestFileProv {
        fn model_name(&self) -> &str {
            "mock-edit-test-file"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e_src".into(),
                            name: "edit_file".into(),
                            args: json!({"path": "app.py", "old": "x = 1", "new": "x = 2"}),
                        },
                        ToolCall {
                            id: "e_tst".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "tests/test_app.py",
                                "old": "assert True",
                                "new": "assert 2 == 2"
                            }),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                Ok(AssistantReply {
                    text: "Perubahan selesai pada app.py.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true).with_self_correct(true);
    let (tx, mut rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let ans = run_turn_ui(
        Arc::new(EditTestFileProv(AtomicUsize::new(0))),
        &reg,
        &ctx,
        &mut msgs,
        "Perbarui app.py dan tesnya.",
        0,
        tx,
    )
    .await
    .unwrap();

    let mut infos = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            infos.push(s);
        }
    }
    assert!(
        infos.iter().any(|l| l.contains("[berkas-tes] diubah: tests/test_app.py")),
        "transkrip sesi wajib menandai mutasi berkas tes saat terjadi: {infos:?}"
    );
    assert!(
        ans.contains("Catatan berkas tes diubah: tests/test_app.py"),
        "ringkasan penutup wajib memuat baris khusus berkas tes yang diubah: {ans}"
    );

    // 3. Verifikasi laporan rilis v0.5.15 (4 butir + 'pengukuran penerimaan oleh Muse') lolos gerbang mesin!
    let py = find_python();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts").join("check_release_report.py");
    let report = root
        .join("docs")
        .join("reports")
        .join("HASIL-V0515-HAPUS-PAGU-DAN-TIGA-UTANG.md");
    let out = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.15")
        .arg("--report")
        .arg(&report)
        .arg("--expected-items")
        .arg("4")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "laporan rilis v0.5.15 wajib lolos check_release_report.py! stdout={stdout} stderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
