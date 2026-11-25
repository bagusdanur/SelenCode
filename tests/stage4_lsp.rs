//! Tes Tahap 4 — Pemahaman Kode Lanjutan (LSP Diagnostics, Navigasi Simbol, Referensi, & Safe Rename).

use selencode::lsp::{
    adapter_for_ext, apply_text_edits, encode_lsp_message, extract_identifier_at,
    file_uri_to_path, path_to_file_uri, read_lsp_message, safe_rename_identifier_in_text,
    tree_sitter_syntax_diagnostics, LspManager, LspTextEdit, LSP_ADAPTERS,
};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::tools::undo::UndoStack;
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use serde_json::json;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-stage4-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn lsp_jsonrpc_content_length_framing_and_uri_roundtrip() {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "result": { "label": "Halo ☾ Selena" }
    });
    let encoded = encode_lsp_message(&payload);
    let mut reader = tokio::io::BufReader::new(Cursor::new(encoded));
    let decoded = read_lsp_message(&mut reader).await.unwrap();
    assert_eq!(decoded["id"], 7);
    assert_eq!(decoded["result"]["label"], "Halo ☾ Selena");

    let cwd = temp_dir("uri");
    let f = cwd.join("sample.rs");
    std::fs::write(&f, "fn main() {}\n").unwrap();
    let uri = path_to_file_uri(&f);
    assert!(uri.starts_with("file://"));
    let back = file_uri_to_path(&uri).unwrap();
    assert_eq!(
        back.canonicalize().unwrap(),
        f.canonicalize().unwrap()
    );
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn lsp_adapters_and_workspace_text_edit_application() {
    assert!(LSP_ADAPTERS.len() >= 4);
    assert_eq!(adapter_for_ext("rs").unwrap().id, "rust");
    assert_eq!(adapter_for_ext("tsx").unwrap().id, "typescript");
    assert_eq!(adapter_for_ext("py").unwrap().id, "python");
    assert_eq!(adapter_for_ext("go").unwrap().id, "go");

    let src = "fn old_fn() {}\nfn call() { old_fn(); }\n";
    let edits = vec![
        LspTextEdit {
            start_line: 0,
            start_col: 3,
            end_line: 0,
            end_col: 9,
            new_text: "new_fn".into(),
        },
        LspTextEdit {
            start_line: 1,
            start_col: 12,
            end_line: 1,
            end_col: 18,
            new_text: "new_fn".into(),
        },
    ];
    let updated = apply_text_edits(src, &edits);
    assert_eq!(updated, "fn new_fn() {}\nfn call() { new_fn(); }\n");
}

#[test]
fn tree_sitter_diagnostics_detects_syntax_errors_and_identifier_boundaries() {
    let broken_rs = "pub fn broken(a: i32 {\n    let x = ;\n}\n";
    let diags = tree_sitter_syntax_diagnostics(broken_rs, "rs", "src/broken.rs");
    assert!(!diags.is_empty(), "harus mendeteksi error sintaks Rust");
    assert_eq!(diags[0].rel_path, "src/broken.rs");

    let clean_rs = "pub fn ok(a: i32) -> i32 { a + 1 }\n";
    assert!(tree_sitter_syntax_diagnostics(clean_rs, "rs", "src/ok.rs").is_empty());

    assert_eq!(
        extract_identifier_at("let total = compute_tax(100);", 1, 16),
        Some("compute_tax".into())
    );

    // Rename batas-identifier TIDAK BOLEH merusak `compute_tax_extra` atau `my_compute_tax`!
    let code = "fn compute_tax() {} fn compute_tax_extra() {} fn my_compute_tax() { compute_tax(); }";
    let (renamed, count) = safe_rename_identifier_in_text(code, "compute_tax", "calc_vat");
    assert_eq!(count, 2);
    assert!(renamed.contains("fn calc_vat() {}"));
    assert!(renamed.contains("fn compute_tax_extra() {}"));
    assert!(renamed.contains("fn my_compute_tax() { calc_vat(); }"));
}

