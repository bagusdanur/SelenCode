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
pub struct TgMessage {
    pub message_id: i64,
    pub chat: TgChat,
    #[serde(default)]
    pub from: Option<TgUser>,
    #[serde(default)]
    pub text: Option<String>,
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
    base_url: String,
}

impl TelegramClient {
    pub fn new(bot_token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .context("gagal membuat HTTP client Telegram")?;
        Ok(Self {
            http,
            base_url: format!("https://api.telegram.org/bot{}", bot_token.trim()),
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
            .with_context(|| format!("gagal menghubungi Telegram API ({method})"))?;
        let status = resp.status();
        let parsed: ApiResponse<T> = resp
            .json()
            .await
            .with_context(|| format!("respons Telegram API ({method}, HTTP {status}) tidak valid"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "Telegram API ({method}) menolak: {}",
                parsed
                    .description
                    .unwrap_or_else(|| format!("HTTP {status}"))
            ));
        }
        parsed
            .result
            .ok_or_else(|| anyhow!("Telegram API ({method}) mengembalikan result kosong"))
    }

    pub async fn get_me(&self) -> Result<TgUser> {
        self.post_json("getMe", json!({})).await
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
    pub async fn dispatch(
        &self,
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
            }
            Err(_) => {
                // Gagal mengirim pesan approval -> tolak demi keamanan.
                let _ = req.reply.send(false);
            }
        }
    }

    /// Proses `callback_query` dari Telegram. Mengembalikan `true` jika cocok dengan approval aktif.
    pub async fn handle_callback(&self, tg: &TelegramClient, cb: &TgCallbackQuery) -> bool {
        let Some(data) = cb.data.as_deref() else {
            return false;
        };
        let Some((id, decision)) = parse_callback_approval(data) else {
            return false;
        };
        let entry = self
            .pending
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&id));
        let Some(item) = entry else {
            let _ = tg
                .answer_callback_query(&cb.id, Some("Permintaan persetujuan ini sudah kedaluwarsa."))
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
