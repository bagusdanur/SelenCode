//! Uji otomatis SelenCode v0.5.24 — Selena Improvement & Penghapusan Loop Self-Correction Penutup.
//!
//! Memverifikasi secara deterministik (mock provider, tanpa panggilan API live):
//! 1. Default hidup tanpa `[aux]` (pakai model sesi utama) + `[aux]` tetap override + `ReviewSkipReason`
//!    tercatat di SQLite dan tampil di `run_doctor`.
//! 2. Prompt review sadar-cache (`FROZEN_REVIEW_SYSTEM_PROMPT` byte-identik lintas sesi),
//!    pencatatan `cache_read_tokens` / `cache_write_tokens` di `review_runs` & `usage(kind='review')`,
//!    serta pagu input keras `<= 5.000` token per panggilan.
//! 3. `CreateSkill` menulis ke `<user_skills_dir>/improvement/<nama>/SKILL.md` dengan metadata
//!    `origin: selena-improvement`, `source_session`, `learned_at`, langsung terindeks lintas sesi/proyek,
//!    presedensi di bawah skill pengguna/proyek, dan tabrakan nama dengan skill pengguna -> staging.
//! 4. Proteksi skill pengguna: `PatchSkill` ke skill pengguna WAJIB masuk staging (file asli utuh),
//!    sedangkan `PatchSkill` ke skill `origin: selena-improvement` dipatch langsung dengan cadangan
//!    di `review_backups` & bisa diputar balik lewat `rollback_direct_review_patch`.
//! 5. Sinyal belajar (`has_recovered_failure`, `has_valuable_review_signal`) + kontrol trivial & `{"actions":[]}` diam.
//! 6. Penghapusan loop refleksi self-correction di akhir giliran: 0 panggilan refleksi tambahan,
//!    0 event `"Self-correction ("`, jawaban akhir terkirim tepat 1x sambil mempertahankan ringkasan
//!    verifikasi deterministik & catatan berkas sementara (`repro_*`).

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::review::{
    build_pruned_review_prompt_with_reads, check_review_eligibility, has_recovered_failure,
    has_valuable_review_signal, resolve_effective_review_model, rollback_direct_review_patch,
    run_background_review, ReviewSkipReason, FROZEN_REVIEW_SYSTEM_PROMPT,
    MAX_REVIEW_CALL_INPUT_TOKENS,
};
use selencode::agent::{run_turn_ui, UiEvent};
use selencode::config::{Config, ProviderCfg};
use selencode::doctor::run_doctor;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::skills::{
    scan_skills_in_dirs, IMPROVEMENT_SKILLS_SUBDIR, IMPROVEMENT_SKILL_ORIGIN,
};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{mpsc, Mutex};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v0524-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct MockCacheProvider {
    model: String,
    response_text: String,
    usage: Usage,
    calls: Arc<AtomicUsize>,
    captured_systems: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for MockCacheProvider {
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
            self.captured_systems
                .lock()
                .unwrap()
                .push(sys.plain_text());
        }
        Ok(AssistantReply {
            text: self.response_text.clone(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(self.usage.clone()),
        })
    }
}

fn config_with_primary_only(model: &str) -> Config {
    let mut cfg = Config::empty();
    cfg.default_provider = "main".into();
    let mut providers = BTreeMap::new();
    providers.insert(
        "main".into(),
        ProviderCfg {
            kind: "openai".into(),
            base_url: "https://example.com/v1".into(),
            model: model.into(),
            api_key_env: "TEST_KEY".into(),
            api_key: None,
            extra_headers: BTreeMap::new(),
            context_window: None,
            max_output_tokens: None,
            models: Vec::new(),
            reasoning_effort: None,
            cache_control: true,
        },
    );
    cfg.providers = providers;
    cfg
}

