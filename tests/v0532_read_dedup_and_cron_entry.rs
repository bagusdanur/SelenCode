//! Uji regresi & penerimaan v0.5.32:
//! - Masalah 1: Dedup pembacaan deterministik `read_file` berbasis SHA-256 (pola Hermes
//!   `tools/file_tools.py:528-558` & `tools/file_tools_read_tracking.py:205-266,344-398`),
//!   selamat dari `/resume` (SQLite `session_read_files` + rekonstruksi riwayat `messages`),
//!   dan tetap segar saat berkas diubah di antara giliran (kontrol kesegaran).
//! - Masalah 2: Validasi metakarakter shell cron di pintu masuk (CLI `selen cron add`,
//!   tool `manage_cron`, `Store::add_cron_job`, `Store::add_job_watch`) + pertahanan lapis-2.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::build_resume_system_prompt;
use selencode::agent::run_turn_counted;
use selencode::cron::{
    is_shell_command_allowlisted, validate_cron_allowlist, validate_cron_shell_command,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::store::Store;
use selencode::tools::cron::ManageCron;
use selencode::tools::file::ReadFile;
use selencode::tools::read_tracker::is_read_dedup_stub;
use selencode::tools::{default_registry, Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0532_{tag}_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// =========================================================================
// MASALAH 1: Dedup `read_file` deterministik + selamat dari resume + kesegaran
// =========================================================================

#[tokio::test]
async fn m1_read_file_dedup_stub_and_escalation_on_unchanged_file() {
    let dir = temp_dir("m1-stub-escalate");
    std::fs::write(dir.join("kata.txt"), "Melati#4821\n").unwrap();
    let ctx = Ctx::new(dir.clone(), true);

    // Panggilan #1: mengembalikan isi penuh.
    let first = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(first.trim(), "Melati#4821");
    assert!(!is_read_dedup_stub(&first));

    // Panggilan #2 (berkas tidak berubah): stub JSON ringkas `status: unchanged` (tanpa isi penuh).
    let second = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert!(is_read_dedup_stub(&second), "harus stub dedup, dapat: {second}");
    assert!(second.contains("\"status\":\"unchanged\""));
    assert!(second.contains("\"dedup\":true"));
    assert!(!second.contains("Melati#4821"), "stub tidak boleh mengulang isi penuh");

    // Panggilan #3 (masih memaksa baca ulang): eskalasi BLOCKED keras (pola Hermes `_dedup_stub_or_block`).
    let third = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert!(is_read_dedup_stub(&third));
    assert!(third.starts_with("BLOCKED:"), "eskalasi ke-2 wajib BLOCKED, dapat: {third}");
    assert!(!third.contains("Melati#4821"));

    // Panggilan #4: tetap BLOCKED, tidak pernah mengembalikan isi penuh selama sidik SHA-256 sama.
    let fourth = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert!(fourth.starts_with("BLOCKED:"));
    assert!(!fourth.contains("Melati#4821"));

    // Simulasi compaction (`reset_generation`): bacaan pemulihan pertama kembali penuh, lalu dedup lagi!
    ctx.read_tracker.lock().unwrap().reset_generation();
    let recovery = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(recovery.trim(), "Melati#4821");
    let after_recovery = ReadFile
        .run(json!({"path": "kata.txt"}), &ctx)
        .await
        .unwrap();
    assert!(after_recovery.contains("\"status\":\"unchanged\""));
}

#[tokio::test]
async fn m1_read_dedup_survives_resume_via_sqlite_and_respects_freshness() {
    let dir = temp_dir("m1-resume-sqlite");
    std::fs::write(dir.join("kata.txt"), "Melati#4821").unwrap();
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let sid = store
        .create_session(&dir.to_string_lossy(), "mock", "mock-m", "Resume E")
        .unwrap();

    // Giliran 1 (sesi sebelum ditutup): baca `kata.txt` penuh -> terekam di SQLite `session_read_files`.
    {
        let ctx1 = Ctx::new(dir.clone(), true).with_store(store.clone(), sid.clone());
        let out1 = ReadFile
            .run(json!({"path": "kata.txt"}), &ctx1)
            .await
            .unwrap();
        assert_eq!(out1, "Melati#4821");
    }

    // Giliran 2 (sesi di-resume dengan Ctx & ReadFileTracker baru dari nol): berkas TIDAK berubah.
    {
        let ctx2 = Ctx::new(dir.clone(), true)
            .with_resumed_session(true)
            .with_store(store.clone(), sid.clone());
        let out2 = ReadFile
            .run(json!({"path": "kata.txt"}), &ctx2)
            .await
            .unwrap();
        assert!(
            out2.contains("\"status\":\"unchanged\""),
            "sesi resume wajib me-dedup berkas tak berubah, dapat: {out2}"
        );
        assert!(!out2.contains("Melati#4821"));

        let out3 = ReadFile
            .run(json!({"path": "kata.txt"}), &ctx2)
            .await
            .unwrap();
        assert!(out3.starts_with("BLOCKED:"));
    }

    // Kontrol kesegaran: ubah isi berkas di disk dengan PANJANG BYTE SAMA (`Melati#4821` -> `Melati#9999`)!
    // Sidik SHA-256 WAJIB mendeteksi perubahan dan mengembalikan isi baru secara utuh.
    std::fs::write(dir.join("kata.txt"), "Melati#9999").unwrap();
    {
        let ctx3 = Ctx::new(dir.clone(), true)
            .with_resumed_session(true)
            .with_store(store.clone(), sid.clone());
        let fresh = ReadFile
            .run(json!({"path": "kata.txt"}), &ctx3)
            .await
            .unwrap();
        assert_eq!(
            fresh, "Melati#9999",
            "berkas yang diubah di antara giliran WAJIB dibaca ulang segar"
        );

        // Pembacaan berikutnya atas versi baru (tidak berubah lagi) kembali di-dedup.
        let stub_again = ReadFile
            .run(json!({"path": "kata.txt"}), &ctx3)
            .await
            .unwrap();
        assert!(stub_again.contains("\"status\":\"unchanged\""));
    }
}

#[tokio::test]
async fn m1_resume_loop_reconstructs_from_messages_and_handles_freshness() {
    let dir = temp_dir("m1-resume-loop");
    std::fs::write(dir.join("kata.txt"), "Melati#4821").unwrap();

    // Verifikasi prompt resume memuat arahan agar tidak membaca ulang berkas tak berubah & tetap ramping.
    let res_sys = build_resume_system_prompt(&dir, &[]);
    assert!(res_sys.contains("read_file"));
    assert!(res_sys.chars().count() < 1400);

    // Mock provider: pada panggilan 0, sengaja mencoba memanggil `read_file("kata.txt")`;
    // pada panggilan 1, memeriksa hasil tool yang diterima lalu menjawab.
    struct ForcedRereadProv {
        step: AtomicUsize,
        seen_snapshots: Arc<Mutex<Vec<String>>>,
        seen_tool_results: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for ForcedRereadProv {
        fn model_name(&self) -> &str {
            "mock-v0532"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let snap_joined: String = msgs
                .iter()
                .map(|m| m.plain_text())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen_snapshots.lock().unwrap().push(snap_joined);

            for m in msgs {
                if m.role == Role::Tool {
                    for b in &m.content {
                        if let Block::ToolResult { content, .. } = b {
                            self.seen_tool_results.lock().unwrap().push(content.clone());
                        }
                    }
                }
            }

            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "re1".into(),
                        name: "read_file".into(),
                        args: json!({"path": "kata.txt"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 2000,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                })
            } else {
                Ok(AssistantReply {
                    text: "Selesai.".into(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 2100,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                })
            }
        }
    }

    // Kasus A: Tanpa SQLite sama sekali — pelacak WAJIB direkonstruksi dari `messages` lama!
    let base_history = vec![
        Message::text(Role::System, "sys lama"),
        Message::text(Role::User, "Baca kata.txt dan konfirmasi saja."),
        Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "t1".into(),
                name: "read_file".into(),
                args: json!({"path": "kata.txt"}),
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "t1".into(),
                content: "Melati#4821".into(),
                is_error: false,
            }],
        },
        Message::text(Role::Assistant, "Sudah kubaca kata.txt."),
    ];

    let snaps_a = Arc::new(Mutex::new(Vec::new()));
    let tool_res_a = Arc::new(Mutex::new(Vec::new()));
    let prov_a: Arc<dyn Provider> = Arc::new(ForcedRereadProv {
        step: AtomicUsize::new(0),
        seen_snapshots: snaps_a.clone(),
        seen_tool_results: tool_res_a.clone(),
    });
    let reg = default_registry();
    let ctx_a = Ctx::new(dir.clone(), true).with_resumed_session(true);
    let mut msgs_a = base_history.clone();
    let _ = run_turn_counted(prov_a, &reg, &ctx_a, &mut msgs_a, "Apa kata sandinya?", 4)
        .await
        .unwrap();

    // Snapshot panggilan 0 sudah menyertakan fakta isi kata.txt tak berubah di konteks!
    let first_snap_a = snaps_a.lock().unwrap()[0].clone();
    assert!(
        first_snap_a.contains("tidak berubah di disk") && first_snap_a.contains("Melati#4821"),
        "snapshot resume untuk berkas tak berubah harus menjaga fakta di konteks: {first_snap_a}"
    );
    // Dan ketika model tetap memanggil `read_file("kata.txt")`, tool mengembalikan stub `unchanged`!
    let got_res_a = tool_res_a.lock().unwrap().clone();
    assert!(
        got_res_a.iter().any(|r| r.contains("\"status\":\"unchanged\"")),
        "panggilan read_file ulang di sesi resume harus mendapat stub unchanged, dapat: {got_res_a:?}"
    );

    // Kasus B (Kontrol kesegaran): ubah `kata.txt` di disk -> snapshot menandai BERUBAH & read_file mengembalikan isi baru!
    std::fs::write(dir.join("kata.txt"), "Anggrek#7777").unwrap();
    let snaps_b = Arc::new(Mutex::new(Vec::new()));
    let tool_res_b = Arc::new(Mutex::new(Vec::new()));
    let prov_b: Arc<dyn Provider> = Arc::new(ForcedRereadProv {
        step: AtomicUsize::new(0),
        seen_snapshots: snaps_b.clone(),
        seen_tool_results: tool_res_b.clone(),
    });
    let ctx_b = Ctx::new(dir.clone(), true).with_resumed_session(true);
    let mut msgs_b = base_history;
    let _ = run_turn_counted(prov_b, &reg, &ctx_b, &mut msgs_b, "Apa kata sandinya sekarang?", 4)
        .await
        .unwrap();

    let first_snap_b = snaps_b.lock().unwrap()[0].clone();
    assert!(
        first_snap_b.contains("BERUBAH di disk"),
        "snapshot resume wajib menandai berkas yang berubah di disk: {first_snap_b}"
    );
    let got_res_b = tool_res_b.lock().unwrap().clone();
    assert!(
        got_res_b.iter().any(|r| r.contains("Anggrek#7777")),
        "berkas yang diubah wajib dibaca ulang segar, dapat: {got_res_b:?}"
    );
}

