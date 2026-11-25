use anyhow::Result;
use clap::{Parser, Subcommand};
use selencode::agent::context::{
    calc_context_pct, compact_messages, estimate_tokens, format_context_compact,
    infer_model_context_limit, sanitize_interrupted_messages,
};
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::ui::ModelPickerData;
use selencode::agent::{
    run_self_improvement_review, run_turn, run_turn_mode, run_turn_ui, run_turn_ui_mode, UiEvent,
};
use selencode::config::{self, Config, ProviderCfg};
use selencode::providers::anthropic::AnthropicProvider;
use selencode::providers::gemini::GeminiProvider;
use selencode::providers::openai::{fetch_models_with_context, OpenAiProvider};
use selencode::providers::router::{DynamicRouterProvider, ModelRoute, RouterTelemetry, TaskKind};
use selencode::providers::{Message, Provider, Role, UnconfiguredProvider};
use selencode::repl::{
    classify_key_spec, help_menu_markup, parse_model_action, parse_provider_action,
    parse_slash_command, strip_menu_markup, KeySpec, ModelAction, ProviderAction, SlashCommand,
    PROVIDER_PRESETS,
};
use selencode::security::{
    is_forbidden_command, redact_secrets, ApprovalMode, ApprovalPolicy, ApprovalRequest, Approver,
};
use selencode::skills::{
    export_conversation_md, init_agents_md, list_skills, load_skill, remove_skill, save_skill,
};
use selencode::store::Store;
use selencode::telegram::{
    parse_telegram_action, TelegramAction, TelegramClient, TgApprovalHub,
};
use selencode::tools::delegate::Delegate;
use selencode::tools::{default_registry_with_shell, read_only_registry, Ctx, Registry};
use selencode::tui::app::{App, SetupResult, SetupWizardState, SETUP_PRESETS};
use selencode::tui::{ModelPickerCmd, TuiChannels, TuiCommand};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Jalankan Telegram Bot Gateway untuk chat & coding jarak jauh dari Telegram
    Telegram,
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

