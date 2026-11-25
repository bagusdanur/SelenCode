//! Telegram Bot Gateway untuk SelenCode (`selen telegram` & `/telegram`).
//!
//! Menggunakan Telegram Bot API long-polling murni di atas `reqwest` (tanpa
//! webhook/ngrok/domain publik), sehingga langsung jalan di Windows, Linux,
//! maupun Termux Android.
//!
//! Keamanan:
//! - Whitelist `allowed_users` wajib diisi agar orang asing tidak bisa
//!   menjalankan perintah/tool di mesin pemilik.
//! - Bila pengirim belum ada di whitelist, bot menolak eksekusi dan membalas
//!   User ID pengirim agar pemilik mudah menambahkannya via `/telegram allow <ID>`.
//! - Permintaan approval tool (`edit_file`, `run_shell`, dll.) dikirim sebagai
//!   tombol inline Telegram (`✅ Setuju` / `⚡ Selalu sesi ini` / `❌ Tolak`).

use crate::repl::{classify_key_spec, KeySpec};
use crate::security::{tool_category_from_title, ApprovalPolicy, ApprovalRequest};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Batas aman panjang satu pesan Telegram (maks resmi 4096 karakter).
pub const TELEGRAM_MAX_CHARS: usize = 3900;

/// Timeout otomatis permintaan persetujuan di Telegram (5 menit) agar chat
/// tidak terkunci selamanya jika user tidak menekan tombol.
pub const APPROVAL_TIMEOUT_SECS: u64 = 300;

/// Sensor token bot Telegram agar tidak pernah bocor ke log error atau chat.
pub fn redact_tg_token(text: &str, token: &str) -> String {
    let t = token.trim();
    if t.is_empty() {
        return text.to_string();
    }
    text.replace(t, "[BOT_TOKEN]")
}

/// Hasil parsing perintah `/telegram <arg>` di REPL/TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramAction {
    Status,
    Setup {
        token_spec: KeySpec,
        user_id: Option<i64>,
    },
    Allow(i64),
    Remove(i64),
    SetApproval(String),
    Disable,
    Usage,
}

/// Parse argumen `/telegram [...]`.
pub fn parse_telegram_action(arg: Option<&str>) -> TelegramAction {
    let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return TelegramAction::Status;
    };
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    match tokens.first().copied().unwrap_or("").to_ascii_lowercase().as_str() {
        "status" | "info" => TelegramAction::Status,
        "setup" | "connect" | "set" => {
            let Some(raw_token) = tokens.get(1).copied() else {
                return TelegramAction::Usage;
            };
            let user_id = match tokens.get(2).copied() {
                Some(s) => match s.parse::<i64>() {
                    Ok(id) => Some(id),
                    Err(_) => return TelegramAction::Usage,
                },
                None => None,
            };
            // Token Telegram selalu berbentuk `<angka>:<hash>` — pastikan tidak
            // salah diklasifikasi sebagai env var.
            let token_spec = if raw_token.contains(':') {
                KeySpec::DirectKey(raw_token.to_string())
            } else {
                classify_key_spec(raw_token)
            };
            TelegramAction::Setup {
                token_spec,
                user_id,
            }
        }
        "allow" | "add" => match tokens.get(1).and_then(|s| s.parse::<i64>().ok()) {
            Some(id) => TelegramAction::Allow(id),
            None => TelegramAction::Usage,
        },
        "rm" | "remove" | "del" | "deny" => match tokens.get(1).and_then(|s| s.parse::<i64>().ok()) {
            Some(id) => TelegramAction::Remove(id),
            None => TelegramAction::Usage,
        },
        "approve" | "approval" | "mode" => match tokens.get(1).copied() {
            Some(m) => TelegramAction::SetApproval(m.to_ascii_lowercase()),
            None => TelegramAction::Usage,
        },
        "off" | "disable" | "disconnect" | "clear" => TelegramAction::Disable,
        _ => TelegramAction::Usage,
    }
}

/// Keputusan tombol inline persetujuan dari Telegram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    ApproveOnce,
    ApproveCategory,
    Deny,
}

