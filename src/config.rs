use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_provider: String,
    pub providers: HashMap<String, ProviderCfg>,
    #[serde(default)]
    pub agent: AgentCfg,
    #[serde(default)]
    pub shell: ShellCfg,
    /// Server MCP eksternal (Fase 7). Default kosong = perilaku Fase 6.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerCfg>,
    /// Pengaturan Telegram Bot Gateway (`selen telegram` & `/telegram`).
    #[serde(default, skip_serializing_if = "TelegramCfg::is_empty")]
    pub telegram: TelegramCfg,
}

/// Konfigurasi Telegram Bot Gateway (`[telegram]` di selencode.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramCfg {
    /// Nama env var berisi Bot API token dari @BotFather (mis. `SELENCODE_TELEGRAM_TOKEN`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bot_token_env: String,
    /// Token bot opsional yang disimpan langsung lewat `/telegram setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    /// Whitelist User ID / Chat ID Telegram yang boleh mengontrol SelenCode.
    /// Kosong = bot menolak eksekusi perintah dan hanya membalas ID pengirim untuk keamanan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_users: Vec<i64>,
    /// Mode persetujuan tool di sesi Telegram: `"ask"` (default, kirim tombol inline),
    /// `"edits"` (otomatis edit file), atau `"auto"` (otomatis semua aksi aman).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub approval: String,
}

impl TelegramCfg {
    pub fn is_empty(&self) -> bool {
        self.bot_token_env.trim().is_empty()
            && self.bot_token.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true)
            && self.allowed_users.is_empty()
            && self.approval.trim().is_empty()
    }

    /// Ambil token bot Telegram (prioritas: env var `bot_token_env` -> `SELENCODE_TELEGRAM_TOKEN` -> `bot_token`).
    pub fn resolve_token(&self) -> Result<String> {
        if !self.bot_token_env.trim().is_empty() {
            if let Ok(v) = std::env::var(self.bot_token_env.trim()) {
                if !v.trim().is_empty() {
                    return Ok(v.trim().to_string());
                }
            }
        }
        if let Ok(v) = std::env::var("SELENCODE_TELEGRAM_TOKEN") {
            if !v.trim().is_empty() {
                return Ok(v.trim().to_string());
            }
        }
        if let Some(tok) = self.bot_token.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return Ok(tok.to_string());
        }
        Err(anyhow!(
            "Token bot Telegram belum diatur. Jalankan `/telegram setup <BOT_TOKEN> <USER_ID>` di SelenCode atau set env SELENCODE_TELEGRAM_TOKEN."
        ))
    }

    pub fn is_token_ready(&self) -> bool {
        self.resolve_token().is_ok()
    }

    /// Cek apakah pengirim / chat termasuk whitelist `allowed_users`.
    pub fn is_allowed(&self, user_id: i64, chat_id: i64) -> bool {
        !self.allowed_users.is_empty()
            && (self.allowed_users.contains(&user_id) || self.allowed_users.contains(&chat_id))
    }
}

/// Satu server MCP stdio (Fase 7): proses lokal yang di-spawn selen,
/// tool-nya muncul di registry sebagai `mcp__<name>__<tool>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerCfg {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env tambahan untuk proses server (opsional).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCfg {
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub model: String,
    /// Daftar model tersimpan untuk provider ini (diisi otomatis dari `/models`
    /// atau `/model add` / `/model set`), untuk pemilihan cepat lewat nomor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default)]
    pub api_key_env: String,
    /// API key opsional yang disimpan langsung lewat `/provider add` (fallback
    /// bila env var `api_key_env` belum diset). Disarankan tetap pakai env var
    /// bila file config ikut di-commit ke git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Harga opsional per 1M token (Fase 3). Bila kosong, biaya TIDAK ditebak —
    /// yang ditampilkan hanya jumlah token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_out: Option<f64>,
    /// Override manual kapasitas context window model untuk provider ini (opsional).
    /// Bila kosong, SelenCode otomatis mendeteksi dari API `/models` atau nama model AI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<usize>,
}

