//! Tool `manage_memory` (Tahap 2 — Project Memory Terstruktur).
//!
//! Memungkinkan agent melihat, menyimpan, memperbarui, dan menghapus catatan
//! proyek lintas sesi secara terstruktur (terpisah dari riwayat chat biasa)
//! dengan kategori jelas (`arch`, `commands`, `conventions`, `decisions`,
//! `issues`, `tasks`, `general`).
//! Guard keamanan ketat di `Store`: rahasia, token, password, dan API key
//! selalu DITOLAK agar tidak pernah tersimpan di memori proyek.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::store::{memory_category_label, normalize_memory_category};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ManageMemory;

#[async_trait]
impl Tool for ManageMemory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_memory".into(),
            description: "Kelola memori proyek terstruktur lintas sesi (terpisah dari riwayat chat). Kategori: arch, commands, conventions, decisions, issues, tasks, general. DILARANG menyimpan rahasia/token/password/API key.".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["list", "save", "update", "delete"],
                        "description": "Aksi: list (lihat memori), save (simpan baru), update (perbarui ID), delete (hapus ID)"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["arch", "commands", "conventions", "decisions", "issues", "tasks", "general"],
                        "description": "Kategori memori proyek (default: general)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Isi catatan proyek yang ringkas & faktual (tanpa rahasia/token/password)"
                    },
                    "id": {
                        "type": "integer",
                        "description": "ID memori (wajib untuk update & delete)"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["project", "global"],
                        "description": "Cakupan memori saat save (default: project)"
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let store = ctx
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("penyimpanan memori (SQLite store) tidak aktif pada sesi ini"))?;
        let action = str_arg(&args, "action").ok_or_else(|| anyhow!("argumen 'action' wajib diisi"))?;
        let cwd_str = ctx.cwd.to_string_lossy().to_string();

        match action {
            "list" => {
                let items = if let Some(cat) = str_arg(&args, "category") {
                    store.list_memories_by_category(&cwd_str, cat, 40)?
                } else {
                    store.list_memories(&cwd_str, 40)?
                };
                if items.is_empty() {
                    return Ok("Belum ada memori proyek tersimpan.".to_string());
                }
                let mut out = format!("Memori proyek & global aktif ({} item):\n", items.len());
                for m in items {
                    let scope = if m.scope == "global" { "global" } else { "project" };
                    out.push_str(&format!(
                        "- #{} [{} | {}] {}\n",
                        m.id, m.category, scope, m.content
                    ));
                }
                Ok(out.trim_end().to_string())
            }
            "save" => {
                let content = str_arg(&args, "content")
                    .ok_or_else(|| anyhow!("argumen 'content' wajib diisi untuk action='save'"))?;
                let category = str_arg(&args, "category").unwrap_or("general");
                let scope_key = match str_arg(&args, "scope").unwrap_or("project") {
                    "global" => "global",
                    _ => cwd_str.as_str(),
                };
                let id = store.add_memory_categorized(scope_key, category, content)?;
                let norm = normalize_memory_category(category);
                Ok(format!(
                    "Memori #{id} disimpan pada kategori '{}' ({}).",
                    norm,
                    memory_category_label(norm)
                ))
            }
            "update" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("argumen 'id' (angka) wajib diisi untuk action='update'"))?;
                let content = str_arg(&args, "content")
                    .ok_or_else(|| anyhow!("argumen 'content' wajib diisi untuk action='update'"))?;
                let cat = str_arg(&args, "category");
                if store.update_memory(id, cat, content)? {
                    Ok(format!("Memori #{id} berhasil diperbarui."))
                } else {
                    Err(anyhow!("memori #{id} tidak ditemukan"))
                }
            }
            "delete" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("argumen 'id' (angka) wajib diisi untuk action='delete'"))?;
                if store.remove_memory(id)? {
                    Ok(format!("Memori #{id} berhasil dihapus."))
                } else {
                    Err(anyhow!("memori #{id} tidak ditemukan"))
                }
            }
            other => Err(anyhow!(
                "action '{other}' tidak dikenal (pilih: list, save, update, delete)"
            )),
        }
    }
}

