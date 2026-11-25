//! Uji penerimaan otomatis SelenCode v0.5.1 — Background Self-Improvement Review:
//! 1. Model aux (`[aux] review_model` -> fallback `compression_model` -> lewati bila kosong).
//! 2. Skenario koreksi: patch skill + tambah memory baru + laporan 1 baris + biaya `usage` terpisah + cooldown.
//! 3. Skenario hapus/timpa: memory/skill lama TIDAK terhapus diam-diam; masuk staging -> approve/discard.
//! 4. Skenario kontrol: tanya-jawab trivial tanpa tool -> 0 panggilan model, 0 file diubah, 0 pesan.
//! 5. Keamanan & kegagalan diam: anggaran max_tokens, provider error, JSON rusak, anti-overwrite ref, & parser `/review`.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use selencode::agent::review::{
    apply_staged_proposal, check_review_eligibility, discard_staged_proposal,
    is_session_substantive, run_background_review, ReviewSkipReason,
};
use selencode::config::Config;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::repl::{parse_slash_command, SlashCommand};
use selencode::store::Store;
use selencode::telegram::parse_callback_review;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

struct MockReviewProvider {
    model: String,
    response_text: Result<String, String>,
    in_tokens: u64,
    out_tokens: u64,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for MockReviewProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.response_text {
            Ok(text) => Ok(AssistantReply {
                text: text.clone(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: self.in_tokens,
                    output_tokens: self.out_tokens,
                }),
            }),
            Err(e) => Err(anyhow!("{e}")),
        }
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v051-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_sample_skill(cwd: &std::path::Path) -> PathBuf {
    let skill_dir = cwd.join(".selencode").join("skills").join("ops").join("raw-downloader");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_md,
        "---\n\
         name: raw-downloader\n\
         description: Panduan unduh gambar RAW pakai rawdl\n\
         version: 1.0.0\n\
         tags: [rawdl, ops]\n\
         ---\n\
         # Raw Downloader\n\
         \n\
         1. Jalankan `rawdl sources` untuk cek sumber.\n\
         2. Simpan hasil unduhan ke folder `/tmp/raws`.\n\
         3. Periksa log bila gagal.\n",
    )
    .unwrap();
    skill_md
}

