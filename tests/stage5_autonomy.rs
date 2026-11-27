//! Tes Tahap 5 — Otonomi Lanjutan (Role-Based Multi-Agent Orchestration + Long-Horizon Task Persistence).

use async_trait::async_trait;
use selencode::agent::orchestrator::{
    role_registry, run_orchestration, AgentRole, OrchestratorCfg, StageRequest,
};
use selencode::config::ShellCfg;
use selencode::providers::{
    AssistantReply, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::store::Store;
use selencode::tools::orchestrate::Orchestrate;
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-stage5-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------- 1. Role-Scoped Registries & Recursion Safety ----------

#[test]
fn role_registries_enforce_least_privilege_and_prevent_recursion() {
    let shell_cfg = ShellCfg::default();
    for role in [
        AgentRole::Planner,
        AgentRole::Researcher,
        AgentRole::Coder,
        AgentRole::Tester,
        AgentRole::Reviewer,
    ] {
        let reg = role_registry(role, &shell_cfg);
        // Rekursi mustahil secara konstruksi: tidak ada delegate/orchestrate di peran anak!
        assert!(reg.get("delegate").is_none());
        assert!(reg.get("orchestrate").is_none());
        assert!(reg.get("git_commit").is_none());
    }

    // Planner, Researcher, Reviewer wajib murni baca-saja!
    for ro_role in [AgentRole::Planner, AgentRole::Researcher, AgentRole::Reviewer] {
        let reg = role_registry(ro_role, &shell_cfg);
        assert!(reg.get("write_file").is_none());
        assert!(reg.get("edit_file").is_none());
        assert!(reg.get("run_shell").is_none());
        assert!(reg.get("lsp_diagnostics").is_some());
    }

    // Coder punya write_file/edit_file/lsp_rename tapi tidak punya run_shell.
    let coder_reg = role_registry(AgentRole::Coder, &shell_cfg);
    assert!(coder_reg.get("write_file").is_some());
    assert!(coder_reg.get("edit_file").is_some());
    assert!(coder_reg.get("lsp_rename").is_some());
    assert!(coder_reg.get("run_shell").is_none());

    // Tester punya verify_project & run_shell tapi tidak punya write_file/edit_file.
    let tester_reg = role_registry(AgentRole::Tester, &shell_cfg);
    assert!(tester_reg.get("verify_project").is_some());
    assert!(tester_reg.get("run_shell").is_some());
    assert!(tester_reg.get("write_file").is_none());

    // Hanya Researcher yang aman dijalankan paralel!
    assert!(AgentRole::Researcher.is_parallel_safe());
    assert!(!AgentRole::Coder.is_parallel_safe());
    assert!(!AgentRole::Tester.is_parallel_safe());
}

// ---------- 2. Multi-Agent Orchestration: Parallel Researchers, Serial Coder + Checkpoint, Budget ----------

struct RoleMockProvider {
    step: Arc<AtomicUsize>,
    tokens_per_call: u64,
    coder_writes_file: bool,
}

#[async_trait]
impl Provider for RoleMockProvider {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let sys = msgs.first().map(|m| m.plain_text()).unwrap_or_default();
        let n = self.step.fetch_add(1, Ordering::SeqCst);

        // Jika peran CODER dan belum menulis file pada panggilan pertamanya, panggil write_file!
        if self.coder_writes_file && sys.contains("agent CODER") && msgs.len() == 2 {
            return Ok(AssistantReply {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call_{n}"),
                    name: "write_file".into(),
                    args: json!({
                        "path": "generated.txt",
                        "content": "hasil dari coder agent\n"
                    }),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: Some(Usage {
                    input_tokens: self.tokens_per_call / 2,
                    output_tokens: self.tokens_per_call / 2,
                    ..Default::default()
                }),
            });
        }

        let role_tag = if sys.contains("agent PLANNER") {
            "rencana selesai"
        } else if sys.contains("agent RESEARCHER") {
            "temuan riset selesai"
        } else if sys.contains("agent CODER") {
            "implementasi selesai"
        } else if sys.contains("agent TESTER") {
            "verifikasi lulus"
        } else {
            "APPROVE — diff aman"
        };

        Ok(AssistantReply {
            text: role_tag.to_string(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: self.tokens_per_call / 2,
                output_tokens: self.tokens_per_call / 2,
                ..Default::default()
            }),
        })
    }

    fn model_name(&self) -> &str {
        "mock-orch-model"
    }
}

