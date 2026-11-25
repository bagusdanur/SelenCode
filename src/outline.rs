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

/// Informasi simbol terstruktur hasil ekstraksi Tree-sitter (Tahap 2: Smart Context Retrieval).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
}

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

/// Ekstrak daftar simbol terstruktur (nama, jenis, signature, rentang baris 1-based)
/// untuk kebutuhan Smart Context Retrieval & indeks repositori inkremental.
pub fn extract_symbols(source: &str, ext: &str) -> Result<Vec<SymbolInfo>> {
    let (language, lang) = lang_for(ext)?;
    if source.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| anyhow!("gagal memuat grammar .{ext}: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("gagal parse file .{ext}"))?;
    let mut out = Vec::new();
    collect_symbols(tree.root_node(), source, lang, &mut out);
    Ok(out)
}

fn collect_symbols(node: Node, src: &str, lang: Lang, out: &mut Vec<SymbolInfo>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(sym) = match_symbol(child, src, lang) {
            out.push(sym);
        }
        collect_symbols(child, src, lang, out);
    }
}

fn match_symbol(node: Node, src: &str, lang: Lang) -> Option<SymbolInfo> {
    let (kind, name, sig) = match lang {
        Lang::Rust => match node.kind() {
            "function_item" | "function_signature_item" => (
                "fn",
                field_text(node, "name", src)?,
                signature(node, src),
            ),
            "struct_item" => {
                let n = field_text(node, "name", src)?;
                ("struct", n.clone(), format!("struct {n}"))
            }
            "enum_item" => {
                let n = field_text(node, "name", src)?;
                ("enum", n.clone(), format!("enum {n}"))
            }
            "trait_item" => {
                let n = field_text(node, "name", src)?;
                ("trait", n.clone(), format!("trait {n}"))
            }
            "impl_item" => {
                let ty = field_text(node, "type", src)?;
                let sig = match node.child_by_field_name("trait") {
                    Some(t) => format!("impl {} for {ty}", text(t, src)),
                    None => format!("impl {ty}"),
                };
                ("impl", ty, sig)
            }
            "const_item" => {
                let n = field_text(node, "name", src)?;
                ("const", n.clone(), format!("const {n}"))
            }
            "static_item" => {
                let n = field_text(node, "name", src)?;
                ("static", n.clone(), format!("static {n}"))
            }
            "type_item" => {
                let n = field_text(node, "name", src)?;
                ("type", n.clone(), format!("type {n}"))
            }
            "mod_item" => {
                let n = field_text(node, "name", src)?;
                ("mod", n.clone(), format!("mod {n}"))
            }
            _ => return None,
        },
        Lang::Python => match node.kind() {
            "function_definition" => (
                "def",
                field_text(node, "name", src)?,
                signature(node, src),
            ),
            "class_definition" => (
                "class",
                field_text(node, "name", src)?,
                signature(node, src),
            ),
            _ => return None,
        },
        Lang::Js | Lang::Ts => {
            if lang == Lang::Ts {
                match node.kind() {
                    "interface_declaration" => {
                        let n = field_text(node, "name", src)?;
                        return Some(SymbolInfo {
                            name: n.clone(),
                            kind: "interface".to_string(),
                            signature: format!("interface {n}"),
                            start_line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                        });
                    }
                    "type_alias_declaration" => {
                        let n = field_text(node, "name", src)?;
                        return Some(SymbolInfo {
                            name: n.clone(),
                            kind: "type".to_string(),
                            signature: format!("type {n}"),
                            start_line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                        });
                    }
                    "enum_declaration" => {
                        let n = field_text(node, "name", src)?;
                        return Some(SymbolInfo {
                            name: n.clone(),
                            kind: "enum".to_string(),
                            signature: format!("enum {n}"),
                            start_line: node.start_position().row + 1,
                            end_line: node.end_position().row + 1,
                        });
                    }
                    _ => {}
                }
            }
            match node.kind() {
                "function_declaration" | "generator_function_declaration" => (
                    "function",
                    field_text(node, "name", src)?,
                    signature(node, src),
                ),
                "class_declaration" | "abstract_class_declaration" => (
                    "class",
                    field_text(node, "name", src)?,
                    signature(node, src),
                ),
                "method_definition" => (
                    "method",
                    field_text(node, "name", src)?,
                    signature(node, src),
                ),
                "variable_declarator" => {
                    let value = node.child_by_field_name("value")?;
                    if !matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "generator_function"
                    ) {
                        return None;
                    }
                    let name_node = node.child_by_field_name("name")?;
                    if name_node.kind() != "identifier" {
                        return None;
                    }
                    let n = text(name_node, src).to_string();
                    let label = js_label(node, src).unwrap_or_else(|| n.clone());
                    ("fn", n, label)
                }
                _ => return None,
            }
        }
    };
    Some(SymbolInfo {
        name,
        kind: kind.to_string(),
        signature: sig,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    })
}

