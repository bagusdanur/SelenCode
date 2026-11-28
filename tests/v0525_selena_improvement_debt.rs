//! Uji otomatis SelenCode v0.5.25 — Penutupan 7 Utang Review (Selena Improvement):
//! 1. Anti-tembak ganda: review akhir sesi berjalan tepat 1x (1 baris `review_runs`, <= 1 baris 💾).
//! 2. Sinyal kelas-satu tertunda tidak pernah ditelan cooldown & cakupan digest menyertakan giliran koreksi.
//! 3. Pagu total request <= 5.000 token/panggilan + pemangkasan progresif + skip `digest_over_budget`.
//! 4. Awalan beku `FROZEN_REVIEW_SYSTEM_PROMPT` byte-identik lintas review, melampaui ambang 2.048 token
//!    implicit caching sambil total request tetap <= 5.000 token, dan `cache_read_tokens > 0` tercatat.
//! 5. Mutu patch: koreksi mengganti teks keliru di tempatnya (tolak append-only -> retry 1x / staging).
//! 6. Plumbing `CreateSkill` ujung-ke-ujung + bebas bias saat ada skill tak relevan.
//! 7. Tracker verifikasi mengenali `curl`/`wget` ke server lokal (`127.0.0.1`/`localhost`/`[::1]`)
//!    dan menolak URL eksternal serta kode HTTP 4xx/5xx.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::corrector::{
    is_local_http_verification_cmd, is_verification_shell_cmd, VerificationTracker,
};
use selencode::agent::review::{
    build_pruned_review_prompt_with_reads_for_session, check_review_eligibility,
    estimate_review_request_tokens, run_background_review, ReviewSkipReason,
    FROZEN_REVIEW_SYSTEM_PROMPT, MAX_REVIEW_CALL_INPUT_TOKENS,
};
use selencode::config::{Config, ProviderCfg};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::skills::{scan_skills_in_dirs, IMPROVEMENT_SKILLS_SUBDIR, IMPROVEMENT_SKILL_ORIGIN};
use selencode::store::Store;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v0525-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_with_primary(model: &str) -> Config {
    let mut cfg = Config::empty();
    cfg.default_provider = "main".into();
    cfg.providers.insert(
        "main".into(),
        ProviderCfg {
            kind: "openai".into(),
            base_url: Some("https://example.com/v1".into()),
            model: model.into(),
            models: Vec::new(),
            api_key_env: "TEST_KEY".into(),
            api_key: None,
            price_in: None,
            price_out: None,
            context_limit: None,
            fallback: Vec::new(),
        },
    );
    cfg
}

struct SequenceMockProvider {
    model: String,
    responses: std::sync::Mutex<Vec<(String, Usage)>>,
    delay_ms: u64,
    calls: Arc<AtomicUsize>,
    captured_users: Arc<std::sync::Mutex<Vec<String>>>,
    captured_systems: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for SequenceMockProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(sys) = messages.iter().find(|m| m.role == Role::System) {
            self.captured_systems.lock().unwrap().push(sys.plain_text());
        }
        if let Some(usr) = messages.iter().find(|m| m.role == Role::User) {
            self.captured_users.lock().unwrap().push(usr.plain_text());
        }
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        let mut guard = self.responses.lock().unwrap();
        let (text, usage) = if guard.len() > 1 {
            guard.remove(0)
        } else {
            guard
                .first()
                .cloned()
                .unwrap_or_else(|| (r#"{"actions":[]}"#.to_string(), Usage::default()))
        };
        Ok(AssistantReply {
            text,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(usage),
        })
    }
}

#[tokio::test]
async fn masalah1_concurrent_and_repeated_end_of_session_review_fires_strictly_once() {
    let _guard = env_lock().lock().await;
    let dir = temp_dir("m1-idempotent");
    let prev = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "main", "primary-m", "uji anti ganda")
        .unwrap();
    let cfg = config_with_primary("primary-m");

