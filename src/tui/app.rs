//! State TUI + reducer event (Fase 4). Murni logika — tanpa terminal —
//! agar bisa dites: UI nyata (mod.rs) hanya menggambar state ini dan
//! meneruskan input keyboard ke sini.

use crate::agent::context::{calc_context_pct, infer_model_context_limit};
use crate::agent::ui::ModelPickerData;
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
}

impl TranscriptLine {
    fn simple(kind: LineKind, text: String) -> Self {
        Self {
            kind,
            text,
            tool_name: None,
            tool_state: None,
        }
    }
}

/// Permintaan approval yang sedang ditampilkan sebagai popup.
pub struct PendingApproval {
    pub title: String,
    pub detail: String,
    pub reply: Option<tokio::sync::oneshot::Sender<bool>>,
}

/// Mode tampilan di dalam modal pemilih model interaktif (ala Hermes).
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
    pub session_in: u64,
    pub session_out: u64,
    pub approval: Option<PendingApproval>,
    approval_queue: VecDeque<PendingApproval>,
    /// Scroll vertikal di dalam modal persetujuan (↑↓ / PageUp / PageDown).
    pub approval_scroll: u16,
    /// Kebijakan persetujuan bersama dengan Approver (untuk tombol `[a]` & `/approve`).
    pub approval_policy: Option<Arc<Mutex<ApprovalPolicy>>>,
    /// Modal pemilih model interaktif (`/model` di TUI).
    pub model_picker: Option<ModelPickerState>,
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
    /// Tampilkan argumen & ringkasan tool secara penuh (toggle via `/details`).
    pub show_tool_details: bool,
}

pub fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("Tanya apa saja…");
    ta.set_placeholder_style(Style::default().fg(moonlit::DIM));
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta
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
            session_in,
            session_out,
            approval: None,
            approval_queue: VecDeque::new(),
            approval_scroll: 0,
            approval_policy: None,
            model_picker: None,
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
            show_tool_details: true,
        }
    }

    /// Info untuk layar sambutan Moonlit, dari state sesi saat ini.
    pub fn welcome_info(&self) -> WelcomeInfo {
        let info = WelcomeInfo::new(
            self.provider.clone(),
            self.provider_label.clone(),
            self.model.clone(),
            self.cwd.clone(),
            self.tool_count,
        );
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
                self.lines.push(TranscriptLine {
                    kind: LineKind::Tool,
                    text,
                    tool_name: Some(name),
                    tool_state: Some(ToolState::Pending),
                });
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
                        self.lines.push(TranscriptLine {
                            kind: LineKind::Tool,
                            text: format!("{name} — {summary}"),
                            tool_name: Some(name),
                            tool_state: Some(state),
                        });
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
                self.context_limit = infer_model_context_limit(&model);
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
                self.context_limit = infer_model_context_limit(&self.model);
                self.step_current = 0;
                self.scroll = 0;
                self.busy = false;
                self.streaming = false;
                self.pending_plan = None;
                self.plan_next = false;
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
