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
    /// Pengaturan Dynamic Model Routing, Fallback Provider, & Retry (Tahap 3).
    #[serde(default, skip_serializing_if = "RoutingCfg::is_empty")]
    pub routing: RoutingCfg,
    /// Pengaturan Security Sandbox v2 (`[sandbox]` di selencode.toml).
    #[serde(default, skip_serializing_if = "SandboxCfg::is_empty")]
    pub sandbox: SandboxCfg,
    /// Custom Hooks (`[[hooks]]` di selencode.toml) — `pre_tool` & `post_tool`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookCfg>,
}

/// Konfigurasi 1 Custom Hook (`[[hooks]]` di selencode.toml).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookCfg {
    /// Event pemicu: `"pre_tool"` atau `"post_tool"`.
    pub event: String,
    /// Pola nama tool: `"*"`, `"edit_file"`, `"edit_file,write_file"`, atau `"mcp__*"`.
    #[serde(default = "default_hook_matcher")]
    pub matcher: String,
    /// Perintah shell yang dijalankan di workspace.
    pub command: String,
    /// Batas waktu eksekusi hook dalam detik (default 15s).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// Khusus `pre_tool`: true = blokir eksekusi tool bila hook keluar non-zero.
    #[serde(default)]
    pub block_on_error: bool,
}

fn default_hook_matcher() -> String {
    "*".to_string()
}

fn default_hook_timeout() -> u64 {
    15
}

/// Konfigurasi Security Sandbox v2 (`[sandbox]` di selencode.toml).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxCfg {
    /// Mode sandbox: `"read-only"`, `"workspace-write"` (default), atau `"unrestricted"`.
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    /// Kebijakan isolasi OS: `"off"` (default), `"auto"` (`bwrap` di Linux bila tersedia),
    /// atau `"strict"` (fail-closed bila isolasi OS tidak tersedia).
    #[serde(default = "default_sandbox_isolate")]
    pub os_isolate: String,
    /// Saring environment variable sensitif (`*KEY*`, `*TOKEN*`, `*SECRET*`, `*PASSWORD*`)
    /// sebelum spawn subprocess shell/bg/verify (default: true).
    #[serde(default = "default_true")]
    pub scrub_env: bool,
    /// Izinkan akses jaringan di dalam sandbox `bwrap` (default: true).
    #[serde(default = "default_true")]
    pub allow_network: bool,
    /// Catat aksi berisiko ke audit log JSONL teredaksi (default: true).
    #[serde(default = "default_true")]
    pub audit_log: bool,
}

fn default_sandbox_mode() -> String {
    "workspace-write".to_string()
}

fn default_sandbox_isolate() -> String {
    "off".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for SandboxCfg {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            os_isolate: default_sandbox_isolate(),
            scrub_env: true,
            allow_network: true,
            audit_log: true,
        }
    }
}