#[tokio::test]
async fn orchestrator_runs_pipeline_records_coder_checkpoint_and_persists_long_task() {
    let cwd = temp_dir("orch");
    let db = cwd.join("state.db");
    let store = Arc::new(Store::open_at(&db).unwrap());
    let cwd_str = cwd.to_string_lossy().to_string();
    let task = store
        .create_long_task(&cwd_str, "Tambahkan modul ekspor", &[], 100_000)
        .unwrap();

    let ctx = Ctx::new(cwd.clone(), true).with_store(store.clone(), "sess-orch".into());
    let provider: Arc<dyn Provider> = Arc::new(RoleMockProvider {
        step: Arc::new(AtomicUsize::new(0)),
        tokens_per_call: 400,
        coder_writes_file: true,
    });

    let stages = vec![
        StageRequest {
            role: AgentRole::Planner,
            prompt: "Pecah tugas ekspor".into(),
        },
        StageRequest {
            role: AgentRole::Researcher,
            prompt: "Cek modul A".into(),
        },
        StageRequest {
            role: AgentRole::Researcher,
            prompt: "Cek modul B".into(),
        },
        StageRequest {
            role: AgentRole::Coder,
            prompt: "Buat file generated.txt".into(),
        },
        StageRequest {
            role: AgentRole::Reviewer,
            prompt: "Review hasil".into(),
        },
    ];

    let report = run_orchestration(
        provider.clone(),
        &ShellCfg::default(),
        &ctx,
        &stages,
        &OrchestratorCfg::default(),
        Some(&task.id),
    )
    .await
    .unwrap();

    assert_eq!(report.stages.len(), 5);
    assert!(!report.budget_reached);
    assert!(report.changed_files.contains(&"generated.txt".to_string()));
    assert!(cwd.join("generated.txt").exists());
    // Coder wajib membuat checkpoint transaksional yang bisa di-rollback!
    let coder_cp = report.stages[3].checkpoint_id.expect("checkpoint coder");
    let rb = ctx
        .undo_stack
        .lock()
        .unwrap()
        .rollback_to(Some(coder_cp))
        .unwrap();
    assert_eq!(rb.restored.len(), 1);
    assert!(!cwd.join("generated.txt").exists());

    // State Long-Horizon Task di SQLite ikut diperbarui!
    let reloaded = store.get_long_task(&task.id).unwrap().unwrap();
    assert_eq!(reloaded.status, "in_progress");
    assert!(reloaded.tokens_used > 0);

    // Uji tool `orchestrate` end-to-end dengan batas token ketat!
    let orch_tool = Orchestrate::new(provider, ShellCfg::default(), 2);
    let limited = orch_tool
        .run(
            json!({
                "stages": [
                    {"role": "planner", "prompt": "rencana"},
                    {"role": "reviewer", "prompt": "review"}
                ],
                "token_budget": 300,
                "task_id": task.id
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(limited.contains("BATAS TOKEN TERCAPAI"));
    let paused = store.get_long_task(&task.id).unwrap().unwrap();
    assert_eq!(paused.status, "paused");

    std::fs::remove_dir_all(&cwd).ok();
}

// ---------- 3. Long-Horizon Task Persistence & Cross-Session Recovery ----------

#[tokio::test]
async fn long_horizon_task_persists_across_store_reopen_and_redacts_secrets() {
    let cwd = temp_dir("longtask");
    let db = cwd.join("tasks.db");
    let cwd_str = cwd.to_string_lossy().to_string();

    let task_id = {
        let store = Arc::new(Store::open_at(&db).unwrap());
        let reg = default_registry();
        assert!(read_only_registry().get("manage_task").is_none());
        let tool = reg.get("manage_task").expect("manage_task di default_registry");
        let ctx = Ctx::new(cwd.clone(), true).with_store(store.clone(), "sess-1".into());

        let created = tool
            .run(
                json!({
                    "action": "create",
                    "goal": "Refactor modul pembayaran penuh",
                    "token_budget": 150000
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(created.contains("task-"));

        let list = store.list_long_tasks(&cwd_str, 10).unwrap();
        assert_eq!(list.len(), 1);
        let tid = list[0].id.clone();

        // Update langkah #1 menjadi done + catatan checkpoint (termasuk uji redaksi rahasia!).
        let upd = tool
            .run(
                json!({
                    "action": "update_step",
                    "task_id": tid,
                    "step_id": 1,
                    "step_status": "done",
                    "step_notes": "Selesai merancang; kunci API_KEY=sk-secret123456789 jangan bocor"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(upd.contains("1/5 selesai"));

        let _ = tool
            .run(
                json!({
                    "action": "checkpoint",
                    "task_id": tid,
                    "status": "paused",
                    "checkpoint_note": "Menunggu verifikasi tahap coder"
                }),
                &ctx,
            )
            .await
            .unwrap();
        tid
    };

    // Buka ulang SQLite di "sesi baru" -> seluruh state & checkpoint wajib pulih utuh!
    let store2 = Store::open_at(&db).unwrap();
    let recovered = store2.get_long_task(&task_id).unwrap().unwrap();
    assert_eq!(recovered.status, "paused");
    assert_eq!(recovered.completed_steps(), 1);
    assert!(recovered.steps[0].notes.contains("[REDACTED]"));
    assert!(!recovered.steps[0].notes.contains("sk-secret123456789"));

    let prompt = recovered.build_resume_prompt();
    assert!(prompt.contains("PEMULIHAN LONG-HORIZON TASK"));
    assert!(prompt.contains("Refactor modul pembayaran penuh"));
    assert!(prompt.contains("Menunggu verifikasi tahap coder"));
    assert!(prompt.contains("1/5 langkah selesai"));

    assert!(store2.delete_long_task(&task_id).unwrap());
    assert!(store2.get_long_task(&task_id).unwrap().is_none());

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn stage5_task_command_parses_and_respects_mobile_layout() {
    assert_eq!(parse_slash_command("/task"), SlashCommand::Task(None));
    assert_eq!(
        parse_slash_command("/tasks resume task-1234abcd"),
        SlashCommand::Task(Some("resume task-1234abcd".into()))
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
