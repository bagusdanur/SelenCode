pub mod ask;
pub mod bg;
pub mod delegate;
pub mod file;
pub mod git;
pub mod lsp;
pub mod memory;
pub mod orchestrate;
pub mod outline;
pub mod plan;
pub mod retrieve;
pub mod search;
pub mod session_search;
pub mod shell;
pub mod skill;
pub mod task;
pub mod toolsets;
pub mod undo;
pub mod verify;
pub mod web;

use crate::config::ShellCfg;
use crate::providers::ToolSpec;
use crate::security::Approver;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Batas ukuran output tool (30 KB, Blueprint Bagian 6).
pub const MAX_TOOL_OUTPUT: usize = 30 * 1024;

/// Konteks eksekusi tool: folder project (sandbox) + penanya persetujuan.
#[derive(Clone)]
pub struct Ctx {
    pub cwd: PathBuf,
    pub approver: Arc<Approver>,
    /// Batas estimasi token konteks untuk compaction (Fase 2).
    pub context_limit_tokens: usize,
    /// Store sesi SQLite + id sesi aktif (Fase 3). None = tanpa persistensi
    /// (dipakai tes & jalur lama); loop menyimpan pesan + usage bila terisi.
    pub store: Option<Arc<crate::store::Store>>,
    pub session_id: Option<String>,
    /// Checklist update_plan bersama (Fase 6). Diclone per Ctx — Arc yang
    /// sama menunjuk state yang sama untuk seluruh sesi.
    pub plan_state: Arc<std::sync::Mutex<Vec<plan::PlanStep>>>,
    /// Riwayat snapshot edit file untuk `/undo` dan `/redo`.
    pub undo_stack: Arc<std::sync::Mutex<undo::UndoStack>>,
    /// Indeks repositori inkremental bersama untuk Smart Context Retrieval (Tahap 2).
    /// Kosong saat dibuat (tanpa I/O) dan diperbarui secara inkremental saat diminta.
    pub repo_index: Arc<std::sync::Mutex<crate::context_index::RepoIndex>>,
    /// Manajer proses latar belakang milik SelenCode (Tahap 3: `bg_start`/`bg_output`/`bg_status`/`bg_stop`).
    pub bg_manager: Arc<bg::BgManager>,
    /// Manajer LSP hibrida + fallback Tree-sitter (Tahap 4: diagnostics, definition, references, hover, rename).
    pub lsp_manager: Arc<crate::lsp::LspManager>,
    /// Kebijakan Security Sandbox v2 (mode izin, env scrubbing, isolasi OS, & audit log).
    pub sandbox: Arc<crate::sandbox::SandboxController>,
    /// Custom Hooks (`pre_tool` & `post_tool`) dari `selencode.toml`.
    pub hooks: Arc<crate::hooks::HookEngine>,
    /// Pengaturan dedup, loop guard, & rem Self-correction (`[guard]`).
    pub guard: crate::config::GuardCfg,
    /// Pengaturan anggaran hasil tool (`[tool_budget]`: cap shell 40/60, spillover, turn budget).
    pub tool_budget: crate::config::ToolBudgetCfg,
    /// Pengaturan Skills System SelenCode (`[skills]`: `index_max_tokens`, `auto_load`).
    pub skills: crate::config::SkillsCfg,
    /// Aktifkan review self-improvement otomatis pasca-giliran kompleks.
    /// Default false di Ctx::new agar tes mock tidak memicu panggilan ekstra;
    /// Session::build mengisinya dari `cfg.agent.auto_skills` (default true).
    pub auto_skills: bool,
    /// Aktifkan Self-Correction Engine & Automatic Verification Gate setelah edit kode
    /// (Tahap 1). Default false di Ctx::new agar tes mock lama tidak berubah;
    /// Session::build mengisinya dari `cfg.agent.self_correct` (default true).
    pub self_correct: bool,
    /// Pengelola pertanyaan klarifikasi `ask_user` (v0.5.6 Bagian 2 — terpisah total dari `approver`).
    pub asker: Arc<ask::UserAsker>,
    /// Batas waktu tunggu `ask_user` dalam detik (`[ask] timeout_secs`, default 600).
    pub ask_timeout_secs: u64,
    /// Telemetri bersama router fallback agar loop mencatat model aktual & notifikasi sekali (v0.5.6 Bagian 1).
    pub router_telemetry: Option<Arc<std::sync::Mutex<crate::providers::router::RouterTelemetry>>>,
}

