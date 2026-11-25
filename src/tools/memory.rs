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
