use anyhow::Result;
use clap::{Parser, Subcommand};
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::{run_turn, run_turn_mode, run_turn_ui, run_turn_ui_mode, UiEvent};
use selencode::config::{self, Config, ProviderCfg};
use selencode::providers::anthropic::AnthropicProvider;
use selencode::providers::gemini::GeminiProvider;
use selencode::providers::openai::OpenAiProvider;
use selencode::providers::{Message, Provider};
use selencode::repl::{parse_slash_command, SlashCommand, HELP_TEXT};
use selencode::security::{ApprovalRequest, Approver};
use selencode::store::Store;
use selencode::tools::delegate::Delegate;
use selencode::tools::{default_registry_with_shell, read_only_registry, Ctx, Registry};
use selencode::tui::app::App;
use selencode::tui::{TuiChannels, TuiCommand};
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "selen", version, about = "SelenCode — terminal AI coding agent (full Rust), persona Selena")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Auto-approve semua tool yang butuh persetujuan (mode interaktif)
    #[arg(long, global = true)]
    yes: bool,
    /// Nama provider di config (default: default_provider)
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Paksa mode polos (tanpa TUI) walau di terminal interaktif
    #[arg(long, global = true)]
    plain: bool,
    /// Plan mode (Fase 6): giliran pertama baca-saja & diakhiri rencana;
    /// untuk `selen run --plan` berhenti sesudah rencana (tanpa eksekusi)
    #[arg(long, global = true)]
    plan: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Sekali jalan: jalankan 1 perintah lalu keluar
    Run {
        /// Perintah / prompt untuk agent
        prompt: Vec<String>,
    },
    /// Lanjutkan sesi terakhir untuk folder ini (mode interaktif)
    Resume,
    /// Perbarui selen ke rilis terbaru dari GitHub Releases
    Update,
}

/// Label tampil provider untuk sambutan: nama + host base_url bila ada,
/// mis. "router (router.ryukomik.com)".
fn provider_label(name: &str, pcfg: &ProviderCfg) -> String {
    match &pcfg.base_url {
        Some(u) => {
            let rest = u
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            let host = rest.split('/').next().unwrap_or(rest);
            if host.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({host})")
            }
        }
        None => name.to_string(),
    }
}

