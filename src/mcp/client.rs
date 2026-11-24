//! Klien MCP transport stdio (Fase 7, Blueprint Bagian 4 & 10).
//!
//! Protokol: JSON-RPC 2.0 di atas stdio, pesan dibingkai newline (NDJSON)
//! sesuai MCP stdio transport. Urutan handshake:
//!   spawn proses server -> request `initialize` ->
//!   notifikasi `notifications/initialized` -> `tools/list` ->
//!   tiap panggilan tool = request `tools/call`.
//!
//! Satu task pembaca memiliki stdout anak: baris NDJSON yang membawa `id`
//! dicocokkan ke request yang menunggu lewat map id -> oneshot. Bila stdout
//! tertutup (proses server mati), semua request yang menunggu digagalkan
//! dan panggilan berikutnya langsung error jelas — tidak pernah panik.

use crate::config::McpServerCfg;
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

/// Info satu tool yang diiklankan server lewat `tools/list`.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
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

struct Inner {
    /// Proses anak dijaga tetap hidup selama klien hidup; kill_on_drop
    /// memastikan server ikut mati saat selen keluar.
    #[allow(dead_code)]
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    next_id: AtomicU64,
    dead: AtomicBool,
    timeout: Duration,
    server_name: String,
}

pub struct McpClient {
    inner: Arc<Inner>,
}

impl McpClient {
    /// Spawn server dari config, jalankan handshake MCP, kembalikan klien
    /// + daftar tool yang diiklankan server. Error apa pun di tahap ini
    /// (spawn gagal, proses langsung mati, handshake timeout/gagal) menjadi
    /// Err — pemanggil memperlakukannya sebagai "server ini dilewati".
    pub async fn start(cfg: &McpServerCfg) -> Result<(Arc<Self>, Vec<McpToolInfo>)> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr server dibuang: mode TUI tidak boleh dikotori output
            // server eksternal. (Kegagalan server tetap terlihat sebagai
            // error handshake/panggilan di sisi selen.)
            .stderr(Stdio::null())
            .kill_on_drop(true);
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

        let inner = Arc::new(Inner {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            dead: AtomicBool::new(false),
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
            server_name: cfg.name.clone(),
        });
        let client = Arc::new(McpClient {
            inner: inner.clone(),
        });

        // Task pembaca: miliki stdout, cocokkan respons ke request menunggu.
        {
            let inner = inner.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = decode_message(&line) else {
                        continue; // baris bukan JSON valid — abaikan
                    };
                    // Hanya respons (punya id, tanpa method) yang dirutekan.
                    // Notifikasi & request dari server diabaikan di versi ini.
                    if msg.get("method").is_some() {
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
                // stdout tertutup: server mati/berhenti. Gagalkan semua
                // request yang masih menunggu; request berikutnya langsung
                // error lewat flag `dead`.
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

        // Handshake MCP: initialize -> notifications/initialized -> tools/list.
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "selencode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .map_err(|e| anyhow!("handshake initialize server MCP '{}' gagal: {e}", cfg.name))?;
        client.notify("notifications/initialized", json!({})).await?;
        let list = client
            .request("tools/list", json!({}))
            .await
            .map_err(|e| anyhow!("tools/list server MCP '{}' gagal: {e}", cfg.name))?;
        Ok((client, parse_tool_list(&list)))
    }

    pub fn server_name(&self) -> &str {
        &self.inner.server_name
    }

    /// Kirim request JSON-RPC dan tunggu responsnya (timeout per config).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
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
        let mut stdin = self.inner.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            anyhow!(
                "gagal menulis ke stdin server MCP '{}': {e}",
                self.inner.server_name
            )
        })?;
        stdin.flush().await?;
        Ok(())
    }

    /// Panggil 1 tool di server (`tools/call`). content[] bertipe text
    /// digabung dengan newline; tipe lain direpresentasikan sebagai JSON
    /// ringkas. `isError: true` dari server menjadi Err.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        Ok(extract_call_text(&result))
            .and_then(|text| {
                if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                    Err(anyhow!("tool MCP '{name}' melaporkan error: {text}"))
                } else {
                    Ok(text)
                }
            })
    }
}

/// Urus hasil `tools/list` menjadi daftar info tool.
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

/// Gabungkan content[] hasil tools/call menjadi teks.
fn extract_call_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
        // Server tidak patuh bentuk standar: tampilkan hasil apa adanya.
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
            _ => parts.push(serde_json::to_string(item).unwrap_or_default()),
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_single_ndjson_line() {
        let line = encode_message(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.starts_with('{'));
    }

    #[test]
    fn decode_roundtrip_and_rejects_empty() {
        let v = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}});
        let back = decode_message(&encode_message(&v)).unwrap();
        assert_eq!(back, v);
        assert!(decode_message("").is_err());
        assert!(decode_message("   ").is_err());
        assert!(decode_message("bukan json").is_err());
    }

    #[test]
    fn extract_text_joins_text_blocks() {
        let r = json!({"content": [
            {"type": "text", "text": "satu"},
            {"type": "text", "text": "dua"},
            {"type": "image", "data": "xx"},
        ]});
        let t = extract_call_text(&r);
        assert!(t.starts_with("satu\ndua\n"));
        assert!(t.contains("\"image\""));
    }

    #[test]
    fn parse_tools_list_defaults() {
        let r = json!({"tools": [
            {"name": "echo", "description": "gema", "inputSchema": {"type": "object"}},
            {"name": "tanpa-skema"},
            {"description": "tanpa nama — dilewati"},
        ]});
        let tools = parse_tool_list(&r);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[1].input_schema, json!({"type": "object"}));
    }
}
