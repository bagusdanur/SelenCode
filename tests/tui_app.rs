//! Tes Fase 4 — TUI: reducer App, popup approval, render TestBackend,
//! diff_view. Semua tanpa terminal sungguhan & tanpa jaringan.

use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use selencode::agent::UiEvent;
use selencode::providers::Usage;
use selencode::security::{ApprovalRequest, Approver};
use selencode::tui::app::{App, LineKind, ToolState};
use selencode::tui::diff_view::{diff_lines, looks_like_diff};

fn app() -> App {
    App::new(
        "/tmp/proyek".to_string(),
        "router".to_string(),
        "model-x".to_string(),
        25,
        0,
        0,
    )
}

#[test]
fn reducer_event_streaming_tool_dan_turn() {
    let mut a = app();
    a.push_user("perbaiki bug");
    a.apply_event(UiEvent::Step {
        current: 1,
        max: 25,
        context_pct: 12,
    });
    a.apply_event(UiEvent::Token("Halo ".into()));
    a.apply_event(UiEvent::Token("dunia".into()));
    a.apply_event(UiEvent::ToolStart {
        name: "read_file".into(),
        args: "{\"path\":\"a.rs\"}".into(),
    });
    a.apply_event(UiEvent::ToolDone {
        name: "read_file".into(),
        summary: "isi file".into(),
        is_error: false,
    });
    a.apply_event(UiEvent::Token("Selesai.".into()));
    a.apply_event(UiEvent::TurnDone {
        usage: Some(Usage {
            input_tokens: 100,
            output_tokens: 40,
            ..Default::default()
        }),
    });

    // Token streaming menyatu dalam 1 baris assistant per segmen.
    let assistants: Vec<&str> = a
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Assistant)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(assistants, vec!["Halo dunia", "Selesai."]);

    // Baris tool berpasangan Start->Done, keadaan Ok, ringkasan menempel.
    let tool = a
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Tool)
        .expect("baris tool ada");
    assert_eq!(tool.tool_state, Some(ToolState::Ok));
    assert!(tool.text.contains("read_file"));
    assert!(tool.text.contains("isi file"));

    // Status: langkah, konteks, token sesi terakumulasi, tidak sibuk.
    assert_eq!(a.step_current, 1);
    assert_eq!(a.context_pct, 12);
    assert_eq!(a.session_in, 100);
    assert_eq!(a.session_out, 40);
    assert!(!a.busy);
}

#[test]
fn reducer_tool_error_dan_model_changed() {
    let mut a = app();
    a.apply_event(UiEvent::ToolStart {
        name: "run_shell".into(),
        args: "cargo test".into(),
    });
    a.apply_event(UiEvent::ToolDone {
        name: "run_shell".into(),
        summary: "ERROR: gagal".into(),
        is_error: true,
    });
    let tool = a.lines.iter().find(|l| l.kind == LineKind::Tool).unwrap();
    assert_eq!(tool.tool_state, Some(ToolState::Err));

    a.apply_event(UiEvent::ModelChanged {
        provider: "claude".into(),
        model: "claude-x".into(),
    });
    assert_eq!(a.provider, "claude");
    assert_eq!(a.model, "claude-x");
}

#[tokio::test]
async fn popup_approval_jawaban_n_dan_y() {
    let mut a = app();

    // Jawaban 'n' -> false sampai ke penanya, popup hilang.
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "edit_file a.rs".into(),
        detail: "diff a.rs\n-lama\n+baru\n".into(),
        reply: tx,
    });
    assert!(a.approval.is_some());
    assert!(a.answer_approval(false));
    assert!(a.approval.is_none());
    assert!(!rx.await.unwrap());

    // Jawaban 'y' -> true.
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "run_shell: ls".into(),
        detail: String::new(),
        reply: tx,
    });
    assert!(a.answer_approval(true));
    assert!(rx.await.unwrap());

    // Antrean approval: permintaan kedua tidak menimpa yang pertama.
    let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
    let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "satu".into(),
        detail: String::new(),
        reply: tx1,
    });
    a.receive_approval(ApprovalRequest {
        title: "dua".into(),
        detail: String::new(),
        reply: tx2,
    });
    assert_eq!(a.approval.as_ref().unwrap().title, "satu");
    assert!(a.answer_approval(true));
    assert_eq!(a.approval.as_ref().unwrap().title, "dua");
    assert!(a.answer_approval(false));
    assert!(a.approval.is_none());
    assert!(rx1.await.unwrap());
    assert!(!rx2.await.unwrap());

    // Tanpa popup: answer mengembalikan false, tidak panik.
    assert!(!a.answer_approval(true));
}

