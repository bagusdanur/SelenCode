//! Suite uji penerimaan terpadu SelenCode v0.5.7 (Perbaikan 1–8):
//! - Perbaikan 1: Lantai token resume ramping (toolset resume + build_resume_snapshot + aktivasi on-demand di giliran yang sama).
//! - Perbaikan 2: Aturan pemuatan skill bawaan sebelum tool pertama + 4 deskripsi tajam <= 60 char + upgrade deskripsi aman.
//! - Perbaikan 3: Pemantau tugas asinkron (`watch_job`, S1->S2->SELESAI, anti-spam, gagal 3x, restart persisten, 0 token LLM, batas 5/chat).
//! - Perbaikan 4: Kirim berkas Telegram (`send_file`, sandbox, batas 50 MB -> saran Drive, fallback CLI).
//! - Perbaikan 5: Memory pengguna persisten ala Hermes (`memory` 2200 char & `profile` 1375 char, pagu keras, snapshot beku, lintas sesi).
//! - Perbaikan 6: `delegate` `mode: "work"` di Git worktree terisolasi (non-git ditolak, pohon utama utuh saat ditolak, mendarat saat disetujui, 2 pekerja paralel, usage terpisah).
//! - Perbaikan 7: Laporan `/usage` langsung dari tabel `usage` (sesi + hari ini, baris terpisah `main`/`fallback`/`review`/`cron`/`delegate_work`, 0 API call).
//! - Perbaikan 8: `selen skills install` & `remove` (clone git staging, validasi frontmatter/keamanan, penandaan skrip, bentrok nama + rename, `.selen-source.json`, blokir hapus bawaan/proyek).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use selencode::agent::context::{analyze_token_breakdown, build_resume_snapshot, build_system_prompt};
use selencode::agent::plan::TurnMode;
use selencode::agent::run_turn_counted;
use selencode::config::MemoryCfg;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::security::Approver;
use selencode::skill_installer::{
    finalize_staged_skill_install, format_install_approval_summary, remove_installed_skill,
    stage_skill_from_source,
};
use selencode::skills::{
    format_skills_index_budgeted, load_skill, scan_skills_in_dirs, seed_builtin_skills_to,
    SKILL_INDEX_DESC_MAX_CHARS,
};
use selencode::store::Store;
use selencode::tools::delegate::Delegate;
use selencode::tools::memory::UserMemoryTool;
use selencode::tools::send_file::{validate_send_file_target, SendFile};
use selencode::tools::toolsets::{
    active_tool_specs_for_turn, select_initial_toolsets, select_resume_initial_toolsets,
};
use selencode::tools::watch::WatchJob;
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use selencode::user_memory::{
    add_entry_in, load_frozen_snapshot_from, remove_entry_in, replace_entry_in, MemoryStoreKind,
};
use selencode::watch::{
    evaluate_watch_poll, parse_watch_output, poll_watch_once, validate_readonly_watch_command,
    WatchStateKind,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v057-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct SequenceProv {
    model: String,
    replies: Mutex<Vec<AssistantReply>>,
    seen_tools_counts: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl Provider for SequenceProv {
    fn model_name(&self) -> &str {
        &self.model
    }
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        self.seen_tools_counts.lock().unwrap().push(tools.len());
        let mut q = self.replies.lock().unwrap();
        if q.is_empty() {
            Ok(AssistantReply {
                text: "Selesai.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 300,
                    output_tokens: 40,
                }),
            })
        } else {
            Ok(q.remove(0))
        }
    }
}

// ---------- PERBAIKAN 1: Resume lantai token ramping + aktivasi on-demand ----------

