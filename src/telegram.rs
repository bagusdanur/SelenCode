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
    Test,
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
        "test" | "ping" | "check" => TelegramAction::Test,
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

/// Parse payload `callback_data` tombol review staging (`rv:<id>:a|d`).
pub fn parse_callback_review(data: &str) -> Option<(i64, bool)> {
    let mut parts = data.trim().split(':');
    if parts.next()? != "rv" {
        return None;
    }
    let id = parts.next()?.parse::<i64>().ok()?;
    let approve = match parts.next()? {
        "a" => true,
        "d" => false,
        _ => return None,
    };
    Some((id, approve))
}

/// Parse payload `callback_data` tombol klarifikasi `ask_user` (`ask:<id>:<choice_idx>`).
/// Terpisah total dari callback approval (`ap:...`).
pub fn parse_callback_ask(data: &str) -> Option<(u64, usize)> {
    let mut parts = data.trim().split(':');
    if parts.next()? != "ask" {
        return None;
    }
    let id = parts.next()?.parse::<u64>().ok()?;
    let idx = parts.next()?.parse::<usize>().ok()?;
    Some((id, idx))
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
    ("review", "Lihat & putuskan usulan background review"),
    ("cron", "Kelola tugas terjadwal (add/list/remove/pause/resume)"),
    ("watch", "Pantau tugas asinkron (list/cancel)"),
    ("usage", "Ringkasan pemakaian token sesi & hari ini"),
    ("search", "Cari percakapan lintas sesi tersimpan (FTS5)"),
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

/// Batas baris blok kode di chat Telegram sebelum otomatis diekstrak menjadi lampiran berkas (`sendDocument`).
pub const TELEGRAM_LONG_CODE_MAX_LINES: usize = 25;
/// Batas karakter blok kode di chat Telegram sebelum otomatis diekstrak menjadi lampiran berkas (`sendDocument`).
pub const TELEGRAM_LONG_CODE_MAX_CHARS: usize = 1500;

/// Blok kode panjang yang diekstrak dari jawaban Markdown untuk dikirim sebagai berkas lampiran Telegram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedCodeBlock {
    pub index: usize,
    pub lang: String,
    pub filename: String,
    pub lines: usize,
    pub content: String,
}

fn code_ext_for_lang(lang: &str) -> &'static str {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rs",
        "py" | "python" => "py",
        "js" | "javascript" | "mjs" | "cjs" => "js",
        "ts" | "typescript" => "ts",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" | "shell" => "sh",
        "ps1" | "powershell" => "ps1",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "go" => "go",
        "c" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "java" => "java",
        "md" | "markdown" => "md",
        "diff" | "patch" => "diff",
        "log" => "log",
        _ => "txt",
    }
}

/// Ekstrak blok kode pagar panjang (`>= 25` baris ATAU `>= 1.500` karakter) dari Markdown
/// agar dikirim sebagai berkas lampiran Telegram (`sendDocument`) + 1 baris ringkasan di chat,
/// sementara blok kode pendek tetap dirender rapi sebagai `<pre><code>`.
pub fn extract_long_code_blocks_for_telegram(md: &str) -> (String, Vec<ExtractedCodeBlock>) {
    let mut out_lines: Vec<String> = Vec::new();
    let mut extracted: Vec<ExtractedCodeBlock> = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut fence_open_line = String::new();
    let mut code_lines: Vec<String> = Vec::new();

    for raw_line in md.lines() {
        let trimmed = raw_line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_code {
                let body = code_lines.join("\n");
                let line_cnt = code_lines.len();
                let char_cnt = body.chars().count();
                if line_cnt >= TELEGRAM_LONG_CODE_MAX_LINES || char_cnt >= TELEGRAM_LONG_CODE_MAX_CHARS
                {
                    let idx = extracted.len() + 1;
                    let ext = code_ext_for_lang(&code_lang);
                    let filename = format!("kode_{idx}.{ext}");
                    let lang_note = if code_lang.is_empty() {
                        String::new()
                    } else {
                        format!(", {code_lang}")
                    };
                    out_lines.push(format!(
                        "Lampiran berkas `{filename}` ({line_cnt} baris{lang_note}) — dikirim sebagai dokumen di bawah."
                    ));
                    extracted.push(ExtractedCodeBlock {
                        index: idx,
                        lang: code_lang.clone(),
                        filename,
                        lines: line_cnt,
                        content: body,
                    });
                } else {
                    out_lines.push(fence_open_line.clone());
                    out_lines.extend(code_lines.iter().cloned());
                    out_lines.push("```".to_string());
                }
                code_lines.clear();
                code_lang.clear();
                fence_open_line.clear();
                in_code = false;
            } else {
                code_lang = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '+')
                    .collect();
                fence_open_line = raw_line.to_string();
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_lines.push(raw_line.to_string());
        } else {
            out_lines.push(raw_line.to_string());
        }
    }

    if in_code {
        let body = code_lines.join("\n");
        let line_cnt = code_lines.len();
        let char_cnt = body.chars().count();
        if line_cnt >= TELEGRAM_LONG_CODE_MAX_LINES || char_cnt >= TELEGRAM_LONG_CODE_MAX_CHARS {
            let idx = extracted.len() + 1;
            let ext = code_ext_for_lang(&code_lang);
            let filename = format!("kode_{idx}.{ext}");
            out_lines.push(format!(
                "Lampiran berkas `{filename}` ({line_cnt} baris) — dikirim sebagai dokumen di bawah."
            ));
            extracted.push(ExtractedCodeBlock {
                index: idx,
                lang: code_lang,
                filename,
                lines: line_cnt,
                content: body,
            });
        } else {
            out_lines.push(fence_open_line);
            out_lines.extend(code_lines);
            out_lines.push("```".to_string());
        }
    }

    (out_lines.join("\n"), extracted)
}

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

