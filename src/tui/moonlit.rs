//! Desain "Moonlit × Hermes" untuk TUI SelenCode — fungsi murni, gampang
//! dites. Menggabungkan estetika Hermes-Agent (banner 3D berbayang di atas
//! kartu berbingkai judul, panel dua kolom emblem + daftar tool & perintah,
//! footer ringkas, status bar bergaya bar) dengan identitas Moonlit SelenCode
//! (bulan sabit braille, bintang emas, aksen gold/amber & lavender).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---------- palet Moonlit × Hermes ----------

pub const BG: Color = Color::Rgb(13, 10, 26);
pub const VIOLET: Color = Color::Rgb(167, 139, 250);
pub const DEEP: Color = Color::Rgb(109, 40, 217);
pub const LAV: Color = Color::Rgb(196, 181, 253);
pub const SILVER: Color = Color::Rgb(229, 231, 235);
pub const DIM: Color = Color::Rgb(148, 140, 180);
pub const FAINT: Color = Color::Rgb(94, 84, 130);
pub const GOLD: Color = Color::Rgb(252, 211, 77);
pub const GOLD_BRIGHT: Color = Color::Rgb(254, 240, 138);
pub const AMBER: Color = Color::Rgb(245, 158, 11);
pub const AMBER_DIM: Color = Color::Rgb(168, 132, 68);
pub const SHADOW: Color = Color::Rgb(138, 92, 38);
pub const BORDER: Color = Color::Rgb(150, 118, 62);

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

/// Bulan sabit dari karakter braille: titik menyala bila berada di dalam
/// lingkaran besar DAN di luar lingkaran geser (potongan sabit). Sel kosong
/// memakai U+2800 (braille blank) agar lebar baris konsisten di semua
/// terminal. Semua baris punya jumlah karakter persis `cols`.
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

// ---------- banner 3D ANSI Shadow bergaya Hermes ----------

/// Banner 3D "SELENCODE" (6 baris, 83 kolom) dengan blok `█` + bayangan
/// box-drawing `╗═╝║╚╔` bergaya Hermes-Agent.
const BANNER_3D_ROWS: [&str; 6] = [
    "███████╗ ███████╗ ██╗      ███████╗ ███╗   ██╗  ██████╗  ██████╗  ██████╗  ███████╗",
    "██╔════╝ ██╔════╝ ██║      ██╔════╝ ████╗  ██║ ██╔════╝ ██╔═══██╗ ██╔══██╗ ██╔════╝",
    "███████╗ █████╗   ██║      █████╗   ██╔██╗ ██║ ██║      ██║   ██║ ██║  ██║ █████╗  ",
    "╚════██║ ██╔══╝   ██║      ██╔══╝   ██║╚██╗██║ ██║      ██║   ██║ ██║  ██║ ██╔══╝  ",
    "███████║ ███████╗ ███████╗ ███████╗ ██║ ╚████║ ╚██████╗ ╚██████╔╝ ██████╔╝ ███████╗",
    "╚══════╝ ╚══════╝ ╚══════╝ ╚══════╝ ╚═╝  ╚═══╝  ╚═════╝  ╚═════╝  ╚═════╝  ╚══════╝",
];

/// Warna gradien vertikal per baris banner 3D (emas terang -> amber hangat).
const BANNER_GRADIENT: [Color; 6] = [
    GOLD_BRIGHT,
    GOLD_BRIGHT,
    GOLD,
    AMBER,
    AMBER,
    SHADOW,
];

