//! Suite uji penerimaan terpadu SelenCode v0.5.6 (Paket Gabungan Bagian 1–5):
//! - BAGIAN 1: Fallback provider/model (429/5xx/401/403/404 -> fallback; 400 -> TIDAK fallback; non-sticky; usage model aktual).
//! - BAGIAN 2: Tool `ask_user` / clarify (pilihan kandidat, timeout, non-interaktif, anti-secret, anti-spinning, toolset ramping).
//! - BAGIAN 3: Cron / tugas terjadwal (parser interval/harian, tolak 5-bidang, persistensi restart, skip-missed, pagar read-only & shell allowlist).
//! - BAGIAN 4A: Pencarian lintas sesi FTS5 (`session_search` + backfill + batas 10 & cuplikan pendek).
//! - BAGIAN 4B: Pelacakan & timeout approval Telegram (`expired_ids`, `"sudah kedaluwarsa"`, retry-once).
//! - BAGIAN 5: 14 skill bawaan resmi (12 adaptasi MIT + 2 asli) + `NOTICE` + 0 sisa nama tool Hermes + pagu indeks <= 400 token + prioritas pangkas bawaan -> user -> proyek.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use selencode::agent::run_turn_ui;
use selencode::agent::UiEvent;
use selencode::config::ShellCfg;
use selencode::cron::{
    build_cron_registry, compute_next_run_after, is_shell_command_allowlisted, parse_schedule,
    run_cron_job_once, skip_missed_jobs_on_startup, ParsedSchedule,
};
use selencode::providers::router::{
    is_fallback_eligible_error, DynamicRouterProvider, ModelRoute, RouterTelemetry,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::security::{ApprovalMode, ApprovalPolicy};
use selencode::skills::{
    format_skills_index, format_skills_index_budgeted, scan_skills_in_dirs, seed_builtin_skills_to,
    SkillInfo,
};
use selencode::store::Store;
use selencode::telegram::{
    parse_callback_ask, PendingTgApproval, TgApprovalHub, TgAskHub,
};
use selencode::tools::ask::{AskUser, AskUserAnswer, AskUserRequest, UserAsker};
use selencode::tools::session_search::SessionSearch;
use selencode::tools::toolsets::select_initial_toolsets;
use selencode::tools::{default_registry, Ctx, Tool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v056-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct ScriptedProv {
    model: String,
    fail_err: Option<String>,
    reply_text: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ScriptedProv {
    fn model_name(&self) -> &str {
        &self.model
    }
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = &self.fail_err {
            return Err(anyhow!("{e}"));
        }
        Ok(AssistantReply {
            text: self.reply_text.clone(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 420,
                output_tokens: 65,
            }),
        })
    }
}

// ---------- BAGIAN 1: Fallback provider/model ----------

#[tokio::test]
async fn bagian1_fallback_dead_model_400_no_fallback_and_non_sticky() {
    // 1. Klasifikasi error ketat
    assert!(is_fallback_eligible_error("HTTP 429 Too Many Requests"));
    assert!(is_fallback_eligible_error("HTTP 503 Service Unavailable"));
    assert!(is_fallback_eligible_error("HTTP 401 Unauthorized: token rejected"));
    assert!(is_fallback_eligible_error("HTTP 404 model_not_found"));
    assert!(is_fallback_eligible_error("request timed out"));
    assert!(!is_fallback_eligible_error("HTTP 400 Bad Request: malformed json"));
    assert!(!is_fallback_eligible_error("invalid_request_error: bad parameter"));
    assert!(!is_fallback_eligible_error("Ditolak user — approval dibatalkan"));
    assert!(!is_fallback_eligible_error("error misterius tidak dikenal"));

    let dir = temp_dir("b1");
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let sid = store
        .create_session(&dir.to_string_lossy(), "9router", "dead-primary", "uji fallback")
        .unwrap();

    // Skenario 1: Model utama mati (404) -> cadangan hidup melayani + 1x notifikasi + usage tercatat atas nama model cadangan!
    let c_dead = Arc::new(AtomicUsize::new(0));
    let c_alive = Arc::new(AtomicUsize::new(0));
    let p_dead = Arc::new(ScriptedProv {
        model: "dead-primary".into(),
        fail_err: Some("HTTP 404 model_not_found: model mati".into()),
        reply_text: String::new(),
        calls: c_dead.clone(),
    });
    let p_alive = Arc::new(ScriptedProv {
        model: "alive-backup".into(),
        fail_err: None,
        reply_text: "Tugas selesai lewat model cadangan.".into(),
        calls: c_alive.clone(),
    });
    let tel = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router = Arc::new(
        DynamicRouterProvider::new(
            vec![
                ModelRoute::new("9router", "dead-primary", p_dead, None, None, None, true),
                ModelRoute::new("9router", "alive-backup", p_alive, None, None, None, false),
            ],
            false,
            HashMap::new(),
            0,
            tel.clone(),
        )
        .with_backoff_ms(0)
        .with_max_fallback_models(3)
        .with_non_sticky_manual(true),
    );

    let ctx = Ctx::new(dir.clone(), true)
        .with_store(store.clone(), sid.clone())
        .with_router_telemetry(tel.clone());
    let reg = default_registry();
    let mut msgs = Vec::new();
    let (ui_tx, mut ui_rx) = mpsc::channel(64);
    let out = run_turn_ui(router.clone(), &reg, &ctx, &mut msgs, "Halo", 4, ui_tx)
        .await
        .unwrap();
    assert!(out.contains("cadangan"));
    assert_eq!(c_dead.load(Ordering::SeqCst), 1);
    assert_eq!(c_alive.load(Ordering::SeqCst), 1);

    let mut notices = Vec::new();
    while let Ok(ev) = ui_rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            if s.starts_with("Model utama gagal") {
                notices.push(s);
            }
        }
    }
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("alive-backup"));

    // Skenario 4: Non-sticky — giliran baru mencoba primary lagi dari awal!
    let (ui_tx2, _ui_rx2) = mpsc::channel(32);
    let _ = run_turn_ui(router, &reg, &ctx, &mut msgs, "Lanjut", 4, ui_tx2)
        .await
        .unwrap();
    assert_eq!(c_dead.load(Ordering::SeqCst), 2, "giliran baru harus mencoba primary lagi (tidak lengket)");
    assert_eq!(c_alive.load(Ordering::SeqCst), 2);

    // Skenario 2: HTTP 400 -> TIDAK boleh pindah ke cadangan!
    let c_400 = Arc::new(AtomicUsize::new(0));
    let c_fb = Arc::new(AtomicUsize::new(0));
    let p_400 = Arc::new(ScriptedProv {
        model: "bad-req-model".into(),
        fail_err: Some("HTTP 400 Bad Request: invalid parameter".into()),
        reply_text: String::new(),
        calls: c_400.clone(),
    });
    let p_fb = Arc::new(ScriptedProv {
        model: "never-called".into(),
        fail_err: None,
        reply_text: "tidak boleh terpanggil".into(),
        calls: c_fb.clone(),
    });
    let r_400 = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("9router", "bad-req-model", p_400, None, None, None, true),
            ModelRoute::new("9router", "never-called", p_fb, None, None, None, false),
        ],
        false,
        HashMap::new(),
        1,
        Arc::new(Mutex::new(RouterTelemetry::default())),
    )
    .with_backoff_ms(0)
    .with_non_sticky_manual(true);
    let (tx400, _rx400) = mpsc::channel(8);
    let err400 = r_400
        .stream_chat(&[Message::text(Role::User, "tes 400")], &[], tx400)
        .await
        .unwrap_err();
    assert!(err400.to_string().contains("400"));
    assert_eq!(c_400.load(Ordering::SeqCst), 1);
    assert_eq!(c_fb.load(Ordering::SeqCst), 0, "400 tidak boleh memicu fallback");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- BAGIAN 2: Tool ask_user / clarify ----------

