use super::cli::Cli;
use super::session::Session;
use anyhow::Result;
use selencode::agent::context::sanitize_interrupted_messages;
use selencode::agent::{run_turn_counted, run_turn_ui, UiEvent};
use selencode::config;
use selencode::repl::{
    help_menu_markup, parse_model_action, parse_slash_command, strip_menu_markup, ModelAction,
    SlashCommand,
};
use selencode::security::{ApprovalMode, ApprovalRequest};
use selencode::telegram::{TelegramClient, TgApprovalHub};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
    let me = tg.get_me().await?;
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
    println!("  Bot      : {bot_handle} (id: {})", me.id);
    println!("  Provider : {prov_name} / {}", pcfg.model);
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

    let hub = TgApprovalHub::new();
    let sessions: Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let busy_chats: Arc<std::sync::Mutex<HashSet<i64>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    let cancel_tokens: Arc<std::sync::Mutex<HashMap<i64, Arc<tokio::sync::Notify>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let chat_gens: Arc<std::sync::Mutex<HashMap<i64, Arc<AtomicU64>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));
    let provider_override = cli.provider.clone();
    let yes_flag = cli.yes;

    // Lewati antrean pesan lama sebelum gateway dinyalakan agar tidak mengeksekusi perintah usang.
    let mut offset: i64 = tg.skip_backlog().await;
    if offset > 0 {
        println!("  (Antrean pesan lama sebelum startup dilewati; siap menerima pesan baru.)\n");
    }
    loop {
        let updates = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n☾ Telegram Gateway dihentikan.");
                break;
            }
            res = tg.get_updates(offset, 25) => match res {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("[telegram] gagal polling: {e} (coba lagi 3 detik)");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            }
        };

        // Muat ulang config terbaru agar perubahan `/telegram allow` langsung berlaku.
        let latest_cfg = config::load().unwrap_or_else(|_| cfg.clone());

        for upd in updates {
            offset = upd.update_id + 1;

            if let Some(cb) = upd.callback_query {
                let chat_id = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
                if latest_cfg.telegram.is_allowed(cb.from.id, chat_id) {
                    let _ = hub.handle_callback(&tg, &cb).await;
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
            let Some(raw_text) = msg.text else {
                continue;
            };
            let text = normalize_tg_command(&raw_text);
            if text.is_empty() {
                continue;
            }
            let chat_id = msg.chat.id;
            let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(chat_id);
            let sender_name = msg
                .from
                .as_ref()
                .and_then(|u| u.username.clone().or_else(|| Some(u.first_name.clone())))
                .unwrap_or_else(|| user_id.to_string());

            // `/id` selalu dijawab agar pemilik bisa langsung tahu ID untuk whitelist.
            if text.eq_ignore_ascii_case("/id") {
                let allowed_note = if latest_cfg.telegram.is_allowed(user_id, chat_id) {
                    "✅ Sudah ada di whitelist."
                } else {
                    "🔒 Belum di whitelist. Jalankan di SelenCode:\n/telegram allow <USER_ID>"
                };
                let _ = tg
                    .send_message(
                        chat_id,
                        &format!(
                            "☾ Info ID Telegram kamu:\nUser ID: {user_id}\nChat ID: {chat_id}\n{allowed_note}"
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
                            "🔒 Akses ditolak — User ID {user_id} belum ada di whitelist SelenCode.\n\
                             Tambahkan dari terminal SelenCode:\n\
                             /telegram allow {user_id}"
                        ),
                    )
                    .await;
                continue;
            }

            // `/cancel` selalu diproses walau chat sedang sibuk — batalkan giliran & approval tertunda!
            if text.eq_ignore_ascii_case("/cancel") || text.eq_ignore_ascii_case("/stop") {
                let cancelled_approvals = hub.cancel_chat_approvals(chat_id);
                let notify_opt = cancel_tokens
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&chat_id).cloned());
                if let Some(n) = notify_opt {
                    n.notify_waiters();
                }
                let msg = if cancelled_approvals > 0 {
                    format!("🛑 Dibatalkan ({cancelled_approvals} persetujuan tertunda ditolak).")
                } else {
                    "🛑 Sinyal pembatalan dikirim ke giliran aktif.".to_string()
                };
                let _ = tg.send_message(chat_id, &msg).await;
                continue;
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

            // Interupsi mulus ala OpenCode/Hermes: bila chat sedang sibuk, langsung
            // batalkan giliran & approval lama lalu jalankan pesan baru!
            let was_busy = {
                let mut set = busy_chats.lock().expect("busy_chats poisoned");
                !set.insert(chat_id)
            };
            if was_busy {
                let dropped = hub.cancel_chat_approvals(chat_id);
                cancel_notify.notify_waiters();
                let notice = if dropped > 0 {
                    format!("⚡ Menginterupsi giliran sebelumnya ({dropped} approval dibatalkan) — memproses pesan barumu…")
                } else {
                    "⚡ Menginterupsi giliran sebelumnya — memproses pesan barumu…".to_string()
                };
                let _ = tg.send_message(chat_id, &notice).await;
            }

            println!("[telegram] {sender_name} ({chat_id}): {text}");
            let tg_clone = tg.clone();
            let hub_clone = hub.clone();
            let sessions_clone = sessions.clone();
            let busy_clone = busy_chats.clone();
            let prov_clone = provider_override.clone();
            let tg_approval_cfg = latest_cfg.telegram.approval.clone();

            tokio::spawn(async move {
                // Pastikan flag busy dibersihkan hanya bila giliran ini masih generasi terbaru.
                let res = handle_telegram_chat_message(
                    tg_clone.clone(),
                    hub_clone,
                    sessions_clone,
                    cancel_notify,
                    gen_counter.clone(),
                    my_gen,
                    prov_clone,
                    yes_flag,
                    tg_approval_cfg,
                    chat_id,
                    text,
                )
                .await;
                if gen_counter.load(Ordering::SeqCst) == my_gen {
                    if let Ok(mut set) = busy_clone.lock() {
                        set.remove(&chat_id);
                    }
                }
                if let Err(e) = res {
                    let _ = tg_clone
                        .send_long_message(chat_id, &format!("❌ Error: {e}"))
                        .await;
                }
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_telegram_chat_message(
    tg: TelegramClient,
    hub: Arc<TgApprovalHub>,
    sessions: Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    cancel: Arc<tokio::sync::Notify>,
    gen_counter: Arc<AtomicU64>,
    my_gen: u64,
    provider_override: Option<String>,
    yes_flag: bool,
    tg_approval_cfg: String,
    chat_id: i64,
    text: String,
) -> Result<()> {
    // Ambil atau buat Session khusus untuk chat_id ini.
    let session_arc = {
        let mut map = sessions.lock().await;
        if let Some(existing) = map.get(&chat_id) {
            existing.clone()
        } else {
            let (approval_tx, mut approval_rx) =
                tokio::sync::mpsc::channel::<ApprovalRequest>(16);
            let s = Session::build(
                provider_override.as_deref(),
                yes_flag,
                false,
                Some(&text),
                Some(approval_tx),
                false,
            )
            .await?;
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
            tokio::spawn(async move {
                while let Some(req) = approval_rx.recv().await {
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
        return Ok(());
    }
    let mut s_guard = session_arc.lock().await;
    if gen_counter.load(Ordering::SeqCst) != my_gen {
        return Ok(());
    }
    let s: &mut Session = &mut *s_guard;

    // Perintah khusus Telegram (/start, /status).
    if text.eq_ignore_ascii_case("/start") {
        let msg = format!(
            "☾ Halo! Aku Selena dari SelenCode.\n\
             Provider: {} / {}\n\
             Folder: {}\n\n\
             Kirim pesan coding apa saja langsung di chat ini!\n\
             Perintah cepat:\n\
             • /model — lihat daftar model bernomor\n\
             • /model 2 · /model add <id> · /model clear\n\
             • /models [cari] — intip katalog API\n\
             • /approve [ask|edits|auto] — atur mode izin\n\
             • /new — mulai sesi baru · /status — info sesi\n\
             • /help — semua perintah",
            s.provider_name,
            s.pcfg.model,
            s.ctx.cwd.display()
        );
        tg.send_long_message(chat_id, &msg).await?;
        return Ok(());
    }
    if text.eq_ignore_ascii_case("/status") {
        let info = strip_menu_markup(&s.session_info_menu());
        tg.send_long_message(chat_id, &info).await?;
        return Ok(());
    }

    match parse_slash_command(&text) {
        SlashCommand::Help => {
            tg.send_long_message(chat_id, &strip_menu_markup(&help_menu_markup()))
                .await?;
            return Ok(());
        }
        SlashCommand::Model(None) => {
            tg.send_long_message(chat_id, &strip_menu_markup(&s.models_menu()))
                .await?;
            return Ok(());
        }
        SlashCommand::Model(Some(arg)) => {
            let reply = if let ModelAction::FetchList(filter) = parse_model_action(&arg) {
                s.fetch_remote_models_menu(filter.as_deref()).await?
            } else {
                s.apply_model_arg(&arg)?
            };
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Models(filter) => {
            let reply = s.fetch_remote_models_menu(filter.as_deref()).await?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Provider(arg) => {
            let reply = s.handle_provider(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Approve(arg) => {
            let reply = s.handle_approve(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::New => {
            let sid = s.reset_session()?;
            tg.send_message(chat_id, &format!("✨ Sesi baru dimulai ({sid})."))
                .await?;
            return Ok(());
        }
        SlashCommand::Compact => {
            let reply = s.compact_now();
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Skills(arg) => {
            let reply = s.handle_skills(arg.as_deref()).await?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Skill(arg) => {
            let reply = s.handle_use_skill(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Learn(arg) => {
            let reply = s.handle_learn_skill(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Memory(arg) => {
            let reply = s.handle_memory(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Undo => {
            let reply = s.handle_undo()?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Redo => {
            let reply = s.handle_redo()?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Checkpoint(arg) => {
            let reply = s.handle_checkpoint(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Rollback(arg) => {
            let reply = s.handle_rollback(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Verify(arg) => {
            let reply = s.handle_verify(arg.as_deref()).await?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Index(arg) => {
            let reply = s.handle_index(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Jobs(arg) => {
            let reply = s.handle_jobs(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Route(arg) => {
            let reply = s.handle_route(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Lsp(arg) => {
            let reply = s.handle_lsp(arg.as_deref()).await?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Task(arg) => {
            let (card, resume_prompt) = s.handle_task(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&card))
                .await?;
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
                tg.send_long_message(chat_id, &reply_text).await?;
            }
            return Ok(());
        }
        SlashCommand::Telegram(arg) => {
            let reply = s.handle_telegram(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Sessions => {
            let reply = s.sessions_menu()?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Resume => {
            tg.send_long_message(chat_id, &strip_menu_markup(&s.session_info_menu()))
                .await?;
            return Ok(());
        }
        SlashCommand::Init => {
            let reply = s.handle_init()?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::Export(arg) => {
            let reply = s.handle_export(arg.as_deref())?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
            return Ok(());
        }
        SlashCommand::InlineShell(cmd) => {
            let reply = s.handle_inline_shell(&cmd).await?;
            tg.send_long_message(chat_id, &strip_menu_markup(&reply))
                .await?;
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
            tg.send_message(chat_id, "🛑 Tidak ada giliran aktif yang berjalan.")
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

    // Giliran agent normal dengan indikator mengetik otomatis setiap 4 detik.
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
    let collector = tokio::spawn(async move {
        let mut tools: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        while let Some(ev) = ui_rx.recv().await {
            match ev {
                UiEvent::ToolDone {
                    name,
                    summary,
                    is_error,
                } => {
                    let icon = if is_error { "✗" } else { "✓" };
                    let short: String = summary.chars().take(70).collect();
                    tools.push(format!("{icon} {name}: {short}"));
                }
                UiEvent::Info(info) if info.contains("Self-improvement") => {
                    notes.push(info);
                }
                _ => {}
            }
        }
        (tools, notes)
    });

    let turn_res = tokio::select! {
        r = run_turn_ui(
            s.provider.clone(),
            &s.registry,
            &s.ctx,
            &mut s.messages,
            &text,
            s.cfg.agent.max_steps,
            ui_tx,
        ) => Some(r),
        _ = cancel.notified() => None,
    };

    typing_stop.notify_waiters();
    let _ = typing_task.await;
    let (tools, notes) = collector.await.unwrap_or_default();

    let Some(turn_res) = turn_res else {
        if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
            let _ = s.store.save_message(&s.session_id, &syn);
        }
        // Jangan kirim pesan "dibatalkan" bila pembatalan dipicu oleh pesan interupsi baru.
        if gen_counter.load(Ordering::SeqCst) == my_gen {
            tg.send_message(chat_id, "🛑 Giliran dibatalkan.").await?;
        }
        return Ok(());
    };
    let answer = turn_res?;
    let mut final_msg = String::new();
    if !tools.is_empty() {
        let shown: Vec<String> = tools.into_iter().take(8).collect();
        final_msg.push_str("🔧 Aksi:\n");
        final_msg.push_str(&shown.join("\n"));
        final_msg.push_str("\n\n");
    }
    final_msg.push_str(answer.trim());
    for note in notes {
        final_msg.push_str(&format!("\n\n{note}"));
    }
    tg.send_long_message(chat_id, &final_msg).await?;
    Ok(())
}

