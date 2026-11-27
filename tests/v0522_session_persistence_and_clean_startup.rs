//! Verifikasi perbaikan v0.5.22:
//! 1. Kunci warisan `[agent] max_steps = 25` diabaikan diam-diam tanpa spam teks peringatan
//!    di terminal (`startup_notices` kosong), sambil tetap 0 (tanpa batas) & tercatat di `selen doctor`.
//! 2. Persistensi sesi aktif terakhir untuk terminal (CLI/TUI) lintas restart:
//!    - buka ulang `selen` -> otomatis lanjut sesi terakhir
//!    - `/resume <lama>` lalu restart -> tetap lanjut sesi pilihan terakhir
//!    - `/new` tanpa kirim pesan lalu restart -> tetap sesi baru; setelah kirim pesan -> lanjut sesi baru
//!    - `selen resume` eksplisit tetap memulihkan `latest_session`
//! 3. Persistensi sesi aktif terakhir untuk Telegram Gateway per `chat_id` lintas restart / idle eviction:
//!    - setiap `chat_id` mengingat sesi masing-masing di SQLite `gateway_state`
//!    - upgrade dari versi lama (belum ada pointer) fallback ke `latest_session(cwd)`
//!    - `/new` + restart tetap sesi baru; `/resume` + restart tetap sesi pilihan
//!    - pesan terputus di tengah `ToolUse` saat crash/restart disanitasi bersih saat pemulihan.

use selencode::agent::context::sanitize_interrupted_messages;
use selencode::config::{load_from, parse_config_str_with_policy, MaxStepsSource, ProviderCfg};
use selencode::doctor::run_doctor;
use selencode::providers::{Block, Message, Role};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0522-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn legacy_max_steps_produces_no_terminal_startup_notice_but_doctor_tracks_it() {
    let cwd = temp_dir("clean-startup");
    let cfg_path = cwd.join("config.toml");
    let raw = r#"
default_provider = "router"

[providers.router]
kind = "openai"
model = "ag/gemini-4-hydrogen-medium"
api_key = "dummy"

[agent]
max_steps = 25
"#;
    std::fs::write(&cfg_path, raw).unwrap();

    let cfg_file = load_from(&cfg_path).unwrap();
    assert_eq!(cfg_file.agent.max_steps, 0);
    assert!(
        cfg_file.startup_notices.is_empty(),
        "tidak boleh ada teks peringatan max_steps di terminal saat startup: {:?}",
        cfg_file.startup_notices
    );
    assert!(matches!(
        cfg_file.max_steps_source,
        MaxStepsSource::DeprecatedIgnored {
            legacy_value: 25,
            ..
        }
    ));

    let cfg_str = parse_config_str_with_policy(raw, "/data/config.toml").unwrap();
    assert_eq!(cfg_str.agent.max_steps, 0);
    assert!(cfg_str.startup_notices.is_empty());

    let store = Store::open_at(&cwd.join("selen.db")).unwrap();
    let sandbox = SandboxController::default();
    let pcfg = ProviderCfg {
        kind: "openai".into(),
        base_url: None,
        model: "ag/gemini-4-hydrogen-medium".into(),
        models: Vec::new(),
        api_key_env: String::new(),
        api_key: Some("dummy".into()),
        price_in: None,
        price_out: None,
        context_limit: None,
        fallback: Vec::new(),
    };
    let rep = run_doctor(&cfg_file, &cwd, "router", &pcfg, &store, &sandbox);
    let chk = rep
        .checks
        .iter()
        .find(|c| c.name == "Batas Langkah")
        .expect("doctor tetap melaporkan Batas Langkah");
    assert!(chk.detail.contains("tanpa batas"));
    assert!(chk.detail.contains("max_steps = 25"));
}