#[tokio::test]
async fn approver_channel_roundtrip() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApprovalRequest>(4);
    let approver = Approver::channel(false, tx);
    let h = tokio::spawn(async move { approver.ask("edit_file x.rs", "+baru").await });
    let req = rx.recv().await.expect("permintaan approval terkirim");
    assert_eq!(req.title, "edit_file x.rs");
    assert_eq!(req.detail, "+baru");
    req.reply.send(true).unwrap();
    assert!(h.await.unwrap());

    // auto_yes pada varian channel: langsung setuju tanpa mengirim apa pun.
    let (tx2, mut rx2) = tokio::sync::mpsc::channel::<ApprovalRequest>(4);
    let auto = Approver::channel(true, tx2);
    assert!(auto.ask("apa pun", "").await);
    assert!(rx2.try_recv().is_err());
}

#[test]
fn render_satu_frame_memuat_sambutan_dan_status() {
    // v0.1.2: header lama digantikan sambutan Moonlit di transkrip —
    // maksud tes sama: identitas + model + status bar tampil dalam frame.
    let mut a = app();
    a.info("SelenCode — halo, aku Selena.");
    a.push_user("halo");
    let backend = TestBackend::new(80, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("SelenCode"), "sambutan harus memuat SelenCode");
    assert!(text.contains("SELENCODE"), "sambutan Moonlit tampil");
    assert!(text.contains("model-x"), "model tampil di sambutan");
    assert!(text.contains("token sesi 0 in / 0 out"), "status bar tampil");
    assert!(text.contains("langkah 0/25"));
}

#[test]
fn render_percakapan_markdown_rapi_tanpa_tanda_mentah() {
    let mut a = app();
    a.push_user("kamu bisa apa aja");
    a.apply_event(UiEvent::Token(
        "1. **Membaca Kode**: gunakan `read_file`.\n2. **Edit**: pakai `edit_file`.".into(),
    ));
    a.apply_event(UiEvent::TurnDone { usage: None });

    let backend = TestBackend::new(100, 30);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("Selena"), "header Selena tampil");
    assert!(text.contains("Membaca Kode"), "teks bold tampil");
    assert!(text.contains("read_file"), "teks kode inline tampil");
    assert!(!text.contains("**Membaca"), "tanda ** mentah tidak boleh tampil");
    assert!(!text.contains("`read_file`"), "tanda ` mentah tidak boleh tampil");
}

