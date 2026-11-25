//! TUI Ratatui SelenCode (Fase 4, Blueprint Bagian 8).
//!
//! Arsitektur: loop agent berjalan sebagai task Tokio terpisah (di main.rs)
//! yang memiliki Session. UI di sini berjalan di thread blocking dan hanya
//! bertukar pesan:
//!   UI --TuiCommand--> worker (prompt / perintah garis miring)
//!   UI <--UiEvent----- worker (token streaming, tool, usage, info)
//!   UI <--ApprovalRequest-- approver channel (popup y/n dijawab oneshot)
//! Terminal selalu dipulihkan — guard Drop + panic hook — di semua jalur.

pub mod app;
pub mod diff_view;
pub mod markdown;
pub mod moonlit;

use crate::agent::UiEvent;
use crate::repl::{filter_commands, help_menu_markup, parse_slash_command, SlashCommand};
use crate::security::{tool_category_from_title, ApprovalRequest};
use anyhow::Result;
use app::{
    App, LineKind, ModelPickerMode, SetupResult, SetupStep, SetupWizardState, ToolState,
    SETUP_PRESETS,
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Aksi dari modal pemilih model interaktif (`/model`) ke worker agent.
#[derive(Debug)]
pub enum ModelPickerCmd {
    /// Buka / muat snapshot daftar model pribadi & provider.
    Open,
    /// Aktifkan model terpilih (tambah ke daftar bila belum ada).
    SelectModel(String),
    /// Tambah 1 model manual ke daftar pribadi tanpa menutup modal.
    AddOne(String),
    /// Toggle 1 model dari katalog API (tambah jika belum ada, hapus jika sudah ada & bukan aktif).
    ToggleOne(String),
    /// Hapus 1 model dari daftar pribadi.
    RemoveOne(String),
    /// Bersihkan semua model tersimpan kecuali model aktif.
    ClearAll,
    /// Tarik katalog model real-time dari endpoint API provider aktif.
    FetchCatalog,
    /// Pindah ke provider lain lalu perbarui isi modal.
    SwitchProvider(String),
}

/// Perintah dari UI ke worker agent (dimiliki main.rs).
#[derive(Debug)]
pub enum TuiCommand {
    Prompt(String),
    /// Interupsi giliran yang sedang berjalan dan langsung jalankan prompt baru (ala OpenCode/Hermes).
    InterruptPrompt(String),
    /// Giliran plan mode (Fase 6): baca-saja, diakhiri rencana.
    PromptPlan(String),
    /// Jalankan rencana yang sudah disetujui user (teks rencana utuh).
    ExecutePlan(String),
    /// `!cmd` — jalankan shell lokal cepat tanpa memanggil LLM.
    InlineShell(String),
    ShowModels,
    /// Aksi modal pemilih model interaktif ala Hermes.
    ModelPicker(ModelPickerCmd),
    /// /model <arg>: ganti provider, `/model set <id>`, `/model <prov>:<id>`, `/model add ...`, `/model rm ...`, `/model clear`.
    SwitchOrSetModel(String),
    /// /models [filter]: intip katalog model dari endpoint API provider aktif (tanpa mengotori config).
    FetchRemoteModels(Option<String>),
    /// /provider [preset|add|model|default|rm ...]: kelola & pasang provider lain.
    ManageProvider(Option<String>),
    /// Simpan provider baru hasil Wizard `/setup`, jadikan default, & aktifkan langsung.
    SaveSetupProvider(SetupResult),
    /// /telegram [setup|allow|rm|approve|off]: kelola koneksi Telegram Gateway.
    Telegram(Option<String>),
    Undo,
    Redo,
    Checkpoint(Option<String>),
    Rollback(Option<String>),
    Worktree(Option<String>),
    McpStatus,
    Doctor,
    Verify(Option<String>),
    Index(Option<String>),
    Jobs(Option<String>),
    Route(Option<String>),
    Lsp(Option<String>),
    Task(Option<String>),
    Memory(Option<String>),
    Skills(Option<String>),
    Review(Option<String>),
    Cron(Option<String>),
    Search(Option<String>),
    UseSkill(Option<String>),
    LearnSkill(Option<String>),
    InitAgents,
    ExportChat(Option<String>),
    /// /approve [ask | edits | auto]: atur mode izin sesi aktif.
    SetApproval(Option<String>),
    /// /sandbox [read-only | workspace-write | unrestricted | isolate ... | audit]
    Sandbox(Option<String>),
    /// /new: mulai sesi baru yang bersih.
    NewSession,
    /// /compact: ringkas riwayat konteks secara manual.
    CompactContext,
    ShowSessions,
    Resume(Option<String>),
    Shutdown,
}

/// Channel-channel antara UI dan worker/approver.
pub struct TuiChannels {
    pub cmd_tx: mpsc::Sender<TuiCommand>,
    pub event_rx: mpsc::Receiver<UiEvent>,
    pub approval_rx: mpsc::Receiver<ApprovalRequest>,
    /// Kanal terpisah untuk pertanyaan klarifikasi `ask_user` (v0.5.6 Bagian 2).
    pub ask_rx: mpsc::Receiver<crate::tools::ask::AskUserRequest>,
    /// Sinyal pembatalan giliran saat user menekan `Esc` / `/stop` / interupsi ketika agent sibuk.
    pub cancel: Arc<Notify>,
    /// True bila pembatalan dipicu oleh pesan interupsi baru (worker langsung lanjut ke giliran baru).
    pub interrupt_pending: Arc<AtomicBool>,
}

// ---------- gambar ----------

/// Pecah teks per kata agar muat dalam `max_cols` kolom (aman UTF-8, tidak pecah di tengah kata).
fn wrap_words(text: &str, max_cols: usize) -> Vec<String> {
    let limit = max_cols.max(12);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    for word in trimmed.split_whitespace() {
        let wlen = word.chars().count();
        if cur_len == 0 {
            if wlen <= limit {
                cur.push_str(word);
                cur_len = wlen;
            } else {
                // Kata tunggal sangat panjang (mis. URL/path): pecah aman per char.
                let mut buf = String::new();
                let mut n = 0usize;
                for ch in word.chars() {
                    if n >= limit {
                        lines.push(buf.clone());
                        buf.clear();
                        n = 0;
                    }
                    buf.push(ch);
                    n += 1;
                }
                cur = buf;
                cur_len = n;
            }
        } else if cur_len + 1 + wlen <= limit {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(cur);
            if wlen <= limit {
                cur = word.to_string();
                cur_len = wlen;
            } else {
                let mut buf = String::new();
                let mut n = 0usize;
                for ch in word.chars() {
                    if n >= limit {
                        lines.push(buf.clone());
                        buf.clear();
                        n = 0;
                    }
                    buf.push(ch);
                    n += 1;
                }
                cur = buf;
                cur_len = n;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Render kartu menu `/` berwarna Moonlit secara responsif sesuai lebar layar
/// (tata letak 2 baris bertumpuk rapi di Termux HP < 72 kolom, tabel sejajar di desktop).
pub fn render_menu_card_width(markup: &str, width: u16) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut opened = false;
    let narrow = width < 72;
    let indent = if narrow { " " } else { "  " };
    let inner_w = (width as usize).saturating_sub(if narrow { 2 } else { 4 }).clamp(24, 92);

    for raw in markup.lines() {
        if let Some(title) = raw.strip_prefix("# ") {
            opened = true;
            let title_s = title.trim();
            let prefix_len = 5 + title_s.chars().count() + 1; // "╭─ ☾ " + title + " "
            let dashes = inner_w.saturating_sub(prefix_len).max(2);
            out.push(Line::from(vec![
                Span::styled(format!("{indent}╭─ "), Style::default().fg(moonlit::BORDER)),
                Span::styled(
                    "☾ ".to_string(),
                    Style::default()
                        .fg(moonlit::VIOLET)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    title_s.to_string(),
                    Style::default()
                        .fg(moonlit::LILAC)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", "─".repeat(dashes)),
                    Style::default().fg(moonlit::BORDER),
                ),
            ]));
        } else if raw.trim() == "---" {
            out.push(Line::from(vec![
                Span::styled(format!("{indent}├"), Style::default().fg(moonlit::BORDER)),
                Span::styled(
                    "─".repeat(inner_w.saturating_sub(1)),
                    Style::default().fg(moonlit::FAINT),
                ),
            ]));
        } else if let Some(rest) = raw.strip_prefix("* ").or_else(|| raw.strip_prefix("> ")) {
            let active = raw.starts_with("* ");
            // splitn(3, '|') agar deskripsi di kolom ke-3 tidak pernah terpotong.
            let parts: Vec<&str> = rest.splitn(3, '|').map(|p| p.trim()).collect();
            let col1 = parts.first().copied().unwrap_or("");
            let col2 = parts.get(1).copied().unwrap_or("");
            let col3 = parts.get(2).copied().unwrap_or("");
            let col1_color = if active { moonlit::LILAC } else { moonlit::LAV };
            let bullet = if active {
                Span::styled(
                    "● ".to_string(),
                    Style::default()
                        .fg(moonlit::MINT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    "› ".to_string(),
                    Style::default()
                        .fg(moonlit::VIOLET)
                        .add_modifier(Modifier::BOLD),
                )
            };

            if narrow {
                // Mobile / Termux HP: baris 1 = nama + argumen, baris 2+ = deskripsi terindentasi rapi!
                let mut head_spans = vec![
                    Span::styled(format!("{indent}│ "), Style::default().fg(moonlit::BORDER)),
                    bullet,
                    Span::styled(
                        col1.to_string(),
                        Style::default().fg(col1_color).add_modifier(Modifier::BOLD),
                    ),
                ];
                let head_used = 4 + col1.chars().count();
                if !col2.is_empty() {
                    if head_used + 1 + col2.chars().count() <= inner_w {
                        head_spans.push(Span::styled(
                            format!(" {col2}"),
                            Style::default().fg(moonlit::CYAN_MOON),
                        ));
                        out.push(Line::from(head_spans));
                    } else {
                        out.push(Line::from(head_spans));
                        for sub in wrap_words(col2, inner_w.saturating_sub(5)) {
                            out.push(Line::from(vec![
                                Span::styled(
                                    format!("{indent}│   "),
                                    Style::default().fg(moonlit::BORDER),
                                ),
                                Span::styled(sub, Style::default().fg(moonlit::CYAN_MOON)),
                            ]));
                        }
                    }
                } else {
                    out.push(Line::from(head_spans));
                }
                if !col3.is_empty() {
                    for sub in wrap_words(col3, inner_w.saturating_sub(5)) {
                        out.push(Line::from(vec![
                            Span::styled(
                                format!("{indent}│   "),
                                Style::default().fg(moonlit::BORDER),
                            ),
                            Span::styled(sub, Style::default().fg(moonlit::SILVER)),
                        ]));
                    }
                }
            } else {
                // Desktop: kolom sejajar dengan wrap rapi bila deskripsi panjang.
                let mut spans = vec![
                    Span::styled(format!("{indent}│ "), Style::default().fg(moonlit::BORDER)),
                    bullet,
                    Span::styled(
                        format!("{col1:<12}"),
                        Style::default().fg(col1_color).add_modifier(Modifier::BOLD),
                    ),
                ];
                let mut used = 4 + col1.chars().count().max(12);
                if !col2.is_empty() {
                    spans.push(Span::styled(
                        format!(" {col2}"),
                        Style::default().fg(moonlit::CYAN_MOON),
                    ));
                    used += 1 + col2.chars().count();
                }
                if !col3.is_empty() {
                    let avail = inner_w.saturating_sub(used + 5);
                    if avail >= col3.chars().count() {
                        spans.push(Span::styled(
                            "  ·  ".to_string(),
                            Style::default().fg(moonlit::FAINT),
                        ));
                        spans.push(Span::styled(
                            col3.to_string(),
                            Style::default().fg(moonlit::SILVER),
                        ));
                        out.push(Line::from(spans));
                    } else {
                        let wrapped = wrap_words(col3, inner_w.saturating_sub(18));
                        if let Some((first, rest_lines)) = wrapped.split_first() {
                            if avail >= 16 && first.chars().count() <= avail {
                                spans.push(Span::styled(
                                    "  ·  ".to_string(),
                                    Style::default().fg(moonlit::FAINT),
                                ));
                                spans.push(Span::styled(
                                    first.clone(),
                                    Style::default().fg(moonlit::SILVER),
                                ));
                                out.push(Line::from(spans));
                                for cont in rest_lines {
                                    out.push(Line::from(vec![
                                        Span::styled(
                                            format!("{indent}│                 "),
                                            Style::default().fg(moonlit::BORDER),
                                        ),
                                        Span::styled(
                                            cont.clone(),
                                            Style::default().fg(moonlit::SILVER),
                                        ),
                                    ]));
                                }
                            } else {
                                out.push(Line::from(spans));
                                for cont in wrapped {
                                    out.push(Line::from(vec![
                                        Span::styled(
                                            format!("{indent}│     "),
                                            Style::default().fg(moonlit::BORDER),
                                        ),
                                        Span::styled(
                                            cont,
                                            Style::default().fg(moonlit::SILVER),
                                        ),
                                    ]));
                                }
                            }
                        } else {
                            out.push(Line::from(spans));
                        }
                    }
                } else {
                    out.push(Line::from(spans));
                }
            }
        } else if let Some(tip) = raw.strip_prefix("! ") {
            let wrapped = wrap_words(tip, inner_w.saturating_sub(5));
            for (idx, part) in wrapped.into_iter().enumerate() {
                if idx == 0 {
                    out.push(Line::from(vec![
                        Span::styled(format!("{indent}│ "), Style::default().fg(moonlit::BORDER)),
                        Span::styled(
                            "✦ ".to_string(),
                            Style::default()
                                .fg(moonlit::GOLD)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(part, Style::default().fg(moonlit::SILVER)),
                    ]));
                } else {
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("{indent}│   "),
                            Style::default().fg(moonlit::BORDER),
                        ),
                        Span::styled(part, Style::default().fg(moonlit::SILVER)),
                    ]));
                }
            }
        } else if !raw.trim().is_empty() {
            for part in wrap_words(raw, inner_w.saturating_sub(3)) {
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}│ "), Style::default().fg(moonlit::BORDER)),
                    Span::styled(part, Style::default().fg(moonlit::DIM)),
                ]));
            }
        }
    }

    if opened {
        out.push(Line::from(Span::styled(
            format!("{indent}╰{}", "─".repeat(inner_w.saturating_sub(1))),
            Style::default().fg(moonlit::BORDER),
        )));
    }
    out
}

