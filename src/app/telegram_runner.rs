use super::cli::Cli;
use super::session::Session;
use anyhow::Result;
use selencode::agent::context::sanitize_interrupted_messages;
use selencode::agent::{run_turn_counted, run_turn_ui, UiEvent};
use selencode::config;
use selencode::gateway_health::{
    format_recovery_notice, spawn_os_watchdog, trim_gateway_session_memory, GatewayHealthClock,
    StallTracker, GATEWAY_SESSION_IDLE_SECS, MAX_CACHED_GATEWAY_SESSIONS,
};
use selencode::providers::{Message, Role};
use selencode::repl::{
    help_menu_markup, parse_model_action, parse_slash_command, ModelAction, SlashCommand,
};
use selencode::security::{ApprovalMode, ApprovalRequest};
use selencode::telegram::{
    escape_telegram_html, format_telegram_progress_html, markdown_to_telegram_html,
    parse_callback_review, TelegramClient, TgApprovalHub, TgAskHub, TELEGRAM_BOT_COMMANDS,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

enum IncomingTgPayload {
    Text(String),
    Photo {
        file_id: String,
        message_id: i64,
        caption: String,
        cwd: PathBuf,
    },
}
// ---------- Telegram Bot Gateway (`selen telegram`) ----------

/// Bersihkan akhiran `@nama_bot` pada perintah grup Telegram (mis. `/model@selen_bot 2` -> `/model 2`).
fn normalize_tg_command(raw: &str) -> String {
    let t = raw.trim();
    if !t.starts_with('/') {
        return t.to_string();
    }
    let mut parts = t.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let cmd = head.split('@').next().unwrap_or(head);
    match parts.next() {
        Some(rest) if !rest.trim().is_empty() => format!("{cmd} {}", rest.trim()),
        _ => cmd.to_string(),
    }
}

pub(crate) async fn run_telegram_gateway(cli: Cli) -> Result<()> {
    let cfg = config::load()?;
    let token = cfg.telegram.resolve_token()?;
    let tg = TelegramClient::new(&token)?;
    let t_conn = Instant::now();
    let me = tg.get_me().await?;
    let conn_ms = t_conn.elapsed().as_millis();
    let bot_handle = me
        .username
        .clone()
        .map(|u| format!("@{u}"))
        .unwrap_or_else(|| me.first_name.clone());
    let cwd = std::env::current_dir()?;
    let (prov_name, pcfg, _) = cfg.resolve_provider(cli.provider.as_deref())?;
    let tg_mode_str = if cli.yes {
        "auto".to_string()
    } else if !cfg.telegram.approval.trim().is_empty() {
        cfg.telegram.approval.clone()
    } else {
        cfg.agent.approval.clone()
    };

    println!("☾ SelenCode Telegram Gateway aktif!");
    println!("  Bot      : {bot_handle} (id: {}) — terhubung ({conn_ms} ms)", me.id);
    println!("  Provider : {prov_name} / {}", pcfg.model);
    println!("  Config   : {}", config::active_config_path().display());
    println!("  Folder   : {}", cwd.display());
    println!("  Approval : {tg_mode_str}");
    if cfg.telegram.allowed_users.is_empty() {
        println!(
            "  Whitelist: (KOSONG!) Kirim `/id` ke {bot_handle} di Telegram, lalu jalankan `/telegram allow <ID>` di SelenCode."
        );
    } else {
        let ids: Vec<String> = cfg
            .telegram
            .allowed_users
            .iter()
            .map(|u| u.to_string())
            .collect();
        println!("  Whitelist: {}", ids.join(", "));
    }
    println!("  Tekan Ctrl+C untuk menghentikan gateway.\n");

    // Bersihkan webhook lama (bila token pernah dipakai app/bot lain) agar long-polling langsung jalan!
    if let Err(e) = tg.delete_webhook(false).await {
        eprintln!("  [peringatan] gagal membersihkan webhook lama: {e}");
    }

    // Sinkronkan seluruh daftar perintah `/` ke tombol menu Telegram (`setMyCommands`) secara otomatis.
    match tg.sync_bot_commands().await {
        Ok(()) => println!(
            "  Menu `/` : {} perintah berhasil disinkronkan ke Telegram.",
            TELEGRAM_BOT_COMMANDS.len()
        ),
        Err(e) => eprintln!("  [peringatan] gagal sinkronkan menu `/` Telegram: {e}"),
    }

    let hub = TgApprovalHub::new();
    hub.set_timeout_mins(cfg.telegram.approval_timeout_mins);
    let ask_hub = TgAskHub::new();

    // BAGIAN 3 (v0.5.6) & Perbaikan 1 (v0.5.10): Store bersama untuk Cron, Job Watch, & Watchdog Gateway.
    let gw_store: Option<Arc<selencode::store::Store>> =
        selencode::store::Store::open().ok().map(Arc::new);
    if let Some(cron_store_arc) = gw_store.clone() {
        let skipped = selencode::cron::skip_missed_jobs_on_startup(&cron_store_arc).unwrap_or(0);
        if skipped > 0 {
            println!("  Cron     : {skipped} jadwal terlewat saat gateway mati ditandai terlewat (skip-missed).");
        }
        let cron_tg = tg.clone();
        let cron_prov_override = cli.provider.clone();
        // Perbaikan 3 (v0.5.7): Loop pemantau tugas asinkron (`job_watches`) persisten di gateway.
        // Otomatis melanjutkan semua pantauan aktif sesudah gateway restart — 0 panggilan LLM!
        let watch_tg = tg.clone();
        let watch_store = cron_store_arc.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Ok(watches) = watch_store.list_job_watches(None, true) else {
                    continue;
                };
                if watches.is_empty() {
                    continue;
                }
                let sandbox = match config::load() {
                    Ok(c) => selencode::sandbox::SandboxController::from_config(
                        &c.sandbox,
                        selencode::sandbox::default_audit_log_path(),
                    ),
                    Err(_) => selencode::sandbox::SandboxController::default(),
                };
                let fallback_chat = config::load()
                    .ok()
                    .and_then(|c| c.telegram.allowed_users.first().copied())
                    .unwrap_or(0);
                let now_utc = chrono::Utc::now();
                for w in watches {
                    if !selencode::watch::is_watch_due(&w, now_utc) {
                        continue;
                    }
                    let (tr, checked_at) =
                        selencode::watch::evaluate_and_poll_watch(&w, &sandbox, chrono::Utc::now())
                            .await;
                    let target_chat = if w.chat_id != 0 {
                        w.chat_id
                    } else {
                        fallback_chat
                    };
                    let mut sent_ok = false;
                    if let Some(msg) = &tr.notification {
                        if target_chat != 0 {
                            sent_ok = watch_tg
                                .send_long_message(target_chat, msg)
                                .await
                                .is_ok();
                        }
                    }
                    let _ = selencode::watch::persist_watch_transition(
                        &watch_store,
                        w.id,
                        &tr,
                        &checked_at,
                        sent_ok,
                    );
                }
            }
        });
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Ok(cur_cfg) = config::load() else {
                    continue;
                };
                let Ok(jobs) = cron_store_arc.list_cron_jobs() else {
                    continue;
                };
                let now_utc = chrono::Utc::now();
                // Antre serial: jalankan satu per satu, tidak pernah 2 job bersamaan.
                for job in jobs {
                    if !job.enabled {
                        continue;
                    }
                    let is_due = chrono::DateTime::parse_from_rfc3339(&job.next_run_at)
                        .map(|dt| dt.with_timezone(&chrono::Utc) <= now_utc)
                        .unwrap_or(false);
                    if !is_due {
                        continue;
                    }
                    let Ok((pname, pcfg, key)) =
                        cur_cfg.resolve_provider(cron_prov_override.as_deref())
                    else {
                        continue;
                    };
                    let tel = Arc::new(std::sync::Mutex::new(
                        selencode::providers::router::RouterTelemetry::default(),
                    ));
                    let Ok(prov) =
                        super::make_routed_provider(&cur_cfg, &pname, &pcfg, &key, tel)
                    else {
                        continue;
                    };
                    let sandbox = Arc::new(selencode::sandbox::SandboxController::from_config(
                        &cur_cfg.sandbox,
                        selencode::sandbox::default_audit_log_path(),
                    ));
                    let run_started = chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let next_slot = selencode::cron::compute_next_run_after(
                        &job.schedule_spec,
                        chrono::Utc::now(),
                    )
                    .unwrap_or_else(|_| run_started.clone());
                    let _ = cron_store_arc.update_cron_job_run(
                        job.id,
                        &run_started,
                        &next_slot,
                        "berjalan",
                        "Sedang dieksekusi...",
                    );
                    let (ok, summary) = selencode::cron::run_cron_job_once(
                        prov,
                        cron_store_arc.clone(),
                        sandbox,
                        &job,
                        &cur_cfg.shell,
                        cur_cfg.agent.max_steps,
                    )
                    .await;
                    let status_lbl = if ok { "sukses" } else { "gagal" };
                    let next_after = selencode::cron::compute_next_run_after(
                        &job.schedule_spec,
                        chrono::Utc::now(),
                    )
                    .unwrap_or(next_slot);
                    let _ = cron_store_arc.update_cron_job_run(
                        job.id,
                        &run_started,
                        &next_after,
                        status_lbl,
                        &summary,
                    );
                    let target_chat = if job.chat_id != 0 {
                        job.chat_id
                    } else {
                        cur_cfg.telegram.allowed_users.first().copied().unwrap_or(0)
                    };
                    if target_chat != 0 {
                        let report = selencode::cron::format_cron_report_message(
                            &job.name,
                            ok,
                            &summary,
                        );
                        let _ = cron_tg.send_long_message(target_chat, &report).await;
                    }
                }
            }
        });
    }
    let sessions: Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let session_last_used: Arc<std::sync::Mutex<HashMap<i64, Instant>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let busy_chats: Arc<std::sync::Mutex<HashSet<i64>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    let cancel_tokens: Arc<std::sync::Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let chat_gens: Arc<std::sync::Mutex<HashMap<i64, Arc<AtomicU64>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    // Pelacak message_id gelembung progres aktif per chat agar task approval dapat menjedanya.
    let active_progress: Arc<std::sync::Mutex<HashMap<i64, i64>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let provider_override = cli.provider.clone();
    let yes_flag = cli.yes;

    // Perbaikan 1 (#4 v0.5.10): Pengawas kelangsungan eksternal di thread OS terpisah (di luar Tokio)
    // + catatan pemulihan sekali pakai sesudah restart watchdog.
    let initial_chat = cfg.telegram.allowed_users.first().copied().unwrap_or(0);
    let health_clock = Arc::new(GatewayHealthClock::new(initial_chat));
    if cfg.telegram.poll_watchdog_secs > 0 {
        if let Some(wd_store) = gw_store.clone() {
            println!(
                "  Watchdog : aktif (ambang polling/loop {} dtk, turn timeout {} dtk, stall notice {} dtk)",
                cfg.telegram.poll_watchdog_secs,
                cfg.telegram.turn_timeout_secs,
                cfg.telegram.stall_notice_secs
            );
            let _wd_handle = spawn_os_watchdog(
                health_clock.clone(),
                wd_store,
                cfg.telegram.poll_watchdog_secs,
                Duration::from_secs(2),
                |_verdict| std::process::exit(2),
            );
        }
    }

    let pending_recovery = gw_store
        .as_ref()
        .and_then(|st| st.take_gateway_recovery().ok().flatten());
    let mut offset: i64 = if let Some((rec_chat, reason, crashed_at)) = pending_recovery {
        let saved_offset = gw_store
            .as_ref()
            .and_then(|st| st.load_gateway_offset().ok().flatten())
            .unwrap_or(0);
        println!(
            "  [pemulihan] Gateway pulih dari restart watchdog ({reason}) — melanjutkan offset {saved_offset} tanpa membuang pesan tertahan.\n"
        );
        let notify_chat = if rec_chat != 0 { rec_chat } else { initial_chat };
        if notify_chat != 0 {
            let notice = format_recovery_notice(&reason, &crashed_at);
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                tg.send_message(notify_chat, &notice),
            )
            .await;
        }
        saved_offset
    } else {
        // Startup normal: lewati antrean pesan lama sebelum gateway dinyalakan agar tidak mengeksekusi perintah usang.
        let off = tg.skip_backlog().await;
        if off > 0 {
            if let Some(st) = &gw_store {
                let _ = st.save_gateway_offset(off);
            }
            println!("  (Antrean pesan lama sebelum startup dilewati; siap menerima pesan baru.)\n");
        }
        off
    };

    let mut poll_err_streak: u32 = 0;
    let mut last_session_gc = Instant::now();
    loop {
        health_clock.mark_loop_heartbeat();

        // Pembersihan berkala sesi chat idle dari RAM (Perbaikan 1 #5 v0.5.10 — anti-kebocoran memori).
        if last_session_gc.elapsed() >= Duration::from_secs(300) {
            last_session_gc = Instant::now();
            evict_idle_gateway_sessions(&sessions, &session_last_used, &busy_chats).await;
        }

        let updates = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                health_clock.request_shutdown();
                println!("\n☾ Telegram Gateway dihentikan.");
                break;
            }
            res = tg.get_updates(offset, 20) => match res {
                Ok(u) => {
                    health_clock.mark_poll_ok();
                    if poll_err_streak > 0 {
                        println!("[telegram] koneksi polling Telegram pulih.");
                        poll_err_streak = 0;
                    }
                    u
                }
                Err(e) => {
                    poll_err_streak = poll_err_streak.saturating_add(1);
                    let err_s = e.to_string();
                    let wait_secs = match poll_err_streak {
                        1 => 2,
                        2 => 4,
                        3 => 8,
                        _ => 10,
                    };
                    if err_s.contains("409") || err_s.contains("Conflict") {
                        eprintln!(
                            "[telegram] 409 Conflict: bot token ini sedang dipakai instance `selen telegram` lain atau webhook aktif! Hentikan instance lain (coba lagi {wait_secs}s)."
                        );
                    } else {
                        eprintln!("[telegram] gagal polling: {err_s} (coba lagi {wait_secs} detik)");
                    }
                    tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                }
            }
        };

        // Muat ulang config terbaru agar perubahan `/telegram allow`, `/model`, & `/provider` langsung berlaku.
        let latest_cfg = config::load().unwrap_or_else(|_| cfg.clone());
        hub.set_timeout_mins(latest_cfg.telegram.approval_timeout_mins);

        let offset_before_batch = offset;
        for upd in updates {
            offset = upd.update_id + 1;
            health_clock.mark_loop_heartbeat();

            if let Some(cb) = upd.callback_query {
                let chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
                if latest_cfg.telegram.is_allowed(cb.from.id, chat_id) {
                    health_clock.set_last_active_chat(chat_id);
                    if ask_hub.handle_callback(&tg, &cb).await {
                        continue;
                    }
                    if let Some((prop_id, approve)) = cb.data.as_deref().and_then(parse_callback_review) {
                        let res_msg = match selencode::store::Store::open() {
                            Ok(st) => {
                                if approve {
                                    selencode::agent::apply_staged_proposal(&st, &cwd, prop_id)
                                } else {
                                    selencode::agent::discard_staged_proposal(&st, prop_id)
                                }
                            }
                            Err(e) => Err(e),
                        };
                        match res_msg {
                            Ok(done_txt) => {
                                let toast = if approve { "Usulan disetujui" } else { "Usulan dibuang" };
                                let _ = tg.answer_callback_query(&cb.id, Some(toast)).await;
                                let _ = tg.send_message(chat_id, &done_txt).await;
                            }
                            Err(e) => {
                                let _ = tg
                                    .answer_callback_query(&cb.id, Some("Sudah diproses atau tidak ditemukan"))
                                    .await;
                                let _ = tg.send_message(chat_id, &format!("Review #{prop_id}: {e}")).await;
                            }
                        }
                    } else {
                        let _ = hub.handle_callback(&tg, &cb).await;
                    }
                } else {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some("Akses ditolak (tidak di whitelist)."))
                        .await;
                }
                continue;
            }

            let Some(msg) = upd.message else {
                continue;
            };
            let chat_id = msg.chat.id;
            let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(chat_id);
            let sender_name = msg
                .from
                .as_ref()
                .and_then(|u| u.username.clone().or_else(|| Some(u.first_name.clone())))
                .unwrap_or_else(|| user_id.to_string());

            // Tentukan payload ringan (unduh foto dijalankan di dalam task chat agar loop polling tidak macet!).
            let (payload, display_text) = if let Some(raw_text) = msg.text.clone() {
                let norm = normalize_tg_command(&raw_text);
                if norm.is_empty() {
                    continue;
                }
                (IncomingTgPayload::Text(norm.clone()), norm)
            } else if !msg.photo.is_empty() {
                if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                    let _ = tg
                        .send_message(
                            chat_id,
                            &format!(
                                "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode."
                            ),
                        )
                        .await;
                    continue;
                }
                let best_photo = msg.photo.last().unwrap();
                let caption = msg
                    .caption
                    .as_deref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Analisis gambar ini dan jelaskan hal penting atau error yang terlihat.")
                    .to_string();
                let label = format!("[foto] {caption}");
                (
                    IncomingTgPayload::Photo {
                        file_id: best_photo.file_id.clone(),
                        message_id: msg.message_id,
                        caption,
                        cwd: cwd.clone(),
                    },
                    label,
                )
            } else {
                continue;
            };

            // `/id` selalu dijawab agar pemilik bisa langsung tahu ID untuk whitelist.
            if display_text.eq_ignore_ascii_case("/id") {
                let allowed_note = if latest_cfg.telegram.is_allowed(user_id, chat_id) {
                    "Status: sudah ada di whitelist."
                } else {
                    "Status: belum di whitelist. Jalankan di SelenCode:\n/telegram allow <USER_ID>"
                };
                let _ = tg
                    .send_message(
                        chat_id,
                        &format!(
                            "Info ID Telegram:\nUser ID: {user_id}\nChat ID: {chat_id}\n{allowed_note}"
                        ),
                    )
                    .await;
                continue;
            }

            if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                println!("[telegram] ditolak dari {sender_name} (user={user_id}, chat={chat_id})");
                let _ = tg
                    .send_message(
                        chat_id,
                        &format!(
                            "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode.\n\
                             Tambahkan dari terminal SelenCode:\n\
                             /telegram allow {user_id}"
                        ),
                    )
                    .await;
                continue;
            }

            health_clock.set_last_active_chat(chat_id);
            if let Ok(mut used) = session_last_used.lock() {
                used.insert(chat_id, Instant::now());
            }

            // `/cancel` selalu diproses walau chat sedang sibuk — batalkan giliran, approval, & ask_user tertunda!
            if display_text.eq_ignore_ascii_case("/cancel")
                || display_text.eq_ignore_ascii_case("/stop")
            {
                let cancelled_approvals = hub.cancel_chat_approvals(chat_id);
                let _ = ask_hub.cancel_chat_asks(chat_id);
                let notify_opt = cancel_tokens
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&chat_id).cloned());
                if let Some(n) = notify_opt {
                    n.notify_waiters();
                }
                let msg = if cancelled_approvals > 0 {
                    format!("Dibatalkan ({cancelled_approvals} persetujuan tertunda ditolak).")
                } else {
                    "Sinyal pembatalan dikirim ke giliran aktif.".to_string()
                };
                let _ = tg.send_message(chat_id, &msg).await;
                continue;
            }

            // BAGIAN 2 (v0.5.6): Jika ada pertanyaan klarifikasi `ask_user` tertunda di chat ini
            // dan user mengetik balasan bebas (bukan slash command), arahkan langsung sebagai jawaban `ask_user`!
            if !display_text.starts_with('/') && ask_hub.has_pending_for_chat(chat_id) {
                if ask_hub
                    .try_consume_text_reply(&tg, chat_id, &display_text)
                    .await
                {
                    continue;
                }
            }

            let cancel_notify = {
                let mut map = cancel_tokens.lock().expect("cancel_tokens poisoned");
                map.entry(chat_id)
                    .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
                    .clone()
            };
            let gen_counter = {
                let mut map = chat_gens.lock().expect("chat_gens poisoned");
                map.entry(chat_id)
                    .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                    .clone()
            };
            let my_gen = gen_counter.fetch_add(1, Ordering::SeqCst) + 1;

            // Interupsi mulus: bila chat sedang sibuk, langsung batalkan giliran & approval lama lalu jalankan pesan baru.
            let was_busy = {
                let mut set = busy_chats.lock().expect("busy_chats poisoned");
                !set.insert(chat_id)
            };
            if was_busy {
                let dropped = hub.cancel_chat_approvals(chat_id);
                cancel_notify.notify_waiters();
                let notice = if dropped > 0 {
                    format!("Menginterupsi giliran sebelumnya ({dropped} persetujuan dibatalkan) — memproses pesan baru.")
                } else {
                    "Menginterupsi giliran sebelumnya — memproses pesan baru.".to_string()
                };
                let tg_int = tg.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(10),
                        tg_int.send_message(chat_id, &notice),
                    )
                    .await;
                });
            }

            println!("[telegram] {sender_name} ({chat_id}): {display_text}");
            let tg_clone = tg.clone();
            let hub_clone = hub.clone();
            let ask_hub_clone = ask_hub.clone();
            let sessions_clone = sessions.clone();
            let busy_clone = busy_chats.clone();
            let active_prog_clone = active_progress.clone();
            let prov_clone = provider_override.clone();
            let latest_cfg_clone = latest_cfg.clone();

            tokio::spawn(async move {
                let t_start = Instant::now();
                let res = handle_telegram_chat_message(
                    tg_clone.clone(),
                    hub_clone,
                    ask_hub_clone,
                    sessions_clone,
                    busy_clone.clone(),
                    active_prog_clone.clone(),
                    cancel_notify,
                    gen_counter.clone(),
                    my_gen,
                    prov_clone,
                    yes_flag,
                    latest_cfg_clone,
                    chat_id,
                    payload,
                    t_start,
                )
                .await;
                if gen_counter.load(Ordering::SeqCst) == my_gen {
                    if let Ok(mut set) = busy_clone.lock() {
                        set.remove(&chat_id);
                    }
                    if let Ok(mut map) = active_prog_clone.lock() {
                        map.remove(&chat_id);
                    }
                }
                match res {
                    Ok(()) => {
                        println!(
                            "[telegram] selesai ({chat_id}) dalam {:.1}s",
                            t_start.elapsed().as_secs_f64()
                        );
                    }
                    Err(e) => {
                        eprintln!("[telegram] error ({chat_id}): {e}");
                        let _ = tokio::time::timeout(
                            Duration::from_secs(15),
                            tg_clone.send_long_message(chat_id, &format!("Error: {e}")),
                        )
                        .await;
                    }
                }
            });
        }
        if offset != offset_before_batch {
            if let Some(st) = &gw_store {
                let _ = st.save_gateway_offset(offset);
            }
        }
    }
    Ok(())
}

