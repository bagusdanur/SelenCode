use super::{make_review_provider, make_routed_provider};
use anyhow::Result;
use selencode::agent::context::{
    compact_messages, compact_messages_with_llm, estimate_tokens, format_context_compact,
    format_context_tokens_exact, infer_model_context_limit, sanitize_interrupted_messages,
};
use selencode::agent::ui::{ModelPickerData, ResumePickerData, ResumePickerItem};
use selencode::config::{self, Config, ProviderCfg};
use selencode::providers::anthropic;
use selencode::providers::gemini;
use selencode::providers::openai::fetch_models_with_context;
use selencode::providers::router::RouterTelemetry;
use selencode::providers::{Block, Message, Provider, Role, UnconfiguredProvider};
use selencode::repl::{
    classify_key_spec, lookup_saved_model, parse_model_action, parse_provider_action, KeySpec,
    ModelAction, ProviderAction, SavedModelLookup,
};
use selencode::security::{ApprovalMode, ApprovalPolicy, ApprovalRequest, Approver};
use selencode::store::Store;
use selencode::tools::delegate::Delegate;
use selencode::tools::orchestrate::Orchestrate;
use selencode::tools::{default_registry_with_shell, read_only_registry, Ctx, Registry};
use selencode::tui::app::SetupResult;
use std::collections::HashMap;
use std::sync::Arc;
pub(crate) struct Session {
    pub(crate) cfg: Config,
    pub(crate) provider_name: String,
    pub(crate) pcfg: ProviderCfg,
    pub(crate) provider: Arc<dyn Provider>,
    /// Handle provider bersama untuk subagent (`delegate` & `orchestrate`) agar `/model` ikut menyinkronkannya (P2.2).
    pub(crate) shared_provider: Arc<std::sync::RwLock<Arc<dyn Provider>>>,
    /// Root workspace awal saat sesi dibuat (batas sandbox `/cd` saat mode != `unrestricted`).
    pub(crate) workspace_root: std::path::PathBuf,
    pub(crate) ctx: Ctx,
    pub(crate) registry: Registry,
    pub(crate) store: Arc<Store>,
    pub(crate) session_id: String,
    pub(crate) messages: Vec<Message>,
    /// Catatan "Melanjutkan sesi: ..." bila resume memuat sesi lama.
    pub(crate) resumed_note: Option<String>,
    /// Cache katalog `/models` terakhir di memori (TIDAK otomatis menimpa config,
    /// agar user bisa memilih 1-per-1 lewat `/model add <nomor>` atau modal `/model`).
    pub(crate) last_remote_models: Vec<String>,
    /// Cache kapasitas context window model yang dilaporkan endpoint API `/models`.
    pub(crate) model_context_cache: HashMap<String, usize>,
    /// True bila belum ada provider/API key yang siap -> langsung buka Wizard `/setup`.
    pub(crate) needs_setup: bool,
    pub(crate) setup_reason: Option<String>,
    /// Telemetri latensi, biaya, retry, fallback, & error Dynamic Model Routing (Tahap 3).
    pub(crate) router_telemetry: Arc<std::sync::Mutex<RouterTelemetry>>,
    /// Jumlah giliran tuntas sejak background review terakhir pada sesi ini.
    pub(crate) turns_since_review: usize,
    /// Panjang `messages` tertinggi yang sudah dijadwalkan ke review latar pada sesi ini
    /// (penjaga anti-tembak ganda di akhir sesi, Masalah 1 v0.5.25).
    pub(crate) last_scheduled_review_msg_len: usize,
}

/// Snapshot ringan yang siap dijalankan di task latar (`tokio::spawn`) untuk Background Review v0.5.1.
pub(crate) struct BgReviewJob {
    provider: Arc<dyn Provider>,
    cfg: Config,
    store: Arc<Store>,
    cwd: std::path::PathBuf,
    session_id: String,
    messages: Vec<Message>,
}

impl BgReviewJob {
    pub(crate) async fn run(
        self,
        mark_delivered: bool,
    ) -> Result<Option<selencode::agent::BgReviewOutcome>> {
        selencode::agent::run_background_review(
            self.provider,
            &self.cfg,
            &self.store,
            &self.cwd,
            &self.session_id,
            &self.messages,
            mark_delivered,
        )
        .await
    }
}

/// Hasil pemulihan sesi saat user menjalankan `/resume <nomor>`.
#[derive(Debug, Clone)]
pub(crate) struct ResumedSessionData {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) context_used: usize,
    pub(crate) context_limit: usize,
    pub(crate) preview_lines: Vec<(String, String)>,
    pub(crate) total_messages: usize,
}

impl Session {
    /// Judul sesi: ringkasan kontekstual dari prompt pertama (maks ~56 karakter).
    pub(crate) fn title_from(prompt: &str) -> String {
        selencode::store::summarize_prompt_title(prompt)
    }

    /// Perbaikan 2 (v0.5.9): jalankan 1 putaran polling atas pantauan `watch_job` aktif di ruang kerja ini
    /// saat sesi CLI/TUI dibuka (+ ambil laporan terminal tertunda), persis 1x per perubahan (0 token).
    pub(crate) async fn poll_cli_watches_on_open(&self) -> Vec<String> {
        selencode::watch::poll_active_watches_for_cli(
            &self.store,
            &self.ctx.sandbox,
            &self.ctx.cwd,
            true,
            true,
        )
        .await
    }

