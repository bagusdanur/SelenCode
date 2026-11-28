use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::{compaction_threshold_tokens, needs_compaction};
use selencode::agent::corrector::{
    detect_tooling_config_error_hint, is_setup_or_scaffold_shell_cmd, is_vendor_or_dependency_path,
    is_verification_shell_cmd, ToolDedupCache, VerificationTracker,
};
use selencode::agent::{run_turn_ui, UiEvent};
use selencode::config::ToolBudgetCfg;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::tools::{default_registry, Ctx, Registry, Tool};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-v0520-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

struct MockShellTool {
    exec_count: Arc<AtomicUsize>,
    handler: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

#[async_trait]
impl Tool for MockShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Mock run_shell untuk uji deterministik v0.5.20".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &Ctx) -> Result<String> {
        self.exec_count.fetch_add(1, Ordering::SeqCst);
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        Ok((self.handler)(cmd))
    }
}

fn registry_with_mock_shell<F>(exec_count: Arc<AtomicUsize>, f: F) -> Registry
where
    F: Fn(&str) -> String + Send + Sync + 'static,
{
    default_registry().register(Arc::new(MockShellTool {
        exec_count,
        handler: Arc::new(f),
    }))
}

// =========================================================================
// MASALAH 1: Perintah setup/scaffold (mis. `npx sv create ... --add vitest`)
// tidak pernah dilacak gerbang; sesudah hijau -> 0 ekor paksa & status LULUS
// =========================================================================

