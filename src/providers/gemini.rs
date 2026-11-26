use super::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Adapter Gemini native (Fase 3).
/// POST {base}/v1beta/models/{model}:streamGenerateContent?alt=sse
/// dengan header x-goog-api-key.
///
/// Catatan format Gemini:
/// - Role hanya user/model; system prompt -> `systemInstruction`.
/// - functionResponse WAJIB membawa NAMA fungsi, bukan id. Karena itu
///   ToolCall dari adapter ini memakai id = nama fungsi (Gemini memang tidak
///   memberi id untuk function call), sehingga ToolResult.id bisa dipetakan
///   balik ke nama fungsi saat membangun request berikutnya.
pub struct GeminiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
    timeouts: ProviderTimeouts,
    client: reqwest::Client,
}

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

impl GeminiProvider {
    pub fn new(base_url: String, api_key: String, model: String, max_output_tokens: u32) -> Self {
        Self::with_timeouts(
            base_url,
            api_key,
            model,
            max_output_tokens,
            ProviderTimeouts::default(),
        )
    }

    pub fn with_timeouts(
        base_url: String,
        api_key: String,
        model: String,
        max_output_tokens: u32,
        timeouts: ProviderTimeouts,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            max_output_tokens,
            timeouts,
            client,
        }
    }
}

/// Bangun body request Gemini dari pesan internal. FUNGSI MURNI (bisa dites).
pub fn build_request(msgs: &[Message], tools: &[ToolSpec]) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    for m in msgs {
        match m.role {
            Role::System => {
                let t = m.plain_text();
                if !t.is_empty() {
                    system_parts.push(t);
                }
            }
            Role::User | Role::Tool => {
                let mut parts: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        Block::Text(t) => {
                            if !t.is_empty() {
                                parts.push(json!({"text": t}));
                            }
                        }
                        Block::Image {
                            media_type,
                            data_base64,
                        } => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": media_type,
                                    "data": data_base64
                                }
                            }));
                        }
                        Block::ToolResult {
                            id,
                            content,
                            is_error,
                        } => {
                            // id di sini = nama fungsi (lihat catatan di atas).
                            let mut resp = json!({"result": content});
                            if *is_error {
                                resp = json!({"result": content, "error": true});
                            }
                            parts.push(json!({"functionResponse": {"name": id, "response": resp}}));
                        }
                        Block::ToolUse { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Role::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        Block::Text(t) => {
                            if !t.is_empty() {
                                parts.push(json!({"text": t}));
                            }
                        }
                        Block::Image {
                            media_type,
                            data_base64,
                        } => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": media_type,
                                    "data": data_base64
                                }
                            }));
                        }
                        Block::ToolUse { name, args, .. } => {
                            parts.push(json!({"functionCall": {"name": name, "args": args}}));
                        }
                        Block::ToolResult { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
        }
    }

    let mut body = json!({"contents": contents});
    let system = system_parts.join("\n\n");
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    if !tools.is_empty() {
        let decls: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "parameters": t.parameters})
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    body
}

/// Parser stream SSE Gemini (per chunk JSON `data:`). Terpisah dari HTTP
/// agar bisa dites. Mengembalikan potongan teks untuk diteruskan ke user.
#[derive(Default)]
pub struct GeminiStreamParser {
    text: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u64,
    output_tokens: u64,
    saw_usage: bool,
    finish_reason: Option<String>,
}

impl GeminiStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_chunk(&mut self, v: &Value) -> Option<String> {
        let mut token = String::new();
        if let Some(u) = v.get("usageMetadata") {
            if u.is_object() {
                self.input_tokens = u["promptTokenCount"].as_u64().unwrap_or(0);
                self.output_tokens = u["candidatesTokenCount"].as_u64().unwrap_or(0);
                self.saw_usage = true;
            }
        }
        let cand = &v["candidates"][0];
        if let Some(fr) = cand["finishReason"].as_str() {
            self.finish_reason = Some(fr.to_string());
        }
        if let Some(parts) = cand["content"]["parts"].as_array() {
            for p in parts {
                if let Some(t) = p["text"].as_str() {
                    if !t.is_empty() {
                        self.text.push_str(t);
                        token.push_str(t);
                    }
                }
                if let Some(fc) = p.get("functionCall") {
                    let name = fc["name"].as_str().unwrap_or_default().to_string();
                    if !name.is_empty() {
                        let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        // id = nama fungsi (Gemini tidak punya id call).
                        self.tool_calls.push(ToolCall {
                            id: name.clone(),
                            name,
                            args,
                        });
                    }
                }
            }
        }
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    pub fn finish(self) -> AssistantReply {
        AssistantReply {
            text: self.text,
            tool_calls: self.tool_calls,
            finish_reason: self.finish_reason,
            usage: if self.saw_usage {
                Some(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                })
            } else {
                None
            },
        }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let mut body = build_request(msgs, tools);
        body["generationConfig"] = json!({"maxOutputTokens": self.max_output_tokens});
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, self.model
        );
        let req_deadline = Instant::now() + self.timeouts.total;
        let resp_wait = self.timeouts.response.min(self.timeouts.total);
        let resp = tokio::time::timeout(
            resp_wait,
            self.client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("content-type", "application/json")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "timeout {} detik menunggu respons awal Gemini di {url} — provider tidak merespons",
                resp_wait.as_secs()
            )
        })?
        .with_context(|| format!("gagal menghubungi Gemini di {url}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let txt = tokio::time::timeout(Duration::from_secs(15), resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            return Err(anyhow!(
                "Gemini membalas {st}: {}",
                txt.chars().take(500).collect::<String>()
            ));
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut parser = GeminiStreamParser::new();
        loop {
            let rem_total = req_deadline.saturating_duration_since(Instant::now());
            if rem_total.is_zero() {
                return Err(anyhow!(
                    "timeout total panggilan Gemini ({} detik) — panggilan dibatalkan",
                    self.timeouts.total.as_secs()
                ));
            }
            let wait_for = self.timeouts.stream_idle.min(rem_total);
            let hit_total = rem_total <= self.timeouts.stream_idle;
            let next_chunk = tokio::time::timeout(wait_for, stream.next())
                .await
                .map_err(|_| {
                    if hit_total {
                        anyhow!(
                            "timeout total panggilan Gemini ({} detik) — panggilan dibatalkan",
                            self.timeouts.total.as_secs()
                        )
                    } else {
                        anyhow!(
                            "timeout ketidakaktifan stream Gemini (tidak ada data selama {} detik) — panggilan dibatalkan",
                            self.timeouts.stream_idle.as_secs()
                        )
                    }
                })?;
            let Some(chunk) = next_chunk else {
                break;
            };
            let chunk = chunk.context("stream Gemini terputus")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(tok) = parser.handle_chunk(&v) {
                    let _ = tx.send(StreamEvent::Token(tok)).await;
                }
            }
        }
        let tail = buf.trim();
        if let Some(data) = tail.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(tok) = parser.handle_chunk(&v) {
                        let _ = tx.send(StreamEvent::Token(tok)).await;
                    }
                }
            }
        }
        Ok(parser.finish())
    }
}
