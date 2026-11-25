//! Pembungkus tool MCP (Fase 7): tool dari server MCP eksternal tampil di
//! registry selen sebagai tool biasa lewat trait `Tool` yang sama.
//!
//! Keputusan yang tercatat:
//! - Nama terekspos ke model: `mcp__<server>__<tool>` (pola umum MCP) agar
//!   tidak tabrakan dengan tool bawaan maupun antar-server.
//! - `needs_approval` = true secara default — server eksternal tidak
//!   dikenal sifatnya. Config per server `trust = true` mematikannya.
//! - Tool MCP HANYA didaftarkan ke registry induk mode Execute (di
//!   Session::build), TIDAK ke registry baca-saja plan mode / subagent.
//! - Kegagalan start 1 server tidak menggagalkan start selen: server itu
//!   dilewati dengan peringatan ke stderr, server lain tetap didaftarkan.
//! - Output tool MCP tetap melewati `redact_secrets` terpusat di loop
//!   agent (agent/loop.rs) — McpTool tidak punya jalur keluaran lain.

pub mod client;

use crate::config::Config;
use crate::providers::ToolSpec;
use crate::tools::{Ctx, Tool};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use client::{validate_tool_args, McpClient, McpToolInfo};
use serde_json::{json, Value};
use std::sync::Arc;

/// Nama terekspos untuk tool MCP: `mcp__<server>__<tool>`.
pub fn exposed_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

pub struct McpTool {
    server_name: String,
    remote_name: String,
    description: String,
    schema: Value,
    client: Arc<McpClient>,
    trust: bool,
}

impl McpTool {
    pub fn new(
        server_name: &str,
        info: &McpToolInfo,
        client: Arc<McpClient>,
        trust: bool,
    ) -> Self {
        let description = if info.description.trim().is_empty() {
            format!("Tool dari server MCP eksternal '{server_name}'.")
        } else {
            format!(
                "Tool dari server MCP eksternal '{server_name}': {}",
                info.description
            )
        };
        Self {
            server_name: server_name.to_string(),
            remote_name: info.name.clone(),
            description,
            schema: info.input_schema.clone(),
            client,
            trust,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: exposed_name(&self.server_name, &self.remote_name),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        // Server eksternal tidak dikenal -> approval, kecuali trust = true.
        !self.trust
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        validate_tool_args(&self.schema, &args)?;
        if self.needs_approval(&args) {
            let detail =
                serde_json::to_string_pretty(&args).unwrap_or_else(|_| args.to_string());
            if !ctx
                .approver
                .ask(
                    &format!("mcp: {}/{}", self.server_name, self.remote_name),
                    &detail,
                )
                .await
            {
                return Ok(format!(
                    "Ditolak user — tool MCP '{}/{}' TIDAK dijalankan.",
                    self.server_name, self.remote_name
                ));
            }
        }
        self.client.call_tool(&self.remote_name, args).await
    }
}

/// Tool MCP untuk `resources/list` & `resources/read` (didaftarkan bila server mengiklankan kapabilitas resources).
pub struct McpResourceTool {
    server_name: String,
    client: Arc<McpClient>,
    trust: bool,
}

#[async_trait]
impl Tool for McpResourceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: exposed_name(&self.server_name, "resources"),
            description: format!(
                "Daftar atau baca resource dari server MCP eksternal '{}' (`action`: \"list\" atau \"read\" dengan `uri`).",
                self.server_name
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "read"] },
                    "uri": { "type": "string", "description": "URI resource (wajib untuk action='read')" }
                },
                "required": ["action"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        !self.trust
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("argumen 'action' wajib (list/read)"))?;
        if self.needs_approval(&args)
            && !ctx
                .approver
                .ask(
                    &format!("mcp: {}/resources ({action})", self.server_name),
                    &args.to_string(),
                )
                .await
        {
            return Ok("Ditolak user — akses resource MCP dibatalkan.".into());
        }
        match action {
            "list" => {
                let items = self.client.list_resources().await?;
                if items.is_empty() {
                    return Ok("Server MCP tidak memiliki resource.".into());
                }
                let mut out = format!("Resources dari MCP '{}' ({}):\n", self.server_name, items.len());
                for r in items {
                    out.push_str(&format!("- {} ({}) — {}\n", r.uri, r.name, r.description));
                }
                Ok(out.trim_end().to_string())
            }
            "read" => {
                let uri = args
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("argumen 'uri' wajib untuk action='read'"))?;
                self.client.read_resource(uri).await
            }
            other => Err(anyhow!("action '{other}' tidak dikenal (pilih: list, read)")),
        }
    }
}

