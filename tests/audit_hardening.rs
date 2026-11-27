//! Tes regresi menyeluruh untuk seluruh temuan Audit v0.3.0 (P0, P1, P2)
//! dan Hasil Tes Pakai Nyata (Diet Konteks P1–P4).

use selencode::agent::context::{estimate_tokens, prune_old_tool_results};
use selencode::config::SandboxCfg;
use selencode::mcp::client::validate_tool_args;
use selencode::providers::{Block, Message, Role};
use selencode::sandbox::{SandboxBackend, SandboxController};
use selencode::security::Approver;
use selencode::store::Store;
use selencode::tools::undo::UndoStack;
use selencode::tools::verify::VerifyProject;
use selencode::tools::{Ctx, Tool};
use selencode::update::format_update_error;
use selencode::worktree::{
    apply_worktree_to_main, create_isolated_worktree, discard_worktree, preview_diff,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

#[test]
fn p0_1_and_p0_3_strict_unrestricted_enforces_bwrap_and_honest_network_status() {
    let audit = std::env::temp_dir().join("selen-p0-audit.jsonl");

    // P0.1: Strict + Unrestricted + bwrap tersedia -> WAJIB tetap memakai bwrap (tidak jatuh ke host shell!).
    let cfg_strict_unres = SandboxCfg {
        mode: "unrestricted".into(),
        os_isolate: "strict".into(),
        scrub_env: true,
        allow_network: false,
        audit_log: false,
    };
    let ctrl_bwrap = SandboxController::with_backend_for_test(
        &cfg_strict_unres,
        audit.clone(),
        SandboxBackend::BubblewrapOsIsolated(PathBuf::from("/usr/bin/bwrap")),
    );
    assert!(ctrl_bwrap.will_use_os_isolation());
    assert!(ctrl_bwrap.network_isolation_enforced());
    let cmd = ctrl_bwrap
        .prepare_shell_command("test", "echo ok", &std::env::temp_dir())
        .unwrap();
    let prog = cmd.as_std().get_program().to_string_lossy().to_string();
    assert!(prog.contains("bwrap"), "strict+unrestricted harus pakai bwrap, dapat: {prog}");
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert!(args.contains(&"--unshare-net".to_string()));

    // P0.3: ProcessHardened dengan allow_network=false -> jujur melaporkan `not enforced`.
    let cfg_off_net = SandboxCfg {
        mode: "workspace-write".into(),
        os_isolate: "off".into(),
        scrub_env: true,
        allow_network: false,
        audit_log: false,
    };
    let ctrl_ph = SandboxController::with_backend_for_test(
        &cfg_off_net,
        audit,
        SandboxBackend::ProcessHardened {
            platform_note: "test".into(),
        },
    );
    assert!(!ctrl_ph.network_isolation_enforced());
    let card = ctrl_ph.format_status_card();
    assert!(card.contains("not enforced"), "{card}");
}

#[tokio::test]
async fn p0_2_verify_project_requires_approval_and_p3_skips_plain_folders() {
    let vp = VerifyProject::new(15);
    assert!(vp.needs_approval(&json!({})));

    // Live-test P3: folder polos tanpa penanda proyek -> dilewati netral tanpa prompt/error.
    let plain_dir = std::env::temp_dir().join(format!(
        "selen-plain-v-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&plain_dir).unwrap();
    let ctx_plain = Ctx::new(plain_dir.clone(), false);
    let res = vp.run(json!({"scope": "all"}), &ctx_plain).await.unwrap();
    assert!(res.contains("verifikasi proyek otomatis dilewati"), "{res}");
    let _ = std::fs::remove_dir_all(&plain_dir);

    // P0.2: folder proyek nyata (Cargo.toml) + user menolak approval -> perintah TIDAK dijalankan!
    let proj_dir = std::env::temp_dir().join(format!(
        "selen-proj-v-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0.1.0\"\n").unwrap();
    let (atx, mut arx) = tokio::sync::mpsc::channel(4);
    let ctx_deny = Ctx::new(proj_dir.clone(), false)
        .with_approver(Arc::new(Approver::channel(false, atx)));
    let deny_task = tokio::spawn(async move {
        if let Some(req) = arx.recv().await {
            assert!(req.title.contains("verify_project"));
            assert!(req.detail.contains("cargo"));
            let _ = req.reply.send(false);
        }
    });
    let out = vp.run(json!({"scope": "check"}), &ctx_deny).await.unwrap();
    deny_task.await.unwrap();
    assert!(out.contains("Ditolak user"), "{out}");
    let _ = std::fs::remove_dir_all(&proj_dir);
}

#[test]
fn p1_1_worktree_captures_committed_changes_and_applies_with_rollback() {
    let dir = std::env::temp_dir().join(format!(
        "selen-wt-commit-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let o = Command::new("git").args(args).current_dir(cwd).output().unwrap();
        assert!(
            o.status.success(),
            "git {:?} gagal: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
    };
    git(&dir, &["init"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "Tester"]);
    std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-m", "base"]);

    // Buat worktree, edit file, lalu COMMIT di dalam branch worktree!
    let wt = create_isolated_worktree(&dir, "committed-refactor").unwrap();
    std::fs::write(wt.path.join("lib.rs"), "pub fn v() -> i32 { 42 }\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-m", "refactor di worktree"]);

    // P1.1: preview_diff & apply WAJIB tetap melihat perubahan yang sudah di-commit di branch worktree!
    let prev = preview_diff(&dir, &wt.id).unwrap();
    assert!(
        prev.changed_files.contains(&"lib.rs".to_string()),
        "commit di worktree tidak boleh hilang dari preview: {:?}",
        prev.changed_files
    );
    assert!(prev.diff_text.contains("42"));

    let undo = Mutex::new(UndoStack::default());
    let applied = apply_worktree_to_main(&dir, &wt.id, &undo).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("lib.rs")).unwrap(), "pub fn v() -> i32 { 42 }\n");

    // Rollback mengembalikan ke isi asli (1)
    undo.lock().unwrap().rollback_to(Some(applied.checkpoint_id)).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("lib.rs")).unwrap(), "pub fn v() -> i32 { 1 }\n");

    discard_worktree(&dir, &wt.id).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p1_2_numeric_uuid_prefix_resolves_deterministically() {
    let db = std::env::temp_dir().join(format!(
        "selen-p12-num-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&db).unwrap();
    let cwd = "/workspace/num";
    let conn = rusqlite::Connection::open(&db).unwrap();
    // Sisipkan sesi dengan ID berawalan seluruhnya digit (`70135330-...`) secara deterministik!
    conn.execute(
        "INSERT INTO sessions (id, cwd, provider, model, created_at, title) VALUES (?1, ?2, 'r', 'm', '2026-11-25T10:00:00.000000001Z', 'Sesi Numerik')",
        rusqlite::params!["70135330-aaaa-bbbb-cccc-111111111111", cwd],
    )
    .unwrap();
    drop(conn);

    // Prefix 8 digit `70135330` HARUS ditemukan sebagai prefix ID, bukan ditolak sebagai nomor urut!
    let found = store.resolve_session(cwd, "70135330", 10).unwrap();
    assert_eq!(found.title, "Sesi Numerik");

    // Nomor urut eksplisit `#1` bekerja, sedangkan `#0` & `#2` ditolak.
    assert_eq!(store.resolve_session(cwd, "#1", 10).unwrap().title, "Sesi Numerik");
    assert!(store.resolve_session(cwd, "#0", 10).is_err());
    assert!(store.resolve_session(cwd, "#2", 10).is_err());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn p1_6_mcp_recursive_schema_validation_enforces_nested_constraints() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["mode", "items"],
        "properties": {
            "mode": { "type": "string", "enum": ["fast", "deep"] },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
            "items": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": { "type": "string", "minLength": 2 }
                    }
                }
            }
        }
    });

    assert!(validate_tool_args(
        &schema,
        &json!({"mode": "fast", "limit": 10, "items": [{"name": "ok"}]})
    )
    .is_ok());
    // Enum salah
    assert!(validate_tool_args(&schema, &json!({"mode": "turbo", "items": [{"name": "ok"}]})).is_err());
    // Field tambahan dilarang
    assert!(validate_tool_args(
        &schema,
        &json!({"mode": "fast", "items": [{"name": "ok"}], "extra": true})
    )
    .is_err());
    // Nested minLength gagal
    assert!(validate_tool_args(&schema, &json!({"mode": "fast", "items": [{"name": "a"}]})).is_err());
    // Minimum numerik gagal
    assert!(validate_tool_args(
        &schema,
        &json!({"mode": "fast", "limit": 0, "items": [{"name": "ok"}]})
    )
    .is_err());
}