#[test]
fn cli_last_session_persists_across_restart_resume_and_new() {
    let cwd = temp_dir("cli-persist");
    let cwd_str = cwd.to_string_lossy().to_string();
    let store = Store::open_at(&cwd.join("selen.db")).unwrap();

    // 0. Folder kosong -> belum ada sesi.
    assert!(store.resolve_cli_resume_session(&cwd_str, false).unwrap().is_none());

    // 1. Sesi lama sudah ada (skenario upgrade sebelum pointer pernah ditulis) -> auto-resume latest!
    let s1 = store
        .create_session(&cwd_str, "router", "model-a", "Sesi Satu")
        .unwrap();
    store
        .save_message(&s1, &Message::text(Role::User, "halo sesi 1"))
        .unwrap();
    let s2 = store
        .create_session(&cwd_str, "router", "model-a", "Sesi Dua")
        .unwrap();
    store
        .save_message(&s2, &Message::text(Role::User, "halo sesi 2"))
        .unwrap();

    let resumed = store
        .resolve_cli_resume_session(&cwd_str, false)
        .unwrap()
        .expect("harus otomatis melanjutkan sesi terbaru");
    assert_eq!(resumed.id, s2);

    // 2. User pindah ke sesi lama `s1` via `/resume`, lalu restart terminal -> harus tetap di `s1`!
    store.set_cli_last_session(&cwd_str, &s1).unwrap();
    let after_restart = store
        .resolve_cli_resume_session(&cwd_str, false)
        .unwrap()
        .expect("harus memuat sesi pilihan terakhir");
    assert_eq!(after_restart.id, s1);

    // 3. User jalankan `/new` lalu keluar tanpa mengirim pesan -> restart `selen` menghormati sesi baru,
    //    sementara `selen resume` eksplisit tetap memulihkan sesi terbaru.
    store
        .set_cli_last_session(&cwd_str, Store::SESSION_POINTER_NEW)
        .unwrap();
    assert!(
        store
            .resolve_cli_resume_session(&cwd_str, false)
            .unwrap()
            .is_none(),
        "auto-resume harus menghormati /new yang belum terisi pesan"
    );
    let explicit = store
        .resolve_cli_resume_session(&cwd_str, true)
        .unwrap()
        .expect("selen resume eksplisit harus tetap memulihkan sesi terakhir");
    assert_eq!(explicit.id, s2);

    // 4. Jika pointer menunjuk sesi yang sudah dihapus -> fallback mulus ke sesi terakhir yang tersisa.
    store.set_cli_last_session(&cwd_str, &s1).unwrap();
    store.delete_session(&s1).unwrap();
    let fallback = store
        .resolve_cli_resume_session(&cwd_str, false)
        .unwrap()
        .expect("fallback ke sesi tersisa saat sesi pointer dihapus");
    assert_eq!(fallback.id, s2);
}

#[test]
fn telegram_chat_sessions_persist_independently_across_gateway_restart_and_sanitize_crash() {
    let cwd = temp_dir("tg-persist");
    let cwd_str = cwd.to_string_lossy().to_string();
    let db_path = cwd.join("selen.db");

    let (chat_a, chat_b) = (1001_i64, 2002_i64);
    let (sid_a, sid_b): (String, String);

    {
        let store = Store::open_at(&db_path).unwrap();
        sid_a = store
            .create_session(&cwd_str, "router", "model-a", "Chat A")
            .unwrap();
        store
            .save_message(&sid_a, &Message::text(Role::User, "kerjakan deploy"))
            .unwrap();
        // Simulasikan InstaBox/gateway mati mendadak tepat sesudah assistant mengeluarkan ToolUse!
        store
            .save_message(
                &sid_a,
                &Message {
                    role: Role::Assistant,
                    content: vec![Block::ToolUse {
                        id: "call_crash_1".into(),
                        name: "run_shell".into(),
                        args: serde_json::json!({"command": "echo hi"}),
                    }],
                },
            )
            .unwrap();
        store.set_telegram_chat_session(chat_a, &sid_a).unwrap();

        sid_b = store
            .create_session(&cwd_str, "router", "model-a", "Chat B")
            .unwrap();
        store
            .save_message(&sid_b, &Message::text(Role::User, "halo chat B"))
            .unwrap();
        store.set_telegram_chat_session(chat_b, &sid_b).unwrap();
    }

    // Simulasikan proses gateway / InstaBox restart (buka ulang SQLite dari awal, RAM kosong!)
    {
        let store = Store::open_at(&db_path).unwrap();

        let restored_a = store
            .resolve_telegram_resume_session(chat_a, &cwd_str)
            .unwrap()
            .expect("chat A harus melanjutkan sesi sebelumnya sesudah restart");
        assert_eq!(restored_a.id, sid_a);

        // Sanitasi dangling ToolUse akibat crash mendadak agar provider API tidak error 400.
        let mut msgs_a = store.load_messages(&restored_a.id).unwrap();
        assert_eq!(msgs_a.len(), 2);
        let syn = sanitize_interrupted_messages(&mut msgs_a)
            .expect("dangling ToolUse harus disanitasi saat pemulihan pasca-restart");
        store.save_message(&restored_a.id, &syn).unwrap();
        assert_eq!(msgs_a.len(), 3);
        assert_eq!(msgs_a[2].role, Role::Tool);

        let restored_b = store
            .resolve_telegram_resume_session(chat_b, &cwd_str)
            .unwrap()
            .expect("chat B harus melanjutkan sesinya sendiri secara terpisah");
        assert_eq!(restored_b.id, sid_b);

        // `/new` pada chat A bertahan lintas restart sampai pesan baru dikirim.
        store
            .set_telegram_chat_session(chat_a, Store::SESSION_POINTER_NEW)
            .unwrap();
        assert!(
            store
                .resolve_telegram_resume_session(chat_a, &cwd_str)
                .unwrap()
                .is_none(),
            "/new pada Telegram harus tetap dihormati sesudah restart"
        );
        // Chat B tidak terpengaruh oleh `/new` milik chat A.
        assert_eq!(
            store
                .resolve_telegram_resume_session(chat_b, &cwd_str)
                .unwrap()
                .unwrap()
                .id,
            sid_b
        );
    }
}
