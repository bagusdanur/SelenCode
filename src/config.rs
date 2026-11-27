use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Nilai pagu langkah bawaan era lama (pra-v0.5.15) untuk kompatibilitas referensi.
pub const LEGACY_DEFAULT_MAX_STEPS: usize = 25;

/// Status sumber `max_steps` untuk pelaporan transparan di startup & `selen doctor` (Opsi A — ala Hermes).
/// Giliran utama pengguna selalu berjalan tanpa batas langkah (`0`); batas iterasi hanya dipakai
/// secara internal oleh subagent/orchestrator/cron latar agar tidak runaway.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MaxStepsSource {
    /// Bersih — tidak ada kunci `max_steps` aktif di config; giliran utama selalu tanpa batas langkah.
    #[default]
    DefaultUnbounded,
    /// Config lama masih memuat `max_steps = N` (`N > 0`) — sudah tidak dipakai dan diabaikan otomatis.
    DeprecatedIgnored {
        legacy_value: usize,
        config_path: String,
    },
}

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
    /// Konfigurasi Guard (dedup, loop guard, & rem Self-correction, `[guard]`).
    #[serde(default, skip_serializing_if = "GuardCfg::is_default")]
    pub guard: GuardCfg,
    /// Konfigurasi model aux murah untuk kompresi riwayat & background review (`[aux]`).
    #[serde(default, skip_serializing_if = "AuxCfg::is_empty")]
    pub aux: AuxCfg,
    /// Konfigurasi Background Self-Improvement Review (`[review]`, v0.5.1).
    #[serde(default, skip_serializing_if = "ReviewCfg::is_default")]
    pub review: ReviewCfg,
    /// Anggaran hasil tool: cap per tool, spillover ke file, & budget per turn (`[tool_budget]`).
    #[serde(default, skip_serializing_if = "ToolBudgetCfg::is_default")]
    pub tool_budget: ToolBudgetCfg,
    /// Pengaturan Skills System SelenCode (`[skills]` di selencode.toml).
    #[serde(default, skip_serializing_if = "SkillsCfg::is_default")]
    pub skills: SkillsCfg,
    /// Konfigurasi fallback model/provider global (`[fallback]`, v0.5.6 Bagian 1).
    #[serde(default, skip_serializing_if = "FallbackCfg::is_default")]
    pub fallback: FallbackCfg,
    /// Konfigurasi tool bertanya `ask_user` (`[ask]`, v0.5.6 Bagian 2).
    #[serde(default, skip_serializing_if = "AskCfg::is_default")]
    pub ask: AskCfg,
    /// Konfigurasi pagu memori persisten & profil pemilik (`[memory]`, v0.5.7 Perbaikan 5).
    #[serde(default, skip_serializing_if = "MemoryCfg::is_default")]
    pub memory: MemoryCfg,
    /// Konfigurasi peta folder kerja otomatis (`[context]`, v0.5.8 Perbaikan 1 Lapis A2).
    #[serde(default, skip_serializing_if = "ContextCfg::is_default")]
    pub context: ContextCfg,
    /// Konfigurasi tampilan aksi tool (`[display]`, v0.5.8 Perbaikan 3).
    #[serde(default, skip_serializing_if = "DisplayCfg::is_default")]
    pub display: DisplayCfg,
    /// Konfigurasi tenggat waktu panggilan provider (`[timeouts]`, v0.5.10 Perbaikan 1).
    #[serde(default, skip_serializing_if = "ProviderTimeoutsCfg::is_default")]
    pub timeouts: ProviderTimeoutsCfg,
    /// Sumber nilai efektif `agent.max_steps` (tidak diserialisasi ke TOML, v0.5.17).
    #[serde(skip)]
    pub max_steps_source: MaxStepsSource,
    /// Pemberitahuan startup penting dari loader config (mis. pembebasan pagu warisan `max_steps = 25`).
    #[serde(skip)]
    pub startup_notices: Vec<String>,
}

/// Konfigurasi tenggat waktu panggilan HTTP/streaming ke provider (`[timeouts]`, v0.5.10 Perbaikan 1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderTimeoutsCfg {
    /// Tenggat sambung TCP/TLS ke provider dalam detik (default: 20).
    #[serde(default = "default_provider_connect_secs")]
    pub connect_secs: u64,
    /// Tenggat menunggu header respons awal dari provider dalam detik (default: 60).
    #[serde(default = "default_provider_response_secs")]
    pub response_secs: u64,
    /// Tenggat ketidakaktifan streaming (tanpa byte/potongan baru) dalam detik (default: 180, sama dengan dasar Hermes).
    #[serde(default = "default_provider_stream_idle_secs")]
    pub stream_idle_secs: u64,
    /// Tenggat keras total 1 panggilan HTTP/stream ke provider dalam detik (default: 600).
    #[serde(default = "default_provider_total_secs")]
    pub total_secs: u64,
}

