//! Tools Skills System SelenCode (Progressive Disclosure 3 Tingkat):
//! - `skills_list`: daftar indeks ringkas skill (nama + deskripsi maks 60 char per kategori).
//! - `skill_view(name, path?)`: Tingkat 2 (isi penuh `SKILL.md` + daftar file referensi) &
//!   Tingkat 3 (muat 1 file referensi di dalam folder skill dengan validasi anti-traversal & anti-secret).
//! - `skill_create` & `skill_edit`: buat/ubah `SKILL.md` SELALU lewat persetujuan pengguna + diff!
//! - `manage_skill`: dipertahankan untuk kompatibilitas mundur & self-improvement review.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::skills::{
    format_skills_index, is_valid_kebab_case, list_skills, load_skill, load_skill_reference,
    load_skill_with_refs, parse_skill_frontmatter, patch_resolved_skill, render_skill_markdown,
    render_skill_markdown_with_origin, sanitize_skill_name, scan_skills_with_warnings,
    skill_file_path_for, upsert_skill,
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

/// Ambang karakter `SKILL.md` di atas mana `skill_view(name)` mengembalikan salinan kerja
/// prosedural yang padat (Perbaikan 1 v0.5.9) agar konteks tidak meledak 14,6 rb karakter.
/// Skill kecil (<= 4.500 char) tetap dikembalikan utuh apa adanya; isi penuh selalu bisa
/// dimuat kapan saja via `skill_view(name, full=true)`.
pub const SKILL_WORKING_COPY_THRESHOLD: usize = 4_500;
/// Batas karakter target untuk salinan kerja prosedural `skill_view` (~450 token; hemat >90% dari 14,6k char).
pub const SKILL_WORKING_COPY_MAX_CHARS: usize = 1_500;

/// Bangun salinan kerja prosedural ringkas untuk `SKILL.md` berukuran besar (> 4.500 char):
/// - Mempertahankan aturan inti (Iron Law, fase 1–4 prosedural, checklist kunci),
/// - Membuang contoh kode panjang, tabel rasionalisasi, dan prosa referensi yang bisa dimuat ulang,
/// - Menyertakan daftar judul bagian lengkap & petunjuk `skill_view(name, full=true)`.
pub fn build_skill_working_copy(skill_name: &str, raw_content: &str) -> String {
    if raw_content.chars().count() <= SKILL_WORKING_COPY_THRESHOLD {
        return raw_content.to_string();
    }
    let body = strip_frontmatter(raw_content);
    let mut all_headings: Vec<String> = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("## ") {
            let clean = h.trim();
            if !clean.is_empty() && !all_headings.iter().any(|x| x == clean) {
                all_headings.push(clean.to_string());
            }
        }
    }

    let mut out = String::new();
    let mut in_code_block = false;
    let mut code_lines_buf: Vec<&str> = Vec::new();
    let mut skip_section = false;
    let mut skip_sub = false;
    let mut section_bullets = 0usize;

    let push_if_fits = |dst: &mut String, text: &str| -> bool {
        if dst.chars().count() + text.chars().count() + 260 > SKILL_WORKING_COPY_MAX_CHARS {
            return false;
        }
        dst.push_str(text);
        dst.push('\n');
        true
    };

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("> Diadaptasi dari") || trimmed == "---" {
            continue;
        }
        if trimmed.starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                code_lines_buf.clear();
            } else {
                in_code_block = false;
                if !skip_section && !skip_sub && !code_lines_buf.is_empty() && code_lines_buf.len() <= 2 {
                    for cl in &code_lines_buf {
                        if !push_if_fits(&mut out, cl) {
                            break;
                        }
                    }
                }
                code_lines_buf.clear();
            }
            continue;
        }
        if in_code_block {
            if !trimmed.is_empty() {
                code_lines_buf.push(trimmed);
            }
            continue;
        }
        if trimmed.starts_with('|') {
            continue;
        }
        if let Some(h2) = trimmed.strip_prefix("## ") {
            let low = h2.to_ascii_lowercase();
            skip_section = low.contains("rationalization")
                || low.contains("real-world impact")
                || low.contains("integration")
                || low.contains("quick reference")
                || low.contains("when to use")
                || low.contains("example")
                || low.contains("red flags")
                || low.contains("the four phases");
            skip_sub = false;
            section_bullets = 0;
            if !skip_section && !push_if_fits(&mut out, trimmed) {
                break;
            }
            continue;
        }
        if skip_section {
            continue;
        }
        if trimmed.starts_with("# ") {
            let _ = push_if_fits(&mut out, trimmed);
            continue;
        }
        if let Some(h3) = trimmed.strip_prefix("### ") {
            let low = h3.to_ascii_lowercase();
            skip_sub = low.contains("checklist") || low.contains("when you don't know");
            section_bullets = 0;
            if !skip_sub && !push_if_fits(&mut out, trimmed) {
                // Jika ruang sempit, lanjut ke bagian ## berikutnya agar semua Phase tercakup.
                skip_sub = true;
            }
            continue;
        }
        if skip_sub || trimmed.is_empty() {
            continue;
        }
        let is_bullet = trimmed.starts_with("- ") || trimmed.starts_with("* ");
        let is_imperative = trimmed.starts_with("**Core")
            || trimmed.starts_with("**STOP")
            || trimmed.contains("NO FIXES");
        if (is_bullet || is_imperative) && section_bullets < 1 {
            section_bullets += 1;
            let short_line: String = if trimmed.chars().count() > 105 {
                format!("{}…", trimmed.chars().take(104).collect::<String>())
            } else {
                trimmed.to_string()
            };
            let _ = push_if_fits(&mut out, &short_line);
        }
    }

    if !all_headings.is_empty() {
        out.push_str("\nBagian: ");
        out.push_str(&all_headings.join(" | "));
        out.push('\n');
    }
    out.push_str(&format!(
        "(Salinan kerja ringkas dari {} char — muat ulang `skill_view(name=\"{skill_name}\")` juga mengembalikan salinan kerja ini; tambah `full=true` hanya bila benar-benar butuh teks penuh.)",
        raw_content.chars().count()
    ));
    out
}