fn is_url_token(s: &str) -> bool {
    let t = s.trim();
    (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
}

fn is_plain_status_word(s: &str) -> bool {
    let up = s.trim().to_ascii_uppercase();
    matches!(
        up.as_str(),
        "SELESAI"
            | "GAGAL"
            | "BERJALAN"
            | "AKTIF"
            | "SUKSES"
            | "LULUS"
            | "TERTUNDA"
            | "DIJEDA"
            | "DIBATALKAN"
            | "KEDALUWARSA"
            | "PENDING"
            | "QUEUED"
            | "RUNNING"
            | "PROCESSING"
            | "COMPLETED"
            | "COMPLETE"
            | "FAILED"
            | "FAIL"
            | "DONE"
            | "ERROR"
            | "OK"
            | "PASS"
            | "SUCCESS"
            | "IN_PROGRESS"
    )
}

/// Klasifikasi isi backtick sebaris untuk Telegram HP:
/// - URL TIDAK PERNAH dibungkus `<code>` (dibiarkan polos agar langsung bisa disentuh/diklik di HP).
/// - Kata status biasa (mis. `SELESAI`, `FAILED`, `PROCESSING`) tidak dibungkus monospace kaku.
/// - Kalimat prosa panjang (>64 char dengan spasi & tanpa penanda teknis) juga tidak dibungkus `<code>`.
/// - Istilah teknis pendek (nama berkas, perintah, id, simbol kode) dibungkus `<code>`.
fn render_inline_backtick_html(code_str: &str) -> String {
    let t = code_str.trim();
    if t.is_empty() {
        return String::new();
    }
    if is_url_token(t) {
        return escape_telegram_html(t);
    }
    if is_plain_status_word(t) {
        return escape_telegram_html(t);
    }
    let has_tech_hint = t.contains('/')
        || t.contains('\\')
        || t.contains('_')
        || t.contains('.')
        || t.contains("--")
        || t.contains("::")
        || t.contains('(')
        || t.contains('=')
        || t.starts_with('#')
        || t.starts_with('$');
    if t.chars().count() > 64 && t.contains(' ') && !has_tech_hint {
        return escape_telegram_html(t);
    }
    format!("<code>{}</code>", escape_telegram_html(code_str))
}

/// Format inline Markdown (code span, link, bold `**`/`__`, strikethrough `~~`, italic `*`) ke Telegram HTML aman.
/// Memakai placeholder untuk backtick agar `**` / `__` yang membungkus `` `kode` `` tidak pernah bocor sebagai bintang mentah!
fn format_inline_md_to_html(line: &str) -> String {
    let mut placeholders: Vec<(String, String)> = Vec::new();
    let mut stripped = String::with_capacity(line.len() + 16);
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0usize;

    while i < n {
        if chars[i] == '`' {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let code_str: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                let token = format!("\x00TGCS{}\x00", placeholders.len());
                let rendered = render_inline_backtick_html(&code_str);
                placeholders.push((token.clone(), rendered));
                stripped.push_str(&token);
                i = i + 2 + rel_end;
                continue;
            }
        }
        stripped.push(chars[i]);
        i += 1;
    }

    let mut formatted = format_non_code_inline(&stripped);
    for (token, rendered) in placeholders {
        formatted = formatted.replace(&token, &rendered);
    }
    formatted
}

