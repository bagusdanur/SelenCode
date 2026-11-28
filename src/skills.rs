//! Skills System SelenCode (Progressive Disclosure 3 Tingkat),
//! `/init` (generator AGENTS.md), dan `/export` (ekspor percakapan ke Markdown).
//!
//! Arsitektur Skills v0.4.0:
//! - Format utama: 1 folder = 1 skill berisi `SKILL.md` ber-frontmatter YAML wajib
//!   (`name` kebab-case unik, `description` 1 baris, plus opsional `version`, `tags`)
//!   serta subfolder referensi opsional (`references/`, `templates/`, `scripts/`).
//! - Dua lokasi dengan prioritas jelas:
//!   1. PROYEK: `<proyek>/.selencode/skills/[<kategori>/]<nama>/SKILL.md` (menang bila nama sama)
//!   2. PENGGUNA: `<data_dir>/selencode/skills/[<kategori>/]<nama>/SKILL.md`
//! - Folder/frontmatter rusak dilewati dengan peringatan, TIDAK pernah menggagalkan startup.
//! - Progressive disclosure 3 tingkat:
//!   - Tingkat 1: indeks ringkas (`name` + `description` maks 60 char, dikelompokkan per kategori)
//!     di snapshot dinamis awal sesi.
//!   - Tingkat 2: isi penuh `SKILL.md` dimuat on-demand via `skill_view(name)`.
//!   - Tingkat 3: satu file referensi di dalam folder skill dimuat via `skill_view(name, path)`
//!     dengan validasi anti-traversal & penolakan file rahasia.

use crate::providers::{Block, Message, Role};
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Panjang maksimum deskripsi skill di indeks prompt / `skills_list` (disiplin token Tahap 2).
pub const SKILL_INDEX_DESC_MAX_CHARS: usize = 60;
/// Penanda asal frontmatter untuk skill yang dibuat/dikelola Selena improvement (v0.5.24).
pub const IMPROVEMENT_SKILL_ORIGIN: &str = "selena-improvement";
/// Nama subfolder fisik di dalam direktori skill pengguna untuk skill baru karya Selena improvement.
pub const IMPROVEMENT_SKILLS_SUBDIR: &str = "improvement";

/// Metadata frontmatter YAML hasil parsing `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub tags: Vec<String>,
    /// Penanda asal penulisan (mis. `"selena-improvement"` untuk skill karya review; `None` = karya pengguna/bawaan).
    pub origin: Option<String>,
    /// ID sesi sumber saat skill dibuat/dipelajari oleh Selena improvement (opsional).
    pub source_session: Option<String>,
    /// Tanggal pembelajaran (YYYY-MM-DD, opsional).
    pub learned_at: Option<String>,
}

/// Detail salinan skill di store prioritas lebih rendah yang tertimpa oleh skill bernama sama
/// di store prioritas lebih tinggi (mis. salinan `project` menimpa skill `user`, v0.5.27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedSkillInfo {
    /// Store yang tertimpa (mis. `"user"`).
    pub scope: &'static str,
    pub category: String,
    pub path: PathBuf,
    pub is_improvement: bool,
}

/// Informasi 1 skill yang terindeks dari disk.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    /// Deskripsi 1 baris utuh (dipotong maks 60 char saat dirender di indeks).
    pub summary: String,
    /// Nama subfolder kategori (default `"umum"` bila langsung di root skills).
    pub category: String,
    pub version: Option<String>,
    pub tags: Vec<String>,
    /// Path menuju file `SKILL.md` (atau file `.md` lama).
    pub path: PathBuf,
    /// Direktori induk folder skill (untuk memuat `references/`, `templates/`, `scripts/`).
    pub dir: PathBuf,
    /// `"project"` (prioritas #1) atau `"user"` (prioritas #2).
    pub scope: &'static str,
    /// Penanda asal penulisan (`Some("selena-improvement")` untuk skill karya Selena improvement).
    pub origin: Option<String>,
    /// Sesi asal pembelajaran (bila ada).
    pub source_session: Option<String>,
    /// Tanggal pembelajaran (bila ada).
    pub learned_at: Option<String>,
    /// Daftar salinan di store prioritas lebih rendah yang tertimpa oleh skill aktif ini
    /// (keterlihatan bayangan v0.5.27 — tidak ada bayangan senyap lagi).
    pub shadows: Vec<ShadowedSkillInfo>,
}

impl SkillInfo {
    /// True bila skill ini berasal dari Selena improvement
    /// (frontmatter `origin: selena-improvement` atau subfolder `skills/improvement/`).
    pub fn is_improvement_skill(&self) -> bool {
        if self
            .origin
            .as_deref()
            .map(|o| o.eq_ignore_ascii_case(IMPROVEMENT_SKILL_ORIGIN))
            .unwrap_or(false)
        {
            return true;
        }
        self.category == IMPROVEMENT_SKILLS_SUBDIR
            || self
                .category
                .starts_with(&format!("{IMPROVEMENT_SKILLS_SUBDIR}/"))
    }

    /// True bila skill proyek ini menimpa skill bernama sama yang ada di store pengguna/global.
    pub fn shadows_user_store(&self) -> bool {
        self.scope == "project" && self.shadows.iter().any(|sh| sh.scope == "user")
    }
}

/// Hasil pemindaian skill beserta daftar peringatan folder/frontmatter yang dilewati.
#[derive(Debug, Default, Clone)]
pub struct SkillScanResult {
    pub skills: Vec<SkillInfo>,
    pub warnings: Vec<String>,
}

pub fn project_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".selencode").join("skills")
}

/// Lokasi skill PENGGUNA (`<data_dir>/selencode/skills/`), menghormati `SELENCODE_DATA_DIR`.
pub fn user_skills_dir() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("SELENCODE_DATA_DIR") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("selencode").join("skills"));
        }
    }
    dirs::data_dir().map(|d| d.join("selencode").join("skills"))
}

/// Alias lama untuk kompatibilitas mundur.
pub fn global_skills_dir() -> Option<PathBuf> {
    user_skills_dir()
}

/// Fallback baca lokasi config lama (`~/.config/selencode/skills/`) bila berbeda dari `user_skills_dir`.
fn legacy_config_skills_dir() -> Option<PathBuf> {
    let cfg = dirs::config_dir()?.join("selencode").join("skills");
    if user_skills_dir().as_ref() == Some(&cfg) {
        None
    } else {
        Some(cfg)
    }
}

