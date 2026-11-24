//! Desain orisinal "Moonlit" untuk TUI SelenCode — fungsi murni, gampang
//! dites. Identitas khas SelenCode: palet ungu malam (Lilac, Lavender, Violet,
//! Deep Indigo) dengan bulan sabit braille bergradien ungu, bintang kecil emas
//! sebagai aksen tipis, kartu Lunar Observatory terpadu, dan status bar bersih.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---------- palet Moonlit (ungu malam khas SelenCode) ----------

pub const BG: Color = Color::Rgb(13, 10, 26);
pub const LILAC: Color = Color::Rgb(233, 213, 255);
pub const LAV: Color = Color::Rgb(196, 181, 253);
pub const VIOLET: Color = Color::Rgb(167, 139, 250);
pub const DEEP: Color = Color::Rgb(109, 40, 217);
pub const SHADOW: Color = Color::Rgb(76, 29, 149);
pub const BORDER: Color = Color::Rgb(99, 78, 158);
pub const CYAN_MOON: Color = Color::Rgb(125, 211, 252);
pub const SILVER: Color = Color::Rgb(229, 231, 235);
pub const DIM: Color = Color::Rgb(148, 140, 180);
pub const FAINT: Color = Color::Rgb(84, 74, 118);
pub const GOLD: Color = Color::Rgb(252, 211, 77);
pub const MINT: Color = Color::Rgb(110, 231, 160);
pub const ROSE: Color = Color::Rgb(248, 113, 113);

fn st(color: Color) -> Style {
    Style::default().fg(color)
}

fn stb(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

// ---------- bulan sabit braille ----------

/// Peta bit braille: (dx, dy) -> bit. Standar pola braille 2x4.
const BRAILLE_BITS: [(usize, usize, u32); 8] = [
    (0, 0, 0x01),
    (1, 0, 0x08),
    (0, 1, 0x02),
    (1, 1, 0x10),
    (0, 2, 0x04),
    (1, 2, 0x20),
    (0, 3, 0x40),
    (1, 3, 0x80),
];

/// Bulan sabit dari karakter braille: titik menyala bila di dalam lingkaran
/// besar DAN di luar lingkaran geser (potongan sabit). Sel kosong memakai
/// U+2800 (braille blank) agar lebar baris konsisten di semua terminal.
pub fn moon_art(cols: usize, rows: usize) -> Vec<String> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    let w = (cols * 2) as f64;
    let h = (rows * 4) as f64;
    let cx = w * 0.44;
    let cy = h * 0.52;
    let r = h * 0.47;
    let cx2 = cx + w * 0.36;
    let cy2 = cy - h * 0.12;
    let r2 = r * 0.94;
    let mut out = Vec::with_capacity(rows);
    for by in 0..rows {
        let mut s = String::with_capacity(cols * 3);
        for bx in 0..cols {
            let mut bits: u32 = 0;
            for (dx, dy, bit) in BRAILLE_BITS {
                let x = (bx * 2 + dx) as f64;
                let y = (by * 4 + dy) as f64;
                let inside = (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
                let cut = (x - cx2).powi(2) + (y - cy2).powi(2) <= r2 * r2;
                if inside && !cut {
                    bits |= bit;
                }
            }
            s.push(char::from_u32(0x2800 + bits).unwrap_or('\u{2800}'));
        }
        out.push(s);
    }
    out
}

// ---------- wordmark piksel 5x7 khas SelenCode ----------

fn glyph(ch: char) -> [&'static str; 7] {
    match ch {
        'S' => ["01110", "10001", "10000", "01110", "00001", "10001", "01110"],
        'E' => ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
        'L' => ["10000", "10000", "10000", "10000", "10000", "10000", "11111"],
        'N' => ["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
        'C' => ["01110", "10001", "10000", "10000", "10000", "10001", "01110"],
        'O' => ["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
        'D' => ["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
        _ => ["00000", "00000", "00000", "00000", "00000", "00000", "00000"],
    }
}

/// Wordmark "SELENCODE" piksel 5x7: 7 baris, lebar seragam 53 kolom.
pub fn wordmark() -> Vec<String> {
    let mut rows: Vec<String> = vec![String::new(); 7];
    for ch in "SELENCODE".chars() {
        let g = glyph(ch);
        for (i, row) in rows.iter_mut().enumerate() {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&g[i].replace('1', "█").replace('0', " "));
        }
    }
    rows
}

// ---------- fase bulan & bar konteks ----------

const PHASES: [char; 5] = ['○', '◔', '◑', '◕', '●'];

/// Glyph fase bulan menurut persen konteks: 0% ○, ~25% ◔, ~50% ◑,
/// ~75% ◕, penuh ●.
pub fn phase_glyph(pct: u8) -> char {
    let idx = match pct {
        0..=12 => 0,
        13..=37 => 1,
        38..=62 => 2,
        63..=87 => 3,
        _ => 4,
    };
    PHASES[idx]
}

/// Bar konteks 10 sel bersih (`━` terisi / `─` kosong) — tajam di Windows
/// Terminal, VS Code Terminal, Linux, dan Termux.
pub fn context_bar_parts(pct: u8) -> (String, String) {
    let filled = if pct == 0 {
        0
    } else {
        (((pct as usize * 10) + 50) / 100).clamp(1, 10)
    };
    ("━".repeat(filled), "─".repeat(10 - filled))
}

/// Format angka gaya id-ID: 12480 -> "12.480".
pub fn fmt_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}

// ---------- info sambutan ----------

#[derive(Debug, Clone)]
pub struct WelcomeInfo {
    pub version: String,
    pub provider_name: String,
    pub provider_label: String,
    pub model: String,
    pub cwd: String,
    pub tool_count: usize,
    pub session_short: String,
}

impl WelcomeInfo {
    pub fn new(
        provider_name: String,
        provider_label: String,
        model: String,
        cwd: String,
        tool_count: usize,
    ) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            provider_name,
            provider_label,
            model,
            cwd,
            tool_count,
            session_short: "baru".to_string(),
        }
    }

    pub fn with_session_id(mut self, session_id: &str) -> Self {
        let short: String = session_id.chars().take(10).collect();
        if !short.is_empty() {
            self.session_short = short;
        }
        self
    }
}