#[test]
fn render_kartu_menu_dan_live_slash_palette() {
    use selencode::repl::{
        classify_key_spec, filter_commands, parse_model_action, parse_provider_action, KeySpec,
        ModelAction, ProviderAction,
    };

    assert_eq!(
        parse_model_action("set ag/gemini-3-flash"),
        ModelAction::SetCurrentModel("ag/gemini-3-flash".into())
    );
    assert_eq!(
        parse_model_action("openrouter:anthropic/claude-3.7-sonnet"),
        ModelAction::SwitchAndSetModel {
            provider: "openrouter".into(),
            model: "anthropic/claude-3.7-sonnet".into(),
        }
    );
    assert_eq!(parse_model_action("2"), ModelAction::SelectIndex(2));
    assert_eq!(parse_model_action("set 3"), ModelAction::SelectIndex(3));
    assert_eq!(
        parse_model_action("add gemini-2.5-pro claude-sonnet-4"),
        ModelAction::AddModels(vec![
            "gemini-2.5-pro".into(),
            "claude-sonnet-4".into(),
        ])
    );
    assert_eq!(
        parse_model_action("rm 2"),
        ModelAction::RemoveModel("2".into())
    );
    assert_eq!(parse_model_action("list"), ModelAction::FetchList(None));
    assert_eq!(
        parse_model_action("list claude"),
        ModelAction::FetchList(Some("claude".into()))
    );
    assert_eq!(
        classify_key_spec("OPENROUTER_API_KEY"),
        KeySpec::EnvVar("OPENROUTER_API_KEY".into())
    );
    assert_eq!(
        classify_key_spec("sk-or-v1-abc123"),
        KeySpec::DirectKey("sk-or-v1-abc123".into())
    );
    assert!(matches!(
        parse_provider_action(Some(
            "add openrouter openai gpt-4o https://openrouter.ai/api/v1 OPENROUTER_API_KEY"
        )),
        ProviderAction::Add { .. }
    ));
    assert!(matches!(
        parse_provider_action(Some("add claude anthropic claude-sonnet-4 ANTHROPIC_API_KEY")),
        ProviderAction::Add { base_url: None, .. }
    ));
    assert!(filter_commands("/mo").len() >= 2);
    assert!(filter_commands("/model set x").is_empty());

    // Render kartu menu + popup palette live di TestBackend.
    let mut a = app();
    a.menu("# Model & Provider Aktif\n* router | ag/gemini-3-flash | aktif\n---\n! Tip cepat");
    a.input.insert_str("/mo");

    let backend = TestBackend::new(100, 30);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("Model & Provider Aktif"), "kartu menu tampil");
    assert!(text.contains("Menu Perintah"), "popup live palette `/` tampil");
    assert!(text.contains("/models"), "filter palette cocok /models");
}

#[test]
fn render_popup_approval_tampil() {
    let mut a = app();
    let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "edit_file a.rs".into(),
        detail: "diff a.rs\n-lama\n+baru\n".into(),
        reply: tx,
    });
    let backend = TestBackend::new(80, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("Persetujuan: edit_file a.rs"));
    assert!(text.contains("[y] setuju"));
    assert!(text.contains("[a] selalu edit_file sesi ini"));
    assert!(text.contains("[n] tolak"));
}

#[tokio::test]
async fn approve_category_for_session_otomatis_loloskan_antrean_serupa() {
    use selencode::security::{ApprovalMode, ApprovalPolicy};
    use std::sync::{Arc, Mutex};

    let policy = Arc::new(Mutex::new(ApprovalPolicy::new(ApprovalMode::Ask)));
    let mut a = app();
    a.approval_policy = Some(policy.clone());

    let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
    let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
    let (tx3, rx3) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "edit_file src/a.rs".into(),
        detail: String::new(),
        reply: tx1,
    });
    a.receive_approval(ApprovalRequest {
        title: "edit_file src/b.rs".into(),
        detail: String::new(),
        reply: tx2,
    });
    a.receive_approval(ApprovalRequest {
        title: "run_shell: cargo build".into(),
        detail: String::new(),
        reply: tx3,
    });

    // Tekan [a] pada edit_file -> a.rs & b.rs otomatis setuju, lalu popup lanjut ke run_shell!
    let cat = a.approve_category_for_session().unwrap();
    assert_eq!(cat, "edit_file");
    assert!(rx1.await.unwrap());
    assert!(rx2.await.unwrap());
    assert_eq!(a.approval.as_ref().unwrap().title, "run_shell: cargo build");
    assert!(a.answer_approval(false));
    assert!(!rx3.await.unwrap());
    assert!(policy.lock().unwrap().is_auto_allowed("edit_file"));
    assert!(!policy.lock().unwrap().is_auto_allowed("run_shell"));
}

