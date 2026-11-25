//! Tools Skills System ala Hermes Agent (Progressive Disclosure 3 Tingkat):
//! - `skills_list`: daftar indeks ringkas skill (nama + deskripsi maks 60 char per kategori).
//! - `skill_view(name, path?)`: Tingkat 2 (isi penuh `SKILL.md` + daftar file referensi) &
//!   Tingkat 3 (muat 1 file referensi di dalam folder skill dengan validasi anti-traversal & anti-secret).
//! - `skill_create` & `skill_edit`: buat/ubah `SKILL.md` SELALU lewat persetujuan pengguna + diff!
//! - `manage_skill`: dipertahankan untuk kompatibilitas mundur & self-improvement review.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::skills::{
    format_skills_index, is_valid_kebab_case, list_skills, load_skill, load_skill_reference,
    load_skill_with_refs, parse_skill_frontmatter, render_skill_markdown, sanitize_skill_name,
    scan_skills_with_warnings, skill_file_path_for, upsert_skill,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

/// Bangun ringkasan unified diff untuk ditampilkan di popup/prompt persetujuan.
fn build_unified_diff(title: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("diff {title}\n");
    let mut shown = 0usize;
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        if shown < 120 {
            out.push_str(sign);
            out.push_str(change.value());
            if !change.value().ends_with('\n') {
                out.push('\n');
            }
            shown += 1;
        }
    }
    out
}

fn extract_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Tingkat 1 (on-demand): kembalikan indeks ringkas skill yang sama seperti di snapshot prompt.
pub struct SkillsList;

#[async_trait]
impl Tool for SkillsList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skills_list".into(),
            description: "Tampilkan indeks ringkas semua skill (nama + deskripsi maks 60 char per kategori).".into(),
            parameters: schema(json!({}), &[]),
        }
    }

    async fn run(&self, _args: Value, ctx: &Ctx) -> Result<String> {
        let res = scan_skills_with_warnings(&ctx.cwd);
        if res.skills.is_empty() {
            return Ok("(belum ada skill terpasang di .selencode/skills atau folder pengguna)".into());
        }
        Ok(format_skills_index(&res.skills))
    }
}

/// Tingkat 2 & 3: muat isi penuh `SKILL.md` (`path` kosong) ATAU 1 file referensi di dalam folder skill (`path` diisi).
pub struct SkillView;

#[async_trait]
impl Tool for SkillView {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill_view".into(),
            description: "Muat isi penuh SKILL.md suatu skill, atau 1 file referensi (references/templates/scripts) jika `path` diisi.".into(),
            parameters: schema(
                json!({
                    "name": {
                        "type": "string",
                        "description": "Nama skill (kebab-case) dari indeks skills"
                    },
                    "path": {
                        "type": "string",
                        "description": "Opsional: path relatif 1 file referensi di dalam folder skill (mis. references/checklist-rust.md)"
                    }
                }),
                &["name"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let name = str_arg(&args, "name")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'name' wajib diisi"))?;

        if let Some(rel) = str_arg(&args, "path").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let (info, content) = load_skill_reference(&ctx.cwd, name, rel)?;
            return Ok(format!(
                "[Skill `{}` ({}) — referensi `{rel}`]\n\
                 (Catatan: file skrip tidak dieksekusi otomatis; jalankan lewat `run_shell` bila perlu.)\n\n\
                 {content}",
                info.name, info.scope
            ));
        }

        let (info, content, refs) = load_skill_with_refs(&ctx.cwd, name)?;
        let mut out = format!(
            "[Skill `{}` ({}, kategori: {}) — {}]\n\n{content}",
            info.name,
            info.scope,
            info.category,
            info.path.display()
        );
        if !refs.is_empty() {
            out.push_str("\n---\nFile referensi tersedia (muat satu per satu via `skill_view(name, path)` bila perlu):\n");
            for r in refs {
                out.push_str(&format!("- {r}\n"));
            }
        }
        Ok(out)
    }
}

/// Buat skill baru (`SKILL.md` ber-frontmatter YAML) di `.selencode/skills/` — WAJIB lewat persetujuan pengguna + diff!
pub struct SkillCreate;

