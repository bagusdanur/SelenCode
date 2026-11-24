//! `selen update` — self-update dari GitHub Releases (v0.1.2).
//!
//! Alur: tanya API GitHub rilis terbaru repo SelenCode, bandingkan versi
//! semver dengan versi crate ini, unduh aset sesuai platform ke file
//! sementara DI SEBELAH exe aktif, lalu rename menimpa exe aktif (di
//! Linux/Termux rename di atas binary yang sedang berjalan itu aman —
//! proses lama tetap memakai inode lama sampai keluar).

use anyhow::{Context, Result};
use serde::Deserialize;

/// Repo sumber rilis. Bisa dioverride env SELENCODE_REPO (tes/dev).
pub const DEFAULT_REPO: &str = "bagusdanur/SelenCode";

pub fn repo_slug() -> String {
    std::env::var("SELENCODE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// Parse versi semver longgar: "v0.1.2" / "0.1.2" / "0.1.2-rc1" ->
/// Some((0, 1, 2)). None bila formatnya tidak dikenal.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    let s = s.split('-').next()?; // buang sufiks pre-release
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// true bila versi remote lebih baru dari versi saat ini.
pub fn is_newer(remote: (u32, u32, u32), current: (u32, u32, u32)) -> bool {
    remote > current
}

/// Nama aset rilis untuk platform ini:
/// - Termux (bionic/Android): android + aarch64 -> selen-termux-arm64
/// - Linux glibc: linux + x86_64 -> selen-linux-x64
/// - Windows: windows + x86_64 -> selen-windows-x64.exe
/// Selain itu None — tidak ada aset resmi.
pub fn asset_name_for(target_os: &str, target_arch: &str) -> Option<&'static str> {
    match (target_os, target_arch) {
        ("android", "aarch64") => Some("selen-termux-arm64"),
        ("linux", "x86_64") => Some("selen-linux-x64"),
        ("windows", "x86_64") => Some("selen-windows-x64.exe"),
        _ => None,
    }
}

/// Bersihkan sisa `.old.exe` dari update Windows sebelumnya (best-effort, no-op di Unix).
pub fn cleanup_old_binary() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old.exe"));
    }
}

/// Ganti binary `exe` dengan `bytes` secara aman lintas platform.
/// - Unix/Termux: tulis ke `.tmp` di sebelah `exe`, chmod 0755, rename atomik.
/// - Windows: exe yang sedang berjalan tidak bisa ditimpa langsung, tapi bisa
///   di-rename ke `.old.exe`; pindahkan `exe` -> `.old.exe` lalu `.tmp.exe` -> `exe`
///   (dengan rollback otomatis bila langkah kedua gagal).
pub fn replace_binary(exe: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(windows)]
    {
        let tmp = exe.with_extension("tmp.exe");
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::write(&tmp, bytes)
            .with_context(|| format!("gagal menulis file sementara {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(exe, &old) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("gagal memindahkan binary lama ke .old.exe");
        }
        if let Err(e) = std::fs::rename(&tmp, exe) {
            let _ = std::fs::rename(&old, exe);
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("gagal mengganti binary lama dengan yang baru");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let tmp = exe.with_extension("tmp");
        std::fs::write(&tmp, bytes)
            .with_context(|| format!("gagal menulis file sementara {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                .context("gagal mengatur izin eksekusi file baru")?;
        }
        if let Err(e) = std::fs::rename(&tmp, exe) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("gagal mengganti binary lama dengan yang baru");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Jalankan self-update. Semua kegagalan dikembalikan sebagai Err dengan
/// pesan yang jelas — tidak ada panic.
pub async fn run() -> Result<()> {
    let current_str = env!("CARGO_PKG_VERSION");
    let current = parse_version(current_str)
        .ok_or_else(|| anyhow::anyhow!("versi crate tidak dikenal: {current_str}"))?;

    let asset_name = asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "platform {}-{} tidak didukung update otomatis; unduh manual dari GitHub Releases",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;

    let repo = repo_slug();
    let client = reqwest::Client::builder()
        .user_agent("selencode")
        .build()
        .context("gagal membuat HTTP client")?;

    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let release: Release = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("gagal menghubungi GitHub — periksa koneksi internet (offline?)")?
        .error_for_status()
        .context("GitHub menolak permintaan rilis terbaru")?
        .json()
        .await
        .context("respons GitHub tidak bisa dibaca")?;

    let remote = parse_version(&release.tag_name).ok_or_else(|| {
        anyhow::anyhow!("tag rilis tidak dikenal: {}", release.tag_name)
    })?;
    if !is_newer(remote, current) {
        println!("Sudah versi terbaru (v{current_str}).");
        return Ok(());
    }

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "aset {asset_name} tidak ditemukan di rilis {}",
                release.tag_name
            )
        })?;

    println!("Mengunduh SelenCode {} ({asset_name})…", release.tag_name);
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("unduhan gagal — periksa koneksi internet")?
        .error_for_status()
        .context("GitHub menolak unduhan aset")?
        .bytes()
        .await
        .context("unduhan terputus di tengah jalan")?;

    let exe = std::env::current_exe().context("tidak bisa menemukan lokasi binary selen")?;
    replace_binary(&exe, &bytes)?;
    println!(
        "Diperbarui ke {} — jalankan `selen` lagi.",
        release.tag_name
    );
    Ok(())
}
