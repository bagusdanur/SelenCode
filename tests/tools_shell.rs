#![cfg(unix)]
//! Tes Fase 2: tool run_shell (Unix/Termux/Linux).
use selencode::tools::shell::RunShell;
use selencode::tools::{Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;

fn temp_cwd(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-shell-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn ctx(dir: &PathBuf) -> Ctx {
    Ctx::new(dir.clone(), true) // auto_yes: approval lolos otomatis
}

#[tokio::test]
async fn shell_echo_ok() {
    let dir = temp_cwd("echo");
    let tool = RunShell::new(vec![], 120);
    let out = tool
        .run(json!({"command": "echo halo-selen"}), &ctx(&dir))
        .await
        .unwrap();
    assert!(out.contains("halo-selen"), "{out}");
    assert!(out.contains("exit code: 0"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn shell_nonzero_exit_is_reported_not_fatal() {
    let dir = temp_cwd("exit3");
    let tool = RunShell::new(vec![], 120);
    let out = tool
        .run(json!({"command": "echo gagal-total; exit 3"}), &ctx(&dir))
        .await
        .unwrap();
    assert!(out.contains("exit code: 3"), "{out}");
    assert!(out.contains("gagal-total"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn shell_timeout_errors() {
    let dir = temp_cwd("timeout");
    let tool = RunShell::new(vec![], 1); // timeout 1 detik
    let err = tool
        .run(json!({"command": "sleep 5"}), &ctx(&dir))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timeout"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn shell_forbidden_rejected_without_running() {
    let dir = temp_cwd("forbidden");
    let tool = RunShell::new(vec![], 120);
    for cmd in ["rm -rf /", "rm -rf ~", "shutdown now", "mkfs.ext4 /dev/sda1", ":(){ :|:& };:"] {
        let err = tool
            .run(json!({"command": cmd}), &ctx(&dir))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ditolak"),
            "perintah '{cmd}' harus ditolak: {err}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn shell_allowlist_controls_needs_approval() {
    let tool = RunShell::new(vec!["echo".into(), "git status".into()], 120);
    assert!(!tool.needs_approval(&json!({"command": "echo halo"})));
    assert!(!tool.needs_approval(&json!({"command": "git status --short"})));
    assert!(tool.needs_approval(&json!({"command": "cargo build"})));
    // "ls" di allowlist tidak boleh meloloskan "lsof"
    let tool2 = RunShell::new(vec!["ls".into()], 120);
    assert!(tool2.needs_approval(&json!({"command": "lsof -i"})));
    assert!(!tool2.needs_approval(&json!({"command": "ls -la"})));
    // Chaining / subshell / redirect TIDAK boleh lolos allowlist
    for sneaky in [
        "ls ; rm -rf x",
        "ls && cat .env",
        "ls | sh",
        "ls $(whoami)",
        "ls `id`",
        "ls > out.txt",
    ] {
        assert!(
            tool2.needs_approval(&json!({"command": sneaky})),
            "perintah '{sneaky}' tidak boleh lolos allowlist"
        );
    }
}
