//! Tes komprehensif SelenCode Skills System (Tahap 1–6, v0.4.0):
//! 1. Parsing frontmatter YAML (valid/rusak), kebab-case, prioritas proyek > pengguna, folder rusak tidak crash.
//! 2. Indeks dikelompokkan per kategori & deskripsi dipotong <= 60 karakter.
//! 3. Tool `skills_list` & `skill_view` (Tingkat 2 & 3) + penolakan path traversal & file rahasia.
//! 4. `skill_create` & `skill_edit` WAJIB lewat persetujuan pengguna + unified diff.
//! 5. Bukti DB (`usage` & `call_token_metrics`) dengan 10 skill: panggilan 1 hanya memuat indeks,
//!    sesudah `skill_view` hanya 1 skill itu yang masuk konteks.
//! 6. Skenario end-to-end: Sesi 1 buat skill lewat persetujuan -> keluar -> Sesi 2 melihat indeks,
//!    memuat skill dengan `skill_view`, dan menjalankan langkah skill hingga menghasilkan output sesuai playbook.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::run_turn;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::security::{ApprovalRequest, Approver};
use selencode::skills::{
    format_skills_index, is_valid_kebab_case, load_skill_reference, parse_skill_frontmatter,
    scan_skills_in_dirs, truncate_index_desc, SKILL_INDEX_DESC_MAX_CHARS,
};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selen-skills-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Gabungkan seluruh isi blok (`Text`, `ToolUse`, `ToolResult`) dari seluruh pesan yang dikirim ke provider.
fn flatten_all_blocks(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for m in messages {
        for b in &m.content {
            match b {
                Block::Text(t) => parts.push(t.clone()),
                Block::ToolUse { name, args, .. } => parts.push(format!("{name}({args})")),
                Block::ToolResult { content, .. } => parts.push(content.clone()),
                Block::Image { media_type, .. } => parts.push(format!("[image:{media_type}]")),
            }
        }
    }
    parts.join("\n---BLK---\n")
}