/// Render banner 3D "SELENCODE" bergaya Hermes dengan gradien vertikal pada
/// badan huruf (`█`) dan warna bayangan redup pada sisi 3D (`╗═╝║╚╔`).
pub fn banner_3d_lines(width: usize) -> Vec<Line<'static>> {
    let art_w = BANNER_3D_ROWS[0].chars().count();
    let pad = width.saturating_sub(art_w) / 2;
    let mut out = Vec::with_capacity(BANNER_3D_ROWS.len());
    for (row_idx, raw) in BANNER_3D_ROWS.iter().enumerate() {
        let face_color = BANNER_GRADIENT[row_idx];
        let mut spans: Vec<Span<'static>> = Vec::new();
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        // Kelompokkan run karakter sejenis agar jumlah Span tetap ringkas.
        let mut run = String::new();
        let mut run_kind: u8 = 0; // 0 = spasi, 1 = blok █, 2 = bayangan 3D
        let flush = |spans: &mut Vec<Span<'static>>, run: &mut String, kind: u8| {
            if run.is_empty() {
                return;
            }
            let text = std::mem::take(run);
            match kind {
                1 => spans.push(Span::styled(text, stb(face_color))),
                2 => spans.push(Span::styled(text, st(SHADOW))),
                _ => spans.push(Span::raw(text)),
            }
        };
        for ch in raw.chars() {
            let kind = match ch {
                '█' => 1,
                ' ' => 0,
                _ => 2,
            };
            if kind != run_kind {
                flush(&mut spans, &mut run, run_kind);
                run_kind = kind;
            }
            run.push(ch);
        }
        flush(&mut spans, &mut run, run_kind);
        out.push(Line::from(fit(spans, width)));
    }
    out
}

// ---------- wordmark balok 5x7 (dipertahankan untuk kompatibilitas) ----------

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

/// Wordmark "SELENCODE" balok 5x7: 7 baris, semua baris lebar karakter sama
/// (9 huruf x 6 - 1 = 53).
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

/// Bar konteks 10 sel: "▓" terisi / "░" kosong — dikembalikan sebagai
/// (terisi, kosong) agar bisa diwarnai terpisah.
pub fn context_bar_parts(pct: u8) -> (String, String) {
    let filled = ((pct as usize * 10) + 50) / 100;
    let filled = filled.min(10);
    ("▓".repeat(filled), "░".repeat(10 - filled))
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
    /// Nama pendek provider, mis. "router".
    pub provider_name: String,
    /// Label tampil provider, mis. "router (router.ryukomik.com)".
    pub provider_label: String,
    pub model: String,
    pub cwd: String,
    pub tool_count: usize,
    /// Potongan pendek ID sesi aktif (mis. 8 karakter pertama UUID).
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
        let short: String = session_id.chars().take(12).collect();
        if !short.is_empty() {
            self.session_short = short;
        }
        self
    }
}

// ---------- perakitan baris berbingkai ----------

fn text_w(s: &str) -> usize {
    s.chars().count()
}

fn spans_w(spans: &[Span]) -> usize {
    spans.iter().map(|s| text_w(&s.content)).sum()
}

/// Potong spans agar total lebar <= budget (per karakter, gaya dipertahankan).
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

/// Bungkus konten menjadi 1 baris berbingkai "│ ... │" selebar persis
/// `width` (dipotong aman bila kepanjangan, di-pad bila kurang).
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

fn border_line(width: usize, left: char, right: char) -> Line<'static> {
    if width < 2 {
        return Line::from("");
    }
    let mut s = String::with_capacity(width);
    s.push(left);
    for _ in 0..width - 2 {
        s.push('─');
    }
    s.push(right);
    Line::from(Span::styled(s, st(BORDER)))
}