    let msgs = vec![
        Message::text(Role::User, "Simpan hasil unduhan ke folder default."),
        Message::text(Role::Assistant, "Saya arahkan ke /tmp/out."),
        Message::text(
            Role::User,
            "Salah, selalu pakai ~/archive/out bukan /tmp/out.",
        ),
        Message::text(Role::Assistant, "Baik, sudah dipindahkan ke ~/archive/out."),
    ];

    let calls = Arc::new(AtomicUsize::new(0));
    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: std::sync::Mutex::new(vec![(
            json!({
                "actions": [{
                    "action": "add_memory",
                    "category": "conventions",
                    "scope": "project",
                    "content": "Selalu simpan keluaran ke ~/archive/out (bukan /tmp/out).",
                    "reason": "aturan folder keluaran"
                }]
            })
            .to_string(),
            Usage {
                input_tokens: 3_100,
                output_tokens: 90,
                ..Default::default()
            },
        )]),
        delay_ms: 80,
        calls: calls.clone(),
        captured_users: Arc::new(std::sync::Mutex::new(Vec::new())),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    // Jalankan 2 jalur review secara SERENTAK untuk snapshot sesi yang sama (simulasi balapan REPL + EOF).
    let (r1, r2) = tokio::join!(
        run_background_review(prov.clone(), &cfg, &store, &dir, &sid, &msgs, false),
        run_background_review(prov.clone(), &cfg, &store, &dir, &sid, &msgs, false)
    );
    let outcomes: Vec<_> = [r1.unwrap(), r2.unwrap()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        outcomes.len(),
        1,
        "tepat 1 jalur yang boleh menghasilkan laporan 💾 pada snapshot akhir sesi yang sama"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "provider wajib dipanggil tepat 1 kali");