    /// Perbaikan 2 (v0.5.9): jalankan 1 putaran polling sebelum sesi CLI/TUI ditutup bersih
    /// hanya bila masih ada pantauan aktif di folder ini.
    pub(crate) async fn poll_cli_watches_on_close(&self, print_immediately: bool) -> Vec<String> {
        let target_cwd =
            selencode::watch::normalize_watch_cwd(&self.ctx.cwd.to_string_lossy());
        let has_active_here = self
            .store
            .list_job_watches(None, true)
            .map(|ws| {
                ws.iter()
                    .any(|w| selencode::watch::normalize_watch_cwd(&w.cwd) == target_cwd)
            })
            .unwrap_or(false);
        if !has_active_here {
            return Vec::new();
        }
        selencode::watch::poll_active_watches_for_cli(
            &self.store,
            &self.ctx.sandbox,
            &self.ctx.cwd,
            print_immediately,
            false,
        )
        .await
    }

    pub(crate) async fn build(
        provider_name: Option<&str>,
        yes: bool,
        resume: bool,
        first_prompt: Option<&str>,
        approval_tx: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
        allow_unconfigured: bool,
    ) -> Result<Session> {
        Self::build_inner(
            provider_name,
            yes,
            resume,
            resume,
            None,
            None,
            first_prompt,
            approval_tx,
            allow_unconfigured,
        )
        .await
    }

    pub(crate) async fn build_with_target(
        provider_name: Option<&str>,
        yes: bool,
        resume: bool,
        explicit_resume: bool,
        resume_target: Option<&str>,
        first_prompt: Option<&str>,
        approval_tx: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
        allow_unconfigured: bool,
    ) -> Result<Session> {
        Self::build_inner(
            provider_name,
            yes,
            resume,
            explicit_resume,
            resume_target,
            None,
            first_prompt,
            approval_tx,
            allow_unconfigured,
        )
        .await
    }

