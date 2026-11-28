//! Tahap 3 — Background Process Manager (`bg_start`, `bg_output`, `bg_status`, `bg_stop`).
//!
//! Mengelola proses berdurasi panjang milik SelenCode (server development,
//! watcher, test suite panjang, build) secara aman di dalam `cwd` workspace:
//! - Menghasilkan `job_id` unik (`job-1`, `job-2`, ...)
//! - Menyimpan buffer log bergulir (maks 128 KB) sehingga output terbaru dapat
//!   dibaca berulang kali tanpa menjalankan ulang proses
//! - Melacak status (`Running` / `Exited(code)` / `Stopped`), PID, durasi, & exit code
//! - Hanya menghentikan proses yang dibuat oleh SelenCode (`owner = "selencode"`)
//! - Memblokir perintah destruktif (`is_forbidden_command`), meminta approval saat
//!   memulai (`bg_start`), meredaksi rahasia dari output, & membatasi jumlah proses aktif.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_forbidden_command, redact_secrets};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

const MAX_CONCURRENT_JOBS: usize = 8;
const MAX_STORED_JOBS: usize = 32;
const MAX_JOB_LOG_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Exited(Option<i32>),
    Stopped,
}

impl JobState {
    pub fn label(&self) -> String {
        match self {
            JobState::Running => "running".to_string(),
            JobState::Exited(Some(0)) => "exited (0)".to_string(),
            JobState::Exited(Some(c)) => format!("exited ({c})"),
            JobState::Exited(None) => "exited (?)".to_string(),
            JobState::Stopped => "stopped".to_string(),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, JobState::Running)
    }
}

/// Snapshot baca-saja dari satu proses latar belakang milik SelenCode.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub state: JobState,
    pub started_at: String,
    pub elapsed_secs: u64,
    pub output_bytes: usize,
    pub owner: &'static str,
}

struct ManagedJob {
    id: String,
    command: String,
    cwd: String,
    pid: Option<u32>,
    started: Instant,
    started_at: String,
    state: Arc<Mutex<JobState>>,
    output: Arc<Mutex<String>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl ManagedJob {
    fn snapshot(&self) -> JobSnapshot {
        let state = self
            .state
            .lock()
            .map(|s| s.clone())
            .unwrap_or(JobState::Stopped);
        let output_bytes = self.output.lock().map(|o| o.len()).unwrap_or(0);
        JobSnapshot {
            id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            state,
            started_at: self.started_at.clone(),
            elapsed_secs: self.started.elapsed().as_secs(),
            output_bytes,
            owner: "selencode",
        }
    }
}

/// Manajer proses latar belakang per sesi SelenCode.
pub struct BgManager {
    next_id: AtomicUsize,
    jobs: Mutex<HashMap<String, ManagedJob>>,
    order: Mutex<Vec<String>>,
}

impl Default for BgManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BgManager {
    fn drop(&mut self) {
        let _ = self.stop_all();
    }
}

impl BgManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicUsize::new(1),
            jobs: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    /// Mulai proses baru di background dalam `cwd` (memakai sandbox default).
    pub fn start(&self, command: &str, cwd: &Path) -> Result<JobSnapshot> {
        self.start_with_sandbox(command, cwd, &crate::sandbox::SandboxController::default())
    }

