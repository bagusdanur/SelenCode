use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

pub const MAX_READ_LINES: usize = 2000;
pub const MAX_READ_BYTES: usize = 100 * 1024;
pub const MAX_READ_IMAGE_BYTES: u64 = 6 * 1024 * 1024;
pub const MAX_READ_PDF_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_LIST: usize = 500;
/// Batas karakter per baris sebelum dipotong rapi (desain internal SelenCode).
pub const DEFAULT_MAX_LINE_LENGTH: usize = 2000;
/// Batas baris bawaan untuk `read_file` tanpa rentang eksplisit `start`/`end` (pola terstruktur `file_tools.py`).
pub const DEFAULT_FILE_READ_LINES: usize = 250;
/// Batas karakter praktis bawaan per panggilan `read_file` (desain internal SelenCode).
pub const DEFAULT_FILE_READ_MAX_CHARS: usize = 16_000;

/// Potong daftar baris `read_file` pada BARIS UTUH terakhir yang muat di dalam `max_chars`
/// (desain internal SelenCode).
/// Mengembalikan `(teks_terjaga, jumlah_baris_terjaga, terpotong)`.
/// Jika baris pertama sendiri melebihi `max_chars`, baris pertama dipotong aman UTF-8 agar kursor tetap maju.
pub fn truncate_lines_to_char_budget(lines: &[String], max_chars: usize) -> (String, usize, bool) {
    if lines.is_empty() || max_chars == 0 {
        return (String::new(), 0, false);
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut running = 0usize;
    for line in lines {
        let line_chars = line.chars().count();
        let sep = usize::from(!kept.is_empty());
        if running + sep + line_chars > max_chars {
            if kept.is_empty() {
                let clamped: String = line.chars().take(max_chars).collect();
                return (clamped, 1, true);
            }
            return (kept.join("\n"), kept.len(), true);
        }
        kept.push(line.as_str());
        running += sep + line_chars;
    }
    (kept.join("\n"), kept.len(), false)
}

fn clamp_line_length(line: &str, max_line_chars: usize) -> String {
    if max_line_chars == 0 || line.chars().count() <= max_line_chars {
        return line.to_string();
    }
    let head: String = line.chars().take(max_line_chars).collect();
    format!("{head} ... [baris dipotong]")
}

fn path_arg(args: &Value) -> String {
    str_arg(args, "path").unwrap_or(".").to_string()
}

fn unified_diff(old: &str, new: &str, label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("diff {label}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(&format!("{sign}{}", change.value()));
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    if out.len() > 6000 {
        let mut end = 6000;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n... (diff dipotong)\n");
    }
    out
}

// ---------- list_files ----------
pub struct ListFiles;

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".into(),
            description: "Daftar file workspace (maks 500; opsional filter glob mis. '*.rs').".into(),
            parameters: schema(
                json!({"path": {"type": "string"}, "glob": {"type": "string"}}),
                &[],
            ),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let cwd_norm = crate::security::normalize_sandbox_path(&ctx.cwd);
        let root = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(&path_arg(&args)))?;
        let glob_filter = super::search::compile_path_glob(str_arg(&args, "glob"))?;
        let mut out = Vec::new();
        for e in ignore::WalkBuilder::new(&root).require_git(false).build().flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let ep = crate::security::strip_windows_verbatim(e.path());
                let rel = ep
                    .strip_prefix(&cwd_norm)
                    .or_else(|_| ep.strip_prefix(&ctx.cwd))
                    .unwrap_or(ep.as_path());
                if !super::search::path_matches_glob(rel, &glob_filter) {
                    continue;
                }
                out.push(rel.display().to_string());
                if out.len() >= MAX_LIST {
                    break;
                }
            }
        }
        out.sort();
        if out.is_empty() {
            return Ok("(kosong)".into());
        }
        Ok(out.join("\n"))
    }
}

// ---------- Multimodal Helpers untuk read_file (Gambar + PDF, v0.5.28) ----------

/// Ekstrak dimensi piksel `(width, height)` dari header biner PNG, GIF, JPEG, atau WebP tanpa dependensi tambahan.
pub fn extract_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature + IHDR length(4) + "IHDR"(4) + width(4 BE) + height(4 BE)
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        return Some((w, h));
    }
    // GIF: "GIF87a" / "GIF89a" + width(2 LE) + height(2 LE)
    if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
        let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
        return Some((w, h));
    }
    // JPEG: pindai marker SOF0..SOF3 (0xFFC0..0xFFC3)
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            while i < bytes.len() && bytes[i] == 0xFF {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let marker = bytes[i];
            i += 1;
            if matches!(marker, 0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7) {
                continue;
            }
            if i + 2 > bytes.len() {
                break;
            }
            let seg_len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
            if seg_len < 2 || i + seg_len > bytes.len() {
                break;
            }
            if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3) && seg_len >= 7 {
                let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                return Some((w, h));
            }
            i += seg_len;
        }
    }
    // WebP: RIFF....WEBP + VP8X / VP8L / VP8
    if bytes.len() >= 30 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if &bytes[12..16] == b"VP8X" {
            let w = 1 + (bytes[24] as u32 | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16));
            let h = 1 + (bytes[27] as u32 | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16));
            return Some((w, h));
        }
        if &bytes[12..16] == b"VP8 " && bytes.len() >= 30 && &bytes[23..26] == b"\x9d\x01\x2a" {
            let w = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF) as u32;
            let h = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF) as u32;
            return Some((w, h));
        }
        if &bytes[12..16] == b"VP8L" && bytes.len() >= 25 && bytes[20] == 0x2F {
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            return Some((w, h));
        }
    }
    None
}