/// Validasi format kebab-case ketat (`a-z`, `0-9`, dipisah tunggal `-`).
pub fn is_valid_kebab_case(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parse frontmatter YAML `SKILL.md` (`--- ... ---` di baris awal file).
/// Mengembalikan `Err` yang jelas bila frontmatter tidak ada/rusak, `name` bukan kebab-case,
/// atau `description` kosong / lebih dari satu baris.
pub fn parse_skill_frontmatter(raw: &str) -> Result<SkillFrontmatter> {
    let text = raw.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    let first = lines.next().map(|l| l.trim()).unwrap_or("");
    if first != "---" {
        return Err(anyhow!("frontmatter YAML wajib diawali baris `---`"));
    }

    let mut fm_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line);
    }
    if !closed {
        return Err(anyhow!("frontmatter YAML tidak ditutup baris `---`"));
    }

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut version: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut origin: Option<String> = None;
    let mut source_session: Option<String> = None;
    let mut learned_at: Option<String> = None;
    let mut in_tags_list = false;

    for raw_line in fm_lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_tags_list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let cleaned = unquote_yaml(item.trim());
                if !cleaned.is_empty() {
                    tags.push(cleaned);
                }
                continue;
            } else {
                in_tags_list = false;
            }
        }
        let Some((k, v)) = trimmed.split_once(':') else {
            return Err(anyhow!("baris frontmatter tidak valid: `{trimmed}`"));
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim();
        match key.as_str() {
            "name" => {
                let n = unquote_yaml(val);
                if !is_valid_kebab_case(&n) {
                    return Err(anyhow!(
                        "field `name` wajib kebab-case huruf kecil (dapat: `{n}`)"
                    ));
                }
                name = Some(n);
            }
            "description" => {
                if matches!(val, "|" | ">" | "|-" | ">-") {
                    return Err(anyhow!("field `description` wajib satu baris, bukan blok multiline"));
                }
                let d = unquote_yaml(val);
                if d.is_empty() {
                    return Err(anyhow!("field `description` tidak boleh kosong"));
                }
                description = Some(d);
            }
            "version" => {
                let ver = unquote_yaml(val);
                if !ver.is_empty() {
                    version = Some(ver);
                }
            }
            "tags" => {
                if val.is_empty() {
                    in_tags_list = true;
                } else if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                {
                    for part in inner.split(',') {
                        let t = unquote_yaml(part.trim());
                        if !t.is_empty() {
                            tags.push(t);
                        }
                    }
                } else {
                    let t = unquote_yaml(val);
                    if !t.is_empty() {
                        tags.push(t);
                    }
                }
            }
            "origin" => {
                let o = unquote_yaml(val);
                if !o.is_empty() {
                    origin = Some(o);
                }
            }
            "source_session" | "session" => {
                let s = unquote_yaml(val);
                if !s.is_empty() {
                    source_session = Some(s);
                }
            }
            "learned_at" | "date" => {
                let d = unquote_yaml(val);
                if !d.is_empty() {
                    learned_at = Some(d);
                }
            }
            _ => {
                // Abaikan field tambahan opsional agar forward-compatible.
            }
        }
    }

    let name = name.ok_or_else(|| anyhow!("field wajib `name` tidak ada di frontmatter"))?;
    let description =
        description.ok_or_else(|| anyhow!("field wajib `description` tidak ada di frontmatter"))?;

    Ok(SkillFrontmatter {
        name,
        description,
        version,
        tags,
        origin,
        source_session,
        learned_at,
    })
}

fn unquote_yaml(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].trim().to_string()
    } else {
        t.to_string()
    }
}

/// Potong deskripsi untuk tampilan indeks (maks 60 karakter, 1 baris bersih).
pub fn truncate_index_desc(desc: &str, max_chars: usize) -> String {
    let one_line: String = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = one_line.chars().count();
    if count <= max_chars {
        return one_line;
    }
    if max_chars <= 1 {
        return one_line.chars().take(max_chars).collect();
    }
    let head: String = one_line.chars().take(max_chars - 1).collect();
    format!("{head}…")
}

/// Kategori dari struktur folder relatif terhadap root skills:
/// - `<root>/<skill>/SKILL.md` -> `"umum"`
/// - `<root>/<kategori>/<skill>/SKILL.md` -> `"<kategori>"`
fn infer_category(root: &Path, skill_dir: &Path) -> String {
    let Ok(rel) = skill_dir.strip_prefix(root) else {
        return "umum".to_string();
    };
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => os.to_str().map(|s| s.to_string()),
            _ => None,
        })
        .collect();
    if comps.len() >= 2 {
        comps[..comps.len() - 1].join("/")
    } else {
        "umum".to_string()
    }
}

/// Pindai satu akar direktori skill secara rekursif (maks kedalaman 3 untuk kategori).
/// Bila `skip_top_subdir` diisi (mis. `Some("improvement")`), subfolder itu dilewati pada pass ini
/// agar dapat dipindai pada pass prioritas lebih rendah.
fn scan_skills_root_filtered(
    root: &Path,
    scope: &'static str,
    skip_top_subdir: Option<&str>,
    seen_in_scope: &mut HashSet<String>,
    out: &mut Vec<SkillInfo>,
    warnings: &mut Vec<String>,
) {
    if !root.is_dir() {
        return;
    }
    scan_dir_recursive_filtered(
        root,
        root,
        0,
        scope,
        skip_top_subdir,
        seen_in_scope,
        out,
        warnings,
    );
}

fn scan_skills_root(
    root: &Path,
    scope: &'static str,
    seen_in_scope: &mut HashSet<String>,
    out: &mut Vec<SkillInfo>,
    warnings: &mut Vec<String>,
) {
    scan_skills_root_filtered(root, scope, None, seen_in_scope, out, warnings);
}