#[tokio::test]
async fn m1_scaffold_command_never_tracked_and_green_verify_closes_with_zero_tail_and_lulus() {
    // 1. Unit klasifikasi setup/scaffold vs verifikasi nyata
    assert!(is_setup_or_scaffold_shell_cmd(
        "npx sv create . --template minimal --types ts --add vitest tailwindcss eslint"
    ));
    assert!(!is_verification_shell_cmd(&json!({
        "command": "npx sv create . --template minimal --types ts --add vitest tailwindcss eslint"
    })));
    assert!(!is_verification_shell_cmd(&json!({
        "command": "npm install -D vitest svelte-check @playwright/test"
    })));
    assert!(!is_verification_shell_cmd(&json!({
        "command": "npm create vite@latest myapp -- --template svelte-ts"
    })));
    assert!(!is_verification_shell_cmd(&json!({
        "command": "npx svelte-kit sync"
    })));
    assert!(is_verification_shell_cmd(&json!({
        "command": "npm install && npm run build"
    })));
    assert!(is_verification_shell_cmd(&json!({
        "command": "npm run check && npm test && npm run build"
    })));

    // 2. Reproduksi persis alur KASKU Putaran 3 end-to-end:
    //    Step 0: `npx sv create ... --add vitest`
    //    Step 1: `write_file` berkas aplikasi
    //    Step 2: `npm run check && npm test && npm run build` (HIJAU)
    //    Step 3: model langsung menyatakan selesai -> WAJIB 0 ekor paksa & status LULUS!
    struct KaskuTailProvider {
        calls: Arc<AtomicUsize>,
        saw_gate: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for KaskuTailProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-hydrogen-medium"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            for m in msgs {
                if m.plain_text().contains("[Self-Correction Gate") {
                    *self.saw_gate.lock().unwrap() = true;
                }
            }
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "sc1".into(),
                        name: "run_shell".into(),
                        args: json!({
                            "command": "npx sv create . --template minimal --types ts --add vitest tailwindcss"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1200,
                        output_tokens: 30,
                        ..Default::default()
                    }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({
                            "path": "src/routes/+page.svelte",
                            "content": "<main><h1>KASKU</h1></main>\n"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1500,
                        output_tokens: 40,
                        ..Default::default()
                    }),
                }),
                2 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "v1".into(),
                        name: "run_shell".into(),
                        args: json!({
                            "command": "npm run check && npm test && npm run build"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 1800,
                        output_tokens: 30,
                        ..Default::default()
                    }),
                }),
                _ => {
                    let done = "Aplikasi KASKU pada `src/routes/+page.svelte` selesai dan lolos check+test+build.".to_string();
                    let _ = tx.send(StreamEvent::Token(done.clone())).await;
                    Ok(AssistantReply {
                        text: done,
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage {
                            input_tokens: 2000,
                            output_tokens: 35,
                            ..Default::default()
                        }),
                    })
                }
            }
        }
    }

    let cwd = temp_dir("m1-kasku-tail");
    let shell_execs = Arc::new(AtomicUsize::new(0));
    let reg = registry_with_mock_shell(shell_execs.clone(), |cmd| {
        if cmd.contains("sv create") {
            "exit code: 0\nProject created with vitest + tailwindcss\n".into()
        } else {
            "exit code: 0\nsvelte-check found 0 errors\n10 passed\nbuilt in 1.1s\n".into()
        }
    });
    let mut ctx = Ctx::new(cwd, true);
    ctx.self_correct = true;
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_gate = Arc::new(Mutex::new(false));
    let (ui_tx, _ui_rx) = mpsc::channel::<UiEvent>(64);
    let mut messages = Vec::new();
    let final_text = run_turn_ui(
        Arc::new(KaskuTailProvider {
            calls: calls.clone(),
            saw_gate: saw_gate.clone(),
        }),
        &reg,
        &ctx,
        &mut messages,
        "Bangun aplikasi KASKU dengan SvelteKit.",
        0,
        ui_tx,
    )
    .await
    .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "harus selesai tepat di 4 panggilan tanpa ekor pengulangan paksa!"
    );
    assert_eq!(
        shell_execs.load(Ordering::SeqCst),
        2,
        "hanya 1 scaffold + 1 verifikasi hijau — 0 pengulangan paksa!"
    );
    assert!(
        !*saw_gate.lock().unwrap(),
        "Self-Correction Gate tidak boleh menyala sesudah verifikasi hijau penuh"
    );
    assert!(
        final_text.contains("npm run check && npm test && npm run build")
            && final_text.contains("(LULUS)"),
        "status penutup wajib mengutip perintah verifikasi nyata dengan LULUS, dapat: {final_text}"
    );
    assert!(
        !final_text.contains("GAGAL") && !final_text.contains("sv create"),
        "status penutup tidak boleh memuat GAGAL atau perintah scaffold basi: {final_text}"
    );

    // 3. Pagar non-regresi: jika ada edit sesudah verifikasi hijau TANPA verifikasi ulang,
    //    tracker tetap mencatat belum diverifikasi; jika verifikasi berurutan (`npm run check` lalu
    //    `npm run build`), perintah verifikasi terakhir ikut terlacak akurat!
    let mut sc = VerificationTracker::new(None).with_strict_known_verify_only(true);
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": "npx sv create . --add vitest"}),
        "exit code: 0\ncreated",
        false,
        1,
    );
    assert!(
        !sc.has_verification_run && sc.last_verify_cmd.is_none(),
        "perintah scaffold tidak boleh pernah dicatat di VerificationTracker"
    );
    sc.observe_tool_at_step(
        "write_file",
        &json!({"path": "src/app.ts"}),
        "ok",
        false,
        2,
    );
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": "npm run check"}),
        "exit code: 0\n0 errors",
        false,
        3,
    );
    sc.observe_tool_at_step(
        "run_shell",
        &json!({"command": "npm run build"}),
        "exit code: 0\nbuilt ok",
        false,
        4,
    );
    assert_eq!(sc.last_verify_cmd.as_deref(), Some("npm run build"));
    assert_eq!(sc.last_verify_passed, Some(true));
    // Edit baru tanpa verifikasi ulang -> WAJIB tercatat belum diverifikasi!
    sc.observe_tool_at_step(
        "edit_file",
        &json!({"path": "src/app.ts"}),
        "ok",
        false,
        5,
    );
    assert!(
        !sc.verified_after_last_edit && sc.honest_unverified_closing_note("Selesai.").is_some(),
        "edit yang belum diverifikasi tetap wajib tercatat belum terverifikasi pada penutup deterministik"
    );
}

// =========================================================================
// MASALAH 2: Pemulihan kegagalan perkakas (tsconfig/svelte-check) cepat,
// mencegah korek berulang di `node_modules/` & menolak edit `node_modules/`
// =========================================================================