/// Muat blok `Block::Image` untuk file gambar hasil `read_file` agar agent loop dapat melampirkannya
/// langsung ke pesan user untuk provider vision (OpenAI / Anthropic / Gemini).
pub fn load_read_file_image_block(
    cwd: &std::path::Path,
    rel: &str,
) -> Option<(String, crate::providers::Block)> {
    let p = resolve_in_sandbox(cwd, std::path::Path::new(rel)).ok()?;
    if is_secret_file(&p) || !p.is_file() {
        return None;
    }
    let mime = crate::providers::detect_image_mime(&p)?;
    let meta = std::fs::metadata(&p).ok()?;
    if meta.len() == 0 || meta.len() > MAX_READ_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&p).ok()?;
    Some((
        rel.to_string(),
        crate::providers::Block::Image {
            media_type: mime.to_string(),
            data_base64: crate::providers::encode_base64(&bytes),
        },
    ))
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|pos| from + pos)
}

fn decode_pdf_literal_string(src: &[u8], start_after_paren: usize) -> (String, usize) {
    let mut out: Vec<u8> = Vec::new();
    let mut i = start_after_paren;
    let mut depth = 1usize;
    while i < src.len() && depth > 0 {
        let b = src[i];
        if b == b'\\' && i + 1 < src.len() {
            i += 1;
            match src[i] {
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0C),
                b'(' => out.push(b'('),
                b')' => out.push(b')'),
                b'\\' => out.push(b'\\'),
                b'\r' => {
                    if i + 1 < src.len() && src[i + 1] == b'\n' {
                        i += 1;
                    }
                }
                b'\n' => {}
                c @ b'0'..=b'7' => {
                    let mut val = (c - b'0') as u32;
                    for _ in 0..2 {
                        if i + 1 < src.len() && matches!(src[i + 1], b'0'..=b'7') {
                            i += 1;
                            val = (val << 3) | ((src[i] - b'0') as u32);
                        } else {
                            break;
                        }
                    }
                    out.push((val & 0xFF) as u8);
                }
                other => out.push(other),
            }
            i += 1;
        } else if b == b'(' {
            depth += 1;
            out.push(b);
            i += 1;
        } else if b == b')' {
            depth -= 1;
            if depth > 0 {
                out.push(b);
            }
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    let text = decode_pdf_bytes_to_string(&out);
    (text, i)
}

fn decode_pdf_hex_string(src: &[u8], start_after_lt: usize) -> (String, usize) {
    let mut hex_digits: Vec<u8> = Vec::new();
    let mut i = start_after_lt;
    while i < src.len() && src[i] != b'>' {
        if src[i].is_ascii_hexdigit() {
            hex_digits.push(src[i]);
        }
        i += 1;
    }
    if i < src.len() && src[i] == b'>' {
        i += 1;
    }
    if hex_digits.len() % 2 == 1 {
        hex_digits.push(b'0');
    }
    let mut raw: Vec<u8> = Vec::with_capacity(hex_digits.len() / 2);
    for pair in hex_digits.chunks_exact(2) {
        if let Ok(s) = std::str::from_utf8(pair) {
            if let Ok(v) = u8::from_str_radix(s, 16) {
                raw.push(v);
            }
        }
    }
    (decode_pdf_bytes_to_string(&raw), i)
}

fn decode_pdf_bytes_to_string(raw: &[u8]) -> String {
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Ok(utf8) = std::str::from_utf8(raw) {
        return utf8.to_string();
    }
    raw.iter().map(|&b| b as char).collect()
}

fn extract_text_from_pdf_content_stream(data: &[u8], out: &mut String) {
    let mut i = 0usize;
    let mut in_text = false;
    let mut pending_strings: Vec<String> = Vec::new();
    let mut cur_line = String::new();

    let flush_line = |cur: &mut String, dst: &mut String| {
        let trimmed = cur.trim();
        if !trimmed.is_empty() {
            if !dst.is_empty() && !dst.ends_with('\n') {
                dst.push('\n');
            }
            dst.push_str(trimmed);
        }
        cur.clear();
    };

    while i < data.len() {
        let b = data[i];
        if b == b'%' {
            while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'(' {
            let (s, next_i) = decode_pdf_literal_string(data, i + 1);
            if in_text {
                pending_strings.push(s);
            }
            i = next_i;
            continue;
        }
        if b == b'<' {
            if i + 1 < data.len() && data[i + 1] == b'<' {
                i += 2;
                continue;
            }
            let (s, next_i) = decode_pdf_hex_string(data, i + 1);
            if in_text {
                pending_strings.push(s);
            }
            i = next_i;
            continue;
        }
        if b == b'[' {
            // Array TJ: kumpulkan string dan tambahkan spasi bila offset negatif besar (<= -180).
            i += 1;
            let mut arr_text = String::new();
            while i < data.len() && data[i] != b']' {
                if data[i] == b'(' {
                    let (s, ni) = decode_pdf_literal_string(data, i + 1);
                    arr_text.push_str(&s);
                    i = ni;
                } else if data[i] == b'<' && !(i + 1 < data.len() && data[i + 1] == b'<') {
                    let (s, ni) = decode_pdf_hex_string(data, i + 1);
                    arr_text.push_str(&s);
                    i = ni;
                } else if data[i] == b'-' || data[i].is_ascii_digit() {
                    let start = i;
                    i += 1;
                    while i < data.len() && (data[i].is_ascii_digit() || data[i] == b'.') {
                        i += 1;
                    }
                    if let Ok(num_str) = std::str::from_utf8(&data[start..i]) {
                        if let Ok(val) = num_str.parse::<f64>() {
                            if val <= -180.0 && !arr_text.ends_with(' ') && !arr_text.is_empty() {
                                arr_text.push(' ');
                            }
                        }
                    }
                } else {
                    i += 1;
                }
            }
            if i < data.len() && data[i] == b']' {
                i += 1;
            }
            if in_text && !arr_text.is_empty() {
                pending_strings.push(arr_text);
            }
            continue;
        }
        // Baca token operator / keyword PDF.
        let start = i;
        while i < data.len()
            && !data[i].is_ascii_whitespace()
            && !matches!(data[i], b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'%' | b'/')
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let tok = &data[start..i];
        match tok {
            b"BT" => {
                in_text = true;
                pending_strings.clear();
            }
            b"ET" => {
                flush_line(&mut cur_line, out);
                in_text = false;
                pending_strings.clear();
            }
            b"Tj" | b"TJ" => {
                if in_text {
                    for s in pending_strings.drain(..) {
                        cur_line.push_str(&s);
                    }
                }
            }
            b"'" | b"\"" => {
                if in_text {
                    flush_line(&mut cur_line, out);
                    for s in pending_strings.drain(..) {
                        cur_line.push_str(&s);
                    }
                }
            }
            b"T*" | b"Td" | b"TD" => {
                if in_text && !cur_line.is_empty() {
                    flush_line(&mut cur_line, out);
                }
            }
            _ => {}
        }
    }
    flush_line(&mut cur_line, out);
}

/// Ekstraktor teks PDF murni Rust (mendukung stream tak terkompresi maupun `/FlateDecode` via `flate2`
/// dengan batas aman 4 MB/stream anti zip-bomb, v0.5.28).
pub fn extract_pdf_text_bytes(bytes: &[u8]) -> Result<String> {
    use std::io::Read;
    if !bytes.starts_with(b"%PDF") {
        return Err(anyhow!("berkas bukan PDF valid (header %PDF tidak ditemukan)"));
    }
    const MAX_STREAM_DECOMP_BYTES: u64 = 4 * 1024 * 1024;
    const MAX_TOTAL_DECOMP_BYTES: usize = 16 * 1024 * 1024;
    const MAX_EXTRACTED_TEXT_BYTES: usize = 1024 * 1024;
    let mut extracted = String::new();
    let mut pos = 0usize;
    let mut stream_count = 0usize;
    let mut total_decomp = 0usize;

    while let Some(st_idx) = find_subslice(bytes, b"stream", pos) {
        if stream_count >= 512
            || total_decomp >= MAX_TOTAL_DECOMP_BYTES
            || extracted.len() >= MAX_EXTRACTED_TEXT_BYTES
        {
            break;
        }
        // Cek header kamus tepat sebelum `stream` (maks 512 byte ke belakang).
        let dict_start = st_idx.saturating_sub(512);
        let dict_slice = &bytes[dict_start..st_idx];
        let mut data_start = st_idx + b"stream".len();
        if data_start < bytes.len() && bytes[data_start] == b'\r' {
            data_start += 1;
        }
        if data_start < bytes.len() && bytes[data_start] == b'\n' {
            data_start += 1;
        } else {
            // Bukan kata kunci stream yang diikuti baris baru (mis. kata "stream" di dalam teks).
            pos = st_idx + 6;
            continue;
        }
        let Some(end_idx) = find_subslice(bytes, b"endstream", data_start) else {
            break;
        };
        let raw_stream = &bytes[data_start..end_idx];
        pos = end_idx + b"endstream".len();
        stream_count += 1;

        let is_flate = find_subslice(dict_slice, b"FlateDecode", 0).is_some()
            || (raw_stream.len() >= 2 && raw_stream[0] == 0x78 && matches!(raw_stream[1], 0x01 | 0x5E | 0x9C | 0xDA));

        let remaining_budget = (MAX_TOTAL_DECOMP_BYTES - total_decomp) as u64;
        let stream_cap = MAX_STREAM_DECOMP_BYTES.min(remaining_budget);
        let decoded: Vec<u8> = if is_flate {
            let mut dec = flate2::read::ZlibDecoder::new(raw_stream).take(stream_cap);
            let mut buf = Vec::new();
            if dec.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                buf
            } else {
                raw_stream[..raw_stream.len().min(stream_cap as usize)].to_vec()
            }
        } else {
            raw_stream[..raw_stream.len().min(stream_cap as usize)].to_vec()
        };
        total_decomp = total_decomp.saturating_add(decoded.len());

        if find_subslice(&decoded, b"BT", 0).is_some() {
            let before_len = extracted.len();
            extract_text_from_pdf_content_stream(&decoded, &mut extracted);
            if extracted.len() > before_len && !extracted.ends_with("\n\n") {
                extracted.push('\n');
            }
        }
    }

    Ok(extracted.trim().to_string())
}

async fn try_pdftotext_fallback(path: &std::path::Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new("pdftotext");
    cmd.arg("-enc")
        .arg("UTF-8")
        .arg(path)
        .arg("-")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::sandbox::scrub_tokio_command(&mut cmd);
    let (code, out) = super::run_timed(&mut cmd, 10).await.ok()?;
    if code == Some(0) && !out.trim().is_empty() {
        Some(out.trim().to_string())
    } else {
        None
    }
}

fn format_sliced_text(
    rel: &str,
    text: &str,
    args: &Value,
    ctx: &Ctx,
) -> Result<String> {
    let has_range = args.get("start").is_some() || args.get("end").is_some();
    let lines: Vec<&str> = text.lines().collect();
    let start = args.get("start").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let raw_end = args.get("end").and_then(|v| v.as_u64()).map(|e| e as usize);
    if let Some(e) = raw_end {
        if e < start && !lines.is_empty() {
            return Err(anyhow!("rentang baris tidak valid: end={e} < start={start}"));
        }
    }
    let end = raw_end.unwrap_or(lines.len()).min(lines.len());
    if start > lines.len() && !lines.is_empty() {
        return Err(anyhow!("start={start} melebihi jumlah baris {}", lines.len()));
    }
    let start_idx = (start - 1).min(lines.len());
    let end_idx = end.max(start_idx);
    let slice = &lines[start_idx..end_idx];
    let default_line_cap = if ctx.tool_budget.file_read_default_lines > 0 {
        ctx.tool_budget.file_read_default_lines.min(MAX_READ_LINES)
    } else {
        MAX_READ_LINES
    };
    let line_cap = if has_range {
        MAX_READ_LINES
    } else {
        default_line_cap
    };
    let take = slice.len().min(line_cap);
    let truncated_by_lines = take < slice.len();
    let clamped_lines: Vec<String> = slice[..take]
        .iter()
        .map(|l| clamp_line_length(l, DEFAULT_MAX_LINE_LENGTH))
        .collect();
    let char_budget = if ctx.tool_budget.file_read_max_chars > 0 {
        ctx.tool_budget.file_read_max_chars
    } else {
        MAX_READ_BYTES
    };
    let (mut out, kept_lines, truncated_by_chars) =
        truncate_lines_to_char_budget(&clamped_lines, char_budget);
    if truncated_by_lines || truncated_by_chars {
        let shown = kept_lines.max(1);
        let last_shown = (start + shown - 1).min(lines.len());
        let next_start = last_shown + 1;
        let next_end = (next_start + default_line_cap.saturating_sub(1)).min(lines.len());
        let reason = if truncated_by_chars {
            format!("pagu {char_budget} char")
        } else {
            format!("pagu {line_cap} baris")
        };
        if next_start <= lines.len() {
            out.push_str(&format!(
                "\n... [dipotong di {reason}: menampilkan baris {start}-{last_shown} dari {}. \
                 Gunakan read_file(path=\"{rel}\", start={next_start}, end={next_end}) untuk melanjutkan, \
                 atau search_text untuk mencari simbol spesifik.]",
                lines.len()
            ));
        } else {
            out.push_str(&format!(
                "\n... [dipotong di {reason}: menampilkan baris {start}-{last_shown} dari {}.]",
                lines.len()
            ));
        }
    } else if out.len() > MAX_READ_BYTES {
        let mut cut = MAX_READ_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n... (hasil dipotong 100KB; persempit start/end)");
    }
    Ok(out)
}

// ---------- read_file ----------
pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Baca file teks (opsional baris start..end, 1-based), gambar (PNG/JPG/WebP/GIF), atau PDF.".into(),
            parameters: schema(
                json!({"path": {"type": "string"},
                       "start": {"type": "integer"},
                       "end": {"type": "integer"}}),
                &["path"],
            ),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rel = str_arg(&args, "path").ok_or_else(|| anyhow!("argumen 'path' wajib"))?;
        let p = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(rel))?;
        if is_secret_file(&p) {
            return Err(anyhow!("file '{rel}' ditolak: termasuk file rahasia (.env/*.key/*.pem/id_rsa)"));
        }
        let meta = std::fs::metadata(&p).map_err(|e| anyhow!("gagal membaca {rel}: {e}"))?;

        // 1. Jalur multimodal GAMBAR (PNG/JPEG/WebP/GIF, v0.5.28).
        if let Some(mime) = crate::providers::detect_image_mime(&p) {
            if meta.len() == 0 {
                return Err(anyhow!("file gambar {rel} kosong (0 byte)"));
            }
            if meta.len() > MAX_READ_IMAGE_BYTES {
                return Err(anyhow!(
                    "file gambar {rel} terlalu besar ({} byte > {MAX_READ_IMAGE_BYTES})",
                    meta.len()
                ));
            }
            let bytes = std::fs::read(&p).map_err(|e| anyhow!("gagal membaca gambar {rel}: {e}"))?;
            let dim_str = match extract_image_dimensions(&bytes) {
                Some((w, h)) => format!("{w}x{h}, "),
                None => String::new(),
            };
            return Ok(format!(
                "[Gambar dimuat: {rel} ({dim_str}{mime}, {} byte) — terlampir ke model untuk inspeksi visual.]",
                bytes.len()
            ));
        }

        // 2. Jalur multimodal PDF (ekstraktor murni Rust FlateDecode + fallback pdftotext, v0.5.28).
        let is_pdf = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if is_pdf {
            if meta.len() > MAX_READ_PDF_BYTES {
                return Err(anyhow!(
                    "file PDF {rel} terlalu besar ({} byte > {MAX_READ_PDF_BYTES})",
                    meta.len()
                ));
            }
            let bytes = std::fs::read(&p).map_err(|e| anyhow!("gagal membaca PDF {rel}: {e}"))?;
            let mut extracted = extract_pdf_text_bytes(&bytes).unwrap_or_default();
            if extracted.is_empty() {
                if let Some(cli_text) = try_pdftotext_fallback(&p).await {
                    extracted = cli_text;
                }
            }
            if extracted.is_empty() {
                return Ok(format!(
                    "[PDF '{rel}' ({} byte) tidak memiliki lapisan teks yang dapat diekstrak (kemungkinan hasil pindaian gambar).]",
                    bytes.len()
                ));
            }
            let safe = crate::security::redact_secrets(&extracted);
            let body = format_sliced_text(rel, &safe, &args, ctx)?;
            return Ok(format!("[PDF: {rel} ({} byte)]\n{body}", bytes.len()));
        }

        // 3. Jalur standar FILE TEKS.
        let has_range = args.get("start").is_some() || args.get("end").is_some();
        const MAX_RAW_SLICE_BYTES: u64 = 10 * 1024 * 1024;
        if !has_range && meta.len() > MAX_READ_BYTES as u64 {
            return Err(anyhow!(
                "file {rel} terlalu besar ({} byte > {MAX_READ_BYTES}); baca per rentang baris dengan start/end",
                meta.len()
            ));
        }
        if meta.len() > MAX_RAW_SLICE_BYTES {
            return Err(anyhow!(
                "file {rel} terlalu besar ({} byte > {MAX_RAW_SLICE_BYTES})",
                meta.len()
            ));
        }
        let bytes = std::fs::read(&p).map_err(|e| anyhow!("gagal membaca {rel}: {e}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let formatted = format_sliced_text(rel, &text, &args, ctx)?;

        // Dedup pembacaan deterministik di lapisan tool (v0.5.32 — pola Hermes `file_tools.py` &
        // `file_tools_read_tracking.py`): lacak sidik SHA-256 isi berkas per sesi (selamat dari resume).
        let start_opt = args
            .get("start")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize);
        let end_opt = args
            .get("end")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let canon = super::read_tracker::canonical_key_path(&p);
        let raw_sha256 = crate::update::sha256_hex(&bytes);
        let rendered_sha256 = crate::update::sha256_hex(formatted.as_bytes());
        let (mtime_ns, file_size) = super::read_tracker::file_mtime_and_size(&p);
        let full_file = start_opt.is_none()
            && end_opt.is_none()
            && !formatted.contains("\n... [dipotong di ")
            && !formatted.contains("\n... (hasil dipotong");

        if let Ok(mut tracker) = ctx.read_tracker.lock() {
            if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
                tracker.hydrate_from_store(store, sid);
            }
            if let Some(stub) = tracker.check_dedup_or_invalidate(
                &canon,
                rel,
                start_opt,
                end_opt,
                &raw_sha256,
                &rendered_sha256,
            ) {
                return Ok(stub);
            }
            tracker.record_read(
                canon.clone(),
                rel.to_string(),
                start_opt,
                end_opt,
                raw_sha256.clone(),
                rendered_sha256,
                mtime_ns,
                file_size,
                full_file,
            );
        }
        if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
            let dedup_key = super::read_tracker::make_dedup_key(&canon, start_opt, end_opt);
            let _ = store.upsert_session_read_file(
                sid,
                &crate::store::PersistedReadFile {
                    dedup_key,
                    rel_path: rel.to_string(),
                    canonical_path: canon,
                    start_line: start_opt,
                    end_line: end_opt,
                    content_sha256: raw_sha256,
                    mtime_ns,
                    file_size,
                },
            );
        }

        Ok(formatted)
    }
}

