//! Tugas Terjadwal / Cron di dalam Telegram Gateway (v0.5.6 Bagian 3).
//!
//! Batas jujur:
//! - Penjadwal hidup di dalam proses gateway Telegram (`selen telegram`).
//! - Job HANYA berjalan selama proses gateway hidup (tidak memasang cron sistem / systemd).
//! - Kebijakan lewati-saja (skip-missed): jadwal yang terlewat saat gateway mati dicatat
//!   terlewat, tidak dikejar membabi buta saat gateway menyala lagi.
//! - Keamanan ketat: default toolset HANYA tool baca; `run_shell` (allowlist per job) dan
//!   `write_file`/`edit_file` tidak pernah ditawarkan kecuali diizinkan eksplisit per job.

use crate::agent::run_turn_counted;
use crate::config::ShellCfg;
use crate::providers::router::is_retryable_error;
use crate::providers::{Provider, ToolSpec};
use crate::sandbox::SandboxController;
use crate::security::{redact_secrets, Approver};
use crate::store::{CronJobRecord, Store};
use crate::tools::file::{EditFile, ListFiles, ReadFile, WriteFile};
use crate::tools::outline::Outline;
use crate::tools::search::SearchText;
use crate::tools::session_search::SessionSearch;
use crate::tools::skill::{SkillView, SkillsList};
use crate::tools::web::FetchUrl;
use crate::tools::{run_timed, schema, str_arg, truncate_output, Ctx, Registry, Tool, MAX_TOOL_OUTPUT};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveTime, TimeZone, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

pub const CRON_GATEWAY_NOTE: &str =
    "Catatan: job terjadwal hanya berjalan selama proses gateway Telegram (`selen telegram`) hidup.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSchedule {
    EveryMinutes(u64),
    EveryHours(u64),
    DailyLocal { hour: u32, minute: u32 },
}

impl ParsedSchedule {
    pub fn canonical_spec(&self) -> String {
        match self {
            Self::EveryMinutes(m) => format!("every:{m}m"),
            Self::EveryHours(h) => format!("every:{h}h"),
            Self::DailyLocal { hour, minute } => format!("daily:{hour:02}:{minute:02}"),
        }
    }

    pub fn human_label(&self) -> String {
        match self {
            Self::EveryMinutes(m) => format!("setiap {m} menit"),
            Self::EveryHours(h) => format!("setiap {h} jam"),
            Self::DailyLocal { hour, minute } => format!("harian pukul {hour:02}:{minute:02} lokal"),
        }
    }
}

/// Parse jadwal sederhana (HANYA 2 bentuk: interval menit/jam ATAU harian HH:MM lokal).
/// Menolak ekspresi cron 5-bidang dan input kosong/mustahil dengan pesan jelas.
pub fn parse_schedule(raw: &str) -> Result<ParsedSchedule> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("jadwal tidak boleh kosong. Contoh: `every 5m`, `every 2h`, atau `daily 08:30`");
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() >= 5 {
        anyhow::bail!(
            "ekspresi cron 5-bidang tidak didukung di versi ini. Gunakan interval (`every 5m` / `every 2h`) atau harian (`daily 08:30`)."
        );
    }
    let low = trimmed.to_ascii_lowercase();
    let normalized = low
        .strip_prefix("every:")
        .or_else(|| low.strip_prefix("every "))
        .or_else(|| low.strip_prefix("tiap "))
        .or_else(|| low.strip_prefix("setiap "));

    if let Some(rest) = normalized {
        let rest = rest.trim();
        return parse_interval_token(rest);
    }

    if let Some(rest) = low
        .strip_prefix("daily:")
        .or_else(|| low.strip_prefix("daily "))
        .or_else(|| low.strip_prefix("harian "))
    {
        return parse_daily_time(rest.trim());
    }

    // Bentuk singkat langsung: `5m`, `2h`, atau `08:30`.
    if low.contains(':') {
        return parse_daily_time(&low);
    }
    parse_interval_token(&low)
}

