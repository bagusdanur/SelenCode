use super::cli::Cli;
use super::session::Session;
use anyhow::Result;
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::{run_turn, run_turn_mode};
use selencode::repl::{
    help_menu_markup, parse_model_action, parse_slash_command, strip_menu_markup, ModelAction,
    SlashCommand,
};
use selencode::tui::app::{SetupResult, SETUP_PRESETS};
use std::io::Write;
fn run_plain_setup_wizard(s: &mut Session) -> Result<()> {
    let stdin = std::io::stdin();
    let prompt_line = |label: &str, default_val: &str| -> Result<String> {
        if default_val.is_empty() {
            print!("{label}: ");
        } else {
            print!("{label} [{default_val}]: ");
        }
        std::io::stdout().flush()?;
        let mut buf = String::new();
        stdin.read_line(&mut buf)?;
        let v = buf.trim();
        Ok(if v.is_empty() {
            default_val.to_string()
        } else {
            v.to_string()
        })
    };

    println!("\n=== ☾ Setup Provider SelenCode ===");
    for (i, p) in SETUP_PRESETS.iter().enumerate() {
        println!("  {}. {} — {}", i + 1, p.label, p.desc);
    }
    let pick_str = prompt_line("Pilih jenis API (1-8)", "1")?;
    let idx = pick_str
        .parse::<usize>()
        .ok()
        .filter(|&n| n >= 1 && n <= SETUP_PRESETS.len())
        .map(|n| n - 1)
        .unwrap_or(0);
    let preset = SETUP_PRESETS[idx];

    let name = prompt_line("Nama provider", preset.default_name)?;
    let base_url = if preset.kind == "openai" {
        let u = prompt_line("Base URL (https://.../v1)", preset.default_url)?;
        if u.trim().is_empty() {
            anyhow::bail!("Base URL wajib diisi untuk OpenAI-compatible");
        }
        Some(u)
    } else {
        None
    };
    let key_default = if preset.needs_key { "" } else { "ollama" };
    let api_key_or_env = prompt_line("API Key (atau nama ENV var)", key_default)?;
    if preset.needs_key && api_key_or_env.trim().is_empty() {
        anyhow::bail!("API key wajib diisi");
    }
    let model = prompt_line("ID Model utama", preset.default_model)?;
    if model.trim().is_empty() {
        anyhow::bail!("ID Model wajib diisi");
    }
    let card = s.apply_setup_result(SetupResult {
        name,
        kind: preset.kind.to_string(),
        base_url,
        api_key_or_env,
        model,
    })?;
    println!("{}", strip_menu_markup(&card));
    Ok(())
}

