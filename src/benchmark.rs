//! Fase 4 — Agent Benchmark Engine (Pengukuran Objektif & Regresi Antar-Model).
//!
//! Mengevaluasi kemampuan nyata SelenCode dan model AI pada kumpulan tugas
//! deterministik (coding, bug fixing, refactoring, test generation, tool usage):
//! - Setiap task dijalankan dalam workspace sementara yang terisolasi + `SandboxController`.
//! - Memverifikasi hasil lewat exit code, keberadaan/isi file, diff, & cek regresi.
//! - Menghitung 7 metrik inti: Task Success Rate, Test Pass Rate, Regression Rate,
//!   Time to Completion, Tool Calls, Token & Cost, serta Recovery Rate.
//! - Mendukung penyimpanan hasil ke SQLite & perbandingan konsisten antar-model/versi.

use crate::agent::run_turn_counted;
use crate::config::SandboxCfg;
use crate::providers::{Block, Message, Provider, Role};
use crate::sandbox::SandboxController;
use crate::tools::{default_registry, Ctx, Registry};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Satu kriteria verifikasi objektif pada hasil benchmark task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BenchCheck {
    /// File relatif harus ada di workspace.
    FileExists { path: String },
    /// File relatif harus memuat potongan teks tertentu.
    FileContains { path: String, needle: String },
    /// File relatif TIDAK boleh memuat potongan teks tertentu (mis. bug lama / regresi).
    FileNotContains { path: String, needle: String },
    /// Perintah shell/test aman yang wajib keluar dengan exit code 0 (dijalankan lewat sandbox).
    CommandSucceeds {
        command: String,
        #[serde(default)]
        is_regression: bool,
    },
    /// Jawaban teks akhir agent harus memuat potongan teks tertentu.
    ReplyContains { needle: String },
}

/// Definisi satu tugas benchmark yang deterministik dan dapat diulang.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchTask {
    pub id: String,
    /// `coding` | `bug_fix` | `refactoring` | `test_gen` | `tool_usage`
    pub category: String,
    pub title: String,
    pub prompt: String,
    /// File awal repository fixture (`rel_path` -> `content`).
    #[serde(default)]
    pub fixture_files: BTreeMap<String, String>,
    /// Daftar pemeriksaan objektif yang menentukan kelulusan task.
    pub checks: Vec<BenchCheck>,
    /// Pemeriksaan regresi tambahan: bila sebelumnya lulus di fixture tapi rusak sesudah edit -> regresi!
    #[serde(default)]
    pub regression_checks: Vec<BenchCheck>,
    #[serde(default = "default_bench_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_bench_timeout")]
    pub timeout_secs: u64,
}

fn default_bench_max_steps() -> usize {
    12
}

fn default_bench_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchCheckOutcome {
    pub description: String,
    pub passed: bool,
    pub is_regression: bool,
    pub detail: String,
}

/// Hasil eksekusi 1 benchmark task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchTaskResult {
    pub task_id: String,
    pub category: String,
    pub title: String,
    pub succeeded: bool,
    pub had_regression: bool,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub elapsed_ms: u64,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub recovered_from_error: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub changed_files: Vec<String>,
    pub check_outcomes: Vec<BenchCheckOutcome>,
    pub error: Option<String>,
}

/// Laporan lengkap 1 kali eksekusi benchmark suite untuk satu (provider, model, versi).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRunReport {
    pub run_id: String,
    pub timestamp: String,
    pub selen_version: String,
    pub provider: String,
    pub model: String,
    pub total_tasks: usize,
    pub passed_tasks: usize,
    pub task_success_rate: f64,
    pub test_pass_rate: f64,
    pub regression_rate: f64,
    pub recovery_rate: f64,
    pub total_elapsed_ms: u64,
    pub total_tool_calls: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: Option<f64>,
    pub results: Vec<BenchTaskResult>,
}

