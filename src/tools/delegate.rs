//! Tool `delegate` (Fase 6): tugaskan 1-5 penjelajahan INDEPENDEN ke
//! subagent baca-saja yang berjalan PARALEL (dibatasi semaphore).
//!
//! Rekursi mustahil secara konstruksi: registry anak (dibangun di
//! tools::read_only_registry) tidak memuat `delegate` maupun `update_plan`.
//! Hasil digabung dengan urutan SESUAI urutan input, bukan urutan selesai;
//! kegagalan satu anak tidak menggagalkan yang lain.

use super::{schema, Ctx, Registry, Tool};
use crate::agent::subagent::run_subagent;
use crate::providers::{Provider, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use tokio::sync::Semaphore;

pub const MAX_DELEGATE_TASKS: usize = 5;

pub struct Delegate {
    provider: Arc<RwLock<Arc<dyn Provider>>>,
    child_registry: Arc<Registry>,
    max_parallel: usize,
}

impl Delegate {
    /// Konstruktor publik agar bisa dibangun & dites terpisah dari main.
    /// `child_registry` WAJIB registry baca-saja (tools::read_only_registry).
    pub fn new(
        provider: Arc<dyn Provider>,
        child_registry: Arc<Registry>,
        max_parallel: usize,
    ) -> Self {
        Self::new_shared(Arc::new(RwLock::new(provider)), child_registry, max_parallel)
    }

    /// Konstruktor dengan handle provider bersama agar pergantian `/model` di sesi induk
    /// otomatis ikut memperbarui provider subagent `delegate` (P2.2).
    pub fn new_shared(
        provider: Arc<RwLock<Arc<dyn Provider>>>,
        child_registry: Arc<Registry>,
        max_parallel: usize,
    ) -> Self {
        Self {
            provider,
            child_registry,
            max_parallel: max_parallel.max(1),
        }
    }
}

fn short_prompt(p: &str) -> String {
    let first = p.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(80).collect();
    if first.chars().count() > 80 {
        out.push('…');
    }
    out
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description: "Tugaskan 1-5 penjelajahan/pencarian INDEPENDEN ke subagent \
baca-saja yang berjalan paralel, lalu terima ringkasan temuan mereka. \
Pakai untuk memeriksa beberapa file/folder/topik sekaligus. Subagent TIDAK \
bisa menulis file / menjalankan shell / commit — jangan delegasikan perubahan."
                .into(),
            parameters: schema(
                json!({"tasks": {"type": "array", "minItems": 1, "maxItems": 5, "items": {
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}},
                    "required": ["prompt"]
                }}}),
                &["tasks"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let arr = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("argumen 'tasks' wajib berupa array"))?;
        if arr.is_empty() {
            return Err(anyhow!("tasks kosong — beri 1 sampai {MAX_DELEGATE_TASKS} tugas"));
        }
        if arr.len() > MAX_DELEGATE_TASKS {
            return Err(anyhow!(
                "terlalu banyak tugas: {} (maks {MAX_DELEGATE_TASKS} per delegate)",
                arr.len()
            ));
        }
        let mut prompts: Vec<String> = Vec::new();
        for (i, t) in arr.iter().enumerate() {
            let p = t
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("tasks[{i}].prompt wajib string non-kosong"))?;
            prompts.push(p);
        }

        let active_provider = self
            .provider
            .read()
            .expect("shared provider lock poisoned")
            .clone();
        let semaphore = Arc::new(Semaphore::new(self.max_parallel));
        let mut futs = Vec::new();
        for prompt in prompts.iter() {
            let provider = active_provider.clone();
            let registry = self.child_registry.clone();
            let ctx = ctx.clone();
            let sem = semaphore.clone();
            let prompt = prompt.clone();
            futs.push(async move {
                // Permit ditahan selama anak berjalan; join_all tetap
                // menjadwalkan semua, semaphore yang membatasi paralelnya.
                let _permit = sem.acquire_owned().await.expect("semaphore delegate");
                run_subagent(&provider, &registry, &ctx, &prompt).await
            });
        }
        // join_all mempertahankan urutan input pada hasil.
        let results = futures::future::join_all(futs).await;

        let mut out = String::new();
        for (i, (prompt, res)) in prompts.iter().zip(results).enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            let body = match res {
                Ok((text, usage)) => {
                    // Agregasi usage anak ke sesi induk (Fase 3 store):
                    // dicatat atas nama sesi aktif agar total sesi &
                    // ringkasan per giliran induk ikut menghitung anak.
                    if let (Some(store), Some(sid), Some(u)) = (&ctx.store, &ctx.session_id, usage)
                    {
                        let _ = store.record_usage(
                            sid,
                            active_provider.model_name(),
                            u.input_tokens,
                            u.output_tokens,
                        );
                    }
                    if text.trim().is_empty() {
                        "(subagent selesai tanpa teks jawaban)".to_string()
                    } else {
                        text
                    }
                }
                Err(e) => format!("ERROR: subagent gagal: {e}"),
            };
            out.push_str(&format!("### Tugas {}: {}\n{}", i + 1, short_prompt(prompt), body));
        }
        Ok(out)
    }
}
