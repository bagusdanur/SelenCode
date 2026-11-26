//! Tool `delegate` (Fase 6): tugaskan 1-5 penjelajahan INDEPENDEN ke
//! subagent baca-saja yang berjalan PARALEL (dibatasi semaphore).
//!
//! Rekursi mustahil secara konstruksi: registry anak (dibangun di
//! tools::read_only_registry) tidak memuat `delegate` maupun `update_plan`.
//! Hasil digabung dengan urutan SESUAI urutan input, bukan urutan selesai;
//! kegagalan satu anak tidak menggagalkan yang lain.

use super::{schema, Ctx, Registry, Tool};
use crate::agent::subagent::{run_subagent, run_worktree_subagent};
use crate::config::ShellCfg;
use crate::providers::{Provider, ToolSpec};
use crate::worktree::{apply_worktree_to_main, discard_worktree, is_git_repo};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use tokio::sync::Semaphore;

pub const MAX_DELEGATE_TASKS: usize = 5;

pub struct Delegate {
    provider: Arc<RwLock<Arc<dyn Provider>>>,
    child_registry: Arc<Registry>,
    max_parallel: usize,
    shell_cfg: ShellCfg,
}

impl Delegate {
    /// Konstruktor publik agar bisa dibangun & dites terpisah dari main.
    /// `child_registry` WAJIB registry baca-saja (tools::read_only_registry).
    pub fn new(
        provider: Arc<dyn Provider>,
        child_registry: Arc<Registry>,
        max_parallel: usize,
    ) -> Self {
        Self::new_shared(Arc::new(RwLock::new(provider)), child_registry, max_parallel)
    }

    /// Konstruktor dengan handle provider bersama agar pergantian `/model` di sesi induk
    /// otomatis ikut memperbarui provider subagent `delegate` (P2.2).
    pub fn new_shared(
        provider: Arc<RwLock<Arc<dyn Provider>>>,
        child_registry: Arc<Registry>,
        max_parallel: usize,
    ) -> Self {
        Self {
            provider,
            child_registry,
            max_parallel: max_parallel.max(1),
            shell_cfg: ShellCfg::default(),
        }
    }

    pub fn with_shell_cfg(mut self, shell_cfg: ShellCfg) -> Self {
        self.shell_cfg = shell_cfg;
        self
    }
}