impl BenchRunReport {
    pub fn format_markdown(&self) -> String {
        let cost_s = self
            .total_cost_usd
            .map(|c| format!("${c:.4}"))
            .unwrap_or_else(|| "n/a".to_string());
        let mut out = format!(
            "# SelenCode Benchmark Report ({})\n\
             - **Run ID:** `{}`\n\
             - **Versi:** `v{}`\n\
             - **Provider / Model:** `{}` / `{}`\n\
             - **Task Success Rate:** {:.1}% ({}/{})\n\
             - **Test Pass Rate:** {:.1}%\n\
             - **Regression Rate:** {:.1}%\n\
             - **Recovery Rate:** {:.1}%\n\
             - **Total Time:** {} ms | **Tool Calls:** {}\n\
             - **Tokens:** {} in / {} out | **Est. Cost:** {}\n\n\
             | Task | Kategori | Status | Checks | Regresi | Tool Calls | Waktu (ms) |\n\
             |---|---|---|---:|---|---:|---:|\n",
            self.timestamp,
            self.run_id,
            self.selen_version,
            self.provider,
            self.model,
            self.task_success_rate * 100.0,
            self.passed_tasks,
            self.total_tasks,
            self.test_pass_rate * 100.0,
            self.regression_rate * 100.0,
            self.recovery_rate * 100.0,
            self.total_elapsed_ms,
            self.total_tool_calls,
            self.total_input_tokens,
            self.total_output_tokens,
            cost_s
        );
        for r in &self.results {
            let st = if r.succeeded { "PASS" } else { "FAIL" };
            let reg = if r.had_regression { "YA" } else { "tidak" };
            out.push_str(&format!(
                "| `{}` | {} | {} | {}/{} | {} | {} | {} |\n",
                r.task_id,
                r.category,
                st,
                r.checks_passed,
                r.checks_total,
                reg,
                r.tool_calls,
                r.elapsed_ms
            ));
        }
        out
    }
}

/// Format tabel perbandingan lintas beberapa hasil benchmark (antar model / versi).
pub fn format_comparison_table(runs: &[BenchRunReport]) -> String {
    if runs.is_empty() {
        return "Belum ada riwayat benchmark tersimpan.".to_string();
    }
    let mut out = String::from(
        "| Run | Versi | Provider / Model | Success | Tests | Regresi | Tools | Waktu (ms) | Tokens |\n\
         |---|---|---|---:|---:|---:|---:|---:|---:|\n",
    );
    for r in runs {
        out.push_str(&format!(
            "| `{}` | v{} | `{}/{}` | {:.1}% ({}/{}) | {:.1}% | {:.1}% | {} | {} | {} |\n",
            r.run_id,
            r.selen_version,
            r.provider,
            r.model,
            r.task_success_rate * 100.0,
            r.passed_tasks,
            r.total_tasks,
            r.test_pass_rate * 100.0,
            r.regression_rate * 100.0,
            r.total_tool_calls,
            r.total_elapsed_ms,
            r.total_input_tokens + r.total_output_tokens
        ));
    }
    out
}