    /// Bangun atau pulihkan sesi persisten untuk satu `chat_id` Telegram lintas restart / idle eviction.
    pub(crate) async fn build_for_telegram_chat(
        provider_name: Option<&str>,
        yes: bool,
        chat_id: i64,
        first_prompt: Option<&str>,
        approval_tx: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
    ) -> Result<Session> {
        Self::build_inner(
            provider_name,
            yes,
            true,
            false,
            None,
            Some(chat_id),
            first_prompt,
            approval_tx,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_inner(
        provider_name: Option<&str>,
        yes: bool,
        resume: bool,
        explicit_resume: bool,
        resume_target: Option<&str>,
        telegram_chat_id: Option<i64>,
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
                        fallback: Vec::new(),
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
        // Backfill FTS5 satu kali di thread latar (Bagian 4A v0.5.6 — non-blocking, tidak mengunci start sesi!).
        let store_fts = store.clone();
        std::thread::spawn(move || {
            let _ = store_fts.backfill_fts_if_needed();
        });

        let (session_id, messages, resumed_title) = if let Some(target) = resume_target
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let info = match store.session_by_id(target)? {
                Some(found) => found,
                None => store.resolve_session(&cwd_str, target, 10)?,
            };
            let mut msgs = store.load_messages(&info.id)?;
            if let Some(syn) = sanitize_interrupted_messages(&mut msgs) {
                let _ = store.save_message(&info.id, &syn);
            }
            (info.id, msgs, Some((info.title, info.created_at)))
        } else if let Some(chat_id) = telegram_chat_id {
            match store.resolve_telegram_resume_session(chat_id, &cwd_str)? {
                Some(info) => {
                    let mut msgs = store.load_messages(&info.id)?;
                    if let Some(syn) = sanitize_interrupted_messages(&mut msgs) {
                        let _ = store.save_message(&info.id, &syn);
                    }
                    (info.id, msgs, Some((info.title, info.created_at)))
                }
                None => {
                    let title = Self::title_from(first_prompt.unwrap_or(""));
                    let id = if first_prompt.is_some() {
                        store.create_session(&cwd_str, &name, &pcfg.model, &title)?
                    } else {
                        store.register_lazy_session(&cwd_str, &name, &pcfg.model, &title)
                    };
                    (id, Vec::new(), None)
                }
            }
        } else if resume {
            match store.resolve_cli_resume_session(&cwd_str, explicit_resume)? {
                Some(info) => {
                    let mut msgs = store.load_messages(&info.id)?;
                    if let Some(syn) = sanitize_interrupted_messages(&mut msgs) {
                        let _ = store.save_message(&info.id, &syn);
                    }
                    (info.id, msgs, Some((info.title, info.created_at)))
                }
                None => {
                    let title = Self::title_from(first_prompt.unwrap_or(""));
                    let id = if first_prompt.is_some() {
                        store.create_session(&cwd_str, &name, &pcfg.model, &title)?
                    } else {
                        store.register_lazy_session(&cwd_str, &name, &pcfg.model, &title)
                    };
                    (id, Vec::new(), None)
                }
            }
        } else {
            let title = Self::title_from(first_prompt.unwrap_or(""));
            let id = if first_prompt.is_some() {
                store.create_session(&cwd_str, &name, &pcfg.model, &title)?
            } else {
                store.register_lazy_session(&cwd_str, &name, &pcfg.model, &title)
            };
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
        let model_context_cache = store.load_model_context_cache();
        let init_ctx_limit = pcfg
            .context_limit
            .filter(|&n| n > 0)
            .or_else(|| model_context_cache.get(&pcfg.model).copied().filter(|&n| n > 0))
            .unwrap_or_else(|| {
                if cfg.agent.context_limit_tokens != 100_000 && cfg.agent.context_limit_tokens > 0 {
                    cfg.agent.context_limit_tokens
                } else {
                    infer_model_context_limit(&pcfg.model)
                }
            });
        let sandbox = Arc::new(selencode::sandbox::SandboxController::from_config(
            &cfg.sandbox,
            selencode::sandbox::default_audit_log_path(),
        ));
        selencode::skills::seed_default_user_skills();
        let hooks = Arc::new(selencode::hooks::HookEngine::from_config(&cfg.hooks));
        // Perbaikan 3 (v0.5.9): migrasi satu kali entri memori lama (SQLite `memories`) ke store persisten `memory.md`
        // sebelum snapshot beku dimuat, menghormati pagu karakter tanpa memotong diam-diam.
        if let Ok(mig) = selencode::user_memory::migrate_legacy_sqlite_memories(
            &store,
            cfg.memory.max_memory_chars,
        ) {
            if let Some(warn) = mig.warning {
                eprintln!("[selen] {warn}");
            }
        }
        let frozen_user_mem = selencode::user_memory::load_frozen_snapshot(
            cfg.memory.max_memory_chars,
            cfg.memory.max_profile_chars,
        );
        let is_resumed = resumed_note.is_some();
        let ctx = Ctx::new(cwd, mode == ApprovalMode::AutoAll)
            .with_approver(approver)
            .with_context_limit(init_ctx_limit)
            .with_auto_skills(cfg.agent.auto_skills)
            .with_self_correct(cfg.agent.self_correct)
            .with_sandbox(sandbox)
            .with_hooks(hooks)
            .with_guard(cfg.guard.clone())
            .with_tool_budget(cfg.tool_budget.clone())
            .with_skills(cfg.skills.clone())
            .with_router_telemetry(router_telemetry.clone())
            .with_ask_timeout_only(cfg.ask.timeout_secs)
            .with_memory_cfg(cfg.memory.clone())
            .with_web_cfg(cfg.web.clone())
            .with_context_cfg(cfg.context.clone())
            .with_tool_preview_length(cfg.display.tool_preview_length)
            .with_frozen_user_memory(frozen_user_mem)
            .with_resumed_session(is_resumed)
            .with_store(store.clone(), session_id.clone());
        // P2.2: subagent `delegate` & `orchestrate` berbagi handle `shared_provider`
        // sehingga ganti provider/model lewat `/model` otomatis memperbarui subagent juga!
        let shared_provider = Arc::new(std::sync::RwLock::new(provider.clone()));
        let mut registry = default_registry_with_shell(&cfg.shell)
            .register(Arc::new(
                Delegate::new_shared(
                    shared_provider.clone(),
                    Arc::new(read_only_registry()),
                    cfg.agent.max_parallel_subagents,
                )
                .with_shell_cfg(cfg.shell.clone()),
            ))
            .register(Arc::new(Orchestrate::new_shared(
                shared_provider.clone(),
                cfg.shell.clone(),
                cfg.agent.max_parallel_subagents,
            )));
        // Tool MCP (Fase 7): HANYA registry induk mode Execute — tidak ikut
        // registry baca-saja plan/subagent. Server yang gagal start dilewati
        // dengan peringatan stderr; build_mcp_tools tidak mengembalikan Err,
        // jadi start selen tidak pernah digagalkan oleh server MCP.
        for tool in selencode::mcp::build_mcp_tools(&cfg).await {
            registry = registry.register(tool);
        }
        let last_scheduled_review_msg_len =
            store.last_reviewed_msg_count(&session_id).unwrap_or(0);
        let workspace_root = selencode::security::normalize_sandbox_path(&ctx.cwd);
        Ok(Session {
            cfg,
            provider_name: name,
            pcfg,
            provider,
            shared_provider,
            workspace_root,
            ctx,
            registry,
            store,
            session_id,
            messages,
            resumed_note,
            last_remote_models: Vec::new(),
            model_context_cache,
            needs_setup,
            setup_reason,
            router_telemetry,
            turns_since_review: 0,
            last_scheduled_review_msg_len,
        })
    }

    /// Terapkan hasil Wizard `/setup`: simpan provider baru ke config, jadikan default,
    /// dan langsung aktifkan di sesi berjalan tanpa perlu restart `selen`.
    pub(crate) fn apply_setup_result(&mut self, res: SetupResult) -> Result<String> {
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
            fallback: Vec::new(),
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
    pub(crate) fn resolve_context_limit(&self) -> usize {
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

    pub(crate) fn model_context_for(&self, model_id: &str) -> usize {
        self.model_context_cache
            .get(model_id)
            .copied()
            .filter(|&n| n > 0)
            .unwrap_or_else(|| infer_model_context_limit(model_id))
    }

    /// Ganti provider aktif (perintah /model <nama>).
    pub(crate) fn switch_provider(&mut self, name: &str) -> Result<()> {
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(name))?;
        let provider = make_routed_provider(
            &self.cfg,
            name,
            &pcfg,
            &api_key,
            self.router_telemetry.clone(),
        )?;
        if let Ok(mut guard) = self.shared_provider.write() {
            *guard = provider.clone();
        }
        self.provider = provider;
        self.provider_name = name.to_string();
        self.pcfg = pcfg;
        self.ctx.context_limit_tokens = self.resolve_context_limit();
        self.store
            .update_session_provider(&self.session_id, name, &self.pcfg.model)?;
        Ok(())
    }

    /// Ganti provider aktif sekaligus menyimpannya sebagai `default_provider` di `config.toml`
    /// agar tidak tertimpa balik saat giliran Telegram berikutnya menyinkronkan config atau setelah restart.
    pub(crate) fn switch_provider_and_persist(&mut self, name: &str) -> Result<()> {
        self.switch_provider(name)?;
        self.cfg.default_provider = name.to_string();
        let _ = config::save(&self.cfg)?;
        Ok(())
    }

    /// Tangani `/model <arg>`: pilih lewat nomor (`/model 2` atau `/model set 2`),
    /// tambah daftar (`/model add <id...>`), hapus (`/model rm <no|id>`),
    /// ganti provider, input model manual, atau cari cepat lewat potongan nama.
    pub(crate) fn apply_model_arg(&mut self, arg: &str) -> Result<String> {
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
                self.cfg.default_provider = self.provider_name.clone();
                let saved_path = config::save(&self.cfg)?;
                let pname = self.provider_name.clone();
                self.switch_provider(&pname)?;
                let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
                Ok(format!(
                    "# Model Dipilih (#{idx})\n\
                     * model | {} | aktif di {} ({})\n\
                     > konteks | {ctx_s} | tersimpan ({})",
                    self.pcfg.model,
                    self.provider_name,
                    self.pcfg.kind,
                    saved_path.display()
                ))
            }
            ModelAction::SetCurrentModel(model) => {
                if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                    p.models = p.effective_models();
                    p.push_model_unique(&model);
                    p.model = model.clone();
                }
                self.cfg.default_provider = self.provider_name.clone();
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
                let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
                Ok(format!(
                    "# Model Diperbarui (#{pos})\n\
                     * model | {} | aktif di {} ({})\n\
                     > konteks | {ctx_s} | tersimpan ({})",
                    self.pcfg.model,
                    self.provider_name,
                    self.pcfg.kind,
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
                    // Dukung ID model ber-titik-dua pada provider aktif (mis. Ollama `llama3:8b`, `deepseek-r1:70b`)
                    // sebelum menolaknya sebagai nama provider yang tidak dikenal.
                    let combined = format!("{provider}:{model}");
                    let cur_list = self.pcfg.effective_models();
                    if let SavedModelLookup::Exact(i) | SavedModelLookup::UniqueSubstring(i) =
                        lookup_saved_model(&cur_list, &combined)
                    {
                        let idx = i + 1;
                        let chosen = cur_list[i].clone();
                        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                            p.models = cur_list;
                            p.model = chosen;
                        }
                        self.cfg.default_provider = self.provider_name.clone();
                        let saved_path = config::save(&self.cfg)?;
                        let pname = self.provider_name.clone();
                        self.switch_provider(&pname)?;
                        let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
                        return Ok(format!(
                            "# Model Dipilih (#{idx})\n\
                             * model | {} | aktif di {} ({})\n\
                             > konteks | {ctx_s} | tersimpan ({})",
                            self.pcfg.model,
                            self.provider_name,
                            self.pcfg.kind,
                            saved_path.display()
                        ));
                    }
                    anyhow::bail!(
                        "provider '{provider}' belum ada (dan model '{combined}' tidak ada di daftar tersimpan '{}'). Tambah provider lewat `/provider add` atau tambah model lewat `/model add {combined}`.",
                        self.provider_name
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
                        match lookup_saved_model(&list, &model) {
                            SavedModelLookup::Exact(i) | SavedModelLookup::UniqueSubstring(i) => {
                                list[i].clone()
                            }
                            SavedModelLookup::Ambiguous(idxs) => {
                                let preview: Vec<String> = idxs
                                    .iter()
                                    .take(5)
                                    .map(|&i| format!("#{} {}", i + 1, list[i]))
                                    .collect();
                                anyhow::bail!(
                                    "potongan '{model}' cocok {} model di '{provider}': {}. Pilih lebih spesifik atau pakai nomor.",
                                    idxs.len(),
                                    preview.join(", ")
                                );
                            }
                            SavedModelLookup::NotFound => {
                                anyhow::bail!(
                                    "model '{model}' tidak ada di daftar tersimpan provider '{provider}'.\nCek daftar lewat `/model`, intip API lewat `/models`, atau tambah manual dengan `/model add {model}`."
                                );
                            }
                        }
                    };
                    p.models = list;
                    p.model = resolved.clone();
                    resolved
                } else {
                    model
                };
                self.cfg.default_provider = provider.clone();
                let saved_path = config::save(&self.cfg)?;
                self.switch_provider(&provider)?;
                let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
                Ok(format!(
                    "# Provider & Model Diperbarui\n\
                     * model | {chosen} | aktif di {} ({})\n\
                     > konteks | {ctx_s} | tersimpan ({})",
                    self.provider_name,
                    self.pcfg.kind,
                    saved_path.display()
                ))
            }
            ModelAction::SwitchOrModel(target) => {
                // 0. Tangani subcommand tanpa argumen atau nomor #0 dengan panduan jelas.
                if target == "0" {
                    let count = self.pcfg.effective_models().len();
                    anyhow::bail!(
                        "nomor model #0 tidak valid (gunakan nomor 1..={count}). Cek daftar lewat `/model`."
                    );
                }
                if target.eq_ignore_ascii_case("add") {
                    anyhow::bail!("format: `/model add <id-model>` atau `/models` lalu `/model add <nomor>`.");
                }
                if target.eq_ignore_ascii_case("rm")
                    || target.eq_ignore_ascii_case("remove")
                    || target.eq_ignore_ascii_case("del")
                {
                    anyhow::bail!("format: `/model rm <nomor|id-model>` atau `/model clear`.");
                }
                if target.eq_ignore_ascii_case("set") {
                    anyhow::bail!("format: `/model set <nomor|id-model>`.");
                }
                // 1. Nama provider terdaftar -> pindah & simpan sebagai default_provider!
                if self.cfg.providers.contains_key(&target) {
                    self.switch_provider_and_persist(&target)?;
                    return Ok(self.models_menu());
                }
                // 2. Cocokkan di daftar model tersimpan provider aktif (exact atau substring unik).
                // Tidak pernah diam-diam membuat/berpindah ke model fiktif bila target tidak ada!
                let list = self.pcfg.effective_models();
                match lookup_saved_model(&list, &target) {
                    SavedModelLookup::Exact(i) | SavedModelLookup::UniqueSubstring(i) => {
                        let idx = i + 1;
                        let chosen = list[i].clone();
                        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
                            p.models = list;
                            p.model = chosen;
                        }
                        self.cfg.default_provider = self.provider_name.clone();
                        let saved_path = config::save(&self.cfg)?;
                        let pname = self.provider_name.clone();
                        self.switch_provider(&pname)?;
                        let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
                        Ok(format!(
                            "# Model Dipilih (#{idx})\n\
                             * model | {} | aktif di {} ({})\n\
                             > konteks | {ctx_s} | tersimpan ({})",
                            self.pcfg.model,
                            self.provider_name,
                            self.pcfg.kind,
                            saved_path.display()
                        ))
                    }
                    SavedModelLookup::Ambiguous(idxs) => {
                        let mut out = format!(
                            "# Ditemukan {} Model Cocok \"{target}\"\n",
                            idxs.len()
                        );
                        for i in idxs.iter().take(25) {
                            let m = &list[*i];
                            let idx = i + 1;
                            let active = *m == self.pcfg.model;
                            let mark = if active { "*" } else { ">" };
                            let m_ctx = format_context_compact(self.model_context_for(m));
                            let note = if active {
                                format!("✓ aktif · ctx {m_ctx}")
                            } else {
                                format!("ctx {m_ctx}")
                            };
                            out.push_str(&format!("{mark} #{idx} | {m} | {note}\n"));
                        }
                        out.push_str("---\n");
                        out.push_str("! Pilih cepat dengan nomor: /model <nomor>   (mis. /model 2)");
                        Ok(out)
                    }
                    SavedModelLookup::NotFound => {
                        anyhow::bail!(
                            "model atau provider '{target}' belum ada di daftar tersimpan provider '{}'.\n\
                             Ketik `/model` untuk memilih lewat tombol, `/models {target}` untuk cari di katalog API, \
                             atau `/model add {target}` bila memang ingin menambahkannya secara manual.",
                            self.provider_name
                        );
                    }
                }
            }
        }
    }

    /// Tarik katalog model real-time dari endpoint API provider aktif (OpenAI-compatible,
    /// Gemini native `/v1beta/models`, atau Anthropic `/v1/models`) sekaligus menyimpan
    /// kapasitas context window hasil deteksi API ke cache SQLite persisten lintas permukaan.
    pub(crate) async fn fetch_remote_models_raw(&mut self) -> Result<Vec<String>> {
        let (_n, pcfg, api_key) = self.cfg.resolve_provider(Some(&self.provider_name))?;
        let items: Vec<(String, Option<usize>)> = match pcfg.kind.as_str() {
            "openai" => {
                let base_url = pcfg.base_url.ok_or_else(|| {
                    anyhow::anyhow!("base_url kosong untuk provider '{}'", self.provider_name)
                })?;
                fetch_models_with_context(&base_url, &api_key).await?
            }
            "gemini" => {
                let base_url = pcfg
                    .base_url
                    .as_deref()
                    .unwrap_or(gemini::DEFAULT_BASE_URL);
                gemini::fetch_models_with_context(base_url, &api_key).await?
            }
            "anthropic" => {
                let base_url = pcfg
                    .base_url
                    .as_deref()
                    .unwrap_or(anthropic::DEFAULT_BASE_URL);
                anthropic::fetch_models_with_context(base_url, &api_key).await?
            }
            other => {
                anyhow::bail!(
                    "provider '{}' (kind={other}) belum mendukung penarikan katalog /models. Tambah manual: `/model add <id-model>`",
                    self.provider_name
                );
            }
        };
        let mut ids = Vec::with_capacity(items.len());
        let mut discovered: Vec<(String, usize)> = Vec::new();
        for (id, ctx_opt) in items {
            if let Some(limit) = ctx_opt {
                self.model_context_cache.insert(id.clone(), limit);
                discovered.push((id.clone(), limit));
            }
            ids.push(id);
        }
        if !discovered.is_empty() {
            if let Ok(merged) = self.store.upsert_model_context_cache(&discovered) {
                self.model_context_cache = merged;
            }
        }
        // Perbarui kapasitas konteks model aktif bila baru ditemukan dari API.
        self.ctx.context_limit_tokens = self.resolve_context_limit();
        self.last_remote_models = ids.clone();
        Ok(ids)
    }

    pub(crate) async fn fetch_remote_models_menu(&mut self, filter: Option<&str>) -> Result<String> {
        if !matches!(self.pcfg.kind.as_str(), "openai" | "gemini" | "anthropic") {
            return Ok(format!(
                "# Katalog Model ({})\n\
                 ! Provider kind '{}' belum mendukung endpoint katalog model.\n\
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
                let m_ctx = format_context_compact(self.model_context_for(id));
                let note = if is_active {
                    format!("✓ aktif · ctx {m_ctx}")
                } else if is_saved {
                    format!("✓ tersimpan · ctx {m_ctx}")
                } else {
                    format!("ctx {m_ctx}")
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
    pub(crate) fn handle_provider(&mut self, arg: Option<&str>) -> Result<String> {
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
                    fallback: Vec::new(),
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

    /// Mulai sesi baru yang bersih (/new) secara lazy (tidak membuat row kosong sampai ada pesan).
    pub(crate) fn reset_session(&mut self) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let new_id = self.store.register_lazy_session(
            &cwd_str,
            &self.provider_name,
            &self.pcfg.model,
            "(sesi baru)",
        );
        self.session_id = new_id.clone();
        self.ctx.session_id = Some(new_id.clone());
        self.ctx.is_resumed_session = false;
        self.ctx.frozen_user_memory = Some(selencode::user_memory::load_frozen_snapshot(
            self.cfg.memory.max_memory_chars,
            self.cfg.memory.max_profile_chars,
        ));
        self.messages.clear();
        self.turns_since_review = 0;
        self.last_scheduled_review_msg_len = 0;
        if let Ok(mut plan) = self.ctx.plan_state.lock() {
            plan.clear();
        }
        if let Ok(mut tracker) = self.ctx.read_tracker.lock() {
            tracker.clear();
        }
        Ok(new_id)
    }

    /// Siapkan pekerjaan Selena Improvement (v0.5.25) bila memenuhi pemicu bernilai & pagar anggaran:
    /// - `end_of_session = true`: dipicu saat sesi berakhir (`/exit`, `/new`, `selen run` selesai).
    /// - `end_of_session = false`: dipicu sesudah giliran selesai bila mencapai `cfg.review.turns_interval`,
    ///   muncul sinyal bernilai tinggi, ATAU ada sinyal kelas-satu tertunda pasca-cooldown.
    /// - Idempoten: menolak snapshot `(session_id, messages.len())` yang sudah pernah dijadwalkan/selesai.
    /// - Memakai `[aux] review_model` / `compression_model` bila terisi, atau model utama sesi secara bawaan.
    pub(crate) fn prepare_bg_review_job(&mut self, end_of_session: bool) -> Option<BgReviewJob> {
        if self.needs_setup {
            return None;
        }
        let cur_len = self.messages.len();
        if cur_len == 0 || cur_len <= self.last_scheduled_review_msg_len {
            return None;
        }
        // Masalah 2 (v0.5.25): rekam sinyal kelas-satu secara persisten apabila cooldown aktif
        // sehingga koreksi pengguna atau kegagalan terpulihkan tidak pernah ditelan cooldown.
        if self
            .store
            .is_review_in_cooldown(&self.session_id, self.cfg.review.cooldown_mins)
            .unwrap_or(false)
        {
            selencode::agent::record_pending_signal_if_present(
                &self.store,
                &self.session_id,
                &self.messages,
            );
        }
        let has_pending_signal = self
            .store
            .get_pending_review_signal(&self.session_id)
            .ok()
            .flatten()
            .is_some();
        if !end_of_session {
            let interval = self.cfg.review.turns_interval;
            let periodic_due = interval > 0 && self.turns_since_review >= interval;
            let since_idx = self
                .store
                .last_reviewed_msg_count(&self.session_id)
                .unwrap_or(0)
                .min(cur_len);
            let slice = if since_idx > 0 && since_idx < cur_len {
                &self.messages[since_idx..]
            } else {
                &self.messages[..]
            };
            let signal_due = selencode::agent::has_valuable_review_signal(
                slice,
                self.cfg.review.min_tool_calls,
            );
            if !periodic_due && !signal_due && !has_pending_signal {
                return None;
            }
        }
        if selencode::agent::check_review_eligibility(
            &self.cfg,
            &self.store,
            &self.session_id,
            &self.messages,
        )
        .is_err()
        {
            return None;
        }
        let rev_prov = make_review_provider(
            &self.cfg,
            &self.provider_name,
            &self.pcfg,
            self.provider.clone(),
        )?;
        self.turns_since_review = 0;
        self.last_scheduled_review_msg_len = cur_len;
        Some(BgReviewJob {
            provider: rev_prov,
            cfg: self.cfg.clone(),
            store: self.store.clone(),
            cwd: self.ctx.cwd.clone(),
            session_id: self.session_id.clone(),
            messages: self.messages.clone(),
        })
    }

    /// Beralih & lanjutkan sesi tersimpan berdasarkan ID persis, nomor urut (`1..=20`), atau prefix ID.
    pub(crate) fn resume_by_target(&mut self, target: &str) -> Result<ResumedSessionData> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let info = match self.store.session_by_id(target.trim())? {
            Some(found) => found,
            None => self.store.resolve_session(&cwd_str, target, 20)?,
        };
        let mut msgs = self.store.load_messages(&info.id)?;
        if let Some(syn) = sanitize_interrupted_messages(&mut msgs) {
            let _ = self.store.save_message(&info.id, &syn);
        }
        let (input_tokens, output_tokens) = self.store.total_usage(&info.id).unwrap_or((0, 0));
        let context_used = estimate_tokens(&msgs);
        let context_limit = self.ctx.context_limit_tokens;
        let preview_lines = build_session_preview(&msgs, 6);
        let total_messages = msgs.len();

        self.session_id = info.id.clone();
        self.ctx.session_id = Some(info.id.clone());
        self.ctx.is_resumed_session = true;
        self.messages = msgs;
        self.turns_since_review = 0;
        self.last_scheduled_review_msg_len = self
            .store
            .last_reviewed_msg_count(&info.id)
            .unwrap_or(0);
        if let Ok(mut plan) = self.ctx.plan_state.lock() {
            plan.clear();
        }
        if let Ok(mut tracker) = self.ctx.read_tracker.lock() {
            tracker.clear();
        }

        Ok(ResumedSessionData {
            session_id: info.id,
            title: info.title,
            provider: self.provider_name.clone(),
            model: self.pcfg.model.clone(),
            input_tokens,
            output_tokens,
            context_used,
            context_limit,
            preview_lines,
            total_messages,
        })
    }

    /// Compaction konteks manual (`/compact`): memakai ringkasan terstruktur berbantuan LLM
    /// dengan fallback deterministik aman bila panggilan LLM gagal/timeout.
    pub(crate) async fn compact_now_smart(&mut self) -> String {
        let before_msgs = self.messages.len();
        let before_tok = estimate_tokens(&self.messages);
        let (compacted, used_llm) = compact_messages_with_llm(&self.provider, &self.messages).await;
        let after_msgs = compacted.len();
        let after_tok = estimate_tokens(&compacted);
        self.messages = compacted;
        if after_msgs == before_msgs {
            format!(
                "# Compaction Konteks\n\
                 ! Riwayat masih pendek ({before_msgs} pesan, ~{before_tok} token) — belum perlu diringkas."
            )
        } else {
            let mode = if used_llm {
                "ringkasan cerdas LLM"
            } else {
                "ringkasan terstruktur deterministik"
            };
            format!(
                "# Compaction Konteks Selesai\n\
                 * pesan | {before_msgs} → {after_msgs} | estimasi ~{before_tok} → ~{after_tok} token\n\
                 > metode | {mode} | prefix system prompt tetap utuh"
            )
        }
    }

    /// Compaction konteks deterministik sinkron (fallback internal).
    #[allow(dead_code)]
    pub(crate) fn compact_now(&mut self) -> String {
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

    /// Ganti folder kerja aktif (`/cd [folder]`) tanpa restart (berlaku di Telegram, TUI, & Plain REPL).
    pub(crate) fn handle_cd(&mut self, arg: Option<&str>) -> Result<String> {
        let Some(raw) = arg.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(format!(
                "# Folder Kerja Aktif (/cd)\n\
                 * folder | {} | aktif sekarang\n\
                 ! Pindah folder: /cd <path_relatif_atau_absolut>",
                self.ctx.cwd.display()
            ));
        };
        let expanded = if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| self.ctx.cwd.clone())
        } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| self.ctx.cwd.join(raw))
        } else {
            let p = std::path::Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.ctx.cwd.join(p)
            }
        };
        let canon_raw = std::fs::canonicalize(&expanded)
            .map_err(|e| anyhow::anyhow!("folder '{}' tidak ditemukan: {e}", expanded.display()))?;
        let canon = selencode::security::strip_windows_verbatim(&canon_raw);
        if !canon.is_dir() {
            anyhow::bail!("'{}' bukan direktori.", canon.display());
        }
        // Keamanan Sandbox: saat mode != `unrestricted`, `/cd` tidak boleh kabur keluar `workspace_root`!
        if self.ctx.sandbox.mode() != selencode::sandbox::SandboxMode::Unrestricted {
            let root_norm = selencode::security::normalize_sandbox_path(&self.workspace_root);
            let target_norm = selencode::security::normalize_sandbox_path(&canon);
            if !target_norm.starts_with(&root_norm) {
                anyhow::bail!(
                    "perpindahan `/cd` ke '{}' ditolak oleh sandbox ({}): di luar workspace root '{}'. Gunakan `/sandbox unrestricted` jika memang ingin berpindah keluar workspace.",
                    canon.display(),
                    self.ctx.sandbox.mode().as_str(),
                    root_norm.display()
                );
            }
        }
        let prev = self.ctx.cwd.display().to_string();
        self.ctx.cwd = canon.clone();
        // Perbarui RepoIndex & LspManager agar mengikuti folder kerja baru secara konsisten.
        self.ctx.repo_index = Arc::new(std::sync::Mutex::new(
            selencode::context_index::RepoIndex::new(canon.clone()),
        ));
        self.ctx.lsp_manager = Arc::new(selencode::lsp::LspManager::new(canon.clone()));
        Ok(format!(
            "# Folder Kerja Diperbarui (/cd)\n\
             * folder | {} | aktif sekarang\n\
             > sebelumnya | {prev} | seluruh tool, indeks, LSP, & sandbox mengikuti folder baru",
            canon.display()
        ))
    }

    /// Snapshot daftar sesi tersimpan untuk modal `/resume` interaktif di TUI.
    pub(crate) fn resume_picker_data(&self, status: Option<String>) -> Result<ResumePickerData> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let _ = self.store.prune_empty_sessions(&cwd_str, &self.session_id);
        let list = self.store.list_sessions(&cwd_str, 20)?;
        let items = list
            .into_iter()
            .map(|s| {
                let msg_count = self.store.count_messages(&s.id).unwrap_or(0);
                let is_active = s.id == self.session_id;
                ResumePickerItem {
                    id: s.id,
                    title: s.title,
                    created_at: s.created_at,
                    model: s.model,
                    msg_count,
                    is_active,
                }
            })
            .collect();
        Ok(ResumePickerData {
            cwd: cwd_str,
            active_session_id: self.session_id.clone(),
            items,
            status,
        })
    }

    /// Periksa apakah model aktif perlu auto-probe kapasitas konteks di latar saat startup:
    /// hanya berjalan bila tidak ada override `[providers].context_limit` dan model belum ada di cache SQLite.
    pub(crate) fn background_context_probe_params(
        &self,
    ) -> Option<(String, Option<String>, String, Arc<Store>)> {
        if self.needs_setup || self.pcfg.context_limit.filter(|&n| n > 0).is_some() {
            return None;
        }
        if self.model_context_cache.contains_key(&self.pcfg.model) {
            return None;
        }
        if !matches!(self.pcfg.kind.as_str(), "openai" | "gemini" | "anthropic") {
            return None;
        }
        let (_, pcfg, api_key) = self.cfg.resolve_provider(Some(&self.provider_name)).ok()?;
        Some((pcfg.kind, pcfg.base_url, api_key, self.store.clone()))
    }

    /// Ringkasan token sesi + estimasi biaya bila harga dikonfigurasi.
    pub(crate) fn usage_line(&self) -> String {
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

    /// Kartu informasi sesi baru (`/new`) untuk terminal TUI & Plain REPL:
    /// menampilkan ID sesi baru, model aktif, provider, kapasitas konteks terdeteksi, dan folder kerja.
    pub(crate) fn new_session_card(&self, sid: &str) -> String {
        let short: String = sid.chars().take(10).collect();
        let ctx_exact = format_context_tokens_exact(self.ctx.context_limit_tokens);
        format!(
            "# Sesi Baru Dimulai\n\
             * model | {} | ✓ aktif · {}\n\
             > provider | {} | {}\n\
             > sesi | {} | folder {}",
            self.pcfg.model,
            ctx_exact,
            self.provider_name,
            self.pcfg.kind,
            short,
            self.ctx.cwd.display(),
        )
    }

    /// Kartu menu `/model`: ringkas & jelas menyorot model aktif + kapasitas konteks tiap model + provider.
    pub(crate) fn models_menu(&self) -> String {
        let list = self.pcfg.effective_models();
        let active_ctx = format_context_tokens_exact(self.ctx.context_limit_tokens);
        let mut out = format!(
            "# Konfigurasi Model · {} ({} tersimpan)\n\
             * aktif | {} | {}\n\
             ---\n",
            self.provider_name,
            list.len(),
            self.pcfg.model,
            active_ctx,
        );
        for (i, m) in list.iter().take(30).enumerate() {
            let active = *m == self.pcfg.model;
            let mark = if active { "*" } else { ">" };
            let m_ctx = format_context_compact(self.model_context_for(m));
            let note = if active {
                format!("✓ aktif · ctx {m_ctx}")
            } else {
                format!("ctx {m_ctx}")
            };
            out.push_str(&format!("{mark} #{} | {m} | {note}\n", i + 1));
        }
        if list.len() > 30 {
            out.push_str(&format!(
                "! ... dan {} model lainnya (cari cepat: /models <kata> atau /model <kata>)\n",
                list.len() - 30
            ));
        }
        if self.cfg.providers.len() > 1 {
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
                    format!("✓ aktif · {} · {count} model · {key_ok}", p.kind)
                } else {
                    format!("{} · {count} model · {key_ok}", p.kind)
                };
                out.push_str(&format!("{mark} {n} | {} | {tag}\n", p.model));
            }
        }
        out.push_str("---\n");
        out.push_str("! Ganti: /model <no|nama> · Tambah: /model add <id> · Katalog API: /models [cari] · Hapus: /model rm <no> · Provider: /provider");
        out
    }

    pub(crate) fn model_picker_data(&self, open_catalog: bool, status: Option<String>) -> ModelPickerData {
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
    pub(crate) fn select_model_literal(&mut self, model_id: &str) -> Result<String> {
        let id = model_id.trim();
        if id.is_empty() {
            anyhow::bail!("ID model tidak boleh kosong.");
        }
        if let Some(p) = self.cfg.providers.get_mut(&self.provider_name) {
            p.models = p.effective_models();
            p.push_model_unique(id);
            p.model = id.to_string();
        }
        self.cfg.default_provider = self.provider_name.clone();
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
        let ctx_s = format_context_tokens_exact(self.ctx.context_limit_tokens);
        Ok(format!(
            "# Model Diperbarui (#{pos})\n\
             * model | {} | aktif di {} ({})\n\
             > konteks | {ctx_s} | tersimpan ({})",
            self.pcfg.model,
            self.provider_name,
            self.pcfg.kind,
            saved_path.display()
        ))
    }

    /// Tambah 1 model secara literal ke daftar pribadi provider aktif (mengembalikan `true` jika baru).
    pub(crate) fn add_model_literal(&mut self, model_id: &str) -> Result<bool> {
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
    pub(crate) fn remove_model_literal(&mut self, model_id: &str) -> Result<()> {
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
}

/// Ambil hingga `max_items` cuplikan pesan User/Assistant terakhir sebagai konteks visual saat `/resume <nomor>`.
fn build_session_preview(messages: &[Message], max_items: usize) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = Vec::new();
    for m in messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            _ => continue,
        };
        let text: String = m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) if !t.trim().is_empty() => Some(t.trim()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        let clipped: String = if text.chars().count() > 280 {
            let s: String = text.chars().take(279).collect();
            format!("{s}…")
        } else {
            text
        };
        items.push((role.to_string(), clipped));
    }
    if items.len() > max_items {
        items.split_off(items.len() - max_items)
    } else {
        items
    }
}