// ---------- write_file ----------
pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Tulis file baru/kecil; untuk ubah lokal file besar pakai edit_file.".into(),
            parameters: schema(
                json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rel = str_arg(&args, "path").ok_or_else(|| anyhow!("argumen 'path' wajib"))?;
        let content = str_arg(&args, "content").ok_or_else(|| anyhow!("argumen 'content' wajib"))?;
        let p = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(rel))?;
        if is_secret_file(&p) {
            return Err(anyhow!("file '{rel}' ditolak: file rahasia tidak boleh ditulis lewat tool"));
        }
        let rel_norm = rel.trim().replace('\\', "/").to_ascii_lowercase();
        if rel_norm.split('/').any(|s| matches!(s, "node_modules" | ".pnpm")) {
            return Err(anyhow!(
                "file '{rel}' ditolak: jangan memodifikasi isi node_modules/ — perbaiki berkas konfigurasi atau kode sumber proyek di luar node_modules"
            ));
        }
        ctx.sandbox.check_write_allowed("write_file", rel)?;
        let before = std::fs::read(&p).ok();
        let existed = before.is_some();
        let old = before
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let diff = unified_diff(&old, content, rel);
        if !ctx.approver.ask(&format!("write_file {rel}"), &diff).await {
            return Ok(format!("Ditolak user — file {rel} TIDAK ditulis."));
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content)?;
        let canon_written = super::read_tracker::canonical_key_path(&p);
        if let Ok(mut tracker) = ctx.read_tracker.lock() {
            tracker.invalidate_path(&canon_written);
        }
        if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
            let _ = store.delete_session_read_file_path(sid, &canon_written);
        }
        if let Ok(mut stack) = ctx.undo_stack.lock() {
            stack.record(super::undo::FileSnapshot {
                path: p,
                rel: rel.to_string(),
                before,
                after: content.as_bytes().to_vec(),
            });
        }
        let mut msg = format!("OK: {rel} ditulis ({} byte).", content.len());
        // Masalah 3 (v0.5.20): beri dorongan terarah saat model menulis ulang utuh berkas besar
        // yang sudah ada padahal mayoritas barisnya tidak berubah (seharusnya memakai `edit_file`).
        if existed && old.len() >= 4_000 {
            let similarity = TextDiff::from_lines(old.as_str(), content).ratio();
            if similarity >= 0.65 {
                msg.push_str(&format!(
                    "\n[Efisiensi] Berkas ini sudah ada ({} byte, {:.0}% baris sama) — untuk perubahan lokal berikutnya pada berkas besar, gunakan `edit_file` (patch terarah) agar hemat token keluaran & langkah.",
                    old.len(),
                    similarity * 100.0
                ));
            }
        }
        Ok(msg)
    }
}