#[test]
fn render_modal_model_picker_interaktif_saved_manual_dan_katalog() {
    use selencode::agent::ui::ModelPickerData;
    use selencode::tui::app::ModelPickerMode;

    let mut a = app();
    a.apply_event(UiEvent::ModelPicker(ModelPickerData {
        provider: "router".into(),
        providers: vec!["openrouter".into(), "router".into()],
        active_model: "ag/gemini-3-flash".into(),
        saved_models: vec!["ag/gemini-3-flash".into(), "ag/claude-sonnet-4".into()],
        remote_models: vec![
            "ag/gemini-3-flash".into(),
            "ag/claude-sonnet-4".into(),
            "ag/gpt-4.1".into(),
        ],
        open_catalog: false,
        status: None,
    }));

    // 1. Mode Saved: daftar bernomor + tombol shortcut terstruktur tampil.
    let backend = TestBackend::new(96, 28);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("Kelola Model · router"));
    assert!(text.contains("Model Saya (2)"));
    assert!(text.contains("ag/claude-sonnet-4"));
    assert!(text.contains("[a] + manual"));
    assert!(text.contains("[c] reset"));

    // 2. Navigasi & mode Catalog dengan filter live + tanda centang [✓] / [ ] + pesan status.
    let mp = a.model_picker.as_mut().unwrap();
    assert_eq!(mp.next_provider().as_deref(), Some("openrouter"));
    mp.mode = ModelPickerMode::Catalog;
    mp.filter_input = "gpt".into();
    mp.status = Some("Disimpan ke Model Saya".into());
    assert_eq!(mp.filtered_remote().len(), 1);
    assert_eq!(mp.selected_remote().as_deref(), Some("ag/gpt-4.1"));
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let text2: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text2.contains("ag/gpt-4.1"));
    assert!(text2.contains("[Space] pilih 1-per-1"));
    assert!(text2.contains("Disimpan ke Model Saya"));

    // 3. Mode AddManual tampil input inline.
    a.model_picker.as_mut().unwrap().mode = ModelPickerMode::AddManual;
    a.model_picker.as_mut().unwrap().manual_input = "ag/deepseek-r1".into();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let text3: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text3.contains("ag/deepseek-r1"));
    assert!(text3.contains("[Enter] simpan"));
}

#[test]
fn render_mobile_help_rapi_dan_context_model_tampil() {
    use selencode::repl::help_menu_markup;
    use selencode::tui::render_menu_card_width;

    // 1. Kartu /help di layar sempit Termux HP (50 kolom): tidak ada baris overflow
    // dan deskripsi /approve serta /skills tidak terpotong karakter `|`.
    let help = help_menu_markup();
    let lines = render_menu_card_width(&help, 50);
    for l in &lines {
        assert!(
            l.width() <= 50,
            "baris menu mobile meluber (>50): {:?}",
            l.to_string()
        );
    }
    let joined: String = lines
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("/stop"), "/stop tampil di /help");
    assert!(joined.contains("kurangi popup izin"), "deskripsi /approve utuh");
    assert!(joined.contains("self-improvement"), "deskripsi /skills utuh");

    // 2. Status bar mobile (50 kolom) menampilkan kapasitas konteks model AI (Gemini -> 1M).
    let mut a = App::new(
        "/tmp/proyek".into(),
        "router".into(),
        "ag/gemini-4-neon-low".into(),
        25,
        0,
        0,
    );
    a.apply_event(UiEvent::ContextInfo {
        used_tokens: 24_000,
        limit_tokens: 1_048_576,
    });
    a.menu(help);
    let backend = TestBackend::new(50, 32);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("24k/1M"), "kapasitas konteks 1M tampil di status bar mobile");
}

#[tokio::test]
async fn stop_cancel_all_approvals_tolak_aktif_dan_antrean() {
    let mut a = app();
    let (tx1, rx1) = tokio::sync::oneshot::channel::<bool>();
    let (tx2, rx2) = tokio::sync::oneshot::channel::<bool>();
    a.receive_approval(ApprovalRequest {
        title: "edit_file a.rs".into(),
        detail: String::new(),
        reply: tx1,
    });
    a.receive_approval(ApprovalRequest {
        title: "run_shell: cargo build".into(),
        detail: String::new(),
        reply: tx2,
    });
    assert_eq!(a.cancel_all_approvals(), 2);
    assert!(a.approval.is_none());
    assert_eq!(a.approval_pending_count(), 0);
    assert!(!rx1.await.unwrap());
    assert!(!rx2.await.unwrap());
}

