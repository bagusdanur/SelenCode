//! State TUI + reducer event (Fase 4). Murni logika — tanpa terminal —
//! agar bisa dites: UI nyata (mod.rs) hanya menggambar state ini dan
//! meneruskan input keyboard ke sini.

use crate::agent::context::{calc_context_pct, infer_model_context_limit};
use crate::agent::ui::{ModelPickerData, ResumePickerData, ResumePickerItem};
use crate::agent::UiEvent;
use crate::security::{tool_category_from_title, ApprovalPolicy, ApprovalRequest};
use crate::tui::moonlit::{self, WelcomeInfo};
use ratatui::style::Style;
use ratatui::text::Line;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tui_textarea::TextArea;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Tool,
    Info,
    /// Kartu output perintah `/` berwarna & terstruktur (bukan abu-abu polos).
    Menu,
    Error,
    /// Blok checklist update_plan (Fase 6).
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Pending,
    Ok,
    Err,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
    /// Khusus baris Tool: nama tool untuk mencocokkan ToolStart/ToolDone.
    pub tool_name: Option<String>,
    pub tool_state: Option<ToolState>,
    /// Cache layout per-pesan (Bagian A — ringan di laptop kentang):
    /// mencegah re-parsing Markdown & wrapping ulang seluruh transkrip tiap frame/ketikan!
    pub cached_width: u16,
    pub cached_details: bool,
    pub cached_text_len: usize,
    pub cached_tool_state: Option<ToolState>,
    pub cached_has_leading_blank: bool,
    pub cached_prev_assistant: bool,
    pub cached_lines: Vec<Line<'static>>,
    pub cached_wrapped_rows: usize,
}

impl TranscriptLine {
    pub fn simple(kind: LineKind, text: String) -> Self {
        Self {
            kind,
            text,
            tool_name: None,
            tool_state: None,
            cached_width: 0,
            cached_details: false,
            cached_text_len: usize::MAX,
            cached_tool_state: None,
            cached_has_leading_blank: false,
            cached_prev_assistant: false,
            cached_lines: Vec::new(),
            cached_wrapped_rows: 0,
        }
    }

    pub fn with_tool(text: String, tool_name: Option<String>, tool_state: Option<ToolState>) -> Self {
        let mut s = Self::simple(LineKind::Tool, text);
        s.tool_name = tool_name;
        s.tool_state = tool_state;
        s
    }
}

/// Permintaan approval yang sedang ditampilkan sebagai popup.
pub struct PendingApproval {
    pub title: String,
    pub detail: String,
    pub reply: Option<tokio::sync::oneshot::Sender<bool>>,
}

/// Preset pilihan API saat onboarding / `/setup` (awal kosong -> pilih API compatible -> isi URL & key).
#[derive(Debug, Clone, Copy)]
pub struct SetupPreset {
    pub label: &'static str,
    pub desc: &'static str,
    pub default_name: &'static str,
    pub kind: &'static str,
    pub default_url: &'static str,
    pub default_model: &'static str,
    pub needs_key: bool,
}

pub const SETUP_PRESETS: &[SetupPreset] = &[
    SetupPreset {
        label: "OpenAI-Compatible Custom",
        desc: "isi Base URL sendiri (9Router, vLLM, LiteLLM, proxy custom)",
        default_name: "custom",
        kind: "openai",
        default_url: "",
        default_model: "",
        needs_key: true,
    },
    SetupPreset {
        label: "OpenRouter",
        desc: "akses banyak model (Claude, GPT, Gemini, DeepSeek)",
        default_name: "openrouter",
        kind: "openai",
        default_url: "https://openrouter.ai/api/v1",
        default_model: "anthropic/claude-sonnet-4",
        needs_key: true,
    },
    SetupPreset {
        label: "OpenAI Resmi",
        desc: "GPT-4.1, GPT-5, o3, GPT-4o",
        default_name: "openai",
        kind: "openai",
        default_url: "https://api.openai.com/v1",
        default_model: "gpt-4.1",
        needs_key: true,
    },
    SetupPreset {
        label: "Anthropic Claude",
        desc: "Claude Sonnet 4, Opus 4, Haiku (API resmi)",
        default_name: "anthropic",
        kind: "anthropic",
        default_url: "",
        default_model: "claude-sonnet-4-20250514",
        needs_key: true,
    },
    SetupPreset {
        label: "Google Gemini",
        desc: "Gemini 2.5 Pro / Flash (context window 1M)",
        default_name: "gemini",
        kind: "gemini",
        default_url: "",
        default_model: "gemini-2.5-flash",
        needs_key: true,
    },
    SetupPreset {
        label: "Groq",
        desc: "inferensi ultra cepat (Llama 3.3, Qwen)",
        default_name: "groq",
        kind: "openai",
        default_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
        needs_key: true,
    },
    SetupPreset {
        label: "DeepSeek",
        desc: "DeepSeek Chat & R1",
        default_name: "deepseek",
        kind: "openai",
        default_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-chat",
        needs_key: true,
    },
    SetupPreset {
        label: "Ollama Lokal",
        desc: "jalan lokal di PC/HP tanpa API key",
        default_name: "ollama",
        kind: "openai",
        default_url: "http://localhost:11434/v1",
        default_model: "qwen2.5-coder:7b",
        needs_key: false,
    },
];