    /// Mulai proses baru di background dengan `SandboxController` sesi (scrub env, mode, & audit).
    pub fn start_with_sandbox(
        &self,
        command: &str,
        cwd: &Path,
        sandbox: &crate::sandbox::SandboxController,
    ) -> Result<JobSnapshot> {
        let cmd_str = command.trim();
        if cmd_str.is_empty() {
            return Err(anyhow!("perintah kosong"));
        }
        if is_forbidden_command(cmd_str) {
            return Err(anyhow!(
                "perintah ditolak demi keamanan (pola terlarang, tidak bisa di-approve): {cmd_str}"
            ));
        }

        {
            let jobs = self.jobs.lock().expect("bg jobs poisoned");
            let running = jobs
                .values()
                .filter(|j| {
                    j.state
                        .lock()
                        .map(|s| s.is_running())
                        .unwrap_or(false)
                })
                .count();
            if running >= MAX_CONCURRENT_JOBS {
                return Err(anyhow!(
                    "batas maksimum {MAX_CONCURRENT_JOBS} proses background aktif tercapai. Hentikan proses lama dengan `bg_stop` atau `/jobs stop <id>`."
                ));
            }
        }

        let mut cmd = sandbox.prepare_shell_command("bg_start", cmd_str, cwd)?;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("gagal menjalankan proses background '{cmd_str}': {e}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let id = format!("job-{id_num}");
        let state = Arc::new(Mutex::new(JobState::Running));
        let output = Arc::new(Mutex::new(String::new()));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let out_handle = stdout.map(|mut out_stream| {
            let buf = output.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = out_stream.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    append_rolling(&buf, &text);
                }
            })
        });
        let err_handle = stderr.map(|mut err_stream| {
            let buf = output.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = err_stream.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    append_rolling(&buf, &text);
                }
            })
        });

        let state_bg = state.clone();
        tokio::spawn(async move {
            tokio::select! {
                res = child.wait() => {
                    let code = res.ok().and_then(|s| s.code());
                    if let Some(h) = out_handle {
                        let _ = h.await;
                    }
                    if let Some(h) = err_handle {
                        let _ = h.await;
                    }
                    if let Ok(mut st) = state_bg.lock() {
                        if st.is_running() {
                            *st = JobState::Exited(code);
                        }
                    }
                }
                _ = stop_rx => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    if let Some(h) = out_handle {
                        h.abort();
                    }
                    if let Some(h) = err_handle {
                        h.abort();
                    }
                    if let Ok(mut st) = state_bg.lock() {
                        *st = JobState::Stopped;
                    }
                }
            }
        });

        let managed = ManagedJob {
            id: id.clone(),
            command: cmd_str.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            pid,
            started: Instant::now(),
            started_at: chrono::Local::now().format("%H:%M:%S").to_string(),
            state,
            output,
            stop_tx: Some(stop_tx),
        };
        let snap = managed.snapshot();

        let mut jobs = self.jobs.lock().expect("bg jobs poisoned");
        let mut order = self.order.lock().expect("bg order poisoned");
        jobs.insert(id.clone(), managed);
        order.push(id);

        // Bersihkan entri selesai tertua jika melewati MAX_STORED_JOBS.
        while jobs.len() > MAX_STORED_JOBS {
            if let Some(pos) = order.iter().position(|jid| {
                jobs.get(jid)
                    .and_then(|j| j.state.lock().ok().map(|s| !s.is_running()))
                    .unwrap_or(false)
            }) {
                let old_id = order.remove(pos);
                jobs.remove(&old_id);
            } else {
                break;
            }
        }
        Ok(snap)
    }

    /// Baca potongan output terbaru dari proses `job_id` tanpa menjalankan ulang.
    /// Output selalu melewati `redact_secrets` dan dibatasi `tail_lines` & `max_bytes`.
    pub fn output(
        &self,
        job_id: &str,
        tail_lines: usize,
        max_bytes: usize,
    ) -> Result<(JobSnapshot, String)> {
        let jobs = self.jobs.lock().expect("bg jobs poisoned");
        let job = jobs
            .get(job_id)
            .ok_or_else(|| anyhow!("job '{job_id}' tidak ditemukan di proses milik SelenCode"))?;
        let snap = job.snapshot();
        let raw = job.output.lock().map(|o| o.clone()).unwrap_or_default();
        let clean = redact_secrets(&raw);
        let lines: Vec<&str> = clean.lines().collect();
        let take_n = tail_lines.clamp(1, 400);
        let start = lines.len().saturating_sub(take_n);
        let mut joined = lines[start..].join("\n");
        let cap = max_bytes.clamp(256, 24 * 1024);
        if joined.len() > cap {
            let mut cut = joined.len() - cap;
            while cut < joined.len() && !joined.is_char_boundary(cut) {
                cut += 1;
            }
            joined = format!("... [output awal dipotong]\n{}", &joined[cut..]);
        }
        Ok((snap, joined))
    }

    /// Periksa status 1 job atau seluruh job milik SelenCode (terbaru dulu).
    pub fn status(&self, job_id: Option<&str>) -> Result<Vec<JobSnapshot>> {
        let jobs = self.jobs.lock().expect("bg jobs poisoned");
        if let Some(id) = job_id.map(str::trim).filter(|s| !s.is_empty()) {
            let job = jobs
                .get(id)
                .ok_or_else(|| anyhow!("job '{id}' tidak ditemukan di proses milik SelenCode"))?;
            return Ok(vec![job.snapshot()]);
        }
        let order = self.order.lock().expect("bg order poisoned");
        let mut out = Vec::new();
        for id in order.iter().rev() {
            if let Some(job) = jobs.get(id) {
                out.push(job.snapshot());
            }
        }
        Ok(out)
    }

    /// Hentikan 1 proses milik SelenCode secara aman.
    pub fn stop(&self, job_id: &str) -> Result<JobSnapshot> {
        let mut jobs = self.jobs.lock().expect("bg jobs poisoned");
        let job = jobs
            .get_mut(job_id)
            .ok_or_else(|| anyhow!("job '{job_id}' tidak ditemukan di proses milik SelenCode"))?;
        if let Some(tx) = job.stop_tx.take() {
            let _ = tx.send(());
            if let Ok(mut st) = job.state.lock() {
                if st.is_running() {
                    *st = JobState::Stopped;
                }
            }
        }
        Ok(job.snapshot())
    }

    /// Hentikan semua proses aktif milik SelenCode (mengembalikan jumlah yang dihentikan).
    pub fn stop_all(&self) -> usize {
        let mut jobs = self.jobs.lock().expect("bg jobs poisoned");
        let mut stopped = 0usize;
        for job in jobs.values_mut() {
            let was_running = job.state.lock().map(|s| s.is_running()).unwrap_or(false);
            if was_running {
                if let Some(tx) = job.stop_tx.take() {
                    let _ = tx.send(());
                }
                if let Ok(mut st) = job.state.lock() {
                    *st = JobState::Stopped;
                }
                stopped += 1;
            }
        }
        stopped
    }

    /// Jumlah proses SelenCode yang saat ini masih berstatus `Running`.
    pub fn running_count(&self) -> usize {
        let jobs = self.jobs.lock().expect("bg jobs poisoned");
        jobs.values()
            .filter(|j| j.state.lock().map(|s| s.is_running()).unwrap_or(false))
            .count()
    }

    /// Format kartu menu Moonlit untuk perintah `/jobs`.
    pub fn format_jobs_card(&self) -> String {
        let list = self.status(None).unwrap_or_default();
        let running = list.iter().filter(|j| j.state.is_running()).count();
        let mut out = format!(
            "# Background Process Manager ({running} aktif · {} total)\n",
            list.len()
        );
        if list.is_empty() {
            out.push_str("! Belum ada proses latar belakang milik SelenCode di sesi ini.\n");
        } else {
            for j in list.iter().take(15) {
                let mark = if j.state.is_running() { "*" } else { ">" };
                let pid = j
                    .pid
                    .map(|p| format!("pid {p}"))
                    .unwrap_or_else(|| "pid -".to_string());
                let cmd_short: String = j.command.chars().take(48).collect();
                out.push_str(&format!(
                    "{mark} {} | {} · {pid} · {}s | {cmd_short}\n",
                    j.id,
                    j.state.label(),
                    j.elapsed_secs
                ));
            }
        }
        out.push_str("---\n");
        out.push_str("! Lihat log: /jobs output <job-id>   ·   Hentikan: /jobs stop <job-id atau all>");
        out
    }
}