fn default_provider_connect_secs() -> u64 {
    20
}
fn default_provider_response_secs() -> u64 {
    60
}
fn default_provider_stream_idle_secs() -> u64 {
    180
}
fn default_provider_total_secs() -> u64 {
    600
}

impl Default for ProviderTimeoutsCfg {
    fn default() -> Self {
        Self {
            connect_secs: default_provider_connect_secs(),
            response_secs: default_provider_response_secs(),
            stream_idle_secs: default_provider_stream_idle_secs(),
            total_secs: default_provider_total_secs(),
        }
    }
}

impl ProviderTimeoutsCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn to_durations(&self) -> crate::providers::ProviderTimeouts {
        crate::providers::ProviderTimeouts {
            connect: std::time::Duration::from_secs(self.connect_secs.max(1)),
            response: std::time::Duration::from_secs(self.response_secs.max(1)),
            stream_idle: std::time::Duration::from_secs(self.stream_idle_secs.max(1)),
            total: std::time::Duration::from_secs(self.total_secs.max(1)),
        }
    }
}

/// Konfigurasi konteks harness (`[context]`, v0.5.8 Perbaikan 1 Lapis A2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextCfg {
    /// Sertakan peta folder kerja ringkas + simbol kunci di snapshot awal sesi (default: true).
    #[serde(default = "default_context_folder_map")]
    pub folder_map: bool,
    /// Pagu token maksimum peta folder kerja (default: 300 token).
    #[serde(default = "default_folder_map_max_tokens")]
    pub folder_map_max_tokens: usize,
}

fn default_context_folder_map() -> bool {
    true
}

fn default_folder_map_max_tokens() -> usize {
    300
}

impl Default for ContextCfg {
    fn default() -> Self {
        Self {
            folder_map: default_context_folder_map(),
            folder_map_max_tokens: default_folder_map_max_tokens(),
        }
    }
}

impl ContextCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi tampilan pratinjau aksi tool (`[display]`, v0.5.8 Perbaikan 3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayCfg {
    /// Panjang maksimum pratinjau argumen utama tool (default: 40 karakter, meniru Hermes).
    #[serde(default = "default_tool_preview_length")]
    pub tool_preview_length: usize,
}

fn default_tool_preview_length() -> usize {
    crate::tool_display::DEFAULT_TOOL_PREVIEW_LENGTH
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            tool_preview_length: default_tool_preview_length(),
        }
    }
}

impl DisplayCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi memori pengguna persisten (`[memory]`, v0.5.7 Perbaikan 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryCfg {
    /// Pagu karakter keras wadah `memory` (default: 2.200).
    #[serde(default = "default_max_memory_chars")]
    pub max_memory_chars: usize,
    /// Pagu karakter keras wadah `profile` (default: 1.375).
    #[serde(default = "default_max_profile_chars")]
    pub max_profile_chars: usize,
}

fn default_max_memory_chars() -> usize {
    crate::user_memory::DEFAULT_MAX_MEMORY_CHARS
}

fn default_max_profile_chars() -> usize {
    crate::user_memory::DEFAULT_MAX_PROFILE_CHARS
}

impl Default for MemoryCfg {
    fn default() -> Self {
        Self {
            max_memory_chars: default_max_memory_chars(),
            max_profile_chars: default_max_profile_chars(),
        }
    }
}

impl MemoryCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi fallback model/provider global (`[fallback]`, v0.5.6 Bagian 1).
/// Kosong secara default (mengisinya adalah keputusan pemilik per mesin).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FallbackCfg {
    /// Daftar pasangan `"provider:model"` atau `"model"` cadangan terurut (default: kosong).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Maksimum total model yang dicoba per giliran (termasuk model utama; default: 3).
    #[serde(default = "default_fallback_max_attempts")]
    pub max_attempts: usize,
}

fn default_fallback_max_attempts() -> usize {
    3
}

impl Default for FallbackCfg {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            max_attempts: default_fallback_max_attempts(),
        }
    }
}