/// Tingkat 2 & 3: muat isi `SKILL.md` (`path` kosong; salinan kerja ringkas untuk skill > 4.500 char baik pada
/// muatan pertama maupun muat-ulang kecuali `full=true`) ATAU 1 file referensi di dalam folder skill (`path` diisi).
pub struct SkillView;

#[async_trait]
impl Tool for SkillView {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill_view".into(),
            description: "Muat salinan kerja ringkas SKILL.md (termasuk saat muat ulang); set `full=true` hanya jika benar-benar butuh teks penuh, atau `path` untuk 1 file referensi.".into(),
            parameters: schema(
                json!({
                    "name": { "type": "string" },
                    "path": { "type": "string" },
                    "full": { "type": "boolean" }
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

        let want_full = args.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
        let (info, content, refs) = load_skill_with_refs(&ctx.cwd, name)?;
        let body = if want_full {
            content
        } else {
            build_skill_working_copy(&info.name, &content)
        };
        let scope_label = if info.shadows_user_store() {
            format!("{}; menimpa user", info.scope)
        } else {
            info.scope.to_string()
        };
        let mut out = format!(
            "[Skill `{}` ({scope_label}, kategori: {}) — {}]\n\n{body}",
            info.name,
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
        let existing_all = scan_skills_with_warnings(&ctx.cwd).skills;
        if existing_all
            .iter()
            .any(|s| s.scope == "project" && s.name == raw_name)
        {
            return Err(anyhow!(
                "skill proyek '{raw_name}' sudah ada — gunakan `skill_edit` untuk memperbaruinya"
            ));
        }
        let shadows_user = existing_all
            .iter()
            .find(|s| s.scope == "user" && s.name == raw_name);

        let target = skill_file_path_for(&ctx.cwd, raw_name, category)?;
        let md = render_skill_markdown(raw_name, desc, body, version, &tags)?;
        let rel_display = target
            .strip_prefix(&ctx.cwd)
            .unwrap_or(&target)
            .display()
            .to_string();
        let diff = build_unified_diff(&rel_display, "", &md);
        let approval_title = if shadows_user.is_some() {
            format!("skill_create (salinan proyek menimpa user) {rel_display}")
        } else {
            format!("skill_create {rel_display}")
        };

        // WAJIB lewat persetujuan pengguna (tidak pernah menulis skill diam-diam!).
        if !ctx.approver.ask(&approval_title, &diff).await {
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
        if let Some(u) = shadows_user {
            Ok(format!(
                "Skill proyek '{raw_name}' berhasil dibuat di {rel_display} (menimpa skill user di {}).",
                u.path.display()
            ))
        } else {
            Ok(format!(
                "Skill '{raw_name}' berhasil dibuat di {rel_display}."
            ))
        }
    }
}

/// Edit skill yang sudah ada (`SKILL.md` atau 1 file referensi di `references/`/`templates/`/`scripts/`) —
/// menampilkan unified diff & WAJIB meminta persetujuan pengguna!
pub struct SkillEdit;

#[async_trait]
impl Tool for SkillEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill_edit".into(),
            description: "Edit `SKILL.md` atau tulis/perbarui 1 file referensi (`ref_path`, mis. `references/audit.md`) di folder skill yang sudah ada. SELALU meminta persetujuan pengguna + diff sebelum menyimpan.".into(),
            parameters: schema(
                json!({
                    "name": {
                        "type": "string",
                        "description": "Nama skill kebab-case yang akan diubah"
                    },
                    "description": {
                        "type": "string",
                        "description": "Opsional: deskripsi baru satu baris (untuk SKILL.md)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Opsional: isi Markdown baru (untuk SKILL.md atau file `ref_path`)"
                    },
                    "ref_path": {
                        "type": "string",
                        "description": "Opsional: path relatif file referensi yang ingin ditulis/diperbarui di dalam folder skill (wajib di bawah references/, templates/, atau scripts/)"
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

        // Jalur khusus: menulis/memperbarui 1 file referensi di dalam folder skill (Perbaikan 3 v0.5.0).
        if let Some(raw_ref) = str_arg(&args, "ref_path").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let ref_norm = raw_ref.replace('\\', "/");
            let rel_path = std::path::Path::new(&ref_norm);
            if rel_path.is_absolute()
                || rel_path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(anyhow!(
                    "ref_path '{raw_ref}' ditolak: path harus relatif tanpa `..`"
                ));
            }
            if !(ref_norm.starts_with("references/")
                || ref_norm.starts_with("templates/")
                || ref_norm.starts_with("scripts/"))
            {
                return Err(anyhow!(
                    "ref_path '{raw_ref}' harus berada di bawah references/, templates/, atau scripts/"
                ));
            }
            if crate::security::is_secret_file(rel_path) {
                return Err(anyhow!(
                    "ref_path '{raw_ref}' ditolak: nama file rahasia tidak diizinkan"
                ));
            }
            let body = str_arg(&args, "body")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("argumen 'body' wajib diisi saat menulis ref_path"))?;
            let safe_body = format!("{}\n", crate::security::redact_secrets(body));

            // v0.5.27 (Masalah 1): edit selalu menulis kembali ke folder ASAL tempat skill dimuat
            // (store pengguna atau proyek), tidak pernah menciptakan salinan bayangan di proyek!
            let target = existing.dir.join(rel_path);
            let old_raw = std::fs::read_to_string(&target).unwrap_or_default();
            let rel_display = target
                .strip_prefix(&ctx.cwd)
                .unwrap_or(&target)
                .display()
                .to_string();
            let diff = build_unified_diff(&rel_display, &old_raw, &safe_body);

            if !ctx
                .approver
                .ask(&format!("skill_edit ({}) {rel_display}", existing.scope), &diff)
                .await
            {
                return Ok(format!(
                    "Ditolak user — penulisan referensi '{raw_ref}' dibatalkan."
                ));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("gagal membuat folder {}", parent.display()))?;
            }
            std::fs::write(&target, &safe_body)
                .with_context(|| format!("gagal menulis referensi {}", target.display()))?;
            return Ok(format!(
                "Referensi skill '{clean}' ({}, {raw_ref}) berhasil disimpan di {rel_display}.\n\n{diff}",
                existing.scope
            ));
        }

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

        // v0.5.27 (Masalah 1): edit WAJIB menargetkan berkas ASAL hasil resolusi pemuatan skill
        // (store + path asalnya), tidak pernah menciptakan salinan bayangan di `.selencode/skills/`.
        let target = existing.path.clone();
        // Pertahankan metadata provenans (`origin`, `source_session`, `learned_at`) bila ada.
        let new_md = render_skill_markdown_with_origin(
            &clean,
            &new_desc,
            &new_body,
            new_version.as_deref(),
            &new_tags,
            old_fm.as_ref().and_then(|f| f.origin.as_deref()),
            old_fm.as_ref().and_then(|f| f.source_session.as_deref()),
            old_fm.as_ref().and_then(|f| f.learned_at.as_deref()),
        )?;
        let rel_display = target
            .strip_prefix(&ctx.cwd)
            .unwrap_or(&target)
            .display()
            .to_string();
        let diff = build_unified_diff(&rel_display, &old_raw, &new_md);

        if !ctx
            .approver
            .ask(&format!("skill_edit ({}) {rel_display}", existing.scope), &diff)
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
            "Skill '{clean}' ({}) berhasil diperbarui di {rel_display}.\n\n{diff}",
            existing.scope
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
                let (path, clean, existed) = if action == "patch" {
                    let (p, c, _scope, ex) =
                        patch_resolved_skill(&ctx.cwd, name, desc, content)?;
                    (p, c, ex)
                } else {
                    upsert_skill(&ctx.cwd, name, desc, content)?
                };
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