fn default_kind() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Review self-improvement otomatis pasca-giliran kompleks (ala Hermes Agent).
    #[serde(default = "default_auto_skills")]
    pub auto_skills: bool,
    /// Self-Correction Engine & Automatic Verification Gate setelah edit kode (Tahap 1).
    #[serde(default = "default_self_correct")]
    pub self_correct: bool,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_output_tokens: default_max_output_tokens(),
            approval: default_approval(),
            context_limit_tokens: default_context_limit_tokens(),
            max_parallel_subagents: default_max_parallel_subagents(),
            auto_skills: default_auto_skills(),
            self_correct: default_self_correct(),
        }
    }
}

fn default_auto_skills() -> bool {
    true
}

fn default_self_correct() -> bool {
    true
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Path config aktif yang sedang dipakai (atau lokasi global default untuk penyimpanan).
pub fn active_config_path() -> PathBuf {
    for p in candidate_paths() {
        if p.exists() {
            return p;
        }
    }
    if let Some(cfg) = dirs::config_dir() {
        return cfg.join("selencode").join("config.toml");
    }
    PathBuf::from("./selencode.toml")
}

/// Simpan config ke path aktif (membuat folder induk bila perlu).
pub fn save(cfg: &Config) -> Result<PathBuf> {
    let path = active_config_path();
    save_to(cfg, &path)?;
    Ok(path)
}

pub fn save_to(cfg: &Config, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("gagal membuat folder config {}", parent.display()))?;
        }
    }
    let text = toml::to_string_pretty(cfg).context("gagal memformat config TOML")?;
    std::fs::write(path, text)
        .with_context(|| format!("gagal menyimpan config {}", path.display()))?;
    #[cfg(unix)]
    if cfg.providers.values().any(|p| p.api_key.is_some()) || cfg.telegram.bot_token.is_some() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl ProviderCfg {
    /// Daftar model efektif (bernomor 1..=N): selalu memastikan `self.model`
    /// yang sedang aktif ada di daftar agar nomor selalu konsisten.
    pub fn effective_models(&self) -> Vec<String> {
        let mut list = self.models.clone();
        if !self.model.trim().is_empty() && !list.iter().any(|m| m == &self.model) {
            list.insert(0, self.model.clone());
        }
        list
    }

    /// Tambahkan satu model ke daftar tersimpan bila belum ada.
    pub fn push_model_unique(&mut self, model: &str) -> bool {
        let m = model.trim();
        if m.is_empty() || self.models.iter().any(|x| x == m) {
            return false;
        }
        self.models.push(m.to_string());
        true
    }
}

impl Config {
    /// Ambil (nama, cfg, api_key) untuk provider terpilih.
    /// Prioritas key: env var `api_key_env` -> field `api_key` di config.
    pub fn resolve_provider(&self, name: Option<&str>) -> Result<(String, ProviderCfg, String)> {
        let name = name.unwrap_or(&self.default_provider).to_string();
        let cfg = self
            .providers
            .get(&name)
            .ok_or_else(|| anyhow!("provider '{name}' tidak ada di config"))?
            .clone();
        let key = if !cfg.api_key_env.trim().is_empty() {
            std::env::var(&cfg.api_key_env).ok().filter(|v| !v.trim().is_empty())
        } else {
            None
        }
        .or_else(|| cfg.api_key.clone().filter(|v| !v.trim().is_empty()))
        .ok_or_else(|| {
            if cfg.api_key_env.trim().is_empty() {
                anyhow!(
                    "provider '{name}' belum punya API key. Atur lewat `/provider add` atau isi api_key_env di config."
                )
            } else {
                anyhow!(
                    "env var {} (api_key_env provider '{name}') belum di-set. Contoh: export {}=...",
                    cfg.api_key_env,
                    cfg.api_key_env
                )
            }
        })?;
        Ok((name, cfg, key))
    }

    /// Apakah API key untuk provider ini siap dipakai (dari env atau config)?
    pub fn is_key_ready(&self, name: &str) -> bool {
        self.resolve_provider(Some(name)).is_ok()
    }

    pub fn auto_approve(&self, yes_flag: bool) -> bool {
        yes_flag || self.agent.approval == "auto"
    }
}
