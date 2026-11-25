//! Tes Fase 6 — Transactional Git Worktree, Preview Diff, Proteksi Konflik User, & Rollback.

use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::tools::undo::UndoStack;
use selencode::worktree::{
    apply_worktree_to_main, create_isolated_worktree, discard_worktree, is_git_repo,
    list_worktrees, preview_diff,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

fn init_temp_git_repo(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-wt-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git tersedia");
        assert!(
            out.status.success(),
            "git {:?} gagal: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init"]);
    run(&["config", "user.email", "selen@example.com"]);
    run(&["config", "user.name", "Selen Test"]);
    std::fs::write(dir.join("app.rs"), "pub fn ver() -> u32 { 1 }\n").unwrap();
    std::fs::write(dir.join("keep.txt"), "asli\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-m", "initial"]);
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn worktree_lifecycle_preview_conflict_protection_apply_and_rollback() {
    let repo = init_temp_git_repo("cycle");
    assert!(is_git_repo(&repo));

    // 1) Buat worktree terisolasi
    let wt = create_isolated_worktree(&repo, "refactor auth").unwrap();
    assert!(wt.path.is_dir());
    assert!(wt.branch.starts_with("selen/wt-"));
    assert_eq!(list_worktrees(&repo).unwrap().len(), 1);

    // 2) Ubah file di dalam worktree (workspace utama tetap bersih!)
    std::fs::write(wt.path.join("app.rs"), "pub fn ver() -> u32 { 2 }\n").unwrap();
    std::fs::write(wt.path.join("new_mod.rs"), "pub fn added() {}\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("app.rs")).unwrap(),
        "pub fn ver() -> u32 { 1 }\n"
    );

    // 3) Simulasikan user ikut mengedit `app.rs` di workspace utama -> preview & apply WAJIB deteksi konflik!
    std::fs::write(repo.join("app.rs"), "pub fn ver() -> u32 { 999 } // edit user\n").unwrap();
    let prev_conflict = preview_diff(&repo, &wt.id).unwrap();
    assert!(prev_conflict.user_dirty_conflicts.contains(&"app.rs".to_string()));
    let undo = Mutex::new(UndoStack::default());
    assert!(apply_worktree_to_main(&repo, &wt.id, &undo).is_err());
    // Perubahan user tidak tertimpa!
    assert!(std::fs::read_to_string(repo.join("app.rs"))
        .unwrap()
        .contains("999"));

    // 4) Setelah user mengembalikan `app.rs`, apply berjalan mulus & mencatat checkpoint UndoStack!
    std::fs::write(repo.join("app.rs"), "pub fn ver() -> u32 { 1 }\n").unwrap();
    let applied = apply_worktree_to_main(&repo, &wt.id, &undo).unwrap();
    assert!(applied.applied_files.contains(&"app.rs".to_string()));
    assert!(applied.applied_files.contains(&"new_mod.rs".to_string()));
    assert_eq!(
        std::fs::read_to_string(repo.join("app.rs")).unwrap(),
        "pub fn ver() -> u32 { 2 }\n"
    );
    assert!(repo.join("new_mod.rs").is_file());

    // 5) Rollback ke checkpoint hasil apply -> mengembalikan `app.rs` & menghapus `new_mod.rs`!
    let rb = undo
        .lock()
        .unwrap()
        .rollback_to(Some(applied.checkpoint_id))
        .unwrap();
    assert_eq!(rb.restored.len(), 2);
    assert_eq!(
        std::fs::read_to_string(repo.join("app.rs")).unwrap(),
        "pub fn ver() -> u32 { 1 }\n"
    );
    assert!(!repo.join("new_mod.rs").exists());

    // 6) Hapus worktree & branch sementara bersih!
    discard_worktree(&repo, &wt.id).unwrap();
    assert!(list_worktrees(&repo).unwrap().is_empty());

    cleanup(&repo);
}

#[test]
fn worktree_and_mcp_slash_commands_parse_and_respect_mobile_layout() {
    assert_eq!(parse_slash_command("/worktree"), SlashCommand::Worktree(None));
    assert_eq!(
        parse_slash_command("/wt apply fix-1234"),
        SlashCommand::Worktree(Some("apply fix-1234".into()))
    );
    assert_eq!(parse_slash_command("/mcp"), SlashCommand::Mcp);
    for c in COMMANDS {
        assert!(
            !c.args.contains('|'),
            "COMMANDS[{}].args mengandung '|' literal: {}",
            c.cmd,
            c.args
        );
    }
}
