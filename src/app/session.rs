use super::{make_routed_provider};
use anyhow::Result;
use selencode::agent::context::{
    compact_messages, estimate_tokens, format_context_compact, infer_model_context_limit,
};
use selencode::agent::ui::ModelPickerData;
use selencode::config::{self, Config, ProviderCfg};
use selencode::providers::openai::fetch_models_with_context;
use selencode::providers::router::RouterTelemetry;
use selencode::providers::{Message, Provider, UnconfiguredProvider};
use selencode::repl::{
    classify_key_spec, parse_model_action, parse_provider_action, KeySpec, ModelAction,
    ProviderAction, PROVIDER_PRESETS,
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
}

impl Session {
    /// Judul sesi: potongan prompt pertama (maks 60 karakter).
    pub(crate) fn title_from(prompt: &str) -> String {
        let t: String = prompt.chars().take(60).collect();
        if t.is_empty() {
            "(sesi baru)".to_string()
        } else {
            t
        }
    }

    pub(crate) async fn build(
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
                "Melanjutkan sesi: \"{title}\" ({created}) â€” {} pesan dimuat.",
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
        let mut registry = default_registry_with_shell(&cfg.shell)
            .register(Arc::new(Delegate::new(
                provider.clone(),
                Arc::new(read_only_registry()),
                cfg.agent.max_parallel_subagents,
            )))
            .register(Arc::new(Orchestrate::new(
                provider.clone(),
                cfg.shell.clone(),
                cfg.agent.max_parallel_subagents,
            )));
        // Tool MCP (Fase 7): HANYA registry induk mode Execute â€” tidak ikut
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
             * {} | {} | aktif Â· ctx {ctx_s}\n\
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
    pub(crate) async fn fetch_remote_models_raw(&mut self) -> Result<Vec<String>> {
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

    pub(crate) async fn fetch_remote_models_menu(&mut self, filter: Option<&str>) -> Result<String> {
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
                "# Katalog API {} ({} tersedia Â· tidak otomatis disimpan)\n",
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
                    "aktif Â· tersimpan"
                } else if is_saved {
                    "âœ“ sudah di daftar"
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
                     > {name} | {model} | kind {kind} Â· {key_info}\n\
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
    pub(crate) fn reset_session(&mut self) -> Result<String> {
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
                 ! Riwayat masih pendek ({before_msgs} pesan, ~{before_tok} token) â€” belum perlu diringkas."
            )
        } else {
            format!(
                "# Compaction Konteks Selesai\n\
                 * pesan | {before_msgs} â†’ {after_msgs} | estimasi ~{before_tok} â†’ ~{after_tok} token"
            )
        }
    }

    /// Ringkasan token sesi + estimasi biaya bila harga dikonfigurasi.
    pub(crate) fn usage_line(&self) -> String {
        match self.store.total_usage(&self.session_id) {
            Ok((i, o)) => {
                let mut s = format!("token sesi ini: {i} in / {o} out");
                if let (Some(pin), Some(pout)) = (self.pcfg.price_in, self.pcfg.price_out) {
                    let cost = (i as f64 / 1_000_000.0) * pin + (o as f64 / 1_000_000.0) * pout;
                    s.push_str(&format!(" Â· estimasi biaya ${cost:.4}"));
                }
                s
            }
            Err(e) => format!("(gagal membaca usage: {e})"),
        }
    }

    /// Kartu menu `/model`: daftar model bernomor milik provider aktif + daftar provider lain.
    pub(crate) fn models_menu(&self) -> String {
        let list = self.pcfg.effective_models();
        let active_ctx = format_context_compact(self.ctx.context_limit_tokens);
        let mut out = format!(
            "# Model di Provider {} ({} tersimpan Â· ctx {active_ctx})\n",
            self.provider_name,
            list.len()
        );
        for (i, m) in list.iter().take(30).enumerate() {
            let active = *m == self.pcfg.model;
            let mark = if active { "*" } else { ">" };
            let m_ctx = format_context_compact(self.model_context_for(m));
            let note = if active {
                format!("aktif Â· ctx {m_ctx}")
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
                format!("provider aktif Â· {} Â· {count} model Â· {key_ok}", p.kind)
            } else {
                format!("provider Â· {} Â· {count} model Â· {key_ok}", p.kind)
            };
            out.push_str(&format!("{mark} {n} | {} | {tag}\n", p.model));
        }
        out.push_str("---\n");
        out.push_str("! Ganti cepat:       /model 2   (pakai nomor)   |   /model claude   (potongan nama)\n");
        out.push_str("! Tambah 1-per-1:    /model add <id>   |   Intip API: /models [cari] lalu /model add 1 3\n");
        out.push_str("! Hapus daftar:      /model rm <no>   |   Hapus semua: /model clear   |   Provider: /provider");
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