fn append_rolling(buf: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut guard) = buf.lock() {
        guard.push_str(text);
        if guard.len() > MAX_JOB_LOG_BYTES {
            let mut cut = guard.len() - MAX_JOB_LOG_BYTES;
            while cut < guard.len() && !guard.is_char_boundary(cut) {
                cut += 1;
            }
            guard.drain(..cut);
        }
    }
}

// ---------- 4 Tool Agent: bg_start, bg_output, bg_status, bg_stop ----------

pub struct BgStart;

#[async_trait]
impl Tool for BgStart {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bg_start".into(),
            description: "Jalankan proses jangka panjang di background (dev server, watcher, build/test lama) di folder project dan kembalikan job_id untuk dipantau dengan bg_output/bg_status/bg_stop.".into(),
            parameters: schema(
                json!({
                    "command": {
                        "type": "string",
                        "description": "Perintah shell yang dijalankan di background"
                    }
                }),
                &["command"],
            ),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let command = str_arg(&args, "command")
            .ok_or_else(|| anyhow!("argumen 'command' wajib diisi"))?
            .trim()
            .to_string();
        if command.is_empty() {
            return Err(anyhow!("perintah kosong"));
        }
        if is_forbidden_command(&command) {
            return Err(anyhow!(
                "perintah ditolak demi keamanan (pola terlarang): {command}"
            ));
        }
        if !ctx
            .approver
            .ask(&format!("bg_start: {command}"), &format!("cwd: {}", ctx.cwd.display()))
            .await
        {
            return Ok(format!(
                "Ditolak user — proses background TIDAK dijalankan: {command}"
            ));
        }
        let snap = ctx
            .bg_manager
            .start_with_sandbox(&command, &ctx.cwd, &ctx.sandbox)?;
        Ok(format!(
            "Proses background dimulai:\n- job_id: {}\n- owner: {}\n- pid: {}\n- status: {}\n- command: {}\nGunakan bg_output(job_id=\"{}\") untuk membaca log atau bg_stop(job_id=\"{}\") untuk menghentikan.",
            snap.id,
            snap.owner,
            snap.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            snap.state.label(),
            snap.command,
            snap.id,
            snap.id
        ))
    }
}