// ---------- perakitan bingkai Lunar Observatory ----------

fn text_w(s: &str) -> usize {
    s.chars().count()
}

fn spans_w(spans: &[Span]) -> usize {
    spans.iter().map(|s| text_w(&s.content)).sum()
}

fn fit(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = text_w(&span.content);
        if used + w <= budget {
            used += w;
            out.push(span);
        } else {
            let remain = budget.saturating_sub(used);
            if remain > 0 {
                let cut: String = span.content.chars().take(remain).collect();
                out.push(Span::styled(cut, span.style));
            }
            break;
        }
    }
    out
}

fn framed(content: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if width < 2 {
        return Line::from("");
    }
    let inner = width - 2;
    let mut spans = fit(content, inner);
    let used = spans_w(&spans);
    if used < inner {
        spans.push(Span::raw(" ".repeat(inner - used)));
    }
    let mut line: Vec<Span<'static>> = vec![Span::styled("│", st(BORDER))];
    line.append(&mut spans);
    line.push(Span::styled("│", st(BORDER)));
    Line::from(line)
}

fn divider_line(width: usize) -> Line<'static> {
    if width < 2 {
        return Line::from("");
    }
    Line::from(vec![
        sp("├", st(BORDER)),
        sp("─".repeat(width - 2), st(FAINT)),
        sp("┤", st(BORDER)),
    ])
}