fn parse_interval_token(tok: &str) -> Result<ParsedSchedule> {
    let tok = tok.trim();
    if let Some(num) = tok
        .strip_suffix("menit")
        .or_else(|| tok.strip_suffix("min"))
        .or_else(|| tok.strip_suffix('m'))
    {
        let n: u64 = num
            .trim()
            .parse()
            .map_err(|_| anyhow!("interval menit tidak valid: '{tok}'"))?;
        if n == 0 || n > 10_080 {
            anyhow::bail!("interval menit harus antara 1 sampai 10080 (dapat: {n})");
        }
        return Ok(ParsedSchedule::EveryMinutes(n));
    }
    if let Some(num) = tok
        .strip_suffix("jam")
        .or_else(|| tok.strip_suffix("hr"))
        .or_else(|| tok.strip_suffix('h'))
    {
        let n: u64 = num
            .trim()
            .parse()
            .map_err(|_| anyhow!("interval jam tidak valid: '{tok}'"))?;
        if n == 0 || n > 168 {
            anyhow::bail!("interval jam harus antara 1 sampai 168 (dapat: {n})");
        }
        return Ok(ParsedSchedule::EveryHours(n));
    }
    anyhow::bail!(
        "format jadwal '{tok}' tidak dikenali. Gunakan interval (`every 5m`, `every 2h`) atau harian (`daily 08:30`)."
    )
}

fn parse_daily_time(tok: &str) -> Result<ParsedSchedule> {
    let Some((hh, mm)) = tok.split_once(':') else {
        anyhow::bail!("format jam harian harus HH:MM (contoh: 08:30), dapat: '{tok}'");
    };
    let hour: u32 = hh
        .trim()
        .parse()
        .map_err(|_| anyhow!("jam tidak valid pada '{tok}'"))?;
    let minute: u32 = mm
        .trim()
        .parse()
        .map_err(|_| anyhow!("menit tidak valid pada '{tok}'"))?;
    if hour > 23 || minute > 59 {
        anyhow::bail!("jam harian di luar rentang 00:00–23:59 (dapat: '{tok}')");
    }
    Ok(ParsedSchedule::DailyLocal { hour, minute })
}

pub fn format_schedule_human(spec: &str) -> String {
    parse_schedule(spec)
        .map(|s| s.human_label())
        .unwrap_or_else(|_| spec.to_string())
}

/// Hitung cap waktu UTC RFC3339 berikutnya sesudah `now_utc` untuk `schedule_spec`.
pub fn compute_next_run_after(spec: &str, now_utc: DateTime<Utc>) -> Result<String> {
    let parsed = parse_schedule(spec)?;
    let next_utc = match parsed {
        ParsedSchedule::EveryMinutes(m) => now_utc + ChronoDuration::minutes(m as i64),
        ParsedSchedule::EveryHours(h) => now_utc + ChronoDuration::hours(h as i64),
        ParsedSchedule::DailyLocal { hour, minute } => {
            let now_local = now_utc.with_timezone(&Local);
            let target_time = NaiveTime::from_hms_opt(hour, minute, 0)
                .ok_or_else(|| anyhow!("waktu harian tidak valid"))?;
            let today_naive = now_local.date_naive().and_time(target_time);
            let candidate = Local
                .from_local_datetime(&today_naive)
                .earliest()
                .unwrap_or(now_local + ChronoDuration::days(1));
            let chosen = if candidate <= now_local {
                let tomorrow = (now_local.date_naive() + ChronoDuration::days(1)).and_time(target_time);
                Local
                    .from_local_datetime(&tomorrow)
                    .earliest()
                    .unwrap_or(now_local + ChronoDuration::days(1))
            } else {
                candidate
            };
            chosen.with_timezone(&Utc)
        }
    };
    Ok(next_utc.to_rfc3339_opts(chrono::SecondsFormat::Seconds, true))
}