/// Ekstrak daftar modul/file yang di-import untuk pelacakan ketergantungan antar-file.
/// Ringan & aman terhadap kode parsial (maks 64 entri unik per file).
pub fn extract_imports(source: &str, ext: &str) -> Vec<String> {
    let ext_low = ext.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |item: String| {
        let clean = item
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | ';' | ',' | '{' | '}' | '(' | ')'))
            .trim_start_matches("./")
            .trim_start_matches("../")
            .to_string();
        if clean.len() < 2 || out.len() >= 64 {
            return;
        }
        let low = clean.to_ascii_lowercase();
        if !out.iter().any(|x| x == &low) {
            out.push(low);
        }
    };

    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        match ext_low.as_str() {
            "rs" => {
                if let Some(rest) = line
                    .strip_prefix("use ")
                    .or_else(|| line.strip_prefix("pub use "))
                    .or_else(|| line.strip_prefix("pub(crate) use "))
                {
                    let head = rest.split(';').next().unwrap_or(rest).trim();
                    for seg in head.split("::") {
                        let clean = seg
                            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .next()
                            .unwrap_or("");
                        if !matches!(clean, "" | "crate" | "super" | "self" | "std" | "core" | "alloc") {
                            push_unique(clean.to_string());
                        }
                    }
                } else if let Some(rest) = line
                    .strip_prefix("mod ")
                    .or_else(|| line.strip_prefix("pub mod "))
                    .or_else(|| line.strip_prefix("pub(crate) mod "))
                {
                    if let Some(name) = rest
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .next()
                    {
                        push_unique(name.to_string());
                    }
                }
            }
            "py" => {
                if let Some(rest) = line.strip_prefix("from ") {
                    if let Some(module) = rest.split_whitespace().next() {
                        let m = module.trim_start_matches('.');
                        if !m.is_empty() {
                            push_unique(m.replace('.', "/"));
                            if let Some(last) = m.rsplit('.').next() {
                                push_unique(last.to_string());
                            }
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("import ") {
                    for part in rest.split(',') {
                        if let Some(module) = part.trim().split_whitespace().next() {
                            push_unique(module.replace('.', "/"));
                            if let Some(last) = module.rsplit('.').next() {
                                push_unique(last.to_string());
                            }
                        }
                    }
                }
            }
            "js" | "mjs" | "jsx" | "ts" | "tsx" => {
                // import ... from 'path' / export ... from "path"
                if (line.starts_with("import ") || line.starts_with("export "))
                    && line.contains(" from ")
                {
                    if let Some(after_from) = line.rsplit(" from ").next() {
                        if let Some(quoted) = extract_quoted(after_from) {
                            let stem = strip_js_ext(&quoted);
                            push_unique(stem.clone());
                            if let Some(last) = stem.rsplit('/').next() {
                                push_unique(last.to_string());
                            }
                        }
                    }
                } else if let Some(idx) = line.find("require(") {
                    if let Some(quoted) = extract_quoted(&line[idx + 8..]) {
                        let stem = strip_js_ext(&quoted);
                        push_unique(stem.clone());
                        if let Some(last) = stem.rsplit('/').next() {
                            push_unique(last.to_string());
                        }
                    }
                }
            }
            "go" => {
                if let Some(quoted) = extract_quoted(line) {
                    if let Some(last) = quoted.rsplit('/').next() {
                        push_unique(last.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_quoted(s: &str) -> Option<String> {
    let trimmed = s.trim();
    for q in ['\'', '"'] {
        if let Some(start) = trimmed.find(q) {
            let rest = &trimmed[start + 1..];
            if let Some(end) = rest.find(q) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn strip_js_ext(p: &str) -> String {
    let mut s = p.trim().trim_start_matches("./").to_string();
    while let Some(stripped) = s.strip_prefix("../") {
        s = stripped.to_string();
    }
    for ext in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(head) = s.strip_suffix(ext) {
            return head.to_string();
        }
    }
    s
}
