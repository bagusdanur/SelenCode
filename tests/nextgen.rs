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

#[test]
fn model_clear_help_panduan_dan_telegram_gateway_helpers() {
    use selencode::config::TelegramCfg;
    use selencode::repl::{help_menu_markup, parse_model_action, ModelAction};
    use selencode::telegram::{
        parse_callback_approval, parse_telegram_action, split_telegram_chunks, ApprovalDecision,
        TelegramAction,
    };

    // 1. /model clear & /model rm all + /help panduan lengkap
    assert_eq!(parse_model_action("clear"), ModelAction::ClearModels);
    assert_eq!(parse_model_action("rm all"), ModelAction::ClearModels);
    assert_eq!(parse_model_action("rm *"), ModelAction::ClearModels);
    assert_eq!(
        parse_slash_command("/telegram setup 123456:ABCdef 998877"),
        SlashCommand::Telegram(Some("setup 123456:ABCdef 998877".into()))
    );
    let help = help_menu_markup();
    assert!(help.contains("Panduan Cepat Kelola Model"));
    assert!(help.contains("/model clear"));
    assert!(help.contains("/telegram"));

    // 2. Parsing aksi /telegram
    assert_eq!(
        parse_telegram_action(Some("setup 123456:ABC_xyz 778899")),
        TelegramAction::Setup {
            token_spec: KeySpec::DirectKey("123456:ABC_xyz".into()),
            user_id: Some(778899),
        }
    );
    assert_eq!(
        parse_telegram_action(Some("allow 112233")),
        TelegramAction::Allow(112233)
    );
    assert_eq!(
        parse_telegram_action(Some("approve edits")),
        TelegramAction::SetApproval("edits".into())
    );
    assert_eq!(parse_telegram_action(Some("off")), TelegramAction::Disable);

    // 3. Whitelist keamanan TelegramCfg
    let mut tcfg = TelegramCfg::default();
    assert!(tcfg.is_empty());
    assert!(!tcfg.is_allowed(111, 111));
    tcfg.bot_token = Some("999:secret".into());
    tcfg.allowed_users.push(111);
    assert!(tcfg.is_allowed(111, 999));
    assert!(!tcfg.is_allowed(222, 999));
    assert_eq!(tcfg.resolve_token().unwrap(), "999:secret");

    // 4. Parsing callback inline approval Telegram
    assert_eq!(
        parse_callback_approval("ap:42:y"),
        Some((42, ApprovalDecision::ApproveOnce))
    );
    assert_eq!(
        parse_callback_approval("ap:42:a"),
        Some((42, ApprovalDecision::ApproveCategory))
    );
    assert_eq!(
        parse_callback_approval("ap:42:n"),
        Some((42, ApprovalDecision::Deny))
    );
    assert_eq!(parse_callback_approval("invalid"), None);

    // 5. Pemecahan pesan panjang aman UTF-8 (emoji & baris)
    let long = "baris halo 🚀\n".repeat(40);
    let chunks = split_telegram_chunks(&long, 120);
    assert!(chunks.len() >= 4);
    for c in &chunks {
        assert!(c.chars().count() <= 120);
    }

    // 6. Redaksi token bot agar tidak bocor di pesan error jaringan
    let leaked = "error sending request for url (https://api.telegram.org/bot999:SECRET_TOK/getUpdates)";
    let safe = selencode::telegram::redact_tg_token(leaked, "999:SECRET_TOK");
    assert!(!safe.contains("999:SECRET_TOK"));
    assert!(safe.contains("[BOT_TOKEN]"));

    // 7. Pembatalan approval tertunda (/cancel) melepas oneshot chat terkait saja
    let hub = selencode::telegram::TgApprovalHub::new();
    let pol = std::sync::Arc::new(std::sync::Mutex::new(
        selencode::security::ApprovalPolicy::default(),
    ));
    let (tx1, mut rx1) = tokio::sync::oneshot::channel();
    let (tx2, mut rx2) = tokio::sync::oneshot::channel();
    hub.register_for_test(
        100,
        selencode::telegram::PendingTgApproval {
            chat_id: 100,
            message_id: 1,
            base_text: "a".into(),
            category: "edit_file".into(),
            policy: pol.clone(),
            reply: tx1,
        },
    );
    hub.register_for_test(
        200,
        selencode::telegram::PendingTgApproval {
            chat_id: 200,
            message_id: 2,
            base_text: "b".into(),
            category: "run_shell".into(),
            policy: pol,
            reply: tx2,
        },
    );
    assert_eq!(hub.cancel_chat_approvals(100), 1);
    assert_eq!(rx1.try_recv(), Ok(false));
    assert!(rx2.try_recv().is_err());

    // 8. Perintah /stop & deteksi kapasitas context window model AI
    assert_eq!(parse_slash_command("/stop"), SlashCommand::Stop);
    assert_eq!(parse_slash_command("/cancel"), SlashCommand::Stop);
    for c in selencode::repl::COMMANDS {
        assert!(
            !c.args.contains('|'),
            "args '{}' pada {} tidak boleh berisi `|` agar kolom /help mobile tidak pecah",
            c.args,
            c.cmd
        );
    }
    assert_eq!(
        selencode::agent::context::infer_model_context_limit("ag/gemini-4-neon-low"),
        1_048_576
    );
    assert_eq!(
        selencode::agent::context::infer_model_context_limit("ag/claude-sonnet-4"),
        200_000
    );
    assert_eq!(
        selencode::agent::context::infer_model_context_limit("openai/gpt-4.1"),
        1_047_576
    );
    assert_eq!(
        selencode::agent::context::infer_model_context_limit("gpt-5"),
        400_000
    );
    assert_eq!(
        selencode::agent::context::infer_model_context_limit("deepseek/deepseek-r1"),
        128_000
    );
    assert_eq!(
        selencode::agent::context::format_context_compact(1_048_576),
        "1M"
    );
    assert_eq!(
        selencode::agent::context::format_context_compact(200_000),
        "200k"
    );
    assert_eq!(
        selencode::agent::context::format_context_compact(2_500),
        "2.5k"
    );

    // 9. Sanitize pesan setelah interupsi di tengah tool_call (cegah HTTP 400 provider)
    use selencode::agent::context::sanitize_interrupted_messages;
    let mut msgs = vec![
        Message::text(Role::User, "tolong cek file"),
        Message {
            role: Role::Assistant,
            content: vec![
                selencode::providers::Block::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "a.rs"}),
                },
                selencode::providers::Block::ToolUse {
                    id: "call_2".into(),
                    name: "run_shell".into(),
                    args: json!({"command": "cargo test"}),
                },
            ],
        },
    ];
    let syn = sanitize_interrupted_messages(&mut msgs).expect("harus sisipkan ToolResult sintetis");
    assert_eq!(syn.role, Role::Tool);
    assert_eq!(syn.content.len(), 2);
    assert_eq!(msgs.len(), 3);
    // Pemanggilan kedua saat ekor bukan dangling ToolUse tidak menambah apa pun.
    assert!(sanitize_interrupted_messages(&mut msgs).is_none());

    // 10. Perintah /setup & Config::empty() untuk onboarding pertama kali tanpa crash
    assert_eq!(parse_slash_command("/setup"), SlashCommand::Setup);
    let empty_cfg = selencode::config::Config::empty();
    assert!(empty_cfg.providers.is_empty());
    let err = empty_cfg.resolve_provider(None).unwrap_err().to_string();
    assert!(err.contains("/setup"), "pesan error harus arahkan ke /setup: {err}");
}
