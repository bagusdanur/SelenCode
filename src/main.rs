use anyhow::Result;
use clap::{Parser, Subcommand};
use selencode::agent::context::{compact_messages, estimate_tokens};
use selencode::agent::plan::{approved_plan_prompt, extract_plan, TurnMode};
use selencode::agent::{
    run_self_improvement_review, run_turn, run_turn_mode, run_turn_ui, run_turn_ui_mode, UiEvent,
};
use selencode::config::{self, Config, ProviderCfg};
use selencode::providers::anthropic::AnthropicProvider;
use selencode::providers::gemini::GeminiProvider;
use selencode::providers::openai::{fetch_models, OpenAiProvider};
use selencode::providers::{Message, Provider, Role};
use selencode::repl::{
    help_menu_markup, parse_model_action, parse_provider_action, parse_slash_command,
    strip_menu_markup, KeySpec, ModelAction, ProviderAction, SlashCommand, PROVIDER_PRESETS,
};
use selencode::security::{
    is_forbidden_command, redact_secrets, ApprovalMode, ApprovalPolicy, ApprovalRequest, Approver,
};
use selencode::skills::{
    export_conversation_md, init_agents_md, list_skills, load_skill, remove_skill, save_skill,
};
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

        let mode = ApprovalMode::from_config(&cfg.agent.approval, yes);
        let policy = Arc::new(std::sync::Mutex::new(ApprovalPolicy::new(mode)));
        let approver = match approval_tx {
            Some(tx) => Arc::new(Approver::channel_with_policy(policy, tx)),
            None => Arc::new(Approver::stdin_with_policy(policy)),
        };
        let ctx = Ctx::new(cwd, mode == ApprovalMode::AutoAll)
            .with_approver(approver)
            .with_context_limit(cfg.agent.context_limit_tokens)
            .with_auto_skills(cfg.agent.auto_skills)
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
                let mut added = 0usize;
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = p.effective_models();
                    for m in &items {
                        if p.push_model_unique(m) {
                            added += 1;
                        }
                    }
                    self.pcfg = p.clone();
                }
                let saved_path = config::save(&self.cfg)?;
                let mut out = format!(
                    "# Daftar Model Diperbarui (+{added} baru, tersimpan ke {})\n",
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
                out.push_str("! Ganti cepat lewat nomor: /model <nomor>");
                Ok(out)
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

    /// Tarik daftar model real-time dari endpoint `/models` provider aktif,
    /// **otomatis menyimpannya** ke daftar bernomor di config, lalu tampilkan
    /// (opsional disaring dengan kata kunci `filter`).
    async fn fetch_remote_models_menu(&mut self, filter: Option<&str>) -> Result<String> {
        if self.pcfg.kind != "openai" {
            return Ok(format!(
                "# Daftar Model ({})\n\
                 ! Provider kind '{}' tidak menyediakan endpoint /models standar OpenAI.\n\
                 ! Tambah manual ke daftar bernomor: /model add <model-1> <model-2>\n\
                 ! Lalu pilih cepat dengan: /model <nomor>",
                self.provider_name, self.pcfg.kind
            ));
        }
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(&self.provider_name))?;
        let base_url = pcfg
            .base_url
            .ok_or_else(|| anyhow::anyhow!("base_url kosong untuk provider '{}'", self.provider_name))?;
        let mut ids = fetch_models(&base_url, &api_key).await?;
        if !self.pcfg.model.trim().is_empty() && !ids.iter().any(|m| m == &self.pcfg.model) {
            ids.insert(0, self.pcfg.model.clone());
        }
        // Simpan otomatis ke config agar langsung bisa dipilih via `/model <nomor>`!
        if !ids.is_empty() {
            if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                p.models = ids.clone();
                self.pcfg = p.clone();
            }
            let _ = config::save(&self.cfg);
        }

        let needle = filter.map(|f| f.trim().to_ascii_lowercase()).filter(|f| !f.is_empty());
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
                "# Model {} cocok \"{f}\" ({}/{} tersimpan)\n",
                self.provider_name,
                shown.len(),
                ids.len()
            ),
            None => format!(
                "# Daftar Model {} ({} tersimpan ke config)\n",
                self.provider_name,
                ids.len()
            ),
        };
        let mut out = header;
        if shown.is_empty() {
            out.push_str("! Tidak ada model yang cocok.\n");
        } else {
            for (idx, id) in shown.iter().take(40) {
                let active = **id == self.pcfg.model;
                let mark = if active { "*" } else { ">" };
                let note = if active { "aktif" } else { "" };
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
        out.push_str("! Pilih cepat lewat nomor: /model <nomor>   (atau /model set <nomor>)");
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
        let mut out = format!(
            "# Model di Provider {} ({} tersimpan)\n",
            self.provider_name,
            list.len()
        );
        for (i, m) in list.iter().take(30).enumerate() {
            let active = *m == self.pcfg.model;
            let mark = if active { "*" } else { ">" };
            let note = if active { "aktif" } else { "" };
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
        out.push_str("! Ganti cepat via nomor: /model 2   atau   /model set 2   atau   /model claude\n");
        out.push_str("! Tarik & simpan API:    /models [filter]   |   Tambah manual: /model add <id>\n");
        out.push_str("! Pindah provider:       /model <provider>   |   Pasang provider: /provider");
        out
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

    fn handle_memory(&self, arg: Option<&str>) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let list = self.store.list_memories(&cwd_str, 30)?;
            let mut out = String::from("# Memori Persisten Selena\n");
            if list.is_empty() {
                out.push_str("! Belum ada memori tersimpan.\n");
            } else {
                for m in list {
                    let scope = if m.scope == "global" { "global" } else { "project" };
                    out.push_str(&format!("> #{} | {scope} | {}\n", m.id, m.content));
                }
            }
            out.push_str("---\n");
            out.push_str("! Simpan memori project: /memory add <catatan>\n");
            out.push_str("! Simpan memori global:  /memory global <catatan>\n");
            out.push_str("! Hapus memori:          /memory rm <id>");
            return Ok(out);
        };
        if let Some(rest) = arg.strip_prefix("add ") {
            let text = rest.trim();
            if text.is_empty() {
                anyhow::bail!("isi memori kosong. Contoh: /memory add Selalu pakai bahasa Indonesia");
            }
            let id = self.store.add_memory(&cwd_str, text)?;
            return Ok(format!(
                "# Memori Project Disimpan\n* #{id} | project | {text}"
            ));
        }
        if let Some(rest) = arg.strip_prefix("global ") {
            let text = rest.trim();
            if text.is_empty() {
                anyhow::bail!("isi memori global kosong.");
            }
            let id = self.store.add_memory("global", text)?;
            return Ok(format!(
                "# Memori Global Disimpan\n* #{id} | global | {text}"
            ));
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
            ! /memory                  — lihat semua memori aktif\n\
            ! /memory add <catatan>    — simpan memori khusus project ini\n\
            ! /memory global <catatan> — simpan memori global lintas project\n\
            ! /memory rm <id>          — hapus memori berdasarkan ID"
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
        format!(
            "# Info Sesi Aktif\n\
             * sesi | {} | {}\n\
             > provider | {} | model {} (kind {})\n\
             > folder | {} | {} pesan di memori",
            self.session_id.chars().take(12).collect::<String>(),
            self.usage_line(),
            self.provider_name,
            self.pcfg.model,
            self.pcfg.kind,
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
    cancel: Arc<tokio::sync::Notify>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            TuiCommand::Prompt(prompt) => {
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
                        let _ = ui_tx.send(UiEvent::Info("Giliran dibatalkan oleh user (Esc).".into())).await;
                        let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
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
                        let _ = ui_tx.send(UiEvent::Info("Giliran plan dibatalkan (Esc).".into())).await;
                        let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
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
                        let _ = ui_tx.send(UiEvent::Info("Eksekusi rencana dibatalkan (Esc).".into())).await;
                        let _ = ui_tx.send(UiEvent::TurnDone { usage: None }).await;
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
    app.approval_policy = Some(s.ctx.approver.policy());
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
    let cancel = Arc::new(tokio::sync::Notify::new());
    let worker = tokio::spawn(tui_worker(s, cmd_rx, ui_tx, cancel.clone()));
    let channels = TuiChannels {
        cmd_tx,
        event_rx: ui_rx,
        approval_rx,
        cancel,
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
