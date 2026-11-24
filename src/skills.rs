//! Skills, `/init` (generator AGENTS.md), dan `/export` (ekspor percakapan ke Markdown).
//!
//! - Skills dibaca dari `.selencode/skills/*.md` (per project) dan
//!   `~/.config/selencode/skills/*.md` (global), terinspirasi Hermes Agent.
//! - `/init` memindai struktur project dan membuat `AGENTS.md` awal (ala OpenCode).
//! - `/export` menyimpan riwayat percakapan aktif sebagai file Markdown bersih.

use crate::providers::{Block, Message, Role};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub summary: String,
    pub path: PathBuf,
    pub scope: &'static str,
}

pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".selencode").join("skills")
}

pub fn global_skills_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("selencode").join("skills"))
}

fn scan_skills_in(dir: &Path, scope: &'static str, out: &mut Vec<SkillInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let summary = text
            .lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty() && *l != "---")
            .unwrap_or("(tanpa deskripsi)")
            .chars()
            .take(72)
            .collect();
        out.push(SkillInfo {
            name: stem.to_string(),
            summary,
            path: p,
            scope,
        });
    }
}

/// Temukan semua skill di folder project & global (project didahulukan bila nama sama).
pub fn list_skills(cwd: &Path) -> Vec<SkillInfo> {
    let mut out = Vec::new();
    scan_skills_in(&project_skills_dir(cwd), "project", &mut out);
    if let Some(g) = global_skills_dir() {
        let mut globals = Vec::new();
        scan_skills_in(&g, "global", &mut globals);
        for s in globals {
            if !out.iter().any(|x| x.name == s.name) {
                out.push(s);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Muat isi file skill berdasarkan nama.
pub fn load_skill(cwd: &Path, name: &str) -> Result<(SkillInfo, String)> {
    let clean = sanitize_skill_name(name)?;
    let all = list_skills(cwd);
    let info = all
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(&clean))
        .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan. Cek daftar lewat `/skills`."))?;
    let content = std::fs::read_to_string(&info.path)
        .with_context(|| format!("gagal membaca skill {}", info.path.display()))?;
    Ok((info, content))
}

/// Simpan atau perbarui (patch) skill ke `.selencode/skills/<nama>.md`.
/// Mengembalikan `(path, is_patch)` di mana `is_patch = true` bila skill sudah
/// ada sebelumnya dan diperbarui. Konten diredaksi dari pola rahasia sebelum ditulis.
pub fn upsert_skill(
    cwd: &Path,
    name: &str,
    description: Option<&str>,
    instruction: &str,
) -> Result<(PathBuf, String, bool)> {
    let clean = sanitize_skill_name(name)?;
    let dir = project_skills_dir(cwd);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("gagal membuat folder skill {}", dir.display()))?;
    let path = dir.join(format!("{clean}.md"));
    let existed = path.exists();
    let safe_body = crate::security::redact_secrets(instruction.trim());
    let body = match description.map(|d| d.trim()).filter(|d| !d.is_empty()) {
        Some(desc) => {
            let safe_desc = crate::security::redact_secrets(desc);
            format!("# {clean}\n\n> {safe_desc}\n\n{safe_body}\n")
        }
        None => format!("# Skill: {clean}\n\n{safe_body}\n"),
    };
    std::fs::write(&path, body)
        .with_context(|| format!("gagal menyimpan skill {}", path.display()))?;
    Ok((path, clean, existed))
}

/// Simpan skill baru ke `.selencode/skills/<nama>.md` (dipakai `/learn`).
pub fn save_skill(cwd: &Path, name: &str, instruction: &str) -> Result<PathBuf> {
    let (path, _, _) = upsert_skill(cwd, name, None, instruction)?;
    Ok(path)
}

/// Hapus skill project berdasarkan nama.
pub fn remove_skill(cwd: &Path, name: &str) -> Result<bool> {
    let clean = sanitize_skill_name(name)?;
    let path = project_skills_dir(cwd).join(format!("{clean}.md"));
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)
        .with_context(|| format!("gagal menghapus skill {}", path.display()))?;
    Ok(true)
}

fn sanitize_skill_name(raw: &str) -> Result<String> {
    let s: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        return Err(anyhow!("nama skill tidak boleh kosong"));
    }
    Ok(s)
}

