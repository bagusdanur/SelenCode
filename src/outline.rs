//! Fase 5 — outline struktur file kode via tree-sitter.
//!
//! Fungsi murni `outline_source(source, ext)`: parse sumber dengan grammar
//! sesuai ekstensi, lalu kumpulkan item deklarasi (fn/struct/class/def/...)
//! beserta rentang barisnya. Keluaran ringkas & stabil, jauh lebih hemat
//! token daripada isi file mentah. Tidak menyentuh filesystem — pemanggil
//! (tool `outline`, konteks pintar) yang mengurus sandbox & batas ukuran.

use anyhow::{anyhow, Result};
use tree_sitter::{Language, Node, Parser};

/// Ekstensi yang didukung outline (tanpa titik, huruf kecil).
pub const SUPPORTED_EXTS: &[&str] = &["rs", "py", "js", "mjs", "jsx", "ts", "tsx"];

const MAX_SIG_CHARS: usize = 100;

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Rust,
    Python,
    Js,
    Ts,
}

pub fn is_supported_ext(ext: &str) -> bool {
    SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

fn lang_for(ext: &str) -> Result<(Language, Lang)> {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Ok((tree_sitter_rust::LANGUAGE.into(), Lang::Rust)),
        "py" => Ok((tree_sitter_python::LANGUAGE.into(), Lang::Python)),
        "js" | "mjs" | "jsx" => Ok((tree_sitter_javascript::LANGUAGE.into(), Lang::Js)),
        "ts" => Ok((tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), Lang::Ts)),
        "tsx" => Ok((tree_sitter_typescript::LANGUAGE_TSX.into(), Lang::Ts)),
        other => Err(anyhow!(
            "ekstensi .{other} belum didukung outline, pakai read_file"
        )),
    }
}

struct Item {
    label: String,
    start: usize, // baris 1-based
    end: usize,
    children: Vec<Item>,
}

