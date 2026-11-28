//! Tes integrasi Brief v0.5.27 — Perbaikan Utang:
//! 1. Masalah 1: `skill_edit` & jalur review menulis kembali ke berkas ASAL (tanpa salinan bayangan),
//!    keterlihatan bayangan di `/skills` & `skills_list`, salinan proyek eksplisit (`/skills override` /
//!    `copy_user_skill_to_project`), serta penanda `(salinan proyek)` pada laporan 💾 & cadangan review.
//! 2. Masalah 2: `review_runs` mencatat setiap panggilan provider secara individual (`call_index`,
//!    `event_calls`, `event_id`, `call_kind`) sehingga kepatuhan pagu `<= 5.000/panggilan` terbaca langsung.
//! 3. Masalah 3: env override khusus uji `SELENCODE_TELEGRAM_API_BASE` dihormati oleh seluruh jalur keluar
//!    `TelegramClient::new` termasuk polling `getUpdates`, `getMe`, `getFile`, `sendMessage`, dan
//!    `download_file_bytes`, sementara absennya env mempertahankan alamat resmi `https://api.telegram.org`.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::review::{run_background_review, MAX_REVIEW_CALL_INPUT_TOKENS};
use selencode::config::{Config, ProviderCfg};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::skills::{
    copy_user_skill_to_project, format_skills_index, format_skills_list_card,
    scan_skills_with_warnings, IMPROVEMENT_SKILLS_SUBDIR, IMPROVEMENT_SKILL_ORIGIN,
};
use selencode::store::Store;
use selencode::telegram::{
    classify_tg_message, resolve_telegram_api_urls, save_tg_document_bytes, TelegramClient,
    TgIngressClassification, DEFAULT_TELEGRAM_API_ORIGIN, TELEGRAM_API_BASE_ENV,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

fn env_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v0527-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_with_primary(model: &str) -> Config {
    let mut cfg = Config::empty();
    cfg.default_provider = "router".into();
    cfg.providers.insert(
        "router".into(),
        ProviderCfg {
            kind: "openai".into(),
            base_url: Some("https://example.invalid/v1".into()),
            model: model.into(),
            models: Vec::new(),
            api_key_env: "DUMMY".into(),
            api_key: None,
            price_in: None,
            price_out: None,
            context_limit: None,
            fallback: Vec::new(),
        },
    );
    cfg.review.enabled = true;
    cfg.review.cooldown_mins = 0;
    cfg.review.max_tokens = 5_000;
    cfg
}

struct SequenceMockProvider {
    model: String,
    responses: Mutex<Vec<(String, Usage)>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for SequenceMockProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let (text, usage) = {
            let guard = self.responses.lock().unwrap();
            guard
                .get(idx)
                .or_else(|| guard.last())
                .cloned()
                .unwrap_or_else(|| ("{\"actions\":[]}".into(), Usage::default()))
        };
        let _ = tx.send(StreamEvent::Token(text.clone())).await;
        Ok(AssistantReply {
            text,
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: Some(usage),
        })
    }
}

