use crate::providers::{Block, Message, Role};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

/// Penyimpanan sesi SQLite (Fase 3, Blueprint Bagian 9).
/// Skema persis blueprint: sessions / messages / usage.
/// content pesan = JSON Vec<Block>; TIDAK ada API key yang disimpan di sini —
/// yang disimpan adalah pesan persis seperti yang dipertahankan loop (hasil
/// tool sudah teredaksi sebelum masuk pesan, lihat agent/loop.rs).
pub struct Store {
    conn: Mutex<Connection>,
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
                    input_tokens INT, output_tokens INT, ts TEXT);
CREATE TABLE IF NOT EXISTS memories (id INTEGER PRIMARY KEY, scope TEXT,
                    content TEXT, ts TEXT,
                    category TEXT NOT NULL DEFAULT 'general',
                    updated_at TEXT NOT NULL DEFAULT '');
";

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "UPDATE sessions SET provider = ?2, model = ?3 WHERE id = ?1",
            rusqlite::params![session_id, provider, model],
        )?;
        Ok(())
    }

    pub fn save_message(&self, session_id: &str, msg: &Message) -> Result<()> {
        let content = blocks_to_json(&msg.content);
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO messages (session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, role_str(msg.role), content, now()],
        )?;
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
    pub fn list_sessions(&self, cwd: &str, limit: usize) -> Result<Vec<SessionInfo>> {
        self.query_sessions(cwd, limit)
    }

    /// Sesi terakhir (terbaru) untuk cwd, untuk `selen resume`.
    pub fn latest_session(&self, cwd: &str) -> Result<Option<SessionInfo>> {
        Ok(self.query_sessions(cwd, 1)?.into_iter().next())
    }

    pub fn record_usage(
        &self,
        session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        conn.execute(
            "INSERT INTO usage (session_id, model, input_tokens, output_tokens, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, model, input_tokens as i64, output_tokens as i64, now()],
        )?;
        Ok(())
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

    pub fn remove_memory(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex store poisoned");
        let n = conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])?;
        Ok(n > 0)
    }
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
