//! Tes fitur `/resume`: daftar 10 sesi terakhir bernomor ID angka,
//! penamaan sesi otomatis sesuai konteks isi percakapan, & hot-resume berdasarkan nomor.

use selencode::providers::{Message, Role};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::store::{infer_session_title, summarize_prompt_title, Store};
use std::path::PathBuf;

fn temp_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "selencode-resume-{tag}-{}.db",
        uuid::Uuid::new_v4().simple()
    ))
}

#[test]
fn contextual_title_strips_fillers_and_skips_greetings() {
    assert_eq!(
        summarize_prompt_title("tolong bantu fix bug autentikasi JWT di src/auth.rs?"),
        "Fix bug autentikasi JWT di src/auth.rs"
    );
    assert_eq!(
        summarize_prompt_title("Halo Selena, bisa tolong refactor modul router.rs"),
        "Refactor modul router.rs"
    );
    assert_eq!(
        summarize_prompt_title(
            "[PEMULIHAN LONG-HORIZON TASK: t-1]\nTujuan utama: Migrasi database SQLite ke WAL"
        ),
        "Migrasi database SQLite ke WAL"
    );

    // Sapaan singkat di pesan ke-1 dilewati; judul diambil dari pesan substantif ke-2!
    let msgs = vec![
        Message::text(Role::User, "halo"),
        Message::text(Role::Assistant, "Halo! Ada yang bisa kubantu?"),
        Message::text(
            Role::User,
            "buatkan perintah /resume dengan daftar 10 sesi bernomor",
        ),
    ];
    assert_eq!(
        infer_session_title(&msgs).as_deref(),
        Some("Buatkan perintah /resume dengan daftar 10 sesi bernomor")
    );
}

#[test]
fn store_auto_names_sessions_lists_ten_and_resolves_numeric_ids() {
    let db = temp_db("ten");
    let store = Store::open_at(&db).unwrap();
    let cwd = "/workspace/selencode";

    // Buat 12 sesi; 10 terakhir harus muncul berurutan terbaru -> terlama dengan ID 1..=10.
    let mut ids = Vec::new();
    for i in 1..=12 {
        let sid = store
            .create_session(cwd, "router", "gemini-pro", "(sesi baru)")
            .unwrap();
        // Pesan pertama sapaan -> judul belum berubah; pesan kedua topik nyata -> auto-rename!
        store
            .save_message(&sid, &Message::text(Role::User, "halo"))
            .unwrap();
        store
            .save_message(
                &sid,
                &Message::text(
                    Role::User,
                    format!("tolong kerjakan fitur nomor {i} di src/app.rs"),
                ),
            )
            .unwrap();
        ids.push(sid);
    }

    let top10 = store.list_sessions(cwd, 10).unwrap();
    assert_eq!(top10.len(), 10);
    // Nomor #1 adalah sesi ke-12 (terbaru), nomor #10 adalah sesi ke-3.
    assert_eq!(top10[0].id, ids[11]);
    assert_eq!(top10[0].title, "Kerjakan fitur nomor 12 di src/app.rs");
    assert_eq!(top10[9].id, ids[2]);
    assert_eq!(top10[9].title, "Kerjakan fitur nomor 3 di src/app.rs");
    assert_eq!(store.count_messages(&top10[0].id).unwrap(), 2);

    // Resolve dengan ID angka (`1`, `#2`, `10`) maupun prefix UUID.
    let s1 = store.resolve_session(cwd, "1", 10).unwrap();
    assert_eq!(s1.id, ids[11]);
    let s2 = store.resolve_session(cwd, "#2", 10).unwrap();
    assert_eq!(s2.id, ids[10]);
    let s10 = store.resolve_session(cwd, "10", 10).unwrap();
    assert_eq!(s10.id, ids[2]);

    let prefix = &ids[5][..8];
    let by_prefix = store.resolve_session(cwd, prefix, 10).unwrap();
    assert_eq!(by_prefix.id, ids[5]);

    // Nomor di luar jangkauan ditolak dengan pesan jelas (pakai `#11` agar tidak bentrok acak dengan prefix UUID hex `11...`).
    assert!(store.resolve_session(cwd, "#0", 10).is_err());
    assert!(store.resolve_session(cwd, "#11", 10).is_err());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn legacy_placeholder_session_backfills_contextual_title_on_list() {
    let db = temp_db("backfill");
    let store = Store::open_at(&db).unwrap();
    let cwd = "/workspace/legacy";

    // Judul eksplisit user tidak ditimpa.
    let explicit_id = store
        .create_session(cwd, "router", "m1", "Judul Khusus User")
        .unwrap();
    store
        .save_message(
            &explicit_id,
            &Message::text(Role::User, "ubah sesuatu di main.rs"),
        )
        .unwrap();

    // Sesi lama yang masih berjudul "(sesi baru)" otomatis diberi nama kontekstual saat list_sessions().
    let legacy_id = store
        .create_session(cwd, "router", "m1", "(sesi baru)")
        .unwrap();
    store
        .save_message(
            &legacy_id,
            &Message::text(Role::User, "optimalkan query sqlite di src/store.rs"),
        )
        .unwrap();
    store
        .update_session_title(&legacy_id, "(sesi baru)")
        .unwrap();

    let list = store.list_sessions(cwd, 10).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, legacy_id);
    assert_eq!(list[0].title, "Optimalkan query sqlite di src/store.rs");
    assert_eq!(list[1].id, explicit_id);
    assert_eq!(list[1].title, "Judul Khusus User");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn resume_slash_command_parsing_and_mobile_layout() {
    assert_eq!(parse_slash_command("/resume"), SlashCommand::Resume(None));
    assert_eq!(
        parse_slash_command("/resume 3"),
        SlashCommand::Resume(Some("3".into()))
    );
    assert_eq!(
        parse_slash_command("/resume info"),
        SlashCommand::Resume(Some("info".into()))
    );
    assert_eq!(
        parse_slash_command("/resume rename Refactor Auth"),
        SlashCommand::Resume(Some("rename Refactor Auth".into()))
    );
    for c in COMMANDS {
        assert!(
            !c.args.contains('|'),
            "COMMANDS[{}].args mengandung '|' literal: {}",
            c.cmd,
            c.args
        );
    }
}
