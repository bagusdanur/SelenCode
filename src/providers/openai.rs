use super::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Provider OpenAI-compatible: 9Router, OpenRouter, OpenAI, dsb.
/// POST {base_url}/chat/completions dengan stream=true + SSE.
pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
    timeouts: ProviderTimeouts,
    client: reqwest::Client,
}

impl OpenAiProvider {
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

/// Parse respons JSON `GET {base_url}/models` OpenAI-compatible menjadi daftar `(model_id, Option<context_limit>)`.
/// Mendukung berbagai skema field context window (OpenAI, OpenRouter, 9Router, LiteLLM, vLLM, Ollama, Fireworks, Groq).
pub fn parse_openai_models_response(v: &Value) -> Vec<(String, Option<usize>)> {
    let mut map: BTreeMap<String, Option<usize>> = BTreeMap::new();
    let arr_opt = v["data"].as_array().or_else(|| v["models"].as_array());
    if let Some(arr) = arr_opt {
        for m in arr {
            let Some(id) = m["id"].as_str().or_else(|| m["name"].as_str()) else {
                continue;
            };
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            let ctx_len = m["context_length"]
                .as_u64()
                .or_else(|| m["max_context_length"].as_u64())
                .or_else(|| m["context_window"].as_u64())
                .or_else(|| m["context_size"].as_u64())
                .or_else(|| m["max_model_len"].as_u64())
                .or_else(|| m["input_token_limit"].as_u64())
                .or_else(|| m["inputTokenLimit"].as_u64())
                .or_else(|| m["top_provider"]["context_length"].as_u64())
                .or_else(|| m["limits"]["context"].as_u64())
                .or_else(|| m["meta"]["context_length"].as_u64())
                .filter(|&n| n >= 1024)
                .map(|n| n as usize);
            map.entry(id.to_string()).or_insert(ctx_len);
        }
    }
    map.into_iter().collect()
}

/// Tarik daftar ID model + kapasitas context window opsional (bila API melaporkan
/// `context_length` / `max_context_length` / `context_window` / `max_model_len` / `input_token_limit`)
/// dari endpoint OpenAI-compatible `GET {base_url}/models`.
pub async fn fetch_models_with_context(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<(String, Option<usize>)>> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("gagal menghubungi {url}"))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        let safe = crate::security::redact_secrets(&txt.chars().take(300).collect::<String>());
        return Err(anyhow!("endpoint /models membalas {st}: {safe}"));
    }
    let v: Value = resp.json().await.context("respons /models bukan JSON valid")?;
    Ok(parse_openai_models_response(&v))
}

/// Tarik daftar ID model yang tersedia dari endpoint OpenAI-compatible
/// `GET {base_url}/models` (dipakai `/model list` atau `/models`).
pub async fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let items = fetch_models_with_context(base_url, api_key).await?;
    Ok(items.into_iter().map(|(id, _)| id).collect())
}

/// Ekstrak `Usage` termasuk `cache_read_tokens` & `cache_write_tokens` dari objek JSON `usage`
/// respons OpenAI-compatible (desain internal SelenCode).
/// Mendukung:
/// - OpenAI / 9Router / Gemini-via-OpenAI (`prompt_tokens_details.cached_tokens`)
/// - Anthropic-style proxy (`cache_read_input_tokens`, `cache_creation_input_tokens`)
/// - DeepSeek (`prompt_cache_hit_tokens`) & Kimi/Moonshot (`cached_tokens`).
pub fn parse_openai_usage(u: &Value) -> Usage {
    let first_nonzero = |paths: &[&[&str]]| -> u64 {
        for path in paths {
            let mut cur = u;
            let mut ok = true;
            for key in *path {
                if let Some(next) = cur.get(*key) {
                    cur = next;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                if let Some(n) = cur.as_u64().filter(|&v| v > 0) {
                    return n;
                }
            }
        }
        0
    };
    let input_tokens = u["prompt_tokens"]
        .as_u64()
        .or_else(|| u["input_tokens"].as_u64())
        .unwrap_or(0);
    let output_tokens = u["completion_tokens"]
        .as_u64()
        .or_else(|| u["output_tokens"].as_u64())
        .unwrap_or(0);
    let cache_read_tokens = first_nonzero(&[
        &["prompt_tokens_details", "cached_tokens"],
        &["cache_read_input_tokens"],
        &["prompt_cache_hit_tokens"],
        &["cached_tokens"],
    ]);
    let cache_write_tokens = first_nonzero(&[
        &["prompt_tokens_details", "cache_write_tokens"],
        &["prompt_tokens_details", "cache_creation_input_tokens"],
        &["cache_creation_input_tokens"],
        &["cache_write_tokens"],
    ]);
    Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
    }
}

