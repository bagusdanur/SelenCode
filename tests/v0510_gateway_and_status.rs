//! Uji penerimaan v0.5.10 — Perbaikan 1 (Ketahanan Gateway Anti-Gantung + Batas Memori)
//! & Perbaikan 2 (Pembukuan Watch, Laporan Gagal Kelas Satu, & Status Segar Bertanggal).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use selencode::agent::context::build_system_prompt;
use selencode::agent::plan::TurnMode;
use selencode::agent::run_turn_counted;
use selencode::config::Config;
use selencode::gateway_health::{
    estimate_messages_bytes, format_recovery_notice, spawn_os_watchdog,
    trim_gateway_session_memory_with_limits, GatewayHealthClock, StallTracker, WatchdogVerdict,
};
use selencode::providers::openai::OpenAiProvider;
use selencode::providers::router::{
    is_retryable_error, DynamicRouterProvider, ModelRoute, RouterTelemetry,
};
use selencode::providers::{
    AssistantReply, Block, Message, Provider, ProviderTimeouts, Role, StreamEvent, ToolSpec, Usage,
};
use selencode::sandbox::SandboxController;
use selencode::store::Store;
use selencode::tools::toolsets::select_initial_toolsets;
use selencode::tools::undo::{FileSnapshot, UndoStack, MAX_UNDO_TOTAL_BYTES};
use selencode::tools::watch::WatchJob;
use selencode::tools::{default_registry, Ctx, Tool};
use selencode::watch::{
    evaluate_and_poll_watch, parse_watch_output, persist_watch_transition,
    poll_active_watches_for_cli, WatchStateKind,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode_v0510_{tag}_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct HealthyMockProvider {
    label: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for HealthyMockProvider {
    fn model_name(&self) -> &str {
        &self.label
    }
    async fn stream_chat(
        &self,
        _msgs: &[Message],
        _tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let text = format!("Balasan sehat dari {} (#{n})", self.label);
        let _ = tx.send(StreamEvent::Token(text.clone())).await;
        Ok(AssistantReply {
            text,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 120,
                output_tokens: 20,
            }),
        })
    }
}

// =========================================================================
// PERBAIKAN 1 (#1): Provider membisu & macet di tengah stream -> batalkan + fallback
// =========================================================================

#[tokio::test]
async fn p1_silent_provider_aborts_on_response_timeout_and_falls_back() {
    // Server TCP mock yang menerima koneksi & membaca request, lalu membisu total (0 byte balasan).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // Membisu selama 30 detik (jauh melebihi timeout 1 detik tes).
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let short_timeouts = ProviderTimeouts {
        connect: Duration::from_secs(2),
        response: Duration::from_secs(1),
        stream_idle: Duration::from_secs(1),
        total: Duration::from_secs(4),
    };
    let silent_primary: Arc<dyn Provider> = Arc::new(OpenAiProvider::with_timeouts(
        format!("http://{addr}/v1"),
        "test-key".into(),
        "silent-primary".into(),
        1024,
        short_timeouts,
    ));

    // 1. Panggilan langsung ke provider membisu HARUS gagal dalam ~1-2 detik (tidak menggantung!).
    let t0 = Instant::now();
    let (tx, _rx) = mpsc::channel(8);
    let direct_err = silent_primary
        .stream_chat(&[Message::text(Role::User, "halo")], &[], tx)
        .await
        .unwrap_err();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "provider membisu harus batal cepat, bukan {elapsed:?}"
    );
    let err_msg = direct_err.to_string();
    assert!(
        is_retryable_error(&err_msg),
        "error timeout provider wajib dianggap retryable/fallback-eligible: {err_msg}"
    );

    // 2. Lewat DynamicRouterProvider: provider membisu otomatis dialihkan ke fallback dan permintaan sukses!
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let server2 = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener2.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    let primary2: Arc<dyn Provider> = Arc::new(OpenAiProvider::with_timeouts(
        format!("http://{addr2}/v1"),
        "test-key".into(),
        "silent-primary-2".into(),
        1024,
        short_timeouts,
    ));
    let fb_calls = Arc::new(AtomicUsize::new(0));
    let fallback: Arc<dyn Provider> = Arc::new(HealthyMockProvider {
        label: "fallback-ok".into(),
        calls: fb_calls.clone(),
    });
    let tel = Arc::new(Mutex::new(RouterTelemetry::default()));
    let routed = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("primary", "silent-primary-2", primary2, None, None, None, true),
            ModelRoute::new("fb", "fallback-ok", fallback, None, None, None, false),
        ],
        false,
        HashMap::new(),
        0,
        tel.clone(),
    )
    .with_backoff_ms(10);

    let (tx2, _rx2) = mpsc::channel(8);
    let reply = routed
        .stream_chat(&[Message::text(Role::User, "uji fallback")], &[], tx2)
        .await
        .unwrap();
    assert!(reply.text.contains("Balasan sehat dari fallback-ok"));
    assert_eq!(fb_calls.load(Ordering::SeqCst), 1);

    server.abort();
    server2.abort();
}

