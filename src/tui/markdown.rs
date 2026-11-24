//! Renderer Markdown ringan untuk jawaban Selena di TUI (murni Rust, tanpa
//! dependensi eksternal). Mengubah heading, **bold**, *italic*, `kode inline`,
//! daftar bernomor/bullet, blockquote, garis pemisah, dan blok kode ```...```
//! menjadi baris Ratatui berpalet Moonlit ungu yang rapi dan mudah dibaca.

use super::moonlit::{BORDER, CYAN_MOON, DEEP, DIM, FAINT, LAV, LILAC, SILVER, VIOLET};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn st(c: Color) -> Style {
    Style::default().fg(c)
}

fn stb(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

fn rail() -> Span<'static> {
    Span::styled("  │ ".to_string(), st(FAINT))
}

/// Render teks Markdown jawaban assistant menjadi baris Ratatui bergaya Moonlit.
/// Aman dipakai saat streaming (fence kode yang belum ditutup tetap rapi).
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut blank_run = 0usize;

    for raw in text.lines() {
        let trimmed = raw.trim_start();

        // Fenced code block (``` atau ```lang)
        if let Some(rest) = trimmed.strip_prefix("```") {
            blank_run = 0;
            if !in_code {
                in_code = true;
                let lang = rest.trim();
                let header = if lang.is_empty() {
                    vec![rail(), Span::styled("╭─ kode ────────────────────", st(BORDER))]
                } else {
                    vec![
                        rail(),
                        Span::styled("╭─ ", st(BORDER)),
                        Span::styled(lang.to_string(), stb(LAV)),
                        Span::styled(" ────────────────────", st(BORDER)),
                    ]
                };
                out.push(Line::from(header));
            } else {
                in_code = false;
                out.push(Line::from(vec![
                    rail(),
                    Span::styled("╰───────────────────────────", st(BORDER)),
                ]));
            }
            continue;
        }

        if in_code {
            out.push(Line::from(vec![
                rail(),
                Span::styled("│ ", st(BORDER)),
                Span::styled(raw.to_string(), st(CYAN_MOON)),
            ]));
            continue;
        }

        // Baris kosong: pertahankan maksimal 1 baris kosong beruntun agar lapang tapi tidak bolong.
        if raw.trim().is_empty() {
            blank_run += 1;
            if blank_run == 1 && !out.is_empty() {
                out.push(Line::from(vec![rail()]));
            }
            continue;
        }
        blank_run = 0;

        // Horizontal rule (--- / *** / ___)
        if is_hr(trimmed) {
            out.push(Line::from(vec![
                rail(),
                Span::styled("────────────────────────────────", st(FAINT)),
            ]));
            continue;
        }

        // Heading (# / ## / ###)
        if let Some((level, heading)) = parse_heading(trimmed) {
            let (bullet, color) = match level {
                1 => ("◆ ", LILAC),
                2 => ("▸ ", VIOLET),
                _ => ("· ", LAV),
            };
            let mut spans = vec![rail(), Span::styled(bullet.to_string(), stb(color))];
            spans.extend(parse_inline(heading, stb(color)));
            out.push(Line::from(spans));
            continue;
        }

        // Blockquote (> ...)
        if let Some(quote) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix('>')) {
            let mut spans = vec![rail(), Span::styled("▎ ", st(DEEP))];
            spans.extend(parse_inline(
                quote.trim_start(),
                st(DIM).add_modifier(Modifier::ITALIC),
            ));
            out.push(Line::from(spans));
            continue;
        }

        // Unordered list (- / * / +)
        let indent = raw.len() - trimmed.len();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let pad = " ".repeat((indent / 2).min(4) * 2);
            let mut spans = vec![
                rail(),
                Span::raw(pad),
                Span::styled("• ", stb(VIOLET)),
            ];
            spans.extend(parse_inline(rest, st(SILVER)));
            out.push(Line::from(spans));
            continue;
        }

        // Ordered list (1. / 2. / 10.)
        if let Some((num, rest)) = parse_ordered_item(trimmed) {
            let pad = " ".repeat((indent / 2).min(4) * 2);
            let mut spans = vec![
                rail(),
                Span::raw(pad),
                Span::styled(format!("{num}. "), stb(VIOLET)),
            ];
            spans.extend(parse_inline(rest, st(SILVER)));
            out.push(Line::from(spans));
            continue;
        }

        // Paragraf biasa dengan inline markdown
        let mut spans = vec![rail()];
        spans.extend(parse_inline(raw, st(SILVER)));
        out.push(Line::from(spans));
    }

    if out.is_empty() {
        out.push(Line::from(vec![rail()]));
    }
    out
}

fn is_hr(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 3 {
        return false;
    }
    t.chars().all(|c| c == '-' || c == ' ') && t.chars().filter(|&c| c == '-').count() >= 3
        || t.chars().all(|c| c == '*' || c == ' ') && t.chars().filter(|&c| c == '*').count() >= 3
        || t.chars().all(|c| c == '_' || c == ' ') && t.chars().filter(|&c| c == '_').count() >= 3
}

fn parse_heading(s: &str) -> Option<(usize, &str)> {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = s[hashes..].strip_prefix(' ')?;
        let title = rest.trim();
        if !title.is_empty() {
            return Some((hashes, title));
        }
    }
    None
}

fn parse_ordered_item(s: &str) -> Option<(&str, &str)> {
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let after = &s[digits..];
    let rest = after.strip_prefix(". ")?;
    Some((&s[..digits], rest))
}

/// Parser inline Markdown aman: `kode`, **tebal**, dan *miring*.
/// Penanda yang belum ditutup (mis. saat token masih streaming) ditampilkan
/// apa adanya tanpa menelan teks.
pub fn parse_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;

    let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    while i < bytes.len() {
        // Inline code: `...`
        if bytes[i] == b'`' {
            if let Some(rel_end) = text[i + 1..].find('`') {
                let code = &text[i + 1..i + 1 + rel_end];
                if !code.is_empty() {
                    flush(&mut spans, &mut buf, base);
                    spans.push(Span::styled(
                        code.to_string(),
                        Style::default().fg(CYAN_MOON),
                    ));
                    i += rel_end + 2;
                    continue;
                }
            }
        }

        // Bold: **...**
        if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            if let Some(rel_end) = text[i + 2..].find("**") {
                let inner = &text[i + 2..i + 2 + rel_end];
                if !inner.is_empty() && !inner.starts_with(' ') {
                    flush(&mut spans, &mut buf, base);
                    let bold_style = Style::default()
                        .fg(LILAC)
                        .add_modifier(Modifier::BOLD);
                    spans.extend(parse_inline(inner, bold_style));
                    i += rel_end + 4;
                    continue;
                }
            }
        }

        // Italic: *...* (tunggal, bukan **)
        if bytes[i] == b'*' && (i + 1 >= bytes.len() || bytes[i + 1] != b'*') {
            if let Some(rel_end) = text[i + 1..].find('*') {
                let inner = &text[i + 1..i + 1 + rel_end];
                if !inner.is_empty() && !inner.starts_with(' ') && !inner.ends_with(' ') {
                    flush(&mut spans, &mut buf, base);
                    spans.push(Span::styled(
                        inner.to_string(),
                        Style::default()
                            .fg(LAV)
                            .add_modifier(Modifier::ITALIC),
                    ));
                    i += rel_end + 2;
                    continue;
                }
            }
        }

        // Karakter UTF-8 biasa
        let ch = text[i..].chars().next().unwrap();
        buf.push(ch);
        i += ch.len_utf8();
    }

    flush(&mut spans, &mut buf, base);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}
