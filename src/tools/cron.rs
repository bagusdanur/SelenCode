//! Tool `manage_cron` (v0.5.28): membuka mesin jadwal persisten SQLite (`src/cron.rs`)
//! agar agent dapat melihat, membuat, menjeda, melanjutkan, dan menghapus tugas terjadwal
//! langsung atas permintaan pengguna (selalu lewat persetujuan eksplisit untuk aksi ubah).

use super::{schema, str_arg, Ctx, Tool};
use crate::cron::{
    compute_next_run_after, format_schedule_human, parse_schedule, CRON_GATEWAY_NOTE,
};
use crate::providers::ToolSpec;
use crate::store::Store;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

pub struct ManageCron;

fn open_or_ctx_store(ctx: &Ctx) -> Result<std::sync::Arc<Store>> {
    if let Some(s) = &ctx.store {
        return Ok(std::sync::Arc::clone(s));
    }
    Ok(std::sync::Arc::new(Store::open()?))
}

#[async_trait]
impl Tool for ManageCron {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_cron".into(),
            description: "Kelola jadwal otomatis persisten SelenCode di SQLite (list/add/enable/disable/remove). Job berjalan selama gateway `selen telegram` hidup.".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "enable", "disable", "remove"],
                        "description": "Aksi: list | add | enable | disable | remove"
                    },
                    "name": {
                        "type": "string",
                        "description": "Nama unik job (atau ID untuk enable/disable/remove)"
                    },
                    "schedule": {
                        "type": "string",
                        "description": "Jadwal untuk 'add': interval (`every 5m`, `every 2h`) atau harian (`daily 08:30`)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Instruksi yang dijalankan saat jadwal tiba (wajib untuk 'add')"
                    },
                    "allow_write": {
                        "type": "boolean",
                        "description": "Opsional (default false): izinkan edit/tulis file di cwd"
                    },
                    "allowed_shell": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Opsional (default []): allowlist perintah shell eksplisit untuk job ini"
                    }
                }),
                &["action"],
            ),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        let action = str_arg(args, "action").unwrap_or("");
        !action.eq_ignore_ascii_case("list")
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let action = str_arg(&args, "action")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'action' wajib (list|add|enable|disable|remove)"))?;
        let store = open_or_ctx_store(ctx)?;

        match action.as_str() {
            "list" => {
                let jobs = store.list_cron_jobs()?;
                if jobs.is_empty() {
                    return Ok(format!(
                        "Belum ada jadwal otomatis tersimpan.\n{CRON_GATEWAY_NOTE}"
                    ));
                }
                let mut out = format!("Daftar jadwal otomatis ({} job):\n", jobs.len());
                for j in jobs {
                    let st = if j.enabled { "aktif" } else { "jeda" };
                    let sched = format_schedule_human(&j.schedule_spec);
                    let perms = format!(
                        "write={} · shell={}",
                        if j.allow_write { "ya" } else { "tidak" },
                        if j.allowed_shell.is_empty() {
                            "-".to_string()
                        } else {
                            j.allowed_shell.join(", ")
                        }
                    );
                    out.push_str(&format!(
                        "- #{} `{}` [{st}] · {sched} · next={} · {perms}\n  prompt: {}\n",
                        j.id, j.name, j.next_run_at, j.prompt
                    ));
                }
                out.push_str(CRON_GATEWAY_NOTE);
                Ok(out)
            }
            "add" => {
                let name = str_arg(&args, "name")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'name' wajib untuk action='add'"))?;
                let raw_sched = str_arg(&args, "schedule")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'schedule' wajib untuk action='add'"))?;
                let prompt = str_arg(&args, "prompt")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'prompt' wajib untuk action='add'"))?;
                let parsed = parse_schedule(raw_sched)?;
                let canonical = parsed.canonical_spec();
                let next_run = compute_next_run_after(&canonical, Utc::now())?;
                let allow_write = args
                    .get("allow_write")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let raw_allowed_shell: Vec<String> = args
                    .get("allowed_shell")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .filter(|s| !s.trim().is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                // Validasi pintu masuk tool `manage_cron` (v0.5.32): tolak metakarakter shell sebelum approval / INSERT.
                let allowed_shell = crate::cron::validate_cron_allowlist(&raw_allowed_shell)?;
                let chat_id = ctx.telegram_chat_id.unwrap_or(0);
                let cwd_str = ctx.cwd.to_string_lossy().to_string();

                let detail = format!(
                    "Nama     : {name}\n\
                     Jadwal   : {} ({canonical})\n\
                     Berikut  : {next_run}\n\
                     Folder   : {cwd_str}\n\
                     Izin     : allow_write={allow_write}, allowed_shell={:?}\n\
                     Prompt   : {prompt}\n\
                     {CRON_GATEWAY_NOTE}",
                    parsed.human_label(),
                    allowed_shell
                );
                if !ctx.approver.ask(&format!("manage_cron add '{name}'"), &detail).await {
                    return Ok(format!("Ditolak user — jadwal '{name}' TIDAK dibuat."));
                }

                let rec = store.add_cron_job(
                    name,
                    &canonical,
                    prompt,
                    &cwd_str,
                    &allowed_shell,
                    allow_write,
                    chat_id,
                    &next_run,
                )?;
                Ok(format!(
                    "Jadwal #{} `{}` berhasil dibuat ({} · eksekusi berikut: {}).\n{CRON_GATEWAY_NOTE}",
                    rec.id,
                    rec.name,
                    parsed.human_label(),
                    rec.next_run_at
                ))
            }
            "enable" | "disable" => {
                let target = str_arg(&args, "name")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'name' (nama/ID job) wajib untuk action='{action}'"))?;
                let existing = store
                    .get_cron_job(target)?
                    .ok_or_else(|| anyhow!("jadwal '{target}' tidak ditemukan"))?;
                let enable = action == "enable";
                let next = if enable {
                    Some(compute_next_run_after(&existing.schedule_spec, Utc::now())?)
                } else {
                    None
                };
                if !ctx
                    .approver
                    .ask(
                        &format!("manage_cron {action} '{}'", existing.name),
                        &format!(
                            "Mengubah status jadwal #{} `{}` ({}) -> {}",
                            existing.id,
                            existing.name,
                            format_schedule_human(&existing.schedule_spec),
                            if enable { "AKTIF" } else { "JEDA" }
                        ),
                    )
                    .await
                {
                    return Ok(format!(
                        "Ditolak user — status jadwal '{}' tidak diubah.",
                        existing.name
                    ));
                }
                let updated = store
                    .set_cron_job_enabled(target, enable, next.as_deref())?
                    .ok_or_else(|| anyhow!("jadwal '{target}' tidak ditemukan"))?;
                Ok(format!(
                    "Jadwal #{} `{}` sekarang {} (next: {}).",
                    updated.id,
                    updated.name,
                    if updated.enabled { "AKTIF" } else { "JEDA" },
                    updated.next_run_at
                ))
            }
            "remove" => {
                let target = str_arg(&args, "name")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'name' (nama/ID job) wajib untuk action='remove'"))?;
                let existing = store
                    .get_cron_job(target)?
                    .ok_or_else(|| anyhow!("jadwal '{target}' tidak ditemukan"))?;
                if !ctx
                    .approver
                    .ask(
                        &format!("manage_cron remove '{}'", existing.name),
                        &format!(
                            "Menghapus permanen jadwal #{} `{}` ({})",
                            existing.id,
                            existing.name,
                            format_schedule_human(&existing.schedule_spec)
                        ),
                    )
                    .await
                {
                    return Ok(format!(
                        "Ditolak user — jadwal '{}' tidak dihapus.",
                        existing.name
                    ));
                }
                let removed = store.remove_cron_job(target)?;
                if removed {
                    Ok(format!("Jadwal `{}` berhasil dihapus.", existing.name))
                } else {
                    Ok(format!("Jadwal '{target}' sudah tidak ada."))
                }
            }
            other => Err(anyhow!(
                "action '{other}' tidak dikenal (pilihan: list, add, enable, disable, remove)"
            )),
        }
    }
}