/// Kebijakan skip-missed saat gateway baru menyala:
/// jika ada job aktif yang `next_run_at <= now_utc`, JANGAN kejar secara membabi buta;
/// catat `terlewat (gateway mati)` dan geser `next_run_at` ke slot masa depan berikutnya.
pub fn skip_missed_jobs_on_startup(store: &Store) -> Result<usize> {
    let now_utc = Utc::now();
    let jobs = store.list_cron_jobs()?;
    let mut skipped = 0usize;
    for job in jobs {
        if !job.enabled {
            continue;
        }
        let is_past = DateTime::parse_from_rfc3339(&job.next_run_at)
            .map(|dt| dt.with_timezone(&Utc) <= now_utc)
            .unwrap_or(true);
        if is_past {
            if let Ok(next_s) = compute_next_run_after(&job.schedule_spec, now_utc) {
                let _ = store.update_cron_job_run(
                    job.id,
                    &job.last_run_at,
                    &next_s,
                    "terlewat",
                    "Jadwal terlewat saat gateway mati (dilewati sesuai kebijakan skip-missed).",
                );
                skipped += 1;
            }
        }
    }
    Ok(skipped)
}

/// Cek apakah perintah shell diizinkan oleh allowlist per job (cocok persis atau prefix token).
pub fn is_shell_command_allowlisted(command: &str, allowlist: &[String]) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() || allowlist.is_empty() {
        return false;
    }
    // Tolak chaining berbahaya (`;`, `&&`, `||`, `|`, backtick, `$(`) pada perintah allowlist agar tidak bisa dibajak.
    if [";", "&&", "||", "|", "`", "$("].iter().any(|op| cmd.contains(op)) {
        return false;
    }
    allowlist.iter().any(|allowed| {
        let a = allowed.trim();
        if a.is_empty() {
            return false;
        }
        cmd == a
            || (cmd.starts_with(a)
                && cmd
                    .as_bytes()
                    .get(a.len())
                    .map(|b| b.is_ascii_whitespace())
                    .unwrap_or(false))
    })
}

/// Wrapper `run_shell` ketat khusus cron job: HANYA menjalankan perintah di allowlist job.
/// Tidak pernah menyimulasikan approval dan tetap tunduk pada `SandboxController`.
pub struct AllowlistedShellTool {
    pub allowlist: Vec<String>,
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for AllowlistedShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: format!(
                "Jalankan perintah shell terbatas khusus job terjadwal (hanya diizinkan dari allowlist: {}).",
                self.allowlist.join(", ")
            ),
            parameters: schema(
                json!({
                    "command": {
                        "type": "string",
                        "description": "Perintah shell yang termasuk allowlist job ini"
                    }
                }),
                &["command"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let command = str_arg(&args, "command")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'command' wajib diisi"))?;
        if !is_shell_command_allowlisted(command, &self.allowlist) {
            return Err(anyhow!(
                "ditolak oleh pagar keamanan cron: perintah `{command}` tidak ada di allowlist job ini ({})",
                self.allowlist.join(", ")
            ));
        }
        ctx.sandbox.check_shell_allowed(command)?;
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&ctx.cwd).kill_on_drop(true);
        ctx.sandbox.prepare_shell_command(&mut cmd, &ctx.cwd, false)?;
        let (code, output) = run_timed(&mut cmd, self.timeout_secs.max(10)).await?;
        let cleaned = truncate_output(redact_secrets(&output), MAX_TOOL_OUTPUT);
        Ok(format!(
            "exit_code={}\n{}",
            code.unwrap_or(-1),
            cleaned.trim_end()
        ))
    }
}

/// Bangun Registry khusus untuk eksekusi 1 cron job (Bagian 3 #4):
/// - Default = HANYA tool baca (`read_file`, `search_text`, `list_files`, `outline`, `fetch_url`, `skills_list`, `skill_view`, `session_search`).
/// - `run_shell` HANYA didaftarkan (via `AllowlistedShellTool`) bila `job.allowed_shell` tidak kosong.
/// - `write_file` & `edit_file` HANYA didaftarkan bila `job.allow_write == true`.
pub fn build_cron_registry(job: &CronJobRecord, shell_cfg: &ShellCfg) -> Registry {
    let mut reg = Registry::new()
        .register(Arc::new(ListFiles))
        .register(Arc::new(ReadFile))
        .register(Arc::new(SearchText))
        .register(Arc::new(Outline))
        .register(Arc::new(FetchUrl))
        .register(Arc::new(SkillsList))
        .register(Arc::new(SkillView))
        .register(Arc::new(SessionSearch));

    if !job.allowed_shell.is_empty() {
        reg = reg.register(Arc::new(AllowlistedShellTool {
            allowlist: job.allowed_shell.clone(),
            timeout_secs: shell_cfg.timeout_secs,
        }));
    }
    if job.allow_write {
        reg = reg
            .register(Arc::new(WriteFile))
            .register(Arc::new(EditFile));
    }
    reg
}