#[tokio::test]
async fn masalah1_skill_edit_and_review_write_back_to_user_origin_without_project_shadow() {
    let _guard = env_lock().lock().await;
    let data_root = temp_dir("m1-data");
    let proj_dir = temp_dir("m1-proj");
    let prev_data = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &data_root) };

    // Tanam skill improvement di store PENGGUNA dengan aturan keliru (`/tmp/staging`).
    let user_skill_dir = data_root
        .join("selencode")
        .join("skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("deploy-helper");
    std::fs::create_dir_all(&user_skill_dir).unwrap();
    let user_skill_md = user_skill_dir.join("SKILL.md");
    let bad_md = format!(
        "---\nname: deploy-helper\ndescription: Prosedur rilis artefak server\nversion: 1.0.0\norigin: {IMPROVEMENT_SKILL_ORIGIN}\nsource_session: sess-old\nlearned_at: 2026-11-27\n---\n\n\
         # Deploy Helper\n\n\
         1. Kompilasi rilis.\n\
         2. Kirim berkas ke `/tmp/staging` memakai rsync.\n\
         3. Mulai ulang layanan.\n"
    );
    std::fs::write(&user_skill_md, &bad_md).unwrap();

    // 1A. `skill_edit` jalur agent utama (dari dalam folder proyek!) HARUS mengubah berkas store PENGGUNA,
    //     mempertahankan `origin: selena-improvement`, dan TIDAK menciptakan salinan bayangan di proyek!
    let reg = default_registry();
    let edit_tool = reg.get("skill_edit").expect("skill_edit terdaftar");
    let ctx_yes = Ctx::new(proj_dir.clone(), true);
    let edit_out = edit_tool
        .run(
            json!({
                "name": "deploy-helper",
                "body": "# Deploy Helper\n\n1. Kompilasi rilis.\n2. Kirim berkas ke `/var/lib/releases` memakai rsync.\n3. Mulai ulang layanan."
            }),
            &ctx_yes,
        )
        .await
        .unwrap();
    assert!(edit_out.contains("berhasil diperbarui"), "{edit_out}");
    assert!(edit_out.contains("(user)"), "output harus menyebut cakupan asal user: {edit_out}");

    let after_agent_edit = std::fs::read_to_string(&user_skill_md).unwrap();
    assert!(
        !after_agent_edit.contains("/tmp/staging"),
        "teks keliru wajib hilang dari berkas store pengguna: {after_agent_edit}"
    );
    assert!(
        after_agent_edit.contains("/var/lib/releases"),
        "aturan benar wajib tertulis di berkas store pengguna: {after_agent_edit}"
    );
    assert!(
        after_agent_edit.contains(&format!("origin: {IMPROVEMENT_SKILL_ORIGIN}")),
        "metadata origin selena-improvement wajib terjaga: {after_agent_edit}"
    );
    assert!(
        !proj_dir.join(".selencode/skills").exists(),
        "DILARANG menciptakan folder/salinan bayangan di .selencode/skills/ proyek!"
    );

    // 1B. `skill_edit` dengan `ref_path` juga WAJIB menulis ke folder asal pengguna tanpa salinan proyek!
    let ref_out = edit_tool
        .run(
            json!({
                "name": "deploy-helper",
                "ref_path": "references/paths.md",
                "body": "Path kanonik: /var/lib/releases"
            }),
            &ctx_yes,
        )
        .await
        .unwrap();
    assert!(ref_out.contains("berhasil disimpan"), "{ref_out}");
    assert!(user_skill_dir.join("references/paths.md").is_file());
    assert!(
        !proj_dir.join(".selencode/skills").exists(),
        "edit ref_path juga tidak boleh membuat salinan proyek!"
    );

    // 1C. Skenario A5 penuh lewat jalur review: kembalikan aturan keliru di store pengguna,
    //     jalankan review dari sesi di dalam `proj_dir` -> berkas store PENGGUNA terperbaiki,
    //     NOL salinan bayangan di proyek, dan laporan 💾 tidak menyebut salinan proyek.
    std::fs::write(&user_skill_md, &bad_md).unwrap();
    let store = Store::open_at(&proj_dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&proj_dir.to_string_lossy(), "router", "primary-m", "uji A5")
        .unwrap();
    let cfg = config_with_primary("primary-m");
    let msgs = vec![
        Message::text(Role::User, "Bantu rilis memakai deploy-helper."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "sv1".into(),
                name: "skill_view".into(),
                args: json!({"name": "deploy-helper"}),
            }],
        },
        Message::text(
            Role::User,
            "Salah! Jangan pakai `/tmp/staging`, gunakan `/var/lib/releases` karena `/tmp` terhapus saat reboot.",
        ),
        Message::text(Role::Assistant, "Sudah diperbaiki memakai `/var/lib/releases`."),
    ];
    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: Mutex::new(vec![(
            json!({
                "actions": [{
                    "action": "patch_skill",
                    "skill": "deploy-helper",
                    "old_snippet": "2. Kirim berkas ke `/tmp/staging` memakai rsync.",
                    "new_snippet": "2. Kirim berkas ke `/var/lib/releases` memakai rsync karena `/tmp` terhapus saat reboot.",
                    "reason": "ganti path staging di tempatnya"
                }]
            })
            .to_string(),
            Usage {
                input_tokens: 3400,
                output_tokens: 90,
                ..Default::default()
            },
        )]),
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let outcome = run_background_review(prov, &cfg, &store, &proj_dir, &sid, &msgs, true)
        .await
        .unwrap()
        .expect("review harus menambal berkas store pengguna");
    let report = outcome.report_message.as_deref().unwrap_or("");
    assert_eq!(
        report,
        "💾 Selena improvement: Skill 'deploy-helper' patched"
    );
    let after_review = std::fs::read_to_string(&user_skill_md).unwrap();
    assert!(!after_review.contains("/tmp/staging"), "{after_review}");
    assert!(after_review.contains("/var/lib/releases"), "{after_review}");
    assert!(
        !proj_dir.join(".selencode/skills").exists(),
        "sesudah review, tetap tidak boleh ada salinan bayangan di proyek!"
    );

    match prev_data {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&data_root);
    let _ = std::fs::remove_dir_all(&proj_dir);
}