/// Bingkai atas berjudul di tengah: `╭──── Title ────╮` (gaya Hermes).
fn titled_top_border(width: usize, title: &str) -> Line<'static> {
    if width < 6 {
        return border_line(width, '╭', '╮');
    }
    let inner = width - 2;
    let label = format!(" {title} ");
    let lw = text_w(&label);
    if lw >= inner {
        let cut: String = label.chars().take(inner).collect();
        return Line::from(vec![
            sp("╭", st(BORDER)),
            sp(cut, stb(GOLD)),
            sp("╮", st(BORDER)),
        ]);
    }
    let rem = inner - lw;
    let left_dash = rem / 2;
    let right_dash = rem - left_dash;
    Line::from(vec![
        sp(format!("╭{}", "─".repeat(left_dash)), st(BORDER)),
        sp(label, stb(GOLD)),
        sp(format!("{}╮", "─".repeat(right_dash)), st(BORDER)),
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

// ---------- layar sambutan (Hermes × Moonlit) ----------

const TOOL_GROUPS: [(&str, &str); 7] = [
    ("file:   ", "list_files, read_file, write_file, edit_file"),
    ("search: ", "search_text, outline (AST tree-sitter)"),
    ("shell:  ", "run_shell (approval / allowlist)"),
    ("git:    ", "git_status, git_diff, git_commit"),
    ("web:    ", "fetch_url (anti-SSRF)"),
    ("plan:   ", "update_plan, delegate (subagent paralel)"),
    ("mcp:    ", "mcp__* (server MCP eksternal)"),
];

const CMD_LIST: [(&str, &str); 6] = [
    ("/plan     ", "rencana dulu, eksekusi sesudah disetujui"),
    ("/model    ", "lihat & ganti provider/model aktif"),
    ("/sessions ", "daftar 10 sesi terakhir di folder ini"),
    ("/resume   ", "info sesi aktif (CLI: selen resume)"),
    ("/help     ", "tampilkan semua perintah"),
    ("/exit     ", "keluar dari SelenCode"),
];

/// Baris-baris sambutan bergaya Hermes × Moonlit selebar `width`.
/// Tata letak lebar bila `width >= 90`, tata letak HP (Termux) bila lebih sempit.
/// Semua baris dijamin `<= width`.
pub fn welcome_lines(width: u16, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let width = (width as usize).max(4);
    if width >= 90 {
        welcome_wide(width, info)
    } else {
        welcome_narrow(width, info)
    }
}

fn welcome_wide(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    // 1. Banner 3D "SELENCODE" berbayang di atas kartu (persis gaya Hermes).
    out.extend(banner_3d_lines(width));
    out.push(Line::from(""));

    // 2. Kartu utama berbingkai judul di tengah.
    let title = format!("SelenCode v{} · edisi Moonlit · Selena", info.version);
    out.push(titled_top_border(width, &title));

    // Kolom kiri (emblem bulan sabit braille + info sesi) lebar tetap 34 sel.
    const LEFT_COL: usize = 34;
    let moon = moon_art(20, 11);
    let mut left_rows: Vec<Vec<Span<'static>>> = Vec::new();
    left_rows.push(vec![]);
    for (i, mrow) in moon.iter().enumerate() {
        let color = if i <= 2 {
            GOLD_BRIGHT
        } else if i <= 7 {
            GOLD
        } else {
            AMBER
        };
        let mut row = vec![Span::raw("     "), sp(mrow.clone(), st(color))];
        if i == 1 {
            row.push(sp(" ✦", stb(GOLD_BRIGHT)));
        } else if i == 3 {
            row.push(sp("  ✧", st(LAV)));
        }
        left_rows.push(row);
    }
    left_rows.push(vec![]);
    left_rows.push(vec![
        sp("  ", st(DIM)),
        sp(truncate_chars(&info.model, 17), stb(GOLD)),
        sp(" / ", st(BORDER)),
        sp(truncate_chars(&info.provider_name, 10), st(AMBER_DIM)),
    ]);
    left_rows.push(vec![
        sp("  ", st(DIM)),
        sp(truncate_chars(&info.cwd, LEFT_COL - 4), st(DIM)),
    ]);
    left_rows.push(vec![
        sp("  Session: ", st(DIM)),
        sp(truncate_chars(&info.session_short, 12), st(LAV)),
        sp(" · Selena", st(DIM)),
    ]);
    left_rows.push(vec![
        sp("  Provider: ", st(DIM)),
        sp(truncate_chars(&info.provider_label, LEFT_COL - 13), st(DIM)),
    ]);

    // Kolom kanan: Available Tools + Available Commands (Perintah) + ringkasan.
    let mut right_rows: Vec<Vec<Span<'static>>> = Vec::new();
    right_rows.push(vec![sp("Available Tools", stb(GOLD))]);
    for (cat, list) in TOOL_GROUPS {
        right_rows.push(vec![
            sp(cat, st(AMBER_DIM)),
            sp(list, st(SILVER)),
        ]);
    }
    right_rows.push(vec![]);
    right_rows.push(vec![sp("Available Commands (Perintah)", stb(GOLD))]);
    for (cmd, desc) in CMD_LIST {
        right_rows.push(vec![
            sp(cmd, stb(LAV)),
            sp(desc, st(SILVER)),
        ]);
    }
    right_rows.push(vec![]);
    right_rows.push(vec![
        sp(format!("{} tool aktif", info.tool_count), stb(GOLD)),
        sp("   ", st(DIM)),
        sp(format!("{} perintah", CMD_LIST.len()), st(AMBER_DIM)),
        sp("   ", st(DIM)),
        sp("sandbox: folder ini saja", st(DIM)),
        sp("   ", st(DIM)),
        sp("/help", st(LAV)),
    ]);

    let total_rows = left_rows.len().max(right_rows.len());
    for i in 0..total_rows {
        let mut content: Vec<Span<'static>> = left_rows.get(i).cloned().unwrap_or_default();
        let cur = spans_w(&content);
        if cur < LEFT_COL {
            content.push(Span::raw(" ".repeat(LEFT_COL - cur)));
        } else {
            content = fit(content, LEFT_COL.saturating_sub(1));
            let c2 = spans_w(&content);
            if c2 < LEFT_COL {
                content.push(Span::raw(" ".repeat(LEFT_COL - c2)));
            }
        }
        if let Some(r) = right_rows.get(i) {
            content.extend(r.iter().cloned());
        }
        out.push(framed(content, width));
    }

    out.push(border_line(width, '╰', '╯'));

    // 3. Baris sambutan & tip di bawah kartu (persis gaya Hermes).
    out.push(Line::from(""));
    out.push(Line::from(fit(
        vec![
            sp("Welcome to ", st(SILVER)),
            sp("SelenCode", stb(GOLD)),
            sp(" (Selena)! Ketik pesanmu atau ", st(SILVER)),
            sp("/help", stb(LAV)),
            sp(" untuk daftar perintah.", st(SILVER)),
        ],
        width,
    )));
    out.push(Line::from(fit(
        vec![
            sp("✦ Tip: ", stb(AMBER)),
            sp(
                "pakai /plan untuk mode baca-saja berencana, Alt+Enter untuk baris baru, Ctrl+C untuk keluar.",
                st(AMBER_DIM),
            ),
        ],
        width,
    )));
    out
}

fn welcome_narrow(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let title = format!("☾ SelenCode v{} · Moonlit", info.version);
    out.push(titled_top_border(width, &title));

    let moon = moon_art(12, 7);
    for (i, mrow) in moon.iter().enumerate() {
        let color = if i <= 2 { GOLD_BRIGHT } else if i <= 4 { GOLD } else { AMBER };
        let mut content: Vec<Span<'static>> = vec![Span::raw("  "), sp(mrow.clone(), st(color))];
        match i {
            1 => {
                content.push(sp(" ✦  ", stb(GOLD_BRIGHT)));
                content.push(sp("SELENCODE", stb(GOLD)));
            }
            2 => {
                content.push(sp("    ", st(DIM)));
                content.push(sp("Selena · edisi Moonlit", st(LAV)));
            }
            4 => {
                content.push(sp("    ", st(DIM)));
                content.push(sp(truncate_chars(&info.model, width.saturating_sub(20)), stb(SILVER)));
            }
            5 => {
                content.push(sp("    ", st(DIM)));
                content.push(sp(
                    format!("{} · {} tool aktif", info.provider_name, info.tool_count),
                    st(AMBER_DIM),
                ));
            }
            _ => {}
        }
        out.push(framed(content, width));
    }

    let inner = width.saturating_sub(4);
    out.push(framed(
        vec![sp(format!(" {}", "─".repeat(inner)), st(FAINT))],
        width,
    ));
    out.push(framed(
        vec![
            sp(" Folder  ", st(AMBER_DIM)),
            sp(truncate_chars(&info.cwd, width.saturating_sub(12)), st(DIM)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp(" Sesi    ", st(AMBER_DIM)),
            sp(format!("{} · sandbox aktif", info.session_short), st(SILVER)),
        ],
        width,
    ));
    out.push(framed(vec![], width));
    out.push(framed(
        vec![sp(" Available Tools & Perintah", stb(GOLD))],
        width,
    ));
    out.push(framed(
        vec![
            sp(" tools: ", st(AMBER_DIM)),
            sp("file, search, outline, shell, git, web, plan, mcp", st(SILVER)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp(" ✦ /plan", stb(LAV)),
            sp(" rencana  ", st(DIM)),
            sp("✦ /model", stb(LAV)),
            sp(" ganti  ", st(DIM)),
            sp("✦ /help", stb(LAV)),
            sp(" bantuan", st(DIM)),
        ],
        width,
    ));
    out.push(border_line(width, '╰', '╯'));

    out.push(Line::from(fit(
        vec![
            sp("✦ Tip: ", stb(AMBER)),
            sp("ketik pesan atau /help untuk semua perintah.", st(AMBER_DIM)),
        ],
        width,
    )));
    out
}

/// Sambutan ringkas untuk sesi resume: 1 baris bergaya Moonlit × Hermes.
pub fn resume_lines(info: &WelcomeInfo) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        sp("☾ ", stb(GOLD)),
        sp("SelenCode", stb(GOLD_BRIGHT)),
        sp(
            format!(
                " — sesi dilanjutkan · {}/{}",
                info.provider_name, info.model
            ),
            st(DIM),
        ),
    ])]
}

