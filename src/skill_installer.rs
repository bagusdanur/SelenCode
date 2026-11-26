//! Pemasang skill dari URL langsung (`SKILL.md`) atau repositori Git (Perbaikan 8 v0.5.7).
//!
//! Alur aman 3 langkah:
//! 1. **Staging:** unduh/clone ke folder sementara di luar direktori skill pengguna.
//! 2. **Validasi + Ringkasan Persetujuan:** validasi frontmatter YAML (`name`, `description`),
//!    tolak path traversal / symlink / file rahasia / biner, tandai jelas setiap file skrip
//!    (skrip TIDAK pernah dijalankan otomatis).
//! 3. **Finalisasi (HANYA sesudah persetujuan):** tolak bentrok nama kecuali user memberi `--rename`,
//!    salin ke `<user_skills_dir>/installed/<nama>/`, catat asal-usul di `.selen-source.json`.
//!    `remove_installed_skill` hanya boleh mencopot skill hasil install di direktori user
//!    (menolak mencopot skill bawaan maupun skill proyek).

use crate::builtin_skills::BUILTIN_SKILL_NAMES;
use crate::security::is_secret_file;
use crate::skills::{
    is_valid_kebab_case, parse_skill_frontmatter, project_skills_dir, scan_skills_in_dirs,
    SkillFrontmatter,
};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const MAX_STAGED_FILE_BYTES: u64 = 256 * 1024;
const MAX_STAGED_FILES: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillProvenance {
    pub source_url: String,
    pub installed_at: String,
    pub original_name: String,
    pub installed_name: String,
}

#[derive(Debug, Clone)]
pub struct StagedFileEntry {
    pub rel_path: String,
    pub bytes: u64,
    pub is_script: bool,
}

#[derive(Debug, Clone)]
pub struct StagedSkill {
    pub source_url: String,
    pub staging_root: PathBuf,
    pub skill_dir: PathBuf,
    pub frontmatter: SkillFrontmatter,
    pub files: Vec<StagedFileEntry>,
}

impl Drop for StagedSkill {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.staging_root);
    }
}

#[derive(Debug, Clone)]
pub struct InstalledSkillReport {
    pub name: String,
    pub target_dir: PathBuf,
    pub files_copied: usize,
    pub scripts_count: usize,
    pub provenance_path: PathBuf,
}

fn is_script_rel_path(rel: &str) -> bool {
    let low = rel.to_ascii_lowercase();
    if low.starts_with("scripts/") {
        return true;
    }
    matches!(
        Path::new(&low).extension().and_then(|e| e.to_str()),
        Some("sh" | "bash" | "zsh" | "py" | "js" | "ts" | "ps1" | "bat" | "cmd" | "rb" | "pl")
    )
}

/// Validasi direktori skill hasil unduh/clone di staging (tidak menyentuh direktori user!).
pub fn validate_staged_skill_dir(
    staging_root: PathBuf,
    skill_dir: PathBuf,
    source_url: &str,
) -> Result<StagedSkill> {
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.is_file() {
        return Err(anyhow!(
            "sumber '{}' tidak mengandung file `SKILL.md` yang valid",
            source_url
        ));
    }
    let raw_md = std::fs::read_to_string(&skill_md)
        .with_context(|| format!("gagal membaca {}", skill_md.display()))?;
    let frontmatter = parse_skill_frontmatter(&raw_md)
        .map_err(|e| anyhow!("frontmatter SKILL.md tidak valid: {e}"))?;

    let mut files = Vec::new();
    collect_and_validate_files(&skill_dir, &skill_dir, &mut files)?;
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    Ok(StagedSkill {
        source_url: source_url.to_string(),
        staging_root,
        skill_dir,
        frontmatter,
        files,
    })
}

