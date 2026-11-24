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
pub mod moonlit;

use crate::agent::UiEvent;
use crate::repl::{parse_slash_command, SlashCommand};
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
use ratatui::style::{Color, Modifier, Style};
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
    if !out.is_empty() {
        out.push(Line::from(""));
    }
    for l in &app.lines {
        match l.kind {
            LineKind::User => {
                let mut first = true;
                for part in l.text.lines() {
                    let prefix = if first { "> " } else { "  " };
                    out.push(Line::from(vec![
                        Span::styled(
                            prefix.to_string(),
                            Style::default()
                                .fg(moonlit::VIOLET)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            part.to_string(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    first = false;
                }
            }
            LineKind::Assistant => {
                for part in l.text.lines() {
                    out.push(Line::from(part.to_string()));
                }
            }
            LineKind::Tool => {
                let (icon, color) = match l.tool_state {
                    Some(ToolState::Pending) => ("◐", Color::Yellow),
                    Some(ToolState::Ok) => ("●", Color::Green),
                    Some(ToolState::Err) => ("✖", Color::Red),
                    None => ("●", Color::DarkGray),
                };
                let mut first = true;
                for part in l.text.lines() {
                    let prefix = if first {
                        format!("{icon} ")
                    } else {
                        "  ".to_string()
                    };
                    out.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(color)),
                        Span::styled(part.to_string(), Style::default().fg(Color::Gray)),
                    ]));
                    first = false;
                }
            }
            LineKind::Plan => {
                for part in l.text.lines() {
                    out.push(Line::from(Span::styled(
                        part.to_string(),
                        Style::default().fg(moonlit::LAV),
                    )));
                }
            }
            LineKind::Info => {
                for part in l.text.lines() {
                    out.push(Line::from(Span::styled(
                        part.to_string(),
                        Style::default().fg(moonlit::DIM),
                    )));
                }
            }
            LineKind::Error => {
                for part in l.text.lines() {
                    out.push(Line::from(Span::styled(
                        format!("[error] {part}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "Ketik perintah untuk Selena…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
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
    let total = lines.len();
    app.last_total_lines = total;
    app.last_viewport = viewport;
    let max_scroll = total.saturating_sub(viewport);
    let from_bottom = (app.scroll as usize).min(max_scroll);
    let y = (max_scroll - from_bottom) as u16;
    let transcript = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((y, 0));
    frame.render_widget(transcript, chunks[0]);

    // Status bar Moonlit: fase bulan + konteks + token + langkah + cwd.
    let status = Paragraph::new(moonlit::status_line(
        app.context_pct,
        app.session_in,
        app.session_out,
        app.step_current,
        app.step_max,
        &app.cwd,
        app.busy,
    ));
    frame.render_widget(status, chunks[1]);

    // Input: bingkai DEEP digambar sebagai Block luar; prompt "❯" violet
    // tebal di sel kiri dalam bingkai; textarea tanpa bingkai di sisanya.
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(moonlit::DEEP));
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
        SlashCommand::Model(None) => {
            let _ = cmd_tx.blocking_send(TuiCommand::ShowModels);
        }
        SlashCommand::Model(Some(name)) => {
            let _ = cmd_tx.blocking_send(TuiCommand::SwitchModel(name));
        }
        SlashCommand::Sessions => {
            let _ = cmd_tx.blocking_send(TuiCommand::ShowSessions);
        }
        SlashCommand::Resume => {
            let _ = cmd_tx.blocking_send(TuiCommand::ShowSessionInfo);
        }
        SlashCommand::Plan => {
            app.plan_next = true;
            app.info(
                "Mode plan aktif: giliran berikutnya baca-saja dan diakhiri rencana untuk disetujui.",
            );
        }
        SlashCommand::Unknown(name) => {
            app.error(format!(
                "Perintah /{name} tidak dikenal. Pilihan: /plan, /model, /sessions, /exit"
            ));
        }
        SlashCommand::NotCommand => {
            // Konfirmasi rencana yang menunggu (Fase 6): input berikutnya
            // harus y/n sebelum prompt baru diterima.
            if app.pending_plan.is_some() {
                match text.to_lowercase().as_str() {
                    "y" | "yes" => {
                        let plan = app.pending_plan.take().unwrap_or_default();
                        app.push_user(&text);
                        let _ = cmd_tx.blocking_send(TuiCommand::ExecutePlan(plan));
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
                let _ = cmd_tx.blocking_send(TuiCommand::PromptPlan(text));
            } else {
                let _ = cmd_tx.blocking_send(TuiCommand::Prompt(text));
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

    // Beri tahu worker untuk berhenti; abaikan bila worker sudah mati.
    let _ = ch.cmd_tx.blocking_send(TuiCommand::Shutdown);
    // Terminal dipulihkan oleh TerminalGuard (Drop) — juga saat `result`
    // berisi Err dan saat panic (panic hook).
    result
}