/// Parse payload `callback_data` tombol persetujuan (`ap:<id>:y|a|n`).
pub fn parse_callback_approval(data: &str) -> Option<(u64, ApprovalDecision)> {
    let mut parts = data.trim().split(':');
    if parts.next()? != "ap" {
        return None;
    }
    let id = parts.next()?.parse::<u64>().ok()?;
    let decision = match parts.next()? {
        "y" => ApprovalDecision::ApproveOnce,
        "a" => ApprovalDecision::ApproveCategory,
        "n" => ApprovalDecision::Deny,
        _ => return None,
    };
    Some((id, decision))
}

/// Potong teks panjang menjadi beberapa potongan aman UTF-8 <= `max_chars`
/// karakter (mengutamakan batas baris `\n` agar blok kode tidak pecah di tengah baris).
pub fn split_telegram_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(64);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return vec!["(selesai tanpa output teks)".to_string()];
    }
    if trimmed.chars().count() <= limit {
        return vec![trimmed.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0usize;

    for line in trimmed.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if cur_chars + line_chars <= limit {
            cur.push_str(line);
            cur_chars += line_chars;
            continue;
        }
        if !cur.is_empty() {
            out.push(cur.trim_end().to_string());
            cur.clear();
            cur_chars = 0;
        }
        if line_chars <= limit {
            cur.push_str(line);
            cur_chars = line_chars;
        } else {
            // Baris tunggal lebih panjang dari batas -> pecah per karakter secara aman UTF-8.
            let mut buf = String::new();
            let mut n = 0usize;
            for ch in line.chars() {
                if n >= limit {
                    out.push(buf.clone());
                    buf.clear();
                    n = 0;
                }
                buf.push(ch);
                n += 1;
            }
            if !buf.is_empty() {
                cur.push_str(&buf);
                cur_chars = n;
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim_end().to_string());
    }
    if out.is_empty() {
        out.push("(selesai tanpa output teks)".to_string());
    }
    out
}

/// Daftar lengkap perintah `/` Telegram yang disinkronkan otomatis lewat `setMyCommands`.
/// Setiap nama mengikuti aturan resmi Telegram: huruf kecil `a-z`, angka `0-9`, `_` (maks 32 karakter).
pub const TELEGRAM_BOT_COMMANDS: &[(&str, &str)] = &[
    ("start", "Mulai & panduan cepat SelenCode"),
    ("help", "Daftar lengkap semua perintah"),
    ("status", "Info sesi, provider, & kapasitas konteks"),
    ("new", "Mulai sesi percakapan baru"),
    ("resume", "Lihat 10 sesi bernomor & lanjutkan sesi"),
    ("sessions", "Daftar 10 sesi terakhir di folder ini"),
    ("model", "Pilih atau kelola model AI"),
    ("models", "Cari katalog model dari API provider"),
    ("provider", "Kelola provider AI & API key"),
    ("approve", "Atur mode persetujuan (ask/edits/auto)"),
    ("sandbox", "Atur mode & isolasi Security Sandbox v2"),
    ("verify", "Jalankan cek build, test, & lint proyek"),
    ("worktree", "Isolasi refactor lewat Git Worktree"),
    ("checkpoint", "Simpan checkpoint untuk rollback aman"),
    ("rollback", "Pulihkan perubahan ke checkpoint"),
    ("undo", "Batalkan edit file terakhir"),
    ("redo", "Ulangi edit yang dibatalkan"),
    ("task", "Kelola tugas panjang lintas sesi"),
    ("jobs", "Pantau proses latar belakang"),
    ("route", "Info & budget Dynamic Model Routing"),
    ("lsp", "Diagnostik & navigasi kode LSP"),
    ("index", "Cari konteks kode repositori"),
    ("memory", "Kelola memori proyek persisten"),
    ("skills", "Daftar & cari skill agen"),
    ("skill", "Aktifkan skill ke percakapan"),
    ("learn", "Simpan pelajaran jadi skill baru"),
    ("mcp", "Status server & fitur protokol MCP"),
    ("doctor", "Cek kesehatan instalasi & lingkungan"),
    ("compact", "Ringkas riwayat konteks manual"),
    ("export", "Ekspor percakapan ke Markdown"),
    ("stop", "Hentikan giliran agent yang berjalan"),
    ("id", "Tampilkan User ID & Chat ID Telegram"),
];

/// Escape karakter khusus HTML untuk Telegram `parse_mode: "HTML"`.
pub fn escape_telegram_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Format inline Markdown (code span, link, bold, strikethrough, italic) ke Telegram HTML aman.
fn format_inline_md_to_html(line: &str) -> String {
    // 1. Pisahkan inline `code` terlebih dahulu agar simbol `*` / `_` di dalam kode tidak ikut terformat.
    let mut result = String::with_capacity(line.len() + 16);
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut plain_buf = String::new();

    while i < n {
        if chars[i] == '`' {
            // Cari penutup backtick pada baris yang sama.
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '`') {
                if !plain_buf.is_empty() {
                    result.push_str(&format_non_code_inline(&plain_buf));
                    plain_buf.clear();
                }
                let code_str: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                result.push_str("<code>");
                result.push_str(&escape_telegram_html(&code_str));
                result.push_str("</code>");
                i = i + 2 + rel_end;
                continue;
            }
        }
        plain_buf.push(chars[i]);
        i += 1;
    }
    if !plain_buf.is_empty() {
        result.push_str(&format_non_code_inline(&plain_buf));
    }
    result
}

