use super::cli::Cli;
use super::commands::ResumeOutcome;
use super::provider_label;
use super::session::Session;
use anyhow::Result;
use selencode::agent::context::{calc_context_pct, estimate_tokens, sanitize_interrupted_messages};
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::{run_turn_ui, run_turn_ui_mode, UiEvent};
use selencode::repl::{parse_model_action, ModelAction};
use selencode::security::ApprovalRequest;
use selencode::tui::app::{App, SetupWizardState};
use selencode::tui::{ModelPickerCmd, TuiChannels, TuiCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
/// Kumpulkan path relatif file workspace (hormati `.gitignore`) untuk `@file` autocomplete TUI.
fn collect_workspace_files(cwd: &std::path::Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(6))
        .build();
    for entry in walker.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if rel_s.starts_with(".git/")
            || rel_s.starts_with(".selencode/")
            || rel_s.starts_with("target/")
            || rel_s.starts_with("node_modules/")
        {
            continue;
        }
        out.push(rel_s);
    }
    out.sort();
    out
}

/// Worker TUI: memiliki Session, menerima TuiCommand, mengirim UiEvent.
/// Loop agent berjalan di task ini; UI (thread blocking) hanya menggambar.
async fn tui_worker(
    mut s: Session,
    mut cmd_rx: tokio::sync::mpsc::Receiver<TuiCommand>,
    ui_tx: tokio::sync::mpsc::Sender<UiEvent>,
    cancel: Arc<tokio::sync::Notify>,
    interrupt_pending: Arc<AtomicBool>,
) {
    let active_turn = Arc::new(AtomicBool::new(false));
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            TuiCommand::Prompt(prompt) | TuiCommand::InterruptPrompt(prompt) => {
                let cwd_str = s.ctx.cwd.to_string_lossy().to_string();
                let _ = s.store.record_prompt_history(&cwd_str, &prompt);
                active_turn.store(true, Ordering::SeqCst);
                let mut turn_ok = false;
                tokio::select! {
                    r = run_turn_ui(
                        s.provider.clone(),
                        &s.registry,
                        &s.ctx,
                        &mut s.messages,
                        &prompt,
                        s.cfg.agent.max_steps,
                        ui_tx.clone(),
                    ) => match r {
                        Ok(_) => {
                            turn_ok = true;
                            let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    },
                    _ = cancel.notified() => {
                        if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                            let _ = s.store.save_message(&s.session_id, &syn);
                        }
                        if !interrupt_pending.swap(false, Ordering::SeqCst) {
                            let _ = ui_tx.send(UiEvent::Info("Giliran dihentikan.".into())).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    }
                }
                active_turn.store(false, Ordering::SeqCst);
                if turn_ok {
                    s.turns_since_review += 1;
                    if let Some(job) = s.prepare_bg_review_job(false) {
                        let ui_bg = ui_tx.clone();
                        let active_bg = active_turn.clone();
                        tokio::spawn(async move {
                            if active_bg.load(Ordering::SeqCst) {
                                return;
                            }
                            if let Ok(Ok(Some(outcome))) =
                                tokio::time::timeout(Duration::from_secs(20), job.run(true)).await
                            {
                                if let Some(msg) = outcome.report_message {
                                    let _ = ui_bg.send(UiEvent::Info(msg)).await;
                                }
                            }
                        });
                    }
                }
            }
            TuiCommand::PromptPlan(prompt) => {
                let cwd_str = s.ctx.cwd.to_string_lossy().to_string();
                let _ = s.store.record_prompt_history(&cwd_str, &prompt);
                tokio::select! {
                    r = run_turn_ui_mode(
                        s.provider.clone(),
                        &s.registry,
                        &s.ctx,
                        &mut s.messages,
                        &prompt,
                        s.cfg.agent.max_steps,
                        ui_tx.clone(),
                        TurnMode::Plan,
                    ) => match r {
                        Ok(text) => {
                            let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                            match extract_plan(&text) {
                                Some(plan) => {
                                    let _ = ui_tx.send(UiEvent::PlanReady(plan)).await;
                                }
                                None => {
                                    let _ = ui_tx
                                        .send(UiEvent::Info(
                                            "Giliran plan selesai tanpa teks rencana.".into(),
                                        ))
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    },
                    _ = cancel.notified() => {
                        if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                            let _ = s.store.save_message(&s.session_id, &syn);
                        }
                        if !interrupt_pending.swap(false, Ordering::SeqCst) {
                            let _ = ui_tx.send(UiEvent::Info("Giliran plan dihentikan.".into())).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    }
                }
            }
            TuiCommand::ExecutePlan(plan) => {
                let prompt = approved_plan_prompt(&plan);
                tokio::select! {
                    r = run_turn_ui(
                        s.provider.clone(),
                        &s.registry,
                        &s.ctx,
                        &mut s.messages,
                        &prompt,
                        s.cfg.agent.max_steps,
                        ui_tx.clone(),
                    ) => match r {
                        Ok(_) => {
                            let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    },
                    _ = cancel.notified() => {
                        if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                            let _ = s.store.save_message(&s.session_id, &syn);
                        }
                        if !interrupt_pending.swap(false, Ordering::SeqCst) {
                            let _ = ui_tx.send(UiEvent::Info("Eksekusi rencana dihentikan.".into())).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    }
                }
            }
            TuiCommand::InlineShell(cmd) => match s.handle_inline_shell(&cmd).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Undo => match s.handle_undo() {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Redo => match s.handle_redo() {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Checkpoint(arg) => match s.handle_checkpoint(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Rollback(arg) => match s.handle_rollback(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Worktree(arg) => match s.handle_worktree(arg.as_deref()).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::McpStatus => {
                let _ = ui_tx.send(UiEvent::Menu(s.handle_mcp_status())).await;
            }
            TuiCommand::Doctor => {
                let _ = ui_tx.send(UiEvent::Menu(s.handle_doctor())).await;
            }
            TuiCommand::Verify(arg) => match s.handle_verify(arg.as_deref()).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Index(arg) => match s.handle_index(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Jobs(arg) => match s.handle_jobs(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Route(arg) => match s.handle_route(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Lsp(arg) => match s.handle_lsp(arg.as_deref()).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Task(arg) => match s.handle_task(arg.as_deref()) {
                Ok((card, resume_prompt)) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                    if let Some(prompt) = resume_prompt {
                        tokio::select! {
                            r = run_turn_ui(
                                s.provider.clone(),
                                &s.registry,
                                &s.ctx,
                                &mut s.messages,
                                &prompt,
                                s.cfg.agent.max_steps,
                                ui_tx.clone(),
                            ) => match r {
                                Ok(_) => {
                                    let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                                }
                                Err(e) => {
                                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                                    let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                                }
                            },
                            _ = cancel.notified() => {
                                if let Some(syn) = sanitize_interrupted_messages(&mut s.messages) {
                                    let _ = s.store.save_message(&s.session_id, &syn);
                                }
                                if !interrupt_pending.swap(false, Ordering::SeqCst) {
                                    let _ = ui_tx.send(UiEvent::Info("Eksekusi task dihentikan.".into())).await;
                                    let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Memory(arg) => match s.handle_memory(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Skills(arg) => match s.handle_skills(arg.as_deref()).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Review(arg) => match s.handle_review(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Cron(arg) => match s.handle_cron(arg.as_deref(), None) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Watch(arg) => match s.handle_watch(arg.as_deref(), None) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Usage => match s.handle_usage() {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Search(arg) => match s.handle_search(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::UseSkill(arg) => match s.handle_use_skill(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::LearnSkill(arg) => match s.handle_learn_skill(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::InitAgents => match s.handle_init() {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::ExportChat(arg) => match s.handle_export(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::SetApproval(arg) => match s.handle_approve(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Sandbox(arg) => match s.handle_sandbox(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::ShowModels => {
                let _ = ui_tx.send(UiEvent::Menu(s.models_menu())).await;
            }
            TuiCommand::ModelPicker(action) => match action {
                ModelPickerCmd::Open => {
                    let _ = ui_tx
                        .send(UiEvent::ModelPicker(s.model_picker_data(false, None)))
                        .await;
                }
                ModelPickerCmd::SelectModel(model_id) => match s.select_model_literal(&model_id) {
                    Ok(card) => {
                        let _ = ui_tx
                            .send(UiEvent::ModelChanged {
                                provider: s.provider_name.clone(),
                                model: s.pcfg.model.clone(),
                            })
                            .await;
                        let _ = ui_tx
                            .send(UiEvent::ContextInfo {
                                used_tokens: estimate_tokens(&s.messages),
                                limit_tokens: s.ctx.context_limit_tokens,
                            })
                            .await;
                        let _ = ui_tx.send(UiEvent::Menu(card)).await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                    }
                },
                ModelPickerCmd::AddOne(model_id) => {
                    let msg = match s.add_model_literal(&model_id) {
                        Ok(true) => format!("Ditambahkan: {model_id}"),
                        Ok(false) => format!("Sudah ada di daftar: {model_id}"),
                        Err(e) => format!("Gagal: {e}"),
                    };
                    let _ = ui_tx
                        .send(UiEvent::ModelPicker(s.model_picker_data(false, Some(msg))))
                        .await;
                }
                ModelPickerCmd::ToggleOne(model_id) => {
                    let exists = s.pcfg.effective_models().iter().any(|m| m == &model_id);
                    let msg = if exists {
                        match s.remove_model_literal(&model_id) {
                            Ok(()) => format!("Dihapus dari daftar: {model_id}"),
                            Err(e) => format!("{e}"),
                        }
                    } else {
                        match s.add_model_literal(&model_id) {
                            Ok(_) => format!("Disimpan ke Model Saya: {model_id}"),
                            Err(e) => format!("Gagal: {e}"),
                        }
                    };
                    let _ = ui_tx
                        .send(UiEvent::ModelPicker(s.model_picker_data(true, Some(msg))))
                        .await;
                }
                ModelPickerCmd::RemoveOne(model_id) => {
                    let msg = match s.remove_model_literal(&model_id) {
                        Ok(()) => format!("Dihapus: {model_id}"),
                        Err(e) => format!("{e}"),
                    };
                    let _ = ui_tx
                        .send(UiEvent::ModelPicker(s.model_picker_data(false, Some(msg))))
                        .await;
                }
                ModelPickerCmd::ClearAll => {
                    let msg = match s.apply_model_arg("clear") {
                        Ok(_) => "Daftar dibersihkan — tersisa 1 model aktif.".to_string(),
                        Err(e) => format!("Gagal: {e}"),
                    };
                    let _ = ui_tx
                        .send(UiEvent::ModelPicker(s.model_picker_data(false, Some(msg))))
                        .await;
                }
                ModelPickerCmd::FetchCatalog => match s.fetch_remote_models_raw().await {
                    Ok(ids) => {
                        let msg = format!(
                            "{} model di katalog API (Space: pilih 1-per-1, Enter: pakai)",
                            ids.len()
                        );
                        let _ = ui_tx
                            .send(UiEvent::ModelPicker(s.model_picker_data(true, Some(msg))))
                            .await;
                    }
                    Err(e) => {
                        let _ = ui_tx
                            .send(UiEvent::ModelPicker(
                                s.model_picker_data(false, Some(format!("Katalog gagal: {e}"))),
                            ))
                            .await;
                    }
                },
                ModelPickerCmd::SwitchProvider(prov) => match s.switch_provider(&prov) {
                    Ok(()) => {
                        s.last_remote_models.clear();
                        let _ = ui_tx
                            .send(UiEvent::ModelChanged {
                                provider: s.provider_name.clone(),
                                model: s.pcfg.model.clone(),
                            })
                            .await;
                        let _ = ui_tx
                            .send(UiEvent::ContextInfo {
                                used_tokens: estimate_tokens(&s.messages),
                                limit_tokens: s.ctx.context_limit_tokens,
                            })
                            .await;
                        let _ = ui_tx
                            .send(UiEvent::ModelPicker(s.model_picker_data(
                                false,
                                Some(format!("Pindah ke provider '{prov}'")),
                            )))
                            .await;
                    }
                    Err(e) => {
                        let _ = ui_tx
                            .send(UiEvent::ModelPicker(
                                s.model_picker_data(false, Some(format!("Gagal pindah: {e}"))),
                            ))
                            .await;
                    }
                },
            },
            TuiCommand::SwitchOrSetModel(arg) => {
                if let ModelAction::FetchList(filter) = parse_model_action(&arg) {
                    match s.fetch_remote_models_menu(filter.as_deref()).await {
                        Ok(card) => {
                            let _ = ui_tx.send(UiEvent::Menu(card)).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                        }
                    }
                } else {
                    match s.apply_model_arg(&arg) {
                        Ok(card) => {
                            let _ = ui_tx
                                .send(UiEvent::ModelChanged {
                                    provider: s.provider_name.clone(),
                                    model: s.pcfg.model.clone(),
                                })
                                .await;
                            let _ = ui_tx
                                .send(UiEvent::ContextInfo {
                                    used_tokens: estimate_tokens(&s.messages),
                                    limit_tokens: s.ctx.context_limit_tokens,
                                })
                                .await;
                            let _ = ui_tx.send(UiEvent::Menu(card)).await;
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                        }
                    }
                }
            }
            TuiCommand::FetchRemoteModels(filter) => {
                match s.fetch_remote_models_menu(filter.as_deref()).await {
                    Ok(card) => {
                        let _ = ui_tx.send(UiEvent::Menu(card)).await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                    }
                }
            }
            TuiCommand::ManageProvider(arg) => match s.handle_provider(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx
                        .send(UiEvent::ModelChanged {
                            provider: s.provider_name.clone(),
                            model: s.pcfg.model.clone(),
                        })
                        .await;
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::SaveSetupProvider(res) => match s.apply_setup_result(res) {
                Ok(card) => {
                    let _ = ui_tx
                        .send(UiEvent::ModelChanged {
                            provider: s.provider_name.clone(),
                            model: s.pcfg.model.clone(),
                        })
                        .await;
                    let _ = ui_tx
                        .send(UiEvent::ContextInfo {
                            used_tokens: estimate_tokens(&s.messages),
                            limit_tokens: s.ctx.context_limit_tokens,
                        })
                        .await;
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Telegram(arg) => match s.handle_telegram(arg.as_deref()).await {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::NewSession => {
                if let Some(job) = s.prepare_bg_review_job(true) {
                    let ui_bg = ui_tx.clone();
                    let active_bg = active_turn.clone();
                    tokio::spawn(async move {
                        if active_bg.load(Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(Ok(Some(outcome))) =
                            tokio::time::timeout(Duration::from_secs(20), job.run(true)).await
                        {
                            if let Some(msg) = outcome.report_message {
                                let _ = ui_bg.send(UiEvent::Info(msg)).await;
                            }
                        }
                    });
                }
                match s.reset_session() {
                    Ok(sid) => {
                        let _ = ui_tx
                            .send(UiEvent::SessionReset {
                                session_id: sid,
                                provider: s.provider_name.clone(),
                                model: s.pcfg.model.clone(),
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                    }
                }
            }
            TuiCommand::CompactContext => {
                let card = s.compact_now();
                let _ = ui_tx.send(UiEvent::Menu(card)).await;
            }
            TuiCommand::ShowSessions => match s.sessions_menu() {
                Ok(t) => {
                    let _ = ui_tx.send(UiEvent::Menu(t)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Resume(arg) => match s.handle_resume(arg.as_deref()) {
                Ok(ResumeOutcome::Menu(card)) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Ok(ResumeOutcome::Switched(data)) => {
                    let _ = ui_tx
                        .send(UiEvent::SessionResumed {
                            session_id: data.session_id,
                            title: data.title,
                            provider: data.provider,
                            model: data.model,
                            input_tokens: data.input_tokens,
                            output_tokens: data.output_tokens,
                            context_used: data.context_used,
                            context_limit: data.context_limit,
                            preview_lines: data.preview_lines,
                            total_messages: data.total_messages,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::Shutdown => {
                // Perbaikan 2 (v0.5.9): majukan status pantauan aktif di folder ini saat sesi TUI ditutup bersih
                // (biarkan laporan terminal `delivered=false` bila baru selesai pas penutupan agar muncul di sesi berikut).
                let _ = s.poll_cli_watches_on_close(false).await;
                // Jalankan review latar akhir sesi bila substantif (disimpan agar tampil di sesi berikutnya).
                if let Some(job) = s.prepare_bg_review_job(true) {
                    let _ = tokio::time::timeout(Duration::from_secs(10), job.run(false)).await;
                }
                break;
            }
        }
    }
}

pub(crate) async fn interactive_tui(
    cli: Cli,
    resume: bool,
    resume_target: Option<String>,
) -> Result<()> {
    // Approver berbasis channel: permintaan approval dari tool mengalir ke
    // popup TUI, jawaban kembali via oneshot (lihat security::Approver).
    // Dibangun di dalam Session::build agar `agent.approval = "auto"` dari
    // config ikut berlaku di mode TUI.
    let (approval_tx, approval_rx) =
        tokio::sync::mpsc::channel::<ApprovalRequest>(8);
    let (ask_tx, ask_rx) =
        tokio::sync::mpsc::channel::<selencode::tools::ask::AskUserRequest>(4);
    let mut s = Session::build_with_target(
        cli.provider.as_deref(),
        cli.yes,
        resume,
        resume_target.as_deref(),
        None,
        Some(approval_tx),
        true,
    )
    .await?;
    s.ctx.asker = Arc::new(selencode::tools::ask::UserAsker::channel(ask_tx));
    let (init_in, init_out) = s.store.total_usage(&s.session_id).unwrap_or((0, 0));
    let mut app = App::new(
        s.ctx.cwd.to_string_lossy().to_string(),
        s.provider_name.clone(),
        s.pcfg.model.clone(),
        s.cfg.agent.max_steps,
        init_in,
        init_out,
    );
    // Sambutan Moonlit (v0.1.2) digambar dari info ini pada frame pertama:
    // penuh untuk sesi baru, 1 baris ringkas untuk resume. Catatan resume
    // (judul/tanggal/jumlah pesan) tetap tampil sebagai baris info.
    app.provider_label = provider_label(&s.provider_name, &s.pcfg);
    app.tool_count = s.registry.names().len();
    app.session_id = s.session_id.clone();
    app.context_limit = s.ctx.context_limit_tokens;
    app.context_used = estimate_tokens(&s.messages);
    app.context_pct = calc_context_pct(app.context_used, app.context_limit);
    app.approval_policy = Some(s.ctx.approver.policy());
    app.resumed = s.resumed_note.is_some();
    let cwd_str = s.ctx.cwd.to_string_lossy().to_string();
    app.prompt_history = s.store.load_prompt_history(&cwd_str, 100).unwrap_or_default();
    let scan_cwd = s.ctx.cwd.clone();
    app.plan_next = cli.plan;
    if let Some(note) = &s.resumed_note {
        app.info(note.clone());
    }
    for notice in &s.cfg.startup_notices {
        app.info(notice.clone());
    }
    // Perbaikan 2 (v0.5.9): jalankan 1 putaran polling pantauan aktif di folder ini + tampilkan bila berubah/selesai.
    for rep in s.poll_cli_watches_on_open().await {
        app.info(rep);
    }
    if let Ok(pending_reports) = s.store.take_undelivered_review_reports(&cwd_str) {
        for rep in pending_reports {
            app.info(rep);
        }
    }
    if cli.plan {
        app.info("Mode plan aktif untuk giliran pertama (--plan).");
    }
    if s.needs_setup {
        app.setup_wizard = Some(SetupWizardState::new(Some(
            "Belum ada provider/API key yang siap — ayo isi provider pertamamu di sini!"
                .to_string(),
        )));
    }

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(32);
    let (ui_tx, ui_rx) = tokio::sync::mpsc::channel::<UiEvent>(256);
    // Bagian A: pindai daftar `@file` secara non-blocking di thread latar agar cold start TUI < 300ms!
    let ui_tx_scan = ui_tx.clone();
    tokio::spawn(async move {
        if let Ok(files) =
            tokio::task::spawn_blocking(move || collect_workspace_files(&scan_cwd, 1500)).await
        {
            let _ = ui_tx_scan.send(UiEvent::WorkspaceFilesLoaded(files)).await;
        }
    });
    let cancel = Arc::new(tokio::sync::Notify::new());
    let interrupt_pending = Arc::new(AtomicBool::new(false));
    let worker = tokio::spawn(tui_worker(
        s,
        cmd_rx,
        ui_tx,
        cancel.clone(),
        interrupt_pending.clone(),
    ));
    let channels = TuiChannels {
        cmd_tx,
        event_rx: ui_rx,
        approval_rx,
        ask_rx,
        cancel,
        interrupt_pending,
    };
    let tui_result =
        tokio::task::spawn_blocking(move || selencode::tui::run_blocking(app, channels)).await;
    // run_blocking mengirim TuiCommand::Shutdown sebelum kembali; beri worker
    // waktu menyelesaikan giliran / background review akhir sesi bila aktif.
    let _ = tokio::time::timeout(Duration::from_secs(12), worker).await;
    match tui_result {
        Ok(r) => r,
        Err(e) => anyhow::bail!("task TUI panik: {e}"),
    }
}
