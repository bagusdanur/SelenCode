//! Klien MCP transport stdio (Fase 7 + Ekspansi Kompatibilitas MCP).
//!
//! Protokol: JSON-RPC 2.0 NDJSON di atas stdio (`PROTOCOL_VERSION = "2024-11-05"`).
//! Fitur protokol yang DIDUKUNG:
//! - Handshake `initialize` + `notifications/initialized` + negosiasi kapabilitas server
//! - `tools/list` & `tools/call` (+ validasi JSON Schema input sebelum panggilan)
//! - `resources/list` & `resources/read`
//! - `prompts/list` & `prompts/get`
//! - `ping`, notifikasi pembatalan `notifications/cancelled` saat timeout,
//!   auto-reconnect 1x bila proses server mati, & scrub env var sensitif.
//!
//! Fitur protokol yang BELUM didukung (dinyatakan eksplisit):
//! - `sampling/createMessage` (permintaan LLM balik dari server) — ditolak secara
//!   eksplisit dengan JSON-RPC error `-32601 Method not found` agar server tidak hang.

use crate::config::McpServerCfg;
use crate::sandbox::sensitive_env_keys_present;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Ringkasan fitur MCP yang didukung oleh klien SelenCode (transparan untuk audit).
pub const SUPPORTED_MCP_FEATURES: &[&str] = &[
    "initialize / notifications/initialized",
    "tools/list & tools/call (+ input schema validation)",
    "resources/list & resources/read",
    "prompts/list & prompts/get",
    "ping & notifications/cancelled on timeout",
    "auto-reconnect on dead stdio server",
];

pub const UNSUPPORTED_MCP_FEATURES: &[&str] = &[
    "sampling/createMessage (server-initiated LLM sampling)",
    "SSE / Streamable HTTP transport (saat ini stdio transport)",
];

/// Kapabilitas yang diiklankan server pada respons `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
    pub logging: bool,
    pub server_name: String,
    pub server_version: String,
}

/// Info satu tool yang diiklankan server lewat `tools/list`.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Info satu resource yang diiklankan server lewat `resources/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

/// Info satu argumen prompt MCP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Info satu prompt yang diiklankan server lewat `prompts/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

/// Bingkai 1 pesan JSON-RPC sebagai 1 baris NDJSON (JSON + '\n').
pub fn encode_message(msg: &Value) -> String {
    let mut s = serde_json::to_string(msg).unwrap_or_else(|_| "null".to_string());
    s.push('\n');
    s
}

/// Urus 1 baris NDJSON menjadi pesan JSON. Baris kosong ditolak.
pub fn decode_message(line: &str) -> Result<Value> {
    let line = line.trim();
    if line.is_empty() {
        return Err(anyhow!("baris MCP kosong"));
    }
    Ok(serde_json::from_str(line)?)
}