/// Kumpulan 5 benchmark task bawaan yang deterministik mencakup semua kategori inti.
pub fn builtin_benchmark_suite() -> Vec<BenchTask> {
    vec![
        BenchTask {
            id: "bench-coding-01".into(),
            category: "coding".into(),
            title: "Implementasi fungsi slugify".into(),
            prompt: "Buat file `slug.rs` yang memuat fungsi `pub fn slugify(input: &str) -> String` \
                     untuk mengubah teks menjadi huruf kecil dengan tanda hubung `-`.".into(),
            fixture_files: BTreeMap::new(),
            checks: vec![
                BenchCheck::FileExists { path: "slug.rs".into() },
                BenchCheck::FileContains {
                    path: "slug.rs".into(),
                    needle: "pub fn slugify".into(),
                },
            ],
            regression_checks: Vec::new(),
            max_steps: 8,
            timeout_secs: 45,
        },
        BenchTask {
            id: "bench-bugfix-01".into(),
            category: "bug_fix".into(),
            title: "Perbaiki bug perhitungan diskon tanpa merusak subtotal".into(),
            prompt: "Perbaiki bug di `pricing.rs`: fungsi `apply_discount(price: u64, pct: u64) -> u64` \
                     saat ini MENAMBAH diskon bukannya MENGURANGI. Ubah agar mengurangi diskon, \
                     tanpa mengubah `compute_subtotal`.".into(),
            fixture_files: BTreeMap::from([(
                "pricing.rs".into(),
                "pub fn compute_subtotal(a: u64, b: u64) -> u64 {\n    a + b\n}\n\n\
                 pub fn apply_discount(price: u64, pct: u64) -> u64 {\n    price + (price * pct / 100)\n}\n"
                    .into(),
            )]),
            checks: vec![
                BenchCheck::FileContains {
                    path: "pricing.rs".into(),
                    needle: "price -".into(),
                },
                BenchCheck::FileNotContains {
                    path: "pricing.rs".into(),
                    needle: "price + (price * pct / 100)".into(),
                },
            ],
            regression_checks: vec![BenchCheck::FileContains {
                path: "pricing.rs".into(),
                needle: "pub fn compute_subtotal(a: u64, b: u64) -> u64".into(),
            }],
            max_steps: 8,
            timeout_secs: 45,
        },
        BenchTask {
            id: "bench-refactor-01".into(),
            category: "refactoring".into(),
            title: "Rename simbol aman lintas 2 file".into(),
            prompt: "Rename fungsi `calc_old` menjadi `calc_total` pada `math.rs` dan `app.rs` \
                     tanpa mengubah `calc_old_backup`.".into(),
            fixture_files: BTreeMap::from([
                (
                    "math.rs".into(),
                    "pub fn calc_old(x: i32) -> i32 { x * 2 }\npub fn calc_old_backup(x: i32) -> i32 { x }\n"
                        .into(),
                ),
                (
                    "app.rs".into(),
                    "fn run() -> i32 { calc_old(10) }\n".into(),
                ),
            ]),
            checks: vec![
                BenchCheck::FileContains {
                    path: "math.rs".into(),
                    needle: "pub fn calc_total".into(),
                },
                BenchCheck::FileContains {
                    path: "app.rs".into(),
                    needle: "calc_total(10)".into(),
                },
            ],
            regression_checks: vec![BenchCheck::FileContains {
                path: "math.rs".into(),
                needle: "pub fn calc_old_backup".into(),
            }],
            max_steps: 8,
            timeout_secs: 45,
        },
        BenchTask {
            id: "bench-testgen-01".into(),
            category: "test_gen".into(),
            title: "Tambahkan unit test untuk validator email".into(),
            prompt: "Baca `validator.rs` lalu tambahkan modul `#[cfg(test)] mod tests` di bagian bawah \
                     file yang menguji `is_valid_email` untuk input valid dan tidak valid.".into(),
            fixture_files: BTreeMap::from([(
                "validator.rs".into(),
                "pub fn is_valid_email(s: &str) -> bool {\n    s.contains('@') && s.contains('.')\n}\n"
                    .into(),
            )]),
            checks: vec![
                BenchCheck::FileContains {
                    path: "validator.rs".into(),
                    needle: "#[cfg(test)]".into(),
                },
                BenchCheck::FileContains {
                    path: "validator.rs".into(),
                    needle: "#[test]".into(),
                },
            ],
            regression_checks: vec![BenchCheck::FileContains {
                path: "validator.rs".into(),
                needle: "pub fn is_valid_email".into(),
            }],
            max_steps: 8,
            timeout_secs: 45,
        },
        BenchTask {
            id: "bench-tools-01".into(),
            category: "tool_usage".into(),
            title: "Analisis multi-file & laporkan temuan".into(),
            prompt: "Baca `config.txt` dan `version.txt`, lalu sebutkan nilai PORT dan APP_VERSION \
                     dalam jawaban akhirmu.".into(),
            fixture_files: BTreeMap::from([
                ("config.txt".into(), "HOST=127.0.0.1\nPORT=8089\n".into()),
                ("version.txt".into(), "APP_VERSION=2.4.1\n".into()),
            ]),
            checks: vec![
                BenchCheck::ReplyContains { needle: "8089".into() },
                BenchCheck::ReplyContains { needle: "2.4.1".into() },
            ],
            regression_checks: Vec::new(),
            max_steps: 8,
            timeout_secs: 45,
        },
    ]
}

/// Muat suite benchmark kustom dari file JSON (`Vec<BenchTask>`).
pub fn load_suite_from_file(path: &Path) -> Result<Vec<BenchTask>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("gagal membaca file benchmark {}", path.display()))?;
    let tasks: Vec<BenchTask> = serde_json::from_str(&text)
        .with_context(|| format!("format JSON benchmark tidak valid di {}", path.display()))?;
    if tasks.is_empty() {
        return Err(anyhow!("file benchmark kosong (0 task)"));
    }
    Ok(tasks)
}

