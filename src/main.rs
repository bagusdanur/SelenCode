//! Entry point SelenCode (`selen`).
//!
//! Bootstrap tipis yang mendelegasikan seluruh logika domain ke modul `src/app/`:
//! - `app::cli`             — definisi argumen CLI (`clap`)
//! - `app::session`         — lifecycle `Session`, provider, & manajemen model
//! - `app::commands`        — handler perintah `/` operasional
//! - `app::tui_runner`      — worker & antarmuka Ratatui Moonlit
//! - `app::plain_runner`    — REPL polos & wizard setup teks
//! - `app::telegram_runner` — Telegram Bot Gateway (`selen telegram`)

mod app;

use anyhow::Result;
use app::cli::{Cli, Commands, CronCliAction, SkillsCliAction, WatchCliAction};
use app::plain_runner::interactive_plain;
use app::session::Session;
use app::telegram_runner::run_telegram_gateway;
use app::tui_runner::interactive_tui;
use clap::Parser;
use selencode::agent::plan::TurnMode;
use selencode::agent::{run_turn, run_turn_mode};
use std::io::IsTerminal;

#[tokio::main]
async fn main() -> Result<()> {
    selencode::update::cleanup_old_binary();
    let mut cli = Cli::parse();
    match cli.command.take() {
        Some(Commands::Run { prompt }) => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt kosong. Contoh: selen run \"jelaskan project ini\"");
            }
            let mut s = Session::build(
                cli.provider.as_deref(),
                cli.yes,
                false,
                Some(&prompt),
                None,
                false,
            )
            .await?;
            // Perbaikan 2 (v0.5.9): cek 1 putaran pantauan aktif milik folder ini saat sesi CLI dibuka.
            for rep in s.poll_cli_watches_on_open().await {
                println!("{rep}");
            }
            if cli.plan {
                // run --plan: 1 giliran plan, lalu berhenti (tanpa eksekusi).
                run_turn_mode(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    0,
                    TurnMode::Plan,
                )
                .await?;
            } else {
                run_turn(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    0,
                )
                .await?;
                // Perbaikan 2 (v0.5.9): 1 putaran polling sebelum sesi CLI ditutup bersih bila ada watch aktif.
                for rep in s.poll_cli_watches_on_close(true).await {
                    println!("{rep}");
                }
                // Background Self-Improvement Review (v0.5.1): jalankan pasca-sesi selesai
                // hanya bila `[aux]` review/compression model terpasang & sesi substantif.
                if let Some(job) = s.prepare_bg_review_job(true) {
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
                }
            }
            Ok(())
        }
        Some(Commands::Resume { target }) => interactive(cli, true, true, target).await,
        Some(Commands::Telegram) => run_telegram_gateway(cli).await,
        Some(Commands::Bench {
            suite,
            filter,
            out,
            history,
        }) => {
            app::bench_runner::run_bench_cli(
                cli.provider.as_deref(),
                suite.as_deref(),
                filter.as_deref(),
                out.as_deref(),
                history,
            )
            .await
        }
        Some(Commands::Doctor) => {
            let s = Session::build(cli.provider.as_deref(), cli.yes, false, None, None, true).await?;
            println!(
                "{}",
                selencode::repl::strip_menu_markup(&s.handle_doctor())
            );
            Ok(())
        }
        Some(Commands::Review { args }) => {
            let mut s =
                Session::build(cli.provider.as_deref(), cli.yes, false, None, None, true).await?;
            let joined = args.join(" ");
            let arg = (!joined.trim().is_empty()).then_some(joined.as_str());
            let card = s.handle_review(arg)?;
            println!("{}", selencode::repl::strip_menu_markup(&card));
            Ok(())
        }
        Some(Commands::Cron { action }) => {
            let cfg = selencode::config::load_or_empty()?.0;
            let store = selencode::store::Store::open()?;
            match action {
                CronCliAction::Add {
                    name,
                    schedule,
                    prompt,
                    cwd,
                    allow_shell,
                    allow_write,
                    chat_id,
                } => {
                    let parsed = selencode::cron::parse_schedule(&schedule)?;
                    let spec = parsed.canonical_spec();
                    let next = selencode::cron::compute_next_run_after(&spec, chrono::Utc::now())?;
                    let cwd_dir = cwd.unwrap_or_else(|| {
                        std::env::current_dir()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| ".".to_string())
                    });
                    let target_chat = chat_id
                        .or_else(|| cfg.telegram.allowed_users.first().copied())
                        .unwrap_or(0);
                    let job = store.add_cron_job(
                        &name,
                        &spec,
                        &prompt,
                        &cwd_dir,
                        &allow_shell,
                        allow_write,
                        target_chat,
                        &next,
                    )?;
                    println!(
                        "Job terjadwal #{} '{}' ditambahkan ({}). Jadwal berikut: {}\n{}",
                        job.id,
                        job.name,
                        selencode::cron::format_schedule_human(&job.schedule_spec),
                        job.next_run_at,
                        selencode::cron::CRON_GATEWAY_NOTE
                    );
                }
                CronCliAction::List => {
                    let s = Session::build(cli.provider.as_deref(), cli.yes, false, None, None, true)
                        .await?;
                    let card = s.handle_cron(Some("list"), None)?;
                    println!("{}", selencode::repl::strip_menu_markup(&card));
                }
                CronCliAction::Remove { target } => {
                    if store.remove_cron_job(&target)? {
                        println!("Job '{target}' berhasil dihapus.");
                    } else {
                        anyhow::bail!("job '{target}' tidak ditemukan");
                    }
                }
                CronCliAction::Pause { target } => {
                    if let Some(j) = store.set_cron_job_enabled(&target, false, None)? {
                        println!("Job '{}' dijeda.", j.name);
                    } else {
                        anyhow::bail!("job '{target}' tidak ditemukan");
                    }
                }
                CronCliAction::Resume { target } => {
                    let Some(ex) = store.get_cron_job(&target)? else {
                        anyhow::bail!("job '{target}' tidak ditemukan");
                    };
                    let next = selencode::cron::compute_next_run_after(
                        &ex.schedule_spec,
                        chrono::Utc::now(),
                    )?;
                    if let Some(j) = store.set_cron_job_enabled(&target, true, Some(&next))? {
                        println!("Job '{}' diaktifkan kembali (berikut: {}).", j.name, j.next_run_at);
                    }
                }
            }
            Ok(())
        }
        Some(Commands::Search { query }) => {
            let s =
                Session::build(cli.provider.as_deref(), cli.yes, false, None, None, true).await?;
            let q = query.join(" ");
            let arg = (!q.trim().is_empty()).then_some(q.as_str());
            let card = s.handle_search(arg)?;
            println!("{}", selencode::repl::strip_menu_markup(&card));
            Ok(())
        }
        Some(Commands::Watch { action }) => {
            let s =
                Session::build(cli.provider.as_deref(), cli.yes, false, None, None, true).await?;
            let arg = match action {
                WatchCliAction::List => Some("list".to_string()),
                WatchCliAction::Cancel { id } => Some(format!("cancel {id}")),
            };
            let card = s.handle_watch(arg.as_deref(), None)?;
            println!("{}", selencode::repl::strip_menu_markup(&card));
            Ok(())
        }
        Some(Commands::Skills { action }) => {
            match action {
                SkillsCliAction::Install { url, rename } => {
                    let staged = selencode::skill_installer::stage_skill_from_source(&url).await?;
                    let summary = selencode::skill_installer::format_install_approval_summary(
                        &staged,
                        rename.as_deref(),
                    );
                    println!("{summary}");
                    let approved = if cli.yes {
                        true
                    } else {
                        use std::io::Write;
                        print!("Pasang skill ini ke direktori pengguna? [y/N]: ");
                        std::io::stdout().flush()?;
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "ya" | "yes")
                    };
                    if !approved {
                        println!("Pemasangan dibatalkan — tidak ada berkas yang ditulis.");
                        return Ok(());
                    }
                    let user_root = selencode::skills::user_skills_dir()
                        .ok_or_else(|| anyhow::anyhow!("direktori skill pengguna tidak tersedia"))?;
                    let rep = selencode::skill_installer::finalize_staged_skill_install(
                        &staged,
                        &user_root,
                        rename.as_deref(),
                    )?;
                    println!(
                        "Skill '{}' berhasil dipasang ke {} ({} berkas, {} skrip; sumber dicatat di {}).",
                        rep.name,
                        rep.target_dir.display(),
                        rep.files_copied,
                        rep.scripts_count,
                        rep.provenance_path.display()
                    );
                }
                SkillsCliAction::Remove { name } => {
                    let user_root = selencode::skills::user_skills_dir()
                        .ok_or_else(|| anyhow::anyhow!("direktori skill pengguna tidak tersedia"))?;
                    let cwd = std::env::current_dir()?;
                    let msg = selencode::skill_installer::remove_installed_skill(
                        &name,
                        &user_root,
                        &cwd,
                    )?;
                    println!("{msg}");
                }
                SkillsCliAction::List => {
                    let mut s = Session::build(
                        cli.provider.as_deref(),
                        cli.yes,
                        false,
                        None,
                        None,
                        true,
                    )
                    .await?;
                    let card = s.handle_skills(None).await?;
                    println!("{}", selencode::repl::strip_menu_markup(&card));
                }
            }
            Ok(())
        }
        Some(Commands::Update) => selencode::update::run().await,
        None => {
            let resume = !cli.new;
            interactive(cli, resume, false, None).await
        }
    }
}

/// Mode interaktif: TUI bila stdin & stdout sama-sama TTY dan tidak ada
/// flag --plain; selain itu REPL polos (aman untuk pipa/script).
async fn interactive(
    cli: Cli,
    resume: bool,
    explicit_resume: bool,
    resume_target: Option<String>,
) -> Result<()> {
    let use_tui = !cli.plain && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if use_tui {
        interactive_tui(cli, resume, explicit_resume, resume_target).await
    } else {
        interactive_plain(cli, resume, explicit_resume, resume_target).await
    }
}