// ---------- status bar (gaya Hermes) ----------

/// Baris status Moonlit × Hermes: model + fase bulan + bar konteks + token
/// sesi + langkah + cwd (+ "bekerja…" saat sibuk).
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
        sp(" ✦ ", stb(GOLD)),
        sp(truncate_chars(model, 24), stb(GOLD_BRIGHT)),
        sp(" │ ", st(BORDER)),
        sp(format!("{} ctx ", phase_glyph(pct)), stb(AMBER)),
        sp("[", st(BORDER)),
        sp(filled, st(GOLD)),
        sp(empty, st(FAINT)),
        sp("]", st(BORDER)),
        sp(format!(" {pct}%"), st(DIM)),
        sp(" │ ", st(BORDER)),
        sp(
            format!(
                "token sesi {} in / {} out",
                fmt_tokens(session_in),
                fmt_tokens(session_out)
            ),
            st(DIM),
        ),
        sp(" │ ", st(BORDER)),
        sp(format!("langkah {step_current}/{step_max}"), st(DIM)),
        sp(" │ ", st(BORDER)),
        sp(truncate_chars(cwd, 32), st(LAV)),
    ];
    if busy {
        spans.push(sp(" │ ", st(BORDER)));
        spans.push(sp("◐ bekerja…", stb(GOLD)));
    }
    Line::from(spans)
}

// ---------- banner mode polos ----------

/// Banner teks sederhana untuk mode polos (non-TUI): bulan braille kecil
/// berwarna emas (ANSI truecolor) + sapaan + baris provider/perintah.
pub fn plain_banner(provider_name: &str, model: &str) -> String {
    let mut out = String::new();
    for row in moon_art(10, 5) {
        out.push_str(&format!("\u{1b}[38;2;252;211;77m{row}\u{1b}[0m\n"));
    }
    out.push_str("SelenCode — halo, aku Selena. Ketik perintah, /help untuk bantuan, /exit untuk keluar.\n");
    out.push_str(&format!(
        "Perintah: /plan · /model [nama] · /sessions · /help · /exit — provider aktif: {provider_name} ({model})"
    ));
    out
}