    // Panggilan ke-3 sesudah selesai pada snapshot yang sama -> langsung ditolak (`AlreadyReviewed`)!
    let r3 = run_background_review(prov, &cfg, &store, &dir, &sid, &msgs, false)
        .await
        .unwrap();
    assert!(r3.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &msgs),
        Err(ReviewSkipReason::AlreadyReviewed)
    );

    let runs = store.list_recent_review_runs(&cwd_str, 10).unwrap();
    assert_eq!(runs.len(), 1, "hanya boleh ada tepat 1 baris di review_runs");
    assert_eq!(runs[0].covered_msg_count, msgs.len());

    match prev {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn masalah2_cooldown_preserves_turn2_user_correction_and_fires_after_cooldown() {
    let _guard = env_lock().lock().await;
    let dir = temp_dir("m2-cooldown-signal");
    let prev = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "main", "primary-m", "uji cooldown sinyal")
        .unwrap();
    let mut cfg = config_with_primary("primary-m");
    cfg.review.cooldown_mins = 10;

    // Giliran 1: review menembak & selesai diam (belum ada pelajaran).
    let mut msgs = vec![
        Message::text(Role::User, "Periksa proyek."),
        Message {
            role: Role::Assistant,
            content: vec![
                Block::ToolUse {
                    id: "t1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "a.txt"}),
                },
                Block::ToolUse {
                    id: "t2".into(),
                    name: "read_file".into(),
                    args: json!({"path": "b.txt"}),
                },
                Block::ToolUse {
                    id: "t3".into(),
                    name: "read_file".into(),
                    args: json!({"path": "c.txt"}),
                },
                Block::ToolUse {
                    id: "t4".into(),
                    name: "read_file".into(),
                    args: json!({"path": "d.txt"}),
                },
            ],
        },
        Message::text(Role::Assistant, "Semua berkas sudah dicek."),
    ];
    let captured_users = Arc::new(std::sync::Mutex::new(Vec::new()));
    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: std::sync::Mutex::new(vec![
            (r#"{"actions":[]}"#.into(), Usage { input_tokens: 2800, output_tokens: 20, ..Default::default() }),
            (
                json!({
                    "actions": [{
                        "action": "add_memory",
                        "category": "conventions",
                        "scope": "project",
                        "content": "Gunakan endpoint /api/v2/export (bukan /api/v1/export).",
                        "reason": "koreksi endpoint dari giliran 2"
                    }]
                })
                .to_string(),
                Usage { input_tokens: 3100, output_tokens: 75, cache_read_tokens: 2200, ..Default::default() },
            ),
        ]),
        delay_ms: 0,
        calls: Arc::new(AtomicUsize::new(0)),
        captured_users: captured_users.clone(),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    // Review giliran 1 -> diam, mencatat covered_msg_count = 3 & memulai cooldown 10 menit.
    assert!(run_background_review(prov.clone(), &cfg, &store, &dir, &sid, &msgs, true)
        .await
        .unwrap()
        .is_none());
    assert_eq!(store.last_reviewed_msg_count(&sid).unwrap(), 3);

    // Giliran 2: koreksi eksplisit datang saat cooldown masih aktif!
    msgs.push(Message::text(
        Role::User,
        "Salah, jangan pakai /api/v1/export — yang benar adalah /api/v2/export!",
    ));
    msgs.push(Message::text(
        Role::Assistant,
        "Dimengerti, sudah diperbaiki ke /api/v2/export.",
    ));
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &msgs),
        Err(ReviewSkipReason::CooldownActive)
    );
    // Sinyal kelas-satu WAJIB tersimpan persisten (tidak ditelan cooldown!).
    let sig = store
        .get_pending_review_signal(&sid)
        .unwrap()
        .expect("koreksi giliran 2 wajib dicatat sebagai pending_review_signal");
    assert!(sig.signal_kind.contains("user_correction"));

    // Giliran 3: obrolan biasa tanpa sinyal baru, cooldown masih aktif -> sinyal TETAP terjaga.
    msgs.push(Message::text(Role::User, "Oke lanjut cek status."));
    msgs.push(Message::text(Role::Assistant, "Status aman."));
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &msgs),
        Err(ReviewSkipReason::CooldownActive)
    );
    assert!(store.get_pending_review_signal(&sid).unwrap().is_some());

    // Simulasikan cooldown selesai (`cooldown_mins = 0`) pada titik kelayakan pertama berikutnya!
    cfg.review.cooldown_mins = 0;
    assert!(check_review_eligibility(&cfg, &store, &sid, &msgs).is_ok());

    let outcome = run_background_review(prov, &cfg, &store, &dir, &sid, &msgs, true)
        .await
        .unwrap()
        .expect("review pasca-cooldown wajib menindaklanjuti sinyal tertunda giliran 2");
    assert_eq!(
        outcome.report_message.as_deref(),
        Some("💾 Selena improvement: Memory updated")
    );
    assert!(
        captured_users.lock().unwrap()[1].contains("/api/v2/export"),
        "digest review pasca-cooldown wajib tetap mencakup koreksi giliran 2"
    );
    assert!(
        store.get_pending_review_signal(&sid).unwrap().is_none(),
        "sinyal tertunda wajib dibersihkan sesudah ditindaklanjuti"
    );

    match prev {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn masalah3_and_4_total_budget_enforced_digest_over_budget_and_cacheable_prefix() {
    let _guard = env_lock().lock().await;
    let dir = temp_dir("m3-m4-budget-cache");
    let prev = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    // Pasang 3 skill panjang (> 4.000 char masing-masing) untuk memicu skenario kaya konteks.
    for idx in 1..=3 {
        let sdir = dir.join(format!(".selencode/skills/ops/heavy-skill-{idx}"));
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("SKILL.md"),
            format!(
                "---\nname: heavy-skill-{idx}\ndescription: Skill panjang #{idx}\ntags: [heavy]\n---\n# Heavy {idx}\n{}\n",
                "Langkah panjang dan detail untuk pengujian pagu token. ".repeat(90)
            ),
        )
        .unwrap();
    }

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "main", "ag/gemini-3-flash", "uji pagu & cache")
        .unwrap();

    // 20 giliran panjang + koreksi eksplisit di tengah sesi.
    let mut rich_msgs = Vec::new();
    for i in 0..10 {
        rich_msgs.push(Message::text(Role::User, format!("Langkah rutin #{i}: baca log.")));
        rich_msgs.push(Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: format!("r{i}"),
                content: format!("output panjang #{i}: {}", "L".repeat(2_000)),
                is_error: false,
            }],
        });
    }
    rich_msgs.push(Message::text(
        Role::User,
        "Salah! Untuk heavy-skill-1, selalu pakai flag --strict-mode bukan --loose.",
    ));
    rich_msgs.push(Message::text(
        Role::Assistant,
        "Sudah diganti ke --strict-mode sesuai koreksi.",
    ));
    for i in 10..20 {
        rich_msgs.push(Message::text(Role::User, format!("Langkah lanjut #{i}: cek berkas.")));
        rich_msgs.push(Message::text(
            Role::Assistant,
            format!("Selesai langkah #{i}: {}", "Z".repeat(600)),
        ));
    }

    // 1. Pagu TOTAL request (System beku + User digest) wajib <= 5.000 token DAN koreksi tetap utuh!
    let (sys, user, _) = build_pruned_review_prompt_with_reads_for_session(
        &dir,
        &store,
        Some(&sid),
        &rich_msgs,
        6_000,
    );
    assert_eq!(sys, FROZEN_REVIEW_SYSTEM_PROMPT);
    let total_est = estimate_review_request_tokens(
        &[
            Message::text(Role::System, sys.as_str()),
            Message::text(Role::User, user.as_str()),
        ],
        &[],
    );
    assert!(
        total_est <= MAX_REVIEW_CALL_INPUT_TOKENS,
        "total request review wajib <= {MAX_REVIEW_CALL_INPUT_TOKENS}, dapat {total_est}"
    );
    assert!(
        user.contains("--strict-mode"),
        "giliran koreksi pengguna wajib dipertahankan paling akhir saat pemangkasan"
    );

    // 2. Masalah 4: ukuran awalan sistem beku >= 2.048 token (melampaui ambang implicit caching Gemini 3 Flash)
    //    sambil total request tetap <= 5.000!
    let sys_est_gemini = (FROZEN_REVIEW_SYSTEM_PROMPT.chars().count() as u64).div_ceil(3);
    assert!(
        sys_est_gemini >= 2_048,
        "awalan sistem beku harus >= 2.048 token untuk menyalakan implicit prefix cache (dapat ~{sys_est_gemini})"
    );

    // 3. Bila pagu terlalu sempit sehingga bahkan digest minimal tak muat -> skip `digest_over_budget`, 0 call!
    let mut tight_cfg = config_with_primary("ag/gemini-3-flash");
    tight_cfg.review.max_tokens = 1_200;
    let calls = Arc::new(AtomicUsize::new(0));
    let prov = Arc::new(SequenceMockProvider {
        model: "ag/gemini-3-flash".into(),
        responses: std::sync::Mutex::new(vec![(r#"{"actions":[]}"#.into(), Usage::default())]),
        delay_ms: 0,
        calls: calls.clone(),
        captured_users: Arc::new(std::sync::Mutex::new(Vec::new())),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    assert!(
        run_background_review(prov, &tight_cfg, &store, &dir, &sid, &rich_msgs, true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "dilarang mengirim request yang melampaui pagu");
    let skip = store.get_last_review_skip().unwrap().unwrap();
    assert_eq!(skip.0, "digest_over_budget");

    match prev {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn masalah5_correction_patch_rejects_append_only_retries_in_place_or_stages() {
    let _guard = env_lock().lock().await;
    let dir = temp_dir("m5-patch-quality");
    let prev = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    let skill_dir = dir
        .join("selencode/skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("deploy-helper");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_path = skill_dir.join("SKILL.md");
    let initial_md = "---\n\
        name: deploy-helper\n\
        description: Panduan deploy layanan\n\
        origin: selena-improvement\n\
        ---\n\
        # Deploy Helper\n\
        \n\
        1. Jalankan tes lokal.\n\
        2. Kirim berkas ke `/tmp/staging` memakai rsync.\n\
        3. Restart dengan `service app restart`.\n";
    std::fs::write(&skill_path, initial_md).unwrap();

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid1 = store
        .create_session(&cwd_str, "main", "primary-m", "uji retry koreksi")
        .unwrap();
    let cfg = config_with_primary("primary-m");

    let msgs = vec![
        Message::text(Role::User, "Gunakan skill deploy-helper."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "v1".into(),
                name: "skill_view".into(),
                args: json!({"name": "deploy-helper"}),
            }],
        },
        Message::text(
            Role::User,
            "Salah! Jangan pakai `/tmp/staging` lagi, folder kanonik adalah `/var/lib/releases`.",
        ),
        Message::text(Role::Assistant, "Sudah diganti ke `/var/lib/releases`."),
    ];

    // Skenario 5A: jawaban pertama murni tempel di akhir (`append_section`) -> DITOLAK & retry 1x ->
    // jawaban retry menunjuk `old_snippet` persis -> teks keliru `/tmp/staging` HILANG total!
    let calls_a = Arc::new(AtomicUsize::new(0));
    let prov_a = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: std::sync::Mutex::new(vec![
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-helper",
                        "append_section": "Catatan: gunakan /var/lib/releases.",
                        "reason": "koreksi path deploy"
                    }]
                })
                .to_string(),
                Usage { input_tokens: 3000, output_tokens: 60, ..Default::default() },
            ),
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-helper",
                        "old_snippet": "2. Kirim berkas ke `/tmp/staging` memakai rsync.",
                        "new_snippet": "2. Kirim berkas ke `/var/lib/releases` memakai rsync karena `/tmp/staging` hilang saat reboot.",
                        "reason": "ganti path staging di tempatnya"
                    }]
                })
                .to_string(),
                Usage { input_tokens: 3200, output_tokens: 80, cache_read_tokens: 2200, ..Default::default() },
            ),
        ]),
        delay_ms: 0,
        calls: calls_a.clone(),
        captured_users: Arc::new(std::sync::Mutex::new(Vec::new())),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let out_a = run_background_review(prov_a, &cfg, &store, &dir, &sid1, &msgs, true)
        .await
        .unwrap()
        .expect("retry koreksi wajib berhasil menambal di tempatnya");
    assert_eq!(calls_a.load(Ordering::SeqCst), 2, "harus 1 panggilan awal + 1 retry koreksi");
    assert_eq!(out_a.applied.len(), 1);
    assert!(out_a.staged_ids.is_empty());
    let after_a = std::fs::read_to_string(&skill_path).unwrap();
    assert!(
        !after_a.contains("/tmp/staging"),
        "aturan keliru `/tmp/staging` WAJIB hilang total dari berkas: {after_a}"
    );
    assert!(
        after_a.contains("/var/lib/releases"),
        "aturan terkoreksi wajib hadir di tempatnya: {after_a}"
    );

    // Skenario 5B: jika retry pun tetap gagal menunjuk `old_snippet` -> DILARANG menulis kontradiksi langsung;
    // wajib dialihkan ke staging `review_proposals` dan file asli tetap bersih!
    std::fs::write(&skill_path, initial_md).unwrap();
    let sid2 = store
        .create_session(&cwd_str, "main", "primary-m", "uji fallback staging")
        .unwrap();
    let prov_b = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: std::sync::Mutex::new(vec![
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-helper",
                        "append_section": "Catatan: gunakan /var/lib/releases.",
                        "reason": "koreksi path deploy"
                    }]
                })
                .to_string(),
                Usage::default(),
            ),
            (
                json!({
                    "actions": [{
                        "action": "patch_skill",
                        "skill": "deploy-helper",
                        "old_snippet": "teks yang tidak pernah ada di berkas",
                        "new_snippet": "2. Kirim ke /var/lib/releases.",
                        "reason": "gagal cocok"
                    }]
                })
                .to_string(),
                Usage::default(),
            ),
        ]),
        delay_ms: 0,
        calls: Arc::new(AtomicUsize::new(0)),
        captured_users: Arc::new(std::sync::Mutex::new(Vec::new())),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let out_b = run_background_review(prov_b, &cfg, &store, &dir, &sid2, &msgs, true)
        .await
        .unwrap()
        .expect("koreksi tanpa old_snippet cocok wajib masuk staging");
    assert!(out_b.applied.is_empty());
    assert_eq!(out_b.staged_ids.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&skill_path).unwrap(),
        initial_md,
        "berkas di disk tidak boleh ditulis patch kontradiktif secara langsung"
    );

    match prev {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn masalah6_create_skill_plumbing_and_prompt_allows_new_skill_despite_unrelated_catalog() {
    let _guard = env_lock().lock().await;
    let dir = temp_dir("m6-create-skill");
    let prev = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    // Pasang skill yang TIDAK relevan (`systematic-debugging`) di katalog.
    let unrel = dir.join(".selencode/skills/debug/systematic-debugging");
    std::fs::create_dir_all(&unrel).unwrap();
    std::fs::write(
        unrel.join("SKILL.md"),
        "---\nname: systematic-debugging\ndescription: Debug sistematis\n---\n# Debug\n",
    )
    .unwrap();

    // Prompt keputusan wajib secara eksplisit menegaskan bahwa `create_skill` dipilih saat tak ada skill relevan secara topik.
    assert!(FROZEN_REVIEW_SYSTEM_PROMPT.contains("RELEVAN secara topik"));
    assert!(FROZEN_REVIEW_SYSTEM_PROMPT.contains("PILIHAN WAJIB DAN TEPAT"));

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&dir.to_string_lossy(), "main", "primary-m", "uji create skill")
        .unwrap();
    let cfg = config_with_primary("primary-m");

    let msgs = vec![
        Message::text(Role::User, "Migrasikan tabel SQLite tanpa lock."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c1".into(),
                name: "run_shell".into(),
                args: json!({"command": "sqlite3 app.db 'ALTER TABLE ...'"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c1".into(),
                content: "Error: database is locked".into(),
                is_error: true,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c2".into(),
                name: "run_shell".into(),
                args: json!({"command": "sqlite3 app.db 'PRAGMA busy_timeout=5000; ALTER TABLE ...'"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c2".into(),
                content: "exit code: 0\nok".into(),
                is_error: false,
            }],
        },
    ];

    let prov = Arc::new(SequenceMockProvider {
        model: "primary-m".into(),
        responses: std::sync::Mutex::new(vec![(
            json!({
                "actions": [{
                    "action": "create_skill",
                    "skill": "sqlite-safe-migration",
                    "description": "Migrasi skema SQLite aman tanpa lock",
                    "body": "# SQLite Safe Migration\n\n1. Setel `PRAGMA busy_timeout = 5000;` sebelum DDL karena koneksi bersamaan langsung gagal bila timeout 0.",
                    "tags": ["sqlite", "migration"],
                    "reason": "prosedur migrasi SQLite baru"
                }]
            })
            .to_string(),
            Usage { input_tokens: 3100, output_tokens: 110, ..Default::default() },
        )]),
        delay_ms: 0,
        calls: Arc::new(AtomicUsize::new(0)),
        captured_users: Arc::new(std::sync::Mutex::new(Vec::new())),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let outcome = run_background_review(prov, &cfg, &store, &dir, &sid, &msgs, true)
        .await
        .unwrap()
        .expect("harus menciptakan skill baru");
    assert_eq!(
        outcome.report_message.as_deref(),
        Some("💾 Selena improvement: Skill 'sqlite-safe-migration' created")
    );

    let created_path = dir
        .join("selencode/skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("sqlite-safe-migration")
        .join("SKILL.md");
    assert!(created_path.exists());
    let raw = std::fs::read_to_string(&created_path).unwrap();
    assert!(raw.contains(&format!("origin: {IMPROVEMENT_SKILL_ORIGIN}")));
    assert!(raw.contains(&format!("source_session: {sid}")));
    assert!(raw.contains("learned_at:"));

    let other_proj = temp_dir("m6-other-proj");
    let scan = scan_skills_in_dirs(
        &other_proj.join(".selencode/skills"),
        Some(&dir.join("selencode/skills")),
    );
    assert!(
        scan.skills
            .iter()
            .any(|s| s.name == "sqlite-safe-migration" && s.is_improvement_skill()),
        "skill baru wajib langsung terindeks di proyek/sesi baru"
    );

    match prev {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&other_proj);
}

#[test]
fn masalah7_local_curl_and_wget_classified_as_verification_while_external_urls_excluded() {
    // 1. URL lokal -> dikenali sebagai perintah verifikasi!
    assert!(is_local_http_verification_cmd("curl -fsS http://127.0.0.1:4173"));
    assert!(is_local_http_verification_cmd("curl -s http://localhost:3000/api/health | grep ok"));
    assert!(is_local_http_verification_cmd("wget -qO- http://[::1]:8080/"));
    assert!(is_verification_shell_cmd(&json!({"command": "curl -fsS http://127.0.0.1:4173"})));
    assert!(is_verification_shell_cmd(&json!({"command": "curl -sS -i http://localhost:5173/ | head -n 5"})));

    // 2. URL eksternal -> BUKAN verifikasi hasil kerja lokal!
    assert!(!is_local_http_verification_cmd("curl -fsS https://example.com"));
    assert!(!is_local_http_verification_cmd("wget https://api.github.com/repos/foo/bar"));
    assert!(!is_verification_shell_cmd(&json!({"command": "curl -s https://example.com"})));

    // 3. VerificationTracker: edit file web + `curl` ke server preview lokal (exit 0 + HTTP 200) -> LULUS!
    let mut tr = VerificationTracker::new(None);
    tr.observe_tool("write_file", &json!({"path": "index.html"}), "ok", false);
    tr.observe_tool(
        "run_shell",
        &json!({"command": "curl -fsS -i http://127.0.0.1:4173"}),
        "exit code: 0\nHTTP/1.1 200 OK\n<title>Catatan Keuangan</title>",
        false,
    );
    assert!(tr.has_verification_run);
    assert!(tr.verified_after_last_edit);
    assert_eq!(tr.last_verify_passed, Some(true));
    let summary = tr.closing_verification_summary_line(true).unwrap();
    assert!(
        summary.contains("curl -fsS -i http://127.0.0.1:4173") && summary.contains("(LULUS)"),
        "cap penutup harus menyebut perintah curl lokal & LULUS: {summary}"
    );

    // 4. `curl` lokal mengembalikan HTTP 404 (meski exit 0 tanpa `-f`) -> GAGAL!
    let mut tr_404 = VerificationTracker::new(None);
    tr_404.observe_tool("write_file", &json!({"path": "index.html"}), "ok", false);
    tr_404.observe_tool(
        "run_shell",
        &json!({"command": "curl -s -i http://127.0.0.1:4173/notfound"}),
        "exit code: 0\nHTTP/1.1 404 Not Found\nNot Found",
        false,
    );
    assert_eq!(tr_404.last_verify_passed, Some(false));
    let sum_404 = tr_404.closing_verification_summary_line(true).unwrap();
    assert!(sum_404.contains("(GAGAL)"), "HTTP 404 harus berstatus GAGAL: {sum_404}");

    // 5. Fetch ke URL eksternal TIDAK mengubah status `belum diverifikasi`!
    let mut tr_ext = VerificationTracker::new(None);
    tr_ext.observe_tool("write_file", &json!({"path": "index.html"}), "ok", false);
    tr_ext.observe_tool(
        "run_shell",
        &json!({"command": "curl -s https://example.com"}),
        "exit code: 0\n<html>Example</html>",
        false,
    );
    assert!(!tr_ext.has_verification_run);
    let sum_ext = tr_ext.closing_verification_summary_line(true).unwrap();
    assert!(
        sum_ext.contains("belum diverifikasi"),
        "curl ke URL eksternal tidak boleh dihitung sebagai verifikasi: {sum_ext}"
    );
}
