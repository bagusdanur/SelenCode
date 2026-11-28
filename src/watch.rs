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
    // Tolak chaining / subshell / redirect / ekspansi variabel agar loop pantau 100% baca-saja.
    if trimmed.contains([';', '|', '&', '`', '>', '<', '\n', '\r'])
        || trimmed.contains("$(")
        || trimmed.contains("${")
    {
        return Err(anyhow!(
            "perintah pemeriksaan `watch_job` wajib tunggal & baca-saja tanpa operator shell (;, |, &, >, <, $())"
        ));
    }
    let low = trimmed.to_ascii_lowercase();
    let tokens: Vec<&str> = low.split_whitespace().collect();
    if let Some(first) = tokens.first() {
        let bin = first.rsplit(['/', '\\']).next().unwrap_or(first).trim_matches(['"', '\'']);
        if matches!(
            bin,
            "rm" | "rm.exe"
                | "del"
                | "mv"
                | "cp"
                | "mkdir"
                | "rmdir"
                | "rd"
                | "touch"
                | "chmod"
                | "chown"
                | "dd"
                | "tee"
                | "kill"
                | "pkill"
                | "shutdown"
                | "reboot"
                | "sh"
                | "bash"
                | "zsh"
                | "dash"
                | "cmd"
                | "cmd.exe"
                | "powershell"
                | "powershell.exe"
                | "pwsh"
                | "curl"
                | "wget"
                | "nc"
                | "ncat"
                | "eval"
        ) {
            return Err(anyhow!(
                "perintah `{trimmed}` ditolak: `{bin}` tidak diizinkan untuk pemeriksaan baca-saja `watch_job`"
            ));
        }
        // Tolak eksekusi kode inline interpreter (`python -c`, `node -e`, `perl -e`, `ruby -e`) dan `find -exec`.
        if matches!(bin, "python" | "python3" | "python.exe" | "node" | "node.exe" | "perl" | "ruby")
            && tokens.iter().any(|t| matches!(*t, "-c" | "-e" | "--eval"))
        {
            return Err(anyhow!(
                "perintah `{trimmed}` ditolak: eksekusi kode inline interpreter (-c/-e) tidak diizinkan pada `watch_job`"
            ));
        }
        if bin == "find" && tokens.iter().any(|t| *t == "-exec" || *t == "-execdir" || *t == "-delete") {
            return Err(anyhow!(
                "perintah `{trimmed}` ditolak: `find -exec/-delete` tidak diizinkan pada `watch_job`"
            ));
        }
    }
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

fn pick_json_str(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> String {
    for k in keys {
        if let Some(v) = obj.get(*k) {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty()
                    && !t.eq_ignore_ascii_case("null")
                    && !t.eq_ignore_ascii_case("none")
                {
                    return t.to_string();
                }
            } else if v.is_number() || v.is_boolean() {
                return v.to_string();
            }
        }
    }
    String::new()
}

fn fill_from_json_obj(
    obj: &serde_json::Map<String, serde_json::Value>,
    status_field: &mut String,
    stage: &mut String,
    progress: &mut String,
    summary: &mut String,
    output_location: &mut String,
    error_text: &mut String,
) {
    if status_field.is_empty() {
        *status_field = pick_json_str(obj, &["status", "state", "job_status"]);
    }
    if stage.is_empty() {
        *stage = pick_json_str(
            obj,
            &[
                "stage",
                "current_stage",
                "active_stage",
                "tahap",
                "phase",
                "step",
                "current_step",
            ],
        );
    }
    if progress.is_empty() {
        *progress = pick_json_str(
            obj,
            &["progress", "progres", "percent", "pct", "pages", "count"],
        );
    }
    if progress.is_empty() {
        let done = pick_json_str(
            obj,
            &[
                "completed_pages",
                "done_pages",
                "processed_pages",
                "current_page",
                "completed",
                "done",
            ],
        );
        let total = pick_json_str(obj, &["total_pages", "pages_total", "total"]);
        if !done.is_empty() && !total.is_empty() {
            *progress = format!("{done}/{total}");
        }
    }
    if summary.is_empty() {
        *summary = pick_json_str(obj, &["summary", "result", "hasil", "message", "msg", "note"]);
    }
    if output_location.is_empty() {
        *output_location = pick_json_str(
            obj,
            &[
                "output",
                "output_path",
                "output_dir",
                "location",
                "lokasi",
                "path",
                "file",
                "artifact",
            ],
        );
    }
    if error_text.is_empty() {
        let candidate = pick_json_str(
            obj,
            &[
                "error",
                "error_message",
                "err",
                "reason",
                "failure_reason",
                "last_error",
                "detail",
                "exception",
            ],
        );
        if !candidate.is_empty()
            && candidate != "-"
            && candidate != "0"
            && !candidate.eq_ignore_ascii_case("false")
        {
            *error_text = candidate;
        }
    }
}

