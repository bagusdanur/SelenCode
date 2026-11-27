use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

pub const MAX_READ_LINES: usize = 2000;
pub const MAX_READ_BYTES: usize = 100 * 1024;
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
            description: "Daftar file workspace (maks 500).".into(),
            parameters: schema(json!({"path": {"type": "string"}}), &[]),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let root = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(&path_arg(&args)))?;
        let mut out = Vec::new();
        for e in ignore::WalkBuilder::new(&root).require_git(false).build().flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let rel = e.path().strip_prefix(&ctx.cwd).unwrap_or(e.path());
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

// ---------- read_file ----------
pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Baca file teks (opsional baris start..end, 1-based).".into(),
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
        let has_range = args.get("start").is_some() || args.get("end").is_some();
        const MAX_RAW_SLICE_BYTES: u64 = 10 * 1024 * 1024;
        let meta = std::fs::metadata(&p).map_err(|e| anyhow!("gagal membaca {rel}: {e}"))?;
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
        let lines: Vec<&str> = text.lines().collect();
        let start = args.get("start").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let end = args
            .get("end")
            .and_then(|v| v.as_u64())
            .map(|e| e as usize)
            .unwrap_or(lines.len())
            .min(lines.len());
        if start > lines.len() && !lines.is_empty() {
            return Err(anyhow!("start={start} melebihi jumlah baris {}", lines.len()));
        }
        let slice = &lines[(start - 1).min(lines.len())..end];
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
        if had_trailing_nl && !repl_body.ends_with('\n') {
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

// ---------- edit_file ----------
pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Patch terarah (old->new atau array edits[]) dengan pencocokan toleran spasi.".into(),
            parameters: schema(
                json!({
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
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
        let mut ops: Vec<(String, String)> = Vec::new();
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
                ops.push((o.to_string(), n.to_string()));
            }
        } else {
            let old_s = str_arg(&args, "old").ok_or_else(|| anyhow!("argumen 'old' wajib"))?;
            let new_s = str_arg(&args, "new").ok_or_else(|| anyhow!("argumen 'new' wajib"))?;
            ops.push((old_s.to_string(), new_s.to_string()));
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
        for (old_s, new_s) in &ops {
            let (updated, strat) = fuzzy_find_and_replace(&working, old_s, new_s, rel)?;
            working = updated;
            strategies.push(strat);
        }
        let diff = unified_diff(&text, &working, rel);
        if !ctx.approver.ask(&format!("edit_file {rel}"), &diff).await {
            return Ok(format!("Ditolak user — file {rel} TIDAK diubah."));
        }
        std::fs::write(&p, &working)?;
        if let Ok(mut stack) = ctx.undo_stack.lock() {
            stack.record(super::undo::FileSnapshot {
                path: p,
                rel: rel.to_string(),
                before: Some(text.into_bytes()),
                after: working.into_bytes(),
            });
        }
        if ops.len() > 1 {
            Ok(format!("OK: {rel} diedit ({} patch diterapkan).", ops.len()))
        } else if strategies[0] != "exact" {
            Ok(format!("OK: {rel} diedit (cocok via {}).", strategies[0]))
        } else {
            Ok(format!("OK: {rel} diedit."))
        }
    }
}
