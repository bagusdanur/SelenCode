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
/// Batas karakter per baris sebelum dipotong rapi (diporting dari Hermes `tools/tool_output_limits.py`).
pub const DEFAULT_MAX_LINE_LENGTH: usize = 2000;
/// Batas baris bawaan untuk `read_file` tanpa rentang eksplisit `start`/`end` (pola Hermes `file_tools.py`).
pub const DEFAULT_FILE_READ_LINES: usize = 250;
/// Batas karakter praktis bawaan per panggilan `read_file` (diporting dari Hermes `tools/file_tools.py`: `_truncate_to_char_budget`).
pub const DEFAULT_FILE_READ_MAX_CHARS: usize = 16_000;

/// Potong daftar baris `read_file` pada BARIS UTUH terakhir yang muat di dalam `max_chars`
/// (diporting dari Hermes Agent `tools/file_tools.py`: `_truncate_to_char_budget`).
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

// ---------- edit_file ----------
pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Patch terarah: ganti teks persis `old` (unik 1x) -> `new`.".into(),
            parameters: schema(
                json!({"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}),
                &["path", "old", "new"],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rel = str_arg(&args, "path").ok_or_else(|| anyhow!("argumen 'path' wajib"))?;
        let old_s = str_arg(&args, "old").ok_or_else(|| anyhow!("argumen 'old' wajib"))?;
        let new_s = str_arg(&args, "new").ok_or_else(|| anyhow!("argumen 'new' wajib"))?;
        if old_s.is_empty() {
            return Err(anyhow!("'old' tidak boleh kosong"));
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
        let count = text.matches(old_s).count();
        if count == 0 {
            return Err(anyhow!("'old' tidak ditemukan di {rel}"));
        }
        if count > 1 {
            return Err(anyhow!("'old' muncul {count} kali di {rel} — harus unik, persempit potongannya"));
        }
        let new_text = text.replacen(old_s, new_s, 1);
        let diff = unified_diff(&text, &new_text, rel);
        if !ctx.approver.ask(&format!("edit_file {rel}"), &diff).await {
            return Ok(format!("Ditolak user — file {rel} TIDAK diubah."));
        }
        std::fs::write(&p, &new_text)?;
        if let Ok(mut stack) = ctx.undo_stack.lock() {
            stack.record(super::undo::FileSnapshot {
                path: p,
                rel: rel.to_string(),
                before: Some(text.into_bytes()),
                after: new_text.into_bytes(),
            });
        }
        Ok(format!("OK: {rel} diedit."))
    }
}
