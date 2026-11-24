//! State TUI + reducer event (Fase 4). Murni logika — tanpa terminal —
//! agar bisa dites: UI nyata (mod.rs) hanya menggambar state ini dan
//! meneruskan input keyboard ke sini.

use crate::agent::UiEvent;
use crate::security::ApprovalRequest;
use crate::tui::moonlit::{self, WelcomeInfo};
use ratatui::style::Style;
use ratatui::text::Line;
use std::collections::VecDeque;
use tui_textarea::TextArea;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    Tool,
    Info,
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
    pub session_in: u64,
    pub session_out: u64,
    pub approval: Option<PendingApproval>,
    approval_queue: VecDeque<PendingApproval>,
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
            session_in,
            session_out,
            approval: None,
            approval_queue: VecDeque::new(),
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
                self.model = model;
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
            UiEvent::Error(text) => {
                self.busy = false;
                self.error(text);
            }
        }
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
                if let Some(next) = self.approval_queue.pop_front() {
                    self.approval = Some(next);
                }
                true
            }
            None => false,
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
