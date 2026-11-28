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

/// Validasi slug repo GitHub (`owner/repo`) — hanya alfanumerik, `_`, `.`, `-`.
pub fn is_valid_repo_slug(slug: &str) -> bool {
    let Some((owner, repo)) = slug.split_once('/') else {
        return false;
    };
    let valid_part = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && !p.starts_with('.')
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    valid_part(owner) && valid_part(repo) && !repo.contains('/')
}

pub fn repo_slug() -> String {
    std::env::var("SELENCODE_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_repo_slug(s))
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// Apakah tag rilis adalah pre-release (mis. `v0.6.0-rc1`, `v0.6.0-beta.1`)?
pub fn is_prerelease_tag(s: &str) -> bool {
    let t = s.trim().strip_prefix('v').unwrap_or(s.trim());
    t.contains('-')
}

/// Validasi bahwa URL unduhan biner hanya memakai HTTPS pada host resmi GitHub Releases.
pub fn is_allowed_release_download_url(raw: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(raw) else {
        return false;
    };
    if u.scheme() != "https" {
        return false;
    }
    let Some(host) = u.host_str().map(|h| h.to_ascii_lowercase()) else {
        return false;
    };
    host == "github.com"
        || host == "objects.githubusercontent.com"
        || host == "release-assets.githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// Validasi header magic biner (ELF di Unix/Linux/Termux, PE/MZ di Windows) agar halaman HTML error
/// dari captive portal / proxy tidak pernah merusak executable `selen`.
pub fn has_valid_executable_magic(bytes: &[u8], target_os: &str) -> bool {
    if bytes.len() < 64 {
        return false;
    }
    match target_os {
        "windows" => bytes.starts_with(b"MZ"),
        "linux" | "android" => bytes.starts_with(b"\x7fELF"),
        "macos" => {
            bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
                || bytes.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
                || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
        }
        _ => true,
    }
}

/// SHA-256 murni Rust (FIPS 180-4) untuk verifikasi integritas biner `SHA256SUMS` tanpa dependensi tambahan.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(((data.len() + 9 + 63) / 64) * 64);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e; e = d.wrapping_add(t1);
            d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        for (slot, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

/// Parse baris `SHA256SUMS` (`<64-hex>  <filename>`) untuk mencari hash asset tertentu.
pub fn find_expected_sha256(sums_text: &str, asset_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?.trim().to_ascii_lowercase();
        let fname = parts.next()?.trim().trim_start_matches('*');
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && fname == asset_name {
            return Some(hash);
        }
    }
    None
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
    use std::io::Write;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    #[cfg(windows)]
    {
        let tmp = exe.with_extension(format!("tmp.{nonce}.exe"));
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("gagal membuat file sementara {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("gagal menulis file sementara {}", tmp.display()))?;
        }
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
        let tmp = exe.with_extension(format!("tmp.{nonce}"));
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o755);
            }
            let mut f = opts
                .open(&tmp)
                .with_context(|| format!("gagal membuat file sementara {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("gagal menulis file sementara {}", tmp.display()))?;
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

/// Ekstrak nama tag rilis (mis. `"v0.2.0"`) dari URL redirect GitHub
/// `https://github.com/<owner>/<repo>/releases/tag/<tag>`.
pub fn extract_tag_from_release_url(url: &str) -> Option<String> {
    let (_, tail) = url.rsplit_once("/releases/tag/")?;
    let tag = tail.split(['?', '#', '/']).next()?.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

/// Bangun URL unduhan langsung aset rilis GitHub (bebas batas rate-limit REST API).
pub fn direct_download_url(repo: &str, tag: &str, asset_name: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset_name}")
}

/// Deteksi tag rilis terbaru via redirect web `https://github.com/{repo}/releases/latest`
/// tanpa menyentuh `api.github.com` (tidak terkena 403 rate limit 60 req/jam di Wi-Fi/CGNAT).
async fn fetch_latest_tag_via_redirect(repo: &str) -> Result<String> {
    let no_redirect = reqwest::Client::builder()
        .user_agent("selencode")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("gagal membuat HTTP client redirect")?;
    let url = format!("https://github.com/{repo}/releases/latest");
    let resp = no_redirect
        .head(&url)
        .send()
        .await
        .context("gagal menghubungi GitHub — periksa koneksi internet (offline?)")?;
    if resp.status().is_redirection() {
        if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
            if let Ok(loc_str) = loc.to_str() {
                if let Some(tag) = extract_tag_from_release_url(loc_str) {
                    return Ok(tag);
                }
            }
        }
    }
    // Fallback GET jika proxy tidak mengizinkan HEAD redirect.
    let get_resp = no_redirect
        .get(&url)
        .send()
        .await
        .context("gagal memeriksa halaman rilis terbaru GitHub")?;
    if get_resp.status().is_redirection() {
        if let Some(loc) = get_resp.headers().get(reqwest::header::LOCATION) {
            if let Ok(loc_str) = loc.to_str() {
                if let Some(tag) = extract_tag_from_release_url(loc_str) {
                    return Ok(tag);
                }
            }
        }
    }
    anyhow::bail!("tidak menemukan redirect tag pada {url}")
}

/// Ambil `(tag_name, download_url)` dengan prioritas jalur bebas rate-limit (web redirect),
/// dan fallback ke GitHub REST API bila perlu.
async fn resolve_latest_release(
    client: &reqwest::Client,
    repo: &str,
    asset_name: &str,
) -> Result<(String, String)> {
    if let Ok(tag) = fetch_latest_tag_via_redirect(repo).await {
        let dl = direct_download_url(repo, &tag, asset_name);
        return Ok((tag, dl));
    }

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
    Ok((release.tag_name, asset.browser_download_url.clone()))
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
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("terlalu banyak redirect");
            }
            if is_allowed_release_download_url(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.error("redirect ke host/skema non-GitHub ditolak demi keamanan")
            }
        }))
        .build()
        .context("gagal membuat HTTP client")?;

    let (tag_name, download_url) = resolve_latest_release(&client, &repo, asset_name)
        .await
        .map_err(format_update_error)?;
    if !is_allowed_release_download_url(&download_url) {
        anyhow::bail!("URL unduhan rilis ditolak demi keamanan (bukan HTTPS GitHub resmi)");
    }
    if is_prerelease_tag(&tag_name) && !is_prerelease_tag(current_str) {
        println!("Rilis terbaru adalah pre-release ({tag_name}); tetap di versi stabil v{current_str}.");
        return Ok(());
    }

    let remote = parse_version(&tag_name)
        .ok_or_else(|| anyhow::anyhow!("tag rilis tidak dikenal: {tag_name}"))?;
    if !is_newer(remote, current) {
        println!("Sudah versi terbaru (v{current_str}).");
        return Ok(());
    }

    println!("Mengunduh SelenCode {tag_name} ({asset_name})…");
    let mut resp = client
        .get(&download_url)
        .send()
        .await
        .context("unduhan gagal — periksa koneksi internet")
        .map_err(format_update_error)?
        .error_for_status()
        .context("GitHub menolak unduhan aset")
        .map_err(format_update_error)?;

    const MAX_BINARY_BYTES: usize = 100 * 1024 * 1024;
    if let Some(cl) = resp.content_length() {
        if cl as usize > MAX_BINARY_BYTES {
            anyhow::bail!("ukuran biner rilis melebihi batas aman 100 MB");
        }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .context("unduhan terputus di tengah jalan")
        .map_err(format_update_error)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_BINARY_BYTES {
            anyhow::bail!("ukuran biner rilis melebihi batas aman 100 MB");
        }
        bytes.extend_from_slice(&chunk);
    }

    if !has_valid_executable_magic(&bytes, std::env::consts::OS) {
        anyhow::bail!(
            "unduhan ditolak: berkas bukan executable {} valid (mungkin halaman error/proxy)",
            std::env::consts::OS
        );
    }

    // Verifikasi SHA-256 otomatis bila aset `SHA256SUMS` tersedia di rilis (fail-closed bila tidak cocok!).
    let sums_url = direct_download_url(&repo, &tag_name, "SHA256SUMS");
    if let Ok(sum_resp) = client.get(&sums_url).send().await {
        if sum_resp.status().is_success() {
            if let Ok(sums_txt) = sum_resp.text().await {
                if let Some(expected) = find_expected_sha256(&sums_txt, asset_name) {
                    let actual = sha256_hex(&bytes);
                    if actual != expected {
                        anyhow::bail!(
                            "verifikasi SHA-256 GAGAL untuk {asset_name} (diharapkan {expected}, didapat {actual}) — pembaruan dibatalkan!"
                        );
                    }
                    println!("  SHA-256 terverifikasi: {}…", &actual[..16]);
                }
            }
        }
    }

    let exe = std::env::current_exe().context("tidak bisa menemukan lokasi binary selen")?;
    replace_binary(&exe, &bytes)?;
    println!("Diperbarui ke {tag_name} — jalankan `selen` lagi.");
    Ok(())
}

/// Live-test P4: sederhanakan rantai error `Caused by:` teknis menjadi 1 baris ramah user;
/// tampilkan rantai lengkap hanya bila `SELENCODE_DEBUG=1`.
pub fn format_update_error(err: anyhow::Error) -> anyhow::Error {
    let verbose = std::env::var("SELENCODE_DEBUG")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if verbose {
        return err;
    }
    let summary = err.to_string();
    let full = format!("{err:?}");
    let hint = if full.contains("UnknownIssuer") || full.contains("certificate") {
        " (sertifikat TLS/proxy jaringan ditolak — jalankan dari jaringan normal atau set SELENCODE_DEBUG=1)"
    } else if full.contains("dns") || full.contains("Connect") || full.contains("timed out") {
        " (set SELENCODE_DEBUG=1 untuk detail teknis)"
    } else {
        ""
    };
    anyhow::anyhow!("{summary}{hint}")
}
