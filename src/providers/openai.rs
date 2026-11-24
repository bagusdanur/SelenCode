use super::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(90);

/// Provider OpenAI-compatible: 9Router, OpenRouter, OpenAI, dsb.
/// POST {base_url}/chat/completions dengan stream=true + SSE.
pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String, model: String, max_output_tokens: u32) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            max_output_tokens,
            client,
        }
    }
}

fn messages_to_openai(msgs: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::System | Role::User => {
                out.push(json!({"role": if m.role == Role::System {"system"} else {"user"},
                                "content": m.plain_text()}));
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
        let mut body = json!({
            "model": self.model,
            "messages": messages_to_openai(msgs),
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
        let resp = tokio::time::timeout(
            RESPONSE_TIMEOUT,
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "timeout {} detik menunggu respons awal provider di {url}",
                RESPONSE_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("gagal menghubungi provider di {url}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(anyhow!("provider membalas {st}: {}", txt.chars().take(500).collect::<String>()));
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut text = String::new();
        let mut calls: BTreeMap<u64, PartialCall> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;

        'stream: loop {
            let next_chunk = tokio::time::timeout(CHUNK_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    anyhow!(
                        "stream provider macet (tidak ada data selama {} detik)",
                        CHUNK_TIMEOUT.as_secs()
                    )
                })?;
            let Some(chunk) = next_chunk else {
                break 'stream;
            };
            let chunk = chunk.context("stream provider terputus")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim();
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
                // Chunk terakhir (include_usage) membawa field usage.
                if v["usage"].is_object() {
                    usage = Some(Usage {
                        input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                        output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                    });
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
        let tail = buf.trim();
        if let Some(data) = tail.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v["usage"].is_object() {
                        usage = Some(Usage {
                            input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                        });
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