impl FallbackCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi tool `ask_user` / clarify (`[ask]`, v0.5.6 Bagian 2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskCfg {
    /// Batas waktu tunggu jawaban pengguna dalam detik (default: 600 = 10 menit).
    #[serde(default = "default_ask_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_ask_timeout_secs() -> u64 {
    600
}

impl Default for AskCfg {
    fn default() -> Self {
        Self {
            timeout_secs: default_ask_timeout_secs(),
        }
    }
}

impl AskCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi Skills System SelenCode (`[skills]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillsCfg {
    /// Override batas token indeks dinamis awal sesi khusus saat banyak skill terpasang
    /// (default `None` = berbagi anggaran `tool_budget.dynamic_inject_max_tokens` yaitu 1.500 token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_max_tokens: Option<usize>,
    /// Daftar eksplisit nama skill yang dimuat otomatis di awal sesi (default kosong: TIDAK ada auto-load!).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_load: Vec<String>,
}

impl SkillsCfg {
    pub fn is_default(&self) -> bool {
        self.index_max_tokens.is_none() && self.auto_load.is_empty()
    }
}

/// Konfigurasi dedup panggilan tool, loop guard, checkpoint anggaran langkah,
/// & batas putaran gerbang verifikasi/Self-correction (`[guard]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuardCfg {
    /// Aktifkan dedup + loop guard + checkpoint langkah + gerbang verifikasi (set `false` untuk debug).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Batas maksimum putaran gerbang verifikasi/refleksi per giliran (default: 3, pola Aider `max_reflections`).
    #[serde(default = "default_max_self_correct_rounds")]
    pub max_self_correct_rounds: usize,
    /// Aktifkan checkpoint kesadaran anggaran langkah & pemicu konvergensi belum-ada-edit (default: true, v0.5.14).
    #[serde(default = "default_true")]
    pub budget_warning_enabled: bool,
    /// Rasio pemakaian `max_steps` saat checkpoint satu-kali disuntikkan ke model (default: 0.7, rentang 0.1..=0.95).
    #[serde(default = "default_budget_warning_ratio")]
    pub budget_warning_ratio: f64,
    /// Ambang langkah tanpa satu pun mutasi berkas di mode tanpa pagu (`max_steps = 0`) sebelum
    /// menyuntikkan satu notice konvergensi keras (default: 20 langkah, v0.5.15).
    #[serde(default = "default_no_edit_convergence_steps")]
    pub no_edit_convergence_steps: usize,
}

fn default_max_self_correct_rounds() -> usize {
    3
}

fn default_budget_warning_ratio() -> f64 {
    0.7
}

fn default_no_edit_convergence_steps() -> usize {
    20
}

impl Default for GuardCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_self_correct_rounds: default_max_self_correct_rounds(),
            budget_warning_enabled: true,
            budget_warning_ratio: default_budget_warning_ratio(),
            no_edit_convergence_steps: default_no_edit_convergence_steps(),
        }
    }
}

