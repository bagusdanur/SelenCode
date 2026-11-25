//! Tes Wave 1 & Wave 2:
//! - Telegram Markdown -> HTML formatter, Menu Markup -> HTML card, code-fence safe chunking, & daftar lengkap `TELEGRAM_BOT_COMMANDS`
//! - Lazy session creation (tidak mengotori SQLite dengan sesi kosong), `delete_session`, `prune_empty_sessions`
//! - `selen doctor` / `/doctor` diagnostics

use selencode::config::{Config, ProviderCfg};
use selencode::doctor::run_doctor;
use selencode::providers::{Message, Role};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use selencode::telegram::{
    markdown_to_telegram_html, menu_markup_to_telegram_html, split_markdown_for_telegram,
    TELEGRAM_BOT_COMMANDS,
};
use std::path::PathBuf;

fn temp_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "selencode-w12-{tag}-{}.db",
        uuid::Uuid::new_v4().simple()
    ))
}

#[test]
fn telegram_markdown_and_menu_render_clean_html_without_raw_symbols() {
    let md = "\
### 1. 9router
**Bisa**, di Termux sudah tersedia `nodejs` native (`pkg install nodejs-lts`).
- **Yang jalan mulus:** Routing API & *native addon*
  - Sub-item `better_sqlite3`
---
```rust
fn hi(a: i32) -> bool { a < 10 && a > 0 }
```
> Catatan penting [Dok](https://example.com)";

    let html = markdown_to_telegram_html(md);
    assert!(!html.contains("###"));
    assert!(!html.contains("**Bisa**"));
    assert!(html.contains("<b>1. 9router</b>"));
    assert!(html.contains("<b>Bisa</b>"));
    assert!(html.contains("<code>nodejs</code>"));
    assert!(html.contains("<i>native addon</i>"));
    assert!(html.contains("• <b>Yang jalan mulus:</b>"));
    assert!(html.contains("◦ Sub-item <code>better_sqlite3</code>"));
    assert!(html.contains("<pre><code class=\"language-rust\">"));
    assert!(html.contains("a &lt; 10 &amp;&amp; a &gt; 0"));
    assert!(html.contains("<blockquote>Catatan penting <a href=\"https://example.com\">Dok</a></blockquote>"));

    let menu = "# 10 Sesi Terakhir\n* #1 | Fix auth | 2026-11-25 · 4 msg\n---\n! Lanjutkan: `/resume 1`";
    let card = menu_markup_to_telegram_html(menu);
    assert!(card.contains("🌙 <b>10 Sesi Terakhir</b>"));
    assert!(card.contains("✦ <b>#1</b> — Fix auth"));
    assert!(card.contains("<code>/resume 1</code>"));
}

#[test]
fn telegram_bot_commands_complete_and_valid_for_set_my_commands() {
    assert!(TELEGRAM_BOT_COMMANDS.len() >= 25);
    let names: Vec<&str> = TELEGRAM_BOT_COMMANDS.iter().map(|(c, _)| *c).collect();
    for expected in ["start", "help", "resume", "model", "worktree", "sandbox", "verify", "mcp", "doctor"] {
        assert!(names.contains(&expected), "menu Telegram kurang perintah /{expected}");
    }
    for (cmd, desc) in TELEGRAM_BOT_COMMANDS {
        assert!(!cmd.is_empty() && cmd.len() <= 32);
        assert!(
            cmd.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "nama perintah Telegram tidak valid: {cmd}"
        );
        assert!(!desc.is_empty() && desc.chars().count() <= 256);
    }
}

#[test]
fn telegram_markdown_chunk_splitter_preserves_code_fences() {
    let mut big = String::from("```rust\n");
    for i in 0..80 {
        big.push_str(&format!("let line_{i} = \"baris panjang untuk pengujian pemotongan\";\n"));
    }
    big.push_str("```");
    let chunks = split_markdown_for_telegram(&big, 500);
    assert!(chunks.len() >= 2);
    for c in &chunks {
        let fence_count = c.lines().filter(|l| l.trim().starts_with("```")).count();
        assert_eq!(fence_count % 2, 0, "blok kode terpotong tanpa ditutup rapi:\n{c}");
    }
}

#[test]
fn lazy_sessions_avoid_empty_db_rows_and_support_delete_and_prune() {
    let db = temp_db("lazy");
    let store = Store::open_at(&db).unwrap();
    let cwd = "/workspace/lazy";

    // Sesi lazy yang tidak pernah menerima pesan TIDAK menulis row kosong ke SQLite!
    let ghost_id = store.register_lazy_session(cwd, "router", "m1", "(sesi baru)");
    assert!(store.list_sessions(cwd, 10).unwrap().is_empty());

    // Begitu pesan pertama dikirim, sesi otomatis termaterialisasi & dinamai sesuai konteks.
    store
        .save_message(
            &ghost_id,
            &Message::text(Role::User, "tolong rapikan tampilan telegram html"),
        )
        .unwrap();
    let list = store.list_sessions(cwd, 10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, ghost_id);
    assert_eq!(list[0].title, "Rapikan tampilan telegram html");

    // Buat 1 sesi kosong lama -> prune_empty_sessions membersihkannya tanpa menyentuh sesi aktif.
    let stale_empty = store
        .create_session(cwd, "router", "m1", "(sesi baru)")
        .unwrap();
    assert_eq!(store.list_sessions(cwd, 10).unwrap().len(), 2);
    let pruned = store.prune_empty_sessions(cwd, &ghost_id).unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(store.list_sessions(cwd, 10).unwrap().len(), 1);
    assert_ne!(store.list_sessions(cwd, 10).unwrap()[0].id, stale_empty);

    // Hapus sesi eksplisit
    assert!(store.delete_session(&ghost_id).unwrap());
    assert!(store.list_sessions(cwd, 10).unwrap().is_empty());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn doctor_command_and_diagnostics_report() {
    assert_eq!(parse_slash_command("/doctor"), SlashCommand::Doctor);
    for c in COMMANDS {
        assert!(!c.args.contains('|'), "COMMANDS[{}] mengandung '|'", c.cmd);
    }
    let db = temp_db("doc");
    let store = Store::open_at(&db).unwrap();
    let cfg = Config::empty();
    let pcfg = ProviderCfg {
        kind: "openai".into(),
        base_url: None,
        model: "test-model".into(),
        models: vec!["test-model".into()],
        api_key_env: String::new(),
        api_key: Some("k".into()),
        price_in: None,
        price_out: None,
        context_limit: None,
    };
    let sandbox = SandboxController::default();
    let rep = run_doctor(&cfg, std::path::Path::new("."), "test", &pcfg, &store, &sandbox);
    assert!(rep.checks.len() >= 7);
    let card = rep.format_menu_card();
    assert!(card.contains("SelenCode Doctor"));
    assert!(card.contains("Sandbox v2"));
    let _ = std::fs::remove_file(&db);
}