#[tokio::test]
async fn m2_tooling_tsconfig_failure_redirects_away_from_node_modules_and_recovers_fast() {
    assert!(is_vendor_or_dependency_path("node_modules/svelte/package.json"));
    assert!(is_vendor_or_dependency_path("./node_modules/@sveltejs/kit/types/index.d.ts"));
    assert!(is_vendor_or_dependency_path(".svelte-kit/output/server/index.js"));
    assert!(!is_vendor_or_dependency_path("tsconfig.json"));
    assert!(!is_vendor_or_dependency_path("src/lib/db.ts"));

    let hint = detect_tooling_config_error_hint(
        "npm run check",
        "exit code: 1\nError: Cannot find module '$app/environment' in tsconfig.json\nsvelte-check found 59 errors",
    );
    assert!(
        hint.is_some() && hint.unwrap().contains("[PEMULIHAN PERKAKAS]"),
        "galat tsconfig/svelte-check wajib memicu panduan pemulihan perkakas"
    );
    assert!(
        detect_tooling_config_error_hint("npm run check", "exit code: 0\n0 errors").is_none(),
        "perintah lulus tidak boleh memicu panduan galat"
    );

    // Tulis/edit di dalam `node_modules/` wajib ditolak oleh tool file
    let cwd = temp_dir("m2-tooling");
    std::fs::create_dir_all(cwd.join("node_modules/svelte")).unwrap();
    std::fs::write(
        cwd.join("node_modules/svelte/package.json"),
        "{\"name\": \"svelte\"}\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("node_modules/svelte/index.d.ts"),
        "export declare const x: number;\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("tsconfig.json"),
        "{\n  \"compilerOptions\": { \"strict\": true }\n}\n",
    )
    .unwrap();

    let reg_plain = default_registry();
    let ctx_plain = Ctx::new(cwd.clone(), true);
    let write_nm = reg_plain
        .get("write_file")
        .unwrap()
        .run(
            json!({"path": "node_modules/svelte/hack.d.ts", "content": "export {}"}),
            &ctx_plain,
        )
        .await;
    assert!(
        write_nm.is_err(),
        "write_file ke node_modules/ wajib ditolak"
    );
    let edit_nm = reg_plain
        .get("edit_file")
        .unwrap()
        .run(
            json!({"path": "node_modules/svelte/package.json", "old": "svelte", "new": "hacked"}),
            &ctx_plain,
        )
        .await;
    assert!(
        edit_nm.is_err(),
        "edit_file ke node_modules/ wajib ditolak"
    );

    // End-to-end: check gagal 59 error tsconfig -> model coba baca 2 berkas di node_modules ->
    // bacaan ke-1 dapat [PANDUAN PERKAKAS], bacaan ke-2 DICEGAH tanpa membaca disk ->
    // model memperbaiki `tsconfig.json` -> check HIJAU kembali dalam <= 6 panggilan!
    struct ToolingRecoveryProvider {
        step: Arc<AtomicUsize>,
        saw_tooling_hint: Arc<Mutex<bool>>,
        saw_vendor_block: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for ToolingRecoveryProvider {
        fn model_name(&self) -> &str {
            "mock-tooling"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            for m in msgs {
                for b in &m.content {
                    if let Block::ToolResult { content, .. } = b {
                        if content.contains("[PEMULIHAN PERKAKAS]") {
                            *self.saw_tooling_hint.lock().unwrap() = true;
                        }
                        if content.contains("[PANDUAN PERKAKAS — BACAAN VENDOR DICEGAH]") {
                            *self.saw_vendor_block.lock().unwrap() = true;
                        }
                    }
                }
            }
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "chk1".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "npm run check"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 800, output_tokens: 20, ..Default::default() }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "nm1".into(),
                            name: "read_file".into(),
                            args: json!({"path": "node_modules/svelte/package.json"}),
                        },
                        ToolCall {
                            id: "nm2".into(),
                            name: "read_file".into(),
                            args: json!({"path": "node_modules/svelte/index.d.ts"}),
                        },
                    ],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 900, output_tokens: 30, ..Default::default() }),
                }),
                2 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "fix_ts".into(),
                        name: "edit_file".into(),
                        args: json!({
                            "path": "tsconfig.json",
                            "old": "\"compilerOptions\": { \"strict\": true }",
                            "new": "\"extends\": \"./.svelte-kit/tsconfig.json\",\n  \"compilerOptions\": { \"strict\": true }"
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 1000, output_tokens: 30, ..Default::default() }),
                }),
                3 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "chk2".into(),
                        name: "run_shell".into(),
                        args: json!({"command": "npm run check"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 1100, output_tokens: 20, ..Default::default() }),
                }),
                _ => {
                    let msg = "Konfigurasi `tsconfig.json` sudah diperbaiki dan `npm run check` hijau.".to_string();
                    let _ = tx.send(StreamEvent::Token(msg.clone())).await;
                    Ok(AssistantReply {
                        text: msg,
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage { input_tokens: 1150, output_tokens: 25, ..Default::default() }),
                    })
                }
            }
        }
    }

    let check_runs = Arc::new(AtomicUsize::new(0));
    let check_runs_c = check_runs.clone();
    let reg = registry_with_mock_shell(Arc::new(AtomicUsize::new(0)), move |_cmd| {
        let k = check_runs_c.fetch_add(1, Ordering::SeqCst);
        if k == 0 {
            "exit code: 1\nsvelte-check found 59 errors: Cannot find module '$app/stores' (tsconfig.json)\n".into()
        } else {
            "exit code: 0\nsvelte-check found 0 errors and 0 warnings\n".into()
        }
    });
    let mut ctx = Ctx::new(cwd, true);
    ctx.self_correct = true;
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_hint = Arc::new(Mutex::new(false));
    let saw_block = Arc::new(Mutex::new(false));
    let (tx, _rx) = mpsc::channel::<UiEvent>(64);
    let mut msgs = Vec::new();
    let out = run_turn_ui(
        Arc::new(ToolingRecoveryProvider {
            step: calls.clone(),
            saw_tooling_hint: saw_hint.clone(),
            saw_vendor_block: saw_block.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        "Perbaiki konfigurasi proyek agar svelte-check lulus.",
        0,
        tx,
    )
    .await
    .unwrap();

    assert!(*saw_hint.lock().unwrap(), "harus menyuntikkan [PEMULIHAN PERKAKAS]");
    assert!(
        *saw_block.lock().unwrap(),
        "bacaan kedua di dalam node_modules/ harus dicegah tanpa membaca disk"
    );
    assert!(calls.load(Ordering::SeqCst) <= 5);
    assert!(out.contains("(LULUS)"), "hasil akhir wajib LULUS: {out}");
}

// =========================================================================
// MASALAH 3: Ekonomi langkah — cegah baca ulang karya sendiri & dorong
// patch terarah (`edit_file`) pada berkas besar yang sudah ada
// =========================================================================

#[tokio::test]
async fn m3_skip_rereading_own_written_files_and_nudge_targeted_edit_on_large_files() {
    let cwd = temp_dir("m3-economy");
    let reg = default_registry();
    let ctx = Ctx::new(cwd.clone(), true);

    // 1. Unit level: ToolDedupCache baseline tulis sendiri
    let mut dedup = ToolDedupCache::new();
    let large_body: String = (1..=120)
        .map(|i| format!("export const item_{i} = 'nilai-panjang-kasku-transaksi-{i:04}';\n"))
        .collect();
    assert!(large_body.len() > 5_000);
    std::fs::write(cwd.join("store.ts"), &large_body).unwrap();
    dedup.record_write("store.ts", &cwd);

    // Bacaan penuh atas berkas yang baru ditulis sendiri -> KENA DEDUP!
    let hit = dedup
        .check_hit("read_file", &json!({"path": "store.ts"}), &cwd)
        .expect("bacaan penuh ulang atas berkas yang baru ditulis sendiri harus kena dedup");
    assert!(
        hit.contains("baru saja kamu tulis/ubah"),
        "stub dedup harus menjelaskan isi sudah ada di konteks: {hit}"
    );

    // Bacaan terarah (`start`/`end`) untuk menyelidiki baris galat -> TETAP DIIZINKAN!
    assert!(
        dedup
            .check_hit(
                "read_file",
                &json!({"path": "store.ts", "start": 10, "end": 25}),
                &cwd
            )
            .is_none(),
        "bacaan rentang baris eksplisit untuk investigasi galat harus tetap diizinkan"
    );

    // Jika berkas diubah secara eksternal di disk -> baseline otomatis tidak berlaku (baca segar jalan!)
    std::fs::write(cwd.join("store.ts"), format!("{large_body}// external change\n")).unwrap();
    assert!(
        dedup
            .check_hit("read_file", &json!({"path": "store.ts"}), &cwd)
            .is_none(),
        "perubahan eksternal di disk harus membatalkan dedup bacaan"
    );

    // Setelah `clear_file_reads()` (prune/compaction), baseline dibersihkan agar recovery read tetap bisa jalan
    dedup.record_write("store.ts", &cwd);
    dedup.clear_file_reads();
    assert!(
        dedup
            .check_hit("read_file", &json!({"path": "store.ts"}), &cwd)
            .is_none(),
        "clear_file_reads harus menghapus baseline tulis setelah prune/compaction"
    );

    // 2. Dorongan `[Efisiensi]` saat menulis ulang utuh berkas besar (>4KB) yang hampir identik
    let tweaked = large_body.replace("item_5 = 'nilai-panjang-kasku-transaksi-0005'", "item_5 = 'diubah'");
    let w_out = reg
        .get("write_file")
        .unwrap()
        .run(json!({"path": "store.ts", "content": tweaked}), &ctx)
        .await
        .unwrap();
    assert!(
        w_out.contains("[Efisiensi]") && w_out.contains("edit_file"),
        "tulis ulang utuh berkas besar dengan perubahan lokal wajib memberi dorongan edit_file: {w_out}"
    );

    // 3. End-to-end `run_turn_ui`: write_file -> read_file(whole) kena dedup (tidak muat ulang 5KB isi!)
    struct SelfRereadProvider {
        step: Arc<AtomicUsize>,
        saw_write_dedup: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for SelfRereadProvider {
        fn model_name(&self) -> &str {
            "mock-m3-econ"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            for m in msgs {
                for b in &m.content {
                    if let Block::ToolResult { content, .. } = b {
                        if content.contains("baru saja kamu tulis/ubah") {
                            *self.saw_write_dedup.lock().unwrap() = true;
                        }
                    }
                }
            }
            match n {
                0 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        args: json!({"path": "fresh.ts", "content": "export const a = 1;\nexport const b = 2;\n"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 500, output_tokens: 25, ..Default::default() }),
                }),
                1 => Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "r1".into(),
                        name: "read_file".into(),
                        args: json!({"path": "fresh.ts"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage { input_tokens: 550, output_tokens: 15, ..Default::default() }),
                }),
                _ => {
                    let txt = "Selesai membuat `fresh.ts`.".to_string();
                    let _ = tx.send(StreamEvent::Token(txt.clone())).await;
                    Ok(AssistantReply {
                        text: txt,
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: Some(Usage { input_tokens: 600, output_tokens: 15, ..Default::default() }),
                    })
                }
            }
        }
    }

    let saw_wd = Arc::new(Mutex::new(false));
    let (utx, _urx) = mpsc::channel::<UiEvent>(64);
    let mut umsgs = Vec::new();
    let _ = run_turn_ui(
        Arc::new(SelfRereadProvider {
            step: Arc::new(AtomicUsize::new(0)),
            saw_write_dedup: saw_wd.clone(),
        }),
        &reg,
        &ctx,
        &mut umsgs,
        "Buat fresh.ts.",
        0,
        utx,
    )
    .await
    .unwrap();
    assert!(
        *saw_wd.lock().unwrap(),
        "read_file penuh segera sesudah write_file sendiri di sesi yang sama harus kena dedup"
    );
}