/// Render kartu menu `/` berwarna Moonlit dengan lebar baku desktop (96 kolom).
pub fn render_menu_card(markup: &str) -> Vec<Line<'static>> {
    render_menu_card_width(markup, 96)
}

/// Render 1 `TranscriptLine` dan simpan hasilnya di cache per-pesan agar ketikan/scroll
/// tidak mem-parse ulang Markdown seluruh riwayat pada setiap frame (Bagian A).
fn ensure_line_cached(
    l: &mut app::TranscriptLine,
    width: u16,
    show_tool_details: bool,
    has_leading_blank: bool,
    prev_assistant: bool,
) {
    if l.cached_width == width
        && l.cached_details == show_tool_details
        && l.cached_text_len == l.text.len()
        && l.cached_tool_state == l.tool_state
        && l.cached_has_leading_blank == has_leading_blank
        && l.cached_prev_assistant == prev_assistant
    {
        return;
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let narrow = width < 72;
    let indent = if narrow { " " } else { "  " };
    let max_w = (width as usize).saturating_sub(1).clamp(24, 110);
    let inner_w = max_w.saturating_sub(indent.len()).max(20);

    match l.kind {
        LineKind::User => {
            if has_leading_blank {
                out.push(Line::from(""));
            }
            let mut first = true;
            for part in l.text.lines() {
                let first_pfx = vec![
                    Span::styled(
                        format!("{indent}┃ "),
                        Style::default().fg(moonlit::DEEP),
                    ),
                    Span::styled(
                        if first { "❯ " } else { "  " }.to_string(),
                        Style::default()
                            .fg(moonlit::VIOLET)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                let cont_pfx = vec![
                    Span::styled(
                        format!("{indent}┃ "),
                        Style::default().fg(moonlit::DEEP),
                    ),
                    Span::raw("  "),
                ];
                let body = vec![Span::styled(
                    part.to_string(),
                    Style::default()
                        .fg(moonlit::SILVER)
                        .add_modifier(Modifier::BOLD),
                )];
                out.extend(markdown::wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
                first = false;
            }
        }
        LineKind::Assistant => {
            if !prev_assistant {
                if has_leading_blank {
                    out.push(Line::from(""));
                }
                let dashes = inner_w.saturating_sub(12).clamp(4, 56);
                out.push(Line::from(vec![
                    Span::styled(
                        format!("{indent}╭─ "),
                        Style::default().fg(moonlit::BORDER),
                    ),
                    Span::styled(
                        "☾ ".to_string(),
                        Style::default()
                            .fg(moonlit::VIOLET)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "Selena".to_string(),
                        Style::default()
                            .fg(moonlit::LILAC)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", "─".repeat(dashes)),
                        Style::default().fg(moonlit::BORDER),
                    ),
                ]));
            }
            out.extend(markdown::render_markdown_width(&l.text, width));
        }
        LineKind::Tool => {
            let (icon, color) = match l.tool_state {
                Some(ToolState::Pending) => ("◐", moonlit::GOLD),
                Some(ToolState::Ok) => ("●", moonlit::MINT),
                Some(ToolState::Err) => ("✖", moonlit::ROSE),
                None => ("●", moonlit::DIM),
            };
            if !show_tool_details {
                let label = l.tool_name.as_deref().unwrap_or("tool");
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}{icon} "), Style::default().fg(color)),
                    Span::styled(label.to_string(), Style::default().fg(moonlit::LAV)),
                    Span::styled(" (ringkas · /details)", Style::default().fg(moonlit::FAINT)),
                ]));
            } else {
                let mut first = true;
                for part in l.text.lines() {
                    let first_pfx = vec![Span::styled(
                        if first {
                            format!("{indent}{icon} ")
                        } else {
                            format!("{indent}  ")
                        },
                        Style::default().fg(color),
                    )];
                    let cont_pfx = vec![Span::raw(format!("{indent}  "))];
                    let part_style = if !first
                        && (part.starts_with("+++ ")
                            || part.starts_with("--- ")
                            || part.starts_with("diff --git "))
                    {
                        Style::default()
                            .fg(moonlit::CYAN_MOON)
                            .add_modifier(Modifier::BOLD)
                    } else if !first && part.starts_with("@@") {
                        Style::default()
                            .fg(moonlit::VIOLET)
                            .add_modifier(Modifier::BOLD)
                    } else if !first && part.starts_with('+') {
                        Style::default().fg(moonlit::MINT)
                    } else if !first && part.starts_with('-') {
                        Style::default().fg(moonlit::ROSE)
                    } else {
                        Style::default().fg(moonlit::DIM)
                    };
                    let body = vec![Span::styled(part.to_string(), part_style)];
                    out.extend(markdown::wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
                    first = false;
                }
            }
        }
        LineKind::Plan => {
            if has_leading_blank {
                out.push(Line::from(""));
            }
            for part in l.text.lines() {
                let pfx = vec![Span::styled(
                    format!("{indent}┃ "),
                    Style::default().fg(moonlit::VIOLET),
                )];
                let body = vec![Span::styled(
                    part.to_string(),
                    Style::default().fg(moonlit::LAV),
                )];
                out.extend(markdown::wrap_styled_spans(pfx.clone(), pfx, body, max_w));
            }
        }
        LineKind::Info => {
            let is_usage = l.text.starts_with("token sesi ini:");
            for (idx, part) in l.text.lines().enumerate() {
                if is_usage {
                    let first_pfx = vec![Span::styled(
                        format!("{indent}╰─ "),
                        Style::default().fg(moonlit::BORDER),
                    )];
                    let cont_pfx = vec![Span::raw(format!("{indent}   "))];
                    let body = vec![Span::styled(
                        part.to_string(),
                        Style::default().fg(moonlit::DIM),
                    )];
                    out.extend(markdown::wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
                } else {
                    let is_interrupt = part.starts_with('⚡');
                    let first_pfx = vec![Span::styled(
                        if idx == 0 {
                            format!("{indent}· ")
                        } else {
                            format!("{indent}  ")
                        },
                        Style::default().fg(if is_interrupt {
                            moonlit::GOLD
                        } else {
                            moonlit::VIOLET
                        }),
                    )];
                    let cont_pfx = vec![Span::raw(format!("{indent}  "))];
                    let body = vec![Span::styled(
                        part.to_string(),
                        Style::default().fg(if is_interrupt {
                            moonlit::LAV
                        } else {
                            moonlit::DIM
                        }),
                    )];
                    out.extend(markdown::wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
                }
            }
        }
        LineKind::Menu => {
            if has_leading_blank {
                out.push(Line::from(""));
            }
            out.extend(render_menu_card_width(&l.text, width));
        }
        LineKind::Error => {
            for (idx, part) in l.text.lines().enumerate() {
                let first_pfx = vec![Span::styled(
                    if idx == 0 {
                        format!("{indent}✖ [error] ")
                    } else {
                        format!("{indent}          ")
                    },
                    Style::default().fg(moonlit::ROSE),
                )];
                let cont_pfx = vec![Span::raw(format!("{indent}  "))];
                let body = vec![Span::styled(
                    part.to_string(),
                    Style::default().fg(moonlit::ROSE),
                )];
                out.extend(markdown::wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
            }
        }
    }

    l.cached_wrapped_rows = wrapped_line_count(&out, width);
    l.cached_lines = out;
    l.cached_width = width;
    l.cached_details = show_tool_details;
    l.cached_text_len = l.text.len();
    l.cached_tool_state = l.tool_state;
    l.cached_has_leading_blank = has_leading_blank;
    l.cached_prev_assistant = prev_assistant;
}

/// Kumpulkan HANYA potongan baris transkrip yang terlihat di viewport (`viewport_start..viewport_end`)
/// dengan memanfaatkan cache per-pesan (O(1) per pesan off-screen — super ringan untuk 100+ pesan!).
fn visible_transcript_slice(
    app: &mut App,
    width: u16,
    viewport: usize,
) -> (Vec<Line<'static>>, usize, u16) {
    let welcome = app.welcome.clone().unwrap_or_default();
    let welcome_rows = wrapped_line_count(&welcome, width);
    let mut has_prev_content = !welcome.is_empty();
    let mut prev_assistant = false;
    let show_details = app.show_tool_details;

    let mut total_rows = welcome_rows;
    for l in &mut app.lines {
        ensure_line_cached(l, width, show_details, has_prev_content, prev_assistant);
        total_rows += l.cached_wrapped_rows;
        if !l.cached_lines.is_empty() {
            has_prev_content = true;
        }
        prev_assistant = l.kind == LineKind::Assistant;
    }

    if total_rows == 0 {
        return (
            vec![Line::from(Span::styled(
                "  ☾ Ketik perintah untuk Selena…",
                Style::default().fg(moonlit::DIM),
            ))],
            1,
            0,
        );
    }

    let max_scroll = total_rows.saturating_sub(viewport);
    let from_bottom = (app.scroll as usize).min(max_scroll);
    let view_start = max_scroll - from_bottom;
    let view_end = view_start + viewport.max(1);

    let mut slice: Vec<Line<'static>> = Vec::new();
    let mut local_scroll = 0usize;
    let mut cursor = 0usize;
    let mut started = false;

    if welcome_rows > 0 {
        let end = cursor + welcome_rows;
        if end > view_start && cursor < view_end {
            local_scroll = view_start.saturating_sub(cursor);
            slice.extend(welcome);
            started = true;
        }
        cursor = end;
    }

    for l in &app.lines {
        let rows = l.cached_wrapped_rows;
        let end = cursor + rows;
        if end > view_start && cursor < view_end {
            if !started {
                local_scroll = view_start.saturating_sub(cursor);
                started = true;
            }
            slice.extend(l.cached_lines.iter().cloned());
        }
        cursor = end;
        if cursor >= view_end {
            break;
        }
    }

    (slice, total_rows, local_scroll.min(u16::MAX as usize) as u16)
}

/// Hitung jumlah baris visual sesudah wrap selebar `width`, agar auto-scroll
/// akurat di layar sempit (mis. Termux HP 52 kolom) dan teks terbaru di bawah
/// tidak tersembunyi di luar viewport.
fn wrapped_line_count(lines: &[Line], width: u16) -> usize {
    let w = width as usize;
    if w == 0 {
        return lines.len();
    }
    let mut total = 0usize;
    for line in lines {
        let lw = line.width();
        if lw <= w {
            total += 1;
            continue;
        }
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let mut rows = 1usize;
        let mut col = 0usize;
        for seg in text.split_inclusive(' ') {
            let sw = seg.chars().count();
            if col > 0 && col + sw > w {
                rows += 1;
                col = 0;
            }
            if sw > w {
                let extra = (col + sw).saturating_sub(1) / w;
                rows += extra;
                col = (col + sw) % w;
                if col == 0 {
                    col = w;
                }
            } else {
                col += sw;
            }
        }
        total += rows;
    }
    total
}

/// Gambar 1 frame. Publik agar bisa dites dengan TestBackend.
///
/// Tata letak Moonlit (v0.1.2): TANPA header terpisah — sambutan Moonlit
/// tinggal di awal transkrip (dibuat sekali pada frame pertama, saat
/// lebar terminal diketahui), status bar Moonlit selalu terlihat di
/// bawah, lalu kotak input berbingkai DEEP dengan prompt "❯".
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if app.welcome.is_none() {
        let info = app.welcome_info();
        app.welcome = Some(if app.resumed {
            moonlit::resume_lines(&info)
        } else {
            moonlit::welcome_lines(area.width, &info)
        });
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transkrip (termasuk sambutan)
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input
        ])
        .split(area);

    // Transkrip (gulir: app.scroll = jarak dari bawah) — hanya render potongan viewport!
    let viewport = chunks[0].height as usize;
    let (visible_lines, total, local_y) = visible_transcript_slice(app, chunks[0].width, viewport);
    app.last_total_lines = total;
    app.last_viewport = viewport;
    let transcript = Paragraph::new(visible_lines)
        .wrap(Wrap { trim: false })
        .scroll((local_y, 0));
    frame.render_widget(transcript, chunks[0]);

    // Status bar Moonlit responsif: model + kapasitas context window AI + token + langkah.
    let status = Paragraph::new(moonlit::status_line_responsive(
        chunks[1].width,
        &app.model,
        app.context_pct,
        app.context_used,
        app.context_limit,
        app.session_in,
        app.session_out,
        app.step_current,
        app.step_max,
        &app.cwd,
        app.busy,
    ));
    frame.render_widget(status, chunks[1]);

    // Input: bingkai BORDER (atau VIOLET saat sibuk + petunjuk interupsi ala OpenCode/Hermes).
    let mut input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.busy {
            moonlit::VIOLET
        } else {
            moonlit::BORDER
        }));
    if app.busy {
        let busy_title = if area.width < 68 {
            " ⚡ bekerja… ketik untuk interupsi · /stop "
        } else {
            " ⚡ Selena bekerja… ketik pesan + Enter untuk interupsi · Esc / /stop "
        };
        input_block = input_block
            .title(busy_title)
            .title_style(Style::default().fg(moonlit::GOLD).add_modifier(Modifier::BOLD));
    }
    let inner = input_block.inner(chunks[2]);
    frame.render_widget(input_block, chunks[2]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "❯",
            Style::default()
                .fg(moonlit::VIOLET)
                .add_modifier(Modifier::BOLD),
        )),
        cols[0],
    );
    app.input.set_block(Block::default());
    frame.render_widget(&app.input, cols[1]);
    // Kursor hanya relevan tanpa popup/modal.
    if app.approval.is_none() && app.model_picker.is_none() && app.setup_wizard.is_none() {
        frame.set_cursor_position((
            cols[1].x + app.input.cursor().1 as u16,
            cols[1].y + app.input.cursor().0 as u16,
        ));
    }

    // Live `/` command palette: muncul otomatis tepat di atas status bar saat
    // user mulai mengetik `/...` (tanpa popup approval/modal aktif).
    if app.approval.is_none() && app.model_picker.is_none() && app.setup_wizard.is_none() {
        let cur_input = app.input.lines().join("\n");
        let palette = filter_commands(&cur_input);
        if !palette.is_empty() && chunks[0].height >= 5 {
            if app.palette_idx >= palette.len() {
                app.palette_idx = 0;
            }
            let h = (palette.len() as u16 + 2)
                .min(chunks[0].height.saturating_sub(1))
                .min(12);
            let w = chunks[0].width.saturating_sub(2).min(86).max(24);
            let popup = Rect {
                x: chunks[0].x + 1,
                y: chunks[0].y + chunks[0].height.saturating_sub(h),
                width: w,
                height: h,
            };
            frame.render_widget(Clear, popup);
            let max_rows = h.saturating_sub(2) as usize;
            let start = if app.palette_idx >= max_rows {
                app.palette_idx + 1 - max_rows
            } else {
                0
            };
            let mut plines: Vec<Line<'static>> = Vec::new();
            for (idx, item) in palette.iter().enumerate().skip(start).take(max_rows) {
                let selected = idx == app.palette_idx;
                let bg = if selected {
                    Style::default().bg(Color::Rgb(42, 26, 78))
                } else {
                    Style::default()
                };
                let prefix = if selected {
                    Span::styled(
                        " ❯ ",
                        bg.fg(moonlit::GOLD).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("   ", bg)
                };
                let mut spans = vec![
                    prefix,
                    Span::styled(
                        format!("{:<10}", item.cmd),
                        bg.fg(if selected {
                            moonlit::LILAC
                        } else {
                            moonlit::LAV
                        })
                        .add_modifier(Modifier::BOLD),
                    ),
                ];
                if !item.args.is_empty() {
                    spans.push(Span::styled(
                        format!("{} ", item.args),
                        bg.fg(moonlit::CYAN_MOON),
                    ));
                }
                spans.push(Span::styled(
                    format!("· {}", item.desc),
                    bg.fg(if selected {
                        moonlit::SILVER
                    } else {
                        moonlit::DIM
                    }),
                ));
                plines.push(Line::from(spans));
            }
            let widget = Paragraph::new(plines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(moonlit::VIOLET))
                    .title_style(
                        Style::default()
                            .fg(moonlit::LILAC)
                            .add_modifier(Modifier::BOLD),
                    )
                    .title(" ☾ Menu Perintah (↑↓ pilih · Tab isi · Enter) "),
            );
            frame.render_widget(widget, popup);
        } else if let Some((_, at_q)) = app::extract_at_query(&cur_input) {
            let matches = app::filter_workspace_files(&app.workspace_files, &at_q, 10);
            if !matches.is_empty() && chunks[0].height >= 5 {
                if app.at_palette_idx >= matches.len() {
                    app.at_palette_idx = 0;
                }
                let h = (matches.len() as u16 + 2)
                    .min(chunks[0].height.saturating_sub(1))
                    .min(10);
                let w = chunks[0].width.saturating_sub(2).min(78).max(24);
                let popup = Rect {
                    x: chunks[0].x + 1,
                    y: chunks[0].y + chunks[0].height.saturating_sub(h),
                    width: w,
                    height: h,
                };
                frame.render_widget(Clear, popup);
                let max_rows = h.saturating_sub(2) as usize;
                let start = if app.at_palette_idx >= max_rows {
                    app.at_palette_idx + 1 - max_rows
                } else {
                    0
                };
                let mut flines: Vec<Line<'static>> = Vec::new();
                for (idx, path) in matches.iter().enumerate().skip(start).take(max_rows) {
                    let selected = idx == app.at_palette_idx;
                    let bg = if selected {
                        Style::default().bg(Color::Rgb(42, 26, 78))
                    } else {
                        Style::default()
                    };
                    let prefix = if selected {
                        Span::styled(" ❯ ", bg.fg(moonlit::GOLD).add_modifier(Modifier::BOLD))
                    } else {
                        Span::styled("   ", bg)
                    };
                    flines.push(Line::from(vec![
                        prefix,
                        Span::styled("@", bg.fg(moonlit::CYAN_MOON)),
                        Span::styled(
                            path.clone(),
                            bg.fg(if selected {
                                moonlit::LILAC
                            } else {
                                moonlit::LAV
                            })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                    ]));
                }
                let widget = Paragraph::new(flines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(moonlit::CYAN_MOON))
                        .title_style(
                            Style::default()
                                .fg(moonlit::LILAC)
                                .add_modifier(Modifier::BOLD),
                        )
                        .title(" ☾ Sisipkan File @ (↑↓ pilih · Tab/Enter isi) "),
                );
                frame.render_widget(widget, popup);
            }
        }
    }

    // Modal Wizard Setup Provider Moonlit Ungu (onboarding pertama kali & `/setup`)
    if let Some(wiz) = &app.setup_wizard {
        let pct_x = if area.width < 90 { 94 } else { 78 };
        let pct_y = if area.height < 26 { 88 } else { 72 };
        let popup = centered_rect(pct_x, pct_y, area);
        frame.render_widget(Clear, popup);

        let modal_bg = Color::Rgb(20, 15, 38);
        let row_hl = Color::Rgb(46, 28, 86);
        let (step_cur, step_total) = wiz.step_number();
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(moonlit::VIOLET))
            .style(Style::default().bg(modal_bg))
            .title_style(
                Style::default()
                    .fg(moonlit::LILAC)
                    .add_modifier(Modifier::BOLD),
            )
            .title(format!(
                " ☾ Setup Provider SelenCode ({step_cur}/{step_total}) "
            ));
        let inner = outer.inner(popup);
        frame.render_widget(outer, popup);

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // judul langkah & info
                Constraint::Min(5),    // isi langkah (daftar preset / kotak ketik)
                Constraint::Length(2), // footer tombol navigasi
            ])
            .split(inner);

        let (step_title, step_hint) = match wiz.step {
            SetupStep::ChooseApi => (
                "Pilih Jenis API / Provider",
                "Pilih API compatible atau preset — ↑↓ pilih lalu Enter",
            ),
            SetupStep::ProviderName => (
                "Beri Nama Provider",
                "Nama singkat tanpa spasi (mis. router, openrouter, claude)",
            ),
            SetupStep::BaseUrl => (
                "Masukkan Base URL Endpoint (OpenAI-Compatible)",
                "Contoh: https://router.ryukomik.com/v1 atau https://openrouter.ai/api/v1",
            ),
            SetupStep::ApiKey => (
                "Masukkan API Key",
                if wiz.needs_key {
                    "Paste API key (sk-...) langsung atau nama ENV var · Tab: lihat/sembunyi"
                } else {
                    "Opsional untuk Ollama lokal — cukup tekan Enter untuk lanjut"
                },
            ),
            SetupStep::ModelId => (
                "Masukkan ID Model Utama",
                "Contoh: ag/gemini-4-neon-low · claude-sonnet-4 · gpt-4.1",
            ),
        };

        let header_lines = vec![
            Line::from(vec![
                Span::styled(" ✦ ", Style::default().fg(moonlit::GOLD)),
                Span::styled(
                    step_title.to_string(),
                    Style::default()
                        .fg(moonlit::LILAC)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                format!("   {step_hint}"),
                Style::default().fg(moonlit::DIM),
            )),
            Line::from(Span::styled(
                "─".repeat(sections[0].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
        ];
        frame.render_widget(
            Paragraph::new(header_lines).style(Style::default().bg(modal_bg)),
            sections[0],
        );

        let mut body_lines: Vec<Line<'static>> = Vec::new();
        if let Some(notice) = &wiz.notice {
            if wiz.step == SetupStep::ChooseApi {
                body_lines.push(Line::from(vec![
                    Span::styled(" ℹ ", Style::default().fg(moonlit::CYAN_MOON)),
                    Span::styled(notice.clone(), Style::default().fg(moonlit::LAV)),
                ]));
                body_lines.push(Line::from(""));
            }
        }
        match wiz.step {
            SetupStep::ChooseApi => {
                let body_h = sections[1].height as usize;
                let max_items = body_h.saturating_sub(if wiz.notice.is_some() { 2 } else { 0 }).max(1);
                let start = if SETUP_PRESETS.len() > max_items && wiz.selected >= max_items {
                    wiz.selected + 1 - max_items
                } else {
                    0
                };
                let wide = sections[1].width >= 60;
                for (idx, p) in SETUP_PRESETS
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(max_items)
                {
                    let is_sel = idx == wiz.selected;
                    let bg = if is_sel {
                        Style::default().bg(row_hl)
                    } else {
                        Style::default().bg(modal_bg)
                    };
                    let mut spans = vec![
                        Span::styled(
                            if is_sel { " ▸ " } else { "   " },
                            bg.fg(moonlit::LILAC),
                        ),
                        Span::styled(
                            format!("{}. ", idx + 1),
                            bg.fg(moonlit::VIOLET).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            p.label.to_string(),
                            bg.fg(if is_sel {
                                moonlit::LILAC
                            } else {
                                moonlit::SILVER
                            })
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                    ];
                    if wide {
                        spans.push(Span::styled("  ·  ", bg.fg(moonlit::FAINT)));
                        spans.push(Span::styled(p.desc.to_string(), bg.fg(moonlit::DIM)));
                    }
                    body_lines.push(Line::from(spans));
                }
            }
            SetupStep::ProviderName
            | SetupStep::BaseUrl
            | SetupStep::ApiKey
            | SetupStep::ModelId => {
                let (label, value) = match wiz.step {
                    SetupStep::ProviderName => ("Nama provider", wiz.name.clone()),
                    SetupStep::BaseUrl => ("Base URL", wiz.base_url.clone()),
                    SetupStep::ApiKey => ("API Key", wiz.masked_key()),
                    SetupStep::ModelId => ("Model utama", wiz.model.clone()),
                    SetupStep::ChooseApi => unreachable!(),
                };
                body_lines.push(Line::from(""));
                body_lines.push(Line::from(vec![
                    Span::styled(
                        format!("   {label}: "),
                        Style::default().fg(moonlit::LAV).add_modifier(Modifier::BOLD),
                    ),
                ]));
                body_lines.push(Line::from(vec![
                    Span::styled("   ❯ ", Style::default().fg(moonlit::VIOLET).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{value}█"),
                        Style::default()
                            .bg(row_hl)
                            .fg(moonlit::MINT)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                body_lines.push(Line::from(""));
                // Ringkasan field yang sudah diisi sejauh ini
                body_lines.push(Line::from(vec![
                    Span::styled("   Ringkasan: ", Style::default().fg(moonlit::FAINT)),
                    Span::styled(
                        format!("kind={}", wiz.kind),
                        Style::default().fg(moonlit::DIM),
                    ),
                    Span::styled(
                        if !wiz.name.is_empty() && wiz.step != SetupStep::ProviderName {
                            format!(" · nama={}", wiz.name)
                        } else {
                            String::new()
                        },
                        Style::default().fg(moonlit::DIM),
                    ),
                    Span::styled(
                        if !wiz.base_url.is_empty()
                            && matches!(wiz.step, SetupStep::ApiKey | SetupStep::ModelId)
                        {
                            format!(" · url={}", wiz.base_url)
                        } else {
                            String::new()
                        },
                        Style::default().fg(moonlit::DIM),
                    ),
                ]));
            }
        }
        if let Some(err) = &wiz.error {
            body_lines.push(Line::from(""));
            body_lines.push(Line::from(vec![
                Span::styled("   ✖ ", Style::default().fg(moonlit::ROSE)),
                Span::styled(err.clone(), Style::default().fg(moonlit::ROSE)),
            ]));
        }
        frame.render_widget(
            Paragraph::new(body_lines)
                .style(Style::default().bg(modal_bg))
                .wrap(Wrap { trim: false }),
            sections[1],
        );

        let pill_ok = Style::default()
            .bg(moonlit::DEEP)
            .fg(moonlit::MINT)
            .add_modifier(Modifier::BOLD);
        let pill_alt = Style::default()
            .bg(row_hl)
            .fg(moonlit::LILAC)
            .add_modifier(Modifier::BOLD);
        let pill_no = Style::default()
            .bg(Color::Rgb(68, 22, 34))
            .fg(moonlit::ROSE)
            .add_modifier(Modifier::BOLD);
        let next_label = if wiz.step == SetupStep::ModelId {
            " [Enter] simpan & pakai "
        } else {
            " [Enter] lanjut "
        };
        let back_label = if wiz.step == SetupStep::ChooseApi {
            " [Esc] tutup "
        } else {
            " [Esc] kembali "
        };
        let mut fspans = vec![
            Span::raw(" "),
            Span::styled(next_label, pill_ok),
            Span::raw("  "),
            Span::styled(back_label, pill_no),
        ];
        if wiz.step == SetupStep::ApiKey {
            fspans.push(Span::raw("  "));
            fspans.push(Span::styled(" [Tab] tampilkan key ", pill_alt));
        }
        let footer_lines = vec![
            Line::from(Span::styled(
                "─".repeat(sections[2].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
            Line::from(fspans),
        ];
        frame.render_widget(
            Paragraph::new(footer_lines).style(Style::default().bg(modal_bg)),
            sections[2],
        );
    }

    // Modal Pemilih Model Interaktif Moonlit Ungu (ala Hermes: pilih 1-per-1, tambah manual, hapus)
    if let Some(mp) = &app.model_picker {
        let pct_x = if area.width < 90 { 94 } else { 80 };
        let pct_y = if area.height < 26 { 84 } else { 72 };
        let popup = centered_rect(pct_x, pct_y, area);
        frame.render_widget(Clear, popup);

        let modal_bg = Color::Rgb(20, 15, 38);
        let row_hl = Color::Rgb(46, 28, 86);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(moonlit::VIOLET))
            .style(Style::default().bg(modal_bg))
            .title_style(
                Style::default()
                    .fg(moonlit::LILAC)
                    .add_modifier(Modifier::BOLD),
            )
            .title(format!(" ☾ Kelola Model · {} ", mp.provider));
        let inner = outer.inner(popup);
        frame.render_widget(outer, popup);

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // tab & info provider / status
                Constraint::Min(4),    // daftar model atau input manual
                Constraint::Length(2), // footer tombol shortcut
            ])
            .split(inner);

        let tab_saved_style = if mp.mode != ModelPickerMode::Catalog {
            Style::default()
                .bg(row_hl)
                .fg(moonlit::LILAC)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(modal_bg).fg(moonlit::DIM)
        };
        let tab_cat_style = if mp.mode == ModelPickerMode::Catalog {
            Style::default()
                .bg(row_hl)
                .fg(moonlit::CYAN_MOON)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(modal_bg).fg(moonlit::DIM)
        };
        let cat_count = if mp.loading_remote {
            "memuat…".to_string()
        } else {
            mp.remote_models.len().to_string()
        };
        let status_line = match mp.mode {
            ModelPickerMode::AddManual => Line::from(vec![
                Span::styled(" ✦ ", Style::default().fg(moonlit::GOLD)),
                Span::styled("Tambah 1 model manual: ", Style::default().fg(moonlit::LAV)),
                Span::styled(
                    format!("{}█", mp.manual_input),
                    Style::default()
                        .fg(moonlit::MINT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            ModelPickerMode::Catalog => {
                let mut spans = vec![
                    Span::styled(" ✦ ", Style::default().fg(moonlit::CYAN_MOON)),
                    Span::styled("Cari katalog: ", Style::default().fg(moonlit::DIM)),
                    Span::styled(
                        if mp.filter_input.is_empty() {
                            "(ketik huruf untuk filter…)"
                        } else {
                            mp.filter_input.as_str()
                        },
                        Style::default().fg(if mp.filter_input.is_empty() {
                            moonlit::FAINT
                        } else {
                            moonlit::LILAC
                        }),
                    ),
                    Span::styled("█", Style::default().fg(moonlit::VIOLET)),
                ];
                if let Some(st) = &mp.status {
                    spans.push(Span::styled("   ·   ", Style::default().fg(moonlit::FAINT)));
                    spans.push(Span::styled(st.clone(), Style::default().fg(moonlit::MINT)));
                }
                Line::from(spans)
            }
            ModelPickerMode::Saved => {
                if let Some(st) = &mp.status {
                    Line::from(vec![
                        Span::styled(" ✦ ", Style::default().fg(moonlit::MINT)),
                        Span::styled(st.clone(), Style::default().fg(moonlit::LAV)),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(" ✦ ", Style::default().fg(moonlit::GOLD)),
                        Span::styled("Aktif: ", Style::default().fg(moonlit::DIM)),
                        Span::styled(
                            mp.active_model.clone(),
                            Style::default()
                                .fg(moonlit::CYAN_MOON)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                }
            }
        };
        let prov_hint = if mp.providers.len() > 1 {
            format!("  ·  Tab: provider ({})", mp.providers.len())
        } else {
            String::new()
        };
        let header_lines = vec![
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!(" Model Saya ({}) ", mp.saved_models.len()),
                    tab_saved_style,
                ),
                Span::raw("  "),
                Span::styled(format!(" Katalog API ({cat_count}) "), tab_cat_style),
                Span::styled(prov_hint, Style::default().fg(moonlit::FAINT)),
            ]),
            status_line,
            Line::from(Span::styled(
                "─".repeat(sections[0].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
        ];
        frame.render_widget(
            Paragraph::new(header_lines).style(Style::default().bg(modal_bg)),
            sections[0],
        );

        let body_h = sections[1].height as usize;
        let mut body_lines: Vec<Line<'static>> = Vec::new();
        match mp.mode {
            ModelPickerMode::AddManual => {
                body_lines.push(Line::from(""));
                body_lines.push(Line::from(vec![
                    Span::styled("   › ", Style::default().fg(moonlit::VIOLET)),
                    Span::styled(
                        "Ketik ID model yang ingin ditambahkan ke daftar pribadimu, lalu tekan Enter.",
                        Style::default().fg(moonlit::SILVER),
                    ),
                ]));
                body_lines.push(Line::from(vec![
                    Span::styled("   · ", Style::default().fg(moonlit::DIM)),
                    Span::styled(
                        "Contoh: ag/claude-sonnet-4  ·  ag/gemini-2.5-pro  ·  gpt-4.1",
                        Style::default().fg(moonlit::DIM),
                    ),
                ]));
                body_lines.push(Line::from(""));
                body_lines.push(Line::from(vec![
                    Span::styled("   · ", Style::default().fg(moonlit::MINT)),
                    Span::styled(
                        "Model akan masuk ke daftar bernomor 1-per-1 (tidak menimpa model lain).",
                        Style::default().fg(moonlit::LAV),
                    ),
                ]));
            }
            ModelPickerMode::Saved => {
                let total = mp.saved_models.len();
                let start = if total > body_h && body_h > 0 && mp.selected >= body_h {
                    mp.selected + 1 - body_h
                } else {
                    0
                };
                for (idx, m) in mp
                    .saved_models
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(body_h.max(1))
                {
                    let is_sel = idx == mp.selected;
                    let is_act = *m == mp.active_model;
                    let bg = if is_sel {
                        Style::default().bg(row_hl)
                    } else {
                        Style::default().bg(modal_bg)
                    };
                    let marker = if is_sel { "▸ " } else { "  " };
                    let num = format!("#{:<2} ", idx + 1);
                    let badge = if is_act { "  ● aktif" } else { "" };
                    body_lines.push(Line::from(vec![
                        Span::styled(format!(" {marker}"), bg.fg(moonlit::LILAC)),
                        Span::styled(num, bg.fg(moonlit::VIOLET).add_modifier(Modifier::BOLD)),
                        Span::styled(
                            m.clone(),
                            bg.fg(if is_sel || is_act {
                                moonlit::LILAC
                            } else {
                                moonlit::SILVER
                            })
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                        Span::styled(badge, bg.fg(moonlit::MINT).add_modifier(Modifier::BOLD)),
                    ]));
                }
            }
            ModelPickerMode::Catalog => {
                if mp.loading_remote {
                    body_lines.push(Line::from(Span::styled(
                        "  ☾ Menarik daftar model dari endpoint API…",
                        Style::default().fg(moonlit::LAV),
                    )));
                } else {
                    let items = mp.filtered_remote();
                    if items.is_empty() {
                        body_lines.push(Line::from(Span::styled(
                            "  (tidak ada model di katalog yang cocok — tekan Esc untuk kembali)",
                            Style::default().fg(moonlit::DIM),
                        )));
                    } else {
                        let total = items.len();
                        let start = if total > body_h && body_h > 0 && mp.selected >= body_h {
                            mp.selected + 1 - body_h
                        } else {
                            0
                        };
                        for (idx, m) in items
                            .iter()
                            .enumerate()
                            .skip(start)
                            .take(body_h.max(1))
                        {
                            let is_sel = idx == mp.selected;
                            let saved = mp.is_saved(m.as_str());
                            let bg = if is_sel {
                                Style::default().bg(row_hl)
                            } else {
                                Style::default().bg(modal_bg)
                            };
                            let check = if saved { "[✓] " } else { "[ ] " };
                            let check_color = if saved { moonlit::MINT } else { moonlit::DIM };
                            body_lines.push(Line::from(vec![
                                Span::styled(
                                    if is_sel { " ▸ " } else { "   " },
                                    bg.fg(moonlit::LILAC),
                                ),
                                Span::styled(check, bg.fg(check_color).add_modifier(Modifier::BOLD)),
                                Span::styled(
                                    m.to_string(),
                                    bg.fg(if is_sel {
                                        moonlit::LILAC
                                    } else {
                                        moonlit::SILVER
                                    }),
                                ),
                            ]));
                        }
                    }
                }
            }
        }
        frame.render_widget(
            Paragraph::new(body_lines).style(Style::default().bg(modal_bg)),
            sections[1],
        );

        let pill_ok = Style::default()
            .bg(moonlit::DEEP)
            .fg(moonlit::MINT)
            .add_modifier(Modifier::BOLD);
        let pill_alt = Style::default()
            .bg(row_hl)
            .fg(moonlit::LILAC)
            .add_modifier(Modifier::BOLD);
        let pill_del = Style::default()
            .bg(Color::Rgb(68, 22, 34))
            .fg(moonlit::ROSE)
            .add_modifier(Modifier::BOLD);
        let footer_spans = match mp.mode {
            ModelPickerMode::AddManual => vec![
                Span::raw(" "),
                Span::styled(" [Enter] simpan ", pill_ok),
                Span::raw("  "),
                Span::styled(" [Esc] batal ", pill_del),
            ],
            ModelPickerMode::Catalog => vec![
                Span::raw(" "),
                Span::styled(" [Space] pilih 1-per-1 ", pill_alt),
                Span::raw(" "),
                Span::styled(" [Enter] pakai ", pill_ok),
                Span::raw(" "),
                Span::styled(" [Esc] kembali ", pill_del),
            ],
            ModelPickerMode::Saved => {
                if sections[2].width >= 68 {
                    vec![
                        Span::raw(" "),
                        Span::styled(" [Enter] pakai ", pill_ok),
                        Span::raw(" "),
                        Span::styled(" [a] + manual ", pill_alt),
                        Span::raw(" "),
                        Span::styled(" [f] katalog ", pill_alt),
                        Span::raw(" "),
                        Span::styled(" [d] hapus ", pill_del),
                        Span::raw(" "),
                        Span::styled(" [c] reset ", pill_del),
                    ]
                } else {
                    vec![
                        Span::raw(" "),
                        Span::styled(" [Enter] pakai ", pill_ok),
                        Span::raw(" "),
                        Span::styled(" [a]+ ", pill_alt),
                        Span::raw(" "),
                        Span::styled(" [f]API ", pill_alt),
                        Span::raw(" "),
                        Span::styled(" [d]rm ", pill_del),
                        Span::raw(" "),
                        Span::styled(" [c]clr ", pill_del),
                    ]
                }
            }
        };
        let footer_lines = vec![
            Line::from(Span::styled(
                "─".repeat(sections[2].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
            Line::from(footer_spans),
        ];
        frame.render_widget(
            Paragraph::new(footer_lines).style(Style::default().bg(modal_bg)),
            sections[2],
        );
    }

    // Modal Persetujuan Moonlit Ungu (modern, scrollable, dengan opsi [a] selalu sesi ini)
    if let Some(p) = &app.approval {
        let pct_x = if area.width < 90 { 92 } else { 78 };
        let pct_y = if area.height < 26 { 80 } else { 66 };
        let popup = centered_rect(pct_x, pct_y, area);
        frame.render_widget(Clear, popup);

        let modal_bg = Color::Rgb(20, 15, 38);
        let category = tool_category_from_title(&p.title);
        let pending_total = app.approval_pending_count();
        let queue_badge = if pending_total > 1 {
            format!(" · 1/{pending_total} antre")
        } else {
            String::new()
        };

        let outer = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(moonlit::VIOLET))
            .style(Style::default().bg(modal_bg))
            .title_style(
                Style::default()
                    .fg(moonlit::LILAC)
                    .add_modifier(Modifier::BOLD),
            )
            .title(format!(" ☾ Persetujuan: {} ", p.title));
        let inner = outer.inner(popup);
        frame.render_widget(outer, popup);

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // header kategori & ringkasan
                Constraint::Min(3),    // isi diff / perintah (scrollable)
                Constraint::Length(2), // footer tombol aksi
            ])
            .split(inner);

        // Header info kategori + badge
        let header_lines = vec![
            Line::from(vec![
                Span::styled(" ✦ ", Style::default().fg(moonlit::GOLD)),
                Span::styled("Tool: ", Style::default().fg(moonlit::DIM)),
                Span::styled(
                    category.clone(),
                    Style::default()
                        .fg(moonlit::CYAN_MOON)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(queue_badge, Style::default().fg(moonlit::LAV)),
                Span::styled(
                    "   ·   ↑↓ gulir detail",
                    Style::default().fg(moonlit::FAINT),
                ),
            ]),
            Line::from(Span::styled(
                "─".repeat(sections[0].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
        ];
        frame.render_widget(
            Paragraph::new(header_lines).style(Style::default().bg(modal_bg)),
            sections[0],
        );

        // Isi detail/diff berwarna (hijau/merah untuk diff, lavender untuk perintah)
        let body = if p.detail.trim().is_empty() {
            vec![Line::from(vec![
                Span::styled("  › ", Style::default().fg(moonlit::VIOLET)),
                Span::styled(p.title.clone(), Style::default().fg(moonlit::SILVER)),
            ])]
        } else {
            diff_view::detail_lines(&p.detail)
        };
        let body_widget = Paragraph::new(body)
            .style(Style::default().bg(modal_bg))
            .wrap(Wrap { trim: false })
            .scroll((app.approval_scroll, 0));
        frame.render_widget(body_widget, sections[1]);

        // Footer pill tombol aksi modern
        let pill_ok = Style::default()
            .bg(moonlit::DEEP)
            .fg(moonlit::MINT)
            .add_modifier(Modifier::BOLD);
        let pill_all = Style::default()
            .bg(Color::Rgb(46, 28, 86))
            .fg(moonlit::LILAC)
            .add_modifier(Modifier::BOLD);
        let pill_no = Style::default()
            .bg(Color::Rgb(68, 22, 34))
            .fg(moonlit::ROSE)
            .add_modifier(Modifier::BOLD);
        let always_label = if sections[2].width >= 60 {
            format!(" [a] selalu {category} sesi ini ")
        } else if sections[2].width >= 50 {
            " [a] selalu sesi ini ".to_string()
        } else {
            " [a] selalu ".to_string()
        };
        let gap = if sections[2].width >= 50 { "  " } else { " " };
        let footer_lines = vec![
            Line::from(Span::styled(
                "─".repeat(sections[2].width as usize),
                Style::default().fg(moonlit::FAINT),
            )),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(" [y] setuju ", pill_ok),
                Span::raw(gap),
                Span::styled(always_label, pill_all),
                Span::raw(gap),
                Span::styled(" [n] tolak ", pill_no),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(footer_lines).style(Style::default().bg(modal_bg)),
            sections[2],
        );
    }

    // Modal pertanyaan klarifikasi `ask_user` (Bagian 2 v0.5.6 — visual & jalur terpisah dari approval!).
    if let Some(ask) = &app.pending_ask {
        let area = centered_rect(76, 56, frame.area());
        frame.render_widget(Clear, area);
        let modal_bg = moonlit::CARD;
        let block = Block::default()
            .title(" Pertanyaan dari Selena (Klarifikasi) ")
            .title_style(
                Style::default()
                    .fg(moonlit::LAVENDER)
                    .add_modifier(Modifier::BOLD),
            )
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(moonlit::VIOLET))
            .style(Style::default().bg(modal_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(ctx) = ask.context.as_deref().filter(|s| !s.trim().is_empty()) {
            lines.push(Line::from(Span::styled(
                format!("  {ctx}"),
                Style::default().fg(moonlit::DIM),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("  {}", ask.question),
            Style::default()
                .fg(moonlit::STARLIGHT)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for (idx, choice) in ask.choices.iter().enumerate() {
            let active = !ask.typing_custom && ask.selected == idx;
            let prefix = if active { "  > " } else { "    " };
            let style = if active {
                Style::default()
                    .fg(moonlit::MINT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(moonlit::SILVER)
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}. {choice}", idx + 1),
                style,
            )));
        }
        let custom_idx = ask.choices.len();
        let custom_active = ask.typing_custom || ask.selected == custom_idx;
        let custom_prefix = if custom_active { "  > " } else { "    " };
        let custom_label = if ask.typing_custom {
            format!("{custom_prefix}Jawaban sendiri: {}_", ask.custom_input)
        } else {
            format!("{custom_prefix}Ketik jawaban sendiri… (Enter / ketik langsung)")
        };
        lines.push(Line::from(Span::styled(
            custom_label,
            if custom_active {
                Style::default().fg(moonlit::LILAC).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(moonlit::DIM)
            },
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↑/↓] pilih · [Enter] kirim · [Esc] lewati (lanjut dengan asumsi)",
            Style::default().fg(moonlit::DIM),
        )));

        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(modal_bg))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

// ---------- jaga terminal ----------

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = stdout();
    let _ = out.execute(crossterm::event::DisableMouseCapture);
    let _ = out.execute(LeaveAlternateScreen);
}

/// Pasang panic hook yang memulihkan terminal sebelum panic diteruskan.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

// ---------- loop UI (blocking) ----------

fn send_cmd(app: &mut App, cmd_tx: &mpsc::Sender<TuiCommand>, cmd: TuiCommand) {
    if let Err(e) = cmd_tx.try_send(cmd) {
        app.error(format!("Gagal mengirim perintah ke worker: {e}"));
    }
}

fn submit(
    app: &mut App,
    cmd_tx: &mpsc::Sender<TuiCommand>,
    cancel: &Arc<Notify>,
    interrupt_pending: &Arc<AtomicBool>,
) {
    let raw = app.take_input();
    let text = raw.trim().to_string();
    if text.is_empty() {
        return;
    }
    app.push_history(&text);
    match parse_slash_command(&text) {
        SlashCommand::Exit => {
            app.should_quit = true;
        }
        SlashCommand::Stop => {
            interrupt_pending.store(false, Ordering::SeqCst);
            let (was_busy, dropped, had_plan) = app.stop_all_active();
            cancel.notify_waiters();
            if was_busy || dropped > 0 || had_plan {
                app.menu(format!(
                    "# Semua Task Agent Dihentikan\n\
                     * status | dihentikan | giliran aktif & sub-task dibatalkan\n\
                     > approval | {dropped} ditolak | rencana tertunda dibersihkan"
                ));
            } else {
                app.info("Tidak ada task agent yang sedang berjalan.");
            }
        }
        SlashCommand::InlineShell(cmd) => {
            if app.busy {
                app.error("Tunggu giliran saat ini selesai atau tekan Esc untuk membatalkan.");
            } else {
                send_cmd(app, cmd_tx, TuiCommand::InlineShell(cmd));
            }
        }
        SlashCommand::Help => {
            app.menu(help_menu_markup());
        }
        SlashCommand::Model(None) => {
            send_cmd(app, cmd_tx, TuiCommand::ModelPicker(ModelPickerCmd::Open));
        }
        SlashCommand::Model(Some(arg)) => {
            send_cmd(app, cmd_tx, TuiCommand::SwitchOrSetModel(arg));
        }
        SlashCommand::Models(filter) => {
            send_cmd(app, cmd_tx, TuiCommand::FetchRemoteModels(filter));
        }
        SlashCommand::Provider(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::ManageProvider(arg));
        }
        SlashCommand::Setup => {
            app.setup_wizard = Some(SetupWizardState::new(None));
        }
        SlashCommand::Telegram(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Telegram(arg));
        }
        SlashCommand::Undo => {
            send_cmd(app, cmd_tx, TuiCommand::Undo);
        }
        SlashCommand::Redo => {
            send_cmd(app, cmd_tx, TuiCommand::Redo);
        }
        SlashCommand::Checkpoint(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Checkpoint(arg));
        }
        SlashCommand::Rollback(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Rollback(arg));
        }
        SlashCommand::Worktree(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Worktree(arg));
        }
        SlashCommand::Mcp => {
            send_cmd(app, cmd_tx, TuiCommand::McpStatus);
        }
        SlashCommand::Doctor => {
            send_cmd(app, cmd_tx, TuiCommand::Doctor);
        }
        SlashCommand::Verify(arg) => {
            if app.busy {
                app.error("Tunggu giliran saat ini selesai atau gunakan /stop dulu.");
            } else {
                send_cmd(app, cmd_tx, TuiCommand::Verify(arg));
            }
        }
        SlashCommand::Index(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Index(arg));
        }
        SlashCommand::Jobs(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Jobs(arg));
        }
        SlashCommand::Route(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Route(arg));
        }
        SlashCommand::Lsp(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Lsp(arg));
        }
        SlashCommand::Task(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Task(arg));
        }
        SlashCommand::Memory(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Memory(arg));
        }
        SlashCommand::Skills(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Skills(arg));
        }
        SlashCommand::Review(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Review(arg));
        }
        SlashCommand::Cron(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Cron(arg));
        }
        SlashCommand::Search(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Search(arg));
        }
        SlashCommand::Skill(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::UseSkill(arg));
        }
        SlashCommand::Learn(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::LearnSkill(arg));
        }
        SlashCommand::Init => {
            send_cmd(app, cmd_tx, TuiCommand::InitAgents);
        }
        SlashCommand::Export(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::ExportChat(arg));
        }
        SlashCommand::Approve(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::SetApproval(arg));
        }
        SlashCommand::Sandbox(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Sandbox(arg));
        }
        SlashCommand::Details => {
            app.show_tool_details = !app.show_tool_details;
            let mode = if app.show_tool_details {
                "detail (argumen & ringkasan tampil)"
            } else {
                "ringkas (hanya nama & status tool)"
            };
            app.menu(format!(
                "# Tampilan Tool Diubah\n* mode | {mode} | jalankan /details lagi untuk mengganti"
            ));
        }
        SlashCommand::New => {
            if app.busy {
                app.error("Tunggu giliran saat ini selesai atau tekan Esc untuk membatalkan.");
            } else {
                send_cmd(app, cmd_tx, TuiCommand::NewSession);
            }
        }
        SlashCommand::Compact => {
            if app.busy {
                app.error("Tunggu giliran saat ini selesai sebelum /compact.");
            } else {
                send_cmd(app, cmd_tx, TuiCommand::CompactContext);
            }
        }
        SlashCommand::Sessions => {
            send_cmd(app, cmd_tx, TuiCommand::ShowSessions);
        }
        SlashCommand::Resume(arg) => {
            let is_switch = arg
                .as_deref()
                .map(|s| {
                    let t = s.trim();
                    !t.is_empty() && !t.eq_ignore_ascii_case("info") && !t.starts_with("rename ")
                })
                .unwrap_or(false);
            if is_switch && app.busy {
                app.error("Tunggu giliran saat ini selesai atau gunakan /stop sebelum pindah sesi.");
            } else {
                send_cmd(app, cmd_tx, TuiCommand::Resume(arg));
            }
        }
        SlashCommand::Plan => {
            app.plan_next = true;
            app.menu(
                "# Mode Plan Aktif\n\
                 ! Giliran berikutnya berjalan baca-saja dan diakhiri rencana untuk disetujui sebelum eksekusi.",
            );
        }
        SlashCommand::Unknown(name) => {
            app.error(format!(
                "Perintah /{name} tidak dikenal. Ketik /help atau / untuk melihat daftar perintah."
            ));
        }
        SlashCommand::NotCommand => {
            // Interupsi mulus ala OpenCode & Hermes: saat Selena sibuk, mengirim pesan
            // baru langsung menghentikan giliran/tool aktif dan memproses instruksi baru!
            if app.busy {
                let dropped = app.cancel_all_approvals();
                app.pending_plan = None;
                app.plan_next = false;
                if dropped > 0 {
                    app.info(format!(
                        "⚡ Menginterupsi giliran aktif ({dropped} approval dibatalkan) — melanjutkan instruksi baru…"
                    ));
                } else {
                    app.info("⚡ Menginterupsi giliran aktif — melanjutkan instruksi baru…");
                }
                app.push_user(&text);
                app.busy = true;
                // Urutan penting: tandai & kirim perintah dulu, BARU picu cancel.notify_waiters()
                // agar worker tidak sempat mengirim TurnDone palsu sebelum giliran baru mulai.
                interrupt_pending.store(true, Ordering::SeqCst);
                send_cmd(app, cmd_tx, TuiCommand::InterruptPrompt(text));
                cancel.notify_waiters();
                return;
            }
            // Konfirmasi rencana yang menunggu (Fase 6): input berikutnya
            // harus y/n sebelum prompt baru diterima.
            if app.pending_plan.is_some() {
                match text.to_lowercase().as_str() {
                    "y" | "yes" => {
                        let plan = app.pending_plan.take().unwrap_or_default();
                        app.push_user(&text);
                        send_cmd(app, cmd_tx, TuiCommand::ExecutePlan(plan));
                    }
                    "n" | "no" => {
                        app.pending_plan = None;
                        app.info("Rencana dibuang.");
                    }
                    _ => {
                        app.error("Jawab y atau n dulu untuk rencana yang menunggu.");
                    }
                }
                return;
            }
            app.push_user(&text);
            if app.plan_next {
                app.plan_next = false;
                send_cmd(app, cmd_tx, TuiCommand::PromptPlan(text));
            } else {
                send_cmd(app, cmd_tx, TuiCommand::Prompt(text));
            }
        }
    }
}

fn handle_key(
    app: &mut App,
    cmd_tx: &mpsc::Sender<TuiCommand>,
    cancel: &Arc<Notify>,
    interrupt_pending: &Arc<AtomicBool>,
    key: KeyEvent,
) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    // Ctrl+C selalu keluar dengan bersih, termasuk saat modal approval, wizard setup, atau pemilih model terbuka.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        cancel.notify_waiters();
        app.should_quit = true;
        return;
    }
    // Modal Wizard Setup Provider (`/setup` atau otomatis saat pertama buka).
    if app.setup_wizard.is_some() {
        let step = app
            .setup_wizard
            .as_ref()
            .map(|w| w.step)
            .unwrap_or(SetupStep::ChooseApi);
        match key.code {
            KeyCode::Esc => {
                let close = app
                    .setup_wizard
                    .as_mut()
                    .map(|w| w.back())
                    .unwrap_or(true);
                if close {
                    app.setup_wizard = None;
                }
            }
            KeyCode::Up if step == SetupStep::ChooseApi => {
                if let Some(w) = &mut app.setup_wizard {
                    w.move_up();
                }
            }
            KeyCode::Down if step == SetupStep::ChooseApi => {
                if let Some(w) = &mut app.setup_wizard {
                    w.move_down();
                }
            }
            KeyCode::Enter => {
                let done = app.setup_wizard.as_mut().and_then(|w| w.advance());
                if let Some(res) = done {
                    app.setup_wizard = None;
                    send_cmd(app, cmd_tx, TuiCommand::SaveSetupProvider(res));
                }
            }
            KeyCode::Tab if step == SetupStep::ApiKey => {
                if let Some(w) = &mut app.setup_wizard {
                    w.show_key = !w.show_key;
                }
            }
            KeyCode::Backspace if step != SetupStep::ChooseApi => {
                if let Some(w) = &mut app.setup_wizard {
                    w.pop_char();
                }
            }
            KeyCode::Char(c)
                if step == SetupStep::ChooseApi
                    && ('1'..='8').contains(&c)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let idx = (c as usize) - ('1' as usize);
                if idx < SETUP_PRESETS.len() {
                    if let Some(w) = &mut app.setup_wizard {
                        w.selected = idx;
                        let _ = w.advance();
                    }
                }
            }
            KeyCode::Char(c)
                if step != SetupStep::ChooseApi
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(w) = &mut app.setup_wizard {
                    w.push_char(c);
                }
            }
            _ => {}
        }
        return;
    }
    // Modal pertanyaan klarifikasi `ask_user` (Bagian 2 v0.5.6 — terpisah dari approval!).
    if app.pending_ask.is_some() {
        let (num_choices, typing_custom, selected) = app
            .pending_ask
            .as_ref()
            .map(|a| (a.choices.len(), a.typing_custom, a.selected))
            .unwrap_or((0, false, 0));
        match key.code {
            KeyCode::Esc => {
                app.cancel_ask();
            }
            KeyCode::Up if !typing_custom => {
                if let Some(a) = &mut app.pending_ask {
                    let total = a.choices.len() + 1;
                    a.selected = if a.selected == 0 { total - 1 } else { a.selected - 1 };
                }
            }
            KeyCode::Down if !typing_custom => {
                if let Some(a) = &mut app.pending_ask {
                    let total = a.choices.len() + 1;
                    a.selected = (a.selected + 1) % total;
                }
            }
            KeyCode::Enter => {
                if typing_custom {
                    let txt = app
                        .pending_ask
                        .as_ref()
                        .map(|a| a.custom_input.trim().to_string())
                        .unwrap_or_default();
                    if !txt.is_empty() {
                        app.submit_ask(txt);
                    }
                } else if selected < num_choices {
                    let chosen = app
                        .pending_ask
                        .as_ref()
                        .and_then(|a| a.choices.get(selected).cloned())
                        .unwrap_or_default();
                    if !chosen.is_empty() {
                        app.submit_ask(chosen);
                    }
                } else if let Some(a) = &mut app.pending_ask {
                    a.typing_custom = true;
                }
            }
            KeyCode::Backspace if typing_custom => {
                if let Some(a) = &mut app.pending_ask {
                    a.custom_input.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !typing_custom && ('1'..='4').contains(&c) {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < num_choices {
                        let chosen = app
                            .pending_ask
                            .as_ref()
                            .and_then(|a| a.choices.get(idx).cloned())
                            .unwrap_or_default();
                        if !chosen.is_empty() {
                            app.submit_ask(chosen);
                            return;
                        }
                    }
                }
                if let Some(a) = &mut app.pending_ask {
                    a.typing_custom = true;
                    a.custom_input.push(c);
                }
            }
            _ => {}
        }
        return;
    }
    // Popup approval mengunci semua input lain (dukung y / a / n / ↑↓ gulir).
    if app.approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.answer_approval(true);
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if let Some(cat) = app.approve_category_for_session() {
                    app.info(format!(
                        "Izin sesi: '{cat}' disetujui otomatis untuk sisa sesi ini (/approve untuk ubah)."
                    ));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.answer_approval(false);
            }
            KeyCode::Up => {
                app.approval_scroll = app.approval_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                app.approval_scroll = app.approval_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                app.approval_scroll = app.approval_scroll.saturating_sub(8);
            }
            KeyCode::PageDown => {
                app.approval_scroll = app.approval_scroll.saturating_add(8);
            }
            _ => {}
        }
        return;
    }
    // Modal Pemilih Model Interaktif (`/model`) menangkap keyboard saat terbuka.
    if app.model_picker.is_some() {
        let mode = app.model_picker.as_ref().map(|m| m.mode).unwrap_or(ModelPickerMode::Saved);
        match mode {
            ModelPickerMode::AddManual => match key.code {
                KeyCode::Esc => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.manual_input.clear();
                        mp.mode = ModelPickerMode::Saved;
                    }
                }
                KeyCode::Enter => {
                    let id = app
                        .model_picker
                        .as_mut()
                        .map(|mp| {
                            let s = mp.manual_input.trim().to_string();
                            mp.manual_input.clear();
                            mp.mode = ModelPickerMode::Saved;
                            s
                        })
                        .unwrap_or_default();
                    if !id.is_empty() {
                        send_cmd(app, cmd_tx, TuiCommand::ModelPicker(ModelPickerCmd::AddOne(id)));
                    }
                }
                KeyCode::Backspace => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.manual_input.pop();
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.manual_input.push(c);
                    }
                }
                _ => {}
            },
            ModelPickerMode::Catalog => match key.code {
                KeyCode::Esc | KeyCode::Tab => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.mode = ModelPickerMode::Saved;
                        mp.filter_input.clear();
                        mp.clamp_selection();
                    }
                }
                KeyCode::Up => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.move_up();
                    }
                }
                KeyCode::Down => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.move_down();
                    }
                }
                KeyCode::Char(' ') => {
                    let chosen = app.model_picker.as_ref().and_then(|mp| mp.selected_remote());
                    if let Some(id) = chosen {
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::ToggleOne(id)),
                        );
                    }
                }
                KeyCode::Enter => {
                    let chosen = app.model_picker.as_ref().and_then(|mp| mp.selected_remote());
                    if let Some(id) = chosen {
                        app.model_picker = None;
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::SelectModel(id)),
                        );
                    }
                }
                KeyCode::Backspace => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.filter_input.pop();
                        mp.selected = 0;
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.filter_input.push(c);
                        mp.selected = 0;
                    }
                }
                _ => {}
            },
            ModelPickerMode::Saved => match key.code {
                KeyCode::Esc => {
                    app.model_picker = None;
                }
                KeyCode::Char('q') | KeyCode::Char('Q')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    app.model_picker = None;
                }
                KeyCode::Up => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.move_up();
                    }
                }
                KeyCode::Down => {
                    if let Some(mp) = &mut app.model_picker {
                        mp.move_down();
                    }
                }
                KeyCode::Enter => {
                    let chosen = app.model_picker.as_ref().and_then(|mp| mp.selected_saved());
                    if let Some(id) = chosen {
                        app.model_picker = None;
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::SelectModel(id)),
                        );
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('A')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(mp) = &mut app.model_picker {
                        mp.manual_input.clear();
                        mp.mode = ModelPickerMode::AddManual;
                    }
                }
                KeyCode::Char('f') | KeyCode::Char('F')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let need_fetch = if let Some(mp) = &mut app.model_picker {
                        mp.mode = ModelPickerMode::Catalog;
                        mp.filter_input.clear();
                        mp.selected = 0;
                        if mp.remote_models.is_empty() {
                            mp.loading_remote = true;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if need_fetch {
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::FetchCatalog),
                        );
                    }
                }
                KeyCode::Char('d') | KeyCode::Char('D')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let chosen = app.model_picker.as_ref().and_then(|mp| mp.selected_saved());
                    if let Some(id) = chosen {
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::RemoveOne(id)),
                        );
                    }
                }
                KeyCode::Delete => {
                    let chosen = app.model_picker.as_ref().and_then(|mp| mp.selected_saved());
                    if let Some(id) = chosen {
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::RemoveOne(id)),
                        );
                    }
                }
                KeyCode::Char('c') | KeyCode::Char('C')
                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    send_cmd(
                        app,
                        cmd_tx,
                        TuiCommand::ModelPicker(ModelPickerCmd::ClearAll),
                    );
                }
                KeyCode::Tab => {
                    let next = app.model_picker.as_ref().and_then(|mp| mp.next_provider());
                    if let Some(prov) = next {
                        send_cmd(
                            app,
                            cmd_tx,
                            TuiCommand::ModelPicker(ModelPickerCmd::SwitchProvider(prov)),
                        );
                    }
                }
                _ => {}
            },
        }
        return;
    }
    // Esc saat Selena sedang bekerja -> batalkan giliran & approval tengah jalan tanpa keluar!
    if key.code == KeyCode::Esc && app.busy {
        interrupt_pending.store(false, Ordering::SeqCst);
        app.cancel_all_approvals();
        cancel.notify_waiters();
        app.info("Membatalkan semua task & giliran (Esc)…");
        return;
    }

    // Navigasi live `/` command palette saat input sedang mengetik prefix perintah.
    let cur_input = app.input.lines().join("\n");
    let palette = filter_commands(&cur_input);
    if !palette.is_empty() {
        let len = palette.len();
        if app.palette_idx >= len {
            app.palette_idx = 0;
        }
        match key.code {
            KeyCode::Up => {
                app.palette_idx = if app.palette_idx == 0 {
                    len - 1
                } else {
                    app.palette_idx - 1
                };
                return;
            }
            KeyCode::Down => {
                app.palette_idx = (app.palette_idx + 1) % len;
                return;
            }
            KeyCode::Tab => {
                let item = palette[app.palette_idx];
                let filled = if item.takes_args {
                    format!("{} ", item.cmd)
                } else {
                    item.cmd.to_string()
                };
                app.input = app::new_textarea();
                app.input.insert_str(&filled);
                app.palette_idx = 0;
                return;
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
                let item = palette[app.palette_idx];
                if cur_input != item.cmd {
                    app.input = app::new_textarea();
                    if item.takes_args {
                        app.input.insert_str(&format!("{} ", item.cmd));
                        app.palette_idx = 0;
                        return;
                    }
                    app.input.insert_str(item.cmd);
                }
                app.palette_idx = 0;
                submit(app, cmd_tx, cancel, interrupt_pending);
                return;
            }
            _ => {}
        }
    } else if let Some((_, at_q)) = app::extract_at_query(&cur_input) {
        let matches = app::filter_workspace_files(&app.workspace_files, &at_q, 10);
        if !matches.is_empty() {
            let len = matches.len();
            if app.at_palette_idx >= len {
                app.at_palette_idx = 0;
            }
            match key.code {
                KeyCode::Up => {
                    app.at_palette_idx = if app.at_palette_idx == 0 {
                        len - 1
                    } else {
                        app.at_palette_idx - 1
                    };
                    return;
                }
                KeyCode::Down => {
                    app.at_palette_idx = (app.at_palette_idx + 1) % len;
                    return;
                }
                KeyCode::Tab | KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
                    let chosen = &matches[app.at_palette_idx];
                    let filled = app::complete_at_mention(&cur_input, chosen);
                    app.input = app::new_textarea();
                    app.input.insert_str(&filled);
                    app.at_palette_idx = 0;
                    return;
                }
                _ => {}
            }
        }
    }

    // Navigasi riwayat prompt persisten (↑ / ↓) saat input 1 baris dan tidak ada popup aktif.
    if app.input.lines().len() <= 1 && key.modifiers == KeyModifiers::NONE {
        if key.code == KeyCode::Up && app.history_up() {
            return;
        }
        if key.code == KeyCode::Down && app.history_down() {
            return;
        }
    }

    match key.code {
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
            app.palette_idx = 0;
            app.at_palette_idx = 0;
            submit(app, cmd_tx, cancel, interrupt_pending);
        }
        KeyCode::PageUp => {
            let p = app.page_size();
            app.scroll_up(p);
        }
        KeyCode::PageDown => {
            let p = app.page_size();
            app.scroll_down(p);
        }
        _ => {
            app.input.input(key);
            app.palette_idx = 0;
            app.at_palette_idx = 0;
            app.history_pos = None;
        }
    }
}