/// `/init`: buat starter `AGENTS.md` sesuai stack project bila belum ada.
pub fn init_agents_md(cwd: &Path) -> Result<(PathBuf, bool)> {
    let path = cwd.join("AGENTS.md");
    if path.exists() {
        return Ok((path, false));
    }
    let mut stacks: Vec<&str> = Vec::new();
    let mut cmds: Vec<&str> = Vec::new();
    if cwd.join("Cargo.toml").exists() {
        stacks.push("Rust (Cargo)");
        cmds.push("- Test: `cargo test`");
        cmds.push("- Build: `cargo build --release`");
    }
    if cwd.join("package.json").exists() {
        stacks.push("Node.js / JavaScript / TypeScript");
        cmds.push("- Test: `npm test`");
        cmds.push("- Build: `npm run build`");
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("requirements.txt").exists() {
        stacks.push("Python");
        cmds.push("- Test: `pytest`");
    }
    if cwd.join("go.mod").exists() {
        stacks.push("Go");
        cmds.push("- Test: `go test ./...`");
    }

    let stack_line = if stacks.is_empty() {
        "Umum (isi stack project kamu di sini)".to_string()
    } else {
        stacks.join(", ")
    };
    let cmds_block = if cmds.is_empty() {
        "- Tambahkan perintah build/test utama project di sini.".to_string()
    } else {
        cmds.join("\n")
    };

    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let content = format!(
        "# AGENTS.md — {project_name}\n\n\
         Panduan kerja untuk Selena (SelenCode) di repository ini.\n\n\
         ## Stack\n\
         - {stack_line}\n\n\
         ## Perintah Penting\n\
         {cmds_block}\n\n\
         ## Aturan Perubahan\n\
         - Baca struktur kode (`outline` / `read_file`) sebelum mengedit.\n\
         - Buat perubahan seminimal mungkin dan jalankan test setelah selesai.\n\
         - Jangan pernah membocorkan secret atau mengubah `.env`.\n"
    );
    std::fs::write(&path, content)
        .with_context(|| format!("gagal menulis {}", path.display()))?;
    Ok((path, true))
}

/// `/export`: ekspor riwayat pesan sesi menjadi file Markdown bersih di `cwd`.
pub fn export_conversation_md(
    cwd: &Path,
    filename: Option<&str>,
    provider: &str,
    model: &str,
    messages: &[Message],
) -> Result<PathBuf> {
    let rel = match filename.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(name) => {
            if name.ends_with(".md") {
                name.to_string()
            } else {
                format!("{name}.md")
            }
        }
        None => format!(
            "selencode-export-{}.md",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        ),
    };
    let path = crate::security::resolve_in_sandbox(cwd, Path::new(&rel))?;
    if crate::security::is_secret_file(&path) {
        return Err(anyhow!("nama file ekspor '{rel}' ditolak"));
    }
    let mut md = format!(
        "# SelenCode Export\n\n\
         - **Provider:** `{provider}`\n\
         - **Model:** `{model}`\n\
         - **Waktu (UTC):** {}\n\n---\n\n",
        chrono::Utc::now().to_rfc3339()
    );
    for m in messages {
        match m.role {
            Role::System => continue,
            Role::User => {
                md.push_str("## User\n\n");
                md.push_str(&m.plain_text());
                md.push_str("\n\n");
            }
            Role::Assistant => {
                md.push_str("## Selena\n\n");
                for b in &m.content {
                    match b {
                        Block::Text(t) if !t.trim().is_empty() => {
                            md.push_str(t);
                            md.push_str("\n\n");
                        }
                        Block::ToolUse { name, args, .. } => {
                            md.push_str(&format!("- *Tool call:* `{name}` `{args}`\n"));
                        }
                        _ => {}
                    }
                }
                md.push('\n');
            }
            Role::Tool => {
                for b in &m.content {
                    if let Block::ToolResult {
                        content, is_error, ..
                    } = b
                    {
                        let first = content.lines().next().unwrap_or("");
                        let tag = if *is_error { "error" } else { "ok" };
                        let short: String = first.chars().take(120).collect();
                        md.push_str(&format!("> Tool ({tag}): `{short}`\n\n"));
                    }
                }
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, md)
        .with_context(|| format!("gagal menulis file ekspor {}", path.display()))?;
    Ok(path)
}
