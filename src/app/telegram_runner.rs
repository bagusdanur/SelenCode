use super::cli::Cli;
use super::session::Session;
use super::telegram_ui::{
    self, tg_build_approve_picker, tg_build_cron_picker, tg_build_model_picker_for_provider,
    tg_build_provider_picker, tg_build_resume_picker, tg_build_sandbox_picker,
    tg_build_skills_picker, tg_sorted_providers, TgUiSharedState,
};
use anyhow::Result;
use selencode::agent::context::{format_context_tokens_exact, sanitize_interrupted_messages};
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::{run_turn_counted, run_turn_ui, run_turn_ui_mode, UiEvent};
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
    build_tg_plan_confirm_card, classify_tg_message, escape_telegram_html,
    extract_tg_voice_or_audio, format_telegram_progress_html, format_tg_document_too_large_reply,
    format_tg_document_turn_prompt, format_tg_model_switched_html, format_tg_new_session_html,
    markdown_to_telegram_html, parse_callback_config, parse_callback_cron, parse_callback_model,
    parse_callback_plan, parse_callback_resume, parse_callback_review, parse_callback_skill,
    save_tg_document_bytes, transcribe_audio_bytes, TelegramClient, TgApprovalHub, TgAskHub,
    TgAudioAttachment, TgIngressClassification, TgModelCallback, TgPlanCallback, TypingLoopGuard,
    TELEGRAM_BOT_COMMANDS, TG_DOCUMENT_MAX_BYTES,
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
    Document {
        file_id: String,
        message_id: i64,
        file_name: Option<String>,
        mime_type: Option<String>,
        file_size: Option<u64>,
        caption: String,
        cwd: PathBuf,
    },
    Audio {
        att: TgAudioAttachment,
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
    if let Ok(api_override) = std::env::var(selencode::telegram::TELEGRAM_API_BASE_ENV) {
        let trimmed = api_override.trim();
        if !trimmed.is_empty() {
            println!(
                "  API Base : {trimmed} (override khusus uji via {})",
                selencode::telegram::TELEGRAM_API_BASE_ENV
            );
        }
    }
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
                                .send_long_markdown(target_chat, msg)
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
                        30, // Pengaman internal cron latar agar tidak runaway tanpa pengawasan user
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
                        let _ = cron_tg.send_long_markdown(target_chat, &report).await;
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
    let tg_ui = Arc::new(TgUiSharedState::default());
    // Auto-probe latar kapasitas context window model default bila belum ada di cache SQLite.
    if let Some(st_probe) = gw_store.clone() {
        let cached = st_probe.load_model_context_cache();
        if pcfg.context_limit.filter(|&n| n > 0).is_none() && !cached.contains_key(&pcfg.model) {
            if let Ok((_, pcfg_p, key_p)) = cfg.resolve_provider(cli.provider.as_deref()) {
                tokio::spawn(async move {
                    let res = tokio::time::timeout(Duration::from_secs(8), async move {
                        match pcfg_p.kind.as_str() {
                            "openai" => {
                                let u = pcfg_p.base_url.as_deref()?;
                                selencode::providers::openai::fetch_models_with_context(u, &key_p)
                                    .await
                                    .ok()
                            }
                            "gemini" => {
                                let u = pcfg_p
                                    .base_url
                                    .as_deref()
                                    .unwrap_or(selencode::providers::gemini::DEFAULT_BASE_URL);
                                selencode::providers::gemini::fetch_models_with_context(u, &key_p)
                                    .await
                                    .ok()
                            }
                            "anthropic" => {
                                let u = pcfg_p
                                    .base_url
                                    .as_deref()
                                    .unwrap_or(selencode::providers::anthropic::DEFAULT_BASE_URL);
                                selencode::providers::anthropic::fetch_models_with_context(
                                    u, &key_p,
                                )
                                .await
                                .ok()
                            }
                            _ => None,
                        }
                    })
                    .await;
                    if let Ok(Some(items)) = res {
                        let discovered: Vec<(String, usize)> = items
                            .into_iter()
                            .filter_map(|(id, lim)| lim.map(|n| (id, n)))
                            .collect();
                        if !discovered.is_empty() {
                            let _ = st_probe.upsert_model_context_cache(&discovered);
                        }
                    }
                });
            }
        }
    }
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
    // Anti-flood DoS: maks 1 balasan penolakan per 60 detik per chat_id asing (pagu 512 entri).
    let mut unauth_reply_cooldown: HashMap<i64, Instant> = HashMap::new();
    let should_reply_unauth = |map: &mut HashMap<i64, Instant>, cid: i64| -> bool {
        let now = Instant::now();
        if let Some(prev) = map.get(&cid) {
            if prev.elapsed() < Duration::from_secs(60) {
                return false;
            }
        }
        if map.len() >= 512 {
            map.retain(|_, ts| ts.elapsed() < Duration::from_secs(60));
            if map.len() >= 512 {
                map.clear();
            }
        }
        map.insert(cid, now);
        true
    };
    loop {
        health_clock.mark_loop_heartbeat();

        // Pembersihan berkala sesi chat idle dari RAM (Perbaikan 1 #5 v0.5.10 — anti-kebocoran memori).
        if last_session_gc.elapsed() >= Duration::from_secs(300) {
            last_session_gc = Instant::now();
            evict_idle_gateway_sessions(
                &sessions,
                &session_last_used,
                &busy_chats,
                &cancel_tokens,
                &chat_gens,
                &tg_ui,
            )
            .await;
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
                    if let Some(model_cb) = cb.data.as_deref().and_then(parse_callback_model) {
                        if matches!(model_cb, TgModelCallback::CatalogPage { .. }) {
                            let tg_c = tg.clone();
                            let cb_c = cb.clone();
                            let cfg_c = latest_cfg.clone();
                            let st_c = gw_store.clone();
                            let sess_c = sessions.clone();
                            let ui_c = tg_ui.clone();
                            tokio::spawn(async move {
                                telegram_ui::handle_tg_model_callback(
                                    &tg_c, &cb_c, chat_id, model_cb, &cfg_c, &st_c, &sess_c, &ui_c,
                                )
                                .await;
                            });
                        } else {
                            telegram_ui::handle_tg_model_callback(
                                &tg,
                                &cb,
                                chat_id,
                                model_cb,
                                &latest_cfg,
                                &gw_store,
                                &sessions,
                                &tg_ui,
                            )
                            .await;
                        }
                        continue;
                    }
                    if let Some(res_cb) = cb.data.as_deref().and_then(parse_callback_resume) {
                        telegram_ui::handle_tg_resume_callback(&tg, &cb, chat_id, res_cb, &sessions)
                            .await;
                        continue;
                    }
                    if let Some(cfg_cb) = cb.data.as_deref().and_then(parse_callback_config) {
                        telegram_ui::handle_tg_config_callback(
                            &tg,
                            &cb,
                            chat_id,
                            cfg_cb,
                            &latest_cfg,
                            &sessions,
                        )
                        .await;
                        continue;
                    }
                    if let Some(crn_cb) = cb.data.as_deref().and_then(parse_callback_cron) {
                        telegram_ui::handle_tg_cron_callback(&tg, &cb, chat_id, crn_cb, &gw_store)
                            .await;
                        continue;
                    }
                    if let Some(skl_cb) = cb.data.as_deref().and_then(parse_callback_skill) {
                        let chat_cwd = resolve_chat_cwd(&sessions, chat_id, &cwd).await;
                        telegram_ui::handle_tg_skill_callback(
                            &tg, &cb, chat_id, skl_cb, &chat_cwd, &tg_ui,
                        )
                        .await;
                        continue;
                    }
                    if let Some(pln_cb) = cb.data.as_deref().and_then(parse_callback_plan) {
                        let mid_opt = cb.message.as_ref().map(|m| m.message_id);
                        match pln_cb {
                            TgPlanCallback::Cancel => {
                                let _ = tg_ui.take_pending_plan(chat_id);
                                let _ = tg
                                    .answer_callback_query(&cb.id, Some("Rencana dibatalkan"))
                                    .await;
                                if let Some(mid) = mid_opt {
                                    let _ = tg
                                        .edit_html_message_with_keyboard(
                                            chat_id,
                                            mid,
                                            "<i>Rencana dibatalkan.</i>",
                                            "Rencana dibatalkan.",
                                            Vec::new(),
                                        )
                                        .await;
                                }
                            }
                            TgPlanCallback::Execute => {
                                if let Some(plan) = tg_ui.take_pending_plan(chat_id) {
                                    let _ = tg
                                        .answer_callback_query(&cb.id, Some("Menjalankan rencana..."))
                                        .await;
                                    if let Some(mid) = mid_opt {
                                        let _ = tg
                                            .edit_html_message_with_keyboard(
                                                chat_id,
                                                mid,
                                                "✅ <b>Rencana disetujui — mulai mengeksekusi...</b>",
                                                "Rencana disetujui — mulai mengeksekusi...",
                                                Vec::new(),
                                            )
                                            .await;
                                    }
                                    let exec_prompt = approved_plan_prompt(&plan);
                                    let gen_counter = {
                                        let mut m = chat_gens.lock().expect("chat_gens poisoned");
                                        m.entry(chat_id)
                                            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                                            .clone()
                                    };
                                    let my_gen = gen_counter.fetch_add(1, Ordering::SeqCst) + 1;
                                    let cancel_notify = {
                                        let mut m =
                                            cancel_tokens.lock().expect("cancel_tokens poisoned");
                                        m.entry(chat_id)
                                            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
                                            .clone()
                                    };
                                    let was_busy = busy_chats
                                        .lock()
                                        .map(|mut b| !b.insert(chat_id))
                                        .unwrap_or(false);
                                    if was_busy {
                                        let _ = hub.cancel_chat_approvals(chat_id);
                                        cancel_notify.notify_waiters();
                                    }
                                    let tg_c = tg.clone();
                                    let hub_c = hub.clone();
                                    let ask_c = ask_hub.clone();
                                    let sess_c = sessions.clone();
                                    let busy_c = busy_chats.clone();
                                    let prog_c = active_progress.clone();
                                    let ui_c = tg_ui.clone();
                                    let prov_c = provider_override.clone();
                                    let cfg_c = latest_cfg.clone();
                                    tokio::spawn(async move {
                                        let t_start = Instant::now();
                                        let res = handle_telegram_chat_message(
                                            tg_c.clone(),
                                            hub_c,
                                            ask_c,
                                            sess_c,
                                            busy_c.clone(),
                                            prog_c.clone(),
                                            ui_c,
                                            cancel_notify,
                                            gen_counter.clone(),
                                            my_gen,
                                            prov_c,
                                            yes_flag,
                                            cfg_c,
                                            chat_id,
                                            IncomingTgPayload::Text(exec_prompt),
                                            t_start,
                                        )
                                        .await;
                                        if gen_counter.load(Ordering::SeqCst) == my_gen {
                                            if let Ok(mut set) = busy_c.lock() {
                                                set.remove(&chat_id);
                                            }
                                            if let Ok(mut map) = prog_c.lock() {
                                                map.remove(&chat_id);
                                            }
                                        }
                                        if let Err(e) = res {
                                            let safe = tg_c.sanitize_error(&e.to_string());
                                            let _ = tg_c
                                                .send_long_markdown(chat_id, &format!("Error: {safe}"))
                                                .await;
                                        }
                                    });
                                } else {
                                    let _ = tg
                                        .answer_callback_query(
                                            &cb.id,
                                            Some("Tidak ada rencana tertunda."),
                                        )
                                        .await;
                                }
                            }
                        }
                        continue;
                    }
                    if let Some((prop_id, approve)) = cb.data.as_deref().and_then(parse_callback_review) {
                        let chat_cwd = resolve_chat_cwd(&sessions, chat_id, &cwd).await;
                        let cwd_str = chat_cwd.to_string_lossy().to_string();
                        let res_msg = match selencode::store::Store::open() {
                            Ok(st) => {
                                // Anti-IDOR: pastikan proposal milik workspace aktif chat ini.
                                let owns_prop = st
                                    .list_review_proposals(Some(&cwd_str), Some("pending"), 50)
                                    .map(|list| list.iter().any(|p| p.id == prop_id))
                                    .unwrap_or(false);
                                if !owns_prop {
                                    Err(anyhow::anyhow!(
                                        "usulan #{prop_id} bukan milik workspace aktif atau sudah diproses"
                                    ))
                                } else if approve {
                                    selencode::agent::apply_staged_proposal(&st, &chat_cwd, prop_id)
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

            // Tentukan payload ringan (unduh foto/dokumen dijalankan di dalam task chat agar loop polling tidak macet!).
            let (payload, display_text) = match classify_tg_message(&msg) {
                TgIngressClassification::Text(raw_text) => {
                    let norm = normalize_tg_command(&raw_text);
                    if norm.is_empty() {
                        continue;
                    }
                    (IncomingTgPayload::Text(norm.clone()), norm)
                }
                TgIngressClassification::Photo {
                    file_id,
                    message_id,
                    caption,
                } => {
                    if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                        if should_reply_unauth(&mut unauth_reply_cooldown, chat_id) {
                            let _ = tg
                                .send_message(
                                    chat_id,
                                    &format!(
                                        "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode."
                                    ),
                                )
                                .await;
                        }
                        continue;
                    }
                    let label = format!("[foto] {caption}");
                    (
                        IncomingTgPayload::Photo {
                            file_id,
                            message_id,
                            caption,
                            cwd: cwd.clone(),
                        },
                        label,
                    )
                }
                TgIngressClassification::Document {
                    file_id,
                    message_id,
                    file_name,
                    mime_type,
                    file_size,
                    caption,
                } => {
                    if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                        if should_reply_unauth(&mut unauth_reply_cooldown, chat_id) {
                            let _ = tg
                                .send_message(
                                    chat_id,
                                    &format!(
                                        "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode."
                                    ),
                                )
                                .await;
                        }
                        continue;
                    }
                    // Pagu 20 MB Bot API: tolak segera sebelum unduh & tanpa giliran agent.
                    if let Some(sz) = file_size.filter(|&sz| sz > TG_DOCUMENT_MAX_BYTES) {
                        let notice =
                            format_tg_document_too_large_reply(file_name.as_deref(), sz);
                        let _ = tg.send_message(chat_id, &notice).await;
                        continue;
                    }
                    let shown = file_name.as_deref().unwrap_or("berkas");
                    let label = format!("[dokumen: {shown}] {caption}");
                    (
                        IncomingTgPayload::Document {
                            file_id,
                            message_id,
                            file_name,
                            mime_type,
                            file_size,
                            caption,
                            cwd: cwd.clone(),
                        },
                        label,
                    )
                }
                TgIngressClassification::Unsupported { reply, .. } => {
                    if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                        if should_reply_unauth(&mut unauth_reply_cooldown, chat_id) {
                            let _ = tg
                                .send_message(
                                    chat_id,
                                    &format!(
                                        "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode."
                                    ),
                                )
                                .await;
                        }
                        continue;
                    }
                    if latest_cfg.telegram.voice_transcription {
                        if let Some(att) = extract_tg_voice_or_audio(&msg) {
                            if let Some(sz) = att.file_size.filter(|&sz| sz > TG_DOCUMENT_MAX_BYTES)
                            {
                                let notice = format_tg_document_too_large_reply(
                                    Some(&att.file_name),
                                    sz,
                                );
                                let _ = tg.send_message(chat_id, &notice).await;
                                continue;
                            }
                            let label = format!(
                                "[{}] {}",
                                if att.is_voice { "suara" } else { "audio" },
                                if att.caption.is_empty() {
                                    att.file_name.as_str()
                                } else {
                                    att.caption.as_str()
                                }
                            );
                            (IncomingTgPayload::Audio { att }, label)
                        } else {
                            let _ = tg.send_message(chat_id, &reply).await;
                            continue;
                        }
                    } else {
                        let _ = tg.send_message(chat_id, &reply).await;
                        continue;
                    }
                }
                TgIngressClassification::Ignore => continue,
            };

            // `/id` selalu dijawab (dibatasi cooldown untuk user non-whitelist agar tahan flood DoS).
            if display_text.eq_ignore_ascii_case("/id") {
                let allowed = latest_cfg.telegram.is_allowed(user_id, chat_id);
                if allowed || should_reply_unauth(&mut unauth_reply_cooldown, chat_id) {
                    let allowed_note = if allowed {
                        "Status: sudah ada di whitelist."
                    } else {
                        "Status: belum di whitelist."
                    };
                    let _ = tg
                        .send_message(
                            chat_id,
                            &format!(
                                "Info ID Telegram:\nUser ID: {user_id}\nChat ID: {chat_id}\n{allowed_note}"
                            ),
                        )
                        .await;
                }
                continue;
            }

            if !latest_cfg.telegram.is_allowed(user_id, chat_id) {
                println!("[telegram] ditolak dari {sender_name} (user={user_id}, chat={chat_id})");
                if should_reply_unauth(&mut unauth_reply_cooldown, chat_id) {
                    let _ = tg
                        .send_message(
                            chat_id,
                            &format!(
                                "Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode."
                            ),
                        )
                        .await;
                }
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
            let tg_ui_clone = tg_ui.clone();
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
                    tg_ui_clone,
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
                        let safe = tg_clone.sanitize_error(&e.to_string());
                        eprintln!("[telegram] error ({chat_id}): {safe}");
                        let _ = tokio::time::timeout(
                            Duration::from_secs(15),
                            tg_clone.send_long_markdown(chat_id, &format!("Error: {safe}")),
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

/// Ambil folder kerja live milik `chat_id` (mengikuti `/cd`) bila sesi sudah ada dan tidak terkunci,
/// atau jatuh aman ke `fallback_cwd` startup gateway.
async fn resolve_chat_cwd(
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    chat_id: i64,
    fallback_cwd: &PathBuf,
) -> PathBuf {
    let maybe_arc = { sessions.lock().await.get(&chat_id).cloned() };
    if let Some(s_arc) = maybe_arc {
        if let Ok(s) = s_arc.try_lock() {
            return s.ctx.cwd.clone();
        }
    }
    fallback_cwd.clone()
}

/// Keluarkan sesi chat yang sudah lama idle atau melebihi pagu cache dari RAM gateway
/// (data riwayat penuh tetap persisten di SQLite; sesi aktif/sibuk tidak pernah disentuh).
async fn evict_idle_gateway_sessions(
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    session_last_used: &Arc<std::sync::Mutex<HashMap<i64, Instant>>>,
    busy_chats: &Arc<std::sync::Mutex<HashSet<i64>>>,
    cancel_tokens: &Arc<std::sync::Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>>,
    chat_gens: &Arc<std::sync::Mutex<HashMap<i64, Arc<AtomicU64>>>>,
    tg_ui: &Arc<TgUiSharedState>,
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
        if let Ok(mut ct) = cancel_tokens.lock() {
            ct.remove(&cid);
        }
        if let Ok(mut cg) = chat_gens.lock() {
            cg.remove(&cid);
        }
        tg_ui.evict_chat(cid);
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
    tg_ui: Arc<TgUiSharedState>,
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
        IncomingTgPayload::Photo { .. }
        | IncomingTgPayload::Document { .. }
        | IncomingTgPayload::Audio { .. } => false,
    };

    // Pengawal abort-on-drop untuk task per-giliran agar tidak ada satu pun task yang pernah ter-detach yatim.
    struct AbortTaskOnDrop<T>(Option<tokio::task::JoinHandle<T>>);
    impl<T> AbortTaskOnDrop<T> {
        fn abort_now(&mut self) {
            if let Some(h) = self.0.take() {
                h.abort();
            }
        }
    }
    impl<T> Drop for AbortTaskOnDrop<T> {
        fn drop(&mut self) {
            self.abort_now();
        }
    }

    // Umpan balik instan (<300ms): indikator mengetik + gelembung status sementara polos tanpa emoji dekoratif.
    let mut progress_msg_id: Option<i64> = None;
    let tg_typing_init = tg.clone();
    let mut typing_init_guard = AbortTaskOnDrop(Some(tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(5), tg_typing_init.send_typing(chat_id))
            .await;
    })));
    if !is_slash_cmd {
        let initial_html = match &payload {
            IncomingTgPayload::Photo { .. } => "<i>Mengunduh foto...</i>",
            IncomingTgPayload::Document { .. } => "<i>Mengunduh berkas...</i>",
            IncomingTgPayload::Audio { .. } => "<i>Mentranskripsi pesan suara...</i>",
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

    // Unduh foto/dokumen di dalam task chat ini agar loop polling Telegram tidak pernah terblokir.
    // Hormati `/cd` dengan mengambil folder kerja live dari sesi chat bila sudah aktif.
    let text = match payload {
        IncomingTgPayload::Text(t) => t,
        IncomingTgPayload::Photo {
            file_id,
            message_id,
            caption,
            cwd,
        } => {
            let live_cwd = resolve_chat_cwd(&sessions, chat_id, &cwd).await;
            let mut attached_path: Option<String> = None;
            if let Ok(fi) = tg.get_file(&file_id).await {
                if let Some(fp) = fi.file_path {
                    if let Ok(bytes) = tg.download_file_bytes_limited(&fp, 6 * 1024 * 1024).await {
                        let base_name = fp.rsplit('/').next().unwrap_or(&fp);
                        let raw_ext = base_name
                            .rsplit_once('.')
                            .map(|(_, e)| e)
                            .unwrap_or("jpg");
                        let ext: String = raw_ext
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .take(8)
                            .collect();
                        let ext = if ext.is_empty() { "jpg" } else { ext.as_str() };
                        let rel = format!(".selencode/tg_uploads/tg_{message_id}.{ext}");
                        let full = live_cwd.join(&rel);
                        if let Some(parent) = full.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::write(&full, &bytes).is_ok() {
                            attached_path = Some(rel);
                        }
                    }
                }
            }
            match attached_path {
                Some(rel) => format!("@{rel} {caption}"),
                None => caption,
            }
        }
        IncomingTgPayload::Document {
            file_id,
            message_id,
            file_name,
            mime_type,
            file_size,
            caption,
            cwd,
        } => {
            let live_cwd = resolve_chat_cwd(&sessions, chat_id, &cwd).await;
            let doc_res: std::result::Result<(String, usize), String> = async {
                if let Some(sz) = file_size.filter(|&sz| sz > TG_DOCUMENT_MAX_BYTES) {
                    return Err(format_tg_document_too_large_reply(file_name.as_deref(), sz));
                }
                let fi = tg
                    .get_file(&file_id)
                    .await
                    .map_err(|e| format!("Gagal mengambil info berkas dari Telegram: {e}"))?;
                if let Some(sz) = fi.file_size.filter(|&sz| sz > TG_DOCUMENT_MAX_BYTES) {
                    return Err(format_tg_document_too_large_reply(file_name.as_deref(), sz));
                }
                let Some(fp) = fi.file_path else {
                    return Err(
                        "Gagal mengunduh berkas: Telegram tidak mengembalikan lokasi file."
                            .to_string(),
                    );
                };
                let bytes = tg
                    .download_file_bytes(&fp)
                    .await
                    .map_err(|e| format!("Gagal mengunduh berkas dari Telegram: {e}"))?;
                let rel = save_tg_document_bytes(
                    &live_cwd,
                    message_id,
                    file_name.as_deref(),
                    mime_type.as_deref(),
                    Some(&fp),
                    &bytes,
                )
                .map_err(|e| format!("{e}"))?;
                Ok((rel, bytes.len()))
            }
            .await;

            match doc_res {
                Ok((rel, size_bytes)) => {
                    let orig_display = file_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("berkas");
                    format_tg_document_turn_prompt(
                        orig_display,
                        &rel,
                        mime_type.as_deref(),
                        size_bytes,
                        &caption,
                    )
                }
                Err(err_msg) => {
                    typing_init_guard.abort_now();
                    if let Some(mid) = progress_msg_id.take() {
                        if let Ok(mut map) = active_progress.lock() {
                            map.remove(&chat_id);
                        }
                        if tg
                            .edit_html_message(
                                chat_id,
                                mid,
                                &escape_telegram_html(&err_msg),
                                &err_msg,
                            )
                            .await
                            .is_err()
                        {
                            let _ = tg.send_message(chat_id, &err_msg).await;
                        }
                    } else {
                        let _ = tg.send_message(chat_id, &err_msg).await;
                    }
                    return Ok(());
                }
            }
        }
        IncomingTgPayload::Audio { att } => {
            let audio_res: std::result::Result<String, String> = async {
                let fi = tg
                    .get_file(&att.file_id)
                    .await
                    .map_err(|e| format!("Gagal mengambil info audio dari Telegram: {e}"))?;
                if let Some(sz) = fi.file_size.filter(|&sz| sz > TG_DOCUMENT_MAX_BYTES) {
                    return Err(format_tg_document_too_large_reply(Some(&att.file_name), sz));
                }
                let Some(fp) = fi.file_path else {
                    return Err("Gagal mengunduh audio: lokasi file Telegram kosong.".to_string());
                };
                let bytes = tg
                    .download_file_bytes(&fp)
                    .await
                    .map_err(|e| format!("Gagal mengunduh audio: {e}"))?;
                let (_, pcfg_a, api_key) = latest_cfg
                    .resolve_provider(provider_override.as_deref())
                    .map_err(|e| format!("Provider belum siap untuk transkripsi: {e}"))?;
                transcribe_audio_bytes(
                    &pcfg_a.kind,
                    pcfg_a.base_url.as_deref(),
                    &api_key,
                    &pcfg_a.model,
                    &bytes,
                    &att.file_name,
                    &att.mime_type,
                )
                .await
                .map_err(|e| format!("{e}"))
            }
            .await;

            match audio_res {
                Ok(transcript) => {
                    let html = format!(
                        "🎙 <b>Transkripsi:</b> <i>{}</i>",
                        escape_telegram_html(&transcript)
                    );
                    let _ = tg
                        .send_html_message(chat_id, &html, &format!("Transkripsi: {transcript}"))
                        .await;
                    if let Some(mid) = progress_msg_id {
                        let _ = tg
                            .edit_html_message(
                                chat_id,
                                mid,
                                "<i>Memproses permintaan...</i>",
                                "Memproses permintaan...",
                            )
                            .await;
                    }
                    if att.caption.is_empty() {
                        transcript
                    } else {
                        format!("{}\n\n[Transkripsi suara]: {transcript}", att.caption)
                    }
                }
                Err(err_msg) => {
                    typing_init_guard.abort_now();
                    if let Some(mid) = progress_msg_id.take() {
                        if let Ok(mut map) = active_progress.lock() {
                            map.remove(&chat_id);
                        }
                        if tg
                            .edit_html_message(
                                chat_id,
                                mid,
                                &escape_telegram_html(&err_msg),
                                &err_msg,
                            )
                            .await
                            .is_err()
                        {
                            let _ = tg.send_message(chat_id, &err_msg).await;
                        }
                    } else {
                        let _ = tg.send_message(chat_id, &err_msg).await;
                    }
                    return Ok(());
                }
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
            let first_prompt_opt = (!is_slash_cmd).then_some(text.as_str());
            let mut s = match Session::build_for_telegram_chat(
                provider_override.as_deref(),
                yes_flag,
                chat_id,
                first_prompt_opt,
                Some(approval_tx),
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
            if s.resumed_note.is_some() || !is_slash_cmd {
                let _ = s.store.set_telegram_chat_session(chat_id, &s.session_id);
            }
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

    // Sinkronkan otomatis bila user mengganti router AI / model / sandbox dari terminal TUI atau callback!
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
    if let Some(sb_mode) = selencode::sandbox::SandboxMode::parse(&latest_cfg.sandbox.mode) {
        if sb_mode != s.ctx.sandbox.mode() {
            s.ctx.sandbox.set_mode(sb_mode);
            s.cfg.sandbox.mode = sb_mode.as_str().to_string();
        }
    }

    // Perintah khusus Telegram (/start, /status).
    if text.eq_ignore_ascii_case("/start") {
        let ctx_exact = format_context_tokens_exact(s.ctx.context_limit_tokens);
        let start_html = format!(
            "<b>Selena — SelenCode v{}</b>\n\
             ──────────────\n\
             • <b>Provider:</b> <code>{}</code> / <code>{}</code>\n\
             • <b>Konteks:</b> {}\n\
             • <b>Folder:</b> <code>{}</code>\n\n\
             Kirim pesan coding apa saja langsung di chat ini.\n\n\
             ▸ <b>Perintah Populer</b>\n\
             • /resume — 10 sesi terakhir bernomor &amp; lanjutkan sesi\n\
             • /model — pilih atau kelola model AI\n\
             • /new — mulai sesi baru yang bersih\n\
             • /skills — daftar &amp; kelola skills\n\
             • /verify — cek build, test, &amp; lint proyek\n\
             • /worktree — isolasi refactor aman pakai Git Worktree\n\
             • /sandbox · /approve — atur keamanan &amp; izin tool\n\
             • /doctor · /mcp · /route — diagnostik &amp; status sistem\n\
             • /help — semua perintah lengkap",
            env!("CARGO_PKG_VERSION"),
            escape_telegram_html(&s.provider_name),
            escape_telegram_html(&s.pcfg.model),
            escape_telegram_html(&ctx_exact),
            escape_telegram_html(&s.ctx.cwd.display().to_string())
        );
        let plain = format!(
            "Selena — SelenCode v{}\nProvider: {} / {}\nKonteks: {ctx_exact}\nFolder: {}\nKetik /help untuk semua perintah.",
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
            // Alur rapi 2-tahap (v0.5.31): tampilkan daftar Provider terlebih dahulu ->
            // tap provider untuk membuka isi model-model di dalamnya.
            if !s.cfg.providers.is_empty() {
                let card = tg_build_provider_picker(
                    &s.cfg,
                    &s.provider_name,
                    &s.pcfg.model,
                    s.ctx.context_limit_tokens,
                    0,
                );
                tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                    .await?;
            } else {
                tg.send_menu_markup(chat_id, &s.models_menu()).await?;
            }
            return Ok(());
        }
        SlashCommand::Model(Some(arg)) => {
            match parse_model_action(&arg) {
                ModelAction::FetchList(filter) => {
                    let reply = s.fetch_remote_models_menu(filter.as_deref()).await?;
                    tg.send_menu_markup(chat_id, &reply).await?;
                }
                ModelAction::SwitchOrModel(ref target) if s.cfg.providers.contains_key(target) => {
                    s.switch_provider_and_persist(target)?;
                    let names = tg_sorted_providers(&s.cfg);
                    let active_pidx = names
                        .iter()
                        .position(|n| n == &s.provider_name)
                        .unwrap_or(0);
                    if let Some(card) = tg_build_model_picker_for_provider(
                        &s.cfg,
                        &s.provider_name,
                        &s.pcfg.model,
                        s.ctx.context_limit_tokens,
                        active_pidx,
                        0,
                        &s.model_context_cache,
                    ) {
                        tg.send_html_message_with_keyboard(
                            chat_id,
                            &card.html,
                            &card.plain,
                            card.rows,
                        )
                        .await?;
                    } else {
                        tg.send_menu_markup(chat_id, &s.models_menu()).await?;
                    }
                }
                ModelAction::SelectIndex(_)
                | ModelAction::SetCurrentModel(_)
                | ModelAction::SwitchAndSetModel { .. } => {
                    let _ = s.apply_model_arg(&arg)?;
                    let (html, plain) = format_tg_model_switched_html(
                        &s.provider_name,
                        &s.pcfg.kind,
                        &s.pcfg.model,
                        s.ctx.context_limit_tokens,
                    );
                    tg.send_html_message(chat_id, &html, &plain).await?;
                }
                ModelAction::SwitchOrModel(_) => {
                    let reply = s.apply_model_arg(&arg)?;
                    if reply.starts_with("# Ditemukan ") {
                        tg.send_menu_markup(chat_id, &reply).await?;
                    } else {
                        let (html, plain) = format_tg_model_switched_html(
                            &s.provider_name,
                            &s.pcfg.kind,
                            &s.pcfg.model,
                            s.ctx.context_limit_tokens,
                        );
                        tg.send_html_message(chat_id, &html, &plain).await?;
                    }
                }
                ModelAction::AddModels(_)
                | ModelAction::RemoveModel(_)
                | ModelAction::ClearModels => {
                    let reply = s.apply_model_arg(&arg)?;
                    tg.send_menu_markup(chat_id, &reply).await?;
                }
            }
            return Ok(());
        }
        SlashCommand::Models(filter) => {
            let reply = s.fetch_remote_models_menu(filter.as_deref()).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Provider(None) => {
            if !s.cfg.providers.is_empty() {
                let card = tg_build_provider_picker(
                    &s.cfg,
                    &s.provider_name,
                    &s.pcfg.model,
                    s.ctx.context_limit_tokens,
                    0,
                );
                tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                    .await?;
            } else {
                let reply = s.handle_provider(None)?;
                tg.send_menu_markup(chat_id, &reply).await?;
            }
            return Ok(());
        }
        SlashCommand::Provider(Some(arg)) => {
            let reply = s.handle_provider(Some(&arg))?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Approve(None) => {
            let card = tg_build_approve_picker(s);
            tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                .await?;
            return Ok(());
        }
        SlashCommand::Approve(Some(arg)) => {
            let reply = s.handle_approve(Some(&arg))?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Sandbox(None) => {
            let card = tg_build_sandbox_picker(s);
            tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                .await?;
            return Ok(());
        }
        SlashCommand::Sandbox(Some(arg)) => {
            let reply = s.handle_sandbox(Some(&arg))?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Cd(arg) => {
            let reply = s.handle_cd(arg.as_deref())?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::New => {
            if let Some(job) = s.prepare_bg_review_job(true) {
                spawn_telegram_bg_review(
                    tg.clone(),
                    chat_id,
                    busy_chats.clone(),
                    gen_counter.clone(),
                    my_gen,
                    s.store.clone(),
                    job,
                );
            }
            let sid = s.reset_session()?;
            let _ = s
                .store
                .set_telegram_chat_session(chat_id, selencode::store::Store::SESSION_POINTER_NEW);
            let (html, plain) = format_tg_new_session_html(
                &sid,
                &s.provider_name,
                &s.pcfg.kind,
                &s.pcfg.model,
                s.ctx.context_limit_tokens,
                &s.ctx.cwd.display().to_string(),
            );
            tg.send_html_message(chat_id, &html, &plain).await?;
            return Ok(());
        }
        SlashCommand::Compact => {
            let reply = s.compact_now_smart().await;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Skills(arg) => {
            let is_list = arg
                .as_deref()
                .map(|a| {
                    let l = a.trim().to_ascii_lowercase();
                    l.is_empty() || matches!(l.as_str(), "list" | "ls")
                })
                .unwrap_or(true);
            let reply = s.handle_skills(arg.as_deref()).await?;
            if is_list {
                let card = tg_build_skills_picker(s, &reply, chat_id, &tg_ui);
                if card.html.chars().count() <= 4050 {
                    tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                        .await?;
                    return Ok(());
                }
            }
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
            let is_list = arg
                .as_deref()
                .map(|a| {
                    let l = a.trim().to_ascii_lowercase();
                    l.is_empty() || matches!(l.as_str(), "list" | "ls" | "status")
                })
                .unwrap_or(true);
            if is_list {
                let card = tg_build_cron_picker(&s.store, chat_id)?;
                tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                    .await?;
            } else {
                let reply = s.handle_cron(arg.as_deref(), Some(chat_id))?;
                tg.send_menu_markup(chat_id, &reply).await?;
            }
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
                let task_typing =
                    TypingLoopGuard::spawn_for_client(tg.clone(), chat_id, Duration::from_secs(4));
                let task_timeout = latest_cfg.telegram.turn_timeout_secs;
                let task_res = tokio::select! {
                    r = run_turn_counted(
                        s.provider.clone(),
                        &s.registry,
                        &s.ctx,
                        &mut s.messages,
                        &prompt,
                        0,
                    ) => Some(r),
                    _ = cancel.notified() => None,
                    _ = async {
                        if task_timeout > 0 {
                            tokio::time::sleep(Duration::from_secs(task_timeout)).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => Some(Err(anyhow::anyhow!("batas waktu giliran ({task_timeout} detik) tercapai"))),
                };
                task_typing.stop_and_join(Duration::from_secs(2)).await;
                match task_res {
                    None => {
                        tg.send_message(chat_id, "Lanjutan task dibatalkan.").await?;
                    }
                    Some(Ok(res)) => {
                        let reply_text = if res.0.trim().is_empty() {
                            "(task dilanjutkan tanpa ringkasan teks)".to_string()
                        } else {
                            res.0
                        };
                        tg.send_long_markdown(chat_id, &reply_text).await?;
                    }
                    Some(Err(e)) => {
                        let safe = tg.sanitize_error(&e.to_string());
                        tg.send_long_markdown(chat_id, &format!("Error: {safe}"))
                            .await?;
                    }
                }
            }
            return Ok(());
        }
        SlashCommand::Telegram(arg) => {
            // Keamanan: mutasi token bot & whitelist `/telegram` HANYA diizinkan dari terminal lokal
            // agar token bot tidak pernah masuk riwayat chat Telegram & mencegah takeover dari chat.
            let act = selencode::telegram::parse_telegram_action(arg.as_deref());
            match act {
                selencode::telegram::TelegramAction::Status
                | selencode::telegram::TelegramAction::Test
                | selencode::telegram::TelegramAction::Usage => {
                    let reply = s.handle_telegram(arg.as_deref()).await?;
                    tg.send_menu_markup(chat_id, &reply).await?;
                }
                _ => {
                    tg.send_message(
                        chat_id,
                        "Demi keamanan, perubahan token bot/whitelist/approval lewat `/telegram setup|allow|rm|off|approve` hanya boleh dijalankan dari terminal lokal (bukan dari chat Telegram). Gunakan `/approve` untuk mengatur mode izin sesi.",
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        SlashCommand::Sessions => {
            let card = tg_build_resume_picker(s)?;
            tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                .await?;
            return Ok(());
        }
        SlashCommand::Resume(None) => {
            let card = tg_build_resume_picker(s)?;
            tg.send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
                .await?;
            return Ok(());
        }
        SlashCommand::Resume(Some(arg)) => {
            match s.handle_resume(Some(&arg))? {
                super::commands::ResumeOutcome::Menu(card) => {
                    tg.send_menu_markup(chat_id, &card).await?;
                }
                super::commands::ResumeOutcome::Switched(data) => {
                    let _ = s
                        .store
                        .set_telegram_chat_session(chat_id, &data.session_id);
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
            if !s
                .ctx
                .approver
                .ask(&format!("inline_shell: {cmd}"), &format!("Jalankan `!{cmd}` di {}", s.ctx.cwd.display()))
                .await
            {
                tg.send_message(chat_id, &format!("Ditolak user — `!{cmd}` TIDAK dijalankan."))
                    .await?;
                return Ok(());
            }
            let reply = s.handle_inline_shell(&cmd).await?;
            tg.send_menu_markup(chat_id, &reply).await?;
            return Ok(());
        }
        SlashCommand::Plan => {
            tg_ui.set_plan_next(chat_id, true);
            let html = "📋 <b>Mode Plan aktif untuk pesan berikutnya.</b>\n\
                        <i>Kirim permintaan coding-mu sekarang — Selena akan menganalisis baca-saja lalu menyajikan rencana dengan tombol Setujui / Batalkan.</i>";
            tg.send_html_message(
                chat_id,
                html,
                "Mode Plan aktif untuk pesan berikutnya. Kirim permintaan coding-mu sekarang.",
            )
            .await?;
            return Ok(());
        }
        SlashCommand::Exit | SlashCommand::Details | SlashCommand::Setup => {
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
    // Perbaikan 1 (v0.5.16): hentikan ping mengetik awal & jalankan `TypingLoopGuard` berbasis `watch`
    // yang mengingat sinyal berhenti + abort eksplisit agar tidak pernah bocor sebagai task yatim.
    typing_init_guard.abort_now();
    let typing_guard =
        TypingLoopGuard::spawn_for_client(tg.clone(), chat_id, Duration::from_secs(4));

    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel::<UiEvent>(256);
    let (col_stop_tx, mut col_stop_rx) = tokio::sync::watch::channel(false);
    let tg_prog = tg.clone();
    let hub_for_col = hub.clone();
    let ask_hub_col = ask_hub.clone();
    let max_steps_ui = 0usize;
    let stall_notice_secs = latest_cfg.telegram.stall_notice_secs;
    let turn_timeout_secs = latest_cfg.telegram.turn_timeout_secs;
    let mut collector_guard = AbortTaskOnDrop(Some(tokio::spawn(async move {
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
            if *col_stop_rx.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = async {
                    while !*col_stop_rx.borrow() {
                        if col_stop_rx.changed().await.is_err() {
                            break;
                        }
                    }
                } => break,
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
                        UiEvent::Info(info) if info.contains("Selena improvement") => {
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
    })));

    enum TurnOutcome {
        Finished(Result<String>),
        Cancelled,
        TimedOut(u64),
    }

    let _ = s.store.set_telegram_chat_session(chat_id, &s.session_id);
    let is_plan_turn = tg_ui.take_plan_next(chat_id);
    let turn_outcome = tokio::select! {
        r = async {
            if is_plan_turn {
                run_turn_ui_mode(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &text,
                    0,
                    ui_tx,
                    TurnMode::Plan,
                )
                .await
            } else {
                run_turn_ui(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &text,
                    0,
                    ui_tx,
                )
                .await
            }
        } => TurnOutcome::Finished(r),
        _ = cancel.notified() => TurnOutcome::Cancelled,
        _ = async {
            if turn_timeout_secs > 0 {
                tokio::time::sleep(Duration::from_secs(turn_timeout_secs)).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => TurnOutcome::TimedOut(turn_timeout_secs),
    };

    // Perbaikan 1 (v0.5.16): hentikan & pastikan kedua task per-giliran (`typing_guard` & `collector`)
    // benar-benar mati (abort eksplisit bila melewati tenggang tunggu — tidak pernah ter-detach!).
    typing_guard.stop_and_join(Duration::from_secs(2)).await;
    let _ = col_stop_tx.send(true);
    let (tool_events, notes) = if let Some(mut col_handle) = collector_guard.0.take() {
        match tokio::time::timeout(Duration::from_secs(3), &mut col_handle).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => (Vec::new(), Vec::new()),
            Err(_) => {
                col_handle.abort();
                let _ = col_handle.await;
                (Vec::new(), Vec::new())
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };
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

    // Perbaikan 1 (#1, #4, #5) & Perbaikan 3 v0.5.8 + Rendering Telegram HP:
    // Jawaban akhir SELALU dikirim sebagai PESAN BARU (`sendMessage`) dengan blok `Aksi Tool (N)`
    // yang menampilkan pratinjau argumen teredaksi, serta blok kode panjang (>=25 baris / >=1.500 char)
    // diekstrak otomatis menjadi lampiran dokumen (`sendDocument`) agar tidak membanjiri chat.
    let (answer_body_md, long_code_blocks) =
        selencode::telegram::extract_long_code_blocks_for_telegram(&answer_md);
    if total_tools > 0 {
        let shown: Vec<String> = summary_lines
            .into_iter()
            .map(|t| escape_telegram_html(&t))
            .collect();
        let tool_block = format!(
            "<blockquote expandable><b>Aksi Tool ({total_tools})</b>\n{}</blockquote>",
            shown.join("\n")
        );
        let body_html = markdown_to_telegram_html(&answer_body_md);
        let combined = if body_html.is_empty() {
            tool_block.clone()
        } else {
            format!("{tool_block}\n\n{body_html}")
        };
        if combined.chars().count() <= 4050 {
            tg.send_html_message(chat_id, &combined, &answer_body_md)
                .await?;
        } else {
            tg.send_html_message(chat_id, &tool_block, "Aksi tool selesai.")
                .await?;
            tg.send_long_markdown(chat_id, &answer_body_md).await?;
        }
    } else {
        let final_html = markdown_to_telegram_html(&answer_body_md);
        if !final_html.is_empty() && final_html.chars().count() <= 4050 {
            tg.send_html_message(chat_id, &final_html, &answer_body_md)
                .await?;
        } else {
            tg.send_long_markdown(chat_id, &answer_body_md).await?;
        }
    }
    for blk in long_code_blocks {
        let cap = format!("{} ({} baris)", blk.filename, blk.lines);
        if let Err(e) = tg
            .send_document_bytes(
                chat_id,
                &blk.filename,
                blk.content.as_bytes().to_vec(),
                Some(&cap),
            )
            .await
        {
            eprintln!(
                "[telegram][code-file] gagal mengunggah {} ({e}) — jatuh ke pengiriman teks kode.",
                blk.filename
            );
            let fallback_md = format!("```{}\n{}\n```", blk.lang, blk.content);
            let _ = tg.send_long_markdown(chat_id, &fallback_md).await;
        }
    }

    if is_plan_turn {
        let plan_text = extract_plan(&answer).unwrap_or_else(|| answer.clone());
        if !plan_text.trim().is_empty() {
            tg_ui.set_pending_plan(chat_id, plan_text.clone());
            // Isi rencana sudah terkirim di pesan jawaban di atas — cukup kirim kartu aksi ringkas
            // agar tidak menduplikasi teks rencana dua kali!
            let card = build_tg_plan_confirm_card(&plan_text);
            let _ = tg
                .send_html_message_with_keyboard(
                    chat_id,
                    "📋 <b>Rencana di atas siap dieksekusi.</b>\n<i>Pilih tombol di bawah untuk menjalankan atau membatalkannya.</i>",
                    "Rencana di atas siap dieksekusi.",
                    card.rows,
                )
                .await;
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

    // Selena Improvement (v0.5.24): picu di latar sesudah penutup terkirim
    // (tidak pernah memblokir giliran aktif; dibatalkan bila giliran live baru masuk).
    s.turns_since_review += 1;
    if let Some(job) = s.prepare_bg_review_job(false) {
        spawn_telegram_bg_review(
            tg.clone(),
            chat_id,
            busy_chats.clone(),
            gen_counter.clone(),
            my_gen,
            s.store.clone(),
            job,
        );
    }
    Ok(())
}

/// Jalankan Selena Improvement di latar untuk chat Telegram (v0.5.24).
/// Tidak pernah memblokir giliran baru; dibatalkan segera bila user mengirim pesan baru (`gen_counter` naik).
/// Menandai `delivered = 1` HANYA sesudah pesan Telegram benar-benar terkirim.
fn spawn_telegram_bg_review(
    tg: TelegramClient,
    chat_id: i64,
    busy_chats: Arc<std::sync::Mutex<HashSet<i64>>>,
    gen_counter: Arc<AtomicU64>,
    expected_gen: u64,
    store: Arc<selencode::store::Store>,
    job: super::session::BgReviewJob,
) {
    tokio::spawn(async move {
        // Beri jeda singkat agar flag busy giliran selesai dilepas, dan cek apakah giliran baru sudah masuk.
        tokio::time::sleep(Duration::from_millis(250)).await;
        if gen_counter.load(Ordering::SeqCst) != expected_gen {
            return;
        }
        if busy_chats
            .lock()
            .map(|set| set.contains(&chat_id))
            .unwrap_or(false)
        {
            return;
        }
        // Prioritas latar (3.5 #5): batalkan bila giliran live baru mulai saat review berjalan.
        let run_fut = tokio::time::timeout(Duration::from_secs(30), job.run(false));
        let cancel_poll = async {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if gen_counter.load(Ordering::SeqCst) != expected_gen {
                    break;
                }
            }
        };
        let outcome = tokio::select! {
            res = run_fut => match res {
                Ok(Ok(Some(o))) => o,
                _ => return,
            },
            _ = cancel_poll => return,
        };
        if gen_counter.load(Ordering::SeqCst) != expected_gen {
            return;
        }
        let Some(report) = outcome.report_message else {
            return;
        };
        let html = escape_telegram_html(&report);
        let sent_ok = if outcome.staged_ids.is_empty() {
            tg.send_html_message(chat_id, &html, &report).await.is_ok()
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
            tg.send_html_message_with_keyboard(chat_id, &html, &report, rows)
                .await
                .is_ok()
        };
        if sent_ok {
            if let Some(rid) = outcome.run_id {
                let _ = store.mark_review_run_delivered(rid);
            }
        }
    });
}