fn format_non_code_inline(s: &str) -> String {
    let escaped = escape_telegram_html(s);
    let mut out = String::with_capacity(escaped.len() + 16);
    let bytes = escaped.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Link Markdown: [label](https://...)
        if bytes[i] == b'[' {
            if let Some(close_br) = escaped[i + 1..].find("](") {
                let label_start = i + 1;
                let label_end = label_start + close_br;
                let url_start = label_end + 2;
                if let Some(close_par) = escaped[url_start..].find(')') {
                    let url = escaped[url_start..url_start + close_par].trim();
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
                    out.push_str(&format_non_code_inline_already_escaped(inner));
                    out.push_str("</b>");
                    i = i + 4 + end;
                    continue;
                }
            }
        }
        // __bold__ (hanya bila bukan underscore di tengah pengenal seperti foo__bar)
        if escaped[i..].starts_with("__") {
            let prev_ok = i == 0
                || !escaped[..i]
                    .chars()
                    .next_back()
                    .map(|c| c.is_alphanumeric())
                    .unwrap_or(false);
            if prev_ok {
                if let Some(end) = escaped[i + 2..].find("__") {
                    if end > 0 {
                        let after_idx = i + 4 + end;
                        let next_ok = after_idx >= escaped.len()
                            || !escaped[after_idx..]
                                .chars()
                                .next()
                                .map(|c| c.is_alphanumeric())
                                .unwrap_or(false);
                        if next_ok {
                            let inner = &escaped[i + 2..i + 2 + end];
                            out.push_str("<b>");
                            out.push_str(&format_non_code_inline_already_escaped(inner));
                            out.push_str("</b>");
                            i = after_idx;
                            continue;
                        }
                    }
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

/// Format inline ringan untuk teks yang sudah di-escape HTML (menangani link/strike/italic di dalam `<b>`).
fn format_non_code_inline_already_escaped(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let bytes = escaped.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close_br) = escaped[i + 1..].find("](") {
                let label_start = i + 1;
                let label_end = label_start + close_br;
                let url_start = label_end + 2;
                if let Some(close_par) = escaped[url_start..].find(')') {
                    let url = escaped[url_start..url_start + close_par].trim();
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
        if escaped[i..].starts_with("~~") {
            if let Some(end) = escaped[i + 2..].find("~~") {
                if end > 0 {
                    out.push_str("<s>");
                    out.push_str(&escaped[i + 2..i + 2 + end]);
                    out.push_str("</s>");
                    i = i + 4 + end;
                    continue;
                }
            }
        }
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

fn split_md_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn is_md_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('-') || !t.contains('|') {
        return false;
    }
    let cells = split_md_table_row(t);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty() && s.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ') && s.contains('-')
        })
}

fn is_md_table_row_candidate(line: &str) -> bool {
    let t = line.trim();
    t.contains('|') && split_md_table_row(t).len() >= 2
}

/// Konversi jawaban Markdown LLM menjadi Telegram HTML yang rapi, terstruktur, & mudah dibaca di HP:
/// - `**tebal**` / `__tebal__` → `<b>`; `*miring*` → `<i>`; `~~coret~~` → `<s>`
/// - Backtick sebaris → `<code>` hanya untuk istilah teknis pendek; URL tidak pernah dibungkus `<code>`
/// - Header `#`/`##`/`###` → baris `<b>` dengan jarak baris kosong sebelum/sesudahnya
/// - Tabel markdown → baris kunci-nilai bertumpuk (`• Kunci: nilai`) agar rapi di layar HP
/// - Blok kode → `<pre><code>` dengan escape HTML penuh
pub fn markdown_to_telegram_html(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut quote_lines: Vec<String> = Vec::new();
    let mut need_blank_after_heading = false;

    let flush_quote = |quotes: &mut Vec<String>, dst: &mut Vec<String>| {
        if !quotes.is_empty() {
            dst.push(format!("<blockquote>{}</blockquote>", quotes.join("\n")));
            quotes.clear();
        }
    };

    let push_blank_if_needed = |dst: &mut Vec<String>| {
        if dst.last().map(|s| !s.is_empty()).unwrap_or(false) {
            dst.push(String::new());
        }
    };

    let mut idx = 0usize;
    while idx < lines.len() {
        let raw_line = lines[idx];
        let trimmed = raw_line.trim();
        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            flush_quote(&mut quote_lines, &mut out);
            need_blank_after_heading = false;
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
            idx += 1;
            continue;
        }

        if in_code {
            code_lines.push(raw_line.to_string());
            idx += 1;
            continue;
        }

        // Tabel Markdown (`| Kol1 | Kol2 |` diikuti baris pemisah `| --- | --- |`) -> tumpukan kunci-nilai rapi di HP.
        if is_md_table_row_candidate(raw_line)
            && idx + 1 < lines.len()
            && is_md_table_separator(lines[idx + 1])
        {
            flush_quote(&mut quote_lines, &mut out);
            need_blank_after_heading = false;
            let headers = split_md_table_row(raw_line);
            idx += 2; // lewati header & garis pemisah
            let mut row_idx = 0usize;
            while idx < lines.len() && is_md_table_row_candidate(lines[idx]) && !lines[idx].trim().is_empty() {
                let cells = split_md_table_row(lines[idx]);
                if row_idx > 0 {
                    push_blank_if_needed(&mut out);
                }
                for (ci, cell) in cells.iter().enumerate() {
                    let val = cell.trim();
                    if val.is_empty() {
                        continue;
                    }
                    let hdr = headers.get(ci).map(|s| s.as_str()).unwrap_or("");
                    let prefix = if ci == 0 { "• " } else { "  – " };
                    if hdr.is_empty() {
                        out.push(format!("{prefix}{}", format_inline_md_to_html(val)));
                    } else {
                        out.push(format!(
                            "{prefix}<b>{}:</b> {}",
                            escape_telegram_html(hdr),
                            format_inline_md_to_html(val)
                        ));
                    }
                }
                row_idx += 1;
                idx += 1;
            }
            push_blank_if_needed(&mut out);
            continue;
        }

        // Blockquote `> ...`
        if let Some(q) = trimmed.strip_prefix("> ") {
            if need_blank_after_heading {
                push_blank_if_needed(&mut out);
                need_blank_after_heading = false;
            }
            quote_lines.push(format_inline_md_to_html(q));
            idx += 1;
            continue;
        } else if trimmed == ">" {
            quote_lines.push(String::new());
            idx += 1;
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
            need_blank_after_heading = false;
            out.push("──────────────".to_string());
            idx += 1;
            continue;
        }

        // Heading Markdown (`#`..`######`) -> baris `<b>` dengan jarak baris kosong sebelum/sesudahnya.
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|&c| c == '#').count();
            if hashes <= 6 {
                let rest = trimmed[hashes..].trim();
                if !rest.is_empty() {
                    push_blank_if_needed(&mut out);
                    let heading_html = format_inline_md_to_html(rest);
                    // Hindari pembungkusan `<b><b>...</b></b>` bila judul sudah mengandung `**`.
                    let clean_heading = heading_html.replace("<b>", "").replace("</b>", "");
                    out.push(format!("<b>{clean_heading}</b>"));
                    need_blank_after_heading = true;
                    idx += 1;
                    continue;
                }
            }
        }

        if trimmed.is_empty() {
            need_blank_after_heading = false;
            push_blank_if_needed(&mut out);
            idx += 1;
            continue;
        }

        if need_blank_after_heading {
            push_blank_if_needed(&mut out);
            need_blank_after_heading = false;
        }

        // Daftar bullet (`- ...`, `* ...`, `+ ...`, termasuk nested)
        let indent = raw_line.chars().take_while(|&c| c == ' ').count();
        let after_indent = &raw_line[indent..];
        if let Some(item) = after_indent
            .strip_prefix("- ")
            .or_else(|| after_indent.strip_prefix("* "))
            .or_else(|| after_indent.strip_prefix("+ "))
        {
            let bullet = if indent >= 2 { "  ◦ " } else { "• " };
            out.push(format!("{bullet}{}", format_inline_md_to_html(item)));
            idx += 1;
            continue;
        }

        out.push(format_inline_md_to_html(raw_line));
        idx += 1;
    }

    flush_quote(&mut quote_lines, &mut out);
    if in_code {
        let body = escape_telegram_html(&code_lines.join("\n"));
        out.push(format!("<pre><code>{body}</code></pre>"));
    }

    out.join("\n").trim().to_string()
}

/// Bersihkan Markdown mentah menjadi TEKS POLOS rapi (tanpa bintang `**`, tanpa backtick mentah,
/// tanpa pipa tabel, mempertahankan struktur baris) — dipakai sebagai fallback anti-gagal-parse Telegram!
pub fn markdown_to_clean_plain_text(md: &str) -> String {
    strip_telegram_html_tags(&markdown_to_telegram_html(md))
}

/// Buang tag HTML Telegram & unescape entitas HTML menjadi teks polos bersih (mempertahankan baris).
pub fn strip_telegram_html_tags(html: &str) -> String {
    let mut no_tags = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in html.chars() {
        match ch {
            '<' if !in_tag => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                // Untuk `<a href="URL">`, tidak perlu apa-apa di sini; teks label tetap keluar di luar tag.
                tag_buf.clear();
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ => no_tags.push(ch),
        }
    }
    if in_tag {
        no_tags.push('<');
        no_tags.push_str(&tag_buf);
    }
    no_tags
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .trim()
        .to_string()
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
            out.push(format!("<b>{}</b>", escape_telegram_html(title.trim())));
            continue;
        }
        if t == "---" {
            out.push("──────────────".to_string());
            continue;
        }
        if let Some(note) = t.strip_prefix("! ") {
            out.push(format!("<i>{}</i>", format_inline_md_to_html(note.trim())));
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

/// Format status progres live (diedit in-place pada gelembung sementara di Telegram).
/// Rapi, teks polos terstruktur tanpa emoji dekoratif (v0.5.0).
pub fn format_telegram_progress_html(
    step: usize,
    max_steps: usize,
    active_tool: Option<&str>,
    tools_done: usize,
    preview_text: &str,
) -> String {
    let step_tag = if step > 0 && max_steps > 0 {
        format!("langkah {step}/{max_steps}")
    } else if step > 0 {
        format!("langkah {step}")
    } else {
        String::new()
    };
    let trimmed = preview_text.trim();
    if !trimmed.is_empty() && active_tool.is_none() {
        let chars: Vec<char> = trimmed.chars().collect();
        let snippet: String = if chars.len() > 700 {
            format!("…{}", chars[chars.len() - 700..].iter().collect::<String>())
        } else {
            trimmed.to_string()
        };
        let header = if step_tag.is_empty() {
            "<b>Mengetik...</b>".to_string()
        } else {
            format!("<b>Mengetik...</b> <i>({step_tag})</i>")
        };
        let rendered_snippet = snippet
            .lines()
            .map(format_inline_md_to_html)
            .collect::<Vec<_>>()
            .join("\n");
        return format!("{header}\n\n<blockquote>{rendered_snippet}</blockquote>");
    }
    let mut parts: Vec<String> = Vec::new();
    if !step_tag.is_empty() {
        parts.push(step_tag);
    }
    if let Some(tool) = active_tool {
        if let Some((tname, tprev)) = tool.split_once(" — ") {
            parts.push(format!(
                "<code>{}</code> — <code>{}</code>",
                escape_telegram_html(tname),
                escape_telegram_html(tprev)
            ));
        } else {
            parts.push(format!("<code>{}</code>", escape_telegram_html(tool)));
        }
    }
    let mut out = if parts.is_empty() {
        "<b>Sedang bekerja...</b>".to_string()
    } else {
        format!("<b>Sedang bekerja</b> — {}", parts.join(" · "))
    };
    if tools_done > 0 {
        out.push_str(&format!("\n<i>{tools_done} aksi tool selesai</i>"));
    }
    out
}

/// Bangun kartu persetujuan Telegram bergaya rapi & hemat emoji (maks 1 emoji ⚠️ di judul)
/// dengan perintah/aksi UTUH tanpa pernah dipotong diam-diam (v0.5.0).
///
/// Mengembalikan `(preamble_html_chunks, card_html, card_plain)`:
/// - Bila seluruh perintah & alasan muat di 1 pesan (`<= 3800` char HTML), `preamble_html_chunks` kosong
///   dan perintah utuh berada langsung di dalam `card_html`.
/// - Bila terlalu panjang (>3800 char), seluruh perintah/detail utuh dipecah ke `preamble_html_chunks`
///   untuk dikirim tepat sebelum kartu tombol — tidak ada satu karakter pun yang dipangkas diam-diam.
pub fn format_approval_card_html(
    category: &str,
    action_title: &str,
    detail: &str,
    timeout_secs: u64,
) -> (Vec<String>, String, String) {
    let mins = (timeout_secs / 60).max(1);
    let action_clean = action_title.trim();
    let detail_clean = detail.trim();

    let mut full_html = format!(
        "<b>⚠️ Perlu persetujuan</b>\n\
         <b>Kategori:</b> <code>{}</code>\n\
         <b>Perintah:</b>\n<pre><code>{}</code></pre>",
        escape_telegram_html(category),
        escape_telegram_html(action_clean)
    );
    let mut full_plain = format!(
        "⚠️ Perlu persetujuan\nKategori: {category}\nPerintah:\n{action_clean}"
    );
    if !detail_clean.is_empty() {
        full_html.push_str(&format!(
            "\n<b>Alasan:</b>\n<pre><code>{}</code></pre>",
            escape_telegram_html(detail_clean)
        ));
        full_plain.push_str(&format!("\n\nAlasan:\n{detail_clean}"));
    }
    full_html.push_str(&format!("\n<i>Batas waktu: {mins} menit.</i>"));
    full_plain.push_str(&format!("\n\nBatas waktu: {mins} menit."));

    if full_html.chars().count() <= 3800 {
        return (Vec::new(), full_html, full_plain);
    }

    // Perintah/detail melebihi batas 1 pesan Telegram -> kirim UTUH sebagai pesan lanjutan sebelumnya!
    let mut preamble = Vec::new();
    let mut combined_raw = format!("Perintah:\n{action_clean}");
    if !detail_clean.is_empty() {
        combined_raw.push_str(&format!("\n\nAlasan:\n{detail_clean}"));
    }
    let chunks = split_telegram_chunks(&combined_raw, 3200);
    let total_parts = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let header = format!(
            "<b>Detail persetujuan ({}/{total_parts})</b>\n<pre><code>{}</code></pre>",
            idx + 1,
            escape_telegram_html(&chunk)
        );
        preamble.push(header);
    }
    let short_card_html = format!(
        "<b>⚠️ Perlu persetujuan</b>\n\
         <b>Kategori:</b> <code>{}</code>\n\
         Perintah dan detail lengkap ditampilkan utuh pada {total_parts} pesan tepat di atas.\n\
         <i>Batas waktu: {mins} menit.</i>",
        escape_telegram_html(category)
    );
    let short_card_plain = format!(
        "⚠️ Perlu persetujuan\nKategori: {category}\nPerintah & detail utuh ada pada {total_parts} pesan di atas.\nBatas waktu: {mins} menit."
    );
    (preamble, short_card_html, short_card_plain)
}

/// Klien ringan Telegram Bot API berbasis `reqwest`.
/// Memisahkan klien cepat (`api_http`) dari klien long-polling (`poll_http`) + HTTP/1.1 & TCP keepalive
/// agar soket zombie di Windows/ISP/NAT tidak pernah menahan pengiriman pesan hingga 60 detik.
#[derive(Clone)]
pub struct TelegramClient {
    api_http: reqwest::Client,
    poll_http: reqwest::Client,
    token: String,
    base_url: String,
}

impl TelegramClient {
    pub fn new(bot_token: &str) -> Result<Self> {
        let tok = bot_token.trim();
        Self::with_base_url(tok, &format!("https://api.telegram.org/bot{tok}"))
    }

    /// Konstruktor dengan `base_url` khusus (dipakai pengujian mock HTTP lokal v0.5.10).
    pub fn with_base_url(bot_token: &str, base_url: &str) -> Result<Self> {
        let api_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .tcp_keepalive(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(25))
            .http1_only()
            .build()
            .context("gagal membuat HTTP API client Telegram")?;
        let poll_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(20))
            .http1_only()
            .build()
            .context("gagal membuat HTTP polling client Telegram")?;
        Ok(Self {
            api_http,
            poll_http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: bot_token.trim().to_string(),
        })
    }

    async fn post_json_with<T: for<'de> Deserialize<'de>>(
        &self,
        client: &reqwest::Client,
        method: &str,
        body: Value,
    ) -> Result<T> {
        let url = format!("{}/{method}", self.base_url);
        let resp = client
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

    async fn post_json<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        self.post_json_with(&self.api_http, method, body).await
    }

    /// Hapus webhook lama yang mungkin menempel pada token bot agar long-polling `getUpdates`
    /// selalu langsung terhubung tanpa gagal `409 Conflict`.
    pub async fn delete_webhook(&self, drop_pending_updates: bool) -> Result<()> {
        let _: Value = self
            .post_json(
                "deleteWebhook",
                json!({ "drop_pending_updates": drop_pending_updates }),
            )
            .await?;
        Ok(())
    }

    /// Ambil status webhook saat ini untuk diagnostik `/telegram test`.
    pub async fn get_webhook_info(&self) -> Result<Value> {
        self.post_json("getWebhookInfo", json!({})).await
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
        let resp = self.api_http.get(&url).send().await.map_err(|e| {
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
        self.post_json_with(
            &self.poll_http,
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

    /// Kirim pesan berformat Telegram HTML dengan fallback anti-gagal-parse ke TEKS POLOS BERSIH
    /// (markdown mentah dibuang, struktur baris dipertahankan, dan kejadian fallback dicatat di log).
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
            Err(e) => {
                eprintln!(
                    "[telegram][render-fallback] HTML ditolak Telegram ({e}) — mengirim ulang sebagai teks polos bersih."
                );
                let clean_plain = if plain_fallback.trim().is_empty() {
                    strip_telegram_html_tags(html)
                } else {
                    markdown_to_clean_plain_text(plain_fallback)
                };
                self.send_message(chat_id, &clean_plain).await
            }
        }
    }

    /// Kirim teks Markdown panjang secara rapi sebagai Telegram HTML (terpecah aman per chunk + fallback teks polos bersih).
    pub async fn send_long_markdown(&self, chat_id: i64, markdown: &str) -> Result<()> {
        // Sisakan margin karena tag HTML (`<b>`, `<code>`, `&amp;`) memperpanjang teks sedikit.
        for chunk in split_markdown_for_telegram(markdown, 3200) {
            let html = markdown_to_telegram_html(&chunk);
            let clean_plain = markdown_to_clean_plain_text(&chunk);
            if html.chars().count() <= 4050 {
                self.send_html_message(chat_id, &html, &clean_plain).await?;
            } else {
                for sub in split_telegram_chunks(&chunk, TELEGRAM_MAX_CHARS) {
                    let sub_html = markdown_to_telegram_html(&sub);
                    let sub_plain = markdown_to_clean_plain_text(&sub);
                    if sub_html.chars().count() <= 4050 {
                        self.send_html_message(chat_id, &sub_html, &sub_plain).await?;
                    } else {
                        self.send_message(chat_id, &sub_plain).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Kirim teks Markdown ke Telegram, sekaligus mengekstrak blok kode panjang (`>= 25` baris
    /// atau `>= 1.500` char) agar dikirim sebagai lampiran dokumen (`sendDocument`) daripada membanjiri chat.
    pub async fn send_long_markdown_with_code_files(
        &self,
        chat_id: i64,
        markdown: &str,
    ) -> Result<()> {
        let (body_md, blocks) = extract_long_code_blocks_for_telegram(markdown);
        self.send_long_markdown(chat_id, &body_md).await?;
        for blk in blocks {
            let cap = format!("{} ({} baris)", blk.filename, blk.lines);
            if let Err(e) = self
                .send_document_bytes(
                    chat_id,
                    &blk.filename,
                    blk.content.as_bytes().to_vec(),
                    Some(&cap),
                )
                .await
            {
                eprintln!(
                    "[telegram][code-file] gagal mengunggah {} ({e}) — jatuh ke pengiriman teks kode terpotong.",
                    blk.filename
                );
                let fallback_md = format!("```{}\n{}\n```", blk.lang, blk.content);
                let _ = self.send_long_markdown(chat_id, &fallback_md).await;
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

    pub async fn send_html_message_with_keyboard(
        &self,
        chat_id: i64,
        html: &str,
        plain_fallback: &str,
        rows: Vec<Vec<(String, String)>>,
    ) -> Result<i64> {
        let inline_keyboard: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(label, cb)| json!({ "text": label, "callback_data": cb }))
                    .collect()
            })
            .collect();
        let res: Result<TgMessage> = self
            .post_json(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                    "reply_markup": { "inline_keyboard": inline_keyboard },
                }),
            )
            .await;
        match res {
            Ok(msg) => Ok(msg.message_id),
            Err(e) => {
                eprintln!(
                    "[telegram][render-fallback] HTML keyboard ditolak Telegram ({e}) — mengirim ulang sebagai teks polos bersih."
                );
                let clean_plain = if plain_fallback.trim().is_empty() {
                    strip_telegram_html_tags(html)
                } else {
                    markdown_to_clean_plain_text(plain_fallback)
                };
                self.send_message_with_keyboard(chat_id, &clean_plain, rows)
                    .await
            }
        }
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

    /// Edit pesan berformat Telegram HTML in-place (mengabaikan "message is not modified",
    /// dan fallback otomatis ke teks polos bersih bila tag HTML ditolak + dicatat di log).
    pub async fn edit_html_message(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
        plain_fallback: &str,
    ) -> Result<()> {
        let res: Result<Value> = self
            .post_json(
                "editMessageText",
                json!({
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                }),
            )
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("message is not modified") => Ok(()),
            Err(e) => {
                eprintln!(
                    "[telegram][render-fallback] editMessageText HTML ditolak ({e}) — fallback teks polos bersih."
                );
                let clean_plain = if plain_fallback.trim().is_empty() {
                    strip_telegram_html_tags(html)
                } else {
                    markdown_to_clean_plain_text(plain_fallback)
                };
                match self
                    .edit_message_text(chat_id, message_id, &clean_plain)
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e2) if e2.to_string().contains("message is not modified") => Ok(()),
                    Err(e2) => Err(e2),
                }
            }
        }
    }

    /// Hapus pesan sementara dan kembalikan `true` bila Telegram mengonfirmasi penghapusan berhasil.
    pub async fn delete_message_checked(&self, chat_id: i64, message_id: i64) -> bool {
        self.post_json::<Value>(
            "deleteMessage",
            json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }),
        )
        .await
        .is_ok()
    }

    /// Hapus pesan sementara (mis. gelembung status progres) secara best-effort.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) {
        let _ = self.delete_message_checked(chat_id, message_id).await;
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

/// Pengawal siklus hidup task indikator mengetik (`sendChatAction(typing)`) per giliran Telegram
/// (Perbaikan 1 v0.5.16):
/// - Memakai `tokio::sync::watch::channel(false)` agar sinyal berhenti SELALU diingat dan tidak
///   pernah hilang meskipun tiba saat panggilan `send_typing().await` sedang berlangsung.
/// - `stop_and_join` menunggu masa tenggang wajar lalu memanggil `JoinHandle::abort()` + await
///   bila task belum berhenti — tidak pernah me-detach task menjadi yatim.
/// - `Drop` ikut mengirim sinyal berhenti dan meng-abort handle yang masih tersisa agar aman
///   pada jalur keluar mana pun (selesai, `/cancel`, timeout, galat, maupun panik).
pub struct TypingLoopGuard {
    stop_tx: tokio::sync::watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl TypingLoopGuard {
    /// Jalankan loop pengetik generik (dapat diuji langsung dengan pengirim tiruan lambat).
    pub fn spawn<F, Fut>(interval: Duration, mut send_fn: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let stopped_during_send = tokio::select! {
                    biased;
                    _ = async {
                        while !*stop_rx.borrow() {
                            if stop_rx.changed().await.is_err() {
                                break;
                            }
                        }
                    } => true,
                    _ = send_fn() => false,
                };
                if stopped_during_send || *stop_rx.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = async {
                        while !*stop_rx.borrow() {
                            if stop_rx.changed().await.is_err() {
                                break;
                            }
                        }
                    } => break,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });
        Self {
            stop_tx,
            handle: Some(handle),
        }
    }

    /// Jalankan loop `sendChatAction(typing)` untuk `TelegramClient` pada `chat_id` tertentu.
    pub fn spawn_for_client(tg: TelegramClient, chat_id: i64, interval: Duration) -> Self {
        Self::spawn(interval, move || {
            let tg_c = tg.clone();
            async move {
                tg_c.send_typing(chat_id).await;
            }
        })
    }

    /// Kirim sinyal berhenti yang teringat, tunggu hingga `grace`, lalu abort eksplisit bila belum selesai.
    pub async fn stop_and_join(mut self, grace: Duration) {
        let _ = self.stop_tx.send(true);
        if let Some(mut handle) = self.handle.take() {
            if tokio::time::timeout(grace, &mut handle).await.is_err() {
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    /// Apakah task latar sudah benar-benar selesai?
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true)
    }
}

impl Drop for TypingLoopGuard {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl TelegramClient {

    /// Unggah berkas lampiran (`sendDocument`, maks 50 MB Bot API) sebagai pesan tersendiri (Perbaikan 4 v0.5.7).
    pub async fn send_document(
        &self,
        chat_id: i64,
        file_path: &std::path::Path,
        caption: Option<&str>,
    ) -> Result<TgMessage> {
        self.send_multipart_media("sendDocument", "document", chat_id, file_path, caption)
            .await
    }

    /// Unggah byte memori langsung sebagai dokumen lampiran Telegram (`sendDocument`) —
    /// dipakai oleh pengiriman otomatis blok kode panjang agar tidak membanjiri chat.
    pub async fn send_document_bytes(
        &self,
        chat_id: i64,
        filename: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<TgMessage> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document".to_string(), part);
        if let Some(cap) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            let short_cap: String = cap.chars().take(1000).collect();
            form = form.text("caption", short_cap);
        }
        let url = format!("{}/sendDocument", self.base_url);
        let upload_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .http1_only()
            .build()
            .context("gagal membuat HTTP upload client Telegram")?;
        let resp = upload_http.post(&url).multipart(form).send().await.map_err(|e| {
            let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
            anyhow!("gagal mengirim sendDocument ke Telegram: {clean}")
        })?;
        let status = resp.status();
        let parsed: ApiResponse<TgMessage> = resp.json().await.map_err(|e| {
            let clean = redact_tg_token(&e.to_string(), &self.token);
            anyhow!("respons Telegram sendDocument tidak valid (HTTP {status}): {clean}")
        })?;
        if !parsed.ok {
            let desc = parsed
                .description
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(anyhow!(
                "Telegram API sendDocument menolak permintaan: {}",
                redact_tg_token(&desc, &self.token)
            ));
        }
        parsed
            .result
            .ok_or_else(|| anyhow!("respons Telegram sendDocument tidak memuat result"))
    }

    /// Unggah foto (`sendPhoto`, maks 10 MB Bot API) sebagai pesan tersendiri (Perbaikan 4 v0.5.7).
    pub async fn send_photo(
        &self,
        chat_id: i64,
        file_path: &std::path::Path,
        caption: Option<&str>,
    ) -> Result<TgMessage> {
        self.send_multipart_media("sendPhoto", "photo", chat_id, file_path, caption)
            .await
    }

    async fn send_multipart_media(
        &self,
        method: &str,
        field_name: &str,
        chat_id: i64,
        file_path: &std::path::Path,
        caption: Option<&str>,
    ) -> Result<TgMessage> {
        let bytes = tokio::fs::read(file_path)
            .await
            .with_context(|| format!("gagal membaca berkas {}", file_path.display()))?;
        let fname = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("berkas")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes).file_name(fname);
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field_name.to_string(), part);
        if let Some(cap) = caption.map(str::trim).filter(|s| !s.is_empty()) {
            let short_cap: String = cap.chars().take(1000).collect();
            form = form.text("caption", short_cap);
        }
        let url = format!("{}/{method}", self.base_url);
        let upload_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .http1_only()
            .build()
            .context("gagal membuat HTTP upload client Telegram")?;
        let resp = upload_http.post(&url).multipart(form).send().await.map_err(|e| {
            let clean = redact_tg_token(&e.without_url().to_string(), &self.token);
            anyhow!("gagal mengirim {method} ke Telegram: {clean}")
        })?;
        let status = resp.status();
        let parsed: ApiResponse<TgMessage> = resp.json().await.map_err(|e| {
            let clean = redact_tg_token(&e.to_string(), &self.token);
            anyhow!("respons Telegram {method} tidak valid (HTTP {status}): {clean}")
        })?;
        if !parsed.ok {
            let desc = parsed
                .description
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(anyhow!(
                "Telegram API {method} menolak permintaan: {}",
                redact_tg_token(&desc, &self.token)
            ));
        }
        parsed
            .result
            .ok_or_else(|| anyhow!("respons Telegram {method} tidak memuat result"))
    }
}

// ---------- Jembatan persetujuan (ApprovalRequest <-> tombol inline Telegram) ----------

/// Status pengiriman kartu approval ke Telegram (Bagian 4B v0.5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDeliveryStatus {
    Sent { message_id: i64 },
    Failed { reason: String },
}

