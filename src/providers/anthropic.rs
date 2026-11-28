use super::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Adapter Anthropic Messages API (Fase 3).
/// POST {base_url}/v1/messages, header x-api-key + anthropic-version, SSE stream.
///
/// Perbedaan format vs internal:
/// - System prompt DIPISAH ke field `system`, bukan pesan role system.
/// - ToolResult dikirim sebagai pesan user berisi block `tool_result`.
/// - Args tool_use datang sebagai `input_json_delta` (potongan string JSON).
pub struct AnthropicProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
    timeouts: ProviderTimeouts,
    client: reqwest::Client,
}

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

impl AnthropicProvider {
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
            .redirect(reqwest::redirect::Policy::none())
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

/// Konversi 1 blok internal -> block JSON Anthropic (untuk pesan user/assistant).
fn block_to_anthropic(b: &Block) -> Value {
    match b {
        Block::Text(t) => json!({"type": "text", "text": t}),
        Block::Image {
            media_type,
            data_base64,
        } => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data_base64
            }
        }),
        Block::ToolUse { id, name, args } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": args})
        }
        Block::ToolResult {
            id,
            content,
            is_error,
        } => json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}),
    }
}

/// Bangun body request Anthropic dari pesan internal. FUNGSI MURNI (bisa dites
/// tanpa jaringan). Mengembalikan (system_prompt, body_json_tanpa_system).
/// `system` dimasukkan juga ke body bila tidak kosong.
pub fn build_request(msgs: &[Message], tools: &[ToolSpec], model: &str, max_tokens: u32) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out_msgs: Vec<Value> = Vec::new();

    for m in msgs {
        match m.role {
            Role::System => {
                let t = m.plain_text();
                if !t.is_empty() {
                    system_parts.push(t);
                }
            }
            Role::User | Role::Tool => {
                // ToolResult (role Tool, atau menempel di User) -> pesan user.
                // Anthropic mensyaratkan role user/assistant bergantian ketat:
                // bila pesan sebelumnya sudah role user, gabungkan block-nya.
                let mut blocks: Vec<Value> = m.content.iter().map(block_to_anthropic).collect();
                if !blocks.is_empty() {
                    if let Some(last) = out_msgs.last_mut() {
                        if last["role"] == "user" {
                            if let Some(arr) = last["content"].as_array_mut() {
                                arr.append(&mut blocks);
                                continue;
                            }
                        }
                    }
                    out_msgs.push(json!({"role": "user", "content": blocks}));
                }
            }
            Role::Assistant => {
                let blocks: Vec<Value> = m.content.iter().map(block_to_anthropic).collect();
                if !blocks.is_empty() {
                    out_msgs.push(json!({"role": "assistant", "content": blocks}));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": out_msgs,
        "stream": true,
    });
    let system = system_parts.join("\n\n");
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        let ts: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "input_schema": t.parameters})
            })
            .collect();
        body["tools"] = Value::Array(ts);
    }
    body
}

#[derive(Default)]
struct PartialBlock {
    id: String,
    name: String,
    args_buf: String,
    is_tool: bool,
}

/// Parser stream SSE Anthropic. Terpisah dari HTTP agar bisa dites dari
/// contoh payload. Panggil `handle_event` per objek JSON `data:`; ia
/// mengembalikan potongan teks (token) yang perlu diteruskan ke user.
#[derive(Default)]
pub struct AnthropicStreamParser {
    text: String,
    blocks: BTreeMap<u64, PartialBlock>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    saw_usage: bool,
    finish_reason: Option<String>,
}

