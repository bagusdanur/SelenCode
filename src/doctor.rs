//! Diagnostik kesehatan instalasi & lingkungan SelenCode (`selen doctor` & `/doctor`).

use crate::config::{active_config_path, Config, ProviderCfg};
use crate::sandbox::SandboxController;
use crate::store::Store;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn ok_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Ok)
            .count()
    }

    pub fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Warn)
            .count()
    }

    pub fn fail_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count()
    }

    /// Render sebagai kartu menu SelenCode (otomatis cantik di TUI, Plain REPL, & Telegram HTML).
    pub fn format_menu_card(&self) -> String {
        let mut out = format!(
            "# SelenCode Doctor v{} ({} OK · {} Warn · {} Fail)\n",
            env!("CARGO_PKG_VERSION"),
            self.ok_count(),
            self.warn_count(),
            self.fail_count()
        );
        for c in &self.checks {
            let (mark, badge) = match c.status {
                CheckStatus::Ok => ("*", "OK"),
                CheckStatus::Warn => (">", "WARN"),
                CheckStatus::Fail => (">", "FAIL"),
            };
            out.push_str(&format!("{mark} {} | {badge} | {}\n", c.name, c.detail));
        }
        out.push_str("---\n");
        if self.fail_count() > 0 {
            out.push_str("! Ada komponen FAIL — jalankan /setup atau perbaiki konfigurasi terkait.");
        } else if self.warn_count() > 0 {
            out.push_str("! Inti SelenCode sehat; item WARN bersifat opsional sesuai kebutuhan proyek.");
        } else {
            out.push_str("! Semua pemeriksaan lingkungan SelenCode sehat & siap tempur.");
        }
        out
    }
}

fn binary_version(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .or_else(|| {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.trim().is_empty() {
                None
            } else {
                Some("tersedia")
            }
        })
        .unwrap_or("tersedia");
    Some(first.trim().chars().take(60).collect())
}