#[async_trait]
impl Tool for SkillCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill_create".into(),
            description: "Buat skill baru (`SKILL.md`) di `.selencode/skills/`. SELALU meminta persetujuan pengguna + diff sebelum menulis.".into(),
            parameters: schema(
                json!({
                    "name": {
                        "type": "string",
                        "description": "Nama skill unik kebab-case (mis. deploy-vercel)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Deskripsi satu baris singkat (<=60 char di indeks) kapan skill ini dipakai"
                    },
                    "body": {
                        "type": "string",
                        "description": "Isi panduan Markdown langkah demi langkah yang reusable (tanpa secret)"
                    },
                    "category": {
                        "type": "string",
                        "description": "Opsional: subfolder kategori (mis. dev, deploy, ops)"
                    },
                    "version": {
                        "type": "string",
                        "description": "Opsional: versi SemVer (default 1.0.0)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Opsional: daftar tag kata kunci"
                    }
                }),
                &["name", "description", "body"],
            ),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let raw_name = str_arg(&args, "name")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'name' wajib diisi"))?;
        if !is_valid_kebab_case(raw_name) {
            return Err(anyhow!(
                "nama skill '{raw_name}' wajib format kebab-case huruf kecil (contoh: rilis-github)"
            ));
        }
        let desc = str_arg(&args, "description")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'description' wajib satu baris non-kosong"))?;
        let body = str_arg(&args, "body")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'body' wajib non-kosong"))?;
        let category = str_arg(&args, "category");
        let version = str_arg(&args, "version").or(Some("1.0.0"));
        let tags = extract_string_array(&args, "tags").unwrap_or_default();

        // Tolak jika skill proyek bernama sama sudah ada (arahkan ke `skill_edit`).
        if scan_skills_with_warnings(&ctx.cwd)
            .skills
            .iter()
            .any(|s| s.scope == "project" && s.name == raw_name)
        {
            return Err(anyhow!(
                "skill proyek '{raw_name}' sudah ada — gunakan `skill_edit` untuk memperbaruinya"
            ));
        }

        let target = skill_file_path_for(&ctx.cwd, raw_name, category)?;
        let md = render_skill_markdown(raw_name, desc, body, version, &tags)?;
        let rel_display = target
            .strip_prefix(&ctx.cwd)
            .unwrap_or(&target)
            .display()
            .to_string();
        let diff = build_unified_diff(&rel_display, "", &md);

        // WAJIB lewat persetujuan pengguna (tidak pernah menulis skill diam-diam!).
        if !ctx
            .approver
            .ask(&format!("skill_create {rel_display}"), &diff)
            .await
        {
            return Ok(format!(
                "Ditolak user — skill '{raw_name}' TIDAK dibuat."
            ));
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("gagal membuat folder skill {}", parent.display()))?;
        }
        std::fs::write(&target, &md)
            .with_context(|| format!("gagal menulis skill {}", target.display()))?;
        Ok(format!(
            "Skill '{raw_name}' berhasil dibuat di {rel_display}."
        ))
    }
}

/// Edit skill yang sudah ada — menampilkan unified diff & WAJIB meminta persetujuan pengguna!
pub struct SkillEdit;