/// Ekstrak outline dari teks sumber. `ext` = ekstensi tanpa titik.
/// Sumber kosong -> Ok("") (bukan error). Parse tidak sempurna -> outline
/// parsial + baris catatan "(parse tidak sempurna)".
pub fn outline_source(source: &str, ext: &str) -> Result<String> {
    let (language, lang) = lang_for(ext)?;
    if source.trim().is_empty() {
        return Ok(String::new());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| anyhow!("gagal memuat grammar .{ext}: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("gagal parse file .{ext}"))?;
    let root = tree.root_node();
    let mut items = Vec::new();
    collect(root, source, lang, &mut items);
    let mut out = String::new();
    render(&items, 0, &mut out);
    if root.has_error() {
        if out.is_empty() {
            out.push_str("(parse tidak sempurna — tidak ada struktur yang dikenali)");
        } else {
            out.push_str("(parse tidak sempurna)\n");
        }
    }
    Ok(out.trim_end().to_string())
}

fn render(items: &[Item], depth: usize, out: &mut String) {
    for it in items {
        let range = if it.end > it.start {
            format!("baris {}-{}", it.start, it.end)
        } else {
            format!("baris {}", it.start)
        };
        out.push_str(&format!(
            "{}{} — {range}\n",
            "  ".repeat(depth),
            it.label
        ));
        render(&it.children, depth + 1, out);
    }
}

/// Jalan pohon: node yang cocok pola item bahasa -> jadi Item (anaknya dicari
/// di dalamnya); node lain diteruskan telusurannya ke daftar yang sama.
fn collect(node: Node, src: &str, lang: Lang, out: &mut Vec<Item>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(mut item) = match_item(child, src, lang) {
            collect(child, src, lang, &mut item.children);
            out.push(item);
        } else {
            collect(child, src, lang, out);
        }
    }
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn field_text(node: Node, field: &str, src: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| text(n, src).to_string())
}

/// Tanda tangan: teks node dari awal sampai awal body (field "body"),
/// whitespace diringkas 1 baris, ':' / ';' di ujung dibuang (Python / trait
/// Rust), dipotong maks MAX_SIG_CHARS karakter. Isi body TIDAK pernah ikut,
/// jadi outline tidak membocorkan isi kode / nilai literal.
fn signature(node: Node, src: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let raw = &src[node.start_byte()..end.min(src.len())];
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one_line.trim_end_matches([':', ';', ' ']).trim();
    let mut out: String = trimmed.chars().take(MAX_SIG_CHARS).collect();
    if trimmed.chars().count() > MAX_SIG_CHARS {
        out.push('…');
    }
    out
}

fn match_item(node: Node, src: &str, lang: Lang) -> Option<Item> {
    let label = match lang {
        Lang::Rust => rust_label(node, src),
        Lang::Python => python_label(node, src),
        Lang::Js => js_label(node, src),
        Lang::Ts => ts_label(node, src).or_else(|| js_label(node, src)),
    }?;
    Some(Item {
        label,
        start: node.start_position().row + 1,
        end: node.end_position().row + 1,
        children: Vec::new(),
    })
}

fn rust_label(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "function_item" | "function_signature_item" => Some(signature(node, src)),
        "struct_item" => Some(format!("struct {}", field_text(node, "name", src)?)),
        "enum_item" => Some(format!("enum {}", field_text(node, "name", src)?)),
        "trait_item" => Some(format!("trait {}", field_text(node, "name", src)?)),
        "impl_item" => {
            let ty = field_text(node, "type", src)?;
            match node.child_by_field_name("trait") {
                Some(t) => Some(format!("impl {} for {}", text(t, src), ty)),
                None => Some(format!("impl {ty}")),
            }
        }
        "const_item" => Some(format!("const {}", field_text(node, "name", src)?)),
        "static_item" => Some(format!("static {}", field_text(node, "name", src)?)),
        "type_item" => Some(format!("type {}", field_text(node, "name", src)?)),
        "mod_item" => Some(format!("mod {}", field_text(node, "name", src)?)),
        _ => None,
    }
}

fn python_label(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        // signature() menghasilkan "def nama(args)" / "async def nama(args)"
        // / "class Nama(Base)" — ':' penutup dibuang di signature().
        "function_definition" | "class_definition" => Some(signature(node, src)),
        _ => None,
    }
}

fn js_label(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => Some(signature(node, src)),
        "class_declaration" | "abstract_class_declaration" => Some(signature(node, src)),
        "method_definition" => Some(signature(node, src)),
        "variable_declarator" => {
            // Hanya deklarasi yang diisi fungsi: const f = (a) => ... / = function(a) ...
            let value = node.child_by_field_name("value")?;
            if !matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            ) {
                return None;
            }
            let name_node = node.child_by_field_name("name")?;
            if name_node.kind() != "identifier" {
                return None; // pola destrukturisasi — lewati
            }
            let kw = node
                .parent()
                .and_then(|p| {
                    if p.kind() == "lexical_declaration" || p.kind() == "variable_declaration" {
                        text(p, src).split_whitespace().next().map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "var".to_string());
            let params = value
                .child_by_field_name("parameters")
                .map(|p| text(p, src).to_string())
                .unwrap_or_else(|| "()".to_string());
            if value.kind() == "arrow_function" {
                Some(format!("{kw} {} = {params} =>", text(name_node, src)))
            } else {
                Some(format!("{kw} {} = function {params}", text(name_node, src)))
            }
        }
        _ => None,
    }
}

fn ts_label(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "interface_declaration" => Some(format!(
            "interface {}",
            field_text(node, "name", src)?
        )),
        "type_alias_declaration" => {
            Some(format!("type {}", field_text(node, "name", src)?))
        }
        "enum_declaration" => Some(format!("enum {}", field_text(node, "name", src)?)),
        _ => None,
    }
}