#[test]
fn tahap1_frontmatter_parsing_valid_invalid_and_priority_project_over_user() {
    // 1. Valid YAML frontmatter (inline tags)
    let valid = "\
---
name: deploy-app
description: Langkah deploy aplikasi produksi tanpa downtime
version: 1.2.0
tags: [deploy, prod, ci]
---

# Langkah Deploy
1. Jalankan tes.
";
    let fm = parse_skill_frontmatter(valid).unwrap();
    assert_eq!(fm.name, "deploy-app");
    assert_eq!(fm.description, "Langkah deploy aplikasi produksi tanpa downtime");
    assert_eq!(fm.version.as_deref(), Some("1.2.0"));
    assert_eq!(fm.tags, vec!["deploy", "prod", "ci"]);

    // 2. Valid YAML frontmatter (multiline list tags)
    let valid_ml = "\
---
name: audit-api-v2
description: \"Audit endpoint REST\"
tags:
  - api
  - security
---
Body.
";
    let fm2 = parse_skill_frontmatter(valid_ml).unwrap();
    assert_eq!(fm2.tags, vec!["api", "security"]);

    // 3. Invalid frontmatter cases
    assert!(!is_valid_kebab_case("BadName"));
    assert!(!is_valid_kebab_case("bad_name"));
    assert!(!is_valid_kebab_case("-leading"));
    assert!(parse_skill_frontmatter("# Tanpa frontmatter\nIsi").is_err());
    assert!(parse_skill_frontmatter("---\nname: BadName\ndescription: ok\n---\n").is_err());
    assert!(parse_skill_frontmatter("---\nname: ok-name\n---\n").is_err());
    assert!(parse_skill_frontmatter("---\nname: ok-name\ndescription: |\n  multi\n---\n").is_err());
    assert!(parse_skill_frontmatter("---\nname: unclosed\ndescription: x\n").is_err());

    // 4. Prioritas PROYEK > PENGGUNA + folder rusak/duplikat dilewati tanpa crash!
    let base = temp_dir("prio");
    let proj_root = base.join("proj_skills");
    let user_root = base.join("user_skills");

    // Skill sama `sama-nama` ada di proyek & pengguna -> versi proyek WAJIB menang!
    std::fs::create_dir_all(proj_root.join("ops").join("sama-nama")).unwrap();
    std::fs::write(
        proj_root.join("ops").join("sama-nama").join("SKILL.md"),
        "---\nname: sama-nama\ndescription: Versi PROYEK yang harus menang\n---\nIsi proyek\n",
    )
    .unwrap();
    std::fs::create_dir_all(user_root.join("ops").join("sama-nama")).unwrap();
    std::fs::write(
        user_root.join("ops").join("sama-nama").join("SKILL.md"),
        "---\nname: sama-nama\ndescription: Versi PENGGUNA yang kalah\n---\nIsi pengguna\n",
    )
    .unwrap();

    // Skill unik pengguna tetap termuat
    std::fs::create_dir_all(user_root.join("dev").join("user-only")).unwrap();
    std::fs::write(
        user_root.join("dev").join("user-only").join("SKILL.md"),
        "---\nname: user-only\ndescription: Hanya ada di folder pengguna\n---\nIsi user-only\n",
    )
    .unwrap();

    // Skill rusak di proyek -> dilewati dengan warning, TIDAK panik/crash!
    std::fs::create_dir_all(proj_root.join("rusak")).unwrap();
    std::fs::write(
        proj_root.join("rusak").join("SKILL.md"),
        "---\nname: TIDAK_KEBAB\ndescription: rusak\n---\n",
    )
    .unwrap();

    // Duplikat nama di dalam scope yang sama -> kedua dilewati dengan warning!
    std::fs::create_dir_all(proj_root.join("dup-a")).unwrap();
    std::fs::write(
        proj_root.join("dup-a").join("SKILL.md"),
        "---\nname: kembar\ndescription: Pertama\n---\nA\n",
    )
    .unwrap();
    std::fs::create_dir_all(proj_root.join("dup-b")).unwrap();
    std::fs::write(
        proj_root.join("dup-b").join("SKILL.md"),
        "---\nname: kembar\ndescription: Kedua\n---\nB\n",
    )
    .unwrap();

    let scan = scan_skills_in_dirs(&proj_root, Some(&user_root));
    assert_eq!(scan.skills.len(), 3, "harus ada 3 skill valid: {:?}", scan.skills);
    assert!(
        scan.warnings.len() >= 2,
        "harus mencatat warning skill rusak & duplikat: {:?}",
        scan.warnings
    );

    let winner = scan.skills.iter().find(|s| s.name == "sama-nama").unwrap();
    assert_eq!(winner.scope, "project");
    assert_eq!(winner.summary, "Versi PROYEK yang harus menang");
    assert_eq!(winner.category, "ops");

    let user_s = scan.skills.iter().find(|s| s.name == "user-only").unwrap();
    assert_eq!(user_s.scope, "user");
    assert_eq!(user_s.category, "dev");

    // 5. Pastikan 2 skill bawaan di repo (`.selencode/skills/dev/{rilis-github,tinjau-rust}`) valid!
    let repo_skills = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".selencode")
        .join("skills");
    let repo_scan = scan_skills_in_dirs(&repo_skills, None);
    assert!(repo_scan.skills.iter().any(|s| s.name == "rilis-github"));
    assert!(repo_scan.skills.iter().any(|s| s.name == "tinjau-rust"));
    assert!(
        repo_scan.warnings.is_empty(),
        "skill bawaan repo tidak boleh punya warning: {:?}",
        repo_scan.warnings
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn tahap2_index_truncated_60_chars_and_grouped_by_category() {
    let base = temp_dir("idx60");
    let proj_root = base.join("skills");
    std::fs::create_dir_all(proj_root.join("deploy").join("panjang-banget")).unwrap();
    let long_desc = "Deskripsi sangat panjang sekali melebihi enam puluh karakter untuk menguji pemotongan indeks ketat";
    std::fs::write(
        proj_root.join("deploy").join("panjang-banget").join("SKILL.md"),
        format!("---\nname: panjang-banget\ndescription: {long_desc}\n---\nIsi panjang.\n"),
    )
    .unwrap();

    let scan = scan_skills_in_dirs(&proj_root, None);
    let idx = format_skills_index(&scan.skills);
    assert!(idx.contains("[deploy]"), "{idx}");
    let line = idx
        .lines()
        .find(|l| l.starts_with("- panjang-banget"))
        .expect("baris skill ada");
    let after_colon = line.split(": ").nth(1).unwrap();
    assert!(
        after_colon.chars().count() <= SKILL_INDEX_DESC_MAX_CHARS,
        "deskripsi indeks melebihi 60 char ({}): {after_colon}",
        after_colon.chars().count()
    );
    assert_eq!(
        truncate_index_desc(long_desc, 60).chars().count(),
        60
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn tahap3_and_tahap4_skill_view_security_and_approval_gated_create_edit() {
    let cwd = temp_dir("sec-appr");
    // Buat 1 skill dengan file referensi sah, file skrip, & file rahasia `.env` di dalamnya.
    let sk_dir = cwd.join(".selencode").join("skills").join("dev").join("uji-aman");
    std::fs::create_dir_all(sk_dir.join("references")).unwrap();
    std::fs::create_dir_all(sk_dir.join("scripts")).unwrap();
    std::fs::write(
        sk_dir.join("SKILL.md"),
        "---\nname: uji-aman\ndescription: Skill uji keamanan referensi\n---\n# Uji Aman\nIsi utama.\n",
    )
    .unwrap();
    std::fs::write(sk_dir.join("references").join("panduan.md"), "Panduan referensi sah.").unwrap();
    std::fs::write(sk_dir.join("scripts").join("cek.sh"), "#!/bin/sh\necho halo\n").unwrap();
    std::fs::write(sk_dir.join(".env"), "API_KEY=rahasia_super_12345\n").unwrap();
    std::fs::write(cwd.join("luar.txt"), "file di luar folder skill").unwrap();

    // Tingkat 3 sah: baca references/panduan.md
    let (_, ref_body) = load_skill_reference(&cwd, "uji-aman", "references/panduan.md").unwrap();
    assert!(ref_body.contains("Panduan referensi sah."));

    // Path traversal ditolak!
    assert!(load_skill_reference(&cwd, "uji-aman", "../../../luar.txt").is_err());
    assert!(load_skill_reference(&cwd, "uji-aman", "references/../../../../luar.txt").is_err());
    let abs_path = cwd.join("luar.txt").display().to_string();
    assert!(load_skill_reference(&cwd, "uji-aman", &abs_path).is_err());

    // File rahasia `.env` ditolak!
    assert!(load_skill_reference(&cwd, "uji-aman", ".env").is_err());

    // Tingkat 2 lewat tool `skill_view`: tampilkan isi SKILL.md & daftar nama referensi (bukan isi referensi).
    let reg = default_registry();
    let view_tool = reg.get("skill_view").expect("skill_view terdaftar");
    let ctx_yes = Ctx::new(cwd.clone(), true);
    let out_l2 = view_tool
        .run(json!({"name": "uji-aman"}), &ctx_yes)
        .await
        .unwrap();
    assert!(out_l2.contains("Isi utama."));
    assert!(out_l2.contains("references/panduan.md"));
    assert!(!out_l2.contains("Panduan referensi sah."), "Tingkat 2 tidak boleh langsung muat isi referensi!");

    // Tahap 4: `skill_create` WAJIB lewat persetujuan pengguna!
    let create_tool = reg.get("skill_create").expect("skill_create terdaftar");
    assert!(create_tool.needs_approval(&json!({})));

    // 4a. Jika user MENOLAK (`false`) -> file TIDAK boleh tertulis!
    let (tx_deny, mut rx_deny) = mpsc::channel::<ApprovalRequest>(4);
    let ctx_deny = Ctx::new(cwd.clone(), false).with_approver(Arc::new(Approver::channel(false, tx_deny)));
    let h_deny = tokio::spawn(async move {
        let req = rx_deny.recv().await.expect("popup approval harus muncul");
        assert!(req.title.contains("skill_create"));
        assert!(req.detail.contains("+name: ditolak-skill"));
        let _ = req.reply.send(false);
    });
    let res_deny = create_tool
        .run(
            json!({
                "name": "ditolak-skill",
                "description": "Skill yang ditolak user",
                "body": "Jangan tulis ini"
            }),
            &ctx_deny,
        )
        .await
        .unwrap();
    h_deny.await.unwrap();
    assert!(res_deny.contains("Ditolak user"));
    assert!(!cwd.join(".selencode/skills/ditolak-skill/SKILL.md").exists());

    // 4b. Jika user MENYETUJUI (`true`) -> tertulis + redaksi rahasia aktif!
    let (tx_ok, mut rx_ok) = mpsc::channel::<ApprovalRequest>(4);
    let ctx_ok = Ctx::new(cwd.clone(), false).with_approver(Arc::new(Approver::channel(false, tx_ok)));
    let h_ok = tokio::spawn(async move {
        let req = rx_ok.recv().await.expect("popup approval harus muncul");
        assert!(req.detail.contains("[REDACTED]"), "diff approval harus sudah teredaksi");
        let _ = req.reply.send(true);
    });
    let res_ok = create_tool
        .run(
            json!({
                "name": "disetujui-skill",
                "description": "Skill disetujui user",
                "body": "Langkah 1 (API_KEY=rahasia12345678)"
            }),
            &ctx_ok,
        )
        .await
        .unwrap();
    h_ok.await.unwrap();
    assert!(res_ok.contains("berhasil dibuat"));
    let written = std::fs::read_to_string(cwd.join(".selencode/skills/disetujui-skill/SKILL.md")).unwrap();
    assert!(written.contains("name: disetujui-skill"));
    assert!(!written.contains("rahasia12345678"));

    // 4c. `skill_edit` juga WAJIB lewat persetujuan + diff!
    let edit_tool = reg.get("skill_edit").expect("skill_edit terdaftar");
    assert!(edit_tool.needs_approval(&json!({})));
    let (tx_ed, mut rx_ed) = mpsc::channel::<ApprovalRequest>(4);
    let ctx_ed = Ctx::new(cwd.clone(), false).with_approver(Arc::new(Approver::channel(false, tx_ed)));
    let h_ed = tokio::spawn(async move {
        let req = rx_ed.recv().await.expect("popup approval edit harus muncul");
        assert!(req.title.contains("skill_edit"));
        assert!(req.detail.contains("-description: Skill disetujui user"));
        assert!(req.detail.contains("+description: Deskripsi baru v2"));
        let _ = req.reply.send(true);
    });
    let res_ed = edit_tool
        .run(
            json!({
                "name": "disetujui-skill",
                "description": "Deskripsi baru v2",
                "version": "1.1.0"
            }),
            &ctx_ed,
        )
        .await
        .unwrap();
    h_ed.await.unwrap();
    assert!(res_ed.contains("berhasil diperbarui"));

    let _ = std::fs::remove_dir_all(&cwd);
}

#[tokio::test]
async fn tahap6_db_token_proof_10_skills_lazy_load_and_single_skill_view() {
    let cwd = temp_dir("ten-skills");
    let skills_dir = cwd.join(".selencode").join("skills").join("cat");
    for i in 1..=10 {
        let name = format!("skill-{i:02}");
        let d = skills_dir.join(&name);
        std::fs::create_dir_all(&d).unwrap();
        let body = format!(
            "---\nname: {name}\ndescription: Playbook singkat nomor {i:02}\n---\n\
             MARKER_ISI_PENUH_{i:02}\n{}\n",
            "X".repeat(3_500)
        );
        std::fs::write(d.join("SKILL.md"), body).unwrap();
    }

    struct TenSkillsMock {
        calls: AtomicUsize,
        snapshots: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Provider for TenSkillsMock {
        fn model_name(&self) -> &str {
            "mock-10-skills"
        }
        async fn stream_chat(
            &self,
            messages: &[Message],
            tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let joined = flatten_all_blocks(messages);
            self.snapshots.lock().unwrap().push(joined.clone());

            match n {
                0 => {
                    // Panggilan 1: `skill_view` HARUS tersedia dan HANYA indeks yang masuk (0 isi penuh!).
                    assert!(
                        tools.iter().any(|t| t.name == "skill_view"),
                        "skill_view harus aktif saat ada skill terpasang"
                    );
                    assert!(joined.contains("skill-03"));
                    for i in 1..=10 {
                        let marker = format!("MARKER_ISI_PENUH_{i:02}");
                        assert!(
                            !joined.contains(&marker),
                            "panggilan 1 TIDAK boleh memuat isi penuh {marker}!"
                        );
                    }
                    Ok(AssistantReply {
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "sv1".into(),
                            name: "skill_view".into(),
                            args: json!({"name": "skill-03"}),
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: Some(Usage {
                            input_tokens: 1_200,
                            output_tokens: 25,
                            ..Default::default()
                        }),
                    })
                }
                _ => {
                    // Panggilan 2: HANYA `skill-03` yang masuk konteks; 9 skill lainnya tetap tidak termuat!
                    assert!(joined.contains("MARKER_ISI_PENUH_03"));
                    for i in 1..=10 {
                        if i == 3 {
                            continue;
                        }
                        let marker = format!("MARKER_ISI_PENUH_{i:02}");
                        assert!(
                            !joined.contains(&marker),
                            "skill lain ({marker}) tidak boleh ikut termuat!"
                        );
                    }
                    let _ = tx.send(StreamEvent::Token("Selesai pakai skill-03.".into())).await;
                    Ok(AssistantReply {
                        text: "Selesai pakai skill-03.".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 2_350,
                            output_tokens: 20,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let db = cwd.join("selen.db");
    let store = Arc::new(Store::open_at(&db).unwrap());
    let sid = store
        .create_session(&cwd.display().to_string(), "mock", "mock-10-skills", "ten")
        .unwrap();
    let ctx = Ctx::new(cwd.clone(), true).with_store(store.clone(), sid.clone());
    let reg = default_registry();
    let mock = Arc::new(TenSkillsMock {
        calls: AtomicUsize::new(0),
        snapshots: Mutex::new(Vec::new()),
    });
    let mut messages = Vec::new();
    let out = run_turn(mock.clone(), &reg, &ctx, &mut messages, "jalankan tugas nomor 03", 6)
        .await
        .unwrap();
    assert_eq!(out, "Selesai pakai skill-03.");

    // Verifikasi bukti DB `call_token_metrics` + `usage`: 2 panggilan tercatat persis!
    let rep = store.session_token_metrics(&sid).unwrap();
    assert_eq!(rep.calls, 2);
    assert_eq!(rep.total_actual_input, 1_200 + 2_350);
    // Pastikan di panggilan pertama `est_tool_results_tokens` = 0, lalu naik hanya di panggilan 2 setelah `skill_view`.
    let snaps = mock.snapshots.lock().unwrap();
    assert_eq!(snaps.len(), 2);
    assert!(snaps[1].len() > snaps[0].len() + 3_000);
    assert!(snaps[1].len() < snaps[0].len() + 5_000, "hanya 1 skill (bukan 10) yang dimuat!");

    let _ = std::fs::remove_dir_all(&cwd);
}

#[tokio::test]
async fn tahap6_end_to_end_create_skill_with_approval_then_new_session_follows_it() {
    let cwd = temp_dir("e2e-skill");
    let reg = default_registry();

    // --- SESI 1: Buat skill baru `format-laporan-kpi` lewat `skill_create` + persetujuan ---
    struct Session1Mock {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for Session1Mock {
        fn model_name(&self) -> &str {
            "mock-s1"
        }
        async fn stream_chat(
            &self,
            _messages: &[Message],
            tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert!(tools.iter().any(|t| t.name == "skill_create"));
                    Ok(AssistantReply {
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "sc1".into(),
                            name: "skill_create".into(),
                            args: json!({
                                "name": "format-laporan-kpi",
                                "category": "ops",
                                "description": "Format baku laporan KPI bulanan tim",
                                "body": "Setiap laporan KPI WAJIB diawali baris persis `=== LAPORAN KPI RESMI ===` lalu cantumkan status `VERIFIED-BY-SKILL`."
                            }),
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: Some(Usage { input_tokens: 1100, output_tokens: 40, ..Default::default() }),
                    })
                }
                _ => {
                    let _ = tx.send(StreamEvent::Token("Skill format-laporan-kpi disimpan.".into())).await;
                    Ok(AssistantReply {
                        text: "Skill format-laporan-kpi disimpan.".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage { input_tokens: 1200, output_tokens: 15, ..Default::default() }),
                    })
                }
            }
        }
    }

    let (tx_appr, mut rx_appr) = mpsc::channel::<ApprovalRequest>(4);
    let appr_task = tokio::spawn(async move {
        let req = rx_appr.recv().await.expect("skill_create wajib minta persetujuan");
        assert!(req.title.contains("skill_create"));
        let _ = req.reply.send(true);
    });
    let ctx1 = Ctx::new(cwd.clone(), false).with_approver(Arc::new(Approver::channel(false, tx_appr)));
    let mut msgs1 = Vec::new();
    let out1 = run_turn(
        Arc::new(Session1Mock { calls: AtomicUsize::new(0) }),
        &reg,
        &ctx1,
        &mut msgs1,
        "buat skill baru bernama format-laporan-kpi",
        5,
    )
    .await
    .unwrap();
    appr_task.await.unwrap();
    assert!(out1.contains("format-laporan-kpi"));
    assert!(cwd.join(".selencode/skills/ops/format-laporan-kpi/SKILL.md").is_file());

    // --- SESI 2 (keluar & mulai sesi BARU, messages kosong): indeks memuat skill baru -> model memakainya! ---
    struct Session2Mock {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for Session2Mock {
        fn model_name(&self) -> &str {
            "mock-s2"
        }
        async fn stream_chat(
            &self,
            messages: &[Message],
            tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let joined = flatten_all_blocks(messages);
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    // Indeks sesi BARU memuat `format-laporan-kpi`, tetapi isi penuh belum masuk!
                    assert!(joined.contains("format-laporan-kpi"));
                    assert!(!joined.contains("VERIFIED-BY-SKILL"));
                    assert!(tools.iter().any(|t| t.name == "skill_view"));
                    Ok(AssistantReply {
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "sv2".into(),
                            name: "skill_view".into(),
                            args: json!({"name": "format-laporan-kpi"}),
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: Some(Usage { input_tokens: 1050, output_tokens: 20, ..Default::default() }),
                    })
                }
                1 => {
                    // Setelah skill_view, isi playbook masuk -> model menulis file mengikuti langkah skill!
                    assert!(joined.contains("=== LAPORAN KPI RESMI ==="));
                    assert!(joined.contains("VERIFIED-BY-SKILL"));
                    Ok(AssistantReply {
                        text: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "wf2".into(),
                            name: "write_file".into(),
                            args: json!({
                                "path": "laporan_kpi.txt",
                                "content": "=== LAPORAN KPI RESMI ===\nStatus: VERIFIED-BY-SKILL\n"
                            }),
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: Some(Usage { input_tokens: 1300, output_tokens: 35, ..Default::default() }),
                    })
                }
                _ => {
                    let _ = tx.send(StreamEvent::Token("Laporan KPI dibuat sesuai skill.".into())).await;
                    Ok(AssistantReply {
                        text: "Laporan KPI dibuat sesuai skill.".into(),
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage { input_tokens: 1350, output_tokens: 15, ..Default::default() }),
                    })
                }
            }
        }
    }

    let ctx2 = Ctx::new(cwd.clone(), true);
    let mut msgs2 = Vec::new();
    let out2 = run_turn(
        Arc::new(Session2Mock { calls: AtomicUsize::new(0) }),
        &reg,
        &ctx2,
        &mut msgs2,
        "buatkan laporan_kpi.txt bulanan",
        6,
    )
    .await
    .unwrap();
    assert!(out2.contains("Laporan KPI"));
    let kpi_file = std::fs::read_to_string(cwd.join("laporan_kpi.txt")).unwrap();
    assert!(kpi_file.contains("=== LAPORAN KPI RESMI ==="));
    assert!(kpi_file.contains("VERIFIED-BY-SKILL"));

    let _ = std::fs::remove_dir_all(&cwd);
}