fn collect_and_validate_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<StagedFileEntry>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        if fname_str == ".git" {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(anyhow!(
                "ditolak demi keamanan: symlink terdeteksi pada '{}'",
                path.display()
            ));
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| anyhow!("path keluar dari direktori skill staging"))?;
        for c in rel.components() {
            if !matches!(c, Component::Normal(_)) {
                return Err(anyhow!(
                    "path traversal terdeteksi pada '{}'",
                    rel.display()
                ));
            }
        }
        if meta.is_dir() {
            collect_and_validate_files(root, &path, out)?;
        } else if meta.is_file() {
            if out.len() >= MAX_STAGED_FILES {
                return Err(anyhow!(
                    "terlalu banyak berkas dalam 1 skill (maks {MAX_STAGED_FILES})"
                ));
            }
            if is_secret_file(&path) {
                return Err(anyhow!(
                    "ditolak demi keamanan: berkas rahasia '{}' ditemukan dalam paket skill",
                    rel.display()
                ));
            }
            if meta.len() > MAX_STAGED_FILE_BYTES {
                return Err(anyhow!(
                    "berkas '{}' terlalu besar ({} byte > {MAX_STAGED_FILE_BYTES})",
                    rel.display(),
                    meta.len()
                ));
            }
            let raw_bytes = std::fs::read(&path)?;
            if raw_bytes.contains(&0) {
                return Err(anyhow!(
                    "ditolak demi keamanan: berkas biner '{}' tidak diizinkan dalam paket skill",
                    rel.display()
                ));
            }
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            out.push(StagedFileEntry {
                is_script: is_script_rel_path(&rel_str),
                rel_path: rel_str,
                bytes: meta.len(),
            });
        }
    }
    Ok(())
}

/// Pecah URL Git + subpath opsional:
/// - `https://github.com/owner/repo/tree/main/skills/my-skill` -> `(repo_url, Some(branch), Some(subpath))`
/// - `https://example.com/repo.git#sub/dir` -> `(repo_url, None, Some(subpath))`
/// - `file:///...` atau path repo lokal untuk tes -> `(url, None, subpath)`
fn parse_git_source(source: &str) -> (String, Option<String>, Option<String>) {
    let trimmed = source.trim();
    if let Some((base, frag)) = trimmed.split_once('#') {
        let sub = frag.trim().trim_matches('/');
        return (
            base.to_string(),
            None,
            if sub.is_empty() {
                None
            } else {
                Some(sub.to_string())
            },
        );
    }
    if let Some(pos) = trimmed.find("/tree/") {
        let base = &trimmed[..pos];
        let rest = &trimmed[pos + 6..];
        let mut parts = rest.splitn(2, '/');
        let branch = parts.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        let sub = parts
            .next()
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty());
        return (format!("{base}.git"), branch, sub);
    }
    (trimmed.to_string(), None, None)
}

fn is_direct_skill_md_url(source: &str) -> bool {
    let low = source.trim().to_ascii_lowercase();
    let without_query = low.split('?').next().unwrap_or(&low);
    (low.starts_with("http://") || low.starts_with("https://"))
        && (without_query.ends_with("/skill.md") || without_query.ends_with(".md"))
}

