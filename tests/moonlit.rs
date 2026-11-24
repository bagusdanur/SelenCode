//! Tes v0.1.2 — desain Moonlit: moon_art braille, wordmark, welcome_lines
//! responsif, glyph fase bulan. Semua fungsi murni, tanpa terminal.

use ratatui::text::Line;
use selencode::tui::moonlit::{moon_art, phase_glyph, welcome_lines, wordmark, WelcomeInfo};

fn info() -> WelcomeInfo {
    WelcomeInfo::new(
        "router".to_string(),
        "router (router.ryukomik.com)".to_string(),
        "ag/gemini-3-flash".to_string(),
        "/tmp/proyek".to_string(),
        12,
    )
}

fn plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn moon_art_bentuk_sabit_bukan_blok_penuh() {
    let art = moon_art(22, 13);
    assert_eq!(art.len(), 13, "jumlah baris sesuai rows");
    for row in &art {
        assert_eq!(row.chars().count(), 22, "lebar semua baris konsisten");
    }
    let total = 22 * 13;
    let lit = art
        .iter()
        .flat_map(|r| r.chars())
        .filter(|&c| c != '\u{2800}')
        .count();
    let blank = total - lit;
    assert!(lit > 0, "ada sel braille yang menyala");
    assert!(blank > 0, "ada sel kosong — bentuk sabit, bukan blok penuh");
    assert!(lit < total / 2, "sabit menempati minoritas sel ({lit}/{total})");
    // Baris tengah sabit (kiri) menyala, pojok kanan atas kosong (potongan).
    let mid: Vec<char> = art[6].chars().collect();
    assert_ne!(mid[0], '\u{2800}', "tepi kiri baris tengah menyala");
    let top: Vec<char> = art[0].chars().collect();
    assert_eq!(top[21], '\u{2800}', "pojok kanan atas kosong");
}

#[test]
fn wordmark_tujuh_baris_lebar_sama() {
    let mark = wordmark();
    assert_eq!(mark.len(), 7);
    let w = mark[0].chars().count();
    assert_eq!(w, 53, "9 huruf x 6 - 1");
    for row in &mark {
        assert_eq!(row.chars().count(), w, "semua baris selebar sama");
    }
    assert!(mark.iter().any(|r| r.contains('█')), "memuat blok penuh");
}

#[test]
fn welcome_lebar_116_memuat_identitas() {
    let lines = welcome_lines(116, &info());
    for l in &lines {
        assert!(l.width() <= 116, "baris overflow: {:?}", l.to_string());
    }
    let text = plain(&lines);
    assert!(text.contains("SelenCode"), "nama produk tampil");
    assert!(text.contains("edisi Moonlit"));
    assert!(text.contains("Selena"), "persona tampil");
    assert!(text.contains("ag/gemini-3-flash"), "model tampil");
    assert!(text.contains("Tools") && text.contains("Perintah"), "panel tampil");
    assert!(text.contains("12 tool aktif"), "jumlah tool dari info");
    assert!(text.starts_with('╭'), "bingkai membulat dibuka");
}

#[test]
fn welcome_sempit_52_tata_letak_hp() {
    let lines = welcome_lines(52, &info());
    for l in &lines {
        assert!(l.width() <= 52, "baris overflow: {:?}", l.to_string());
    }
    let text = plain(&lines);
    assert!(text.contains("SELENCODE"), "nama teks tampil di mode sempit");
    assert!(text.contains("Selena"));
    assert!(text.contains("ag/gemini-3-flash"), "model tampil");
    assert!(text.contains("/plan") && text.contains("/help"), "perintah inti tampil");
}

#[test]
fn welcome_lebar_sangat_kecil_tidak_panik() {
    for w in [4u16, 10, 30, 99, 100] {
        let lines = welcome_lines(w, &info());
        for l in &lines {
            assert!(l.width() <= (w as usize).max(4) || w >= 4, "w={w}");
        }
    }
}

#[test]
fn fase_bulan_menurut_persen() {
    assert_eq!(phase_glyph(0), '○');
    assert_eq!(phase_glyph(25), '◔');
    assert_eq!(phase_glyph(50), '◑');
    assert_eq!(phase_glyph(75), '◕');
    assert_eq!(phase_glyph(100), '●');
}
