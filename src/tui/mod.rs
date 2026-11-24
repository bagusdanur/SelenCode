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
use app::{App, LineKind, ToolState};
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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Perintah dari UI ke worker agent (dimiliki main.rs).
#[derive(Debug)]
pub enum TuiCommand {
    Prompt(String),
    /// Giliran plan mode (Fase 6): baca-saja, diakhiri rencana.
    PromptPlan(String),
    /// Jalankan rencana yang sudah disetujui user (teks rencana utuh).
    ExecutePlan(String),
    /// `!cmd` — jalankan shell lokal cepat tanpa memanggil LLM.
    InlineShell(String),
    ShowModels,
    /// /model <arg>: ganti provider, `/model set <id>`, `/model <prov>:<id>`, atau `/model list`.
    SwitchOrSetModel(String),
    /// /models [filter]: tarik daftar model dari endpoint API provider aktif & simpan.
    FetchRemoteModels(Option<String>),
    /// /provider [preset|add|model|default|rm ...]: kelola & pasang provider lain.
    ManageProvider(Option<String>),
    Undo,
    Redo,
    Memory(Option<String>),
    Skills(Option<String>),
    UseSkill(Option<String>),
    LearnSkill(Option<String>),
    InitAgents,
    ExportChat(Option<String>),
    /// /approve [ask | edits | auto]: atur mode izin sesi aktif.
    SetApproval(Option<String>),
    /// /new: mulai sesi baru yang bersih.
    NewSession,
    /// /compact: ringkas riwayat konteks secara manual.
    CompactContext,
    ShowSessions,
    ShowSessionInfo,
    Shutdown,
}

/// Channel-channel antara UI dan worker/approver.
pub struct TuiChannels {
    pub cmd_tx: mpsc::Sender<TuiCommand>,
    pub event_rx: mpsc::Receiver<UiEvent>,
    pub approval_rx: mpsc::Receiver<ApprovalRequest>,
    /// Sinyal pembatalan giliran saat user menekan `Esc` ketika agent sibuk.
    pub cancel: Arc<Notify>,
}

// ---------- gambar ----------