fn format_non_code_inline(s: &str) -> String {
    let escaped = escape_telegram_html(s);
    // Link Markdown: [label](https://...)
    let mut out = String::with_capacity(escaped.len() + 16);
    let bytes = escaped.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // [label](url)
        if bytes[i] == b'[' {
            if let Some(close_br) = escaped[i + 1..].find("](") {
                let label_start = i + 1;
                let label_end = label_start + close_br;
                let url_start = label_end + 2;
                if let Some(close_par) = escaped[url_start..].find(')') {
                    let url = &escaped[url_start..url_start + close_par];
                    if url.starts_with("http://") || url.starts_with("https://") {
                        let label = &escaped[label_start..label_end];
                        let safe_url = url.replace('"', "&quot;");
                        out.push_str(&format!("<a href=\"{safe_url}\">{label}</a>"));
                        i = url_start + close_par + 1;
                        continue;
                    }
                }
            }
        }
        // **bold**
        if escaped[i..].starts_with("**") {
            if let Some(end) = escaped[i + 2..].find("**") {
                if end > 0 {
                    let inner = &escaped[i + 2..i + 2 + end];
                    out.push_str("<b>");
                    out.push_str(inner);
                    out.push_str("</b>");
                    i = i + 4 + end;
                    continue;
                }
            }
        }
        // ~~strike~~
        if escaped[i..].starts_with("~~") {
            if let Some(end) = escaped[i + 2..].find("~~") {
                if end > 0 {
                    let inner = &escaped[i + 2..i + 2 + end];
                    out.push_str("<s>");
                    out.push_str(inner);
                    out.push_str("</s>");
                    i = i + 4 + end;
                    continue;
                }
            }
        }
        // *italic* (jangan makan spasi segera setelah `*`)
        if bytes[i] == b'*' && !escaped[i..].starts_with("**") {
            if let Some(next_ch) = escaped[i + 1..].chars().next() {
                if !next_ch.is_whitespace() {
                    if let Some(end) = escaped[i + 1..].find('*') {
                        let inner = &escaped[i + 1..i + 1 + end];
                        if !inner.is_empty() && !inner.ends_with(char::is_whitespace) {
                            out.push_str("<i>");
                            out.push_str(inner);
                            out.push_str("</i>");
                            i = i + 2 + end;
                            continue;
                        }
                    }
                }
            }
        }
        let ch = escaped[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Konversi jawaban Markdown LLM menjadi Telegram HTML yang rapi, terstruktur, & mudah dibaca di HP.
pub fn markdown_to_telegram_html(md: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut quote_lines: Vec<String> = Vec::new();

    let flush_quote = |quotes: &mut Vec<String>, dst: &mut Vec<String>| {
        if !quotes.is_empty() {
            dst.push(format!("<blockquote>{}</blockquote>", quotes.join("\n")));
            quotes.clear();
        }
    };

    for raw_line in md.lines() {
        let trimmed = raw_line.trim();
        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            flush_quote(&mut quote_lines, &mut out);
            if in_code {
                let body = escape_telegram_html(&code_lines.join("\n"));
                if code_lang.is_empty() {
                    out.push(format!("<pre><code>{body}</code></pre>"));
                } else {
                    let lang = escape_telegram_html(&code_lang);
                    out.push(format!("<pre><code class=\"language-{lang}\">{body}</code></pre>"));
                }
                code_lines.clear();
                code_lang.clear();
                in_code = false;
            } else {
                code_lang = fence_rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '+')
                    .collect();
                in_code = true;
            }
            continue;
        }

        if in_code {
            code_lines.push(raw_line.to_string());
            continue;
        }

        // Blockquote `> ...`
        if let Some(q) = trimmed.strip_prefix("> ") {
            quote_lines.push(format_inline_md_to_html(q));
            continue;
        } else if trimmed == ">" {
            quote_lines.push(String::new());
            continue;
        } else {
            flush_quote(&mut quote_lines, &mut out);
        }

        // Garis pemisah horizontal (`---`, `***`, `___`)
        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            out.push("──────────────".to_string());
            continue;
        }

        // Heading Markdown (`#`, `##`, `###`, `####`) -> judul tebal rapi tanpa `###` mentah.
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|&c| c == '#').count();
            if hashes <= 6 {
                let rest = trimmed[hashes..].trim();
                if !rest.is_empty() {
                    let icon = if hashes == 1 { "📌 " } else { "▸ " };
                    out.push(format!("{icon}<b>{}</b>", format_inline_md_to_html(rest)));
                    continue;
                }
            }
        }

        // Daftar bullet (`- ...` atau `* ...`, termasuk nested)
        let indent = raw_line.chars().take_while(|&c| c == ' ').count();
        let after_indent = &raw_line[indent..];
        if let Some(item) = after_indent
            .strip_prefix("- ")
            .or_else(|| after_indent.strip_prefix("* "))
        {
            let bullet = if indent >= 2 { "  ◦ " } else { "• " };
            out.push(format!("{bullet}{}", format_inline_md_to_html(item)));
            continue;
        }

        if trimmed.is_empty() {
            // Cegah baris kosong bertumpuk berlebihan.
            if out.last().map(|s| !s.is_empty()).unwrap_or(false) {
                out.push(String::new());
            }
            continue;
        }

        out.push(format_inline_md_to_html(raw_line));
    }

    flush_quote(&mut quote_lines, &mut out);
    if in_code {
        let body = escape_telegram_html(&code_lines.join("\n"));
        out.push(format!("<pre><code>{body}</code></pre>"));
    }

    out.join("\n").trim().to_string()
}

