//! Tool `retrieve_context` (Tahap 2 — Smart Context Retrieval).
//!
//! Menggunakan indeks repositori inkremental (`Ctx.repo_index`) berbasis
//! Tree-sitter + pelacakan import untuk menemukan file, simbol, dan potongan
//! kode paling relevan sesuai kueri dengan batas budget token yang ketat.

use super::{schema, str_arg, Ctx, Tool};
use crate::context_index::format_hits_for_prompt;
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct RetrieveContext;

#[async_trait]
impl Tool for RetrieveContext {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "retrieve_context".into(),
            description: "Cari file, simbol (fn/struct/class/trait), ketergantungan import, dan cuplikan kode paling relevan dari indeks repositori inkremental. Hemat token dibanding membaca banyak file penuh.".into(),
            parameters: schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Kata kunci, nama fungsi/struct/class, pesan error, atau topik fitur yang dicari"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Jumlah maksimum file teratas (default 5, maks 12)"
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Batas budget ukuran output dalam byte (default 8192, maks 20480)"
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let query = str_arg(&args, "query")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'query' wajib diisi"))?;
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 12))
            .unwrap_or(5);
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1024, 20 * 1024))
            .unwrap_or(8 * 1024);

        let hits = {
            let mut idx = ctx.repo_index.lock().expect("repo_index poisoned");
            idx.query(query, max_results, max_bytes)
        };
        if hits.is_empty() {
            return Ok(format!(
                "Tidak ditemukan simbol atau file relevan untuk kueri '{query}'. Coba kata kunci lebih spesifik atau gunakan search_text."
            ));
        }
        let body = format_hits_for_prompt(&hits, max_bytes);
        Ok(format!(
            "Ditemukan {} file relevan untuk '{query}' (diurutkan berdasarkan skor simbol, path, & import):\n\n{body}",
            hits.len()
        ))
    }
}