// =========================================================================
// MASALAH 2: Validasi metakarakter cron & watch di pintu masuk
// =========================================================================

#[test]
fn m2_cron_validator_and_store_reject_metacharacters_and_allow_plain() {
    let dir = temp_dir("m2-store");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();

    // Payload eksak dari brief + seluruh metakarakter shell yang wajib ditolak.
    let bad_cmds = [
        "echo ok; touch /tmp/bukti-cron-jebol",
        "echo ok && touch /tmp/bukti",
        "echo ok & touch /tmp/bukti",
        "echo ok | sh",
        "echo `id`",
        "echo $(id)",
        "echo ${HOME}",
        "echo ok > /tmp/out",
        "cat < /etc/passwd",
        "echo ok\ntouch /tmp/bukti",
        "echo ok\rtouch /tmp/bukti",
    ];

    for (i, bad) in bad_cmds.iter().enumerate() {
        let err = validate_cron_shell_command(bad).unwrap_err().to_string();
        assert!(
            err.contains("metakarakter shell terlarang"),
            "pesan penolakan harus jelas untuk '{bad}', dapat: {err}"
        );
        // Lapisan kedua sisi eksekusi juga tetap menolak!
        assert!(!is_shell_command_allowlisted(bad, &[(*bad).to_string()]));

        // Simpan ke Store: WAJIB ditolak dan tidak boleh tersimpan sebagian!
        let mixed = vec!["echo ok".to_string(), (*bad).to_string()];
        assert!(validate_cron_allowlist(&mixed).is_err());
        let res = store.add_cron_job(
            &format!("bad-{i}"),
            "every:5m",
            "cek",
            ".",
            &mixed,
            false,
            0,
            "2099-01-01T00:00:00Z",
        );
        assert!(res.is_err(), "Store::add_cron_job wajib menolak '{bad}'");
    }

    // Pastikan 0 entri tersimpan sebagian setelah seluruh percobaan ditolak.
    assert!(
        store.list_cron_jobs().unwrap().is_empty(),
        "entri yang ditolak tidak boleh tersimpan sebagian"
    );

    // Kontrol positif: perintah polos tanpa metakarakter tetap diterima!
    let ok_cmds = vec![
        "echo ok".to_string(),
        "git status".to_string(),
        "ls -la".to_string(),
    ];
    let rec = store
        .add_cron_job(
            "good-job",
            "every:5m",
            "cek status",
            ".",
            &ok_cmds,
            false,
            0,
            "2099-01-01T00:00:00Z",
        )
        .unwrap();
    assert_eq!(rec.allowed_shell, ok_cmds);
    assert_eq!(store.list_cron_jobs().unwrap().len(), 1);
}

