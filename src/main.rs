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
use app::cli::{Cli, Commands};
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
            if cli.plan {
                // run --plan: 1 giliran plan, lalu berhenti (tanpa eksekusi).
                run_turn_mode(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    s.cfg.agent.max_steps,
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
                    s.cfg.agent.max_steps,
                )
                .await?;
            }
            Ok(())
        }
        Some(Commands::Resume { target }) => interactive(cli, true, target).await,
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
        Some(Commands::Update) => selencode::update::run().await,
        None => interactive(cli, false, None).await,
    }
}

/// Mode interaktif: TUI bila stdin & stdout sama-sama TTY dan tidak ada
/// flag --plain; selain itu REPL polos (aman untuk pipa/script).
async fn interactive(cli: Cli, resume: bool, resume_target: Option<String>) -> Result<()> {
    let use_tui = !cli.plain && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if use_tui {
        interactive_tui(cli, resume, resume_target).await
    } else {
        interactive_plain(cli, resume, resume_target).await
    }
}