impl AnthropicStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_event(&mut self, v: &Value) -> Option<String> {
        match v["type"].as_str()? {
            "message_start" => {
                let u = &v["message"]["usage"];
                if u.is_object() {
                    self.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                    self.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                    self.cache_read_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    self.cache_write_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    self.saw_usage = true;
                }
                None
            }
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let cb = &v["content_block"];
                let e = self.blocks.entry(idx).or_default();
                if cb["type"].as_str() == Some("tool_use") {
                    e.is_tool = true;
                    e.id = cb["id"].as_str().unwrap_or_default().to_string();
                    e.name = cb["name"].as_str().unwrap_or_default().to_string();
                    // input awal kadang datang utuh di block start.
                    if let Some(inp) = cb.get("input") {
                        if !inp.is_null() && *inp != json!({}) {
                            e.args_buf = inp.to_string();
                        }
                    }
                }
                None
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0);
                let delta = &v["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let t = delta["text"].as_str().unwrap_or_default();
                        self.text.push_str(t);
                        if t.is_empty() {
                            None
                        } else {
                            Some(t.to_string())
                        }
                    }
                    Some("input_json_delta") => {
                        let part = delta["partial_json"].as_str().unwrap_or_default();
                        self.blocks.entry(idx).or_default().args_buf.push_str(part);
                        None
                    }
                    _ => None,
                }
            }
            "message_delta" => {
                if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                    self.finish_reason = Some(sr.to_string());
                }
                let u = &v["usage"];
                if u.is_object() {
                    if let Some(o) = u["output_tokens"].as_u64() {
                        self.output_tokens = o;
                        self.saw_usage = true;
                    }
                    if let Some(i) = u["input_tokens"].as_u64() {
                        self.input_tokens = i;
                    }
                    if let Some(cr) = u["cache_read_input_tokens"].as_u64() {
                        self.cache_read_tokens = cr;
                    }
                    if let Some(cw) = u["cache_creation_input_tokens"].as_u64() {
                        self.cache_write_tokens = cw;
                    }
                }
                None
            }
            _ => None,
        }
    }

    pub fn finish(self) -> AssistantReply {
        let tool_calls = self
            .blocks
            .into_values()
            .filter(|b| b.is_tool && !b.name.is_empty())
            .map(|b| ToolCall {
                id: if b.id.is_empty() {
                    format!("toolu_{}", uuid::Uuid::new_v4().simple())
                } else {
                    b.id
                },
                name: b.name,
                args: serde_json::from_str(&b.args_buf).unwrap_or_else(|_| {
                    if b.args_buf.trim().is_empty() {
                        json!({})
                    } else {
                        json!({"_raw": b.args_buf})
                    }
                }),
            })
            .collect();
        AssistantReply {
            text: self.text,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: if self.saw_usage {
                Some(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_read_tokens: self.cache_read_tokens,
                    cache_write_tokens: self.cache_write_tokens,
                })
            } else {
                None
            },
        }
    }
}