#[allow(clippy::too_many_arguments)]
fn scan_dir_recursive_filtered(
    root: &Path,
    dir: &Path,
    depth: usize,
    scope: &'static str,
    skip_top_subdir: Option<&str>,
    seen_in_scope: &mut HashSet<String>,
    out: &mut Vec<SkillInfo>,
    warnings: &mut Vec<String>,
) {
    if depth > 3 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    for p in entries {
        if p.is_dir() {
            // Jangan turun ke folder referensi internal milik sebuah skill.
            if let Some(fname) = p.file_name().and_then(|s| s.to_str()) {
                if matches!(fname, "references" | "templates" | "scripts" | ".git") {
                    continue;
                }
                if depth == 0 && skip_top_subdir == Some(fname) {
                    continue;
                }
            }
            let skill_md = p.join("SKILL.md");
            if skill_md.is_file() {
                match std::fs::read_to_string(&skill_md) {
                    Ok(raw) => match parse_skill_frontmatter(&raw) {
                        Ok(fm) => {
                            if !seen_in_scope.insert(fm.name.clone()) {
                                warnings.push(format!(
                                    "skill duplikat `{}` di {} ({}) dilewati",
                                    fm.name,
                                    skill_md.display(),
                                    scope
                                ));
                                continue;
                            }
                            let category = infer_category(root, &p);
                            out.push(SkillInfo {
                                name: fm.name,
                                summary: fm.description,
                                category,
                                version: fm.version,
                                tags: fm.tags,
                                path: skill_md,
                                dir: p,
                                scope,
                                origin: fm.origin,
                                source_session: fm.source_session,
                                learned_at: fm.learned_at,
                                shadows: Vec::new(),
                            });
                        }
                        Err(e) => {
                            warnings.push(format!(
                                "skill tidak valid di {} dilewati: {e}",
                                skill_md.display()
                            ));
                        }
                    },
                    Err(e) => {
                        warnings.push(format!(
                            "gagal membaca {} dilewati: {e}",
                            skill_md.display()
                        ));
                    }
                }
            } else {
                // Mungkin folder kategori yang berisi subfolder skill.
                let before = out.len();
                let before_w = warnings.len();
                scan_dir_recursive_filtered(
                    root,
                    &p,
                    depth + 1,
                    scope,
                    None,
                    seen_in_scope,
                    out,
                    warnings,
                );
                // Bila folder langsung di dalam root tidak punya SKILL.md dan tidak berisi subfolder skill
                // maupun file .md lain, catat peringatan ringan (bukan crash!).
                if depth == 0 && out.len() == before && warnings.len() == before_w {
                    let has_any_file = std::fs::read_dir(&p)
                        .ok()
                        .map(|mut it| it.next().is_some())
                        .unwrap_or(false);
                    if has_any_file {
                        warnings.push(format!(
                            "folder skill tanpa SKILL.md di {} dilewati",
                            p.display()
                        ));
                    }
                }
            }
        } else if p.is_file() && depth == 0 {
            // Kompatibilitas mundur dengan file flat `*.md` lama di root skills.
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            if p.file_name().and_then(|x| x.to_str()) == Some("SKILL.md") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let (name, summary, version, tags, origin, source_session, learned_at) =
                if raw.trim_start().starts_with("---") {
                    match parse_skill_frontmatter(&raw) {
                        Ok(fm) => (
                            fm.name,
                            fm.description,
                            fm.version,
                            fm.tags,
                            fm.origin,
                            fm.source_session,
                            fm.learned_at,
                        ),
                        Err(e) => {
                            warnings.push(format!(
                                "skill frontmatter rusak di {} dilewati: {e}",
                                p.display()
                            ));
                            continue;
                        }
                    }
                } else {
                    let desc = raw
                        .lines()
                        .map(|l| l.trim().trim_start_matches(['#', '>']).trim())
                        .find(|l| !l.is_empty() && *l != "---")
                        .unwrap_or("(tanpa deskripsi)")
                        .to_string();
                    (
                        stem.to_ascii_lowercase(),
                        desc,
                        None,
                        Vec::new(),
                        None,
                        None,
                        None,
                    )
                };
            if !seen_in_scope.insert(name.clone()) {
                warnings.push(format!(
                    "skill duplikat `{name}` di {} ({scope}) dilewati",
                    p.display()
                ));
                continue;
            }
            out.push(SkillInfo {
                name,
                summary,
                category: "umum".to_string(),
                version,
                tags,
                dir: p.parent().unwrap_or(root).to_path_buf(),
                path: p,
                scope,
                origin,
                source_session,
                learned_at,
                shadows: Vec::new(),
            });
        }
    }
}

fn record_shadow(skills: &mut [SkillInfo], shadowed: &SkillInfo) {
    if let Some(winner) = skills.iter_mut().find(|w| w.name == shadowed.name) {
        if winner.path != shadowed.path
            && !winner.shadows.iter().any(|sh| sh.path == shadowed.path)
        {
            winner.shadows.push(ShadowedSkillInfo {
                scope: shadowed.scope,
                category: shadowed.category.clone(),
                path: shadowed.path.clone(),
                is_improvement: shadowed.is_improvement_skill(),
            });
        }
    }
}

/// Pindai skill dari 2 direktori eksplisit (`project_root` menang bila nama sama terhadap `user_root`).
/// Berguna untuk pengujian terisolasi tanpa mengubah env var global.
pub fn scan_skills_in_dirs(project_root: &Path, user_root: Option<&Path>) -> SkillScanResult {
    let mut res = SkillScanResult::default();
    let mut seen_names: HashSet<String> = HashSet::new();

    let mut proj_seen = HashSet::new();
    let mut proj_skills = Vec::new();
    scan_skills_root(
        project_root,
        "project",
        &mut proj_seen,
        &mut proj_skills,
        &mut res.warnings,
    );
    for s in proj_skills {
        seen_names.insert(s.name.clone());
        res.skills.push(s);
    }

    if let Some(u_dir) = user_root {
        // Prioritas #2: skill pengguna & bawaan di luar subfolder `improvement/`.
        let mut user_seen = HashSet::new();
        let mut user_skills = Vec::new();
        scan_skills_root_filtered(
            u_dir,
            "user",
            Some(IMPROVEMENT_SKILLS_SUBDIR),
            &mut user_seen,
            &mut user_skills,
            &mut res.warnings,
        );
        for s in user_skills {
            if seen_names.insert(s.name.clone()) {
                res.skills.push(s);
            } else {
                record_shadow(&mut res.skills, &s);
            }
        }

        // Prioritas #3 (lebih rendah dari skill pengguna): skill karya Selena improvement di `skills/improvement/`.
        // Bila ada nama kembar dengan skill proyek atau skill pengguna, skill prioritas lebih tinggi menang,
        // tetapi keberadaan bayangan dicatat di `winner.shadows` agar tidak pernah senyap.
        let imp_dir = u_dir.join(IMPROVEMENT_SKILLS_SUBDIR);
        if imp_dir.is_dir() {
            let mut imp_seen = HashSet::new();
            let mut imp_skills = Vec::new();
            scan_dir_recursive_filtered(
                u_dir,
                &imp_dir,
                1,
                "user",
                None,
                &mut imp_seen,
                &mut imp_skills,
                &mut res.warnings,
            );
            for s in imp_skills {
                if seen_names.insert(s.name.clone()) {
                    res.skills.push(s);
                } else {
                    record_shadow(&mut res.skills, &s);
                }
            }
        }
    }

    res.skills
        .sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    res
}