/// Konversi kartu menu SelenCode (`# Judul`, `* a | b | c`, `> a | b | c`, `---`, `! catatan`)
/// menjadi kartu HTML Telegram yang bersih, rapi, dan enak dibaca di layar HP.
pub fn menu_markup_to_telegram_html(markup: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in markup.lines() {
        let t = raw.trim();
        if t.is_empty() {
            if out.last().map(|s| !s.is_empty()).unwrap_or(false) {
                out.push(String::new());
            }
            continue;
        }
        if let Some(title) = t.strip_prefix("# ") {
            out.push(format!("🌙 <b>{}</b>", escape_telegram_html(title.trim())));
            continue;
        }
        if t == "---" {
            out.push("──────────────".to_string());
            continue;
        }
        if let Some(note) = t.strip_prefix("! ") {
            out.push(format!("💡 <i>{}</i>", format_inline_md_to_html(note.trim())));
            continue;
        }
        if let Some(rest) = t.strip_prefix("* ").or_else(|| t.strip_prefix("> ")) {
            let icon = if t.starts_with("* ") { "✦" } else { "•" };
            let cols: Vec<&str> = rest.split('|').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            match cols.as_slice() {
                [c1] => out.push(format!("{icon} <b>{}</b>", escape_telegram_html(c1))),
                [c1, c2] => out.push(format!(
                    "{icon} <b>{}</b> — {}",
                    escape_telegram_html(c1),
                    escape_telegram_html(c2)
                )),
                [c1, c2, rest_cols @ ..] => {
                    let tail = rest_cols.join(" · ");
                    out.push(format!(
                        "{icon} <b>{}</b> — {}\n   <i>{}</i>",
                        escape_telegram_html(c1),
                        escape_telegram_html(c2),
                        escape_telegram_html(&tail)
                    ));
                }
                _ => out.push(format!("{icon} {}", escape_telegram_html(rest))),
            }
            continue;
        }
        out.push(format_inline_md_to_html(raw));
    }
    out.join("\n").trim().to_string()
}

