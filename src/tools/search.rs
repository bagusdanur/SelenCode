use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub const MAX_RESULTS: usize = 100;
const MAX_FILE_BYTES: u64 = 1_000_000;

/// search_text — regex di seluruh folder project (hormati .gitignore, maks 100 hasil).
pub struct SearchText;

#[async_trait]
impl Tool for SearchText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_text".into(),
            description: "Cari teks/regex di file project. Hasil format path:baris: teks, maks 100.".into(),
            parameters: schema(
                json!({"query": {"type": "string"}, "path": {"type": "string", "description": "folder awal, default ."}}),
                &["query"],
            ),
        }
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let query = str_arg(&args, "query").ok_or_else(|| anyhow!("argumen 'query' wajib"))?;
        let re = regex::Regex::new(query).map_err(|e| anyhow!("regex tidak valid: {e}"))?;
        let sub = str_arg(&args, "path").unwrap_or(".");
        let root = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(sub))?;
        let mut out = Vec::new();
        'walk: for e in ignore::WalkBuilder::new(&root).require_git(false).build().flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = e.path();
            if is_secret_file(p) {
                continue;
            }
            if e.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(p) else { continue };
            if bytes.contains(&0) {
                continue; // biner, lewati
            }
            let text = String::from_utf8_lossy(&bytes);
            let rel = p.strip_prefix(&ctx.cwd).unwrap_or(p);
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{}:{}: {}", rel.display(), i + 1, line.trim()));
                    if out.len() >= MAX_RESULTS {
                        break 'walk;
                    }
                }
            }
        }
        if out.is_empty() {
            return Ok(format!("Tidak ada hasil untuk '{query}'."));
        }
        Ok(out.join("\n"))
    }
}
