//! Tes Tahap 3 — Eksekusi & Model (Background Process Manager + Dynamic Model Routing).

use async_trait::async_trait;
use selencode::config::RoutingCfg;
use selencode::providers::router::{
    classify_task, is_retryable_error, DynamicRouterProvider, ModelRoute, RouterTelemetry, TaskKind,
};
use selencode::providers::{AssistantReply, Message, Provider, Role, StreamEvent, ToolSpec, Usage};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::tools::bg::{BgManager, JobState};
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-stage3-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------- 1. Background Process Manager ----------

#[tokio::test]
async fn bg_manager_runs_reads_output_redacts_secrets_and_exits() {
    let cwd = temp_dir("bg-out");
    let mgr = BgManager::new();

    #[cfg(windows)]
    let cmd = "echo halo_bg_test && echo API_KEY=sk-secret9988776655";
    #[cfg(not(windows))]
    let cmd = "echo halo_bg_test && echo API_KEY=sk-secret9988776655";

    let snap = mgr.start(cmd, &cwd).unwrap();
    assert_eq!(snap.id, "job-1");
    assert_eq!(snap.owner, "selencode");

    // Tunggu sebentar hingga proses singkat selesai.
    for _ in 0..30 {
        let st = mgr.status(Some(&snap.id)).unwrap();
        if !st[0].state.is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (latest, out) = mgr.output(&snap.id, 50, 4096).unwrap();
    assert_eq!(latest.state, JobState::Exited(Some(0)));
    assert!(out.contains("halo_bg_test"));
    assert!(out.contains("[REDACTED]"));
    assert!(!out.contains("sk-secret9988776655"));

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn bg_manager_can_stop_long_running_process_and_blocks_forbidden() {
    let cwd = temp_dir("bg-stop");
    let mgr = BgManager::new();

    // Perintah destruktif WAJIB ditolak!
    assert!(mgr.start("rm -rf /", &cwd).is_err());

    #[cfg(windows)]
    let long_cmd = "ping -n 30 127.0.0.1 > NUL";
    #[cfg(not(windows))]
    let long_cmd = "sleep 30";

    let snap = mgr.start(long_cmd, &cwd).unwrap();
    assert!(snap.state.is_running());
    assert_eq!(mgr.running_count(), 1);

    let stopped = mgr.stop(&snap.id).unwrap();
    assert_eq!(stopped.state, JobState::Stopped);
    assert_eq!(mgr.running_count(), 0);

    // Job yang tidak dikenal (proses eksternal / id asing) ditolak!
    assert!(mgr.stop("job-999").is_err());

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn bg_tools_registered_only_in_execute_registry_and_need_approval_for_start() {
    let reg = default_registry();
    let ro = read_only_registry();
    for name in ["bg_start", "bg_output", "bg_status", "bg_stop"] {
        assert!(reg.get(name).is_some(), "{name} harus ada di default_registry");
        assert!(ro.get(name).is_none(), "{name} tidak boleh ada di read_only_registry");
    }
    let start_tool = reg.get("bg_start").unwrap();
    assert!(start_tool.needs_approval(&json!({"command": "echo ok"})));

    let cwd = temp_dir("bg-tool");
    let ctx = Ctx::new(cwd.clone(), true);
    let started = start_tool
        .run(json!({"command": "echo dari_tool_bg"}), &ctx)
        .await
        .unwrap();
    assert!(started.contains("job-1"));
    assert!(started.contains("selencode"));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let status_tool = reg.get("bg_status").unwrap();
    let st_text = status_tool.run(json!({}), &ctx).await.unwrap();
    assert!(st_text.contains("job-1"));

    std::fs::remove_dir_all(&cwd).ok();
}

// ---------- 2. Dynamic Model Routing + Retry + Fallback + Telemetry ----------

struct ScriptedProvider {
    name: String,
    calls: Arc<AtomicUsize>,
    fail_times: usize,
    fail_msg: String,
    reply_text: String,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            anyhow::bail!("{}", self.fail_msg);
        }
        let _ = tx.send(StreamEvent::Token(self.reply_text.clone())).await;
        Ok(AssistantReply {
            text: self.reply_text.clone(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 1_000,
                output_tokens: 500,
            }),
        })
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

#[test]
fn task_classifier_and_retryable_error_detection() {
    assert_eq!(
        classify_task(&[Message::text(Role::User, "tolong perbaiki error panic di parser")], &[]),
        TaskKind::Debugging
    );
    assert_eq!(
        classify_task(&[Message::text(Role::User, "ringkas perubahan di file ini")], &[]),
        TaskKind::Summary
    );
    assert_eq!(
        classify_task(&[Message::text(Role::User, "analisis arsitektur & rencana modul")], &[]),
        TaskKind::Reasoning
    );
    assert_eq!(
        classify_task(&[Message::text(Role::User, "tolong review diff dan audit keamanan")], &[]),
        TaskKind::Review
    );
    assert_eq!(
        classify_task(&[Message::text(Role::User, "tambahkan fungsi export CSV baru")], &[]),
        TaskKind::Coding
    );
    assert!(is_retryable_error("HTTP 429 Too Many Requests"));
    assert!(is_retryable_error("503 Service Unavailable"));
    assert!(!is_retryable_error("401 Unauthorized: invalid api key"));
}

#[tokio::test]
async fn router_retries_transient_error_then_falls_back_and_records_telemetry() {
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));

    let p1 = Arc::new(ScriptedProvider {
        name: "gpt-primary".into(),
        calls: c1.clone(),
        fail_times: 5, // selalu gagal 429
        fail_msg: "HTTP 429 rate limit exceeded".into(),
        reply_text: "from p1".into(),
    });
    let p2 = Arc::new(ScriptedProvider {
        name: "claude-backup".into(),
        calls: c2.clone(),
        fail_times: 0,
        fail_msg: String::new(),
        reply_text: "sukses dari fallback".into(),
    });

    let telemetry = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("primary", "gpt-primary", p1, Some(128_000), Some(2.0), Some(6.0), true),
            ModelRoute::new("backup", "claude-backup", p2, Some(200_000), Some(3.0), Some(15.0), false),
        ],
        false, // mode manual + fallback
        HashMap::new(),
        2, // max_retries = 2 -> primary dicoba 3x (1 + 2 retry), lalu pindah ke backup
        telemetry.clone(),
    )
    .with_backoff_ms(0);

    let (tx, _rx) = mpsc::channel(16);
    let reply = router
        .stream_chat(&[Message::text(Role::User, "tolong buat fungsi baru")], &[], tx)
        .await
        .unwrap();

    assert_eq!(reply.text, "sukses dari fallback");
    assert_eq!(c1.load(Ordering::SeqCst), 3); // 1 awal + 2 retry terbatas
    assert_eq!(c2.load(Ordering::SeqCst), 1);

    let tel = telemetry.lock().unwrap();
    assert_eq!(tel.total_retries, 2);
    assert_eq!(tel.total_fallbacks, 1);
    let m1 = tel.routes.get("primary:gpt-primary").unwrap();
    assert_eq!(m1.rate_limits, 3);
    assert!(m1.in_cooldown());
    let m2 = tel.routes.get("backup:claude-backup").unwrap();
    assert_eq!(m2.successes, 1);
    assert!(m2.has_cost_data);
    assert!(m2.estimated_cost_usd > 0.0);
}