// ---------- Fuzzy find-and-replace (desain internal SelenCode) ----------

fn normalize_unicode_typographic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{201c}' | '\u{201d}' => out.push('"'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{2014}' => out.push_str("--"),
            '\u{2013}' | '\u{2212}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            '\u{00a0}'
            | '\u{2000}'..='\u{200a}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

fn split_lines_preserve_ending(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(&text[start..=idx]);
            start = idx + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn line_body(line_with_nl: &str) -> &str {
    line_with_nl
        .strip_suffix('\n')
        .unwrap_or(line_with_nl)
        .strip_suffix('\r')
        .unwrap_or_else(|| line_with_nl.strip_suffix('\n').unwrap_or(line_with_nl))
}

fn leading_ws(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| !matches!(c, ' ' | '\t'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..end]
}

fn reindent_replacement(matched_lines: &[&str], old_lines: &[&str], new_text: &str, nl: &str) -> String {
    let target_indent = matched_lines
        .iter()
        .map(|l| line_body(l))
        .find(|l| !l.trim().is_empty())
        .map(leading_ws)
        .unwrap_or("");
    let old_indent = old_lines
        .iter()
        .map(|l| line_body(l))
        .find(|l| !l.trim().is_empty())
        .map(leading_ws)
        .unwrap_or("");
    let new_lines: Vec<&str> = new_text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let adjusted: Vec<String> = new_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else if let Some(rest) = l.strip_prefix(old_indent) {
                format!("{target_indent}{rest}")
            } else {
                format!("{target_indent}{}", l.trim_start())
            }
        })
        .collect();
    adjusted.join(nl)
}

