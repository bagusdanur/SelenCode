//! Subagent (Fase 6): loop agent ANAK baca-saja untuk tool `delegate`.
//!
//! Anak berjalan dengan konteks BARU (system prompt khusus subagent),
//! registry baca-saja, tanpa store sesi (tidak menulis sesi sendiri), dan
//! tanpa approver interaktif (registry anak memang tidak punya tool yang
//! butuh persetujuan). Keluaran anak tidak dicetak ke terminal induk:
//! loop dijalankan lewat jalur UiEvent dengan receiver yang dibuang.

use crate::providers::{Message, Provider, Role, Usage};
use crate::tools::plan::PlanStep;
use crate::tools::{Ctx, Registry};
use anyhow::Result;
use std::sync::{Arc, Mutex};

/// Batas langkah loop anak (lebih kecil dari induk: anak hanya menjelajah).
pub const CHILD_MAX_STEPS: usize = 10;

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

/// Jalankan 1 subagent sampai selesai. Mengembalikan (teks final anak,
/// usage anak bila provider melaporkannya).
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