/// Catatan terstruktur status pengiriman kartu persetujuan.
#[derive(Debug, Clone)]
pub struct ApprovalDeliveryRecord {
    pub approval_id: u64,
    pub chat_id: i64,
    pub category: String,
    pub status: ApprovalDeliveryStatus,
    pub retried: bool,
}

pub struct PendingTgApproval {
    pub chat_id: i64,
    pub message_id: i64,
    pub base_text: String,
    pub category: String,
    pub policy: Arc<Mutex<ApprovalPolicy>>,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Penyimpan permintaan persetujuan aktif dari tool yang menunggu tap tombol di Telegram.
pub struct TgApprovalHub {
    next_id: AtomicU64,
    /// Batas waktu tunggu dalam detik (default 600 = 10 menit; 0 = tanpa batas).
    timeout_secs: AtomicU64,
    pending: Mutex<HashMap<u64, PendingTgApproval>>,
    /// ID permintaan yang telah kedaluwarsa agar klik tombol lama dijawab "sudah kedaluwarsa".
    expired_ids: Mutex<std::collections::HashSet<u64>>,
    /// Log terstruktur status terkirim/gagal kartu persetujuan.
    delivery_log: Mutex<Vec<ApprovalDeliveryRecord>>,
}

impl Default for TgApprovalHub {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            timeout_secs: AtomicU64::new(600), // default 10 menit (Bagian 4B)
            pending: Mutex::new(HashMap::new()),
            expired_ids: Mutex::new(std::collections::HashSet::new()),
            delivery_log: Mutex::new(Vec::new()),
        }
    }
}