impl Ctx {
    pub fn new(cwd: PathBuf, auto_yes: bool) -> Self {
        let repo_index = Arc::new(std::sync::Mutex::new(
            crate::context_index::RepoIndex::new(cwd.clone()),
        ));
        let lsp_manager = Arc::new(crate::lsp::LspManager::new(cwd.clone()));
        Self {
            cwd,
            approver: Arc::new(Approver::new(auto_yes)),
            context_limit_tokens: 100_000,
            store: None,
            session_id: None,
            plan_state: Arc::new(std::sync::Mutex::new(Vec::new())),
            undo_stack: Arc::new(std::sync::Mutex::new(undo::UndoStack::default())),
            repo_index,
            bg_manager: Arc::new(bg::BgManager::new()),
            lsp_manager,
            sandbox: Arc::new(crate::sandbox::SandboxController::default()),
            hooks: Arc::new(crate::hooks::HookEngine::default()),
            guard: crate::config::GuardCfg::default(),
            tool_budget: crate::config::ToolBudgetCfg::default(),
            skills: crate::config::SkillsCfg::default(),
            auto_skills: false,
            self_correct: false,
            asker: Arc::new(ask::UserAsker::non_interactive()),
            ask_timeout_secs: 600,
            router_telemetry: None,
        }
    }
    pub fn with_asker(mut self, asker: Arc<ask::UserAsker>, timeout_secs: u64) -> Self {
        self.asker = asker;
        self.ask_timeout_secs = timeout_secs.max(1);
        self
    }
    pub fn with_ask_timeout_only(mut self, timeout_secs: u64) -> Self {
        self.ask_timeout_secs = timeout_secs.max(1);
        self
    }
    pub fn with_router_telemetry(
        mut self,
        tel: Arc<std::sync::Mutex<crate::providers::router::RouterTelemetry>>,
    ) -> Self {
        self.router_telemetry = Some(tel);
        self
    }
    pub fn with_sandbox(mut self, sandbox: Arc<crate::sandbox::SandboxController>) -> Self {
        self.sandbox = sandbox;
        self
    }
    pub fn with_hooks(mut self, hooks: Arc<crate::hooks::HookEngine>) -> Self {
        self.hooks = hooks;
        self
    }
    pub fn with_guard(mut self, guard: crate::config::GuardCfg) -> Self {
        self.guard = guard;
        self
    }
    pub fn with_tool_budget(mut self, tool_budget: crate::config::ToolBudgetCfg) -> Self {
        self.tool_budget = tool_budget;
        self
    }
    pub fn with_skills(mut self, skills: crate::config::SkillsCfg) -> Self {
        self.skills = skills;
        self
    }
    pub fn with_auto_skills(mut self, enabled: bool) -> Self {
        self.auto_skills = enabled;
        self
    }
    pub fn with_self_correct(mut self, enabled: bool) -> Self {
        self.self_correct = enabled;
        self
    }
    /// Ganti approver (Fase 4: mode TUI memakai approver berbasis channel).
    pub fn with_approver(mut self, approver: Arc<Approver>) -> Self {
        self.approver = approver;
        self
    }
    pub fn with_store(mut self, store: Arc<crate::store::Store>, session_id: String) -> Self {
        self.store = Some(store);
        self.session_id = Some(session_id);
        self
    }
    pub fn with_context_limit(mut self, n: usize) -> Self {
        if n > 0 {
            self.context_limit_tokens = n;
        }
        self
    }
}

/// Jalankan command sampai selesai dengan timeout; gabungkan stdout+stderr.
/// Error timeout berbunyi "timeout setelah N detik" (disengaja: jelas di log).
/// Command harus sudah di-set `kill_on_drop(true)` oleh pemanggil.
pub(crate) async fn run_timed(
    cmd: &mut tokio::process::Command,
    timeout_secs: u64,
) -> Result<(Option<i32>, String)> {
    let out = match tokio::time::timeout(
        Duration::from_secs(timeout_secs.max(1)),
        cmd.output(),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("timeout setelah {timeout_secs} detik"),
    };
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code(), s))
}

