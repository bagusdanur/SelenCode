use crate::providers::{Block, Message, Role};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone)]
struct PendingSessionMeta {
    cwd: String,
    provider: String,
    model: String,
    title: String,
}

/// Penyimpanan sesi SQLite (Fase 3, Blueprint Bagian 9).
/// Skema persis blueprint: sessions / messages / usage.
/// content pesan = JSON Vec<Block>; TIDAK ada API key yang disimpan di sini —
/// yang disimpan adalah pesan persis seperti yang dipertahankan loop (hasil
/// tool sudah teredaksi sebelum masuk pesan, lihat agent/loop.rs).
pub struct Store {
    conn: Mutex<Connection>,
    /// Metadata sesi interaktif baru yang belum memiliki pesan (lazy persistence:
    /// baru ditulis ke SQLite saat pesan/usage pertama benar-benar disimpan).
    pending_sessions: Mutex<HashMap<String, PendingSessionMeta>>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub created_at: String,
    pub title: String,
}

/// Kategori terstruktur untuk Memori Proyek (Tahap 2).
pub const MEMORY_CATEGORIES: &[(&str, &str)] = &[
    ("arch", "Struktur & Arsitektur Proyek"),
    ("commands", "Perintah Build, Test, & Lint"),
    ("conventions", "Konvensi Penamaan & Gaya Kode"),
    ("decisions", "Keputusan Teknis"),
    ("issues", "Masalah & Solusi"),
    ("tasks", "Status Tugas Belum Selesai"),
    ("general", "Catatan Umum"),
];

/// Normalisasi nama/alias kategori memori proyek ke kode kanonik.
pub fn normalize_memory_category(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "arch" | "architecture" | "arsitektur" | "struktur" => "arch",
        "commands" | "command" | "cmd" | "build" | "test" | "lint" | "perintah" => "commands",
        "conventions" | "convention" | "style" | "gaya" | "konvensi" => "conventions",
        "decisions" | "decision" | "keputusan" => "decisions",
        "issues" | "issue" | "bug" | "bugs" | "fix" | "troubleshoot" | "masalah" | "solusi" => {
            "issues"
        }
        "tasks" | "task" | "todo" | "tugas" | "wip" => "tasks",
        _ => "general",
    }
}

/// Cek apakah token pertama adalah nama/alias kategori eksplisit (bukan teks biasa).
pub fn parse_explicit_category(word: &str) -> Option<&'static str> {
    let low = word.trim().trim_end_matches(':').to_ascii_lowercase();
    match low.as_str() {
        "arch" | "architecture" | "arsitektur" | "struktur" => Some("arch"),
        "commands" | "command" | "cmd" | "build" | "test" | "lint" | "perintah" => Some("commands"),
        "conventions" | "convention" | "style" | "gaya" | "konvensi" => Some("conventions"),
        "decisions" | "decision" | "keputusan" => Some("decisions"),
        "issues" | "issue" | "bug" | "bugs" | "troubleshoot" | "masalah" | "solusi" => {
            Some("issues")
        }
        "tasks" | "task" | "todo" | "tugas" | "wip" => Some("tasks"),
        "general" | "umum" => Some("general"),
        _ => None,
    }
}

/// Label tampilan manusiawi untuk kode kategori memori.
pub fn memory_category_label(cat: &str) -> &'static str {
    let norm = normalize_memory_category(cat);
    MEMORY_CATEGORIES
        .iter()
        .find(|(k, _)| *k == norm)
        .map(|(_, label)| *label)
        .unwrap_or("Catatan Umum")
}

#[derive(Debug, Clone)]
pub struct MemoryItem {
    pub id: i64,
    pub scope: String,
    pub category: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Satu langkah dalam Long-Horizon Task persisten lintas sesi (Tahap 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LongTaskStep {
    pub id: usize,
    pub title: String,
    /// Peran yang direkomendasikan: `planner` | `researcher` | `coder` | `tester` | `reviewer`
    #[serde(default = "default_step_role")]
    pub role: String,
    /// `pending` | `in_progress` | `done` | `blocked` | `failed`
    #[serde(default = "default_step_status")]
    pub status: String,
    #[serde(default)]
    pub notes: String,
}

fn default_step_role() -> String {
    "coder".to_string()
}

fn default_step_status() -> String {
    "pending".to_string()
}

/// State persisten tugas jangka panjang (Long-Horizon Task Execution, Tahap 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LongTaskRecord {
    pub id: String,
    pub cwd: String,
    pub goal: String,
    /// `pending` | `in_progress` | `paused` | `completed` | `failed`
    pub status: String,
    pub steps: Vec<LongTaskStep>,
    pub checkpoint_note: String,
    pub tokens_used: u64,
    pub token_budget: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl LongTaskRecord {
    pub fn completed_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.status == "done").count()
    }

    /// Bangun prompt pemulihan lintas sesi yang ringkas dan siap dilanjutkan agent.
    pub fn build_resume_prompt(&self) -> String {
        let mut out = format!(
            "[PEMULIHAN LONG-HORIZON TASK: {}]\n\
             Tujuan utama: {}\n\
             Status terakhir: {} ({}/{} langkah selesai)\n\
             Pemakaian token: {} / {} token\n",
            self.id,
            self.goal,
            self.status,
            self.completed_steps(),
            self.steps.len(),
            self.tokens_used,
            self.token_budget
        );
        if !self.checkpoint_note.trim().is_empty() {
            out.push_str(&format!("Catatan checkpoint terakhir: {}\n", self.checkpoint_note));
        }
        out.push_str("\nChecklist langkah:\n");
        for s in &self.steps {
            let note = if s.notes.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", s.notes.trim())
            };
            out.push_str(&format!(
                "{}. [{}] ({}) {}{}\n",
                s.id, s.status, s.role, s.title, note
            ));
        }
        out.push_str(
            "\nInstruksi: Lanjutkan dari langkah yang belum selesai (jangan mengulang langkah `done`), \
             perbarui progress dengan tool `manage_task` (action=\"checkpoint\"), dan verifikasi hasil akhir."
        );
        out
    }
}

/// Catatan performa aktual per `(route_id, task_kind)` lintas sesi & benchmark (Fase 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPerfRecord {
    pub route_id: String,
    pub task_kind: String,
    pub successes: u64,
    pub failures: u64,
    pub total_latency_ms: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub updated_at: String,
}

impl ModelPerfRecord {
    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            0.5
        } else {
            self.successes as f64 / total as f64
        }
    }

    pub fn avg_latency_ms(&self) -> u64 {
        let total = self.successes + self.failures;
        if total == 0 {
            0
        } else {
            self.total_latency_ms / total
        }
    }
}

/// Catatan rincian token per panggilan API provider (Tahap 0 — Instrumentasi).
#[derive(Debug, Clone, Default)]
pub struct CallTokenMetric {
    pub session_id: String,
    pub model: String,
    pub est_system_tokens: usize,
    pub est_tools_tokens: usize,
    pub est_history_tokens: usize,
    pub est_tool_results_tokens: usize,
    pub actual_input_tokens: u64,
    pub actual_output_tokens: u64,
    pub active_tools_count: usize,
    pub pruned_count: usize,
    pub deduped_count: usize,
}

/// Ringkasan metrik token agregat untuk satu sesi (dipakai `/resume info` & uji regresi).
#[derive(Debug, Clone, Default)]
pub struct SessionTokenMetricsSummary {
    pub calls: usize,
    pub avg_system_tokens: usize,
    pub avg_tools_tokens: usize,
    pub avg_history_tokens: usize,
    pub avg_tool_results_tokens: usize,
    pub total_actual_input: u64,
    pub total_actual_output: u64,
    pub total_pruned: usize,
    pub total_deduped: usize,
}

/// Proposal perubahan berbahaya/destruktif dari Background Review yang ditahan di staging (v0.5.1).
/// Hanya berlaku setelah pengguna menyetujui lewat `/review approve <id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewProposal {
    pub id: i64,
    pub session_id: String,
    pub cwd: String,
    /// `delete_memory` | `overwrite_memory` | `delete_skill` | `overwrite_skill`
    pub kind: String,
    /// ID memori (mis. `"12"`) atau nama skill (mis. `"raw-downloader"`).
    pub target: String,
    pub summary: String,
    pub payload_json: String,
    /// `pending` | `approved` | `discarded`
    pub status: String,
    pub created_at: String,
    pub resolved_at: String,
}

/// Catatan satu kali jalan Background Self-Improvement Review (cooldown + laporan tertunda CLI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRunRecord {
    pub id: i64,
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    pub report_text: String,
    pub delivered: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub created_at: String,
}

/// Definisi tugas terjadwal persisten (Bagian 3 v0.5.6 — berjalan selama gateway Telegram hidup).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobRecord {
    pub id: i64,
    pub name: String,
    /// `every:<N>m` | `every:<N>h` | `daily:HH:MM`
    pub schedule_spec: String,
    pub prompt: String,
    pub cwd: String,
    /// Allowlist perintah shell eksplisit per job (kosong = `run_shell` tidak ditawarkan sama sekali).
    pub allowed_shell: Vec<String>,
    /// Izin tulis file eksplisit di folder `cwd` (default false = hanya tool baca).
    pub allow_write: bool,
    pub enabled: bool,
    /// Target chat Telegram pemilik (0 = kirim ke semua `allowed_users` pertama).
    pub chat_id: i64,
    pub last_run_at: String,
    pub next_run_at: String,
    pub last_status: String,
    pub last_summary: String,
    pub created_at: String,
}

/// Satu hasil pencarian lintas sesi berbasis SQLite FTS5 (Bagian 4A v0.5.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub title: String,
    pub created_at: String,
    pub role: String,
    pub snippet: String,
    pub score: f64,
}

