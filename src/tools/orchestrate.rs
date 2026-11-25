//! Tool `orchestrate` (Tahap 5 — Role-Based Multi-Agent Orchestration).
//!
//! Memungkinkan agent induk menjalankan pipeline multi-agent berbasis peran
//! (`planner`, `researcher`, `coder`, `tester`, `reviewer`) dengan:
//! - Eksekusi paralel aman khusus untuk tahap `researcher` yang berturut-turut
//! - Eksekusi serial + checkpoint transaksional untuk `coder` (anti-konflik edit)
//! - Handoff konteks antar-tahap, batas token ketat, dan checkpoint opsional ke `task_id`.

use super::{schema, str_arg, Ctx, Tool};
use crate::agent::orchestrator::{
    run_orchestration, AgentRole, OrchestratorCfg, StageRequest, MAX_ORCHESTRATION_STAGES,
};
use crate::config::ShellCfg;
use crate::providers::{Provider, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

pub struct Orchestrate {
    provider: Arc<RwLock<Arc<dyn Provider>>>,
    shell_cfg: ShellCfg,
    max_parallel_researchers: usize,
}

impl Orchestrate {
    pub fn new(
        provider: Arc<dyn Provider>,
        shell_cfg: ShellCfg,
        max_parallel_researchers: usize,
    ) -> Self {
        Self::new_shared(Arc::new(RwLock::new(provider)), shell_cfg, max_parallel_researchers)
    }

    /// Konstruktor dengan handle provider bersama agar pergantian `/model` di sesi induk
    /// otomatis ikut memperbarui provider `orchestrate` (P2.2).
    pub fn new_shared(
        provider: Arc<RwLock<Arc<dyn Provider>>>,
        shell_cfg: ShellCfg,
        max_parallel_researchers: usize,
    ) -> Self {
        Self {
            provider,
            shell_cfg,
            max_parallel_researchers: max_parallel_researchers.max(1),
        }
    }
}

#[async_trait]
impl Tool for Orchestrate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "orchestrate".into(),
            description: "Orkestrasi multi-agent berbasis peran (planner, researcher, coder, tester, reviewer) untuk tugas kompleks. Tahap researcher berturut-turut berjalan paralel; planner/coder/tester/reviewer berjalan berurutan dengan handoff konteks, checkpoint aman, dan batas token.".into(),
            parameters: schema(
                json!({
                    "stages": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_ORCHESTRATION_STAGES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {
                                    "type": "string",
                                    "enum": ["planner", "researcher", "coder", "tester", "reviewer"]
                                },
                                "prompt": { "type": "string" }
                            },
                            "required": ["role", "prompt"]
                        }
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Opsional: ID Long-Horizon Task dari manage_task agar progress tiap tahap disimpan ke SQLite"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Batas maksimum total token lintas seluruh tahap (default 120000)"
                    },
                    "max_steps_per_stage": {
                        "type": "integer",
                        "description": "Maksimum langkah tool per peran (default 10, maks 15)"
                    }
                }),
                &["stages"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let arr = args
            .get("stages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("argumen 'stages' wajib berupa array"))?;
        if arr.is_empty() {
            return Err(anyhow!("stages kosong — beri 1 sampai {MAX_ORCHESTRATION_STAGES} tahap"));
        }
        let mut stages = Vec::with_capacity(arr.len());
        for (idx, item) in arr.iter().enumerate() {
            let role_str = item
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("stages[{idx}].role wajib diisi"))?;
            let role = AgentRole::parse(role_str).ok_or_else(|| {
                anyhow!("stages[{idx}].role '{role_str}' tidak dikenal (pilih: planner, researcher, coder, tester, reviewer)")
            })?;
            let prompt = item
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("stages[{idx}].prompt wajib string non-kosong"))?;
            stages.push(StageRequest { role, prompt });
        }

        let cfg = OrchestratorCfg {
            max_steps_per_stage: args
                .get("max_steps_per_stage")
                .and_then(|v| v.as_u64())
                .map(|n| (n as usize).clamp(1, 15))
                .unwrap_or(10),
            token_budget: args
                .get("token_budget")
                .and_then(|v| v.as_u64())
                .unwrap_or(120_000),
            max_parallel_researchers: self.max_parallel_researchers,
        };
        let task_id = str_arg(&args, "task_id");
        let active_provider = self
            .provider
            .read()
            .expect("shared provider lock poisoned")
            .clone();
        let report = run_orchestration(
            active_provider,
            &self.shell_cfg,
            ctx,
            &stages,
            &cfg,
            task_id,
        )
        .await?;
        Ok(report.format_summary())
    }
}