/// Bingkai atas asimetris khas SelenCode: `╭── ☾ SelenCode · edisi Moonlit ─────── v0.1.4 ──╮`
fn top_frame(width: usize, left_tag: &str, right_tag: &str) -> Line<'static> {
    if width < 10 {
        return Line::from(Span::styled(
            format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
            st(BORDER),
        ));
    }
    let inner = width - 2;
    let left = format!("── {left_tag} ");
    let right = format!(" {right_tag} ──");
    let lw = text_w(&left);
    let rw = text_w(&right);
    if lw + rw >= inner {
        let cut = truncate_chars(left_tag, inner.saturating_sub(2));
        let fill = inner.saturating_sub(text_w(&cut) + 2);
        return Line::from(vec![
            sp("╭─", st(BORDER)),
            sp(format!(" {cut} "), stb(LAV)),
            sp(format!("{}╮", "─".repeat(fill.saturating_sub(1))), st(BORDER)),
        ]);
    }
    let mid = inner - lw - rw;
    Line::from(vec![
        sp("╭── ", st(BORDER)),
        sp(left_tag.to_string(), stb(LAV)),
        sp(format!(" {} ", "─".repeat(mid)), st(BORDER)),
        sp(right_tag.to_string(), st(DIM)),
        sp(" ──╮", st(BORDER)),
    ])
}

/// Bingkai bawah terpadu dengan footer badge: `╰── sandbox: folder ini saja ──── /help ──╯`
fn bottom_frame(width: usize, left_note: &str, right_note: &str) -> Line<'static> {
    if width < 10 {
        return Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
            st(BORDER),
        ));
    }
    let inner = width - 2;
    let left = format!("── {left_note} ");
    let right = format!(" {right_note} ──");
    let lw = text_w(&left);
    let rw = text_w(&right);
    if lw + rw >= inner {
        return Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), st(BORDER)));
    }
    let mid = inner - lw - rw;
    Line::from(vec![
        sp("╰── ", st(BORDER)),
        sp(left_note.to_string(), st(DIM)),
        sp(format!(" {} ", "─".repeat(mid)), st(BORDER)),
        sp(right_note.to_string(), st(VIOLET)),
        sp(" ──╯", st(BORDER)),
    ])
}

fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

// ---------- layar sambutan orisinal Moonlit ----------

const TOOL_ROWS: [(&str, &str); 6] = [
    ("file    ", "list_files · read_file · write_file · edit_file"),
    ("cari    ", "search_text · outline (AST tree-sitter)"),
    ("shell   ", "run_shell (izin dulu / allowlist)"),
    ("git     ", "git_status · git_diff · git_commit"),
    ("web     ", "fetch_url (anti-SSRF)"),
    ("agent   ", "update_plan · delegate · mcp__*"),
];

const CMD_ROWS: [(&str, &str); 6] = [
    ("/model    ", "ganti provider · input model manual"),
    ("/provider ", "pasang provider baru & API key"),
    ("/plan     ", "mode baca-saja berencana"),
    ("/new      ", "mulai sesi baru · /compact ringkas"),
    ("/sessions ", "riwayat sesi · /models tarik API"),
    ("/help     ", "semua perintah · ketik / palette"),
];

/// Layar sambutan Moonlit orisinal selebar `width` (wide >= 96, HP < 96).
/// Semua baris dijamin `<= width`.
pub fn welcome_lines(width: u16, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let width = (width as usize).max(4);
    if width >= 96 {
        welcome_wide(width, info)
    } else {
        welcome_narrow(width, info)
    }
}

