use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Apakah nama file termasuk rahasia (tidak boleh dibaca/dikirim ke provider)?
pub fn is_secret_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".key")
        || name.ends_with(".pem")
        || name == "id_rsa"
        || name.starts_with("id_rsa.")
        || name.starts_with("id_rsa_")
        || name == "id_ed25519"
        || name.starts_with("id_ed25519.")
}

/// Normalisasi leksikal (tanpa menyentuh FS): buang `.`, proses `..` dengan pop.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Selesaikan path relatif terhadap cwd project dan PASTIKAN tetap di dalamnya.
/// Menolak `..` keluar, path absolut di luar cwd, dan target symlink yang keluar.
pub fn resolve_in_sandbox(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let cwd_abs = normalize(&cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()));
    let joined = if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&cwd_abs.join(path))
    };
    if !joined.starts_with(&cwd_abs) {
        return Err(anyhow!(
            "path '{}' ditolak: di luar folder project ({})",
            path.display(),
            cwd_abs.display()
        ));
    }
    // Bila target (atau induk terdekatnya) ada di FS, cek symlink tidak kabur keluar.
    let mut probe = joined.clone();
    loop {
        if probe.exists() {
            if let Ok(real) = probe.canonicalize() {
                if !real.starts_with(&cwd_abs) {
                    return Err(anyhow!(
                        "path '{}' ditolak: symlink mengarah keluar folder project",
                        path.display()
                    ));
                }
            }
            break;
        }
        if !probe.pop() {
            break;
        }
    }
    Ok(joined)
}

/// Mode persetujuan tool pada sesi aktif.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    /// Tanya setiap aksi tulis/shell/web/mcp (kecuali kategori yang sudah diizinkan sesi ini).
    #[default]
    Ask,
    /// Otomatis izinkan `edit_file` & `write_file`; tetap tanya untuk shell/git_commit/web/mcp.
    AutoEdits,
    /// Otomatis izinkan semua aksi (perintah destruktif & file rahasia `.env` tetap diblokir keras).
    AutoAll,
}

impl ApprovalMode {
    pub fn from_config(approval: &str, yes_flag: bool) -> Self {
        if yes_flag || approval.eq_ignore_ascii_case("auto") {
            Self::AutoAll
        } else if approval.eq_ignore_ascii_case("edits") {
            Self::AutoEdits
        } else {
            Self::Ask
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask (tanya per aksi)",
            Self::AutoEdits => "edits (otomatis edit/tulis file, tanya shell/git/web)",
            Self::AutoAll => "auto (otomatis semua aksi aman)",
        }
    }
}

/// Kebijakan persetujuan bersama antara `Approver`, UI TUI (tombol `[a]`), dan `/approve`.
#[derive(Debug, Default)]
pub struct ApprovalPolicy {
    pub mode: ApprovalMode,
    /// Kategori tool yang diizinkan user untuk seluruh sisa sesi via `[a]`.
    pub session_allowed: HashSet<String>,
}

impl ApprovalPolicy {
    pub fn new(mode: ApprovalMode) -> Self {
        Self {
            mode,
            session_allowed: HashSet::new(),
        }
    }

    pub fn is_auto_allowed(&self, category: &str) -> bool {
        let by_mode = match self.mode {
            ApprovalMode::AutoAll => true,
            ApprovalMode::AutoEdits => matches!(category, "edit_file" | "write_file"),
            ApprovalMode::Ask => false,
        };
        by_mode || self.session_allowed.contains(category)
    }

    pub fn allow_category(&mut self, category: impl Into<String>) {
        self.session_allowed.insert(category.into());
    }
}

/// Ekstrak kategori ringkas dari judul approval (mis. `"edit_file src/a.rs"` -> `"edit_file"`,
/// `"run_shell: cargo build"` -> `"run_shell"`, `"mcp: fs/read"` -> `"mcp:fs"`).
pub fn tool_category_from_title(title: &str) -> String {
    let t = title.trim();
    if let Some(rest) = t.strip_prefix("mcp:") {
        let server = rest.trim().split('/').next().unwrap_or("mcp").trim();
        return format!("mcp:{server}");
    }
    t.split(|c: char| c.is_whitespace() || c == ':')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("tool")
        .to_string()
}