/// Render kartu menu `/` berwarna Moonlit (bukan teks abu-abu polos).
/// Markup sederhana: `# judul`, `---`, `* kiri | tengah | kanan` (aktif),
/// `> kiri | tengah | kanan` (biasa), `! tip`.
pub fn render_menu_card(markup: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut opened = false;

    for raw in markup.lines() {
        if let Some(title) = raw.strip_prefix("# ") {
            opened = true;
            out.push(Line::from(vec![
                Span::styled("  ╭─ ".to_string(), Style::default().fg(moonlit::BORDER)),
                Span::styled(
                    "☾ ".to_string(),
                    Style::default()
                        .fg(moonlit::VIOLET)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    title.trim().to_string(),
                    Style::default()
                        .fg(moonlit::LILAC)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " ────────────────────────".to_string(),
                    Style::default().fg(moonlit::BORDER),
                ),
            ]));
        } else if raw.trim() == "---" {
            out.push(Line::from(vec![
                Span::styled("  ├".to_string(), Style::default().fg(moonlit::BORDER)),
                Span::styled(
                    "────────────────────────────────────────".to_string(),
                    Style::default().fg(moonlit::FAINT),
                ),
            ]));
        } else if let Some(rest) = raw.strip_prefix("* ").or_else(|| raw.strip_prefix("> ")) {
            let active = raw.starts_with("* ");
            let parts: Vec<&str> = rest.split('|').map(|p| p.trim()).collect();
            let mut spans = vec![
                Span::styled("  │ ".to_string(), Style::default().fg(moonlit::BORDER)),
                if active {
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
                },
            ];
            if let Some(&col1) = parts.first() {
                let col1_color = if active { moonlit::LILAC } else { moonlit::LAV };
                spans.push(Span::styled(
                    format!("{col1:<12}"),
                    Style::default().fg(col1_color).add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(&col2) = parts.get(1) {
                if !col2.is_empty() {
                    spans.push(Span::styled(
                        format!(" {col2}"),
                        Style::default().fg(moonlit::CYAN_MOON),
                    ));
                }
            }
            if let Some(&col3) = parts.get(2) {
                if !col3.is_empty() {
                    spans.push(Span::styled(
                        "  ·  ".to_string(),
                        Style::default().fg(moonlit::FAINT),
                    ));
                    spans.push(Span::styled(
                        col3.to_string(),
                        Style::default().fg(moonlit::SILVER),
                    ));
                }
            }
            out.push(Line::from(spans));
        } else if let Some(tip) = raw.strip_prefix("! ") {
            out.push(Line::from(vec![
                Span::styled("  │ ".to_string(), Style::default().fg(moonlit::BORDER)),
                Span::styled(
                    "✦ ".to_string(),
                    Style::default()
                        .fg(moonlit::GOLD)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(tip.to_string(), Style::default().fg(moonlit::SILVER)),
            ]));
        } else if !raw.trim().is_empty() {
            out.push(Line::from(vec![
                Span::styled("  │ ".to_string(), Style::default().fg(moonlit::BORDER)),
                Span::styled(raw.to_string(), Style::default().fg(moonlit::DIM)),
            ]));
        }
    }

    if opened {
        out.push(Line::from(Span::styled(
            "  ╰─────────────────────────────────────────".to_string(),
            Style::default().fg(moonlit::BORDER),
        )));
    }
    out
}

fn display_lines(app: &App) -> Vec<Line<'static>> {
    // Sambutan Moonlit menempel di awal transkrip & ikut bergulir.
    let mut out: Vec<Line<'static>> = app.welcome.clone().unwrap_or_default();
    let mut prev_kind: Option<LineKind> = None;

    for l in &app.lines {
        match l.kind {
            LineKind::User => {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
                let mut first = true;
                for part in l.text.lines() {
                    let (bar, prompt) = if first {
                        ("  ┃ ", "❯ ")
                    } else {
                        ("  ┃ ", "  ")
                    };
                    out.push(Line::from(vec![
                        Span::styled(bar.to_string(), Style::default().fg(moonlit::DEEP)),
                        Span::styled(
                            prompt.to_string(),
                            Style::default()
                                .fg(moonlit::VIOLET)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            part.to_string(),
                            Style::default()
                                .fg(moonlit::SILVER)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    first = false;
                }
            }
            LineKind::Assistant => {
                if prev_kind != Some(LineKind::Assistant) {
                    if !out.is_empty() {
                        out.push(Line::from(""));
                    }
                    out.push(Line::from(vec![
                        Span::styled(
                            "  ☾ ".to_string(),
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
                    ]));
                }
                out.extend(markdown::render_markdown(&l.text));
            }
            LineKind::Tool => {
                let (icon, color) = match l.tool_state {
                    Some(ToolState::Pending) => ("◐", moonlit::GOLD),
                    Some(ToolState::Ok) => ("●", moonlit::MINT),
                    Some(ToolState::Err) => ("✖", moonlit::ROSE),
                    None => ("●", moonlit::DIM),
                };
                if !app.show_tool_details {
                    let label = l.tool_name.as_deref().unwrap_or("tool");
                    out.push(Line::from(vec![
                        Span::styled(format!("  {icon} "), Style::default().fg(color)),
                        Span::styled(label.to_string(), Style::default().fg(moonlit::LAV)),
                        Span::styled(" (ringkas · /details)", Style::default().fg(moonlit::FAINT)),
                    ]));
                } else {
                    let mut first = true;
                    for part in l.text.lines() {
                        let prefix = if first {
                            format!("  {icon} ")
                        } else {
                            "    ".to_string()
                        };
                        out.push(Line::from(vec![
                            Span::styled(prefix, Style::default().fg(color)),
                            Span::styled(part.to_string(), Style::default().fg(moonlit::DIM)),
                        ]));
                        first = false;
                    }
                }
            }
            LineKind::Plan => {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
                for part in l.text.lines() {
                    out.push(Line::from(vec![
                        Span::styled("  ┃ ".to_string(), Style::default().fg(moonlit::VIOLET)),
                        Span::styled(part.to_string(), Style::default().fg(moonlit::LAV)),
                    ]));
                }
            }
            LineKind::Info => {
                let is_usage = l.text.starts_with("token sesi ini:");
                for (idx, part) in l.text.lines().enumerate() {
                    if is_usage {
                        out.push(Line::from(vec![
                            Span::styled("  ╰─ ".to_string(), Style::default().fg(moonlit::FAINT)),
                            Span::styled(part.to_string(), Style::default().fg(moonlit::DIM)),
                        ]));
                    } else {
                        let prefix = if idx == 0 { "  · " } else { "    " };
                        out.push(Line::from(vec![
                            Span::styled(prefix.to_string(), Style::default().fg(moonlit::VIOLET)),
                            Span::styled(part.to_string(), Style::default().fg(moonlit::DIM)),
                        ]));
                    }
                }
            }
            LineKind::Menu => {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
                out.extend(render_menu_card(&l.text));
            }
            LineKind::Error => {
                for (idx, part) in l.text.lines().enumerate() {
                    let prefix = if idx == 0 { "  ✖ [error] " } else { "            " };
                    out.push(Line::from(vec![
                        Span::styled(prefix.to_string(), Style::default().fg(moonlit::ROSE)),
                        Span::styled(part.to_string(), Style::default().fg(moonlit::ROSE)),
                    ]));
                }
            }
        }
        prev_kind = Some(l.kind);
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "  ☾ Ketik perintah untuk Selena…",
            Style::default().fg(moonlit::DIM),
        )));
    }
    out
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

    // Transkrip (gulir: app.scroll = jarak dari bawah)
    let lines = display_lines(app);
    let viewport = chunks[0].height as usize;
    let total = wrapped_line_count(&lines, chunks[0].width);
    app.last_total_lines = total;
    app.last_viewport = viewport;
    let max_scroll = total.saturating_sub(viewport);
    let from_bottom = (app.scroll as usize).min(max_scroll);
    let y = (max_scroll - from_bottom).min(u16::MAX as usize) as u16;
    let transcript = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((y, 0));
    frame.render_widget(transcript, chunks[0]);

    // Status bar Moonlit (ungu bersih): model + fase bulan + konteks + token + langkah + cwd.
    let status = Paragraph::new(moonlit::status_line(
        &app.model,
        app.context_pct,
        app.session_in,
        app.session_out,
        app.step_current,
        app.step_max,
        &app.cwd,
        app.busy,
    ));
    frame.render_widget(status, chunks[1]);

    // Input: bingkai BORDER (ungu Moonlit); prompt "❯" violet tebal di sel
    // kiri dalam bingkai; textarea tanpa bingkai di sisanya.
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(moonlit::BORDER));
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
    // Kursor hanya relevan tanpa popup.
    if app.approval.is_none() {
        frame.set_cursor_position((
            cols[1].x + app.input.cursor().1 as u16,
            cols[1].y + app.input.cursor().0 as u16,
        ));
    }

    // Live `/` command palette: muncul otomatis tepat di atas status bar saat
    // user mulai mengetik `/...` (tanpa popup approval aktif).
    if app.approval.is_none() {
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
        }
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

fn submit(app: &mut App, cmd_tx: &mpsc::Sender<TuiCommand>) {
    let raw = app.take_input();
    let text = raw.trim().to_string();
    if text.is_empty() {
        return;
    }
    match parse_slash_command(&text) {
        SlashCommand::Exit => {
            app.should_quit = true;
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
            send_cmd(app, cmd_tx, TuiCommand::ShowModels);
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
        SlashCommand::Undo => {
            send_cmd(app, cmd_tx, TuiCommand::Undo);
        }
        SlashCommand::Redo => {
            send_cmd(app, cmd_tx, TuiCommand::Redo);
        }
        SlashCommand::Memory(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Memory(arg));
        }
        SlashCommand::Skills(arg) => {
            send_cmd(app, cmd_tx, TuiCommand::Skills(arg));
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
        SlashCommand::Resume => {
            send_cmd(app, cmd_tx, TuiCommand::ShowSessionInfo);
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
            if app.busy {
                app.info(
                    "Selena masih bekerja — tekan Esc untuk membatalkan atau tunggu selesai.",
                );
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
    key: KeyEvent,
) {
    if key.kind == KeyEventKind::Release {
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
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        cancel.notify_waiters();
        app.should_quit = true;
        return;
    }
    // Esc saat Selena sedang bekerja -> batalkan giliran tengah jalan tanpa keluar!
    if key.code == KeyCode::Esc && app.busy {
        cancel.notify_waiters();
        app.info("Membatalkan giliran (Esc)…");
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
                submit(app, cmd_tx);
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
            app.palette_idx = 0;
            submit(app, cmd_tx);
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
        loop {
            terminal.draw(|f| draw(f, &mut app))?;
            // Event UI & approval yang menunggu.
            while let Ok(ev) = ch.event_rx.try_recv() {
                app.apply_event(ev);
            }
            while let Ok(req) = ch.approval_rx.try_recv() {
                app.receive_approval(req);
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => handle_key(&mut app, &ch.cmd_tx, &ch.cancel, key),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            if app.approval.is_some() {
                                app.approval_scroll = app.approval_scroll.saturating_sub(3);
                            } else {
                                app.scroll_up(3);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if app.approval.is_some() {
                                app.approval_scroll = app.approval_scroll.saturating_add(3);
                            } else {
                                app.scroll_down(3);
                            }
                        }
                        _ => {}
                    },
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
