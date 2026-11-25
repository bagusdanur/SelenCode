//! Tool `session_search` — Pencarian lintas sesi berbasis SQLite FTS5 (v0.5.6 Bagian 4A).
//!
//! Mencari hanya dari data transkrip teredaksi yang tersimpan di DB sesi lokal,
//! mengembalikan maks 10 hasil terurut relevansi dengan cuplikan pendek (hemat token).

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct SessionSearch;

#[async_trait]
impl Tool for SessionSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "session_search".into(),
            description: "Cari kata kunci/frasa pada riwayat sesi percakapan tersimpan (FTS5). Mengembalikan maks 10 sesi paling relevan beserta id sesi, judul, waktu, dan cuplikan singkat.".into(),
            parameters: schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Kata kunci atau frasa yang dicari pada riwayat sesi"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Opsional: jumlah hasil maksimum (default 10, maks 10)"
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let store = ctx
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("penyimpanan sesi (SQLite store) tidak aktif"))?;
        let query = str_arg(&args, "query")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'query' wajib diisi"))?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 10))
            .unwrap_or(10);

        let _ = store.backfill_fts_if_needed();
        let hits = store.search_sessions_fts(query, None, limit)?;
        if hits.is_empty() {
            return Ok(format!(
                "Tidak ditemukan sesi tersimpan yang cocok dengan \"{query}\" di database sesi lokal."
            ));
        }
        let mut out = format!(
            "Hasil pencarian sesi untuk \"{query}\" ({} ditemukan, maks {limit}):\n",
            hits.len()
        );
        for (idx, h) in hits.iter().enumerate() {
            let short_id: String = h.session_id.chars().take(10).collect();
            let date_short: String = h.created_at.chars().take(19).collect();
            out.push_str(&format!(
                "{}. [{}] \"{}\" ({}) — id: `{}`\n   {}\n",
                idx + 1,
                short_id,
                h.title,
                date_short,
                h.session_id,
                h.snippet
            ));
        }
        out.push_str("\nLanjutkan sesi tertentu dengan `/resume <id_sesi>` (pencarian hanya mencakup teks tersimpan di DB sesi lokal).");
        Ok(out)
    }
}
