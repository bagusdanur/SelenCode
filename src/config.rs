use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub default_provider: String,
    pub providers: HashMap<String, ProviderCfg>,
    #[serde(default)]
    pub agent: AgentCfg,
    #[serde(default)]
    pub shell: ShellCfg,
    /// Server MCP eksternal (Fase 7). Default kosong = perilaku Fase 6.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerCfg>,
}

/// Satu server MCP stdio (Fase 7): proses lokal yang di-spawn selen,
/// tool-nya muncul di registry sebagai `mcp__<name>__<tool>`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerCfg {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env tambahan untuk proses server (opsional).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// true = tool server ini berjalan TANPA approval. Default false:
    /// server eksternal tidak dikenal, jadi setiap panggilan ditanya dulu.
    #[serde(default)]
    pub trust: bool,
    /// Timeout per request JSON-RPC ke server ini (detik).
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_mcp_timeout_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCfg {
    #[serde(default = "default_kind")]
    pub kind: String,
    pub base_url: Option<String>,
    pub model: String,
    pub api_key_env: String,
    /// Harga opsional per 1M token (Fase 3). Bila kosong, biaya TIDAK ditebak —
    /// yang ditampilkan hanya jumlah token.
    #[serde(default)]
    pub price_in: Option<f64>,
    #[serde(default)]
    pub price_out: Option<f64>,
}

fn default_kind() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCfg {
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_approval")]
    pub approval: String,
    /// Batas konteks (estimasi token) untuk compaction; memicu di 80% (Fase 2).
    #[serde(default = "default_context_limit_tokens")]
    pub context_limit_tokens: usize,
    /// Batas subagent `delegate` yang berjalan paralel (Fase 6).
    #[serde(default = "default_max_parallel_subagents")]
    pub max_parallel_subagents: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_output_tokens: default_max_output_tokens(),
            approval: default_approval(),
            context_limit_tokens: default_context_limit_tokens(),
            max_parallel_subagents: default_max_parallel_subagents(),
        }
    }
}

fn default_max_parallel_subagents() -> usize {
    3
}

fn default_max_steps() -> usize {
    25
}
fn default_max_output_tokens() -> u32 {
    8192
}
fn default_approval() -> String {
    "ask".to_string()
}
fn default_context_limit_tokens() -> usize {
    100_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShellCfg {
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default = "default_shell_timeout")]
    pub timeout_secs: u64,
}

impl Default for ShellCfg {
    fn default() -> Self {
        Self {
            allowlist: Vec::new(),
            timeout_secs: default_shell_timeout(),
        }
    }
}

fn default_shell_timeout() -> u64 {
    120
}

/// Urutan pencarian config: ./selencode.toml dulu, lalu ~/.config/selencode/config.toml
pub fn candidate_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("./selencode.toml")];
    if let Some(cfg) = dirs::config_dir() {
        v.push(cfg.join("selencode").join("config.toml"));
    }
    v
}

pub fn load() -> Result<Config> {
    for p in candidate_paths() {
        if p.exists() {
            return load_from(&p);
        }
    }
    Err(anyhow!(
        "Config SelenCode tidak ditemukan. Buat salah satu file ini:\n  ./selencode.toml\n  ~/.config/selencode/config.toml\n\nContoh isi (lihat juga config.example.toml di repo):\n\ndefault_provider = \"router\"\n\n[providers.router]\nkind = \"openai\"\nbase_url = \"https://router.ryukomik.com/v1\"\nmodel = \"isi-model-kamu\"\napi_key_env = \"SELENCODE_ROUTER_KEY\"\n\nLalu set API key-nya: export SELENCODE_ROUTER_KEY=... (nama env mengikuti api_key_env)"
    ))
}

pub fn load_from(path: &std::path::Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("gagal membaca config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("config {} tidak valid", path.display()))?;
    Ok(cfg)
}

impl Config {
    /// Ambil (nama, cfg, api_key) untuk provider terpilih.
    pub fn resolve_provider(&self, name: Option<&str>) -> Result<(String, ProviderCfg, String)> {
        let name = name.unwrap_or(&self.default_provider).to_string();
        let cfg = self
            .providers
            .get(&name)
            .ok_or_else(|| anyhow!("provider '{name}' tidak ada di config"))?
            .clone();
        let key = std::env::var(&cfg.api_key_env).map_err(|_| {
            anyhow!(
                "env var {} (api_key_env provider '{name}') belum di-set. Contoh: export {}=...",
                cfg.api_key_env,
                cfg.api_key_env
            )
        })?;
        Ok((name, cfg, key))
    }

    pub fn auto_approve(&self, yes_flag: bool) -> bool {
        yes_flag || self.agent.approval == "auto"
    }
}