fn closest_window_hint(text: &str, old: &str) -> Option<String> {
    let file_lines: Vec<&str> = text.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();
    if file_lines.is_empty() || old_lines.is_empty() {
        return None;
    }
    let win = old_lines.len().clamp(1, 20).min(file_lines.len());
    let mut best_score = 0.0f32;
    let mut best_start = 0usize;
    for i in 0..=file_lines.len() - win {
        let cand = file_lines[i..i + win].join("\n");
        let score = TextDiff::from_lines(cand.as_str(), old).ratio();
        if score > best_score {
            best_score = score;
            best_start = i;
        }
    }
    if best_score < 0.45 {
        return None;
    }
    let preview: String = file_lines[best_start..best_start + win]
        .iter()
        .enumerate()
        .map(|(k, l)| format!("{:>4}| {}", best_start + k + 1, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Maksudmu baris {}-{} (kemiripan {:.0}%)?\n{preview}",
        best_start + 1,
        best_start + win,
        best_score * 100.0
    ))
}

/// Pencocokan & penggantian bertingkat untuk `edit_file` (desain internal SelenCode):
/// 1. `exact` — cocok persis (unik 1x).
/// 2. `line_trimmed` — toleran spasi ujung & CRLF/LF (unik 1x).
/// 3. `indent_preserving` — toleran selisih indentasi dengan melestarikan indentasi blok target (unik 1x).
/// 4. `unicode_normalized` — normalisasi tanda kutip pintar/dash/NBSP (SelenCode `UNICODE_MAP`, unik 1x).
/// Mengembalikan `(isi_baru, nama_strategi)`.
pub fn fuzzy_find_and_replace(
    content: &str,
    old_s: &str,
    new_s: &str,
    label: &str,
) -> Result<(String, &'static str)> {
    if old_s.is_empty() {
        return Err(anyhow!("'old' tidak boleh kosong"));
    }
    if old_s == new_s {
        return Err(anyhow!(
            "tidak ada perubahan karena 'old' dan 'new' identik di {label}"
        ));
    }
    // 1. Exact literal match
    let exact_count = content.matches(old_s).count();
    if exact_count == 1 {
        return Ok((content.replacen(old_s, new_s, 1), "exact"));
    }
    if exact_count > 1 {
        return Err(anyhow!(
            "'old' muncul {exact_count} kali di {label} — harus unik, persempit potongannya"
        ));
    }

    let nl = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let file_lines = split_lines_preserve_ending(content);
    let old_lines_raw = split_lines_preserve_ending(old_s);
    if file_lines.is_empty() || old_lines_raw.is_empty() || old_lines_raw.len() > file_lines.len() {
        let hint = closest_window_hint(content, old_s)
            .map(|h| format!("\n{h}"))
            .unwrap_or_default();
        return Err(anyhow!("'old' tidak ditemukan di {label}{hint}"));
    }
    let n = old_lines_raw.len();
    let mut offsets = Vec::with_capacity(file_lines.len() + 1);
    let mut off = 0usize;
    for l in &file_lines {
        offsets.push(off);
        off += l.len();
    }
    offsets.push(off);

    let replace_window = |start_idx: usize, repl_body: String| -> String {
        let span_start = offsets[start_idx];
        let last_line = file_lines[start_idx + n - 1];
        let had_trailing_nl = last_line.ends_with('\n');
        let span_end = offsets[start_idx + n];
        let mut out = String::with_capacity(content.len() + repl_body.len());
        out.push_str(&content[..span_start]);
        out.push_str(&repl_body);
        if !repl_body.is_empty() && had_trailing_nl && !repl_body.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str(&content[span_end..]);
        out
    };

    // 2. Line-end trimmed (toleran trailing spaces & CRLF vs LF)
    let old_rt: Vec<&str> = old_lines_raw.iter().map(|l| line_body(l).trim_end()).collect();
    let hits_rt: Vec<usize> = (0..=file_lines.len() - n)
        .filter(|&i| {
            (0..n).all(|k| line_body(file_lines[i + k]).trim_end() == old_rt[k])
        })
        .collect();
    if hits_rt.len() == 1 {
        let norm_new = new_s
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect::<Vec<_>>()
            .join(nl);
        return Ok((replace_window(hits_rt[0], norm_new), "line_trimmed"));
    }
    if hits_rt.len() > 1 {
        return Err(anyhow!(
            "'old' muncul {} kali di {label} (line_trimmed) — harus unik, persempit potongannya",
            hits_rt.len()
        ));
    }

    // 3. Indentation-preserving trimmed lines
    let old_tr: Vec<&str> = old_lines_raw.iter().map(|l| line_body(l).trim()).collect();
    let hits_tr: Vec<usize> = (0..=file_lines.len() - n)
        .filter(|&i| (0..n).all(|k| line_body(file_lines[i + k]).trim() == old_tr[k]))
        .collect();
    if hits_tr.len() == 1 {
        let i = hits_tr[0];
        let repl = reindent_replacement(&file_lines[i..i + n], &old_lines_raw, new_s, nl);
        return Ok((replace_window(i, repl), "indent_preserving"));
    }
    if hits_tr.len() > 1 {
        return Err(anyhow!(
            "'old' muncul {} kali di {label} (indent_preserving) — harus unik, persempit potongannya",
            hits_tr.len()
        ));
    }

    // 4. Unicode typographic normalization (SelenCode UNICODE_MAP)
    let old_uni: Vec<String> = old_lines_raw
        .iter()
        .map(|l| normalize_unicode_typographic(line_body(l).trim_end()))
        .collect();
    let hits_uni: Vec<usize> = (0..=file_lines.len() - n)
        .filter(|&i| {
            (0..n).all(|k| normalize_unicode_typographic(line_body(file_lines[i + k]).trim_end()) == old_uni[k])
        })
        .collect();
    if hits_uni.len() == 1 {
        let norm_new = new_s
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect::<Vec<_>>()
            .join(nl);
        return Ok((replace_window(hits_uni[0], norm_new), "unicode_normalized"));
    }

    let hint = closest_window_hint(content, old_s)
        .map(|h| format!("\n{h}"))
        .unwrap_or_default();
    Err(anyhow!("'old' tidak ditemukan di {label}{hint}"))
}