/// Pindai seluruh skill dari folder PROYEK (prioritas #1) lalu PENGGUNA (prioritas #2),
/// mengembalikan daftar skill valid + daftar peringatan folder/frontmatter yang dilewati.
pub fn scan_skills_with_warnings(cwd: &Path) -> SkillScanResult {
    let user_dir = user_skills_dir();
    let mut res = scan_skills_in_dirs(&project_skills_dir(cwd), user_dir.as_deref());
    let mut seen_names: HashSet<String> = res.skills.iter().map(|s| s.name.clone()).collect();

    // 3. Fallback lokasi config lama bila ada.
    if let Some(legacy_dir) = legacy_config_skills_dir() {
        let mut leg_seen = HashSet::new();
        let mut leg_skills = Vec::new();
        scan_skills_root(
            &legacy_dir,
            "user",
            &mut leg_seen,
            &mut leg_skills,
            &mut res.warnings,
        );
        for s in leg_skills {
            if seen_names.insert(s.name.clone()) {
                res.skills.push(s);
            } else {
                record_shadow(&mut res.skills, &s);
            }
        }
    }

    res.skills
        .sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    res
}

/// Temukan semua skill di folder proyek & pengguna (proyek didahulukan bila nama sama).
/// Peringatan folder rusak dicatat ke stderr tanpa pernah menggagalkan startup.
pub fn list_skills(cwd: &Path) -> Vec<SkillInfo> {
    let res = scan_skills_with_warnings(cwd);
    for w in &res.warnings {
        eprintln!("[selen] peringatan skills: {w}");
    }
    res.skills
}

/// Cek apakah nama skill termasuk salah satu dari 14 skill bawaan resmi SelenCode.
pub fn is_builtin_skill_name(name: &str) -> bool {
    crate::builtin_skills::BUILTIN_SKILL_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name))
}

/// Format kartu daftar `/skills` (dipakai TUI, REPL polos, Telegram, dan CLI `selen skills list`).
/// Menampilkan penanda bayangan eksplisit + kedua store asal apabila salinan proyek menimpa skill global (v0.5.27).
pub fn format_skills_list_card(scan: &SkillScanResult, auto_skills: bool) -> String {
    let auto_state = if auto_skills { "AKTIF" } else { "NONAKTIF" };
    let shadow_count = scan.skills.iter().filter(|s| !s.shadows.is_empty()).count();
    let shadow_hdr = if shadow_count > 0 {
        format!(" · {shadow_count} salinan proyek menimpa global")
    } else {
        String::new()
    };
    let mut out = format!(
        "# Daftar Skills (Proyek > Pengguna · {} terpasang{shadow_hdr} · auto-review: {auto_state})\n",
        scan.skills.len()
    );
    if scan.skills.is_empty() {
        out.push_str("! Belum ada skill tersimpan.\n");
    } else {
        for s in &scan.skills {
            let desc = truncate_index_desc(&s.summary, SKILL_INDEX_DESC_MAX_CHARS);
            let origin_badge = if s.is_improvement_skill() {
                " · [selena-improvement]"
            } else {
                ""
            };
            let scope_badge = if !s.shadows.is_empty() {
                let sh_labels: Vec<&str> = s
                    .shadows
                    .iter()
                    .map(|sh| {
                        if sh.is_improvement {
                            "user/improvement"
                        } else {
                            sh.scope
                        }
                    })
                    .collect();
                format!("{} [MENIMPA {}]", s.scope, sh_labels.join(", "))
            } else {
                s.scope.to_string()
            };
            let meta = match &s.version {
                Some(v) => format!("{scope_badge} · {} · v{v}{origin_badge}", s.category),
                None => format!("{scope_badge} · {}{origin_badge}", s.category),
            };
            out.push_str(&format!("> {} | {meta} | {desc}\n", s.name));
            for sh in &s.shadows {
                let sh_store = if sh.is_improvement {
                    "user (improvement)"
                } else {
                    sh.scope
                };
                out.push_str(&format!(
                    "!   ↳ proyek menang: {} ({}) menimpa {} ({})\n",
                    s.scope,
                    s.path.display(),
                    sh_store,
                    sh.path.display()
                ));
            }
        }
    }
    if !scan.warnings.is_empty() {
        out.push_str("---\n");
        for w in scan.warnings.iter().take(5) {
            out.push_str(&format!("! Peringatan: {w}\n"));
        }
    }
    out.push_str("---\n");
    out.push_str("! Muat paksa skill:         /skill <nama>   ·   Lihat isi: /skills show <nama>\n");
    out.push_str("! Pasang save-token / rm:   /skills save-token   ·   /skills rm <nama>\n");
    out.push_str("! Timpa ke proyek eksplisit:/skills override <nama>\n");
    out.push_str("! Review / buat manual:     /skills review   ·   /learn <nama> <instruksi>");
    out
}

/// Format indeks skill ringkas dikelompokkan per kategori (deskripsi dipotong maks 60 char)
/// untuk disuntikkan ke snapshot dinamis awal sesi & tool `skills_list`.
pub fn format_skills_index(skills: &[SkillInfo]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut by_cat: BTreeMap<&str, Vec<&SkillInfo>> = BTreeMap::new();
    for s in skills {
        by_cat.entry(s.category.as_str()).or_default().push(s);
    }
    let mut out = String::from(
        "Indeks Skills (muat isi penuh dengan `skill_view(name)` hanya jika cocok):\n",
    );
    for (cat, items) in by_cat {
        out.push_str(&format!("[{cat}]\n"));
        for s in items {
            let desc = truncate_index_desc(&s.summary, SKILL_INDEX_DESC_MAX_CHARS);
            if s.shadows_user_store() {
                out.push_str(&format!("- {} (project; menimpa user): {desc}\n", s.name));
            } else {
                out.push_str(&format!("- {} ({}): {desc}\n", s.name, s.scope));
            }
        }
    }
    out.trim_end().to_string()
}

