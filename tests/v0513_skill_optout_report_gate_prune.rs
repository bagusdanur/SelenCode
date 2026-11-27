//! Uji penerimaan v0.5.13 (Penyelesaian):
//! - Perbaikan 1: Opt-out eksplisit gerbang skill ("jangan muat skill", "tanpa skill",
//!   "langsung saja kerjakan", dll.) mematikan suntikan `skill_view` & mencatat baris terlihat
//!   `[gerbang-skill] dilewati: instruksi pengguna`, sementara giliran debugging tanpa larangan
//!   tetap menembak gerbang persis seperti v0.5.11.
//! - Perbaikan 2: Gerbang laporan terperiksa mesin (`scripts/check_release_report.py`) menolak
//!   laporan yang kehilangan baris vonis atau memiliki sel angka kosong tanpa pernyataan eksplisit,
//!   dan meloloskan laporan yang lengkap.
//! - Perbaikan 3: Memaku kontrak prune `skill_view(full=true)` tanpa mengubah perilaku:
//!   (a) hadir utuh pada panggilan LLM tepat sesudahnya,
//!   (b) mulai panggilan kedua sesudahnya sudah berupa stub + penunjuk arsip,
//!   (c) arsipnya byte-exact dan bisa diambil ulang.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::build_resume_snapshot;
use selencode::agent::{
    classify_required_skill_for_turn, prompt_explicitly_forbids_skill,
    prompt_opts_out_of_skill_gate, run_turn_counted, run_turn_ui, UiEvent,
    SKILL_GATE_SKIPPED_NOTE,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::skills::{seed_builtin_skills_to, SkillInfo};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0513_{tag}_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn db_tool_sequence(store: &Store, sid: &str) -> Vec<(String, String)> {
    let msgs = store.load_messages(sid).unwrap();
    let mut seq = Vec::new();
    for m in &msgs {
        if m.role == Role::Assistant {
            for b in &m.content {
                if let Block::ToolUse { id, name, .. } = b {
                    seq.push((id.clone(), name.clone()));
                }
            }
        }
    }
    seq
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
    panic!("python3/python 3 tidak ditemukan di PATH untuk menguji scripts/check_release_report.py");
}

// =========================================================================
// PERBAIKAN 1: Opt-out gerbang skill + catatan terlihat + kontrol tanpa larangan
// =========================================================================

#[tokio::test]
async fn p1_skill_gate_optout_suppresses_gate_logs_visibly_and_normal_turn_still_fires() {
    let dir = temp_dir("p1-optout");
    let skills_root = dir.join(".selencode").join("skills");
    seed_builtin_skills_to(&skills_root);
    let installed: Vec<SkillInfo> = selencode::skills::list_skills(&dir);

    // 1. Detektor opt-out eksplisit vs kontrol normal
    assert!(prompt_explicitly_forbids_skill(
        "Perbaiki bug di hitung_total.js, jangan muat skill apa pun."
    ));
    assert!(prompt_explicitly_forbids_skill(
        "Fix bug di app.js tanpa skill."
    ));
    assert!(prompt_opts_out_of_skill_gate(
        "Ada bug diskon di hitung_total.js, langsung saja kerjakan."
    ));
    assert!(prompt_opts_out_of_skill_gate(
        "Fix hitung_total.js, don't load any skill."
    ));
    assert!(!prompt_opts_out_of_skill_gate(
        "Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Perbaiki."
    ));

    // Klasifier mengembalikan None saat opt-out, dan tetap Some("systematic-debugging") tanpa opt-out
    let prompt_with_ban = "Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Jangan muat skill apa pun, langsung saja perbaiki.";
    let prompt_no_ban = "Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Perbaiki.";
    assert_eq!(
        classify_required_skill_for_turn(prompt_with_ban, &installed),
        None
    );
    assert_eq!(
        classify_required_skill_for_turn(prompt_no_ban, &installed).as_deref(),
        Some("systematic-debugging")
    );

    // Provider tiruan untuk skenario B (`hitung_total.js`: 35000 - 10 -> 35000 * 0.9 = 31500)
    struct ScenarioBProv {
        step: AtomicUsize,
    }
    #[async_trait]
    impl Provider for ScenarioBProv {
        fn model_name(&self) -> &str {
            "mock-scen-b-v0513"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c_read".into(),
                        name: "read_file".into(),
                        args: json!({"path": "hitung_total.js"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1200,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c_edit".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "hitung_total.js",
                            "old": "const total = 35000 - 10;",
                            "new": "const total = 35000 * 0.9;"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1350,
                        output_tokens: 35,
                        ..Default::default()
                    }),
                }),
                _ => Ok(AssistantReply {
                    text: "Bug diskon diperbaiki: hasil akhir 31500.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 1400,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                }),
            }
        }
    }

    let reg = default_registry();

    // 2. GILIRAN BERLARANGAN EKSPLISIT: nol `skill_view` di DB, catatan penekanan terlihat, bug terperbaiki!
    let ws_ban = dir.join("ws_ban");
    std::fs::create_dir_all(&ws_ban).unwrap();
    seed_builtin_skills_to(&ws_ban.join(".selencode").join("skills"));
    std::fs::write(
        ws_ban.join("hitung_total.js"),
        "const total = 35000 - 10;\nconsole.log(total);\n",
    )
    .unwrap();
    let store_ban = Arc::new(Store::open_at(&ws_ban.join("selen.db")).unwrap());
    let sid_ban = store_ban
        .create_session(&ws_ban.to_string_lossy(), "9router", "mock-scen-b-v0513", "ban")
        .unwrap();
    let ctx_ban = Ctx::new(ws_ban.clone(), true).with_store(store_ban.clone(), sid_ban.clone());
    let prov_ban: Arc<dyn Provider> = Arc::new(ScenarioBProv {
        step: AtomicUsize::new(0),
    });
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs_ban = Vec::new();
    let ans_ban = run_turn_ui(prov_ban, &reg, &ctx_ban, &mut msgs_ban, prompt_with_ban, 6, ui_tx)
        .await
        .unwrap();
    assert!(ans_ban.contains("31500"));

    let mut info_lines_ban = Vec::new();
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            info_lines_ban.push(s);
        }
    }
    assert!(
        info_lines_ban.iter().any(|l| l.contains(SKILL_GATE_SKIPPED_NOTE)),
        "catatan penekanan harus terlihat di UiEvent::Info: {info_lines_ban:?}"
    );

    let seq_ban = db_tool_sequence(&store_ban, &sid_ban);
    let names_ban: Vec<&str> = seq_ban.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names_ban,
        vec!["read_file", "edit_file"],
        "giliran berlarangan TIDAK boleh menyuntikkan skill_view: {seq_ban:?}"
    );
    assert!(
        !seq_ban.iter().any(|(id, _)| id.starts_with("gate_skill")),
        "tidak boleh ada gate_skill_* di DB: {seq_ban:?}"
    );
    let db_ban_msgs = store_ban.load_messages(&sid_ban).unwrap();
    assert!(
        db_ban_msgs
            .iter()
            .any(|m| m.plain_text().contains(SKILL_GATE_SKIPPED_NOTE)),
        "catatan penekanan harus tercatat di transkrip sesi DB"
    );
    let fixed_ban = std::fs::read_to_string(ws_ban.join("hitung_total.js")).unwrap();
    assert!(fixed_ban.contains("35000 * 0.9"), "bug wajib diperbaiki benar: {fixed_ban}");

    // 3. GILIRAN TANPA LARANGAN (KONTROL v0.5.11): gerbang menembak persis seperti v0.5.11!
    let ws_ok = dir.join("ws_ok");
    std::fs::create_dir_all(&ws_ok).unwrap();
    seed_builtin_skills_to(&ws_ok.join(".selencode").join("skills"));
    std::fs::write(
        ws_ok.join("hitung_total.js"),
        "const total = 35000 - 10;\nconsole.log(total);\n",
    )
    .unwrap();
    let store_ok = Arc::new(Store::open_at(&ws_ok.join("selen.db")).unwrap());
    let sid_ok = store_ok
        .create_session(&ws_ok.to_string_lossy(), "9router", "mock-scen-b-v0513", "ok")
        .unwrap();
    let ctx_ok = Ctx::new(ws_ok.clone(), true).with_store(store_ok.clone(), sid_ok.clone());
    let prov_ok: Arc<dyn Provider> = Arc::new(ScenarioBProv {
        step: AtomicUsize::new(0),
    });
    let (ui_tx2, mut ui_rx2) = mpsc::channel::<UiEvent>(64);
    let mut msgs_ok = Vec::new();
    let ans_ok = run_turn_ui(prov_ok, &reg, &ctx_ok, &mut msgs_ok, prompt_no_ban, 6, ui_tx2)
        .await
        .unwrap();
    assert!(ans_ok.contains("31500"));

    let mut info_lines_ok = Vec::new();
    while let Ok(ev) = ui_rx2.try_recv() {
        if let UiEvent::Info(s) = ev {
            info_lines_ok.push(s);
        }
    }
    assert!(
        !info_lines_ok.iter().any(|l| l.contains(SKILL_GATE_SKIPPED_NOTE)),
        "giliran tanpa larangan tidak boleh mencatat penekanan gerbang: {info_lines_ok:?}"
    );
    let seq_ok = db_tool_sequence(&store_ok, &sid_ok);
    let names_ok: Vec<&str> = seq_ok.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names_ok,
        vec!["read_file", "skill_view", "edit_file"],
        "giliran tanpa larangan wajib menembak gerbang skill sebelum edit_file persis v0.5.11: {seq_ok:?}"
    );
    assert_eq!(seq_ok[1].0, "gate_skill_step0");

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Gerbang laporan terperiksa mesin (3 contoh buatan + cross-check)
// =========================================================================