// =========================================================================
// MASALAH 4: Porsi cache tinggi — tidak memotong/compaction prematur di 28k/48k
// pada jendela 1M (seperti KASKU 58k–65k) + histeresis rearm penuh terstruktur
// =========================================================================

#[tokio::test]
async fn m4_large_context_preserves_cache_prefix_without_premature_prune_or_compaction() {
    // 1. Ambang bawaan baru mencegah cache break prematur di sesi bangun 30k–80k token pada jendela 1M
    let tb = ToolBudgetCfg::default();
    assert_eq!(tb.proactive_prune_tokens, 96_000);
    assert_eq!(tb.proactive_prune_min_reclaim_tokens, 4_096);
    assert_eq!(compaction_threshold_tokens(1_048_576), 200_000);
    assert_eq!(compaction_threshold_tokens(100_000), 50_000);
    assert_eq!(compaction_threshold_tokens(64_000), 32_000);

    // Simulasi konteks ~60.000 token (rata-rata KASKU SelenCode = 58.772 token):
    // pada model 1M, `needs_compaction` WAJIB false (sebelumnya v0.5.19 true karena cap 48.000!).
    let big_msgs: Vec<Message> = (0..30)
        .map(|i| Message::text(Role::User, format!("blok-{i}: {}", "k".repeat(7_000))))
        .collect();
    assert!(
        !needs_compaction(&big_msgs, 1_048_576),
        "konteks ~60k pada model 1M tidak boleh memicu compaction prematur yang menghancurkan prompt cache"
    );
    assert!(
        needs_compaction(&big_msgs, 100_000),
        "pada model 100k, konteks ~60k (>50%) tetap wajib memicu compaction"
    );

    // 2. End-to-end `run_turn_ui` pada jendela 1M: tumbuh melewati 35.000 token (di atas batas lama 28k) ->
    //    0 prune proaktif, 0 compaction, dan prefiks 100% byte-identik di seluruh panggilan!
    struct CacheHotBuildProvider {
        step: Arc<AtomicUsize>,
        snapshots: Arc<Mutex<Vec<Vec<String>>>>,
    }
    #[async_trait]
    impl Provider for CacheHotBuildProvider {
        fn model_name(&self) -> &str {
            "ag/gemini-4-hydrogen-medium"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let ser: Vec<String> = msgs
                .iter()
                .map(|m| {
                    let ps: Vec<String> = m
                        .content
                        .iter()
                        .map(|b| match b {
                            Block::Text(t) => format!("T:{t}"),
                            Block::ToolUse { id, name, args } => format!("U:{id}:{name}:{args}"),
                            Block::ToolResult { id, content, is_error } => {
                                format!("R:{id}:{is_error}:{content}")
                            }
                            Block::Image { media_type, .. } => format!("I:{media_type}"),
                        })
                        .collect();
                    format!("{:?}|{}", m.role, ps.join("|"))
                })
                .collect();
            self.snapshots.lock().unwrap().push(ser);

            // Setiap langkah menambah ~4.500 token -> 9 langkah = ~40.500+ token (> batas lama 28k!)
            if n < 9 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("m{n}"),
                        name: "write_file".into(),
                        args: json!({
                            "path": format!("src/mod_{n}.ts"),
                            "content": format!("// modul {n}\nexport const data_{n} = '{}';\n", "z".repeat(14_000))
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 6_000 + (n as u64) * 4_500,
                        output_tokens: 120,
                        cache_read_tokens: if n == 0 { 0 } else { 5_500 + ((n - 1) as u64) * 4_500 },
                        cache_write_tokens: 0,
                    }),
                })
            } else {
                let done = "Semua 9 modul selesai dibangun tanpa pemutusan cache.".to_string();
                let _ = tx.send(StreamEvent::Token(done.clone())).await;
                Ok(AssistantReply {
                    text: done,
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 46_500,
                        output_tokens: 40,
                        cache_read_tokens: 41_500,
                        cache_write_tokens: 0,
                    }),
                })
            }
        }
    }

    let cwd = temp_dir("m4-cache-hot");
    let reg = default_registry();
    let mut ctx = Ctx::new(cwd, true).with_context_limit(1_048_576);
    ctx.self_correct = false;
    let snaps = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = mpsc::channel::<UiEvent>(128);
    let mut msgs = Vec::new();
    let _ = run_turn_ui(
        Arc::new(CacheHotBuildProvider {
            step: Arc::new(AtomicUsize::new(0)),
            snapshots: snaps.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        "Bangun 9 modul aplikasi secara bertahap.",
        0,
        tx,
    )
    .await
    .unwrap();

    let mut prune_or_compact_events = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let UiEvent::Info(s) = ev {
            if s.contains("Prune proaktif") || s.contains("compaction") {
                prune_or_compact_events += 1;
            }
        }
    }
    assert_eq!(
        prune_or_compact_events, 0,
        "sesi bangun ~46k token di jendela 1M tidak boleh memicu prune/compaction prematur"
    );

    let all_snaps = snaps.lock().unwrap();
    assert_eq!(all_snaps.len(), 10);
    for i in 1..all_snaps.len() {
        let prev = &all_snaps[i - 1];
        let curr = &all_snaps[i];
        assert!(curr.len() >= prev.len());
        assert_eq!(
            &curr[..prev.len()],
            &prev[..],
            "prefiks panggilan #{i} tidak boleh bermutasi — menjaga rasio cache read ~85%"
        );
    }
}
