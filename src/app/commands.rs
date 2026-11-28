use super::session::{ResumedSessionData, Session};
use anyhow::Result;
use selencode::agent::context::{calc_context_pct, estimate_tokens, format_context_compact};
use selencode::agent::run_self_improvement_review;
use selencode::config;
use selencode::providers::router::TaskKind;
use selencode::providers::{Message, Role};
use selencode::repl::{strip_menu_markup, KeySpec, PROVIDER_PRESETS};
use selencode::security::{redact_secrets, ApprovalMode};
use selencode::skills::{
    export_conversation_md, init_agents_md, load_skill, remove_skill, save_skill,
};
use selencode::telegram::{parse_telegram_action, TelegramAction};
use std::time::Duration;

/// Hasil eksekusi perintah `/resume [nomor | info | rename <judul>]`.
pub(crate) enum ResumeOutcome {
    /// Tampilkan kartu menu (daftar 10 sesi bernomor, info sesi aktif, atau konfirmasi rename).
    Menu(String),
    /// Berhasil berpindah & memuat ulang sesi terpilih.
    Switched(ResumedSessionData),
}

impl Session {
    /// Tangani `/telegram [status|test|setup|allow|rm|approve|off]`.
    pub(crate) async fn handle_telegram(&mut self, arg: Option<&str>) -> Result<String> {
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
                let cfg_path = config::active_config_path();
                Ok(format!(
                    "# Telegram Gateway SelenCode\n\
                     * bot token | {tok_state} | dari @BotFather\n\
                     > allowed_users | {users_str} | whitelist ID Telegram\n\
                     > approval | {appr} | ask / edits / auto\n\
                     > config | {} | persisten lintas folder\n\
                     ---\n\
                     ! Cek koneksi bot:   /telegram test\n\
                     ! Pasang cepat:      /telegram setup <BOT_TOKEN> <USER_ID>\n\
                     ! Tambah/hapus ID:   /telegram allow <USER_ID>   |   /telegram rm <USER_ID>\n\
                     ! Mode izin bot:     /telegram approve <ask|edits|auto>   |   Matikan: /telegram off\n\
                     ! Jalankan gateway:  ketik `selen telegram` di terminal terpisah agar bot aktif menerima chat!",
                    cfg_path.display()
                ))
            }
            TelegramAction::Test => {
                let token = self.cfg.telegram.resolve_token()?;
                let tg = selencode::telegram::TelegramClient::new(&token)?;
                let t0 = std::time::Instant::now();
                let me = tg.get_me().await?;
                let ms = t0.elapsed().as_millis();
                let handle = me
                    .username
                    .as_deref()
                    .map(|u| format!("@{u}"))
                    .unwrap_or_else(|| me.first_name.clone());
                let webhook_note = match tg.get_webhook_info().await {
                    Ok(v) => {
                        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
                        if url.is_empty() {
                            "tidak ada webhook (siap long-polling)".to_string()
                        } else {
                            format!("webhook aktif ({url}) — akan dibersihkan otomatis saat `selen telegram` mulai")
                        }
                    }
                    Err(_) => "tidak diperiksa".to_string(),
                };
                let wl = if self.cfg.telegram.allowed_users.is_empty() {
                    "KOSONG (kirim `/id` ke bot lalu `/telegram allow <ID>`)"
                } else {
                    "siap"
                };
                Ok(format!(
                    "# Uji Koneksi Telegram Berhasil ✅\n\
                     * bot | {handle} (id: {}) | respons {ms} ms\n\
                     > webhook | {webhook_note}\n\
                     > whitelist | {wl}\n\
                     ---\n\
                     ! Jalankan `selen telegram` di terminal untuk mulai menerima & membalas chat Telegram.",
                    me.id
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
                 > /telegram test | | uji koneksi & latensi bot ke Telegram API\n\
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

    pub(crate) fn handle_checkpoint(&self, arg: Option<&str>) -> Result<String> {
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

    pub(crate) fn handle_rollback(&self, arg: Option<&str>) -> Result<String> {
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
        out.push_str("---\n! Disarankan jalankan `/verify check` untuk memastikan workspace sehat pasca-rollback.");
        Ok(out)
    }

    pub(crate) async fn handle_worktree(&self, arg: Option<&str>) -> Result<String> {
        use selencode::worktree;
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let list = worktree::list_worktrees(&self.ctx.cwd)?;
            let mut out = format!("# Transactional Git Worktree ({} aktif)\n", list.len());
            if list.is_empty() {
                out.push_str("! Belum ada worktree SelenCode aktif di repository ini.\n");
            } else {
                for w in list {
                    let short_sha: String = w.base_commit.chars().take(7).collect();
                    out.push_str(&format!(
                        "* {} | {} ({short_sha}) | {}\n",
                        w.id,
                        w.branch,
                        w.path.display()
                    ));
                }
            }
            out.push_str("---\n");
            out.push_str("! Buat baru:      /worktree new <label>\n");
            out.push_str("! Verifikasi:     /worktree verify <id> [check/test/lint/all]\n");
            out.push_str("! Preview diff:   /worktree diff <id>\n");
            out.push_str("! Terapkan aman:  /worktree apply <id>  (otomatis buat checkpoint + blokir konflik user)\n");
            out.push_str("! Hapus:          /worktree rm <id>");
            return Ok(out);
        };
        if let Some(rest) = raw.strip_prefix("verify ") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let id = parts.next().unwrap_or("").trim();
            let scope = parts
                .next()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("all");
            if id.is_empty() {
                anyhow::bail!("format: `/worktree verify <id> [check|test|lint|all]`");
            }
            let report = worktree::verify_worktree(
                &self.ctx.cwd,
                id,
                scope,
                self.cfg.shell.timeout_secs.max(120),
                &self.ctx.sandbox,
            )
            .await?;
            return Ok(report.format_menu_card());
        }
        if let Some(rest) = raw
            .strip_prefix("new ")
            .or_else(|| raw.strip_prefix("create "))
        {
            let info = worktree::create_isolated_worktree(&self.ctx.cwd, rest.trim())?;
            return Ok(format!(
                "# Git Worktree Terisolasi Dibuat\n\
                 * {} | branch {} | base {}\n\
                 > path | {} |\n\
                 ! Preview perubahan: /worktree diff {}   ·   Terapkan: /worktree apply {}",
                info.id,
                info.branch,
                info.base_commit.chars().take(7).collect::<String>(),
                info.path.display(),
                info.id,
                info.id
            ));
        }
        if let Some(rest) = raw.strip_prefix("diff ").or_else(|| raw.strip_prefix("preview ")) {
            let prev = worktree::preview_diff(&self.ctx.cwd, rest.trim())?;
            let mut out = format!(
                "# Preview Diff Worktree {} ({})\n\
                 * diubah | {} file | dihapus: {} file\n",
                prev.id,
                prev.branch,
                prev.changed_files.len(),
                prev.deleted_files.len()
            );
            if !prev.user_dirty_conflicts.is_empty() {
                out.push_str(&format!(
                    "! KONFLIK file kotor pengguna: {} (apply akan diblokir sampai diselesaikan)\n",
                    prev.user_dirty_conflicts.join(", ")
                ));
            }
            if !prev.diff_text.trim().is_empty() {
                out.push_str("---\n");
                for line in prev.diff_text.lines().take(35) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw
            .strip_prefix("apply ")
            .or_else(|| raw.strip_prefix("merge "))
        {
            let rep = worktree::apply_worktree_to_main(
                &self.ctx.cwd,
                rest.trim(),
                &self.ctx.undo_stack,
            )?;
            return Ok(format!(
                "# Worktree Diterapkan (Checkpoint #{})\n\
                 * {} | {} file diterapkan · {} dihapus | aman di-rollback\n\
                 ! Rollback kapan saja: /rollback {}   ·   Verifikasi: /verify",
                rep.checkpoint_id,
                rep.id,
                rep.applied_files.len(),
                rep.deleted_files.len(),
                rep.checkpoint_id
            ));
        }
        if let Some(rest) = raw
            .strip_prefix("rm ")
            .or_else(|| raw.strip_prefix("del "))
            .or_else(|| raw.strip_prefix("discard "))
        {
            let id = rest.trim();
            worktree::discard_worktree(&self.ctx.cwd, id)?;
            return Ok(format!(
                "# Worktree Dihapus\n> {id} | worktree & branch sementara dihapus bersih"
            ));
        }
        anyhow::bail!("subperintah `/worktree {raw}` tidak dikenal. Ketik `/worktree` untuk cara pakai.")
    }

    pub(crate) fn handle_doctor(&self) -> String {
        selencode::doctor::run_doctor(
            &self.cfg,
            &self.ctx.cwd,
            &self.provider_name,
            &self.pcfg,
            &self.store,
            &self.ctx.sandbox,
        )
        .format_menu_card()
    }

    pub(crate) fn handle_mcp_status(&self) -> String {
        use selencode::mcp::client::{
            PROTOCOL_VERSION, SUPPORTED_MCP_FEATURES, UNSUPPORTED_MCP_FEATURES,
        };
        let mcp_tools: Vec<String> = self
            .registry
            .names()
            .into_iter()
            .filter(|n| n.starts_with("mcp__"))
            .collect();
        let mut out = format!(
            "# Status Kompatibilitas MCP (spec {PROTOCOL_VERSION})\n\
             * server terpasang | {} di config | {} tool MCP aktif\n",
            self.cfg.mcp_servers.len(),
            mcp_tools.len()
        );
        for srv in &self.cfg.mcp_servers {
            let trust = if srv.trust { "trust=true" } else { "approval aktif" };
            let target = srv
                .url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|u| format!("http: {u}"))
                .unwrap_or_else(|| format!("stdio: {}", srv.command));
            out.push_str(&format!(
                "> {} | {target} | {trust} · timeout {}s\n",
                srv.name, srv.timeout_secs
            ));
        }
        out.push_str("---\n");
        for f in SUPPORTED_MCP_FEATURES {
            out.push_str(&format!("* didukung | {f} |\n"));
        }
        for u in UNSUPPORTED_MCP_FEATURES {
            out.push_str(&format!("! belum didukung: {u}\n"));
        }
        out
    }

    pub(crate) async fn handle_verify(&self, arg: Option<&str>) -> Result<String> {
        let scope = arg.map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("all");
        let report = selencode::tools::verify::run_verify_plan_with_sandbox(
            &self.ctx.cwd,
            scope,
            &[],
            self.cfg.shell.timeout_secs.max(120),
            &self.ctx.sandbox,
        )
        .await?;
        Ok(report.format_menu_card())
    }

    pub(crate) fn providers_menu(&self) -> String {
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

    pub(crate) fn provider_presets_menu(&self) -> String {
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
    pub(crate) async fn handle_inline_shell(&self, command: &str) -> Result<String> {
        let mut cmd = self
            .ctx
            .sandbox
            .prepare_shell_command("inline_shell", command, &self.ctx.cwd)?;
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

    pub(crate) fn handle_undo(&self) -> Result<String> {
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

    pub(crate) fn handle_redo(&self) -> Result<String> {
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

    pub(crate) fn handle_index(&self, arg: Option<&str>) -> Result<String> {
        let mut idx = self
            .ctx
            .repo_index
            .lock()
            .expect("repo_index poisoned");
        Ok(idx.format_status_card(arg))
    }

    pub(crate) fn handle_jobs(&self, arg: Option<&str>) -> Result<String> {
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

    pub(crate) fn handle_route(&mut self, arg: Option<&str>) -> Result<String> {
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
            let budget_s = self
                .cfg
                .routing
                .max_cost_usd
                .map(|c| format!("${c:.4}"))
                .unwrap_or_else(|| "(tanpa batas)".to_string());
            let mut out = format!(
                "# Intelligent Model Orchestration & Telemetri\n\
                 * mode | {mode} | retry maks {} · budget {budget_s}\n\
                 > primary | {}:{} | fallback: {fb}\n",
                self.cfg.routing.max_retries, self.provider_name, self.pcfg.model
            );
            if let Ok(tel) = self.router_telemetry.lock() {
                if let Some(dec) = &tel.last_decision {
                    let esc = if dec.escalated { " · ESKALASI" } else { "" };
                    out.push_str(&format!(
                        "> keputusan akhir | {} (skor {}{esc}) | {}\n",
                        dec.route_id, dec.score, dec.reason
                    ));
                }
            }
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
            out.push_str("! Batas retry:     /route retries <0-3>\n");
            out.push_str("! Batas biaya:     /route budget <usd atau clear>   ·   Riwayat: /route history");
            return Ok(out);
        };

        if raw.eq_ignore_ascii_case("history") || raw.eq_ignore_ascii_case("hist") {
            let mut out = String::from("# Riwayat Keputusan & Performa Model\n");
            if let Ok(tel) = self.router_telemetry.lock() {
                if tel.recent_decisions.is_empty() {
                    out.push_str("! Belum ada keputusan routing di sesi ini.\n");
                } else {
                    for d in tel.recent_decisions.iter().rev().take(8) {
                        let esc = if d.escalated { " [eskalasi]" } else { "" };
                        out.push_str(&format!(
                            "* {} | skor {}{esc} | {}\n",
                            d.route_id, d.score, d.reason
                        ));
                    }
                }
            }
            if let Ok(perf) = self.store.load_model_perf() {
                if !perf.is_empty() {
                    out.push_str("---\n");
                    for p in perf.iter().take(8) {
                        out.push_str(&format!(
                            "> {} ({}) | {:.0}% ({}/{}) | avg {}ms\n",
                            p.route_id,
                            p.task_kind,
                            p.success_rate() * 100.0,
                            p.successes,
                            p.successes + p.failures,
                            p.avg_latency_ms()
                        ));
                    }
                }
            }
            return Ok(out);
        }

        if let Some(rest) = raw.strip_prefix("budget ") {
            let val = rest.trim();
            if val.eq_ignore_ascii_case("clear")
                || val.eq_ignore_ascii_case("off")
                || val.eq_ignore_ascii_case("none")
            {
                self.cfg.routing.max_cost_usd = None;
            } else {
                let usd: f64 = val
                    .trim_start_matches('$')
                    .parse()
                    .map_err(|_| anyhow::anyhow!("budget harus angka USD (mis. /route budget 1.50)"))?;
                if usd <= 0.0 {
                    anyhow::bail!("budget USD harus lebih besar dari 0");
                }
                self.cfg.routing.max_cost_usd = Some(usd);
            }
            let saved = config::save(&self.cfg)?;
            if !self.needs_setup {
                let pname = self.provider_name.clone();
                let _ = self.switch_provider(&pname);
            }
            let disp = self
                .cfg
                .routing
                .max_cost_usd
                .map(|c| format!("${c:.4}"))
                .unwrap_or_else(|| "(tanpa batas)".to_string());
            return Ok(format!(
                "# Batas Biaya Routing Diperbarui\n* max_cost_usd | {disp} | disimpan ke {}",
                saved.display()
            ));
        }

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

    pub(crate) async fn handle_lsp(&self, arg: Option<&str>) -> Result<String> {
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(self.ctx.lsp_manager.format_status_card());
        };
        if raw.eq_ignore_ascii_case("diag") || raw.eq_ignore_ascii_case("diagnostics") {
            let rep = self.ctx.lsp_manager.diagnostics(None).await?;
            let mut out = format!("# LSP Diagnostics ({})\n", rep.backend);
            if rep.diagnostics.is_empty() {
                out.push_str("* status | bersih | tidak ditemukan error sintaks/bahasa\n");
            } else {
                for d in rep.diagnostics.iter().take(20) {
                    out.push_str(&format!(
                        "> {}:{} | {} | {}\n",
                        d.rel_path, d.line, d.severity, d.message
                    ));
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw
            .strip_prefix("diag ")
            .or_else(|| raw.strip_prefix("diagnostics "))
        {
            let target = rest.trim();
            let rep = self.ctx.lsp_manager.diagnostics(Some(target)).await?;
            let mut out = format!("# LSP Diagnostics: {target} ({})\n", rep.backend);
            if rep.diagnostics.is_empty() {
                out.push_str("* status | bersih | tidak ditemukan error pada file ini\n");
            } else {
                for d in rep.diagnostics.iter().take(20) {
                    out.push_str(&format!(
                        "> L{}:C{} | {} | {}\n",
                        d.line, d.col, d.severity, d.message
                    ));
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw
            .strip_prefix("def ")
            .or_else(|| raw.strip_prefix("definition "))
        {
            let sym = rest.trim();
            let rep = self
                .ctx
                .lsp_manager
                .definition(None, None, None, Some(sym))
                .await?;
            let mut out = format!("# Definisi: {} ({})\n", rep.symbol, rep.backend);
            if rep.locations.is_empty() {
                out.push_str("! Definisi simbol tidak ditemukan di workspace.\n");
            } else {
                for loc in rep.locations.iter().take(15) {
                    out.push_str(&format!(
                        "* {}:{} | L{} | {}\n",
                        loc.rel_path, loc.col, loc.line, loc.preview
                    ));
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw
            .strip_prefix("refs ")
            .or_else(|| raw.strip_prefix("ref "))
            .or_else(|| raw.strip_prefix("references "))
        {
            let sym = rest.trim();
            let rep = self
                .ctx
                .lsp_manager
                .references(None, None, None, Some(sym))
                .await?;
            let mut out = format!(
                "# Referensi: {} ({} lokasi · {})\n",
                rep.symbol,
                rep.locations.len(),
                rep.backend
            );
            if rep.locations.is_empty() {
                out.push_str("! Tidak ada referensi simbol ditemukan.\n");
            } else {
                for loc in rep.locations.iter().take(20) {
                    out.push_str(&format!(
                        "> {}:{} | C{} | {}\n",
                        loc.rel_path, loc.line, loc.col, loc.preview
                    ));
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw.strip_prefix("hover ") {
            let sym = rest.trim();
            let rep = self
                .ctx
                .lsp_manager
                .hover(None, None, None, Some(sym))
                .await?;
            return Ok(format!(
                "# Hover: {} ({})\n{}",
                rep.symbol, rep.backend, rep.content
            ));
        }
        Ok(self.ctx.lsp_manager.format_status_card())
    }

    /// Kelola Long-Horizon Tasks persisten lintas sesi (`/task`).
    /// Mengembalikan `(kartu_menu, optional_resume_prompt)` — bila user menjalankan
    /// `/task resume <id>`, `optional_resume_prompt` berisi prompt pemulihan lengkap.
    pub(crate) fn handle_task(&self, arg: Option<&str>) -> Result<(String, Option<String>)> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let list = self.store.list_long_tasks(&cwd_str, 20)?;
            let mut out = String::from("# Long-Horizon Tasks (Lintas Sesi)\n");
            if list.is_empty() {
                out.push_str("! Belum ada tugas panjang tersimpan di folder ini.\n");
            } else {
                for t in list {
                    let mark = if t.status == "in_progress" { "*" } else { ">" };
                    let goal_short: String = t.goal.chars().take(52).collect();
                    out.push_str(&format!(
                        "{mark} {} | {} · {}/{} | {goal_short}\n",
                        t.id,
                        t.status,
                        t.completed_steps(),
                        t.steps.len()
                    ));
                }
            }
            out.push_str("---\n");
            out.push_str("! Buat tugas:   /task new <tujuan>\n");
            out.push_str("! Lihat detail: /task show <id>\n");
            out.push_str("! Jeda:         /task pause <id> [catatan]\n");
            out.push_str("! Lanjutkan:    /task resume <id>   ·   Hapus: /task rm <id>");
            return Ok((out, None));
        };

        if let Some(rest) = raw
            .strip_prefix("new ")
            .or_else(|| raw.strip_prefix("create "))
            .or_else(|| raw.strip_prefix("add "))
        {
            let goal = rest.trim();
            let rec = self.store.create_long_task(&cwd_str, goal, &[], 200_000)?;
            let card = format!(
                "# Long-Horizon Task Dibuat\n\
                 * {} | {} langkah | budget {} token\n\
                 > tujuan | {} |\n\
                 ---\n\
                 ! Jalankan / lanjutkan kapan saja: /task resume {}",
                rec.id,
                rec.steps.len(),
                rec.token_budget,
                rec.goal,
                rec.id
            );
            return Ok((card, None));
        }

        if let Some(rest) = raw
            .strip_prefix("show ")
            .or_else(|| raw.strip_prefix("get "))
        {
            let tid = rest.trim();
            let rec = self
                .store
                .get_long_task(tid)?
                .ok_or_else(|| anyhow::anyhow!("task '{tid}' tidak ditemukan"))?;
            let mut out = format!(
                "# Detail Task {} ({})\n\
                 * progres | {}/{} langkah | {}/{} token\n\
                 > tujuan | {} |\n\
                 > checkpoint | {} |\n\
                 ---\n",
                rec.id,
                rec.status,
                rec.completed_steps(),
                rec.steps.len(),
                rec.tokens_used,
                rec.token_budget,
                rec.goal,
                rec.checkpoint_note
            );
            for s in &rec.steps {
                let mark = if s.status == "done" { "*" } else { ">" };
                out.push_str(&format!(
                    "{mark} #{} [{}] | {} | {}\n",
                    s.id, s.status, s.role, s.title
                ));
            }
            out.push_str(&format!("---\n! Lanjutkan tugas ini: /task resume {}", rec.id));
            return Ok((out, None));
        }

        if let Some(rest) = raw.strip_prefix("pause ") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let tid = parts.next().unwrap_or("").trim();
            let note = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Dijeda manual oleh user");
            let rec = self
                .store
                .save_long_task_checkpoint(tid, Some("paused"), None, Some(note), 0)?
                .ok_or_else(|| anyhow::anyhow!("task '{tid}' tidak ditemukan"))?;
            return Ok((
                format!(
                    "# Task Dijeda & Disimpan\n* {} | paused | {}\n! Lanjutkan nanti: /task resume {}",
                    rec.id, rec.checkpoint_note, rec.id
                ),
                None,
            ));
        }

        if let Some(rest) = raw.strip_prefix("resume ") {
            let tid = rest.trim();
            let rec = self
                .store
                .save_long_task_checkpoint(
                    tid,
                    Some("in_progress"),
                    None,
                    Some("Dilanjutkan oleh user"),
                    0,
                )?
                .ok_or_else(|| anyhow::anyhow!("task '{tid}' tidak ditemukan"))?;
            let resume_prompt = rec.build_resume_prompt();
            let card = format!(
                "# Melanjutkan Long-Horizon Task {}\n\
                 * status | in_progress ({}/{}) | {}\n\
                 ! Mengirim konteks pemulihan ke Selena…",
                rec.id,
                rec.completed_steps(),
                rec.steps.len(),
                rec.goal
            );
            return Ok((card, Some(resume_prompt)));
        }

        if let Some(rest) = raw.strip_prefix("rm ").or_else(|| raw.strip_prefix("del ")) {
            let tid = rest.trim();
            if self.store.delete_long_task(tid)? {
                return Ok((format!("# Task Dihapus\n> {tid} | dihapus"), None));
            }
            anyhow::bail!("task '{tid}' tidak ditemukan");
        }

        anyhow::bail!("subperintah `/task {raw}` tidak dikenal. Ketik `/task` untuk melihat daftar & cara pakai.")
    }

    pub(crate) fn handle_memory(&self, arg: Option<&str>) -> Result<String> {
        use selencode::user_memory::{
            add_entry_in, load_frozen_snapshot, remove_entry_in, replace_entry_in,
            user_memory_dir, MemoryStoreKind,
        };
        let mem_dir = user_memory_dir();
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            // Perbaikan 3 (v0.5.9): SATU sumber kebenaran — baca langsung berkas store persisten
            // (`profile.md` & `memory.md`) agar `/memory` selalu 100% sama dengan isi berkas store.
            let live = load_frozen_snapshot(
                self.cfg.memory.max_memory_chars,
                self.cfg.memory.max_profile_chars,
            );
            let mem_txt = live.memory_text();
            let prof_txt = live.profile_text();
            let mut out = format!(
                "# Memori Persisten & Profil Pemilik\n\
                 * profil | {}/{} char | {}\n",
                prof_txt.chars().count(),
                self.cfg.memory.max_profile_chars,
                mem_dir.join("profile.md").display(),
            );
            if live.profile_entries.is_empty() {
                out.push_str("! (profil pemilik masih kosong)\n");
            } else {
                for (i, e) in live.profile_entries.iter().enumerate() {
                    out.push_str(&format!("> P#{} | profile | {e}\n", i + 1));
                }
            }
            out.push_str(&format!(
                "* memory | {}/{} char | {}\n",
                mem_txt.chars().count(),
                self.cfg.memory.max_memory_chars,
                mem_dir.join("memory.md").display(),
            ));
            if live.memory_entries.is_empty() {
                out.push_str("! (wadah memory masih kosong)\n");
            } else {
                for (i, e) in live.memory_entries.iter().enumerate() {
                    out.push_str(&format!("> M#{} | memory | {e}\n", i + 1));
                }
            }
            out.push_str("---\n");
            out.push_str("! Kelola memory:  /memory add <teks> · /memory update <no|teks> => <baru> · /memory rm <no|teks>\n");
            out.push_str("! Kelola profil:  /memory profile add <teks> · /memory profile replace <no|teks> => <baru> · /memory profile rm <no|teks>");
            return Ok(out);
        };
        // Perintah langsung pemilik untuk wadah `profile` / `memory` (Perbaikan 5 v0.5.7 & Perbaikan 3 v0.5.9).
        if let Some(rest) = arg
            .strip_prefix("profile ")
            .or_else(|| arg.strip_prefix("profil "))
        {
            let rest = rest.trim();
            if let Some(t) = rest.strip_prefix("add ") {
                let (idx, chars) = add_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Profile,
                    t,
                    self.cfg.memory.max_profile_chars,
                )?;
                return Ok(format!(
                    "# Profil Pemilik Disimpan\n* P#{idx} | {chars}/{} char | {}",
                    self.cfg.memory.max_profile_chars,
                    t.trim()
                ));
            }
            if let Some(t) = rest
                .strip_prefix("rm ")
                .or_else(|| rest.strip_prefix("del "))
                .or_else(|| rest.strip_prefix("remove "))
            {
                let (removed, chars) = remove_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Profile,
                    t,
                    self.cfg.memory.max_profile_chars,
                )?;
                return Ok(format!(
                    "# Profil Pemilik Dihapus\n> dihapus | {chars}/{} char | {removed}",
                    self.cfg.memory.max_profile_chars
                ));
            }
            if let Some(t) = rest.strip_prefix("replace ") {
                let Some((sel, new_val)) = t.split_once("=>") else {
                    anyhow::bail!("format: `/memory profile replace <no_atau_teks_lama> => <teks_baru>`");
                };
                let (idx, chars) = replace_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Profile,
                    sel,
                    new_val,
                    self.cfg.memory.max_profile_chars,
                )?;
                return Ok(format!(
                    "# Profil Pemilik Diperbarui\n* P#{idx} | {chars}/{} char | {}",
                    self.cfg.memory.max_profile_chars,
                    new_val.trim()
                ));
            }
            let (idx, chars) = add_entry_in(
                &mem_dir,
                MemoryStoreKind::Profile,
                rest,
                self.cfg.memory.max_profile_chars,
            )?;
            return Ok(format!(
                "# Profil Pemilik Disimpan\n* P#{idx} | {chars}/{} char | {rest}",
                self.cfg.memory.max_profile_chars
            ));
        }
        if let Some(rest) = arg.strip_prefix("user ") {
            let rest = rest.trim();
            if let Some(t) = rest.strip_prefix("add ") {
                let (idx, chars) = add_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Memory,
                    t,
                    self.cfg.memory.max_memory_chars,
                )?;
                return Ok(format!(
                    "# Memori Persisten Disimpan\n* M#{idx} | {chars}/{} char | {}",
                    self.cfg.memory.max_memory_chars,
                    t.trim()
                ));
            }
            if let Some(t) = rest
                .strip_prefix("rm ")
                .or_else(|| rest.strip_prefix("del "))
                .or_else(|| rest.strip_prefix("remove "))
            {
                let (removed, chars) = remove_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Memory,
                    t,
                    self.cfg.memory.max_memory_chars,
                )?;
                return Ok(format!(
                    "# Memori Persisten Dihapus\n> dihapus | {chars}/{} char | {removed}",
                    self.cfg.memory.max_memory_chars
                ));
            }
            if let Some(t) = rest.strip_prefix("replace ") {
                let Some((sel, new_val)) = t.split_once("=>") else {
                    anyhow::bail!("format: `/memory user replace <no_atau_teks_lama> => <teks_baru>`");
                };
                let (idx, chars) = replace_entry_in(
                    &mem_dir,
                    MemoryStoreKind::Memory,
                    sel,
                    new_val,
                    self.cfg.memory.max_memory_chars,
                )?;
                return Ok(format!(
                    "# Memori Persisten Diperbarui\n* M#{idx} | {chars}/{} char | {}",
                    self.cfg.memory.max_memory_chars,
                    new_val.trim()
                ));
            }
        }
        if let Some(rest) = arg
            .strip_prefix("add ")
            .or_else(|| arg.strip_prefix("global "))
        {
            let text = rest.trim();
            if text.is_empty() {
                anyhow::bail!("isi memori kosong. Contoh: /memory add Jalankan cargo test sebelum rilis");
            }
            let (idx, chars) = add_entry_in(
                &mem_dir,
                MemoryStoreKind::Memory,
                text,
                self.cfg.memory.max_memory_chars,
            )?;
            return Ok(format!(
                "# Memori Persisten Disimpan\n* M#{idx} | {chars}/{} char | {text}",
                self.cfg.memory.max_memory_chars
            ));
        }
        if let Some(rest) = arg
            .strip_prefix("update ")
            .or_else(|| arg.strip_prefix("edit "))
            .or_else(|| arg.strip_prefix("replace "))
        {
            let rest = rest.trim();
            let (sel, new_val) = if let Some((s, v)) = rest.split_once("=>") {
                (s.trim(), v.trim())
            } else {
                let mut parts = rest.splitn(2, char::is_whitespace);
                (
                    parts.next().unwrap_or("").trim(),
                    parts.next().unwrap_or("").trim(),
                )
            };
            if sel.is_empty() || new_val.is_empty() {
                anyhow::bail!(
                    "format: `/memory update <no|teks_lama> => <teks_baru>` atau `/memory update <no> <teks_baru>`"
                );
            }
            let clean_sel = sel.trim_start_matches(['M', 'm', '#']);
            let (idx, chars) = replace_entry_in(
                &mem_dir,
                MemoryStoreKind::Memory,
                clean_sel,
                new_val,
                self.cfg.memory.max_memory_chars,
            )?;
            return Ok(format!(
                "# Memori Persisten Diperbarui\n* M#{idx} | {chars}/{} char | {new_val}",
                self.cfg.memory.max_memory_chars
            ));
        }
        if let Some(rest) = arg
            .strip_prefix("rm ")
            .or_else(|| arg.strip_prefix("del "))
            .or_else(|| arg.strip_prefix("remove "))
        {
            let clean_sel = rest.trim().trim_start_matches(['M', 'm', '#']);
            let (removed, chars) = remove_entry_in(
                &mem_dir,
                MemoryStoreKind::Memory,
                clean_sel,
                self.cfg.memory.max_memory_chars,
            )?;
            return Ok(format!(
                "# Memori Persisten Dihapus\n> dihapus | {chars}/{} char | {removed}",
                self.cfg.memory.max_memory_chars
            ));
        }
        Ok("# Cara Pakai /memory\n\
            ! /memory                                       — lihat isi memory.md & profile.md\n\
            ! /memory add <catatan>                         — tambah ke memory.md (pagu 2.200 char)\n\
            ! /memory update <no|teks> => <catatan baru>    — perbarui entri memory.md\n\
            ! /memory rm <no|teks>                          — hapus entri memory.md\n\
            ! /memory profile add|replace|rm ...            — kelola profile.md (pagu 1.375 char)"
            .to_string())
    }

    pub(crate) async fn handle_skills(&mut self, arg: Option<&str>) -> Result<String> {
        let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            let scan = selencode::skills::scan_skills_with_warnings(&self.ctx.cwd);
            return Ok(selencode::skills::format_skills_list_card(
                &scan,
                self.ctx.auto_skills,
            ));
        };

        if arg.eq_ignore_ascii_case("save-token")
            || arg.eq_ignore_ascii_case("save token")
            || arg.eq_ignore_ascii_case("save")
        {
            let p = selencode::skills::install_token_efficiency_skill(&self.ctx.cwd)?;
            return Ok(format!(
                "# Skill `save-token` Terpasang\n\
                 * lokasi | {} | siap dipakai via `/skill save-token`\n\
                 ! Mengikuti pola Progressive Disclosure SelenCode (langkah & pembacaan minimal).",
                p.display()
            ));
        }

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
                "# Auto Selena Improvement Skills\n\
                 * status | {label} | tersimpan ke {}",
                saved.display()
            ));
        }

        if arg.eq_ignore_ascii_case("review") {
            if self.messages.is_empty() {
                return Ok(
                    "# Selena Improvement\n! Belum ada percakapan untuk ditinjau.".into(),
                );
            }
            match run_self_improvement_review(self.provider.clone(), &self.ctx, &self.messages)
                .await?
            {
                Some(summary) => Ok(format!("# {summary}\n* tersimpan | .selencode/skills/ | siap dipakai di sesi berikutnya")),
                None => Ok("# Selena Improvement\n! Belum ditemukan pola baru yang perlu disimpan atau di-patch sebagai skill.".into()),
            }
        } else if let Some(name) = arg
            .strip_prefix("override ")
            .or_else(|| arg.strip_prefix("copy-to-project "))
        {
            let name = name.trim();
            if name.is_empty() {
                anyhow::bail!("format: `/skills override <nama-skill-user>`");
            }
            let detail = format!(
                "Menyalin skill pengguna '{name}' ke store proyek (.selencode/skills/) sebagai salinan timpa proyek (override)."
            );
            if !self
                .ctx
                .approver
                .ask(&format!("skills override {name}"), &detail)
                .await
            {
                return Ok(format!(
                    "# Salin-ke-Proyek Dibatalkan\n! Ditolak user — skill '{name}' tetap di store pengguna."
                ));
            }
            let (src, dst) = selencode::skills::copy_user_skill_to_project(&self.ctx.cwd, name)?;
            Ok(format!(
                "# Salinan Proyek Dibuat (Override Eksplisit)\n\
                 * aktif (project) | {}\n\
                 > menimpa (user)  | {}\n\
                 ! Hapus kapan saja dengan `/skills rm {name}` untuk kembali memakai versi global.",
                dst.display(),
                src.display()
            ))
        } else if let Some(name) = arg.strip_prefix("show ").or_else(|| arg.strip_prefix("view ")) {
            let (info, content) = load_skill(&self.ctx.cwd, name.trim())?;
            let mut out = format!("# Skill: {} ({}) — {}\n", info.name, info.scope, info.path.display());
            for sh in &info.shadows {
                out.push_str(&format!(
                    "! Catatan: salinan {} ini menimpa versi {} di {}\n",
                    info.scope,
                    sh.scope,
                    sh.path.display()
                ));
            }
            for line in content.lines().take(40) {
                out.push_str(line);
                out.push('\n');
            }
            Ok(out)
        } else if let Some(rest) = arg.strip_prefix("install ") {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            let Some(url) = toks.first().copied() else {
                anyhow::bail!("format: `/skills install <url-git|url-SKILL.md> [--rename <nama>]`");
            };
            let mut rename: Option<&str> = None;
            for i in 1..toks.len() {
                if toks[i] == "--rename" && i + 1 < toks.len() {
                    rename = Some(toks[i + 1]);
                }
            }
            let staged = selencode::skill_installer::stage_skill_from_source(url).await?;
            let summary = selencode::skill_installer::format_install_approval_summary(&staged, rename);
            if !self
                .ctx
                .approver
                .ask(&format!("skills install {}", rename.unwrap_or(&staged.frontmatter.name)), &summary)
                .await
            {
                return Ok("# Pemasangan Skill Dibatalkan\n! Ditolak user — tidak ada berkas yang ditulis.".to_string());
            }
            let user_root = selencode::skills::user_skills_dir()
                .ok_or_else(|| anyhow::anyhow!("direktori skill pengguna tidak tersedia"))?;
            let rep = selencode::skill_installer::finalize_staged_skill_install(&staged, &user_root, rename)?;
            Ok(format!(
                "# Skill '{}' Berhasil Dipasang\n\
                 * lokasi | {} | {} berkas ({} skrip, tidak dijalankan otomatis)\n\
                 > sumber | {} | dicatat di .selen-source.json\n\
                 ! Siap dipakai via `skill_view` atau `/skill {}` pada sesi berikutnya.",
                rep.name,
                rep.target_dir.display(),
                rep.files_copied,
                rep.scripts_count,
                staged.source_url,
                rep.name
            ))
        } else if let Some(name) = arg.strip_prefix("remove ") {
            let user_root = selencode::skills::user_skills_dir()
                .ok_or_else(|| anyhow::anyhow!("direktori skill pengguna tidak tersedia"))?;
            let msg = selencode::skill_installer::remove_installed_skill(
                name.trim(),
                &user_root,
                &self.ctx.cwd,
            )?;
            Ok(format!("# Skill Dicopot\n> {} | {msg}", name.trim()))
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

    /// Tangani `/review [pending | approve <id> | discard <id> | status]` (v0.5.1).
    pub(crate) fn handle_review(&mut self, arg: Option<&str>) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        let raw = arg.map(|s| s.trim()).filter(|s| !s.is_empty());
        let Some(cmd) = raw else {
            return self.format_pending_review_menu(&cwd_str);
        };
        let lower = cmd.to_ascii_lowercase();
        if matches!(lower.as_str(), "pending" | "list" | "ls") {
            return self.format_pending_review_menu(&cwd_str);
        }
        if matches!(lower.as_str(), "status" | "stats" | "info") {
            let model_str = match self.cfg.aux.effective_review_model() {
                Some(m) => format!("{m} (aux override)"),
                None if !self.pcfg.model.trim().is_empty() && self.pcfg.model != "(belum diatur)" => {
                    format!("{} (model utama sesi)", self.pcfg.model)
                }
                None => "(belum ada model)".to_string(),
            };
            let runs = self.store.list_recent_review_runs(&cwd_str, 5).unwrap_or_default();
            let pending = self
                .store
                .list_review_proposals(Some(&cwd_str), Some("pending"), 20)
                .unwrap_or_default();
            let backups = self
                .store
                .list_review_backups(10)
                .unwrap_or_default()
                .into_iter()
                .filter(|b| !b.rolled_back)
                .count();
            let skip_line = match self.store.get_last_review_skip().ok().flatten() {
                Some((r, det, _)) => format!("> lewati terakhir | {r} | {det}\n"),
                None => String::new(),
            };
            let mut out = format!(
                "# Status Selena Improvement\n\
                 * aktif | {} | model: {model_str}\n\
                 > batas | max_tokens={} (<=5000/call) · cooldown={} mnt · interval={} giliran · min_tool={}\n\
                 > staging | {} usulan menunggu persetujuan · {} cadangan patch siap `/review undo`\n\
                 {skip_line}",
                if self.cfg.review.enabled { "ya" } else { "nonaktif" },
                self.cfg.review.max_tokens,
                self.cfg.review.cooldown_mins,
                self.cfg.review.turns_interval,
                self.cfg.review.min_tool_calls,
                pending.len(),
                backups
            );
            if !runs.is_empty() {
                out.push_str("---\n");
                for r in runs {
                    let note = if r.report_text.is_empty() {
                        if r.event_calls > 1 && r.call_index < r.event_calls {
                            "(panggilan awal; lanjut retry)".to_string()
                        } else {
                            "(tanpa perubahan)".to_string()
                        }
                    } else {
                        r.report_text
                    };
                    let call_tag = if r.event_calls > 1 {
                        format!(" [call {}/{} {}]", r.call_index, r.event_calls, r.call_kind)
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "> {} | {}{call_tag} ({} in · {} cache / {} out) | {note}\n",
                        &r.created_at.chars().take(19).collect::<String>(),
                        r.model,
                        r.input_tokens,
                        r.cache_read_tokens,
                        r.output_tokens
                    ));
                }
            }
            out.push_str("---\n! Staging: /review pending · Setujui: /review approve <id> · Buang: /review discard <id> · Putar balik patch: /review undo [id]");
            return Ok(out);
        }
        if matches!(lower.as_str(), "backups" | "history") {
            let items = self.store.list_review_backups(10)?;
            if items.is_empty() {
                return Ok("# Cadangan Patch Selena Improvement (0)\n! Belum ada patch langsung yang tercatat.".into());
            }
            let mut out = format!("# Cadangan Patch Selena Improvement ({})\n", items.len());
            for b in items {
                let st = if b.rolled_back { "sudah diputar balik" } else { "aktif" };
                out.push_str(&format!(
                    "> #{} | {} ({st}) | {} — /review undo {}\n",
                    b.id, b.skill_name, b.reason, b.id
                ));
            }
            return Ok(out);
        }
        if lower == "undo" || lower == "rollback" {
            let msg = selencode::agent::rollback_direct_review_patch(&self.store, None)?;
            return Ok(format!("# Patch Diputar Balik\n* undo | {msg}"));
        }
        if let Some(rest) = lower
            .strip_prefix("undo ")
            .or_else(|| lower.strip_prefix("rollback "))
        {
            let id = rest
                .trim()
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("ID cadangan tidak valid: '{rest}'. Contoh: /review undo 1"))?;
            let msg = selencode::agent::rollback_direct_review_patch(&self.store, Some(id))?;
            return Ok(format!("# Patch Diputar Balik\n* #{id} | {msg}"));
        }
        if let Some(rest) = lower
            .strip_prefix("approve ")
            .or_else(|| lower.strip_prefix("ok "))
            .or_else(|| lower.strip_prefix("yes "))
        {
            let id = rest
                .trim()
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("ID proposal tidak valid: '{rest}'. Contoh: /review approve 1"))?;
            let msg = selencode::agent::apply_staged_proposal(&self.store, &self.ctx.cwd, id)?;
            return Ok(format!("# Usulan Review Disetujui\n* #{id} | {msg} | selesai"));
        }
        if let Some(rest) = lower
            .strip_prefix("discard ")
            .or_else(|| lower.strip_prefix("reject "))
            .or_else(|| lower.strip_prefix("rm "))
            .or_else(|| lower.strip_prefix("del "))
        {
            let id = rest
                .trim()
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("ID proposal tidak valid: '{rest}'. Contoh: /review discard 1"))?;
            let msg = selencode::agent::discard_staged_proposal(&self.store, id)?;
            return Ok(format!("# Usulan Review Dibuang\n> #{id} | {msg} | data utuh"));
        }
        Ok("# Cara Pakai /review (Selena Improvement)\n\
            > /review pending | | lihat usulan staging yang menunggu persetujuan\n\
            > /review approve <id> | | terapkan usulan staging tertentu\n\
            > /review discard <id> | | buang usulan tanpa mengubah data asli\n\
            > /review undo [id] | | putar balik patch langsung terakhir ke skill improvement\n\
            > /review status | | lihat model efektif, cache, alasan lewati terakhir, & riwayat token"
            .to_string())
    }

    fn format_pending_review_menu(&self, cwd_str: &str) -> Result<String> {
        let items = self
            .store
            .list_review_proposals(Some(cwd_str), Some("pending"), 20)?;
        if items.is_empty() {
            return Ok(
                "# Usulan Review Tertunda (0)\n\
                 ! Tidak ada usulan hapus/timpa memory atau skill yang menunggu persetujuan.\n\
                 ! Cek konfigurasi & biaya review: /review status"
                    .to_string(),
            );
        }
        let mut out = format!("# Usulan Review Tertunda ({})\n", items.len());
        for p in &items {
            let detail = selencode::agent::review::format_proposal_detail(p);
            for (idx, line) in detail.lines().enumerate() {
                if idx == 0 {
                    out.push_str(&format!("* #{} | {} ({}) | {}\n", p.id, p.kind, p.target, p.summary));
                } else {
                    out.push_str(&format!("  {line}\n"));
                }
            }
        }
        out.push_str("---\n! Setujui: /review approve <id>   ·   Buang: /review discard <id>");
        Ok(out)
    }

    /// Tangani `/search <kata kunci>` — pencarian lintas sesi berbasis SQLite FTS5 (v0.5.6 Bagian 4A).
    pub(crate) fn handle_search(&self, arg: Option<&str>) -> Result<String> {
        let Some(query) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(
                "# Pencarian Lintas Sesi (`/search`)\n\
                 > /search <kata kunci> | | cari frasa/kata di seluruh transkrip sesi tersimpan (FTS5)\n\
                 ! Contoh: `/search KODE-UNIK-7391` — lalu lanjutkan sesi dengan `/resume <id_sesi>`."
                    .to_string(),
            );
        };
        let _ = self.store.backfill_fts_if_needed();
        let hits = self.store.search_sessions_fts(query, None, 10)?;
        if hits.is_empty() {
            return Ok(format!(
                "# Hasil Pencarian Sesi: \"{query}\"\n\
                 ! Tidak ada sesi tersimpan yang cocok di database sesi lokal.\n\
                 ! Catatan: pencarian hanya mencakup teks percakapan yang tersimpan di DB sesi mesin ini."
            ));
        }
        let mut out = format!("# Hasil Pencarian Sesi ({} cocok untuk \"{query}\")\n", hits.len());
        for (i, h) in hits.iter().enumerate() {
            let short_id: String = h.session_id.chars().take(10).collect();
            let dt: String = h.created_at.chars().take(19).collect();
            out.push_str(&format!(
                "* #{} | {} ({dt}) | id: {short_id}\n",
                i + 1,
                h.title
            ));
            out.push_str(&format!("  > cuplikan | {} |\n", h.snippet));
        }
        out.push_str("---\n! Lanjutkan sesi: /resume <id_sesi>   ·   Cakupan: teks tersimpan di DB sesi lokal");
        Ok(out)
    }

    /// Tangani `/watch [list | cancel <id>]` — kelola pemantauan tugas asinkron (Perbaikan 3 v0.5.7).
    pub(crate) fn handle_watch(&self, arg: Option<&str>, chat_id: Option<i64>) -> Result<String> {
        let raw = arg.map(|s| s.trim()).filter(|s| !s.is_empty());
        if let Some(cmd) = raw {
            let lower = cmd.to_ascii_lowercase();
            if let Some(rest) = lower
                .strip_prefix("cancel ")
                .or_else(|| lower.strip_prefix("stop "))
                .or_else(|| lower.strip_prefix("rm "))
            {
                let id: i64 = rest
                    .trim()
                    .trim_start_matches('#')
                    .parse()
                    .map_err(|_| anyhow::anyhow!("ID pantauan tidak valid: '{rest}'. Contoh: /watch cancel 1"))?;
                if self.store.cancel_job_watch(id, chat_id)? {
                    return Ok(format!(
                        "# Pemantauan #{id} Dibatalkan\n> #{id} | dihentikan | tidak akan mengirim laporan lagi"
                    ));
                }
                anyhow::bail!("pemantauan aktif #{id} tidak ditemukan");
            }
        }
        let watches = self.store.list_job_watches(chat_id, false)?;
        let active_n = watches.iter().filter(|w| w.status == "active").count();
        if watches.is_empty() {
            return Ok(
                "# Pemantauan Tugas Asinkron (0)\n\
                 ! Belum ada tugas asinkron yang dipantau.\n\
                 ! Saat agent memasukkan job ke antrean, pantauan didaftarkan otomatis via `watch_job`."
                    .to_string(),
            );
        }
        let mut out = format!(
            "# Pemantauan Tugas Asinkron ({active_n} aktif / {} total)\n",
            watches.len()
        );
        for w in watches.iter().take(12) {
            let mark = if w.status == "active" { "*" } else { ">" };
            let stage = if w.last_stage.is_empty() {
                "menunggu cek".to_string()
            } else if w.last_progress.is_empty() {
                w.last_stage.clone()
            } else {
                format!("{} ({})", w.last_stage, w.last_progress)
            };
            let checked = if w.last_checked_at.trim().is_empty() {
                "belum dicek".to_string()
            } else {
                let ts: String = w.last_checked_at.chars().take(19).collect();
                format!("dicek {} UTC", ts.replace('T', " "))
            };
            out.push_str(&format!(
                "{mark} #{} {} | {} · {} | {checked} · tiap {} dtk ({})\n",
                w.id, w.label, w.status, stage, w.interval_secs, w.check_kind
            ));
        }
        out.push_str("---\n! Batalkan pantauan aktif: /watch cancel <id>   ·   Maks 5 pantauan aktif per chat (0 token LLM).");
        Ok(out)
    }

    /// Tangani `/usage` — laporan pemakaian token sesi & hari ini langsung dari SQLite `usage`
    /// (Perbaikan 7 v0.5.7 — 0 panggilan provider, format bersih maks 1 emoji).
    pub(crate) fn handle_usage(&self) -> Result<String> {
        let today_utc = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let rep = self.store.usage_report(&self.session_id, &today_utc)?;

        let sum_rows = |rows: &[selencode::store::UsageBreakdownRow]| -> (u64, u64, u64, u64, u64) {
            rows.iter().fold((0, 0, 0, 0, 0), |(c, i, o, cr, cw), r| {
                (
                    c + r.calls,
                    i + r.input_tokens,
                    o + r.output_tokens,
                    cr + r.cache_read_tokens,
                    cw + r.cache_write_tokens,
                )
            })
        };
        let (sc, si, so, scr, scw) = sum_rows(&rep.session_rows);
        let (tc, ti, to, tcr, tcw) = sum_rows(&rep.today_rows);
        let fmt_cache = |inp: u64, cr: u64, cw: u64| -> String {
            if cr == 0 && cw == 0 {
                String::new()
            } else if cr > 0 && inp > 0 {
                let pct = (cr as f64 / inp as f64) * 100.0;
                if cw > 0 {
                    format!(" · cache read {cr} ({pct:.1}%) / write {cw}")
                } else {
                    format!(" · cache read {cr} ({pct:.1}%)")
                }
            } else {
                format!(" · cache read {cr} / write {cw}")
            }
        };

        let mut models_seen: Vec<String> = Vec::new();
        for r in rep.session_rows.iter().chain(rep.today_rows.iter()) {
            if !models_seen.contains(&r.model) {
                models_seen.push(r.model.clone());
            }
        }
        let models_line = if models_seen.is_empty() {
            self.pcfg.model.clone()
        } else {
            models_seen.join(", ")
        };

        let mut out = format!(
            "# 📊 Laporan Pemakaian Token (/usage)\n\
             * sesi aktif | {sc} panggilan | {si} in / {so} out{}\n\
             * hari ini ({}) | {tc} panggilan | {ti} in / {to} out{}\n\
             > model terpakai | {models_line} | langsung dari tabel `usage` (0 panggilan API)\n",
            fmt_cache(si, scr, scw),
            rep.today_date,
            fmt_cache(ti, tcr, tcw),
        );

        let push_kind_breakdown = |out: &mut String, title: &str, rows: &[selencode::store::UsageBreakdownRow]| {
            if rows.is_empty() {
                return;
            }
            out.push_str(&format!("---\n! Rincian {title}:\n"));
            for r in rows {
                let lbl = match r.kind.as_str() {
                    "main" => "utama",
                    "fallback" => "fallback",
                    "review" => "review latar",
                    "cron" => "cron terjadwal",
                    "delegate_work" => "delegate work",
                    other => other,
                };
                out.push_str(&format!(
                    "> {lbl} | {} | {} call · {} in / {} out\n",
                    r.model, r.calls, r.input_tokens, r.output_tokens
                ));
            }
        };
        push_kind_breakdown(&mut out, "sesi ini", &rep.session_rows);
        push_kind_breakdown(&mut out, "hari ini", &rep.today_rows);
        Ok(out.trim_end().to_string())
    }

    /// Tangani `/cron [add|list|remove|pause|resume]` (v0.5.6 Bagian 3).
    pub(crate) fn handle_cron(&self, arg: Option<&str>, chat_id: Option<i64>) -> Result<String> {
        use selencode::cron::{
            compute_next_run_after, format_schedule_human, parse_schedule, CRON_GATEWAY_NOTE,
        };
        let raw = arg.map(|s| s.trim()).filter(|s| !s.is_empty());
        let Some(cmd) = raw else {
            return self.format_cron_list_menu();
        };
        let lower = cmd.to_ascii_lowercase();
        if matches!(lower.as_str(), "list" | "ls" | "status") {
            return self.format_cron_list_menu();
        }
        if let Some(rest) = cmd
            .strip_prefix("add ")
            .or_else(|| cmd.strip_prefix("ADD "))
        {
            // Format: `/cron add <nama> <jadwal_1_token_atau_every_5m> <prompt...>`
            // Contoh: `/cron add cek-log 5m Baca file log.txt dan laporkan baris pertamanya`
            // atau `/cron add pagi every 2h Cek status proyek` / `/cron add harian daily 08:30 Ringkas progress`
            let mut toks = rest.trim().split_whitespace();
            let Some(name) = toks.next() else {
                anyhow::bail!("format: `/cron add <nama> <5m|2h|08:30> <prompt>`");
            };
            let Some(t1) = toks.next() else {
                anyhow::bail!("jadwal wajib diisi. Contoh: `/cron add cek 5m Baca file X`");
            };
            let (sched_raw, prompt) = if matches!(
                t1.to_ascii_lowercase().as_str(),
                "every" | "tiap" | "setiap" | "daily" | "harian"
            ) {
                let Some(t2) = toks.next() else {
                    anyhow::bail!("jadwal tidak lengkap setelah '{t1}'. Contoh: `every 5m` atau `daily 08:30`");
                };
                let rest_prompt: Vec<&str> = toks.collect();
                (format!("{t1} {t2}"), rest_prompt.join(" "))
            } else {
                let rest_prompt: Vec<&str> = toks.collect();
                (t1.to_string(), rest_prompt.join(" "))
            };
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt job tidak boleh kosong. Contoh: `/cron add cek 5m Baca file X`");
            }
            let parsed = parse_schedule(&sched_raw)?;
            let spec = parsed.canonical_spec();
            let next_run = compute_next_run_after(&spec, chrono::Utc::now())?;
            let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
            let target_chat = chat_id
                .or_else(|| self.cfg.telegram.allowed_users.first().copied())
                .unwrap_or(0);
            let job = self.store.add_cron_job(
                name,
                &spec,
                &prompt,
                &cwd_str,
                &[],
                false,
                target_chat,
                &next_run,
            )?;
            return Ok(format!(
                "# Job Terjadwal Ditambahkan (#{})\n\
                 * {} | {} | izin: hanya baca (default aman)\n\
                 > jadwal berikut | {} | folder: {}\n\
                 ---\n\
                 ! {CRON_GATEWAY_NOTE}\n\
                 ! Untuk izin shell/tulis eksplisit per job, gunakan CLI: `selen cron add --name ... --allow-shell ...`",
                job.id,
                job.name,
                format_schedule_human(&job.schedule_spec),
                job.next_run_at,
                job.cwd
            ));
        }
        if let Some(rest) = cmd
            .strip_prefix("remove ")
            .or_else(|| cmd.strip_prefix("rm "))
            .or_else(|| cmd.strip_prefix("del "))
        {
            let target = rest.trim();
            if self.store.remove_cron_job(target)? {
                return Ok(format!("# Job Terjadwal Dihapus\n> {target} | dihapus permanen"));
            }
            anyhow::bail!("job '{target}' tidak ditemukan");
        }
        if let Some(rest) = cmd.strip_prefix("pause ") {
            let target = rest.trim();
            if let Some(j) = self.store.set_cron_job_enabled(target, false, None)? {
                return Ok(format!("# Job Terjadwal Dijeda\n> {} | nonaktif sementara", j.name));
            }
            anyhow::bail!("job '{target}' tidak ditemukan");
        }
        if let Some(rest) = cmd.strip_prefix("resume ") {
            let target = rest.trim();
            let Some(existing) = self.store.get_cron_job(target)? else {
                anyhow::bail!("job '{target}' tidak ditemukan");
            };
            let next = compute_next_run_after(&existing.schedule_spec, chrono::Utc::now())?;
            if let Some(j) = self.store.set_cron_job_enabled(target, true, Some(&next))? {
                return Ok(format!(
                    "# Job Terjadwal Diaktifkan Kembali\n* {} | {} | jadwal berikut: {}",
                    j.name,
                    format_schedule_human(&j.schedule_spec),
                    j.next_run_at
                ));
            }
            anyhow::bail!("job '{target}' tidak ditemukan");
        }
        Ok(format!(
            "# Cara Pakai /cron\n\
             > /cron list | | lihat semua job terjadwal\n\
             > /cron add | <nama> <5m/2h/08:30> <prompt> | tambah job baca-saja\n\
             > /cron pause | <nama/id> | jeda sementara\n\
             > /cron resume | <nama/id> | aktifkan kembali\n\
             > /cron remove | <nama/id> | hapus job\n\
             ---\n\
             ! {CRON_GATEWAY_NOTE}"
        ))
    }

    fn format_cron_list_menu(&self) -> Result<String> {
        use selencode::cron::{format_schedule_human, CRON_GATEWAY_NOTE};
        let jobs = self.store.list_cron_jobs()?;
        if jobs.is_empty() {
            return Ok(format!(
                "# Daftar Job Terjadwal (0)\n\
                 ! Belum ada job terjadwal.\n\
                 ! Tambah: `/cron add <nama> <5m/2h/08:30> <prompt>` atau `selen cron add --name ... --schedule ... --prompt ...`\n\
                 ! {CRON_GATEWAY_NOTE}"
            ));
        }
        let mut out = format!("# Daftar Job Terjadwal ({})\n", jobs.len());
        for j in &jobs {
            let st = if j.enabled { "aktif" } else { "dijeda" };
            let last = if j.last_run_at.is_empty() {
                "belum pernah".to_string()
            } else {
                format!("{} ({})", j.last_run_at.chars().take(19).collect::<String>(), j.last_status)
            };
            let perms = if j.allowed_shell.is_empty() && !j.allow_write {
                "baca-saja".to_string()
            } else {
                format!(
                    "shell:[{}] write:{}",
                    j.allowed_shell.join(","),
                    if j.allow_write { "ya" } else { "tidak" }
                )
            };
            let mark = if j.enabled { "*" } else { ">" };
            out.push_str(&format!(
                "{mark} #{} {} | {} · {st} · izin: {perms} | terakhir: {last}\n",
                j.id,
                j.name,
                format_schedule_human(&j.schedule_spec)
            ));
        }
        out.push_str(&format!("---\n! {CRON_GATEWAY_NOTE}"));
        Ok(out)
    }

    pub(crate) fn handle_use_skill(&mut self, arg: Option<&str>) -> Result<String> {
        let raw = arg
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("sebutkan nama skill. Contoh: /skill tinjau-rust"))?;
        let mut parts = raw.split_whitespace();
        let name = parts.next().unwrap_or(raw);
        let want_full = parts.any(|p| p == "--full" || p.eq_ignore_ascii_case("full"));
        let (info, content, refs) = selencode::skills::load_skill_with_refs(&self.ctx.cwd, name)?;
        let body = if want_full {
            content
        } else {
            selencode::tools::skill::build_skill_working_copy(&info.name, &content)
        };
        let mut msg_body = format!("[Skill dimuat manual via /skill: {}]\n{body}", info.name);
        if !refs.is_empty() {
            msg_body.push_str("\n---\nReferensi tersedia (muat dengan `skill_view(name, path)`):\n");
            for r in &refs {
                msg_body.push_str(&format!("- {r}\n"));
            }
        }
        // Sisipkan sebagai pesan User (BUKAN System) agar prefix System Prompt statis tetap beku!
        let msg = Message::text(Role::User, msg_body);
        let _ = self.store.save_message(&self.session_id, &msg);
        self.messages.push(msg);
        let refs_note = if refs.is_empty() {
            String::new()
        } else {
            format!("\n! Referensi: {}", refs.join(", "))
        };
        Ok(format!(
            "# Skill Diaktifkan\n* {} | {} · {} | {}{refs_note}",
            info.name,
            info.scope,
            info.category,
            selencode::skills::truncate_index_desc(
                &info.summary,
                selencode::skills::SKILL_INDEX_DESC_MAX_CHARS
            )
        ))
    }

    pub(crate) fn handle_learn_skill(&self, arg: Option<&str>) -> Result<String> {
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

    pub(crate) fn handle_init(&self) -> Result<String> {
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

    pub(crate) fn handle_sandbox(&mut self, arg: Option<&str>) -> Result<String> {
        use selencode::sandbox::{OsIsolationPolicy, SandboxMode};
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(self.ctx.sandbox.format_status_card());
        };
        if raw.eq_ignore_ascii_case("audit") || raw.starts_with("audit ") {
            let limit = raw
                .strip_prefix("audit ")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .map(|n| n.clamp(1, 50))
                .unwrap_or(15);
            let entries = self.ctx.sandbox.recent_audit_entries(limit);
            let mut out = format!("# Audit Log Sandbox ({} entri terbaru)\n", entries.len());
            if entries.is_empty() {
                out.push_str("! Belum ada entri audit log tersimpan.\n");
            } else {
                for e in entries {
                    let mark = if e.allowed { "*" } else { ">" };
                    let verdict = if e.allowed { "OK" } else { "BLOKIR" };
                    let short_ts: String = e.ts.chars().take(19).collect();
                    out.push_str(&format!(
                        "{mark} {short_ts} | {} · {verdict} | {} ({})\n",
                        e.action, e.target, e.detail
                    ));
                }
            }
            return Ok(out);
        }
        if let Some(rest) = raw.strip_prefix("isolate ") {
            let pol = OsIsolationPolicy::parse(rest).ok_or_else(|| {
                anyhow::anyhow!(
                    "kebijakan isolasi '{}' tidak dikenal. Pilihan: off, auto, strict",
                    rest.trim()
                )
            })?;
            self.ctx.sandbox.set_os_isolation(pol);
            self.cfg.sandbox.os_isolate = pol.as_str().to_string();
            let saved = config::save(&self.cfg)?;
            return Ok(format!(
                "# Isolasi OS Sandbox Diperbarui\n\
                 * os_isolate | {} | backend: {}\n\
                 ! Config disimpan ke: {}",
                pol.as_str(),
                self.ctx.sandbox.backend().label(),
                saved.display()
            ));
        }
        if let Some(mode) = SandboxMode::parse(raw) {
            self.ctx.sandbox.set_mode(mode);
            self.cfg.sandbox.mode = mode.as_str().to_string();
            let saved = config::save(&self.cfg)?;
            return Ok(format!(
                "# Mode Sandbox Diperbarui\n\
                 * mode | {} | backend: {}\n\
                 ! Config disimpan ke: {}",
                mode.as_str(),
                self.ctx.sandbox.backend().label(),
                saved.display()
            ));
        }
        anyhow::bail!(
            "argumen `/sandbox {raw}` tidak dikenal. Pilihan: read-only, workspace-write, unrestricted, isolate <off|auto|strict>, audit"
        )
    }

    pub(crate) fn handle_approve(&mut self, arg: Option<&str>) -> Result<String> {
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

    pub(crate) fn handle_export(&self, arg: Option<&str>) -> Result<String> {
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

    pub(crate) fn provider_usage_footer(&self) -> String {
        "! Preset siap pakai:     /provider preset   |   /provider preset <id> [model] [ENV|key]\n\
         ! Tambah OpenAI-compat:  /provider add <nama> openai <model> <base_url> <ENV_atau_key>\n\
         ! Tambah Claude/Gemini:  /provider add <nama> <anthropic|gemini> <model> <ENV_atau_key>\n\
         ! Ubah model / default:  /provider model <nama> <id-model>   |   /provider default <nama>"
            .to_string()
    }

    pub(crate) fn provider_usage_menu(&self) -> String {
        format!(
            "# Cara Pakai /provider\n\
             {}\n\
             ---\n\
             ! Contoh OpenRouter: /provider add openrouter openai anthropic/claude-3.7-sonnet https://openrouter.ai/api/v1 OPENROUTER_API_KEY\n\
             ! Contoh Gemini:     /provider add gemini gemini gemini-2.5-pro GEMINI_API_KEY",
            self.provider_usage_footer()
        )
    }

    pub(crate) fn print_models(&self) {
        println!("{}", strip_menu_markup(&self.models_menu()));
    }

    /// Kartu daftar 10 sesi terakhir cwd ini bernomor (`#1..#10`) dengan judul kontekstual.
    pub(crate) fn sessions_menu(&self) -> Result<String> {
        let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
        // Bersihkan sesi kosong 0-pesan lama agar daftar 10 sesi selalu berisi percakapan nyata.
        let _ = self.store.prune_empty_sessions(&cwd_str, &self.session_id);
        let list = self.store.list_sessions(&cwd_str, 10)?;
        if list.is_empty() {
            return Ok(
                "# Sesi Tersimpan\n! Belum ada sesi tersimpan untuk folder ini (sesi baru otomatis tersimpan setelah pesan pertama dikirim).".to_string(),
            );
        }
        let mut out = format!("# {} Sesi Terakhir di Folder Ini\n", list.len());
        for (i, s) in list.iter().enumerate() {
            let num = i + 1;
            let is_cur = s.id == self.session_id;
            let mark = if is_cur { "*" } else { ">" };
            let short_ts: String = s.created_at.chars().take(16).collect();
            let short_ts = short_ts.replace('T', " ");
            let msg_count = self.store.count_messages(&s.id).unwrap_or(0);
            let active_tag = if is_cur { " (aktif)" } else { "" };
            out.push_str(&format!(
                "{mark} #{num} | {}{active_tag} | {short_ts} · {msg_count} msg · {}\n",
                s.title, s.model
            ));
        }
        out.push_str("---\n");
        out.push_str("! Lanjutkan sesi:  /resume <nomor>   ·   Hapus sesi: /resume rm <nomor>\n");
        out.push_str("! Info sesi aktif: /resume info   ·   Ubah judul: /resume rename <judul baru>\n");
        out.push_str("! Dari terminal:   selen resume <nomor>");
        Ok(out)
    }

    /// Tangani `/resume [nomor | rm <nomor> | info | rename <judul>]`:
    /// - Tanpa argumen -> daftar 10 sesi terakhir bernomor (`#1..#10`)
    /// - `info` -> detail sesi aktif
    /// - `rm <nomor>` -> hapus sesi terpilih dari SQLite
    /// - `rename <judul>` -> ubah judul sesi aktif di SQLite
    /// - `<nomor>` atau `<prefix_id>` -> lanjutkan & muat ulang sesi terpilih
    pub(crate) fn handle_resume(
        &mut self,
        arg: Option<&str>,
    ) -> Result<ResumeOutcome> {
        let Some(raw) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(ResumeOutcome::Menu(self.sessions_menu()?));
        };
        if raw.eq_ignore_ascii_case("info") || raw.eq_ignore_ascii_case("status") {
            return Ok(ResumeOutcome::Menu(self.session_info_menu()));
        }
        if raw.eq_ignore_ascii_case("list") || raw.eq_ignore_ascii_case("ls") {
            return Ok(ResumeOutcome::Menu(self.sessions_menu()?));
        }
        if let Some(rest) = raw
            .strip_prefix("rm ")
            .or_else(|| raw.strip_prefix("del "))
            .or_else(|| raw.strip_prefix("delete "))
        {
            let cwd_str = self.ctx.cwd.to_string_lossy().to_string();
            let target = self.store.resolve_session(&cwd_str, rest.trim(), 10)?;
            if target.id == self.session_id {
                anyhow::bail!(
                    "tidak bisa menghapus sesi yang sedang aktif; mulai sesi baru (`/new`) atau pindah dulu."
                );
            }
            self.store.delete_session(&target.id)?;
            return Ok(ResumeOutcome::Menu(format!(
                "# Sesi Dihapus\n> {} | \"{}\" | seluruh pesan & histori usage sesi dihapus",
                target.id.chars().take(10).collect::<String>(),
                target.title
            )));
        }
        if let Some(rest) = raw
            .strip_prefix("rename ")
            .or_else(|| raw.strip_prefix("title "))
        {
            let new_title = selencode::store::summarize_prompt_title(rest);
            self.store
                .update_session_title(&self.session_id, &new_title)?;
            return Ok(ResumeOutcome::Menu(format!(
                "# Judul Sesi Diperbarui\n* sesi | {} | \"{new_title}\"",
                self.session_id.chars().take(10).collect::<String>()
            )));
        }
        let resumed = self.resume_by_target(raw)?;
        Ok(ResumeOutcome::Switched(resumed))
    }

    pub(crate) fn session_info_menu(&self) -> String {
        let active_ts = selencode::tools::toolsets::select_initial_toolsets(
            "",
            &self.ctx.cwd,
            !self.cfg.mcp_servers.is_empty(),
            selencode::agent::plan::TurnMode::Execute,
        );
        let active_specs = selencode::tools::toolsets::active_tool_specs(
            &self.registry,
            &active_ts,
            selencode::agent::plan::TurnMode::Execute,
        );
        let bd = selencode::agent::context::analyze_token_breakdown(
            &self.messages,
            &active_specs,
            self.registry.names().len(),
        );
        let est = bd.total_tokens();
        let lim = self.ctx.context_limit_tokens;
        let pct = calc_context_pct(est, lim);
        let metrics_line = match self.store.session_token_metrics(&self.session_id) {
            Ok(m) if m.calls > 0 => format!(
                "\n> metrik | {} call · avg sys ~{} / tools ~{} / chat ~{} / res ~{} | pruned {} · dedup {}",
                m.calls,
                m.avg_system_tokens,
                m.avg_tools_tokens,
                m.avg_history_tokens,
                m.avg_tool_results_tokens,
                m.total_pruned,
                m.total_deduped
            ),
            _ => String::new(),
        };
        format!(
            "# Info Sesi Aktif\n\
             * sesi | {} | {}\n\
             > provider | {} | model {} (kind {})\n\
             > konteks | {} / {} ({pct}%) | sys ~{} · tools ~{} ({}/{}) · chat ~{} · tool_res ~{}{metrics_line}\n\
             > folder | {} | {} pesan di memori",
            self.session_id.chars().take(12).collect::<String>(),
            self.usage_line(),
            self.provider_name,
            self.pcfg.model,
            self.pcfg.kind,
            format_context_compact(est),
            format_context_compact(lim),
            bd.system_tokens,
            bd.tools_tokens,
            bd.active_tools,
            bd.total_tools,
            bd.conversation_tokens,
            bd.tool_results_tokens,
            self.ctx.cwd.display(),
            self.messages.len(),
        )
    }

    pub(crate) fn print_sessions(&self) -> Result<()> {
        println!("{}", strip_menu_markup(&self.sessions_menu()?));
        Ok(())
    }
}