#[test]
fn p2_release_report_gate_rejects_missing_verdict_and_empty_cells_and_accepts_complete() {
    let dir = temp_dir("p2-report-gate");
    let py = find_python();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("check_release_report.py");
    assert!(script.is_file(), "skrip gerbang laporan harus ada: {}", script.display());

    // Contoh buatan 1: satu baris vonis hilang (Butir 2 hilang dari 3 butir) -> WAJIB DITOLAK & menyebut Butir 2
    let rep_missing = dir.join("contoh1_hilang_butir.md");
    std::fs::write(
        &rep_missing,
        "\
# Laporan Rilis v0.5.13
- **Versi:** v0.5.13
- **Jumlah butir:** 3

## Ringkasan Vonis
- Butir 1 — LULUS: Opt-out gerbang skill diuji dan bebas suntikan skill_view di DB.
- Butir 3 — LULUS: Tes kontrak prune skill_view full=true lulus penuh.

## Tabel Regresi
| Metrik | v0.5.13 | Catatan |
| --- | --- | --- |
| Tes unit | 100% (38 suite) | Semua lulus |

## Bagian yang Tidak Diubah
- Konverter Telegram v0.5.12 tetap utuh.
",
    )
    .unwrap();
    let out1 = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.13")
        .arg("--report")
        .arg(&rep_missing)
        .output()
        .unwrap();
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    assert!(
        !out1.status.success(),
        "contoh 1 (hilang Butir 2) WAJIB ditolak!"
    );
    assert!(
        stderr1.contains("Butir 2"),
        "pesan penolakan harus menyebut 'Butir 2': {stderr1}"
    );

    // Contoh buatan 2: sel angka kosong / '-' tanpa pernyataan eksplisit -> WAJIB DITOLAK & menyebut metrik/kolom
    let rep_empty_cell = dir.join("contoh2_sel_kosong.md");
    std::fs::write(
        &rep_empty_cell,
        "\
# Laporan Rilis v0.5.13
- **Versi:** v0.5.13
- **Jumlah butir:** 3

## Ringkasan Vonis
- Butir 1 — LULUS: Opt-out gerbang skill mematikan suntikan skill_view dan mencatat baris terlihat.
- Butir 2 — LULUS: Skrip pemeriksaan menolak laporan tidak lengkap dan lolos untuk laporan lengkap.
- Butir 3 — LULUS: Tes regresi memaku kontrak prune skill_view full=true beserta arsip byte-exact.

## Tabel Regresi
| Metrik | Nilai v0.5.13 | Status |
| --- | --- | --- |
| Proyek A (usage) | - | Tidak dijalankan |
| Skenario B opt-out | 31500 | LULUS |

## Bagian yang Tidak Diubah
- Konverter Telegram v0.5.12 tetap utuh.
",
    )
    .unwrap();
    let out2 = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.13")
        .arg("--report")
        .arg(&rep_empty_cell)
        .output()
        .unwrap();
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        !out2.status.success(),
        "contoh 2 (sel angka kosong tanpa pernyataan) WAJIB ditolak!"
    );
    assert!(
        stderr2.contains("Proyek A (usage)"),
        "pesan penolakan harus menyebut baris metrik kosong: {stderr2}"
    );

    // Contoh buatan 3: laporan lengkap (3 baris vonis + tabel regresi terisi angka + pernyataan eksplisit 'Tidak diukur') -> WAJIB LOLOS
    let rep_complete = dir.join("contoh3_lengkap.md");
    std::fs::write(
        &rep_complete,
        "\
# Laporan Rilis v0.5.13
- **Versi:** v0.5.13
- **Jumlah butir:** 3

## Ringkasan Vonis
- Butir 1 — LULUS: Opt-out gerbang skill mematikan suntikan skill_view dan mencatat baris terlihat.
- Butir 2 — LULUS: Skrip pemeriksaan menolak laporan tidak lengkap dan lolos untuk laporan lengkap.
- Butir 3 — LULUS: Tes regresi memaku kontrak prune skill_view full=true beserta arsip byte-exact.

## Tabel Regresi
| Metrik | Nilai v0.5.13 | Status |
| --- | --- | --- |
| Skenario B opt-out | 31500 (0 skill_view) | LULUS |
| Proyek A (usage) | Tidak diukur | Terarah |

- Tidak diukur — Proyek A (usage): Rilis v0.5.13 bersifat uji terarah tanpa baterai A/B/E penuh sesuai instruksi prompt.

## Bagian yang Tidak Diubah
- Konverter Telegram v0.5.12 tetap utuh.
",
    )
    .unwrap();
    let out3 = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.13")
        .arg("--report")
        .arg(&rep_complete)
        .output()
        .unwrap();
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    assert!(
        out3.status.success(),
        "contoh 3 (laporan lengkap) WAJIB lolos! stdout={stdout3} stderr={stderr3}"
    );
    assert!(stdout3.contains("LULUS"));

    // Cek silang dengan berkas prompt: bila prompt menyatakan 4 butir tapi laporan hanya 3 -> WAJIB DITOLAK
    let prompt4 = dir.join("prompt_4_butir.md");
    std::fs::write(&prompt4, "# Prompt Rilis\nJumlah butir: 4\n").unwrap();
    let out4 = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.13")
        .arg("--report")
        .arg(&rep_complete)
        .arg("--prompt-file")
        .arg(&prompt4)
        .output()
        .unwrap();
    let stderr4 = String::from_utf8_lossy(&out4.stderr);
    assert!(!out4.status.success(), "ketidakcocokan jumlah butir prompt vs laporan wajib ditolak!");
    assert!(stderr4.contains("Butir 4") || stderr4.contains("Ketidakcocokan jumlah butir"));

    // Verifikasi laporan v0.5.13 sendiri lolos gerbang mesin baru (memuat 3 baris vonis lengkap)
    let v0513_report = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("reports")
        .join("HASIL-V0513-PENYELESAIAN-GERBANG-LAPORAN-PRUNE.md");
    let out_self = Command::new(&py)
        .arg(&script)
        .arg("--tag")
        .arg("v0.5.13")
        .arg("--report")
        .arg(&v0513_report)
        .arg("--expected-items")
        .arg("3")
        .output()
        .unwrap();
    let stdout_self = String::from_utf8_lossy(&out_self.stdout);
    let stderr_self = String::from_utf8_lossy(&out_self.stderr);
    assert!(
        out_self.status.success(),
        "laporan rilis v0.5.13 sendiri WAJIB lolos gerbang mesin! stdout={stdout_self} stderr={stderr_self}"
    );

    // Bukti lubang v0.5.12 benar-benar tertutup: laporan v0.5.12 lama ditolak gerbang baru
    let v0512_report = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("reports")
        .join("HASIL-V0512-TAMPILAN-TELEGRAM-HTML.md");
    if v0512_report.is_file() {
        let out_old = Command::new(&py)
            .arg(&script)
            .arg("--tag")
            .arg("v0.5.12")
            .arg("--report")
            .arg(&v0512_report)
            .arg("--expected-items")
            .arg("4")
            .output()
            .unwrap();
        assert!(
            !out_old.status.success(),
            "laporan v0.5.12 lama (tanpa baris vonis 4 butir & tabel regresi terverifikasi) wajib ditolak gerbang baru"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 3: Kontrak prune `skill_view(full=true)` (tanpa mengubah perilaku)
// =========================================================================

#[tokio::test]
async fn p3_skill_view_full_true_prune_contract_exact_1_call_then_stub_and_byte_exact_archive() {
    let dir = temp_dir("p3-prune-contract");
    let skills_root = dir.join(".selencode").join("skills");
    seed_builtin_skills_to(&skills_root);

    // Ambil isi `skill_view(name="systematic-debugging", full=true)` untuk memastikan ukurannya teks penuh (>10.000 char)
    struct PruneInspectProv {
        step: AtomicUsize,
        seen_on_call1: Arc<Mutex<Option<String>>>,
        seen_on_call2: Arc<Mutex<Option<String>>>,
    }

    fn find_tool_result(msgs: &[Message], target_id: &str) -> Option<String> {
        for m in msgs {
            if m.role == Role::Tool {
                for b in &m.content {
                    if let Block::ToolResult { id, content, .. } = b {
                        if id == target_id {
                            return Some(content.clone());
                        }
                    }
                }
            }
        }
        None
    }

    #[async_trait]
    impl Provider for PruneInspectProv {
        fn model_name(&self) -> &str {
            "mock-prune-contract-v0513"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "sv_full_contract".into(),
                        name: "skill_view".into(),
                        args: json!({"name": "systematic-debugging", "full": true}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1500,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                1 => {
                    // (a) Panggilan tepat sesudah `skill_view(full=true)`: hasil teks penuh HARUS hadir utuh!
                    *self.seen_on_call1.lock().unwrap() =
                        find_tool_result(msgs, "sv_full_contract");
                    Ok(AssistantReply {
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "after_skill_step".into(),
                            name: "list_files".into(),
                            args: json!({}),
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: Some(Usage {
                            input_tokens: 5200,
                            output_tokens: 20,
                            ..Default::default()
                        }),
                    })
                }
                2 => {
                    // (b) Panggilan kedua sesudah `skill_view(full=true)`: WAJIB sudah jadi stub + penunjuk arsip!
                    *self.seen_on_call2.lock().unwrap() =
                        find_tool_result(msgs, "sv_full_contract");
                    Ok(AssistantReply {
                        text: "Selesai menerapkan prosedur debugging.".into(),
                        tool_calls: Vec::new(),
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 1800,
                            output_tokens: 20,
                            ..Default::default()
                        }),
                    })
                }
                _ => Ok(AssistantReply {
                    text: "Selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                }),
            }
        }
    }

    let seen1 = Arc::new(Mutex::new(None));
    let seen2 = Arc::new(Mutex::new(None));
    let prov: Arc<dyn Provider> = Arc::new(PruneInspectProv {
        step: AtomicUsize::new(0),
        seen_on_call1: seen1.clone(),
        seen_on_call2: seen2.clone(),
    });
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let sid = store
        .create_session(&dir.to_string_lossy(), "9router", "mock-prune-contract-v0513", "prune")
        .unwrap();
    let ctx = Ctx::new(dir.clone(), true).with_store(store, sid);
    let reg = default_registry();
    let mut msgs = Vec::new();

    // Pakai prompt non-debugging (agar gerbang tidak menyuntik salinan kerja terpisah) & model eksplisit panggil `skill_view(full=true)`
    let (ans, _) = run_turn_counted(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Lihat daftar file proyek setelah meninjau panduan lengkap.",
        6,
    )
    .await
    .unwrap();
    assert!(ans.contains("Selesai"));

    // Verifikasi (a): hadir UTUH di panggilan LLM tepat sesudahnya
    let full_on_call1 = seen1
        .lock()
        .unwrap()
        .clone()
        .expect("hasil skill_view(full=true) harus hadir pada panggilan ke-1 sesudahnya");
    assert!(
        full_on_call1.chars().count() > 10_000,
        "hasil skill_view(full=true) harus berupa teks penuh (>10.000 char), dapat {} char",
        full_on_call1.chars().count()
    );
    assert!(
        !full_on_call1.contains("isi penuh terarsip:"),
        "pada panggilan ke-1 sesudahnya, hasil skill_view(full=true) BELUM boleh di-prune"
    );

    // Verifikasi (b): mulai panggilan ke-2 sesudahnya sudah berubah menjadi stub + penunjuk arsip
    let stub_on_call2 = seen2
        .lock()
        .unwrap()
        .clone()
        .expect("entri skill_view harus tetap ada sebagai stub pada panggilan ke-2 sesudahnya");
    assert!(
        stub_on_call2.contains("[Skill `systematic-debugging` aktif — isi penuh terarsip: "),
        "pada panggilan ke-2 sesudahnya harus berupa stub + penunjuk arsip: {stub_on_call2}"
    );
    assert!(
        stub_on_call2.chars().count() < 1_000,
        "stub harus jauh lebih ringkas daripada teks penuh (dapat {} char)",
        stub_on_call2.chars().count()
    );

    // Verifikasi (c): arsipnya ada di disk, BYTE-EXACT dengan hasil penuh call #1, & bisa dibaca ulang utuh
    let marker = "isi penuh terarsip: ";
    let after_marker = stub_on_call2
        .split_once(marker)
        .map(|(_, rest)| rest)
        .expect("stub harus memuat penunjuk arsip");
    let archive_path_str = after_marker
        .split_once(']')
        .map(|(p, _)| p.trim())
        .expect("penunjuk arsip harus ditutup `]`");
    let archive_path = PathBuf::from(archive_path_str);
    assert!(
        archive_path.is_file(),
        "berkas arsip harus benar-benar ada di disk: {}",
        archive_path.display()
    );
    let archived_bytes = std::fs::read(&archive_path).unwrap();
    assert_eq!(
        archived_bytes,
        full_on_call1.as_bytes(),
        "arsip hasil skill_view(full=true) wajib BYTE-EXACT dengan hasil penuh yang diterima panggilan #1!"
    );
    // Pastikan arsip dapat diambil ulang sebagai string utuh
    let reloaded = std::fs::read_to_string(&archive_path).unwrap();
    assert_eq!(reloaded.len(), full_on_call1.len());

    // Bonus: pastikan snapshot resume setelah giliran ini tetap ramping (tidak membawa 14k char mentah)
    let mut resume_input = msgs.clone();
    resume_input.push(Message::text(Role::User, "Lanjutkan ringkas."));
    let resume_snap = build_resume_snapshot(&resume_input, Some("sys resume"));
    let total_resume_chars: usize = resume_snap
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    Block::Text(t) => t.chars().count(),
                    Block::ToolResult { content, .. } => content.chars().count(),
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum();
    assert!(
        total_resume_chars < 3_000,
        "giliran resume sesudah skill_view(full=true) wajib mulai ramping (<3000 char), dapat {total_resume_chars}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