#[tokio::test]
async fn m2_manage_cron_tool_rejects_metacharacters_at_entry() {
    let dir = temp_dir("m2-tool");
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let ctx = Ctx::new(dir.clone(), true).with_store(store.clone(), "s1".into());

    // Payload eksak dari brief melalui tool `manage_cron`.
    let err = ManageCron
        .run(
            json!({
                "action": "add",
                "name": "bad-tool-cron",
                "schedule": "every 5m",
                "prompt": "jalankan cek",
                "allowed_shell": ["echo ok; touch /tmp/bukti-cron-jebol"]
            }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("metakarakter shell terlarang"),
        "manage_cron wajib menolak metakarakter di pintu masuk, dapat: {err}"
    );
    assert!(store.list_cron_jobs().unwrap().is_empty());

    // Kontrol positif: `echo ok` diterima.
    let ok = ManageCron
        .run(
            json!({
                "action": "add",
                "name": "good-tool-cron",
                "schedule": "every 5m",
                "prompt": "jalankan cek",
                "allowed_shell": ["echo ok"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(ok.contains("berhasil dibuat"), "dapat: {ok}");
    assert_eq!(store.list_cron_jobs().unwrap().len(), 1);
}

#[test]
fn m2_watch_store_rejects_shell_metacharacters() {
    let dir = temp_dir("m2-watch-store");
    let store = Store::open_at(&dir.join("selen.db")).unwrap();

    let err = store
        .add_job_watch(
            "bad-watch",
            0,
            ".",
            "shell",
            "echo ok; touch /tmp/bukti-watch",
            "DONE",
            "FAIL",
            60,
            3600,
            "2099-01-01T00:00:00Z",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("tanpa operator shell") || err.contains("ditolak"),
        "Store::add_job_watch wajib menolak metakarakter shell, dapat: {err}"
    );
    assert!(store.list_job_watches(None, false).unwrap().is_empty());
}

#[test]
fn m2_cli_cron_add_rejects_metacharacters_and_accepts_plain() {
    let dir = temp_dir("m2-cli-bin");
    let db_path = dir.join("cli_cron.db");
    let home_dir = dir.join("home");
    let cfg_dir = home_dir.join(".config");
    let data_dir = home_dir.join(".local").join("share");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_selen");
    let run_cli = |args: &[&str]| {
        std::process::Command::new(bin)
            .args(args)
            .current_dir(&dir)
            .env("SELENCODE_DB", &db_path)
            .env("HOME", &home_dir)
            .env("USERPROFILE", &home_dir)
            .env("APPDATA", &cfg_dir)
            .env("LOCALAPPDATA", &data_dir)
            .env("XDG_CONFIG_HOME", &cfg_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .output()
            .expect("gagal menjalankan binary selen")
    };

    // 1. Payload eksak dari brief WAJIB ditolak dengan exit code non-zero & pesan jelas!
    let bad = run_cli(&[
        "cron",
        "add",
        "--name",
        "uji-jebol",
        "--schedule",
        "every 5m",
        "--prompt",
        "cek",
        "--allow-shell",
        "echo ok; touch /tmp/bukti-cron-jebol",
    ]);
    assert!(
        !bad.status.success(),
        "CLI `selen cron add` WAJIB gagal pada payload metakarakter; stdout={}",
        String::from_utf8_lossy(&bad.stdout)
    );
    let stderr_bad = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr_bad.contains("metakarakter shell terlarang"),
        "stderr CLI harus menjelaskan alasan penolakan, dapat: {stderr_bad}"
    );

    // 2. Pastikan entri ditolak TIDAK tersimpan di SQLite maupun `selen cron list`.
    if db_path.exists() {
        let st = Store::open_at(&db_path).unwrap();
        assert!(
            st.list_cron_jobs().unwrap().is_empty(),
            "job yang ditolak tidak boleh tersimpan sebagian di tabel cron_jobs"
        );
    }

    // 3. Kontrol positif: perintah polos `echo ok` tetap diterima!
    let good = run_cli(&[
        "cron",
        "add",
        "--name",
        "uji-polos",
        "--schedule",
        "every 5m",
        "--prompt",
        "cek",
        "--allow-shell",
        "echo ok",
    ]);
    assert!(
        good.status.success(),
        "CLI `selen cron add` dengan perintah polos `echo ok` wajib sukses; stderr={}",
        String::from_utf8_lossy(&good.stderr)
    );
    let st = Store::open_at(&db_path).unwrap();
    let jobs = st.list_cron_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "uji-polos");
    assert_eq!(jobs[0].allowed_shell, vec!["echo ok".to_string()]);
}