/// Keluarkan sesi chat yang sudah lama idle atau melebihi pagu cache dari RAM gateway
/// (data riwayat penuh tetap persisten di SQLite; sesi aktif/sibuk tidak pernah disentuh).
async fn evict_idle_gateway_sessions(
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    session_last_used: &Arc<std::sync::Mutex<HashMap<i64, Instant>>>,
    busy_chats: &Arc<std::sync::Mutex<HashSet<i64>>>,
) {
    let busy: HashSet<i64> = busy_chats
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let idle_limit = Duration::from_secs(GATEWAY_SESSION_IDLE_SECS);
    let mut map = sessions.lock().await;
    let mut used = match session_last_used.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut to_remove: Vec<i64> = map
        .keys()
        .copied()
        .filter(|cid| {
            !busy.contains(cid)
                && used
                    .get(cid)
                    .map(|t| t.elapsed() >= idle_limit)
                    .unwrap_or(false)
        })
        .collect();

    if map.len().saturating_sub(to_remove.len()) > MAX_CACHED_GATEWAY_SESSIONS {
        let mut candidates: Vec<(i64, Instant)> = map
            .keys()
            .copied()
            .filter(|cid| !busy.contains(cid) && !to_remove.contains(cid))
            .map(|cid| (cid, used.get(&cid).copied().unwrap_or_else(Instant::now)))
            .collect();
        candidates.sort_by_key(|(_, ts)| *ts);
        let excess = map
            .len()
            .saturating_sub(to_remove.len())
            .saturating_sub(MAX_CACHED_GATEWAY_SESSIONS);
        for (cid, _) in candidates.into_iter().take(excess) {
            to_remove.push(cid);
        }
    }

    for cid in to_remove {
        map.remove(&cid);
        used.remove(&cid);
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_telegram_chat_message(
    tg: TelegramClient,
    hub: Arc<TgApprovalHub>,
    ask_hub: Arc<TgAskHub>,
    sessions: Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    busy_chats: Arc<std::sync::Mutex<HashSet<i64>>>,
    active_progress: Arc<std::sync::Mutex<HashMap<i64, i64>>>,
    cancel: Arc<tokio::sync::Notify>,
    gen_counter: Arc<AtomicU64>,
    my_gen: u64,
    provider_override: Option<String>,
    yes_flag: bool,
    latest_cfg: config::Config,
    chat_id: i64,
    payload: IncomingTgPayload,
    t_start: Instant,
) -> Result<()> {
    let is_slash_cmd = match &payload {
        IncomingTgPayload::Text(t) => t.trim().starts_with('/'),
        IncomingTgPayload::Photo { .. } => false,
    };

    // Umpan balik instan (<300ms): indikator mengetik + gelembung status sementara polos tanpa emoji dekoratif.
    let mut progress_msg_id: Option<i64> = None;
    let tg_typing_init = tg.clone();
    tokio::spawn(async move {
        tg_typing_init.send_typing(chat_id).await;
    });
    if !is_slash_cmd {
        let initial_html = match &payload {
            IncomingTgPayload::Photo { .. } => "<i>Mengunduh foto...</i>",
            IncomingTgPayload::Text(_) => "<i>Memproses permintaan...</i>",
        };
        if let Ok(mid) = tg
            .send_html_message(chat_id, initial_html, "Memproses permintaan...")
            .await
        {
            progress_msg_id = Some(mid);
            if let Ok(mut map) = active_progress.lock() {
                map.insert(chat_id, mid);
            }
        }
    }

    // Unduh foto di dalam task chat ini agar loop polling Telegram tidak pernah terblokir.
    let text = match payload {
        IncomingTgPayload::Text(t) => t,
        IncomingTgPayload::Photo {
            file_id,
            message_id,
            caption,
            cwd,
        } => {
            let mut attached_path: Option<String> = None;
            if let Ok(fi) = tg.get_file(&file_id).await {
                if let Some(fp) = fi.file_path {
                    if let Ok(bytes) = tg.download_file_bytes(&fp).await {
                        if bytes.len() <= 6 * 1024 * 1024 {
                            let ext = fp.rsplit('.').next().unwrap_or("jpg");
                            let rel = format!(".selencode/tg_uploads/tg_{message_id}.{ext}");
                            let full = cwd.join(&rel);
                            if let Some(parent) = full.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if std::fs::write(&full, &bytes).is_ok() {
                                attached_path = Some(rel);
                            }
                        }
                    }
                }
            }
            match attached_path {
                Some(rel) => format!("@{rel} {caption}"),
                None => caption,
            }
        }
    };

    let tg_approval_cfg = latest_cfg.telegram.approval.clone();

    // Ambil atau buat Session khusus untuk chat_id ini.
    let session_arc = {
        let mut map = sessions.lock().await;
        if let Some(existing) = map.get(&chat_id) {
            existing.clone()
        } else {
            let (approval_tx, mut approval_rx) =
                tokio::sync::mpsc::channel::<ApprovalRequest>(16);
            let (ask_tx, mut ask_rx) =
                tokio::sync::mpsc::channel::<selencode::tools::ask::AskUserRequest>(8);
            let mut s = match Session::build(
                provider_override.as_deref(),
                yes_flag,
                false,
                Some(&text),
                Some(approval_tx),
                false,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    if let Some(mid) = progress_msg_id {
                        tg.delete_message(chat_id, mid).await;
                    }
                    return Err(e);
                }
            };
            let ask_timeout = s.cfg.ask.timeout_secs;
            s.ctx.asker = Arc::new(selencode::tools::ask::UserAsker::channel(ask_tx));
            s.ctx.ask_timeout_secs = ask_timeout;
            s.ctx = s.ctx.with_telegram_context(chat_id, tg.clone());
            let tg_ask = tg.clone();
            let ask_hub_bg = ask_hub.clone();
            tokio::spawn(async move {
                while let Some(req) = ask_rx.recv().await {
                    ask_hub_bg.dispatch(&tg_ask, chat_id, req).await;
                }
            });
            let policy = s.ctx.approver.policy();
            if !yes_flag && !tg_approval_cfg.trim().is_empty() {
                let mode = ApprovalMode::from_config(&tg_approval_cfg, false);
                if let Ok(mut p) = policy.lock() {
                    p.mode = mode;
                }
            }
            let tg_appr = tg.clone();
            let hub_appr = hub.clone();
            let pol_appr = policy.clone();
            let active_prog_appr = active_progress.clone();
            tokio::spawn(async move {
                while let Some(req) = approval_rx.recv().await {
                    // Perbaikan 1 (#3): jeda gelembung progres saat menunggu keputusan tombol approval!
                    let paused_mid = active_prog_appr
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&chat_id).copied());
                    if let Some(mid) = paused_mid {
                        let _ = tg_appr
                            .edit_html_message(
                                chat_id,
                                mid,
                                "<i>Menunggu persetujuan — lihat pesan di bawah.</i>",
                                "Menunggu persetujuan — lihat pesan di bawah.",
                            )
                            .await;
                    }
                    hub_appr
                        .dispatch(&tg_appr, chat_id, pol_appr.clone(), req)
                        .await;
                }
            });
            let arc = Arc::new(tokio::sync::Mutex::new(s));
            map.insert(chat_id, arc.clone());
            arc
        }
    };

    // Bila sudah ada pesan baru yang menginterupsi sebelum lock giliran ini didapat, lewati.
    if gen_counter.load(Ordering::SeqCst) != my_gen {
        if let Some(mid) = progress_msg_id {
            tg.delete_message(chat_id, mid).await;
        }
        return Ok(());
    }
    let mut s_guard = session_arc.lock().await;
    if gen_counter.load(Ordering::SeqCst) != my_gen {
        if let Some(mid) = progress_msg_id {
            tg.delete_message(chat_id, mid).await;
        }
        return Ok(());
    }
    let s: &mut Session = &mut *s_guard;

    // Sinkronkan otomatis bila user mengganti router AI / model dari terminal TUI!
    if provider_override.is_none() {
        let target_prov = latest_cfg.default_provider.clone();
        let target_model = latest_cfg
            .providers
            .get(&target_prov)
            .map(|p| p.model.clone());
        if !target_prov.is_empty()
            && (target_prov != s.provider_name
                || target_model.as_deref() != Some(s.pcfg.model.as_str()))
        {
            s.cfg = latest_cfg.clone();
            let _ = s.switch_provider(&target_prov);
        }
    }

    // Perintah khusus Telegram (/start, /status).
    if text.eq_ignore_ascii_case("/start") {
        let start_html = format!(
            "<b>Selena — SelenCode v{}</b>\n\
             ──────────────\n\
             • <b>Provider:</b> <code>{}</code> / <code>{}</code>\n\
             • <b>Folder:</b> <code>{}</code>\n\n\
             Kirim pesan coding apa saja langsung di chat ini.\n\n\
             ▸ <b>Perintah Populer</b>\n\
             • /resume — 10 sesi terakhir bernomor &amp; lanjutkan sesi\n\
             • /model — pilih atau kelola model AI\n\
             • /skills — daftar &amp; kelola skills\n\
             • /verify — cek build, test, &amp; lint proyek\n\
             • /worktree — isolasi refactor aman pakai Git Worktree\n\
             • /sandbox · /approve — atur keamanan &amp; izin tool\n\
             • /doctor · /mcp · /route — diagnostik &amp; status sistem\n\
             • /help — semua perintah lengkap",
            env!("CARGO_PKG_VERSION"),
            escape_telegram_html(&s.provider_name),
            escape_telegram_html(&s.pcfg.model),
            escape_telegram_html(&s.ctx.cwd.display().to_string())
        );
        let plain = format!(
            "Selena — SelenCode v{}\nProvider: {} / {}\nFolder: {}\nKetik /help untuk semua perintah.",
            env!("CARGO_PKG_VERSION"),
            s.provider_name,
            s.pcfg.model,
            s.ctx.cwd.display()
        );
        tg.send_html_message(chat_id, &start_html, &plain).await?;
        return Ok(());
    }
    if text.eq_ignore_ascii_case("/status") {
        tg.send_menu_markup(chat_id, &s.session_info_menu()).await?;
        return Ok(());
    }

    match parse_slash_command(&text) {
        SlashCommand::Help => {
            tg.send_menu_markup(chat_id, &help_menu_markup()).await?;
            return Ok(());
        }
        SlashCommand::Model(None) => {
            tg.send_menu_markup(chat_id, &s.models_menu()).await?;
            return Ok(());
        }
        SlashCommand::Model(Some(arg)) => {
            let reply = if let ModelAction::FetchList(filter) = parse_model_action(&arg) {
                s.fetch_remote_models_menu(filter.as_deref()).await?
            } else {
                s.apply_model_arg(&arg)?
            };
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Models(filter) => {
            let reply = s.fetch_remote_models_menu(filter.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Provider(arg) => {
            let reply = s.handle_provider(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Approve(arg) => {
            let reply = s.handle_approve(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Sandbox(arg) => {
            let reply = s.handle_sandbox(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::New => {
            if let Some(job) = s.prepare_bg_review_job(true) {
                spawn_telegram_bg_review(tg.clone(), chat_id, busy_chats.clone(), job);
            }
            let sid = s.reset_session()?;
            let short: String = sid.chars().take(10).collect();
            tg.send_html_message(
                chat_id,
                &format!("<b>Sesi baru dimulai</b> (<code>{short}</code>)."),
                &format!("Sesi baru dimulai ({short})."),
            )
            .await?;
            return Ok(());
        }
        SlashCommand::Compact => {
            let reply = s.compact_now();
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Skills(arg) => {
            let reply = s.handle_skills(arg.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Review(arg) => {
            let reply = s.handle_review(arg.as_deref())?;
            let is_pending_view = arg
                .as_deref()
                .map(|a| {
                    let l = a.trim().to_ascii_lowercase();
                    l.is_empty() || matches!(l.as_str(), "pending" | "list" | "ls")
                })
                .unwrap_or(true);
            if is_pending_view {
                let cwd_str = s.ctx.cwd.to_string_lossy().to_string();
                let pending = s
                    .store
                    .list_review_proposals(Some(&cwd_str), Some("pending"), 5)
                    .unwrap_or_default();
                if !pending.is_empty() {
                    let html = selencode::telegram::menu_markup_to_telegram_html(&reply);
                    let plain = selencode::repl::strip_menu_markup(&reply);
                    let rows: Vec<Vec<(String, String)>> = pending
                        .iter()
                        .map(|p| {
                            vec![
                                (format!("Setujui #{}", p.id), format!("rv:{}:a", p.id)),
                                (format!("Buang #{}", p.id), format!("rv:{}:d", p.id)),
                            ]
                        })
                        .collect();
                    tg.send_html_message_with_keyboard(chat_id, &html, &plain, rows)
                        .await?;
                    return Ok(());
                }
            }
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Skill(arg) => {
            let reply = s.handle_use_skill(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Cron(arg) => {
            let reply = s.handle_cron(arg.as_deref(), Some(chat_id))?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Watch(arg) => {
            let reply = s.handle_watch(arg.as_deref(), Some(chat_id))?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Usage => {
            let reply = s.handle_usage()?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Search(arg) => {
            let reply = s.handle_search(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Learn(arg) => {
            let reply = s.handle_learn_skill(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Memory(arg) => {
            let reply = s.handle_memory(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Undo => {
            let reply = s.handle_undo()?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Redo => {
            let reply = s.handle_redo()?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Checkpoint(arg) => {
            let reply = s.handle_checkpoint(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Rollback(arg) => {
            let reply = s.handle_rollback(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Worktree(arg) => {
            let reply = s.handle_worktree(arg.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Mcp => {
            tg.send_menu_markup(chat_id, &s.handle_mcp_status()).await?;
            return Ok(());
        }
        SlashCommand::Doctor => {
            tg.send_menu_markup(chat_id, &s.handle_doctor()).await?;
            return Ok(());
        }
        SlashCommand::Verify(arg) => {
            let reply = s.handle_verify(arg.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Index(arg) => {
            let reply = s.handle_index(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Jobs(arg) => {
            let reply = s.handle_jobs(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Route(arg) => {
            let reply = s.handle_route(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Lsp(arg) => {
            let reply = s.handle_lsp(arg.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Task(arg) => {
            let (card, resume_prompt) = s.handle_task(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &card).await?;
            if let Some(prompt) = resume_prompt {
                let res = run_turn_counted(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    s.cfg.agent.max_steps,
                )
                .await?;
                let reply_text = if res.0.trim().is_empty() {
                    "(task dilanjutkan tanpa ringkasan teks)".to_string()
                } else {
                    res.0
                };
                tg.send_long_markdown(chat_id, &reply_text).await?;
            }
            return Ok(());
        }
        SlashCommand::Telegram(arg) => {
            let reply = s.handle_telegram(arg.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Sessions => {
            let reply = s.sessions_menu()?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Resume(arg) => {
            match s.handle_resume(arg.as_deref())? {
                super::commands::ResumeOutcome::Menu(card) => {
                    tg.send_menu_markup(chat_id, &card).await?;
                }
                super::commands::ResumeOutcome::Switched(data) => {
                    let short: String = data.session_id.chars().take(10).collect();
                    let mut html = format!(
                        "<b>Melanjutkan Sesi</b>\n\
                         • <b>{}</b> (<code>{short}</code>)\n\
                         <i>{} pesan dimuat · model {}</i>",
                        escape_telegram_html(&data.title),
                        data.total_messages,
                        escape_telegram_html(&data.model)
                    );
                    let mut plain = format!(
                        "Melanjutkan sesi: \"{}\" ({short}) — {} pesan dimuat.",
                        data.title, data.total_messages
                    );
                    if !data.preview_lines.is_empty() {
                        html.push_str("\n\n<blockquote expandable><b>Cuplikan Terakhir</b>");
                        for (role, text) in &data.preview_lines {
                            let tag = if role == "user" { "Kamu" } else { "Selena" };
                            html.push_str(&format!(
                                "\n<b>{tag}:</b> {}",
                                escape_telegram_html(text)
                            ));
                            plain.push_str(&format!("\n- {tag}: {text}"));
                        }
                        html.push_str("</blockquote>");
                    }
                    tg.send_html_message(chat_id, &html, &plain).await?;
                }
            }
            return Ok(());
        }
        SlashCommand::Init => {
            let reply = s.handle_init()?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Export(arg) => {
            let reply = s.handle_export(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::InlineShell(cmd) => {
            let reply = s.handle_inline_shell(&cmd).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Exit | SlashCommand::Details | SlashCommand::Plan | SlashCommand::Setup => {
            tg.send_message(
                chat_id,
                "Perintah tersebut khusus mode terminal. Ketik /help untuk perintah Telegram.",
            )
            .await?;
            return Ok(());
        }
        SlashCommand::Stop => {
            tg.send_message(chat_id, "Tidak ada giliran aktif yang berjalan.")
                .await?;
            return Ok(());
        }
        SlashCommand::Unknown(name) => {
            tg.send_message(
                chat_id,
                &format!("Perintah /{name} tidak dikenal. Ketik /help untuk daftar perintah."),
            )
            .await?;
            return Ok(());
        }
        SlashCommand::NotCommand => {}
    }

    // Giliran agent normal: indikator mengetik tiap 4s + gelembung progres sementara
    // (otomatis jeda saat menunggu persetujuan tombol, dan hasil akhir SELALU dikirim sebagai pesan BARU!).
    let typing_stop = Arc::new(tokio::sync::Notify::new());
    let typing_stop_clone = typing_stop.clone();
    let tg_typing = tg.clone();
    let typing_task = tokio::spawn(async move {
        loop {
            tg_typing.send_typing(chat_id).await;
            tokio::select! {
                _ = typing_stop_clone.notified() => break,
                _ = tokio::time::sleep(Duration::from_secs(4)) => {}
            }
        }
    });

    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel::<UiEvent>(256);
    let tg_prog = tg.clone();
    let hub_for_col = hub.clone();
    let ask_hub_col = ask_hub.clone();
    let max_steps_ui = s.cfg.agent.max_steps;
    let stall_notice_secs = latest_cfg.telegram.stall_notice_secs;
    let turn_timeout_secs = latest_cfg.telegram.turn_timeout_secs;
    let collector = tokio::spawn(async move {
        let mut tool_events: Vec<selencode::tool_display::ToolActionEvent> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut cur_step: usize = 0;
        let mut active_tool_display: Option<String> = None;
        let mut active_tool_preview = String::new();
        let mut live_preview = String::new();
        let mut last_edit = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);
        let mut last_sent_html = String::new();
        let mut stall_tracker =
            StallTracker::new(Duration::from_secs(stall_notice_secs), Instant::now());
        let mut stall_tick = tokio::time::interval(Duration::from_secs(5));
        stall_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                ev_opt = ui_rx.recv() => {
                    let Some(ev) = ev_opt else {
                        break;
                    };
                    let mut changed = false;
                    match ev {
                        UiEvent::Step { current, .. } => {
                            cur_step = current;
                            stall_tracker.record_progress(Instant::now());
                            changed = true;
                        }
                        UiEvent::Token(tok) => {
                            stall_tracker.record_progress(Instant::now());
                            live_preview.push_str(&tok);
                            if live_preview.len() > 2400 {
                                let tail: String = live_preview
                                    .chars()
                                    .rev()
                                    .take(1200)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect();
                                live_preview = tail;
                            }
                            changed = true;
                        }
                        UiEvent::ToolStart { name, args } => {
                            stall_tracker.record_progress(Instant::now());
                            active_tool_preview = args.clone();
                            active_tool_display = Some(
                                selencode::tool_display::format_tool_event_from_preview(&name, &args),
                            );
                            live_preview.clear();
                            changed = true;
                        }
                        UiEvent::ToolDone {
                            name,
                            summary,
                            is_error,
                        } => {
                            stall_tracker.record_progress(Instant::now());
                            active_tool_display = None;
                            let preview = std::mem::take(&mut active_tool_preview);
                            tool_events.push(selencode::tool_display::ToolActionEvent {
                                name,
                                preview,
                                is_error,
                                error_reason: is_error
                                    .then(|| selencode::tool_display::short_error_reason(&summary)),
                            });
                            changed = true;
                        }
                        UiEvent::Info(info) if info.starts_with("Model utama gagal") => {
                            // BAGIAN 1 (#4): Beri tahu pengguna 1x sebagai pesan baru singkat saat fallback aktif.
                            let _ = tokio::time::timeout(
                                Duration::from_secs(10),
                                tg_prog.send_message(chat_id, &info),
                            )
                            .await;
                        }
                        UiEvent::Info(info) if info.contains("Self-improvement") => {
                            notes.push(info);
                        }
                        _ => {}
                    }
                    if changed {
                        // Perbaikan 1 (#3): jangan edit gelembung progres selama persetujuan sedang menunggu user.
                        if hub_for_col.has_pending_for_chat(chat_id) {
                            continue;
                        }
                        if let Some(mid) = progress_msg_id {
                            if last_edit.elapsed() >= Duration::from_millis(1800) {
                                let html = format_telegram_progress_html(
                                    cur_step,
                                    max_steps_ui,
                                    active_tool_display.as_deref(),
                                    tool_events.len(),
                                    &live_preview,
                                );
                                if html != last_sent_html {
                                    let _ = tokio::time::timeout(
                                        Duration::from_secs(10),
                                        tg_prog.edit_html_message(
                                            chat_id,
                                            mid,
                                            &html,
                                            "Sedang bekerja...",
                                        ),
                                    )
                                    .await;
                                    last_sent_html = html;
                                    last_edit = Instant::now();
                                }
                            }
                        }
                    }
                }
                _ = stall_tick.tick() => {
                    let waiting_user = hub_for_col.has_pending_for_chat(chat_id)
                        || ask_hub_col.has_pending_for_chat(chat_id);
                    stall_tracker.set_paused(waiting_user, Instant::now());
                    if let Some(notice) = stall_tracker.check_stall(Instant::now()) {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(10),
                            tg_prog.send_message(chat_id, &notice),
                        )
                        .await;
                    }
                }
            }
        }
        (tool_events, notes)
    });

    enum TurnOutcome {
        Finished(Result<String>),
        Cancelled,
        TimedOut(u64),
    }

    let turn_outcome = tokio::select! {
        r = run_turn_ui(
            s.provider.clone(),
            &s.registry,
            &s.ctx,
            &mut s.messages,
            &text,
            s.cfg.agent.max_steps,
            ui_tx,
        ) => TurnOutcome::Finished(r),
        _ = cancel.notified() => TurnOutcome::Cancelled,
        _ = async {
            if turn_timeout_secs > 0 {
                tokio::time::sleep(Duration::from_secs(turn_timeout_secs)).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => TurnOutcome::TimedOut(turn_timeout_secs),
    };

    typing_stop.notify_waiters();
    let _ = tokio::time::timeout(Duration::from_secs(2), typing_task).await;
    let (tool_events, notes) = tokio::time::timeout(Duration::from_secs(3), collector)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    if let Ok(mut map) = active_progress.lock() {
        map.remove(&chat_id);
    }

    let archive_dir = s.ctx.cwd.join(".selencode").join("tool_archive");

    let answer = match turn_outcome {
        TurnOutcome::Cancelled => {
            if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                let _ = s.store.save_message(&s.session_id, &syn);
            }
            let _ = trim_gateway_session_memory(&mut s.messages, &archive_dir);
            if gen_counter.load(Ordering::SeqCst) == my_gen {
                if let Some(mid) = progress_msg_id {
                    if !tg.delete_message_checked(chat_id, mid).await {
                        let _ = tg.edit_message_text(chat_id, mid, "Giliran dibatalkan.").await;
                    } else {
                        tg.send_message(chat_id, "Giliran dibatalkan.").await?;
                    }
                } else {
                    tg.send_message(chat_id, "Giliran dibatalkan.").await?;
                }
            } else if let Some(mid) = progress_msg_id {
                tg.delete_message(chat_id, mid).await;
            }
            return Ok(());
        }
        TurnOutcome::TimedOut(secs) => {
            let _ = hub.cancel_chat_approvals(chat_id);
            let _ = ask_hub.cancel_chat_asks(chat_id);
            if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                let _ = s.store.save_message(&s.session_id, &syn);
            }
            let reason = format!("batas waktu giliran ({secs} detik) tercapai");
            let summary_md = if !tool_events.is_empty() {
                selencode::tool_display::build_template_closing_summary(&tool_events, &reason)
            } else {
                format!(
                    "Giliran dibatalkan otomatis karena {reason} tanpa jawaban selesai. Gateway tetap aktif — silakan kirim pesan lanjutan atau ulangi permintaan."
                )
            };
            let timeout_msg = Message::text(Role::Assistant, summary_md.clone());
            let _ = s.store.save_message(&s.session_id, &timeout_msg);
            s.messages.push(timeout_msg);
            let _ = trim_gateway_session_memory(&mut s.messages, &archive_dir);

            let (total_tools, summary_lines) = selencode::tool_display::build_action_summary_lines(
                &tool_events,
                selencode::tool_display::DEFAULT_ACTION_BLOCK_MAX_LINES,
            );
            if total_tools > 0 {
                let shown: Vec<String> = summary_lines
                    .into_iter()
                    .map(|t| escape_telegram_html(&t))
                    .collect();
                let tool_block = format!(
                    "<blockquote expandable><b>Aksi Tool ({total_tools})</b>\n{}</blockquote>",
                    shown.join("\n")
                );
                let body_html = markdown_to_telegram_html(&summary_md);
                let combined = format!("{tool_block}\n\n{body_html}");
                if combined.chars().count() <= 4050 {
                    let _ = tg.send_html_message(chat_id, &combined, &summary_md).await;
                } else {
                    let _ = tg
                        .send_html_message(chat_id, &tool_block, "Aksi tool terhenti karena batas waktu.")
                        .await;
                    let _ = tg.send_long_markdown(chat_id, &summary_md).await;
                }
            } else {
                let _ = tg.send_long_markdown(chat_id, &summary_md).await;
            }
            if let Some(mid) = progress_msg_id {
                if !tg.delete_message_checked(chat_id, mid).await {
                    let _ = tg
                        .edit_message_text(chat_id, mid, &format!("Terhenti — batas waktu ({secs} dtk)"))
                        .await;
                }
            }
            eprintln!("[telegram] turn timeout ({chat_id}) setelah {secs}s — dibatalkan bersih");
            return Ok(());
        }
        TurnOutcome::Finished(Ok(a)) => a,
        TurnOutcome::Finished(Err(e)) => {
            if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                let _ = s.store.save_message(&s.session_id, &syn);
            }
            let _ = trim_gateway_session_memory(&mut s.messages, &archive_dir);
            if let Some(mid) = progress_msg_id {
                tg.delete_message(chat_id, mid).await;
            }
            return Err(e);
        }
    };
    let mut answer_md = answer.trim().to_string();
    // Perbaikan 2 (v0.5.8): jaring pengaman gateway — bila jawaban akhir masih kosong sesudah aksi tool,
    // susun ringkasan templat dari daftar aksi sehingga Telegram tidak pernah mengirim blok aksi kosong.
    if answer_md.is_empty() && !tool_events.is_empty() {
        answer_md = selencode::tool_display::build_template_closing_summary(
            &tool_events,
            "giliran berakhir",
        );
    }
    for note in &notes {
        if !answer_md.is_empty() {
            answer_md.push_str("\n\n");
        }
        answer_md.push_str(&format!("> {note}"));
    }

    let (total_tools, summary_lines) = selencode::tool_display::build_action_summary_lines(
        &tool_events,
        selencode::tool_display::DEFAULT_ACTION_BLOCK_MAX_LINES,
    );

    // Perbaikan 1 (#1, #4, #5) & Perbaikan 3 v0.5.8:
    // Jawaban akhir SELALU dikirim sebagai PESAN BARU (`sendMessage`) dengan blok `Aksi Tool (N)`
    // yang menampilkan pratinjau argumen teredaksi, penggabungan pengulangan, & semua baris gagal.
    if total_tools > 0 {
        let shown: Vec<String> = summary_lines
            .into_iter()
            .map(|t| escape_telegram_html(&t))
            .collect();
        let tool_block = format!(
            "<blockquote expandable><b>Aksi Tool ({total_tools})</b>\n{}</blockquote>",
            shown.join("\n")
        );
        let body_html = markdown_to_telegram_html(&answer_md);
        let combined = if body_html.is_empty() {
            tool_block.clone()
        } else {
            format!("{tool_block}\n\n{body_html}")
        };
        if combined.chars().count() <= 4050 {
            tg.send_html_message(chat_id, &combined, &answer_md).await?;
        } else {
            tg.send_html_message(chat_id, &tool_block, "Aksi tool selesai.")
                .await?;
            tg.send_long_markdown(chat_id, &answer_md).await?;
        }
    } else {
        let final_html = markdown_to_telegram_html(&answer_md);
        if !final_html.is_empty() && final_html.chars().count() <= 4050 {
            tg.send_html_message(chat_id, &final_html, &answer_md).await?;
        } else {
            tg.send_long_markdown(chat_id, &answer_md).await?;
        }
    }

    // Perbaikan 1 (#2): sesudah pesan jawaban baru terkirim, hapus gelembung progres sementara —
    // atau bila penghapusan gagal, ubah menjadi 1 baris status singkat TANPA isi jawaban.
    if let Some(mid) = progress_msg_id {
        if !tg.delete_message_checked(chat_id, mid).await {
            let elapsed_s = t_start.elapsed().as_secs();
            let done_line = if total_tools > 0 {
                format!("Selesai — {total_tools} aksi tool · {elapsed_s} dtk")
            } else {
                format!("Selesai — {elapsed_s} dtk")
            };
            let _ = tg.edit_message_text(chat_id, mid, &done_line).await;
        }
    }

    // Perbaikan 1 (#5 v0.5.10): pangkas lampiran gambar lama & batasi retensi RAM sesi gateway.
    let trim = trim_gateway_session_memory(&mut s.messages, &archive_dir);
    if trim.images_stripped > 0 || trim.compacted {
        println!(
            "[telegram] trim memori sesi ({chat_id}): {} -> {} byte (gambar dilepas: {}, padatkan: {})",
            trim.bytes_before, trim.bytes_after, trim.images_stripped, trim.compacted
        );
    }

    // Background Self-Improvement Review (v0.5.1): picu di latar setiap 15 giliran tuntas
    // (tidak pernah memblokir giliran aktif; laporan dikirim sebagai pesan baru Telegram).
    s.turns_since_review += 1;
    if let Some(job) = s.prepare_bg_review_job(false) {
        spawn_telegram_bg_review(tg.clone(), chat_id, busy_chats.clone(), job);
    }
    Ok(())
}

/// Jalankan Background Self-Improvement Review di latar untuk chat Telegram (v0.5.1).
/// Menunggu sampai giliran aktif tidak sibuk, lalu mengirim TEPAT SATU pesan baru bila ada hasil.
fn spawn_telegram_bg_review(
    tg: TelegramClient,
    chat_id: i64,
    busy_chats: Arc<std::sync::Mutex<HashSet<i64>>>,
    job: super::session::BgReviewJob,
) {
    tokio::spawn(async move {
        // Beri jeda singkat agar flag busy giliran selesai dilepas, dan pastikan tidak jalan
        // bersamaan dengan giliran aktif.
        tokio::time::sleep(Duration::from_millis(400)).await;
        if busy_chats
            .lock()
            .map(|set| set.contains(&chat_id))
            .unwrap_or(false)
        {
            return;
        }
        let Ok(Ok(Some(outcome))) = tokio::time::timeout(Duration::from_secs(30), job.run(true)).await
        else {
            return;
        };
        let Some(report) = outcome.report_message else {
            return;
        };
        let html = escape_telegram_html(&report);
        if outcome.staged_ids.is_empty() {
            let _ = tg.send_html_message(chat_id, &html, &report).await;
        } else {
            let rows: Vec<Vec<(String, String)>> = outcome
                .staged_ids
                .iter()
                .take(4)
                .map(|id| {
                    vec![
                        (format!("Setujui #{id}"), format!("rv:{id}:a")),
                        (format!("Buang #{id}"), format!("rv:{id}:d")),
                    ]
                })
                .collect();
            let _ = tg
                .send_html_message_with_keyboard(chat_id, &html, &report, rows)
                .await;
        }
    });
}