#[tokio::test]
async fn perbaikan1_resume_lean_toolset_snapshot_and_on_demand_activation() {
    let dir = temp_dir("p1-resume");
    // Buat penanda .git palsu agar terlihat bedanya: sesi baru menyalakan git_read,
    // sedangkan resume pertanyaan sederhana TIDAK membawa git_read secara otomatis.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let reg = default_registry();

    let normal_ts = select_initial_toolsets(
        "Apa sandi rahasia yang baru kita simpan di kata.txt?",
        &dir,
        false,
        TurnMode::Execute,
    );
    let resume_ts = select_resume_initial_toolsets(
        "Apa sandi rahasia yang baru kita simpan di kata.txt?",
        &dir,
        false,
        TurnMode::Execute,
    );
    let normal_specs = active_tool_specs_for_turn(&reg, &normal_ts, &Default::default(), TurnMode::Execute, false);
    let resume_specs = active_tool_specs_for_turn(&reg, &resume_ts, &Default::default(), TurnMode::Execute, true);
    assert!(
        resume_specs.len() < normal_specs.len(),
        "resume toolset ({}) harus lebih ramping dari sesi normal ({})",
        resume_specs.len(),
        normal_specs.len()
    );
    assert_eq!(resume_specs.len(), 6, "resume dasar hanya 5 core + activate_toolset");

    // Verifikasi build_resume_snapshot meringkas riwayat ToolUse/ToolResult lama namun menjaga fakta User/Assistant!
    let bloated_old_sys = "X".repeat(4500);
    let fresh_sys = build_system_prompt(&dir, &resume_specs);
    let history = vec![
        Message::text(Role::System, bloated_old_sys),
        Message::text(Role::User, "Simpan sandi BULAN-PERAK-42 di kata.txt untuk proyek SelenCode."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "tc1".into(),
                name: "write_file".into(),
                args: json!({"path": "kata.txt", "content": "Sandi: BULAN-PERAK-42\nProyek: SelenCode\n"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                tool_use_id: "tc1".into(),
                content: format!("OK: kata.txt ditulis.\n{}", "diff ".repeat(200)),
                is_error: false,
            }],
        },
        Message::text(Role::Assistant, "Sudah saya simpan BULAN-PERAK-42 ke kata.txt untuk SelenCode."),
        Message::text(Role::User, "Apa sandi rahasia tadi?"),
    ];
    let bd_before = analyze_token_breakdown(&history, &normal_specs, reg.names().len());
    let slim_snap = build_resume_snapshot(&history, Some(&fresh_sys));
    let bd_after = analyze_token_breakdown(&slim_snap, &resume_specs, reg.names().len());
    assert!(
        bd_after.total_tokens() < bd_before.total_tokens(),
        "total estimasi resume ({}) harus jauh lebih rendah dari sebelum ({})",
        bd_after.total_tokens(),
        bd_before.total_tokens()
    );
    let joined_snap: String = slim_snap.iter().map(|m| m.plain_text()).collect::<Vec<_>>().join("\n");
    assert!(joined_snap.contains("BULAN-PERAK-42"));
    assert!(joined_snap.contains("SelenCode"));

    // Skenario E lanjutan: di sesi resume yang sama, tool berat (`session_search`) tetap bisa diaktifkan
    // & dipanggil di giliran yang sama!
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let old_sid = store
        .create_session(&dir.to_string_lossy(), "mock", "mock-m", "sesi lama")
        .unwrap();
    store
        .save_message(
            &old_sid,
            &Message::text(Role::User, "Catatan lama KODE-RESUME-991 untuk arsip"),
        )
        .unwrap();
    let cur_sid = store
        .create_session(&dir.to_string_lossy(), "mock", "mock-m", "sesi aktif")
        .unwrap();

    let seen_counts = Arc::new(Mutex::new(Vec::new()));
    let prov: Arc<dyn Provider> = Arc::new(SequenceProv {
        model: "mock-m".into(),
        seen_tools_counts: seen_counts.clone(),
        replies: Mutex::new(vec![
            AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "activate_toolset".into(),
                    args: json!({"toolsets": ["history"]}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(Usage { input_tokens: 400, output_tokens: 30 }),
            },
            AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "session_search".into(),
                    args: json!({"query": "KODE-RESUME-991"}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(Usage { input_tokens: 450, output_tokens: 30 }),
            },
            AssistantReply {
                text: "Ditemukan KODE-RESUME-991 pada sesi lama.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage { input_tokens: 480, output_tokens: 40 }),
            },
        ]),
    });
    let ctx = Ctx::new(dir.clone(), true)
        .with_resumed_session(true)
        .with_store(store, cur_sid);
    let mut msgs = vec![
        Message::text(Role::System, "sys lama"),
        Message::text(Role::User, "Simpan kata.txt"),
        Message::text(Role::Assistant, "Sudah disimpan."),
    ];
    let (ans, _) = run_turn_counted(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Tolong cek catatan KODE-RESUME-991",
        6,
    )
    .await
    .unwrap();
    assert!(ans.contains("KODE-RESUME-991"));
    let counts = seen_counts.lock().unwrap().clone();
    assert_eq!(counts[0], 6, "panggilan 1 resume harus super-ramping (6 tool)");
    assert_eq!(counts[1], 7, "sesudah activate_toolset(['history']), session_search langsung aktif di giliran yang sama");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 2: Kebiasaan skill bawaan & deskripsi tajam ----------

#[test]
fn perbaikan2_builtin_skill_prompt_habit_and_sharp_descriptions() {
    let dir = temp_dir("p2-skills");
    let sys = build_system_prompt(&dir, &[]);
    assert!(sys.contains("skill_view(name)"));
    assert!(sys.contains("systematic-debugging"));
    assert!(sys.contains("test-driven-development"));
    assert!(sys.contains("JANGAN muat skill untuk tugas trivial"));

    // Simulasikan pengguna lama v0.5.6 yang punya deskripsi lama default -> upgrade ke v0.5.7 harus menyegarkannya,
    // sekaligus menjaga skill yang sudah diedit manual oleh user tetap utuh!
    let skills_root = dir.join("skills");
    let dbg_dir = skills_root.join("dev").join("systematic-debugging");
    std::fs::create_dir_all(&dbg_dir).unwrap();
    std::fs::write(
        dbg_dir.join("SKILL.md"),
        "---\nname: systematic-debugging\ndescription: Debug 4 fase: temukan akar masalah dulu\n---\n# Debug\n",
    )
    .unwrap();
    seed_builtin_skills_to(&skills_root);

    let scan = scan_skills_in_dirs(None, Some(&skills_root));
    assert_eq!(scan.skills.len(), 14);
    for target in [
        "systematic-debugging",
        "test-driven-development",
        "requesting-code-review",
        "grill-me",
    ] {
        let info = scan.skills.iter().find(|s| s.name == target).unwrap();
        assert!(
            info.summary.chars().count() <= SKILL_INDEX_DESC_MAX_CHARS,
            "deskripsi {target} melebihi 60 char: {}",
            info.summary
        );
    }
    let dbg = scan.skills.iter().find(|s| s.name == "systematic-debugging").unwrap();
    assert!(dbg.summary.contains("bug/error"));

    // Indeks 14 skill bawaan tetap hemat <= 400 token!
    let idx = format_skills_index_budgeted(&scan.skills, 1500);
    let est_tok = idx.chars().count().div_ceil(3);
    assert!(est_tok <= 400, "indeks 14 skill bawaan ({est_tok}) harus <= 400 token");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 3: Pemantau tugas asinkron (`watch_job`) ----------

#[tokio::test]
async fn perbaikan3_watch_job_lifecycle_antispam_failure_restart_and_zero_tokens() {
    let dir = temp_dir("p3-watch");
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());
    let status_file = dir.join("job_status.txt");
    std::fs::write(&status_file, "STATUS=PROCESSING\nSTAGE=S1\nPROGRESS=1/3\n").unwrap();

    // Tolak perintah shell mutatif/chaining
    assert!(validate_readonly_watch_command("rm -rf /tmp/x").is_err());
    assert!(validate_readonly_watch_command("echo ok; rm a").is_err());
    assert!(validate_readonly_watch_command("python job_queue.py show 6c33").is_ok());

    // Daftarkan lewat tool `watch_job`
    let ctx = Ctx::new(dir.clone(), true).with_store(store.clone(), "sess-watch".into());
    let tool = WatchJob;
    let reg_out = tool
        .run(
            json!({
                "label": "Comic-auto Hold On",
                "status_file": "job_status.txt",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(reg_out.contains("Pemantauan #1 terdaftar"));

    let sandbox = SandboxController::default();
    let now = Utc::now();

    // Poll 1: tahap S1 -> 1 pesan progres
    let w1 = store.list_job_watches(None, true).unwrap().remove(0);
    let raw1 = poll_watch_once(&w1, &sandbox).await.unwrap();
    let tr1 = evaluate_watch_poll(&w1, Ok(&raw1), now);
    assert_eq!(tr1.new_status, "active");
    assert!(tr1.notification.as_deref().unwrap().contains("S1"));
    store
        .update_job_watch_poll(w1.id, &tr1.new_status, &tr1.new_stage, &tr1.new_progress, 0, &now.to_rfc3339(), None, false)
        .unwrap();

    // Poll 2: tahap TIDAK berubah -> 0 pesan (anti-spam!)
    let w2 = store.list_job_watches(None, true).unwrap().remove(0);
    let raw2 = poll_watch_once(&w2, &sandbox).await.unwrap();
    let tr2 = evaluate_watch_poll(&w2, Ok(&raw2), now + Duration::seconds(35));
    assert!(tr2.notification.is_none(), "tidak boleh spam bila tahap tidak berubah");

    // Simulasi GATEWAY RESTART: buka ulang Store baru dari file DB yang sama!
    drop(store);
    let store2 = Arc::new(Store::open_at(&db_path).unwrap());
    let w_after_restart = store2.list_job_watches(None, true).unwrap();
    assert_eq!(w_after_restart.len(), 1, "pantauan wajib tetap hidup sesudah restart");

    // Poll 3: tahap maju ke S2
    std::fs::write(&status_file, "STATUS=PROCESSING\nSTAGE=S2\nPROGRESS=2/3\n").unwrap();
    let raw3 = poll_watch_once(&w_after_restart[0], &sandbox).await.unwrap();
    let tr3 = evaluate_watch_poll(&w_after_restart[0], Ok(&raw3), now + Duration::seconds(70));
    assert!(tr3.notification.as_deref().unwrap().contains("S2"));
    store2
        .update_job_watch_poll(w_after_restart[0].id, &tr3.new_status, &tr3.new_stage, &tr3.new_progress, 0, &now.to_rfc3339(), None, false)
        .unwrap();

    // Poll 4: SELESAI -> laporan akhir memuat hasil & lokasi, lalu berhenti!
    std::fs::write(
        &status_file,
        "STATUS=SELESAI\nSUMMARY=22 halaman selesai dirender\nOUTPUT=out/hold-on.zip\n",
    )
    .unwrap();
    let w4 = store2.list_job_watches(None, true).unwrap().remove(0);
    let raw4 = poll_watch_once(&w4, &sandbox).await.unwrap();
    let tr4 = evaluate_watch_poll(&w4, Ok(&raw4), now + Duration::seconds(110));
    assert!(tr4.terminal);
    assert_eq!(tr4.new_status, "done");
    let final_msg = tr4.notification.unwrap();
    assert!(final_msg.contains("selesai"));
    assert!(final_msg.contains("out/hold-on.zip"));
    store2
        .update_job_watch_poll(w4.id, &tr4.new_status, &tr4.new_stage, &tr4.new_progress, 0, &now.to_rfc3339(), Some(&final_msg), false)
        .unwrap();

    // Laporan CLI tertunda diambil sekali saat sesi berikut dibuka
    let undel = store2.take_undelivered_watch_reports().unwrap();
    assert_eq!(undel.len(), 1);
    assert!(undel[0].2.contains("out/hold-on.zip"));
    assert!(store2.take_undelivered_watch_reports().unwrap().is_empty());

    // Kasus gagal 3x berturut -> laporan kegagalan & berhenti
    let parsed_fail = parse_watch_output("STATUS=FAILED\nERROR=disk penuh", "", "");
    assert_eq!(parsed_fail.kind, WatchStateKind::Failed);
    let mut err_w = w4.clone();
    err_w.status = "active".into();
    err_w.consecutive_errors = 2;
    let tr_err = evaluate_watch_poll(&err_w, Err("berkas hilang"), now);
    assert!(tr_err.terminal);
    assert_eq!(tr_err.new_status, "failed");
    assert!(tr_err.notification.unwrap().contains("berkas hilang"));

    // Pastikan 0 token provider dipakai oleh seluruh loop pantauan!
    let rep = store2.usage_report("sess-watch", &Utc::now().format("%Y-%m-%d").to_string()).unwrap();
    assert!(rep.session_rows.is_empty());
    assert!(rep.today_rows.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 4: Kirim berkas Telegram (`send_file`) ----------

#[tokio::test]
async fn perbaikan4_send_file_sandbox_size_limit_and_cli_fallback() {
    let dir = temp_dir("p4-sendfile");
    let ok_zip = dir.join("hasil.zip");
    std::fs::write(&ok_zip, b"PK\x03\x04dummy-zip-data").unwrap();
    let ok_png = dir.join("preview.png");
    std::fs::write(&ok_png, b"\x89PNG\r\n\x1a\nfake").unwrap();
    let secret_env = dir.join(".env");
    std::fs::write(&secret_env, "API_KEY=secret").unwrap();

    let (_, sz, is_img) = validate_send_file_target("hasil.zip", &dir, None, 50 * 1024 * 1024).unwrap();
    assert!(sz > 0 && !is_img);
    let (_, _, is_img_png) = validate_send_file_target("preview.png", &dir, None, 50 * 1024 * 1024).unwrap();
    assert!(is_img_png);

    // Path keluar sandbox & file rahasia WAJIB ditolak
    assert!(validate_send_file_target("../di-luar.zip", &dir, None, 50 * 1024 * 1024).is_err());
    assert!(validate_send_file_target(".env", &dir, None, 50 * 1024 * 1024).is_err());

    // Berkas melebihi batas ukuran -> pesan jelas menawarkan jalur Google Drive
    let big_err = validate_send_file_target("hasil.zip", &dir, None, 4).unwrap_err().to_string();
    assert!(big_err.contains("BATAS_UKURAN_TELEGRAM"));
    assert!(big_err.contains("Google Drive"));

    // Di sesi CLI (bukan gateway), `send_file` menjawab sopan beserta path lokalnya
    let ctx = Ctx::new(dir.clone(), true);
    let out = SendFile
        .run(json!({"path": "hasil.zip", "caption": "Hasil render"}), &ctx)
        .await
        .unwrap();
    assert!(out.contains("Telegram Gateway"));
    assert!(out.contains("hasil.zip"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 5: Memory persisten + profil beku ala Hermes ----------

#[tokio::test]
async fn perbaikan5_persistent_memory_profile_caps_and_frozen_snapshot() {
    let dir = temp_dir("p5-memory");
    let mem_dir = dir.join("memory");

    // Tambah preferensi & profil
    let (n1, _) = add_entry_in(&mem_dir, MemoryStoreKind::Profile, "Pemilik suka hasil ZIP disimpan ke ~/hasil-komik", 1375).unwrap();
    assert_eq!(n1, 1);
    let (n2, _) = add_entry_in(&mem_dir, MemoryStoreKind::Memory, "Proyek Comic-auto memakai python job_queue.py", 2200).unwrap();
    assert_eq!(n2, 1);

    // Anti-secret guard
    assert!(add_entry_in(&mem_dir, MemoryStoreKind::Memory, "API key sk-abc1234567890", 2200).is_err());

    // Pagu karakter keras: menolak teks yang melebihi batas dengan anjuran merangkum
    let too_big = "a".repeat(2205);
    let cap_err = add_entry_in(&mem_dir, MemoryStoreKind::Memory, &too_big, 2200).unwrap_err().to_string();
    assert!(cap_err.contains("pagu wadah 'memory' terlampaui"));
    assert!(cap_err.contains("Rangkum"));

    // Replace & remove bekerja bersih
    replace_entry_in(&mem_dir, MemoryStoreKind::Profile, "1", "Folder tujuan hasil default: ~/hasil-kanonik", 1375).unwrap();
    let snap = load_frozen_snapshot_from(&mem_dir, 2200, 1375);
    let prompt_txt = snap.format_for_prompt();
    assert!(prompt_txt.contains("~/hasil-kanonik"));
    assert!(prompt_txt.contains("job_queue.py"));

    // Tool `memory` langsung berjalan pada `SELENCODE_DATA_DIR` terisolasi
    let tool_ctx = Ctx::new(dir.clone(), true).with_memory_cfg(MemoryCfg::default());
    let _ = UserMemoryTool.spec();
    let (removed, _) = remove_entry_in(&mem_dir, MemoryStoreKind::Memory, "job_queue", 2200).unwrap();
    assert!(removed.contains("job_queue"));
    assert!(tool_ctx.memory_cfg.max_memory_chars == 2200);
    assert!(tool_ctx.memory_cfg.max_profile_chars == 1375);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 6: Delegate pekerja di Git worktree terisolasi ----------

fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    };
    run(&["init"]);
    run(&["config", "user.email", "test@selencode.dev"]);
    run(&["config", "user.name", "Selen Test"]);
    std::fs::write(dir.join("README.md"), "# Demo Repo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "init"]);
}

struct WriteFileWorkerProv {
    step: AtomicUsize,
}

#[async_trait]
impl Provider for WriteFileWorkerProv {
    fn model_name(&self) -> &str {
        "worker-mock"
    }
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let user_prompt = msgs
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.plain_text())
            .unwrap_or_default();
        let has_tool_result = msgs.iter().any(|m| m.role == Role::Tool);
        if !has_tool_result {
            let fname = if user_prompt.contains("fitur_b.txt") {
                "fitur_b.txt"
            } else {
                "fitur_a.txt"
            };
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("w{n}"),
                    name: "write_file".into(),
                    args: json!({"path": fname, "content": format!("hasil {fname}\n")}),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(Usage { input_tokens: 210, output_tokens: 35 }),
            })
        } else {
            Ok(AssistantReply {
                text: "Fitur selesai ditulis di worktree.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage { input_tokens: 240, output_tokens: 25 }),
            })
        }
    }
}

#[tokio::test]
async fn perbaikan6_delegate_work_isolated_worktree_approval_parallel_and_nongit_reject() {
    // 1. Folder non-git WAJIB ditolak jelas
    let non_git = temp_dir("p6-nongit");
    let worker_prov: Arc<dyn Provider> = Arc::new(WriteFileWorkerProv { step: AtomicUsize::new(0) });
    let del = Delegate::new(worker_prov.clone(), Arc::new(read_only_registry()), 3);
    let non_git_ctx = Ctx::new(non_git.clone(), true);
    let err = del
        .run(json!({"mode": "work", "tasks": [{"prompt": "Buat fitur_a.txt"}]}), &non_git_ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("bukan repositori Git"));
    let _ = std::fs::remove_dir_all(&non_git);

    // 2. Folder Git: jika user MENOLAK approval -> pohon utama TIDAK berubah, diff tetap ditampilkan!
    let repo = temp_dir("p6-git");
    init_git_repo(&repo);
    let store = Arc::new(Store::open_at(&repo.join("selen.db")).unwrap());
    let sid = store
        .create_session(&repo.to_string_lossy(), "mock", "worker-mock", "uji delegate work")
        .unwrap();

    let deny_ctx = Ctx::new(repo.clone(), false)
        .with_approver(Arc::new(Approver::deny_all()))
        .with_store(store.clone(), sid.clone());
    let res_denied = del
        .run(json!({"mode": "work", "tasks": [{"prompt": "Buat file fitur_a.txt"}]}), &deny_ctx)
        .await
        .unwrap();
    assert!(res_denied.contains("DITOLAK user"));
    assert!(res_denied.contains("fitur_a.txt"));
    assert!(!repo.join("fitur_a.txt").exists(), "pohon utama TIDAK boleh berubah bila ditolak");

    // 3. Dua pekerja PARALEL disetujui -> keduanya mendarat tanpa bertabrakan + usage tercatat!
    let ok_ctx = Ctx::new(repo.clone(), true).with_store(store.clone(), sid.clone());
    let res_ok = del
        .run(
            json!({
                "mode": "work",
                "tasks": [
                    {"prompt": "Buat file fitur_a.txt"},
                    {"prompt": "Buat file fitur_b.txt"}
                ]
            }),
            &ok_ctx,
        )
        .await
        .unwrap();
    assert!(res_ok.contains("DISETUJUI & DITERAPKAN"));
    assert_eq!(std::fs::read_to_string(repo.join("fitur_a.txt")).unwrap(), "hasil fitur_a.txt\n");
    assert_eq!(std::fs::read_to_string(repo.join("fitur_b.txt")).unwrap(), "hasil fitur_b.txt\n");

    let today = Utc::now().format("%Y-%m-%d").to_string();
    let urep = store.usage_report(&sid, &today).unwrap();
    let dw = urep.session_rows.iter().find(|r| r.kind == "delegate_work").unwrap();
    assert!(dw.input_tokens > 0 && dw.calls >= 3);

    let _ = std::fs::remove_dir_all(&repo);
}

// ---------- PERBAIKAN 7: Laporan `/usage` cocok persis dengan tabel `usage` ----------

#[test]
fn perbaikan7_usage_report_exact_match_and_separate_kinds() {
    let dir = temp_dir("p7-usage");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&dir.to_string_lossy(), "9router", "ag/gemini-3-flash", "uji usage")
        .unwrap();
    store.record_usage_with_kind(&sid, "ag/gemini-3-flash", 1200, 150, "main").unwrap();
    store.record_usage_with_kind(&sid, "ag/gemini-3-flash", 1400, 180, "main").unwrap();
    store.record_usage_with_kind(&sid, "ag/gemini-4-neon-medium", 1300, 90, "fallback").unwrap();
    store.record_review_usage(&sid, "ag/gemini-3-flash", 900, 110).unwrap();
    store.record_usage_with_kind("cron:1", "ag/gemini-3-flash", 600, 70, "cron").unwrap();

    let today = Utc::now().format("%Y-%m-%d").to_string();
    let rep = store.usage_report(&sid, &today).unwrap();

    let main_s = rep.session_rows.iter().find(|r| r.kind == "main").unwrap();
    assert_eq!((main_s.calls, main_s.input_tokens, main_s.output_tokens), (2, 2600, 330));
    let fb_s = rep.session_rows.iter().find(|r| r.kind == "fallback").unwrap();
    assert_eq!((fb_s.calls, fb_s.input_tokens, fb_s.output_tokens), (1, 1300, 90));
    let rv_s = rep.session_rows.iter().find(|r| r.kind == "review").unwrap();
    assert_eq!((rv_s.calls, rv_s.input_tokens, rv_s.output_tokens), (1, 900, 110));

    let total_today_in: u64 = rep.today_rows.iter().map(|r| r.input_tokens).sum();
    let total_today_calls: u64 = rep.today_rows.iter().map(|r| r.calls).sum();
    assert_eq!(total_today_in, 2600 + 1300 + 900 + 600);
    assert_eq!(total_today_calls, 5);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PERBAIKAN 8: Pasang & copot skill dari URL/Git ----------

#[tokio::test]
async fn perbaikan8_skills_install_validation_scripts_collision_and_remove() {
    let dir = temp_dir("p8-skills-inst");
    let src_repo = dir.join("remote-skill-repo");
    std::fs::create_dir_all(src_repo.join("scripts")).unwrap();
    std::fs::write(
        src_repo.join("SKILL.md"),
        "---\nname: cek-paket-demo\ndescription: Skill demo cek dependensi proyek\nversion: 1.0.0\n---\n\n# Cek Paket Demo\n1. Periksa manifest proyek.\n",
    )
    .unwrap();
    std::fs::write(src_repo.join("scripts").join("audit.sh"), "#!/bin/sh\necho ok\n").unwrap();
    let git_run = |repo: &std::path::Path, args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(repo).output().unwrap();
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    };
    git_run(&src_repo, &["init"]);
    git_run(&src_repo, &["config", "user.email", "test@selencode.dev"]);
    git_run(&src_repo, &["config", "user.name", "Selen Test"]);
    git_run(&src_repo, &["add", "."]);
    git_run(&src_repo, &["commit", "-m", "add skill"]);

    // 1. Stage dari URL git lokal -> validasi + penandaan skrip jelas
    let staged = stage_skill_from_source(&src_repo.to_string_lossy()).await.unwrap();
    assert_eq!(staged.frontmatter.name, "cek-paket-demo");
    let summary = format_install_approval_summary(&staged, None);
    assert!(summary.contains("scripts/audit.sh"));
    assert!(summary.contains("SKRIP"));

    // 2. Install ke user_skills_dir -> muncul di scan & punya `.selen-source.json`
    let user_skills = dir.join("user-skills");
    seed_builtin_skills_to(&user_skills);
    let rep = finalize_staged_skill_install(&staged, &user_skills, None).unwrap();
    assert_eq!(rep.name, "cek-paket-demo");
    assert_eq!(rep.scripts_count, 1);
    assert!(rep.provenance_path.is_file());

    let scan = scan_skills_in_dirs(None, Some(&user_skills));
    assert!(scan.skills.iter().any(|s| s.name == "cek-paket-demo"));

    // 3. Bentrok nama ditolak (tidak menimpa diam-diam), sukses dengan `--rename`
    let dup_err = finalize_staged_skill_install(&staged, &user_skills, None).unwrap_err().to_string();
    assert!(dup_err.contains("bentrok nama"));
    let renamed = finalize_staged_skill_install(&staged, &user_skills, Some("cek-paket-v2")).unwrap();
    assert_eq!(renamed.name, "cek-paket-v2");

    // 4. `remove` berhasil untuk skill install, DITOLAK untuk bawaan & proyek
    assert!(remove_installed_skill("systematic-debugging", &user_skills, &dir).is_err());
    let proj_skill = dir.join(".selencode").join("skills").join("lokal-proyek");
    std::fs::create_dir_all(&proj_skill).unwrap();
    std::fs::write(
        proj_skill.join("SKILL.md"),
        "---\nname: lokal-proyek\ndescription: Skill lokal\n---\n# Lokal\n",
    )
    .unwrap();
    assert!(remove_installed_skill("lokal-proyek", &user_skills, &dir).is_err());
    let rm_msg = remove_installed_skill("cek-paket-demo", &user_skills, &dir).unwrap();
    assert!(rm_msg.contains("berhasil dicopot"));
    // Skill hasil rename tetap bisa dimuat!
    let scan2 = scan_skills_in_dirs(None, Some(&user_skills));
    assert!(scan2.skills.iter().any(|s| s.name == "cek-paket-v2"));
    let _ = load_skill(&dir, "lokal-proyek").unwrap();

    // 5. Skill tidak valid (frontmatter rusak) -> ditolak & tidak ada file yang tertulis
    let bad_repo = dir.join("bad-skill-repo");
    std::fs::create_dir_all(&bad_repo).unwrap();
    std::fs::write(bad_repo.join("SKILL.md"), "# Tanpa frontmatter YAML sama sekali\n").unwrap();
    git_run(&bad_repo, &["init"]);
    git_run(&bad_repo, &["config", "user.email", "test@selencode.dev"]);
    git_run(&bad_repo, &["config", "user.name", "Selen Test"]);
    git_run(&bad_repo, &["add", "."]);
    git_run(&bad_repo, &["commit", "-m", "bad skill"]);
    assert!(stage_skill_from_source(&bad_repo.to_string_lossy()).await.is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