pub struct BgOutput;

#[async_trait]
impl Tool for BgOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bg_output".into(),
            description: "Baca output/log terbaru dari proses background SelenCode berdasarkan job_id tanpa menjalankannya ulang.".into(),
            parameters: schema(
                json!({
                    "job_id": {
                        "type": "string",
                        "description": "ID job dari bg_start (mis. job-1)"
                    },
                    "tail_lines": {
                        "type": "integer",
                        "description": "Jumlah baris terakhir yang dibaca (default 80, maks 400)"
                    }
                }),
                &["job_id"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let job_id = str_arg(&args, "job_id")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'job_id' wajib diisi"))?;
        let tail_lines = args
            .get("tail_lines")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(80);
        let (snap, out) = ctx.bg_manager.output(job_id, tail_lines, 16 * 1024)?;
        let body = if out.trim().is_empty() {
            "(belum ada output)".to_string()
        } else {
            out
        };
        Ok(format!(
            "job_id: {} | status: {} | durasi: {}s | pid: {}\n--- output terbaru ---\n{body}",
            snap.id,
            snap.state.label(),
            snap.elapsed_secs,
            snap.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
        ))
    }
}

pub struct BgStatus;

#[async_trait]
impl Tool for BgStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bg_status".into(),
            description: "Periksa status, PID, dan exit code satu proses background (job_id) atau daftar semua proses background milik SelenCode.".into(),
            parameters: schema(
                json!({
                    "job_id": {
                        "type": "string",
                        "description": "Opsional: ID job tertentu. Kosongkan untuk daftar semua job."
                    }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let job_id = str_arg(&args, "job_id");
        let list = ctx.bg_manager.status(job_id)?;
        if list.is_empty() {
            return Ok("Belum ada proses background milik SelenCode.".to_string());
        }
        let mut out = String::new();
        for j in list {
            out.push_str(&format!(
                "- {} [{}] status={} pid={} durasi={}s log={}B cmd={}\n",
                j.id,
                j.owner,
                j.state.label(),
                j.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                j.elapsed_secs,
                j.output_bytes,
                j.command
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct BgStop;

#[async_trait]
impl Tool for BgStop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bg_stop".into(),
            description: "Hentikan proses background milik SelenCode secara aman berdasarkan job_id, atau isi 'all' untuk menghentikan semua proses aktif milik SelenCode.".into(),
            parameters: schema(
                json!({
                    "job_id": {
                        "type": "string",
                        "description": "ID job yang akan dihentikan (mis. job-1) atau 'all'"
                    }
                }),
                &["job_id"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let job_id = str_arg(&args, "job_id")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'job_id' wajib diisi"))?;
        if job_id.eq_ignore_ascii_case("all") {
            let count = ctx.bg_manager.stop_all();
            return Ok(format!(
                "Berhasil menghentikan {count} proses background aktif milik SelenCode."
            ));
        }
        let snap = ctx.bg_manager.stop(job_id)?;
        Ok(format!(
            "Proses {} ({}) dihentikan. Status akhir: {}.",
            snap.id,
            snap.command,
            snap.state.label()
        ))
    }
}
