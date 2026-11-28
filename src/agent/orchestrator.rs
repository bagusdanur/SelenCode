//! Tahap 5 — Role-Based Multi-Agent Orchestration.
//!
//! Mengorkestrasi 5 peran khusus di atas fondasi SelenCode yang sudah teruji:
//! - `Planner`    (baca-saja) : memecah tugas besar menjadi langkah kecil & terurut
//! - `Researcher` (baca-saja) : menelusuri repositori/simbol/LSP (aman berjalan paralel!)
//! - `Coder`      (tulis kode): mengimplementasikan perubahan seminimal mungkin (SELALU serial + checkpoint!)
//! - `Tester`     (verifikasi): menjalankan `verify_project` / `run_shell` & cek regresi
//! - `Reviewer`   (baca-saja) : meninjau `git_diff`, keamanan, & kualitas sebelum selesai
//!
//! Keamanan & kendali biaya:
//! - Tidak ada tool `delegate` atau `orchestrate` di registry anak -> rekursi mustahil.
//! - `Coder` tidak pernah berjalan paralel sehingga konflik tulis antar-agent dicegah secara struktural.
//! - Setiap tahap `Coder` membuka checkpoint transaksional pada `UndoStack` & mencatat file berubah.
//! - Batas token (`token_budget`) dicek sebelum setiap tahap; bila tercapai, orkestrasi berhenti
//!   secara bersih dan menyimpan checkpoint ke `Store` (jika terhubung ke `task_id`).

use crate::config::ShellCfg;
use crate::providers::{Message, Provider, Role, Usage};
use crate::security::redact_secrets;
use crate::tools::plan::PlanStep;
use crate::tools::{file, lsp, read_only_registry, shell, verify, Ctx, Registry};
use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

pub const MAX_ORCHESTRATION_STAGES: usize = 6;
pub const DEFAULT_ROLE_MAX_STEPS: usize = 10;

/// 5 peran resmi Multi-Agent Orchestration (Blueprint Tahap 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    Planner,
    Researcher,
    Coder,
    Tester,
    Reviewer,
}

impl AgentRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "planner" | "plan" => Some(AgentRole::Planner),
            "researcher" | "research" | "explorer" => Some(AgentRole::Researcher),
            "coder" | "code" | "implementer" => Some(AgentRole::Coder),
            "tester" | "test" | "verifier" => Some(AgentRole::Tester),
            "reviewer" | "review" | "auditor" => Some(AgentRole::Reviewer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentRole::Planner => "planner",
            AgentRole::Researcher => "researcher",
            AgentRole::Coder => "coder",
            AgentRole::Tester => "tester",
            AgentRole::Reviewer => "reviewer",
        }
    }

    /// Hanya `Researcher` yang aman dijalankan paralel (murni baca-saja & independen).
    pub fn is_parallel_safe(self) -> bool {
        matches!(self, AgentRole::Researcher)
    }

    pub fn is_read_only(self) -> bool {
        matches!(
            self,
            AgentRole::Planner | AgentRole::Researcher | AgentRole::Reviewer
        )
    }
}

/// Bangun registry tool khusus untuk tiap peran — tidak pernah memuat `delegate`/`orchestrate`.
pub fn role_registry(role: AgentRole, shell_cfg: &ShellCfg) -> Registry {
    match role {
        AgentRole::Planner | AgentRole::Researcher | AgentRole::Reviewer => read_only_registry(),
        AgentRole::Coder => read_only_registry()
            .register(Arc::new(file::WriteFile))
            .register(Arc::new(file::EditFile))
            .register(Arc::new(lsp::LspRename)),
        AgentRole::Tester => read_only_registry()
            .register(Arc::new(verify::VerifyProject::new(shell_cfg.timeout_secs)))
            .register(Arc::new(shell::RunShell::new(
                shell_cfg.allowlist.clone(),
                shell_cfg.timeout_secs,
            ))),
    }
}