#[test]
fn aux_model_resolution_and_trivial_control_eligibility() {
    let dir = temp_dir("elig");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let sid = store.create_session(&dir.to_string_lossy(), "9router", "main-model", "uji").unwrap();

    let mut cfg = Config::empty();
    let substantive_msgs = vec![
        Message::text(Role::User, "Unduh raw pakai rawdl ke /tmp/raws"),
        Message::text(Role::Assistant, "Siap, saya arahkan ke /tmp/raws."),
        Message::text(Role::User, "Salah, folder yang benar adalah ~/data/raws, bukan /tmp/raws."),
        Message::text(Role::Assistant, "Dimengerti, sudah diarahkan ke ~/data/raws."),
    ];

    // 1. Kedua model aux kosong -> WAJIB dilewati (tidak diam-diam pakai model utama!).
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &substantive_msgs),
        Err(ReviewSkipReason::NoAuxModelConfigured)
    );

    // 2. compression_model terisi -> dipakai sebagai fallback review_model.
    cfg.aux.compression_model = Some("cheap-comp".into());
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &substantive_msgs),
        Ok("cheap-comp".to_string())
    );

    // 3. review_model eksplisit -> diprioritaskan di atas compression_model.
    cfg.aux.review_model = Some("cheap-review".into());
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &substantive_msgs),
        Ok("cheap-review".to_string())
    );

    // 4. Sesi tanya-jawab trivial tanpa tool -> TIDAK memicu review.
    let trivial_msgs = vec![
        Message::text(Role::User, "Halo, apa ibu kota Indonesia?"),
        Message::text(Role::Assistant, "Ibu kota Indonesia adalah Jakarta."),
    ];
    assert!(!is_session_substantive(&trivial_msgs, 5));
    assert_eq!(
        check_review_eligibility(&cfg, &store, &sid, &trivial_msgs),
        Err(ReviewSkipReason::TrivialSession)
    );

    // 5. Parser `/review` dan callback Telegram `rv:<id>:a|d`.
    assert_eq!(
        parse_slash_command("/review pending"),
        SlashCommand::Review(Some("pending".into()))
    );
    assert_eq!(
        parse_slash_command("/review approve 42"),
        SlashCommand::Review(Some("approve 42".into()))
    );
    assert_eq!(parse_callback_review("rv:12:a"), Some((12, true)));
    assert_eq!(parse_callback_review("rv:12:d"), Some((12, false)));
    assert_eq!(parse_callback_review("ap:12:y"), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scenario1_correction_patches_skill_records_separate_usage_and_enforces_cooldown() {
    let dir = temp_dir("sc1");
    let skill_md = write_sample_skill(&dir);
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "9router", "ag/gemini-3-flash", "Perbaiki folder rawdl")
        .unwrap();

    let mut cfg = Config::empty();
    cfg.aux.review_model = Some("aux/gemini-lite".into());
    cfg.review.cooldown_mins = 30;
    cfg.review.max_tokens = 6_000;

    let messages = vec![
        Message::text(Role::User, "Jalankan rawdl untuk unduh chapter terbaru."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "t1".into(),
                name: "skill_view".into(),
                args: serde_json::json!({"name": "raw-downloader"}),
            }],
        },
        Message::text(Role::Assistant, "Saya simpan hasil unduhan ke `/tmp/raws`."),
        Message::text(
            Role::User,
            "Salah, folder kanonik yang benar adalah `~/data/raws`, bukan `/tmp/raws`.",
        ),
        Message::text(Role::Assistant, "Sudah diperbaiki ke `~/data/raws` dan selesai."),
    ];

    let mock_json = serde_json::json!({
        "actions": [
            {
                "action": "patch_skill",
                "skill": "raw-downloader",
                "old_snippet": "2. Simpan hasil unduhan ke folder `/tmp/raws`.",
                "new_snippet": "2. Simpan hasil unduhan ke folder kanonik `~/data/raws`.",
                "reason": "tambah aturan folder kanonik"
            },
            {
                "action": "add_memory",
                "category": "conventions",
                "scope": "project",
                "content": "Folder kanonik unduhan rawdl adalah ~/data/raws (bukan /tmp/raws).",
                "reason": "aturan folder kanonik rawdl"
            }
        ]
    })
    .to_string();

    let calls = Arc::new(AtomicUsize::new(0));
    let prov = Arc::new(MockReviewProvider {
        model: "aux/gemini-lite".into(),
        response_text: Ok(mock_json),
        in_tokens: 920,
        out_tokens: 145,
        calls: calls.clone(),
    });

    let outcome = run_background_review(prov.clone(), &cfg, &store, &dir, &sid, &messages, false)
        .await
        .unwrap()
        .expect("skenario koreksi harus menghasilkan outcome review");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.applied.len(), 2);
    assert!(outcome.staged_ids.is_empty());

    // Skill benar-benar ter-patch dengan path baru & diff tersedia!
    let updated_skill = std::fs::read_to_string(&skill_md).unwrap();
    assert!(updated_skill.contains("~/data/raws"));
    assert!(!updated_skill.contains("/tmp/raws"));
    let diff = outcome.applied[0].diff.as_deref().unwrap_or("");
    assert!(diff.contains("-2. Simpan hasil unduhan ke folder `/tmp/raws`."));
    assert!(diff.contains("+2. Simpan hasil unduhan ke folder kanonik `~/data/raws`."));

    // Memory baru tersimpan!
    let mems = store.list_memories(&cwd_str, 10).unwrap();
    assert_eq!(mems.len(), 1);
    assert!(mems[0].content.contains("~/data/raws"));

    // Laporan tepat satu baris bergaya Hermes/SelenCode!
    let report = outcome.report_message.as_deref().unwrap();
    assert!(report.starts_with("💾 Self-improvement review:"));
    assert!(report.contains("Skill 'raw-downloader' patched (tambah aturan folder kanonik)"));
    assert!(!report.contains('\n'));

    // Laporan tertunda (mark_delivered=false) muncul sekali pada sesi CLI berikutnya!
    let undelivered = store.take_undelivered_review_reports(&cwd_str).unwrap();
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0], report);
    assert!(store.take_undelivered_review_reports(&cwd_str).unwrap().is_empty());

    // Biaya token review tercatat TERPISAH di tabel `usage` (`kind='review'`); jalur utama = (0, 0)!
    assert_eq!(store.total_usage(&sid).unwrap(), (0, 0));
    assert_eq!(store.review_usage_for_session(&sid).unwrap(), (920, 145, 1));

    // Cooldown 30 menit: panggilan kedua di sesi yang sama WAJIB dilewati (0 panggilan tambahan)!
    let second = run_background_review(prov, &cfg, &store, &dir, &sid, &messages, true)
        .await
        .unwrap();
    assert!(second.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scenario2_delete_and_overwrite_are_staged_until_approved_or_discarded() {
    let dir = temp_dir("sc2");
    let skill_md = write_sample_skill(&dir);
    let orig_skill = std::fs::read_to_string(&skill_md).unwrap();
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "9router", "ag/gemini-3-flash", "Audit memory usang")
        .unwrap();

    let m_old1 = store
        .add_memory_categorized(&cwd_str, "commands", "Gunakan server lama old.example.com untuk sync.")
        .unwrap();
    let m_old2 = store
        .add_memory_categorized(&cwd_str, "conventions", "Selalu jalankan script legacy-sync.sh.")
        .unwrap();

    let mut cfg = Config::empty();
    cfg.aux.compression_model = Some("aux/comp-mini".into());

    let messages = vec![
        Message::text(
            Role::User,
            "Catatan lama soal old.example.com dan legacy-sync.sh sudah usang, jangan dipakai lagi.",
        ),
        Message::text(Role::Assistant, "Baik, saya tidak akan menggunakan keduanya lagi."),
    ];

    let mock_json = serde_json::json!({
        "actions": [
            {
                "action": "delete_memory",
                "memory_id": m_old1,
                "reason": "server old.example.com sudah usang"
            },
            {
                "action": "delete_memory",
                "memory_id": m_old2,
                "reason": "legacy-sync.sh sudah tidak dipakai"
            },
            {
                "action": "overwrite_skill",
                "skill": "raw-downloader",
                "body": "# Raw Downloader v2\nHanya satu baris baru.",
                "reason": "tulis ulang seluruh panduan"
            }
        ]
    })
    .to_string();

    let prov = Arc::new(MockReviewProvider {
        model: "aux/comp-mini".into(),
        response_text: Ok(mock_json),
        in_tokens: 780,
        out_tokens: 130,
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let outcome = run_background_review(prov, &cfg, &store, &dir, &sid, &messages, true)
        .await
        .unwrap()
        .expect("harus menghasilkan proposal staging");

    assert!(outcome.applied.is_empty());
    assert_eq!(outcome.staged_ids.len(), 3);
    let report = outcome.report_message.as_deref().unwrap();
    assert!(report.contains("/review pending"));

    // DILARANG terhapus atau tertimpa tanpa persetujuan!
    assert!(store.get_memory(m_old1).unwrap().is_some());
    assert!(store.get_memory(m_old2).unwrap().is_some());
    assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), orig_skill);

    let pending = store
        .list_review_proposals(Some(&cwd_str), Some("pending"), 20)
        .unwrap();
    assert_eq!(pending.len(), 3);

    let p_del1 = pending
        .iter()
        .find(|p| p.kind == "delete_memory" && p.target == m_old1.to_string())
        .unwrap()
        .id;
    let p_del2 = pending
        .iter()
        .find(|p| p.kind == "delete_memory" && p.target == m_old2.to_string())
        .unwrap()
        .id;

    // `/review approve <id>` -> memory #1 BENAR-BENAR terhapus!
    let app_msg = apply_staged_proposal(&store, &dir, p_del1).unwrap();
    assert!(app_msg.contains("berhasil dihapus"));
    assert!(store.get_memory(m_old1).unwrap().is_none());

    // `/review discard <id>` -> memory #2 TETAP UTUH!
    let disc_msg = discard_staged_proposal(&store, p_del2).unwrap();
    assert!(disc_msg.contains("tetap utuh"));
    assert!(store.get_memory(m_old2).unwrap().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scenario3_control_silent_failures_budget_and_safe_reference_guard() {
    let dir = temp_dir("sc3");
    let _skill_md = write_sample_skill(&dir);
    let store = Store::open_at(&dir.join("selen.db")).unwrap();
    let cwd_str = dir.to_string_lossy().to_string();
    let sid = store
        .create_session(&cwd_str, "9router", "ag/gemini-3-flash", "Tanya jawab ringan")
        .unwrap();

    let mut cfg = Config::empty();
    cfg.aux.review_model = Some("aux/cheap".into());

    // 1. Skenario kontrol: trivial Q&A -> 0 panggilan model, 0 perubahan, 0 pesan.
    let trivial = vec![
        Message::text(Role::User, "Apa itu Rust?"),
        Message::text(Role::Assistant, "Rust adalah bahasa pemrograman sistem."),
    ];
    let calls = Arc::new(AtomicUsize::new(0));
    let prov = Arc::new(MockReviewProvider {
        model: "aux/cheap".into(),
        response_text: Ok(r#"{"actions":[]}"#.into()),
        in_tokens: 100,
        out_tokens: 10,
        calls: calls.clone(),
    });
    assert!(
        run_background_review(prov.clone(), &cfg, &store, &dir, &sid, &trivial, true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // 2. Sesi substantif (>=5 tool) tetapi tanpa temuan (`{"actions":[]}`) -> tanpa pesan (anti-spam)!
    let mut five_tools = vec![Message::text(Role::User, "Periksa file proyek.")];
    for i in 0..5 {
        five_tools.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: format!("c{i}"),
                name: "read_file".into(),
                args: serde_json::json!({"path": "README.md"}),
            }],
        });
    }
    assert!(
        run_background_review(prov, &cfg, &store, &dir, &sid, &five_tools, true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // 3. Kegagalan provider & JSON rusak -> diam (Ok(None)), tidak pernah panik/error ke sesi berikutnya!
    let sid2 = store.create_session(&cwd_str, "9router", "m", "err").unwrap();
    let err_prov = Arc::new(MockReviewProvider {
        model: "aux/cheap".into(),
        response_text: Err("HTTP 503 upstream down".into()),
        in_tokens: 0,
        out_tokens: 0,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    assert!(
        run_background_review(err_prov, &cfg, &store, &dir, &sid2, &five_tools, true)
            .await
            .unwrap()
            .is_none()
    );

    // 4. Tambah referensi BARU berhasil; mencoba menimpa referensi yang sudah ada -> masuk STAGING!
    let sid3 = store.create_session(&cwd_str, "9router", "m", "ref1").unwrap();
    let ref_json = serde_json::json!({
        "actions": [
            {
                "action": "add_skill_reference",
                "skill": "raw-downloader",
                "ref_path": "references/paths.md",
                "content": "# Path Kanonik\nSelalu pakai ~/data/raws.",
                "reason": "dokumentasi path kanonik"
            },
            {
                "action": "add_skill_reference",
                "skill": "raw-downloader",
                "ref_path": "../escape.md",
                "content": "hack",
                "reason": "harus ditolak"
            }
        ]
    })
    .to_string();
    let ref_prov = Arc::new(MockReviewProvider {
        model: "aux/cheap".into(),
        response_text: Ok(ref_json),
        in_tokens: 500,
        out_tokens: 80,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let out3 = run_background_review(ref_prov, &cfg, &store, &dir, &sid3, &five_tools, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out3.applied.len(), 1);
    assert!(dir
        .join(".selencode/skills/ops/raw-downloader/references/paths.md")
        .exists());
    assert!(!dir
        .join(".selencode/skills/ops/raw-downloader/escape.md")
        .exists());

    // Jalankan lagi pada sesi baru dengan isi berbeda ke file referensi YANG SUDAH ADA -> WAJIB STAGING!
    let sid4 = store.create_session(&cwd_str, "9router", "m", "ref2").unwrap();
    let ref_overwrite_json = serde_json::json!({
        "actions": [{
            "action": "add_skill_reference",
            "skill": "raw-downloader",
            "ref_path": "references/paths.md",
            "content": "# Path Baru\nMenimpa isi lama.",
            "reason": "coba timpa referensi lama"
        }]
    })
    .to_string();
    let ref_prov2 = Arc::new(MockReviewProvider {
        model: "aux/cheap".into(),
        response_text: Ok(ref_overwrite_json),
        in_tokens: 500,
        out_tokens: 60,
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let out4 = run_background_review(ref_prov2, &cfg, &store, &dir, &sid4, &five_tools, true)
        .await
        .unwrap()
        .unwrap();
    assert!(out4.applied.is_empty());
    assert_eq!(out4.staged_ids.len(), 1);
    let intact = std::fs::read_to_string(
        dir.join(".selencode/skills/ops/raw-downloader/references/paths.md"),
    )
    .unwrap();
    assert!(intact.contains("Selalu pakai ~/data/raws"));

    let _ = std::fs::remove_dir_all(&dir);
}