impl TgApprovalHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Atur batas waktu approval dalam menit (`0` = tanpa batas / perilaku lama).
    pub fn set_timeout_mins(&self, mins: u64) {
        self.timeout_secs.store(mins.saturating_mul(60), Ordering::Relaxed);
    }

    /// Atur batas waktu approval dalam detik secara langsung (berguna untuk tes unit timeout pendek).
    pub fn set_timeout_secs(&self, secs: u64) {
        self.timeout_secs.store(secs, Ordering::Relaxed);
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs.load(Ordering::Relaxed)
    }

    /// Ambil salinan catatan pengiriman kartu approval (terkirim/gagal).
    pub fn delivery_records(&self) -> Vec<ApprovalDeliveryRecord> {
        self.delivery_log
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    fn push_delivery_record(&self, rec: ApprovalDeliveryRecord) {
        match &rec.status {
            ApprovalDeliveryStatus::Sent { message_id } => {
                eprintln!(
                    "[telegram][approval] id={} chat={} cat={} status=terkirim msg_id={} retried={}",
                    rec.approval_id, rec.chat_id, rec.category, message_id, rec.retried
                );
            }
            ApprovalDeliveryStatus::Failed { reason } => {
                eprintln!(
                    "[telegram][approval] id={} chat={} cat={} status=gagal retried={} sebab={}",
                    rec.approval_id, rec.chat_id, rec.category, rec.retried, reason
                );
            }
        }
        if let Ok(mut log) = self.delivery_log.lock() {
            log.push(rec);
            if log.len() > 200 {
                log.remove(0);
            }
        }
    }

    /// Tandai 1 approval kedaluwarsa (dipakai oleh timer maupun tes unit):
    /// menolak aksi secara aman (`false`) dan mencatat ID di `expired_ids`.
    pub fn expire_approval_now(&self, id: u64) -> Option<PendingTgApproval> {
        let item = self.pending.lock().ok().and_then(|mut m| m.remove(&id))?;
        if let Ok(mut exp) = self.expired_ids.lock() {
            exp.insert(id);
        }
        Some(item)
    }

    pub fn is_expired(&self, id: u64) -> bool {
        self.expired_ids
            .lock()
            .map(|s| s.contains(&id))
            .unwrap_or(false)
    }

    /// Apakah masih ada permintaan persetujuan tertunda untuk `chat_id` ini?
    /// Dipakai oleh collector progres agar gelembung progres tetap jeda saat menunggu tombol user.
    pub fn has_pending_for_chat(&self, chat_id: i64) -> bool {
        self.pending
            .lock()
            .map(|map| map.values().any(|v| v.chat_id == chat_id))
            .unwrap_or(false)
    }

    /// Kirim pesan persetujuan dengan 3 tombol inline polos ke Telegram & daftarkan callback-nya.
    /// Perintah/aksi ditampilkan UTUH; gagal kirim dicoba ulang TEPAT 1 kali lalu ditolak + lapor ke user;
    /// kedaluwarsa mengikuti `[telegram] approval_timeout_mins` (default 10 menit, 0 = tanpa batas).
    pub async fn dispatch(
        self: &Arc<Self>,
        tg: &TelegramClient,
        chat_id: i64,
        policy: Arc<Mutex<ApprovalPolicy>>,
        req: ApprovalRequest,
    ) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let category = tool_category_from_title(&req.title);
        let t_secs = self.timeout_secs.load(Ordering::Relaxed);
        let display_timeout = if t_secs == 0 { APPROVAL_TIMEOUT_SECS } else { t_secs };
        let (preamble_chunks, card_html, card_plain) = format_approval_card_html(
            &category,
            &req.title,
            &req.detail,
            display_timeout,
        );
        for chunk_html in preamble_chunks {
            let _ = tg
                .send_html_message(chat_id, &chunk_html, &req.title)
                .await;
        }
        let rows = vec![
            vec![
                ("Setuju".to_string(), format!("ap:{id}:y")),
                ("Tolak".to_string(), format!("ap:{id}:n")),
            ],
            vec![(
                format!("Selalu untuk {category} — sesi ini"),
                format!("ap:{id}:a"),
            )],
        ];
        // Bagian 4B (#1 & #2): catat status terkirim/gagal; bila gagal coba kirim ulang TEPAT 1 kali.
        let mut retried = false;
        let send_res = match tg
            .send_html_message_with_keyboard(chat_id, &card_html, &card_plain, rows.clone())
            .await
        {
            Ok(mid) => Ok(mid),
            Err(_first_err) => {
                retried = true;
                tokio::time::sleep(Duration::from_millis(600)).await;
                tg.send_html_message_with_keyboard(chat_id, &card_html, &card_plain, rows)
                    .await
            }
        };
        match send_res {
            Ok(message_id) => {
                self.push_delivery_record(ApprovalDeliveryRecord {
                    approval_id: id,
                    chat_id,
                    category: category.clone(),
                    status: ApprovalDeliveryStatus::Sent { message_id },
                    retried,
                });
                if let Ok(mut map) = self.pending.lock() {
                    map.insert(
                        id,
                        PendingTgApproval {
                            chat_id,
                            message_id,
                            base_text: card_html,
                            category,
                            policy,
                            reply: req.reply,
                        },
                    );
                }
                if t_secs > 0 {
                    let hub_exp = Arc::clone(self);
                    let tg_exp = tg.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(t_secs)).await;
                        if let Some(item) = hub_exp.expire_approval_now(id) {
                            let _ = item.reply.send(false);
                            let _ = tg_exp
                                .edit_message_text(
                                    item.chat_id,
                                    item.message_id,
                                    "Kedaluwarsa — aksi dibatalkan",
                                )
                                .await;
                        }
                    });
                }
            }
            Err(e) => {
                let reason = e.to_string();
                self.push_delivery_record(ApprovalDeliveryRecord {
                    approval_id: id,
                    chat_id,
                    category: category.clone(),
                    status: ApprovalDeliveryStatus::Failed {
                        reason: reason.clone(),
                    },
                    retried,
                });
                // Gagal mengirim kartu persetujuan sesudah 1x retry -> TOLAK demi keamanan + beri tahu user!
                let _ = req.reply.send(false);
                let _ = tg
                    .send_message(
                        chat_id,
                        &format!(
                            "Persetujuan untuk aksi `{category}` gagal dikirim ke Telegram — aksi dibatalkan demi keamanan."
                        ),
                    )
                    .await;
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

    /// Proses `callback_query` dari Telegram. Mengembalikan `true` jika cocok dengan approval aktif/kedaluwarsa.
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
            let msg = if self.is_expired(id) {
                "sudah kedaluwarsa"
            } else {
                "Permintaan persetujuan ini sudah kedaluwarsa atau sudah diproses."
            };
            let _ = tg.answer_callback_query(&cb.id, Some(msg)).await;
            return true;
        };
        let (approved, toast, badge) = match decision {
            ApprovalDecision::ApproveOnce => (true, "Disetujui", "Disetujui."),
            ApprovalDecision::ApproveCategory => {
                if let Ok(mut pol) = item.policy.lock() {
                    pol.allow_category(item.category.clone());
                }
                (
                    true,
                    "Selalu diizinkan sesi ini",
                    "Selalu diizinkan untuk kategori ini (sesi ini).",
                )
            }
            ApprovalDecision::Deny => (false, "Ditolak", "Ditolak."),
        };
        let _ = item.reply.send(approved);
        let _ = tg.answer_callback_query(&cb.id, Some(toast)).await;
        let updated_html = format!("{}\n\n<b>Status:</b> {badge}", item.base_text);
        let _ = tg
            .edit_html_message(item.chat_id, item.message_id, &updated_html, badge)
            .await;
        true
    }
}