/// Pecah teks Markdown panjang menjadi potongan <= `max_chars` karakter dengan tetap
/// menutup & membuka ulang pagar blok kode ```` ``` ```` bila pemotongan jatuh di tengah blok kode.
pub fn split_markdown_for_telegram(md: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(256);
    let trimmed = md.trim();
    if trimmed.is_empty() {
        return vec!["(selesai tanpa output teks)".to_string()];
    }
    if trimmed.chars().count() <= limit {
        return vec![trimmed.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0usize;
    let mut open_fence: Option<String> = None;

    for line in trimmed.split_inclusive('\n') {
        let line_trim = line.trim();
        let line_chars = line.chars().count();
        // Sisakan ruang cadangan 32 char untuk penutup fence bila diperlukan.
        if cur_chars + line_chars + 32 > limit && !cur.trim().is_empty() {
            if open_fence.is_some() {
                if !cur.ends_with('\n') {
                    cur.push('\n');
                }
                cur.push_str("```");
            }
            chunks.push(cur.trim_end().to_string());
            cur.clear();
            cur_chars = 0;
            if let Some(ref lang) = open_fence {
                let reopen = format!("```{lang}\n");
                cur_chars = reopen.chars().count();
                cur.push_str(&reopen);
            }
        }
        if let Some(rest) = line_trim.strip_prefix("```") {
            if open_fence.is_some() {
                open_fence = None;
            } else {
                open_fence = Some(rest.trim().to_string());
            }
        }
        if line_chars > limit {
            for part in split_telegram_chunks(line, limit.saturating_sub(32)) {
                chunks.push(part);
            }
            continue;
        }
        cur.push_str(line);
        cur_chars += line_chars;
    }
    if !cur.trim().is_empty() {
        chunks.push(cur.trim_end().to_string());
    }
    if chunks.is_empty() {
        chunks.push("(selesai tanpa output teks)".to_string());
    }
    chunks
}

/// Potong string aman UTF-8 maksimal `max_chars` karakter.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}\n… (dipotong)")
}

// ---------- Tipe respons Telegram Bot API ----------

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgUser {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgChat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgPhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgFileInfo {
    pub file_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: TgChat,
    #[serde(default)]
    pub from: Option<TgUser>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub photo: Vec<TgPhotoSize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgCallbackQuery {
    pub id: String,
    pub from: TgUser,
    #[serde(default)]
    pub message: Option<TgMessage>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TgMessage>,
    #[serde(default)]
    pub callback_query: Option<TgCallbackQuery>,
}

/// Klien ringan Telegram Bot API berbasis `reqwest`.
#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    token: String,
    base_url: String,
}

impl TelegramClient {
    pub fn new(bot_token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .context("gagal membuat HTTP client Telegram")?;
        let tok = bot_token.trim().to_string();
        Ok(Self {
            http,
            base_url: format!("https://api.telegram.org/bot{tok}"),
            token: tok,
        })
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        let url = format!("{}/{method}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
                anyhow!("gagal menghubungi Telegram API ({method}): {clean}")
            })?;
        let status = resp.status();
        let parsed: ApiResponse<T> = resp.json().await.map_err(|e| {
            let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
            anyhow!("respons Telegram API ({method}, HTTP {status}) tidak valid: {clean}")
        })?;
        if !parsed.ok {
            let desc = redact_tg_token(
                parsed
                    .description
                    .as_deref()
                    .unwrap_or("permintaan ditolak"),
                &self.token,
            );
            return Err(anyhow!("Telegram API ({method}) menolak (HTTP {status}): {desc}"));
        }
        parsed
            .result
            .ok_or_else(|| anyhow!("Telegram API ({method}) mengembalikan result kosong"))
    }

    /// Lewati antrean pesan lama sebelum gateway dinyalakan agar perintah lama
    /// tidak ikut tereksekusi saat `selen telegram` baru mulai.
    pub async fn skip_backlog(&self) -> i64 {
        match self.get_updates(-1, 0).await {
            Ok(list) => list.last().map(|u| u.update_id + 1).unwrap_or(0),
            Err(_) => 0,
        }
    }

    pub async fn get_me(&self) -> Result<TgUser> {
        self.post_json("getMe", json!({})).await
    }

    /// Sinkronkan daftar lengkap menu perintah `/` ke Telegram Bot API (`setMyCommands`)
    /// agar pengguna langsung melihat seluruh perintah SelenCode saat mengetik `/` di Telegram.
    pub async fn sync_bot_commands(&self) -> Result<()> {
        let commands: Vec<Value> = TELEGRAM_BOT_COMMANDS
            .iter()
            .map(|(cmd, desc)| json!({ "command": cmd, "description": desc }))
            .collect();
        let _: Value = self
            .post_json("setMyCommands", json!({ "commands": commands }))
            .await?;
        Ok(())
    }

    /// Ambil metadata file Telegram (`getFile`) untuk mengunduh foto/lampiran pengguna.
    pub async fn get_file(&self, file_id: &str) -> Result<TgFileInfo> {
        self.post_json("getFile", json!({ "file_id": file_id })).await
    }

    /// Unduh byte mentah file Telegram berdasarkan `file_path` dari `getFile`.
    pub async fn download_file_bytes(&self, file_path: &str) -> Result<Vec<u8>> {
        let url = format!("https://api.telegram.org/file/bot{}/{file_path}", self.token);
        let resp = self.http.get(&url).send().await.map_err(|e| {
            let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
            anyhow!("gagal mengunduh file Telegram: {clean}")
        })?;
        if !resp.status().is_success() {
            return Err(anyhow!("unduh file Telegram ditolak (HTTP {})", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| {
            let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
            anyhow!("gagal membaca byte file Telegram: {clean}")
        })?;
        Ok(bytes.to_vec())
    }

    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<TgUpdate>> {
        self.post_json(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"]
            }),
        )
        .await
    }

    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<i64> {
        let msg: TgMessage = self
            .post_json(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": text,
                }),
            )
            .await?;
        Ok(msg.message_id)
    }

    /// Kirim pesan berformat Telegram HTML dengan fallback otomatis ke teks polos bila HTML ditolak.
    pub async fn send_html_message(
        &self,
        chat_id: i64,
        html: &str,
        plain_fallback: &str,
    ) -> Result<i64> {
        let res: Result<TgMessage> = self
            .post_json(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                }),
            )
            .await;
        match res {
            Ok(msg) => Ok(msg.message_id),
            Err(_) => self.send_message(chat_id, plain_fallback).await,
        }
    }

    /// Kirim teks Markdown panjang secara rapi sebagai Telegram HTML (terpecah aman per chunk + fallback).
    pub async fn send_long_markdown(&self, chat_id: i64, markdown: &str) -> Result<()> {
        // Sisakan margin karena tag HTML (`<b>`, `<code>`, `&amp;`) memperpanjang teks sedikit.
        for chunk in split_markdown_for_telegram(markdown, 3200) {
            let html = markdown_to_telegram_html(&chunk);
            if html.chars().count() <= 4050 {
                self.send_html_message(chat_id, &html, &chunk).await?;
            } else {
                for sub in split_telegram_chunks(&chunk, TELEGRAM_MAX_CHARS) {
                    let sub_html = markdown_to_telegram_html(&sub);
                    if sub_html.chars().count() <= 4050 {
                        self.send_html_message(chat_id, &sub_html, &sub).await?;
                    } else {
                        self.send_message(chat_id, &sub).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Kirim kartu menu SelenCode sebagai kartu Telegram HTML rapi (otomatis terpecah & fallback aman).
    pub async fn send_menu_markup(&self, chat_id: i64, markup: &str) -> Result<()> {
        let plain = crate::repl::strip_menu_markup(markup);
        let html = menu_markup_to_telegram_html(markup);
        if html.chars().count() <= 4050 {
            self.send_html_message(chat_id, &html, &plain).await?;
            return Ok(());
        }
        for chunk in split_telegram_chunks(markup, 2800) {
            let chunk_html = menu_markup_to_telegram_html(&chunk);
            let chunk_plain = crate::repl::strip_menu_markup(&chunk);
            if chunk_html.chars().count() <= 4050 {
                self.send_html_message(chat_id, &chunk_html, &chunk_plain).await?;
            } else {
                self.send_message(chat_id, &chunk_plain).await?;
            }
        }
        Ok(())
    }

    /// Kirim pesan panjang secara otomatis terpecah agar tidak melewati batas 4096 karakter.
    pub async fn send_long_message(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in split_telegram_chunks(text, TELEGRAM_MAX_CHARS) {
            self.send_message(chat_id, &chunk).await?;
        }
        Ok(())
    }

    pub async fn send_message_with_keyboard(
        &self,
        chat_id: i64,
        text: &str,
        rows: Vec<Vec<(String, String)>>,
    ) -> Result<i64> {
        let inline_keyboard: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(label, cb)| json!({ "text": label, "callback_data": cb }))
                    .collect()
            })
            .collect();
        let msg: TgMessage = self
            .post_json(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": text,
                    "reply_markup": { "inline_keyboard": inline_keyboard },
                }),
            )
            .await?;
        Ok(msg.message_id)
    }

    pub async fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let _: Value = self
            .post_json(
                "editMessageText",
                json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "text": text,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn answer_callback_query(&self, callback_id: &str, text: Option<&str>) -> Result<()> {
        let mut payload = json!({ "callback_query_id": callback_id });
        if let Some(t) = text {
            payload["text"] = json!(t);
        }
        let _: Value = self.post_json("answerCallbackQuery", payload).await?;
        Ok(())
    }

    pub async fn send_typing(&self, chat_id: i64) {
        let _ = self
            .post_json::<Value>(
                "sendChatAction",
                json!({ "chat_id": chat_id, "action": "typing" }),
            )
            .await;
    }
}

