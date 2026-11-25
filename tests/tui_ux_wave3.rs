//! Tes Wave 3 — TUI UX: `@file` fuzzy autocomplete, riwayat prompt persisten (`↑`/`↓`), & rendering diff.

use selencode::store::Store;
use selencode::tui::app::{
    complete_at_mention, extract_at_query, filter_workspace_files, App,
};
use std::path::PathBuf;

fn temp_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "selencode-w3-{tag}-{}.db",
        uuid::Uuid::new_v4().simple()
    ))
}

#[test]
fn at_file_autocomplete_extracts_ranks_and_completes() {
    assert_eq!(
        extract_at_query("tolong baca @sand"),
        Some((12, "sand".to_string()))
    );
    assert_eq!(extract_at_query("/model @abc"), None);
    assert_eq!(extract_at_query("baris1\n@foo"), None);
    assert_eq!(extract_at_query("tanpa mention"), None);

    let files = vec![
        "README.md".into(),
        "src/app/commands.rs".into(),
        "src/sandbox.rs".into(),
        "src/store.rs".into(),
        "tests/phase2_sandbox.rs".into(),
    ];
    let ranked = filter_workspace_files(&files, "sand", 5);
    assert_eq!(ranked[0], "src/sandbox.rs");
    assert!(ranked.contains(&"tests/phase2_sandbox.rs".to_string()));

    let fuzzy = filter_workspace_files(&files, "sacm", 5);
    assert!(fuzzy.contains(&"src/app/commands.rs".to_string()));

    assert_eq!(
        complete_at_mention("tolong tinjau @sand", "src/sandbox.rs"),
        "tolong tinjau @src/sandbox.rs "
    );
}

#[test]
fn prompt_history_persists_in_sqlite_and_navigates_with_draft_restore() {
    let db = temp_db("hist");
    let store = Store::open_at(&db).unwrap();
    let cwd = "/workspace/tui";

    store.record_prompt_history(cwd, "prompt pertama").unwrap();
    store.record_prompt_history(cwd, "prompt pertama").unwrap(); // duplikat beruntun diabaikan
    store.record_prompt_history(cwd, "prompt kedua").unwrap();

    let loaded = store.load_prompt_history(cwd, 50).unwrap();
    assert_eq!(loaded, vec!["prompt pertama", "prompt kedua"]);

    let mut app = App::new(cwd.into(), "router".into(), "m1".into(), 10, 0, 0);
    app.prompt_history = loaded;
    app.input.insert_str("draft sedang diketik");

    assert!(app.history_up());
    assert_eq!(app.input.lines().join("\n"), "prompt kedua");
    assert!(app.history_up());
    assert_eq!(app.input.lines().join("\n"), "prompt pertama");
    assert!(!app.history_up()); // sudah di paling lama

    assert!(app.history_down());
    assert_eq!(app.input.lines().join("\n"), "prompt kedua");
    assert!(app.history_down());
    assert_eq!(app.input.lines().join("\n"), "draft sedang diketik");
    assert!(!app.history_down());

    let _ = std::fs::remove_file(&db);
}
