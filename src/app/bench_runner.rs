//! Runner CLI `selen bench` (Fase 4 — Agent Benchmark Engine).

use super::make_routed_provider;
use anyhow::Result;
use selencode::benchmark::{
    builtin_benchmark_suite, format_comparison_table, load_suite_from_file, run_benchmark_suite,
};
use selencode::config;
use selencode::providers::router::RouterTelemetry;
use selencode::store::Store;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) async fn run_bench_cli(
    provider_override: Option<&str>,
    suite_path: Option<&str>,
    filter: Option<&str>,
    out_path: Option<&str>,
    history_only: bool,
) -> Result<()> {
    let store = Store::open()?;
    if history_only {
        let runs = store.list_benchmark_runs(15)?;
        println!("{}", format_comparison_table(&runs));
        return Ok(());
    }

    let cfg = config::load()?;
    let (prov_name, pcfg, key) = cfg.resolve_provider(provider_override)?;
    let telemetry = Arc::new(Mutex::new(RouterTelemetry::default()));
    let provider = make_routed_provider(&cfg, &prov_name, &pcfg, &key, telemetry)?;

    let mut tasks = match suite_path {
        Some(p) => load_suite_from_file(Path::new(p))?,
        None => builtin_benchmark_suite(),
    };
    if let Some(f) = filter.map(str::trim).filter(|s| !s.is_empty()) {
        let needle = f.to_ascii_lowercase();
        tasks.retain(|t| {
            t.id.to_ascii_lowercase().contains(&needle)
                || t.category.to_ascii_lowercase().contains(&needle)
        });
        if tasks.is_empty() {
            anyhow::bail!("tidak ada task benchmark yang cocok dengan filter '{f}'");
        }
    }

    println!(
        "☾ Menjalankan {} benchmark task pada {}/{}…\n",
        tasks.len(),
        prov_name,
        pcfg.model
    );
    let report = run_benchmark_suite(
        provider,
        &prov_name,
        &pcfg.model,
        &tasks,
        pcfg.price_in,
        pcfg.price_out,
    )
    .await?;
    let _ = store.save_benchmark_run(&report);
    // Umpan balik langsung hasil benchmark objektif ke tabel `model_perf` untuk Intelligent Routing!
    let route_id = format!("{prov_name}:{}", pcfg.model);
    for tr in &report.results {
        let task_kind = match tr.category.as_str() {
            "bug_fix" => "debugging",
            "refactoring" | "test_gen" | "coding" => "coding",
            _ => "general",
        };
        let _ = store.record_model_outcome(
            &route_id,
            task_kind,
            tr.succeeded,
            tr.elapsed_ms,
            tr.input_tokens + tr.output_tokens,
            tr.estimated_cost_usd.unwrap_or(0.0),
        );
    }

    println!("{}", report.format_markdown());
    if let Some(out) = out_path {
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(out, json)?;
        println!("Laporan JSON disimpan ke: {out}");
    }
    Ok(())
}
