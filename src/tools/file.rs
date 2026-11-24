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
            description: "Daftar file di folder project (hormati .gitignore, maks 500).".into(),
            parameters: schema(json!({"path": {"type": "string", "description": "folder relatif, default ."}}), &[]),
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
            description: "Baca isi file teks (maks 2000 baris / 100KB). start/end = nomor baris 1-based opsional.".into(),
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
        let take = slice.len().min(MAX_READ_LINES);
        let mut out = slice[..take].join("\n");
        if out.len() > MAX_READ_BYTES {
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
            description: "Tulis file baru / timpa file (butuh persetujuan, diff ditampilkan).".into(),
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
        let before = std::fs::read(&p).ok();
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
        Ok(format!("OK: {rel} ditulis ({} byte).", content.len()))
    }
}

// ---------- edit_file ----------
pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Ganti potongan teks persis dalam file. 'old' WAJIB muncul tepat 1 kali. Butuh persetujuan.".into(),
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
