//! Fase 2 — Security Sandbox v2: kebijakan izin terpusat, pembersihan env var
//! sensitif untuk subprocess, mode sandbox (`read-only`, `workspace-write`,
//! `unrestricted`), deteksi isolasi OS nyata (`bwrap` Linux vs status jujur
//! `Process-Hardened` di Windows/Termux), fail-closed pada mode `strict`,
//! serta audit log teredaksi (JSONL).
//!
//! Catatan keamanan penting: validasi string perintah BUKAN sandbox OS.
//! Modul ini memisahkan secara jujur dua tingkat isolasi:
//! - `OsIsolated (bwrap)` — isolasi mount/namespace nyata via bubblewrap di Linux
//! - `ProcessHardened` — pembersihan env var rahasia, canonical `cwd`, batas
//!   waktu/output/proses, `kill_on_drop`, & audit log (Windows / Termux non-root).

use crate::config::SandboxCfg;
use crate::security::{is_forbidden_command, redact_secrets};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

/// Mode izin sandbox aktif untuk satu sesi SelenCode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Hanya baca: menolak tulis file, rename simbol, commit, & eksekusi shell tulis.
    ReadOnly,
    /// Default aman: baca/tulis terbatas dalam canonical workspace + subprocess terkeraskan.
    #[default]
    WorkspaceWrite,
    /// Tanpa batasan mode tulis ekstra (perintah destruktif fatal & redaksi secret tetap aktif).
    Unrestricted,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "ro" => Some(Self::ReadOnly),
            "workspace-write" | "workspace" | "rw" | "default" => Some(Self::WorkspaceWrite),
            "unrestricted" | "off" | "full" => Some(Self::Unrestricted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Unrestricted => "unrestricted",
        }
    }
}

/// Kebijakan pemakaian backend isolasi OS (`bwrap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OsIsolationPolicy {
    /// Jangan bungkus dengan `bwrap`; pakai `ProcessHardened` (kompatibilitas maksimum).
    #[default]
    Off,
    /// Gunakan `bwrap` otomatis jika tersedia di Linux, selain itu `ProcessHardened` jujur.
    Auto,
    /// Fail-closed: WAJIB isolasi OS (`bwrap`); tolak menjalankan subprocess bila tidak tersedia!
    Strict,
}

impl OsIsolationPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            "strict" | "required" | "fail-closed" => Some(Self::Strict),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Strict => "strict",
        }
    }
}

/// Backend isolasi subprocess yang benar-benar aktif di mesin ini (jujur, tidak diklaim berlebihan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Isolasi mount namespace Linux nyata via `bwrap` (bubblewrap).
    BubblewrapOsIsolated(PathBuf),
    /// Pengerasan proses (scrub env sensitif + cwd + timeout + kill_on_drop + audit log),
    /// tanpa container/namespace OS penuh.
    ProcessHardened { platform_note: String },
}

impl SandboxBackend {
    pub fn label(&self) -> String {
        match self {
            Self::BubblewrapOsIsolated(p) => format!("OS-Isolated (bwrap: {})", p.display()),
            Self::ProcessHardened { platform_note } => {
                format!("Process-Hardened ({platform_note})")
            }
        }
    }

    pub fn is_os_isolated(&self) -> bool {
        matches!(self, Self::BubblewrapOsIsolated(_))
    }
}

