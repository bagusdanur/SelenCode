//! Tool LSP & Pemahaman Kode Lanjutan (Tahap 4):
//! - `lsp_diagnostics` (baca-saja)
//! - `lsp_definition`  (baca-saja)
//! - `lsp_references`  (baca-saja)
//! - `lsp_hover`       (baca-saja)
//! - `lsp_rename`      (refactoring simbol lintas workspace, butuh approval kecuali `dry_run=true`,
//!                      terintegrasi penuh dengan `UndoStack` & `/rollback`)

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

pub struct LspDiagnostics;

#[async_trait]
impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_diagnostics".into(),
            description: "Ambil diagnostik/error bahasa untuk 1 file atau seluruh workspace via Language Server (rust-analyzer/tsserver/pyright/gopls) atau fallback sintaks Tree-sitter.".into(),
            parameters: schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Opsional: path relatif file. Kosongkan untuk memeriksa seluruh workspace."
                    }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let path = str_arg(&args, "path");
        let rep = ctx.lsp_manager.diagnostics(path).await?;
        if rep.diagnostics.is_empty() {
            return Ok(format!(
                "[{}] Tidak ada diagnostik error yang ditemukan.",
                rep.backend
            ));
        }
        let mut out = format!(
            "[{}] Ditemukan {} diagnostik:\n",
            rep.backend,
            rep.diagnostics.len()
        );
        for d in rep.diagnostics.iter().take(40) {
            out.push_str(&format!(
                "- [{}] {}:{}:{} ({}) — {}\n",
                d.severity, d.rel_path, d.line, d.col, d.source, d.message
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct LspDefinition;

#[async_trait]
impl Tool for LspDefinition {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_definition".into(),
            description: "Go to definition: temukan lokasi deklarasi simbol berdasarkan nama `symbol` atau posisi (`path`, `line`, `col`).".into(),
            parameters: schema(
                json!({
                    "symbol": { "type": "string", "description": "Nama simbol (opsional bila path+line+col diberikan)" },
                    "path": { "type": "string", "description": "Path relatif file (opsional)" },
                    "line": { "type": "integer", "description": "Nomor baris 1-based (opsional)" },
                    "col": { "type": "integer", "description": "Nomor kolom 1-based (opsional)" }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rep = ctx
            .lsp_manager
            .definition(
                str_arg(&args, "path"),
                opt_usize(&args, "line"),
                opt_usize(&args, "col"),
                str_arg(&args, "symbol"),
            )
            .await?;
        if rep.locations.is_empty() {
            return Ok(format!(
                "[{}] Definisi simbol '{}' tidak ditemukan.",
                rep.backend, rep.symbol
            ));
        }
        let mut out = format!(
            "[{}] Definisi '{}' ({} lokasi):\n",
            rep.backend,
            rep.symbol,
            rep.locations.len()
        );
        for loc in &rep.locations {
            out.push_str(&format!(
                "- {}:{}:{} — {}\n",
                loc.rel_path, loc.line, loc.col, loc.preview
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct LspReferences;

#[async_trait]
impl Tool for LspReferences {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_references".into(),
            description: "Find references: lacak semua lokasi pemakaian simbol di workspace berdasarkan `symbol` atau posisi (`path`, `line`, `col`).".into(),
            parameters: schema(
                json!({
                    "symbol": { "type": "string", "description": "Nama simbol (opsional bila path+line+col diberikan)" },
                    "path": { "type": "string", "description": "Path relatif file (opsional)" },
                    "line": { "type": "integer", "description": "Nomor baris 1-based (opsional)" },
                    "col": { "type": "integer", "description": "Nomor kolom 1-based (opsional)" }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rep = ctx
            .lsp_manager
            .references(
                str_arg(&args, "path"),
                opt_usize(&args, "line"),
                opt_usize(&args, "col"),
                str_arg(&args, "symbol"),
            )
            .await?;
        if rep.locations.is_empty() {
            return Ok(format!(
                "[{}] Tidak ada referensi simbol '{}' di workspace.",
                rep.backend, rep.symbol
            ));
        }
        let mut out = format!(
            "[{}] Ditemukan {} referensi '{}':\n",
            rep.backend,
            rep.locations.len(),
            rep.symbol
        );
        for loc in &rep.locations {
            out.push_str(&format!(
                "- {}:{}:{} — {}\n",
                loc.rel_path, loc.line, loc.col, loc.preview
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct LspHover;

#[async_trait]
impl Tool for LspHover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_hover".into(),
            description: "Ambil informasi tipe, signature, dan dokumentasi simbol (hover) via LSP atau fallback Tree-sitter.".into(),
            parameters: schema(
                json!({
                    "symbol": { "type": "string", "description": "Nama simbol (opsional bila path+line+col diberikan)" },
                    "path": { "type": "string", "description": "Path relatif file (opsional)" },
                    "line": { "type": "integer", "description": "Nomor baris 1-based (opsional)" },
                    "col": { "type": "integer", "description": "Nomor kolom 1-based (opsional)" }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rep = ctx
            .lsp_manager
            .hover(
                str_arg(&args, "path"),
                opt_usize(&args, "line"),
                opt_usize(&args, "col"),
                str_arg(&args, "symbol"),
            )
            .await?;
        Ok(format!("[{}] Hover '{}':\n{}", rep.backend, rep.symbol, rep.content))
    }
}

pub struct LspRename;

#[async_trait]
impl Tool for LspRename {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_rename".into(),
            description: "Rename simbol secara aman lintas workspace (menggunakan LSP rename atau penggantian batas-identifier ketat). Mendukung dry_run=true untuk pratinjau tanpa menulis, meminta izin user saat menulis, dan mencatat snapshot Undo/Rollback.".into(),
            parameters: schema(
                json!({
                    "old_name": { "type": "string", "description": "Nama simbol lama (opsional bila path+line+col diberikan)" },
                    "new_name": { "type": "string", "description": "Nama simbol baru (identifier valid)" },
                    "path": { "type": "string", "description": "Path relatif file (opsional)" },
                    "line": { "type": "integer", "description": "Nomor baris 1-based (opsional)" },
                    "col": { "type": "integer", "description": "Nomor kolom 1-based (opsional)" },
                    "dry_run": { "type": "boolean", "description": "true = pratinjau dampak file tanpa menulis ke disk (default: false)" }
                }),
                &["new_name"],
            ),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        !args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let new_name = str_arg(&args, "new_name")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'new_name' wajib diisi"))?;
        let dry_run = args
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let path = str_arg(&args, "path");
        let line = opt_usize(&args, "line");
        let col = opt_usize(&args, "col");
        let old_name = str_arg(&args, "old_name");

        // Selalu pratinjau dulu untuk detail approval yang informatif saat dry_run == false!
        let preview = ctx
            .lsp_manager
            .rename_symbol(path, line, col, old_name, new_name, true, &ctx.undo_stack)
            .await?;
        if preview.total_replacements == 0 {
            return Ok(format!(
                "[{}] Tidak ada kemunculan '{}' yang berubah.",
                preview.backend, preview.old_name
            ));
        }

        if dry_run {
            let mut out = format!(
                "[{}] Pratinjau rename '{}' -> '{}' ({} penggantian di {} file — BELUM ditulis):\n",
                preview.backend,
                preview.old_name,
                preview.new_name,
                preview.total_replacements,
                preview.files.len()
            );
            for f in &preview.files {
                out.push_str(&format!("- {} ({} lokasi)\n", f.rel_path, f.replacements));
            }
            return Ok(out.trim_end().to_string());
        }

        ctx.sandbox.check_write_allowed(
            "lsp_rename",
            &format!("{} -> {}", preview.old_name, preview.new_name),
        )?;
        let detail = preview
            .files
            .iter()
            .take(12)
            .map(|f| format!("  {} ({} penggantian)", f.rel_path, f.replacements))
            .collect::<Vec<_>>()
            .join("\n");
        if !ctx
            .approver
            .ask(
                &format!("lsp_rename {} -> {}", preview.old_name, preview.new_name),
                &format!(
                    "Total {} penggantian di {} file:\n{detail}",
                    preview.total_replacements,
                    preview.files.len()
                ),
            )
            .await
        {
            return Ok("Ditolak user — rename simbol TIDAK diterapkan.".to_string());
        }

        let applied = ctx
            .lsp_manager
            .rename_symbol(path, line, col, old_name, new_name, false, &ctx.undo_stack)
            .await?;
        let mut out = format!(
            "[{}] Rename '{}' -> '{}' diterapkan ({} penggantian di {} file, snapshot /undo & /rollback tercatat):\n",
            applied.backend,
            applied.old_name,
            applied.new_name,
            applied.total_replacements,
            applied.files.len()
        );
        for f in &applied.files {
            out.push_str(&format!("- {} ({} penggantian)\n", f.rel_path, f.replacements));
        }
        Ok(out.trim_end().to_string())
    }
}