#[tokio::test]
async fn router_auto_selects_task_route_and_fast_model_for_summary() {
    let c_big = Arc::new(AtomicUsize::new(0));
    let c_fast = Arc::new(AtomicUsize::new(0));

    let p_big = Arc::new(ScriptedProvider {
        name: "claude-sonnet-4".into(),
        calls: c_big.clone(),
        fail_times: 0,
        fail_msg: String::new(),
        reply_text: "from sonnet".into(),
    });
    let p_fast = Arc::new(ScriptedProvider {
        name: "gemini-2.5-flash".into(),
        calls: c_fast.clone(),
        fail_times: 0,
        fail_msg: String::new(),
        reply_text: "from flash".into(),
    });

    let mut rules = HashMap::new();
    rules.insert("debugging".to_string(), "main:claude-sonnet-4".to_string());

    let telemetry = Arc::new(Mutex::new(RouterTelemetry::default()));
    let router = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("main", "claude-sonnet-4", p_big, Some(200_000), Some(3.0), Some(15.0), true),
            ModelRoute::new("main", "gemini-2.5-flash", p_fast, Some(1_000_000), Some(0.1), Some(0.4), false),
        ],
        true,
        rules,
        1,
        telemetry,
    )
    .with_backoff_ms(0);

    // 1) Tugas summary -> pilih model cepat/hemat (`gemini-2.5-flash`).
    let (tx1, _rx1) = mpsc::channel(16);
    let r1 = router
        .stream_chat(&[Message::text(Role::User, "ringkas dokumen ini singkat")], &[], tx1)
        .await
        .unwrap();
    assert_eq!(r1.text, "from flash");

    // 2) Tugas debugging -> ikuti aturan eksplisit `claude-sonnet-4` (bukan yang konteksnya terbesar!).
    let (tx2, _rx2) = mpsc::channel(16);
    let r2 = router
        .stream_chat(&[Message::text(Role::User, "perbaiki error panic di main.rs")], &[], tx2)
        .await
        .unwrap();
    assert_eq!(r2.text, "from sonnet");
}

#[test]
fn stage3_commands_and_routing_config_defaults() {
    assert_eq!(parse_slash_command("/jobs"), SlashCommand::Jobs(None));
    assert_eq!(
        parse_slash_command("/bg stop job-1"),
        SlashCommand::Jobs(Some("stop job-1".into()))
    );
    assert_eq!(parse_slash_command("/route"), SlashCommand::Route(None));
    assert_eq!(
        parse_slash_command("/routing auto on"),
        SlashCommand::Route(Some("auto on".into()))
    );
    assert!(RoutingCfg::default().is_empty());
    for c in COMMANDS {
        assert!(
            !c.args.contains('|'),
            "COMMANDS[{}].args mengandung '|' literal: {}",
            c.cmd,
            c.args
        );
    }
}
