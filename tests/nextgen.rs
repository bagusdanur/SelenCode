//! Tes fitur baru ala OpenCode & Hermes Agent:
//! - `/undo` & `/redo` checkpoint edit file (`WriteFile` / `EditFile`)
//! - Memori persisten SQLite (`/memory`) & format recall ke system prompt
//! - `@file` context expansion (sandboxed, anti-secret, redaksi)
//! - Skills (`/learn`, `/skills`, `/skill`), `/init` (`AGENTS.md`), `/export`
//! - Parsing `!cmd` inline shell & `/provider preset`

use async_trait::async_trait;
use selencode::agent::context::{expand_at_mentions, format_memories_section};
use selencode::agent::run_self_improvement_review;
use selencode::providers::{
    AssistantReply, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec,
};
use selencode::repl::{
    parse_provider_action, parse_slash_command, KeySpec, ProviderAction, SlashCommand,
};
use selencode::skills::{
    export_conversation_md, init_agents_md, list_skills, load_skill, remove_skill, save_skill,
};
use selencode::store::Store;
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-nextgen-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test]
async fn undo_and_redo_write_and_edit_roundtrip() {
    let cwd = temp_dir("undo");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();

    // 1. write_file baru -> undo harus menghapus file; redo membuat ulang.
    reg.get("write_file")
        .unwrap()
        .run(json!({"path": "baru.txt", "content": "versi-1"}), &ctx)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(cwd.join("baru.txt")).unwrap(), "versi-1");

    // 2. edit_file -> ubah ke versi-2
    reg.get("edit_file")
        .unwrap()
        .run(
            json!({"path": "baru.txt", "old": "versi-1", "new": "versi-2"}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(cwd.join("baru.txt")).unwrap(), "versi-2");

    // Undo 1 -> kembali ke versi-1
    {
        let mut st = ctx.undo_stack.lock().unwrap();
        assert_eq!(st.undo_len(), 2);
        let msg = st.undo().unwrap().expect("undo 1 harus berhasil");
        assert!(msg.contains("baru.txt"));
    }
    assert_eq!(std::fs::read_to_string(cwd.join("baru.txt")).unwrap(), "versi-1");

    // Undo 2 -> file baru terhapus
    {
        let mut st = ctx.undo_stack.lock().unwrap();
        let msg = st.undo().unwrap().expect("undo 2 harus berhasil");
        assert!(msg.contains("dihapus"));
    }
    assert!(!cwd.join("baru.txt").exists());

    // Redo 1 -> file kembali versi-1
    {
        let mut st = ctx.undo_stack.lock().unwrap();
        st.redo().unwrap().expect("redo 1 harus berhasil");
    }
    assert_eq!(std::fs::read_to_string(cwd.join("baru.txt")).unwrap(), "versi-1");

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn persistent_memory_store_and_prompt_section() {
    let db = std::env::temp_dir().join(format!(
        "selencode-mem-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let store = Store::open_at(&db).unwrap();
    let id1 = store.add_memory("global", "Selalu jawab singkat").unwrap();
    let id2 = store.add_memory("/proyek/a", "Pakai cargo test").unwrap();
    let _id3 = store.add_memory("/proyek/b", "Pakai pytest").unwrap();

    let list_a = store.list_memories("/proyek/a", 20).unwrap();
    assert_eq!(list_a.len(), 2);
    assert!(list_a.iter().any(|m| m.id == id1 && m.scope == "global"));
    assert!(list_a.iter().any(|m| m.id == id2 && m.scope == "/proyek/a"));

    let section = format_memories_section(&list_a);
    assert!(section.contains("[global] Selalu jawab singkat"));
    assert!(section.contains("[project] Pakai cargo test"));

    assert!(store.remove_memory(id2).unwrap());
    assert_eq!(store.list_memories("/proyek/a", 20).unwrap().len(), 1);
    std::fs::remove_file(&db).ok();
}

#[test]
fn expand_at_mentions_attaches_valid_and_blocks_secrets() {
    let cwd = temp_dir("at");
    std::fs::write(cwd.join("hello.rs"), "fn main() {}\n// API_KEY=superrahasia12345\n").unwrap();
    std::fs::write(cwd.join(".env"), "SECRET=abc").unwrap();

    let (block, files) =
        expand_at_mentions(&cwd, "tolong cek @hello.rs dan @.env serta @../kabur.txt");
    assert_eq!(files, vec!["hello.rs"]);
    assert!(block.contains("fn main() {}"));
    assert!(block.contains("[REDACTED]"));
    assert!(!block.contains("superrahasia12345"));
    assert!(!block.contains("SECRET=abc"));
    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn skills_init_and_export_work() {
    let cwd = temp_dir("skills");
    std::fs::write(cwd.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();

    // /learn + /skills + /skill
    save_skill(&cwd, "Code Review", "Cek bug keamanan dan test dulu.").unwrap();
    let list = list_skills(&cwd);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "code-review");
    let (info, body) = load_skill(&cwd, "code-review").unwrap();
    assert_eq!(info.scope, "project");
    assert!(body.contains("Cek bug keamanan"));

    // /init
    let (agents, created) = init_agents_md(&cwd).unwrap();
    assert!(created);
    assert!(std::fs::read_to_string(&agents).unwrap().contains("Rust (Cargo)"));
    let (_, created2) = init_agents_md(&cwd).unwrap();
    assert!(!created2, "tidak menimpa AGENTS.md yang sudah ada");

    // /export
    let msgs = vec![
        Message::text(Role::User, "halo"),
        Message::text(Role::Assistant, "hai dari Selena"),
    ];
    let exp = export_conversation_md(&cwd, Some("catatan"), "router", "model-x", &msgs).unwrap();
    let md = std::fs::read_to_string(&exp).unwrap();
    assert!(md.contains("halo") && md.contains("hai dari Selena"));

    std::fs::remove_dir_all(&cwd).ok();
}

#[test]
fn repl_inline_shell_new_commands_and_presets() {
    assert_eq!(
        parse_slash_command("!cargo test"),
        SlashCommand::InlineShell("cargo test".into())
    );
    assert_eq!(parse_slash_command("/undo"), SlashCommand::Undo);
    assert_eq!(parse_slash_command("/redo"), SlashCommand::Redo);
    assert_eq!(parse_slash_command("/skills"), SlashCommand::Skills(None));
    assert_eq!(
        parse_slash_command("/skills auto"),
        SlashCommand::Skills(Some("auto".into()))
    );
    assert_eq!(
        parse_slash_command("/skills review"),
        SlashCommand::Skills(Some("review".into()))
    );
    assert_eq!(parse_slash_command("/init"), SlashCommand::Init);
    assert_eq!(parse_slash_command("/details"), SlashCommand::Details);
    assert_eq!(
        parse_slash_command("/approve edits"),
        SlashCommand::Approve(Some("edits".into()))
    );
    assert_eq!(
        selencode::security::ApprovalMode::from_config("edits", false),
        selencode::security::ApprovalMode::AutoEdits
    );
    let mut pol = selencode::security::ApprovalPolicy::new(selencode::security::ApprovalMode::AutoEdits);
    assert!(pol.is_auto_allowed("edit_file"));
    assert!(pol.is_auto_allowed("write_file"));
    assert!(!pol.is_auto_allowed("run_shell"));
    pol.allow_category("run_shell");
    assert!(pol.is_auto_allowed("run_shell"));
    assert_eq!(
        parse_slash_command("/memory add selalu ringkas"),
        SlashCommand::Memory(Some("add selalu ringkas".into()))
    );
    assert_eq!(
        parse_provider_action(Some("preset")),
        ProviderAction::ListPresets
    );
    assert_eq!(
        parse_provider_action(Some("preset ollama qwen2.5-coder:7b")),
        ProviderAction::Add {
            name: "ollama".into(),
            kind: "openai".into(),
            model: "qwen2.5-coder:7b".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            key_spec: KeySpec::DirectKey("ollama".into()),
        }
    );
    assert_eq!(
        parse_provider_action(Some("preset openrouter openai/gpt-4.1 OPENROUTER_API_KEY")),
        ProviderAction::Add {
            name: "openrouter".into(),
            kind: "openai".into(),
            model: "openai/gpt-4.1".into(),
            base_url: Some("https://openrouter.ai/api/v1".into()),
            key_spec: KeySpec::EnvVar("OPENROUTER_API_KEY".into()),
        }
    );
}

struct ReviewPatchMock {
    should_patch: bool,
}

#[async_trait]
impl Provider for ReviewPatchMock {
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        _tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<AssistantReply> {
        if !self.should_patch {
            return Ok(AssistantReply {
                text: "NONE".into(),
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            });
        }
        Ok(AssistantReply {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "sk1".into(),
                name: "manage_skill".into(),
                args: json!({
                    "action": "patch",
                    "name": "ryukomik-comic-scraper",
                    "description": "Scraper chapter & bypass selector HTML Ryukomik",
                    "content": "1. Gunakan selector `.ch-list a`\n2. Hindari rate limit 429 dengan jeda 1s"
                }),
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn autonomous_manage_skill_and_self_improvement_review() {
    let cwd = temp_dir("autoskill");
    let ctx = Ctx::new(cwd.clone(), true);
    let reg = default_registry();
    let tool = reg.get("manage_skill").expect("manage_skill terdaftar");

    // 1. Create awal lewat tool manage_skill (sekaligus uji redaksi secret).
    let out = tool
        .run(
            json!({
                "action": "create",
                "name": "ryukomik-comic-scraper",
                "description": "Scraper awal",
                "content": "Langkah 1: fetch HTML (API_KEY=rahasia12345678)"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.contains("created"), "{out}");
    let (_, body) = load_skill(&cwd, "ryukomik-comic-scraper").unwrap();
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains("rahasia12345678"));

    // 2. Self-improvement review melakukan patch otomatis ala Hermes.
    let history = vec![
        Message::text(Role::User, "perbaiki selector scraper ryukomik"),
        Message::text(Role::Assistant, "Sudah diganti ke .ch-list a dan jeda 1s."),
    ];
    let provider: Arc<dyn Provider> = Arc::new(ReviewPatchMock { should_patch: true });
    let note = run_self_improvement_review(provider, &ctx, &history)
        .await
        .unwrap()
        .expect("harus menghasilkan catatan patch skill");
    assert_eq!(
        note,
        "Self-improvement review: Skill 'ryukomik-comic-scraper' patched"
    );
    let (_, patched_body) = load_skill(&cwd, "ryukomik-comic-scraper").unwrap();
    assert!(patched_body.contains(".ch-list a"));

    // 3. Bila model menjawab NONE -> tidak ada notifikasi.
    let noop: Arc<dyn Provider> = Arc::new(ReviewPatchMock { should_patch: false });
    assert!(run_self_improvement_review(noop, &ctx, &history)
        .await
        .unwrap()
        .is_none());

    // 4. Hapus skill lewat remove_skill.
    assert!(remove_skill(&cwd, "ryukomik-comic-scraper").unwrap());
    assert!(list_skills(&cwd).is_empty());

    std::fs::remove_dir_all(&cwd).ok();
}
