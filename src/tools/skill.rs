//! Tool `manage_skill` — sistem skill otonom & self-improvement ala Hermes Agent.
//!
//! Memungkinkan Selena membaca skill yang sudah ada sebelum mengerjakan tugas,
//! membuat skill baru (`create`), atau memperbaiki/memperkaya skill yang sudah
//! ada (`patch`) di `.selencode/skills/<nama>.md`. Semua nama disanitasi dan
//! isi diredaksi dari secret sebelum ditulis.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::skills::{list_skills, load_skill, upsert_skill};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ManageSkill;

#[async_trait]
impl Tool for ManageSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "manage_skill".into(),
            description: "Kelola skill prosedural project di `.selencode/skills/*.md` (ala Hermes Agent). \
action='list' untuk melihat daftar, action='read' untuk membaca prosedur skill sebelum bekerja, \
action='create' untuk menyimpan workflow/solusi baru yang reusable, action='patch' untuk memperbaiki/melengkapi skill yang sudah ada."
                .into(),
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
                let lines: Vec<String> = skills
                    .iter()
                    .map(|s| format!("- {} [{}]: {}", s.name, s.scope, s.summary))
                    .collect();
                Ok(lines.join("\n"))
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