fn welcome_wide(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let inner = width - 2;
    let mut out: Vec<Line<'static>> = Vec::new();

    // 1. Top frame asimetris ungu khas SelenCode
    out.push(top_frame(
        width,
        "☾ SelenCode · edisi Moonlit",
        &format!("v{}", info.version),
    ));

    // 2. Zona Hero: bulan sabit braille gradien ungu di kiri + wordmark 2-nada
    //    ungu + strip status sesi di kanan.
    let moon = moon_art(20, 11);
    let mark = wordmark();
    let mut right_rows: Vec<Vec<Span<'static>>> = Vec::new();
    right_rows.push(vec![]);
    for (i, mrow) in mark.iter().enumerate() {
        let color = match i {
            0..=1 => LILAC,
            2..=4 => VIOLET,
            _ => DEEP,
        };
        right_rows.push(vec![sp(mrow.clone(), stb(color))]);
    }
    right_rows.push(vec![
        sp("☾ Selena", stb(LAV)),
        sp("  ·  asisten coding terminal  ·  full Rust", st(DIM)),
    ]);
    right_rows.push(vec![
        sp("● ", st(MINT)),
        sp(truncate_chars(&info.model, 28), stb(LILAC)),
        sp("  via  ", st(FAINT)),
        sp(truncate_chars(&info.provider_label, 30), st(SILVER)),
    ]);
    right_rows.push(vec![
        sp("⌂ ", st(VIOLET)),
        sp(truncate_chars(&info.cwd, 42), st(DIM)),
        sp("   sesi ", st(FAINT)),
        sp(info.session_short.clone(), st(LAV)),
    ]);

    for i in 0..11 {
        let mcolor = match i {
            0..=2 => LILAC,
            3..=7 => VIOLET,
            _ => DEEP,
        };
        let mut row: Vec<Span<'static>> = vec![Span::raw("   "), sp(moon[i].clone(), st(mcolor))];
        if i == 1 {
            row.push(sp(" ✦", stb(GOLD)));
        } else if i == 3 {
            row.push(sp("  ✧", st(LAV)));
        } else if i == 8 {
            row.push(sp(" ·", st(FAINT)));
        }
        let cur = spans_w(&row);
        if cur < 30 {
            row.push(Span::raw(" ".repeat(30 - cur)));
        }
        if let Some(rr) = right_rows.get(i) {
            row.extend(rr.iter().cloned());
        }
        out.push(framed(row, width));
    }

    // 3. Pemisah halus antar Hero & Dock
    out.push(divider_line(width));

    // 4. Dock dua kolom: Kemampuan (kiri) & Perintah (kanan) dengan pemisah │
    let split_col = (inner * 58 / 100).max(48);
    let dock_row = |left: Vec<Span<'static>>, right: Vec<Span<'static>>| -> Line<'static> {
        let mut content = fit(left, split_col.saturating_sub(2));
        let cur = spans_w(&content);
        if cur < split_col {
            content.push(Span::raw(" ".repeat(split_col - cur)));
        }
        content.push(sp("│ ", st(FAINT)));
        content.extend(right);
        framed(content, width)
    };

    out.push(dock_row(
        vec![
            sp("  ☾ Kemampuan ", stb(LAV)),
            sp(format!("({} tool aktif)", info.tool_count), st(DIM)),
        ],
        vec![sp("⌘ Perintah", stb(LAV))],
    ));

    for ((tk, tv), (ck, cv)) in TOOL_ROWS.iter().zip(CMD_ROWS.iter()) {
        out.push(dock_row(
            vec![
                sp("  › ", st(VIOLET)),
                sp(*tk, stb(VIOLET)),
                sp(*tv, st(SILVER)),
            ],
            vec![sp(*ck, stb(LAV)), sp(*cv, st(DIM))],
        ));
    }

    // 5. Bottom frame terpadu
    out.push(bottom_frame(
        width,
        "sandbox: folder ini saja",
        "/help untuk bantuan",
    ));

    // 6. Sapaan singkat di bawah kartu
    out.push(Line::from(""));
    out.push(Line::from(fit(
        vec![
            sp("  ☾ ", stb(VIOLET)),
            sp("Selena siap. ", stb(LILAC)),
            sp("Ketik pesanmu, ", st(DIM)),
            sp("Alt+Enter", st(LAV)),
            sp(" baris baru, ", st(DIM)),
            sp("/plan", st(LAV)),
            sp(" untuk rencana dulu.", st(DIM)),
        ],
        width,
    )));
    out
}