/// Format indeks skill dengan pengaman pagu token (Bagian 5 #6 v0.5.6):
/// bila indeks gabungan melebihi `max_tokens`, urutan pemangkasan prioritas adalah
/// **bawaan (builtin) duluan -> user kustom -> proyek (dipertahankan paling akhir)**.
pub fn format_skills_index_budgeted(skills: &[SkillInfo], max_tokens: usize) -> String {
    let full = format_skills_index(skills);
    if max_tokens == 0 || full.chars().count().div_ceil(3) <= max_tokens {
        return full;
    }
    let mut kept: Vec<SkillInfo> = skills.to_vec();
    let trim_pass = |list: &mut Vec<SkillInfo>, pred: &dyn Fn(&SkillInfo) -> bool| {
        while format_skills_index(list).chars().count().div_ceil(3) > max_tokens {
            if let Some(pos) = list.iter().rposition(pred) {
                list.remove(pos);
            } else {
                break;
            }
        }
    };
    // 1. Pangkas skill bawaan (scope user + nama builtin) duluan.
    trim_pass(&mut kept, &|s| s.scope != "project" && is_builtin_skill_name(&s.name));
    // 2. Pangkas skill pengguna kustom berikutnya.
    trim_pass(&mut kept, &|s| s.scope != "project" && !is_builtin_skill_name(&s.name));
    // 3. Paling akhir pangkas skill proyek bila tetap melampaui pagu.
    trim_pass(&mut kept, &|s| s.scope == "project");
    format_skills_index(&kept)
}

/// Daftar file referensi opsional (`references/`, `templates/`, `scripts/`) di dalam folder skill.
pub fn list_skill_references(skill_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["references", "templates", "scripts"] {
        let sub_dir = skill_dir.join(sub);
        collect_rel_files(skill_dir, &sub_dir, 0, &mut out);
    }
    out.sort();
    out
}

fn collect_rel_files(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 3 || !dir.is_dir() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect_rel_files(base, &p, depth + 1, out);
        } else if p.is_file() && !crate::security::is_secret_file(&p) {
            if let Ok(rel) = p.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Tingkat 2: Muat isi penuh `SKILL.md` + daftar nama file referensi (belum dibaca isinya).
pub fn load_skill_with_refs(cwd: &Path, name: &str) -> Result<(SkillInfo, String, Vec<String>)> {
    let clean = sanitize_skill_name(name)?;
    let all = scan_skills_with_warnings(cwd).skills;
    let info = all
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(&clean))
        .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan. Cek daftar lewat `/skills` atau `skills_list`."))?;
    let content = std::fs::read_to_string(&info.path)
        .with_context(|| format!("gagal membaca skill {}", info.path.display()))?;
    let refs = if info.path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
        list_skill_references(&info.dir)
    } else {
        Vec::new()
    };
    Ok((info, crate::security::redact_secrets(&content), refs))
}

/// Muat isi file skill berdasarkan nama (kompatibilitas pemanggil lama).
pub fn load_skill(cwd: &Path, name: &str) -> Result<(SkillInfo, String)> {
    let (info, content, _) = load_skill_with_refs(cwd, name)?;
    Ok((info, content))
}

/// Tingkat 3: Muat SATU file referensi di dalam folder skill dengan keamanan ketat:
/// - Menolak path absolut, `..` (path traversal), dan symlink keluar dari folder skill.
/// - Menolak file rahasia (`.env`, `*.key`, `id_rsa`, dll.) dan meredaksi pola rahasia.
/// - Skrip di dalam `scripts/` hanya dibaca sebagai teks, TIDAK PERNAH dieksekusi otomatis.
pub fn load_skill_reference(cwd: &Path, name: &str, rel_path: &str) -> Result<(SkillInfo, String)> {
    let clean = sanitize_skill_name(name)?;
    let rel_trim = rel_path.trim();
    if rel_trim.is_empty() {
        return Err(anyhow!("argumen `path` referensi tidak boleh kosong"));
    }
    let rel = Path::new(rel_trim);
    if rel.is_absolute() {
        return Err(anyhow!("akses ditolak: path referensi tidak boleh absolut (`{rel_trim}`)"));
    }
    for comp in rel.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "akses ditolak: path traversal tidak diizinkan (`{rel_trim}`)"
                ));
            }
            _ => {}
        }
    }

    let all = scan_skills_with_warnings(cwd).skills;
    let info = all
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(&clean))
        .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan."))?;

    let canon_dir = std::fs::canonicalize(&info.dir)
        .with_context(|| format!("folder skill tidak valid: {}", info.dir.display()))?;
    let candidate = canon_dir.join(rel);
    if crate::security::is_secret_file(&candidate) {
        return Err(anyhow!(
            "akses ditolak: file rahasia `{rel_trim}` tidak boleh dibaca"
        ));
    }
    let canon_target = std::fs::canonicalize(&candidate)
        .map_err(|_| anyhow!("file referensi `{rel_trim}` tidak ditemukan di skill `{clean}`"))?;
    if !canon_target.starts_with(&canon_dir) {
        return Err(anyhow!(
            "akses ditolak: path `{rel_trim}` keluar dari folder skill `{clean}`"
        ));
    }
    if crate::security::is_secret_file(&canon_target) {
        return Err(anyhow!(
            "akses ditolak: file rahasia `{rel_trim}` tidak boleh dibaca"
        ));
    }
    if !canon_target.is_file() {
        return Err(anyhow!("`{rel_trim}` bukan file reguler"));
    }
    let raw = std::fs::read_to_string(&canon_target)
        .with_context(|| format!("gagal membaca referensi `{rel_trim}`"))?;
    Ok((info, crate::security::redact_secrets(&raw)))
}

