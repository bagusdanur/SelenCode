//! Tool `manage_task` (Tahap 5 — Long-Horizon Task Execution & Checkpoint Lintas Sesi).
//!
//! Menyimpan, memperbarui, menjeda, dan memulihkan state tugas jangka panjang
//! di SQLite (`long_tasks`) terpisah dari riwayat chat, sehingga tugas besar
//! tetap bisa dilanjutkan dengan mulus meski sesi terputus atau berpindah hari.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::store::LongTaskStep;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ManageTask;

#[async_trait]
impl Tool for ManageTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_task".into(),
            description: "Kelola state Long-Horizon Task persisten lintas sesi (create, list, get, checkpoint, update_step, resume_prompt, delete) agar pekerjaan besar tetap punya checkpoint & bisa dilanjutkan di sesi berikutnya.".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["create", "list", "get", "checkpoint", "update_step", "resume_prompt", "delete"],
                        "description": "Aksi manajemen tugas persisten"
                    },
                    "task_id": { "type": "string", "description": "ID tugas (mis. task-a1b2c3d4)" },
                    "goal": { "type": "string", "description": "Tujuan utama tugas (wajib untuk create)" },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "paused", "completed", "failed"],
                        "description": "Status tugas keseluruhan"
                    },
                    "checkpoint_note": { "type": "string", "description": "Ringkasan progress / posisi terakhir untuk pemulihan" },
                    "step_id": { "type": "integer", "description": "Nomor langkah (untuk update_step)" },
                    "step_status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done", "blocked", "failed"],
                        "description": "Status langkah baru (untuk update_step)"
                    },
                    "step_notes": { "type": "string", "description": "Catatan singkat hasil langkah (untuk update_step)" },
                    "token_budget": { "type": "integer", "description": "Batas budget token (opsional, default 200000)" },
                    "steps": {
                        "type": "array",
                        "description": "Daftar langkah kustom saat create (opsional)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "role": { "type": "string" }
                            },
                            "required": ["title"]
                        }
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let store = ctx
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("store SQLite tidak aktif pada sesi ini"))?;
        let action = str_arg(&args, "action").ok_or_else(|| anyhow!("argumen 'action' wajib diisi"))?;
        let cwd_str = ctx.cwd.to_string_lossy().to_string();

        match action {
            "create" => {
                let goal = str_arg(&args, "goal")
                    .ok_or_else(|| anyhow!("argumen 'goal' wajib diisi untuk action='create'"))?;
                let budget = args
                    .get("token_budget")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(200_000);
                let mut steps = Vec::new();
                if let Some(arr) = args.get("steps").and_then(|v| v.as_array()) {
                    for (idx, item) in arr.iter().enumerate() {
                        if let Some(title) = item.get("title").and_then(|v| v.as_str()) {
                            let role = item
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("coder")
                                .to_string();
                            steps.push(LongTaskStep {
                                id: idx + 1,
                                title: title.trim().to_string(),
                                role,
                                status: "pending".to_string(),
                                notes: String::new(),
                            });
                        }
                    }
                }
                let rec = store.create_long_task(&cwd_str, goal, &steps, budget)?;
                Ok(format!(
                    "Long-Horizon Task dibuat: {} ({} langkah, budget {} token)\nGoal: {}",
                    rec.id,
                    rec.steps.len(),
                    rec.token_budget,
                    rec.goal
                ))
            }
            "list" => {
                let list = store.list_long_tasks(&cwd_str, 20)?;
                if list.is_empty() {
                    return Ok("Belum ada Long-Horizon Task tersimpan di folder ini.".to_string());
                }
                let mut out = format!("Long-Horizon Tasks ({}):\n", list.len());
                for t in list {
                    out.push_str(&format!(
                        "- {} [{}] {}/{} langkah · {}/{} token — {}\n",
                        t.id,
                        t.status,
                        t.completed_steps(),
                        t.steps.len(),
                        t.tokens_used,
                        t.token_budget,
                        t.goal
                    ));
                }
                Ok(out.trim_end().to_string())
            }
            "get" => {
                let tid = str_arg(&args, "task_id")
                    .ok_or_else(|| anyhow!("argumen 'task_id' wajib diisi"))?;
                let rec = store
                    .get_long_task(tid)?
                    .ok_or_else(|| anyhow!("task '{tid}' tidak ditemukan"))?;
                Ok(rec.build_resume_prompt())
            }
            "resume_prompt" => {
                let tid = str_arg(&args, "task_id")
                    .ok_or_else(|| anyhow!("argumen 'task_id' wajib diisi"))?;
                let rec = store
                    .get_long_task(tid)?
                    .ok_or_else(|| anyhow!("task '{tid}' tidak ditemukan"))?;
                Ok(rec.build_resume_prompt())
            }
            "checkpoint" => {
                let tid = str_arg(&args, "task_id")
                    .ok_or_else(|| anyhow!("argumen 'task_id' wajib diisi"))?;
                let st = str_arg(&args, "status");
                let note = str_arg(&args, "checkpoint_note");
                let rec = store
                    .save_long_task_checkpoint(tid, st, None, note, 0)?
                    .ok_or_else(|| anyhow!("task '{tid}' tidak ditemukan"))?;
                Ok(format!(
                    "Checkpoint tersimpan untuk {} [{}]: {}",
                    rec.id, rec.status, rec.checkpoint_note
                ))
            }
            "update_step" => {
                let tid = str_arg(&args, "task_id")
                    .ok_or_else(|| anyhow!("argumen 'task_id' wajib diisi"))?;
                let step_id = args
                    .get("step_id")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .ok_or_else(|| anyhow!("argumen 'step_id' (angka) wajib diisi"))?;
                let step_status = str_arg(&args, "step_status")
                    .ok_or_else(|| anyhow!("argumen 'step_status' wajib diisi"))?;
                let step_notes = str_arg(&args, "step_notes");
                let mut rec = store
                    .get_long_task(tid)?
                    .ok_or_else(|| anyhow!("task '{tid}' tidak ditemukan"))?;
                let Some(step) = rec.steps.iter_mut().find(|s| s.id == step_id) else {
                    return Err(anyhow!("langkah #{step_id} tidak ditemukan di task '{tid}'"));
                };
                step.status = step_status.trim().to_ascii_lowercase();
                if let Some(n) = step_notes {
                    step.notes = crate::security::redact_secrets(n.trim());
                }
                let all_done = !rec.steps.is_empty() && rec.steps.iter().all(|s| s.status == "done");
                let overall = if all_done { "completed" } else { "in_progress" };
                let saved = store
                    .save_long_task_checkpoint(
                        tid,
                        Some(overall),
                        Some(&rec.steps),
                        Some(&format!("Langkah #{step_id} -> {step_status}")),
                        0,
                    )?
                    .ok_or_else(|| anyhow!("gagal menyimpan update task '{tid}'"))?;
                Ok(format!(
                    "Task {} diperbarui: langkah #{step_id} = {} (total {}/{} selesai, status: {}).",
                    saved.id,
                    step_status,
                    saved.completed_steps(),
                    saved.steps.len(),
                    saved.status
                ))
            }
            "delete" => {
                let tid = str_arg(&args, "task_id")
                    .ok_or_else(|| anyhow!("argumen 'task_id' wajib diisi"))?;
                if store.delete_long_task(tid)? {
                    Ok(format!("Task '{tid}' berhasil dihapus."))
                } else {
                    Err(anyhow!("task '{tid}' tidak ditemukan"))
                }
            }
            other => Err(anyhow!("action '{other}' tidak dikenal untuk manage_task")),
        }
    }
}