fn welcome_narrow(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(top_frame(
        width,
        "☾ SelenCode",
        &format!("v{}", info.version),
    ));

    let moon = moon_art(12, 7);
    for (i, mrow) in moon.iter().enumerate() {
        let mcolor = match i {
            0..=1 => LILAC,
            2..=4 => VIOLET,
            _ => DEEP,
        };
        let mut row: Vec<Span<'static>> = vec![Span::raw("  "), sp(mrow.clone(), st(mcolor))];
        match i {
            1 => {
                row.push(sp(" ✦  ", stb(GOLD)));
                row.push(sp("SELENCODE", stb(LILAC)));
            }
            2 => {
                row.push(sp("    ", st(DIM)));
                row.push(sp("Selena · edisi Moonlit", st(LAV)));
            }
            4 => {
                row.push(sp("    ", st(DIM)));
                row.push(sp("● ", st(MINT)));
                row.push(sp(
                    truncate_chars(&info.model, width.saturating_sub(22)),
                    stb(SILVER),
                ));
            }
            5 => {
                row.push(sp("    ", st(DIM)));
                row.push(sp(
                    format!("{} · {} tool aktif", info.provider_name, info.tool_count),
                    st(DIM),
                ));
            }
            _ => {}
        }
        out.push(framed(row, width));
    }

    out.push(divider_line(width));
    out.push(framed(
        vec![
            sp(" ⌂ ", st(VIOLET)),
            sp(truncate_chars(&info.cwd, width.saturating_sub(6)), st(DIM)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp(" ☾ Tools: ", stb(LAV)),
            sp("file, search, outline, shell, git, web, plan, mcp", st(SILVER)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp(" ⌘ ", st(VIOLET)),
            sp("/plan", stb(LAV)),
            sp(" rencana  ", st(DIM)),
            sp("/model", stb(LAV)),
            sp(" model  ", st(DIM)),
            sp("/help", stb(LAV)),
            sp(" bantuan", st(DIM)),
        ],
        width,
    ));
    out.push(bottom_frame(
        width,
        "sandbox aktif",
        &format!("sesi {}", info.session_short),
    ));
    out
}

/// Sambutan ringkas untuk sesi resume: 1 baris bergaya Moonlit ungu.
pub fn resume_lines(info: &WelcomeInfo) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        sp("☾ ", stb(VIOLET)),
        sp("SelenCode", stb(LILAC)),
        sp(
            format!(
                " — sesi dilanjutkan · {}/{}",
                info.provider_name, info.model
            ),
            st(DIM),
        ),
    ])]
}

// ---------- status bar Moonlit (ungu bersih) ----------

/// Baris status Moonlit: model + fase bulan + bar konteks tajam + token sesi
/// + langkah + cwd (+ indikator bekerja saat sibuk).
pub fn status_line(
    model: &str,
    pct: u8,
    session_in: u64,
    session_out: u64,
    step_current: usize,
    step_max: usize,
    cwd: &str,
    busy: bool,
) -> Line<'static> {
    let (filled, empty) = context_bar_parts(pct);
    let mut spans = vec![
        sp(" ☾ ", stb(VIOLET)),
        sp(truncate_chars(model, 22), stb(LAV)),
        sp(" │ ", st(FAINT)),
        sp(format!("{} ", phase_glyph(pct)), stb(VIOLET)),
        sp(filled, st(VIOLET)),
        sp(empty, st(FAINT)),
        sp(format!(" {pct}%"), st(DIM)),
        sp(" │ ", st(FAINT)),
        sp(
            format!(
                "token sesi {} in / {} out",
                fmt_tokens(session_in),
                fmt_tokens(session_out)
            ),
            st(DIM),
        ),
        sp(" │ ", st(FAINT)),
        sp(format!("langkah {step_current}/{step_max}"), st(DIM)),
        sp(" │ ", st(FAINT)),
        sp(truncate_chars(cwd, 30), st(DIM)),
    ];
    if busy {
        spans.push(sp(" │ ", st(FAINT)));
        spans.push(sp("✦ bekerja…", stb(GOLD)));
    }
    Line::from(spans)
}

// ---------- banner mode polos ----------

/// Banner teks sederhana untuk mode polos (non-TUI): bulan braille kecil
/// berwarna violet (ANSI truecolor) + sapaan + baris provider/perintah.
pub fn plain_banner(provider_name: &str, model: &str) -> String {
    let mut out = String::new();
    for row in moon_art(10, 5) {
        out.push_str(&format!("\u{1b}[38;2;167;139;250m{row}\u{1b}[0m\n"));
    }
    out.push_str(
        "SelenCode — halo, aku Selena. Ketik perintah, /help untuk bantuan, /exit untuk keluar.\n",
    );
    out.push_str(&format!(
        "Perintah: /plan · /model [nama] · /sessions · /help · /exit — provider aktif: {provider_name} ({model})"
    ));
    out
}