#[tokio::test]
async fn masalah1_explicit_project_override_shadow_visibility_and_review_project_scope() {
    let _guard = env_lock().lock().await;
    let data_root = temp_dir("m1-shadow-data");
    let proj_dir = temp_dir("m1-shadow-proj");
    let prev_data = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &data_root) };

    let user_skill_dir = data_root
        .join("selencode")
        .join("skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("deploy-helper");
    std::fs::create_dir_all(&user_skill_dir).unwrap();
    let user_skill_md = user_skill_dir.join("SKILL.md");
    let initial_md = format!(
        "---\nname: deploy-helper\ndescription: Prosedur rilis global\nversion: 1.0.0\norigin: {IMPROVEMENT_SKILL_ORIGIN}\n---\n\n\
         # Deploy Helper\n\n\
         1. Bangun biner.\n\
         2. Kirim ke `/tmp/staging`.\n"
    );
    std::fs::write(&user_skill_md, &initial_md).unwrap();

    // Buat salinan proyek secara EKSPLISIT (`/skills override deploy-helper`).
    let (src_path, dst_path) = copy_user_skill_to_project(&proj_dir, "deploy-helper").unwrap();
    assert_eq!(src_path, user_skill_md);
    assert!(dst_path.is_file());
    assert!(dst_path.starts_with(proj_dir.join(".selencode/skills")));

    // Verifikasi presedensi pemuatan (proyek menang) + keterlihatan bayangan di `/skills` & `skills_list`.
    let scan = scan_skills_with_warnings(&proj_dir);
    let skill = scan
        .skills
        .iter()
        .find(|s| s.name == "deploy-helper")
        .expect("deploy-helper harus terindeks");
    assert_eq!(skill.scope, "project", "presedensi pemuatan proyek > pengguna wajib utuh");
    assert!(skill.shadows_user_store(), "bayangan atas store user wajib tercatat");
    assert_eq!(skill.shadows.len(), 1);
    assert_eq!(skill.shadows[0].path, user_skill_md);

    let card = format_skills_list_card(&scan, false);
    assert!(card.contains("deploy-helper"), "{card}");
    assert!(card.contains("MENIMPA"), "kartu /skills wajib menandai bayangan: {card}");
    assert!(card.contains("proyek menang"), "{card}");
    assert!(
        card.contains(&dst_path.display().to_string()),
        "path store proyek wajib terlihat: {card}"
    );
    assert!(
        card.contains(&user_skill_md.display().to_string()),
        "path store pengguna yang tertimpa wajib terlihat: {card}"
    );

    let idx = format_skills_index(&scan.skills);
    assert!(
        idx.contains("deploy-helper (project; menimpa user)"),
        "skills_list / indeks wajib menandai bayangan proyek atas user: {idx}"
    );

    // Jalur review atas salinan proyek yang sah menulis ke salinan proyek DAN menyebut `(salinan proyek)`!
    let store = Store::open_at(&proj_dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&proj_dir.to_string_lossy(), "router", "primary-m", "uji scope proyek")
        .unwrap();
    let cfg = config_with_primary("primary-m");
    let msgs = vec![
        Message::text(Role::User, "Jalankan deploy-helper."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "sv2".into(),
                name: "skill_view".into(),
                args: json!({"name": "deploy-helper"}),
            }],
        },
        Message::text(
            Role::User,
            "Salah! Di proyek ini pakai `/srv/proj-releases`, bukan `/tmp/staging`.",
        ),
        Message::text(Role::Assistant, "Oke, memakai `/srv/proj-releases`."),
    ];
    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: Mutex::new(vec![(
            json!({
                "actions": [{
                    "action": "patch_skill",
                    "skill": "deploy-helper",
                    "old_snippet": "2. Kirim ke `/tmp/staging`.",
                    "new_snippet": "2. Kirim ke `/srv/proj-releases`.",
                    "reason": "koreksi path proyek"
                }]
            })
            .to_string(),
            Usage {
                input_tokens: 3100,
                output_tokens: 70,
                ..Default::default()
            },
        )]),
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let outcome = run_background_review(prov, &cfg, &store, &proj_dir, &sid, &msgs, true)
        .await
        .unwrap()
        .expect("review wajib menambal salinan proyek");
    assert_eq!(
        outcome.report_message.as_deref(),
        Some("💾 Selena improvement: Skill 'deploy-helper' patched (salinan proyek)")
    );
    assert!(std::fs::read_to_string(&dst_path)
        .unwrap()
        .contains("/srv/proj-releases"));
    assert_eq!(
        std::fs::read_to_string(&user_skill_md).unwrap(),
        initial_md,
        "berkas global tidak disentuh saat target teresolusi adalah salinan proyek eksplisit"
    );
    let backups = store.list_review_backups(5).unwrap();
    assert!(!backups.is_empty());
    assert!(
        backups[0].reason.contains("[salinan proyek]"),
        "catatan cadangan review wajib menyebut salinan proyek: {}",
        backups[0].reason
    );

    match prev_data {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&data_root);
    let _ = std::fs::remove_dir_all(&proj_dir);
}

