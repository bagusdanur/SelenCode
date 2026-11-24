//! Desain "Moonlit" untuk TUI SelenCode (v0.1.2) — fungsi murni, gampang
//! dites. Palet & tata letak mengikuti mockup yang disetujui user:
//! design/mockup-moonlit-wide.png & design/mockup-moonlit-hp.png
//! (sumber: design/moonlit_mockup.py).
//!
//! Identitas: bulan sabit titik-titik (braille) ungu + bintang emas kecil,
//! wordmark balok "SELENCODE" dua nada ungu, panel Tools/Perintah ala
//! Hermes versi kita, status bar dengan fase bulan sebagai penanda konteks.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ---------- palet (RGB persis mockup) ----------

pub const BG: Color = Color::Rgb(13, 10, 26);
pub const VIOLET: Color = Color::Rgb(167, 139, 250);
pub const DEEP: Color = Color::Rgb(109, 40, 217);
pub const LAV: Color = Color::Rgb(196, 181, 253);
pub const SILVER: Color = Color::Rgb(229, 231, 235);
pub const DIM: Color = Color::Rgb(148, 140, 180);
pub const FAINT: Color = Color::Rgb(94, 84, 130);
pub const GOLD: Color = Color::Rgb(252, 211, 77);

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

/// Bulan sabit dari karakter braille, mengikuti logika mockup: titik
/// menyala bila berada di dalam lingkaran besar DAN di luar lingkaran
/// geser (potongan sabit). Sel kosong memakai U+2800 (braille blank) agar
/// lebar baris konsisten di semua terminal. Semua baris punya jumlah
/// karakter persis `cols`.
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

// ---------- wordmark balok 5x7 ----------

/// Font piksel 5x7 persis tabel PIX di design/moonlit_mockup.py.
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

/// Wordmark "SELENCODE" balok: 7 baris, semua baris lebar karakter sama
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

/// Bar konteks 10 sel: "▓" terisi (DEEP) / "░" kosong (FAINT) — dikembalikan
/// sebagai (terisi, kosong) agar bisa diwarnai terpisah.
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
}

impl WelcomeInfo {
    pub fn new(provider_name: String, provider_label: String, model: String, cwd: String, tool_count: usize) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            provider_name,
            provider_label,
            model,
            cwd,
            tool_count,
        }
    }
}

// ---------- perakitan baris berbingkai ----------

/// Lebar tampil teks: semua glyph yang kami hasilkan selebar 1 sel;
/// teks dari config (cwd/model) diasumsikan sama — dihitung per char.
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
    let mut line: Vec<Span<'static>> = vec![Span::styled("│", st(FAINT))];
    line.append(&mut spans);
    line.push(Span::styled("│", st(FAINT)));
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
    Line::from(Span::styled(s, st(FAINT)))
}

fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

// ---------- layar sambutan ----------

/// Baris-baris sambutan Moonlit selebar `width`. Tata letak lebar bila
/// width >= 100, tata letak HP (menumpuk) bila lebih sempit. Semua baris
/// dijamin <= width.
pub fn welcome_lines(width: u16, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let width = (width as usize).max(4);
    if width >= 100 {
        welcome_wide(width, info)
    } else {
        welcome_narrow(width, info)
    }
}