/// Jalankan 1 cron job secara terisolasi (sesi baru khusus job, konteks segar, tanpa approval interaktif).
/// Maks 1 kali percobaan ulang untuk error jaringan sesaat; mencatat biaya token ke `usage` (`kind='cron'`).
pub async fn run_cron_job_once(
    provider: Arc<dyn Provider>,
    store: Arc<Store>,
    sandbox: Arc<SandboxController>,
    job: &CronJobRecord,
    shell_cfg: &ShellCfg,
    max_steps: usize,
) -> (bool, String) {
    let run_id = format!(
        "cron:{}:{}",
        job.name,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let cwd = PathBuf::from(&job.cwd);
    let registry = build_cron_registry(job, shell_cfg);
    // Approver::deny_all() memastikan approval interaktif TIDAK pernah disimulasikan;
    // hanya jika `allow_write` eksplisit diizinkan per job, mode `AutoEdits` dipakai terbatas di cwd.
    let approver = if job.allow_write {
        let mut pol = crate::security::ApprovalPolicy::new(crate::security::ApprovalMode::AutoEdits);
        pol.allow_category("write_file");
        pol.allow_category("edit_file");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        Arc::new(Approver::channel_with_policy(
            Arc::new(std::sync::Mutex::new(pol)),
            tx,
        ))
    } else {
        Arc::new(Approver::deny_all())
    };

    let mut ctx = Ctx::new(cwd, false)
        .with_approver(approver)
        .with_sandbox(sandbox)
        .with_auto_skills(false)
        .with_self_correct(false);
    // Sediakan handle `store` hanya untuk tool baca seperti `session_search`, sementara `session_id = None`
    // agar sesi interaktif pemilik tidak tercemari dan biaya token dicatat terpisah sebagai `kind = 'cron'`.
    ctx.store = Some(store.clone());

    let mut prompt = format!(
        "[TUGAS TERJADWAL: {}]\n\
         Kamu berjalan otomatis tanpa pengguna di tempat. Jangan mengarang hasil atau berpura-pura menjalankan tool yang tidak tersedia. \
         Bila tugas meminta aksi di luar izin tool yang tersedia, laporkan keterbatasan itu secara jujur.\n\n\
         Tugas: {}",
        job.name, job.prompt
    );
    if job.allowed_shell.is_empty() && !job.allow_write {
        prompt.push_str("\n(Mode izin job ini: HANYA BACA.)");
    }

    let mut last_err = String::new();
    for attempt in 0..2 {
        let mut messages = Vec::new();
        match run_turn_counted(
            provider.clone(),
            &registry,
            &ctx,
            &mut messages,
            &prompt,
            max_steps.min(15),
        )
        .await
        {
            Ok((answer, usage)) => {
                if let Some(u) = usage {
                    let _ = store.record_usage_with_kind(
                        &run_id,
                        provider.model_name(),
                        u.input_tokens,
                        u.output_tokens,
                        "cron",
                    );
                }
                let clean = redact_secrets(answer.trim());
                let text = if clean.is_empty() {
                    "(selesai tanpa ringkasan teks)".to_string()
                } else {
                    clean
                };
                return (true, text);
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt == 0 && is_retryable_error(&last_err) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                break;
            }
        }
    }

    (
        false,
        format!("Gagal menjalankan job '{}': {}", job.name, redact_secrets(&last_err)),
    )
}

/// Format laporan hasil job terjadwal sebagai pesan Telegram baru (gaya v0.5.0, hemat emoji).
pub fn format_cron_report_message(job_name: &str, ok: bool, summary: &str) -> String {
    let now_local = Local::now().format("%Y-%m-%d %H:%M");
    let status = if ok { "sukses" } else { "gagal" };
    format!(
        "Job Terjadwal: {job_name} · {now_local} · {status}\n\n{}",
        summary.trim()
    )
}