impl GuardCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Konfigurasi model tambahan murah untuk meringkas konteks & background review (`[aux]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuxCfg {
    /// Nama model termurah untuk meringkas riwayat (kosong = pakai model utama saat kompresi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_model: Option<String>,
    /// Nama model opsional untuk Background Self-Improvement Review (v0.5.1).
    /// Kosong = ikut `compression_model`; bila keduanya kosong, background review dilewati.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
}

impl AuxCfg {
    pub fn is_empty(&self) -> bool {
        self.compression_model
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
            && self
                .review_model
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
    }

    /// Model efektif untuk background review: `review_model` -> `compression_model` -> `None` (lewati).
    pub fn effective_review_model(&self) -> Option<&str> {
        self.review_model
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.compression_model
                    .as_deref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
            })
    }
}

/// Konfigurasi Background Self-Improvement Review (`[review]`, v0.5.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewCfg {
    /// Aktifkan background self-improvement review (default: true; tetap butuh `[aux]` model terisi).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Batas anggaran total token per 1 kali jalan background review (default konservatif: 6.000).
    #[serde(default = "default_review_max_tokens")]
    pub max_tokens: u64,
    /// Cooldown minimum per sesi dalam menit (default: 30 menit).
    #[serde(default = "default_review_cooldown_mins")]
    pub cooldown_mins: u64,
    /// Picu review berkala setiap N giliran tuntas dalam sesi panjang (default: 15).
    #[serde(default = "default_review_turns_interval")]
    pub turns_interval: usize,
    /// Jumlah panggilan tool minimum agar sesi dianggap substantif (default: 5).
    #[serde(default = "default_review_min_tool_calls")]
    pub min_tool_calls: usize,
}

fn default_review_max_tokens() -> u64 {
    6_000
}
fn default_review_cooldown_mins() -> u64 {
    30
}
fn default_review_turns_interval() -> usize {
    15
}
fn default_review_min_tool_calls() -> usize {
    5
}

impl Default for ReviewCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: default_review_max_tokens(),
            cooldown_mins: default_review_cooldown_mins(),
            turns_interval: default_review_turns_interval(),
            min_tool_calls: default_review_min_tool_calls(),
        }
    }
}

impl ReviewCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Anggaran hasil tool per panggilan & agregat per turn (`[tool_budget]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolBudgetCfg {
    /// Batas karakter output `run_shell` (40% kepala + 60% ekor, default 12.000).
    #[serde(default = "default_shell_cap_chars")]
    pub shell_cap_chars: usize,
    /// Ambang spillover hasil tunggal ke file arsip kerja sesi (default 50.000).
    #[serde(default = "default_spillover_chars")]
    pub spillover_chars: usize,
    /// Budget agregat hasil tool per turn sebelum hasil terbesar di-spill duluan (default 60.000).
    #[serde(default = "default_turn_budget_chars")]
    pub turn_budget_chars: usize,
    /// Batas maksimum estimasi token injeksi dinamis awal sesi (default 1.500).
    #[serde(default = "default_dynamic_inject_max_tokens")]
    pub dynamic_inject_max_tokens: usize,
    /// Batas karakter praktis per panggilan `read_file` sebelum dipotong di akhir baris utuh + petunjuk `start`/`end`
    /// (diporting dari Hermes `tools/file_tools.py`, default 16.000 char; 0 = batas 100KB penuh).
    #[serde(default = "default_file_read_max_chars")]
    pub file_read_max_chars: usize,
    /// Batas baris bawaan `read_file` saat model tidak menentukan `start`/`end` (default 250 baris; 0 = 2000).
    #[serde(default = "default_file_read_default_lines")]
    pub file_read_default_lines: usize,
    /// Ambang token pemicu pemangkasan proaktif hasil tool/argumen lama tanpa menunggu 80% jendela 1M
    /// (diporting dari Hermes `agent/context_compressor.py`, default 18.000 token; 0 = matikan).
    #[serde(default = "default_proactive_prune_tokens")]
    pub proactive_prune_tokens: usize,
    /// Penghematan token minimum agar proactive prune di-commit (mencegah pemecahan prompt cache untuk
    /// penghematan kecil — diporting dari Hermes `proactive_prune_min_reclaim_tokens`, default 2.000 token).
    #[serde(default = "default_proactive_prune_min_reclaim_tokens")]
    pub proactive_prune_min_reclaim_tokens: usize,
}

fn default_shell_cap_chars() -> usize {
    12_000
}
fn default_spillover_chars() -> usize {
    50_000
}
fn default_turn_budget_chars() -> usize {
    60_000
}
fn default_dynamic_inject_max_tokens() -> usize {
    1_500
}
fn default_file_read_max_chars() -> usize {
    crate::tools::file::DEFAULT_FILE_READ_MAX_CHARS
}
fn default_file_read_default_lines() -> usize {
    crate::tools::file::DEFAULT_FILE_READ_LINES
}
fn default_proactive_prune_tokens() -> usize {
    96_000
}
fn default_proactive_prune_min_reclaim_tokens() -> usize {
    4_096
}

impl Default for ToolBudgetCfg {
    fn default() -> Self {
        Self {
            shell_cap_chars: default_shell_cap_chars(),
            spillover_chars: default_spillover_chars(),
            turn_budget_chars: default_turn_budget_chars(),
            dynamic_inject_max_tokens: default_dynamic_inject_max_tokens(),
            file_read_max_chars: default_file_read_max_chars(),
            file_read_default_lines: default_file_read_default_lines(),
            proactive_prune_tokens: default_proactive_prune_tokens(),
            proactive_prune_min_reclaim_tokens: default_proactive_prune_min_reclaim_tokens(),
        }
    }
}

impl ToolBudgetCfg {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Batas waktu tunggu kartu persetujuan Telegram dalam menit (default: 10; 0 = tanpa batas).
    #[serde(default = "default_tg_approval_timeout_mins")]
    pub approval_timeout_mins: u64,
    /// Batas waktu keras 1 giliran agent di gateway Telegram dalam detik (default: 900 = 15 menit, v0.5.10).
    #[serde(default = "default_tg_turn_timeout_secs")]
    pub turn_timeout_secs: u64,
    /// Ambang diam tanpa kemajuan sebelum 1 peringatan macet dikirim ke pengguna (default: 180 detik, v0.5.10).
    #[serde(default = "default_tg_stall_notice_secs")]
    pub stall_notice_secs: u64,
    /// Ambang keras kegagalan/kemacetan polling Telegram sebelum gateway keluar non-nol untuk restart supervisor (default: 300 detik, v0.5.10).
    #[serde(default = "default_tg_poll_watchdog_secs")]
    pub poll_watchdog_secs: u64,
}

fn default_tg_approval_timeout_mins() -> u64 {
    10
}
fn default_tg_turn_timeout_secs() -> u64 {
    900
}
fn default_tg_stall_notice_secs() -> u64 {
    180
}
fn default_tg_poll_watchdog_secs() -> u64 {
    300
}

impl Default for TelegramCfg {
    fn default() -> Self {
        Self {
            bot_token_env: String::new(),
            bot_token: None,
            allowed_users: Vec::new(),
            approval: String::new(),
            approval_timeout_mins: default_tg_approval_timeout_mins(),
            turn_timeout_secs: default_tg_turn_timeout_secs(),
            stall_notice_secs: default_tg_stall_notice_secs(),
            poll_watchdog_secs: default_tg_poll_watchdog_secs(),
        }
    }
}

impl TelegramCfg {
    pub fn is_empty(&self) -> bool {
        self.bot_token_env.trim().is_empty()
            && self.bot_token.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true)
            && self.allowed_users.is_empty()
            && self.approval.trim().is_empty()
            && self.approval_timeout_mins == default_tg_approval_timeout_mins()
            && self.turn_timeout_secs == default_tg_turn_timeout_secs()
            && self.stall_notice_secs == default_tg_stall_notice_secs()
            && self.poll_watchdog_secs == default_tg_poll_watchdog_secs()
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
    /// Daftar model cadangan terurut saat model utama gagal karena 429/5xx/timeout/401/403/404 (v0.5.6).
    /// Kosong secara default = tanpa fallback otomatis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
}

fn default_kind() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCfg {
    /// Bidang warisan (deprecated): tidak pernah membatasi giliran utama lagi dan tidak ditulis saat save.
    #[serde(default = "default_max_steps", skip_serializing)]
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

/// Bawaan tanpa batas langkah ala Hermes (`0` = tanpa batas; angka > 0 = pagu keras, v0.5.15).
fn default_max_steps() -> usize {
    0
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

/// Lokasi config global lintas-folder (Windows: `%APPDATA%\selencode\config.toml`,
/// Linux/Termux: `~/.config/selencode/config.toml`).
pub fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|cfg| cfg.join("selencode").join("config.toml"))
}

/// Lokasi config lokal di folder proyek saat ini (`./selencode.toml`).
pub fn local_config_path() -> PathBuf {
    PathBuf::from("./selencode.toml")
}

/// Urutan pencarian config: ./selencode.toml dan config global lintas-folder.
pub fn candidate_paths() -> Vec<PathBuf> {
    let mut v = vec![local_config_path()];
    if let Some(g) = global_config_path() {
        v.push(g);
    }
    v
}

/// Gabungkan 2 nilai TOML (`base` = global, `overlay` = lokal `./selencode.toml`) secara rekursif:
/// - Key di `overlay` menimpa key yang sama di `base`, tetapi key milik global yang TIDAK ada di
///   `overlay` (mis. `[telegram]`, `[providers.*]` lain, `api_key`) tetap dipertahankan!
/// - Khusus `telegram.allowed_users`, kedua daftar digabung tanpa duplikat.
pub fn merge_toml_values(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base_tbl), toml::Value::Table(over_tbl)) => {
            for (k, v) in over_tbl {
                if k == "allowed_users" {
                    if let (Some(toml::Value::Array(mut b_arr)), toml::Value::Array(o_arr)) =
                        (base_tbl.remove(&k), v.clone())
                    {
                        for item in o_arr {
                            if !b_arr.contains(&item) {
                                b_arr.push(item);
                            }
                        }
                        base_tbl.insert(k, toml::Value::Array(b_arr));
                        continue;
                    }
                }
                match base_tbl.remove(&k) {
                    Some(b_val) => {
                        base_tbl.insert(k, merge_toml_values(b_val, v));
                    }
                    None => {
                        base_tbl.insert(k, v);
                    }
                }
            }
            toml::Value::Table(base_tbl)
        }
        (_, over) => over,
    }
}