pub(crate) async fn interactive_plain(
    cli: Cli,
    resume: bool,
    resume_target: Option<String>,
) -> Result<()> {
    let mut s = Session::build_with_target(
        cli.provider.as_deref(),
        cli.yes,
        resume,
        resume_target.as_deref(),
        None,
        None,
        true,
    )
    .await?;
    // Mode CLI polos interaktif (TTY): `ask_user` membaca pilihan bernomor/teks bebas dari stdin;
    // bila dijalankan lewat pipe (non-TTY), tetap non-interaktif agar tidak menggantung.
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        s.ctx.asker = std::sync::Arc::new(selencode::tools::ask::UserAsker::stdin());
    }
    if s.needs_setup {
        println!(
            "☾ Provider belum siap ({}). Mulai setup interaktif…",
            s.setup_reason.as_deref().unwrap_or("belum dikonfigurasi")
        );
        if let Err(e) = run_plain_setup_wizard(&mut s) {
            eprintln!("[setup] {e} — kamu bisa ulangi kapan saja lewat `/setup`.");
        }
    }
    if let Some(note) = &s.resumed_note {
        println!("{note}");
    }
    println!(
        "{}",
        selencode::tui::moonlit::plain_banner(&s.provider_name, &s.pcfg.model)
    );
    // Tampilkan laporan Background Self-Improvement Review tertunda dari sesi sebelumnya (v0.5.1).
    let cwd_str_init = s.ctx.cwd.to_string_lossy().to_string();
    if let Ok(pending_reports) = s.store.take_undelivered_review_reports(&cwd_str_init) {
        for rep in pending_reports {
            println!("{rep}");
        }
    }
    let mut plan_next = cli.plan;
    if plan_next {
        println!("Mode plan aktif untuk giliran pertama (--plan).");
    }
    let stdin = std::io::stdin();
    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_slash_command(line) {
            SlashCommand::Exit => break,
            SlashCommand::Model(None) => s.print_models(),
            SlashCommand::Model(Some(arg)) => {
                if let ModelAction::FetchList(filter) = parse_model_action(&arg) {
                    match s.fetch_remote_models_menu(filter.as_deref()).await {
                        Ok(card) => println!("{}", strip_menu_markup(&card)),
                        Err(e) => eprintln!("[error] {e}"),
                    }
                } else {
                    match s.apply_model_arg(&arg) {
                        Ok(card) => println!("{}", strip_menu_markup(&card)),
                        Err(e) => eprintln!("[error] {e}"),
                    }
                }
            }
            SlashCommand::Models(filter) => {
                match s.fetch_remote_models_menu(filter.as_deref()).await {
                    Ok(card) => println!("{}", strip_menu_markup(&card)),
                    Err(e) => eprintln!("[error] {e}"),
                }
            }
            SlashCommand::Provider(arg) => match s.handle_provider(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Setup => {
                if let Err(e) = run_plain_setup_wizard(&mut s) {
                    eprintln!("[error] {e}");
                }
            }
            SlashCommand::Telegram(arg) => match s.handle_telegram(arg.as_deref()).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::InlineShell(cmd) => match s.handle_inline_shell(&cmd).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Undo => match s.handle_undo() {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Redo => match s.handle_redo() {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Checkpoint(arg) => match s.handle_checkpoint(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Rollback(arg) => match s.handle_rollback(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Worktree(arg) => match s.handle_worktree(arg.as_deref()).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Mcp => {
                println!("{}", strip_menu_markup(&s.handle_mcp_status()));
            }
            SlashCommand::Doctor => {
                println!("{}", strip_menu_markup(&s.handle_doctor()));
            }
            SlashCommand::Verify(arg) => match s.handle_verify(arg.as_deref()).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Index(arg) => match s.handle_index(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Jobs(arg) => match s.handle_jobs(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Route(arg) => match s.handle_route(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Lsp(arg) => match s.handle_lsp(arg.as_deref()).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Task(arg) => match s.handle_task(arg.as_deref()) {
                Ok((card, resume_prompt)) => {
                    println!("{}", strip_menu_markup(&card));
                    if let Some(prompt) = resume_prompt {
                        match run_turn(
                            s.provider.clone(),
                            &s.registry,
                            &s.ctx,
                            &mut s.messages,
                            &prompt,
                            s.cfg.agent.max_steps,
                        )
                        .await
                        {
                            Ok(_) => println!("[{}]", s.usage_line()),
                            Err(e) => eprintln!("[error] {e}"),
                        }
                    }
                }
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Memory(arg) => match s.handle_memory(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Skills(arg) => match s.handle_skills(arg.as_deref()).await {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Review(arg) => match s.handle_review(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Cron(arg) => match s.handle_cron(arg.as_deref(), None) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Search(arg) => match s.handle_search(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Skill(arg) => match s.handle_use_skill(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Learn(arg) => match s.handle_learn_skill(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Init => match s.handle_init() {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Export(arg) => match s.handle_export(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Approve(arg) => match s.handle_approve(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Sandbox(arg) => match s.handle_sandbox(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Details => {
                println!("Perintah /details hanya berlaku pada mode TUI.");
            }
            SlashCommand::New => {
                if let Some(job) = s.prepare_bg_review_job(true) {
                    tokio::spawn(async move {
                        if let Ok(Ok(Some(outcome))) = tokio::time::timeout(
                            std::time::Duration::from_secs(20),
                            job.run(true),
                        )
                        .await
                        {
                            if let Some(msg) = outcome.report_message {
                                eprintln!("\n{msg}");
                            }
                        }
                    });
                }
                match s.reset_session() {
                    Ok(sid) => println!("Sesi baru dimulai ({sid})."),
                    Err(e) => eprintln!("[error] {e}"),
                }
            }
            SlashCommand::Compact => {
                println!("{}", strip_menu_markup(&s.compact_now()));
            }
            SlashCommand::Stop => {
                plan_next = false;
                println!("Tidak ada task latar belakang yang berjalan (mode polos sinkron).");
            }
            SlashCommand::Sessions => {
                if let Err(e) = s.print_sessions() {
                    eprintln!("[error] {e}");
                }
            }
            SlashCommand::Resume(arg) => match s.handle_resume(arg.as_deref()) {
                Ok(super::commands::ResumeOutcome::Menu(card)) => {
                    println!("{}", strip_menu_markup(&card));
                }
                Ok(super::commands::ResumeOutcome::Switched(data)) => {
                    println!(
                        "Melanjutkan sesi: \"{}\" ({}) — {} pesan dimuat.",
                        data.title,
                        data.session_id.chars().take(10).collect::<String>(),
                        data.total_messages
                    );
                    for (role, text) in data.preview_lines {
                        let prefix = if role == "user" { ">" } else { "☾" };
                        println!("  {prefix} {text}");
                    }
                }
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Plan => {
                plan_next = true;
                println!("Mode plan aktif untuk giliran berikutnya (baca-saja, diakhiri rencana).");
            }
            SlashCommand::Help => {
                println!("{}", strip_menu_markup(&help_menu_markup()));
            }
            SlashCommand::Unknown(name) => {
                eprintln!(
                    "Perintah /{name} tidak dikenal. Ketik /help untuk daftar semua perintah."
                );
            }
            SlashCommand::NotCommand => {
                if plan_next {
                    plan_next = false;
                    match run_turn_mode(
                        s.provider.clone(),
                        &s.registry,
                        &s.ctx,
                        &mut s.messages,
                        line,
                        s.cfg.agent.max_steps,
                        TurnMode::Plan,
                    )
                    .await
                    {
                        Ok(text) => {
                            println!("[{}]", s.usage_line());
                            match extract_plan(&text) {
                                Some(plan) => {
                                    print!("\nJalankan rencana ini? [y/N] ");
                                    std::io::stdout().flush()?;
                                    let mut ans = String::new();
                                    let _ = stdin.read_line(&mut ans);
                                    if matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
                                        let exec_prompt = approved_plan_prompt(&plan);
                                        match run_turn(
                                            s.provider.clone(),
                                            &s.registry,
                                            &s.ctx,
                                            &mut s.messages,
                                            &exec_prompt,
                                            s.cfg.agent.max_steps,
                                        )
                                        .await
                                        {
                                            Ok(_) => println!("[{}]", s.usage_line()),
                                            Err(e) => eprintln!("[error] {e}"),
                                        }
                                    } else {
                                        println!("Rencana dibuang.");
                                    }
                                }
                                None => println!("(giliran plan selesai tanpa teks rencana)"),
                            }
                        }
                        Err(e) => eprintln!("[error] {e}"),
                    }
                    continue;
                }
                match run_turn(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    line,
                    s.cfg.agent.max_steps,
                )
                .await
                {
                    Ok(_) => {
                        println!("[{}]", s.usage_line());
                        s.turns_since_review += 1;
                        if let Some(job) = s.prepare_bg_review_job(false) {
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(20),
                                    job.run(false),
                                )
                                .await;
                            });
                        }
                    }
                    Err(e) => eprintln!("[error] {e}"),
                }
            }
        }
    }
    // Akhir sesi REPL polos (`/exit` atau EOF): jalankan background review bila substantif.
    if let Some(job) = s.prepare_bg_review_job(true) {
        if let Ok(Ok(Some(outcome))) =
            tokio::time::timeout(std::time::Duration::from_secs(15), job.run(true)).await
        {
            if let Some(msg) = outcome.report_message {
                println!("\n{msg}");
            }
        }
    }
    Ok(())
}