/// Ganti SEMUA kemunculan non-tumpang-tindih `old_s` -> `new_s` (exact dulu, fallback line-trimmed) untuk `edit_file(replace_all=true)`.
pub fn replace_all_occurrences(
    content: &str,
    old_s: &str,
    new_s: &str,
    label: &str,
) -> Result<(String, usize, &'static str)> {
    if old_s.is_empty() {
        return Err(anyhow!("'old' tidak boleh kosong"));
    }
    let exact_count = content.matches(old_s).count();
    if exact_count > 0 {
        return Ok((content.replace(old_s, new_s), exact_count, "exact_all"));
    }

    let nl = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let file_lines = split_lines_preserve_ending(content);
    let old_lines_raw = split_lines_preserve_ending(old_s);
    let n = old_lines_raw.len();
    if n > 0 && n <= file_lines.len() {
        let old_rt: Vec<&str> = old_lines_raw.iter().map(|l| line_body(l).trim_end()).collect();
        let norm_new = new_s
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect::<Vec<_>>()
            .join(nl);
        let mut out = String::with_capacity(content.len());
        let mut i = 0usize;
        let mut replaced = 0usize;
        while i < file_lines.len() {
            if i + n <= file_lines.len()
                && (0..n).all(|k| line_body(file_lines[i + k]).trim_end() == old_rt[k])
            {
                let had_trailing_nl = file_lines[i + n - 1].ends_with('\n');
                out.push_str(&norm_new);
                if had_trailing_nl && !norm_new.ends_with('\n') {
                    out.push_str(nl);
                }
                i += n;
                replaced += 1;
            } else {
                out.push_str(file_lines[i]);
                i += 1;
            }
        }
        if replaced > 0 {
            return Ok((out, replaced, "line_trimmed_all"));
        }
    }

    let hint = closest_window_hint(content, old_s)
        .map(|h| format!("\n{h}"))
        .unwrap_or_default();
    Err(anyhow!("'old' tidak ditemukan di {label}{hint}"))
}