fn welcome_wide(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let inner = width - 2;
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(border_line(width, '╭', '╮'));

    // Pojok kanan atas: "SelenCode vX · edisi Moonlit"
    let right = vec![
        sp(format!("SelenCode v{} ", info.version), stb(VIOLET)),
        sp("· edisi Moonlit", st(DIM)),
    ];
    let rw = spans_w(&right);
    let mut head = vec![Span::raw(" ".repeat(inner.saturating_sub(rw)))];
    head.extend(right);
    out.push(framed(head, width));

    // Blok utama: bulan (kiri, zona 30) + wordmark & info (kanan).
    let moon = moon_art(22, 13);
    let mark = wordmark();
    let mut right_rows: Vec<Vec<Span<'static>>> = Vec::new();
    right_rows.push(vec![]);
    for (i, mrow) in mark.iter().enumerate() {
        let color = if (3..=4).contains(&i) { LAV } else { VIOLET };
        right_rows.push(vec![sp(mrow.clone(), stb(color))]);
    }
    right_rows.push(vec![]);
    right_rows.push(vec![sp(
        "  terminal AI coding agent · full Rust · Termux & Linux",
        st(DIM),
    )]);
    right_rows.push(vec![]);
    right_rows.push(vec![
        sp("  Selena", stb(LAV)),
        sp("  menemanimu malam ini — sesi baru, konteks bersih.", st(SILVER)),
    ]);
    right_rows.push(vec![
        sp("  Provider  ", st(DIM)),
        sp(info.provider_label.clone(), st(SILVER)),
        sp("   Model  ", st(DIM)),
        sp(info.model.clone(), stb(LAV)),
    ]);
    for i in 0..13 {
        let mut content: Vec<Span<'static>> = Vec::new();
        content.push(Span::raw("  "));
        content.push(sp(moon[i].clone(), st(VIOLET)));
        if i == 1 {
            // Bintang emas kecil di celah kanan-atas bulan (sesuai mockup).
            content.push(Span::raw("    "));
            content.push(sp("✦", st(GOLD)));
            content.push(Span::raw(" "));
        }
        let cur = spans_w(&content);
        if cur < 32 {
            content.push(Span::raw(" ".repeat(32 - cur)));
        }
        content.extend(right_rows[i].iter().cloned());
        out.push(framed(content, width));
    }

    out.push(framed(vec![], width));

    // Panel dua kolom: kanan mulai di kolom konten 58.
    const RIGHT_AT: usize = 58;
    let panel_row = |left: Vec<Span<'static>>, right: Vec<Span<'static>>| -> Line<'static> {
        let mut content = left;
        let cur = spans_w(&content);
        if cur < RIGHT_AT {
            content.push(Span::raw(" ".repeat(RIGHT_AT - cur)));
        } else {
            content.push(Span::raw(" "));
        }
        content.extend(right);
        framed(content, width)
    };
    out.push(panel_row(
        vec![sp("  ✦ Tools", stb(GOLD))],
        vec![sp("✦ Perintah", stb(GOLD))],
    ));
    let tools: [(&str, &str); 7] = [
        ("file    ", "list_files, read_file, write_file, edit_file"),
        ("cari    ", "search_text, outline (struktur kode)"),
        ("shell   ", "run_shell (izin dulu)"),
        ("git     ", "git_status, git_diff, git_commit"),
        ("web     ", "fetch_url"),
        ("rencana ", "update_plan, delegate (subagent paralel)"),
        ("mcp     ", "mcp__* dari server eksternalmu"),
    ];
    let cmds: [(&str, &str); 7] = [
        ("/plan     ", "rencana dulu, eksekusi sesudah disetujui"),
        ("/model    ", "lihat & ganti provider/model"),
        ("/sessions ", "daftar sesi tersimpan"),
        ("/resume   ", "lanjutkan sesi terakhir"),
        ("/help     ", "semua perintah"),
        ("", ""),
        ("", ""),
    ];
    for (idx, ((tk, tv), (ck, cv))) in tools.iter().zip(cmds.iter()).enumerate() {
        let left = vec![sp(format!("  {tk}"), stb(DEEP)), sp(tv.to_string(), st(SILVER))];
        let right = if idx == 6 {
            vec![sp(
                format!("{} tool aktif · sandbox: folder ini saja", info.tool_count),
                st(SILVER),
            )]
        } else if ck.is_empty() {
            vec![]
        } else {
            vec![sp(ck.to_string(), stb(DEEP)), sp(cv.to_string(), st(SILVER))]
        };
        out.push(panel_row(left, right));
    }

    out.push(framed(vec![], width));
    out.push(framed(
        vec![sp(
            "  Selamat datang kembali. Ketik pesanmu, atau /help untuk perintah.",
            st(SILVER),
        )],
        width,
    ));
    out.push(border_line(width, '╰', '╯'));
    out
}