#[tokio::test]
async fn p1_mid_stream_stalled_provider_aborts_on_stream_idle_and_falls_back() {
    // Server HTTP mock yang mengirim header 200 + 1 chunk SSE lalu membisu tanpa EOF.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
                let first_chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"sebagian \"}}]}\n\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(first_chunk.as_bytes()).await;
                let _ = sock.flush().await;
                // Macet di tengah stream!
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    let short_timeouts = ProviderTimeouts {
        connect: Duration::from_secs(2),
        response: Duration::from_secs(3),
        stream_idle: Duration::from_secs(1),
        total: Duration::from_secs(10),
    };
    let stalled_primary: Arc<dyn Provider> = Arc::new(OpenAiProvider::with_timeouts(
        format!("http://{addr}/v1"),
        "test-key".into(),
        "stalled-stream".into(),
        1024,
        short_timeouts,
    ));

    let t0 = Instant::now();
    let (tx, mut rx) = mpsc::channel(16);
    let err = stalled_primary
        .stream_chat(&[Message::text(Role::User, "halo")], &[], tx)
        .await
        .unwrap_err();
    assert!(t0.elapsed() < Duration::from_secs(5));
    let mut got_partial = String::new();
    while let Ok(StreamEvent::Token(tok)) = rx.try_recv() {
        got_partial.push_str(&tok);
    }
    assert_eq!(got_partial, "sebagian ");
    let msg = err.to_string();
    assert!(
        msg.contains("stream provider macet") || msg.contains("ketidakaktifan"),
        "pesan error macet tengah stream harus jelas: {msg}"
    );
    assert!(
        is_retryable_error(&msg),
        "error macet tengah stream wajib memicu rantai fallback: {msg}"
    );

    // Uji fallback otomatis sesudah stream macet
    let fb_calls = Arc::new(AtomicUsize::new(0));
    let fallback: Arc<dyn Provider> = Arc::new(HealthyMockProvider {
        label: "fallback-stream".into(),
        calls: fb_calls.clone(),
    });
    let tel = Arc::new(Mutex::new(RouterTelemetry::default()));
    let routed = DynamicRouterProvider::new(
        vec![
            ModelRoute::new("primary", "stalled-stream", stalled_primary, None, None, None, true),
            ModelRoute::new("fb", "fallback-stream", fallback, None, None, None, false),
        ],
        false,
        HashMap::new(),
        0,
        tel,
    )
    .with_backoff_ms(10);
    let (tx2, _rx2) = mpsc::channel(16);
    let reply = routed
        .stream_chat(&[Message::text(Role::User, "uji")], &[], tx2)
        .await
        .unwrap();
    assert!(reply.text.contains("fallback-stream"));
    assert_eq!(fb_calls.load(Ordering::SeqCst), 1);

    server.abort();
}

// =========================================================================
// PERBAIKAN 1 (#2 & #3): Turn timeout membatalkan task provider (no socket leak) + StallTracker
// =========================================================================

