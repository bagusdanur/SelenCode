//! Suite uji penerimaan terpadu SelenCode v0.5.9 — Tiga Utang dari Uji Penerimaan v0.5.7:
//! 1. Perbaikan 1: Biaya `skill_view` terkendali (salinan kerja ringkas untuk skill > 4.500 char,
//!    prune lebih dini ke stub + arsip byte-exact di sesi panjang, serta reload utuh `full=true`).
//! 2. Perbaikan 2: Pantauan `watch_job` hidup di CLI murni tanpa gateway (1 implementasi bersama,
//!    laporan selesai/gagal persis 1x, anti-dobel saat CLI ↔ gateway bergantian, kontrol sunyi 0 token).
//! 3. Perbaikan 3: Satu store memori persisten (pensiunkan `manage_memory` dari daftar tool aktif,
//!    migrasi otomatis entri SQLite lama ke `memory.md` dengan pagu keras tanpa potong diam-diam).

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use selencode::agent::context::{
    build_system_prompt, prune_old_skill_results_with_archive,
};
use selencode::agent::run_turn_counted;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use selencode::tools::skill::{build_skill_working_copy, SkillView, SKILL_WORKING_COPY_MAX_CHARS};
use selencode::tools::toolsets::EXTENDED_TOOLSETS;
use selencode::tools::watch::WatchJob;
use selencode::tools::{default_registry, Ctx, Tool};
use selencode::user_memory::{
    add_entry_in, load_entries_from, load_frozen_snapshot_from, migrate_legacy_sqlite_memories_in,
    MemoryStoreKind,
};
use selencode::watch::{
    evaluate_and_poll_watch, persist_watch_transition, poll_active_watches_for_cli,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v059-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// =========================================================================
// PERBAIKAN 1: Biaya `skill_view` terkendali (working copy + early archive stub + reload)
// =========================================================================

#[tokio::test]
async fn p1_large_skill_view_returns_compact_working_copy_and_full_reload() {
    let dir = temp_dir("p1-workcopy");
    let raw_dbg = include_str!("../assets/builtin_skills/dev/systematic-debugging/SKILL.md");
    assert!(
        raw_dbg.chars().count() > 10_000,
        "skill bawaan systematic-debugging memang besar (>10k char)"
    );

    // Salinan kerja ringkas memangkas >80% karakter namun mempertahankan Iron Law + 4 fase inti!
    let wc = build_skill_working_copy("systematic-debugging", raw_dbg);
    assert!(
        wc.chars().count() <= SKILL_WORKING_COPY_MAX_CHARS + 300,
        "salinan kerja terlalu panjang: {} char",
        wc.chars().count()
    );
    assert!(wc.contains("NO FIXES WITHOUT ROOT CAUSE INVESTIGATION FIRST"));
    assert!(wc.contains("Phase 1: Root Cause Investigation"));
    assert!(wc.contains("Phase 2: Pattern Analysis"));
    assert!(wc.contains("Phase 3: Hypothesis and Testing"));
    assert!(wc.contains("Phase 4: Implementation"));
    assert!(wc.contains("full=true"));

    // Uji langsung lewat tool `skill_view` (proyek skill besar)
    let skill_dir = dir.join(".selencode").join("skills").join("dev").join("systematic-debugging");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), raw_dbg).unwrap();

    let ctx = Ctx::new(dir.clone(), true);
    let compact_out = SkillView
        .run(json!({"name": "systematic-debugging"}), &ctx)
        .await
        .unwrap();
    assert!(
        compact_out.chars().count() < 3_200,
        "output default skill_view harus ringkas (<3200 char), dapat {}",
        compact_out.chars().count()
    );
    assert!(compact_out.contains("Phase 1: Root Cause Investigation"));

    // Muat ulang penuh dengan `full: true` mengembalikan seluruh isi (> 10.000 char)
    let full_out = SkillView
        .run(json!({"name": "systematic-debugging", "full": true}), &ctx)
        .await
        .unwrap();
    assert!(
        full_out.chars().count() > 10_000,
        "skill_view(full=true) wajib mengembalikan isi penuh, dapat {}",
        full_out.chars().count()
    );
    assert!(full_out.contains("Common Rationalizations"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn p1_long_session_prunes_skill_result_to_stub_archives_byte_exact_and_allows_reload() {
    let dir = temp_dir("p1-long-session");
    let raw_dbg = include_str!("../assets/builtin_skills/dev/systematic-debugging/SKILL.md");
    let skill_dir = dir.join(".selencode").join("skills").join("dev").join("systematic-debugging");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), raw_dbg).unwrap();
    std::fs::write(dir.join("a.txt"), "baris A\n").unwrap();
    std::fs::write(dir.join("b.txt"), "baris B\n").unwrap();
    std::fs::write(dir.join("c.txt"), "baris C\n").unwrap();

    // Unit test langsung `prune_old_skill_results_with_archive` (termasuk arsip byte-exact & idempoten).
    let archive_dir = dir.join("arch");
    let mut unit_msgs = vec![
        Message::text(Role::System, "sys"),
        Message::text(Role::User, "debug bug"),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "sv-1".into(),
                name: "skill_view".into(),
                args: json!({"name": "systematic-debugging"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "sv-1".into(),
                content: raw_dbg.to_string(),
                is_error: false,
            }],
        },
        Message::text(Role::Assistant, "step 2"),
        Message::text(Role::User, "lanjut"),
        Message::text(Role::Assistant, "step 3"),
        Message::text(Role::User, "lanjut lagi"),
    ];
    let n1 = prune_old_skill_results_with_archive(&mut unit_msgs, &archive_dir, 4);
    assert_eq!(n1, 1);
    let stub_text = match &unit_msgs[3].content[0] {
        Block::ToolResult { content, .. } => content.clone(),
        _ => panic!("expected ToolResult"),
    };
    assert!(stub_text.contains("[Skill `systematic-debugging` aktif — isi penuh terarsip:"));
    assert!(stub_text.contains("Bagian:"));
    assert!(stub_text.contains("Phase 1: Root Cause Investigation"));
    assert!(stub_text.chars().count() < 550, "stub harus ringkas: {stub_text}");
    // Idempoten: panggilan kedua mengembalikan 0
    assert_eq!(prune_old_skill_results_with_archive(&mut unit_msgs, &archive_dir, 4), 0);
    // Isi asli diarsipkan byte-exact di disk!
    let archived_files: Vec<_> = std::fs::read_dir(&archive_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(archived_files.len(), 1);
    assert_eq!(std::fs::read_to_string(&archived_files[0]).unwrap(), raw_dbg);

    // Integrasi sesi panjang via `run_turn_counted`:
    // Call 1: panggil skill_view(systematic-debugging)
    // Call 2: read_file(a.txt) — skill masih terlihat utuh (working copy)
    // Call 3: read_file(b.txt) — skill masih terlihat / mulai mendekati ekor
    // Call 4: read_file(c.txt) — hasil skill_view pertama WAJIB sudah jadi stub arsip!
    // Call 5: model memuat ulang skill_view(systematic-debugging, full=true) -> isi penuh masuk lagi!
    // Call 6: selesai.
    struct LongSkillProv {
        step: AtomicUsize,
        seen_skill_results_per_call: Arc<Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait]
    impl Provider for LongSkillProv {
        fn model_name(&self) -> &str {
            "mock-long-skill"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let mut sv_contents = Vec::new();
            for m in msgs {
                if m.role == Role::Tool {
                    for b in &m.content {
                        if let Block::ToolResult { id, content, .. } = b {
                            if id.starts_with("sv") {
                                sv_contents.push(content.clone());
                            }
                        }
                    }
                }
            }
            self.seen_skill_results_per_call.lock().unwrap().push(sv_contents);
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let call = match n {
                0 => Some(ToolCall {
                    id: "sv1".into(),
                    name: "skill_view".into(),
                    args: json!({"name": "systematic-debugging"}),
                }),
                1 => Some(ToolCall {
                    id: "r1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "a.txt"}),
                }),
                2 => Some(ToolCall {
                    id: "r2".into(),
                    name: "read_file".into(),
                    args: json!({"path": "b.txt"}),
                }),
                3 => Some(ToolCall {
                    id: "r3".into(),
                    name: "read_file".into(),
                    args: json!({"path": "c.txt"}),
                }),
                4 => Some(ToolCall {
                    id: "sv2".into(),
                    name: "skill_view".into(),
                    args: json!({"name": "systematic-debugging", "full": true}),
                }),
                _ => None,
            };
            if let Some(tc) = call {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![tc],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 500, output_tokens: 30, ..Default::default() }),
                })
            } else {
                Ok(AssistantReply {
                    text: "Debug selesai dengan skill yang di-stub lalu dimuat ulang.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage { input_tokens: 600, output_tokens: 40, ..Default::default() }),
                })
            }
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let prov: Arc<dyn Provider> = Arc::new(LongSkillProv {
        step: AtomicUsize::new(0),
        seen_skill_results_per_call: seen.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true);
    let mut msgs = Vec::new();
    let (ans, _) = run_turn_counted(prov, &reg, &ctx, &mut msgs, "perbaiki bug di a.txt", 10)
        .await
        .unwrap();
    assert!(ans.contains("Debug selesai"));

    let history = seen.lock().unwrap();
    // Call 2 (indeks 1): sv1 baru masuk -> salinan kerja ringkas (< 3200 char, belum stub)
    assert_eq!(history[1].len(), 1);
    assert!(history[1][0].contains("Phase 1: Root Cause Investigation"));
    assert!(!history[1][0].contains("isi penuh terarsip:"));
    // Call 4 (indeks 3): sesudah 2 langkah tool (r1, r2), sv1 WAJIB sudah jadi stub arsip ringkas!
    assert_eq!(history[3].len(), 1);
    assert!(
        history[3][0].contains("isi penuh terarsip:"),
        "sv1 pada call 4 wajib sudah berupa stub arsip, dapat: {}",
        history[3][0]
    );
    assert!(history[3][0].chars().count() < 550);
    // Call 6 (indeks 5): sesudah reload `skill_view(full=true)`, sv2 masuk penuh (> 10.000 char)!
    assert_eq!(history[5].len(), 2);
    assert!(history[5][0].contains("isi penuh terarsip:"));
    assert!(
        history[5][1].chars().count() > 10_000,
        "reload skill_view(full=true) harus mengembalikan isi penuh, dapat {}",
        history[5][1].chars().count()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Polling `watch_job` hidup di CLI murni + anti-dobel CLI ↔ gateway
// =========================================================================

#[tokio::test]
async fn p2_cli_watch_polling_completion_failure_gateway_interleave_and_quiet_control() {
    let dir = temp_dir("p2-cli-watch");
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());
    let sandbox = SandboxController::default();

    // 0. Kontrol: tanpa pantauan aktif -> pembukaan sesi CLI sunyi (0 pesan, 0 token).
    let quiet = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert!(quiet.is_empty(), "tanpa watch aktif harus sunyi total");

    // 1. Kasus SELESAI di CLI murni (tanpa gateway berjalan sama sekali):
    let status_file = dir.join("job_done.txt");
    std::fs::write(&status_file, "STATUS=PROCESSING\nSTAGE=S1\nPROGRESS=1/3\n").unwrap();
    let ctx = Ctx::new(dir.clone(), true).with_store(store.clone(), "cli-sess-1".into());
    let reg_msg = WatchJob
        .run(
            json!({
                "label": "Build CLI Murni",
                "status_file": "job_done.txt",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(reg_msg.contains("CLI/TUI"));

    // Penutupan sesi 1: 1 putaran poll mencatat tahap awal S1
    let close1 = poll_active_watches_for_cli(&store, &sandbox, &dir, true, false).await;
    assert_eq!(close1.len(), 1);
    assert!(close1[0].contains("S1"));

    // Job selesai saat tidak ada sesi terbuka!
    std::fs::write(
        &status_file,
        "STATUS=SELESAI\nSUMMARY=semua 10 bab rampung\nOUTPUT=out/cli-hasil.zip\n",
    )
    .unwrap();

    // Buka sesi CLI 2 di folder yang sama -> ringkasan selesai tercetak PERSIS SATU KALI!
    let open2 = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert_eq!(open2.len(), 1);
    assert!(open2[0].contains("selesai"));
    assert!(open2[0].contains("out/cli-hasil.zip"));

    // Buka sesi CLI 3 di folder yang sama -> TIDAK ada laporan ulang!
    let open3 = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert!(open3.is_empty(), "sesi berikut tidak boleh mengulang laporan selesai");

    // 2. Kasus GAGAL di tengah -> dilaporkan 1x saat sesi berikut dibuka & pantauan berhenti!
    let fail_file = dir.join("job_fail.txt");
    std::fs::write(&fail_file, "STATUS=PROCESSING\nSTAGE=RENDER\n").unwrap();
    WatchJob
        .run(
            json!({
                "label": "Render Gagal",
                "status_file": "job_fail.txt",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();
    std::fs::write(&fail_file, "STATUS=FAILED\nERROR=out of memory di halaman 7\n").unwrap();
    let fail_open = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert_eq!(fail_open.len(), 1);
    assert!(fail_open[0].contains("gagal"));
    assert!(fail_open[0].contains("out of memory di halaman 7"));
    // Watch harus sudah berhenti (status='failed', bukan active) dan sesi berikutnya sunyi.
    assert!(store.list_job_watches(None, true).unwrap().is_empty());
    assert!(poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await.is_empty());

    // 3. Pemakaian bergantian CLI ↔ gateway atas pantauan YANG SAMA -> NOL laporan dobel!
    let shared_file = dir.join("job_shared.txt");
    std::fs::write(&shared_file, "STATUS=PROCESSING\nSTAGE=S1\nPROGRESS=1/3\n").unwrap();
    WatchJob
        .run(
            json!({
                "label": "Job Bersama",
                "status_file": "job_shared.txt",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Gateway poll tahap S1 & tandai terkirim ke Telegram
    let w_s1 = store.list_job_watches(None, true).unwrap().remove(0);
    let (tr_s1, ts_s1) = evaluate_and_poll_watch(&w_s1, &sandbox, Utc::now()).await;
    assert!(tr_s1.notification.as_deref().unwrap().contains("S1"));
    persist_watch_transition(&store, w_s1.id, &tr_s1, &ts_s1, true).unwrap();

    // CLI dibuka saat status masih S1 -> 0 pesan (tidak dobel dengan gateway!)
    assert!(poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await.is_empty());

    // Status maju ke S2; sekarang CLI yang polling lebih dulu
    std::fs::write(&shared_file, "STATUS=PROCESSING\nSTAGE=S2\nPROGRESS=2/3\n").unwrap();
    let cli_s2 = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert_eq!(cli_s2.len(), 1);
    assert!(cli_s2[0].contains("S2"));

    // Gateway tick sesudah CLI -> melihat last_stage sudah S2 -> notification = None (0 duplikat!)
    let w_after_cli = store.list_job_watches(None, true).unwrap().remove(0);
    let (tr_gw_s2, _) = evaluate_and_poll_watch(&w_after_cli, &sandbox, Utc::now()).await;
    assert!(tr_gw_s2.notification.is_none(), "gateway tidak boleh mengirim ulang S2 yang sudah dilaporkan CLI");

    // Status SELESAI; gateway poll & kirim ke Telegram (delivered=true)
    std::fs::write(&shared_file, "STATUS=SELESAI\nOUTPUT=out/shared.zip\n").unwrap();
    let (tr_gw_done, ts_gw_done) = evaluate_and_poll_watch(&w_after_cli, &sandbox, Utc::now()).await;
    assert!(tr_gw_done.terminal);
    persist_watch_transition(&store, w_after_cli.id, &tr_gw_done, &ts_gw_done, true).unwrap();

    // CLI dibuka sesudahnya -> 0 pesan (sudah delivered oleh gateway!)
    assert!(poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await.is_empty());

    // Biaya token provider untuk seluruh putaran watch = 0!
    let rep = store
        .usage_report("cli-sess-1", &Utc::now().format("%Y-%m-%d").to_string())
        .unwrap();
    assert!(rep.session_rows.is_empty());
    assert!(rep.today_rows.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 3: Satu store memori persisten + pensiunkan `manage_memory` + migrasi lama
// =========================================================================

#[test]
fn p3_single_memory_tool_in_registry_toolsets_and_prompt() {
    let dir = temp_dir("p3-single-tool");
    let reg = default_registry();
    let names = reg.names();
    assert!(names.iter().any(|n| n == "memory"), "tool `memory` wajib aktif");
    assert!(
        !names.iter().any(|n| n == "manage_memory"),
        "tool lama `manage_memory` WAJIB dipensiunkan dari daftar tool aktif"
    );

    // Pastikan tidak ada tool kedua di toolset `skills_mem` yang menulis memori antar-sesi
    let mem_tools_in_toolsets: Vec<&str> = EXTENDED_TOOLSETS
        .iter()
        .flat_map(|(_, _, members)| members.iter().copied())
        .filter(|m| m.contains("memory"))
        .collect();
    assert_eq!(mem_tools_in_toolsets, vec!["memory"]);

    // System prompt hanya mengarahkan ke tool `memory` (tidak menyebut `manage_memory`)
    let sys = build_system_prompt(&dir, &[]);
    assert!(sys.contains("tool `memory`"));
    assert!(!sys.contains("manage_memory"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p3_legacy_sqlite_memory_migration_respects_cap_without_silent_truncation() {
    let dir = temp_dir("p3-migrate");
    let db_path = dir.join("selen.db");
    let mem_dir = dir.join("memory");
    let store = Store::open_at(&db_path).unwrap();

    // Tanam 2 entri lama (seolah berasal dari `manage_memory` versi sebelumnya)
    store
        .add_memory_categorized(&dir.to_string_lossy(), "conventions", "Selalu pakai format tanggal YYYY-MM-DD di laporan.")
        .unwrap();
    store
        .add_memory_categorized("global", "commands", "Gunakan folder kanonik ~/hasil-komik untuk ekspor.")
        .unwrap();

    // Migrasi pertama: keduanya masuk `memory.md` dan dibersihkan dari SQLite lama!
    let rep1 = migrate_legacy_sqlite_memories_in(&store, &mem_dir, 2_200).unwrap();
    assert_eq!(rep1.migrated, 2);
    assert_eq!(rep1.overflow_remaining, 0);
    assert!(rep1.warning.is_none());
    assert!(store.list_all_legacy_memories().unwrap().is_empty());

    let snap = load_frozen_snapshot_from(&mem_dir, 2_200, 1_375);
    let prompt_txt = snap.format_for_prompt();
    assert!(prompt_txt.contains("YYYY-MM-DD"));
    assert!(prompt_txt.contains("~/hasil-komik"));

    // Idempoten: jalankan lagi -> 0 perubahan
    let rep2 = migrate_legacy_sqlite_memories_in(&store, &mem_dir, 2_200).unwrap();
    assert_eq!(rep2.migrated, 0);

    // Kasus meluber pagu: isi `memory.md` nyaris penuh (pagu 220 char), lalu ada entri lama tambahan ->
    // TIDAK BOLEH dipotong diam-diam; entri berlebih ditahan & peringatan minta rangkum dikembalikan!
    let cap_dir = dir.join("memory_cap");
    add_entry_in(&cap_dir, MemoryStoreKind::Memory, &"x".repeat(180), 220).unwrap();
    let long_legacy = "Catatan lama panjang yang tidak boleh terpotong diam-diam saat migrasi karena melampaui sisa pagu.";
    store
        .add_memory_categorized("global", "general", long_legacy)
        .unwrap();

    let rep_cap = migrate_legacy_sqlite_memories_in(&store, &cap_dir, 220).unwrap();
    assert_eq!(rep_cap.migrated, 0);
    assert_eq!(rep_cap.overflow_remaining, 1);
    let warn = rep_cap.warning.expect("wajib ada peringatan pagu penuh");
    assert!(warn.contains("rangkum"));
    assert!(warn.contains("tidak dipotong diam-diam"));
    // File memory.md tetap valid & tidak terpotong; entri lama tetap utuh di SQLite menunggu dirangkum!
    let entries_after = load_entries_from(&cap_dir, MemoryStoreKind::Memory);
    assert_eq!(entries_after.len(), 1);
    let remaining_legacy = store.list_all_legacy_memories().unwrap();
    assert_eq!(remaining_legacy.len(), 1);
    assert_eq!(remaining_legacy[0].content, long_legacy);

    let _ = std::fs::remove_dir_all(&dir);
}