/// Jalankan seluruh `tasks` secara terisolasi satu per satu dan hasilkan `BenchRunReport`.
pub async fn run_benchmark_suite(
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model_name: &str,
    tasks: &[BenchTask],
    price_in: Option<f64>,
    price_out: Option<f64>,
) -> Result<BenchRunReport> {
    if tasks.is_empty() {
        return Err(anyhow!("daftar task benchmark kosong"));
    }
    let reg = default_registry();
    let mut results = Vec::with_capacity(tasks.len());

    for task in tasks {
        let res = run_single_bench_task(
            provider.clone(),
            &reg,
            task,
            price_in,
            price_out,
        )
        .await;
        results.push(res);
    }

    let total_tasks = results.len();
    let passed_tasks = results.iter().filter(|r| r.succeeded).count();
    let total_checks: usize = results.iter().map(|r| r.checks_total).sum();
    let passed_checks: usize = results.iter().map(|r| r.checks_passed).sum();
    let regressed_tasks = results.iter().filter(|r| r.had_regression).count();
    let tasks_with_errors = results.iter().filter(|r| r.tool_errors > 0).count();
    let recovered_tasks = results.iter().filter(|r| r.recovered_from_error).count();

    let total_elapsed_ms: u64 = results.iter().map(|r| r.elapsed_ms).sum();
    let total_tool_calls: usize = results.iter().map(|r| r.tool_calls).sum();
    let total_input_tokens: u64 = results.iter().map(|r| r.input_tokens).sum();
    let total_output_tokens: u64 = results.iter().map(|r| r.output_tokens).sum();
    let total_cost_usd = if price_in.is_some() && price_out.is_some() {
        Some(results.iter().filter_map(|r| r.estimated_cost_usd).sum())
    } else {
        None
    };

    let short_id = &uuid::Uuid::new_v4().simple().to_string()[..8];
    Ok(BenchRunReport {
        run_id: format!("bench-{short_id}"),
        timestamp: chrono::Utc::now().to_rfc3339(),
        selen_version: env!("CARGO_PKG_VERSION").to_string(),
        provider: provider_name.to_string(),
        model: model_name.to_string(),
        total_tasks,
        passed_tasks,
        task_success_rate: passed_tasks as f64 / total_tasks as f64,
        test_pass_rate: if total_checks == 0 {
            1.0
        } else {
            passed_checks as f64 / total_checks as f64
        },
        regression_rate: regressed_tasks as f64 / total_tasks as f64,
        recovery_rate: if tasks_with_errors == 0 {
            1.0
        } else {
            recovered_tasks as f64 / tasks_with_errors as f64
        },
        total_elapsed_ms,
        total_tool_calls,
        total_input_tokens,
        total_output_tokens,
        total_cost_usd,
        results,
    })
}