/// Deteksi backend isolasi OS yang benar-benar tersedia di platform saat ini.
pub fn detect_os_backend() -> SandboxBackend {
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        if let Some(p) = crate::lsp::find_binary_on_path("bwrap") {
            return SandboxBackend::BubblewrapOsIsolated(p);
        }
        return SandboxBackend::ProcessHardened {
            platform_note: "Linux tanpa bwrap — pasang `bubblewrap` untuk isolasi mount OS".to_string(),
        };
    }
    #[cfg(target_os = "android")]
    {
        SandboxBackend::ProcessHardened {
            platform_note: "Termux Android non-root — isolasi namespace OS tidak tersedia".to_string(),
        }
    }
    #[cfg(windows)]
    {
        SandboxBackend::ProcessHardened {
            platform_note: "Windows — scrub env + sandbox path + kill_on_drop (tanpa container OS)".to_string(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
    {
        SandboxBackend::ProcessHardened {
            platform_note: format!("{} — pengerasan proses tanpa container OS", std::env::consts::OS),
        }
    }
}

/// Apakah nama environment variable termasuk sensitif (API key, token, secret, password,
/// kredensial cloud/DB) sehingga HARUS dibuang sebelum menjalankan subprocess agent?
pub fn is_sensitive_env_key(key: &str) -> bool {
    let up = key.trim().to_ascii_uppercase();
    if up.is_empty() {
        return false;
    }
    // Variabel sistem aman yang diperlukan shell/toolchain tetap dipertahankan.
    if matches!(
        up.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "USERNAME"
            | "USERPROFILE"
            | "SHELL"
            | "TERM"
            | "LANG"
            | "LC_ALL"
            | "PWD"
            | "TMP"
            | "TEMP"
            | "TMPDIR"
            | "SYSTEMROOT"
            | "COMSPEC"
            | "PATHEXT"
            | "WINDIR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "PREFIX"
            | "ANDROID_ROOT"
            | "ANDROID_DATA"
    ) {
        return false;
    }
    let sensitive_parts = [
        "API_KEY",
        "APIKEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTH_HEADER",
        "BEARER",
        "AWS_ACCESS",
        "AWS_SECRET",
        "AWS_SESSION",
        "OPENAI_",
        "ANTHROPIC_",
        "GEMINI_",
        "OPENROUTER_",
        "GROQ_",
        "DEEPSEEK_",
        "TELEGRAM_",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "NPM_TOKEN",
        "PYPI_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "DATABASE_URL",
        "REDIS_URL",
        "SELENCODE_ROUTER_KEY",
    ];
    sensitive_parts.iter().any(|p| up.contains(p))
        || (up.starts_with("SELENCODE_") && (up.ends_with("_KEY") || up.ends_with("_TOKEN")))
}

/// Kumpulkan daftar nama env var sensitif yang saat ini ada di proses induk.
pub fn sensitive_env_keys_present() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(k, _)| {
            let s = k.to_string_lossy().into_owned();
            is_sensitive_env_key(&s).then_some(s)
        })
        .collect()
}

/// Satu entri audit log persisten (JSONL) — seluruh teks sudah teredaksi!
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub mode: String,
    pub backend: String,
    pub action: String,
    pub target: String,
    pub allowed: bool,
    pub detail: String,
}

/// Path default audit log: env `SELENCODE_AUDIT_LOG` (dipakai tes) atau
/// `~/.local/share/selencode/audit.jsonl`.
pub fn default_audit_log_path() -> PathBuf {
    if let Ok(p) = std::env::var("SELENCODE_AUDIT_LOG") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("selencode").join("audit.jsonl")
}

/// State kebijakan sandbox terpusat per sesi (dibagikan via `Arc<SandboxController>` di `Ctx`).
#[derive(Debug)]
pub struct SandboxController {
    inner: Mutex<SandboxState>,
    audit_path: PathBuf,
}

#[derive(Debug, Clone)]
struct SandboxState {
    mode: SandboxMode,
    os_isolation: OsIsolationPolicy,
    scrub_env: bool,
    allow_network: bool,
    audit_enabled: bool,
    backend: SandboxBackend,
}

impl Default for SandboxController {
    fn default() -> Self {
        Self::from_config(&SandboxCfg::default(), default_audit_log_path())
    }
}

impl SandboxController {
    pub fn from_config(cfg: &SandboxCfg, audit_path: PathBuf) -> Self {
        let mode = SandboxMode::parse(&cfg.mode).unwrap_or(SandboxMode::WorkspaceWrite);
        let os_isolation = OsIsolationPolicy::parse(&cfg.os_isolate).unwrap_or(OsIsolationPolicy::Off);
        Self {
            inner: Mutex::new(SandboxState {
                mode,
                os_isolation,
                scrub_env: cfg.scrub_env,
                allow_network: cfg.allow_network,
                audit_enabled: cfg.audit_log,
                backend: detect_os_backend(),
            }),
            audit_path,
        }
    }

    /// Konstruktor khusus untuk tes dengan backend eksplisit (memungkinkan uji fail-closed deterministik).
    pub fn with_backend_for_test(
        cfg: &SandboxCfg,
        audit_path: PathBuf,
        backend: SandboxBackend,
    ) -> Self {
        let ctrl = Self::from_config(cfg, audit_path);
        if let Ok(mut st) = ctrl.inner.lock() {
            st.backend = backend;
        }
        ctrl
    }

    pub fn mode(&self) -> SandboxMode {
        self.inner.lock().map(|s| s.mode).unwrap_or(SandboxMode::WorkspaceWrite)
    }

    pub fn set_mode(&self, mode: SandboxMode) {
        if let Ok(mut st) = self.inner.lock() {
            st.mode = mode;
        }
    }

    pub fn os_isolation(&self) -> OsIsolationPolicy {
        self.inner
            .lock()
            .map(|s| s.os_isolation)
            .unwrap_or(OsIsolationPolicy::Off)
    }

    pub fn set_os_isolation(&self, pol: OsIsolationPolicy) {
        if let Ok(mut st) = self.inner.lock() {
            st.os_isolation = pol;
        }
    }

    pub fn backend(&self) -> SandboxBackend {
        self.inner
            .lock()
            .map(|s| s.backend.clone())
            .unwrap_or_else(|_| detect_os_backend())
    }

