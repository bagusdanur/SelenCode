use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Buang prefiks verbatim Windows (`\\?\C:\...` -> `C:\...`, `\\?\UNC\srv\sh` -> `\\srv\sh`)
/// agar `strip_prefix`, `starts_with`, `cmd.exe`, dan SQLite `cwd` selalu konsisten lintas OS.
pub fn strip_windows_verbatim(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// Normalisasi path sandbox (kanonisasi bila ada + buang prefiks verbatim Windows + bersihkan `.`/`..`).
pub fn normalize_sandbox_path(p: &Path) -> PathBuf {
    let base = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    normalize(&strip_windows_verbatim(&base))
}

/// Apakah nama file termasuk rahasia (tidak boleh dibaca/dikirim ke provider)?
/// Tahan terhadap perbedaan huruf besar/kecil (Windows/macOS), NTFS Alternate Data Stream (`:`),
/// dan melindungi file config/database/audit milik SelenCode sendiri.
pub fn is_secret_file(path: &Path) -> bool {
    let Some(raw_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Tolak NTFS Alternate Data Streams (mis. `.env::$DATA`, `server.key:stream`)
    let base_name = raw_name.split(':').next().unwrap_or(raw_name);
    let trimmed = base_name.trim_end_matches(['.', ' ']);
    let name = trimmed.to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    // Jika file memiliki ADS (`:` selain drive letter — di sini sudah file_name), blokir bila dasarnya rahasia
    // atau mengandung `::$data`.
    if raw_name.contains(':') {
        let lower_raw = raw_name.to_ascii_lowercase();
        if lower_raw.contains("::$data") {
            return true;
        }
    }
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".key")
        || name.ends_with(".pem")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name == "id_rsa"
        || name.starts_with("id_rsa.")
        || name.starts_with("id_rsa_")
        || name == "id_ed25519"
        || name.starts_with("id_ed25519.")
        || name == ".netrc"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == "selencode.toml"
        || name == "selen.db"
        || name == "selen.db-wal"
        || name == "selen.db-shm"
        || name == "selencode.db"
        || name == "selencode.db-wal"
        || name == "selencode.db-shm"
        || name == "audit.jsonl"
        || name == "audit.log"
        || (name == "config.toml"
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|p| p.eq_ignore_ascii_case("selencode"))
                .unwrap_or(false))
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
/// Menolak `..` keluar, path absolut di luar cwd, dangling symlink keluar, dan
/// mengembalikan path kanonis agar symlink ke `.env` selalu tertangkap `is_secret_file`.
pub fn resolve_in_sandbox(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let cwd_abs = normalize_sandbox_path(cwd);
    let clean_req = strip_windows_verbatim(path);
    let joined = if clean_req.is_absolute() {
        normalize(&clean_req)
    } else {
        normalize(&cwd_abs.join(&clean_req))
    };
    if !joined.starts_with(&cwd_abs) {
        return Err(anyhow!(
            "path '{}' ditolak: di luar folder project ({})",
            path.display(),
            cwd_abs.display()
        ));
    }
    // Cek semua komponen yang ada di FS (termasuk dangling symlink via `symlink_metadata`)
    // agar tidak bisa menulis lewat symlink putus keluar workspace atau membaca symlink -> `.env`.
    let mut probe = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&probe) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    // Symlink yang ada (termasuk dangling!): wajib terkanonisasi DAN tetap dalam cwd_abs.
                    let real = match probe.canonicalize() {
                        Ok(r) => normalize(&strip_windows_verbatim(&r)),
                        Err(_) => {
                            // Dangling symlink -> cek target read_link secara eksplisit (fail-closed!)
                            let target = std::fs::read_link(&probe).map_err(|_| {
                                anyhow!(
                                    "path '{}' ditolak: symlink tidak dapat diverifikasi di dalam sandbox",
                                    path.display()
                                )
                            })?;
                            let resolved_link = if target.is_absolute() {
                                normalize(&strip_windows_verbatim(&target))
                            } else {
                                let parent = probe.parent().unwrap_or(&cwd_abs);
                                let parent_real = parent
                                    .canonicalize()
                                    .map(|r| normalize(&strip_windows_verbatim(&r)))
                                    .unwrap_or_else(|_| parent.to_path_buf());
                                normalize(&parent_real.join(&target))
                            };
                            if !resolved_link.starts_with(&cwd_abs) {
                                return Err(anyhow!(
                                    "path '{}' ditolak: symlink mengarah keluar folder project",
                                    path.display()
                                ));
                            }
                            resolved_link
                        }
                    };
                    if !real.starts_with(&cwd_abs) {
                        return Err(anyhow!(
                            "path '{}' ditolak: symlink mengarah keluar folder project",
                            path.display()
                        ));
                    }
                    let mut final_path = real;
                    for seg in tail.into_iter().rev() {
                        final_path.push(seg);
                    }
                    let final_norm = normalize(&final_path);
                    if !final_norm.starts_with(&cwd_abs) {
                        return Err(anyhow!(
                            "path '{}' ditolak: symlink mengarah keluar folder project",
                            path.display()
                        ));
                    }
                    return Ok(final_norm);
                } else {
                    let real = probe.canonicalize().map_err(|e| {
                        anyhow!(
                            "path '{}' ditolak: gagal memverifikasi path kanonis ({e})",
                            path.display()
                        )
                    })?;
                    let real_norm = normalize(&strip_windows_verbatim(&real));
                    if !real_norm.starts_with(&cwd_abs) {
                        return Err(anyhow!(
                            "path '{}' ditolak: symlink mengarah keluar folder project",
                            path.display()
                        ));
                    }
                    let mut final_path = real_norm;
                    for seg in tail.into_iter().rev() {
                        final_path.push(seg);
                    }
                    let final_norm = normalize(&final_path);
                    if !final_norm.starts_with(&cwd_abs) {
                        return Err(anyhow!(
                            "path '{}' ditolak: di luar folder project ({})",
                            path.display(),
                            cwd_abs.display()
                        ));
                    }
                    return Ok(final_norm);
                }
            }
            Err(_) => {
                if let Some(name) = probe.file_name() {
                    tail.push(name.to_os_string());
                }
                if !probe.pop() {
                    break;
                }
            }
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
/// `"run_shell: cargo build"` -> `"run_shell"`, `"mcp: fs/read"` -> `"mcp:fs/read"`).
pub fn tool_category_from_title(title: &str) -> String {
    let t = title.trim();
    if let Some(rest) = t.strip_prefix("mcp:") {
        let target = rest
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("mcp")
            .trim();
        return format!("mcp:{target}");
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

    /// Varian yang selalu menolak semua permintaan persetujuan tanpa blokir (dipakai cron job, v0.5.6).
    pub fn deny_all() -> Self {
        let (tx, _rx_dropped) = tokio::sync::mpsc::channel::<ApprovalRequest>(1);
        Approver::Channel {
            policy: Arc::new(Mutex::new(ApprovalPolicy::new(ApprovalMode::Ask))),
            tx,
        }
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
/// pola destruktif tingkat sistem (Unix & Windows). Pengecekan token-based dengan normalisasi
/// tanda kutip/escape agar tahan diselipkan di tengah rantai perintah (`;`, `&&`, `|`).
pub fn is_forbidden_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    // Fork bomb: :(){ :|:& };: atau variasi nama fungsi lain f(){ f|f& };f
    let squashed: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if squashed.contains(":(){") || (squashed.contains("(){") && squashed.contains("|") && squashed.contains("&};")) {
        return true;
    }
    // Tulis langsung ke disk mentah (Unix & Windows physical drive).
    if lower.contains("/dev/sd")
        || lower.contains("/dev/nvme")
        || lower.contains("/dev/vd")
        || lower.contains("/dev/mmcblk")
        || lower.contains(r"\\.\physicaldrive")
    {
        return true;
    }
    let tokens: Vec<String> = lower
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '\n' | '\r')
        })
        .map(|t| {
            t.trim_matches(|c: char| matches!(c, '\'' | '"' | '\\'))
                .replace('\\', "")
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let has_word = |w: &str| {
        tokens.iter().any(|t| {
            t == w
                || t == &format!("{w}.exe")
                || t.ends_with(&format!("/{w}"))
                || t.ends_with(&format!("/{w}.exe"))
        })
    };
    if tokens.iter().any(|t| t.starts_with("mkfs")) {
        return true;
    }
    if has_word("shutdown")
        || has_word("reboot")
        || has_word("poweroff")
        || has_word("halt")
        || has_word("diskpart")
    {
        return true;
    }
    // dd if=... (baca/timpa perangkat mentah)
    if has_word("dd") && lower.contains("if=") {
        return true;
    }
    // rm rekursif+paksa ke /, /*, /., //, ~, atau $HOME
    if has_word("rm") {
        let combined_rf = tokens.iter().any(|t| {
            t.starts_with('-') && !t.starts_with("--") && t.contains('r') && t.contains('f')
        });
        let has_r = tokens.iter().any(|t| t == "-r" || t == "--recursive");
        let has_f = tokens.iter().any(|t| t == "-f" || t == "--force");
        let danger_target = tokens.iter().any(|t| {
            matches!(
                t.as_str(),
                "/" | "/*"
                    | "/."
                    | "//"
                    | "~"
                    | "~/"
                    | "~/*"
                    | "$home"
                    | "$home/"
                    | "$home/*"
                    | "${home}"
                    | "${home}/"
                    | "${home}/*"
            )
        });
        if (combined_rf || (has_r && has_f)) && danger_target {
            return true;
        }
    }
    // Perintah destruktif Windows (`rd /s /q C:\`, `rmdir /s /q`, `del /f /s /q`, `format C:`, PowerShell `Remove-Item -Recurse -Force`)
    if has_word("format")
        && tokens
            .iter()
            .any(|t| t.len() == 2 && t.ends_with(':') && t.as_bytes()[0].is_ascii_alphabetic())
    {
        return true;
    }
    let has_win_root = tokens.iter().any(|t| {
        let s = t.as_str();
        matches!(s, "c:" | "c:/" | "c:/*" | "d:" | "d:/" | "/" | "/*")
            || (s.len() <= 4 && s.as_bytes().get(1) == Some(&b':'))
    });
    if (has_word("rd") || has_word("rmdir"))
        && tokens.iter().any(|t| t == "/s")
        && tokens.iter().any(|t| t == "/q")
        && has_win_root
    {
        return true;
    }
    if has_word("del")
        && tokens.iter().any(|t| t == "/s")
        && tokens.iter().any(|t| t == "/f" || t == "/q")
        && has_win_root
    {
        return true;
    }
    if (has_word("remove-item") || has_word("ri"))
        && tokens.iter().any(|t| t == "-recurse" || t == "-r")
        && tokens.iter().any(|t| t == "-force" || t == "-fo")
        && (has_win_root || tokens.iter().any(|t| matches!(t.as_str(), "~" | "$home")))
    {
        return true;
    }
    false
}

fn secret_regexes() -> &'static Vec<(regex::Regex, &'static str)> {
    use std::sync::OnceLock;
    static RES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        let pats: Vec<(&str, &'static str)> = vec![
            // Blok private key (multiline — termasuk yang terpotong tanpa baris END)
            (
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)",
                "[REDACTED]",
            ),
            // Header Authorization: Bearer / Basic ...
            (r"(?i)(authorization:\s*(?:bearer|basic)\s+)\S+", "${1}[REDACTED]"),
            // Bearer token lepas
            (r"(?i)(bearer\s+)[A-Za-z0-9._\-]{16,}", "${1}[REDACTED]"),
            // Kredensial dalam URL (`scheme://user:pass@host`)
            (r"(?i)([a-z][a-z0-9+\-.]*://[^/\s:@]+:)([^/\s@]+)(@)", "${1}[REDACTED]${3}"),
            // Token bergaya OpenAI / GitHub / AWS / Google Gemini / Slack / GitLab / Square / Telegram
            (r"sk-[A-Za-z0-9_\-]{8,}", "[REDACTED]"),
            (r"gh[pousr]_[A-Za-z0-9]{16,}", "[REDACTED]"),
            (r"github_pat_[A-Za-z0-9_]{16,}", "[REDACTED]"),
            (r"(?:AKIA|ASIA)[0-9A-Z]{16}", "[REDACTED]"),
            (r"AIzaSy[A-Za-z0-9_\-]{20,}", "[REDACTED]"),
            (r"xox[baprs]-[A-Za-z0-9\-]{10,}", "[REDACTED]"),
            (r"glpat-[A-Za-z0-9_\-]{16,}", "[REDACTED]"),
            (r"sq0csp-[A-Za-z0-9_\-]{16,}", "[REDACTED]"),
            (r"\b\d{8,10}:[A-Za-z0-9_\-]{30,}\b", "[REDACTED]"),
            // JWT 3-bagian (eyJ...)
            (r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b", "[REDACTED]"),
            // NAME=VALUE dengan nama mengandung KEY/TOKEN/SECRET/PASSWORD/PASSWD (nilai >= 8)
            (
                r#"(?i)([A-Za-z0-9_.\-]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD)[A-Za-z0-9_.\-]*\s*=\s*["']?)([^\s"']{8,})"#,
                "${1}[REDACTED]",
            ),
            // Gaya JSON/YAML: "api_key": "value" atau api_key: value
            (
                r#"(?i)([A-Za-z0-9_.\-]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD)[A-Za-z0-9_.\-]*["']?\s*:\s*["'])([^"']{8,})"#,
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

/// Deteksi apakah teks mengandung rahasia/token/password/private key (dipakai
/// guard Project Memory agar kredensial TIDAK PERNAH tersimpan di memori proyek).
pub fn contains_secret(text: &str) -> bool {
    if redact_secrets(text) != text {
        return true;
    }
    use std::sync::OnceLock;
    static EXTRA: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let extra = EXTRA.get_or_init(|| {
        [
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----",
            r"xox[baprs]-[A-Za-z0-9\-]{10,}",
            r"AIzaSy[A-Za-z0-9_\-]{20,}",
            r"\b\d{8,10}:[A-Za-z0-9_\-]{30,}\b", // Telegram bot token
            r"glpat-[A-Za-z0-9_\-]{16,}",
            r"sq0csp-[A-Za-z0-9_\-]{16,}",
            r"(?i)\b(?:password|passwd|secret|api_key|apikey|access_token|private_key)\b\s*(?:is|adalah|=|:)\s*\S{6,}",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("pola deteksi secret valid"))
        .collect()
    });
    extra.iter().any(|re| re.is_match(text))
}