#[tokio::test]
async fn masalah2_multi_call_review_records_each_call_individually_under_budget() {
    let _guard = env_lock().lock().await;
    let data_root = temp_dir("m2-data");
    let proj_dir = temp_dir("m2-proj");
    let prev_data = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &data_root) };

    let skill_dir = data_root
        .join("selencode")
        .join("skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("deploy-retry");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_md,
        format!(
            "---\nname: deploy-retry\ndescription: Uji retry dua panggilan\norigin: {IMPROVEMENT_SKILL_ORIGIN}\n---\n\n\
             # Deploy Retry\n\n\
             1. Siapkan artefak.\n\
             2. Kirim ke `/tmp/old-dir`.\n"
        ),
    )
    .unwrap();

    let store = Store::open_at(&proj_dir.join("selen.db")).unwrap();
    let cwd_str = proj_dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "router", "primary-m", "uji multi-call review_runs")
        .unwrap();
    let cfg = config_with_primary("primary-m");

    let msgs = vec![
        Message::text(Role::User, "Jalankan deploy-retry."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "sv3".into(),
                name: "skill_view".into(),
                args: json!({"name": "deploy-retry"}),
            }],
        },
        Message::text(
            Role::User,
            "Salah! Jangan pakai `/tmp/old-dir`, pakai `/var/lib/new-dir`.",
        ),
        Message::text(Role::Assistant, "Sudah diganti ke `/var/lib/new-dir`."),
    ];

    // Panggilan 1 (3.750 in): append-only -> ditolak pagar mutu -> Panggilan 2 retry (3.958 in): in-place!
    // Gabungan keduanya = 7.708 (persis kasus bukti Muse), tetapi tiap panggilan <= 5.000!
    let calls = Arc::new(AtomicUsize::new(0));
    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: Mutex::new(vec![
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-retry",
                        "append_section": "Catatan: gunakan /var/lib/new-dir.",
                        "reason": "koreksi direktori deploy"
                    }]
                })
                .to_string(),
                Usage {
                    input_tokens: 3_750,
                    output_tokens: 65,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            ),
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-retry",
                        "old_snippet": "2. Kirim ke `/tmp/old-dir`.",
                        "new_snippet": "2. Kirim ke `/var/lib/new-dir` karena `/tmp` terhapus saat reboot.",
                        "reason": "ganti path di tempatnya"
                    }]
                })
                .to_string(),
                Usage {
                    input_tokens: 3_958,
                    output_tokens: 85,
                    cache_read_tokens: 1_100,
                    cache_write_tokens: 0,
                },
            ),
        ]),
        calls: calls.clone(),
    });

    // mark_delivered = false untuk memastikan take_undelivered_review_reports hanya mengembalikan 1 laporan 💾!
    let outcome = run_background_review(prov, &cfg, &store, &proj_dir, &sid, &msgs, false)
        .await
        .unwrap()
        .expect("retry koreksi wajib berhasil");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.calls_count, 2);
    assert_eq!(outcome.max_call_input_tokens, 3_958);
    assert!(outcome.max_call_input_tokens <= MAX_REVIEW_CALL_INPUT_TOKENS);

    let runs = store.list_recent_review_runs(&cwd_str, 10).unwrap();
    assert_eq!(
        runs.len(),
        2,
        "peristiwa review 2 panggilan wajib tercatat sebagai 2 baris individual di review_runs"
    );
    // list_recent_review_runs diurutkan DESC (id terbesar / panggilan 2 dulu).
    let call2 = &runs[0];
    let call1 = &runs[1];
    assert!(!call1.event_id.is_empty());
    assert_eq!(call1.event_id, call2.event_id);
    assert_eq!(call1.call_index, 1);
    assert_eq!(call1.event_calls, 2);
    assert_eq!(call1.call_kind, "initial");
    assert_eq!(call1.input_tokens, 3_750);
    assert!(call1.input_tokens <= MAX_REVIEW_CALL_INPUT_TOKENS);
    assert!(call1.report_text.is_empty(), "baris panggilan antara tidak boleh menduplikasi laporan 💾");

    assert_eq!(call2.call_index, 2);
    assert_eq!(call2.event_calls, 2);
    assert_eq!(call2.call_kind, "correction_retry");
    assert_eq!(call2.input_tokens, 3_958);
    assert!(call2.input_tokens <= MAX_REVIEW_CALL_INPUT_TOKENS);
    assert!(call2.report_text.starts_with("💾 Selena improvement:"));

    let undelivered = store.take_undelivered_review_reports(&cwd_str).unwrap();
    assert_eq!(undelivered.len(), 1, "hanya boleh ada tepat 1 baris 💾 yang dikirim");

    match prev_data {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&data_root);
    let _ = std::fs::remove_dir_all(&proj_dir);
}