#[tokio::test]
async fn lsp_manager_definition_references_hover_and_safe_rename_with_rollback() {
    let cwd = temp_dir("hybrid");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/tax.rs"),
        "/// Hitung pajak pertambahan nilai.\n\
         pub fn compute_vat(amount: u64) -> u64 {\n\
             amount / 10\n\
         }\n\
         pub fn compute_vat_extra() -> u64 { 0 }\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("src/order.rs"),
        "use crate::tax::compute_vat;\n\
         pub fn checkout() -> u64 { compute_vat(1000) }\n",
    )
    .unwrap();
    // File rahasia tidak boleh pernah tersentuh oleh references maupun rename!
    std::fs::write(cwd.join(".env"), "compute_vat=secret_value\n").unwrap();

    let mgr = LspManager::new(cwd.clone());

    // 1) Definition
    let defs = mgr
        .definition(None, None, None, Some("compute_vat"))
        .await
        .unwrap();
    assert!(defs.locations.iter().any(|l| l.rel_path == "src/tax.rs" && l.line == 2));

    // 2) References
    let refs = mgr
        .references(None, None, None, Some("compute_vat"))
        .await
        .unwrap();
    assert!(refs.locations.iter().any(|l| l.rel_path == "src/tax.rs"));
    assert!(refs.locations.iter().any(|l| l.rel_path == "src/order.rs"));
    assert!(!refs.locations.iter().any(|l| l.rel_path == ".env"));

    // 3) Hover (menyertakan signature & komentar dokumentasi)
    let hov = mgr
        .hover(None, None, None, Some("compute_vat"))
        .await
        .unwrap();
    assert!(hov.content.contains("compute_vat"));
    assert!(hov.content.contains("pajak pertambahan nilai"));

    // 4) Dry-run rename TIDAK mengubah disk
    let undo = Mutex::new(UndoStack::default());
    let dry = mgr
        .rename_symbol(None, None, None, Some("compute_vat"), "calculate_ppn", true, &undo)
        .await
        .unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.total_replacements, 3); // 1 di tax.rs + 2 di order.rs
    assert!(std::fs::read_to_string(cwd.join("src/tax.rs"))
        .unwrap()
        .contains("compute_vat("));

    // 5) Rename nyata + checkpoint rollback!
    let cp_id = undo.lock().unwrap().begin_checkpoint("sebelum rename");
    let applied = mgr
        .rename_symbol(None, None, None, Some("compute_vat"), "calculate_ppn", false, &undo)
        .await
        .unwrap();
    assert!(!applied.dry_run);
    assert_eq!(applied.total_replacements, 3);

    let tax_after = std::fs::read_to_string(cwd.join("src/tax.rs")).unwrap();
    let order_after = std::fs::read_to_string(cwd.join("src/order.rs")).unwrap();
    let env_after = std::fs::read_to_string(cwd.join(".env")).unwrap();
    assert!(tax_after.contains("pub fn calculate_ppn("));
    assert!(tax_after.contains("pub fn compute_vat_extra(")); // TIDAK rusak!
    assert!(order_after.contains("calculate_ppn(1000)"));
    assert_eq!(env_after, "compute_vat=secret_value\n"); // .env aman!

    // Rollback ke checkpoint mengembalikan semua file hasil rename!
    let rb = undo.lock().unwrap().rollback_to(Some(cp_id)).unwrap();
    assert_eq!(rb.restored.len(), 2);
    assert!(std::fs::read_to_string(cwd.join("src/tax.rs"))
        .unwrap()
        .contains("pub fn compute_vat("));

    // Identifier tidak valid wajib ditolak
    assert!(mgr
        .rename_symbol(None, None, None, Some("compute_vat"), "123-bad!", false, &undo)
        .await
        .is_err());

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn lsp_tools_registry_parity_and_slash_command() {
    let reg = default_registry();
    let ro = read_only_registry();
    for name in ["lsp_diagnostics", "lsp_definition", "lsp_references", "lsp_hover"] {
        assert!(reg.get(name).is_some(), "{name} di default_registry");
        assert!(ro.get(name).is_some(), "{name} di read_only_registry");
    }
    let rename_tool = reg.get("lsp_rename").expect("lsp_rename di default_registry");
    assert!(ro.get("lsp_rename").is_none(), "lsp_rename BUKAN baca-saja");
    assert!(rename_tool.needs_approval(&json!({"old_name": "a", "new_name": "b"})));
    assert!(!rename_tool.needs_approval(&json!({"old_name": "a", "new_name": "b", "dry_run": true})));

    // Pastikan tool lsp_diagnostics jalan lewat Ctx
    let cwd = temp_dir("tool-lsp");
    std::fs::write(cwd.join("bad.rs"), "fn broken( {\n").unwrap();
    let ctx = Ctx::new(cwd.clone(), true);
    let diag_out = ro
        .get("lsp_diagnostics")
        .unwrap()
        .run(json!({"path": "bad.rs"}), &ctx)
        .await
        .unwrap();
    assert!(diag_out.contains("bad.rs"));
    std::fs::remove_dir_all(&cwd).ok();

    assert_eq!(parse_slash_command("/lsp"), SlashCommand::Lsp(None));
    assert_eq!(
        parse_slash_command("/lsp refs compute_vat"),
        SlashCommand::Lsp(Some("refs compute_vat".into()))
    );
    for c in COMMANDS {
        assert!(
            !c.args.contains('|'),
            "COMMANDS[{}].args mengandung '|' literal: {}",
            c.cmd,
            c.args
        );
    }
}