fn role_system_prompt(role: AgentRole, ctx: &Ctx, registry: &Registry) -> String {
    let names: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
    let directive = match role {
        AgentRole::Planner => {
            "Kamu adalah agent PLANNER (baca-saja). Pecah tugas menjadi langkah kerja kecil yang konkret, \
             sebutkan file/simbol target, risiko, dan urutan eksekusi. Jangan menulis atau mengubah file."
        }
        AgentRole::Researcher => {
            "Kamu adalah agent RESEARCHER (baca-saja). Telusuri kode, simbol, referensi LSP, dan dokumentasi \
             seperlunya, lalu berikan ringkasan faktual dengan path file & nomor baris. Jangan menulis file."
        }
        AgentRole::Coder => {
            "Kamu adalah agent CODER. Implementasikan perubahan kode sesuai rencana dengan diff seminimal \
             mungkin memakai edit_file / write_file / lsp_rename. Patuhi sandbox workspace dan jangan ubah file di luar lingkup."
        }
        AgentRole::Tester => {
            "Kamu adalah agent TESTER. Jalankan verify_project atau perintah test/check yang relevan untuk \
             memverifikasi perubahan dan mendeteksi regresi. Laporkan hasil lulus/gagal secara jujur beserta detail error."
        }
        AgentRole::Reviewer => {
            "Kamu adalah agent REVIEWER (baca-saja). Periksa git_status, git_diff, dan lsp_diagnostics untuk \
             menilai keamanan, bug tersembunyi, dan kualitas kode. Akhiri dengan status: APPROVE atau CHANGES_REQUESTED."
        }
    };
    format!(
        "{directive}\n\n\
         Folder project (cwd): {}\n\
         Tool tersedia untuk peran {}: {}.\n\
         Jawab RINGKAS, terstruktur, dan langsung ke inti (hasilmu diteruskan ke orchestrator, bukan menyapa user).",
        ctx.cwd.display(),
        role.as_str(),
        names.join(", ")
    )
}

