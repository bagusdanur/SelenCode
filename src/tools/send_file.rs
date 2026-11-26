//! Tool `send_file` (Perbaikan 4 v0.5.7): kirim berkas hasil kerja (ZIP, laporan, gambar)
//! langsung sebagai lampiran pesan Telegram dari sesi gateway.
//!
//! Keamanan & batas:
//! - Berkas WAJIB berada di dalam wilayah sandbox (`cwd` proyek atau direktori data SelenCode)
//!   dan bukan berkas rahasia (`.env`, `*.key`, `id_rsa`).
//! - Batas ukuran mengikuti Telegram Bot API (50 MB): berkas > 50 MB ditolak jelas dengan
//!   saran jalur lama (tautan Google Drive), tidak gagal diam dan tidak dipotong.
//! - Di sesi CLI/TUI (non-gateway), menjawab sopan bahwa lampiran hanya tersedia via Telegram
//!   beserta path lokal berkasnya.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Batas ukuran unggahan Telegram Bot API standar: 50 MB.
pub const TELEGRAM_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Direktori data SelenCode (`<data_dir>/selencode`, menghormati `SELENCODE_DATA_DIR`).
pub fn selen_data_root() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("SELENCODE_DATA_DIR") {
        let t = custom.trim();
        if !t.is_empty() {
            return Some(PathBuf::from(t).join("selencode"));
        }
    }
    dirs::data_dir().map(|d| d.join("selencode"))
}

/// Validasi path & ukuran berkas untuk `send_file` (murni & mudah diuji unit).
/// Mengembalikan `(canonical_path, size_bytes, is_image)` atau error yang jelas.
pub fn validate_send_file_target(
    raw_path: &str,
    cwd: &Path,
    data_root: Option<&Path>,
    max_bytes: u64,
) -> Result<(PathBuf, u64, bool)> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("argumen 'path' wajib diisi"));
    }
    let req = Path::new(trimmed);
    // 1. Coba resolve di sandbox `cwd` terlebih dahulu.
    let resolved = match resolve_in_sandbox(cwd, req) {
        Ok(p) => p,
        Err(cwd_err) => {
            // 2. Izinkan juga berkas yang berada di dalam direktori data SelenCode.
            if let Some(droot) = data_root {
                let droot_canon = droot.canonicalize().unwrap_or_else(|_| droot.to_path_buf());
                let cand = if req.is_absolute() {
                    req.canonicalize().ok()
                } else {
                    resolve_in_sandbox(&droot_canon, req).ok()
                };
                match cand {
                    Some(p) if p.starts_with(&droot_canon) => p,
                    _ => {
                        return Err(anyhow!(
                            "berkas '{}' ditolak: berada di luar wilayah sandbox workspace/data SelenCode ({cwd_err})",
                            trimmed
                        ));
                    }
                }
            } else {
                return Err(anyhow!(
                    "berkas '{}' ditolak: berada di luar wilayah sandbox ({cwd_err})",
                    trimmed
                ));
            }
        }
    };

    if is_secret_file(&resolved) {
        return Err(anyhow!(
            "berkas '{}' ditolak: file rahasia (.env/*.key/*.pem/id_rsa) dilarang dikirim ke chat",
            trimmed
        ));
    }
    if !resolved.is_file() {
        return Err(anyhow!(
            "berkas '{}' tidak ditemukan atau bukan file biasa",
            trimmed
        ));
    }
    let meta = std::fs::metadata(&resolved)
        .map_err(|e| anyhow!("gagal membaca metadata berkas '{}': {e}", trimmed))?;
    let size = meta.len();
    if size == 0 {
        return Err(anyhow!("berkas '{}' kosong (0 byte)", trimmed));
    }
    if size > max_bytes {
        let mb = size as f64 / (1024.0 * 1024.0);
        let cap_mb = max_bytes as f64 / (1024.0 * 1024.0);
        return Err(anyhow!(
            "BATAS_UKURAN_TELEGRAM: berkas '{}' berukuran {:.1} MB (melebihi batas Telegram Bot API {:.0} MB). \
             Beri tahu pengguna dengan jelas bahwa berkas terlalu besar untuk lampiran langsung Telegram dan gunakan jalur tautan Google Drive / cloud seperti biasa.",
            trimmed,
            mb,
            cap_mb
        ));
    }
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") && size <= 10 * 1024 * 1024;
    Ok((resolved, size, is_image))
}

/// Handle pengirim berkas Telegram yang dipasang pada `Ctx` khusus di sesi gateway Telegram.
#[derive(Clone)]
pub struct TgFileSender {
    pub client: crate::telegram::TelegramClient,
    pub chat_id: i64,
}

pub struct SendFile;

#[async_trait]
impl Tool for SendFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_file".into(),
            description: "Kirim berkas hasil kerja (ZIP, laporan, gambar, maks 50 MB) sebagai lampiran ke chat Telegram.".into(),
            parameters: schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path berkas di dalam workspace atau folder data SelenCode"
                    },
                    "caption": {
                        "type": "string",
                        "description": "Keterangan singkat opsional untuk lampiran (maks 1 emoji)"
                    }
                }),
                &["path"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let raw_path = str_arg(&args, "path")
            .ok_or_else(|| anyhow!("argumen 'path' wajib diisi"))?;
        let caption = str_arg(&args, "caption").map(|s| s.trim()).filter(|s| !s.is_empty());

        let data_root = selen_data_root();
        let (canonical, size, is_image) =
            validate_send_file_target(raw_path, &ctx.cwd, data_root.as_deref(), TELEGRAM_MAX_FILE_BYTES)?;

        let Some(sender) = &ctx.tg_file_sender else {
            return Ok(format!(
                "Pengiriman lampiran langsung hanya tersedia pada sesi Telegram Gateway (`selen telegram`). \
                 Berkas siap di lokasi lokal: {} ({:.1} KB).",
                canonical.display(),
                size as f64 / 1024.0
            ));
        };

        let file_name = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("berkas")
            .to_string();
        if is_image {
            sender
                .client
                .send_photo(sender.chat_id, &canonical, caption)
                .await?;
        } else {
            sender
                .client
                .send_document(sender.chat_id, &canonical, caption)
                .await?;
        }
        Ok(format!(
            "OK: berkas '{}' ({:.1} KB) berhasil dikirim ke chat Telegram sebagai lampiran tersendiri.",
            file_name,
            size as f64 / 1024.0
        ))
    }
}
