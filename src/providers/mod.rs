pub mod anthropic;
pub mod gemini;
pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

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