/// Permintaan persetujuan yang dikirim ke UI (Fase 4, mode TUI):
/// UI menampilkan popup lalu menjawab lewat `reply` (true = setuju).
#[derive(Debug)]
pub struct ApprovalRequest {
    pub title: String,
    pub detail: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Penanya persetujuan dengan dukungan mode (`ask` / `edits` / `auto`) serta
/// izin per kategori selama sesi (`[a]` — agar tidak terus-terusan minta izin).
#[derive(Debug, Clone)]
pub enum Approver {
    Stdin {
        policy: Arc<Mutex<ApprovalPolicy>>,
    },
    Channel {
        policy: Arc<Mutex<ApprovalPolicy>>,
        tx: tokio::sync::mpsc::Sender<ApprovalRequest>,
    },
}

impl Approver {
    pub fn new(auto_yes: bool) -> Self {
        let mode = if auto_yes {
            ApprovalMode::AutoAll
        } else {
            ApprovalMode::Ask
        };
        Approver::Stdin {
            policy: Arc::new(Mutex::new(ApprovalPolicy::new(mode))),
        }
    }

    /// Varian Stdin dengan kebijakan bersama (dipakai mode polos + `/approve`).
    pub fn stdin_with_policy(policy: Arc<Mutex<ApprovalPolicy>>) -> Self {
        Approver::Stdin { policy }
    }

    /// Varian TUI: approval diminta lewat channel ke UI.
    pub fn channel(auto_yes: bool, tx: tokio::sync::mpsc::Sender<ApprovalRequest>) -> Self {
        let mode = if auto_yes {
            ApprovalMode::AutoAll
        } else {
            ApprovalMode::Ask
        };
        Approver::Channel {
            policy: Arc::new(Mutex::new(ApprovalPolicy::new(mode))),
            tx,
        }
    }

    /// Varian TUI dengan kebijakan bersama (dipakai tombol `[a]` & `/approve`).
    pub fn channel_with_policy(
        policy: Arc<Mutex<ApprovalPolicy>>,
        tx: tokio::sync::mpsc::Sender<ApprovalRequest>,
    ) -> Self {
        Approver::Channel { policy, tx }
    }

    pub fn policy(&self) -> Arc<Mutex<ApprovalPolicy>> {
        match self {
            Approver::Stdin { policy } | Approver::Channel { policy, .. } => policy.clone(),
        }
    }

    pub fn auto_yes(&self) -> bool {
        let policy = self.policy();
        let yes = policy
            .lock()
            .map(|p| p.mode == ApprovalMode::AutoAll)
            .unwrap_or(false);
        yes
    }

    /// Minta persetujuan. true = disetujui.
    pub async fn ask(&self, title: &str, detail: &str) -> bool {
        let category = tool_category_from_title(title);
        let allowed = {
            let policy = self.policy();
            let ok = policy
                .lock()
                .map(|p| p.is_auto_allowed(&category))
                .unwrap_or(false);
            ok
        };
        if allowed {
            if matches!(self, Approver::Stdin { .. }) {
                println!("[auto-approve] {title}");
            }
            return true;
        }
        match self {
            Approver::Stdin { policy } => {
                println!("\n--- PERLU PERSETUJUAN: {title} ---");
                if !detail.is_empty() {
                    println!("{detail}");
                }
                print!("Jalankan? [y] setuju / [a] selalu izinkan '{category}' sesi ini / [N] tolak: ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    return false;
                }
                match line.trim().to_lowercase().as_str() {
                    "y" | "yes" => true,
                    "a" | "always" => {
                        if let Ok(mut p) = policy.lock() {
                            p.allow_category(category);
                        }
                        true
                    }
                    _ => false,
                }
            }
            Approver::Channel { tx, .. } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let req = ApprovalRequest {
                    title: title.to_string(),
                    detail: detail.to_string(),
                    reply: reply_tx,
                };
                if tx.send(req).await.is_err() {
                    return false; // UI sudah mati — tolak demi keamanan
                }
                reply_rx.await.unwrap_or(false)
            }
        }
    }
}

