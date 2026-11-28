//! Tes integrasi v0.5.30:
//! 1. Seluruh parser callback & builder kartu inline Telegram baru (`mdl:c/a`, `res:`, `cfg:`, `crn:`, `skl:`, `pln:`)
//!    beserta jaminan panjang `callback_data <= 64` byte.
//! 2. Ekstraksi & transkripsi pesan suara/audio Telegram (`extract_tg_voice_or_audio` + mock HTTP OpenAI & Gemini).
//! 3. Perintah `/cd` (`SlashCommand::Cd`), bilah progres visual (`render_progress_bar`),
//!    dan modal `/resume` TUI (`ResumePickerState` + `UiEvent::ResumePicker` + `UiEvent::CwdChanged`).
//! 4. Compaction terstruktur berbantuan LLM (`compact_messages_with_llm`) + fallback deterministik aman.
//! 5. Deteksi shell SPA (`detect_spa_shell`) untuk fallback headless Chrome pada `fetch_url`.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::context::{
    compact_messages_with_llm, render_progress_bar, STRUCTURED_SUMMARY_MARKER,
};
use selencode::agent::ui::{ResumePickerData, ResumePickerItem, UiEvent};
use selencode::providers::{AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolSpec};
use selencode::repl::{parse_slash_command, SlashCommand, COMMANDS};
use selencode::telegram::{
    build_tg_approve_picker_card, build_tg_catalog_picker_card, build_tg_cron_picker_card,
    build_tg_plan_confirm_card, build_tg_resume_picker_card, build_tg_sandbox_picker_card,
    build_tg_skills_picker_card, extract_tg_voice_or_audio, parse_callback_config,
    parse_callback_cron, parse_callback_model, parse_callback_plan, parse_callback_resume,
    parse_callback_skill, transcribe_audio_bytes, TgConfigCallback, TgCronCallback, TgCronItem,
    TgMessage, TgModelCallback, TgModelEntry, TgPlanCallback, TgResumeCallback, TgResumeItem,
    TgSkillCallback, TgSkillItem,
};
use selencode::tools::web::detect_spa_shell;
use selencode::tui::app::App;
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[test]
fn telegram_inline_callbacks_dan_kartu_baru_valid_dan_aman_64_byte() {
    // 1. Parser callback baru
    assert_eq!(
        parse_callback_model("mdl:c:1:2"),
        Some(TgModelCallback::CatalogPage {
            provider_idx: 1,
            page: 2
        })
    );
    assert_eq!(
        parse_callback_model("mdl:a:0:14"),
        Some(TgModelCallback::AddAndSelectCatalog {
            provider_idx: 0,
            remote_idx: 14
        })
    );
    assert_eq!(parse_callback_resume("res:s:3"), Some(TgResumeCallback::Select(3)));
    assert_eq!(parse_callback_resume("res:s:0"), None);
    assert_eq!(parse_callback_resume("res:new"), Some(TgResumeCallback::NewSession));
    assert_eq!(parse_callback_resume("res:x"), Some(TgResumeCallback::Close));

    assert_eq!(
        parse_callback_config("cfg:ap:edits"),
        Some(TgConfigCallback::SetApprove("edits".to_string()))
    );
    assert_eq!(
        parse_callback_config("cfg:sb:ww"),
        Some(TgConfigCallback::SetSandbox("workspace-write".to_string()))
    );
    assert_eq!(parse_callback_config("cfg:x"), Some(TgConfigCallback::Close));

    assert_eq!(parse_callback_cron("crn:t:7"), Some(TgCronCallback::Toggle(7)));
    assert_eq!(parse_callback_cron("crn:d:7"), Some(TgCronCallback::Delete(7)));
    assert_eq!(parse_callback_cron("crn:x"), Some(TgCronCallback::Close));

    assert_eq!(parse_callback_skill("skl:v:2"), Some(TgSkillCallback::View(2)));
    assert_eq!(parse_callback_skill("skl:x"), Some(TgSkillCallback::Close));

    assert_eq!(parse_callback_plan("pln:run"), Some(TgPlanCallback::Execute));
    assert_eq!(parse_callback_plan("pln:x"), Some(TgPlanCallback::Cancel));

    // 2. Bangun semua kartu & verifikasi seluruh callback_data <= 64 byte
    let cards = vec![
        build_tg_catalog_picker_card(
            "router",
            "ag/gemini-3-flash",
            0,
            "router",
            "openai",
            &[
                TgModelEntry {
                    id: "ag/gemini-3-flash".into(),
                    context_limit: 1_048_576,
                    is_active: true,
                },
                TgModelEntry {
                    id: "anthropic/claude-sonnet-4-6".into(),
                    context_limit: 200_000,
                    is_active: false,
                },
            ],
            0,
        ),
        build_tg_resume_picker_card(
            "/workspace/demo",
            &[TgResumeItem {
                num: 1,
                id_short: "abc1234567".into(),
                title: "Refactor auth middleware".into(),
                msg_count: 14,
                model: "ag/gemini-3-flash".into(),
                is_active: true,
            }],
        ),
        build_tg_approve_picker_card("ask", "(belum ada)"),
        build_tg_sandbox_picker_card("workspace-write", "native"),
        build_tg_cron_picker_card(&[TgCronItem {
            id: 5,
            name: "cek-ci".into(),
            schedule_human: "tiap 15 menit".into(),
            enabled: true,
        }]),
        build_tg_skills_picker_card(
            "# Skills\n* demo | skill demo | project",
            &[TgSkillItem {
                name: "demo-skill".into(),
                scope: "project".into(),
            }],
        ),
        build_tg_plan_confirm_card("1. Edit `src/main.rs`\n2. Jalankan tes"),
    ];

    for card in &cards {
        assert!(!card.html.is_empty());
        assert!(!card.plain.is_empty());
        assert!(!card.rows.is_empty());
        for row in &card.rows {
            for (lbl, cb) in row {
                assert!(!lbl.is_empty());
                assert!(cb.len() <= 64, "callback_data terlalu panjang: {cb}");
            }
        }
    }
}

