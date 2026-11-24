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
use crate::repl::{parse_slash_command, SlashCommand, HELP_TEXT};
use crate::security::ApprovalRequest;
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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::time::Duration;
use tokio::sync::mpsc;

/// Perintah dari UI ke worker agent (dimiliki main.rs).
#[derive(Debug)]
pub enum TuiCommand {
    Prompt(String),
    /// Giliran plan mode (Fase 6): baca-saja, diakhiri rencana.
    PromptPlan(String),
    /// Jalankan rencana yang sudah disetujui user (teks rencana utuh).
    ExecutePlan(String),
    ShowModels,
    SwitchModel(String),
    ShowSessions,
    ShowSessionInfo,
    Shutdown,
}

/// Channel-channel antara UI dan worker/approver.
pub struct TuiChannels {
    pub cmd_tx: mpsc::Sender<TuiCommand>,
    pub event_rx: mpsc::Receiver<UiEvent>,
    pub approval_rx: mpsc::Receiver<ApprovalRequest>,
}

// ---------- gambar ----------

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

    // Popup approval (modal, di atas segalanya) — bingkai GOLD Moonlit;
    // warna diff di badan popup tetap hijau/merah (kode universal).
    if let Some(p) = &app.approval {
        let popup = centered_rect(70, 60, area);
        frame.render_widget(Clear, popup);
        let mut body = diff_view::detail_lines(&p.detail);
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "[y] setuju    [n] tolak",
            Style::default()
                .fg(moonlit::GOLD)
                .add_modifier(Modifier::BOLD),
        )));
        let widget = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(moonlit::GOLD))
                    .title_style(
                        Style::default()
                            .fg(moonlit::GOLD)
                            .add_modifier(Modifier::BOLD),
                    )
                    .title(format!(" Persetujuan: {} ", p.title)),
            );
        frame.render_widget(widget, popup);
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
        SlashCommand::Help => {
            app.info(HELP_TEXT);
        }
        SlashCommand::Model(None) => {
            send_cmd(app, cmd_tx, TuiCommand::ShowModels);
        }
        SlashCommand::Model(Some(name)) => {
            send_cmd(app, cmd_tx, TuiCommand::SwitchModel(name));
        }
        SlashCommand::Sessions => {
            send_cmd(app, cmd_tx, TuiCommand::ShowSessions);
        }
        SlashCommand::Resume => {
            send_cmd(app, cmd_tx, TuiCommand::ShowSessionInfo);
        }
        SlashCommand::Plan => {
            app.plan_next = true;
            app.info(
                "Mode plan aktif: giliran berikutnya baca-saja dan diakhiri rencana untuk disetujui.",
            );
        }
        SlashCommand::Unknown(name) => {
            app.error(format!(
                "Perintah /{name} tidak dikenal. Pilihan: /plan, /model, /sessions, /help, /exit"
            ));
        }
        SlashCommand::NotCommand => {
            if app.busy {
                app.info(
                    "Selena masih bekerja — tunggu giliran selesai atau tekan Ctrl+C untuk keluar.",
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

fn handle_key(app: &mut App, cmd_tx: &mpsc::Sender<TuiCommand>, key: KeyEvent) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    // Popup approval mengunci semua input lain.
    if app.approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.answer_approval(true);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.answer_approval(false);
            }
            _ => {}
        }
        return;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    match key.code {
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
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
                    Event::Key(key) => handle_key(&mut app, &ch.cmd_tx, key),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
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