// ---------- Fase 2: perintah terlarang + redaksi rahasia ----------

/// Perintah yang SELALU ditolak walau user menyetujui (Blueprint Bagian 6):
/// pola destruktif tingkat sistem. Pengecekan token-based agar tahan diselipkan
/// di tengah rantai perintah (`;`, `&&`, `|`).
pub fn is_forbidden_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    // Fork bomb: :(){ :|:& };:
    let squashed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if squashed.contains(":(){") {
        return true;
    }
    // Tulis langsung ke disk mentah.
    if lower.contains("/dev/sd") {
        return true;
    }
    let tokens: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
        .filter(|t| !t.is_empty())
        .collect();
    let has_word = |w: &str| {
        tokens
            .iter()
            .any(|t| *t == w || t.ends_with(&format!("/{w}")))
    };
    if tokens.iter().any(|t| t.starts_with("mkfs")) {
        return true;
    }
    if has_word("shutdown") || has_word("reboot") || has_word("poweroff") || has_word("halt") {
        return true;
    }
    // dd if=... (baca/timpa perangkat mentah)
    if has_word("dd") && lower.contains("if=") {
        return true;
    }
    // rm rekursif+paksa ke / atau ~
    if has_word("rm") {
        let combined_rf = tokens
            .iter()
            .any(|t| t.starts_with('-') && !t.starts_with("--") && t.contains('r') && t.contains('f'));
        let separate_rf = tokens.contains(&"-r") && tokens.contains(&"-f");
        let danger_target = tokens.iter().any(|t| {
            matches!(
                *t,
                "/" | "/*" | "~" | "~/" | "~/*" | "$home" | "$home/" | "$home/*"
            )
        });
        if (combined_rf || separate_rf) && danger_target {
            return true;
        }
    }
    false
}

fn secret_regexes() -> &'static Vec<(regex::Regex, &'static str)> {
    use std::sync::OnceLock;
    static RES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        let pats: Vec<(&str, &'static str)> = vec![
            // Blok private key (multiline)
            (
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                "[REDACTED]",
            ),
            // Header Authorization: Bearer ...
            (r"(?i)(authorization:\s*bearer\s+)\S+", "${1}[REDACTED]"),
            // Bearer token lepas
            (r"(?i)(bearer\s+)[A-Za-z0-9._\-]{16,}", "${1}[REDACTED]"),
            // Token bergaya OpenAI / GitHub / AWS
            (r"sk-[A-Za-z0-9_\-]{8,}", "[REDACTED]"),
            (r"ghp_[A-Za-z0-9]{16,}", "[REDACTED]"),
            (r"github_pat_[A-Za-z0-9_]{16,}", "[REDACTED]"),
            (r"AKIA[0-9A-Z]{16}", "[REDACTED]"),
            // NAME=VALUE dengan nama mengandung KEY/TOKEN/SECRET/PASSWORD (nilai >= 8)
            (
                r#"(?i)([A-Za-z0-9_.\-]*(?:KEY|TOKEN|SECRET|PASSWORD)[A-Za-z0-9_.\-]*\s*=\s*["']?)([^\s"']{8,})"#,
                "${1}[REDACTED]",
            ),
            // Gaya JSON: "api_key": "value"
            (
                r#"(?i)([A-Za-z0-9_.\-]*(?:KEY|TOKEN|SECRET|PASSWORD)[A-Za-z0-9_.\-]*["']?\s*:\s*["'])([^"']{8,})"#,
                "${1}[REDACTED]",
            ),
        ];
        pats.into_iter()
            .map(|(p, r)| (regex::Regex::new(p).expect("pola redaksi valid"), r))
            .collect()
    })
}

/// Ganti pola yang tampak seperti rahasia dengan `[REDACTED]` sebelum teks
/// dikirim ke provider. Konservatif: lebih baik kelebihan redaksi daripada bocor.
pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for (re, repl) in secret_regexes() {
        out = re.replace_all(&out, *repl).into_owned();
    }
    out
}
