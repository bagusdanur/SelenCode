//! Tool memori persisten SelenCode (`memory`, Perbaikan 5 v0.5.7 & Perbaikan 3 v0.5.9).
//!
//! SATU sumber kebenaran: wadah persisten `memory.md` (pagu 2.200 char) & `profile.md`
//! (pagu 1.375 char) dengan snapshot beku di awal sesi.
//! Tool lama `manage_memory` telah dipensiunkan dari registry aktif & toolset agar
//! tidak ada dua tempat penyimpanan memori yang berbeda. Struct `ManageMemory`
//! dipertahankan hanya sebagai alias tipis ke store yang sama untuk kompatibilitas kompilasi.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Alias kompatibilitas yang sudah dipensiunkan dari registry aktif — seluruh operasi
/// diarahkan ke store persisten tunggal `user_memory` (TIDAK pernah menulis ke SQLite lama).
pub struct ManageMemory;

#[async_trait]
impl Tool for ManageMemory {
    fn spec(&self) -> ToolSpec {
        UserMemoryTool.spec()
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let action = str_arg(&args, "action").unwrap_or("view");
        let mapped = match action {
            "save" => json!({
                "action": "add",
                "target": "memory",
                "content": args.get("content").cloned().unwrap_or(Value::Null),
            }),
            "update" => json!({
                "action": "replace",
                "target": "memory",
                "selector": args.get("id").map(|v| v.to_string()).unwrap_or_default(),
                "content": args.get("content").cloned().unwrap_or(Value::Null),
            }),
            "delete" => json!({
                "action": "remove",
                "target": "memory",
                "selector": args.get("id").map(|v| v.to_string()).unwrap_or_default(),
            }),
            _ => json!({ "action": "view", "target": "memory" }),
        };
        UserMemoryTool.run(mapped, ctx).await
    }
}

/// Tool `memory` (Perbaikan 5 v0.5.7 — wadah `memory` 2.200 char & `profile` 1.375 char terstruktur).
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
