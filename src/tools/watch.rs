//! Tool `watch_job` (Perbaikan 3 v0.5.7 + Perbaikan 2 v0.5.10):
//! - `action="register"` (default): daftarkan pemantauan tugas asinkron (antrean baru maupun
//!   pengulangan job gagal) ke SQLite agar gateway/CLI memantau progres, selesai, & gagal otomatis.
//! - `action="check"`: jalankan pemeriksaan SEGAR langsung ke sumber kebenaran saat ini juga
//!   (memperbarui `last_checked_at`/`last_stage`/`last_progress` di SQLite dan mengembalikan status
//!   bertanggal atau pengakuan jujur bila cek gagal).
//! - `action="list"`: lihat daftar pantauan beserta cap waktu pemeriksaan terakhir.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use crate::watch::{
    compute_watch_expires_at, evaluate_watch_poll, format_watch_fresh_status, normalize_watch_cwd,
    persist_watch_transition, poll_watch_once, validate_readonly_watch_command,
    DEFAULT_WATCH_INTERVAL_SECS, DEFAULT_WATCH_MAX_AGE_SECS, MAX_ACTIVE_WATCHES_PER_CHAT,
    MIN_WATCH_INTERVAL_SECS,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

pub struct WatchJob;

#[async_trait]
impl Tool for WatchJob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "watch_job".into(),
            description: "Daftarkan pantauan job asinkron (`action=\"register\"`, default; wajib saat antrekan/ulang job), cek status SEGAR sekarang (`action=\"check\"`), atau lihat daftar bertanggal (`action=\"list\"`).".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["register", "check", "list"],
                        "description": "Aksi: 'register' (default, daftar pantauan baru), 'check' (cek segar sumber kebenaran sekarang), atau 'list' (daftar watch bertanggal)"
                    },
                    "watch_id": {
                        "type": "integer",
                        "description": "Opsional untuk action='check': ID pantauan tertentu yang ingin dicek segar"
                    },
                    "label": {
                        "type": "string",
                        "description": "Wajib untuk action='register': nama singkat tugas yang dipantau (mis. 'Comic-auto Hold On #6c33')"
                    },
                    "check_command": {
                        "type": "string",
                        "description": "Perintah shell BACA-SAJA untuk cek status (mis. 'python job_queue.py show 6c33')"
                    },
                    "status_file": {
                        "type": "string",
                        "description": "Alternatif check_command: path berkas status di dalam sandbox"
                    },
                    "done_marker": {
                        "type": "string",
                        "description": "Opsional: teks penanda selesai (default deteksi otomatis DONE/SELESAI/COMPLETED)"
                    },
                    "fail_marker": {
                        "type": "string",
                        "description": "Opsional: teks penanda gagal (default deteksi otomatis FAILED/GAGAL/ERROR)"
                    },
                    "interval_secs": {
                        "type": "integer",
                        "description": "Interval cek dalam detik (default 60, minimum 30)"
                    },
                    "max_age_secs": {
                        "type": "integer",
                        "description": "Batas umur pantauan dalam detik (default 21600 = 6 jam)"
                    }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let store = ctx
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("store SQLite tidak aktif — tidak dapat memakai watch_job"))?;

        let action = str_arg(&args, "action")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "register".to_string());

        let target_cwd = normalize_watch_cwd(&ctx.cwd.to_string_lossy());

        if action == "list" {
            let watches = store.list_job_watches(ctx.telegram_chat_id, false)?;
            let matching: Vec<_> = watches
                .into_iter()
                .filter(|w| normalize_watch_cwd(&w.cwd) == target_cwd)
                .take(10)
                .collect();
            if matching.is_empty() {
                return Ok(
                    "Belum ada pantauan terdaftar di folder ini. Untuk menjawab status job, jalankan pemeriksaan langsung terhadap sumber kebenaran (CLI/file status).".to_string(),
                );
            }
            let mut out = format!("Daftar watch di folder ini ({}):\n", matching.len());
            for w in &matching {
                let checked = if w.last_checked_at.trim().is_empty() {
                    "belum pernah dicek".to_string()
                } else {
                    format!(
                        "{} UTC",
                        w.last_checked_at.chars().take(19).collect::<String>()
                    )
                };
                let stage = if w.last_stage.is_empty() {
                    "-".to_string()
                } else if w.last_progress.is_empty() {
                    w.last_stage.clone()
                } else {
                    format!("{} ({})", w.last_stage, w.last_progress)
                };
                out.push_str(&format!(
                    "- #{} '{}' | status={} | tahap={} | terakhir dicek: {}\n",
                    w.id, w.label, w.status, stage, checked
                ));
            }
            out.push_str("Catatan: panggil `watch_job(action=\"check\")` untuk pemeriksaan segar sebelum melaporkan status terbaru ke user.");
            return Ok(out);
        }

        if action == "check" {
            let watch_id = args.get("watch_id").and_then(|v| v.as_i64());
            let all = store.list_job_watches(ctx.telegram_chat_id, false)?;
            let targets: Vec<_> = if let Some(wid) = watch_id {
                all.into_iter().filter(|w| w.id == wid).collect()
            } else {
                let mut active: Vec<_> = all
                    .iter()
                    .filter(|w| w.status == "active" && normalize_watch_cwd(&w.cwd) == target_cwd)
                    .cloned()
                    .collect();
                if active.is_empty() {
                    if let Some(latest) = all
                        .into_iter()
                        .find(|w| normalize_watch_cwd(&w.cwd) == target_cwd)
                    {
                        active.push(latest);
                    }
                }
                active
            };

            if targets.is_empty() {
                return Ok(
                    "Tidak ada pantauan terdaftar yang cocok di folder ini. Jalankan pemeriksaan segar langsung lewat sumber kebenaran (CLI antrean / file status) sebelum menjawab status pekerjaan.".to_string(),
                );
            }

            let mut blocks = Vec::new();
            for w in targets {
                let now_utc = chrono::Utc::now();
                let checked_at = now_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let poll_out = poll_watch_once(&w, &ctx.sandbox).await;
                let poll_res = match &poll_out {
                    Ok(s) => Ok(s.as_str()),
                    Err(e) => Err(e.to_string()),
                };
                let res_ref = match &poll_res {
                    Ok(s) => Ok(*s),
                    Err(e) => Err(e.as_str()),
                };
                if w.status == "active" {
                    let tr = evaluate_watch_poll(&w, res_ref, now_utc);
                    let _ = persist_watch_transition(store, w.id, &tr, &checked_at, true);
                }
                blocks.push(format_watch_fresh_status(&w, res_ref, &checked_at));
            }
            return Ok(blocks.join("\n\n"));
        }

        if action != "register" {
            return Err(anyhow!(
                "action '{action}' tidak dikenal untuk watch_job (gunakan: register, check, list)"
            ));
        }

        let label = str_arg(&args, "label")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'label' wajib diisi"))?;

        let check_cmd = str_arg(&args, "check_command")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let status_file = str_arg(&args, "status_file")
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let (check_kind, check_target) = match (check_cmd, status_file) {
            (Some(cmd), _) => ("shell", validate_readonly_watch_command(cmd)?),
            (None, Some(path_str)) => {
                let p = resolve_in_sandbox(&ctx.cwd, Path::new(path_str))?;
                if is_secret_file(&p) {
                    return Err(anyhow!("status_file '{path_str}' ditolak: file rahasia"));
                }
                ("file", path_str.to_string())
            }
            (None, None) => {
                return Err(anyhow!(
                    "wajib isi salah satu: 'check_command' (perintah baca-saja) atau 'status_file'"
                ));
            }
        };

        let chat_id = ctx.telegram_chat_id.unwrap_or(0);
        let active_count = store.count_active_job_watches(chat_id)?;
        if active_count >= MAX_ACTIVE_WATCHES_PER_CHAT {
            return Err(anyhow!(
                "batas maksimum {MAX_ACTIVE_WATCHES_PER_CHAT} pantauan aktif per chat tercapai — hentikan salah satu lewat `/watch cancel <id>`"
            ));
        }

        let interval_secs = args
            .get("interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WATCH_INTERVAL_SECS)
            .max(MIN_WATCH_INTERVAL_SECS);
        let max_age_secs = args
            .get("max_age_secs")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_WATCH_MAX_AGE_SECS);
        let done_marker = str_arg(&args, "done_marker").unwrap_or("").trim();
        let fail_marker = str_arg(&args, "fail_marker").unwrap_or("").trim();
        let expires_at = compute_watch_expires_at(chrono::Utc::now(), max_age_secs);
        let cwd_str = ctx.cwd.to_string_lossy().to_string();

        let rec = store.add_job_watch(
            label,
            chat_id,
            &cwd_str,
            check_kind,
            &check_target,
            done_marker,
            fail_marker,
            interval_secs,
            max_age_secs,
            &expires_at,
        )?;

        Ok(format!(
            "Pemantauan #{} terdaftar untuk '{}' (metode: {}, interval {} dtk, batas {} jam). \
             Progres/selesai/gagal akan dilaporkan otomatis oleh gateway maupun saat sesi CLI/TUI berikutnya dibuka (tanpa biaya token). \
             Kelola via `/watch list`, cek segar via `watch_job(action=\"check\", watch_id={})`, atau batalkan via `/watch cancel {}`.",
            rec.id,
            rec.label,
            rec.check_kind,
            rec.interval_secs,
            (rec.max_age_secs / 3600).max(1),
            rec.id,
            rec.id
        ))
    }
}