#[async_trait]
impl Tool for SkillEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill_edit".into(),
            description: "Edit `SKILL.md` skill yang sudah ada. SELALU meminta persetujuan pengguna + diff sebelum menyimpan.".into(),
            parameters: schema(
                json!({
                    "name": {
                        "type": "string",
                        "description": "Nama skill kebab-case yang akan diubah"
                    },
                    "description": {
                        "type": "string",
                        "description": "Opsional: deskripsi baru satu baris"
                    },
                    "body": {
                        "type": "string",
                        "description": "Opsional: isi Markdown baru (tanpa frontmatter)"
                    },
                    "version": {
                        "type": "string",
                        "description": "Opsional: versi baru (mis. 1.1.0)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Opsional: daftar tag baru"
                    }
                }),
                &["name"],
            ),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let raw_name = str_arg(&args, "name")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'name' wajib diisi"))?;
        let clean = sanitize_skill_name(raw_name)?;

        let existing = scan_skills_with_warnings(&ctx.cwd)
            .skills
            .into_iter()
            .find(|s| s.name == clean)
            .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan untuk diedit"))?;

        let old_raw = std::fs::read_to_string(&existing.path)
            .with_context(|| format!("gagal membaca {}", existing.path.display()))?;
        let old_fm = parse_skill_frontmatter(&old_raw).ok();
        let old_body = strip_frontmatter(&old_raw);

        let new_desc = str_arg(&args, "description")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                old_fm
                    .as_ref()
                    .map(|f| f.description.clone())
                    .unwrap_or_else(|| existing.summary.clone())
            });
        let new_body = str_arg(&args, "body")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| old_body.trim().to_string());
        let new_version = str_arg(&args, "version")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| old_fm.as_ref().and_then(|f| f.version.clone()));
        let new_tags = extract_string_array(&args, "tags")
            .unwrap_or_else(|| old_fm.as_ref().map(|f| f.tags.clone()).unwrap_or_default());

        // Jika skill berasal dari folder pengguna, simpan salinan override di folder proyek agar aman.
        let target = if existing.scope == "project" {
            existing.path.clone()
        } else {
            skill_file_path_for(&ctx.cwd, &clean, Some(&existing.category))?
        };
        let new_md = render_skill_markdown(
            &clean,
            &new_desc,
            &new_body,
            new_version.as_deref(),
            &new_tags,
        )?;
        let rel_display = target
            .strip_prefix(&ctx.cwd)
            .unwrap_or(&target)
            .display()
            .to_string();
        let diff = build_unified_diff(&rel_display, &old_raw, &new_md);

        if !ctx
            .approver
            .ask(&format!("skill_edit {rel_display}"), &diff)
            .await
        {
            return Ok(format!(
                "Ditolak user — perubahan pada skill '{clean}' dibatalkan."
            ));
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("gagal membuat folder skill {}", parent.display()))?;
        }
        std::fs::write(&target, &new_md)
            .with_context(|| format!("gagal menyimpan skill {}", target.display()))?;
        Ok(format!(
            "Skill '{clean}' berhasil diperbarui di {rel_display}."
        ))
    }
}

fn strip_frontmatter(raw: &str) -> &str {
    let text = raw.trim_start_matches('\u{feff}');
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(after_first_nl) = rest.find('\n').map(|i| &rest[i + 1..]) {
            if let Some(end_idx) = after_first_nl.find("\n---") {
                let after = &after_first_nl[end_idx + 4..];
                return after.trim_start_matches(['\r', '\n']);
            }
        }
    }
    text
}

/// Tool kompatibilitas `manage_skill` (dipakai tes lama & review opsional).
pub struct ManageSkill;

#[async_trait]
impl Tool for ManageSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_skill".into(),
            description: "Kelola skill prosedural project di `.selencode/skills/` (list/read/create/patch).".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "create", "patch"],
                        "description": "aksi: list | read | create | patch"
                    },
                    "name": {
                        "type": "string",
                        "description": "nama skill kebab-case (mis. ryukomik-comic-scraper)"
                    },
                    "description": {
                        "type": "string",
                        "description": "ringkasan 1 baris kapan skill ini dipakai (untuk create/patch)"
                    },
                    "content": {
                        "type": "string",
                        "description": "isi prosedur Markdown langkah demi langkah, jebakan, & solusi (untuk create/patch)"
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let action = str_arg(&args, "action")
            .ok_or_else(|| anyhow!("argumen 'action' wajib (list|read|create|patch)"))?;
        match action {
            "list" => {
                let skills = list_skills(&ctx.cwd);
                if skills.is_empty() {
                    return Ok("(belum ada skill di .selencode/skills)".into());
                }
                Ok(format_skills_index(&skills))
            }
            "read" => {
                let name = str_arg(&args, "name")
                    .ok_or_else(|| anyhow!("argumen 'name' wajib untuk action='read'"))?;
                let (_info, content) = load_skill(&ctx.cwd, name)?;
                Ok(content)
            }
            "create" | "patch" => {
                let name = str_arg(&args, "name")
                    .ok_or_else(|| anyhow!("argumen 'name' wajib untuk action='{action}'"))?;
                let content = str_arg(&args, "content")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("argumen 'content' wajib non-kosong untuk action='{action}'"))?;
                let desc = str_arg(&args, "description");
                let (path, clean, existed) = upsert_skill(&ctx.cwd, name, desc, content)?;
                let verb = if existed || action == "patch" {
                    "patched"
                } else {
                    "created"
                };
                Ok(format!(
                    "Skill '{clean}' {verb} ({}).",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or(&clean)
                ))
            }
            other => Err(anyhow!(
                "action '{other}' tidak dikenal (pilihan: list, read, create, patch)"
            )),
        }
    }
}