async fn run_single_bench_task(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    task: &BenchTask,
    price_in: Option<f64>,
    price_out: Option<f64>,
) -> BenchTaskResult {
    let ws = std::env::temp_dir().join(format!(
        "selencode-bench-{}-{}",
        task.id,
        uuid::Uuid::new_v4().simple()
    ));
    let _ = std::fs::create_dir_all(&ws);

    // Tulis seluruh fixture file ke workspace terisolasi.
    for (rel, content) in &task.fixture_files {
        let target = ws.join(rel);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(target, content);
    }

    let sandbox = Arc::new(SandboxController::from_config(
        &SandboxCfg {
            audit_log: false,
            ..SandboxCfg::default()
        },
        ws.join("bench-audit.jsonl"),
    ));
    let ctx = Ctx::new(ws.clone(), true).with_sandbox(sandbox.clone());
    let mut messages: Vec<Message> = Vec::new();

    let started = Instant::now();
    let timeout = Duration::from_secs(task.timeout_secs.max(5));
    let run_res = tokio::time::timeout(
        timeout,
        run_turn_counted(
            provider,
            registry,
            &ctx,
            &mut messages,
            &task.prompt,
            task.max_steps.max(1),
        ),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let (reply_text, usage, run_err) = match run_res {
        Ok(Ok((txt, u))) => (txt, u, None),
        Ok(Err(e)) => (String::new(), None, Some(e.to_string())),
        Err(_) => (
            String::new(),
            None,
            Some(format!("timeout setelah {} detik", task.timeout_secs)),
        ),
    };

    let (tool_calls, tool_errors) = count_tool_activity(&messages);
    let mut outcomes = Vec::new();
    for chk in &task.checks {
        outcomes.push(evaluate_check(&ws, chk, &reply_text, &sandbox, false).await);
    }
    for rchk in &task.regression_checks {
        outcomes.push(evaluate_check(&ws, rchk, &reply_text, &sandbox, true).await);
    }

    let checks_total = outcomes.len();
    let checks_passed = outcomes.iter().filter(|o| o.passed).count();
    let had_regression = outcomes.iter().any(|o| o.is_regression && !o.passed);
    let succeeded = run_err.is_none() && checks_passed == checks_total && !had_regression;
    let recovered_from_error = tool_errors > 0 && succeeded;

    let changed_files = detect_changed_fixture_files(&ws, &task.fixture_files);
    let input_tokens = usage.map(|u| u.input_tokens).unwrap_or(0);
    let output_tokens = usage.map(|u| u.output_tokens).unwrap_or(0);
    let estimated_cost_usd = match (price_in, price_out) {
        (Some(pin), Some(pout)) => Some(
            (input_tokens as f64 / 1_000_000.0) * pin
                + (output_tokens as f64 / 1_000_000.0) * pout,
        ),
        _ => None,
    };

    let _ = std::fs::remove_dir_all(&ws);

    BenchTaskResult {
        task_id: task.id.clone(),
        category: task.category.clone(),
        title: task.title.clone(),
        succeeded,
        had_regression,
        checks_passed,
        checks_total,
        elapsed_ms,
        tool_calls,
        tool_errors,
        recovered_from_error,
        input_tokens,
        output_tokens,
        estimated_cost_usd,
        changed_files,
        check_outcomes: outcomes,
        error: run_err,
    }
}

fn count_tool_activity(messages: &[Message]) -> (usize, usize) {
    let mut calls = 0usize;
    let mut errs = 0usize;
    for m in messages {
        for b in &m.content {
            match b {
                Block::ToolUse { .. } if m.role == Role::Assistant => calls += 1,
                Block::ToolResult { is_error: true, .. } => errs += 1,
                _ => {}
            }
        }
    }
    (calls, errs)
}

async fn evaluate_check(
    ws: &Path,
    check: &BenchCheck,
    reply_text: &str,
    sandbox: &SandboxController,
    force_regression: bool,
) -> BenchCheckOutcome {
    match check {
        BenchCheck::FileExists { path } => {
            let ok = ws.join(path).is_file();
            BenchCheckOutcome {
                description: format!("file_exists({path})"),
                passed: ok,
                is_regression: force_regression,
                detail: if ok { "ada".into() } else { "tidak ditemukan".into() },
            }
        }
        BenchCheck::FileContains { path, needle } => {
            let txt = std::fs::read_to_string(ws.join(path)).unwrap_or_default();
            let ok = txt.contains(needle.as_str());
            BenchCheckOutcome {
                description: format!("file_contains({path}, {needle:?})"),
                passed: ok,
                is_regression: force_regression,
                detail: if ok { "cocok".into() } else { "tidak ditemukan".into() },
            }
        }
        BenchCheck::FileNotContains { path, needle } => {
            let txt = std::fs::read_to_string(ws.join(path)).unwrap_or_default();
            let ok = !txt.contains(needle.as_str());
            BenchCheckOutcome {
                description: format!("file_not_contains({path}, {needle:?})"),
                passed: ok,
                is_regression: force_regression,
                detail: if ok { "bersih".into() } else { "masih muncul".into() },
            }
        }
        BenchCheck::ReplyContains { needle } => {
            let ok = reply_text.contains(needle.as_str());
            BenchCheckOutcome {
                description: format!("reply_contains({needle:?})"),
                passed: ok,
                is_regression: force_regression,
                detail: if ok { "cocok".into() } else { "tidak ada di jawaban".into() },
            }
        }
        BenchCheck::CommandSucceeds {
            command,
            is_regression,
        } => {
            let is_reg = force_regression || *is_regression;
            match sandbox.prepare_shell_command("bench_check", command, ws) {
                Ok(mut cmd) => match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await
                {
                    Ok(Ok(out)) => {
                        let ok = out.status.success();
                        BenchCheckOutcome {
                            description: format!("command({command})"),
                            passed: ok,
                            is_regression: is_reg,
                            detail: format!("exit {:?}", out.status.code()),
                        }
                    }
                    Ok(Err(e)) => BenchCheckOutcome {
                        description: format!("command({command})"),
                        passed: false,
                        is_regression: is_reg,
                        detail: format!("error: {e}"),
                    },
                    Err(_) => BenchCheckOutcome {
                        description: format!("command({command})"),
                        passed: false,
                        is_regression: is_reg,
                        detail: "timeout".into(),
                    },
                },
                Err(e) => BenchCheckOutcome {
                    description: format!("command({command})"),
                    passed: false,
                    is_regression: is_reg,
                    detail: format!("sandbox error: {e}"),
                },
            }
        }
    }
}

fn detect_changed_fixture_files(ws: &Path, fixtures: &BTreeMap<String, String>) -> Vec<String> {
    let mut changed = Vec::new();
    for (rel, orig) in fixtures {
        let now = std::fs::read_to_string(ws.join(rel)).unwrap_or_default();
        if &now != orig {
            changed.push(rel.clone());
        }
    }
    if let Ok(entries) = std::fs::read_dir(ws) {
        for ent in entries.flatten() {
            if ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let name = ent.file_name().to_string_lossy().to_string();
                if name != "bench-audit.jsonl" && !fixtures.contains_key(&name) {
                    changed.push(name);
                }
            }
        }
    }
    changed.sort();
    changed
}