fn short_prompt(p: &str) -> String {
    let first = p.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(80).collect();
    if first.chars().count() > 80 {
        out.push('…');
    }
    out
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description: "Tugaskan 1-5 tugas paralel ke subagent. Default mode='read' (baca-saja). Mode='work' menjalankan pekerja di Git worktree terisolasi (boleh write/edit/run_shell), mengembalikan diff lengkap, dan baru menerapkan ke pohon utama sesudah persetujuan user."
                .into(),
            parameters: schema(
                json!({
                    "mode": {
                        "type": "string",
                        "enum": ["read", "work"],
                        "description": "'read' (default, baca-saja) atau 'work' (pekerja di Git worktree terisolasi + diff + approval)"
                    },
                    "tasks": {"type": "array", "minItems": 1, "maxItems": 5, "items": {
                        "type": "object",
                        "properties": {"prompt": {"type": "string"}},
                        "required": ["prompt"]
                    }}
                }),
                &["tasks"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "read".to_string());
        if mode != "read" && mode != "work" {
            return Err(anyhow!(
                "mode '{mode}' tidak dikenal — gunakan 'read' (baca-saja) atau 'work' (worktree terisolasi)"
            ));
        }

        let arr = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("argumen 'tasks' wajib berupa array"))?;
        if arr.is_empty() {
            return Err(anyhow!("tasks kosong — beri 1 sampai {MAX_DELEGATE_TASKS} tugas"));
        }
        if arr.len() > MAX_DELEGATE_TASKS {
            return Err(anyhow!(
                "terlalu banyak tugas: {} (maks {MAX_DELEGATE_TASKS} per delegate)",
                arr.len()
            ));
        }
        let mut prompts: Vec<String> = Vec::new();
        for (i, t) in arr.iter().enumerate() {
            let p = t
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("tasks[{i}].prompt wajib string non-kosong"))?;
            prompts.push(p);
        }

        let active_provider = self
            .provider
            .read()
            .expect("shared provider lock poisoned")
            .clone();

        if mode == "work" {
            if !is_git_repo(&ctx.cwd) {
                return Err(anyhow!(
                    "delegate mode='work' ditolak: folder '{}' bukan repositori Git — pekerja terisolasi membutuhkan Git worktree.",
                    ctx.cwd.display()
                ));
            }
            let semaphore = Arc::new(Semaphore::new(self.max_parallel));
            let mut futs = Vec::new();
            for prompt in prompts.iter() {
                let provider = active_provider.clone();
                let ctx_c = ctx.clone();
                let shell_c = self.shell_cfg.clone();
                let sem = semaphore.clone();
                let prompt = prompt.clone();
                futs.push(async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore delegate work");
                    run_worktree_subagent(&provider, &ctx_c, &shell_c, &prompt).await
                });
            }
            let results = futures::future::join_all(futs).await;

            let mut out = String::new();
            for (i, (prompt, res)) in prompts.iter().zip(results).enumerate() {
                if i > 0 {
                    out.push_str("\n\n");
                }
                match res {
                    Ok(worker) => {
                        if let (Some(store), Some(sid), Some(u)) =
                            (&ctx.store, &ctx.session_id, worker.usage)
                        {
                            let _ = store.record_usage_with_kind(
                                sid,
                                active_provider.model_name(),
                                u.input_tokens,
                                u.output_tokens,
                                "delegate_work",
                            );
                        }
                        let has_changes = !worker.preview.changed_files.is_empty()
                            || !worker.preview.deleted_files.is_empty();
                        let diff_block = if worker.preview.diff_text.trim().is_empty() {
                            "(tidak ada perubahan file di worktree)".to_string()
                        } else {
                            worker.preview.diff_text.clone()
                        };
                        let apply_status = if !has_changes {
                            let _ = discard_worktree(&ctx.cwd, &worker.worktree_id);
                            "Tidak ada perubahan file untuk diterapkan.".to_string()
                        } else if !worker.preview.user_dirty_conflicts.is_empty() {
                            let _ = discard_worktree(&ctx.cwd, &worker.worktree_id);
                            format!(
                                "TIDAK diterapkan: bentrok dengan file yang sedang dimodifikasi di workspace utama ({}).",
                                worker.preview.user_dirty_conflicts.join(", ")
                            )
                        } else {
                            let title = format!(
                                "delegate_work apply {} ({} ubah, {} hapus)",
                                worker.worktree_id,
                                worker.preview.changed_files.len(),
                                worker.preview.deleted_files.len()
                            );
                            if ctx.approver.ask(&title, &diff_block).await {
                                match apply_worktree_to_main(
                                    &ctx.cwd,
                                    &worker.worktree_id,
                                    &ctx.undo_stack,
                                ) {
                                    Ok(rep) => {
                                        let _ = discard_worktree(&ctx.cwd, &worker.worktree_id);
                                        format!(
                                            "DISETUJUI & DITERAPKAN ke workspace utama (checkpoint #{}, {} file diubah, {} dihapus).",
                                            rep.checkpoint_id,
                                            rep.applied_files.len(),
                                            rep.deleted_files.len()
                                        )
                                    }
                                    Err(e) => {
                                        let _ = discard_worktree(&ctx.cwd, &worker.worktree_id);
                                        format!("GAGAL diterapkan ke workspace utama: {e}")
                                    }
                                }
                            } else {
                                let _ = discard_worktree(&ctx.cwd, &worker.worktree_id);
                                "DITOLAK user — perubahan TIDAK diterapkan ke workspace utama (worktree dibersihkan)."
                                    .to_string()
                            }
                        };
                        out.push_str(&format!(
                            "### Pekerja {} [worktree {}]: {}\nRingkasan:\n{}\n\nStatus penerapan: {}\n\nDiff:\n```diff\n{}\n```",
                            i + 1,
                            worker.worktree_id,
                            short_prompt(prompt),
                            if worker.summary.trim().is_empty() {
                                "(selesai tanpa ringkasan teks)"
                            } else {
                                worker.summary.trim()
                            },
                            apply_status,
                            diff_block
                        ));
                    }
                    Err(e) => {
                        out.push_str(&format!(
                            "### Pekerja {}: {}\nERROR: subagent pekerja gagal: {e}",
                            i + 1,
                            short_prompt(prompt)
                        ));
                    }
                }
            }
            return Ok(out);
        }

        let semaphore = Arc::new(Semaphore::new(self.max_parallel));
        let mut futs = Vec::new();
        for prompt in prompts.iter() {
            let provider = active_provider.clone();
            let registry = self.child_registry.clone();
            let ctx = ctx.clone();
            let sem = semaphore.clone();
            let prompt = prompt.clone();
            futs.push(async move {
                // Permit ditahan selama anak berjalan; join_all tetap
                // menjadwalkan semua, semaphore yang membatasi paralelnya.
                let _permit = sem.acquire_owned().await.expect("semaphore delegate");
                run_subagent(&provider, &registry, &ctx, &prompt).await
            });
        }
        // join_all mempertahankan urutan input pada hasil.
        let results = futures::future::join_all(futs).await;

        let mut out = String::new();
        for (i, (prompt, res)) in prompts.iter().zip(results).enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            let body = match res {
                Ok((text, usage)) => {
                    // Agregasi usage anak ke sesi induk (Fase 3 store):
                    // dicatat atas nama sesi aktif agar total sesi &
                    // ringkasan per giliran induk ikut menghitung anak.
                    if let (Some(store), Some(sid), Some(u)) = (&ctx.store, &ctx.session_id, usage)
                    {
                        let _ = store.record_usage(
                            sid,
                            active_provider.model_name(),
                            u.input_tokens,
                            u.output_tokens,
                        );
                    }
                    if text.trim().is_empty() {
                        "(subagent selesai tanpa teks jawaban)".to_string()
                    } else {
                        text
                    }
                }
                Err(e) => format!("ERROR: subagent gagal: {e}"),
            };
            out.push_str(&format!("### Tugas {}: {}\n{}", i + 1, short_prompt(prompt), body));
        }
        Ok(out)
    }
}