impl SandboxCfg {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi Dynamic Model Routing (`[routing]` di selencode.toml, Tahap 3 & Fase 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingCfg {
    /// true = aktifkan pemilihan model dinamis berbasis jenis tugas (coding/debugging/reasoning/review/summary).
    /// false (default) = hormati pilihan model manual user, namun fallback + retry + metrik tetap aktif.
    #[serde(default)]
    pub auto: bool,
    /// Daftar nama provider cadangan (atau `"provider:model"`) berurutan saat provider utama gagal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
    /// Batas retry untuk error sementara (429/timeout/5xx), maks 3. Default: 1.
    #[serde(default = "default_routing_retries")]
    pub max_retries: u8,
    /// Aturan rute eksplisit per jenis tugas (`coding`, `debugging`, `reasoning`, `review`, `summary`)
    /// ke `"provider"` atau `"provider:model"`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_routes: HashMap<String, String>,
    /// Batas biaya maksimum per sesi dalam USD (opsional, hanya berlaku untuk rute yang punya price_in/price_out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

fn default_routing_retries() -> u8 {
    1
}

impl Default for RoutingCfg {
    fn default() -> Self {
        Self {
            auto: false,
            fallbacks: Vec::new(),
            max_retries: default_routing_retries(),
            task_routes: HashMap::new(),
            max_cost_usd: None,
        }
    }
}

impl RoutingCfg {
    pub fn is_empty(&self) -> bool {
        !self.auto
            && self.fallbacks.is_empty()
            && self.max_retries == default_routing_retries()
            && self.task_routes.is_empty()
            && self.max_cost_usd.is_none()
    }
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

/// Satu server MCP (stdio lokal ATAU Streamable HTTP/SSE remote):
/// tool-nya muncul di registry sebagai `mcp__<name>__<tool>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerCfg {
    pub name: String,
    /// Perintah executable untuk transport `stdio` (kosongkan jika memakai `url` HTTP/SSE).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env tambahan untuk proses server stdio (opsional).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// URL endpoint Streamable HTTP / JSON-RPC remote (opsional; jika diisi memakai transport HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Header HTTP opsional untuk transport remote (mis. `Authorization: "Bearer ${TOKEN}"`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// true = tool server ini berjalan TANPA approval. Default false:
    /// server eksternal tidak dikenal, jadi setiap panggilan ditanya dulu.
    #[serde(default)]
    pub trust: bool,
    /// Timeout per request JSON-RPC ke server ini (detik).
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for McpServerCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            trust: false,
            timeout_secs: default_mcp_timeout_secs(),
        }
    }
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
    #[cfg(windows)]
    let env_hint = r#"$env:SELENCODE_ROUTER_KEY="isi-api-key-kamu""#;
    #[cfg(not(windows))]
    let env_hint = r#"export SELENCODE_ROUTER_KEY="isi-api-key-kamu""#;
    let cfg_path = active_config_path();
    Err(anyhow!(
        "Config SelenCode tidak ditemukan. Jalankan `selen` untuk setup interaktif otomatis, atau buat file:\n  ./selencode.toml\n  {}\n\nContoh set API key di terminal:\n  {env_hint}",
        cfg_path.display()
    ))
}

/// Muat config bila sudah ada; bila file config belum ada, kembalikan `Config::empty()`
/// (provider kosong) agar TUI bisa langsung membuka Wizard Setup Provider tanpa crash!
/// Bila file config ADA tetapi rusak/tidak valid, tetap kembalikan `Err`.
pub fn load_or_empty() -> Result<(Config, bool)> {
    for p in candidate_paths() {
        if p.exists() {
            return Ok((load_from(&p)?, true));
        }
    }
    Ok((Config::empty(), false))
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
    /// Config awal kosong (belum ada provider) untuk onboarding interaktif pertama kali.
    pub fn empty() -> Self {
        Self {
            default_provider: String::new(),
            providers: HashMap::new(),
            agent: AgentCfg::default(),
            shell: ShellCfg::default(),
            mcp_servers: Vec::new(),
            telegram: TelegramCfg::default(),
            routing: RoutingCfg::default(),
            sandbox: SandboxCfg::default(),
            hooks: Vec::new(),
        }
    }

    /// Ambil (nama, cfg, api_key) untuk provider terpilih.
    /// Prioritas key: env var `api_key_env` -> field `api_key` di config.
    pub fn resolve_provider(&self, name: Option<&str>) -> Result<(String, ProviderCfg, String)> {
        if self.providers.is_empty() {
            return Err(anyhow!(
                "belum ada provider terpasang. Jalankan `/setup` untuk mengisi API provider & key."
            ));
        }
        let name = name
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if !self.default_provider.trim().is_empty()
                    && self.providers.contains_key(&self.default_provider)
                {
                    self.default_provider.clone()
                } else {
                    let mut keys: Vec<&String> = self.providers.keys().collect();
                    keys.sort();
                    keys.first().map(|s| (*s).clone()).unwrap_or_default()
                }
            });
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
                    "provider '{name}' belum punya API key. Jalankan `/setup` atau `/provider add` untuk mengisinya."
                )
            } else {
                #[cfg(windows)]
                let cmd_hint = format!(r#"$env:{}="isi-api-key""#, cfg.api_key_env);
                #[cfg(not(windows))]
                let cmd_hint = format!(r#"export {}="isi-api-key""#, cfg.api_key_env);
                anyhow!(
                    "env var {} (api_key_env provider '{name}') belum di-set. Contoh: {cmd_hint} — atau jalankan `/setup` untuk menyimpan API key langsung di config.",
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