#[tokio::test]
async fn telegram_voice_ekstraksi_dan_transkripsi_openai_serta_gemini() {
    let voice_msg: TgMessage = serde_json::from_value(json!({
        "message_id": 91,
        "chat": { "id": 1001 },
        "voice": {
            "file_id": "AwACAgIAAxkBA",
            "duration": 6,
            "mime_type": "audio/ogg",
            "file_size": 12345
        }
    }))
    .unwrap();
    let att = extract_tg_voice_or_audio(&voice_msg).expect("voice harus terekstrak");
    assert!(att.is_voice);
    assert_eq!(att.file_id, "AwACAgIAAxkBA");
    assert_eq!(att.mime_type, "audio/ogg");
    assert_eq!(att.duration_secs, Some(6));

    // Mock HTTP server untuk OpenAI `/audio/transcriptions` & Gemini `generateContent`
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.contains("/audio/transcriptions") {
                r#"{"text":"Tolong cek error di main.rs"}"#
            } else {
                r#"{"candidates":[{"content":{"parts":[{"text":"Buat unit test untuk router"}]}}]}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let base = format!("http://{addr}");
    let txt_oai = transcribe_audio_bytes(
        "openai",
        Some(&base),
        "test-key",
        "gpt-4o",
        b"OggS-fake-audio-bytes",
        "voice_91.ogg",
        "audio/ogg",
    )
    .await
    .unwrap();
    assert_eq!(txt_oai, "Tolong cek error di main.rs");

    let txt_gem = transcribe_audio_bytes(
        "gemini",
        Some(&base),
        "test-key",
        "gemini-2.5-flash",
        b"OggS-fake-audio-bytes",
        "voice_91.ogg",
        "audio/ogg",
    )
    .await
    .unwrap();
    assert_eq!(txt_gem, "Buat unit test untuk router");

    let _ = srv.await;
}

#[test]
fn slash_cd_progress_bar_tui_resume_modal_dan_spa_detector() {
    // 1. `/cd` & `/cwd`
    assert_eq!(parse_slash_command("/cd"), SlashCommand::Cd(None));
    assert_eq!(
        parse_slash_command("/cd ../other"),
        SlashCommand::Cd(Some("../other".to_string()))
    );
    assert_eq!(
        parse_slash_command("/cwd src"),
        SlashCommand::Cd(Some("src".to_string()))
    );
    assert!(COMMANDS.iter().any(|c| c.cmd == "/cd"));

    // 2. Bilah progres visual
    assert_eq!(render_progress_bar(0, 10), "░░░░░░░░░░");
    assert_eq!(render_progress_bar(50, 10), "█████░░░░░");
    assert_eq!(render_progress_bar(100, 10), "██████████");

    // 3. TUI ResumePickerState & CwdChanged
    let mut app = App::new("/tmp/a".into(), "router".into(), "gpt-4.1".into(), 0, 0, 0);
    app.apply_event(UiEvent::CwdChanged("/tmp/b".into()));
    assert_eq!(app.cwd, "/tmp/b");

    app.apply_event(UiEvent::ResumePicker(ResumePickerData {
        cwd: "/tmp/b".into(),
        active_session_id: "s1".into(),
        items: vec![
            ResumePickerItem {
                id: "s1".into(),
                title: "Setup Telegram Bot".into(),
                created_at: "2026-11-28T10:00:00Z".into(),
                model: "gpt-4.1".into(),
                msg_count: 8,
                is_active: true,
            },
            ResumePickerItem {
                id: "s2".into(),
                title: "Refactor Database Store".into(),
                created_at: "2026-11-28T09:00:00Z".into(),
                model: "claude-sonnet-4".into(),
                msg_count: 16,
                is_active: false,
            },
        ],
        status: None,
    }));
    let rp = app.resume_picker.as_mut().expect("resume_picker aktif");
    assert_eq!(rp.filtered_items().len(), 2);
    rp.filter_input = "database".into();
    rp.clamp_selection();
    assert_eq!(rp.filtered_items().len(), 1);
    assert_eq!(rp.filtered_items()[0].id, "s2");

    // 4. Deteksi SPA shell untuk headless Chrome fallback
    assert!(detect_spa_shell(
        r#"<!doctype html><html><body><div id="root"></div><noscript>You need to enable JavaScript to run this app.</noscript></body></html>"#
    ));
    assert!(!detect_spa_shell(
        "<!doctype html><html><body><article><h1>Panduan Rust Lengkap</h1><p>Rust adalah bahasa pemrograman sistem yang fokus pada keamanan memori, performa tinggi, dan konkurensi aman tanpa garbage collector. Artikel ini membahas ownership, borrowing, lifetime, async tokio, dan praktik terbaik produksi secara mendalam.</p></article></body></html>"
    ));
}

#[tokio::test]
async fn compaction_llm_summary_dan_fallback_deterministik_aman() {
    struct SummaryOkProvider;
    #[async_trait]
    impl Provider for SummaryOkProvider {
        fn model_name(&self) -> &str {
            "mock-summarizer"
        }
        async fn stream_chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            Ok(AssistantReply {
                text: "1. Tujuan: Memperbaiki auth.\n2. File Diubah: src/auth.rs.\n3. Keputusan: Pakai JWT HS256.\n4. Sisa: Tambah tes integrasi.".into(),
                tool_calls: Vec::new(),
                usage: None,
            })
        }
    }

    struct SummaryErrProvider;
    #[async_trait]
    impl Provider for SummaryErrProvider {
        fn model_name(&self) -> &str {
            "mock-err"
        }
        async fn stream_chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            anyhow::bail!("simulated provider failure")
        }
    }

    // Bangun riwayat 14 pesan termasuk pasangan ToolUse/ToolResult
    let mut msgs = vec![
        Message::text(Role::System, "System prompt tetap beku."),
        Message::text(Role::User, "Mulai kerjakan fitur auth"),
    ];
    for i in 0..6 {
        msgs.push(Message {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: format!("c{i}"),
                name: "read_file".into(),
                args: json!({"path": format!("src/f{i}.rs")}),
            }],
        });
        msgs.push(Message {
            role: Role::Tool,
            content: vec![Block::ToolResult {
                id: format!("c{i}"),
                content: format!("isi file {i} cukup panjang untuk diringkas"),
                is_error: false,
            }],
        });
    }

    let ok_prov: Arc<dyn Provider> = Arc::new(SummaryOkProvider);
    let (compacted, used_llm) = compact_messages_with_llm(&ok_prov, &msgs).await;
    assert!(used_llm, "harus menggunakan ringkasan LLM saat provider sukses");
    assert!(compacted.len() < msgs.len());
    assert_eq!(compacted[0].role, Role::System);
    assert!(
        compacted
            .iter()
            .any(|m| m.plain_text().contains(STRUCTURED_SUMMARY_MARKER) && m.plain_text().contains("JWT HS256")),
        "ringkasan LLM harus tersisip sebagai checkpoint"
    );

    let err_prov: Arc<dyn Provider> = Arc::new(SummaryErrProvider);
    let (fallback, used_llm_err) = compact_messages_with_llm(&err_prov, &msgs).await;
    assert!(!used_llm_err, "harus jatuh ke fallback deterministik saat LLM gagal");
    assert!(fallback.len() < msgs.len());
    assert_eq!(fallback[0].role, Role::System);
}