/// Tool MCP untuk `prompts/list` & `prompts/get` (didaftarkan bila server mengiklankan kapabilitas prompts).
pub struct McpPromptTool {
    server_name: String,
    client: Arc<McpClient>,
    trust: bool,
}

#[async_trait]
impl Tool for McpPromptTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: exposed_name(&self.server_name, "prompts"),
            description: format!(
                "Daftar atau ambil prompt template dari server MCP eksternal '{}' (`action`: \"list\" atau \"get\" dengan `name`).",
                self.server_name
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get"] },
                    "name": { "type": "string", "description": "Nama prompt (wajib untuk action='get')" },
                    "arguments": { "type": "object", "description": "Argumen opsional untuk prompt" }
                },
                "required": ["action"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        !self.trust
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("argumen 'action' wajib (list/get)"))?;
        if self.needs_approval(&args)
            && !ctx
                .approver
                .ask(
                    &format!("mcp: {}/prompts ({action})", self.server_name),
                    &args.to_string(),
                )
                .await
        {
            return Ok("Ditolak user — akses prompt MCP dibatalkan.".into());
        }
        match action {
            "list" => {
                let items = self.client.list_prompts().await?;
                if items.is_empty() {
                    return Ok("Server MCP tidak memiliki prompt.".into());
                }
                let mut out = format!("Prompts dari MCP '{}' ({}):\n", self.server_name, items.len());
                for p in items {
                    out.push_str(&format!("- {} — {}\n", p.name, p.description));
                }
                Ok(out.trim_end().to_string())
            }
            "get" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("argumen 'name' wajib untuk action='get'"))?;
                let p_args = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.client.get_prompt(name, p_args).await
            }
            other => Err(anyhow!("action '{other}' tidak dikenal (pilih: list, get)")),
        }
    }
}

/// Hubungkan 1 server MCP dari config; kembalikan tool-toolnya (+ resource & prompt tools bila didukung server).
pub async fn connect_server(cfg: &crate::config::McpServerCfg) -> Result<Vec<Arc<dyn Tool>>> {
    let (client, infos) = McpClient::start(cfg).await?;
    let caps = client.capabilities().await;
    let mut tools: Vec<Arc<dyn Tool>> = infos
        .iter()
        .map(|info| {
            Arc::new(McpTool::new(&cfg.name, info, client.clone(), cfg.trust)) as Arc<dyn Tool>
        })
        .collect();
    if caps.resources {
        tools.push(Arc::new(McpResourceTool {
            server_name: cfg.name.clone(),
            client: client.clone(),
            trust: cfg.trust,
        }));
    }
    if caps.prompts {
        tools.push(Arc::new(McpPromptTool {
            server_name: cfg.name.clone(),
            client: client.clone(),
            trust: cfg.trust,
        }));
    }
    Ok(tools)
}

/// Bangun semua tool MCP dari config. Server yang gagal start dilewati
/// dengan peringatan stderr (start selen TIDAK pernah digagalkan MCP).
pub async fn build_mcp_tools(cfg: &Config) -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = Vec::new();
    for server in &cfg.mcp_servers {
        match connect_server(server).await {
            Ok(tools) => {
                if tools.is_empty() {
                    eprintln!(
                        "[mcp] server '{}' jalan tapi tidak mengiklankan tool apa pun",
                        server.name
                    );
                }
                out.extend(tools);
            }
            Err(e) => {
                eprintln!("[mcp] server '{}' dilewati: {e}", server.name);
            }
        }
    }
    out
}
