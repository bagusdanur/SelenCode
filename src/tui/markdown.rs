//! Renderer Markdown ringan untuk jawaban Selena di TUI (murni Rust, tanpa
//! dependensi eksternal). Mengubah heading, **bold**, *italic*, `kode inline`,
//! daftar bernomor/bullet, blockquote, garis pemisah, dan blok kode ```...```
//! menjadi baris Ratatui berpalet Moonlit ungu yang rapi, terbungkus per kata,
//! dengan garis tepi `│` utuh dan hanging-indent presisi di desktop & HP Termux.

use super::moonlit::{BORDER, CYAN_MOON, DEEP, DIM, FAINT, LAV, LILAC, SILVER, VIOLET};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const CODE_INLINE_BG: Color = Color::Rgb(34, 24, 62);

fn st(c: Color) -> Style {
    Style::default().fg(c)
}

fn stb(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

fn rail(narrow: bool) -> Span<'static> {
    let s = if narrow { " │ " } else { "  │ " };
    Span::styled(s.to_string(), st(BORDER))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Pecah rangkaian `Span` berwarna agar muat dalam `max_width` kolom secara
/// utuh per kata, dengan `first_prefix` pada baris pertama dan `cont_prefix`
/// pada baris lanjutan (menjaga garis tepi `│` & hanging-indent tetap lurus!).
pub fn wrap_styled_spans(
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
    body: Vec<Span<'static>>,
    max_width: usize,
) -> Vec<Line<'static>> {
    let limit = max_width.max(20);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = first_prefix;
    let mut cur_w = spans_width(&cur);
    let mut on_first_line = true;

    let push_styled = |line: &mut Vec<Span<'static>>, text: &str, style: Style| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = line.last_mut() {
            if last.style == style {
                last.content.to_mut().push_str(text);
                return;
            }
        }
        line.push(Span::styled(text.to_string(), style));
    };

    // Pecah body menjadi token (kata atau spasi) sambil mempertahankan Style tiap span.
    let mut tokens: Vec<(String, Style, bool)> = Vec::new();
    for sp in body {
        let style = sp.style;
        let text = sp.content.as_ref();
        let mut buf = String::new();
        let mut in_ws: Option<bool> = None;
        for ch in text.chars() {
            let ws = ch.is_whitespace();
            if in_ws == Some(ws) {
                buf.push(ch);
            } else {
                if !buf.is_empty() {
                    tokens.push((std::mem::take(&mut buf), style, in_ws.unwrap_or(false)));
                }
                buf.push(ch);
                in_ws = Some(ws);
            }
        }
        if !buf.is_empty() {
            tokens.push((buf, style, in_ws.unwrap_or(false)));
        }
    }

    let first_prefix_w = cur_w;
    let cont_prefix_w = spans_width(&cont_prefix);

    for (tok, style, is_ws) in tokens {
        let tlen = tok.chars().count();
        let line_prefix_w = if on_first_line {
            first_prefix_w
        } else {
            cont_prefix_w
        };
        let at_line_start = cur_w == line_prefix_w;

        if is_ws {
            // Jangan taruh spasi di awal baris baru hasil wrap.
            if at_line_start {
                continue;
            }
            if cur_w + tlen <= limit {
                push_styled(&mut cur, &tok, style);
                cur_w += tlen;
            } else {
                // Spasi menyentuh batas kanan -> pindah baris baru.
                lines.push(Line::from(cur));
                cur = cont_prefix.clone();
                cur_w = cont_prefix_w;
                on_first_line = false;
            }
            continue;
        }

        if cur_w + tlen <= limit {
            push_styled(&mut cur, &tok, style);
            cur_w += tlen;
            continue;
        }

        // Kata tidak muat di sisa baris saat ini -> pindah baris baru bila bukan di awal baris.
        if !at_line_start {
            lines.push(Line::from(cur));
            cur = cont_prefix.clone();
            cur_w = cont_prefix_w;
            on_first_line = false;
        }

        let avail = limit.saturating_sub(cur_w).max(6);
        if tlen <= avail {
            push_styled(&mut cur, &tok, style);
            cur_w += tlen;
        } else {
            // Kata tunggal lebih panjang dari lebar baris (URL/path): pecah aman per karakter UTF-8.
            let mut chunk = String::new();
            let mut clen = 0usize;
            for ch in tok.chars() {
                let cap = limit.saturating_sub(cur_w).max(6);
                if clen >= cap {
                    push_styled(&mut cur, &chunk, style);
                    chunk.clear();
                    clen = 0;
                    lines.push(Line::from(cur));
                    cur = cont_prefix.clone();
                    cur_w = cont_prefix_w;
                    on_first_line = false;
                }
                chunk.push(ch);
                clen += 1;
            }
            if !chunk.is_empty() {
                push_styled(&mut cur, &chunk, style);
                cur_w += clen;
            }
        }
    }

    lines.push(Line::from(cur));
    lines
}