#[test]
fn live_test_p1_context_diet_prunes_old_tool_outputs_and_p4_friendly_error() {
    let big_body = "x".repeat(8000);
    let mut msgs = vec![
        Message::text(Role::System, "sys"),
        Message::text(Role::User, "halo"),
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "t1".into(),
                content: big_body.clone(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "t2".into(),
                content: big_body.clone(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "t3".into(),
                content: big_body.clone(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: "t4".into(),
                content: big_body,
                is_error: false,
            }],
        },
    ];
    let before = estimate_tokens(&msgs);
    let pruned = prune_old_tool_results(&mut msgs, 2);
    let after = estimate_tokens(&msgs);
    assert_eq!(pruned, 2);
    assert!(after < before * 60 / 100, "token harus turun drastis: {before} -> {after}");

    // Live-test P4: error ramah 1 baris tanpa rantai `Caused by:` saat SELENCODE_DEBUG tidak diset.
    unsafe { std::env::remove_var("SELENCODE_DEBUG") };
    let raw_err = anyhow::anyhow!("UnknownIssuer certificate failed")
        .context("gagal menghubungi GitHub — periksa koneksi internet");
    let clean = format_update_error(raw_err);
    assert!(clean.to_string().contains("gagal menghubungi GitHub"));
    assert!(clean.to_string().contains("sertifikat TLS"));
    assert!(!format!("{clean:?}").contains("Caused by:"));
}