/// Validasi argumen tool terhadap JSON Schema `inputSchema` sebelum dikirim ke server MCP.
/// Mencegah server eksternal crash atau hang akibat argumen tidak valid dari model.
pub fn validate_tool_args(schema: &Value, args: &Value) -> Result<()> {
    let Some(obj) = args.as_object() else {
        if schema.get("type").and_then(|v| v.as_str()) == Some("object") {
            return Err(anyhow!("argumen tool MCP harus berupa objek JSON"));
        }
        return Ok(());
    };

    if let Some(reqs) = schema.get("required").and_then(|v| v.as_array()) {
        for r in reqs {
            if let Some(key) = r.as_str() {
                match obj.get(key) {
                    None | Some(Value::Null) => {
                        return Err(anyhow!(
                            "validasi schema MCP gagal: field wajib '{key}' tidak ada atau null"
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
        for (k, val) in obj {
            let Some(prop_schema) = props.get(k) else {
                continue;
            };
            let Some(expected) = prop_schema.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            let ok = match expected {
                "string" => val.is_string(),
                "integer" => val.as_i64().is_some() || val.as_u64().is_some(),
                "number" => val.is_number(),
                "boolean" => val.is_boolean(),
                "array" => val.is_array(),
                "object" => val.is_object(),
                "null" => val.is_null(),
                _ => true,
            };
            if !ok {
                return Err(anyhow!(
                    "validasi schema MCP gagal: field '{k}' harus bertipe '{expected}'"
                ));
            }
        }
    }

    Ok(())
}

struct Transport {
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
}

struct Inner {
    cfg: McpServerCfg,
    transport: Mutex<Transport>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    next_id: AtomicU64,
    dead: AtomicBool,
    timeout: Duration,
    server_name: String,
    capabilities: Mutex<McpCapabilities>,
    reconnect_lock: Mutex<()>,
}

pub struct McpClient {
    inner: Arc<Inner>,
}

fn spawn_transport(cfg: &McpServerCfg) -> Result<(Transport, tokio::process::ChildStdout)> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Saring env sensitif dari proses induk, lalu terapkan `cfg.env` eksplisit milik server ini.
    for key in sensitive_env_keys_present() {
        if !cfg.env.contains_key(&key) {
            cmd.env_remove(key);
        }
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| {
        anyhow!(
            "gagal menjalankan server MCP '{}' ({}): {e}",
            cfg.name,
            cfg.command
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("stdin server MCP '{}' tidak tersedia", cfg.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("stdout server MCP '{}' tidak tersedia", cfg.name))?;
    Ok((Transport { child, stdin }, stdout))
}

fn spawn_reader_task(inner: Arc<Inner>, stdout: tokio::process::ChildStdout) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = decode_message(&line) else {
                continue;
            };
            // Jika server mengirim request (punya `method` DAN `id`), balas eksplisit agar server tidak hang!
            if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                if let Some(req_id) = msg.get("id").cloned() {
                    let reply = if method == "ping" {
                        json!({"jsonrpc": "2.0", "id": req_id, "result": {}})
                    } else {
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "error": {
                                "code": -32601,
                                "message": format!("Method '{method}' belum didukung oleh klien SelenCode")
                            }
                        })
                    };
                    let mut tr = inner.transport.lock().await;
                    let _ = tr.stdin.write_all(encode_message(&reply).as_bytes()).await;
                    let _ = tr.stdin.flush().await;
                }
                continue;
            }
            let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
                continue;
            };
            let tx = inner.pending.lock().await.remove(&id);
            if let Some(tx) = tx {
                let res = if let Some(err) = msg.get("error") {
                    Err(anyhow!("error JSON-RPC dari server MCP: {err}"))
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(res);
            }
        }
        inner.dead.store(true, Ordering::SeqCst);
        let mut pending = inner.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(anyhow!(
                "server MCP '{}' berhenti di tengah sesi",
                inner.server_name
            )));
        }
    });
}

impl McpClient {
    /// Spawn server dari config, jalankan handshake MCP, negosiasi kapabilitas,
    /// lalu ambil daftar tool via `tools/list`.
    pub async fn start(cfg: &McpServerCfg) -> Result<(Arc<Self>, Vec<McpToolInfo>)> {
        let (transport, stdout) = spawn_transport(cfg)?;
        let inner = Arc::new(Inner {
            cfg: cfg.clone(),
            transport: Mutex::new(transport),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            dead: AtomicBool::new(false),
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
            server_name: cfg.name.clone(),
            capabilities: Mutex::new(McpCapabilities::default()),
            reconnect_lock: Mutex::new(()),
        });
        spawn_reader_task(inner.clone(), stdout);

        let client = Arc::new(McpClient { inner });
        client.perform_handshake().await?;
        let list = client
            .request_raw("tools/list", json!({}))
            .await
            .map_err(|e| anyhow!("tools/list server MCP '{}' gagal: {e}", cfg.name))?;
        Ok((client, parse_tool_list(&list)))
    }