// ---------- edit_file ----------
pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Patch terarah (old->new, replace_all opsional, atau array edits[]) dengan pencocokan toleran spasi.".into(),
            parameters: schema(
                json!({
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "replace_all": {"type": "boolean"},
                    "edits": {"type": "array", "items": {"type": "object"}}
                }),
                &["path"],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rel = str_arg(&args, "path").ok_or_else(|| anyhow!("argumen 'path' wajib"))?;
        let top_replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut ops: Vec<(String, String, bool)> = Vec::new();
        if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
            if arr.is_empty() {
                return Err(anyhow!("'edits' tidak boleh kosong"));
            }
            for (idx, item) in arr.iter().enumerate() {
                let o = item
                    .get("old")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("edits[{idx}].old wajib"))?;
                let n = item
                    .get("new")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("edits[{idx}].new wajib"))?;
                let rep_all = item
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(top_replace_all);
                ops.push((o.to_string(), n.to_string(), rep_all));
            }
        } else {
            let old_s = str_arg(&args, "old").ok_or_else(|| anyhow!("argumen 'old' wajib"))?;
            let new_s = str_arg(&args, "new").ok_or_else(|| anyhow!("argumen 'new' wajib"))?;
            ops.push((old_s.to_string(), new_s.to_string(), top_replace_all));
        }
        let p = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(rel))?;
        if is_secret_file(&p) {
            return Err(anyhow!("file '{rel}' ditolak: file rahasia tidak boleh diedit lewat tool"));
        }
        let rel_norm = rel.trim().replace('\\', "/").to_ascii_lowercase();
        if rel_norm.split('/').any(|s| matches!(s, "node_modules" | ".pnpm")) {
            return Err(anyhow!(
                "file '{rel}' ditolak: jangan mengedit isi node_modules/ — perbaiki berkas konfigurasi atau kode sumber proyek di luar node_modules"
            ));
        }
        ctx.sandbox.check_write_allowed("edit_file", rel)?;
        let text = std::fs::read_to_string(&p).map_err(|e| anyhow!("gagal membaca {rel}: {e}"))?;
        let mut working = text.clone();
        let mut strategies: Vec<&'static str> = Vec::with_capacity(ops.len());
        let mut total_replacements = 0usize;
        for (old_s, new_s, rep_all) in &ops {
            if *rep_all {
                let (updated, count, strat) = replace_all_occurrences(&working, old_s, new_s, rel)?;
                working = updated;
                total_replacements += count;
                strategies.push(strat);
            } else {
                let (updated, strat) = fuzzy_find_and_replace(&working, old_s, new_s, rel)?;
                working = updated;
                total_replacements += 1;
                strategies.push(strat);
            }
        }
        let diff = unified_diff(&text, &working, rel);
        if !ctx.approver.ask(&format!("edit_file {rel}"), &diff).await {
            return Ok(format!("Ditolak user — file {rel} TIDAK diubah."));
        }
        std::fs::write(&p, &working)?;
        let canon_edited = super::read_tracker::canonical_key_path(&p);
        if let Ok(mut tracker) = ctx.read_tracker.lock() {
            tracker.invalidate_path(&canon_edited);
        }
        if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
            let _ = store.delete_session_read_file_path(sid, &canon_edited);
        }
        if let Ok(mut stack) = ctx.undo_stack.lock() {
            stack.record(super::undo::FileSnapshot {
                path: p,
                rel: rel.to_string(),
                before: Some(text.into_bytes()),
                after: working.into_bytes(),
            });
        }
        if ops.len() > 1 {
            if total_replacements > ops.len() {
                Ok(format!(
                    "OK: {rel} diedit ({} patch diterapkan, {total_replacements} penggantian).",
                    ops.len()
                ))
            } else {
                Ok(format!("OK: {rel} diedit ({} patch diterapkan).", ops.len()))
            }
        } else if ops[0].2 {
            Ok(format!(
                "OK: {rel} diedit ({total_replacements} penggantian via {}).",
                strategies[0]
            ))
        } else if strategies[0] != "exact" {
            Ok(format!("OK: {rel} diedit (cocok via {}).", strategies[0]))
        } else {
            Ok(format!("OK: {rel} diedit."))
        }
    }
}