/// Jalankan seluruh pemeriksaan diagnostik lokal secara cepat & aman (tanpa bocorkan rahasia).
pub fn run_doctor(
    cfg: &Config,
    cwd: &Path,
    provider_name: &str,
    pcfg: &ProviderCfg,
    store: &Store,
    sandbox: &SandboxController,
) -> DoctorReport {
    let mut checks = Vec::new();

    // 1. Versi & Platform
    checks.push(DoctorCheck {
        name: "Runtime".into(),
        status: CheckStatus::Ok,
        detail: format!(
            "selen v{} ({}/{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    });

    // 2. Konfigurasi & Provider aktif
    let cfg_path = active_config_path();
    let prov_ok = cfg.resolve_provider(Some(provider_name)).is_ok()
        || cfg.resolve_provider(None).is_ok();
    checks.push(DoctorCheck {
        name: "Provider".into(),
        status: if prov_ok {
            CheckStatus::Ok
        } else {
            CheckStatus::Fail
        },
        detail: if prov_ok {
            format!(
                "{} / {} (config: {})",
                provider_name,
                pcfg.model,
                cfg_path.display()
            )
        } else {
            format!(
                "belum ada provider/API key siap (config: {})",
                cfg_path.display()
            )
        },
    });

    // 2b. Validasi kunci config (Perbaikan 3 v0.5.11: kunci tak dikenal & penempatan salah tidak boleh diam)
    let cfg_warnings = crate::config::collect_active_config_warnings();
    checks.push(DoctorCheck {
        name: "Config Keys".into(),
        status: if cfg_warnings.is_empty() {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: if cfg_warnings.is_empty() {
            "semua kunci config sesuai skema resmi".into()
        } else {
            let preview: Vec<String> = cfg_warnings
                .iter()
                .take(4)
                .map(|(_, w)| w.message.clone())
                .collect();
            let extra = if cfg_warnings.len() > 4 {
                format!(" (+{} lainnya)", cfg_warnings.len() - 4)
            } else {
                String::new()
            };
            format!("{} peringatan: {}{extra}", cfg_warnings.len(), preview.join("; "))
        },
    });

    // 3. SQLite Store & Memori Persisten (v0.5.9: satu sumber kebenaran di user_memory)
    let cwd_str = cwd.to_string_lossy().to_string();
    let sess_cnt = store.list_sessions(&cwd_str, 50).map(|v| v.len()).unwrap_or(0);
    let umem = crate::user_memory::load_frozen_snapshot(
        cfg.memory.max_memory_chars,
        cfg.memory.max_profile_chars,
    );
    let mem_cnt = umem.memory_entries.len() + umem.profile_entries.len();
    checks.push(DoctorCheck {
        name: "SQLite Store".into(),
        status: CheckStatus::Ok,
        detail: format!(
            "{} sesi folder ini · {} memori persisten ({})",
            sess_cnt,
            mem_cnt,
            crate::store::db_path().display()
        ),
    });

    // 4. Git & Worktree
    match binary_version("git", &["--version"]) {
        Some(ver) => {
            let is_repo = crate::worktree::is_git_repo(cwd);
            let wt_cnt = crate::worktree::list_worktrees(cwd)
                .map(|v| v.len())
                .unwrap_or(0);
            checks.push(DoctorCheck {
                name: "Git & Worktree".into(),
                status: CheckStatus::Ok,
                detail: if is_repo {
                    format!("{ver} · repo aktif ({wt_cnt} worktree)")
                } else {
                    format!("{ver} · folder saat ini bukan repo Git")
                },
            });
        }
        None => {
            checks.push(DoctorCheck {
                name: "Git & Worktree".into(),
                status: CheckStatus::Warn,
                detail: "git tidak ditemukan di PATH (worktree & git tools terbatas)".into(),
            });
        }
    }

    // 5. Security Sandbox v2
    checks.push(DoctorCheck {
        name: "Sandbox v2".into(),
        status: CheckStatus::Ok,
        detail: format!(
            "mode={} · isolasi={} ({})",
            sandbox.mode().as_str(),
            sandbox.os_isolation().as_str(),
            sandbox.backend().label()
        ),
    });

    // 6. Toolchain & Search helpers
    let mut found_tools = Vec::new();
    for (bin, arg) in [
        ("rg", "--version"),
        ("cargo", "--version"),
        ("node", "--version"),
        ("python3", "--version"),
    ] {
        if binary_version(bin, &[arg]).is_some() {
            found_tools.push(bin);
        }
    }
    checks.push(DoctorCheck {
        name: "Dev Tools".into(),
        status: if found_tools.is_empty() {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        detail: if found_tools.is_empty() {
            "tidak ada rg/cargo/node/python3 di PATH".into()
        } else {
            format!("tersedia: {}", found_tools.join(", "))
        },
    });

    // 7. LSP Servers
    let mut lsps = Vec::new();
    for bin in [
        "rust-analyzer",
        "pyright-langserver",
        "typescript-language-server",
        "gopls",
    ] {
        if binary_version(bin, &["--version"]).is_some() {
            lsps.push(bin);
        }
    }
    checks.push(DoctorCheck {
        name: "LSP Servers".into(),
        status: if lsps.is_empty() {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        detail: if lsps.is_empty() {
            "belum ada LSP binary terpasang (fallback Tree-sitter tetap aktif)".into()
        } else {
            format!("aktif: {}", lsps.join(", "))
        },
    });

    // 8. MCP & Telegram
    let tg_status = if cfg.telegram.is_token_ready() {
        format!(
            "token siap ({} whitelist)",
            cfg.telegram.allowed_users.len()
        )
    } else {
        "belum diatur".into()
    };
    checks.push(DoctorCheck {
        name: "MCP & Telegram".into(),
        status: CheckStatus::Ok,
        detail: format!(
            "{} server MCP · Telegram: {tg_status}",
            cfg.mcp_servers.len()
        ),
    });

    DoctorReport { checks }
}
