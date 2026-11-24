//! Render teks diff menjadi baris Ratatui berwarna (Fase 4).
//!
//! Keputusan Fase 4: TANPA syntect. Pewarnaan cukup dari gaya Ratatui —
//! baris `+` hijau, `-` merah, header `diff ...`/`@@` sian, konteks redup.
//! Itu sudah membedakan tambah/kurang dengan jelas di terminal Termux dan
//! tidak menambah dependensi berat ke binary.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

/// Apakah teks ini tampak seperti diff? (baris pertama "diff " dari
/// unified_diff internal, atau mayoritas baris berawalan +/-/spasi-diff)
pub fn looks_like_diff(text: &str) -> bool {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.starts_with("diff ") => return true,
        _ => {}
    }
    let total = text.lines().count();
    if total < 2 {
        return false;
    }
    let marked = text
        .lines()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .count();
    marked * 2 >= total
}

/// Ubah teks diff menjadi baris berwarna + statistik (+n/-n).
/// Baris header file gaya unified (`+++`/`---`) tidak dihitung sebagai
/// tambah/kurang.
pub fn diff_lines(text: &str) -> (Vec<Line<'static>>, DiffStats) {
    let mut stats = DiffStats::default();
    let mut out = Vec::new();
    for raw in text.lines() {
        let style = if raw.starts_with("diff ") || raw.starts_with("@@") {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if raw.starts_with("+++") || raw.starts_with("---") {
            Style::default().fg(Color::DarkGray)
        } else if let Some(rest) = raw.strip_prefix('+') {
            stats.added += 1;
            let _ = rest;
            Style::default().fg(Color::Green)
        } else if raw.starts_with('-') {
            stats.removed += 1;
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        out.push(Line::from(Span::styled(raw.to_string(), style)));
    }
    (out, stats)
}

/// Baris-baris untuk badan popup approval: diff diwarnai, teks biasa
/// (mis. perintah shell) ditampilkan apa adanya.
pub fn detail_lines(detail: &str) -> Vec<Line<'static>> {
    if looks_like_diff(detail) {
        let (mut lines, stats) = diff_lines(detail);
        lines.push(Line::from(Span::styled(
            format!("(+{} / -{})", stats.added, stats.removed),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines
    } else {
        detail
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect()
    }
}
