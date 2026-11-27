use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::corrector::{can_run_tool_batch_in_parallel, ToolDedupCache};
use selencode::agent::{run_turn_ui, UiEvent};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::tools::file::fuzzy_find_and_replace;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0521-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

// =========================================================================
// BUTIR 1: Cache-safe fixed toolset — kemajuan nyata mereset counter anti-spin
// dan mode non-interaktif tidak pernah memutasi array `tools` di tengah sesi
// =========================================================================

#[tokio::test]
async fn b1_productive_session_keeps_tools_fixed_and_no_false_antispin() {
    struct ProductiveBuildProvider {
        step: Arc<AtomicUsize>,
        seen_tool_counts: Arc<Mutex<Vec<usize>>>,
        saw_antispin: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for ProductiveBuildProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-hydrogen-medium"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            self.seen_tool_counts.lock().unwrap().push(tools.len());
            for m in msgs {
                for b in &m.content {
                    if let Block::ToolResult { content, .. } = b {
                        if content.contains("[GUARD ANTI-BERPUTAR]") {
                            *self.saw_antispin.lock().unwrap() = true;
                        }
                    }
                }
            }
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            // Pola sesi bangun nyata: list_files -> list_files -> list_files -> list_files ->
            // write_file (kemajuan!) -> list_files -> list_files -> selesai.
            // Tanpa reset pada `write_file`, langkah ke-5 akan memicu GUARD ANTI-BERPUTAR & mutasi tools!
            match n {
                0..=3 | 5..=6 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("ls{n}"),
                        name: "list_files".into(),
                        args: json!({"path": "."}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 4_000,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                }),
                4 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "app.ts", "content": "export const ok = 1;\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 4_500,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                }),
                _ => {
                    let done = "Selesai membuat `app.ts`.".to_string();
                    let _ = tx.send(StreamEvent::Token(done.clone())).await;
                    Ok(AssistantReply {
                        text: done,
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 4_800,
                            output_tokens: 20,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let cwd = temp_dir("b1-fixed-tools");
    let reg = default_registry();
    let ctx = Ctx::new(cwd, true);
    let counts = Arc::new(Mutex::new(Vec::new()));
    let saw_spin = Arc::new(Mutex::new(false));
    let (tx, _rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let _ = run_turn_ui(
        Arc::new(ProductiveBuildProvider {
            step: Arc::new(AtomicUsize::new(0)),
            seen_tool_counts: counts.clone(),
            saw_antispin: saw_spin.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        "Buat app.ts di proyek ini.",
        0,
        tx,
    )
    .await
    .unwrap();

    let cs = counts.lock().unwrap();
    assert!(cs.len() >= 7);
    assert!(
        cs.iter().all(|&c| c == cs[0]),
        "jumlah tool di prefiks WAJIB tetap konstan sepanjang sesi agar prompt cache tidak patah: {cs:?}"
    );
    assert!(
        !*saw_spin.lock().unwrap(),
        "sesi dengan write_file tidak boleh memicu false-positive GUARD ANTI-BERPUTAR"
    );
}

// =========================================================================
// BUTIR 2: Fuzzy / whitespace-tolerant edit_file + multi-edit atomik +
// petunjuk "Maksudmu baris X-Y?" (diporting dari Hermes tools/fuzzy_match.py)
// =========================================================================

#[tokio::test]
async fn b2_fuzzy_edit_file_strategies_multiedit_and_no_match_hint() {
    // 1. Tolak no-op (old == new)
    assert!(fuzzy_find_and_replace("let x = 1;\n", "let x = 1;", "let x = 1;", "a.ts").is_err());

    // 2. Strategy line_trimmed (trailing space & CRLF)
    let crlf_src = "function foo() {  \r\n  return 1;\r\n}\r\n";
    let (res_rt, strat_rt) =
        fuzzy_find_and_replace(crlf_src, "function foo() {\n  return 1;\n}", "function foo() {\n  return 2;\n}", "a.ts")
            .unwrap();
    assert_eq!(strat_rt, "line_trimmed");
    assert!(res_rt.contains("return 2;"));

    // 3. Strategy indent_preserving (model kirim 2 spasi, file pakai 4 spasi -> indentasi file terjaga!)
    let indented = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
    let (res_ind, strat_ind) =
        fuzzy_find_and_replace(indented, "  let a = 1;\n  let b = 2;", "  let a = 10;\n  let b = 20;", "main.rs")
            .unwrap();
    assert_eq!(strat_ind, "indent_preserving");
    assert!(
        res_ind.contains("    let a = 10;\n    let b = 20;"),
        "indentasi 4 spasi milik file asli harus terjaga: {res_ind:?}"
    );

    // 4. Strategy unicode_normalized (smart quotes / en-dash / NBSP ala Hermes UNICODE_MAP)
    let uni_src = "const msg = \u{201c}Halo\u{00a0}Dunia\u{201d};\n";
    let (res_uni, strat_uni) =
        fuzzy_find_and_replace(uni_src, "const msg = \"Halo Dunia\";", "const msg = \"Halo KASKU\";", "u.ts")
            .unwrap();
    assert_eq!(strat_uni, "unicode_normalized");
    assert!(res_uni.contains("Halo KASKU"));

    // 5. No-match hint menyertakan "Maksudmu baris ..." untuk potongan mirip
    let err = fuzzy_find_and_replace(
        "const alpha = 1;\nconst beta = 2;\nconst gamma = 3;\n",
        "const beta = 999;\nconst gamma = 3;",
        "const beta = 4;\nconst gamma = 3;",
        "h.ts",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Maksudmu baris"),
        "error tidak cocok harus memberi petunjuk baris terdekat: {err}"
    );

    // 6. Multi-edit atomik lewat tool `edit_file` (`edits: [...]`)
    let cwd = temp_dir("b2-multiedit");
    std::fs::write(cwd.join("multi.ts"), "export const a = 1;\nexport const b = 2;\n").unwrap();
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let out = reg
        .get("edit_file")
        .unwrap()
        .run(
            json!({
                "path": "multi.ts",
                "edits": [
                    {"old": "export const a = 1;", "new": "export const a = 100;"},
                    {"old": "export const b = 2;", "new": "export const b = 200;"}
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.contains("2 patch diterapkan"), "output harus menyebut 2 patch: {out}");
    let disk = std::fs::read_to_string(cwd.join("multi.ts")).unwrap();
    assert_eq!(disk, "export const a = 100;\nexport const b = 200;\n");
}

// =========================================================================
// BUTIR 3: Eksekusi paralel batch tool independen (Hermes tool_dispatch_helpers)
// =========================================================================

#[tokio::test]
async fn b3_parallel_batch_admission_and_deterministic_order() {
    // 1. Unit admission rules
    let reads = vec![
        ToolCall { id: "1".into(), name: "read_file".into(), args: json!({"path": "a.txt"}) },
        ToolCall { id: "2".into(), name: "read_file".into(), args: json!({"path": "b.txt"}) },
    ];
    assert!(can_run_tool_batch_in_parallel(&reads, false));

    let distinct_writes = vec![
        ToolCall { id: "1".into(), name: "write_file".into(), args: json!({"path": "a.txt", "content": "A"}) },
        ToolCall { id: "2".into(), name: "write_file".into(), args: json!({"path": "b.txt", "content": "B"}) },
    ];
    assert!(can_run_tool_batch_in_parallel(&distinct_writes, true));
    assert!(
        !can_run_tool_batch_in_parallel(&distinct_writes, false),
        "tanpa auto_yes, write_file tidak boleh paralel agar dialog approval tidak bertabrakan"
    );

    let same_path_writes = vec![
        ToolCall { id: "1".into(), name: "write_file".into(), args: json!({"path": "a.txt", "content": "A"}) },
        ToolCall { id: "2".into(), name: "write_file".into(), args: json!({"path": "./a.txt", "content": "B"}) },
    ];
    assert!(
        !can_run_tool_batch_in_parallel(&same_path_writes, true),
        "dua write_file pada jalur normalisasi sama WAJIB serial"
    );

    let read_write_conflict = vec![
        ToolCall { id: "1".into(), name: "read_file".into(), args: json!({"path": "a.txt"}) },
        ToolCall { id: "2".into(), name: "write_file".into(), args: json!({"path": "a.txt", "content": "X"}) },
    ];
    assert!(
        !can_run_tool_batch_in_parallel(&read_write_conflict, true),
        "read_file + write_file pada berkas sama WAJIB serial"
    );

    // 2. End-to-end `run_turn_ui`: 4 write_file independen sekaligus dalam 1 giliran -> semua tertulis & terurut!
    struct ParallelWriteProvider {
        step: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for ParallelWriteProvider {
        fn model_name(&self) -> &str {
            "mock-par"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: (1..=4)
                        .map(|i| ToolCall {
                            id: format!("pw{i}"),
                            name: "write_file".into(),
                            args: json!({"path": format!("f{i}.txt"), "content": format!("isi-{i}")}),
                        })
                        .collect(),
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 400, output_tokens: 40, ..Default::default() }),
                })
            } else {
                let done = "4 berkas paralel selesai ditulis.".to_string();
                let _ = tx.send(StreamEvent::Token(done.clone())).await;
                Ok(AssistantReply {
                    text: done,
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage { input_tokens: 500, output_tokens: 20, ..Default::default() }),
                })
            }
        }
    }

    let cwd = temp_dir("b3-parallel");
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);
    let (tx, _rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let _ = run_turn_ui(
        Arc::new(ParallelWriteProvider { step: Arc::new(AtomicUsize::new(0)) }),
        &reg,
        &ctx,
        &mut msgs,
        "Buat 4 berkas sekaligus.",
        0,
        tx,
    )
    .await
    .unwrap();

    for i in 1..=4 {
        assert_eq!(
            std::fs::read_to_string(cwd.join(format!("f{i}.txt"))).unwrap(),
            format!("isi-{i}")
        );
    }
    // Pastikan urutan ToolResult pada pesan Tool tetap persis pw1, pw2, pw3, pw4!
    let tool_ids: Vec<String> = msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::ToolResult { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ids, vec!["pw1", "pw2", "pw3", "pw4"]);
}

// =========================================================================
// BUTIR 4: Negative cache not-found read_file (TTL 60 dtk) & eskalasi
// kegagalan patch beruntun (Hermes tools/file_tools_read_tracking.py)
// =========================================================================

#[test]
fn b4_not_found_negative_cache_and_edit_failure_escalation() {
    let cwd = temp_dir("b4-tracking");
    let mut dedup = ToolDedupCache::new();

    // 1. Catat berkas tidak ditemukan -> panggilan ulang langsung kena cache not-found
    dedup.record_read_not_found("hilang.ts", "ERROR: gagal membaca hilang.ts: No such file or directory (os error 2)");
    let hit = dedup
        .check_hit("read_file", &json!({"path": "hilang.ts"}), &cwd)
        .expect("bacaan ulang path yang baru gagal not-found harus kena negative cache");
    assert!(hit.contains("Cache Not-Found"), "pesan cache harus jelas: {hit}");

    // 2. Begitu berkas dibuat di disk -> negative cache otomatis invalid dan mengizinkan baca segar!
    std::fs::write(cwd.join("hilang.ts"), "export const ada = true;\n").unwrap();
    assert!(
        dedup
            .check_hit("read_file", &json!({"path": "hilang.ts"}), &cwd)
            .is_none(),
        "negative cache wajib gugur otomatis begitu berkas muncul di disk"
    );

    // 3. Penghitung kegagalan edit_file beruntun per berkas & reset saat berhasil
    assert_eq!(dedup.record_edit_failure("src/app.ts"), 1);
    assert_eq!(dedup.record_edit_failure("src/app.ts"), 2);
    dedup.reset_edit_failures("src/app.ts");
    assert_eq!(dedup.record_edit_failure("src/app.ts"), 1);
}