/// Potong output di batas `max` byte (aman UTF-8) + penanda "[output dipotong]".
pub(crate) fn truncate_output(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n[output dipotong]");
    out
}

/// Kontrak tool (Blueprint Bagian 5).
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String>;
}

pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            order: Vec::new(),
        }
    }
    pub fn register(mut self, t: Arc<dyn Tool>) -> Self {
        let name = t.spec().name;
        self.order.push(name.clone());
        self.tools.insert(name, t);
        self
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n).map(|t| t.spec()))
            .collect()
    }
    /// Specs yang namanya lolos predikat (Fase 6: plan mode hanya baca-saja).
    pub fn specs_where(&self, keep: impl Fn(&str) -> bool) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter(|n| keep(n))
            .filter_map(|n| self.tools.get(n).map(|t| t.spec()))
            .collect()
    }
    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry default: config shell bawaan (allowlist kosong, timeout 120 dtk).
pub fn default_registry() -> Registry {
    default_registry_with_shell(&ShellCfg::default())
}

/// Registry: tool file + search + shell + git + web (fetch_url, Fase 3).
pub fn default_registry_with_shell(shell_cfg: &ShellCfg) -> Registry {
    Registry::new()
        .register(Arc::new(file::ListFiles))
        .register(Arc::new(file::ReadFile))
        .register(Arc::new(file::WriteFile))
        .register(Arc::new(file::EditFile))
        .register(Arc::new(search::SearchText))
        .register(Arc::new(outline::Outline))
        .register(Arc::new(retrieve::RetrieveContext))
        .register(Arc::new(lsp::LspDiagnostics))
        .register(Arc::new(lsp::LspDefinition))
        .register(Arc::new(lsp::LspReferences))
        .register(Arc::new(lsp::LspHover))
        .register(Arc::new(lsp::LspRename))
        .register(Arc::new(shell::RunShell::new(
            shell_cfg.allowlist.clone(),
            shell_cfg.timeout_secs,
        )))
        .register(Arc::new(bg::BgStart))
        .register(Arc::new(bg::BgOutput))
        .register(Arc::new(bg::BgStatus))
        .register(Arc::new(bg::BgStop))
        .register(Arc::new(git::GitStatus))
        .register(Arc::new(git::GitDiff))
        .register(Arc::new(git::GitCommit))
        .register(Arc::new(web::FetchUrl))
        .register(Arc::new(plan::UpdatePlan))
        .register(Arc::new(skill::SkillsList))
        .register(Arc::new(skill::SkillView))
        .register(Arc::new(skill::SkillCreate))
        .register(Arc::new(skill::SkillEdit))
        .register(Arc::new(skill::ManageSkill))
        .register(Arc::new(memory::ManageMemory))
        .register(Arc::new(task::ManageTask))
        .register(Arc::new(verify::VerifyProject::new(shell_cfg.timeout_secs)))
        .register(Arc::new(ask::AskUser))
        .register(Arc::new(session_search::SessionSearch))
}

/// Registry BACA-SAJA (Fase 6): persis daftar agent::plan::READ_ONLY_TOOLS.
/// Dipakai subagent `delegate` — tanpa tool menulis, tanpa shell, tanpa
/// delegate/update_plan (rekursi mustahil secara konstruksi).
pub fn read_only_registry() -> Registry {
    Registry::new()
        .register(Arc::new(file::ListFiles))
        .register(Arc::new(file::ReadFile))
        .register(Arc::new(search::SearchText))
        .register(Arc::new(outline::Outline))
        .register(Arc::new(retrieve::RetrieveContext))
        .register(Arc::new(lsp::LspDiagnostics))
        .register(Arc::new(lsp::LspDefinition))
        .register(Arc::new(lsp::LspReferences))
        .register(Arc::new(lsp::LspHover))
        .register(Arc::new(git::GitStatus))
        .register(Arc::new(git::GitDiff))
        .register(Arc::new(web::FetchUrl))
        .register(Arc::new(skill::SkillsList))
        .register(Arc::new(skill::SkillView))
        .register(Arc::new(session_search::SessionSearch))
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

pub(crate) fn schema(props: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}