fn same_canonical_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Peringatan kunci config tak dikenal / salah tempat (Perbaikan 3 v0.5.11).
/// Bersifat peringatan (tidak pernah memblokir start), namun selalu terlihat di stderr & `selen doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub key_path: String,
    pub message: String,
}

fn check_allowed_keys(
    section_label: &str,
    tbl: &toml::map::Map<String, toml::Value>,
    allowed: &[&str],
    out: &mut Vec<ConfigWarning>,
) {
    for key in tbl.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        let key_path = if section_label.is_empty() {
            key.clone()
        } else {
            format!("{section_label}.{key}")
        };
        let message = if section_label.starts_with("providers.") && key == "timeouts" {
            format!(
                "`{key_path}`: `timeouts` seharusnya top-level (`[timeouts]`), bukan di bawah provider — diabaikan"
            )
        } else if section_label.starts_with("providers.")
            && matches!(
                key.as_str(),
                "turn_timeout_secs" | "stall_notice_secs" | "poll_watchdog_secs"
            )
        {
            format!(
                "`{key_path}`: seharusnya berada di bawah `[telegram]`, bukan di bawah provider — diabaikan"
            )
        } else if section_label.is_empty()
            && matches!(
                key.as_str(),
                "connect_secs" | "response_secs" | "stream_idle_secs" | "total_secs"
            )
        {
            format!(
                "`{key_path}`: seharusnya berada di bawah seksi `[timeouts]`, bukan langsung di top-level — diabaikan"
            )
        } else if section_label.is_empty()
            && matches!(
                key.as_str(),
                "turn_timeout_secs" | "stall_notice_secs" | "poll_watchdog_secs"
            )
        {
            format!(
                "`{key_path}`: seharusnya berada di bawah seksi `[telegram]`, bukan langsung di top-level — diabaikan"
            )
        } else {
            format!("`{key_path}`: kunci config tidak dikenal — diabaikan")
        };
        out.push(ConfigWarning { key_path, message });
    }
}

