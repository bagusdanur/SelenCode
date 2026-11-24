//! Harness bukti visual Moonlit (v0.1.2): gambar frame TUI nyata ke
//! TestBackend lalu dump semua sel (simbol + warna + bold) ke file teks.
//! design/render_bukti.py mengubah dump ini menjadi PNG.
//!
//! Pakai: cargo run --example render_welcome -- <file-dump> <lebar> <tinggi>

use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use selencode::tui::app::App;

fn fg_of(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("{r},{g},{b}"),
        Color::Reset => "-".to_string(),
        Color::Black => "0,0,0".to_string(),
        Color::Red => "248,113,113".to_string(),
        Color::Green => "110,231,160".to_string(),
        Color::Yellow => "252,211,77".to_string(),
        Color::Blue => "96,165,250".to_string(),
        Color::Magenta => "196,181,253".to_string(),
        Color::Cyan => "167,139,250".to_string(),
        Color::Gray => "180,180,190".to_string(),
        Color::DarkGray => "148,140,180".to_string(),
        Color::White => "245,245,245".to_string(),
        _ => "-".to_string(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let w: u16 = args[2].parse().unwrap();
    let h: u16 = args[3].parse().unwrap();

    let mut app = App::new(
        "~/project-kamu".to_string(),
        "router".to_string(),
        "ag/gemini-3-flash".to_string(),
        25,
        0,
        0,
    );
    app.provider_label = "router (router.ryukomik.com)".to_string();
    app.tool_count = 12;
    app.context_pct = 43;
    app.session_in = 12_480;
    app.step_current = 3;

    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer().clone();

    let mut out = String::new();
    out.push_str(&format!("{w} {h}\n"));
    for y in 0..h {
        let mut first = true;
        for x in 0..w {
            let cell = &buf[(x, y)];
            if !first {
                out.push('\u{1e}');
            }
            first = false;
            let bold = if cell.modifier.contains(Modifier::BOLD) {
                1
            } else {
                0
            };
            out.push_str(&format!(
                "{}\u{1f}{}\u{1f}{}",
                cell.symbol(),
                fg_of(cell.fg),
                bold
            ));
        }
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
    println!("dump ditulis: {path} ({w}x{h})");
}