/// Render teks Markdown jawaban assistant menjadi baris Ratatui bergaya Moonlit
/// yang sudah dibungkus rapi sesuai lebar terminal `width` (aman di HP Termux
/// maupun desktop, menjaga garis tepi `│` & hanging-indent tidak pernah patah).
pub fn render_markdown_width(text: &str, width: u16) -> Vec<Line<'static>> {
    let narrow = width < 72;
    let max_w = (width as usize).saturating_sub(1).clamp(24, 110);
    let inner_code_w = max_w.saturating_sub(if narrow { 6 } else { 7 }).max(16);

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
                    let dashes = inner_code_w.saturating_sub(7).max(4);
                    vec![
                        rail(narrow),
                        Span::styled(format!("╭─ kode {}", "─".repeat(dashes)), st(BORDER)),
                    ]
                } else {
                    let dashes = inner_code_w
                        .saturating_sub(4 + lang.chars().count())
                        .max(3);
                    vec![
                        rail(narrow),
                        Span::styled("╭─ ", st(BORDER)),
                        Span::styled(lang.to_string(), stb(LAV)),
                        Span::styled(format!(" {}", "─".repeat(dashes)), st(BORDER)),
                    ]
                };
                out.push(Line::from(header));
            } else {
                in_code = false;
                out.push(Line::from(vec![
                    rail(narrow),
                    Span::styled(format!("╰{}", "─".repeat(inner_code_w)), st(BORDER)),
                ]));
            }
            continue;
        }

        if in_code {
            let pfx = vec![rail(narrow), Span::styled("│ ", st(BORDER))];
            if raw.is_empty() {
                out.push(Line::from(pfx));
            } else {
                let code_spans = vec![Span::styled(raw.to_string(), st(CYAN_MOON))];
                out.extend(wrap_styled_spans(pfx.clone(), pfx, code_spans, max_w));
            }
            continue;
        }

        // Baris kosong: pertahankan maksimal 1 baris kosong beruntun agar lapang tapi tidak bolong.
        if raw.trim().is_empty() {
            blank_run += 1;
            if blank_run == 1 && !out.is_empty() {
                out.push(Line::from(vec![rail(narrow)]));
            }
            continue;
        }
        blank_run = 0;

        // Horizontal rule (--- / *** / ___)
        if is_hr(trimmed) {
            out.push(Line::from(vec![
                rail(narrow),
                Span::styled("─".repeat(inner_code_w), st(FAINT)),
            ]));
            continue;
        }

        // Heading (# / ## / ###)
        if let Some((level, heading)) = parse_heading(trimmed) {
            if !out.is_empty() {
                let last_empty_rail = out.last().map(|l| l.spans.len() == 1).unwrap_or(false);
                if !last_empty_rail {
                    out.push(Line::from(vec![rail(narrow)]));
                }
            }
            let (bullet, color) = match level {
                1 => ("◆ ", LILAC),
                2 => ("▸ ", VIOLET),
                _ => ("· ", LAV),
            };
            let first_pfx = vec![rail(narrow), Span::styled(bullet.to_string(), stb(color))];
            let cont_pfx = vec![rail(narrow), Span::raw("  ")];
            let body = parse_inline(heading, stb(color));
            out.extend(wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
            continue;
        }

        // Blockquote (> ...)
        if let Some(quote) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix('>')) {
            let pfx = vec![rail(narrow), Span::styled("▎ ", st(DEEP))];
            let body = parse_inline(
                quote.trim_start(),
                st(DIM).add_modifier(Modifier::ITALIC),
            );
            out.extend(wrap_styled_spans(pfx.clone(), pfx, body, max_w));
            continue;
        }

        // Unordered list (- / * / +) dengan hanging-indent rapi
        let indent = raw.len() - trimmed.len();
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let pad_len = (indent / 2).min(3) * 2;
            let pad = " ".repeat(pad_len);
            let first_pfx = vec![
                rail(narrow),
                Span::raw(pad.clone()),
                Span::styled("• ", stb(VIOLET)),
            ];
            let cont_pfx = vec![rail(narrow), Span::raw(format!("{pad}  "))];
            let body = parse_inline(rest, st(SILVER));
            out.extend(wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
            continue;
        }

        // Ordered list (1. / 2. / 10.) dengan hanging-indent rapi
        if let Some((num, rest)) = parse_ordered_item(trimmed) {
            let pad_len = (indent / 2).min(3) * 2;
            let pad = " ".repeat(pad_len);
            let num_prefix = format!("{num}. ");
            let hang = " ".repeat(pad_len + num_prefix.chars().count());
            let first_pfx = vec![
                rail(narrow),
                Span::raw(pad),
                Span::styled(num_prefix, stb(VIOLET)),
            ];
            let cont_pfx = vec![rail(narrow), Span::raw(hang)];
            let body = parse_inline(rest, st(SILVER));
            out.extend(wrap_styled_spans(first_pfx, cont_pfx, body, max_w));
            continue;
        }

        // Paragraf biasa dengan inline markdown & wrap menjaga garis tepi `│`
        let pfx = vec![rail(narrow)];
        let body = parse_inline(raw.trim_end(), st(SILVER));
        out.extend(wrap_styled_spans(pfx.clone(), pfx, body, max_w));
    }

    if out.is_empty() {
        out.push(Line::from(vec![rail(narrow)]));
    }
    out
}

/// Render teks Markdown jawaban assistant dengan lebar baku desktop (96 kolom).
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    render_markdown_width(text, 96)
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
                        Style::default().fg(CYAN_MOON).bg(CODE_INLINE_BG),
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
