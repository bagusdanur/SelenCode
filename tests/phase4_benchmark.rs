//! Tes Fase 4 — Agent Benchmark Engine (eksekusi terisolasi, metrik lengkap, regresi, & SQLite history).

use async_trait::async_trait;
use selencode::benchmark::{
    builtin_benchmark_suite, format_comparison_table, load_suite_from_file, run_benchmark_suite,
    BenchCheck, BenchTask,
};
use selencode::providers::{
    AssistantReply, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::store::Store;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

struct BenchMockProvider {
    call_idx: Arc<AtomicUsize>,
    /// Jika true, gagal dulu 1 tool call sebelum memanggil edit yang benar (uji recovery_rate!).
    trigger_recoverable_error: bool,
}

#[async_trait]
impl Provider for BenchMockProvider {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        let n = self.call_idx.fetch_add(1, Ordering::SeqCst);
        let user_prompt = msgs
            .iter()
            .find(|m| m.role == selencode::providers::Role::User)
            .map(|m| m.plain_text())
            .unwrap_or_default();

        // Task 1: coding (`slug.rs`)
        if user_prompt.contains("slug.rs") {
            if msgs.len() == 2 && self.trigger_recoverable_error {
                // Panggilan pertama sengaja gagal (baca file tidak ada) -> lalu pulih di langkah 2!
                return Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("c_{n}"),
                        name: "read_file".into(),
                        args: json!({"path": "tidak_ada.rs"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 200,
                        output_tokens: 50,
                        ..Default::default()
                    }),
                });
            }
            if (msgs.len() == 2 && !self.trigger_recoverable_error) || msgs.len() == 4 {
                return Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("c_{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": "slug.rs",
                            "content": "pub fn slugify(input: &str) -> String { input.to_lowercase().replace(' ', \"-\") }\n"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 300,
                        output_tokens: 80,
                        ..Default::default()
                    }),
                });
            }
            return Ok(AssistantReply {
                text: "slug.rs selesai dibuat.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 250,
                    output_tokens: 40,
                    ..Default::default()
                }),
            });
        }

        // Task 2: sengaja merusak fungsi lama -> memicu regression check!
        if user_prompt.contains("pricing.rs") {
            if msgs.len() == 2 {
                return Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("c_{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": "pricing.rs",
                            "content": "pub fn apply_discount(price: u64, pct: u64) -> u64 { price - (price * pct / 100) }\n"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 300,
                        output_tokens: 80,
                        ..Default::default()
                    }),
                });
            }
            return Ok(AssistantReply {
                text: "pricing.rs diperbaiki.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 200,
                    output_tokens: 30,
                    ..Default::default()
                }),
            });
        }

        Ok(AssistantReply {
            text: "ok".into(),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: None,
        })
    }

    fn model_name(&self) -> &str {
        "mock-bench-v1"
    }
}

#[tokio::test]
async fn benchmark_engine_runs_isolated_tasks_measures_metrics_regressions_and_persists_history() {
    let suite = builtin_benchmark_suite();
    assert_eq!(suite.len(), 5);

    // Uji serialisasi + pemuatan suite JSON kustom
    let suite_file = std::env::temp_dir().join(format!(
        "selencode-suite-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    let two_tasks = vec![suite[0].clone(), suite[1].clone()];
    std::fs::write(&suite_file, serde_json::to_string_pretty(&two_tasks).unwrap()).unwrap();
    let loaded = load_suite_from_file(&suite_file).unwrap();
    assert_eq!(loaded.len(), 2);
    std::fs::remove_file(&suite_file).ok();

    let provider: Arc<dyn Provider> = Arc::new(BenchMockProvider {
        call_idx: Arc::new(AtomicUsize::new(0)),
        trigger_recoverable_error: true,
    });

    let report = run_benchmark_suite(
        provider,
        "mock",
        "mock-bench-v1",
        &loaded,
        Some(2.0),
        Some(6.0),
    )
    .await
    .unwrap();

    assert_eq!(report.total_tasks, 2);
    // Task 1 (`bench-coding-01`): sempat error read_file lalu pulih & lulus!
    assert!(report.results[0].succeeded);
    assert!(report.results[0].recovered_from_error);
    assert!(report.results[0].tool_calls >= 2);
    assert!(report.results[0].changed_files.contains(&"slug.rs".to_string()));

    // Task 2 (`bench-bugfix-01`): menghapus `compute_subtotal` -> terdeteksi sebagai regresi!
    assert!(!report.results[1].succeeded);
    assert!(report.results[1].had_regression);

    assert!((report.task_success_rate - 0.5).abs() < 0.01);
    assert!((report.regression_rate - 0.5).abs() < 0.01);
    assert!((report.recovery_rate - 1.0).abs() < 0.01);
    assert!(report.total_cost_usd.unwrap() > 0.0);

    let md = report.format_markdown();
    assert!(md.contains("Task Success Rate"));
    assert!(md.contains("bench-coding-01"));

    // Simpan ke SQLite & bandingkan riwayat
    let db = std::env::temp_dir().join(format!(
        "selencode-bench-db-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&db).unwrap();
    store.save_benchmark_run(&report).unwrap();
    let history = store.list_benchmark_runs(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].run_id, report.run_id);

    let cmp = format_comparison_table(&history);
    assert!(cmp.contains("mock/mock-bench-v1"));
    std::fs::remove_file(&db).ok();
}

#[tokio::test]
async fn benchmark_command_check_runs_inside_sandbox() {
    let custom = vec![BenchTask {
        id: "bench-cmd-01".into(),
        category: "tool_usage".into(),
        title: "Cek command check".into(),
        prompt: "Balas singkat saja".into(),
        fixture_files: BTreeMap::new(),
        checks: vec![
            BenchCheck::ReplyContains { needle: "ok".into() },
            BenchCheck::CommandSucceeds {
                command: "echo bench_ok".into(),
                is_regression: false,
            },
        ],
        regression_checks: Vec::new(),
        max_steps: 2,
        timeout_secs: 20,
    }];
    let provider: Arc<dyn Provider> = Arc::new(BenchMockProvider {
        call_idx: Arc::new(AtomicUsize::new(0)),
        trigger_recoverable_error: false,
    });
    let rep = run_benchmark_suite(provider, "mock", "mock-bench-v1", &custom, None, None)
        .await
        .unwrap();
    assert_eq!(rep.passed_tasks, 1);
    assert!(rep.results[0].succeeded);
}