#[tokio::test]
async fn p1_turn_timeout_drops_and_aborts_provider_task_and_next_turn_works() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct HangOnceThenHealthyProvider {
        call_no: AtomicUsize,
        aborted_first: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Provider for HangOnceThenHealthyProvider {
        fn model_name(&self) -> &str {
            "hang-once"
        }
        async fn stream_chat(
            &self,
            _msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.call_no.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                let _guard = DropFlag(self.aborted_first.clone());
                // Gantung selama 60 detik kecuali dibatalkan oleh `AbortOnDrop`.
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            }
            Ok(AssistantReply {
                text: "Giliran berikutnya berjalan mulus.".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 15,
                }),
            })
        }
    }

    let dir = temp_dir("turn-timeout");
    let aborted = Arc::new(AtomicBool::new(false));
    let prov: Arc<dyn Provider> = Arc::new(HangOnceThenHealthyProvider {
        call_no: AtomicUsize::new(0),
        aborted_first: aborted.clone(),
    });
    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true);
    let mut msgs = Vec::new();

    // Giliran 1: terkena batas waktu giliran 250ms -> harus langsung membatalkan task provider!
    let res1 = tokio::time::timeout(
        Duration::from_millis(250),
        run_turn_counted(prov.clone(), &reg, &ctx, &mut msgs, "giliran macet", 4),
    )
    .await;
    assert!(res1.is_err(), "giliran 1 harus kena timeout");

    // Beri sedikit waktu bagi runtime Tokio menjalankan Drop pada task provider yang di-abort.
    for _ in 0..20 {
        if aborted.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        aborted.load(Ordering::SeqCst),
        "task provider WAJIB langsung dibatalkan (AbortOnDrop) agar soket tidak bocor!"
    );

    // Giliran 2 di sesi yang sama harus langsung berhasil tanpa macet.
    let (ans2, _) = run_turn_counted(prov, &reg, &ctx, &mut msgs, "pesan berikutnya", 4)
        .await
        .unwrap();
    assert!(ans2.contains("Giliran berikutnya berjalan mulus"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn p1_stall_tracker_emits_once_pauses_for_approval_and_resets_silently() {
    let t0 = Instant::now();
    let mut tracker = StallTracker::new(Duration::from_secs(180), t0);

    // Belum melewati ambang
    assert!(tracker.check_stall(t0 + Duration::from_secs(179)).is_none());

    // Melewati ambang -> tepat 1 pemberitahuan
    let msg1 = tracker.check_stall(t0 + Duration::from_secs(185));
    assert!(msg1.is_some());
    assert!(msg1.unwrap().contains("3 menit"));
    assert!(tracker.is_notified());

    // Masih di episode yang sama -> tidak boleh spam!
    assert!(tracker.check_stall(t0 + Duration::from_secs(300)).is_none());

    // Ada kemajuan -> menutup episode diam-diam
    let t_prog = t0 + Duration::from_secs(310);
    tracker.record_progress(t_prog);
    assert!(!tracker.is_notified());

    // Dijeda saat menunggu tombol approval / ask_user -> tidak boleh memicu stall!
    tracker.set_paused(true, t_prog + Duration::from_secs(10));
    assert!(tracker.check_stall(t_prog + Duration::from_secs(600)).is_none());

    // Dilanjutkan setelah user menjawab -> timer mulai dari waktu lanjut
    let t_resume = t_prog + Duration::from_secs(610);
    tracker.set_paused(false, t_resume);
    assert!(tracker.check_stall(t_resume + Duration::from_secs(100)).is_none());
    let msg2 = tracker.check_stall(t_resume + Duration::from_secs(185));
    assert!(msg2.is_some(), "episode macet kedua harus mengirim 1 peringatan baru");
}

// =========================================================================
// PERBAIKAN 1 (#4): Watchdog OS-thread + pemulihan sekali pakai + offset + persistensi DB
// =========================================================================

#[test]
fn p1_os_watchdog_triggers_records_recovery_and_preserves_watches_and_cron() {
    let dir = temp_dir("watchdog");
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());

    // Simpan watch aktif, cron aktif, & offset polling sebelum simulasi macet
    let exp = selencode::watch::compute_watch_expires_at(Utc::now(), 3600);
    store
        .add_job_watch(
            "Comic-auto #42",
            777001,
            &dir.to_string_lossy(),
            "file",
            "status.txt",
            "",
            "",
            60,
            3600,
            &exp,
        )
        .unwrap();
    store
        .add_cron_job(
            "cek-pagi",
            "every:3600",
            "Cek status",
            &dir.to_string_lossy(),
            &[],
            false,
            777001,
            &exp,
        )
        .unwrap();
    store.save_gateway_offset(987654).unwrap();

    // Paksa jam polling tampak macet 400 detik (di atas ambang 300 detik)
    let clock = Arc::new(GatewayHealthClock::new(777001));
    let now_ms = selencode::gateway_health::now_epoch_ms();
    clock.force_timestamps_for_test(now_ms.saturating_sub(400_000), now_ms);

    let fired: Arc<Mutex<Option<WatchdogVerdict>>> = Arc::new(Mutex::new(None));
    let fired_c = fired.clone();
    let handle = spawn_os_watchdog(
        clock.clone(),
        store.clone(),
        300,
        Duration::from_millis(50),
        move |v| {
            *fired_c.lock().unwrap() = Some(v);
        },
    );
    handle.join().unwrap();

    let verdict = fired.lock().unwrap().clone().expect("watchdog wajib terpicu");
    assert!(matches!(verdict, WatchdogVerdict::PollStalled { .. }));

    // Simulasi proses gateway baru bangun sesudah restart systemd (buka Store baru):
    drop(store);
    let store2 = Store::open_at(&db_path).unwrap();

    // 1. Recovery marker terbaca tepat 1x (anti-loop restart)
    let rec = store2
        .take_gateway_recovery()
        .unwrap()
        .expect("marker pemulihan wajib ada sesudah watchdog keluar");
    assert_eq!(rec.0, 777001);
    assert!(rec.1.contains("polling Telegram macet"));
    let notice = format_recovery_notice(&rec.1, &rec.2);
    assert!(notice.contains("pulih lewat restart otomatis"));
    assert!(
        store2.take_gateway_recovery().unwrap().is_none(),
        "marker pemulihan wajib langsung terhapus setelah diambil (tepat sekali pakai)"
    );

    // 2. Offset terakhir tetap tersimpan agar pesan tertahan tidak dibuang `skip_backlog()`
    assert_eq!(store2.load_gateway_offset().unwrap(), Some(987654));

    // 3. Watch aktif & cron aktif utuh di SQLite sesudah restart!
    assert_eq!(store2.list_job_watches(None, true).unwrap().len(), 1);
    assert_eq!(store2.list_cron_jobs().unwrap().len(), 1);

    // 4. Default konfigurasi timeout & watchdog v0.5.10 aktif walau tanpa entri toml
    let empty_cfg = Config::empty();
    assert_eq!(empty_cfg.timeouts.connect_secs, 20);
    assert_eq!(empty_cfg.timeouts.response_secs, 60);
    assert_eq!(empty_cfg.timeouts.stream_idle_secs, 90);
    assert_eq!(empty_cfg.timeouts.total_secs, 300);
    assert_eq!(empty_cfg.telegram.turn_timeout_secs, 900);
    assert_eq!(empty_cfg.telegram.stall_notice_secs, 180);
    assert_eq!(empty_cfg.telegram.poll_watchdog_secs, 300);

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 1 (#5): Batas retensi memori (UndoStack 32 MB + trim sesi gateway)
// =========================================================================

#[test]
fn p1_memory_bounds_cap_undo_stack_and_gateway_session_ram() {
    // 1. UndoStack tidak boleh melampaui pagu MAX_UNDO_TOTAL_BYTES (32 MB)
    let mut stack = UndoStack::default();
    let big_payload = vec![b'A'; 6 * 1024 * 1024]; // 6 MB per snapshot
    for i in 0..8 {
        stack.record(FileSnapshot {
            path: PathBuf::from(format!("file_{i}.txt")),
            rel: format!("file_{i}.txt"),
            before: Some(big_payload.clone()),
            after: b"new".to_vec(),
        });
    }
    assert!(
        stack.total_bytes() <= MAX_UNDO_TOTAL_BYTES,
        "UndoStack RAM ({}) wajib <= MAX_UNDO_TOTAL_BYTES ({})",
        stack.total_bytes(),
        MAX_UNDO_TOTAL_BYTES
    );

    // 2. trim_gateway_session_memory melepas gambar base64 lama & memadatkan riwayat besar
    let dir = temp_dir("mem-trim");
    let archive_dir = dir.join("archive");
    let mut msgs = vec![Message::text(Role::System, "sys")];
    // Tambahkan 2 pesan lama berisi lampiran gambar 1 MB base64 + 6 pesan teks baru
    for i in 0..2 {
        msgs.push(Message {
            role: Role::User,
            content: vec![
                Block::Text(format!("Foto lama #{i}")),
                Block::Image {
                    media_type: "image/jpeg".into(),
                    data_base64: "X".repeat(1024 * 1024),
                },
            ],
        });
        msgs.push(Message::text(Role::Assistant, format!("Oke foto #{i}")));
    }
    for i in 0..8 {
        msgs.push(Message::text(Role::User, format!("Pesan {i}")));
        msgs.push(Message::text(Role::Assistant, format!("Jawab {i}")));
    }
    let before = estimate_messages_bytes(&msgs);
    assert!(before > 2 * 1024 * 1024);
    let rep = trim_gateway_session_memory_with_limits(&mut msgs, &archive_dir, 80, 2 * 1024 * 1024);
    assert_eq!(rep.images_stripped, 2, "gambar lama wajib dilepas dari RAM");
    assert!(
        rep.bytes_after < 100 * 1024,
        "RAM pesan harus turun drastis sesudah trim: {} -> {}",
        rep.bytes_before,
        rep.bytes_after
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =========================================================================
// PERBAIKAN 2: Pembukuan watch, gagal kelas satu, JSON bersarang, & status segar
// =========================================================================

#[tokio::test]
async fn p2_watch_bookkeeping_first_class_failure_nested_json_and_fresh_check() {
    let dir = temp_dir("p2-status");
    let db_path = dir.join("selen.db");
    let store = Arc::new(Store::open_at(&db_path).unwrap());
    let sandbox = SandboxController::default();

    // 1. Parse JSON bersarang + pages terpisah (completed_pages/total_pages) & gagal kelas satu
    let p_nested = parse_watch_output(
        r#"{"job":{"status":"PROCESSING","current_stage":"REMOVAL","completed_pages":3,"total_pages":22}}"#,
        "",
        "",
    );
    assert_eq!(p_nested.kind, WatchStateKind::Running);
    assert_eq!(p_nested.stage, "REMOVAL");
    assert_eq!(p_nested.progress, "3/22");

    let p_fail = parse_watch_output(
        r#"{"job":{"status":"FAILED","current_stage":"INPAINT","error_message":"PIXEL_OUTSIDE_ALLOWED_MASK on page 7"}}"#,
        "",
        "",
    );
    assert_eq!(p_fail.kind, WatchStateKind::Failed);
    assert!(p_fail.error_text.contains("PIXEL_OUTSIDE_ALLOWED_MASK"));

    // 2. Uji siklikal nyata: S1 -> S2 (notifikasi maju) -> S2 lagi (diam, tapi last_checked_at tetap maju!) -> FAILED (lapor otomatis!)
    let status_file = dir.join("job_fail.json");
    std::fs::write(
        &status_file,
        r#"{"job":{"status":"PROCESSING","current_stage":"S1","completed_pages":1,"total_pages":3}}"#,
    )
    .unwrap();
    let ctx = Ctx::new(dir.clone(), true).with_store(store.clone(), "sess-p2".into());
    let tool = WatchJob;
    let reg = tool
        .run(
            json!({
                "label": "Comic-auto #6c33",
                "status_file": "job_fail.json",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(reg.contains("Pemantauan #1 terdaftar"));

    let t1 = Utc::now();
    let w = store.list_job_watches(None, true).unwrap().remove(0);
    let (tr1, ts1) = evaluate_and_poll_watch(&w, &sandbox, t1).await;
    persist_watch_transition(&store, w.id, &tr1, &ts1, true).unwrap();
    assert!(tr1.notification.as_deref().unwrap().contains("S1"));

    // Poll 2: tahap maju ke S2
    std::fs::write(
        &status_file,
        r#"{"job":{"status":"PROCESSING","current_stage":"S2","completed_pages":2,"total_pages":3}}"#,
    )
    .unwrap();
    let t2 = t1 + ChronoDuration::seconds(35);
    let w2 = store.list_job_watches(None, true).unwrap().remove(0);
    let (tr2, ts2) = evaluate_and_poll_watch(&w2, &sandbox, t2).await;
    persist_watch_transition(&store, w2.id, &tr2, &ts2, true).unwrap();
    assert!(tr2.notification.as_deref().unwrap().contains("S2"));
    assert!(tr2.notification.as_deref().unwrap().contains("2/3"));

    // Poll 3: tahap tetap S2 -> 0 pesan (anti-spam), NAMUN last_checked_at tetap diperbarui di SQLite!
    let t3 = t2 + ChronoDuration::seconds(35);
    let w3 = store.list_job_watches(None, true).unwrap().remove(0);
    let (tr3, ts3) = evaluate_and_poll_watch(&w3, &sandbox, t3).await;
    persist_watch_transition(&store, w3.id, &tr3, &ts3, true).unwrap();
    assert!(tr3.notification.is_none());
    let w3_after = store.list_job_watches(None, true).unwrap().remove(0);
    assert_eq!(w3_after.last_stage, "S2");
    assert_eq!(w3_after.last_progress, "2/3");
    assert_ne!(w3_after.last_checked_at, ts2, "last_checked_at wajib selalu maju tiap poll");
    assert_eq!(w3_after.last_checked_at, ts3);

    // Poll 4: job gagal -> laporan kegagalan kelas satu memuat teks error persis, lalu berhenti!
    std::fs::write(
        &status_file,
        r#"{"job":{"status":"FAILED","current_stage":"INPAINT","error_message":"PIXEL_OUTSIDE_ALLOWED_MASK on page 7"}}"#,
    )
    .unwrap();
    let cli_reports = poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await;
    assert_eq!(cli_reports.len(), 1);
    assert!(cli_reports[0].contains("gagal"));
    assert!(cli_reports[0].contains("PIXEL_OUTSIDE_ALLOWED_MASK"));
    // Sesi berikutnya sunyi (tidak dobel)
    assert!(poll_active_watches_for_cli(&store, &sandbox, &dir, true, true).await.is_empty());

    // 3. `watch_job(action="check")`: pemeriksaan SEGAR bertanggal & jujur saat sumber gagal
    let fresh_file = dir.join("job_retry.json");
    std::fs::write(
        &fresh_file,
        r#"{"status":"PROCESSING","stage":"OCR","progress":"5/10"}"#,
    )
    .unwrap();
    let reg2 = tool
        .run(
            json!({
                "label": "Comic-auto Retry #6c33",
                "status_file": "job_retry.json",
                "interval_secs": 30
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(reg2.contains("Pemantauan #2 terdaftar"));

    // Job selesai di luar pengetahuan sesi!
    std::fs::write(
        &fresh_file,
        r#"{"status":"COMPLETED","summary":"10/10 halaman selesai","output":"out/chapter.zip"}"#,
    )
    .unwrap();
    let check_out = tool
        .run(json!({"action": "check", "watch_id": 2}), &ctx)
        .await
        .unwrap();
    assert!(check_out.contains("Status segar #2"));
    assert!(check_out.contains("UTC"));
    assert!(check_out.contains("SELESAI"));
    assert!(check_out.contains("out/chapter.zip"));

    // Simulasi perintah/berkas status rusak (dihapus) -> wajib mengaku jujur + tampilkan catatan bertanggal!
    std::fs::remove_file(&fresh_file).unwrap();
    let err_out = tool
        .run(json!({"action": "check", "watch_id": 2}), &ctx)
        .await
        .unwrap();
    assert!(err_out.contains("Belum bisa mengecek sumber kebenaran saat ini"));
    assert!(err_out.contains("Catatan pantauan terakhir"));
    assert!(err_out.contains("UTC"));

    // `action="list"` menampilkan cap waktu
    let list_out = tool.run(json!({"action": "list"}), &ctx).await.unwrap();
    assert!(list_out.contains("terakhir dicek:"));
    assert!(list_out.contains("UTC"));

    // 4. Aturan status segar & watch pada retry hadir di system prompt + keyword trigger aktif tepat sasaran
    let sys = build_system_prompt(&dir, &[]);
    assert!(sys.contains("ATURAN STATUS SEGAR"));
    assert!(sys.contains("watch_job(action=\"check\")"));
    assert!(sys.contains("belum bisa kucek sekarang"));
    assert!(sys.contains("WAJIB daftarkan `watch_job` baru untuk percobaan baru"));

    let ts_ask = select_initial_toolsets("Udah belum job nya?", &dir, false, TurnMode::Execute);
    assert!(ts_ask.contains("watch"), "pertanyaan status job wajib mengaktifkan toolset watch");
    let ts_trivial = select_initial_toolsets("balas ok saja", &dir, false, TurnMode::Execute);
    assert!(!ts_trivial.contains("watch"), "tugas trivial tidak boleh menyalakan watch");

    // 5. Pastikan 0 panggilan provider dipakai oleh seluruh polling & action="check"
    let usage = store
        .usage_report("sess-p2", &Utc::now().format("%Y-%m-%d").to_string())
        .unwrap();
    assert!(usage.session_rows.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