#[tokio::test]
async fn masalah3_telegram_api_base_env_routes_all_api_and_file_downloads_to_mock_server() {
    let _guard = env_lock().lock().await;
    let prev_base = std::env::var_os(TELEGRAM_API_BASE_ENV);

    // 1. Resolusi URL murni tanpa env -> alamat resmi Telegram!
    let (default_api, default_file) = resolve_telegram_api_urls("123:TOK", None);
    assert_eq!(default_api, format!("{DEFAULT_TELEGRAM_API_ORIGIN}/bot123:TOK"));
    assert_eq!(default_file, format!("{DEFAULT_TELEGRAM_API_ORIGIN}/file/bot123:TOK"));

    // 2. Resolusi URL dengan env (baik origin polos maupun dengan akhiran /bot<tok>).
    let (ov_api1, ov_file1) = resolve_telegram_api_urls("123:TOK", Some("http://127.0.0.1:18081/"));
    assert_eq!(ov_api1, "http://127.0.0.1:18081/bot123:TOK");
    assert_eq!(ov_file1, "http://127.0.0.1:18081/file/bot123:TOK");

    let (ov_api2, ov_file2) =
        resolve_telegram_api_urls("123:TOK", Some("http://127.0.0.1:18081/bot123:TOK"));
    assert_eq!(ov_api2, "http://127.0.0.1:18081/bot123:TOK");
    assert_eq!(ov_file2, "http://127.0.0.1:18081/file/bot123:TOK");

    // 3. Jalankan server Bot API palsu lokal & verifikasi TelegramClient::new menembak server lokal
    //    untuk SEMUA jalur: getMe, getUpdates (dokumen teks), getFile, download_file_bytes, sendMessage!
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen_paths.clone();

    let server_task = tokio::spawn(async move {
        for _ in 0..5 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("").to_string();
            seen_clone.lock().unwrap().push(path.clone());

            let (content_type, body): (&str, Vec<u8>) = if path.ends_with("/getMe") {
                (
                    "application/json",
                    json!({
                        "ok": true,
                        "result": { "id": 999, "first_name": "SelenaMock", "username": "SelenaMockBot" }
                    })
                    .to_string()
                    .into_bytes(),
                )
            } else if path.ends_with("/getUpdates") {
                (
                    "application/json",
                    json!({
                        "ok": true,
                        "result": [{
                            "update_id": 501,
                            "message": {
                                "message_id": 77,
                                "chat": { "id": 12345 },
                                "from": { "id": 12345, "first_name": "Bagus" },
                                "caption": "Bacakan kode di dokumen ini",
                                "document": {
                                    "file_id": "DOC_MOCK_77",
                                    "file_name": "ringkasan.txt",
                                    "mime_type": "text/plain",
                                    "file_size": 31
                                }
                            }
                        }]
                    })
                    .to_string()
                    .into_bytes(),
                )
            } else if path.ends_with("/getFile") {
                (
                    "application/json",
                    json!({
                        "ok": true,
                        "result": {
                            "file_id": "DOC_MOCK_77",
                            "file_size": 31,
                            "file_path": "documents/ringkasan.txt"
                        }
                    })
                    .to_string()
                    .into_bytes(),
                )
            } else if path == "/file/botTEST_TOKEN/documents/ringkasan.txt" {
                (
                    "application/octet-stream",
                    b"KODE_VERIFIKASI_V0527=LULUS_B2\n".to_vec(),
                )
            } else if path.ends_with("/sendMessage") {
                (
                    "application/json",
                    json!({
                        "ok": true,
                        "result": {
                            "message_id": 78,
                            "chat": { "id": 12345 },
                            "text": "ok"
                        }
                    })
                    .to_string()
                    .into_bytes(),
                )
            } else {
                (
                    "application/json",
                    json!({ "ok": false, "description": format!("unexpected path: {path}") })
                        .to_string()
                        .into_bytes(),
                )
            };

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.shutdown().await;
        }
    });

    unsafe { std::env::set_var(TELEGRAM_API_BASE_ENV, format!("http://{addr}")) };
    let tg = TelegramClient::new("TEST_TOKEN").unwrap();
    assert_eq!(tg.api_base_url(), format!("http://{addr}/botTEST_TOKEN"));
    assert_eq!(tg.file_base_url(), format!("http://{addr}/file/botTEST_TOKEN"));

    let me = tg.get_me().await.unwrap();
    assert_eq!(me.username.as_deref(), Some("SelenaMockBot"));

    let updates = tg.get_updates(0, 1).await.unwrap();
    assert_eq!(updates.len(), 1);
    let msg = updates[0].message.as_ref().unwrap();
    let (file_id, file_name, mime_type) = match classify_tg_message(msg) {
        TgIngressClassification::Document {
            file_id,
            file_name,
            mime_type,
            ..
        } => (file_id, file_name, mime_type),
        other => panic!("harus terklasifikasi sebagai Document, dapat: {other:?}"),
    };

    let fi = tg.get_file(&file_id).await.unwrap();
    let fp = fi.file_path.as_deref().unwrap();
    let bytes = tg.download_file_bytes(fp).await.unwrap();
    assert_eq!(bytes, b"KODE_VERIFIKASI_V0527=LULUS_B2\n");

    let work_dir = temp_dir("m3-uploads");
    let saved_rel = save_tg_document_bytes(
        &work_dir,
        msg.message_id,
        file_name.as_deref(),
        mime_type.as_deref(),
        Some(fp),
        &bytes,
    )
    .unwrap();
    let saved_abs = work_dir.join(&saved_rel);
    assert!(saved_abs.is_file());
    assert!(saved_rel.starts_with(".selencode/tg_uploads/tg_77_"));
    assert_eq!(
        std::fs::read_to_string(&saved_abs).unwrap(),
        "KODE_VERIFIKASI_V0527=LULUS_B2\n"
    );

    let sent_id = tg.send_message(12345, "KODE_VERIFIKASI_V0527=LULUS_B2").await.unwrap();
    assert_eq!(sent_id, 78);

    server_task.await.unwrap();
    let recorded = seen_paths.lock().unwrap().clone();
    assert!(recorded.iter().any(|p| p == "/botTEST_TOKEN/getMe"));
    assert!(recorded.iter().any(|p| p == "/botTEST_TOKEN/getUpdates"));
    assert!(recorded.iter().any(|p| p == "/botTEST_TOKEN/getFile"));
    assert!(recorded
        .iter()
        .any(|p| p == "/file/botTEST_TOKEN/documents/ringkasan.txt"));
    assert!(recorded.iter().any(|p| p == "/botTEST_TOKEN/sendMessage"));

    // 4. Tanpa env, TelegramClient::new kembali memakai alamat resmi Telegram!
    unsafe { std::env::remove_var(TELEGRAM_API_BASE_ENV) };
    let prod_tg = TelegramClient::new("TEST_TOKEN").unwrap();
    assert_eq!(
        prod_tg.api_base_url(),
        "https://api.telegram.org/botTEST_TOKEN"
    );
    assert_eq!(
        prod_tg.file_base_url(),
        "https://api.telegram.org/file/botTEST_TOKEN"
    );

    match prev_base {
        Some(v) => unsafe { std::env::set_var(TELEGRAM_API_BASE_ENV, v) },
        None => unsafe { std::env::remove_var(TELEGRAM_API_BASE_ENV) },
    }
    let _ = std::fs::remove_dir_all(&work_dir);
}