/// Periksa seluruh kunci pada `toml::Value` config terhadap skema resmi SelenCode (Perbaikan 3 v0.5.11).
pub fn validate_toml_keys(val: &toml::Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    let Some(root) = val.as_table() else {
        return warnings;
    };

    const TOP_KEYS: &[&str] = &[
        "default_provider",
        "providers",
        "agent",
        "shell",
        "mcp_servers",
        "telegram",
        "routing",
        "sandbox",
        "hooks",
        "guard",
        "aux",
        "review",
        "tool_budget",
        "skills",
        "fallback",
        "ask",
        "memory",
        "context",
        "display",
        "timeouts",
    ];
    check_allowed_keys("", root, TOP_KEYS, &mut warnings);

    if let Some(provs) = root.get("providers").and_then(|v| v.as_table()) {
        const PROV_KEYS: &[&str] = &[
            "kind",
            "base_url",
            "model",
            "models",
            "api_key_env",
            "api_key",
            "price_in",
            "price_out",
            "context_limit",
            "fallback",
        ];
        for (pname, pval) in provs {
            if let Some(ptbl) = pval.as_table() {
                check_allowed_keys(&format!("providers.{pname}"), ptbl, PROV_KEYS, &mut warnings);
            }
        }
    }

    let sections: &[(&str, &[&str])] = &[
        (
            "timeouts",
            &["connect_secs", "response_secs", "stream_idle_secs", "total_secs"],
        ),
        (
            "telegram",
            &[
                "bot_token_env",
                "bot_token",
                "allowed_users",
                "approval",
                "approval_timeout_mins",
                "turn_timeout_secs",
                "stall_notice_secs",
                "poll_watchdog_secs",
            ],
        ),
        (
            "agent",
            &[
                "max_steps",
                "max_output_tokens",
                "approval",
                "context_limit_tokens",
                "max_parallel_subagents",
                "auto_skills",
                "self_correct",
            ],
        ),
        ("shell", &["allowlist", "timeout_secs"]),
        (
            "routing",
            &["auto", "fallbacks", "max_retries", "task_routes", "max_cost_usd"],
        ),
        (
            "sandbox",
            &["mode", "os_isolate", "scrub_env", "allow_network", "audit_log"],
        ),
        (
            "guard",
            &[
                "enabled",
                "max_self_correct_rounds",
                "budget_warning_enabled",
                "budget_warning_ratio",
                "no_edit_convergence_steps",
            ],
        ),
        ("aux", &["compression_model", "review_model"]),
        (
            "review",
            &[
                "enabled",
                "max_tokens",
                "cooldown_mins",
                "turns_interval",
                "min_tool_calls",
            ],
        ),
        (
            "tool_budget",
            &[
                "shell_cap_chars",
                "spillover_chars",
                "turn_budget_chars",
                "dynamic_inject_max_tokens",
                "file_read_max_chars",
                "file_read_default_lines",
                "proactive_prune_tokens",
                "proactive_prune_min_reclaim_tokens",
            ],
        ),
        ("skills", &["index_max_tokens", "auto_load"]),
        ("fallback", &["models", "max_attempts"]),
        ("ask", &["timeout_secs"]),
        ("memory", &["max_memory_chars", "max_profile_chars"]),
        ("context", &["folder_map", "folder_map_max_tokens"]),
        ("display", &["tool_preview_length"]),
    ];
    for (sec, keys) in sections {
        if let Some(tbl) = root.get(*sec).and_then(|v| v.as_table()) {
            check_allowed_keys(sec, tbl, keys, &mut warnings);
        }
    }

    if let Some(arr) = root.get("mcp_servers").and_then(|v| v.as_array()) {
        const MCP_KEYS: &[&str] = &[
            "name",
            "command",
            "args",
            "env",
            "url",
            "headers",
            "trust",
            "timeout_secs",
        ];
        for (idx, item) in arr.iter().enumerate() {
            if let Some(tbl) = item.as_table() {
                check_allowed_keys(&format!("mcp_servers[{idx}]"), tbl, MCP_KEYS, &mut warnings);
            }
        }
    }
    if let Some(arr) = root.get("hooks").and_then(|v| v.as_array()) {
        const HOOK_KEYS: &[&str] = &[
            "event",
            "matcher",
            "command",
            "timeout_secs",
            "block_on_error",
        ];
        for (idx, item) in arr.iter().enumerate() {
            if let Some(tbl) = item.as_table() {
                check_allowed_keys(&format!("hooks[{idx}]"), tbl, HOOK_KEYS, &mut warnings);
            }
        }
    }

    warnings.sort_by(|a, b| a.key_path.cmp(&b.key_path));
    warnings
}

/// Validasi kunci TOML dari string mentah config (dipakai juga oleh unit test & `selen doctor`).
pub fn validate_config_str(text: &str) -> Vec<ConfigWarning> {
    toml::from_str::<toml::Value>(text)
        .map(|v| validate_toml_keys(&v))
        .unwrap_or_default()
}

