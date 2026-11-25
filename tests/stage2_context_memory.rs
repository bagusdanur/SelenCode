//! Tes Tahap 2 — Konteks & Memori (Smart Context Retrieval + Structured Project Memory).

use selencode::agent::context::format_memories_section;
use selencode::context_index::{format_hits_for_prompt, tokenize_query, RepoIndex};
use selencode::outline::{extract_imports, extract_symbols};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::security::contains_secret;
use selencode::store::{normalize_memory_category, Store};
use selencode::tools::{default_registry, read_only_registry, Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn temp_dir(prefix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "selencode-stage2-{prefix}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn tree_sitter_extracts_symbols_and_imports_across_languages() {
    let rs = r#"
use crate::auth::TokenValidator;
mod billing;

pub struct PaymentGateway {
    pub timeout: u64,
}

impl PaymentGateway {
    pub fn charge_invoice(&self, amount: u64) -> bool {
        amount > 0
    }
}
"#;
    let syms = extract_symbols(rs, "rs").unwrap();
    assert!(syms.iter().any(|s| s.kind == "struct" && s.name == "PaymentGateway"));
    assert!(syms.iter().any(|s| s.kind == "fn" && s.name == "charge_invoice"));
    let imps = extract_imports(rs, "rs");
    assert!(imps.contains(&"auth".to_string()));
    assert!(imps.contains(&"billing".to_string()));

    let py = r#"
from services.mailer import send_welcome
import sqlite3

class OrderProcessor:
    def dispatch_order(self, order_id):
        return order_id
"#;
    let py_syms = extract_symbols(py, "py").unwrap();
    assert!(py_syms.iter().any(|s| s.kind == "class" && s.name == "OrderProcessor"));
    assert!(py_syms.iter().any(|s| s.kind == "def" && s.name == "dispatch_order"));
    let py_imps = extract_imports(py, "py");
    assert!(py_imps.iter().any(|i| i.contains("mailer")));

    let ts = r#"
import { verifyJwt } from './security/jwt.ts';

export interface SessionPayload {
    userId: string;
}

export const buildSession = (id: string) => ({ userId: id });
"#;
    let ts_syms = extract_symbols(ts, "ts").unwrap();
    assert!(ts_syms.iter().any(|s| s.kind == "interface" && s.name == "SessionPayload"));
    assert!(ts_syms.iter().any(|s| s.name == "buildSession"));
    let ts_imps = extract_imports(ts, "ts");
    assert!(ts_imps.iter().any(|i| i.contains("jwt")));
}

#[test]
fn incremental_repo_index_skips_unchanged_prunes_deleted_and_ignores_secrets() {
    let cwd = temp_dir("inc");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/auth.rs"),
        "pub fn verify_session_token(tok: &str) -> bool { !tok.is_empty() }\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("src/utils.rs"),
        "pub fn format_date() -> &'static str { \"2026\" }\n",
    )
    .unwrap();
    // File rahasia tidak boleh pernah terindeks!
    std::fs::write(cwd.join(".env"), "OPENAI_API_KEY=sk-supersecret12345678\n").unwrap();
    std::fs::write(cwd.join("server.key"), "-----BEGIN PRIVATE KEY-----\nabc\n").unwrap();

    let mut idx = RepoIndex::new(cwd.clone());
    let s1 = idx.refresh();
    assert_eq!(s1.total_files, 2);
    assert_eq!(s1.indexed_new_or_changed, 2);
    assert_eq!(s1.reused_cached, 0);
    assert!(idx.get(".env").is_none());
    assert!(idx.get("server.key").is_none());

    // Refresh kedua tanpa perubahan -> semua memakai cache inkremental.
    let s2 = idx.refresh();
    assert_eq!(s2.total_files, 2);
    assert_eq!(s2.indexed_new_or_changed, 0);
    assert_eq!(s2.reused_cached, 2);

    // Ubah 1 file & hapus 1 file -> hanya 1 yang diparse ulang dan 1 dihapus dari indeks.
    std::fs::write(
        cwd.join("src/auth.rs"),
        "pub fn verify_session_token(tok: &str) -> bool { tok.len() > 5 }\npub fn revoke_session() {}\n",
    )
    .unwrap();
    std::fs::remove_file(cwd.join("src/utils.rs")).unwrap();

    let s3 = idx.refresh();
    assert_eq!(s3.total_files, 1);
    assert_eq!(s3.indexed_new_or_changed, 1);
    assert_eq!(s3.removed_files, 1);
    assert!(idx
        .get("src/auth.rs")
        .unwrap()
        .symbols
        .iter()
        .any(|s| s.name == "revoke_session"));

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn smart_retrieval_ranks_symbols_boosts_dependencies_and_respects_budget_and_redaction() {
    let cwd = temp_dir("rank");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(
        cwd.join("src/token_engine.rs"),
        "pub struct TokenEngine;\n\
         pub fn calculate_refund_total(base: u64) -> u64 {\n\
             // API_KEY=sk-secretvalue99999999\n\
             base + 10\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("src/checkout_handler.rs"),
        "use crate::token_engine::calculate_refund_total;\n\
         pub fn handle_checkout() -> u64 { calculate_refund_total(50) }\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join("src/unrelated.rs"),
        "pub fn render_footer() -> &'static str { \"ok\" }\n",
    )
    .unwrap();

    let mut idx = RepoIndex::new(cwd.clone());
    let hits = idx.query("tolong cek calculate_refund_total", 5, 2048);
    assert!(!hits.is_empty());
    // File dengan definisi simbol tepat harus di urutan #1!
    assert_eq!(hits[0].rel_path, "src/token_engine.rs");
    assert!(hits[0]
        .matched_symbols
        .iter()
        .any(|s| s.name == "calculate_refund_total"));
    // File caller yang meng-import token_engine ikut terbawa oleh dependency boost!
    assert!(hits.iter().any(|h| h.rel_path == "src/checkout_handler.rs"));
    // File tidak relevan tidak boleh muncul.
    assert!(!hits.iter().any(|h| h.rel_path == "src/unrelated.rs"));

    // Rahasia dalam snippet WAJIB teredaksi!
    assert!(hits[0].snippet.contains("[REDACTED]"));
    assert!(!hits[0].snippet.contains("sk-secretvalue99999999"));

    // Format prompt menghormati batas byte ketat.
    let prompt_block = format_hits_for_prompt(&hits, 350);
    assert!(prompt_block.len() <= 450);

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn query_tokenizer_handles_camel_case_snake_case_and_stopwords() {
    let toks = tokenize_query("tolong cek fungsi LoopGuard dan verify_project di src");
    assert!(toks.contains(&"loop".to_string()));
    assert!(toks.contains(&"guard".to_string()));
    assert!(toks.contains(&"verify_project".to_string()));
    assert!(!toks.contains(&"tolong".to_string()));
    assert!(!toks.contains(&"dan".to_string()));
}

#[tokio::test]
async fn retrieve_context_tool_is_read_only_and_works() {
    let cwd = temp_dir("tool-ret");
    std::fs::write(
        cwd.join("parser.rs"),
        "pub fn parse_invoice_xml(raw: &str) -> usize { raw.len() }\n",
    )
    .unwrap();

    // retrieve_context harus tersedia di default_registry DAN read_only_registry!
    let ro = read_only_registry();
    let tool = ro.get("retrieve_context").expect("retrieve_context di read_only");
    assert!(!tool.needs_approval(&json!({"query": "parse_invoice_xml"})));

    let ctx = Ctx::new(cwd.clone(), true);
    let out = tool
        .run(json!({"query": "parse_invoice_xml", "max_results": 3}), &ctx)
        .await
        .unwrap();
    assert!(out.contains("parser.rs"));
    assert!(out.contains("parse_invoice_xml"));

    // manage_memory hanya ada di default_registry, BUKAN di read_only_registry!
    assert!(default_registry().get("manage_memory").is_some());
    assert!(ro.get("manage_memory").is_none());

    std::fs::remove_dir_all(&cwd).ok();
}

#[tokio::test]
async fn structured_project_memory_crud_grouping_migration_and_secret_guard() {
    let db = std::env::temp_dir().join(format!(
        "selencode-stage2-mem-{}.db",
        uuid::Uuid::new_v4().simple()
    ));

    // Simulasikan database lama tanpa kolom `category` dan `updated_at` untuk uji migrasi aman!
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (id INTEGER PRIMARY KEY, scope TEXT, content TEXT, ts TEXT);
             INSERT INTO memories (scope, content, ts) VALUES ('/proj/x', 'Catatan lama', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
    }

    let store = Arc::new(Store::open_at(&db).unwrap());
    let old_list = store.list_memories("/proj/x", 20).unwrap();
    assert_eq!(old_list.len(), 1);
    assert_eq!(old_list[0].category, "general");

    // Simpan memori terstruktur ke berbagai kategori.
    let id_arch = store
        .add_memory_categorized("/proj/x", "architecture", "Backend Rust modular di src/")
        .unwrap();
    let id_cmd = store
        .add_memory_categorized("/proj/x", "commands", "Tes pakai cargo test --workspace")
        .unwrap();
    assert_eq!(normalize_memory_category("arsitektur"), "arch");
    assert_eq!(normalize_memory_category("cmd"), "commands");

    // Update memori.
    assert!(store
        .update_memory(id_cmd, Some("commands"), "Tes pakai cargo test && cargo clippy")
        .unwrap());
    let cmds = store.list_memories_by_category("/proj/x", "commands", 10).unwrap();
    assert_eq!(cmds.len(), 1);
    assert!(cmds[0].content.contains("cargo clippy"));

    // Guard anti-rahasia WAJIB menolak token / API key / password / Telegram bot token!
    assert!(contains_secret("OPENAI_API_KEY=sk-1234567890abcdef"));
    assert!(contains_secret("123456789:AAH_abcdefghijklmnopqrstuvwxyz123456"));
    assert!(store
        .add_memory_categorized("/proj/x", "decisions", "Pakai key sk-1234567890abcdef")
        .is_err());
    assert!(store
        .update_memory(id_arch, None, "password=supersecretpass123")
        .is_err());

    // Format section prompt terstruktur per kategori & tetap kompatibel [project]/[global].
    let all = store.list_memories("/proj/x", 20).unwrap();
    let sec = format_memories_section(&all);
    assert!(sec.contains("Struktur & Arsitektur Proyek (arch)"));
    assert!(sec.contains("Perintah Build, Test, & Lint (commands)"));
    assert!(sec.contains("[project] Backend Rust modular di src/"));

    // Uji tool `manage_memory` end-to-end.
    let reg = default_registry();
    let mem_tool = reg.get("manage_memory").unwrap();
    let ctx = Ctx::new(PathBuf::from("/proj/x"), true).with_store(store.clone(), "sess-1".into());

    let saved = mem_tool
        .run(
            json!({"action": "save", "category": "issues", "content": "Hindari pipe literal di COMMANDS.args"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(saved.contains("issues"));

    let rejected = mem_tool
        .run(
            json!({"action": "save", "category": "general", "content": "ghp_1234567890abcdefghijklmnop"}),
            &ctx,
        )
        .await;
    assert!(rejected.is_err());

    let listed = mem_tool.run(json!({"action": "list"}), &ctx).await.unwrap();
    assert!(listed.contains("Hindari pipe literal"));

    assert!(store.remove_memory(id_arch).unwrap());
    std::fs::remove_file(&db).ok();
}

#[test]
fn stage2_slash_commands_parse_and_respect_mobile_layout() {
    assert_eq!(parse_slash_command("/index"), SlashCommand::Index(None));
    assert_eq!(
        parse_slash_command("/index auth"),
        SlashCommand::Index(Some("auth".into()))
    );
    assert_eq!(
        parse_slash_command("/context verify"),
        SlashCommand::Index(Some("verify".into()))
    );
    // Pastikan tidak ada `|` literal di COMMANDS[].args agar menu mobile tetap rapi.
    for c in COMMANDS {
        assert!(
            !c.args.contains('|'),
            "COMMANDS[{}].args mengandung '|' literal: {}",
            c.cmd,
            c.args
        );
    }
}