/// Lokasi DB: env SELENCODE_DB (dipakai tes) atau
/// ~/.local/share/selencode/selen.db (dirs::data_dir).
pub fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("SELENCODE_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("selencode").join("selen.db")
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, cwd TEXT, provider TEXT,
                       model TEXT, created_at TEXT, title TEXT);
CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, session_id TEXT,
                       role TEXT, content TEXT, ts TEXT);
CREATE TABLE IF NOT EXISTS usage (id INTEGER PRIMARY KEY, session_id TEXT, model TEXT,
                    input_tokens INT, output_tokens INT, ts TEXT,
                    kind TEXT NOT NULL DEFAULT 'main');
CREATE TABLE IF NOT EXISTS memories (id INTEGER PRIMARY KEY, scope TEXT,
                    content TEXT, ts TEXT,
                    category TEXT NOT NULL DEFAULT 'general',
                    updated_at TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS long_tasks (id TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                    goal TEXT NOT NULL, status TEXT NOT NULL,
                    steps_json TEXT NOT NULL, checkpoint_note TEXT NOT NULL,
                    tokens_used INT NOT NULL DEFAULT 0,
                    token_budget INT NOT NULL DEFAULT 200000,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS benchmark_runs (id TEXT PRIMARY KEY, ts TEXT NOT NULL,
                    selen_version TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
                    task_success_rate REAL NOT NULL, test_pass_rate REAL NOT NULL,
                    regression_rate REAL NOT NULL, total_elapsed_ms INT NOT NULL,
                    report_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS model_perf (route_id TEXT NOT NULL, task_kind TEXT NOT NULL,
                    successes INT NOT NULL DEFAULT 0, failures INT NOT NULL DEFAULT 0,
                    total_latency_ms INT NOT NULL DEFAULT 0, total_tokens INT NOT NULL DEFAULT 0,
                    total_cost_usd REAL NOT NULL DEFAULT 0.0, updated_at TEXT NOT NULL,
                    PRIMARY KEY (route_id, task_kind));
CREATE TABLE IF NOT EXISTS prompt_history (id INTEGER PRIMARY KEY, cwd TEXT NOT NULL,
                    prompt TEXT NOT NULL, ts TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS call_token_metrics (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                    ts TEXT NOT NULL, model TEXT NOT NULL,
                    est_system_tokens INT NOT NULL, est_tools_tokens INT NOT NULL,
                    est_history_tokens INT NOT NULL, est_tool_results_tokens INT NOT NULL,
                    actual_input_tokens INT NOT NULL DEFAULT 0, actual_output_tokens INT NOT NULL DEFAULT 0,
                    active_tools_count INT NOT NULL DEFAULT 0,
                    pruned_count INT NOT NULL DEFAULT 0, deduped_count INT NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS review_proposals (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                    cwd TEXT NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL,
                    summary TEXT NOT NULL, payload_json TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    created_at TEXT NOT NULL, resolved_at TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS review_runs (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL,
                    cwd TEXT NOT NULL, model TEXT NOT NULL,
                    report_text TEXT NOT NULL DEFAULT '',
                    delivered INTEGER NOT NULL DEFAULT 0,
                    input_tokens INT NOT NULL DEFAULT 0, output_tokens INT NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cron_jobs (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL,
                    schedule_spec TEXT NOT NULL, prompt TEXT NOT NULL, cwd TEXT NOT NULL,
                    allowed_shell_json TEXT NOT NULL DEFAULT '[]',
                    allow_write INTEGER NOT NULL DEFAULT 0,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    chat_id INTEGER NOT NULL DEFAULT 0,
                    last_run_at TEXT NOT NULL DEFAULT '',
                    next_run_at TEXT NOT NULL DEFAULT '',
                    last_status TEXT NOT NULL DEFAULT '',
                    last_summary TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS job_watches (id INTEGER PRIMARY KEY, label TEXT NOT NULL,
                    chat_id INTEGER NOT NULL DEFAULT 0, cwd TEXT NOT NULL,
                    check_kind TEXT NOT NULL, check_target TEXT NOT NULL,
                    done_marker TEXT NOT NULL DEFAULT '',
                    fail_marker TEXT NOT NULL DEFAULT '',
                    interval_secs INTEGER NOT NULL DEFAULT 60,
                    max_age_secs INTEGER NOT NULL DEFAULT 21600,
                    status TEXT NOT NULL DEFAULT 'active',
                    last_stage TEXT NOT NULL DEFAULT '',
                    last_progress TEXT NOT NULL DEFAULT '',
                    consecutive_errors INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL, expires_at TEXT NOT NULL,
                    last_checked_at TEXT NOT NULL DEFAULT '',
                    final_report TEXT NOT NULL DEFAULT '',
                    delivered INTEGER NOT NULL DEFAULT 0);
";

fn now() -> String {
    // Presisi nanodetik tetap (UTC Z) agar pengurutan ORDER BY created_at DESC 100% deterministik.
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_from(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

// Format JSON penyimpanan Block. TIDAK memakai serde derive Block langsung:
// varian newtype `Text(String)` pada enum internally-tagged serde tidak bisa
// diserialisasi ("cannot serialize tagged newtype variant containing a
// string") — ketahuan oleh tes Fase 3. Format manual ini stabil & eksplisit.
fn blocks_to_json(blocks: &[Block]) -> String {
    let arr: Vec<serde_json::Value> = blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => serde_json::json!({"type": "text", "text": t}),
            Block::Image {
                media_type,
                data_base64,
            } => serde_json::json!({"type": "image", "media_type": media_type, "data_base64": data_base64}),
            Block::ToolUse { id, name, args } => {
                serde_json::json!({"type": "tool_use", "id": id, "name": name, "args": args})
            }
            Block::ToolResult {
                id,
                content,
                is_error,
            } => serde_json::json!({"type": "tool_result", "id": id, "content": content, "is_error": is_error}),
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

fn blocks_from_json(s: &str) -> Result<Vec<Block>> {
    let v: serde_json::Value = serde_json::from_str(s).context("isi pesan di DB bukan JSON")?;
    let arr = v.as_array().context("isi pesan di DB bukan array Block")?;
    let mut out = Vec::new();
    for b in arr {
        match b["type"].as_str() {
            Some("text") => out.push(Block::Text(
                b["text"].as_str().unwrap_or_default().to_string(),
            )),
            Some("image") => out.push(Block::Image {
                media_type: b["media_type"]
                    .as_str()
                    .unwrap_or("image/png")
                    .to_string(),
                data_base64: b["data_base64"].as_str().unwrap_or_default().to_string(),
            }),
            Some("tool_use") => out.push(Block::ToolUse {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                name: b["name"].as_str().unwrap_or_default().to_string(),
                args: b["args"].clone(),
            }),
            Some("tool_result") => out.push(Block::ToolResult {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                content: b["content"].as_str().unwrap_or_default().to_string(),
                is_error: b["is_error"].as_bool().unwrap_or(false),
            }),
            other => anyhow::bail!("tipe Block tidak dikenal di DB: {other:?}"),
        }
    }
    Ok(out)
}

impl Store {
    pub fn open() -> Result<Self> {
        Self::open_at(&db_path())
    }

    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("gagal membuat folder DB {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("gagal membuka DB sesi {}", path.display()))?;
        let _ = conn.execute_batch("PRAGMA busy_timeout = 5000;");
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
        conn.execute_batch(SCHEMA).context("gagal membuat skema sesi")?;
        migrate_memories_schema(&conn)?;
        migrate_usage_schema(&conn)?;
        init_fts_schema(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
            pending_sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Daftarkan sesi interaktif baru secara lazy (belum menulis row kosong ke SQLite
    /// sampai pesan/usage pertama dikirim — mencegah daftar `/resume` penuh sesi `0 msg`).
    pub fn register_lazy_session(
        &self,
        cwd: &str,
        provider: &str,
        model: &str,
        title: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut pending) = self.pending_sessions.lock() {
            pending.insert(
                id.clone(),
                PendingSessionMeta {
                    cwd: cwd.to_string(),
                    provider: provider.to_string(),
                    model: model.to_string(),
                    title: title.to_string(),
                },
            );
        }
        id
    }

    /// Pastikan sesi lazy sudah tercatat di tabel `sessions` sebelum menyimpan pesan/usage/judul.
    fn materialize_if_pending(&self, conn: &Connection, session_id: &str) -> Result<()> {
        let meta = self
            .pending_sessions
            .lock()
            .ok()
            .and_then(|mut map| map.remove(session_id));
        if let Some(m) = meta {
            conn.execute(
                "INSERT OR IGNORE INTO sessions (id, cwd, provider, model, created_at, title) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![session_id, m.cwd, m.provider, m.model, now(), m.title],
            )?;
        }
        Ok(())
    }

    /// Hapus 1 sesi beserta seluruh pesan & catatan usage-nya dari SQLite.
    pub fn delete_session(&self, session_id: &str) -> Result<bool> {
        if let Ok(mut pending) = self.pending_sessions.lock() {
            if pending.remove(session_id).is_some() {
                return Ok(true);
            }
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        let _ = conn.execute(
            "DELETE FROM messages_fts WHERE session_id = ?1",
            rusqlite::params![session_id],
        );
        conn.execute(
            "DELETE FROM usage WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        let n = conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(n > 0)
    }

    /// Bersihkan sesi kosong lama (`0 msg` dengan judul placeholder) agar daftar `/resume` rapi.
    pub fn prune_empty_sessions(&self, cwd: &str, keep_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute(
            "DELETE FROM sessions WHERE cwd = ?1 AND id != ?2 \
             AND (title = '' OR title = '(sesi baru)') \
             AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.session_id = sessions.id)",
            rusqlite::params![cwd, keep_session_id],
        )?;
        Ok(n)
    }

    /// Simpan 1 entri riwayat prompt untuk navigasi `↑`/`↓` lintas sesi di TUI.
    /// Melewati duplikat beruntun & menjaga maksimal 200 entri terbaru per folder.
    pub fn record_prompt_history(&self, cwd: &str, prompt: &str) -> Result<()> {
        let clean = prompt.trim();
        if clean.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        let last: Option<String> = conn
            .query_row(
                "SELECT prompt FROM prompt_history WHERE cwd = ?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![cwd],
                |row| row.get(0),
            )
            .ok();
        if last.as_deref() == Some(clean) {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO prompt_history (cwd, prompt, ts) VALUES (?1, ?2, ?3)",
            rusqlite::params![cwd, clean, now()],
        )?;
        let _ = conn.execute(
            "DELETE FROM prompt_history WHERE cwd = ?1 AND id NOT IN \
             (SELECT id FROM prompt_history WHERE cwd = ?1 ORDER BY id DESC LIMIT 200)",
            rusqlite::params![cwd],
        );
        Ok(())
    }

    /// Muat riwayat prompt tersimpan (urutan terlama -> terbaru) untuk navigasi `↑`/`↓`.
    pub fn load_prompt_history(&self, cwd: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT prompt FROM prompt_history WHERE cwd = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut rows: Vec<String> = stmt
            .query_map(rusqlite::params![cwd, limit as i64], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        rows.reverse();
        Ok(rows)
    }

    /// Catat rincian estimasi & aktual token 1 panggilan provider (Tahap 0).
    pub fn record_call_token_metric(&self, m: &CallTokenMetric) -> Result<()> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        self.materialize_if_pending(&conn, &m.session_id)?;
        conn.execute(
            "INSERT INTO call_token_metrics \
             (session_id, ts, model, est_system_tokens, est_tools_tokens, est_history_tokens, \
              est_tool_results_tokens, actual_input_tokens, actual_output_tokens, \
              active_tools_count, pruned_count, deduped_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                m.session_id,
                now(),
                m.model,
                m.est_system_tokens as i64,
                m.est_tools_tokens as i64,
                m.est_history_tokens as i64,
                m.est_tool_results_tokens as i64,
                m.actual_input_tokens as i64,
                m.actual_output_tokens as i64,
                m.active_tools_count as i64,
                m.pruned_count as i64,
                m.deduped_count as i64,
            ],
        )?;
        Ok(())
    }

    /// Ringkas metrik rincian token seluruh panggilan pada satu sesi (Tahap 0).
    pub fn session_token_metrics(&self, session_id: &str) -> Result<SessionTokenMetricsSummary> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let row = conn.query_row(
            "SELECT COUNT(*), \
                    COALESCE(AVG(est_system_tokens), 0), \
                    COALESCE(AVG(est_tools_tokens), 0), \
                    COALESCE(AVG(est_history_tokens), 0), \
                    COALESCE(AVG(est_tool_results_tokens), 0), \
                    COALESCE(SUM(actual_input_tokens), 0), \
                    COALESCE(SUM(actual_output_tokens), 0), \
                    COALESCE(SUM(pruned_count), 0), \
                    COALESCE(SUM(deduped_count), 0) \
             FROM call_token_metrics WHERE session_id = ?1",
            rusqlite::params![session_id],
            |r| {
                Ok(SessionTokenMetricsSummary {
                    calls: r.get::<_, i64>(0)?.max(0) as usize,
                    avg_system_tokens: r.get::<_, f64>(1)?.max(0.0).round() as usize,
                    avg_tools_tokens: r.get::<_, f64>(2)?.max(0.0).round() as usize,
                    avg_history_tokens: r.get::<_, f64>(3)?.max(0.0).round() as usize,
                    avg_tool_results_tokens: r.get::<_, f64>(4)?.max(0.0).round() as usize,
                    total_actual_input: r.get::<_, i64>(5)?.max(0) as u64,
                    total_actual_output: r.get::<_, i64>(6)?.max(0) as u64,
                    total_pruned: r.get::<_, i64>(7)?.max(0) as usize,
                    total_deduped: r.get::<_, i64>(8)?.max(0) as usize,
                })
            },
        )?;
        Ok(row)
    }

    /// Buat sesi baru; mengembalikan id sesi (uuid).
    pub fn create_session(
        &self,
        cwd: &str,
        provider: &str,
        model: &str,
        title: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO sessions (id, cwd, provider, model, created_at, title) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, cwd, provider, model, now(), title],
        )?;
        Ok(id)
    }

    /// Perbarui provider/model sesi (dipakai perintah /model).
    pub fn update_session_provider(&self, session_id: &str, provider: &str, model: &str) -> Result<()> {
        if let Ok(mut pending) = self.pending_sessions.lock() {
            if let Some(m) = pending.get_mut(session_id) {
                m.provider = provider.to_string();
                m.model = model.to_string();
                return Ok(());
            }
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "UPDATE sessions SET provider = ?2, model = ?3 WHERE id = ?1",
            rusqlite::params![session_id, provider, model],
        )?;
        Ok(())
    }

    /// Perbarui judul sesi secara eksplisit atau otomatis dari konteks percakapan.
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        let clean = title.trim();
        if clean.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        self.materialize_if_pending(&conn, session_id)?;
        conn.execute(
            "UPDATE sessions SET title = ?2 WHERE id = ?1",
            rusqlite::params![session_id, clean],
        )?;
        Ok(())
    }

    /// Hitung jumlah pesan tersimpan pada satu sesi.
    pub fn count_messages(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    pub fn save_message(&self, session_id: &str, msg: &Message) -> Result<()> {
        let content = blocks_to_json(&msg.content);
        let conn = self.conn.lock().expect("mutex store poisoned");
        self.materialize_if_pending(&conn, session_id)?;
        conn.execute(
            "INSERT INTO messages (session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, role_str(msg.role), content, now()],
        )?;
        let msg_id = conn.last_insert_rowid();
        // Indeks FTS5 (Bagian 4A v0.5.6): bangun HANYA dari isi pesan tersimpan yang sudah teredaksi.
        if msg.role != Role::System {
            let searchable = extract_searchable_text_from_blocks(&msg.content);
            if !searchable.is_empty() {
                let _ = conn.execute(
                    "INSERT INTO messages_fts (session_id, message_id, role, content_text) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![session_id, msg_id, role_str(msg.role), searchable],
                );
            }
        }
        // Otomatis beri nama sesi sesuai konteks pesan user pertama yang substantif
        // bila judul sesi masih placeholder ("(sesi baru)" atau kosong).
        if msg.role == Role::User {
            let cur_title: Option<String> = conn
                .query_row(
                    "SELECT title FROM sessions WHERE id = ?1",
                    rusqlite::params![session_id],
                    |row| row.get(0),
                )
                .ok();
            if let Some(cur) = cur_title {
                if is_placeholder_title(&cur) {
                    if let Some(inferred) = infer_title_from_blocks(&msg.content) {
                        let _ = conn.execute(
                            "UPDATE sessions SET title = ?2 WHERE id = ?1",
                            rusqlite::params![session_id, inferred],
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn save_messages(&self, session_id: &str, msgs: &[Message]) -> Result<()> {
        for m in msgs {
            self.save_message(session_id, m)?;
        }
        Ok(())
    }

    /// Muat ulang seluruh pesan sesi (urutan penyimpanan) untuk resume.
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt =
            conn.prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            Ok((role, content))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (role, content) = r?;
            let blocks = blocks_from_json(&content)?;
            out.push(Message {
                role: role_from(&role),
                content: blocks,
            });
        }
        Ok(out)
    }

    fn query_sessions(&self, cwd: &str, limit: usize) -> Result<Vec<SessionInfo>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, cwd, provider, model, created_at, title FROM sessions \
             WHERE cwd = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cwd, limit as i64], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                cwd: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                created_at: row.get(4)?,
                title: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Daftar sesi untuk satu cwd, terbaru dulu.
    /// Untuk sesi lama yang judulnya masih placeholder `"(sesi baru)"`, otomatis
    /// menyimpulkan judul kontekstual dari isi percakapan tersimpan & menyimpannya.
    pub fn list_sessions(&self, cwd: &str, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut list = self.query_sessions(cwd, limit)?;
        for s in &mut list {
            if is_placeholder_title(&s.title) {
                if let Ok(msgs) = self.load_messages(&s.id) {
                    if let Some(inferred) = infer_session_title(&msgs) {
                        let _ = self.update_session_title(&s.id, &inferred);
                        s.title = inferred;
                    }
                }
            }
        }
        Ok(list)
    }

    /// Cari sesi berdasarkan nomor urut (`#1..=#10` atau `1..=10`) atau prefix ID sesi (termasuk prefix UUID numerik).
    pub fn resolve_session(
        &self,
        cwd: &str,
        target: &str,
        limit: usize,
    ) -> Result<SessionInfo> {
        let raw = target.trim();
        let explicit_index = raw.starts_with('#');
        let clean = raw.trim_start_matches('#').trim();
        if clean.is_empty() {
            anyhow::bail!("nomor atau ID sesi tidak boleh kosong");
        }
        let list = self.list_sessions(cwd, limit)?;
        if list.is_empty() {
            anyhow::bail!("belum ada sesi tersimpan untuk folder ini");
        }
        let is_all_digits = clean.chars().all(|c| c.is_ascii_digit());
        if is_all_digits {
            if let Ok(idx) = clean.parse::<usize>() {
                if idx >= 1 && idx <= list.len() {
                    return Ok(list[idx - 1].clone());
                }
                if explicit_index || idx == 0 {
                    anyhow::bail!(
                        "nomor sesi #{idx} di luar jangkauan (tersedia 1..={}). Ketik /resume untuk melihat daftar.",
                        list.len()
                    );
                }
            } else if explicit_index {
                anyhow::bail!("nomor sesi '{target}' tidak valid");
            }
            // P1.2: string angka panjang (mis. prefix UUID `70135330` yang kebetulan semua digit)
            // jatuh ke pencocokan prefix ID sebelum menolak!
        }
        if let Some(found) = list
            .into_iter()
            .find(|s| s.id == clean || s.id.starts_with(clean))
        {
            return Ok(found);
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        let pattern = format!("{clean}%");
        let mut stmt = conn.prepare(
            "SELECT id, cwd, provider, model, created_at, title FROM sessions \
             WHERE cwd = ?1 AND id LIKE ?2 ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![cwd, pattern], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                cwd: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                created_at: row.get(4)?,
                title: row.get(5)?,
            })
        })?;
        if let Some(r) = rows.next() {
            return Ok(r?);
        }
        if is_all_digits {
            anyhow::bail!(
                "nomor/ID sesi '{target}' tidak ditemukan (daftar tersedia 1..={limit}). Ketik /resume untuk melihat daftar."
            );
        }
        anyhow::bail!("sesi '{target}' tidak ditemukan di folder ini. Ketik /resume untuk melihat 10 sesi terakhir.")
    }

    /// Sesi terakhir (terbaru) untuk cwd, untuk `selen resume`.
    pub fn latest_session(&self, cwd: &str) -> Result<Option<SessionInfo>> {
        Ok(self.list_sessions(cwd, 1)?.into_iter().next())
    }

    pub fn record_usage(
        &self,
        session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        self.record_usage_with_kind(session_id, model, input_tokens, output_tokens, "main")
    }

    /// Catat pemakaian token dengan penanda jenis (`'main'`, `'fallback'`, `'review'`, `'cron'`).
    pub fn record_usage_with_kind(
        &self,
        session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        kind: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        if matches!(kind, "main" | "fallback") {
            self.materialize_if_pending(&conn, session_id)?;
        }
        conn.execute(
            "INSERT INTO usage (session_id, model, input_tokens, output_tokens, ts, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![session_id, model, input_tokens as i64, output_tokens as i64, now(), kind],
        )?;
        Ok(())
    }

    /// Catat biaya token Background Self-Improvement Review secara TERPISAH dari jalur utama (v0.5.1).
    /// Memakai `session_id = "review:<orig_session_id>"` dan `kind = 'review'` agar query jalur utama
    /// (`WHERE session_id = <orig_session_id>`) tidak pernah terkotori, namun tetap teraudit di tabel `usage`.
    pub fn record_review_usage(
        &self,
        orig_session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let review_sid = format!("review:{orig_session_id}");
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO usage (session_id, model, input_tokens, output_tokens, ts, kind) VALUES (?1, ?2, ?3, ?4, ?5, 'review')",
            rusqlite::params![review_sid, model, input_tokens as i64, output_tokens as i64, now()],
        )?;
        Ok(())
    }

    /// Ringkasan biaya token review (total_in, total_out, jumlah_jalan) untuk satu sesi asal.
    pub fn review_usage_for_session(&self, orig_session_id: &str) -> Result<(u64, u64, usize)> {
        let review_sid = format!("review:{orig_session_id}");
        let conn = self.conn.lock().expect("mutex store poisoned");
        let (i, o, n): (Option<i64>, Option<i64>, i64) = conn.query_row(
            "SELECT SUM(input_tokens), SUM(output_tokens), COUNT(*) FROM usage WHERE session_id = ?1 AND kind = 'review'",
            rusqlite::params![review_sid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok((
            i.unwrap_or(0).max(0) as u64,
            o.unwrap_or(0).max(0) as u64,
            n.max(0) as usize,
        ))
    }

    /// Daftar catatan biaya token background review terbaru dari tabel `usage` (`kind = 'review'`).
    pub fn list_review_usage(&self, limit: usize) -> Result<Vec<(String, String, u64, u64, String)>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT session_id, model, input_tokens, output_tokens, ts FROM usage \
             WHERE kind = 'review' ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?.max(0) as u64,
                r.get::<_, i64>(3)?.max(0) as u64,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Total (input, output) token satu sesi.
    pub fn total_usage(&self, session_id: &str) -> Result<(u64, u64)> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let (i, o): (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT SUM(input_tokens), SUM(output_tokens) FROM usage WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            i.unwrap_or(0).max(0) as u64,
            o.unwrap_or(0).max(0) as u64,
        ))
    }

    // ---------- Memori Proyek & Global Terstruktur (/memory, Tahap 2) ----------

    pub fn add_memory(&self, scope: &str, content: &str) -> Result<i64> {
        self.add_memory_categorized(scope, "general", content)
    }

    pub fn add_memory_categorized(
        &self,
        scope: &str,
        category: &str,
        content: &str,
    ) -> Result<i64> {
        let clean = validate_memory_content(content)?;
        let cat = normalize_memory_category(category);
        let ts = now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO memories (scope, category, content, ts, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![scope, cat, clean, ts, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Perbarui isi (dan opsional kategori) memori yang sudah ada.
    pub fn update_memory(
        &self,
        id: i64,
        category: Option<&str>,
        content: &str,
    ) -> Result<bool> {
        let clean = validate_memory_content(content)?;
        let ts = now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        let affected = if let Some(cat) = category {
            let norm = normalize_memory_category(cat);
            conn.execute(
                "UPDATE memories SET category = ?2, content = ?3, updated_at = ?4 WHERE id = ?1",
                rusqlite::params![id, norm, clean, ts],
            )?
        } else {
            conn.execute(
                "UPDATE memories SET content = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, clean, ts],
            )?
        };
        Ok(affected > 0)
    }

    /// Ambil memori untuk `cwd` ini + memori `"global"`, terbaru dulu.
    pub fn list_memories(&self, cwd: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, scope, category, content, ts, updated_at FROM memories \
             WHERE scope = 'global' OR scope = ?1 \
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cwd, limit as i64], |row| {
            let ts: String = row.get(4)?;
            let upd: String = row.get::<_, String>(5).unwrap_or_default();
            Ok(MemoryItem {
                id: row.get(0)?,
                scope: row.get(1)?,
                category: row.get::<_, String>(2).unwrap_or_else(|_| "general".to_string()),
                content: row.get(3)?,
                created_at: ts.clone(),
                updated_at: if upd.is_empty() { ts } else { upd },
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Ambil memori untuk kategori tertentu (`cwd` + `"global"`).
    pub fn list_memories_by_category(
        &self,
        cwd: &str,
        category: &str,
        limit: usize,
    ) -> Result<Vec<MemoryItem>> {
        let cat = normalize_memory_category(category);
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, scope, category, content, ts, updated_at FROM memories \
             WHERE (scope = 'global' OR scope = ?1) AND category = ?2 \
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![cwd, cat, limit as i64], |row| {
            let ts: String = row.get(4)?;
            let upd: String = row.get::<_, String>(5).unwrap_or_default();
            Ok(MemoryItem {
                id: row.get(0)?,
                scope: row.get(1)?,
                category: row.get(2)?,
                content: row.get(3)?,
                created_at: ts.clone(),
                updated_at: if upd.is_empty() { ts } else { upd },
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_memory(&self, id: i64) -> Result<Option<MemoryItem>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, scope, category, content, ts, updated_at FROM memories WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id], |row| {
            let ts: String = row.get(4)?;
            let upd: String = row.get::<_, String>(5).unwrap_or_default();
            Ok(MemoryItem {
                id: row.get(0)?,
                scope: row.get(1)?,
                category: row.get::<_, String>(2).unwrap_or_else(|_| "general".to_string()),
                content: row.get(3)?,
                created_at: ts.clone(),
                updated_at: if upd.is_empty() { ts } else { upd },
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn remove_memory(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
        Ok(n > 0)
    }

    // ---------- Background Self-Improvement Review: Staging & Cooldown (v0.5.1) ----------

    pub fn create_review_proposal(
        &self,
        session_id: &str,
        cwd: &str,
        kind: &str,
        target: &str,
        summary: &str,
        payload_json: &str,
    ) -> Result<i64> {
        let safe_summary = crate::security::redact_secrets(summary.trim());
        let safe_payload = crate::security::redact_secrets(payload_json);
        let ts = now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        // Hindari duplikat proposal `pending` identik untuk (cwd, kind, target).
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM review_proposals WHERE cwd = ?1 AND kind = ?2 AND target = ?3 AND status = 'pending' LIMIT 1",
                rusqlite::params![cwd, kind, target],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO review_proposals (session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, '')",
            rusqlite::params![session_id, cwd, kind, target, safe_summary, safe_payload, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_review_proposals(
        &self,
        cwd: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReviewProposal>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut out = Vec::new();
        let mapper = |r: &rusqlite::Row<'_>| -> rusqlite::Result<ReviewProposal> {
            Ok(ReviewProposal {
                id: r.get(0)?,
                session_id: r.get(1)?,
                cwd: r.get(2)?,
                kind: r.get(3)?,
                target: r.get(4)?,
                summary: r.get(5)?,
                payload_json: r.get(6)?,
                status: r.get(7)?,
                created_at: r.get(8)?,
                resolved_at: r.get(9)?,
            })
        };
        match (cwd, status) {
            (Some(c), Some(st)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at \
                     FROM review_proposals WHERE cwd = ?1 AND status = ?2 ORDER BY id DESC LIMIT ?3",
                )?;
                for r in stmt.query_map(rusqlite::params![c, st, limit as i64], mapper)? {
                    out.push(r?);
                }
            }
            (Some(c), None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at \
                     FROM review_proposals WHERE cwd = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                for r in stmt.query_map(rusqlite::params![c, limit as i64], mapper)? {
                    out.push(r?);
                }
            }
            (None, Some(st)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at \
                     FROM review_proposals WHERE status = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                for r in stmt.query_map(rusqlite::params![st, limit as i64], mapper)? {
                    out.push(r?);
                }
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at \
                     FROM review_proposals ORDER BY id DESC LIMIT ?1",
                )?;
                for r in stmt.query_map(rusqlite::params![limit as i64], mapper)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn get_review_proposal(&self, id: i64) -> Result<Option<ReviewProposal>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, session_id, cwd, kind, target, summary, payload_json, status, created_at, resolved_at \
             FROM review_proposals WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id], |r| {
            Ok(ReviewProposal {
                id: r.get(0)?,
                session_id: r.get(1)?,
                cwd: r.get(2)?,
                kind: r.get(3)?,
                target: r.get(4)?,
                summary: r.get(5)?,
                payload_json: r.get(6)?,
                status: r.get(7)?,
                created_at: r.get(8)?,
                resolved_at: r.get(9)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn resolve_review_proposal(&self, id: i64, new_status: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute(
            "UPDATE review_proposals SET status = ?2, resolved_at = ?3 WHERE id = ?1 AND status = 'pending'",
            rusqlite::params![id, new_status, now()],
        )?;
        Ok(n > 0)
    }

    pub fn is_review_in_cooldown(&self, session_id: &str, cooldown_mins: u64) -> Result<bool> {
        if cooldown_mins == 0 {
            return Ok(false);
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        let last_ts: Option<String> = conn
            .query_row(
                "SELECT created_at FROM review_runs WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![session_id],
                |r| r.get(0),
            )
            .ok();
        let Some(ts) = last_ts else {
            return Ok(false);
        };
        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ts) else {
            return Ok(false);
        };
        let elapsed = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
        Ok(elapsed.num_seconds() < (cooldown_mins as i64) * 60)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_review_run(
        &self,
        session_id: &str,
        cwd: &str,
        model: &str,
        report_text: &str,
        delivered: bool,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<i64> {
        let safe_report = crate::security::redact_secrets(report_text.trim());
        let del_int: i64 = if delivered || safe_report.is_empty() { 1 } else { 0 };
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO review_runs (session_id, cwd, model, report_text, delivered, input_tokens, output_tokens, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                session_id,
                cwd,
                model,
                safe_report,
                del_int,
                input_tokens as i64,
                output_tokens as i64,
                now()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Ambil semua laporan review latar yang belum sempat ditampilkan ke CLI/TUI dan tandai sudah terkirim.
    pub fn take_undelivered_review_reports(&self, cwd: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, report_text FROM review_runs \
             WHERE cwd = ?1 AND delivered = 0 AND report_text != '' ORDER BY id ASC LIMIT 10",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(rusqlite::params![cwd], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut out = Vec::new();
        for (id, text) in rows {
            let _ = conn.execute(
                "UPDATE review_runs SET delivered = 1 WHERE id = ?1",
                rusqlite::params![id],
            );
            out.push(text);
        }
        Ok(out)
    }

    pub fn list_recent_review_runs(&self, cwd: &str, limit: usize) -> Result<Vec<ReviewRunRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, session_id, cwd, model, report_text, delivered, input_tokens, output_tokens, created_at \
             FROM review_runs WHERE cwd = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cwd, limit as i64], |r| {
            Ok(ReviewRunRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                cwd: r.get(2)?,
                model: r.get(3)?,
                report_text: r.get(4)?,
                delivered: r.get::<_, i64>(5).unwrap_or(0) != 0,
                input_tokens: r.get::<_, i64>(6).unwrap_or(0).max(0) as u64,
                output_tokens: r.get::<_, i64>(7).unwrap_or(0).max(0) as u64,
                created_at: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---------- Tugas Terjadwal / Cron Persisten (v0.5.6 Bagian 3) ----------

    #[allow(clippy::too_many_arguments)]
    pub fn add_cron_job(
        &self,
        name: &str,
        schedule_spec: &str,
        prompt: &str,
        cwd: &str,
        allowed_shell: &[String],
        allow_write: bool,
        chat_id: i64,
        next_run_at: &str,
    ) -> Result<CronJobRecord> {
        let clean_name = name.trim().to_string();
        if clean_name.is_empty() {
            anyhow::bail!("nama job tidak boleh kosong");
        }
        let safe_prompt = crate::security::redact_secrets(prompt.trim());
        if safe_prompt.is_empty() {
            anyhow::bail!("prompt job tidak boleh kosong");
        }
        let shell_json = serde_json::to_string(allowed_shell)?;
        let ts = now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO cron_jobs (name, schedule_spec, prompt, cwd, allowed_shell_json, allow_write, enabled, chat_id, last_run_at, next_run_at, last_status, last_summary, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, '', ?8, 'terjadwal', '', ?9)",
            rusqlite::params![
                clean_name,
                schedule_spec,
                safe_prompt,
                cwd,
                shell_json,
                if allow_write { 1i64 } else { 0i64 },
                chat_id,
                next_run_at,
                ts
            ],
        )
        .with_context(|| format!("gagal menyimpan cron job '{clean_name}' (mungkin nama sudah dipakai)"))?;
        let id = conn.last_insert_rowid();
        Ok(CronJobRecord {
            id,
            name: clean_name,
            schedule_spec: schedule_spec.to_string(),
            prompt: safe_prompt,
            cwd: cwd.to_string(),
            allowed_shell: allowed_shell.to_vec(),
            allow_write,
            enabled: true,
            chat_id,
            last_run_at: String::new(),
            next_run_at: next_run_at.to_string(),
            last_status: "terjadwal".to_string(),
            last_summary: String::new(),
            created_at: ts,
        })
    }

    pub fn list_cron_jobs(&self) -> Result<Vec<CronJobRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, schedule_spec, prompt, cwd, allowed_shell_json, allow_write, enabled, chat_id, last_run_at, next_run_at, last_status, last_summary, created_at \
             FROM cron_jobs ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], row_to_cron_job)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_cron_job(&self, name_or_id: &str) -> Result<Option<CronJobRecord>> {
        let key = name_or_id.trim().trim_start_matches('#');
        if key.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().expect("mutex store poisoned");
        if let Ok(id) = key.parse::<i64>() {
            let mut stmt = conn.prepare(
                "SELECT id, name, schedule_spec, prompt, cwd, allowed_shell_json, allow_write, enabled, chat_id, last_run_at, next_run_at, last_status, last_summary, created_at \
                 FROM cron_jobs WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(rusqlite::params![id], row_to_cron_job)?;
            if let Some(r) = rows.next() {
                return Ok(Some(r?));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT id, name, schedule_spec, prompt, cwd, allowed_shell_json, allow_write, enabled, chat_id, last_run_at, next_run_at, last_status, last_summary, created_at \
             FROM cron_jobs WHERE LOWER(name) = LOWER(?1)",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![key], row_to_cron_job)?;
        let found = match rows.next() {
            Some(r) => Some(r?),
            None => None,
        };
        Ok(found)
    }

    pub fn remove_cron_job(&self, name_or_id: &str) -> Result<bool> {
        let Some(job) = self.get_cron_job(name_or_id)? else {
            return Ok(false);
        };
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute("DELETE FROM cron_jobs WHERE id = ?1", rusqlite::params![job.id])?;
        Ok(n > 0)
    }

    pub fn set_cron_job_enabled(&self, name_or_id: &str, enabled: bool, next_run_at: Option<&str>) -> Result<Option<CronJobRecord>> {
        let Some(mut job) = self.get_cron_job(name_or_id)? else {
            return Ok(None);
        };
        let conn = self.conn.lock().expect("mutex store poisoned");
        let next_s = next_run_at.unwrap_or(&job.next_run_at);
        conn.execute(
            "UPDATE cron_jobs SET enabled = ?2, next_run_at = ?3 WHERE id = ?1",
            rusqlite::params![job.id, if enabled { 1i64 } else { 0i64 }, next_s],
        )?;
        job.enabled = enabled;
        job.next_run_at = next_s.to_string();
        Ok(Some(job))
    }

    pub fn update_cron_job_run(
        &self,
        id: i64,
        last_run_at: &str,
        next_run_at: &str,
        last_status: &str,
        last_summary: &str,
    ) -> Result<()> {
        let safe_sum = crate::security::redact_secrets(last_summary.trim());
        let cut: String = safe_sum.chars().take(240).collect();
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "UPDATE cron_jobs SET last_run_at = ?2, next_run_at = ?3, last_status = ?4, last_summary = ?5 WHERE id = ?1",
            rusqlite::params![id, last_run_at, next_run_at, last_status, cut],
        )?;
        Ok(())
    }

    // ---------- Pencarian Lintas Sesi FTS5 (v0.5.6 Bagian 4A) ----------

    /// Backfill satu kali (atau inkremental) dari tabel `messages` yang sudah ada ke `messages_fts`
    /// tanpa mengubah skema `messages`. Mengembalikan `(jumlah_diindeks, durasi_ms)`.
    pub fn backfill_fts_if_needed(&self) -> Result<(usize, u128)> {
        let started = std::time::Instant::now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        let max_msg: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))
            .unwrap_or(0);
        if max_msg == 0 {
            return Ok((0, started.elapsed().as_millis()));
        }
        let max_indexed: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(CAST(message_id AS INTEGER)), 0) FROM messages_fts",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if max_indexed >= max_msg {
            return Ok((0, started.elapsed().as_millis()));
        }
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content FROM messages WHERE id > ?1 AND role != 'system' ORDER BY id ASC",
        )?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map(rusqlite::params![max_indexed], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut indexed = 0usize;
        let _ = conn.execute_batch("BEGIN IMMEDIATE;");
        for (mid, sid, role, raw_json) in rows {
            if let Ok(blocks) = blocks_from_json(&raw_json) {
                let text = extract_searchable_text_from_blocks(&blocks);
                if !text.is_empty() {
                    if conn
                        .execute(
                            "INSERT INTO messages_fts (session_id, message_id, role, content_text) VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![sid, mid, role, text],
                        )
                        .is_ok()
                    {
                        indexed += 1;
                    }
                }
            }
        }
        let _ = conn.execute_batch("COMMIT;");
        Ok((indexed, started.elapsed().as_millis()))
    }

    /// Cari lintas sesi memakai SQLite FTS5 dengan cuplikan pendek (maks 180 char) & maks `limit` (<= 10).
    pub fn search_sessions_fts(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>> {
        let fts_query = build_safe_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let lim = limit.clamp(1, 10) as i64;
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut raw_hits: Vec<SessionSearchHit> = Vec::new();
        let sql = if cwd.is_some() {
            "SELECT f.session_id, COALESCE(s.title, '(tanpa judul)'), COALESCE(s.created_at, ''), f.role, \
                    snippet(messages_fts, 3, '', '', '…', 24), bm25(messages_fts) \
             FROM messages_fts f \
             LEFT JOIN sessions s ON s.id = f.session_id \
             WHERE messages_fts MATCH ?1 AND s.cwd = ?2 \
             ORDER BY bm25(messages_fts) ASC LIMIT ?3"
        } else {
            "SELECT f.session_id, COALESCE(s.title, '(tanpa judul)'), COALESCE(s.created_at, ''), f.role, \
                    snippet(messages_fts, 3, '', '', '…', 24), bm25(messages_fts) \
             FROM messages_fts f \
             LEFT JOIN sessions s ON s.id = f.session_id \
             WHERE messages_fts MATCH ?1 \
             ORDER BY bm25(messages_fts) ASC LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let fetch_limit = (lim * 4).min(40);
        let mapper = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SessionSearchHit> {
            let snip: String = r.get::<_, String>(4).unwrap_or_default();
            let clean_snip: String = snip
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(180)
                .collect();
            Ok(SessionSearchHit {
                session_id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                role: r.get(3)?,
                snippet: clean_snip,
                score: r.get::<_, f64>(5).unwrap_or(0.0),
            })
        };
        if let Some(c) = cwd {
            for r in stmt.query_map(rusqlite::params![fts_query, c, fetch_limit], mapper)? {
                raw_hits.push(r?);
            }
        } else {
            for r in stmt.query_map(rusqlite::params![fts_query, fetch_limit], mapper)? {
                raw_hits.push(r?);
            }
        }
        // Ambil 1 cuplikan paling relevan per sesi agar daftar hasil tidak didominasi 1 sesi saja.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for h in raw_hits {
            if seen.insert(h.session_id.clone()) {
                out.push(h);
                if out.len() >= lim as usize {
                    break;
                }
            }
        }
        Ok(out)
    }

    // ---------- Long-Horizon Task Persistence Lintas Sesi (/task, Tahap 5) ----------

    pub fn create_long_task(
        &self,
        cwd: &str,
        goal: &str,
        steps: &[LongTaskStep],
        token_budget: u64,
    ) -> Result<LongTaskRecord> {
        let clean_goal = crate::security::redact_secrets(goal.trim());
        if clean_goal.is_empty() {
            anyhow::bail!("tujuan (goal) tugas tidak boleh kosong");
        }
        let short_uuid = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("task-{}", &short_uuid[..8]);
        let ts = now();
        let budget = if token_budget == 0 { 200_000 } else { token_budget };
        let steps_vec = if steps.is_empty() {
            default_long_task_steps(&clean_goal)
        } else {
            steps.to_vec()
        };
        let steps_json = serde_json::to_string(&steps_vec)?;
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO long_tasks (id, cwd, goal, status, steps_json, checkpoint_note, tokens_used, token_budget, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                id,
                cwd,
                clean_goal,
                "pending",
                steps_json,
                "Tugas dibuat — siap dimulai",
                0i64,
                budget as i64,
                ts,
                ts
            ],
        )?;
        Ok(LongTaskRecord {
            id,
            cwd: cwd.to_string(),
            goal: clean_goal,
            status: "pending".to_string(),
            steps: steps_vec,
            checkpoint_note: "Tugas dibuat — siap dimulai".to_string(),
            tokens_used: 0,
            token_budget: budget,
            created_at: ts.clone(),
            updated_at: ts,
        })
    }

    pub fn get_long_task(&self, id: &str) -> Result<Option<LongTaskRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, cwd, goal, status, steps_json, checkpoint_note, tokens_used, token_budget, created_at, updated_at \
             FROM long_tasks WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id.trim()], row_to_long_task)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn list_long_tasks(&self, cwd: &str, limit: usize) -> Result<Vec<LongTaskRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, cwd, goal, status, steps_json, checkpoint_note, tokens_used, token_budget, created_at, updated_at \
             FROM long_tasks WHERE cwd = ?1 ORDER BY updated_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cwd, limit as i64], row_to_long_task)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn save_long_task_checkpoint(
        &self,
        id: &str,
        status: Option<&str>,
        steps: Option<&[LongTaskStep]>,
        checkpoint_note: Option<&str>,
        add_tokens: u64,
    ) -> Result<Option<LongTaskRecord>> {
        let Some(mut rec) = self.get_long_task(id)? else {
            return Ok(None);
        };
        if let Some(st) = status.map(str::trim).filter(|s| !s.is_empty()) {
            let norm = match st.to_ascii_lowercase().as_str() {
                "pending" | "in_progress" | "paused" | "completed" | "failed" => {
                    st.to_ascii_lowercase()
                }
                "done" => "completed".to_string(),
                _ => rec.status.clone(),
            };
            rec.status = norm;
        }
        if let Some(new_steps) = steps {
            rec.steps = new_steps.to_vec();
        }
        if let Some(note) = checkpoint_note.map(str::trim).filter(|s| !s.is_empty()) {
            rec.checkpoint_note = crate::security::redact_secrets(note);
        }
        rec.tokens_used = rec.tokens_used.saturating_add(add_tokens);
        rec.updated_at = now();
        let steps_json = serde_json::to_string(&rec.steps)?;
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "UPDATE long_tasks SET status = ?2, steps_json = ?3, checkpoint_note = ?4, tokens_used = ?5, updated_at = ?6 WHERE id = ?1",
            rusqlite::params![
                rec.id,
                rec.status,
                steps_json,
                rec.checkpoint_note,
                rec.tokens_used as i64,
                rec.updated_at
            ],
        )?;
        Ok(Some(rec))
    }

    pub fn delete_long_task(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute("DELETE FROM long_tasks WHERE id = ?1", rusqlite::params![id.trim()])?;
        Ok(n > 0)
    }

    // ---------- Benchmark History Persistence (Fase 4) ----------

    pub fn save_benchmark_run(&self, report: &crate::benchmark::BenchRunReport) -> Result<()> {
        let json = serde_json::to_string(report)?;
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO benchmark_runs \
             (id, ts, selen_version, provider, model, task_success_rate, test_pass_rate, regression_rate, total_elapsed_ms, report_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                report.run_id,
                report.timestamp,
                report.selen_version,
                report.provider,
                report.model,
                report.task_success_rate,
                report.test_pass_rate,
                report.regression_rate,
                report.total_elapsed_ms as i64,
                json
            ],
        )?;
        Ok(())
    }

    pub fn list_benchmark_runs(&self, limit: usize) -> Result<Vec<crate::benchmark::BenchRunReport>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT report_json FROM benchmark_runs ORDER BY ts DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let s = r?;
            if let Ok(rep) = serde_json::from_str::<crate::benchmark::BenchRunReport>(&s) {
                out.push(rep);
            }
        }
        Ok(out)
    }

    // ---------- Persistent Model Performance Registry (Fase 5) ----------

    pub fn record_model_outcome(
        &self,
        route_id: &str,
        task_kind: &str,
        succeeded: bool,
        latency_ms: u64,
        tokens: u64,
        cost_usd: f64,
    ) -> Result<()> {
        let succ_inc: i64 = if succeeded { 1 } else { 0 };
        let fail_inc: i64 = if succeeded { 0 } else { 1 };
        let ts = now();
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO model_perf (route_id, task_kind, successes, failures, total_latency_ms, total_tokens, total_cost_usd, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(route_id, task_kind) DO UPDATE SET \
               successes = successes + excluded.successes, \
               failures = failures + excluded.failures, \
               total_latency_ms = total_latency_ms + excluded.total_latency_ms, \
               total_tokens = total_tokens + excluded.total_tokens, \
               total_cost_usd = total_cost_usd + excluded.total_cost_usd, \
               updated_at = excluded.updated_at",
            rusqlite::params![
                route_id,
                task_kind,
                succ_inc,
                fail_inc,
                latency_ms as i64,
                tokens as i64,
                cost_usd,
                ts
            ],
        )?;
        Ok(())
    }

    pub fn load_model_perf(&self) -> Result<Vec<ModelPerfRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT route_id, task_kind, successes, failures, total_latency_ms, total_tokens, total_cost_usd, updated_at \
             FROM model_perf ORDER BY (successes + failures) DESC, route_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelPerfRecord {
                route_id: row.get(0)?,
                task_kind: row.get(1)?,
                successes: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                failures: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                total_latency_ms: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                total_tokens: row.get::<_, i64>(5).unwrap_or(0).max(0) as u64,
                total_cost_usd: row.get::<_, f64>(6).unwrap_or(0.0).max(0.0),
                updated_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---------- Perbaikan 3 (v0.5.7): Job Watches (pemantau tugas asinkron persisten) ----------

    #[allow(clippy::too_many_arguments)]
    pub fn add_job_watch(
        &self,
        label: &str,
        chat_id: i64,
        cwd: &str,
        check_kind: &str,
        check_target: &str,
        done_marker: &str,
        fail_marker: &str,
        interval_secs: u64,
        max_age_secs: u64,
        expires_at: &str,
    ) -> Result<crate::watch::JobWatchRecord> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let ts = now();
        conn.execute(
            "INSERT INTO job_watches \
             (label, chat_id, cwd, check_kind, check_target, done_marker, fail_marker, \
              interval_secs, max_age_secs, status, last_stage, last_progress, consecutive_errors, \
              created_at, expires_at, last_checked_at, final_report, delivered) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', '', '', 0, ?10, ?11, '', '', 0)",
            rusqlite::params![
                label,
                chat_id,
                cwd,
                check_kind,
                check_target,
                done_marker,
                fail_marker,
                interval_secs as i64,
                max_age_secs as i64,
                ts,
                expires_at,
            ],
        )?;
        let id = conn.last_insert_rowid();
        Ok(crate::watch::JobWatchRecord {
            id,
            label: label.to_string(),
            chat_id,
            cwd: cwd.to_string(),
            check_kind: check_kind.to_string(),
            check_target: check_target.to_string(),
            done_marker: done_marker.to_string(),
            fail_marker: fail_marker.to_string(),
            interval_secs,
            max_age_secs,
            status: "active".to_string(),
            last_stage: String::new(),
            last_progress: String::new(),
            consecutive_errors: 0,
            created_at: ts,
            expires_at: expires_at.to_string(),
            last_checked_at: String::new(),
            final_report: String::new(),
            delivered: false,
        })
    }

    pub fn count_active_job_watches(&self, chat_id: i64) -> Result<usize> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM job_watches WHERE status = 'active' AND chat_id = ?1",
            rusqlite::params![chat_id],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    pub fn list_job_watches(&self, chat_id_filter: Option<i64>, active_only: bool) -> Result<Vec<crate::watch::JobWatchRecord>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut out = Vec::new();
        let sql = match (chat_id_filter, active_only) {
            (Some(_), true) => {
                "SELECT id, label, chat_id, cwd, check_kind, check_target, done_marker, fail_marker, \
                 interval_secs, max_age_secs, status, last_stage, last_progress, consecutive_errors, \
                 created_at, expires_at, last_checked_at, final_report, delivered \
                 FROM job_watches WHERE chat_id = ?1 AND status = 'active' ORDER BY id ASC"
            }
            (Some(_), false) => {
                "SELECT id, label, chat_id, cwd, check_kind, check_target, done_marker, fail_marker, \
                 interval_secs, max_age_secs, status, last_stage, last_progress, consecutive_errors, \
                 created_at, expires_at, last_checked_at, final_report, delivered \
                 FROM job_watches WHERE chat_id = ?1 ORDER BY id DESC LIMIT 20"
            }
            (None, true) => {
                "SELECT id, label, chat_id, cwd, check_kind, check_target, done_marker, fail_marker, \
                 interval_secs, max_age_secs, status, last_stage, last_progress, consecutive_errors, \
                 created_at, expires_at, last_checked_at, final_report, delivered \
                 FROM job_watches WHERE status = 'active' ORDER BY id ASC"
            }
            (None, false) => {
                "SELECT id, label, chat_id, cwd, check_kind, check_target, done_marker, fail_marker, \
                 interval_secs, max_age_secs, status, last_stage, last_progress, consecutive_errors, \
                 created_at, expires_at, last_checked_at, final_report, delivered \
                 FROM job_watches ORDER BY id DESC LIMIT 20"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        if let Some(cid) = chat_id_filter {
            let rows = stmt.query_map(rusqlite::params![cid], row_to_job_watch)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let rows = stmt.query_map([], row_to_job_watch)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn update_job_watch_poll(
        &self,
        id: i64,
        status: &str,
        stage: &str,
        progress: &str,
        consecutive_errors: u32,
        checked_at: &str,
        final_report: Option<&str>,
        delivered: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "UPDATE job_watches \
             SET status = ?1, last_stage = ?2, last_progress = ?3, consecutive_errors = ?4, \
                 last_checked_at = ?5, final_report = COALESCE(?6, final_report), \
                 delivered = CASE WHEN ?7 = 1 THEN 1 ELSE delivered END \
             WHERE id = ?8",
            rusqlite::params![
                status,
                stage,
                progress,
                consecutive_errors as i64,
                checked_at,
                final_report,
                if delivered { 1i64 } else { 0i64 },
                id
            ],
        )?;
        Ok(())
    }

    pub fn cancel_job_watch(&self, id: i64, chat_id_filter: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = match chat_id_filter {
            Some(cid) => conn.execute(
                "UPDATE job_watches SET status = 'cancelled' WHERE id = ?1 AND chat_id = ?2 AND status = 'active'",
                rusqlite::params![id, cid],
            )?,
            None => conn.execute(
                "UPDATE job_watches SET status = 'cancelled' WHERE id = ?1 AND status = 'active'",
                rusqlite::params![id],
            )?,
        };
        Ok(n > 0)
    }

    /// Ambil laporan watch terminal yang selesai saat sesi CLI tidak aktif, lalu tandai terkirim.
    pub fn take_undelivered_watch_reports(&self) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, label, final_report FROM job_watches \
             WHERE chat_id = 0 AND status IN ('done', 'failed', 'expired') AND delivered = 0 AND final_report != '' \
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        for (id, _, _) in &out {
            let _ = conn.execute(
                "UPDATE job_watches SET delivered = 1 WHERE id = ?1",
                rusqlite::params![id],
            );
        }
        Ok(out)
    }

    // ---------- Perbaikan 7 (v0.5.7): Laporan `/usage` langsung dari tabel `usage` ----------

    pub fn usage_report(&self, session_id: &str, today_utc_prefix: &str) -> Result<UsageReportSummary> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let review_sid = format!("review:{session_id}");
        let mut session_rows = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT model, kind, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) \
                 FROM usage WHERE session_id = ?1 OR session_id = ?2 \
                 GROUP BY model, kind ORDER BY kind ASC, model ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![session_id, review_sid], |row| {
                Ok(UsageBreakdownRow {
                    model: row.get(0)?,
                    kind: row.get(1)?,
                    calls: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                    input_tokens: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                    output_tokens: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                })
            })?;
            for r in rows {
                session_rows.push(r?);
            }
        }
        let like_prefix = format!("{today_utc_prefix}%");
        let mut today_rows = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT model, kind, COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) \
                 FROM usage WHERE ts LIKE ?1 \
                 GROUP BY model, kind ORDER BY kind ASC, model ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![like_prefix], |row| {
                Ok(UsageBreakdownRow {
                    model: row.get(0)?,
                    kind: row.get(1)?,
                    calls: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                    input_tokens: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                    output_tokens: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                })
            })?;
            for r in rows {
                today_rows.push(r?);
            }
        }
        Ok(UsageReportSummary {
            session_id: session_id.to_string(),
            today_date: today_utc_prefix.to_string(),
            session_rows,
            today_rows,
        })
    }
}

/// Rincian 1 kelompok baris `usage` berdasarkan `(model, kind)` untuk `/usage` (Perbaikan 7 v0.5.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageBreakdownRow {
    pub model: String,
    pub kind: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Ringkasan lengkap `/usage` (sesi berjalan + total hari ini) langsung dari SQLite `usage`.
#[derive(Debug, Clone)]
pub struct UsageReportSummary {
    pub session_id: String,
    pub today_date: String,
    pub session_rows: Vec<UsageBreakdownRow>,
    pub today_rows: Vec<UsageBreakdownRow>,
}

fn row_to_job_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::watch::JobWatchRecord> {
    Ok(crate::watch::JobWatchRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        chat_id: row.get(2)?,
        cwd: row.get(3)?,
        check_kind: row.get(4)?,
        check_target: row.get(5)?,
        done_marker: row.get(6)?,
        fail_marker: row.get(7)?,
        interval_secs: row.get::<_, i64>(8).unwrap_or(60).max(30) as u64,
        max_age_secs: row.get::<_, i64>(9).unwrap_or(21600).max(60) as u64,
        status: row.get(10)?,
        last_stage: row.get(11)?,
        last_progress: row.get(12)?,
        consecutive_errors: row.get::<_, i64>(13).unwrap_or(0).max(0) as u32,
        created_at: row.get(14)?,
        expires_at: row.get(15)?,
        last_checked_at: row.get(16)?,
        final_report: row.get(17)?,
        delivered: row.get::<_, i64>(18).unwrap_or(0) != 0,
    })
}

fn default_long_task_steps(goal: &str) -> Vec<LongTaskStep> {
    let short: String = goal.chars().take(60).collect();
    vec![
        LongTaskStep {
            id: 1,
            title: format!("Rencanakan pemecahan tugas: {short}"),
            role: "planner".to_string(),
            status: "pending".to_string(),
            notes: String::new(),
        },
        LongTaskStep {
            id: 2,
            title: "Telusuri kode, simbol, & ketergantungan terkait".to_string(),
            role: "researcher".to_string(),
            status: "pending".to_string(),
            notes: String::new(),
        },
        LongTaskStep {
            id: 3,
            title: "Implementasikan perubahan kode seminimal & sebersih mungkin".to_string(),
            role: "coder".to_string(),
            status: "pending".to_string(),
            notes: String::new(),
        },
        LongTaskStep {
            id: 4,
            title: "Jalankan build, test, & cek regresi".to_string(),
            role: "tester".to_string(),
            status: "pending".to_string(),
            notes: String::new(),
        },
        LongTaskStep {
            id: 5,
            title: "Tinjau git diff, keamanan, & validasi akhir".to_string(),
            role: "reviewer".to_string(),
            status: "pending".to_string(),
            notes: String::new(),
        },
    ]
}

fn row_to_long_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<LongTaskRecord> {
    let steps_json: String = row.get(4)?;
    let steps: Vec<LongTaskStep> = serde_json::from_str(&steps_json).unwrap_or_default();
    Ok(LongTaskRecord {
        id: row.get(0)?,
        cwd: row.get(1)?,
        goal: row.get(2)?,
        status: row.get(3)?,
        steps,
        checkpoint_note: row.get(5)?,
        tokens_used: row.get::<_, i64>(6).unwrap_or(0).max(0) as u64,
        token_budget: row.get::<_, i64>(7).unwrap_or(200_000).max(0) as u64,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// Migrasi aman tabel `memories` dari database SelenCode versi lama.
fn migrate_memories_schema(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if !cols.iter().any(|c| c == "category") {
        conn.execute(
            "ALTER TABLE memories ADD COLUMN category TEXT NOT NULL DEFAULT 'general'",
            [],
        )
        .context("gagal migrasi kolom category pada tabel memories")?;
    }
    if !cols.iter().any(|c| c == "updated_at") {
        conn.execute(
            "ALTER TABLE memories ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
            [],
        )
        .context("gagal migrasi kolom updated_at pada tabel memories")?;
    }
    Ok(())
}

/// Migrasi aman tabel `usage` untuk menambahkan kolom `kind` (`'main'` | `'review'` | dll.).
fn migrate_usage_schema(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(usage)")?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if !cols.iter().any(|c| c == "kind") {
        conn.execute(
            "ALTER TABLE usage ADD COLUMN kind TEXT NOT NULL DEFAULT 'main'",
            [],
        )
        .context("gagal migrasi kolom kind pada tabel usage")?;
    }
    Ok(())
}

/// Inisialisasi tabel bayangan FTS5 untuk pencarian lintas sesi tanpa menyentuh tabel `messages`.
fn init_fts_schema(conn: &Connection) {
    let _ = conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(\
            session_id UNINDEXED, \
            message_id UNINDEXED, \
            role UNINDEXED, \
            content_text, \
            tokenize='unicode61'\
        );",
    );
}

fn extract_searchable_text_from_blocks(blocks: &[Block]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            Block::Text(t) if !t.trim().is_empty() => {
                let cut: String = t.trim().chars().take(4000).collect();
                parts.push(cut);
            }
            Block::ToolUse { name, args, .. } => {
                let a: String = args.to_string().chars().take(200).collect();
                parts.push(format!("[tool:{name}] {a}"));
            }
            Block::ToolResult { content, .. } if !content.trim().is_empty() => {
                let first: String = content.lines().take(3).collect::<Vec<_>>().join(" ");
                let cut: String = first.chars().take(400).collect();
                parts.push(cut);
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Ubah kueri pengguna bebas menjadi ekspresi FTS5 yang aman dari syntax error (tanda hubung/kutip).
fn build_safe_fts_query(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let tokens: Vec<String> = trimmed
        .split_whitespace()
        .map(|tok| tok.replace('"', ""))
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{tok}\""))
        .collect();
    tokens.join(" AND ")
}

fn row_to_cron_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJobRecord> {
    let shell_json: String = row.get(5)?;
    let allowed_shell: Vec<String> = serde_json::from_str(&shell_json).unwrap_or_default();
    Ok(CronJobRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        schedule_spec: row.get(2)?,
        prompt: row.get(3)?,
        cwd: row.get(4)?,
        allowed_shell,
        allow_write: row.get::<_, i64>(6).unwrap_or(0) != 0,
        enabled: row.get::<_, i64>(7).unwrap_or(1) != 0,
        chat_id: row.get(8)?,
        last_run_at: row.get(9)?,
        next_run_at: row.get(10)?,
        last_status: row.get(11)?,
        last_summary: row.get(12)?,
        created_at: row.get(13)?,
    })
}

/// Guard keamanan Project Memory: tolak isi kosong atau yang mengandung rahasia/token/password.
pub fn validate_memory_content(content: &str) -> Result<String> {
    let clean = content.trim();
    if clean.is_empty() {
        anyhow::bail!("isi memori tidak boleh kosong");
    }
    if crate::security::contains_secret(clean) {
        anyhow::bail!(
            "ditolak: memori proyek tidak boleh menyimpan rahasia, token, password, private key, atau API key"
        );
    }
    Ok(clean.to_string())
}

/// Apakah judul sesi masih placeholder default yang boleh diperbarui otomatis dari konteks?
pub fn is_placeholder_title(title: &str) -> bool {
    let t = title.trim();
    t.is_empty() || t == "(sesi baru)"
}

fn is_trivial_filler(text: &str) -> bool {
    let clean: String = text
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let norm = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        norm.as_str(),
        "" | "halo"
            | "hai"
            | "hi"
            | "hello"
            | "hey"
            | "p"
            | "ping"
            | "ok"
            | "oke"
            | "okay"
            | "ya"
            | "iya"
            | "siap"
            | "lanjut"
            | "lanjutkan"
            | "lanjutt"
            | "tes"
            | "test"
            | "thanks"
            | "makasih"
            | "terima kasih"
            | "selamat pagi"
            | "selamat siang"
            | "selamat sore"
            | "selamat malam"
    )
}

/// Ringkas teks prompt menjadi judul sesi yang bersih, ringkas, & sesuai konteks (maks ~56 karakter).
pub fn summarize_prompt_title(raw: &str) -> String {
    // Kasus khusus: prompt pemulihan Long-Horizon Task -> ambil baris "Tujuan utama:".
    for line in raw.lines() {
        if let Some(rest) = line.trim().strip_prefix("Tujuan utama:") {
            let goal = rest.trim();
            if !goal.is_empty() {
                return truncate_title_words(goal, 56);
            }
        }
    }

    // Ambil baris substantif pertama di luar blok kode ```...``` dan tag sistem `[...]`.
    let mut in_fence = false;
    let mut chosen = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            continue;
        }
        let cleaned = trimmed.trim_start_matches(['#', '>', '-', '*', '•', ' ']);
        if !cleaned.is_empty() {
            chosen = cleaned.to_string();
            break;
        }
    }
    if chosen.is_empty() {
        chosen = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    } else {
        chosen = chosen.split_whitespace().collect::<Vec<_>>().join(" ");
    }

    // Pangkas kalimat pembuka basa-basi agar topik inti langsung terbaca di daftar /resume.
    const FILLER_PREFIXES: &[&str] = &[
        "halo selena,",
        "halo selena",
        "hai selena,",
        "hai selena",
        "hey selena,",
        "hey selena",
        "hi selena,",
        "hi selena",
        "selena,",
        "selena ",
        "tolong bantu aku ",
        "tolong bantu saya ",
        "tolong bantu ",
        "tolong ",
        "bisa tolong bantu ",
        "bisa tolong ",
        "bisa bantu ",
        "bantu aku ",
        "bantu saya ",
        "bantu ",
        "coba tolong ",
        "coba ",
        "mohon ",
        "please help me ",
        "please ",
        "can you please ",
        "can you ",
        "could you ",
        "help me ",
    ];
    let mut working = chosen.trim().to_string();
    for _ in 0..3 {
        let lower = working.to_ascii_lowercase();
        let mut stripped = false;
        for pref in FILLER_PREFIXES {
            if lower.starts_with(pref) {
                let candidate = working[pref.len()..].trim_start_matches([',', ':', '-', ' ']);
                if candidate.chars().count() >= 4 {
                    working = candidate.to_string();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }

    // Jika ada lebih dari satu kalimat panjang, ambil kalimat pertama yang utuh.
    if working.chars().count() > 60 {
        if let Some(pos) = working.find(". ").or_else(|| working.find("? ")) {
            if pos >= 12 {
                working.truncate(pos);
            }
        }
    }

    let trimmed = working
        .trim()
        .trim_end_matches(['.', '?', '!', ',', ':', ';'])
        .trim();
    if trimmed.is_empty() {
        return "(sesi baru)".to_string();
    }

    // Kapitalisasi huruf awal bila kata biasa (bukan path/command seperti `/resume` atau `src/`).
    let formatted = if trimmed.starts_with('/')
        || trimmed.starts_with('.')
        || trimmed.starts_with("src/")
        || trimmed.starts_with("tests/")
    {
        trimmed.to_string()
    } else {
        let mut chars = trimmed.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
            None => String::new(),
        }
    };

    truncate_title_words(&formatted, 56)
}

fn truncate_title_words(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = String::new();
    for word in s.split_whitespace() {
        let next_len = if out.is_empty() {
            word.chars().count()
        } else {
            out.chars().count() + 1 + word.chars().count()
        };
        if next_len + 1 > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        format!("{out}…")
    }
}

fn extract_blocks_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) if !t.trim().is_empty() => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hasilkan judul sesi kontekstual dari 1 pesan user; kembalikan `None` bila pesan cuma basa-basi singkat.
pub fn infer_title_from_blocks(blocks: &[Block]) -> Option<String> {
    let text = extract_blocks_text(blocks);
    if text.trim().is_empty() || is_trivial_filler(&text) {
        return None;
    }
    let title = summarize_prompt_title(&text);
    if is_placeholder_title(&title) {
        None
    } else {
        Some(title)
    }
}

/// Simpulkan judul sesi kontekstual dari seluruh riwayat pesan sesi (lewati sapaan singkat di awal).
pub fn infer_session_title(messages: &[Message]) -> Option<String> {
    let mut fallback: Option<String> = None;
    for m in messages {
        if m.role != Role::User {
            continue;
        }
        if let Some(title) = infer_title_from_blocks(&m.content) {
            return Some(title);
        }
        if fallback.is_none() {
            let text = extract_blocks_text(&m.content);
            if !text.trim().is_empty() {
                let s = summarize_prompt_title(&text);
                if !is_placeholder_title(&s) {
                    fallback = Some(s);
                }
            }
        }
    }
    fallback
}
