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
use anyhow::Result;
use async_trait::async_trait;
use client::{McpClient, McpToolInfo};
use serde_json::Value;
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

/// Hubungkan 1 server MCP dari config; kembalikan tool-toolnya.
/// Err berarti server gagal start/handshake — pemanggil melewatinya.
pub async fn connect_server(cfg: &crate::config::McpServerCfg) -> Result<Vec<Arc<dyn Tool>>> {
    let (client, infos) = McpClient::start(cfg).await?;
    Ok(infos
        .iter()
        .map(|info| {
            Arc::new(McpTool::new(&cfg.name, info, client.clone(), cfg.trust)) as Arc<dyn Tool>
        })
        .collect())
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