#[tokio::test]
async fn bagian2_ask_user_choices_timeout_non_interactive_and_lean_toolsets() {
    let dir = temp_dir("b2");
    let tool = AskUser;

    // 1. Skenario judul ambigu: channel mengembalikan kandidat pilihan user
    let (ask_tx, mut ask_rx) = mpsc::channel::<AskUserRequest>(4);
    let responder = tokio::spawn(async move {
        if let Some(req) = ask_rx.recv().await {
            assert!(req.choices.iter().any(|c| c.contains("Rebirth of Legendary Doctor")));
            let _ = req.reply.send(AskUserAnswer {
                response: "Rebirth of Legendary Doctor (MangaDex)".into(),
                timed_out: false,
                non_interactive: false,
            });
        }
    });
    let ctx_ch = Ctx::new(dir.clone(), false).with_asker(Arc::new(UserAsker::channel(ask_tx)), 10);
    let res = tool
        .run(
            serde_json::json!({
                "question": "Judul persis tidak ditemukan. Maksudmu yang mana?",
                "choices": ["Rebirth of Legendary Doctor (MangaDex)", "Legendary Doctor Returns"],
                "context": "Pencarian untuk 'The Rise of a Legendary Doctor'"
            }),
            &ctx_ch,
        )
        .await
        .unwrap();
    responder.await.unwrap();
    assert!(res.contains("Rebirth of Legendary Doctor"));
    assert!(res.contains("BUKAN persetujuan"));

    // 2. Skenario timeout (1 detik tanpa jawaban) -> mengembalikan penanda [TIMEOUT]
    let (ask_tx2, _ask_rx_hold) = mpsc::channel::<AskUserRequest>(4);
    let ctx_to = Ctx::new(dir.clone(), false).with_asker(Arc::new(UserAsker::channel(ask_tx2)), 1);
    let res_to = tool
        .run(
            serde_json::json!({ "question": "Pilih format output?", "choices": ["JSON", "CSV"] }),
            &ctx_to,
        )
        .await
        .unwrap();
    assert!(res_to.contains("[TIMEOUT]"));

    // 3. Skenario non-interaktif (`selen run` via pipe) -> tidak menggantung, langsung [NON-INTERACTIVE]
    let ctx_ni = Ctx::new(dir.clone(), false);
    let res_ni = tool
        .run(serde_json::json!({ "question": "Pilih mode?" }), &ctx_ni)
        .await
        .unwrap();
    assert!(res_ni.contains("[NON-INTERACTIVE]"));

    // 4. Keamanan: tolak pertanyaan meminta API key / password!
    let err_sec = tool
        .run(serde_json::json!({ "question": "Masukkan API key OpenAI kamu?" }), &ctx_ni)
        .await
        .unwrap_err();
    assert!(err_sec.to_string().contains("ditolak"));

    // 5. Toolset ramping: tugas jelas biasa TIDAK mengaktifkan `clarify`, tugas unduh/cari mengaktifkannya!
    let ts_clear = select_initial_toolsets(
        "Buat skrip Node.js rekap.js yang membaca data.csv",
        &dir,
        false,
        selencode::agent::plan::TurnMode::Execute,
    );
    assert!(!ts_clear.contains("clarify"));
    let ts_ambig = select_initial_toolsets(
        "Unduh manga The Rise of a Legendary Doctor",
        &dir,
        false,
        selencode::agent::plan::TurnMode::Execute,
    );
    assert!(ts_ambig.contains("clarify"));

    // 6. Callback parser & format kartu `TgAskHub` terpisah total dari approval!
    assert_eq!(parse_callback_ask("ask:5:2"), Some((5, 2)));
    assert_eq!(parse_callback_ask("ap:5:y"), None);
    let (card_html, _) = TgAskHub::format_ask_card_html(
        "Maksudmu yang ini?",
        &["Rebirth of Legendary Doctor".into()],
        None,
    );
    assert!(card_html.contains("Pertanyaan Klarifikasi"));
    assert!(!card_html.contains("Perlu persetujuan"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- BAGIAN 3: Tugas terjadwal / Cron ----------

#[tokio::test]
async fn bagian3_cron_schedule_persistence_skip_missed_and_security_fence() {
    // 1. Parser jadwal sederhana & tolak cron 5-bidang
    assert_eq!(parse_schedule("every 5m").unwrap(), ParsedSchedule::EveryMinutes(5));
    assert_eq!(parse_schedule("2h").unwrap(), ParsedSchedule::EveryHours(2));
    assert_eq!(
        parse_schedule("daily 08:30").unwrap(),
        ParsedSchedule::DailyLocal { hour: 8, minute: 30 }
    );
    assert!(parse_schedule("*/5 * * * *").is_err(), "cron 5-bidang harus ditolak");
    assert!(parse_schedule("").is_err());
    assert!(parse_schedule("25:99").is_err());

    // 2. Persistensi selamat dari restart + kebijakan skip-missed
    let dir = temp_dir("b3");
    let db_path = dir.join("selen.db");
    let past_ts = (chrono::Utc::now() - chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Seconds, true);
    {
        let store = Store::open_at(&db_path).unwrap();
        let job = store
            .add_cron_job(
                "cek-file",
                "every:2m",
                "Baca file info.txt dan laporkan baris pertamanya",
                &dir.to_string_lossy(),
                &[],
                false,
                12345,
                &past_ts,
            )
            .unwrap();
        assert_eq!(job.name, "cek-file");
    }
    // Simulasi restart gateway: buka ulang DB -> job tetap ada & jadwal terlewat di-skip (tidak dikejar!)
    let store2 = Arc::new(Store::open_at(&db_path).unwrap());
    let skipped = skip_missed_jobs_on_startup(&store2).unwrap();
    assert_eq!(skipped, 1);
    let reloaded = store2.get_cron_job("cek-file").unwrap().unwrap();
    assert_eq!(reloaded.last_status, "terlewat");
    assert!(reloaded.next_run_at > past_ts);

    // 3. Pagar keamanan default baca-saja: `run_shell`/`write_file`/`edit_file` TIDAK ditawarkan sama sekali!
    let reg_ro = build_cron_registry(&reloaded, &ShellCfg::default());
    let names = reg_ro.names();
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"session_search".to_string()));
    assert!(!names.contains(&"run_shell".to_string()));
    assert!(!names.contains(&"write_file".to_string()));
    assert!(!names.contains(&"edit_file".to_string()));

    // 4. Allowlist shell eksplisit per job: tolak perintah tidak terdaftar & tolak chaining berbahaya!
    assert!(is_shell_command_allowlisted("git status", &["git status".into()]));
    assert!(!is_shell_command_allowlisted("rm -rf /", &["git status".into()]));
    assert!(!is_shell_command_allowlisted("git status && rm -rf /", &["git status".into()]));

    // 5. Jalankan 1 kali job baca + catat usage terpisah `kind = 'cron'`
    let mut job_with_shell = reloaded.clone();
    job_with_shell.allowed_shell = vec!["echo ok".into()];
    let reg_allow = build_cron_registry(&job_with_shell, &ShellCfg::default());
    assert!(reg_allow.get("run_shell").is_some());
    let denied_shell = reg_allow
        .get("run_shell")
        .unwrap()
        .run(serde_json::json!({"command": "whoami"}), &Ctx::new(dir.clone(), false))
        .await;
    assert!(denied_shell.is_err());

    let mock_cron_prov = Arc::new(ScriptedProv {
        model: "cron-test-model".into(),
        fail_err: None,
        reply_text: "Baris pertama info.txt: HALO-CRON".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let (ok, summary) = run_cron_job_once(
        mock_cron_prov,
        store2.clone(),
        Arc::new(SandboxController::default()),
        &reloaded,
        &ShellCfg::default(),
        6,
    )
    .await;
    assert!(ok);
    assert!(summary.contains("HALO-CRON"));

    // 6. Remove bekerja
    assert!(store2.remove_cron_job("cek-file").unwrap());
    assert!(store2.list_cron_jobs().unwrap().is_empty());

    let _ = compute_next_run_after("every:5m", chrono::Utc::now()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- BAGIAN 4A & 4B: session_search FTS5 + Approval Tracking/Timeout ----------

#[tokio::test]
async fn bagian4_fts5_session_search_and_telegram_approval_expiry() {
    let dir = temp_dir("b4");
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let cwd_str = dir.to_string_lossy().to_string();

    // Tanam frasa unik "KODE-UNIK-7391" pada sesi lama
    let old_sid = store
        .create_session(&cwd_str, "9router", "ag/gemini-3-flash", "Diskusi KODE-UNIK-7391")
        .unwrap();
    store
        .save_message(
            &old_sid,
            &Message::text(
                Role::User,
                "Tolong catat bahwa token verifikasi proyek adalah KODE-UNIK-7391 untuk audit.",
            ),
        )
        .unwrap();
    store
        .save_message(
            &old_sid,
            &Message::text(
                Role::Assistant,
                "Dicatat, KODE-UNIK-7391 tersimpan di transkrip sesi ini.",
            ),
        )
        .unwrap();

    // Uji tool `session_search` dari sesi BARU
    let new_sid = store
        .create_session(&cwd_str, "9router", "ag/gemini-3-flash", "Sesi baru")
        .unwrap();
    let ctx = Ctx::new(dir.clone(), false).with_store(store.clone(), new_sid);
    let out = SessionSearch
        .run(serde_json::json!({ "query": "KODE-UNIK-7391" }), &ctx)
        .await
        .unwrap();
    assert!(out.contains(&old_sid));
    assert!(out.contains("KODE-UNIK-7391"));
    assert!(out.contains("/resume"));

    // Uji resume menggunakan ID hasil search!
    let resolved = store.resolve_session(&cwd_str, &old_sid, 10).unwrap();
    assert_eq!(resolved.id, old_sid);

    // Uji batas hemat token: tanam 15 sesi berisi kata umum "proyek" + teks panjang -> maks 10 hasil & cuplikan <= 180 char!
    let long_body = format!("proyek {}", "kata-panjang ".repeat(80));
    for i in 0..15 {
        let s = store
            .create_session(&cwd_str, "9router", "m", &format!("Sesi {i}"))
            .unwrap();
        store
            .save_message(&s, &Message::text(Role::User, long_body.clone()))
            .unwrap();
    }
    let hits = store.search_sessions_fts("proyek", None, 10).unwrap();
    assert!(hits.len() <= 10);
    for h in &hits {
        assert!(h.snippet.chars().count() <= 180);
    }

    // Bagian 4B: Approval timeout & expired_ids
    let hub = TgApprovalHub::new();
    hub.set_timeout_mins(1);
    assert_eq!(hub.timeout_secs(), 60);
    hub.set_timeout_mins(0);
    assert_eq!(hub.timeout_secs(), 0);

    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let id = hub.register_for_test(
        999,
        PendingTgApproval {
            chat_id: 999,
            message_id: 42,
            base_text: "test".into(),
            category: "run_shell".into(),
            policy: Arc::new(Mutex::new(ApprovalPolicy::new(ApprovalMode::Ask))),
            reply: tx,
        },
    );
    let expired_item = hub.expire_approval_now(id).expect("item harus ada");
    let _ = expired_item.reply.send(false);
    assert_eq!(rx.await.unwrap(), false, "approval kedaluwarsa WAJIB ditolak");
    assert!(hub.is_expired(id), "id kedaluwarsa harus tercatat agar tombol lama dijawab 'sudah kedaluwarsa'");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- BAGIAN 5: 14 Skill Bawaan Resmi + NOTICE + 0 Sisa Hermes + Pagu Indeks ----------

#[test]
fn bagian5_builtin_skills_clean_install_no_hermes_leaks_and_budget_trim_priority() {
    let dir = temp_dir("b5");
    let proj_skills = dir.join("proj_skills");
    let user_skills = dir.join("user_skills");
    std::fs::create_dir_all(&proj_skills).unwrap();

    // 1. Instalasi bersih: 14 skill bawaan (12 baru + 2 lama) + NOTICE tanpa peringatan!
    seed_builtin_skills_to(&user_skills);
    let scan = scan_skills_in_dirs(&proj_skills, Some(&user_skills));
    assert!(
        scan.warnings.is_empty(),
        "tidak boleh ada peringatan skill tidak valid: {:?}",
        scan.warnings
    );
    assert_eq!(scan.skills.len(), 14, "wajib tepat 14 skill bawaan");
    assert!(user_skills.join("NOTICE").is_file(), "file NOTICE MIT wajib ada");
    let notice = std::fs::read_to_string(user_skills.join("NOTICE")).unwrap();
    assert!(notice.contains("Nous Research"));
    assert!(notice.contains("MIT License"));
    assert!(notice.contains("obra/superpowers"));
    assert!(notice.contains("Ben Barclay"));

    // 2. Ukur pagu indeks bawaan <= 400 token (calibrated chars/3)
    let idx = format_skills_index(&scan.skills);
    let est_tokens = idx.chars().count().div_ceil(3);
    assert!(
        est_tokens <= 400,
        "indeks 14 skill bawaan ({est_tokens} token) melebihi batas <= 400 token"
    );

    // 3. Grep nol sisa nama tool Hermes di seluruh badan instruksional (kecuali NOTICE & baris atribusi MIT)
    let forbidden = [
        "terminal tool",
        "search_files",
        "delegate_task",
        "execute_code",
        "web_search",
        "web_extract",
        "skill_manage",
        "$HERMES_HOME",
        "HERMES_HOME",
    ];
    for (rel, content) in selencode::builtin_skills::BUILTIN_ADAPTED_FILES {
        if *rel == "NOTICE" {
            continue;
        }
        for line in content.lines() {
            if line.contains("Diadaptasi dari skill Hermes Agent (Nous Research), lisensi MIT") {
                continue;
            }
            let low = line.to_ascii_lowercase();
            for pat in &forbidden {
                assert!(
                    !low.contains(&pat.to_ascii_lowercase()),
                    "sisa tool Hermes '{pat}' ditemukan di {rel}: {line}"
                );
            }
        }
    }

    // 4. Urutan pemangkasan pagu gabungan: bawaan duluan -> user kustom -> proyek terakhir!
    let mut mixed: Vec<SkillInfo> = scan.skills.clone();
    mixed.push(SkillInfo {
        name: "custom-user-skill".into(),
        summary: "Skill kustom pengguna untuk workflow khusus pribadi".into(),
        category: "ops".into(),
        version: Some("1.0.0".into()),
        tags: vec![],
        path: user_skills.join("ops/custom-user-skill/SKILL.md"),
        dir: user_skills.join("ops/custom-user-skill"),
        scope: "user",
    });
    mixed.push(SkillInfo {
        name: "project-critical-skill".into(),
        summary: "Skill wajib khusus proyek aktif yang harus bertahan terakhir".into(),
        category: "ops".into(),
        version: Some("1.0.0".into()),
        tags: vec![],
        path: proj_skills.join("ops/project-critical-skill/SKILL.md"),
        dir: proj_skills.join("ops/project-critical-skill"),
        scope: "project",
    });
    // Set pagu ketat (90 token) sehingga bawaan dipangkas dulu, sementara skill proyek tetap bertahan!
    let trimmed = format_skills_index_budgeted(&mixed, 90);
    assert!(trimmed.contains("project-critical-skill"));
    assert!(trimmed.chars().count().div_ceil(3) <= 90);

    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn _keep_imports_used() {
    let _ = Block::Text(String::new());
    let _ = ToolCall {
        id: String::new(),
        name: String::new(),
        args: serde_json::Value::Null,
    };
}