    pub fn scrub_env_enabled(&self) -> bool {
        self.inner.lock().map(|s| s.scrub_env).unwrap_or(true)
    }

    /// Apakah eksekusi subprocess saat ini benar-benar akan dibungkus `bwrap` (OS-isolated)?
    pub fn will_use_os_isolation(&self) -> bool {
        let Ok(st) = self.inner.lock() else {
            return false;
        };
        matches!(
            (&st.backend, st.os_isolation, st.mode),
            (
                SandboxBackend::BubblewrapOsIsolated(_),
                OsIsolationPolicy::Strict,
                SandboxMode::WorkspaceWrite | SandboxMode::Unrestricted
            ) | (
                SandboxBackend::BubblewrapOsIsolated(_),
                OsIsolationPolicy::Auto,
                SandboxMode::WorkspaceWrite
            )
        )
    }

    /// Apakah isolasi jaringan (`--unshare-net`) benar-benar ditegakkan oleh backend saat ini?
    pub fn network_isolation_enforced(&self) -> bool {
        let allow_network = self.inner.lock().map(|s| s.allow_network).unwrap_or(true);
        !allow_network && self.will_use_os_isolation()
    }

    /// Periksa apakah aksi tulis (`write_file`, `edit_file`, `lsp_rename`, `git_commit`)
    /// diizinkan oleh `SandboxMode` saat ini, sambil mencatat audit log.
    pub fn check_write_allowed(&self, action: &str, target: &str) -> Result<()> {
        let mode = self.mode();
        if mode == SandboxMode::ReadOnly {
            self.record_audit(action, target, false, "diblokir oleh mode sandbox read-only");
            return Err(anyhow!(
                "aksi '{action}' ke '{target}' ditolak: mode sandbox saat ini adalah `read-only`. Ubah dengan `/sandbox workspace-write` bila ingin mengizinkan tulis."
            ));
        }
        self.record_audit(action, target, true, "diizinkan dalam batas workspace");
        Ok(())
    }

