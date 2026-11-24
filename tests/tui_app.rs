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
fn render_satu_frame_memuat_header_dan_status() {
    let mut a = app();
    a.info("SelenCode — halo, aku Selena.");
    a.push_user("halo");
    let backend = TestBackend::new(80, 24);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| selencode::tui::draw(f, &mut a)).unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("SelenCode"), "header harus memuat SelenCode");
    assert!(text.contains("model: router/model-x"));
    assert!(text.contains("token sesi 0 in / 0 out"), "status bar tampil");
    assert!(text.contains("langkah 0/25"));
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