/// Tool `memory` (Perbaikan 5 v0.5.7 — wadah `memory` 2.200 char & `profile` 1.375 char ala Hermes).
/// Menyimpan fakta/preferensi/profil pemilik secara persisten dengan pagu karakter keras;
/// snapshot keduanya dibekukan di awal sesi agar prompt tidak membengkak diam-diam.
pub struct UserMemoryTool;

#[async_trait]
impl Tool for UserMemoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory".into(),
            description: "Kelola wadah memori persisten (`memory`, maks 2200 char) & profil pemilik (`profile`, maks 1375 char). Aksi: add, replace, remove, view.".into(),
            parameters: schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["add", "replace", "remove", "view"],
                        "description": "add (tambah), replace (ganti), remove (hapus), view (lihat)"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["memory", "profile"],
                        "description": "Wadah tujuan: 'memory' (default) atau 'profile'"
                    },
                    "content": {
                        "type": "string",
                        "description": "Isi catatan baru (wajib untuk add & replace; tanpa rahasia/API key)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "Nomor entri (1-based) atau potongan teks lama (wajib untuk replace & remove)"
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        use crate::user_memory::{
            add_entry_in, load_frozen_snapshot_from, remove_entry_in, replace_entry_in,
            user_memory_dir, MemoryStoreKind,
        };
        let action = str_arg(&args, "action")
            .ok_or_else(|| anyhow!("argumen 'action' wajib diisi"))?
            .trim()
            .to_ascii_lowercase();
        let target_raw = str_arg(&args, "target").unwrap_or("memory");
        let kind = MemoryStoreKind::parse(target_raw)?;
        let dir = user_memory_dir();
        let max_chars = match kind {
            MemoryStoreKind::Memory => ctx.memory_cfg.max_memory_chars,
            MemoryStoreKind::Profile => ctx.memory_cfg.max_profile_chars,
        };

        match action.as_str() {
            "view" | "list" => {
                let snap = load_frozen_snapshot_from(
                    &dir,
                    ctx.memory_cfg.max_memory_chars,
                    ctx.memory_cfg.max_profile_chars,
                );
                let mem_txt = snap.memory_text();
                let prof_txt = snap.profile_text();
                Ok(format!(
                    "Wadah memory ({}/{} char):\n{}\n\nWadah profile ({}/{} char):\n{}",
                    mem_txt.chars().count(),
                    ctx.memory_cfg.max_memory_chars,
                    if mem_txt.is_empty() { "(kosong)" } else { &mem_txt },
                    prof_txt.chars().count(),
                    ctx.memory_cfg.max_profile_chars,
                    if prof_txt.is_empty() { "(kosong)" } else { &prof_txt },
                ))
            }
            "add" => {
                let content = str_arg(&args, "content")
                    .ok_or_else(|| anyhow!("argumen 'content' wajib diisi untuk action='add'"))?;
                let (count, chars) = add_entry_in(&dir, kind, content, max_chars)?;
                Ok(format!(
                    "Tersimpan di wadah '{}' (entri #{count}, {chars}/{max_chars} char). Akan aktif pada snapshot sesi berikutnya.",
                    kind.label()
                ))
            }
            "replace" => {
                let selector = str_arg(&args, "selector")
                    .or_else(|| str_arg(&args, "old_text"))
                    .ok_or_else(|| anyhow!("argumen 'selector' wajib diisi untuk action='replace'"))?;
                let content = str_arg(&args, "content")
                    .ok_or_else(|| anyhow!("argumen 'content' wajib diisi untuk action='replace'"))?;
                let (idx, chars) = replace_entry_in(&dir, kind, selector, content, max_chars)?;
                Ok(format!(
                    "Entri #{idx} di wadah '{}' diperbarui ({chars}/{max_chars} char).",
                    kind.label()
                ))
            }
            "remove" | "delete" => {
                let selector = str_arg(&args, "selector")
                    .or_else(|| str_arg(&args, "old_text"))
                    .ok_or_else(|| anyhow!("argumen 'selector' wajib diisi untuk action='remove'"))?;
                let (removed, chars) = remove_entry_in(&dir, kind, selector, max_chars)?;
                Ok(format!(
                    "Entri dihapus dari '{}': \"{removed}\" (sisa {chars}/{max_chars} char).",
                    kind.label()
                ))
            }
            other => Err(anyhow!(
                "action '{other}' tidak dikenal (pilih: add, replace, remove, view)"
            )),
        }
    }
}