/// Buat instance provider dari config sesuai kind:
/// openai (base_url wajib) | anthropic | gemini (base_url default resmi).
fn make_provider(
    cfg: &Config,
    name: &str,
    pcfg: &ProviderCfg,
    api_key: &str,
) -> Result<Arc<dyn Provider>> {
    let max_tokens = cfg.agent.max_output_tokens;
    match pcfg.kind.as_str() {
        "openai" => {
            let base_url = pcfg
                .base_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("base_url wajib untuk provider '{name}' kind=openai"))?;
            Ok(Arc::new(OpenAiProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        "anthropic" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::anthropic::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(AnthropicProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        "gemini" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::gemini::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(GeminiProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        other => anyhow::bail!(
            "provider kind '{other}' tidak dikenal (pilihan: openai, anthropic, gemini)"
        ),
    }
}

struct Session {
    cfg: Config,
    provider_name: String,
    pcfg: ProviderCfg,
    provider: Arc<dyn Provider>,
    ctx: Ctx,
    registry: Registry,
    store: Arc<Store>,
    session_id: String,
    messages: Vec<Message>,
    /// Catatan "Melanjutkan sesi: ..." bila resume memuat sesi lama.
    resumed_note: Option<String>,
}

impl Session {
    /// Judul sesi: potongan prompt pertama (maks 60 karakter).
    fn title_from(prompt: &str) -> String {
        let t: String = prompt.chars().take(60).collect();
        if t.is_empty() {
            "(sesi baru)".to_string()
        } else {
            t
        }
    }

    async fn build(
        provider_name: Option<&str>,
        yes: bool,
        resume: bool,
        first_prompt: Option<&str>,
        approval_tx: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
    ) -> Result<Session> {
        let cfg = config::load()?;
        let (name, pcfg, api_key) = cfg.resolve_provider(provider_name)?;
        let provider = make_provider(&cfg, &name, &pcfg, &api_key)?;
        let cwd = std::env::current_dir()?;
        let cwd_str = cwd.to_string_lossy().to_string();
        let store = Arc::new(Store::open()?);

        let (session_id, messages, resumed_title) = if resume {
            match store.latest_session(&cwd_str)? {
                Some(info) => {
                    let msgs = store.load_messages(&info.id)?;
                    (info.id, msgs, Some((info.title, info.created_at)))
                }
                None => {
                    let title = Self::title_from(first_prompt.unwrap_or(""));
                    let id = store.create_session(&cwd_str, &name, &pcfg.model, &title)?;
                    (id, Vec::new(), None)
                }
            }
        } else {
            let title = Self::title_from(first_prompt.unwrap_or(""));
            let id = store.create_session(&cwd_str, &name, &pcfg.model, &title)?;
            (id, Vec::new(), None)
        };
        let resumed_note = resumed_title.map(|(title, created)| {
            format!(
                "Melanjutkan sesi: \"{title}\" ({created}) — {} pesan dimuat.",
                messages.len()
            )
        });

        let auto_yes = cfg.auto_approve(yes);
        let mut ctx = Ctx::new(cwd, auto_yes)
            .with_context_limit(cfg.agent.context_limit_tokens)
            .with_store(store.clone(), session_id.clone());
        if let Some(tx) = approval_tx {
            ctx = ctx.with_approver(Arc::new(Approver::channel(auto_yes, tx)));
        }
        // Delegate (Fase 6): subagent memakai provider sesi saat build;
        // ganti provider via /model TIDAK mengganti provider subagent
        // (dicatat di HASIL-FASE-6). Registry anak selalu baca-saja.
        let mut registry = default_registry_with_shell(&cfg.shell).register(Arc::new(
            Delegate::new(
                provider.clone(),
                Arc::new(read_only_registry()),
                cfg.agent.max_parallel_subagents,
            ),
        ));
        // Tool MCP (Fase 7): HANYA registry induk mode Execute — tidak ikut
        // registry baca-saja plan/subagent. Server yang gagal start dilewati
        // dengan peringatan stderr; build_mcp_tools tidak mengembalikan Err,
        // jadi start selen tidak pernah digagalkan oleh server MCP.
        for tool in selencode::mcp::build_mcp_tools(&cfg).await {
            registry = registry.register(tool);
        }
        Ok(Session {
            cfg,
            provider_name: name,
            pcfg,
            provider,
            ctx,
            registry,
            store,
            session_id,
            messages,
            resumed_note,
        })
    }

    /// Ganti provider aktif (perintah /model <nama>).
    fn switch_provider(&mut self, name: &str) -> Result<()> {
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(name))?;
        let provider = make_provider(&self.cfg, name, &pcfg, &api_key)?;
        self.provider = provider;
        self.provider_name = name.to_string();
        self.pcfg = pcfg;
        self.store
            .update_session_provider(&self.session_id, name, &self.pcfg.model)?;
        Ok(())
    }

    /// Ringkasan token sesi + estimasi biaya bila harga dikonfigurasi.
    fn usage_line(&self) -> String {
        match self.store.total_usage(&self.session_id) {
            Ok((i, o)) => {
                let mut s = format!("token sesi ini: {i} in / {o} out");
                if let (Some(pin), Some(pout)) = (self.pcfg.price_in, self.pcfg.price_out) {
                    let cost = (i as f64 / 1_000_000.0) * pin + (o as f64 / 1_000_000.0) * pout;
                    s.push_str(&format!(" · estimasi biaya ${cost:.4}"));
                }
                s
            }
            Err(e) => format!("(gagal membaca usage: {e})"),
        }
    }

    /// Teks daftar provider (dipakai mode polos & TUI — 1 sumber kebenaran).
    fn models_text(&self) -> String {
        let mut out = format!(
            "Provider aktif: {} (kind {}, model {})\nProvider tersedia di config:",
            self.provider_name, self.pcfg.kind, self.pcfg.model
        );
        let mut names: Vec<&String> = self.cfg.providers.keys().collect();
        names.sort();
        for n in names {
            let p = &self.cfg.providers[n];
            let mark = if *n == self.provider_name { " *" } else { "" };
            out.push_str(&format!("\n  - {n}: kind {}, model {}{mark}", p.kind, p.model));
        }
        out
    }

    fn print_models(&self) {
        println!("{}", self.models_text());
    }

    /// Teks daftar sesi terakhir cwd ini (mode polos & TUI).
    fn sessions_text(&self) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let list = self.store.list_sessions(&cwd_str, 10)?;
        if list.is_empty() {
            return Ok("(belum ada sesi tersimpan untuk folder ini)".to_string());
        }
        let mut out = "Sesi terakhir di folder ini:".to_string();
        for s in list {
            let mark = if s.id == self.session_id { " *" } else { "" };
            out.push_str(&format!(
                "\n  - {} · {}/{} · \"{}\"{}",
                s.created_at, s.provider, s.model, s.title, mark
            ));
        }
        Ok(out)
    }

    fn print_sessions(&self) -> Result<()> {
        println!("{}", self.sessions_text()?);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Run { prompt }) => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt kosong. Contoh: selen run \"jelaskan project ini\"");
            }
            let mut s =
                Session::build(cli.provider.as_deref(), cli.yes, false, Some(&prompt), None).await?;
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
        Some(Commands::Resume) => interactive(cli, true).await,
        Some(Commands::Update) => selencode::update::run().await,
        None => interactive(cli, false).await,
    }
}