/// Unduh / clone skill dari URL `SKILL.md` langsung atau URL repositori Git ke area staging sementara.
pub async fn stage_skill_from_source(source: &str) -> Result<StagedSkill> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("URL atau sumber skill tidak boleh kosong"));
    }
    let staging_root = std::env::temp_dir().join(format!(
        "selen-skill-stage-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&staging_root)?;

    if is_direct_skill_md_url(trimmed) {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client
            .get(trimmed)
            .send()
            .await
            .with_context(|| format!("gagal mengunduh '{trimmed}'"))?;
        if !resp.status().is_success() {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(anyhow!(
                "gagal mengunduh '{}': HTTP {}",
                trimmed,
                resp.status()
            ));
        }
        let body = resp.text().await?;
        if body.len() as u64 > MAX_STAGED_FILE_BYTES {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(anyhow!("file SKILL.md melebihi batas 256 KB"));
        }
        let skill_dir = staging_root.join("skill");
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(skill_dir.join("SKILL.md"), body)?;
        return validate_staged_skill_dir(staging_root, skill_dir, trimmed);
    }

    // Mode Git clone shallow (`--depth 1`) ke staging.
    let (repo_url, branch, subpath) = parse_git_source(trimmed);
    let clone_dir = staging_root.join("repo");
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if let Some(br) = &branch {
        cmd.arg("--branch").arg(br);
    }
    cmd.arg(&repo_url).arg(&clone_dir);
    let out = cmd
        .output()
        .with_context(|| format!("gagal menjalankan `git clone` untuk '{repo_url}'"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(anyhow!("git clone gagal untuk '{}': {}", trimmed, err.trim()));
    }

    let skill_dir = if let Some(sub) = subpath {
        // Tolak path traversal pada subpath URL!
        let rel = Path::new(&sub);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(anyhow!("subpath '{sub}' ditolak: terdeteksi path traversal"));
        }
        clone_dir.join(rel)
    } else if clone_dir.join("SKILL.md").is_file() {
        clone_dir
    } else {
        // Cari tepat 1 folder skill bila SKILL.md berada 1–2 tingkat di dalam repo.
        let mut found = Vec::new();
        find_skill_md_dirs(&clone_dir, 0, &mut found);
        if found.len() == 1 {
            found.remove(0)
        } else if found.is_empty() {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(anyhow!(
                "tidak ditemukan `SKILL.md` pada repositori '{trimmed}'"
            ));
        } else {
            let names: Vec<String> = found
                .iter()
                .filter_map(|p| p.strip_prefix(&clone_dir).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .take(8)
                .collect();
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(anyhow!(
                "repositori memuat {} skill ({}). Tentukan subpath dengan `#<path>` (mis. `{}#{}`).",
                found.len(),
                names.join(", "),
                repo_url,
                names.first().cloned().unwrap_or_default()
            ));
        }
    };

    validate_staged_skill_dir(staging_root, skill_dir, trimmed)
}

fn find_skill_md_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if p.is_dir() {
            if p.join("SKILL.md").is_file() {
                out.push(p);
            } else {
                find_skill_md_dirs(&p, depth + 1, out);
            }
        }
    }
}

/// Format ringkasan persetujuan pemasangan skill untuk ditampilkan ke pengguna SEBELUM menyalin.
pub fn format_install_approval_summary(staged: &StagedSkill, rename: Option<&str>) -> String {
    let target_name = rename.unwrap_or(&staged.frontmatter.name);
    let scripts: Vec<&StagedFileEntry> = staged.files.iter().filter(|f| f.is_script).collect();
    let mut out = format!(
        "Pasang skill '{}' dari {}\n\
         Deskripsi: {}\n\
         Total berkas: {} (skrip: {})\n",
        target_name,
        staged.source_url,
        staged.frontmatter.description,
        staged.files.len(),
        scripts.len()
    );
    for f in &staged.files {
        let tag = if f.is_script {
            " [SKRIP — tidak dijalankan otomatis]"
        } else {
            ""
        };
        out.push_str(&format!("  - {} ({} byte){tag}\n", f.rel_path, f.bytes));
    }
    if !scripts.is_empty() {
        out.push_str(
            "Peringatan: paket memuat berkas skrip. Skrip TIDAK pernah dijalankan otomatis oleh SelenCode.\n",
        );
    }
    out.trim_end().to_string()
}