/// Format isi `SKILL.md` lengkap dengan frontmatter YAML yang valid & diredaksi dari rahasia.
pub fn render_skill_markdown(
    name: &str,
    description: &str,
    body: &str,
    version: Option<&str>,
    tags: &[String],
) -> Result<String> {
    let clean = sanitize_skill_name(name)?;
    let safe_desc = crate::security::redact_secrets(
        &description.split_whitespace().collect::<Vec<_>>().join(" "),
    );
    if safe_desc.is_empty() {
        return Err(anyhow!("`description` wajib diisi (satu baris)"));
    }
    let safe_body = crate::security::redact_secrets(body.trim());
    if safe_body.is_empty() {
        return Err(anyhow!("isi `body` skill tidak boleh kosong"));
    }
    let mut out = format!("---\nname: {clean}\ndescription: {safe_desc}\n");
    if let Some(v) = version.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        out.push_str(&format!("version: {v}\n"));
    }
    if !tags.is_empty() {
        let joined: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if !joined.is_empty() {
            out.push_str(&format!("tags: [{}]\n", joined.join(", ")));
        }
    }
    out.push_str("---\n\n");
    out.push_str(&safe_body);
    out.push('\n');
    Ok(out)
}

/// Render file `SKILL.md` lengkap dengan metadata provenans Selena improvement
/// (`origin`, `source_session`, `learned_at`) di frontmatter YAML.
#[allow(clippy::too_many_arguments)]
pub fn render_skill_markdown_with_origin(
    name: &str,
    description: &str,
    body: &str,
    version: Option<&str>,
    tags: &[String],
    origin: Option<&str>,
    source_session: Option<&str>,
    learned_at: Option<&str>,
) -> Result<String> {
    let base = render_skill_markdown(name, description, body, version, tags)?;
    let has_meta = origin.map(|s| !s.trim().is_empty()).unwrap_or(false)
        || source_session.map(|s| !s.trim().is_empty()).unwrap_or(false)
        || learned_at.map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !has_meta {
        return Ok(base);
    }
    let mut extra = String::new();
    if let Some(o) = origin.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        extra.push_str(&format!("origin: {o}\n"));
    }
    if let Some(ss) = source_session.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        extra.push_str(&format!("source_session: {ss}\n"));
    }
    if let Some(la) = learned_at.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        extra.push_str(&format!("learned_at: {la}\n"));
    }
    if let Some(idx) = base.find("\n---\n") {
        let mut out = String::with_capacity(base.len() + extra.len());
        out.push_str(&base[..idx + 1]);
        out.push_str(&extra);
        out.push_str(&base[idx + 1..]);
        Ok(out)
    } else {
        Ok(base)
    }
}

/// Path target `SKILL.md` untuk skill BARU karya Selena improvement:
/// `<user_skills_dir>/improvement/<nama>/SKILL.md` (lintas sesi, terpisah dari skill pengguna).
pub fn improvement_skill_file_path(user_root: &Path, name: &str) -> Result<PathBuf> {
    let clean = sanitize_skill_name(name)?;
    Ok(user_root
        .join(IMPROVEMENT_SKILLS_SUBDIR)
        .join(clean)
        .join("SKILL.md"))
}

/// Tentukan path target `SKILL.md` untuk skill proyek baru (`<proyek>/.selencode/skills/[<cat>/]<name>/SKILL.md`).
pub fn skill_file_path_for(cwd: &Path, name: &str, category: Option<&str>) -> Result<PathBuf> {
    let clean = sanitize_skill_name(name)?;
    let mut base = project_skills_dir(cwd);
    if let Some(cat) = category.map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "umum") {
        let clean_cat = sanitize_skill_name(cat)?;
        base = base.join(clean_cat);
    }
    Ok(base.join(clean).join("SKILL.md"))
}

/// Perbarui (patch) skill yang sudah ada langsung ke berkas ASAL tempat skill dimuat
/// (baik di store proyek maupun store pengguna, tanpa menciptakan salinan bayangan, v0.5.27).
/// Bila belum ada di mana pun, buat skill baru di `.selencode/skills/<nama>/SKILL.md`.
pub fn patch_resolved_skill(
    cwd: &Path,
    name: &str,
    description: Option<&str>,
    instruction: &str,
) -> Result<(PathBuf, String, &'static str, bool)> {
    let clean = sanitize_skill_name(name)?;
    let existing = scan_skills_with_warnings(cwd)
        .skills
        .into_iter()
        .find(|s| s.name == clean);

    if let Some(info) = existing {
        let old_raw = std::fs::read_to_string(&info.path).unwrap_or_default();
        let old_fm = parse_skill_frontmatter(&old_raw).ok();
        let desc = description
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
            .or_else(|| old_fm.as_ref().map(|f| f.description.as_str()))
            .unwrap_or(&info.summary);
        let ver = old_fm.as_ref().and_then(|f| f.version.as_deref()).or(Some("1.0.0"));
        let tags = old_fm.as_ref().map(|f| f.tags.clone()).unwrap_or_default();
        let md = render_skill_markdown_with_origin(
            &clean,
            desc,
            instruction,
            ver,
            &tags,
            old_fm.as_ref().and_then(|f| f.origin.as_deref()),
            old_fm.as_ref().and_then(|f| f.source_session.as_deref()),
            old_fm.as_ref().and_then(|f| f.learned_at.as_deref()),
        )?;
        if let Some(parent) = info.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("gagal membuat folder skill {}", parent.display()))?;
        }
        std::fs::write(&info.path, md)
            .with_context(|| format!("gagal menyimpan skill {}", info.path.display()))?;
        return Ok((info.path, clean, info.scope, true));
    }

    let (path, clean, existed) = upsert_skill(cwd, &clean, description, instruction)?;
    Ok((path, clean, "project", existed))
}