/// Parse keluaran teks atau JSON dari perintah pemeriksaan / berkas status secara deterministik
/// (0 panggilan LLM, 0 token). Mendukung JSON datar & bersarang (`{"job": {...}}`), key=value,
/// serta deteksi kegagalan kelas satu (v0.5.10 Perbaikan 2).
pub fn parse_watch_output(raw: &str, done_marker: &str, fail_marker: &str) -> ParsedWatchStatus {
    let cleaned = redact_secrets(raw.trim());
    let mut stage = String::new();
    let mut progress = String::new();
    let mut summary = String::new();
    let mut output_location = String::new();
    let mut error_text = String::new();
    let mut status_field = String::new();

    // 1. Coba parse sebagai JSON object bila berbentuk `{...}` (termasuk wrapper `job`/`data`/`task`/`result`).
    if cleaned.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&cleaned) {
            if let Some(obj) = val.as_object() {
                fill_from_json_obj(
                    obj,
                    &mut status_field,
                    &mut stage,
                    &mut progress,
                    &mut summary,
                    &mut output_location,
                    &mut error_text,
                );
                for nested_key in ["job", "data", "task", "result", "payload", "metadata"] {
                    if let Some(nested) = obj.get(nested_key).and_then(|v| v.as_object()) {
                        fill_from_json_obj(
                            nested,
                            &mut status_field,
                            &mut stage,
                            &mut progress,
                            &mut summary,
                            &mut output_location,
                            &mut error_text,
                        );
                    }
                }
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
            if val.is_empty()
                || val.eq_ignore_ascii_case("null")
                || val.eq_ignore_ascii_case("none")
            {
                continue;
            }
            match key.as_str() {
                "status" | "state" | "job_status" if status_field.is_empty() => {
                    status_field = val.to_string()
                }
                "stage" | "current_stage" | "active_stage" | "tahap" | "phase" | "step"
                | "current_step"
                    if stage.is_empty() =>
                {
                    stage = val.to_string()
                }
                "progress" | "progres" | "pages" | "percent" | "pct" if progress.is_empty() => {
                    progress = val.to_string()
                }
                "summary" | "result" | "hasil" | "message" | "msg" if summary.is_empty() => {
                    summary = val.to_string()
                }
                "output" | "output_path" | "output_dir" | "location" | "lokasi" | "file"
                | "artifact"
                    if output_location.is_empty() =>
                {
                    output_location = val.to_string()
                }
                "error" | "error_message" | "err" | "reason" | "failure_reason" | "last_error"
                | "exception"
                    if error_text.is_empty() && val != "-" && val != "0" =>
                {
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
    let status_up = status_field.trim().to_ascii_uppercase();
    let stage_up = stage.trim().to_ascii_uppercase();
    let is_explicit_running = matches!(
        status_up.as_str(),
        "PROCESSING" | "RUNNING" | "QUEUED" | "PENDING" | "IN_PROGRESS" | "ACTIVE" | "STARTED"
    );
    let is_explicit_done = matches!(
        status_up.as_str(),
        "DONE" | "SELESAI" | "COMPLETED" | "COMPLETE" | "SUCCESS" | "SUCCEEDED" | "FINISHED" | "OK"
    );

    let fail_hit = if !fail_marker.trim().is_empty() {
        check_haystack
            .to_ascii_lowercase()
            .contains(&fail_marker.trim().to_ascii_lowercase())
    } else {
        false
    } || matches!(
        status_up.as_str(),
        "FAILED" | "FAIL" | "ERROR" | "GAGAL" | "CRASHED" | "ABORTED" | "REJECTED" | "FATAL"
    ) || matches!(
        stage_up.as_str(),
        "FAILED" | "FAIL" | "ERROR" | "GAGAL" | "CRASHED" | "ABORTED" | "FATAL"
    ) || (!error_text.is_empty() && !is_explicit_running && !is_explicit_done)
        || upper.contains("PIXEL_OUTSIDE_ALLOWED_MASK")
        || upper.lines().any(|l| {
            let t = l.trim();
            t == "FAILED"
                || t == "GAGAL"
                || t == "ERROR"
                || t.starts_with("FAILED:")
                || t.starts_with("FAILED ")
                || t.starts_with("FAIL:")
                || t.starts_with("ERROR:")
                || t.starts_with("ERROR ")
                || t.starts_with("GAGAL:")
                || t.starts_with("GAGAL ")
                || t.starts_with("STATUS=FAILED")
                || t.starts_with("STATUS=GAGAL")
                || t.starts_with("STATUS=ERROR")
                || t.starts_with("STATE=FAILED")
                || t.starts_with("STATE=ERROR")
                || t.starts_with("TRACEBACK")
                || t.starts_with("EXCEPTION:")
        });

    let done_hit = if !done_marker.trim().is_empty() {
        check_haystack
            .to_ascii_lowercase()
            .contains(&done_marker.trim().to_ascii_lowercase())
    } else {
        false
    } || is_explicit_done
        || upper.lines().any(|l| {
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
                || t.starts_with("STATE=COMPLETED")
                || t.starts_with("STATE=DONE")
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

/// Format laporan status segar bertanggal dari satu kali cek langsung sumber kebenaran
/// (Perbaikan 2 v0.5.10 — dipakai `watch_job(action="check")` agar model tidak pernah
/// menjawab status pekerjaan dari ingatan/ringkasan basi, dan jujur saat cek gagal).
pub fn format_watch_fresh_status(
    watch: &JobWatchRecord,
    poll_res: Result<&str, &str>,
    checked_at: &str,
) -> String {
    let ts_short: String = checked_at.chars().take(19).collect();
    match poll_res {
        Ok(raw) => {
            let parsed = parse_watch_output(raw, &watch.done_marker, &watch.fail_marker);
            let state_lbl = match parsed.kind {
                WatchStateKind::Running => "BERJALAN",
                WatchStateKind::Done => "SELESAI",
                WatchStateKind::Failed => "GAGAL",
            };
            let mut out = format!(
                "[Status segar #{id} '{}' · dicek {ts_short} UTC]\nKeadaan: {state_lbl}",
                watch.label,
                id = watch.id,
            );
            if !parsed.stage.is_empty() {
                out.push_str(&format!("\nTahap: {}", parsed.stage));
            }
            if !parsed.progress.is_empty() {
                out.push_str(&format!("\nProgres: {}", parsed.progress));
            }
            if parsed.kind == WatchStateKind::Failed && !parsed.error_text.is_empty() {
                out.push_str(&format!("\nError: {}", parsed.error_text));
            } else if !parsed.summary.is_empty() && parsed.summary != parsed.stage {
                out.push_str(&format!("\nRingkasan: {}", parsed.summary));
            }
            if !parsed.output_location.is_empty() {
                out.push_str(&format!("\nLokasi: {}", parsed.output_location));
            }
            out
        }
        Err(err) => {
            let clean_err = redact_secrets(err.trim());
            let prev_ts = if watch.last_checked_at.trim().is_empty() {
                "belum pernah dicek".to_string()
            } else {
                format!(
                    "{} UTC",
                    watch.last_checked_at.chars().take(19).collect::<String>()
                )
            };
            let prev_stage = if watch.last_stage.is_empty() {
                "-".to_string()
            } else if watch.last_progress.is_empty() {
                watch.last_stage.clone()
            } else {
                format!("{} ({})", watch.last_stage, watch.last_progress)
            };
            format!(
                "[Gagal cek status segar #{id} '{}' · percobaan {ts_short} UTC]\n\
                 Belum bisa mengecek sumber kebenaran saat ini: {clean_err}\n\
                 Catatan pantauan terakhir ({prev_ts}): status={}, tahap={prev_stage}.",
                watch.label,
                watch.status,
                id = watch.id,
            )
        }
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
    let interval = Duration::seconds(watch.interval_secs.clamp(MIN_WATCH_INTERVAL_SECS, 86_400) as i64);
    now_utc >= last.with_timezone(&Utc) + interval
}

/// Jalankan 1 kali pemeriksaan baca-saja untuk satu `JobWatchRecord` (berkas atau perintah shell).
pub async fn poll_watch_once(
    watch: &JobWatchRecord,
    sandbox: &SandboxController,
) -> Result<String> {
    let cwd = Path::new(&watch.cwd);
    if watch.check_kind == "file" {
        use std::io::Read;
        let p = resolve_in_sandbox(cwd, Path::new(&watch.check_target))?;
        if is_secret_file(&p) {
            return Err(anyhow!("berkas status ditolak: file rahasia"));
        }
        let f = std::fs::File::open(&p)
            .map_err(|e| anyhow!("gagal membaca berkas status '{}': {e}", watch.check_target))?;
        let mut buf = Vec::new();
        f.take(64 * 1024)
            .read_to_end(&mut buf)
            .map_err(|e| anyhow!("gagal membaca berkas status '{}': {e}", watch.check_target))?;
        return Ok(String::from_utf8_lossy(&buf).into_owned());
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

/// Hitung timestamp kedaluwarsa RFC3339 dari `now_utc + max_age_secs` (aman dari overflow i64).
pub fn compute_watch_expires_at(now_utc: DateTime<Utc>, max_age_secs: u64) -> String {
    let secs = max_age_secs.clamp(60, 604_800) as i64;
    let exp = now_utc
        .checked_add_signed(Duration::seconds(secs))
        .unwrap_or(now_utc);
    exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Normalisasi path folder kerja untuk pencocokan `job_watches.cwd` lintas separator/kapitalisasi OS.
pub fn normalize_watch_cwd(raw: &str) -> String {
    let canon = std::fs::canonicalize(raw)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| raw.to_string());
    let mut norm = canon.replace('\\', "/");
    if let Some(stripped) = norm.strip_prefix("//?/") {
        norm = stripped.to_string();
    }
    while norm.len() > 1 && norm.ends_with('/') {
        norm.pop();
    }
    if cfg!(windows) {
        norm.to_ascii_lowercase()
    } else {
        norm
    }
}

/// Satu implementasi polling & evaluasi untuk 1 `JobWatchRecord` — dipakai bersama
/// oleh loop gateway Telegram (`telegram_runner.rs`) maupun sesi CLI/TUI (`plain_runner.rs`,
/// `tui_runner.rs`, `main.rs`) agar logika 100% tunggal dan biaya provider = 0 token.
pub async fn evaluate_and_poll_watch(
    watch: &JobWatchRecord,
    sandbox: &SandboxController,
    now_utc: DateTime<Utc>,
) -> (WatchTransition, String) {
    let poll_out = poll_watch_once(watch, sandbox).await;
    let poll_res = match &poll_out {
        Ok(s) => Ok(s.as_str()),
        Err(e) => Err(e.to_string()),
    };
    let tr = evaluate_watch_poll(
        watch,
        match &poll_res {
            Ok(s) => Ok(*s),
            Err(e) => Err(e.as_str()),
        },
        now_utc,
    );
    let checked_at = now_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    (tr, checked_at)
}

/// Simpan hasil satu putaran polling `WatchTransition` ke SQLite `job_watches` secara persisten.
pub fn persist_watch_transition(
    store: &crate::store::Store,
    watch_id: i64,
    tr: &WatchTransition,
    checked_at: &str,
    delivered: bool,
) -> Result<()> {
    let final_rep = if tr.terminal {
        tr.notification.as_deref()
    } else {
        None
    };
    store.update_job_watch_poll(
        watch_id,
        &tr.new_status,
        &tr.new_stage,
        &tr.new_progress,
        tr.new_consecutive_errors,
        checked_at,
        final_rep,
        tr.terminal && delivered,
    )
}

/// Perbaikan 2 (v0.5.9) — Jalankan 1 putaran polling atas pantauan aktif milik ruang kerja `cwd`
/// (dan ambil laporan terminal tertunda bila ada) pada pembukaan / penutupan sesi CLI/TUI.
/// - Memakai fungsi polling yang SAMA persis dengan gateway (`evaluate_and_poll_watch` + `persist_watch_transition`).
/// - Status "sudah dilaporkan" (`last_stage`/`last_progress` & `delivered=1` untuk terminal) disimpan
///   persisten ke SQLite sehingga pemakaian bergantian CLI ↔ gateway tidak pernah menghasilkan laporan dobel.
/// - Bila tidak ada perubahan maupun laporan tertunda, mengembalikan `Vec::new()` (0 baris, 0 token).
pub async fn poll_active_watches_for_cli(
    store: &crate::store::Store,
    sandbox: &SandboxController,
    cwd: &Path,
    mark_terminal_delivered: bool,
    include_undelivered: bool,
) -> Vec<String> {
    let target_cwd = normalize_watch_cwd(&cwd.to_string_lossy());
    let mut reports = Vec::new();

    if let Ok(active_watches) = store.list_job_watches(None, true) {
        for w in active_watches {
            if normalize_watch_cwd(&w.cwd) != target_cwd {
                continue;
            }
            let (tr, checked_at) =
                evaluate_and_poll_watch(&w, sandbox, chrono::Utc::now()).await;
            let _ = persist_watch_transition(
                store,
                w.id,
                &tr,
                &checked_at,
                mark_terminal_delivered,
            );
            if let Some(msg) = tr.notification {
                reports.push(msg);
            }
        }
    }

    if include_undelivered {
        if let Ok(pending) = store.take_undelivered_watch_reports_for_cwd(&target_cwd) {
            for (_wid, _lbl, rep) in pending {
                reports.push(rep);
            }
        }
    }

    reports
}