/// Tentukan apakah rute OpenAI-wire ini menerima & diuntungkan oleh penanda `cache_control`
/// gaya Anthropic pada amplop pesan (desain internal SelenCode).
///
/// - Claude pada gateway non-OpenAI (OpenRouter, Nous, LiteLLM, 9Router): `true`.
/// - Kimi/Moonshot & Qwen pada OpenRouter/Nous/Alibaba/DashScope: `true`.
/// - Gemini (`gemini-*`, `ag/gemini-*`), OpenAI (`gpt-*`/`o1`/`o3`/`o4`), DeepSeek, & server lokal:
///   `false` (memakai automatic prefix caching berbasis byte prefiks stabil — tidak kirim `cache_control`
///   agar relay ketat tidak membalas HTTP 400).
pub fn should_apply_openai_envelope_cache_control(base_url: &str, model: &str) -> bool {
    let url_low = base_url.trim().to_ascii_lowercase();
    let model_low = model.trim().to_ascii_lowercase();
    if url_low.contains("api.openai.com")
        || url_low.contains("127.0.0.1")
        || url_low.contains("localhost")
    {
        return false;
    }
    let is_claude = model_low.contains("claude");
    let is_kimi = model_low.contains("kimi") || model_low.contains("moonshot");
    let is_qwen = model_low.contains("qwen");
    let is_openrouter = url_low.contains("openrouter.ai");
    let is_nous = url_low.contains("nousresearch.com");
    let is_alibaba = url_low.contains("dashscope") || url_low.contains("opencode");
    if is_claude {
        return true;
    }
    if (is_openrouter || is_nous) && (is_kimi || is_qwen) {
        return true;
    }
    if is_alibaba && is_qwen {
        return true;
    }
    false
}

/// Terapkan penanda `cache_control: {"type": "ephemeral"}` gaya amplop pada array pesan OpenAI-wire
/// (desain internal SelenCode).
pub fn apply_openai_envelope_cache_control(messages: &mut [Value]) {
    if messages.is_empty() {
        return;
    }
    let mark_msg = |msg: &mut Value| {
        let Some(obj) = msg.as_object_mut() else {
            return;
        };
        let marker = json!({"type": "ephemeral"});
        match obj.get_mut("content") {
            Some(Value::String(s)) if !s.is_empty() => {
                let text = s.clone();
                obj.insert(
                    "content".into(),
                    json!([{"type": "text", "text": text, "cache_control": marker}]),
                );
            }
            Some(Value::Array(parts)) => {
                if let Some(last) = parts.last_mut().and_then(|v| v.as_object_mut()) {
                    last.insert("cache_control".into(), marker);
                }
            }
            _ => {}
        }
    };
    let mut used = 0usize;
    if messages[0].get("role").and_then(|v| v.as_str()) == Some("system") {
        mark_msg(&mut messages[0]);
        used += 1;
    }
    let budget = 4usize.saturating_sub(used);
    if budget == 0 {
        return;
    }
    let cacheable_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let role = m.get("role").and_then(|v| v.as_str())?;
            if role == "system" || role == "tool" {
                return None;
            }
            let has_body = match m.get("content") {
                Some(Value::String(s)) => !s.is_empty(),
                Some(Value::Array(a)) => !a.is_empty(),
                _ => false,
            };
            has_body.then_some(i)
        })
        .collect();
    let start = cacheable_idxs.len().saturating_sub(budget);
    for &idx in &cacheable_idxs[start..] {
        mark_msg(&mut messages[idx]);
    }
}