/// Salin EKSPLISIT satu skill dari store pengguna ke store proyek (`.selencode/skills/`)
/// sebagai salinan timpa proyek (`/skills override <nama>`, v0.5.27).
pub fn copy_user_skill_to_project(cwd: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    let clean = sanitize_skill_name(name)?;
    let existing = scan_skills_with_warnings(cwd)
        .skills
        .into_iter()
        .find(|s| s.name == clean)
        .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan"))?;
    if existing.scope == "project" {
        return Err(anyhow!(
            "skill '{clean}' sudah memiliki salinan proyek di {}",
            existing.path.display()
        ));
    }
    let cat_opt = if existing.category == "umum" {
        None
    } else {
        Some(existing.category.as_str())
    };
    let dst_skill_md = skill_file_path_for(cwd, &clean, cat_opt)?;
    let dst_dir = dst_skill_md
        .parent()
        .ok_or_else(|| anyhow!("path tujuan skill proyek tidak valid"))?;
    std::fs::create_dir_all(dst_dir)
        .with_context(|| format!("gagal membuat folder {}", dst_dir.display()))?;
    let raw = std::fs::read_to_string(&existing.path)
        .with_context(|| format!("gagal membaca {}", existing.path.display()))?;
    std::fs::write(&dst_skill_md, raw)
        .with_context(|| format!("gagal menulis {}", dst_skill_md.display()))?;
    // Salin subfolder referensi opsional bila ada.
    for rel_ref in list_skill_references(&existing.dir) {
        let src_ref = existing.dir.join(&rel_ref);
        let dst_ref = dst_dir.join(&rel_ref);
        if let Some(p) = dst_ref.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::copy(&src_ref, &dst_ref);
    }
    Ok((existing.path, dst_skill_md))
}

/// Simpan atau perbarui (patch) skill ke `<proyek>/.selencode/skills/<nama>/SKILL.md`
/// dengan frontmatter YAML standar SelenCode. Konten diredaksi dari pola rahasia sebelum ditulis.
pub fn upsert_skill(
    cwd: &Path,
    name: &str,
    description: Option<&str>,
    instruction: &str,
) -> Result<(PathBuf, String, bool)> {
    let clean = sanitize_skill_name(name)?;
    let existing_proj = scan_skills_with_warnings(cwd)
        .skills
        .into_iter()
        .find(|s| s.scope == "project" && s.name == clean);

    let (path, existed, fallback_desc) = match existing_proj {
        Some(info) => (info.path, true, info.summary),
        None => (
            project_skills_dir(cwd).join(&clean).join("SKILL.md"),
            false,
            format!("Playbook skill {clean}"),
        ),
    };
    let desc = description
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .unwrap_or(&fallback_desc);
    let md = render_skill_markdown(&clean, desc, instruction, Some("1.0.0"), &[])?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("gagal membuat folder skill {}", parent.display()))?;
    }
    std::fs::write(&path, md)
        .with_context(|| format!("gagal menyimpan skill {}", path.display()))?;
    Ok((path, clean, existed))
}

/// Template skill bawaan `rilis-github` (disemai ke `<data_dir>/selencode/skills/dev/`).
pub const BUILTIN_RILIS_GITHUB_MD: &str = "\
---
name: rilis-github
description: Prosedur rilis versi SelenCode, verifikasi CI, & tag GitHub
version: 1.0.0
tags: [release, github, ci, rust]
---

# Playbook Rilis GitHub SelenCode

1. Cek `git_status` dan pastikan tidak ada file rahasia ikut ter-stage.
2. Bump `version` di `Cargo.toml` dan `Cargo.lock` secara seragam.
3. Commit lalu push ke `main`, dan tunggu CI Linux x64 serta Windows x64 hijau.
4. Buat annotated tag `vX.Y.Z` lalu push tag untuk memicu Release workflow (Linux x64, Termux arm64, Windows x64).
5. Verifikasi ketiga aset rilis terunggah dan uji `selen update`.
";

pub const BUILTIN_RILIS_GITHUB_REF: &str = "\
# Checklist Detail Rilis GitHub

- [ ] `Cargo.toml` & `Cargo.lock` sudah sinkron.
- [ ] Seluruh tes & Clippy CI Linux + Windows x64 hijau.
- [ ] Tidak ada rahasia/API key di diff commit.
- [ ] Aset `selen-linux-x64`, `selen-termux-arm64`, dan `selen-windows-x64.exe` terbit utuh.
";

/// Template skill bawaan `tinjau-rust` (disemai ke `<data_dir>/selencode/skills/dev/`).
pub const BUILTIN_TINJAU_RUST_MD: &str = "\
---
name: tinjau-rust
description: Checklist tinjauan kode Rust (keamanan, error, performa, tes)
version: 1.0.0
tags: [rust, code-review, security, performance]
---

# Playbook Tinjauan Kode Rust

1. Baca perubahan utama via `git_diff` / `read_file` seperlunya.
2. Periksa keamanan path (anti-traversal `..`, absolut, symlink escape) & redaksi rahasia.
3. Pastikan penanganan error bebas `unwrap()` pada input eksternal dan aman UTF-8.
4. Cek performa (hindari I/O blocking di loop UI, jaga batas ukuran output/memori).
5. Jalankan verifikasi `cargo test` / `cargo clippy` 1 kali dan pastikan semua tes lulus.
";

pub const BUILTIN_TINJAU_RUST_REF: &str = "\
# Checklist Rinci Review Kode Rust

- Path user dikanonisasi dan diverifikasi tetap di dalam direktori yang diizinkan.
- File rahasia (`.env`, `*.key`, `id_rsa`) ditolak dan output diredaksi.
- Pemotongan string memakai batas karakter UTF-8 (`chars()` / `is_char_boundary`).
";