/// Jalankan TUI sampai user keluar. Fungsi sinkron — panggil dari
/// spawn_blocking. Mengembalikan terminal ke keadaan semula di semua jalur.
pub fn run_blocking(mut app: App, mut ch: TuiChannels) -> Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(crossterm::event::EnableMouseCapture)?;
    let _guard = TerminalGuard;
    let backend = ratatui::backend::CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> Result<()> {
        // Event-driven dirty-flag loop (Bagian A — ringan di laptop kentang):
        // - Saat idle (`!app.busy`): 0 redraw/detik (~0% CPU), poll 200ms.
        // - Saat streaming/sibuk (`app.busy`): gabungkan seluruh token antre dalam 1 frame,
        //   batasi redraw streaming maksimal ~12 fps (80ms) dan spinner ~6 fps (160ms).
        // - Input keyboard / paste / resize / mouse langsung redraw instan (<1ms lag).
        let mut dirty = true;
        let mut last_draw = std::time::Instant::now();
        let mut last_anim_tick = std::time::Instant::now();

        loop {
            let mut got_ui_event = false;
            while let Ok(ev) = ch.event_rx.try_recv() {
                app.apply_event(ev);
                got_ui_event = true;
            }
            while let Ok(req) = ch.approval_rx.try_recv() {
                app.receive_approval(req);
                got_ui_event = true;
            }
            while let Ok(req) = ch.ask_rx.try_recv() {
                app.receive_ask(req);
                got_ui_event = true;
            }
            if got_ui_event {
                dirty = true;
            }

            // Saat sedang sibuk tanpa event baru, animasi spinner dibatasi ~6 fps (160ms).
            if app.busy && !dirty && last_anim_tick.elapsed() >= Duration::from_millis(160) {
                dirty = true;
                last_anim_tick = std::time::Instant::now();
            }

            // Saat streaming cepat, coalesce burst token hingga maks ~12 fps (65-80ms) bila
            // tidak ada input user, agar CPU lemah tidak pernah freeze.
            let should_draw =
                dirty && (!got_ui_event || !app.busy || last_draw.elapsed() >= Duration::from_millis(65));

            if should_draw {
                terminal.draw(|f| draw(f, &mut app))?;
                dirty = false;
                last_draw = std::time::Instant::now();
                if app.busy {
                    last_anim_tick = last_draw;
                }
            }

            let poll_ms = if dirty {
                16
            } else if app.busy {
                60
            } else {
                200
            };

            if event::poll(Duration::from_millis(poll_ms))? {
                match event::read()? {
                    Event::Key(key) => {
                        handle_key(&mut app, &ch.cmd_tx, &ch.cancel, &ch.interrupt_pending, key);
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        if let Some(w) = &mut app.setup_wizard {
                            w.push_str(&text);
                        } else if let Some(mp) = &mut app.model_picker {
                            if mp.mode == ModelPickerMode::AddManual {
                                mp.manual_input.push_str(text.trim());
                            } else if mp.mode == ModelPickerMode::Catalog {
                                mp.filter_input.push_str(text.trim());
                                mp.selected = 0;
                            }
                        } else if app.approval.is_none() {
                            app.input.insert_str(&text);
                        }
                        dirty = true;
                    }
                    Event::Mouse(m) => {
                        match m.kind {
                            MouseEventKind::ScrollUp => {
                                if app.approval.is_some() {
                                    app.approval_scroll = app.approval_scroll.saturating_sub(3);
                                } else if let Some(mp) = &mut app.model_picker {
                                    mp.move_up();
                                } else {
                                    app.scroll_up(3);
                                }
                                dirty = true;
                            }
                            MouseEventKind::ScrollDown => {
                                if app.approval.is_some() {
                                    app.approval_scroll = app.approval_scroll.saturating_add(3);
                                } else if let Some(mp) = &mut app.model_picker {
                                    mp.move_down();
                                } else {
                                    app.scroll_down(3);
                                }
                                dirty = true;
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(_, _) => {
                        dirty = true;
                    }
                    _ => {}
                }
            }
            if app.should_quit {
                break;
            }
        }
        Ok(())
    })();

    // Beri tahu worker untuk berhenti tanpa memblokir (agar Ctrl+C tidak
    // pernah macet bila worker sedang sibuk/hang).
    let _ = ch.cmd_tx.try_send(TuiCommand::Shutdown);
    // Terminal dipulihkan oleh TerminalGuard (Drop) — juga saat `result`
    // berisi Err dan saat panic (panic hook).
    result
}
