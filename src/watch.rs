//! Pemantau tugas asinkron (`watch_job`, Perbaikan 3 v0.5.7).
//!
//! Loop pemantau dimiliki oleh gateway (persisten di SQLite tabel `job_watches` dan
//! dilanjutkan otomatis sesudah gateway restart) — TANPA memanggil model LLM sama sekali
//! (biaya token provider = 0). Laporan disusun dari templat bidang status:
//! - Tahap/status berubah -> 1 pesan ringkas (label, tahap, progres).
//! - Selesai -> 1 pesan laporan akhir (label, hasil ringkas, lokasi keluaran) & berhenti.
//! - Gagal (status gagal ATAU pemeriksaan gagal 3x berturut) -> 1 pesan kegagalan & berhenti.
//! - Kedaluwarsa -> 1 pesan penutup & berhenti.
//! - Tidak berubah -> 0 pesan (anti-spam ketat).

use crate::sandbox::SandboxController;
use crate::security::{is_forbidden_command, is_secret_file, redact_secrets, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_WATCH_INTERVAL_SECS: u64 = 60;
pub const MIN_WATCH_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_WATCH_MAX_AGE_SECS: u64 = 6 * 3600;
pub const MAX_ACTIVE_WATCHES_PER_CHAT: usize = 5;
pub const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobWatchRecord {
    pub id: i64,
    pub label: String,
    pub chat_id: i64,
    pub cwd: String,
    /// `"shell"` atau `"file"`
    pub check_kind: String,
    /// Perintah shell baca-saja ATAU path berkas status relatif/absolut dalam sandbox.
    pub check_target: String,
    pub done_marker: String,
    pub fail_marker: String,
    pub interval_secs: u64,
    pub max_age_secs: u64,
    /// `"active"` | `"done"` | `"failed"` | `"expired"` | `"cancelled"`
    pub status: String,
    pub last_stage: String,
    pub last_progress: String,
    pub consecutive_errors: u32,
    pub created_at: String,
    pub expires_at: String,
    pub last_checked_at: String,
    pub final_report: String,
    pub delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchStateKind {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWatchStatus {
    pub kind: WatchStateKind,
    pub stage: String,
    pub progress: String,
    pub summary: String,
    pub output_location: String,
    pub error_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTransition {
    pub new_status: String,
    pub new_stage: String,
    pub new_progress: String,
    pub new_consecutive_errors: u32,
    /// `Some(msg)` jika WAJIB mengirim pesan baru; `None` jika tidak ada perubahan (anti-spam).
    pub notification: Option<String>,
    pub terminal: bool,
}

/// Validasi bahwa perintah shell pemeriksaan status benar-benar baca-saja & aman.
pub fn validate_readonly_watch_command(cmd: &str) -> Result<String> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("perintah pemeriksaan status tidak boleh kosong"));
    }
    if is_forbidden_command(trimmed) {
        return Err(anyhow!(
            "perintah pemeriksaan ditolak demi keamanan: {trimmed}"
        ));
    }
    // Tolak chaining / subshell / redirect / penulisan eksplisit agar loop pantau 100% baca-saja.
    if trimmed.contains([';', '|', '&', '`', '>', '<', '\n', '\r']) || trimmed.contains("$(") {
        return Err(anyhow!(
            "perintah pemeriksaan `watch_job` wajib tunggal & baca-saja tanpa operator shell (;, |, &, >, <, $())"
        ));
    }
    let low = trimmed.to_ascii_lowercase();
    for bad in [
        "rm ", "del ", "mv ", "cp ", "mkdir ", "rmdir ", "touch ", "chmod ", "chown ", "dd ",
        "tee ", "kill ", "pkill ", "shutdown", "reboot", "git push", "git commit", "git reset",
    ] {
        if low.starts_with(bad) || low.contains(&format!(" {bad}")) {
            return Err(anyhow!(
                "perintah `{trimmed}` mengandung aksi tulis/mutasi — `watch_job` hanya mengizinkan perintah baca-saja"
            ));
        }
    }
    Ok(trimmed.to_string())
}

/// Parse keluaran teks atau JSON dari perintah pemeriksaan / berkas status secara deterministik
/// (0 panggilan LLM, 0 token).
pub fn parse_watch_output(raw: &str, done_marker: &str, fail_marker: &str) -> ParsedWatchStatus {
    let cleaned = redact_secrets(raw.trim());
    let mut stage = String::new();
    let mut progress = String::new();
    let mut summary = String::new();
    let mut output_location = String::new();
    let mut error_text = String::new();
    let mut status_field = String::new();

    // 1. Coba parse sebagai JSON object bila berbentuk `{...}`.
    if cleaned.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&cleaned) {
            if let Some(obj) = val.as_object() {
                let pick = |keys: &[&str]| -> String {
                    for k in keys {
                        if let Some(v) = obj.get(*k) {
                            if let Some(s) = v.as_str() {
                                if !s.trim().is_empty() {
                                    return s.trim().to_string();
                                }
                            } else if !v.is_null() {
                                return v.to_string();
                            }
                        }
                    }
                    String::new()
                };
                status_field = pick(&["status", "state"]);
                stage = pick(&["stage", "tahap", "phase", "step"]);
                progress = pick(&["progress", "progres", "percent", "count"]);
                summary = pick(&["summary", "result", "hasil", "message"]);
                output_location = pick(&["output", "output_path", "location", "lokasi", "path", "file"]);
                error_text = pick(&["error", "err", "reason", "detail"]);
            }
        }
    }

    // 2. Parse baris key=value / key: value (seperti `STATUS=PROCESSING`, `STAGE=REMOVAL`, `PROGRESS=3/22`).
    for line in cleaned.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let pair = l.split_once('=').or_else(|| l.split_once(':'));
        if let Some((k, v)) = pair {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if val.is_empty() {
                continue;
            }
            match key.as_str() {
                "status" | "state" if status_field.is_empty() => status_field = val.to_string(),
                "stage" | "tahap" | "phase" | "step" if stage.is_empty() => stage = val.to_string(),
                "progress" | "progres" if progress.is_empty() => progress = val.to_string(),
                "summary" | "result" | "hasil" | "message" if summary.is_empty() => {
                    summary = val.to_string()
                }
                "output" | "output_path" | "location" | "lokasi" | "file"
                    if output_location.is_empty() =>
                {
                    output_location = val.to_string()
                }
                "error" | "err" | "reason" if error_text.is_empty() => {
                    error_text = val.to_string()
                }
                _ => {}
            }
        }
    }

    // Fallback bila teks polos 1–2 baris (mis. `S1` atau `SELESAI /tmp/out.zip`).
    let first_line = cleaned
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .unwrap_or("");
    if stage.is_empty() && !status_field.is_empty() {
        stage = status_field.clone();
    }
    if stage.is_empty() && !first_line.is_empty() {
        stage = first_line.chars().take(80).collect();
    }
    if summary.is_empty() && !first_line.is_empty() {
        summary = first_line.chars().take(160).collect();
    }

    let check_haystack = format!("{status_field}\n{stage}\n{cleaned}");
    let upper = check_haystack.to_ascii_uppercase();

    let fail_hit = if !fail_marker.trim().is_empty() {
        check_haystack
            .to_ascii_lowercase()
            .contains(&fail_marker.trim().to_ascii_lowercase())
    } else {
        false
    } || matches!(
        status_field.trim().to_ascii_uppercase().as_str(),
        "FAILED" | "FAIL" | "ERROR" | "GAGAL" | "CRASHED" | "ABORTED"
    ) || upper.lines().any(|l| {
        let t = l.trim();
        t == "FAILED"
            || t == "GAGAL"
            || t == "ERROR"
            || t.starts_with("FAILED:")
            || t.starts_with("FAILED ")
            || t.starts_with("GAGAL:")
            || t.starts_with("GAGAL ")
            || t.starts_with("STATUS=FAILED")
            || t.starts_with("STATUS=GAGAL")
            || t.starts_with("STATUS=ERROR")
    });

    let done_hit = if !done_marker.trim().is_empty() {
        check_haystack
            .to_ascii_lowercase()
            .contains(&done_marker.trim().to_ascii_lowercase())
    } else {
        false
    } || matches!(
        status_field.trim().to_ascii_uppercase().as_str(),
        "DONE" | "SELESAI" | "COMPLETED" | "COMPLETE" | "SUCCESS" | "SUCCEEDED" | "FINISHED" | "OK"
    ) || upper.lines().any(|l| {
        let t = l.trim();
        t == "DONE"
            || t == "SELESAI"
            || t == "COMPLETED"
            || t == "SUCCESS"
            || t.starts_with("SELESAI ")
            || t.starts_with("SELESAI:")
            || t.starts_with("DONE ")
            || t.starts_with("DONE:")
            || t.starts_with("STATUS=DONE")
            || t.starts_with("STATUS=SELESAI")
            || t.starts_with("STATUS=COMPLETED")
    });

    let kind = if fail_hit {
        if error_text.is_empty() {
            error_text = cleaned.chars().take(300).collect();
        }
        WatchStateKind::Failed
    } else if done_hit {
        WatchStateKind::Done
    } else {
        WatchStateKind::Running
    };

    ParsedWatchStatus {
        kind,
        stage: stage.chars().take(80).collect(),
        progress: progress.chars().take(60).collect(),
        summary: summary.chars().take(240).collect(),
        output_location: output_location.chars().take(200).collect(),
        error_text: error_text.chars().take(300).collect(),
    }
}

