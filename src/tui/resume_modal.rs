//! Modal pemilih sesi `/resume` interaktif untuk TUI Moonlit (`v0.5.30`).
//! Mendukung pencarian judul live, navigasi `↑/↓`, `Enter` untuk melanjutkan sesi,
//! `Ctrl+N` untuk mulai sesi baru, `Del`/`Ctrl+D` untuk hapus sesi lama, dan `Esc` untuk tutup.

use super::app::{App, ResumePickerState};
use super::{moonlit, send_cmd, TuiCommand};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;

fn modal_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
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
        .split(vert[1])[1]
}

/// Render popup modal pemilih `/resume` interaktif bergaya Moonlit ungu.
pub(crate) fn render_resume_modal(frame: &mut Frame, area: Rect, rp: &ResumePickerState) {
    let pct_x = if area.width < 84 { 94 } else { 78 };
    let pct_y = if area.height < 24 { 86 } else { 70 };
    let popup = modal_rect(pct_x, pct_y, area);
    frame.render_widget(Clear, popup);

    let modal_bg = Color::Rgb(20, 15, 38);
    let row_hl = Color::Rgb(42, 32, 74);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(modal_bg))
        .border_style(
            Style::default()
                .fg(moonlit::VIOLET)
                .add_modifier(Modifier::BOLD),
        )
        .title(format!(" ☾ Lanjutkan Sesi ({}) ", rp.items.len()));
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(inner);

    let mut filter_spans = vec![
        Span::styled(" ✦ ", Style::default().fg(moonlit::CYAN_MOON)),
        Span::styled("Cari sesi: ", Style::default().fg(moonlit::DIM)),
        Span::styled(
            if rp.filter_input.is_empty() {
                "(ketik judul / ID untuk filter…)"
            } else {
                rp.filter_input.as_str()
            },
            Style::default().fg(if rp.filter_input.is_empty() {
                moonlit::FAINT
            } else {
                moonlit::LILAC
            }),
        ),
        Span::styled("█", Style::default().fg(moonlit::VIOLET)),
    ];
    if let Some(st) = &rp.status {
        filter_spans.push(Span::styled("  ·  ", Style::default().fg(moonlit::FAINT)));
        filter_spans.push(Span::styled(st.clone(), Style::default().fg(moonlit::MINT)));
    }

    let header_lines = vec![
        Line::from(vec![
            Span::styled(" ⌂ ", Style::default().fg(moonlit::VIOLET)),
            Span::styled(rp.cwd.clone(), Style::default().fg(moonlit::DIM)),
        ]),
        Line::from(filter_spans),
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
    let filtered = rp.filtered_items();
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    if filtered.is_empty() {
        body_lines.push(Line::from(Span::styled(
            "  (tidak ada sesi tersimpan yang cocok — tekan Ctrl+N untuk sesi baru)",
            Style::default().fg(moonlit::DIM),
        )));
    } else {
        let total = filtered.len();
        let start = if total > body_h && body_h > 0 && rp.selected >= body_h {
            rp.selected + 1 - body_h
        } else {
            0
        };
        for (idx, item) in filtered
            .iter()
            .enumerate()
            .skip(start)
            .take(body_h.max(1))
        {
            let is_sel = idx == rp.selected;
            let bg = if is_sel {
                Style::default().bg(row_hl)
            } else {
                Style::default().bg(modal_bg)
            };
            let marker = if is_sel { "▸ " } else { "  " };
            let num = format!("#{:<2} ", idx + 1);
            let title_clip: String = if item.title.chars().count() > 38 {
                let s: String = item.title.chars().take(37).collect();
                format!("{s}…")
            } else {
                item.title.clone()
            };
            let meta = format!("  {} msg · {}", item.msg_count, item.model);
            let badge = if item.is_active { "  ✓ aktif" } else { "" };
            body_lines.push(Line::from(vec![
                Span::styled(format!(" {marker}"), bg.fg(moonlit::LILAC)),
                Span::styled(num, bg.fg(moonlit::VIOLET).add_modifier(Modifier::BOLD)),
                Span::styled(
                    title_clip,
                    bg.fg(if is_sel || item.is_active {
                        moonlit::LILAC
                    } else {
                        moonlit::SILVER
                    })
                    .add_modifier(if is_sel || item.is_active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(meta, bg.fg(moonlit::DIM)),
                Span::styled(badge, bg.fg(moonlit::MINT).add_modifier(Modifier::BOLD)),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(body_lines).style(Style::default().bg(modal_bg)),
        sections[1],
    );

    let pill_ok = Style::default()
        .bg(Color::Rgb(24, 56, 46))
        .fg(moonlit::MINT)
        .add_modifier(Modifier::BOLD);
    let pill_alt = Style::default()
        .bg(Color::Rgb(34, 28, 66))
        .fg(moonlit::CYAN_MOON)
        .add_modifier(Modifier::BOLD);
    let pill_del = Style::default()
        .bg(Color::Rgb(58, 22, 36))
        .fg(moonlit::ROSE)
        .add_modifier(Modifier::BOLD);

    let footer_lines = vec![
        Line::from(Span::styled(
            "─".repeat(sections[2].width as usize),
            Style::default().fg(moonlit::FAINT),
        )),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(" [Enter] lanjut ", pill_ok),
            Span::raw(" "),
            Span::styled(" [Ctrl+N] sesi baru ", pill_alt),
            Span::raw(" "),
            Span::styled(" [Del/Ctrl+D] hapus ", pill_del),
            Span::raw(" "),
            Span::styled(" [Esc] tutup ", pill_del),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(footer_lines).style(Style::default().bg(modal_bg)),
        sections[2],
    );
}

/// Tangani tombol keyboard saat modal `/resume` sedang terbuka.
pub(crate) fn handle_resume_picker_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::Sender<TuiCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            app.resume_picker = None;
        }
        KeyCode::Up => {
            if let Some(rp) = &mut app.resume_picker {
                rp.selected = rp.selected.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(rp) = &mut app.resume_picker {
                let len = rp.filtered_items().len();
                if len > 0 && rp.selected + 1 < len {
                    rp.selected += 1;
                }
            }
        }
        KeyCode::Enter => {
            let chosen_id = app.resume_picker.as_ref().and_then(|rp| {
                let items = rp.filtered_items();
                items.get(rp.selected).map(|it| it.id.clone())
            });
            app.resume_picker = None;
            if let Some(id) = chosen_id {
                send_cmd(app, cmd_tx, TuiCommand::Resume(Some(id)));
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.resume_picker = None;
            send_cmd(app, cmd_tx, TuiCommand::NewSession);
        }
        KeyCode::Delete => {
            let del_id = app.resume_picker.as_ref().and_then(|rp| {
                let items = rp.filtered_items();
                items.get(rp.selected).map(|it| it.id.clone())
            });
            if let Some(id) = del_id {
                send_cmd(app, cmd_tx, TuiCommand::DeleteSession(id));
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let del_id = app.resume_picker.as_ref().and_then(|rp| {
                let items = rp.filtered_items();
                items.get(rp.selected).map(|it| it.id.clone())
            });
            if let Some(id) = del_id {
                send_cmd(app, cmd_tx, TuiCommand::DeleteSession(id));
            }
        }
        KeyCode::Backspace => {
            if let Some(rp) = &mut app.resume_picker {
                rp.filter_input.pop();
                rp.clamp_selection();
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(rp) = &mut app.resume_picker {
                rp.filter_input.push(c);
                rp.clamp_selection();
            }
        }
        _ => {}
    }
}