#[test]
fn default_alive_without_aux_and_skip_reason_visible_in_doctor() {
    let dir = temp_dir("default-alive");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&dir.to_string_lossy(), "main", "claude-sonnet-4", "uji")
        .unwrap();

    // 1. Tanpa [aux], model efektif otomatis jatuh ke model sesi utama!
    let mut cfg = config_with_primary_only("claude-sonnet-4");
    assert_eq!(
        resolve_effective_review_model(&cfg).as_deref(),
        Some("claude-sonnet-4")
    );

    let substantive = vec![
        Message::text(Role::User, "Jalankan build proyek."),
        Message::text(Role::Assistant, "Gagal dengan error PATH."),
        Message::text(
            Role::User,
            "Salah, selalu pakai `npm run build:prod` bukan `npm run build`.",
        ),
        Message::text(Role::Assistant, "Baik, sudah pakai `npm run build:prod`."),
    ];
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &substantive),
        Ok("claude-sonnet-4".to_string())
    );

    // 2. [aux] review_model tetap menjadi override opsional saat diisi.
    cfg.aux.review_model = Some("aux-fast-model".into());
    assert_eq!(
        resolve_effective_review_model(&cfg).as_deref(),
        Some("aux-fast-model")
    );

    // 3. Sesi trivial -> TrivialSession tercatat di SQLite & muncul di `run_doctor`!
    let trivial = vec![
        Message::text(Role::User, "Halo!"),
        Message::text(Role::Assistant, "Halo, ada yang bisa dibantu?"),
    ];
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &trivial),
        Err(ReviewSkipReason::TrivialSession)
    );
    let last_skip = store.get_last_review_skip().unwrap().unwrap();
    assert_eq!(last_skip.0, "trivial_session");

    let pcfg = cfg.providers.get("main").unwrap();
    let sandbox = SandboxController::new(selencode::config::SandboxCfg::default());
    let report = run_doctor(&cfg, &dir, "main", pcfg, &store, &sandbox);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "Selena Improvement")
        .expect("run_doctor wajib memuat pemeriksaan Selena Improvement");
    assert!(
        check.detail.contains("aux-fast-model") && check.detail.contains("trivial_session"),
        "detail doctor harus menampilkan model efektif & alasan lewati terakhir: {}",
        check.detail
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn frozen_system_prompt_budget_cap_and_cache_metrics_recording() {
    let _guard = env_lock().await;
    let dir = temp_dir("cache-budget");
    let prev_data_dir = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "main", "gpt-5-primary", "uji cache")
        .unwrap();

    // 1. Prompt sistem beku byte-identik & pagu input keras <= 5.000 token meski histori masif.
    let mut huge_msgs = vec![Message::text(
        Role::User,
        "Salah, selalu pakai port 8081 bukan 8080.",
    )];
    for i in 0..40 {
        huge_msgs.push(Message::text(
            Role::Assistant,
            format!("Log panjang iterasi #{i}: {}", "X".repeat(1_500)),
        ));
    }
    let (sys1, user1, _) = build_pruned_review_prompt_with_reads(&dir, &store, &huge_msgs, 6_000);
    let (sys2, _user2, _) =
        build_pruned_review_prompt_with_reads(&dir, &store, &huge_msgs[..5], 6_000);
    assert_eq!(sys1, FROZEN_REVIEW_SYSTEM_PROMPT);
    assert_eq!(sys1, sys2, "awalan sistem review wajib byte-identik untuk prompt cache");
    let est_tokens = (sys1.chars().count() + user1.chars().count()).div_ceil(3) as u64;
    assert!(
        est_tokens <= MAX_REVIEW_CALL_INPUT_TOKENS,
        "estimasi input per panggilan wajib <= {MAX_REVIEW_CALL_INPUT_TOKENS}, dapat {est_tokens}"
    );

    // 2. Jalankan review tanpa [aux] (pakai model utama) dan pastikan cache_read/cache_write tercatat!
    let cfg = config_with_primary_only("gpt-5-primary");
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let prov = Arc::new(MockCacheProvider {
        model: "gpt-5-primary".into(),
        response_text: json!({
            "actions": [{
                "action": "add_memory",
                "category": "conventions",
                "scope": "project",
                "content": "Gunakan port 8081 untuk server lokal proyek ini (bukan 8080).",
                "reason": "aturan port lokal"
            }]
        })
        .to_string(),
        usage: Usage {
            input_tokens: 1_850,
            output_tokens: 120,
            cache_read_tokens: 1_200,
            cache_write_tokens: 350,
            ..Default::default()
        },
        calls: calls.clone(),
        captured_systems: captured.clone(),
    });

    let outcome = run_background_review(prov, &cfg, &store, &dir, &sid, &huge_msgs, false)
        .await
        .unwrap()
        .expect("harus menghasilkan outcome review");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.cache_read_tokens, 1_200);
    assert_eq!(outcome.cache_write_tokens, 350);
    assert_eq!(
        captured.lock().unwrap()[0],
        FROZEN_REVIEW_SYSTEM_PROMPT
    );
    let report = outcome.report_message.as_deref().unwrap();
    assert_eq!(report, "💾 Selena improvement: Memory updated");

    let runs = store.recent_review_runs(5).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].cache_read_tokens, 1_200);
    assert_eq!(runs[0].cache_write_tokens, 350);
    assert!(!runs[0].delivered);

    // Tandai terkirim hanya sesudah delivery berhasil.
    store.mark_review_run_delivered(runs[0].id).unwrap();
    assert!(store.take_undelivered_review_reports(&cwd_str).unwrap().is_empty());

    // Jalur utama tetap 0, review tercatat terpisah di tabel usage.
    assert_eq!(store.total_usage(&sid).unwrap(), (0, 0));
    assert_eq!(store.review_usage_for_session(&sid).unwrap(), (1_850, 120, 1));

    match prev_data_dir {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_skill_in_improvement_dir_cross_session_precedence_and_collision_staging() {
    let _guard = env_lock().await;
    let data_root = temp_dir("data-root");
    let proj_a = temp_dir("proj-a");
    let proj_b = temp_dir("proj-b");
    let prev_data_dir = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &data_root) };

    let store = Store::open_at(&data_root.join("selen.db")).unwrap();
    let sid = store
        .create_session(&proj_a.to_string_lossy(), "main", "primary-m", "sesi A")
        .unwrap();
    let cfg = config_with_primary_only("primary-m");

    // Sinyal: kegagalan tool yang berhasil pulih (recovered failure) -> cukup untuk memicu review!
    let recovered_msgs = vec![
        Message::text(Role::User, "Ambil daftar paket dari registry internal."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c1".into(),
                name: "run_shell".into(),
                args: json!({"command": "curl https://registry.internal/list"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c1".into(),
                content: "exit code: 60\nSSL certificate problem".into(),
                is_error: true,
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "c2".into(),
                name: "run_shell".into(),
                args: json!({"command": "curl --cacert /etc/ssl/corp.pem https://registry.internal/list"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "c2".into(),
                content: "exit code: 0\n{\"ok\":true}".into(),
                is_error: false,
            }],
        },
        Message::text(Role::Assistant, "Berhasil setelah memakai --cacert /etc/ssl/corp.pem."),
    ];
    assert!(has_recovered_failure(&recovered_msgs));
    assert!(has_valuable_review_signal(&recovered_msgs));

    // 1. CreateSkill -> tulis ke <user_skills_dir>/improvement/<nama>/SKILL.md + metadata provenance.
    let prov = Arc::new(MockCacheProvider {
        model: "primary-m".into(),
        response_text: json!({
            "actions": [{
                "action": "create_skill",
                "name": "corp-registry-tls",
                "description": "Gunakan CA korporat saat curl ke registry internal",
                "body": "# Corp Registry TLS\n\n- Selalu sertakan `--cacert /etc/ssl/corp.pem` saat mengakses `registry.internal` karena CA internal tidak masuk bundle sistem.",
                "tags": ["tls", "curl"],
                "reason": "workaround TLS registry internal"
            }]
        })
        .to_string(),
        usage: Usage {
            input_tokens: 900,
            output_tokens: 110,
            ..Default::default()
        },
        calls: Arc::new(AtomicUsize::new(0)),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let outcome = run_background_review(prov, &cfg, &store, &proj_a, &sid, &recovered_msgs, true)
        .await
        .unwrap()
        .expect("harus membuat skill baru di skills/improvement/");
    assert_eq!(
        outcome.report_message.as_deref(),
        Some("💾 Selena improvement: Skill 'corp-registry-tls' created")
    );

    let imp_path = data_root
        .join("selencode")
        .join("skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("corp-registry-tls")
        .join("SKILL.md");
    assert!(imp_path.exists(), "file harus dibuat di {}", imp_path.display());
    let imp_raw = std::fs::read_to_string(&imp_path).unwrap();
    assert!(imp_raw.contains(&format!("origin: {IMPROVEMENT_SKILL_ORIGIN}")));
    assert!(imp_raw.contains(&format!("source_session: {sid}")));
    assert!(imp_raw.contains("learned_at:"));

    // 2. Lintas sesi/proyek: dari proyek B yang sama sekali berbeda, skill langsung terindeks!
    let user_skills = data_root.join("selencode").join("skills");
    let scan_b = scan_skills_in_dirs(&proj_b.join(".selencode/skills"), Some(&user_skills));
    let found = scan_b
        .skills
        .iter()
        .find(|s| s.name == "corp-registry-tls")
        .expect("skill improvement harus langsung terindeks di proyek/sesi baru");
    assert!(found.is_improvement_skill());

    // 3. Presedensi: bila pengguna punya skill dengan nama sama, skill PENGGUNA selalu menang!
    let user_owned_dir = user_skills.join("ops").join("corp-registry-tls");
    std::fs::create_dir_all(&user_owned_dir).unwrap();
    std::fs::write(
        user_owned_dir.join("SKILL.md"),
        "---\nname: corp-registry-tls\ndescription: Versi kanonik buatan pengguna\n---\n# Versi Pengguna\n",
    )
    .unwrap();
    let scan_prec = scan_skills_in_dirs(&proj_b.join(".selencode/skills"), Some(&user_skills));
    let winner = scan_prec
        .skills
        .iter()
        .find(|s| s.name == "corp-registry-tls")
        .unwrap();
    assert!(
        !winner.is_improvement_skill(),
        "skill pengguna WAJIB menang di atas skill improvement saat nama bertabrakan"
    );
    assert_eq!(winner.summary, "Versi kanonik buatan pengguna");

    // 4. Tabrakan nama saat `create_skill` -> DILARANG membuat duplikat bayangan di improvement/, masuk staging!
    let user_other_dir = user_skills.join("ops").join("existing-user-skill");
    std::fs::create_dir_all(&user_other_dir).unwrap();
    std::fs::write(
        user_other_dir.join("SKILL.md"),
        "---\nname: existing-user-skill\ndescription: Skill asli pengguna\n---\n# Asli\n",
    )
    .unwrap();

    let sid2 = store
        .create_session(&proj_b.to_string_lossy(), "main", "primary-m", "sesi B")
        .unwrap();
    let prov_collide = Arc::new(MockCacheProvider {
        model: "primary-m".into(),
        response_text: json!({
            "actions": [{
                "action": "create_skill",
                "name": "existing-user-skill",
                "description": "Coba timpa lewat create_skill",
                "body": "# Versi Review\nTidak boleh menimpa skill pengguna.",
                "reason": "uji anti tabrakan"
            }]
        })
        .to_string(),
        usage: Usage::default(),
        calls: Arc::new(AtomicUsize::new(0)),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let out_col = run_background_review(
        prov_collide,
        &cfg,
        &store,
        &proj_b,
        &sid2,
        &recovered_msgs,
        true,
    )
    .await
    .unwrap()
    .expect("tabrakan nama harus masuk staging");
    assert!(out_col.applied.is_empty());
    assert_eq!(out_col.staged_ids.len(), 1);
    assert!(
        !user_skills
            .join(IMPROVEMENT_SKILLS_SUBDIR)
            .join("existing-user-skill")
            .exists(),
        "dilarang membuat duplikat bayangan di skills/improvement/ saat nama sudah dimiliki skill pengguna"
    );

    match prev_data_dir {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&data_root);
    let _ = std::fs::remove_dir_all(&proj_a);
    let _ = std::fs::remove_dir_all(&proj_b);
}

#[tokio::test]
async fn user_skill_always_staged_while_improvement_skill_patches_directly_with_rollback() {
    let _guard = env_lock().await;
    let dir = temp_dir("protect-rollback");
    let prev_data_dir = std::env::var_os("SELENCODE_DATA_DIR");
    unsafe { std::env::set_var("SELENCODE_DATA_DIR", &dir) };

    // 1. Skill pengguna biasa (tanpa origin: selena-improvement)
    let user_skill_dir = dir.join(".selencode/skills/ops/user-deploy");
    std::fs::create_dir_all(&user_skill_dir).unwrap();
    let user_skill_path = user_skill_dir.join("SKILL.md");
    let user_orig = "---\nname: user-deploy\ndescription: Panduan deploy pengguna\n---\n# Deploy\n- Pakai `deploy.sh --staging`.\n";
    std::fs::write(&user_skill_path, user_orig).unwrap();

    // 2. Skill milik Selena improvement (di user_skills/improvement/ + origin: selena-improvement)
    let imp_skill_dir = dir
        .join("selencode/skills")
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join("imp-deploy");
    std::fs::create_dir_all(&imp_skill_dir).unwrap();
    let imp_skill_path = imp_skill_dir.join("SKILL.md");
    let imp_orig = "---\nname: imp-deploy\ndescription: Catatan deploy improvement\norigin: selena-improvement\n---\n# Imp Deploy\n- Pakai `--timeout 30`.\n";
    std::fs::write(&imp_skill_path, imp_orig).unwrap();

    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store
        .create_session(&dir.to_string_lossy(), "main", "primary-m", "uji proteksi")
        .unwrap();
    let cfg = config_with_primary_only("primary-m");

    let msgs = vec![
        Message::text(Role::User, "Jalankan deploy dengan kedua skill."),
        Message {
            role: Role::Assistant,
            content: vec![
                Block::ToolUse {
                    id: "v1".into(),
                    name: "skill_view".into(),
                    args: json!({"name": "user-deploy"}),
                },
                Block::ToolUse {
                    id: "v2".into(),
                    name: "skill_view".into(),
                    args: json!({"name": "imp-deploy"}),
                },
            ],
        },
        Message::text(
            Role::User,
            "Salah, selalu pakai `--timeout 60` dan `deploy.sh --prod`.",
        ),
        Message::text(Role::Assistant, "Sudah disesuaikan."),
    ];

    let prov = Arc::new(MockCacheProvider {
        model: "primary-m".into(),
        response_text: json!({
            "actions": [
                {
                    "action": "patch_skill",
                    "skill": "user-deploy",
                    "old_snippet": "- Pakai `deploy.sh --staging`.",
                    "new_snippet": "- Pakai `deploy.sh --prod`.",
                    "reason": "ubah flag deploy"
                },
                {
                    "action": "patch_skill",
                    "skill": "imp-deploy",
                    "old_snippet": "- Pakai `--timeout 30`.",
                    "new_snippet": "- Pakai `--timeout 60`.",
                    "reason": "naikkan timeout"
                }
            ]
        })
        .to_string(),
        usage: Usage::default(),
        calls: Arc::new(AtomicUsize::new(0)),
        captured_systems: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let out = run_background_review(prov, &cfg, &store, &dir, &sid, &msgs, true)
        .await
        .unwrap()
        .expect("harus menerapkan patch improvement & men-staging patch user");

    // Skill pengguna WAJIB tetap 100% utuh dan masuk staging!
    assert_eq!(std::fs::read_to_string(&user_skill_path).unwrap(), user_orig);
    assert_eq!(out.staged_ids.len(), 1);

    // Skill improvement dipatch langsung & tersimpan di review_backups!
    assert_eq!(out.applied.len(), 1);
    assert_eq!(out.applied[0].target, "imp-deploy");
    assert_eq!(out.backup_ids.len(), 1);
    let patched = std::fs::read_to_string(&imp_skill_path).unwrap();
    assert!(patched.contains("--timeout 60"));

    // Putar balik (`/review undo`) mengembalikan isi sebelum patch secara persis!
    let undo_msg = rollback_direct_review_patch(&store, None).unwrap();
    assert!(undo_msg.contains("imp-deploy"));
    assert_eq!(std::fs::read_to_string(&imp_skill_path).unwrap(), imp_orig);

    match prev_data_dir {
        Some(v) => unsafe { std::env::set_var("SELENCODE_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("SELENCODE_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn closing_turn_has_zero_self_correction_reflection_and_keeps_deterministic_footer() {
    let dir = temp_dir("no-sc-loop");
    std::fs::write(
        dir.join("calc.js"),
        "exports.calc = (a, b) => a - b;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_calc.js"),
        "const { calc } = require('./calc.js');\nif (calc(10, 5) !== 15) { console.error('FAIL_CALC'); process.exit(1); }\n",
    )
    .unwrap();

    struct SingleCloseProvider {
        calls: Arc<AtomicUsize>,
        saw_gate_prompt: Arc<std::sync::Mutex<bool>>,
    }

    #[async_trait]
    impl Provider for SingleCloseProvider {
        fn model_name(&self) -> &str {
            "mock-single-close"
        }

        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            if msgs
                .iter()
                .any(|m| m.plain_text().contains("[Self-Correction Gate"))
            {
                *self.saw_gate_prompt.lock().unwrap() = true;
            }
            let step = self.calls.fetch_add(1, Ordering::SeqCst);
            if step == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "e1".into(),
                            name: "edit_file".into(),
                            args: json!({
                                "path": "calc.js",
                                "old": "a - b",
                                "new": "a * b"
                            }),
                        },
                        ToolCall {
                            id: "s1".into(),
                            name: "write_file".into(),
                            args: json!({
                                "path": "repro_tmp.js",
                                "content": "console.log('repro');\n"
                            }),
                        },
                        ToolCall {
                            id: "t1".into(),
                            name: "run_shell".into(),
                            args: json!({"command": "node test_calc.js"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                let text = "Saya sudah mengedit calc.js.".to_string();
                let _ = tx.send(StreamEvent::Token(text.clone())).await;
                Ok(AssistantReply {
                    text,
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let saw_gate = Arc::new(std::sync::Mutex::new(false));
    let prov: Arc<dyn Provider> = Arc::new(SingleCloseProvider {
        calls: calls.clone(),
        saw_gate_prompt: saw_gate.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true).with_self_correct(true);
    let (tx, mut rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let final_ans = run_turn_ui(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Perbaiki calc.js agar test_calc.js lulus.",
        0,
        tx,
    )
    .await
    .unwrap();

    // 1. Tepat 2 panggilan LLM (tool + jawaban penutup), 0 putaran refleksi tambahan!
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "tidak boleh ada panggilan LLM tambahan untuk loop refleksi self-correction"
    );
    assert!(!*saw_gate.lock().unwrap());

    // 2. Tidak ada satupun UiEvent::Info berisi "Self-correction ("!
    let mut saw_sc_event = false;
    while let Ok(ev) = rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            if s.contains("Self-correction (") {
                saw_sc_event = true;
            }
        }
    }
    assert!(
        !saw_sc_event,
        "event UI 'Self-correction (n/3)' tidak boleh pernah muncul lagi"
    );

    // 3. Catatan penutup deterministik tetap jujur melaporkan GAGAL + berkas sementara repro_tmp.js!
    assert!(
        final_ans.contains("[Status Verifikasi Akhir — GAGAL]")
            && final_ans.contains("node test_calc.js"),
        "catatan verifikasi deterministik wajib melampirkan status gagal jujur: {final_ans}"
    );
    assert!(
        final_ans.contains("repro_tmp.js"),
        "catatan deterministik wajib melaporkan berkas sementara repro_tmp.js yang masih tersisa: {final_ans}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
