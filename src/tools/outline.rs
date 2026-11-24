//! Tool `outline` (Fase 5): struktur file kode via tree-sitter, tanpa
//! membaca isi penuh. Sandbox, penolakan file rahasia, dan batas ukuran
//! memakai ulang aturan read_file (file.rs) persis.

use super::file::MAX_READ_BYTES;
use super::{schema, str_arg, Ctx, Tool};
use crate::outline::outline_source;
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct Outline;

#[async_trait]
impl Tool for Outline {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "outline".into(),
            description: "Lihat struktur file kode (fungsi, class, struct, impl, interface per baris) TANPA membaca isi penuh — jauh lebih hemat token. Bahasa: rs, py, js, mjs, jsx, ts, tsx. Pakai SEBELUM read_file untuk file kode besar: pahami strukturnya dulu, baru baca bagian yang perlu.".into(),
            parameters: schema(json!({"path": {"type": "string"}}), &["path"]),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let rel = str_arg(&args, "path").ok_or_else(|| anyhow!("argumen 'path' wajib"))?;
        let p = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(rel))?;
        if is_secret_file(&p) {
            return Err(anyhow!(
                "file '{rel}' ditolak: termasuk file rahasia (.env/*.key/*.pem/id_rsa)"
            ));
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext.is_empty() {
            return Err(anyhow!(
                "file '{rel}' tidak punya ekstensi yang didukung outline, pakai read_file"
            ));
        }
        let bytes = std::fs::read(&p).map_err(|e| anyhow!("gagal membaca {rel}: {e}"))?;
        if bytes.len() > MAX_READ_BYTES {
            return Err(anyhow!(
                "file {rel} terlalu besar ({} byte > {MAX_READ_BYTES}) untuk outline",
                bytes.len()
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        let out = outline_source(&text, &ext)?;
        if out.is_empty() {
            return Ok(format!(
                "(tidak ada struktur ditemukan di {rel} — file kosong atau tanpa deklarasi)"
            ));
        }
        Ok(out)
    }
}