/// Rakit `DynamicRouterProvider` vendor-independen (primary + fallback + task routes + telemetri).
fn make_routed_provider(
    cfg: &Config,
    primary_name: &str,
    primary_cfg: &ProviderCfg,
    primary_key: &str,
    telemetry: Arc<std::sync::Mutex<RouterTelemetry>>,
) -> Result<Arc<dyn Provider>> {
    let mut routes: Vec<ModelRoute> = Vec::new();
    let primary_prov = make_provider(cfg, primary_name, primary_cfg, primary_key)?;
    routes.push(ModelRoute::new(
        primary_name.to_string(),
        primary_cfg.model.clone(),
        primary_prov,
        primary_cfg.context_limit,
        primary_cfg.price_in,
        primary_cfg.price_out,
        true,
    ));

    let push_spec = |routes: &mut Vec<ModelRoute>, spec: &str| {
        let spec = spec.trim();
        if spec.is_empty() {
            return;
        }
        let (pname, model_override) = match spec.split_once(':') {
            Some((p, m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
                (p.trim(), Some(m.trim()))
            }
            _ => (spec, None),
        };
        let Ok((resolved_name, mut pcfg, key)) = cfg.resolve_provider(Some(pname)) else {
            return;
        };
        if let Some(m) = model_override {
            pcfg.model = m.to_string();
        }
        let id = format!("{resolved_name}:{}", pcfg.model);
        if routes.iter().any(|r| r.id == id) {
            return;
        }
        if let Ok(prov) = make_provider(cfg, &resolved_name, &pcfg, &key) {
            routes.push(ModelRoute::new(
                resolved_name,
                pcfg.model,
                prov,
                pcfg.context_limit,
                pcfg.price_in,
                pcfg.price_out,
                false,
            ));
        }
    };

    for fb in &cfg.routing.fallbacks {
        push_spec(&mut routes, fb);
    }
    for target in cfg.routing.task_routes.values() {
        push_spec(&mut routes, target);
    }
    if cfg.routing.auto {
        // Tambahkan model tersimpan lain di provider aktif agar auto-routing bisa membedakan
        // model cepat/hemat untuk ringkasan vs model kuat untuk coding/debugging.
        for m in primary_cfg.effective_models() {
            if m != primary_cfg.model {
                push_spec(&mut routes, &format!("{primary_name}:{m}"));
            }
        }
        let mut other_provs: Vec<&String> = cfg.providers.keys().collect();
        other_provs.sort();
        for pname in other_provs {
            if pname != primary_name {
                push_spec(&mut routes, pname);
            }
        }
    }

    Ok(Arc::new(DynamicRouterProvider::new(
        routes,
        cfg.routing.auto,
        cfg.routing.task_routes.clone(),
        cfg.routing.max_retries,
        telemetry,
    )))
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
    /// Cache katalog `/models` terakhir di memori (TIDAK otomatis menimpa config,
    /// agar user bisa memilih 1-per-1 lewat `/model add <nomor>` atau modal `/model`).
    last_remote_models: Vec<String>,
    /// Cache kapasitas context window model yang dilaporkan endpoint API `/models`.
    model_context_cache: HashMap<String, usize>,
    /// True bila belum ada provider/API key yang siap -> langsung buka Wizard `/setup`.
    needs_setup: bool,
    setup_reason: Option<String>,
    /// Telemetri latensi, biaya, retry, fallback, & error Dynamic Model Routing (Tahap 3).
    router_telemetry: Arc<std::sync::Mutex<RouterTelemetry>>,
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
        allow_unconfigured: bool,
    ) -> Result<Session> {
        let cfg = if allow_unconfigured {
            config::load_or_empty()?.0
        } else {
            config::load()?
        };
        let router_telemetry = Arc::new(std::sync::Mutex::new(RouterTelemetry::default()));
        let (name, pcfg, provider, needs_setup, setup_reason) =
            match cfg.resolve_provider(provider_name) {
                Ok((n, p, key)) => {
                    let prov = make_routed_provider(&cfg, &n, &p, &key, router_telemetry.clone())?;
                    (n, p, prov, false, None)
                }
                Err(e) if allow_unconfigured => {
                    let reason = e.to_string();
                    let empty_pcfg = ProviderCfg {
                        kind: "openai".to_string(),
                        base_url: None,
                        model: "(belum diatur)".to_string(),
                        models: Vec::new(),
                        api_key_env: String::new(),
                        api_key: None,
                        price_in: None,
                        price_out: None,
                        context_limit: None,
                    };
                    let stub: Arc<dyn Provider> = Arc::new(UnconfiguredProvider {
                        reason: reason.clone(),
                    });
                    (
                        "(belum diatur)".to_string(),
                        empty_pcfg,
                        stub,
                        true,
                        Some(reason),
                    )
                }
                Err(e) => return Err(e),
            };
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

        let mode = ApprovalMode::from_config(&cfg.agent.approval, yes);
        let policy = Arc::new(std::sync::Mutex::new(ApprovalPolicy::new(mode)));
        let approver = match approval_tx {
            Some(tx) => Arc::new(Approver::channel_with_policy(policy, tx)),
            None => Arc::new(Approver::stdin_with_policy(policy)),
        };
        let init_ctx_limit = pcfg
            .context_limit
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                if cfg.agent.context_limit_tokens != 100_000 && cfg.agent.context_limit_tokens > 0 {
                    cfg.agent.context_limit_tokens
                } else {
                    infer_model_context_limit(&pcfg.model)
                }
            });
        let ctx = Ctx::new(cwd, mode == ApprovalMode::AutoAll)
            .with_approver(approver)
            .with_context_limit(init_ctx_limit)
            .with_auto_skills(cfg.agent.auto_skills)
            .with_self_correct(cfg.agent.self_correct)
            .with_store(store.clone(), session_id.clone());
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
            last_remote_models: Vec::new(),
            model_context_cache: HashMap::new(),
            needs_setup,
            setup_reason,
            router_telemetry,
        })
    }

    /// Terapkan hasil Wizard `/setup`: simpan provider baru ke config, jadikan default,
    /// dan langsung aktifkan di sesi berjalan tanpa perlu restart `selen`.
    fn apply_setup_result(&mut self, res: SetupResult) -> Result<String> {
        let (api_key_env, api_key) = match classify_key_spec(&res.api_key_or_env) {
            KeySpec::EnvVar(env_name) => (env_name, None),
            KeySpec::DirectKey(raw) => (String::new(), Some(raw)),
        };
        let pcfg = ProviderCfg {
            kind: res.kind.clone(),
            base_url: res.base_url.clone(),
            model: res.model.clone(),
            models: vec![res.model.clone()],
            api_key_env,
            api_key,
            price_in: None,
            price_out: None,
            context_limit: None,
        };
        self.cfg.providers.insert(res.name.clone(), pcfg);
        self.cfg.default_provider = res.name.clone();
        let saved_path = config::save(&self.cfg)?;
        self.switch_provider(&res.name)?;
        self.needs_setup = false;
        self.setup_reason = None;
        let ctx_s = format_context_compact(self.ctx.context_limit_tokens);
        Ok(format!(
            "# Provider Siap Dipakai!\n\
             * {} | {} | aktif · ctx {ctx_s}\n\
             > config | {} | tersimpan & jadi default\n\
             ---\n\
             ! Sekarang langsung ketik pertanyaanmu, atau `/model` untuk tambah model 1-per-1.",
            self.provider_name,
            self.pcfg.model,
            saved_path.display()
        ))
    }

    /// Hitung kapasitas context window aktif untuk model saat ini:
    /// 1) override `[providers.<nama>].context_limit`, 2) metadata API `/models`,
    /// 3) override eksplisit `[agent].context_limit_tokens`, 4) deteksi otomatis nama model AI.
    fn resolve_context_limit(&self) -> usize {
        if let Some(n) = self.pcfg.context_limit.filter(|&n| n > 0) {
            return n;
        }
        if let Some(&n) = self.model_context_cache.get(&self.pcfg.model) {
            if n > 0 {
                return n;
            }
        }
        if self.cfg.agent.context_limit_tokens != 100_000 && self.cfg.agent.context_limit_tokens > 0
        {
            return self.cfg.agent.context_limit_tokens;
        }
        infer_model_context_limit(&self.pcfg.model)
    }

    fn model_context_for(&self, model_id: &str) -> usize {
        self.model_context_cache
            .get(model_id)
            .copied()
            .filter(|&n| n > 0)
            .unwrap_or_else(|| infer_model_context_limit(model_id))
    }

    /// Ganti provider aktif (perintah /model <nama>).
    fn switch_provider(&mut self, name: &str) -> Result<()> {
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(name))?;
        let provider = make_routed_provider(
            &self.cfg,
            name,
            &pcfg,
            &api_key,
            self.router_telemetry.clone(),
        )?;
        self.provider = provider;
        self.provider_name = name.to_string();
        self.pcfg = pcfg;
        self.ctx.context_limit_tokens = self.resolve_context_limit();
        self.store
            .update_session_provider(&self.session_id, name, &self.pcfg.model)?;
        Ok(())
    }

    /// Tangani `/model <arg>`: pilih lewat nomor (`/model 2` atau `/model set 2`),
    /// tambah daftar (`/model add <id...>`), hapus (`/model rm <no|id>`),
    /// ganti provider, input model manual, atau cari cepat lewat potongan nama.
    fn apply_model_arg(&mut self, arg: &str) -> Result<String> {
        match parse_model_action(arg) {
            ModelAction::FetchList(_) => anyhow::bail!("gunakan jalur async fetch_remote_models"),
            ModelAction::SelectIndex(idx) => {
                let list = self.pcfg.effective_models();
                if idx == 0 || idx > list.len() {
                    anyhow::bail!(
                        "nomor model #{idx} tidak ada (rentang tersedia: 1..={}). Cek daftar lewat `/model` atau `/models`.",
                        list.len()
                    );
                }
                let chosen = list[idx - 1].clone();
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = list;
                    p.model = chosen.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                let pname = self.provider_name.clone();
                self.switch_provider(&pname)?;
                Ok(format!(
                    "# Model Dipilih (#{idx})\n\
                     * #{idx} | {} | aktif di provider {}\n\
                     ! Config disimpan ke: {}",
                    self.pcfg.model,
                    self.provider_name,
                    saved_path.display()
                ))
            }
            ModelAction::SetCurrentModel(model) => {
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = p.effective_models();
                    p.push_model_unique(&model);
                    p.model = model.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                let pname = self.provider_name.clone();
                self.switch_provider(&pname)?;
                let pos = self
                    .pcfg
                    .effective_models()
                    .iter()
                    .position(|m| m == &self.pcfg.model)
                    .map(|i| i + 1)
                    .unwrap_or(1);
                Ok(format!(
                    "# Model Diperbarui (#{pos})\n\
                     * #{pos} | {} | aktif di provider {} & tersimpan\n\
                     ! Config disimpan ke: {}",
                    self.pcfg.model,
                    self.provider_name,
                    saved_path.display()
                ))
            }
            ModelAction::AddModels(items) => {
                // Dukung: `/model add <id_manual>`, `/model add 1 3` (ambil nomor dari `/models` terakhir),
                // atau `/model add all` (impor semua hasil `/models` terakhir).
                let mut resolved: Vec<String> = Vec::new();
                for token in &items {
                    if token.eq_ignore_ascii_case("all") {
                        if self.last_remote_models.is_empty() {
                            anyhow::bail!(
                                "belum ada katalog API yang dimuat. Jalankan `/models` dulu atau tambah manual: `/model add <id-model>`."
                            );
                        }
                        resolved.extend(self.last_remote_models.iter().cloned());
                    } else if let Ok(idx) = token.parse::<usize>() {
                        if idx >= 1 && idx <= self.last_remote_models.len() {
                            resolved.push(self.last_remote_models[idx - 1].clone());
                        } else if self.last_remote_models.is_empty() {
                            anyhow::bail!(
                                "nomor #{idx} butuh katalog `/models` dulu. Jalankan `/models` lalu `/model add {idx}`, atau tulis ID model langsung: `/model add <id-model>`."
                            );
                        } else {
                            anyhow::bail!(
                                "nomor katalog #{idx} di luar rentang 1..={}.",
                                self.last_remote_models.len()
                            );
                        }
                    } else {
                        resolved.push(token.clone());
                    }
                }
                let mut added = 0usize;
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = p.effective_models();
                    for m in &resolved {
                        if p.push_model_unique(m) {
                            added += 1;
                        }
                    }
                    self.pcfg = p.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                let mut out = format!(
                    "# Model Ditambahkan (+{added} baru, tersimpan ke {})\n",
                    saved_path.display()
                );
                let list = self.pcfg.effective_models();
                for (i, m) in list.iter().enumerate() {
                    let active = *m == self.pcfg.model;
                    let mark = if active { "*" } else { ">" };
                    let note = if active { "aktif" } else { "" };
                    out.push_str(&format!("{mark} #{} | {m} | {note}\n", i + 1));
                }
                out.push_str("---\n");
                out.push_str("! Ganti cepat lewat nomor: /model <nomor>   |   Hapus: /model rm <nomor> atau /model clear");
                Ok(out)
            }
            ModelAction::ClearModels => {
                let mut removed_count = 0usize;
                let active_model = self.pcfg.model.clone();
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    let before = p.effective_models().len();
                    p.models = if p.model.trim().is_empty() {
                        Vec::new()
                    } else {
                        vec![p.model.clone()]
                    };
                    removed_count = before.saturating_sub(p.models.len());
                    self.pcfg = p.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                Ok(format!(
                    "# Daftar Model Dibersihkan (-{removed_count} dihapus)\n\
                     * #1 | {active_model} | satu-satunya model aktif tersisa di {}\n\
                     ---\n\
                     ! Config disimpan ke: {}\n\
                     ! Tambah 1-per-1 yang kamu mau: /model add <id>   atau   buka pemilih: /model",
                    self.provider_name,
                    saved_path.display()
                ))
            }
            ModelAction::RemoveModel(target) => {
                let mut removed: Option<String> = None;
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    let mut list = p.effective_models();
                    let pos = if let Ok(idx) = target.parse::<usize>() {
                        if idx >= 1 && idx <= list.len() {
                            Some(idx - 1)
                        } else {
                            None
                        }
                    } else {
                        list.iter().position(|m| m == &target)
                    };
                    let Some(idx) = pos else {
                        anyhow::bail!("model '{target}' tidak ditemukan di daftar.");
                    };
                    if list[idx] == p.model {
                        anyhow::bail!(
                            "tidak bisa menghapus model '{}' yang sedang aktif; ganti dulu ke nomor lain.",
                            p.model
                        );
                    }
                    removed = Some(list.remove(idx));
                    p.models = list;
                    self.pcfg = p.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                Ok(format!(
                    "# Model Dihapus dari Daftar\n\
                     > dihapus | {} | tersimpan ke {}",
                    removed.unwrap_or(target),
                    saved_path.display()
                ))
            }
            ModelAction::SwitchAndSetModel { provider, model } => {
                if !self.cfg.providers.contains_key(&provider) {
                    anyhow::bail!(
                        "provider '{provider}' belum ada. Tambah dulu lewat `/provider add` atau `/provider preset`."
                    );
                }
                let chosen = if let Some(p) = self.cfg.providers.get_mut(&provider) {
                    let list = p.effective_models();
                    let resolved = if let Ok(idx) = model.parse::<usize>() {
                        if idx == 0 || idx > list.len() {
                            anyhow::bail!(
                                "nomor model #{idx} tidak ada di provider '{provider}' (rentang 1..={}).",
                                list.len()
                            );
                        }
                        list[idx - 1].clone()
                    } else {
                        model.clone()
                    };
                    p.models = list;
                    p.push_model_unique(&resolved);
                    p.model = resolved.clone();
                    resolved
                } else {
                    model
                };
                let saved_path = config::save(&self.cfg)?;
                self.switch_provider(&provider)?;
                Ok(format!(
                    "# Provider & Model Diperbarui\n\
                     * {} | {chosen} | aktif & tersimpan\n\
                     ! Config disimpan ke: {}",
                    self.provider_name,
                    saved_path.display()
                ))
            }
            ModelAction::SwitchOrModel(target) => {
                // 1. Nama provider terdaftar -> pindah ke provider itu.
                if self.cfg.providers.contains_key(&target) {
                    self.switch_provider(&target)?;
                    return Ok(self.models_menu());
                }
                // 2. Cek kecocokan di daftar model tersimpan provider aktif (exact atau substring).
                let list = self.pcfg.effective_models();
                let needle = target.to_ascii_lowercase();
                let exact = list.iter().position(|m| m.eq_ignore_ascii_case(&target));
                let matches: Vec<(usize, &String)> = if let Some(i) = exact {
                    vec![(i + 1, &list[i])]
                } else {
                    list.iter()
                        .enumerate()
                        .filter(|(_, m)| m.to_ascii_lowercase().contains(&needle))
                        .map(|(i, m)| (i + 1, m))
                        .collect()
                };
                if matches.len() == 1 {
                    let (idx, chosen) = (matches[0].0, matches[0].1.clone());
                    if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                        p.models = list;
                        p.model = chosen;
                    }
                    let saved_path = config::save(&self.cfg)?;
                    let pname = self.provider_name.clone();
                    self.switch_provider(&pname)?;
                    return Ok(format!(
                        "# Model Dipilih (#{idx})\n\
                         * #{idx} | {} | aktif di provider {}\n\
                         ! Config disimpan ke: {}",
                        self.pcfg.model,
                        self.provider_name,
                        saved_path.display()
                    ));
                }
                if matches.len() > 1 {
                    let mut out = format!(
                        "# Ditemukan {} Model Cocok \"{target}\"\n",
                        matches.len()
                    );
                    for (idx, m) in matches.iter().take(25) {
                        let active = **m == self.pcfg.model;
                        let mark = if active { "*" } else { ">" };
                        let note = if active { "aktif" } else { "" };
                        out.push_str(&format!("{mark} #{idx} | {m} | {note}\n"));
                    }
                    out.push_str("---\n");
                    out.push_str("! Pilih cepat dengan nomor: /model <nomor>   (mis. /model 2)");
                    return Ok(out);
                }
                // 3. Tidak cocok di daftar -> pasang sebagai model manual baru & simpan!
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = list;
                    p.push_model_unique(&target);
                    p.model = target.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                let pname = self.provider_name.clone();
                self.switch_provider(&pname)?;
                let pos = self
                    .pcfg
                    .effective_models()
                    .iter()
                    .position(|m| m == &self.pcfg.model)
                    .map(|i| i + 1)
                    .unwrap_or(1);
                Ok(format!(
                    "# Model Manual Dipasang (#{pos})\n\
                     * #{pos} | {} | aktif & tersimpan ke daftar\n\
                     ! Config disimpan ke: {}",
                    self.pcfg.model,
                    saved_path.display()
                ))
            }
        }
    }

    /// Tarik katalog model real-time dari endpoint `/models` provider aktif
    /// (disimpan ke cache memori sesi, TIDAK mengotori daftar pribadi di config)
    /// agar user bisa memilih 1-per-1 yang memang ingin dipakai.
    async fn fetch_remote_models_raw(&mut self) -> Result<Vec<String>> {
        if self.pcfg.kind != "openai" {
            anyhow::bail!(
                "provider '{}' (kind={}) tidak menyediakan endpoint /models standar OpenAI. Tambah manual: `/model add <id-model>`",
                self.provider_name,
                self.pcfg.kind
            );
        }
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(&self.provider_name))?;
        let base_url = pcfg
            .base_url
            .ok_or_else(|| anyhow::anyhow!("base_url kosong untuk provider '{}'", self.provider_name))?;
        let items = fetch_models_with_context(&base_url, &api_key).await?;
        let mut ids = Vec::with_capacity(items.len());
        for (id, ctx_opt) in items {
            if let Some(limit) = ctx_opt {
                self.model_context_cache.insert(id.clone(), limit);
            }
            ids.push(id);
        }
        // Perbarui kapasitas konteks model aktif bila baru ditemukan dari API.
        self.ctx.context_limit_tokens = self.resolve_context_limit();
        self.last_remote_models = ids.clone();
        Ok(ids)
    }

    async fn fetch_remote_models_menu(&mut self, filter: Option<&str>) -> Result<String> {
        if self.pcfg.kind != "openai" {
            return Ok(format!(
                "# Katalog Model ({})\n\
                 ! Provider kind '{}' tidak menyediakan endpoint /models standar OpenAI.\n\
                 ! Tambah manual 1-per-1 ke daftar: /model add <id-model>\n\
                 ! Lalu pilih cepat dengan: /model <nomor>",
                self.provider_name, self.pcfg.kind
            ));
        }
        let ids = self.fetch_remote_models_raw().await?;
        let saved = self.pcfg.effective_models();
        let needle = filter
            .map(|f| f.trim().to_ascii_lowercase())
            .filter(|f| !f.is_empty());
        let shown: Vec<(usize, &String)> = ids
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                needle
                    .as_deref()
                    .map(|n| m.to_ascii_lowercase().contains(n))
                    .unwrap_or(true)
            })
            .map(|(i, m)| (i + 1, m))
            .collect();

        let header = match &needle {
            Some(f) => format!(
                "# Katalog API {} cocok \"{f}\" ({}/{} tersedia)\n",
                self.provider_name,
                shown.len(),
                ids.len()
            ),
            None => format!(
                "# Katalog API {} ({} tersedia · tidak otomatis disimpan)\n",
                self.provider_name,
                ids.len()
            ),
        };
        let mut out = header;
        if shown.is_empty() {
            out.push_str("! Tidak ada model yang cocok.\n");
        } else {
            for (idx, id) in shown.iter().take(40) {
                let is_active = **id == self.pcfg.model;
                let is_saved = saved.iter().any(|s| s == *id);
                let mark = if is_active { "*" } else { ">" };
                let note = if is_active {
                    "aktif · tersimpan"
                } else if is_saved {
                    "✓ sudah di daftar"
                } else {
                    ""
                };
                out.push_str(&format!("{mark} #{idx} | {id} | {note}\n"));
            }
            if shown.len() > 40 {
                out.push_str(&format!(
                    "! ... dan {} model lainnya (saring pakai `/models <kata>`, mis. `/models claude`)\n",
                    shown.len() - 40
                ));
            }
        }
        out.push_str("---\n");
        out.push_str("! Daftar pribadimu tetap bersih! Ambil 1-per-1 yang mau dipakai saja:\n");
        out.push_str("! /model add 1 3   (simpan #1 & #3)   |   /model add all   |   /model (pemilih interaktif)");
        Ok(out)
    }

    /// Tangani `/provider` (list, add, model, default, rm).
    fn handle_provider(&mut self, arg: Option<&str>) -> Result<String> {
        match parse_provider_action(arg) {
            ProviderAction::List => Ok(self.providers_menu()),
            ProviderAction::ListPresets => Ok(self.provider_presets_menu()),
            ProviderAction::Add {
                name,
                kind,
                model,
                base_url,
                key_spec,
            } => {
                if !matches!(kind.as_str(), "openai" | "anthropic" | "gemini") {
                    anyhow::bail!(
                        "kind '{kind}' tidak dikenal. Pilihan: openai, anthropic, gemini"
                    );
                }
                if kind == "openai" && base_url.is_none() {
                    anyhow::bail!(
                        "base_url wajib untuk kind=openai. Contoh:\n  /provider add openrouter openai anthropic/claude-3.7-sonnet https://openrouter.ai/api/v1 OPENROUTER_API_KEY"
                    );
                }
                let (api_key_env, api_key) = match key_spec {
                    KeySpec::EnvVar(env_name) => (env_name, None),
                    KeySpec::DirectKey(raw) => (String::new(), Some(raw)),
                };
                let pcfg = ProviderCfg {
                    kind: kind.clone(),
                    base_url: base_url.clone(),
                    model: model.clone(),
                    models: vec![model.clone()],
                    api_key_env: api_key_env.clone(),
                    api_key,
                    price_in: None,
                    price_out: None,
                    context_limit: None,
                };
                self.cfg.providers.insert(name.clone(), pcfg);
                let saved_path = config::save(&self.cfg)?;
                let key_info = if !api_key_env.is_empty() {
                    if std::env::var(&api_key_env).is_ok() {
                        format!("env {api_key_env} (siap)")
                    } else {
                        format!("env {api_key_env} (belum di-export)")
                    }
                } else {
                    "api_key tersimpan di config".to_string()
                };
                // Bila provider yang baru ditambah sedang aktif, muat ulang instance-nya.
                if name == self.provider_name && self.cfg.is_key_ready(&name) {
                    let _ = self.switch_provider(&name);
                }
                Ok(format!(
                    "# Provider Ditambahkan\n\
                     > {name} | {model} | kind {kind} · {key_info}\n\
                     ---\n\
                     ! Config disimpan ke: {}\n\
                     ! Pakai sekarang: /model {name}   |   Jadikan default: /provider default {name}",
                    saved_path.display()
                ))
            }
            ProviderAction::SetModel { name, model } => {
                let p = self
                    .cfg
                    .providers
                    .get_mut(&name)
                    .ok_or_else(|| anyhow::anyhow!("provider '{name}' tidak ada di config"))?;
                p.models = p.effective_models();
                p.push_model_unique(&model);
                p.model = model.clone();
                let saved_path = config::save(&self.cfg)?;
                if name == self.provider_name {
                    self.switch_provider(&name)?;
                }
                Ok(format!(
                    "# Model Provider Diperbarui\n\
                     * {name} | {model} | tersimpan\n\
                     ! Config disimpan ke: {}",
                    saved_path.display()
                ))
            }
            ProviderAction::SetDefault(name) => {
                if !self.cfg.providers.contains_key(&name) {
                    anyhow::bail!("provider '{name}' tidak ada di config");
                }
                self.cfg.default_provider = name.clone();
                let saved_path = config::save(&self.cfg)?;
                Ok(format!(
                    "# Default Provider Diperbarui\n\
                     * {name} | default | tersimpan ke {}",
                    saved_path.display()
                ))
            }
            ProviderAction::Remove(name) => {
                if name == self.provider_name {
                    anyhow::bail!("tidak bisa menghapus provider '{name}' yang sedang aktif; pindah dulu lewat `/model <lain>`.");
                }
                if self.cfg.providers.remove(&name).is_none() {
                    anyhow::bail!("provider '{name}' tidak ada di config");
                }
                if self.cfg.default_provider == name {
                    self.cfg.default_provider = self.provider_name.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                Ok(format!(
                    "# Provider Dihapus\n\
                     > {name} | dihapus | tersimpan ke {}",
                    saved_path.display()
                ))
            }
            ProviderAction::Usage => Ok(self.provider_usage_menu()),
        }
    }

    /// Mulai sesi baru yang bersih (/new).
    fn reset_session(&mut self) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let new_id = self.store.create_session(
            &cwd_str,
            &self.provider_name,
            &self.pcfg.model,
            "(sesi baru)",
        )?;
        self.session_id = new_id.clone();
        self.ctx.session_id = Some(new_id.clone());
        self.messages.clear();
        if let Ok(mut plan) = self.ctx.plan_state.lock() {
            plan.clear();
        }
        Ok(new_id)
    }

    /// Compaction konteks manual (/compact).
    fn compact_now(&mut self) -> String {
        let before_msgs = self.messages.len();
        let before_tok = estimate_tokens(&self.messages);
        let compacted = compact_messages(&self.messages);
        let after_msgs = compacted.len();
        let after_tok = estimate_tokens(&compacted);
        self.messages = compacted;
        if after_msgs == before_msgs {
            format!(
                "# Compaction Konteks\n\
                 ! Riwayat masih pendek ({before_msgs} pesan, ~{before_tok} token) — belum perlu diringkas."
            )
        } else {
            format!(
                "# Compaction Konteks Selesai\n\
                 * pesan | {before_msgs} → {after_msgs} | estimasi ~{before_tok} → ~{after_tok} token"
            )
        }
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

    /// Kartu menu `/model`: daftar model bernomor milik provider aktif + daftar provider lain.
    fn models_menu(&self) -> String {
        let list = self.pcfg.effective_models();
        let active_ctx = format_context_compact(self.ctx.context_limit_tokens);
        let mut out = format!(
            "# Model di Provider {} ({} tersimpan · ctx {active_ctx})\n",
            self.provider_name,
            list.len()
        );
        for (i, m) in list.iter().take(30).enumerate() {
            let active = *m == self.pcfg.model;
            let mark = if active { "*" } else { ">" };
            let m_ctx = format_context_compact(self.model_context_for(m));
            let note = if active {
                format!("aktif · ctx {m_ctx}")
            } else {
                format!("ctx {m_ctx}")
            };
            out.push_str(&format!("{mark} #{} | {m} | {note}\n", i + 1));
        }
        if list.len() > 30 {
            out.push_str(&format!(
                "! ... dan {} model lainnya (cari cepat: `/models <kata>` atau `/model <kata>`)\n",
                list.len() - 30
            ));
        }
        out.push_str("---\n");
        let mut names: Vec<&String> = self.cfg.providers.keys().collect();
        names.sort();
        for n in names {
            let p = &self.cfg.providers[n];
            let active = *n == self.provider_name;
            let mark = if active { "*" } else { ">" };
            let key_ok = if self.cfg.is_key_ready(n) {
                "siap"
            } else {
                "key belum diset"
            };
            let count = p.effective_models().len();
            let tag = if active {
                format!("provider aktif · {} · {count} model · {key_ok}", p.kind)
            } else {
                format!("provider · {} · {count} model · {key_ok}", p.kind)
            };
            out.push_str(&format!("{mark} {n} | {} | {tag}\n", p.model));
        }
        out.push_str("---\n");
        out.push_str("! Ganti cepat:       /model 2   (pakai nomor)   |   /model claude   (potongan nama)\n");
        out.push_str("! Tambah 1-per-1:    /model add <id>   |   Intip API: /models [cari] lalu /model add 1 3\n");
        out.push_str("! Hapus daftar:      /model rm <no>   |   Hapus semua: /model clear   |   Provider: /provider");
        out
    }

    fn model_picker_data(&self, open_catalog: bool, status: Option<String>) -> ModelPickerData {
        let mut providers: Vec<String> = self.cfg.providers.keys().cloned().collect();
        providers.sort();
        ModelPickerData {
            provider: self.provider_name.clone(),
            providers,
            active_model: self.pcfg.model.clone(),
            saved_models: self.pcfg.effective_models(),
            remote_models: self.last_remote_models.clone(),
            open_catalog,
            status,
        }
    }

    /// Aktifkan 1 model secara literal (tanpa parsing ulang string agar ID numerik/"all" tetap aman).
    fn select_model_literal(&mut self, model_id: &str) -> Result<String> {
        let id = model_id.trim();
        if id.is_empty() {
            anyhow::bail!("ID model tidak boleh kosong.");
        }
        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
            p.models = p.effective_models();
            p.push_model_unique(id);
            p.model = id.to_string();
        }
        let saved_path = config::save(&self.cfg)?;
        let pname = self.provider_name.clone();
        self.switch_provider(&pname)?;
        let pos = self
            .pcfg
            .effective_models()
            .iter()
            .position(|m| m == &self.pcfg.model)
            .map(|i| i + 1)
            .unwrap_or(1);
        Ok(format!(
            "# Model Diperbarui (#{pos})\n\
             * #{pos} | {} | aktif di provider {} & tersimpan\n\
             ! Config disimpan ke: {}",
            self.pcfg.model,
            self.provider_name,
            saved_path.display()
        ))
    }

    /// Tambah 1 model secara literal ke daftar pribadi provider aktif (mengembalikan `true` jika baru).
    fn add_model_literal(&mut self, model_id: &str) -> Result<bool> {
        let id = model_id.trim();
        if id.is_empty() {
            anyhow::bail!("ID model tidak boleh kosong.");
        }
        let mut added = false;
        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
            p.models = p.effective_models();
            added = p.push_model_unique(id);
            self.pcfg = p.clone();
        }
        config::save(&self.cfg)?;
        Ok(added)
    }

    /// Hapus 1 model secara literal dari daftar pribadi provider aktif.
    fn remove_model_literal(&mut self, model_id: &str) -> Result<()> {
        let id = model_id.trim();
        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
            let mut list = p.effective_models();
            let Some(pos) = list.iter().position(|m| m == id) else {
                anyhow::bail!("model '{id}' tidak ditemukan di daftar.");
            };
            if list[pos] == p.model {
                anyhow::bail!(
                    "tidak bisa menghapus model '{}' yang sedang aktif; pindah dulu ke model lain.",
                    p.model
                );
            }
            list.remove(pos);
            p.models = list;
            self.pcfg = p.clone();
        }
        config::save(&self.cfg)?;
        Ok(())
    }

    /// Tangani `/telegram [setup|allow|rm|approve|off]`.
    fn handle_telegram(&mut self, arg: Option<&str>) -> Result<String> {
        // Sinkronkan bagian `[telegram]` terbaru dari disk agar edit terminal & bot tidak saling timpa.
        if let Ok(fresh) = config::load() {
            self.cfg.telegram = fresh.telegram;
        }
        match parse_telegram_action(arg) {
            TelegramAction::Status => {
                let tok_state = if self.cfg.telegram.is_token_ready() {
                    if !self.cfg.telegram.bot_token_env.trim().is_empty() {
                        format!("siap (env {})", self.cfg.telegram.bot_token_env)
                    } else if self.cfg.telegram.bot_token.is_some() {
                        "siap (tersimpan di config)".to_string()
                    } else {
                        "siap (env SELENCODE_TELEGRAM_TOKEN)".to_string()
                    }
                } else {
                    "belum diatur".to_string()
                };
                let users_str = if self.cfg.telegram.allowed_users.is_empty() {
                    "(kosong — wajib isi whitelist agar aman)".to_string()
                } else {
                    self.cfg
                        .telegram
                        .allowed_users
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let appr = if self.cfg.telegram.approval.trim().is_empty() {
                    "ask (tombol inline di Telegram)".to_string()
                } else {
                    self.cfg.telegram.approval.clone()
                };
                Ok(format!(
                    "# Telegram Gateway SelenCode\n\
                     * bot token | {tok_state} | dari @BotFather\n\
                     > allowed_users | {users_str} | whitelist ID Telegram\n\
                     > approval | {appr} | ask / edits / auto\n\
                     ---\n\
                     ! Pasang cepat:      /telegram setup <BOT_TOKEN> <USER_ID>\n\
                     ! Tambah/hapus ID:   /telegram allow <USER_ID>   |   /telegram rm <USER_ID>\n\
                     ! Mode izin bot:     /telegram approve <ask|edits|auto>   |   Matikan: /telegram off\n\
                     ! Jalankan gateway:  ketik `selen telegram` di terminal (bisa di PC/Termux), lalu chat bot kamu!"
                ))
            }
            TelegramAction::Setup {
                token_spec,
                user_id,
            } => {
                match token_spec {
                    KeySpec::EnvVar(env_name) => {
                        self.cfg.telegram.bot_token_env = env_name;
                        self.cfg.telegram.bot_token = None;
                    }
                    KeySpec::DirectKey(raw) => {
                        self.cfg.telegram.bot_token = Some(raw);
                    }
                }
                if let Some(uid) = user_id {
                    if !self.cfg.telegram.allowed_users.contains(&uid) {
                        self.cfg.telegram.allowed_users.push(uid);
                    }
                }
                let saved = config::save(&self.cfg)?;
                let uid_note = if self.cfg.telegram.allowed_users.is_empty() {
                    "Belum ada User ID di whitelist — jalankan `selen telegram`, kirim `/id` ke bot, lalu `/telegram allow <ID>`."
                        .to_string()
                } else {
                    format!(
                        "Whitelist aktif: {}",
                        self.cfg
                            .telegram
                            .allowed_users
                            .iter()
                            .map(|u| u.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                Ok(format!(
                    "# Telegram Gateway Tersimpan\n\
                     * status | token tersimpan | {}\n\
                     > config | {} | aman (chmod 600 di Unix)\n\
                     ---\n\
                     ! Jalankan bot sekarang dari terminal: selen telegram",
                    uid_note,
                    saved.display()
                ))
            }
            TelegramAction::Allow(uid) => {
                if !self.cfg.telegram.allowed_users.contains(&uid) {
                    self.cfg.telegram.allowed_users.push(uid);
                }
                let saved = config::save(&self.cfg)?;
                Ok(format!(
                    "# User Telegram Diizinkan\n\
                     * allowed | {uid} | total {} user di whitelist\n\
                     ! Config disimpan ke: {}",
                    self.cfg.telegram.allowed_users.len(),
                    saved.display()
                ))
            }
            TelegramAction::Remove(uid) => {
                let before = self.cfg.telegram.allowed_users.len();
                self.cfg.telegram.allowed_users.retain(|x| *x != uid);
                if self.cfg.telegram.allowed_users.len() == before {
                    anyhow::bail!("User ID {uid} tidak ada di whitelist Telegram.");
                }
                let saved = config::save(&self.cfg)?;
                Ok(format!(
                    "# User Telegram Dihapus\n\
                     > dihapus | {uid} | sisa {} user di whitelist\n\
                     ! Config disimpan ke: {}",
                    self.cfg.telegram.allowed_users.len(),
                    saved.display()
                ))
            }
            TelegramAction::SetApproval(mode) => {
                if !matches!(mode.as_str(), "ask" | "edits" | "auto") {
                    anyhow::bail!("mode '{mode}' tidak dikenal. Pilihan: ask, edits, auto");
                }
                self.cfg.telegram.approval = mode.clone();
                let saved = config::save(&self.cfg)?;
                Ok(format!(
                    "# Mode Izin Telegram Diperbarui\n\
                     * approval | {mode} | berlaku saat menjalankan `selen telegram`\n\
                     ! Config disimpan ke: {}",
                    saved.display()
                ))
            }
            TelegramAction::Disable => {
                self.cfg.telegram.bot_token = None;
                self.cfg.telegram.bot_token_env.clear();
                let saved = config::save(&self.cfg)?;
                Ok(format!(
                    "# Telegram Gateway Dinonaktifkan\n\
                     > token | dihapus dari config | {}\n\
                     ! Aktifkan lagi kapan saja: /telegram setup <BOT_TOKEN> <USER_ID>",
                    saved.display()
                ))
            }
            TelegramAction::Usage => Ok(
                "# Cara Pakai /telegram\n\
                 > /telegram | (cek status) | lihat token, whitelist, & mode approval\n\
                 > /telegram setup | <BOT_TOKEN> [USER_ID] | simpan token @BotFather (+ whitelist ID opsional)\n\
                 > /telegram allow | <USER_ID> | izinkan user/chat Telegram tertentu\n\
                 > /telegram rm | <USER_ID> | hapus user dari whitelist\n\
                 > /telegram approve | <ask|edits|auto> | atur mode izin tool di bot\n\
                 > /telegram off | | hapus token bot dari config\n\
                 ---\n\
                 ! Setelah setup, jalankan di terminal: selen telegram"
                    .to_string(),
            ),
        }
    }

    fn handle_checkpoint(&self, arg: Option<&str>) -> Result<String> {
        let mut stack = self.ctx.undo_stack.lock().expect("undo_stack poisoned");
        let label = arg.map(|s| s.trim()).filter(|s| !s.is_empty());
        if let Some(lbl) = label {
            if !lbl.eq_ignore_ascii_case("list") && !lbl.eq_ignore_ascii_case("ls") {
                let id = stack.begin_checkpoint(lbl.to_string());
                return Ok(format!(
                    "# Checkpoint Transaksional Dibuat (#{id})\n\
                     * #{id} | {lbl} | titik aman aktif\n\
                     ! Gunakan `/rollback {id}` kapan saja untuk memulihkan perubahan SelenCode ke titik ini."
                ));
            }
        }
        let cps = stack.list_checkpoints();
        if cps.is_empty() {
            return Ok(
                "# Daftar Checkpoint\n\
                 ! Belum ada checkpoint tersimpan.\n\
                 ! Buat manual: /checkpoint <label>   (atau otomatis dibuat di awal giliran edit kode)"
                    .to_string(),
            );
        }
        let mut out = format!("# Daftar Checkpoint ({} tersimpan)\n", cps.len());
        for (cp, count) in cps.iter().rev().take(15) {
            out.push_str(&format!(
                "> #{} | {} | {} · {} perubahan sejak titik ini\n",
                cp.id, cp.label, cp.created_at, count
            ));
        }
        out.push_str("---\n");
        out.push_str("! Rollback ke checkpoint: /rollback <id>   |   Buat baru: /checkpoint <label>");
        Ok(out)
    }

    fn handle_rollback(&self, arg: Option<&str>) -> Result<String> {
        let target_id = match arg.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(s) => {
                let clean = s.trim_start_matches('#');
                let n = clean
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("ID checkpoint tidak valid: '{s}'. Contoh: /rollback 1"))?;
                Some(n)
            }
            None => None,
        };
        let mut stack = self.ctx.undo_stack.lock().expect("undo_stack poisoned");
        let report = stack.rollback_to(target_id)?;
        let mut out = format!(
            "# Rollback ke Checkpoint #{} ({})\n",
            report.checkpoint_id, report.label
        );
        if report.restored.is_empty() {
            out.push_str("> status | 0 file dipulihkan | tidak ada perubahan SelenCode sejak checkpoint ini\n");
        } else {
            for item in &report.restored {
                out.push_str(&format!("* pulih | {item} |\n"));
            }
        }
        if !report.skipped_conflicts.is_empty() {
            out.push_str("---\n");
            for c in &report.skipped_conflicts {
                out.push_str(&format!("! dilewati (konflik eksternal): {c}\n"));
            }
        }
        Ok(out)
    }

    async fn handle_verify(&self, arg: Option<&str>) -> Result<String> {
        let scope = arg.map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("all");
        let report = selencode::tools::verify::run_verify_plan(
            &self.ctx.cwd,
            scope,
            self.cfg.shell.timeout_secs.max(120),
        )
        .await?;
        Ok(report.format_menu_card())
    }

    fn providers_menu(&self) -> String {
        let mut out = format!(
            "# Manajemen Provider (config: {})\n",
            config::active_config_path().display()
        );
        let mut names: Vec<&String> = self.cfg.providers.keys().collect();
        names.sort();
        for n in names {
            let p = &self.cfg.providers[n];
            let mark = if *n == self.provider_name { "*" } else { ">" };
            let url = p.base_url.as_deref().unwrap_or("(endpoint resmi)");
            let key_status = if self.cfg.is_key_ready(n) {
                if !p.api_key_env.is_empty() {
                    format!("env {} siap", p.api_key_env)
                } else {
                    "api_key siap".to_string()
                }
            } else if !p.api_key_env.is_empty() {
                format!("env {} KOSONG", p.api_key_env)
            } else {
                "key KOSONG".to_string()
            };
            let def_tag = if *n == self.cfg.default_provider {
                " · default"
            } else {
                ""
            };
            out.push_str(&format!(
                "{mark} {n} | {} | {} · {url} · {key_status}{def_tag}\n",
                p.model, p.kind
            ));
        }
        out.push_str("---\n");
        out.push_str(&self.provider_usage_footer());
        out
    }

    fn provider_presets_menu(&self) -> String {
        let mut out = String::from("# Preset Provider Siap Pakai\n");
        for p in PROVIDER_PRESETS {
            let url = p.base_url.unwrap_or("(endpoint resmi)");
            let env = if p.default_env.is_empty() {
                "(tanpa API key — lokal)"
            } else {
                p.default_env
            };
            out.push_str(&format!(
                "> {} | {} | {} · {url} · {env}\n",
                p.id, p.default_model, p.kind
            ));
        }
        out.push_str("---\n");
        out.push_str("! Pasang cepat: /provider preset <id> [model] [ENV_atau_api_key]\n");
        out.push_str("! Contoh:       /provider preset openrouter anthropic/claude-3.7-sonnet OPENROUTER_API_KEY\n");
        out.push_str("! Contoh lokal: /provider preset ollama qwen2.5-coder:7b");
        out
    }

    /// `!perintah` — jalankan shell lokal cepat tanpa memanggil LLM (ala OpenCode).
    async fn handle_inline_shell(&self, command: &str) -> Result<String> {
        if is_forbidden_command(command) {
            anyhow::bail!("perintah ditolak demi keamanan (pola destruktif): {command}");
        }
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", command]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&self.ctx.cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("timeout 60 detik menjalankan `!{command}`"))??;
        let code = out.status.code().unwrap_or(-1);
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        let clean = redact_secrets(&combined);
        let mut lines: Vec<&str> = clean.lines().collect();
        let truncated = if lines.len() > 30 {
            lines.truncate(30);
            true
        } else {
            false
        };
        let mut card = format!("# Shell: !{command} (exit {code})\n");
        if lines.is_empty() {
            card.push_str("! (tanpa output)\n");
        } else {
            for l in lines {
                card.push_str(l);
                card.push('\n');
            }
            if truncated {
                card.push_str("! ... (output dipotong 30 baris)\n");
            }
        }
        Ok(card)
    }

    fn handle_undo(&self) -> Result<String> {
        let mut stack = self.ctx.undo_stack.lock().expect("undo_stack poisoned");
        match stack.undo()? {
            Some(msg) => Ok(format!(
                "# Undo Berhasil\n* dipulihkan | {msg} | sisa undo: {}, redo: {}",
                stack.undo_len(),
                stack.redo_len()
            )),
            None => Ok("# Undo\n! Tidak ada perubahan file yang bisa di-undo di sesi ini.".into()),
        }
    }

    fn handle_redo(&self) -> Result<String> {
        let mut stack = self.ctx.undo_stack.lock().expect("undo_stack poisoned");
        match stack.redo()? {
            Some(msg) => Ok(format!(
                "# Redo Berhasil\n* diterapkan | {msg} | sisa undo: {}, redo: {}",
                stack.undo_len(),
                stack.redo_len()
            )),
            None => Ok("# Redo\n! Tidak ada perubahan file yang bisa di-redo.".into()),
        }
    }

    fn handle_index(&self, arg: Option<&str>) -> Result<String> {
        let mut idx = self
            .ctx
            .repo_index
            .lock()
            .expect("repo_index poisoned");
        Ok(idx.format_status_card(arg))
    }

    fn handle_jobs(&self, arg: Option<&str>) -> Result<String> {
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(self.ctx.bg_manager.format_jobs_card());
        };
        if let Some(rest) = raw
            .strip_prefix("stop ")
            .or_else(|| raw.strip_prefix("kill "))
        {
            let target = rest.trim();
            if target.eq_ignore_ascii_case("all") {
                let n = self.ctx.bg_manager.stop_all();
                return Ok(format!(
                    "# Proses Background Dihentikan\n* dihentikan | {n} proses | semua job aktif milik SelenCode dihentikan"
                ));
            }
            let snap = self.ctx.bg_manager.stop(target)?;
            return Ok(format!(
                "# Proses Background Dihentikan\n* {} | {} | {}",
                snap.id,
                snap.state.label(),
                snap.command
            ));
        }
        if let Some(rest) = raw
            .strip_prefix("output ")
            .or_else(|| raw.strip_prefix("log "))
            .or_else(|| raw.strip_prefix("logs "))
        {
            let target = rest.trim();
            let (snap, out) = self.ctx.bg_manager.output(target, 40, 8192)?;
            let mut card = format!(
                "# Log {} ({})\n> perintah | {} | durasi {}s\n---\n",
                snap.id,
                snap.state.label(),
                snap.command,
                snap.elapsed_secs
            );
            if out.trim().is_empty() {
                card.push_str("! (belum ada output)\n");
            } else {
                for line in out.lines().take(40) {
                    card.push_str(line);
                    card.push('\n');
                }
            }
            return Ok(card);
        }
        Ok(self.ctx.bg_manager.format_jobs_card())
    }

    fn handle_route(&mut self, arg: Option<&str>) -> Result<String> {
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let mode = if self.cfg.routing.auto {
                "auto (dinamis sesuai jenis tugas)"
            } else {
                "manual (hormati model pilihan user + fallback & retry)"
            };
            let fb = if self.cfg.routing.fallbacks.is_empty() {
                "(belum diatur)".to_string()
            } else {
                self.cfg.routing.fallbacks.join(" -> ")
            };
            let mut out = format!(
                "# Dynamic Model Routing & Telemetri\n\
                 * mode | {mode} | retry maks {}\n\
                 > primary | {}:{} | fallback: {fb}\n",
                self.cfg.routing.max_retries, self.provider_name, self.pcfg.model
            );
            if !self.cfg.routing.task_routes.is_empty() {
                out.push_str("---\n");
                let mut pairs: Vec<(&String, &String)> = self.cfg.routing.task_routes.iter().collect();
                pairs.sort_by(|a, b| a.0.cmp(b.0));
                for (task, target) in pairs {
                    out.push_str(&format!("> tugas {task} | {target} | aturan eksplisit\n"));
                }
            }
            if let Ok(tel) = self.router_telemetry.lock() {
                let metrics = tel.snapshot_routes();
                if !metrics.is_empty() {
                    out.push_str("---\n");
                    for m in metrics.iter().take(6) {
                        let cost = if m.has_cost_data {
                            format!(" · ${:.4}", m.estimated_cost_usd)
                        } else {
                            String::new()
                        };
                        let cd = if m.in_cooldown() { " [cooldown]" } else { "" };
                        out.push_str(&format!(
                            "> {} | {} ok / {} err | avg {}ms · {}in/{}out{cost}{cd}\n",
                            m.route_id,
                            m.successes,
                            m.errors,
                            m.avg_latency_ms(),
                            m.input_tokens,
                            m.output_tokens
                        ));
                    }
                }
            }
            out.push_str("---\n");
            out.push_str("! Aktifkan auto:   /route auto on  (atau /route auto off)\n");
            out.push_str("! Atur fallback:   /route fallback <prov1,prov2>  (atau /route fallback clear)\n");
            out.push_str("! Atur per tugas:  /route set <coding/debugging/reasoning/review/summary> <prov[:model]>\n");
            out.push_str("! Batas retry:     /route retries <0-3>");
            return Ok(out);
        };

        if let Some(rest) = raw.strip_prefix("auto ") {
            let val = rest.trim().to_ascii_lowercase();
            let enabled = match val.as_str() {
                "on" | "true" | "1" | "aktif" => true,
                "off" | "false" | "0" | "manual" => false,
                _ => anyhow::bail!("pakai `/route auto on` atau `/route auto off`"),
            };
            self.cfg.routing.auto = enabled;
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            let st = if enabled { "AUTO AKTIF" } else { "MANUAL" };
            return Ok(format!(
                "# Mode Routing Diperbarui\n* routing | {st} | disimpan ke {}",
                saved.display()
            ));
        }

        if let Some(rest) = raw.strip_prefix("fallback ") {
            let val = rest.trim();
            if val.eq_ignore_ascii_case("clear") || val.eq_ignore_ascii_case("none") {
                self.cfg.routing.fallbacks.clear();
            } else {
                self.cfg.routing.fallbacks = val
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
            }
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            let disp = if self.cfg.routing.fallbacks.is_empty() {
                "(kosong)".to_string()
            } else {
                self.cfg.routing.fallbacks.join(" -> ")
            };
            return Ok(format!(
                "# Fallback Provider Diperbarui\n* fallbacks | {disp} | disimpan ke {}",
                saved.display()
            ));
        }

        if let Some(rest) = raw.strip_prefix("set ") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let task_raw = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("").trim();
            let task = TaskKind::parse(task_raw).ok_or_else(|| {
                anyhow::anyhow!(
                    "jenis tugas '{task_raw}' tidak dikenal. Pilihan: coding, debugging, reasoning, review, summary"
                )
            })?;
            if target.is_empty() {
                anyhow::bail!("target kosong. Contoh: /route set debugging anthropic:claude-sonnet-4");
            }
            self.cfg
                .routing
                .task_routes
                .insert(task.as_str().to_string(), target.to_string());
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            return Ok(format!(
                "# Aturan Tugas Diperbarui\n* {} | {target} | disimpan ke {}",
                task.as_str(),
                saved.display()
            ));
        }

        if let Some(rest) = raw.strip_prefix("unset ").or_else(|| raw.strip_prefix("rm ")) {
            let key = TaskKind::parse(rest.trim())
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| rest.trim().to_ascii_lowercase());
            if self.cfg.routing.task_routes.remove(&key).is_none() {
                anyhow::bail!("aturan tugas '{key}' tidak ditemukan");
            }
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            return Ok(format!(
                "# Aturan Tugas Dihapus\n> {key} | dihapus | disimpan ke {}",
                saved.display()
            ));
        }

        if let Some(rest) = raw.strip_prefix("retries ") {
            let n: u8 = rest
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("jumlah retry harus angka 0..=3"))?;
            self.cfg.routing.max_retries = n.min(3);
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            return Ok(format!(
                "# Batas Retry Diperbarui\n* max_retries | {} | disimpan ke {}",
                self.cfg.routing.max_retries,
                saved.display()
            ));
        }

        anyhow::bail!(
            "subperintah `/route {raw}` tidak dikenal. Ketik `/route` untuk melihat status & cara pakai."
        )
    }

    fn handle_memory(&self, arg: Option<&str>) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let list = self.store.list_memories(&cwd_str, 40)?;
            let mut out = String::from("# Memori Proyek & Global Selena\n");
            if list.is_empty() {
                out.push_str("! Belum ada memori tersimpan.\n");
            } else {
                for m in list {
                    let scope = if m.scope == "global" { "global" } else { "project" };
                    out.push_str(&format!(
                        "> #{} | {} · {scope} | {}\n",
                        m.id, m.category, m.content
                    ));
                }
            }
            out.push_str("---\n");
            out.push_str("! Kategori: arch, commands, conventions, decisions, issues, tasks, general\n");
            out.push_str("! Simpan project: /memory add [kategori] <catatan>\n");
            out.push_str("! Perbarui:       /memory update <id> [kategori] <catatan>\n");
            out.push_str("! Simpan global:  /memory global [kategori] <catatan>\n");
            out.push_str("! Hapus memori:   /memory rm <id>  (rahasia/token/password selalu ditolak)");
            return Ok(out);
        };
        if let Some(rest) = arg.strip_prefix("add ") {
            let (cat, text) = split_optional_category(rest.trim());
            if text.is_empty() {
                anyhow::bail!(
                    "isi memori kosong. Contoh: /memory add commands Jalankan cargo test sebelum rilis"
                );
            }
            let id = self.store.add_memory_categorized(&cwd_str, cat, text)?;
            return Ok(format!(
                "# Memori Proyek Disimpan\n* #{id} | {cat} · project | {text}"
            ));
        }
        if let Some(rest) = arg.strip_prefix("global ") {
            let (cat, text) = split_optional_category(rest.trim());
            if text.is_empty() {
                anyhow::bail!("isi memori global kosong.");
            }
            let id = self.store.add_memory_categorized("global", cat, text)?;
            return Ok(format!(
                "# Memori Global Disimpan\n* #{id} | {cat} · global | {text}"
            ));
        }
        if let Some(rest) = arg.strip_prefix("update ").or_else(|| arg.strip_prefix("edit ")) {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let id_raw = parts.next().unwrap_or("").trim_start_matches('#');
            let id: i64 = id_raw
                .parse()
                .map_err(|_| anyhow::anyhow!("id memori harus angka (mis. /memory update 1 arch ...)"))?;
            let tail = parts.next().unwrap_or("").trim();
            if tail.is_empty() {
                anyhow::bail!("isi memori baru kosong. Contoh: /memory update {id} [kategori] <catatan baru>");
            }
            let (explicit_cat, text) = split_explicit_category_opt(tail);
            if text.is_empty() {
                anyhow::bail!("isi memori baru kosong.");
            }
            if self.store.update_memory(id, explicit_cat, text)? {
                let cat_disp = explicit_cat.unwrap_or("(tetap)");
                return Ok(format!(
                    "# Memori Diperbarui\n* #{id} | {cat_disp} | {text}"
                ));
            }
            anyhow::bail!("memori #{id} tidak ditemukan");
        }
        if let Some(rest) = arg.strip_prefix("rm ").or_else(|| arg.strip_prefix("del ")) {
            let id: i64 = rest
                .trim()
                .trim_start_matches('#')
                .parse()
                .map_err(|_| anyhow::anyhow!("id memori harus angka (mis. /memory rm 1)"))?;
            if self.store.remove_memory(id)? {
                return Ok(format!("# Memori Dihapus\n> #{id} | dihapus"));
            }
            anyhow::bail!("memori #{id} tidak ditemukan");
        }
        Ok("# Cara Pakai /memory\n\
            ! /memory                              — lihat semua memori aktif\n\
            ! /memory add [kategori] <catatan>     — simpan memori proyek (arch/commands/conventions/decisions/issues/tasks)\n\
            ! /memory update <id> [kat] <catatan>  — perbarui memori berdasarkan ID\n\
            ! /memory global [kategori] <catatan>  — simpan memori global lintas proyek\n\
            ! /memory rm <id>                      — hapus memori berdasarkan ID"
            .to_string())
    }

    async fn handle_skills(&mut self, arg: Option<&str>) -> Result<String> {
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let skills = list_skills(&self.ctx.cwd);
            let auto_state = if self.ctx.auto_skills {
                "AKTIF"
            } else {
                "NONAKTIF"
            };
            let mut out = format!(
                "# Daftar Skills (.selencode/skills · auto self-improvement: {auto_state})\n"
            );
            if skills.is_empty() {
                out.push_str("! Belum ada skill tersimpan.\n");
            } else {
                for s in skills {
                    out.push_str(&format!("> {} | {} | {}\n", s.name, s.scope, s.summary));
                }
            }
            out.push_str("---\n");
            out.push_str("! Review otomatis sekarang: /skills review   |   Toggle auto: /skills auto\n");
            out.push_str("! Lihat / hapus skill:      /skills show <nama>   |   /skills rm <nama>\n");
            out.push_str("! Pakai / buat manual:      /skill <nama>   |   /learn <nama> <instruksi>");
            return Ok(out);
        };

        if arg.eq_ignore_ascii_case("auto") {
            let next = !self.ctx.auto_skills;
            self.ctx.auto_skills = next;
            self.cfg.agent.auto_skills = next;
            let saved = config::save(&self.cfg)?;
            let label = if next {
                "AKTIF (review otomatis setelah giliran kompleks >= 3 tool)"
            } else {
                "NONAKTIF (hanya buat skill saat diminta manual)"
            };
            return Ok(format!(
                "# Auto Self-Improvement Skills\n\
                 * status | {label} | tersimpan ke {}",
                saved.display()
            ));
        }

        if arg.eq_ignore_ascii_case("review") {
            if self.messages.is_empty() {
                return Ok(
                    "# Self-Improvement Review\n! Belum ada percakapan untuk ditinjau.".into(),
                );
            }
            match run_self_improvement_review(self.provider.clone(), &self.ctx, &self.messages)
                .await?
            {
                Some(summary) => Ok(format!("# {summary}\n* tersimpan | .selencode/skills/ | siap dipakai di sesi berikutnya")),
                None => Ok("# Self-Improvement Review\n! Belum ditemukan pola baru yang perlu disimpan atau di-patch sebagai skill.".into()),
            }
        } else if let Some(name) = arg.strip_prefix("show ").or_else(|| arg.strip_prefix("view ")) {
            let (info, content) = load_skill(&self.ctx.cwd, name.trim())?;
            let mut out = format!("# Skill: {} ({})\n", info.name, info.scope);
            for line in content.lines().take(40) {
                out.push_str(line);
                out.push('\n');
            }
            Ok(out)
        } else if let Some(name) = arg.strip_prefix("rm ").or_else(|| arg.strip_prefix("del ")) {
            let name = name.trim();
            if remove_skill(&self.ctx.cwd, name)? {
                Ok(format!("# Skill Dihapus\n> {name} | dihapus dari .selencode/skills/"))
            } else {
                anyhow::bail!("skill project '{name}' tidak ditemukan");
            }
        } else {
            // `/skills <nama>` sebagai pintasan lihat isi skill
            let (info, content) = load_skill(&self.ctx.cwd, arg)?;
            let mut out = format!("# Skill: {} ({})\n", info.name, info.scope);
            for line in content.lines().take(40) {
                out.push_str(line);
                out.push('\n');
            }
            Ok(out)
        }
    }

    fn handle_use_skill(&mut self, arg: Option<&str>) -> Result<String> {
        let name = arg
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("sebutkan nama skill. Contoh: /skill review"))?;
        let (info, content) = load_skill(&self.ctx.cwd, name)?;
        let sys = Message::text(
            Role::System,
            format!("[Skill aktif: {}]\n{content}", info.name),
        );
        let _ = self.store.save_message(&self.session_id, &sys);
        self.messages.push(sys);
        Ok(format!(
            "# Skill Diaktifkan\n* {} | {} | {}",
            info.name, info.scope, info.summary
        ))
    }

    fn handle_learn_skill(&self, arg: Option<&str>) -> Result<String> {
        let raw = arg
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "format: /learn <nama> <instruksi>. Contoh: /learn test Jalankan cargo test sebelum commit"
                )
            })?;
        let mut parts = raw.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let instr = parts.next().map(|s| s.trim()).unwrap_or("");
        if name.is_empty() || instr.is_empty() {
            anyhow::bail!(
                "format: /learn <nama> <instruksi>. Contoh: /learn test Jalankan cargo test sebelum commit"
            );
        }
        let path = save_skill(&self.ctx.cwd, name, instr)?;
        Ok(format!(
            "# Skill Baru Tersimpan\n* {name} | project | {}\n! Aktifkan kapan saja dengan: /skill {name}",
            path.display()
        ))
    }

    fn handle_init(&self) -> Result<String> {
        let (path, created) = init_agents_md(&self.ctx.cwd)?;
        if created {
            Ok(format!(
                "# AGENTS.md Dibuat\n* dibuat | {} | otomatis dibaca Selena di sesi berikutnya",
                path.display()
            ))
        } else {
            Ok(format!(
                "# AGENTS.md Sudah Ada\n! File {} sudah ada (tidak ditimpa).",
                path.display()
            ))
        }
    }

    fn handle_approve(&mut self, arg: Option<&str>) -> Result<String> {
        let policy = self.ctx.approver.policy();
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let (mode, mut cats) = {
                let p = policy.lock().expect("approval policy poisoned");
                let cats: Vec<String> = p.session_allowed.iter().cloned().collect();
                (p.mode, cats)
            };
            cats.sort();
            let allowed_str = if cats.is_empty() {
                "(belum ada)".to_string()
            } else {
                cats.join(", ")
            };
            return Ok(format!(
                "# Pengaturan Persetujuan (Approval)\n\
                 * mode | {} | aktif sesi ini\n\
                 > izin sesi | {allowed_str} | diizinkan via tombol [a]\n\
                 ---\n\
                 ! /approve edits — otomatis izinkan edit/tulis file (tetap tanya shell/git/web)\n\
                 ! /approve auto  — otomatis izinkan semua aksi aman sesi ini (.env & perintah destruktif tetap diblokir)\n\
                 ! /approve ask   — kembali tanya per aksi & reset izin sesi",
                mode.label()
            ));
        };

        let (new_mode, reset_cats) = match arg.to_ascii_lowercase().as_str() {
            "ask" => (ApprovalMode::Ask, true),
            "edits" | "edit" => (ApprovalMode::AutoEdits, false),
            "auto" | "all" | "yes" => (ApprovalMode::AutoAll, false),
            other => anyhow::bail!("mode '{other}' tidak dikenal. Pilihan: ask, edits, auto"),
        };
        {
            let mut p = policy.lock().expect("approval policy poisoned");
            p.mode = new_mode;
            if reset_cats {
                p.session_allowed.clear();
            }
        }
        Ok(format!(
            "# Mode Persetujuan Diubah\n\
             * mode | {} | berlaku langsung di sesi ini\n\
             ! Untuk permanen di config, isi `approval = \"{}\"` di bagian `[agent]` selencode.toml.",
            new_mode.label(),
            match new_mode {
                ApprovalMode::Ask => "ask",
                ApprovalMode::AutoEdits => "edits",
                ApprovalMode::AutoAll => "auto",
            }
        ))
    }

    fn handle_export(&self, arg: Option<&str>) -> Result<String> {
        let path = export_conversation_md(
            &self.ctx.cwd,
            arg,
            &self.provider_name,
            &self.pcfg.model,
            &self.messages,
        )?;
        Ok(format!(
            "# Percakapan Diekspor\n* markdown | {} | {} pesan diekspor",
            path.display(),
            self.messages.len()
        ))
    }

    fn provider_usage_footer(&self) -> String {
        "! Preset siap pakai:     /provider preset   |   /provider preset <id> [model] [ENV|key]\n\
         ! Tambah OpenAI-compat:  /provider add <nama> openai <model> <base_url> <ENV_atau_key>\n\
         ! Tambah Claude/Gemini:  /provider add <nama> <anthropic|gemini> <model> <ENV_atau_key>\n\
         ! Ubah model / default:  /provider model <nama> <id-model>   |   /provider default <nama>"
            .to_string()
    }

    fn provider_usage_menu(&self) -> String {
        format!(
            "# Cara Pakai /provider\n\
             {}\n\
             ---\n\
             ! Contoh OpenRouter: /provider add openrouter openai anthropic/claude-3.7-sonnet https://openrouter.ai/api/v1 OPENROUTER_API_KEY\n\
             ! Contoh Gemini:     /provider add gemini gemini gemini-2.5-pro GEMINI_API_KEY",
            self.provider_usage_footer()
        )
    }

    fn print_models(&self) {
        println!("{}", strip_menu_markup(&self.models_menu()));
    }

    /// Kartu daftar sesi terakhir cwd ini (mode TUI & polos).
    fn sessions_menu(&self) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let list = self.store.list_sessions(&cwd_str, 10)?;
        if list.is_empty() {
            return Ok(
                "# Sesi Tersimpan\n! Belum ada sesi tersimpan untuk folder ini.".to_string(),
            );
        }
        let mut out = String::from("# 10 Sesi Terakhir di Folder Ini\n");
        for s in list {
            let mark = if s.id == self.session_id { "*" } else { ">" };
            let short_ts: String = s.created_at.chars().take(16).collect();
            out.push_str(&format!(
                "{mark} {short_ts} | {}/{} | \"{}\"\n",
                s.provider, s.model, s.title
            ));
        }
        out.push_str("---\n");
        out.push_str("! Lanjutkan sesi terakhir dari terminal: selen resume");
        Ok(out)
    }

    fn session_info_menu(&self) -> String {
        let est = estimate_tokens(&self.messages);
        let lim = self.ctx.context_limit_tokens;
        let pct = calc_context_pct(est, lim);
        format!(
            "# Info Sesi Aktif\n\
             * sesi | {} | {}\n\
             > provider | {} | model {} (kind {})\n\
             > konteks | {} / {} ({pct}%) | kapasitas model AI\n\
             > folder | {} | {} pesan di memori",
            self.session_id.chars().take(12).collect::<String>(),
            self.usage_line(),
            self.provider_name,
            self.pcfg.model,
            self.pcfg.kind,
            format_context_compact(est),
            format_context_compact(lim),
            self.ctx.cwd.display(),
            self.messages.len(),
        )
    }

    fn print_sessions(&self) -> Result<()> {
        println!("{}", strip_menu_markup(&self.sessions_menu()?));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    selencode::update::cleanup_old_binary();
    let cli = Cli::parse();
    match cli.command {
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
        Some(Commands::Resume) => interactive(cli, true).await,
        Some(Commands::Telegram) => run_telegram_gateway(cli).await,
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
    cancel: Arc<tokio::sync::Notify>,
    interrupt_pending: Arc<AtomicBool>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            TuiCommand::Prompt(prompt) | TuiCommand::InterruptPrompt(prompt) => {
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
                            let _ = ui_tx.send(UiEvent::Info("Giliran dihentikan.".into())).await;
                            let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
                        }
                    }
                }
            }
            TuiCommand::PromptPlan(prompt) => {
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
            TuiCommand::Telegram(arg) => match s.handle_telegram(arg.as_deref()) {
                Ok(card) => {
                    let _ = ui_tx.send(UiEvent::Menu(card)).await;
                }
                Err(e) => {
                    let _ = ui_tx.send(UiEvent::Error(format!("{e}"))).await;
                }
            },
            TuiCommand::NewSession => match s.reset_session() {
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
            },
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
            TuiCommand::ShowSessionInfo => {
                let _ = ui_tx.send(UiEvent::Menu(s.session_info_menu())).await;
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
    let s = Session::build(
        cli.provider.as_deref(),
        cli.yes,
        resume,
        None,
        Some(approval_tx),
        true,
    )
    .await?;
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
    app.plan_next = cli.plan;
    if let Some(note) = &s.resumed_note {
        app.info(note.clone());
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
        cancel,
        interrupt_pending,
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

async fn interactive_plain(cli: Cli, resume: bool) -> Result<()> {
    let mut s = Session::build(cli.provider.as_deref(), cli.yes, resume, None, None, true).await?;
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
            SlashCommand::Telegram(arg) => match s.handle_telegram(arg.as_deref()) {
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
            SlashCommand::Memory(arg) => match s.handle_memory(arg.as_deref()) {
                Ok(card) => println!("{}", strip_menu_markup(&card)),
                Err(e) => eprintln!("[error] {e}"),
            },
            SlashCommand::Skills(arg) => match s.handle_skills(arg.as_deref()).await {
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
            SlashCommand::Details => {
                println!("Perintah /details hanya berlaku pada mode TUI.");
            }
            SlashCommand::New => match s.reset_session() {
                Ok(sid) => println!("Sesi baru dimulai ({sid})."),
                Err(e) => eprintln!("[error] {e}"),
            },
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
            SlashCommand::Resume => {
                println!("{}", strip_menu_markup(&s.session_info_menu()));
            }
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
                    Ok(_) => println!("[{}]", s.usage_line()),
                    Err(e) => eprintln!("[error] {e}"),
                }
            }
        }
    }
    Ok(())
}

/// Pisahkan kategori eksplisit di awal argumen `/memory add [kat] <teks>`.
/// Bila kata pertama bukan kategori resmi, kategori default `"general"` dan seluruh teks dipertahankan.
fn split_optional_category(s: &str) -> (&'static str, &str) {
    let mut parts = s.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    if let (Some(cat), Some(rest)) = (selencode::store::parse_explicit_category(first), parts.next()) {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return (cat, trimmed);
        }
    }
    ("general", s.trim())
}

/// Sama seperti `split_optional_category`, tapi mengembalikan `None` untuk kategori bila tidak disebut
/// secara eksplisit (dipakai `/memory update <id> [kat] <teks>` agar kategori lama tetap terjaga).
fn split_explicit_category_opt(s: &str) -> (Option<&'static str>, &str) {
    let mut parts = s.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    if let (Some(cat), Some(rest)) = (selencode::store::parse_explicit_category(first), parts.next()) {
        let trimmed = rest.trim();
        if !trimmed.is_empty() {
            return (Some(cat), trimmed);
        }
    }
    (None, s.trim())
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

async fn run_telegram_gateway(cli: Cli) -> Result<()> {
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
