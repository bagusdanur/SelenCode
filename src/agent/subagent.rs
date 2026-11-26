//! Subagent (Fase 6): loop agent ANAK baca-saja untuk tool `delegate`.
//!
//! Anak berjalan dengan konteks BARU (system prompt khusus subagent),
//! registry baca-saja, tanpa store sesi (tidak menulis sesi sendiri), dan
//! tanpa approver interaktif (registry anak memang tidak punya tool yang
//! butuh persetujuan). Keluaran anak tidak dicetak ke terminal induk:
//! loop dijalankan lewat jalur UiEvent dengan receiver yang dibuang.

use crate::config::ShellCfg;
use crate::providers::{Message, Provider, Role, Usage};
use crate::security::Approver;
use crate::tools::plan::PlanStep;
use crate::tools::{worker_registry, Ctx, Registry};
use crate::worktree::{
    create_isolated_worktree, discard_worktree, preview_diff, WorktreeDiffPreview,
};
use anyhow::Result;
use std::sync::{Arc, Mutex};

/// Batas langkah loop anak baca-saja (Fase 6).
pub const CHILD_MAX_STEPS: usize = 10;
/// Batas langkah loop anak pekerja di worktree terisolasi (Perbaikan 6 v0.5.7).
pub const WORKER_MAX_STEPS: usize = 16;

fn child_system_prompt(ctx: &Ctx, registry: &Registry) -> String {
    let names: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
    format!(
        "Kamu adalah subagent baca-saja dari SelenCode. Kamu diberi SATU tugas \
penjelajahan/pencarian oleh agent induk; kerjakan hanya tugas itu.\n\n\
Folder project (cwd): {}\n\
Tool baca-saja yang tersedia: {}.\n\n\
Aturan:\n\
1. Kamu TIDAK bisa menulis file, menjalankan perintah shell, atau commit — tool-nya memang tidak tersedia.\n\
2. Jelajahi seperlunya (list_files / outline / read_file / search_text), lalu jawab RINGKAS dan padat: \
hanya temuan yang relevan dengan tugas, sebut path & baris bila penting.\n\
3. Jawabanmu dibaca agent induk, bukan user — jangan menyapa, jangan menawarkan langkah lanjutan.",
        ctx.cwd.display(),
        names.join(", ")
    )
}

fn worker_system_prompt(ctx: &Ctx, registry: &Registry) -> String {
    let names: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
    format!(
        "Kamu adalah subagent PEKERJA terisolasi SelenCode yang berjalan di dalam Git worktree khusus.\n\
Folder worktree (cwd): {}\n\
Tool tersedia: {}.\n\n\
Aturan:\n\
1. Semua perubahanmu terisolasi di worktree ini — pohon kerja utama user tidak tersentuh sampai user menyetujui diff-mu.\n\
2. Kerjakan tugas secara lengkap, minimal, dan jalankan verifikasi singkat bila proyek punya tes.\n\
3. Jangan mengubah file di luar cwd; jangan menjalankan `git push`; akhiri dengan ringkasan padat file yang diubah & hasil verifikasi.",
        ctx.cwd.display(),
        names.join(", ")
    )
}

/// Hasil eksekusi 1 subagent pekerja di dalam Git worktree terisolasi (Perbaikan 6 v0.5.7).
#[derive(Debug, Clone)]
pub struct WorktreeWorkerResult {
    pub worktree_id: String,
    pub branch: String,
    pub summary: String,
    pub preview: WorktreeDiffPreview,
    pub usage: Option<Usage>,
}

/// Jalankan 1 subagent baca-saja sampai selesai. Mengembalikan (teks final anak, usage anak).
pub async fn run_subagent(
    provider: &Arc<dyn Provider>,
    registry: &Registry,
    parent_ctx: &Ctx,
    task: &str,
) -> Result<(String, Option<Usage>)> {
    let child_ctx = Ctx {
        cwd: parent_ctx.cwd.clone(),
        // Warisi approver induk agar tool baca-saja yang butuh izin (fetch_url)
        // tetap menghormati persetujuan user (tidak pernah diam-diam auto_yes).
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
        frozen_user_memory: None,
        is_resumed_session: false,
        telegram_chat_id: None,
        tg_file_sender: None,
    };
    let mut messages = vec![Message::text(
        Role::System,
        child_system_prompt(&child_ctx, registry),
    )];
    super::run::run_turn_counted(
        provider.clone(),
        registry,
        &child_ctx,
        &mut messages,
        task,
        CHILD_MAX_STEPS,
    )
    .await
}

/// Jalankan 1 subagent PEKERJA di dalam Git worktree terisolasi (Perbaikan 6 v0.5.7).
/// Pohon kerja utama TIDAK pernah diubah selama eksekusi; hasil dikembalikan sebagai ringkasan + diff lengkap
/// untuk ditinjau & disetujui user sebelum diterapkan.
pub async fn run_worktree_subagent(
    provider: &Arc<dyn Provider>,
    parent_ctx: &Ctx,
    shell_cfg: &ShellCfg,
    task: &str,
) -> Result<WorktreeWorkerResult> {
    let label: String = task
        .lines()
        .next()
        .unwrap_or("work")
        .chars()
        .take(24)
        .collect();
    let wt = create_isolated_worktree(&parent_ctx.cwd, &label)?;
    let reg = worker_registry(shell_cfg);
    let wt_ctx = Ctx {
        cwd: wt.path.clone(),
        // Di dalam worktree terisolasi, pekerja boleh menulis/edit/shell tanpa mengganggu pohon utama;
        // file rahasia & perintah terlarang tetap diblokir keras oleh sandbox/security!
        approver: Arc::new(Approver::new(true)),
        context_limit_tokens: parent_ctx.context_limit_tokens,
        store: None,
        session_id: None,
        plan_state: Arc::new(Mutex::new(Vec::<PlanStep>::new())),
        undo_stack: Arc::new(Mutex::new(crate::tools::undo::UndoStack::default())),
        repo_index: Arc::new(Mutex::new(crate::context_index::RepoIndex::new(
            wt.path.clone(),
        ))),
        bg_manager: Arc::new(crate::tools::bg::BgManager::new()),
        lsp_manager: Arc::new(crate::lsp::LspManager::new(wt.path.clone())),
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
        frozen_user_memory: None,
        is_resumed_session: false,
        telegram_chat_id: None,
        tg_file_sender: None,
    };

    let mut messages = vec![Message::text(
        Role::System,
        worker_system_prompt(&wt_ctx, &reg),
    )];
    let run_res = super::run::run_turn_counted(
        provider.clone(),
        &reg,
        &wt_ctx,
        &mut messages,
        task,
        WORKER_MAX_STEPS,
    )
    .await;

    let (summary, usage) = match run_res {
        Ok(pair) => pair,
        Err(e) => {
            let _ = discard_worktree(&parent_ctx.cwd, &wt.id);
            return Err(e);
        }
    };

    let preview = match preview_diff(&parent_ctx.cwd, &wt.id) {
        Ok(p) => p,
        Err(e) => {
            let _ = discard_worktree(&parent_ctx.cwd, &wt.id);
            return Err(e);
        }
    };

    Ok(WorktreeWorkerResult {
        worktree_id: wt.id,
        branch: wt.branch,
        summary,
        preview,
        usage,
    })
}
