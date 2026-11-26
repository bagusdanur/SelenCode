pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod router;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

/// Tenggat waktu panggilan HTTP/streaming ke provider (Perbaikan 1 v0.5.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTimeouts {
    /// Tenggat sambung TCP/TLS (default: 20s).
    pub connect: Duration,
    /// Tenggat menunggu respons header awal (default: 60s).
    pub response: Duration,
    /// Tenggat ketidakaktifan streaming tanpa potongan data baru (default: 90s).
    pub stream_idle: Duration,
    /// Tenggat keras total satu panggilan ke provider (default: 300s).
    pub total: Duration,
}

impl Default for ProviderTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(20),
            response: Duration::from_secs(60),
            stream_idle: Duration::from_secs(90),
            total: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text(String),
    /// Lampiran gambar multimodal (`@screenshot.png` / foto Telegram) untuk model vision.
    Image {
        media_type: String,
        data_base64: String,
    },
    ToolUse {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

/// Deteksi tipe MIME gambar dari ekstensi file.
pub fn detect_image_mime(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Encoder Base64 standar RFC 4648 murni Rust (tanpa dependency eksternal ekstra).
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn text(role: Role, s: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Block::Text(s.into())],
        }
    }
    pub fn plain_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema objek argumen.
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Pemakaian token 1 panggilan provider (bila provider melaporkannya).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantReply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// Usage token panggilan ini (Fase 3); None bila provider tidak melapor.
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Token(String),
}

/// Kontrak provider (Blueprint Bagian 5). Semua provider tunduk ke bentuk ini;
/// loop agent tidak pernah tahu provider mana yang dipakai.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply>;

    fn model_name(&self) -> &str {
        ""
    }
}

/// Provider placeholder saat user pertama kali menjalankan `selen` tanpa config /
/// tanpa API key agar TUI tetap terbuka bersih dan langsung membuka Wizard `/setup`.
pub struct UnconfiguredProvider {
    pub reason: String,
}

#[async_trait]
impl Provider for UnconfiguredProvider {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        anyhow::bail!(
            "Provider AI belum dikonfigurasi ({}). Jalankan `/setup` untuk mengisi API provider & key.",
            self.reason
        )
    }

    fn model_name(&self) -> &str {
        "(belum diatur)"
    }
}