// ---------- Jembatan klarifikasi `ask_user` Telegram (Bagian 2 v0.5.6 — TERPISAH dari approval!) ----------

struct PendingTgAsk {
    chat_id: i64,
    message_id: i64,
    question: String,
    choices: Vec<String>,
    reply: tokio::sync::oneshot::Sender<crate::tools::ask::AskUserAnswer>,
}

/// Hub pertanyaan klarifikasi `ask_user` di Telegram (jalur kode & visual 100% terpisah dari `TgApprovalHub`).
#[derive(Default)]
pub struct TgAskHub {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingTgAsk>>,
}

impl TgAskHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn has_pending_for_chat(&self, chat_id: i64) -> bool {
        self.pending
            .lock()
            .map(|m| m.values().any(|p| p.chat_id == chat_id))
            .unwrap_or(false)
    }

    /// Format kartu pertanyaan klarifikasi (gaya v0.5.0: polos, 0 emoji dekoratif, jelas bukan approval).
    /// Teks pertanyaan & konteks melewati konverter Markdown -> Telegram HTML yang sama.
    pub fn format_ask_card_html(
        question: &str,
        choices: &[String],
        context: Option<&str>,
    ) -> (String, String) {
        let mut html = String::from("<b>Pertanyaan Klarifikasi</b>\n");
        let mut plain = String::from("Pertanyaan Klarifikasi\n");
        if let Some(ctx) = context.map(str::trim).filter(|s| !s.is_empty()) {
            html.push_str(&format!("<i>{}</i>\n\n", markdown_to_telegram_html(ctx)));
            plain.push_str(&format!("{}\n\n", markdown_to_clean_plain_text(ctx)));
        }
        html.push_str(&markdown_to_telegram_html(question));
        plain.push_str(&markdown_to_clean_plain_text(question));
        if choices.is_empty() {
            html.push_str("\n\n<i>Balas pesan ini dengan jawabanmu.</i>");
            plain.push_str("\n\nBalas pesan ini dengan jawabanmu.");
        } else {
            html.push_str("\n\n<i>Pilih tombol di bawah atau balas langsung dengan teks bebas.</i>");
            plain.push_str("\n\nPilih tombol di bawah atau balas langsung dengan teks bebas.");
        }
        (html, plain)
    }

    /// Kirim pertanyaan `ask_user` sebagai pesan Telegram baru (tombol pilihan maks 4 + dukung teks bebas).
    pub async fn dispatch(
        self: &Arc<Self>,
        tg: &TelegramClient,
        chat_id: i64,
        req: crate::tools::ask::AskUserRequest,
    ) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (card_html, card_plain) =
            Self::format_ask_card_html(&req.question, &req.choices, req.context.as_deref());
        let rows: Vec<Vec<(String, String)>> = req
            .choices
            .iter()
            .take(4)
            .enumerate()
            .map(|(idx, label)| {
                let short_lbl: String = label.chars().take(48).collect();
                vec![(short_lbl, format!("ask:{id}:{idx}"))]
            })
            .collect();

        let send_res = if rows.is_empty() {
            tg.send_html_message(chat_id, &card_html, &card_plain).await
        } else {
            tg.send_html_message_with_keyboard(chat_id, &card_html, &card_plain, rows)
                .await
        };

        match send_res {
            Ok(message_id) => {
                if let Ok(mut map) = self.pending.lock() {
                    map.insert(
                        id,
                        PendingTgAsk {
                            chat_id,
                            message_id,
                            question: req.question,
                            choices: req.choices,
                            reply: req.reply,
                        },
                    );
                }
                let timeout = req.timeout_secs.max(1);
                let hub_exp = Arc::clone(self);
                let tg_exp = tg.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(timeout)).await;
                    let expired = hub_exp.pending.lock().ok().and_then(|mut m| m.remove(&id));
                    if let Some(item) = expired {
                        let _ = item.reply.send(crate::tools::ask::AskUserAnswer {
                            response: String::new(),
                            timed_out: true,
                            non_interactive: false,
                        });
                        let _ = tg_exp
                            .edit_message_text(
                                item.chat_id,
                                item.message_id,
                                &format!(
                                    "Kedaluwarsa — lanjut dengan asumsi agen (pertanyaan: {})",
                                    item.question.chars().take(120).collect::<String>()
                                ),
                            )
                            .await;
                    }
                });
            }
            Err(_) => {
                let _ = req.reply.send(crate::tools::ask::AskUserAnswer {
                    response: String::new(),
                    timed_out: false,
                    non_interactive: true,
                });
            }
        }
    }

    /// Tangani jawaban tombol inline `ask:<id>:<idx>`.
    pub async fn handle_callback(&self, tg: &TelegramClient, cb: &TgCallbackQuery) -> bool {
        let Some(data) = cb.data.as_deref() else {
            return false;
        };
        let Some((id, idx)) = parse_callback_ask(data) else {
            return false;
        };
        let cb_chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
        let entry = self.pending.lock().ok().and_then(|mut m| {
            if m.get(&id).map(|p| p.chat_id == cb_chat_id).unwrap_or(false) {
                m.remove(&id)
            } else {
                None
            }
        });
        let Some(item) = entry else {
            let _ = tg
                .answer_callback_query(&cb.id, Some("Pertanyaan ini sudah dijawab atau kedaluwarsa."))
                .await;
            return true;
        };
        let chosen = item
            .choices
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("Pilihan #{}", idx + 1));
        let _ = item.reply.send(crate::tools::ask::AskUserAnswer {
            response: chosen.clone(),
            timed_out: false,
            non_interactive: false,
        });
        let _ = tg.answer_callback_query(&cb.id, Some("Jawaban diterima")).await;
        let summary = format!("Kamu memilih: {chosen}");
        let _ = tg
            .edit_message_text(item.chat_id, item.message_id, &summary)
            .await;
        true
    }

    /// Jika ada pertanyaan `ask_user` aktif untuk `chat_id`, konsumsi pesan teks biasa user sebagai jawaban bebas!
    pub async fn try_consume_text_reply(&self, tg: &TelegramClient, chat_id: i64, text: &str) -> bool {
        let id_opt = self.pending.lock().ok().and_then(|m| {
            m.iter()
                .find_map(|(k, v)| (v.chat_id == chat_id).then_some(*k))
        });
        let Some(id) = id_opt else {
            return false;
        };
        let Some(item) = self.pending.lock().ok().and_then(|mut m| m.remove(&id)) else {
            return false;
        };
        let clean = text.trim().to_string();
        let _ = item.reply.send(crate::tools::ask::AskUserAnswer {
            response: clean.clone(),
            timed_out: false,
            non_interactive: false,
        });
        let summary = format!("Kamu memilih: {clean}");
        let _ = tg
            .edit_message_text(item.chat_id, item.message_id, &summary)
            .await;
        true
    }

    /// Batalkan semua pertanyaan `ask_user` tertunda untuk `chat_id` (mis. saat `/cancel`).
    pub fn cancel_chat_asks(&self, chat_id: i64) -> usize {
        let removed: Vec<PendingTgAsk> = {
            let Ok(mut map) = self.pending.lock() else {
                return 0;
            };
            let ids: Vec<u64> = map
                .iter()
                .filter_map(|(k, v)| (v.chat_id == chat_id).then_some(*k))
                .collect();
            ids.into_iter().filter_map(|k| map.remove(&k)).collect()
        };
        let n = removed.len();
        for item in removed {
            let _ = item.reply.send(crate::tools::ask::AskUserAnswer {
                response: String::new(),
                timed_out: true,
                non_interactive: false,
            });
        }
        n
    }
}
