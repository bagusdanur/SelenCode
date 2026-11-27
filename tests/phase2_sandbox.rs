//! Tes Fase 2 — Security Sandbox v2 (mode izin, scrub env sensitif, fail-closed strict, & audit log).

use selencode::config::SandboxCfg;
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::sandbox::{
    is_sensitive_env_key, OsIsolationPolicy, SandboxBackend, SandboxController, SandboxMode,
};
use selencode::tools::{default_registry, Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-sandbox-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn sensitive_env_classifier_blocks_secrets_and_preserves_toolchain_vars() {
    assert!(is_sensitive_env_key("OPENAI_API_KEY"));
    assert!(is_sensitive_env_key("SELENCODE_ROUTER_KEY"));
    assert!(is_sensitive_env_key("SELENCODE_TELEGRAM_TOKEN"));
    assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
    assert!(is_sensitive_env_key("GITHUB_TOKEN"));
    assert!(is_sensitive_env_key("DB_PASSWORD"));
    assert!(is_sensitive_env_key("DATABASE_URL"));

    assert!(!is_sensitive_env_key("PATH"));
    assert!(!is_sensitive_env_key("HOME"));
    assert!(!is_sensitive_env_key("CARGO_HOME"));
    assert!(!is_sensitive_env_key("RUSTUP_HOME"));
    assert!(!is_sensitive_env_key("USERPROFILE"));
}

#[tokio::test]
async fn sandbox_read_only_blocks_write_edit_and_shell_and_records_audit_log() {
    let cwd = temp_dir("ro");
    let audit = cwd.join("audit.jsonl");
    let cfg = SandboxCfg {
        mode: "read-only".to_string(),
        os_isolate: "off".to_string(),
        scrub_env: true,
        allow_network: true,
        audit_log: true,
    };
    let sandbox = Arc::new(SandboxController::from_config(&cfg, audit.clone()));
    let ctx = Ctx::new(cwd.clone(), true).with_sandbox(sandbox.clone());
    let reg = default_registry();

    // write_file wajib ditolak pada mode read-only!
    let wr = reg
        .get("write_file")
        .unwrap()
        .run(json!({"path": "a.txt", "content": "halo"}), &ctx)
        .await;
    assert!(wr.is_err());
    assert!(!cwd.join("a.txt").exists());

    // run_shell wajib ditolak pada mode read-only!
    let sh = reg
        .get("run_shell")
        .unwrap()
        .run(json!({"command": "echo tes"}), &ctx)
        .await;
    assert!(sh.is_err());

    // Audit log mencatat kedua penolakan!
    let entries = sandbox.recent_audit_entries(10);
    assert!(entries.len() >= 2);
    assert!(entries.iter().any(|e| e.action == "write_file" && !e.allowed));
    assert!(entries.iter().any(|e| e.action == "run_shell" && !e.allowed));

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn sandbox_scrubs_sensitive_env_from_subprocess_and_redacts_audit_log() {
    let cwd = temp_dir("scrub");
    let audit = cwd.join("audit.jsonl");
    unsafe { std::env::set_var("SELENCODE_TEST_SECRET_TOKEN", "sk-leaktest99999999") };

    let sandbox = Arc::new(SandboxController::from_config(
        &SandboxCfg::default(),
        audit.clone(),
    ));
    let ctx = Ctx::new(cwd.clone(), true).with_sandbox(sandbox.clone());
    let reg = default_registry();

    #[cfg(windows)]
    let check_cmd = "echo VAL=%SELENCODE_TEST_SECRET_TOKEN%";
    #[cfg(not(windows))]
    let check_cmd = "echo VAL=$SELENCODE_TEST_SECRET_TOKEN";

    let out = reg
        .get("run_shell")
        .unwrap()
        .run(json!({"command": check_cmd}), &ctx)
        .await
        .unwrap();
    assert!(!out.contains("sk-leaktest99999999"), "secret env var tidak boleh bocor ke subprocess!");

    // Audit log juga wajib meredaksi secret yang tertulis di command/target!
    sandbox.record_audit("test_audit", "OPENAI_API_KEY=sk-auditsecret123456", true, "ok");
    let raw_audit = std::fs::read_to_string(&audit).unwrap();
    assert!(raw_audit.contains("[REDACTED]"));
    assert!(!raw_audit.contains("sk-auditsecret123456"));

    unsafe { std::env::remove_var("SELENCODE_TEST_SECRET_TOKEN") };
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn sandbox_strict_os_isolation_fails_closed_when_backend_unavailable() {
    let cwd = temp_dir("strict");
    let audit = cwd.join("audit.jsonl");
    let cfg = SandboxCfg {
        mode: "workspace-write".to_string(),
        os_isolate: "strict".to_string(),
        scrub_env: true,
        allow_network: false,
        audit_log: true,
    };
    let sandbox = SandboxController::with_backend_for_test(
        &cfg,
        audit,
        SandboxBackend::ProcessHardened {
            platform_note: "simulasi tanpa bwrap".to_string(),
        },
    );

    // WAJIB fail-closed: tidak boleh diam-diam menjalankan subprocess tanpa isolasi OS!
    let res = sandbox.prepare_shell_command("run_shell", "echo hello", &cwd);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("fail-closed"));

    assert_eq!(sandbox.os_isolation(), OsIsolationPolicy::Strict);
    assert_eq!(sandbox.mode(), SandboxMode::WorkspaceWrite);
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn sandbox_slash_command_parses_and_respects_mobile_layout() {
    assert_eq!(parse_slash_command("/sandbox"), SlashCommand::Sandbox(None));
    assert_eq!(
        parse_slash_command("/sandbox read-only"),
        SlashCommand::Sandbox(Some("read-only".into()))
    );
    assert_eq!(
        parse_slash_command("/sandbox isolate strict"),
        SlashCommand::Sandbox(Some("isolate strict".into()))
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