/// Selesaikan pemasangan dari area staging ke direktori skill pengguna (`user_root`) SESUDAH persetujuan.
/// Menolak bentrok nama dengan skill bawaan maupun skill pengguna lain kecuali user memberi `rename`.
pub fn finalize_staged_skill_install(
    staged: &StagedSkill,
    user_root: &Path,
    rename: Option<&str>,
) -> Result<InstalledSkillReport> {
    let target_name = match rename.map(str::trim).filter(|s| !s.is_empty()) {
        Some(custom) => {
            if !is_valid_kebab_case(custom) {
                return Err(anyhow!(
                    "nama ganti '{custom}' tidak valid (wajib kebab-case huruf kecil)"
                ));
            }
            custom.to_string()
        }
        None => staged.frontmatter.name.clone(),
    };

    // Cek bentrok nama terhadap skill bawaan dan seluruh skill yang sudah ada di user_root.
    if BUILTIN_SKILL_NAMES.contains(&target_name.as_str()) {
        return Err(anyhow!(
            "bentrok nama: '{}' adalah skill bawaan resmi SelenCode. Gunakan `--rename <nama-baru>` agar tidak menimpa bawaan.",
            target_name
        ));
    }
    let existing = scan_skills_in_dirs(None, Some(user_root));
    if existing.skills.iter().any(|s| s.name == target_name) {
        return Err(anyhow!(
            "bentrok nama: skill '{}' sudah terpasang di direktori pengguna. Tidak menimpa diam-diam — gunakan `--rename <nama-baru>` atau copot dulu dengan `selen skills remove {}`.",
            target_name,
            target_name
        ));
    }

    let target_dir = user_root.join("installed").join(&target_name);
    if target_dir.exists() {
        return Err(anyhow!(
            "folder tujuan '{}' sudah ada — gunakan `--rename <nama-baru>`",
            target_dir.display()
        ));
    }
    std::fs::create_dir_all(&target_dir)?;

    let mut scripts_count = 0usize;
    for f in &staged.files {
        let src = staged.skill_dir.join(&f.rel_path);
        let dst = target_dir.join(&f.rel_path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if f.rel_path == "SKILL.md" && target_name != staged.frontmatter.name {
            // Perbarui field `name:` pada frontmatter agar cocok dengan nama hasil `--rename`.
            let raw = std::fs::read_to_string(&src)?;
            let updated = replace_frontmatter_name(&raw, &staged.frontmatter.name, &target_name);
            std::fs::write(&dst, updated)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
        if f.is_script {
            scripts_count += 1;
        }
    }

    let prov = SkillProvenance {
        source_url: staged.source_url.clone(),
        installed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        original_name: staged.frontmatter.name.clone(),
        installed_name: target_name.clone(),
    };
    let prov_path = target_dir.join(".selen-source.json");
    std::fs::write(&prov_path, serde_json::to_string_pretty(&prov)?)?;

    Ok(InstalledSkillReport {
        name: target_name,
        target_dir,
        files_copied: staged.files.len(),
        scripts_count,
        provenance_path: prov_path,
    })
}

fn replace_frontmatter_name(raw: &str, old_name: &str, new_name: &str) -> String {
    let mut done = false;
    let mut lines = Vec::new();
    for line in raw.lines() {
        if !done && line.trim_start().starts_with("name:") && line.contains(old_name) {
            lines.push(format!("name: {new_name}"));
            done = true;
        } else {
            lines.push(line.to_string());
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// Copot skill hasil pemasangan (`selen skills remove <nama>`).
/// Ditolak untuk skill bawaan resmi maupun skill lokal proyek (`.selencode/skills/`).
pub fn remove_installed_skill(name: &str, user_root: &Path, cwd: &Path) -> Result<String> {
    let clean = name.trim();
    if !is_valid_kebab_case(clean) {
        return Err(anyhow!("nama skill tidak valid: '{clean}'"));
    }
    if BUILTIN_SKILL_NAMES.contains(&clean) {
        return Err(anyhow!(
            "skill '{}' adalah skill bawaan resmi SelenCode dan tidak dapat dicopot dengan `skills remove`.",
            clean
        ));
    }
    let proj_dir = project_skills_dir(cwd);
    let proj_scan = scan_skills_in_dirs(Some(&proj_dir), None);
    let user_scan = scan_skills_in_dirs(None, Some(user_root));
    let Some(target) = user_scan.skills.into_iter().find(|s| s.name == clean) else {
        if proj_scan.skills.iter().any(|s| s.name == clean) {
            return Err(anyhow!(
                "skill '{}' adalah skill lokal proyek (.selencode/skills/), bukan skill hasil install pengguna — tidak dicopot.",
                clean
            ));
        }
        return Err(anyhow!(
            "skill hasil install '{}' tidak ditemukan di direktori pengguna.",
            clean
        ));
    };
    if !target.dir.join(".selen-source.json").is_file()
        && !target.dir.starts_with(user_root.join("installed"))
    {
        return Err(anyhow!(
            "skill '{}' bukan hasil `skills install` (tidak memiliki catatan asal-usul `.selen-source.json`) — hapus manual bila memang diinginkan.",
            clean
        ));
    }
    std::fs::remove_dir_all(&target.dir)
        .with_context(|| format!("gagal menghapus folder skill {}", target.dir.display()))?;
    Ok(format!(
        "Skill '{}' berhasil dicopot dari {}",
        clean,
        target.dir.display()
    ))
}