/// Langkah-langkah pada Wizard Setup Provider interaktif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    ChooseApi,
    ProviderName,
    BaseUrl,
    ApiKey,
    ModelId,
}

/// Hasil akhir Wizard Setup Provider yang siap disimpan ke config & langsung diaktifkan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupResult {
    pub name: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub api_key_or_env: String,
    pub model: String,
}

/// State modal Wizard Setup Provider interaktif (`/setup` atau otomatis saat pertama buka).
#[derive(Debug, Clone)]
pub struct SetupWizardState {
    pub step: SetupStep,
    pub selected: usize,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub needs_key: bool,
    pub show_key: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl SetupWizardState {
    pub fn new(notice: Option<String>) -> Self {
        Self {
            step: SetupStep::ChooseApi,
            selected: 0,
            name: String::new(),
            kind: "openai".to_string(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            needs_key: true,
            show_key: false,
            notice,
            error: None,
        }
    }

    pub fn step_number(&self) -> (usize, usize) {
        let total = if self.kind == "openai" || self.step == SetupStep::ChooseApi {
            5
        } else {
            4
        };
        let cur = match self.step {
            SetupStep::ChooseApi => 1,
            SetupStep::ProviderName => 2,
            SetupStep::BaseUrl => 3,
            SetupStep::ApiKey => {
                if self.kind == "openai" {
                    4
                } else {
                    3
                }
            }
            SetupStep::ModelId => total,
        };
        (cur, total)
    }

    pub fn move_up(&mut self) {
        if self.step == SetupStep::ChooseApi {
            let len = SETUP_PRESETS.len();
            self.selected = if self.selected == 0 { len - 1 } else { self.selected - 1 };
        }
    }

    pub fn move_down(&mut self) {
        if self.step == SetupStep::ChooseApi {
            self.selected = (self.selected + 1) % SETUP_PRESETS.len();
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.error = None;
        match self.step {
            SetupStep::ProviderName => self.name.push(c),
            SetupStep::BaseUrl => self.base_url.push(c),
            SetupStep::ApiKey => self.api_key.push(c),
            SetupStep::ModelId => self.model.push(c),
            SetupStep::ChooseApi => {}
        }
    }

    pub fn push_str(&mut self, s: &str) {
        let clean = s.trim();
        for c in clean.chars() {
            if !c.is_control() {
                self.push_char(c);
            }
        }
    }

    pub fn pop_char(&mut self) {
        self.error = None;
        match self.step {
            SetupStep::ProviderName => {
                self.name.pop();
            }
            SetupStep::BaseUrl => {
                self.base_url.pop();
            }
            SetupStep::ApiKey => {
                self.api_key.pop();
            }
            SetupStep::ModelId => {
                self.model.pop();
            }
            SetupStep::ChooseApi => {}
        }
    }

    /// Mundur 1 langkah. Mengembalikan `true` jika user menekan Esc pada langkah 1 (tutup modal).
    pub fn back(&mut self) -> bool {
        self.error = None;
        match self.step {
            SetupStep::ChooseApi => true,
            SetupStep::ProviderName => {
                self.step = SetupStep::ChooseApi;
                false
            }
            SetupStep::BaseUrl => {
                self.step = SetupStep::ProviderName;
                false
            }
            SetupStep::ApiKey => {
                self.step = if self.kind == "openai" {
                    SetupStep::BaseUrl
                } else {
                    SetupStep::ProviderName
                };
                false
            }
            SetupStep::ModelId => {
                self.step = SetupStep::ApiKey;
                false
            }
        }
    }

    /// Maju 1 langkah; mengembalikan `Some(SetupResult)` saat langkah terakhir valid.
    pub fn advance(&mut self) -> Option<SetupResult> {
        self.error = None;
        match self.step {
            SetupStep::ChooseApi => {
                let p = SETUP_PRESETS[self.selected.min(SETUP_PRESETS.len() - 1)];
                self.name = p.default_name.to_string();
                self.kind = p.kind.to_string();
                self.base_url = p.default_url.to_string();
                self.model = p.default_model.to_string();
                self.needs_key = p.needs_key;
                self.api_key.clear();
                self.step = SetupStep::ProviderName;
                None
            }
            SetupStep::ProviderName => {
                let n = self.name.trim();
                if n.is_empty() || n.chars().any(|c| c.is_whitespace()) {
                    self.error = Some("Nama provider wajib diisi tanpa spasi (mis. router, openrouter).".into());
                    return None;
                }
                self.name = n.to_string();
                self.step = if self.kind == "openai" {
                    SetupStep::BaseUrl
                } else {
                    SetupStep::ApiKey
                };
                None
            }
            SetupStep::BaseUrl => {
                let u = self.base_url.trim();
                if !u.starts_with("http://") && !u.starts_with("https://") {
                    self.error = Some("Base URL wajib diawali http:// atau https:// (mis. https://api.openai.com/v1).".into());
                    return None;
                }
                self.base_url = u.to_string();
                self.step = SetupStep::ApiKey;
                None
            }
            SetupStep::ApiKey => {
                let k = self.api_key.trim();
                if self.needs_key && k.is_empty() {
                    self.error = Some("API key wajib diisi (paste sk-... atau nama ENV var).".into());
                    return None;
                }
                self.api_key = if k.is_empty() {
                    "ollama".to_string()
                } else {
                    k.to_string()
                };
                self.step = SetupStep::ModelId;
                None
            }
            SetupStep::ModelId => {
                let m = self.model.trim();
                if m.is_empty() {
                    self.error = Some("ID model wajib diisi (mis. gpt-4.1, claude-sonnet-4, ag/gemini-4).".into());
                    return None;
                }
                self.model = m.to_string();
                Some(SetupResult {
                    name: self.name.clone(),
                    kind: self.kind.clone(),
                    base_url: if self.kind == "openai" {
                        Some(self.base_url.clone())
                    } else {
                        None
                    },
                    api_key_or_env: self.api_key.clone(),
                    model: self.model.clone(),
                })
            }
        }
    }

    pub fn masked_key(&self) -> String {
        if self.show_key {
            return self.api_key.clone();
        }
        let chars: Vec<char> = self.api_key.chars().collect();
        if chars.len() <= 8 {
            return "•".repeat(chars.len());
        }
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}••••••{tail}")
    }
}

/// Mode tampilan di dalam modal pemilih model interaktif (terstruktur).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPickerMode {
    /// Daftar model pribadi tersimpan (curated 1-per-1).
    Saved,
    /// Input inline untuk menambah 1 model manual.
    AddManual,
    /// Katalog model dari API (filter live, pilih 1-per-1 dengan Space).
    Catalog,
}

/// State modal pemilih model interaktif di TUI (`/model`).
#[derive(Debug, Clone)]
pub struct ModelPickerState {
    pub provider: String,
    pub providers: Vec<String>,
    pub active_model: String,
    pub saved_models: Vec<String>,
    pub remote_models: Vec<String>,
    pub mode: ModelPickerMode,
    pub selected: usize,
    pub manual_input: String,
    pub filter_input: String,
    pub loading_remote: bool,
    pub status: Option<String>,
}

impl ModelPickerState {
    pub fn from_data(d: ModelPickerData) -> Self {
        let active_idx = d
            .saved_models
            .iter()
            .position(|m| m == &d.active_model)
            .unwrap_or(0);
        let mode = if d.open_catalog {
            ModelPickerMode::Catalog
        } else {
            ModelPickerMode::Saved
        };
        Self {
            provider: d.provider,
            providers: d.providers,
            active_model: d.active_model,
            saved_models: d.saved_models,
            remote_models: d.remote_models,
            mode,
            selected: if mode == ModelPickerMode::Catalog { 0 } else { active_idx },
            manual_input: String::new(),
            filter_input: String::new(),
            loading_remote: false,
            status: d.status,
        }
    }