    async fn perform_handshake(&self) -> Result<()> {
        let init_res = self
            .request_raw(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "roots": { "listChanged": false }
                    },
                    "clientInfo": {
                        "name": "selencode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "handshake initialize server MCP '{}' gagal: {e}",
                    self.inner.server_name
                )
            })?;
        let caps = parse_capabilities(&init_res);
        *self.inner.capabilities.lock().await = caps;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Sambungkan ulang server MCP jika proses mati di tengah sesi.
    pub async fn reconnect(&self) -> Result<()> {
        let _guard = self.inner.reconnect_lock.lock().await;
        if !self.inner.dead.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (new_transport, stdout) = spawn_transport(&self.inner.cfg)?;
        {
            let mut tr = self.inner.transport.lock().await;
            *tr = new_transport;
        }
        self.inner.dead.store(false, Ordering::SeqCst);
        spawn_reader_task(self.inner.clone(), stdout);
        self.perform_handshake().await
    }

    pub fn server_name(&self) -> &str {
        &self.inner.server_name
    }

    pub async fn capabilities(&self) -> McpCapabilities {
        self.inner.capabilities.lock().await.clone()
    }

    pub fn is_alive(&self) -> bool {
        !self.inner.dead.load(Ordering::SeqCst)
    }

    /// Kirim request JSON-RPC dengan auto-reconnect 1x bila proses server mati sebelumnya.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.inner.dead.load(Ordering::SeqCst) {
            self.reconnect().await?;
        }
        match self.request_raw(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if self.inner.dead.load(Ordering::SeqCst) => {
                if self.reconnect().await.is_ok() {
                    self.request_raw(method, params).await
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn request_raw(&self, method: &str, params: Value) -> Result<Value> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "server MCP '{}' sudah mati — panggilan '{method}' dibatalkan",
                self.inner.server_name
            ));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.write_line(&encode_message(&msg)).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(self.inner.timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(anyhow!(
                "server MCP '{}' berhenti sebelum menjawab '{method}'",
                self.inner.server_name
            )),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                // Kirim notifikasi pembatalan resmi sesuai spesifikasi MCP.
                let _ = self
                    .notify(
                        "notifications/cancelled",
                        json!({ "requestId": id, "reason": "client timeout" }),
                    )
                    .await;
                Err(anyhow!(
                    "timeout {} detik menunggu respons '{method}' dari server MCP '{}'",
                    self.inner.timeout.as_secs(),
                    self.inner.server_name
                ))
            }
        }
    }

    /// Kirim notifikasi JSON-RPC (tanpa id, tanpa menunggu jawaban).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_line(&encode_message(&msg)).await
    }

    async fn write_line(&self, line: &str) -> Result<()> {
        let mut tr = self.inner.transport.lock().await;
        tr.stdin.write_all(line.as_bytes()).await.map_err(|e| {
            anyhow!(
                "gagal menulis ke stdin server MCP '{}': {e}",
                self.inner.server_name
            )
        })?;
        tr.stdin.flush().await?;
        Ok(())
    }

    /// Uji kesehatan koneksi server MCP (`ping`).
    pub async fn ping(&self) -> Result<()> {
        let _ = self.request("ping", json!({})).await?;
        Ok(())
    }

    /// Panggil 1 tool di server (`tools/call`).
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        let text = extract_call_text(&result);
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            Err(anyhow!("tool MCP '{name}' melaporkan error: {text}"))
        } else {
            Ok(text)
        }
    }

    /// Ambil daftar resources (`resources/list`).
    pub async fn list_resources(&self) -> Result<Vec<McpResourceInfo>> {
        let res = self.request("resources/list", json!({})).await?;
        Ok(parse_resource_list(&res))
    }

    /// Baca isi satu resource berdasarkan URI (`resources/read`).
    pub async fn read_resource(&self, uri: &str) -> Result<String> {
        let res = self
            .request("resources/read", json!({ "uri": uri }))
            .await?;
        Ok(extract_resource_text(&res))
    }

    /// Ambil daftar prompt template (`prompts/list`).
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptInfo>> {
        let res = self.request("prompts/list", json!({})).await?;
        Ok(parse_prompt_list(&res))
    }

    /// Ambil hasil prompt template (`prompts/get`).
    pub async fn get_prompt(&self, name: &str, arguments: Value) -> Result<String> {
        let res = self
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        Ok(extract_prompt_text(&res))
    }
}