/// Semai 14 skill bawaan resmi SelenCode (2 asli + 12 hasil adaptasi MIT) + file `NOTICE`
/// ke direktori target `root`. Tidak pernah menimpa file yang sudah dimodifikasi user;
/// hanya menyegarkan deskripsi bawaan lama yang masih persis teks default v0.5.6.
pub fn seed_builtin_skills_to(root: &Path) {
    let dev = root.join("dev");
    let rilis_dir = dev.join("rilis-github");
    let tinjau_dir = dev.join("tinjau-rust");
    if !rilis_dir.join("SKILL.md").exists() && std::fs::create_dir_all(rilis_dir.join("references")).is_ok() {
        let _ = std::fs::write(rilis_dir.join("SKILL.md"), BUILTIN_RILIS_GITHUB_MD);
        let _ = std::fs::write(
            rilis_dir.join("references").join("checklist-rilis.md"),
            BUILTIN_RILIS_GITHUB_REF,
        );
    }
    if !tinjau_dir.join("SKILL.md").exists() && std::fs::create_dir_all(tinjau_dir.join("references")).is_ok() {
        let _ = std::fs::write(tinjau_dir.join("SKILL.md"), BUILTIN_TINJAU_RUST_MD);
        let _ = std::fs::write(
            tinjau_dir.join("references").join("checklist-rust.md"),
            BUILTIN_TINJAU_RUST_REF,
        );
    }
    for (rel_path, content) in crate::builtin_skills::BUILTIN_ADAPTED_FILES {
        let dst = root.join(rel_path);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        let _ = std::fs::write(&dst, content);
    }
    // Perbaikan 2 (v0.5.7): segarkan 4 deskripsi indeks bawaan jika masih persis versi default v0.5.6
    // (tidak menyentuh skill yang telah diedit manual oleh pengguna).
    let stamp_path = root.join(".builtin_version");
    let cur_ver = env!("CARGO_PKG_VERSION");
    let stamped = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    if stamped.trim() != cur_ver {
        let upgrades: &[(&str, &str, &str)] = &[
            (
                "dev/systematic-debugging/SKILL.md",
                "description: Debug 4 fase: temukan akar masalah dulu",
                "description: WAJIB untuk bug/error/job gagal/ulang: debug 4 fase",
            ),
            (
                "dev/systematic-debugging/SKILL.md",
                "description: WAJIB untuk bug/error/salah hitung: debug 4 fase",
                "description: WAJIB untuk bug/error/job gagal/ulang: debug 4 fase",
            ),
            (
                "dev/test-driven-development/SKILL.md",
                "description: TDD ketat: tes gagal, kode minimal, refactor",
                "description: Untuk fitur baru+tes/TDD: tes gagal dulu, lalu kode",
            ),
            (
                "dev/requesting-code-review/SKILL.md",
                "description: Review diff berbasis spesifikasi sebelum merge",
                "description: Untuk tinjau/review diff kode sebelum merge/commit",
            ),
            (
                "dev/grill-me/SKILL.md",
                "description: Wawancara kritis desain untuk tutup celah",
                "description: Untuk wawancara rencana/desain kritis sebelum koding",
            ),
        ];
        for (rel, old_desc, new_desc) in upgrades {
            let p = root.join(rel);
            if let Ok(raw) = std::fs::read_to_string(&p) {
                if raw.contains(old_desc) {
                    let _ = std::fs::write(&p, raw.replacen(old_desc, new_desc, 1));
                }
            }
        }
        let _ = std::fs::write(&stamp_path, format!("{cur_ver}\n"));
    }
}

/// Semai 14 skill bawaan resmi SelenCode + `NOTICE` ke `<data_dir>/selencode/skills/`.
/// Tidak menimpa skill yang sudah ada; bisa dimatikan dengan `SELENCODE_NO_DEFAULT_SKILLS=1`.
pub fn seed_default_user_skills() {
    if std::env::var("SELENCODE_NO_DEFAULT_SKILLS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return;
    }
    let Some(root) = user_skills_dir() else {
        return;
    };
    seed_builtin_skills_to(&root);
}

/// Template skill bawaan SelenCode untuk efisiensi token & langkah minimal.
pub const TOKEN_EFFICIENCY_SKILL_BODY: &str = "\
Gunakan disiplin hemat token SelenCode (Progressive Disclosure):
1. Jangan baca file penuh jika cukup memakai `search_text` atau rentang baris (`start_line`/`end_line`).
2. Jangan aktifkan toolset tambahan lewat `activate_toolset` kecuali benar-benar diperlukan giliran ini.
3. Lakukan perubahan kode seminimal mungkin dalam 1–2 panggilan `edit_file`, bukan berulang kali.
4. Verifikasi CUKUP 1 KALI dengan perintah paling langsung (jalankan program atau test yang sudah ada); bila lulus langsung selesai.
5. Jangan membuat file test baru, `git init`, atau membaca file di luar ruang lingkup pertanyaan user.";

/// Pasang skill bawaan `save-token` ke `.selencode/skills/save-token/SKILL.md`.
pub fn install_token_efficiency_skill(cwd: &Path) -> Result<PathBuf> {
    let (path, _, _) = upsert_skill(
        cwd,
        "save-token",
        Some("Disiplin hemat token & langkah minimal SelenCode"),
        TOKEN_EFFICIENCY_SKILL_BODY,
    )?;
    Ok(path)
}

/// Simpan skill baru ke `.selencode/skills/<nama>/SKILL.md` (dipakai `/learn`).
pub fn save_skill(cwd: &Path, name: &str, instruction: &str) -> Result<PathBuf> {
    let (path, _, _) = upsert_skill(cwd, name, None, instruction)?;
    Ok(path)
}

/// Hapus skill project berdasarkan nama (mendukung format folder `SKILL.md` maupun file `.md` lama).
pub fn remove_skill(cwd: &Path, name: &str) -> Result<bool> {
    let clean = sanitize_skill_name(name)?;
    let proj_skills = scan_skills_with_warnings(cwd).skills;
    if let Some(info) = proj_skills
        .into_iter()
        .find(|s| s.scope == "project" && s.name == clean)
    {
        if info.path.file_name().and_then(|x| x.to_str()) == Some("SKILL.md") {
            std::fs::remove_dir_all(&info.dir)
                .with_context(|| format!("gagal menghapus folder skill {}", info.dir.display()))?;
        } else {
            std::fs::remove_file(&info.path)
                .with_context(|| format!("gagal menghapus skill {}", info.path.display()))?;
        }
        return Ok(true);
    }
    let legacy = project_skills_dir(cwd).join(format!("{clean}.md"));
    if legacy.exists() {
        std::fs::remove_file(&legacy)
            .with_context(|| format!("gagal menghapus skill {}", legacy.display()))?;
        return Ok(true);
    }
    Ok(false)
}

/// Sanitasi nama skill menjadi kebab-case huruf kecil yang aman.
pub fn sanitize_skill_name(raw: &str) -> Result<String> {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            s.push(ch);
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() || !is_valid_kebab_case(&s) {
        return Err(anyhow!("nama skill tidak valid (harus kebab-case non-kosong)"));
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
    if path.exists() {
        return Err(anyhow!(
            "berkas ekspor '{}' sudah ada — pilih nama file baru agar tidak menimpa berkas proyek",
            rel
        ));
    }
    let safe_md = crate::security::redact_secrets(&md);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, safe_md)
        .with_context(|| format!("gagal menulis file ekspor {}", path.display()))?;
    Ok(path)
}