/// Evaluasi satu hasil polling terhadap state `watch` sebelumnya.
/// Menerapkan aturan ketat:
/// - 0 panggilan LLM
/// - Anti-spam: tidak ada perubahan -> `notification = None`
/// - Gagal 3x berturut -> status `failed` + laporan kegagalan
/// - Kedaluwarsa -> status `expired` + 1 pesan penutup
pub fn evaluate_watch_poll(
    watch: &JobWatchRecord,
    poll_res: Result<&str, &str>,
    now_utc: DateTime<Utc>,
) -> WatchTransition {
    // 1. Cek kedaluwarsa terlebih dahulu.
    let expired = DateTime::parse_from_rfc3339(&watch.expires_at)
        .map(|dt| now_utc >= dt.with_timezone(&Utc))
        .unwrap_or(false);
    if expired {
        let msg = format!(
            "⏱ Pemantauan '{}' (#{}) berakhir karena batas waktu ({} jam) tercapai.\nTahap terakhir: {}",
            watch.label,
            watch.id,
            (watch.max_age_secs / 3600).max(1),
            if watch.last_stage.is_empty() {
                "belum diketahui"
            } else {
                &watch.last_stage
            }
        );
        return WatchTransition {
            new_status: "expired".to_string(),
            new_stage: watch.last_stage.clone(),
            new_progress: watch.last_progress.clone(),
            new_consecutive_errors: watch.consecutive_errors,
            notification: Some(msg),
            terminal: true,
        };
    }

    // 2. Tangani kegagalan perintah/baca status (3x berturut -> hentikan & laporkan error).
    let raw = match poll_res {
        Ok(s) => s,
        Err(err_msg) => {
            let errs = watch.consecutive_errors.saturating_add(1);
            if errs >= MAX_CONSECUTIVE_POLL_ERRORS {
                let clean_err = redact_secrets(err_msg.trim());
                let msg = format!(
                    "❌ Tugas '{}' (#{}) gagal dipantau ({}x pemeriksaan gagal berturut-turut).\nError: {}",
                    watch.label,
                    watch.id,
                    errs,
                    clean_err.chars().take(280).collect::<String>()
                );
                return WatchTransition {
                    new_status: "failed".to_string(),
                    new_stage: "error".to_string(),
                    new_progress: watch.last_progress.clone(),
                    new_consecutive_errors: errs,
                    notification: Some(msg),
                    terminal: true,
                };
            }
            return WatchTransition {
                new_status: "active".to_string(),
                new_stage: watch.last_stage.clone(),
                new_progress: watch.last_progress.clone(),
                new_consecutive_errors: errs,
                notification: None,
                terminal: false,
            };
        }
    };

    let parsed = parse_watch_output(raw, &watch.done_marker, &watch.fail_marker);
    match parsed.kind {
        WatchStateKind::Done => {
            let mut msg = format!("✅ Tugas '{}' (#{}) selesai.", watch.label, watch.id);
            if !parsed.summary.is_empty() {
                msg.push_str(&format!("\nHasil: {}", parsed.summary));
            }
            if !parsed.output_location.is_empty() {
                msg.push_str(&format!("\nLokasi: {}", parsed.output_location));
            }
            WatchTransition {
                new_status: "done".to_string(),
                new_stage: if parsed.stage.is_empty() {
                    "selesai".to_string()
                } else {
                    parsed.stage
                },
                new_progress: parsed.progress,
                new_consecutive_errors: 0,
                notification: Some(msg),
                terminal: true,
            }
        }
        WatchStateKind::Failed => {
            let err_txt = if parsed.error_text.is_empty() {
                parsed.summary.clone()
            } else {
                parsed.error_text.clone()
            };
            let msg = format!(
                "❌ Tugas '{}' (#{}) gagal.\nError: {}",
                watch.label,
                watch.id,
                if err_txt.is_empty() {
                    "(tanpa detail error)"
                } else {
                    &err_txt
                }
            );
            WatchTransition {
                new_status: "failed".to_string(),
                new_stage: if parsed.stage.is_empty() {
                    "gagal".to_string()
                } else {
                    parsed.stage
                },
                new_progress: parsed.progress,
                new_consecutive_errors: 0,
                notification: Some(msg),
                terminal: true,
            }
        }
        WatchStateKind::Running => {
            let changed =
                parsed.stage != watch.last_stage || parsed.progress != watch.last_progress;
            if !changed {
                return WatchTransition {
                    new_status: "active".to_string(),
                    new_stage: watch.last_stage.clone(),
                    new_progress: watch.last_progress.clone(),
                    new_consecutive_errors: 0,
                    notification: None,
                    terminal: false,
                };
            }
            let mut msg = format!("⏳ Progres '{}': {}", watch.label, parsed.stage);
            if !parsed.progress.is_empty() {
                msg.push_str(&format!(" ({})", parsed.progress));
            }
            WatchTransition {
                new_status: "active".to_string(),
                new_stage: parsed.stage,
                new_progress: parsed.progress,
                new_consecutive_errors: 0,
                notification: Some(msg),
                terminal: false,
            }
        }
    }
}