/// Mode interaktif: TUI bila stdin & stdout sama-sama TTY dan tidak ada
/// flag --plain; selain itu REPL polos (perilaku Fase 0-3, aman untuk pipa).
async fn interactive(cli: Cli, resume: bool) -> Result<()> {
    let use_tui = !cli.plain && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if use_tui {
        interactive_tui(cli, resume).await
    } else {
        interactive_plain(cli, resume).await
    }
}

/// Worker TUI: memiliki Session, menerima TuiCommand, mengirim UiEvent.
/// Loop agent berjalan di task ini; UI (thread blocking) hanya menggambar.
async fn tui_worker(
    mut s: Session,
    mut cmd_rx: tokio::sync::mpsc::Receiver<TuiCommand>,
    ui_tx: tokio::sync::mpsc::Sender<UiEvent>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            TuiCommand::Prompt(prompt) => {
                let r = run_turn_ui(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    s.cfg.agent.max_steps,
                    ui_tx.clone(),
                )
                .await;
                match r {
                    Ok(_) => {
                        let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                        // Pastikan UI lepas dari keadaan sibuk.
                        let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                    }
                }
            }
            TuiCommand::PromptPlan(prompt) => {
                let r = run_turn_ui_mode(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    s.cfg.agent.max_steps,
                    ui_tx.clone(),
                    TurnMode::Plan,
                )
                .await;
                match r {
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
                }
            }
            TuiCommand::ExecutePlan(plan) => {
                let prompt = approved_plan_prompt(&plan);
                let r = run_turn_ui(
                    s.provider.clone(),
                    &s.registry,
                    &s.ctx,
                    &mut s.messages,
                    &prompt,
                    s.cfg.agent.max_steps,
                    ui_tx.clone(),
                )
                .await;
                match r {
                    Ok(_) => {
                        let _ = ui_tx.send(UiEvent::Info(s.usage_line())).await;
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                        let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                    }
                }
            }
            TuiCommand::ShowModels => {
                let _ = ui_tx.send(UiEvent::Info(s.models_text())).await;
            }
            TuiCommand::SwitchModel(name) => match s.switch_provider(&name) {
                Ok(()) => {
                    let _ = ui_tx
                        .send(UiEvent::ModelChanged {
                            provider: s.provider_name.clone(),
                            model: s.pcfg.model.clone(),
                        })
                        .await;
                    let _ = ui_tx
                        .send(UiEvent::Info(format!(
                            "Provider diganti ke {} (model {}). {}",
                            s.provider_name,
                            s.pcfg.model,
                            s.usage_line()
                        )))
                        .await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::ShowSessions => match s.sessions_text() {
                Ok(t) => {
                    let _ = ui_tx.send(UiEvent::Info(t)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::ShowSessionInfo => {
                let _ = ui_tx
                    .send(UiEvent::Info(format!(
                        "Sesi aktif: {} · provider {} · {}",
                        s.session_id,
                        s.provider_name,
                        s.usage_line()
                    )))
                    .await;
            }
            TuiCommand::Shutdown => break,
        }
    }
}

async fn interactive_tui(cli: Cli, resume: bool) -> Result<()> {
    // Approver berbasis channel: permintaan approval dari tool mengalir ke
    // popup TUI, jawaban kembali via oneshot (lihat security::Approver).
    // Dibangun di dalam Session::build agar `agent.approval = "auto"` dari
    // config ikut berlaku di mode TUI.
    let (approval_tx, approval_rx) =
        tokio::sync::mpsc::channel::<ApprovalRequest>(8);
    let s =
        Session::build(cli.provider.as_deref(), cli.yes, resume, None, Some(approval_tx)).await?;
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
    app.resumed = s.resumed_note.is_some();
    app.plan_next = cli.plan;
    if let Some(note) = &s.resumed_note {
        app.info(note.clone());
    }
    if cli.plan {
        app.info("Mode plan aktif untuk giliran pertama (--plan).");
    }

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(32);
    let (ui_tx, ui_rx) = tokio::sync::mpsc::channel::<UiEvent>(256);
    let worker = tokio::spawn(tui_worker(s, cmd_rx, ui_tx));
    let channels = TuiChannels {
        cmd_tx,
        event_rx: ui_rx,
        approval_rx,
    };
    let tui_result =
        tokio::task::spawn_blocking(move || selencode::tui::run_blocking(app, channels)).await;
    // run_blocking mengirim TuiCommand::Shutdown sebelum kembali; beri worker
    // waktu singkat menyelesaikan giliran yang mungkin masih berjalan.
    let _ = tokio::time::timeout(Duration::from_secs(2), worker).await;
    match tui_result {
        Ok(r) => r,
        Err(e) => anyhow::bail!("task TUI panik: {e}"),
    }
}

async fn interactive_plain(cli: Cli, resume: bool) -> Result<()> {
    let mut s = Session::build(cli.provider.as_deref(), cli.yes, resume, None, None).await?;
    if let Some(note) = &s.resumed_note {
        println!("{note}");
    }
    println!(
        "{}",
        selencode::tui::moonlit::plain_banner(&s.provider_name, &s.pcfg.model)
    );
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
            SlashCommand::Model(Some(name)) => match s.switch_provider(&name) {
                Ok(()) => println!(
                    "Provider diganti ke {} (model {}). {}",
                    s.provider_name,
                    s.pcfg.model,
                    s.usage_line()
                ),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Sessions => {
                if let Err(e) = s.print_sessions() {
                    eprintln!("[error] {e}");
                }
            }
            SlashCommand::Resume => {
                println!(
                    "Sesi aktif: {} · provider {} · {}",
                    s.session_id,
                    s.provider_name,
                    s.usage_line()
                );
            }
            SlashCommand::Plan => {
                plan_next = true;
                println!("Mode plan aktif untuk giliran berikutnya (baca-saja, diakhiri rencana).");
            }
            SlashCommand::Help => {
                println!("{HELP_TEXT}");
            }
            SlashCommand::Unknown(name) => {
                eprintln!(
                    "Perintah /{name} tidak dikenal. Pilihan: /plan, /model, /sessions, /help, /exit"
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
                    Ok(_) => println!("[{}]", s.usage_line()),
                    Err(e) => eprintln!("[error] {e}"),
                }
            }
        }
    }
    Ok(())
}