#[test]
fn setup_wizard_alur_lengkap_openai_compatible_dan_render_tui() {
    use selencode::tui::app::{SetupStep, SetupWizardState};

    // 1. Alur OpenAI-Compatible Custom (5 langkah)
    let mut w = SetupWizardState::new(Some("Belum ada provider".into()));
    assert_eq!(w.step, SetupStep::ChooseApi);
    assert_eq!(w.selected, 0); // OpenAI-Compatible Custom
    assert!(w.advance().is_none());
    assert_eq!(w.step, SetupStep::ProviderName);

    w.name = "router".into();
    assert!(w.advance().is_none());
    assert_eq!(w.step, SetupStep::BaseUrl);

    // Validasi URL tidak valid ditolak
    w.base_url = "bukan-url".into();
    assert!(w.advance().is_none());
    assert!(w.error.is_some());

    w.base_url = "https://router.ryukomik.com/v1".into();
    assert!(w.advance().is_none());
    assert_eq!(w.step, SetupStep::ApiKey);

    // Masking API key
    w.api_key = "sk-1234567890abcd".into();
    assert!(w.masked_key().contains("••••••"));
    assert!(w.advance().is_none());
    assert_eq!(w.step, SetupStep::ModelId);

    w.model = "ag/gemini-4-neon-low".into();
    let res = w.advance().expect("langkah terakhir harus hasilkan SetupResult");
    assert_eq!(res.name, "router");
    assert_eq!(res.kind, "openai");
    assert_eq!(
        res.base_url.as_deref(),
        Some("https://router.ryukomik.com/v1")
    );
    assert_eq!(res.api_key_or_env, "sk-1234567890abcd");
    assert_eq!(res.model, "ag/gemini-4-neon-low");

    // 2. Alur Anthropic/Gemini melewati langkah BaseUrl otomatis
    let mut w2 = SetupWizardState::new(None);
    w2.selected = 3; // Anthropic Claude
    w2.advance();
    assert_eq!(w2.step, SetupStep::ProviderName);
    w2.advance();
    assert_eq!(w2.step, SetupStep::ApiKey, "Anthropic tidak perlu BaseUrl");

    // 3. Render modal wizard di desktop (80x24) & mobile HP (50x28) tidak panik
    let mut a = app();
    a.setup_wizard = Some(SetupWizardState::new(Some("Selamat datang!".into())));
    for (cols, rows) in [(80, 24), (50, 28)] {
        let backend = TestBackend::new(cols, rows);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Setup Provider SelenCode"));
        assert!(text.contains("OpenAI-Compatible Custom"));
    }
}

#[test]
fn diff_view_warna_dan_hitung() {
    let diff = "diff src/a.rs\n konteks\n-lama\n+baru\n+kedua\n";
    assert!(looks_like_diff(diff));
    assert!(!looks_like_diff("cargo test --release"));
    let (lines, stats) = diff_lines(diff);
    assert_eq!(stats.added, 2);
    assert_eq!(stats.removed, 1);
    // Cari baris +baru: gaya fg hijau; -lama: merah.
    let plus = lines
        .iter()
        .find(|l| l.spans[0].content.as_ref() == "+baru")
        .expect("baris +baru ada");
    assert_eq!(plus.spans[0].style.fg, Some(Color::Green));
    let minus = lines
        .iter()
        .find(|l| l.spans[0].content.as_ref() == "-lama")
        .expect("baris -lama ada");
    assert_eq!(minus.spans[0].style.fg, Some(Color::Red));
    // Header file unified tidak dihitung tambah/kurang.
    let (_, stats2) = diff_lines("--- a/x\n+++ b/x\n+a\n");
    assert_eq!(stats2.added, 1);
    assert_eq!(stats2.removed, 0);
}
