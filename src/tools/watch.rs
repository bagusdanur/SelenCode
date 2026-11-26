//! Tool `watch_job` (Perbaikan 3 v0.5.7): daftarkan pemantauan tugas asinkron
//! (antrean Comic-auto, unduhan panjang, build latar) ke SQLite agar gateway
//! memantau sampai selesai & melapor otomatis tanpa biaya token LLM.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use crate::watch::{
    compute_watch_expires_at, validate_readonly_watch_command, DEFAULT_WATCH_INTERVAL_SECS,
    DEFAULT_WATCH_MAX_AGE_SECS, MAX_ACTIVE_WATCHES_PER_CHAT, MIN_WATCH_INTERVAL_SECS,
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
            description: "Daftarkan pemantauan otomatis tugas asinkron/antrean hingga selesai (tanpa biaya token). Wajib pakai setelah memasukkan job ke antrean.".into(),
            parameters: schema(
                json!({
                    "label": {
                        "type": "string",
                        "description": "Nama singkat tugas yang dipantau (mis. 'Comic-auto Hold On #6c33')"
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
                &["label"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let store = ctx
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("store SQLite tidak aktif — tidak dapat mendaftarkan watch_job"))?;

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
             Gateway akan melapor otomatis saat tahap berubah, selesai, atau gagal (tanpa biaya token). \
             Kelola via `/watch list` atau `/watch cancel {}`.",
            rec.id,
            rec.label,
            rec.check_kind,
            rec.interval_secs,
            (rec.max_age_secs / 3600).max(1),
            rec.id
        ))
    }
}