// ---------- Jembatan persetujuan (ApprovalRequest <-> tombol inline Telegram) ----------

pub struct PendingTgApproval {
    pub chat_id: i64,
    pub message_id: i64,
    pub base_text: String,
    pub category: String,
    pub policy: Arc<Mutex<ApprovalPolicy>>,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Penyimpan permintaan persetujuan aktif dari tool yang menunggu tap tombol di Telegram.
#[derive(Default)]
pub struct TgApprovalHub {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingTgApproval>>,
}

impl TgApprovalHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Kirim pesan persetujuan dengan 3 tombol inline ke Telegram & daftarkan callback-nya.
    /// Otomatis kedaluwarsa setelah `APPROVAL_TIMEOUT_SECS` agar chat tidak pernah macet selamanya.
    pub async fn dispatch(
        self: &Arc<Self>,
        tg: &TelegramClient,
        chat_id: i64,
        policy: Arc<Mutex<ApprovalPolicy>>,
        req: ApprovalRequest,
    ) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let category = tool_category_from_title(&req.title);
        let detail_block = if req.detail.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n{}", truncate_chars(req.detail.trim(), 1500))
        };
        let base_text = format!(
            "🔐 Perlu Persetujuan SelenCode\nTool: {category}\nAksi: {}{detail_block}",
            req.title
        );
        let rows = vec![
            vec![
                ("✅ Setuju".to_string(), format!("ap:{id}:y")),
                ("❌ Tolak".to_string(), format!("ap:{id}:n")),
            ],
            vec![(
                format!("⚡ Selalu {category} sesi ini"),
                format!("ap:{id}:a"),
            )],
        ];
        match tg.send_message_with_keyboard(chat_id, &base_text, rows).await {
            Ok(message_id) => {
                if let Ok(mut map) = self.pending.lock() {
                    map.insert(
                        id,
                        PendingTgApproval {
                            chat_id,
                            message_id,
                            base_text,
                            category,
                            policy,
                            reply: req.reply,
                        },
                    );
                }
                let hub_exp = Arc::clone(self);
                let tg_exp = tg.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(APPROVAL_TIMEOUT_SECS)).await;
                    let expired = hub_exp.pending.lock().ok().and_then(|mut m| m.remove(&id));
                    if let Some(item) = expired {
                        let _ = item.reply.send(false);
                        let updated = format!(
                            "{}\n\n⏰ Persetujuan kedaluwarsa (ditolak otomatis).",
                            item.base_text
                        );
                        let _ = tg_exp
                            .edit_message_text(item.chat_id, item.message_id, &updated)
                            .await;
                    }
                });
            }
            Err(_) => {
                // Gagal mengirim pesan approval -> tolak demi keamanan.
                let _ = req.reply.send(false);
            }
        }
    }

    /// Daftarkan approval secara langsung (khusus helper tes unit tanpa jaringan).
    pub fn register_for_test(&self, chat_id: i64, item: PendingTgApproval) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = chat_id;
        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, item);
        }
        id
    }

    /// Batalkan & tolak semua permintaan approval yang tertunda untuk `chat_id` (mis. `/cancel`).
    pub fn cancel_chat_approvals(&self, chat_id: i64) -> usize {
        let removed: Vec<PendingTgApproval> = {
            let Ok(mut map) = self.pending.lock() else {
                return 0;
            };
            let ids: Vec<u64> = map
                .iter()
                .filter_map(|(k, v)| (v.chat_id == chat_id).then_some(*k))
                .collect();
            ids.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        let count = removed.len();
        for item in removed {
            let _ = item.reply.send(false);
        }
        count
    }

    /// Proses `callback_query` dari Telegram. Mengembalikan `true` jika cocok dengan approval aktif.
    pub async fn handle_callback(&self, tg: &TelegramClient, cb: &TgCallbackQuery) -> bool {
        let Some(data) = cb.data.as_deref() else {
            return false;
        };
        let Some((id, decision)) = parse_callback_approval(data) else {
            return false;
        };
        let cb_chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
        let entry = {
            let Ok(mut map) = self.pending.lock() else {
                return false;
            };
            if let Some(existing) = map.get(&id) {
                if existing.chat_id != cb_chat_id {
                    None
                } else {
                    map.remove(&id)
                }
            } else {
                None
            }
        };
        let Some(item) = entry else {
            let _ = tg
                .answer_callback_query(
                    &cb.id,
                    Some("Permintaan persetujuan ini sudah kedaluwarsa atau bukan dari chat ini."),
                )
                .await;
            return true;
        };
        let (approved, toast, badge) = match decision {
            ApprovalDecision::ApproveOnce => (true, "Disetujui ✅", "✅ Disetujui"),
            ApprovalDecision::ApproveCategory => {
                if let Ok(mut pol) = item.policy.lock() {
                    pol.allow_category(item.category.clone());
                }
                (
                    true,
                    "Selalu diizinkan sesi ini ⚡",
                    "⚡ Selalu diizinkan untuk sisa sesi ini",
                )
            }
            ApprovalDecision::Deny => (false, "Ditolak ❌", "❌ Ditolak"),
        };
        let _ = item.reply.send(approved);
        let _ = tg.answer_callback_query(&cb.id, Some(toast)).await;
        let updated = format!("{}\n\n{badge}", item.base_text);
        let _ = tg
            .edit_message_text(item.chat_id, item.message_id, &updated)
            .await;
        true
    }
}