/// Cek apakah satu `JobWatchRecord` sudah jatuh tempo untuk diperiksa ulang pada `now_utc`.
pub fn is_watch_due(watch: &JobWatchRecord, now_utc: DateTime<Utc>) -> bool {
    if watch.status != "active" {
        return false;
    }
    if watch.last_checked_at.trim().is_empty() {
        return true;
    }
    let Ok(last) = DateTime::parse_from_rfc3339(&watch.last_checked_at) else {
        return true;
    };
    let interval = Duration::seconds(watch.interval_secs.max(MIN_WATCH_INTERVAL_SECS) as i64);
    now_utc >= last.with_timezone(&Utc) + interval
}

/// Jalankan 1 kali pemeriksaan baca-saja untuk satu `JobWatchRecord` (berkas atau perintah shell).
pub async fn poll_watch_once(
    watch: &JobWatchRecord,
    sandbox: &SandboxController,
) -> Result<String> {
    let cwd = Path::new(&watch.cwd);
    if watch.check_kind == "file" {
        let p = resolve_in_sandbox(cwd, Path::new(&watch.check_target))?;
        if is_secret_file(&p) {
            return Err(anyhow!("berkas status ditolak: file rahasia"));
        }
        let text = std::fs::read_to_string(&p)
            .map_err(|e| anyhow!("gagal membaca berkas status '{}': {e}", watch.check_target))?;
        return Ok(text);
    }

    let safe_cmd = validate_readonly_watch_command(&watch.check_target)?;
    let mut cmd = sandbox.prepare_shell_command("watch_job", &safe_cmd, cwd)?;
    let (code, output) = crate::tools::run_timed(&mut cmd, 20).await?;
    if code.unwrap_or(-1) != 0 {
        let snippet: String = output.trim().chars().take(240).collect();
        return Err(anyhow!(
            "exit code {} — {}",
            code.unwrap_or(-1),
            if snippet.is_empty() {
                "tanpa output"
            } else {
                &snippet
            }
        ));
    }
    Ok(output)
}

/// Hitung timestamp kedaluwarsa RFC3339 dari `now_utc + max_age_secs`.
pub fn compute_watch_expires_at(now_utc: DateTime<Utc>, max_age_secs: u64) -> String {
    let secs = max_age_secs.max(60) as i64;
    (now_utc + Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