fn messages_to_openai(msgs: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::System => {
                out.push(json!({"role": "system", "content": m.plain_text()}));
            }
            Role::User => {
                let has_image = m
                    .content
                    .iter()
                    .any(|b| matches!(b, Block::Image { .. }));
                if has_image {
                    let mut parts: Vec<Value> = Vec::new();
                    for b in &m.content {
                        match b {
                            Block::Text(t) if !t.is_empty() => {
                                parts.push(json!({"type": "text", "text": t}));
                            }
                            Block::Image {
                                media_type,
                                data_base64,
                            } => {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{media_type};base64,{data_base64}")
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                    out.push(json!({"role": "user", "content": parts}));
                } else {
                    out.push(json!({"role": "user", "content": m.plain_text()}));
                }
            }
            Role::Assistant => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                let text = m.plain_text();
                obj.insert(
                    "content".into(),
                    if text.is_empty() { Value::Null } else { json!(text) },
                );
                let calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, args } => Some(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": args.to_string()}
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    obj.insert("tool_calls".into(), Value::Array(calls));
                }
                out.push(Value::Object(obj));
            }
            Role::Tool => {
                for b in &m.content {
                    if let Block::ToolResult {
                        id, content, ..
                    } = b
                    {
                        out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                    }
                }
            }
        }
        // ToolResult yang menempel di role User (kompat) juga diterjemahkan.
        if m.role == Role::User {
            for b in &m.content {
                if let Block::ToolResult { id, content, .. } = b {
                    out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                }
            }
        }
    }
    out
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args_buf: String,
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn stream_chat(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let mut wire_msgs = messages_to_openai(msgs);
        if should_apply_openai_envelope_cache_control(&self.base_url, &self.model) {
            apply_openai_envelope_cache_control(&mut wire_msgs);
        }
        let mut body = json!({
            "model": self.model,
            "messages": wire_msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": self.max_output_tokens,
        });
        if !tools.is_empty() {
            let ts: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.parameters}})
                })
                .collect();
            body["tools"] = Value::Array(ts);
        }

        let url = format!("{}/chat/completions", self.base_url);
        let est_tokens = crate::agent::context::estimate_tokens(msgs);
        let eff_idle = self.timeouts.effective_stream_idle(est_tokens);
        let eff_total = self.timeouts.effective_total(est_tokens);
        let req_deadline = Instant::now() + eff_total;
        let resp_wait = self.timeouts.response.min(eff_total);
        let resp = tokio::time::timeout(
            resp_wait,
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "timeout {} detik menunggu respons awal provider di {url} — provider tidak merespons",
                resp_wait.as_secs()
            )
        })?
        .with_context(|| format!("gagal menghubungi provider di {url}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let txt = tokio::time::timeout(Duration::from_secs(15), resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let safe = crate::security::redact_secrets(&txt.chars().take(500).collect::<String>());
            return Err(anyhow!("provider membalas {st}: {safe}"));
        }

        const MAX_SSE_LINE_BYTES: usize = 4 * 1024 * 1024;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut calls: BTreeMap<u64, PartialCall> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;

        'stream: loop {
            let rem_total = req_deadline.saturating_duration_since(Instant::now());
            if rem_total.is_zero() {
                return Err(anyhow!(
                    "timeout total panggilan provider ({} detik) — koneksi dihentikan",
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
                            "timeout total panggilan provider ({} detik) — koneksi dihentikan",
                            eff_total.as_secs()
                        )
                    } else {
                        anyhow!(
                            "timeout ketidakaktifan stream provider (tidak ada data selama {} detik) — koneksi dihentikan",
                            eff_idle.as_secs()
                        )
                    }
                })?;
            let Some(chunk) = next_chunk else {
                break 'stream;
            };
            let chunk = chunk.context("stream provider terputus")?;
            if buf.len().saturating_add(chunk.len()) > MAX_SSE_LINE_BYTES {
                return Err(anyhow!("stream provider melebihi batas buffer SSE (4 MB)"));
            }
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw_line: Vec<u8> = buf.drain(..=pos).collect();
                let line_str = String::from_utf8_lossy(&raw_line);
                let line = line_str.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    buf.clear();
                    break 'stream;
                }
                if data.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // Chunk terakhir (include_usage) membawa field usage (+ detail cache read/write).
                if v["usage"].is_object() {
                    usage = Some(parse_openai_usage(&v["usage"]));
                }
                let choice = &v["choices"][0];
                if let Some(fr) = choice["finish_reason"].as_str() {
                    finish_reason = Some(fr.to_string());
                }
                let delta = &choice["delta"];
                if let Some(tok) = delta["content"].as_str() {
                    if !tok.is_empty() {
                        text.push_str(tok);
                        let _ = tx.send(StreamEvent::Token(tok.to_string())).await;
                    }
                }
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let idx = tc["index"].as_u64().unwrap_or(0);
                        let e = calls.entry(idx).or_default();
                        if let Some(id) = tc["id"].as_str() {
                            if !id.is_empty() {
                                e.id = id.to_string();
                            }
                        }
                        if let Some(n) = tc["function"]["name"].as_str() {
                            if !n.is_empty() {
                                e.name = n.to_string();
                            }
                        }
                        if let Some(a) = tc["function"]["arguments"].as_str() {
                            e.args_buf.push_str(a);
                        }
                    }
                }
            }
        }
        // Tangani baris terakhir tanpa '\n' penutup bila stream langsung EOF.
        let tail_str = String::from_utf8_lossy(&buf);
        let tail = tail_str.trim();
        if let Some(data) = tail.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v["usage"].is_object() {
                        usage = Some(parse_openai_usage(&v["usage"]));
                    }
                    let choice = &v["choices"][0];
                    if let Some(fr) = choice["finish_reason"].as_str() {
                        finish_reason = Some(fr.to_string());
                    }
                    if let Some(tok) = choice["delta"]["content"].as_str() {
                        if !tok.is_empty() {
                            text.push_str(tok);
                            let _ = tx.send(StreamEvent::Token(tok.to_string())).await;
                        }
                    }
                }
            }
        }

        let tool_calls = calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|c| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4().simple())
                } else {
                    c.id
                },
                name: c.name,
                args: serde_json::from_str(&c.args_buf).unwrap_or_else(|_| {
                    if c.args_buf.trim().is_empty() {
                        json!({})
                    } else {
                        json!({"_raw": c.args_buf})
                    }
                }),
            })
            .collect();

        Ok(AssistantReply {
            text,
            tool_calls,
            finish_reason,
            usage,
        })
    }
}