fn parse_capabilities(init_result: &Value) -> McpCapabilities {
    let caps = init_result.get("capabilities").unwrap_or(&Value::Null);
    let info = init_result.get("serverInfo").unwrap_or(&Value::Null);
    McpCapabilities {
        tools: caps.get("tools").is_some(),
        resources: caps.get("resources").is_some(),
        prompts: caps.get("prompts").is_some(),
        logging: caps.get("logging").is_some(),
        server_name: info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        server_version: info
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn parse_tool_list(result: &Value) -> Vec<McpToolInfo> {
    let Some(tools) = result.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            Some(McpToolInfo {
                name,
                description,
                input_schema,
            })
        })
        .collect()
}

pub fn parse_resource_list(result: &Value) -> Vec<McpResourceInfo> {
    let Some(arr) = result.get("resources").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|r| {
            let uri = r.get("uri")?.as_str()?.to_string();
            let name = r
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&uri)
                .to_string();
            let description = r
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mime_type = r
                .get("mimeType")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(McpResourceInfo {
                uri,
                name,
                description,
                mime_type,
            })
        })
        .collect()
}

pub fn parse_prompt_list(result: &Value) -> Vec<McpPromptInfo> {
    let Some(arr) = result.get("prompts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?.to_string();
            let description = p
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = p
                .get("arguments")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|arg| {
                            Some(McpPromptArg {
                                name: arg.get("name")?.as_str()?.to_string(),
                                description: arg
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                required: arg
                                    .get("required")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(McpPromptInfo {
                name,
                description,
                arguments,
            })
        })
        .collect()
}

fn extract_resource_text(result: &Value) -> String {
    let Some(contents) = result.get("contents").and_then(|v| v.as_array()) else {
        return serde_json::to_string_pretty(result).unwrap_or_default();
    };
    let mut parts = Vec::new();
    for item in contents {
        let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
            if uri.is_empty() {
                parts.push(txt.to_string());
            } else {
                parts.push(format!("--- {uri} ---\n{txt}"));
            }
        } else if let Some(blob) = item.get("blob").and_then(|v| v.as_str()) {
            parts.push(format!("[binary blob base64: {} chars ({uri})]", blob.len()));
        }
    }
    parts.join("\n\n")
}

fn extract_prompt_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(desc) = result.get("description").and_then(|v| v.as_str()) {
        if !desc.trim().is_empty() {
            out.push_str(&format!("# {desc}\n"));
        }
    }
    if let Some(msgs) = result.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m.get("content");
            let text = content
                .and_then(|c| c.get("text").and_then(|v| v.as_str()))
                .or_else(|| content.and_then(|c| c.as_str()))
                .unwrap_or("");
            out.push_str(&format!("[{role}] {text}\n"));
        }
    } else {
        out.push_str(&serde_json::to_string_pretty(result).unwrap_or_default());
    }
    out.trim_end().to_string()
}

fn extract_call_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
        return match result.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(result).unwrap_or_default(),
        };
    };
    let mut parts: Vec<String> = Vec::new();
    for item in content {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
            Some("resource") => {
                if let Some(t) = item
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(|v| v.as_str())
                {
                    parts.push(t.to_string());
                } else {
                    parts.push(item.to_string());
                }
            }
            _ => parts.push(item.to_string()),
        }
    }
    parts.join("\n")
}