#[derive(Debug, Clone)]
pub struct StageRequest {
    pub role: AgentRole,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct StageOutput {
    pub index: usize,
    pub role: AgentRole,
    pub output: String,
    pub changed_files: Vec<String>,
    pub checkpoint_id: Option<usize>,
    pub tokens_used: u64,
    pub succeeded: bool,
}

#[derive(Debug, Clone)]
pub struct OrchestratorCfg {
    pub max_steps_per_stage: usize,
    pub token_budget: u64,
    pub max_parallel_researchers: usize,
}

impl Default for OrchestratorCfg {
    fn default() -> Self {
        Self {
            max_steps_per_stage: DEFAULT_ROLE_MAX_STEPS,
            token_budget: 120_000,
            max_parallel_researchers: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrchestrationReport {
    pub stages: Vec<StageOutput>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub budget_reached: bool,
    pub changed_files: Vec<String>,
}

impl OrchestrationReport {
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    pub fn format_summary(&self) -> String {
        let mut out = format!(
            "Orkestrasi Multi-Agent selesai ({} tahap · {} token total{}):\n",
            self.stages.len(),
            self.total_tokens(),
            if self.budget_reached {
                " · BATAS TOKEN TERCAPAI"
            } else {
                ""
            }
        );
        if !self.changed_files.is_empty() {
            out.push_str(&format!(
                "File diubah oleh Coder: {}\n",
                self.changed_files.join(", ")
            ));
        }
        for st in &self.stages {
            let status = if st.succeeded { "OK" } else { "GAGAL" };
            let cp = st
                .checkpoint_id
                .map(|id| format!(" · cp #{id}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n### Tahap {} [{} — {status}{cp} · {} token]\n{}\n",
                st.index + 1,
                st.role.as_str(),
                st.tokens_used,
                st.output
            ));
        }
        out.trim_end().to_string()
    }
}

fn child_ctx_for_role(parent_ctx: &Ctx) -> Ctx {
    Ctx {
        cwd: parent_ctx.cwd.clone(),
        approver: parent_ctx.approver.clone(),
        context_limit_tokens: parent_ctx.context_limit_tokens,
        store: None,
        session_id: None,
        plan_state: Arc::new(Mutex::new(Vec::<PlanStep>::new())),
        undo_stack: parent_ctx.undo_stack.clone(),
        repo_index: parent_ctx.repo_index.clone(),
        bg_manager: parent_ctx.bg_manager.clone(),
        lsp_manager: parent_ctx.lsp_manager.clone(),
        sandbox: parent_ctx.sandbox.clone(),
        hooks: parent_ctx.hooks.clone(),
        guard: parent_ctx.guard.clone(),
        tool_budget: parent_ctx.tool_budget.clone(),
        skills: parent_ctx.skills.clone(),
        auto_skills: false,
        self_correct: false,
        asker: Arc::new(crate::tools::ask::UserAsker::non_interactive()),
        ask_timeout_secs: parent_ctx.ask_timeout_secs,
        router_telemetry: None,
        memory_cfg: parent_ctx.memory_cfg.clone(),
        frozen_user_memory: parent_ctx.frozen_user_memory.clone(),
        is_resumed_session: false,
        telegram_chat_id: None,
        tg_file_sender: None,
        context_cfg: parent_ctx.context_cfg.clone(),
        tool_preview_length: parent_ctx.tool_preview_length,
        api_max_retries: parent_ctx.api_max_retries,
        api_retry_base_delay_ms: parent_ctx.api_retry_base_delay_ms,
        web_cfg: parent_ctx.web_cfg.clone(),
        read_tracker: Arc::new(Mutex::new(crate::tools::read_tracker::ReadFileTracker::new())),
    }
}

async fn run_single_role(
    provider: &Arc<dyn Provider>,
    role: AgentRole,
    shell_cfg: &ShellCfg,
    parent_ctx: &Ctx,
    prompt: &str,
    max_steps: usize,
) -> Result<(String, Option<Usage>)> {
    let reg = role_registry(role, shell_cfg);
    let cctx = child_ctx_for_role(parent_ctx);
    let mut messages = vec![Message::text(
        Role::System,
        role_system_prompt(role, &cctx, &reg),
    )];
    super::run::run_turn_counted(
        provider.clone(),
        &reg,
        &cctx,
        &mut messages,
        prompt,
        max_steps.max(1),
    )
    .await
}

/// Jalankan alur orkestrasi multi-agent dengan penjadwalan cerdas:
/// - Tahap `Researcher` berturut-turut dijalankan paralel (dibatasi `max_parallel_researchers`).
/// - Tahap `Planner`, `Coder`, `Tester`, `Reviewer` dijalankan berurutan dengan handoff konteks,
///   checkpoint transaksional untuk `Coder`, serta pemantauan `token_budget` & persistensi `task_id`.
pub async fn run_orchestration(
    provider: Arc<dyn Provider>,
    shell_cfg: &ShellCfg,
    parent_ctx: &Ctx,
    stages: &[StageRequest],
    cfg: &OrchestratorCfg,
    task_id: Option<&str>,
) -> Result<OrchestrationReport> {
    if stages.is_empty() {
        return Err(anyhow!("daftar tahap orkestrasi kosong"));
    }
    if stages.len() > MAX_ORCHESTRATION_STAGES {
        return Err(anyhow!(
            "terlalu banyak tahap orkestrasi: {} (maks {MAX_ORCHESTRATION_STAGES})",
            stages.len()
        ));
    }

    let mut report = OrchestrationReport {
        stages: Vec::new(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        budget_reached: false,
        changed_files: Vec::new(),
    };
    let mut handoff_notes: Vec<String> = Vec::new();
    let mut i = 0usize;

    while i < stages.len() {
        if cfg.token_budget > 0 && report.total_tokens() >= cfg.token_budget {
            report.budget_reached = true;
            if let (Some(tid), Some(store)) = (task_id, &parent_ctx.store) {
                let _ = store.save_long_task_checkpoint(
                    tid,
                    Some("paused"),
                    None,
                    Some("Dijeda otomatis: batas token orkestrasi tercapai; siap dilanjutkan."),
                    0,
                );
            }
            break;
        }

        // Batch paralel KHUSUS jika ada >= 2 tahap `Researcher` berturut-turut!
        if stages[i].role.is_parallel_safe() {
            let start = i;
            while i < stages.len() && stages[i].role.is_parallel_safe() {
                i += 1;
            }
            let batch = &stages[start..i];
            let prior_ctx = build_handoff_context(&handoff_notes, &report.changed_files);
            let sem = Arc::new(Semaphore::new(cfg.max_parallel_researchers.max(1)));

            let mut futs = Vec::new();
            for (offset, req) in batch.iter().enumerate() {
                let idx = start + offset;
                let prov = provider.clone();
                let scfg = shell_cfg.clone();
                let pctx = parent_ctx.clone();
                let sem_c = sem.clone();
                let full_prompt = combine_prompt(&req.prompt, &prior_ctx);
                let max_steps = cfg.max_steps_per_stage;
                futs.push(async move {
                    let _permit = sem_c.acquire_owned().await.expect("orchestrator sem");
                    let res = run_single_role(
                        &prov,
                        AgentRole::Researcher,
                        &scfg,
                        &pctx,
                        &full_prompt,
                        max_steps,
                    )
                    .await;
                    (idx, res)
                });
            }

            let results = futures::future::join_all(futs).await;
            for (idx, res) in results {
                let (out_text, usage, ok) = match res {
                    Ok((text, u)) => (redact_secrets(&text), u, true),
                    Err(e) => (format!("ERROR: {e}"), None, false),
                };
                let used = record_usage_into(parent_ctx, &provider, &mut report, usage);
                handoff_notes.push(format!(
                    "[Tahap {} — researcher]: {}",
                    idx + 1,
                    trim_handoff(&out_text, 1200)
                ));
                report.stages.push(StageOutput {
                    index: idx,
                    role: AgentRole::Researcher,
                    output: out_text,
                    changed_files: Vec::new(),
                    checkpoint_id: None,
                    tokens_used: used,
                    succeeded: ok,
                });
            }
            if let (Some(tid), Some(store)) = (task_id, &parent_ctx.store) {
                let _ = store.save_long_task_checkpoint(
                    tid,
                    Some("in_progress"),
                    None,
                    Some(&format!("Selesai tahap riset paralel sampai tahap {}", i)),
                    0,
                );
            }
            continue;
        }

        // Eksekusi serial untuk Planner, Coder, Tester, Reviewer.
        let req = &stages[i];
        let prior_ctx = build_handoff_context(&handoff_notes, &report.changed_files);
        let full_prompt = combine_prompt(&req.prompt, &prior_ctx);

        let cp_id = if req.role == AgentRole::Coder {
            parent_ctx
                .undo_stack
                .lock()
                .ok()
                .map(|mut st| st.begin_checkpoint(format!("orchestrator coder tahap {}", i + 1)))
        } else {
            None
        };

        let res = run_single_role(
            &provider,
            req.role,
            shell_cfg,
            parent_ctx,
            &full_prompt,
            cfg.max_steps_per_stage,
        )
        .await;

        let (out_text, usage, ok) = match res {
            Ok((text, u)) => (redact_secrets(&text), u, true),
            Err(e) => (format!("ERROR: {e}"), None, false),
        };
        let used = record_usage_into(parent_ctx, &provider, &mut report, usage);

        let mut stage_changed = Vec::new();
        if let Some(cid) = cp_id {
            if let Ok(mut st) = parent_ctx.undo_stack.lock() {
                stage_changed = st.files_changed_since(cid);
                if stage_changed.is_empty() {
                    st.discard_empty_checkpoint(cid);
                }
            }
            for f in &stage_changed {
                if !report.changed_files.contains(f) {
                    report.changed_files.push(f.clone());
                }
            }
        }

        handoff_notes.push(format!(
            "[Tahap {} — {}]: {}",
            i + 1,
            req.role.as_str(),
            trim_handoff(&out_text, 1500)
        ));
        report.stages.push(StageOutput {
            index: i,
            role: req.role,
            output: out_text,
            changed_files: stage_changed,
            checkpoint_id: cp_id,
            tokens_used: used,
            succeeded: ok,
        });

        if let (Some(tid), Some(store)) = (task_id, &parent_ctx.store) {
            let _ = store.save_long_task_checkpoint(
                tid,
                Some("in_progress"),
                None,
                Some(&format!("Selesai tahap {} ({})", i + 1, req.role.as_str())),
                used,
            );
        }

        i += 1;
    }

    Ok(report)
}

fn record_usage_into(
    ctx: &Ctx,
    provider: &Arc<dyn Provider>,
    report: &mut OrchestrationReport,
    usage: Option<Usage>,
) -> u64 {
    let Some(u) = usage else {
        return 0;
    };
    report.total_input_tokens += u.input_tokens;
    report.total_output_tokens += u.output_tokens;
    if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
        let _ = store.record_usage(sid, provider.model_name(), u.input_tokens, u.output_tokens);
    }
    u.input_tokens + u.output_tokens
}

fn build_handoff_context(notes: &[String], changed_files: &[String]) -> String {
    if notes.is_empty() && changed_files.is_empty() {
        return String::new();
    }
    let mut out = String::from("[Ringkasan hasil tahap sebelumnya]\n");
    if !changed_files.is_empty() {
        out.push_str(&format!("File yang sudah diubah: {}\n", changed_files.join(", ")));
    }
    for n in notes.iter().rev().take(4).rev() {
        out.push_str(n);
        out.push('\n');
    }
    out
}

fn combine_prompt(prompt: &str, prior: &str) -> String {
    if prior.is_empty() {
        prompt.to_string()
    } else {
        format!("{prior}\n[Tugasmu sekarang]\n{prompt}")
    }
}

fn trim_handoff(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push_str("…");
    }
    out
}