    /// Bangun `tokio::process::Command` yang sudah melewati seluruh lapisan Sandbox v2:
    /// 1) tolak perintah destruktif fatal (`is_forbidden_command`),
    /// 2) tolak eksekusi shell tulis saat mode `read-only`,
    /// 3) fail-closed bila `os_isolation == Strict` dan tidak ada backend OS (`bwrap`),
    /// 4) scrub seluruh env var sensitif (`scrub_env`),
    /// 5) bungkus dalam `bwrap` di Linux bila aktif,
    /// 6) selalu set `current_dir(cwd)`, `stdin(null)`, `kill_on_drop(true)` + catat audit log.
    pub fn prepare_shell_command(
        &self,
        action: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<tokio::process::Command> {
        let cmd_clean = command.trim();
        if cmd_clean.is_empty() {
            return Err(anyhow!("perintah kosong"));
        }
        if is_forbidden_command(cmd_clean) {
            self.record_audit(action, cmd_clean, false, "pola destruktif terlarang");
            return Err(anyhow!(
                "perintah ditolak demi keamanan (pola terlarang, tidak bisa di-approve): {cmd_clean}"
            ));
        }

        let st = self.inner.lock().expect("sandbox state poisoned").clone();
        if st.mode == SandboxMode::ReadOnly {
            self.record_audit(action, cmd_clean, false, "eksekusi shell diblokir mode read-only");
            return Err(anyhow!(
                "eksekusi perintah '{cmd_clean}' ditolak: mode sandbox saat ini adalah `read-only`."
            ));
        }

        // Fail-closed: jika user meminta isolasi OS strict tetapi tidak tersedia di platform ini!
        if st.os_isolation == OsIsolationPolicy::Strict && !st.backend.is_os_isolated() {
            self.record_audit(
                action,
                cmd_clean,
                false,
                "fail-closed: isolasi OS strict aktif namun backend OS tidak tersedia",
            );
            return Err(anyhow!(
                "sandbox fail-closed: kebijakan `os_isolate = strict` aktif, tetapi isolasi OS tidak tersedia di platform ini ({}). Nonaktifkan strict dengan `/sandbox isolate auto` atau `/sandbox isolate off` jika Anda memahami risikonya.",
                st.backend.label()
            ));
        }

        // P0.1: `strict` WAJIB menegakkan isolasi OS (`bwrap`) untuk semua mode tulis
        // (termasuk `unrestricted`) — TIDAK BOLEH diam-diam jatuh ke host shell!
        let use_bwrap = match (&st.backend, st.os_isolation, st.mode) {
            (SandboxBackend::BubblewrapOsIsolated(bwrap_path), OsIsolationPolicy::Strict, _) => {
                Some(bwrap_path.clone())
            }
            (
                SandboxBackend::BubblewrapOsIsolated(bwrap_path),
                OsIsolationPolicy::Auto,
                SandboxMode::WorkspaceWrite,
            ) => Some(bwrap_path.clone()),
            _ => None,
        };
        if st.os_isolation == OsIsolationPolicy::Strict && use_bwrap.is_none() {
            self.record_audit(
                action,
                cmd_clean,
                false,
                "fail-closed: strict mode tidak boleh mengeksekusi tanpa bwrap",
            );
            return Err(anyhow!(
                "sandbox fail-closed: kebijakan `os_isolate = strict` melarang eksekusi tanpa isolasi OS."
            ));
        }

        let mut cmd = if let Some(bwrap_path) = use_bwrap {
            let cwd_can = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
            let mut c = tokio::process::Command::new(bwrap_path);
            c.args(["--ro-bind", "/", "/"])
                .args(["--dev", "/dev"])
                .args(["--proc", "/proc"])
                .args(["--tmpfs", "/tmp"])
                .arg("--bind")
                .arg(&cwd_can)
                .arg(&cwd_can)
                .arg("--chdir")
                .arg(&cwd_can)
                .arg("--die-with-parent")
                .arg("--new-session");
            if !st.allow_network {
                c.arg("--unshare-net");
            }
            c.args(["--", "sh", "-c", cmd_clean]);
            c
        } else {
            #[cfg(windows)]
            let mut c = {
                let mut inner = tokio::process::Command::new("cmd");
                inner.args(["/C", cmd_clean]);
                inner
            };
            #[cfg(not(windows))]
            let mut c = {
                let mut inner = tokio::process::Command::new("sh");
                inner.args(["-c", cmd_clean]);
                inner
            };
            c.current_dir(cwd);
            c
        };

        cmd.stdin(Stdio::null()).kill_on_drop(true);

        if st.scrub_env {
            for sensitive_key in sensitive_env_keys_present() {
                cmd.env_remove(sensitive_key);
            }
        }

        self.record_audit(
            action,
            cmd_clean,
            true,
            &format!(
                "siap dijalankan (scrub_env={}, isolate={})",
                st.scrub_env,
                st.os_isolation.as_str()
            ),
        );
        Ok(cmd)
    }

    /// Catat 1 peristiwa ke audit log JSONL (seluruh target & detail melewati `redact_secrets`).
    pub fn record_audit(&self, action: &str, target: &str, allowed: bool, detail: &str) {
        let st = match self.inner.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        if !st.audit_enabled {
            return;
        }
        let entry = AuditEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            mode: st.mode.as_str().to_string(),
            backend: st.backend.label(),
            action: action.to_string(),
            target: redact_secrets(target).chars().take(200).collect(),
            allowed,
            detail: redact_secrets(detail).chars().take(200).collect(),
        };
        if let Some(parent) = self.audit_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.audit_path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Baca `limit` entri audit log terakhir (terbaru dulu).
    pub fn recent_audit_entries(&self, limit: usize) -> Vec<AuditEntry> {
        let Ok(text) = std::fs::read_to_string(&self.audit_path) else {
            return Vec::new();
        };
        let mut parsed: Vec<AuditEntry> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
            .collect();
        parsed.reverse();
        parsed.truncate(limit);
        parsed
    }

    /// Format kartu status Moonlit untuk perintah `/sandbox`.
    pub fn format_status_card(&self) -> String {
        let st = self.inner.lock().expect("sandbox state poisoned").clone();
        let scrubbed_count = sensitive_env_keys_present().len();
        let scrub_label = if st.scrub_env {
            format!("aktif ({scrubbed_count} env sensitif disaring dari subprocess)")
        } else {
            "nonaktif".to_string()
        };
        let audit_label = if st.audit_enabled {
            format!("aktif ({})", self.audit_path.display())
        } else {
            "nonaktif".to_string()
        };
        let net_label = if st.allow_network {
            "diizinkan (allow_network=true)".to_string()
        } else if self.network_isolation_enforced() {
            "diblokir (enforced via bwrap --unshare-net)".to_string()
        } else {
            "not enforced (backend Process-Hardened / isolate=off tidak menegakkan blokir jaringan)"
                .to_string()
        };
        format!(
            "# Security Sandbox v2\n\
             * mode | {} | os_isolate: {}\n\
             > backend | {} |\n\
             > network | {} |\n\
             > env guard | {} |\n\
             > audit log | {} |\n\
             ---\n\
             ! Ganti mode:   /sandbox read-only  ·  /sandbox workspace-write  ·  /sandbox unrestricted\n\
             ! Isolasi OS:   /sandbox isolate off  ·  auto  ·  strict (fail-closed)\n\
             ! Lihat audit:  /sandbox audit [jumlah]",
            st.mode.as_str(),
            st.os_isolation.as_str(),
            st.backend.label(),
            net_label,
            scrub_label,
            audit_label
        )
    }
}