    /// Terapkan pembaruan data tanpa membuang mode/filter yang sedang dibuka user.
    pub fn update_from(&mut self, d: ModelPickerData) {
        let same_provider = self.provider == d.provider;
        self.provider = d.provider;
        self.providers = d.providers;
        self.active_model = d.active_model;
        self.saved_models = d.saved_models;
        if !d.remote_models.is_empty() || !same_provider {
            self.remote_models = d.remote_models;
        }
        self.loading_remote = false;
        self.status = d.status;
        if d.open_catalog {
            self.mode = ModelPickerMode::Catalog;
        } else if !same_provider {
            self.mode = ModelPickerMode::Saved;
            self.filter_input.clear();
        }
        self.clamp_selection();
    }

    pub fn filtered_remote(&self) -> Vec<&String> {
        let needle = self.filter_input.trim().to_ascii_lowercase();
        self.remote_models
            .iter()
            .filter(|m| needle.is_empty() || m.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn is_saved(&self, model_id: &str) -> bool {
        self.saved_models.iter().any(|m| m == model_id)
    }

    pub fn current_len(&self) -> usize {
        match self.mode {
            ModelPickerMode::Saved => self.saved_models.len(),
            ModelPickerMode::Catalog => self.filtered_remote().len(),
            ModelPickerMode::AddManual => 0,
        }
    }

    pub fn clamp_selection(&mut self) {
        let len = self.current_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn move_up(&mut self) {
        let len = self.current_len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            len - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        let len = self.current_len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected + 1) % len;
    }

    pub fn selected_saved(&self) -> Option<String> {
        self.saved_models.get(self.selected).cloned()
    }

    pub fn selected_remote(&self) -> Option<String> {
        self.filtered_remote()
            .get(self.selected)
            .map(|s| (*s).clone())
    }

    pub fn next_provider(&self) -> Option<String> {
        if self.providers.len() <= 1 {
            return None;
        }
        let idx = self
            .providers
            .iter()
            .position(|p| p == &self.provider)
            .unwrap_or(0);
        Some(self.providers[(idx + 1) % self.providers.len()].clone())
    }
}

/// State modal pemilih sesi `/resume` interaktif di TUI (`v0.5.30`).
#[derive(Debug, Clone)]
pub struct ResumePickerState {
    pub cwd: String,
    pub active_session_id: String,
    pub items: Vec<ResumePickerItem>,
    pub selected: usize,
    pub filter_input: String,
    pub status: Option<String>,
}

impl ResumePickerState {
    pub fn from_data(d: ResumePickerData) -> Self {
        let active_idx = d.items.iter().position(|it| it.is_active).unwrap_or(0);
        Self {
            cwd: d.cwd,
            active_session_id: d.active_session_id,
            items: d.items,
            selected: active_idx,
            filter_input: String::new(),
            status: d.status,
        }
    }

    pub fn update_from(&mut self, d: ResumePickerData) {
        self.cwd = d.cwd;
        self.active_session_id = d.active_session_id;
        self.items = d.items;
        self.status = d.status;
        self.clamp_selection();
    }

    pub fn filtered_items(&self) -> Vec<&ResumePickerItem> {
        let needle = self.filter_input.trim().to_ascii_lowercase();
        self.items
            .iter()
            .filter(|it| {
                needle.is_empty()
                    || it.title.to_ascii_lowercase().contains(&needle)
                    || it.id.to_ascii_lowercase().contains(&needle)
                    || it.model.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn clamp_selection(&mut self) {
        let len = self.filtered_items().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

/// State modal pertanyaan klarifikasi `ask_user` di TUI (v0.5.6 Bagian 2 — bukan approval!).
pub struct PendingAsk {
    pub question: String,
    pub choices: Vec<String>,
    pub context: Option<String>,
    /// Indeks pilihan terpilih (`0..choices.len()` = pilihan preset; `choices.len()` = ketik jawaban sendiri).
    pub selected: usize,
    pub custom_input: String,
    pub typing_custom: bool,
    pub reply: Option<tokio::sync::oneshot::Sender<crate::tools::ask::AskUserAnswer>>,
}

pub struct App {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub max_steps: usize,
    pub lines: Vec<TranscriptLine>,
    pub input: TextArea<'static>,
    /// Offset gulir dari bawah (0 = selalu ikuti baris terbaru).
    pub scroll: u16,
    pub busy: bool,
    pub step_current: usize,
    pub step_max: usize,
    pub context_pct: u8,
    /// Estimasi / pemakaian token konteks aktif saat ini.
    pub context_used: usize,
    /// Kapasitas context window model AI yang sedang aktif (dalam token).
    pub context_limit: usize,
    /// Cache kapasitas context window per model ID hasil deteksi API `/models` (persisten dari SQLite).
    pub model_contexts: std::collections::HashMap<String, usize>,
    pub session_in: u64,
    pub session_out: u64,
    pub approval: Option<PendingApproval>,
    approval_queue: VecDeque<PendingApproval>,
    /// Scroll vertikal di dalam modal persetujuan (↑↓ / PageUp / PageDown).
    pub approval_scroll: u16,
    /// Kebijakan persetujuan bersama dengan Approver (untuk tombol `[a]` & `/approve`).
    pub approval_policy: Option<Arc<Mutex<ApprovalPolicy>>>,
    /// Modal pertanyaan klarifikasi `ask_user` (v0.5.6 Bagian 2 — terpisah total dari `approval`).
    pub pending_ask: Option<PendingAsk>,
    /// Modal pemilih model interaktif (`/model` di TUI).
    pub model_picker: Option<ModelPickerState>,
    /// Modal pemilih sesi interaktif (`/resume` di TUI, v0.5.30).
    pub resume_picker: Option<ResumePickerState>,
    /// Modal Wizard Setup Provider interaktif (otomatis saat belum ada provider/key, atau via `/setup`).
    pub setup_wizard: Option<SetupWizardState>,
    pub should_quit: bool,
    /// Rencana dari giliran plan yang menunggu konfirmasi y/n (Fase 6).
    pub pending_plan: Option<String>,
    /// Giliran berikutnya dijalankan sebagai plan mode (dari /plan).
    pub plan_next: bool,
    /// Label tampil provider untuk sambutan, mis.
    /// "router (router.ryukomik.com)". Diisi main.rs dari config;
    /// bawaan = nama provider.
    pub provider_label: String,
    /// Jumlah tool registry mode Execute (untuk layar sambutan Moonlit).
    pub tool_count: usize,
    /// ID sesi aktif (opsional, untuk tampilan metadata kartu sambutan).
    pub session_id: String,
    /// Sesi ini hasil resume? Sambutan penuh diganti 1 baris ringkas.
    pub resumed: bool,
    /// Baris sambutan Moonlit — dibuat sekali pada frame pertama (saat
    /// lebar terminal diketahui) lalu ikut bergulir di transkrip.
    pub welcome: Option<Vec<Line<'static>>>,
    /// Baris assistant terakhir masih menerima token streaming.
    streaming: bool,
    /// Diisi oleh fungsi gambar: total baris transkrip & tinggi viewport
    /// terakhir, untuk menghitung lompatan PageUp/PageDown.
    pub last_total_lines: usize,
    pub last_viewport: usize,
    /// Indeks item terpilih pada popup live `/` command palette.
    pub palette_idx: usize,
    /// Indeks item terpilih pada popup `@file` fuzzy autocomplete.
    pub at_palette_idx: usize,
    /// Daftar path relatif file workspace untuk `@file` autocomplete cepat.
    pub workspace_files: Vec<String>,
    /// Riwayat prompt persisten lintas sesi (terlama -> terbaru) untuk navigasi `↑`/`↓`.
    pub prompt_history: Vec<String>,
    /// Posisi navigasi riwayat prompt saat ini (`None` = sedang mengetik draft baru).
    pub history_pos: Option<usize>,
    /// Simpanan draft input sebelum user menekan panah atas.
    pub history_draft: String,
    /// Tampilkan argumen & ringkasan tool secara penuh (toggle via `/details`).
    pub show_tool_details: bool,
}

pub fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("Tanya apa saja… (ketik / untuk perintah, @ untuk sisipkan file)");
    ta.set_placeholder_style(Style::default().fg(moonlit::DIM));
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta
}

/// Ekstrak token `@query` aktif di akhir baris input untuk popup `@file` autocomplete.
/// Mengembalikan `(byte_offset_at, query_tanpa_at)`.
pub fn extract_at_query(input: &str) -> Option<(usize, String)> {
    if input.contains('\n') || input.trim_start().starts_with('/') {
        return None;
    }
    let token_start = input
        .char_indices()
        .rev()
        .find(|&(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token = &input[token_start..];
    let rest = token.strip_prefix('@')?;
    Some((token_start, rest.to_string()))
}

fn is_fuzzy_subseq(haystack: &str, needle: &str) -> bool {
    let mut it = haystack.chars();
    for nc in needle.chars() {
        if !it.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

/// Filter & urutkan daftar file workspace berdasarkan query fuzzy `@file`.
pub fn filter_workspace_files(files: &[String], query: &str, max_results: usize) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return files.iter().take(max_results).cloned().collect();
    }
    let mut scored: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|p| {
            let low = p.to_ascii_lowercase();
            let fname = low.rsplit('/').next().unwrap_or(&low);
            if fname.starts_with(&q) {
                Some((0u8, p.len(), p))
            } else if low.contains(&q) {
                Some((1u8, p.len(), p))
            } else if is_fuzzy_subseq(&low, &q) {
                Some((2u8, p.len(), p))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(max_results)
        .map(|(_, _, p)| p.clone())
        .collect()
}

/// Ganti token `@query` di akhir input dengan `@{selected} `.
pub fn complete_at_mention(input: &str, selected: &str) -> String {
    if let Some((start, _)) = extract_at_query(input) {
        format!("{}@{selected} ", &input[..start])
    } else {
        format!("{input}@{selected} ")
    }
}

impl App {
    pub fn new(
        cwd: String,
        provider: String,
        model: String,
        max_steps: usize,
        session_in: u64,
        session_out: u64,
    ) -> Self {
        let context_limit = infer_model_context_limit(&model);
        Self {
            cwd,
            provider_label: provider.clone(),
            provider,
            model,
            max_steps,
            lines: Vec::new(),
            input: new_textarea(),
            scroll: 0,
            busy: false,
            step_current: 0,
            step_max: max_steps,
            context_pct: 0,
            context_used: 0,
            context_limit,
            model_contexts: std::collections::HashMap::new(),
            session_in,
            session_out,
            approval: None,
            approval_queue: VecDeque::new(),
            approval_scroll: 0,
            approval_policy: None,
            pending_ask: None,
            model_picker: None,
            resume_picker: None,
            setup_wizard: None,
            should_quit: false,
            pending_plan: None,
            plan_next: false,
            tool_count: 0,
            session_id: String::new(),
            resumed: false,
            welcome: None,
            streaming: false,
            last_total_lines: 0,
            last_viewport: 0,
            palette_idx: 0,
            at_palette_idx: 0,
            workspace_files: Vec::new(),
            prompt_history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            show_tool_details: true,
        }
    }

    /// Catat prompt ke riwayat memori (menghindari duplikat beruntun, maks 200).
    pub fn push_history(&mut self, text: &str) {
        let clean = text.trim();
        if clean.is_empty() {
            return;
        }
        if self.prompt_history.last().map(|s| s.as_str()) != Some(clean) {
            self.prompt_history.push(clean.to_string());
            if self.prompt_history.len() > 200 {
                let extra = self.prompt_history.len() - 200;
                self.prompt_history.drain(0..extra);
            }
        }
        self.history_pos = None;
        self.history_draft.clear();
    }

    /// Mundur ke prompt sebelumnya pada riwayat (`↑`). Mengembalikan `true` jika teks input berubah.
    pub fn history_up(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        let next_pos = match self.history_pos {
            None => {
                self.history_draft = self.input.lines().join("\n");
                self.prompt_history.len() - 1
            }
            Some(0) => return false,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next_pos);
        let item = self.prompt_history[next_pos].clone();
        self.input = new_textarea();
        self.input.insert_str(&item);
        true
    }

    /// Maju ke prompt lebih baru atau kembali ke draft awal (`↓`). Mengembalikan `true` jika teks berubah.
    pub fn history_down(&mut self) -> bool {
        let Some(pos) = self.history_pos else {
            return false;
        };
        if pos + 1 < self.prompt_history.len() {
            let next_pos = pos + 1;
            self.history_pos = Some(next_pos);
            let item = self.prompt_history[next_pos].clone();
            self.input = new_textarea();
            self.input.insert_str(&item);
        } else {
            self.history_pos = None;
            let draft = self.history_draft.clone();
            self.input = new_textarea();
            if !draft.is_empty() {
                self.input.insert_str(&draft);
            }
        }
        true
    }

    /// Deteksi kapasitas context window (token) untuk `model_id` menggunakan prioritas:
    /// cache metadata API `/models` -> kapasitas aktif (bila cocok model aktif) -> heuristik nama model.
    pub fn context_limit_for_model(&self, model_id: &str) -> usize {
        if let Some(&n) = self.model_contexts.get(model_id) {
            if n >= 1024 {
                return n;
            }
        }
        if model_id == self.model && self.context_limit >= 1024 {
            return self.context_limit;
        }
        infer_model_context_limit(model_id)
    }

    /// Info untuk layar sambutan Moonlit, dari state sesi saat ini.
    pub fn welcome_info(&self) -> WelcomeInfo {
        let info = WelcomeInfo::new(
            self.provider.clone(),
            self.provider_label.clone(),
            self.model.clone(),
            self.cwd.clone(),
            self.tool_count,
        )
        .with_context_limit(self.context_limit);
        if self.session_id.is_empty() {
            info
        } else {
            info.with_session_id(&self.session_id)
        }
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.lines
            .push(TranscriptLine::simple(LineKind::Info, text.into()));
        self.streaming = false;
    }

    pub fn menu(&mut self, markup: impl Into<String>) {
        self.lines
            .push(TranscriptLine::simple(LineKind::Menu, markup.into()));
        self.streaming = false;
        self.scroll = 0;
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.lines
            .push(TranscriptLine::simple(LineKind::Error, text.into()));
        self.streaming = false;
    }

    pub fn push_user(&mut self, text: &str) {
        self.lines
            .push(TranscriptLine::simple(LineKind::User, text.to_string()));
        self.streaming = false;
        self.busy = true;
        self.scroll = 0;
    }

    /// Reducer: terapkan 1 UiEvent dari loop agent / host ke state.
    pub fn apply_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Token(t) => {
                if self.streaming {
                    if let Some(last) = self.lines.last_mut() {
                        if last.kind == LineKind::Assistant {
                            last.text.push_str(&t);
                            return;
                        }
                    }
                }
                self.lines
                    .push(TranscriptLine::simple(LineKind::Assistant, t));
                self.streaming = true;
            }
            UiEvent::ToolStart { name, args } => {
                self.streaming = false;
                self.busy = true;
                let text = if args.is_empty() {
                    name.clone()
                } else {
                    format!("{name} {args}")
                };
                self.lines.push(TranscriptLine::with_tool(
                    text,
                    Some(name),
                    Some(ToolState::Pending),
                ));
            }
            UiEvent::ToolDone {
                name,
                summary,
                is_error,
            } => {
                self.streaming = false;
                let state = if is_error {
                    ToolState::Err
                } else {
                    ToolState::Ok
                };
                // Cari baris tool pending terakhir dengan nama yang sama.
                let found = self
                    .lines
                    .iter_mut()
                    .rev()
                    .find(|l| {
                        l.kind == LineKind::Tool
                            && l.tool_state == Some(ToolState::Pending)
                            && l.tool_name.as_deref() == Some(name.as_str())
                    });
                match found {
                    Some(line) => {
                        line.tool_state = Some(state);
                        if !summary.is_empty() {
                            line.text.push_str(&format!(" — {summary}"));
                        }
                    }
                    None => {
                        // ToolDone tanpa ToolStart (tidak terjadi di loop
                        // sekarang) — tetap tampilkan sebagai baris tool.
                        self.lines.push(TranscriptLine::with_tool(
                            format!("{name} — {summary}"),
                            Some(name),
                            Some(state),
                        ));
                    }
                }
            }
            UiEvent::Step {
                current,
                max,
                context_pct,
            } => {
                self.step_current = current;
                self.step_max = max;
                self.context_pct = context_pct;
                self.busy = true;
            }
            UiEvent::TurnDone { usage } => {
                self.streaming = false;
                self.busy = false;
                if let Some(u) = usage {
                    self.session_in += u.input_tokens;
                    self.session_out += u.output_tokens;
                }
            }
            UiEvent::ModelChanged { provider, model } => {
                self.provider = provider;
                self.context_limit = self
                    .model_contexts
                    .get(&model)
                    .copied()
                    .filter(|&n| n >= 1024)
                    .unwrap_or_else(|| infer_model_context_limit(&model));
                self.context_pct = calc_context_pct(self.context_used, self.context_limit);
                self.model = model;
            }
            UiEvent::ContextInfo {
                used_tokens,
                limit_tokens,
            } => {
                self.context_used = used_tokens;
                if limit_tokens > 0 {
                    self.context_limit = limit_tokens;
                }
                self.context_pct = calc_context_pct(self.context_used, self.context_limit);
            }
            UiEvent::ModelContexts(map) => {
                for (k, v) in map {
                    if v >= 1024 {
                        self.model_contexts.insert(k, v);
                    }
                }
                if let Some(&n) = self.model_contexts.get(&self.model) {
                    if n >= 1024 {
                        self.context_limit = n;
                        self.context_pct = calc_context_pct(self.context_used, self.context_limit);
                    }
                }
            }
            UiEvent::PlanUpdated(checklist) => {
                self.streaming = false;
                self.lines
                    .push(TranscriptLine::simple(LineKind::Plan, checklist));
            }
            UiEvent::PlanReady(plan) => {
                self.streaming = false;
                self.busy = false;
                self.pending_plan = Some(plan);
                self.info(
                    "Rencana siap. Ketik y lalu Enter untuk menjalankan rencana ini, atau n untuk membuangnya.",
                );
            }
            UiEvent::Info(text) => self.info(text),
            UiEvent::Menu(markup) => self.menu(markup),
            UiEvent::ModelPicker(data) => match &mut self.model_picker {
                Some(mp) => mp.update_from(data),
                None => self.model_picker = Some(ModelPickerState::from_data(data)),
            },
            UiEvent::ResumePicker(data) => match &mut self.resume_picker {
                Some(rp) => rp.update_from(data),
                None => self.resume_picker = Some(ResumePickerState::from_data(data)),
            },
            UiEvent::CwdChanged(new_cwd) => {
                self.cwd = new_cwd;
            }
            UiEvent::SessionReset {
                session_id,
                provider,
                model,
            } => {
                self.lines.clear();
                self.welcome = None;
                self.resumed = false;
                self.session_id = session_id;
                self.provider = provider;
                self.model = model;
                self.session_in = 0;
                self.session_out = 0;
                self.context_pct = 0;
                self.context_used = 0;
                self.context_limit = self
                    .model_contexts
                    .get(&self.model)
                    .copied()
                    .filter(|&n| n >= 1024)
                    .unwrap_or_else(|| infer_model_context_limit(&self.model));
                self.step_current = 0;
                self.scroll = 0;
                self.busy = false;
                self.streaming = false;
                self.pending_plan = None;
                self.plan_next = false;
            }
            UiEvent::SessionResumed {
                session_id,
                title,
                provider,
                model,
                input_tokens,
                output_tokens,
                context_used,
                context_limit,
                preview_lines,
                total_messages,
            } => {
                self.lines.clear();
                self.welcome = None;
                self.resumed = true;
                self.session_id = session_id;
                self.provider = provider;
                self.model = model;
                self.session_in = input_tokens;
                self.session_out = output_tokens;
                self.context_used = context_used;
                self.context_limit = context_limit.max(1);
                self.context_pct = calc_context_pct(context_used, self.context_limit);
                self.step_current = 0;
                self.scroll = 0;
                self.busy = false;
                self.streaming = false;
                self.pending_plan = None;
                self.plan_next = false;
                self.info(format!(
                    "Melanjutkan sesi: \"{title}\" — {total_messages} pesan dimuat."
                ));
                for (role, text) in preview_lines {
                    let kind = if role == "user" {
                        LineKind::User
                    } else {
                        LineKind::Assistant
                    };
                    self.lines.push(TranscriptLine::simple(kind, text));
                }
            }
            UiEvent::WorkspaceFilesLoaded(files) => {
                self.workspace_files = files;
            }
            UiEvent::Error(text) => {
                self.busy = false;
                self.error(text);
            }
        }
    }

    /// Jumlah total approval yang menunggu (aktif + antrean).
    pub fn approval_pending_count(&self) -> usize {
        self.approval.is_some() as usize + self.approval_queue.len()
    }

    /// Tolak & bersihkan SEMUA popup approval aktif dan yang mengantre (dipakai `/stop` & `Esc`).
    pub fn cancel_all_approvals(&mut self) -> usize {
        let mut dropped = 0usize;
        if let Some(mut p) = self.approval.take() {
            if let Some(tx) = p.reply.take() {
                let _ = tx.send(false);
            }
            dropped += 1;
        }
        while let Some(mut next) = self.approval_queue.pop_front() {
            if let Some(tx) = next.reply.take() {
                let _ = tx.send(false);
            }
            dropped += 1;
        }
        self.approval_scroll = 0;
        dropped
    }

    /// Hentikan state aktif (streaming, busy, pending_plan, plan_next) + tolak semua approval.
    pub fn stop_all_active(&mut self) -> (bool, usize, bool) {
        let was_busy = self.busy;
        let dropped = self.cancel_all_approvals();
        let had_plan = self.pending_plan.take().is_some() || self.plan_next;
        self.plan_next = false;
        self.busy = false;
        self.streaming = false;
        (was_busy, dropped, had_plan)
    }

    /// Popup approval masuk dari channel approver. Bila sudah ada popup
    /// aktif, antrekan agar permintaan berikutnya tidak hilang/tertimpa.
    pub fn receive_approval(&mut self, req: ApprovalRequest) {
        let pending = PendingApproval {
            title: req.title,
            detail: req.detail,
            reply: Some(req.reply),
        };
        if self.approval.is_none() {
            self.approval = Some(pending);
            self.approval_scroll = 0;
        } else {
            self.approval_queue.push_back(pending);
        }
    }

    /// Jawab popup yang sedang tampil. true bila memang ada popup.
    pub fn answer_approval(&mut self, yes: bool) -> bool {
        match self.approval.take() {
            Some(mut p) => {
                if let Some(tx) = p.reply.take() {
                    let _ = tx.send(yes);
                }
                self.approval_scroll = 0;
                self.advance_approval_queue();
                true
            }
            None => false,
        }
    }

    /// Jawab setuju DAN izinkan kategori tool ini otomatis untuk sisa sesi (`[a]`).
    /// Mengembalikan nama kategori yang baru diizinkan.
    pub fn approve_category_for_session(&mut self) -> Option<String> {
        let mut p = self.approval.take()?;
        let category = tool_category_from_title(&p.title);
        if let Some(policy) = &self.approval_policy {
            if let Ok(mut pol) = policy.lock() {
                pol.allow_category(category.clone());
            }
        }
        if let Some(tx) = p.reply.take() {
            let _ = tx.send(true);
        }
        self.approval_scroll = 0;
        self.advance_approval_queue();
        Some(category)
    }

    /// Ambil item antrean berikutnya; bila item berikutnya sudah otomatis
    /// diizinkan oleh `approval_policy`, langsung setujui tanpa memunculkan popup.
    fn advance_approval_queue(&mut self) {
        while let Some(mut next) = self.approval_queue.pop_front() {
            let cat = tool_category_from_title(&next.title);
            let auto_ok = match &self.approval_policy {
                Some(pol) => pol
                    .lock()
                    .map(|p| p.is_auto_allowed(&cat))
                    .unwrap_or(false),
                None => false,
            };
            if auto_ok {
                if let Some(tx) = next.reply.take() {
                    let _ = tx.send(true);
                }
                continue;
            }
            self.approval = Some(next);
            break;
        }
    }

    /// Terima pertanyaan klarifikasi `ask_user` dari channel terpisah (Bagian 2 v0.5.6).
    pub fn receive_ask(&mut self, req: crate::tools::ask::AskUserRequest) {
        let typing_custom = req.choices.is_empty();
        self.pending_ask = Some(PendingAsk {
            question: req.question,
            choices: req.choices,
            context: req.context,
            selected: 0,
            custom_input: String::new(),
            typing_custom,
            reply: Some(req.reply),
        });
    }

    /// Kirim jawaban modal `ask_user` (teks pilihan atau jawaban bebas).
    pub fn submit_ask(&mut self, answer: String) -> bool {
        let Some(mut p) = self.pending_ask.take() else {
            return false;
        };
        if let Some(tx) = p.reply.take() {
            let _ = tx.send(crate::tools::ask::AskUserAnswer {
                response: answer,
                timed_out: false,
                non_interactive: false,
            });
        }
        true
    }

    /// Batalkan modal `ask_user` (Esc -> biarkan agen melanjutkan dengan asumsi).
    pub fn cancel_ask(&mut self) -> bool {
        let Some(mut p) = self.pending_ask.take() else {
            return false;
        };
        if let Some(tx) = p.reply.take() {
            let _ = tx.send(crate::tools::ask::AskUserAnswer {
                response: String::new(),
                timed_out: true,
                non_interactive: false,
            });
        }
        true
    }

    /// Ambil isi kotak input & kosongkan. Mengembalikan teks apa adanya.
    pub fn take_input(&mut self) -> String {
        let text = self.input.lines().join("\n");
        self.input = new_textarea();
        text
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Tinggi 1 halaman untuk PageUp/PageDown (dari gambar terakhir).
    pub fn page_size(&self) -> u16 {
        (self.last_viewport.max(1)) as u16
    }
}