/// Cetak peringatan kunci config ke stderr TEPAT 1x per kunci per proses (anti-spam saat config dimuat ulang di loop).
fn emit_config_warnings_once(source_label: &str, warnings: &[ConfigWarning]) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut guard) = seen.lock() else {
        return;
    };
    for w in warnings {
        let dedup_key = format!("{source_label}::{}", w.key_path);
        if guard.insert(dedup_key) {
            eprintln!("[config peringatan] ({source_label}) {}", w.message);
        }
    }
}

/// Kumpulkan seluruh peringatan kunci tak dikenal/salah tempat dari file config aktif (`./selencode.toml` & global).
pub fn collect_active_config_warnings() -> Vec<(PathBuf, ConfigWarning)> {
    let mut out = Vec::new();
    let mut visited: Vec<PathBuf> = Vec::new();
    for p in candidate_paths() {
        if !p.exists() {
            continue;
        }
        if visited.iter().any(|v| same_canonical_path(v, &p)) {
            continue;
        }
        visited.push(p.clone());
        if let Ok(txt) = std::fs::read_to_string(&p) {
            for w in validate_config_str(&txt) {
                out.push((p.clone(), w));
            }
        }
    }
    out
}

/// Ambil nilai eksplisit `[agent] max_steps` dari pohon TOML mentah bila memang tertulis di berkas.
fn extract_explicit_max_steps(val: &toml::Value) -> Option<usize> {
    val.as_table()?
        .get("agent")?
        .as_table()?
        .get("max_steps")?
        .as_integer()
        .filter(|&n| n >= 0)
        .map(|n| n as usize)
}

/// Cetak pemberitahuan pembebasan pagu warisan `max_steps = 25` ke stderr tepat 1x per path per proses.
fn emit_legacy_max_steps_notice_once(path_label: &str, notice: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut guard) = seen.lock() {
        if guard.insert(path_label.to_string()) {
            eprintln!("[selen] {notice}");
        }
    }
}

/// Terapkan kebijakan tanpa `max_steps` ala Hermes (Opsi A):
/// - Giliran utama pengguna SELALU tanpa batas langkah (`0`); tidak ada pagu langkah dari config user.
/// - Jika config lama masih memuat `max_steps = N` (`N > 0`, mis. `25`, `40`, dll.), nilai itu
///   diabaikan otomatis dengan 1 pemberitahuan terlihat dan dicatat di `selen doctor`.
/// - Batas langkah hanya dipertahankan sebagai pengaman internal pada subagent/orchestrator/cron latar.
fn apply_max_steps_policy(
    cfg: &mut Config,
    explicit: Option<(usize, String)>,
    emit_stderr: bool,
) {
    match explicit {
        None | Some((0, _)) => {
            cfg.agent.max_steps = 0;
            cfg.max_steps_source = MaxStepsSource::DefaultUnbounded;
        }
        Some((legacy_value, config_path)) => {
            cfg.agent.max_steps = 0;
            cfg.max_steps_source = MaxStepsSource::DeprecatedIgnored {
                legacy_value,
                config_path: config_path.clone(),
            };
            let notice = format!(
                "Kunci [agent] max_steps = {legacy_value} di {config_path} sudah tidak dipakai dan diabaikan — \
                 SelenCode selalu berjalan tanpa batas langkah ala Hermes (rem memakai LoopGuard, konvergensi, & timeout)."
            );
            cfg.startup_notices.push(notice.clone());
            if emit_stderr {
                emit_legacy_max_steps_notice_once(&config_path, &notice);
            }
        }
    }
}

/// Parse string TOML config sekaligus menerapkan kebijakan pembebasan `max_steps = 25` warisan
/// (dipakai oleh loader & unit test agar dapat diuji deterministik tanpa menyentuh file global).
pub fn parse_config_str_with_policy(text: &str, source_label: &str) -> Result<Config> {
    let val: toml::Value =
        toml::from_str(text).with_context(|| format!("config {source_label} tidak valid"))?;
    let explicit_steps = extract_explicit_max_steps(&val).map(|v| (v, source_label.to_string()));
    let mut cfg: Config = val
        .try_into()
        .with_context(|| format!("config {source_label} tidak valid"))?;
    apply_max_steps_policy(&mut cfg, explicit_steps, false);
    Ok(cfg)
}