/// Terapkan Prompt Caching terstruktur pada body request Anthropic (maks 4 breakpoint sesuai API):
/// - Tandai blok `system` sebagai ephemeral cache breakpoint (1).
/// - Tandai skema tool terakhir bila ada (1).
/// - Tandai sisa kuota (`4 - used`, maks 3) pada pesan non-system terakhir agar tidak pernah HTTP 400.
pub fn apply_prompt_caching(body: &mut Value) {
    let ephem = json!({"type": "ephemeral"});
    let mut used = 0usize;
    if let Some(sys_str) = body.get("system").and_then(|v| v.as_str()).map(|s| s.to_string()) {
        if !sys_str.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": sys_str,
                "cache_control": ephem
            }]);
            used += 1;
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        if let Some(last_tool) = tools.last_mut().and_then(|v| v.as_object_mut()) {
            last_tool.insert("cache_control".into(), json!({"type": "ephemeral"}));
            used += 1;
        }
    }
    let msg_budget = 4usize.saturating_sub(used);
    if msg_budget > 0 {
        if let Some(msgs) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
            let len = msgs.len();
            let start = len.saturating_sub(msg_budget);
            for m in &mut msgs[start..] {
                if let Some(blocks) = m.get_mut("content").and_then(|v| v.as_array_mut()) {
                    if let Some(last_blk) = blocks.last_mut().and_then(|v| v.as_object_mut()) {
                        last_blk.insert("cache_control".into(), json!({"type": "ephemeral"}));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let mut body = build_request(msgs, tools, &self.model, self.max_output_tokens);
        apply_prompt_caching(&mut body);
        let url = format!("{}/v1/messages", self.base_url);
        let est_tokens = crate::agent::context::estimate_tokens(msgs);
        let eff_idle = self.timeouts.effective_stream_idle(est_tokens);
        let eff_total = self.timeouts.effective_total(est_tokens);
        let req_deadline = Instant::now() + eff_total;
        let resp_wait = self.timeouts.response.min(eff_total);
        let resp = tokio::time::timeout(
            resp_wait,
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "timeout {} detik menunggu respons awal Anthropic di {url} — provider tidak merespons",
                resp_wait.as_secs()
            )
        })?
        .with_context(|| format!("gagal menghubungi Anthropic di {url}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let txt = tokio::time::timeout(Duration::from_secs(15), resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let safe = crate::security::redact_secrets(&txt.chars().take(500).collect::<String>());
            return Err(anyhow!("Anthropic membalas {st}: {safe}"));
        }

        const MAX_SSE_LINE_BYTES: usize = 4 * 1024 * 1024;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut parser = AnthropicStreamParser::new();
        'stream: loop {
            let rem_total = req_deadline.saturating_duration_since(Instant::now());
            if rem_total.is_zero() {
                return Err(anyhow!(
                    "timeout total panggilan Anthropic ({} detik) — koneksi dihentikan",
                    eff_total.as_secs()
                ));
            }
            let wait_for = eff_idle.min(rem_total);
            let hit_total = rem_total <= eff_idle;
            let next_chunk = tokio::time::timeout(wait_for, stream.next())
                .await
                .map_err(|_| {
                    if hit_total {
                        anyhow!(
                            "timeout total panggilan Anthropic ({} detik) — koneksi dihentikan",
                            eff_total.as_secs()
                        )
                    } else {
                        anyhow!(
                            "timeout ketidakaktifan stream Anthropic (tidak ada data selama {} detik) — koneksi dihentikan",
                            eff_idle.as_secs()
                        )
                    }
                })?;
            let Some(chunk) = next_chunk else {
                break 'stream;
            };
            let chunk = chunk.context("stream Anthropic terputus")?;
            if buf.len().saturating_add(chunk.len()) > MAX_SSE_LINE_BYTES {
                return Err(anyhow!("stream Anthropic melebihi batas buffer SSE (4 MB)"));
            }
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw_line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw_line);
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
                if v["type"].as_str() == Some("message_stop") {
                    buf.clear();
                    break 'stream;
                }
                if let Some(tok) = parser.handle_event(&v) {
                    let _ = tx.send(StreamEvent::Token(tok)).await;
                }
            }
        }
        // Tangani baris terakhir tanpa '\n' penutup saat EOF.
        if !buf.is_empty() {
            let tail = String::from_utf8_lossy(&buf);
            if let Some(data) = tail.trim().strip_prefix("data:") {
                let data = data.trim();
                if !data.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(tok) = parser.handle_event(&v) {
                            let _ = tx.send(StreamEvent::Token(tok)).await;
                        }
                    }
                }
            }
        }
        Ok(parser.finish())
    }
}

/// Parse respons JSON `GET {base_url}/v1/models` Anthropic menjadi `(model_id, Option<context_limit>)`.
pub fn parse_anthropic_models_response(v: &Value) -> Vec<(String, Option<usize>)> {
    let mut map: BTreeMap<String, Option<usize>> = BTreeMap::new();
    if let Some(arr) = v["data"].as_array() {
        for m in arr {
            let Some(id) = m["id"].as_str() else {
                continue;
            };
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            let ctx_len = m["context_window"]
                .as_u64()
                .or_else(|| m["max_input_tokens"].as_u64())
                .or_else(|| m["context_length"].as_u64())
                .filter(|&n| n >= 1024)
                .map(|n| n as usize);
            map.entry(id.to_string()).or_insert(ctx_len);
        }
    }
    map.into_iter().collect()
}

/// Tarik daftar model Anthropic dari endpoint `GET {base_url}/v1/models?limit=100`.
pub async fn fetch_models_with_context(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<(String, Option<usize>)>> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let base = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url.trim_end_matches('/')
    };
    let url = format!("{base}/v1/models?limit=100");
    let resp = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .with_context(|| format!("gagal menghubungi {url}"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        let safe = crate::security::redact_secrets(&txt.chars().take(300).collect::<String>());
        return Err(anyhow!("endpoint Anthropic /v1/models membalas {st}: {safe}"));
    }
    let v: Value = resp
        .json()
        .await
        .context("respons Anthropic /v1/models bukan JSON valid")?;
    Ok(parse_anthropic_models_response(&v))
}