fn welcome_narrow(width: usize, info: &WelcomeInfo) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(border_line(width, '╭', '╮'));
    let moon = moon_art(14, 8);
    for (i, mrow) in moon.iter().enumerate() {
        let mut content: Vec<Span<'static>> = vec![Span::raw("   "), sp(mrow.clone(), st(VIOLET))];
        if i == 3 {
            content.push(Span::raw("   "));
            content.push(sp("SELENCODE", stb(LAV)));
        } else if i == 4 {
            content.push(Span::raw("   "));
            content.push(sp(format!("v{} · Moonlit", info.version), st(DIM)));
        }
        out.push(framed(content, width));
    }
    out.push(framed(vec![], width));
    out.push(framed(
        vec![
            sp("  Selena", stb(LAV)),
            sp(" menemanimu malam ini.", st(SILVER)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp("  Provider ", st(DIM)),
            sp(info.provider_name.clone(), st(SILVER)),
            sp("  ·  ", st(DIM)),
            sp(info.model.clone(), stb(LAV)),
        ],
        width,
    ));
    out.push(framed(
        vec![sp("  Folder   ", st(DIM)), sp(info.cwd.clone(), st(SILVER))],
        width,
    ));
    out.push(framed(
        vec![
            sp("  Sesi     ", st(DIM)),
            sp(
                format!("baru · {} tool aktif · sandbox aktif", info.tool_count),
                st(SILVER),
            ),
        ],
        width,
    ));
    out.push(framed(vec![], width));
    out.push(framed(
        vec![
            sp("  ✦ /plan", stb(GOLD)),
            sp(" rencana dulu   ", st(SILVER)),
            sp("✦ /model", stb(GOLD)),
            sp(" ganti model", st(SILVER)),
        ],
        width,
    ));
    out.push(framed(
        vec![
            sp("  ✦ /help", stb(GOLD)),
            sp(" semua perintah", st(SILVER)),
        ],
        width,
    ));
    out.push(border_line(width, '╰', '╯'));
    out
}

/// Sambutan ringkas untuk sesi resume: 1 baris bergaya Moonlit.
pub fn resume_lines(info: &WelcomeInfo) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        sp("☾ ", stb(VIOLET)),
        sp("SelenCode", stb(LAV)),
        sp(
            format!(
                " — sesi dilanjutkan · {}/{}",
                info.provider_name, info.model
            ),
            st(DIM),
        ),
    ])]
}

// ---------- status bar ----------

/// Baris status Moonlit: fase bulan + bar konteks + token sesi + langkah
/// + cwd (+ "bekerja…" saat sibuk).
pub fn status_line(
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
        sp(format!(" {} konteks ", phase_glyph(pct)), stb(VIOLET)),
        sp(filled, st(DEEP)),
        sp(empty, st(FAINT)),
        sp(format!(" {pct}%  ·  ", ), st(DIM)),
        sp(
            format!(
                "token sesi {} in / {} out  ·  ",
                fmt_tokens(session_in),
                fmt_tokens(session_out)
            ),
            st(DIM),
        ),
        sp(format!("langkah {step_current}/{step_max}  ·  "), st(DIM)),
        sp(cwd.to_string(), st(LAV)),
    ];
    if busy {
        spans.push(sp("  ·  bekerja…", st(GOLD)));
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
    out.push_str("SelenCode — halo, aku Selena. Ketik perintah, /exit untuk keluar.\n");
    out.push_str(&format!(
        "Perintah: /plan · /model [nama] · /sessions · /exit — provider aktif: {provider_name} ({model})"
    ));
    out
}