fn load_merged_config() -> Result<Option<Config>> {
    let local_p = local_config_path();
    let global_p = global_config_path();
    let has_local = local_p.exists();
    let has_global = global_p.as_ref().map(|p| p.exists()).unwrap_or(false);

    match (has_global, has_local) {
        (true, true) => {
            let gp = global_p.unwrap();
            if same_canonical_path(&gp, &local_p) {
                return Ok(Some(load_from(&gp)?));
            }
            let g_txt = std::fs::read_to_string(&gp)
                .with_context(|| format!("gagal membaca config {}", gp.display()))?;
            let l_txt = std::fs::read_to_string(&local_p)
                .with_context(|| format!("gagal membaca config {}", local_p.display()))?;
            let g_val: toml::Value = toml::from_str(&g_txt)
                .with_context(|| format!("config {} tidak valid", gp.display()))?;
            let l_val: toml::Value = toml::from_str(&l_txt)
                .with_context(|| format!("config {} tidak valid", local_p.display()))?;
            emit_config_warnings_once(&gp.display().to_string(), &validate_toml_keys(&g_val));
            emit_config_warnings_once(&local_p.display().to_string(), &validate_toml_keys(&l_val));
            let explicit_steps = extract_explicit_max_steps(&l_val)
                .map(|v| (v, local_p.display().to_string()))
                .or_else(|| {
                    extract_explicit_max_steps(&g_val).map(|v| (v, gp.display().to_string()))
                });
            let merged = merge_toml_values(g_val, l_val);
            let mut cfg: Config = merged.try_into().with_context(|| {
                format!(
                    "gagal menggabungkan config {} + {}",
                    gp.display(),
                    local_p.display()
                )
            })?;
            apply_max_steps_policy(&mut cfg, explicit_steps, true);
            Ok(Some(cfg))
        }
        (true, false) => Ok(Some(load_from(&global_p.unwrap())?)),
        (false, true) => Ok(Some(load_from(&local_p)?)),
        (false, false) => Ok(None),
    }
}

pub fn load() -> Result<Config> {
    if let Some(cfg) = load_merged_config()? {
        return Ok(cfg);
    }
    #[cfg(windows)]
    let env_hint = r#"$env:SELENCODE_ROUTER_KEY="isi-api-key-kamu""#;
    #[cfg(not(windows))]
    let env_hint = r#"export SELENCODE_ROUTER_KEY="isi-api-key-kamu""#;
    let cfg_path = active_config_path();
    Err(anyhow!(
        "Config SelenCode tidak ditemukan. Jalankan `selen` untuk setup interaktif otomatis, atau buat file:\n  {}\n  ./selencode.toml\n\nContoh set API key di terminal:\n  {env_hint}",
        cfg_path.display()
    ))
}

/// Muat config bila sudah ada; bila file config belum ada, kembalikan `Config::empty()`
/// (provider kosong) agar TUI bisa langsung membuka Wizard Setup Provider tanpa crash!
/// Bila file config ADA tetapi rusak/tidak valid, tetap kembalikan `Err`.
pub fn load_or_empty() -> Result<(Config, bool)> {
    match load_merged_config()? {
        Some(cfg) => Ok((cfg, true)),
        None => Ok((Config::empty(), false)),
    }
}

pub fn load_from(path: &std::path::Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("gagal membaca config {}", path.display()))?;
    let val: toml::Value =
        toml::from_str(&text).with_context(|| format!("config {} tidak valid", path.display()))?;
    let path_label = path.display().to_string();
    emit_config_warnings_once(&path_label, &validate_toml_keys(&val));
    let explicit_steps = extract_explicit_max_steps(&val).map(|v| (v, path_label));
    let mut cfg: Config = val
        .try_into()
        .with_context(|| format!("config {} tidak valid", path.display()))?;
    apply_max_steps_policy(&mut cfg, explicit_steps, true);
    Ok(cfg)
}

/// Path config utama (mengutamakan lokasi global agar setelan router AI & Telegram
/// selalu persisten lintas folder di Windows, Linux, maupun Termux).
pub fn active_config_path() -> PathBuf {
    if let Some(g) = global_config_path() {
        if g.exists() {
            return g;
        }
    }
    let local = local_config_path();
    if local.exists() {
        return local;
    }
    global_config_path().unwrap_or(local)
}

/// Simpan config secara persisten ke lokasi global (`%APPDATA%\selencode\config.toml` di Windows /
/// `~/.config/selencode/config.toml` di Unix) agar tidak hilang saat berpindah folder.
/// Bila `./selencode.toml` lokal juga ada di folder saat ini, ikut disinkronkan agar tidak menimpa
/// pengaturan router AI / model / Telegram terbaru.
pub fn save(cfg: &Config) -> Result<PathBuf> {
    let local = local_config_path();
    if let Some(global) = global_config_path() {
        save_to(cfg, &global)?;
        if local.exists() && !same_canonical_path(&global, &local) {
            let _ = save_to(cfg, &local);
        }
        return Ok(global);
    }
    save_to(cfg, &local)?;
    Ok(local)
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
            guard: GuardCfg::default(),
            aux: AuxCfg::default(),
            review: ReviewCfg::default(),
            tool_budget: ToolBudgetCfg::default(),
            skills: SkillsCfg::default(),
            fallback: FallbackCfg::default(),
            ask: AskCfg::default(),
            memory: MemoryCfg::default(),
            context: ContextCfg::default(),
            display: DisplayCfg::default(),
            timeouts: ProviderTimeoutsCfg::default(),
            max_steps_source: MaxStepsSource::DefaultUnbounded,
            startup_notices: Vec::new(),
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
