//! Self-Correction Engine & Loop/Stagnation Detector (Tahap 1).
//!
//! Menjaga keandalan eksekusi coding agent:
//! 1. `LoopGuard`: mendeteksi pemanggilan tool identik berulang (`>= 3x`) atau
//!    kegagalan berulang tanpa kemajuan (`>= 4` error beruntun), lalu memberi
//!    peringatan agar agent mengganti strategi atau berhenti jujur.
//! 2. `SelfCorrectState`: melacak file yang diubah dalam giliran aktif, apakah
//!    verifikasi sudah dijalankan sesudah edit terakhir, dan memicu siklus
//!    verifikasi & perbaikan terbatas (`MAX_REPAIR_ATTEMPTS = 3`) sebelum
//!    agent mengklaim selesai.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Batas pengulangan panggilan tool dengan argumen persis sama sebelum peringatan loop.
pub const MAX_IDENTICAL_CALLS: usize = 3;
/// Batas error tool beruntun tanpa kemajuan sebelum peringatan stagnasi.
pub const MAX_CONSECUTIVE_ERRORS: usize = 4;
/// Batas siklus perbaikan/verifikasi otomatis default di struct (di runtime diatur `[guard]`, default 1).
pub const MAX_REPAIR_ATTEMPTS: usize = 3;

/// Kanonisasi nilai JSON (urutkan kunci objek secara alfabetis secara rekursif) agar
/// `{"a":1,"b":2}` dan `{"b":2,"a":1}` menghasilkan fingerprint identik (Tahap 2).
pub fn canonicalize_json(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted: BTreeMap<&String, Value> = BTreeMap::new();
            for (k, val) in map {
                sorted.insert(k, canonicalize_json(val));
            }
            let mut out = Map::new();
            for (k, val) in sorted {
                out.insert(k.clone(), val);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

/// Fingerprint deterministik FNV-1a 64-bit atas nama tool + argumen JSON kanonis.
pub fn canonical_tool_fingerprint(name: &str, args: &Value) -> u64 {
    let canon = canonicalize_json(args).to_string();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes().iter().chain(b"::".iter()).chain(canon.as_bytes()) {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn file_signature(cwd: &Path, rel: &str) -> Option<(u128, u64)> {
    let p = crate::security::resolve_in_sandbox(cwd, Path::new(rel)).ok()?;
    let meta = std::fs::metadata(&p).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

#[derive(Debug, Clone)]
struct DedupRecord {
    tool_name: String,
    file_rel: Option<String>,
    file_sig: Option<(u128, u64)>,
    preview: String,
}

/// Cache deduplikasi panggilan tool identik yang SUDAH SUKSES dalam sesi/turn aktif (Tahap 2):
/// - `read_file`: cek mtime/ukuran file — kalau belum berubah, kembalikan stub hemat token; kalau berubah, baca ulang!
/// - `run_shell` & `verify_project`: dedup perintah identik yang baru saja sukses sejak edit file terakhir!
#[derive(Debug, Default)]
pub struct ToolDedupCache {
    records: HashMap<u64, DedupRecord>,
    pub dedup_hits: usize,
}

impl ToolDedupCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cek apakah panggilan ini bisa di-dedup. Mengembalikan `Some(stub)` bila identik & masih valid.
    pub fn check_hit(&mut self, name: &str, args: &Value, cwd: &Path) -> Option<String> {
        let fp = canonical_tool_fingerprint(name, args);
        let rec = self.records.get(&fp)?.clone();
        match name {
            "read_file" => {
                let rel = rec.file_rel.as_deref()?;
                let cur_sig = file_signature(cwd, rel)?;
                if rec.file_sig == Some(cur_sig) {
                    self.dedup_hits += 1;
                    return Some(format!(
                        "[Dedup: `read_file` '{rel}' tidak berubah sejak pembacaan sebelumnya — gunakan isi sebelumnya tanpa membaca ulang. Cuplikan: {}]",
                        rec.preview
                    ));
                }
                // File berubah di disk -> hapus cache lama dan izinkan baca ulang beneran!
                self.records.remove(&fp);
                None
            }
            "run_shell" | "verify_project" => {
                self.dedup_hits += 1;
                Some(format!(
                    "[Dedup: perintah `{name}` identik baru saja LULUS pada giliran ini tanpa perubahan file baru — tidak dijalankan ulang. Hasil sebelumnya: {}]",
                    rec.preview
                ))
            }
            _ => None,
        }
    }

    /// Catat panggilan tool yang SUKSES agar panggilan identik berikutnya dapat di-dedup.
    pub fn record_success(&mut self, name: &str, args: &Value, output: &str, cwd: &Path) {
        if !matches!(name, "read_file" | "run_shell" | "verify_project") {
            return;
        }
        if name == "run_shell" && !output.starts_with("exit code: 0") {
            return;
        }
        if name == "verify_project"
            && !output.contains("LULUS")
            && !output.contains("verifikasi proyek otomatis dilewati")
        {
            return;
        }
        let fp = canonical_tool_fingerprint(name, args);
        let (file_rel, file_sig) = if name == "read_file" {
            let rel = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let sig = rel.as_deref().and_then(|r| file_signature(cwd, r));
            (rel, sig)
        } else {
            (None, None)
        };
        let preview: String = output
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(100)
            .collect();
        self.records.insert(
            fp,
            DedupRecord {
                tool_name: name.to_string(),
                file_rel,
                file_sig,
                preview,
            },
        );
    }

    /// Invalidasi cache verifikasi/shell & file yang baru diedit agar perubahan baru tetap terverifikasi!
    pub fn invalidate_on_edit(&mut self, edited_path: Option<&str>) {
        self.records.retain(|_, rec| {
            if matches!(rec.tool_name.as_str(), "run_shell" | "verify_project") {
                return false;
            }
            if let (Some(ed), Some(ref f)) = (edited_path, &rec.file_rel) {
                if ed == f {
                    return false;
                }
            }
            true
        });
    }
}

/// Hasil evaluasi `LoopGuard` setelah satu pemanggilan tool selesai.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopWarning {
    None,
    /// Tool yang sama dipanggil berulang kali dengan argumen identik.
    RepeatedCall { name: String, count: usize },
    /// Terlalu banyak error beruntun tanpa ada perubahan kode yang berhasil.
    Stagnation { consecutive_errors: usize },
}

impl LoopWarning {
    pub fn hint_message(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::RepeatedCall { name, count } => Some(format!(
                "[Self-Correction / Deteksi Loop] Kamu sudah memanggil tool `{name}` dengan argumen identik sebanyak {count} kali. \
                 HENTIKAN pengulangan ini! Evaluasi ulang akar masalah, ganti pendekatan, atau jelaskan secara jujur ke user jika masalah tidak bisa diselesaikan."
            )),
            Self::Stagnation { consecutive_errors } => Some(format!(
                "[Self-Correction / Deteksi Stagnasi] Terjadi {consecutive_errors} kegagalan tool beruntun tanpa kemajuan. \
                 Jangan terus mencoba hal yang sama: baca ulang error secara teliti, periksa asumsi atau file terkait, atau laporkan kendala sebenarnya ke user."
            )),
        }
    }
}

/// Pendeteksi pengulangan panggilan tool, kegagalan berulang, & stagnasi error dalam satu giliran agent.
#[derive(Debug, Default)]
pub struct LoopGuard {
    call_counts: HashMap<u64, usize>,
    identical_fail_counts: HashMap<u64, usize>,
    tool_fail_counts: HashMap<String, usize>,
    consecutive_errors: usize,
    stagnated: bool,
    /// Batas kegagalan tool yang sama sebelum peringatan (aktif di runtime Tahap 2 = `Some(3)`).
    tool_fail_limit: Option<usize>,
}

impl LoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Guard lengkap Tahap 2 (Hermes): peringatkan pada 2x gagal identik ATAU 3x gagal tool yang sama.
    pub fn with_hermes_guards() -> Self {
        Self {
            tool_fail_limit: Some(3),
            ..Self::default()
        }
    }

    pub fn is_stagnated(&self) -> bool {
        self.stagnated
    }

    /// Catat satu eksekusi tool dan periksa apakah terjadi loop / kegagalan berulang / stagnasi.
    pub fn record(&mut self, tool_name: &str, args: &Value, is_error: bool) -> LoopWarning {
        let fp = canonical_tool_fingerprint(tool_name, args);
        let count = self.call_counts.entry(fp).or_insert(0);
        *count += 1;
        let current_count = *count;

        if is_error {
            self.consecutive_errors += 1;
            let id_fails = self.identical_fail_counts.entry(fp).or_insert(0);
            *id_fails += 1;
            let id_fail_now = *id_fails;

            let t_fails = self.tool_fail_counts.entry(tool_name.to_string()).or_insert(0);
            *t_fails += 1;
            let t_fail_now = *t_fails;

            if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                self.stagnated = true;
                return LoopWarning::Stagnation {
                    consecutive_errors: self.consecutive_errors,
                };
            }
            // Tahap 2: peringatan pada 2x kegagalan identik, atau 3x kegagalan tool sama bila guard aktif.
            if id_fail_now >= 2
                || self
                    .tool_fail_limit
                    .map(|lim| t_fail_now >= lim)
                    .unwrap_or(false)
            {
                return LoopWarning::RepeatedCall {
                    name: tool_name.to_string(),
                    count: id_fail_now.max(t_fail_now),
                };
            }
        } else {
            self.consecutive_errors = 0;
        }

        if current_count >= MAX_IDENTICAL_CALLS {
            return LoopWarning::RepeatedCall {
                name: tool_name.to_string(),
                count: current_count,
            };
        }
        LoopWarning::None
    }
}

/// Apakah perintah shell ini merupakan aksi verifikasi (test / build / check / lint)?
fn is_verification_shell_cmd(args: &Value) -> bool {
    let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let lower = cmd.to_ascii_lowercase();
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo build",
        "npm test",
        "npm run test",
        "npm run build",
        "npm run lint",
        "npm run typecheck",
        "pnpm test",
        "yarn test",
        "tsc",
        "pytest",
        "go test",
        "go build",
        "go vet",
        "make test",
        "make check",
        "unittest",
        "ruff check",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

/// Apakah ekstensi file termasuk kode sumber yang idealnya diverifikasi setelah diedit?
fn is_code_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".c", ".cpp", ".h",
        ".java", ".kt", ".swift", ".rb", ".php", ".cs", "cargo.toml", "package.json", "go.mod",
        "pyproject.toml",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// State pelacakan self-correction dalam satu giliran eksekusi agent.
#[derive(Debug)]
pub struct SelfCorrectState {
    pub edited_files: Vec<String>,
    pub edited_code_file: bool,
    pub verified_after_last_edit: bool,
    pub last_verify_passed: Option<bool>,
    pub repair_attempts: usize,
    pub max_attempts: usize,
    pub checkpoint_id: Option<usize>,
}

impl Default for SelfCorrectState {
    fn default() -> Self {
        Self {
            edited_files: Vec::new(),
            edited_code_file: false,
            verified_after_last_edit: false,
            last_verify_passed: None,
            repair_attempts: 0,
            max_attempts: MAX_REPAIR_ATTEMPTS,
            checkpoint_id: None,
        }
    }
}

impl SelfCorrectState {
    pub fn new(checkpoint_id: Option<usize>) -> Self {
        Self {
            checkpoint_id,
            ..Self::default()
        }
    }

    pub fn with_max_attempts(mut self, max_attempts: usize) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Perbarui state berdasarkan hasil eksekusi 1 tool.
    pub fn observe_tool(&mut self, name: &str, args: &Value, output: &str, is_error: bool) {
        match name {
            "write_file" | "edit_file" => {
                // Abaikan jika gagal atau ditolak user.
                if is_error || output.starts_with("Ditolak user") {
                    return;
                }
                if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                    if !self.edited_files.iter().any(|p| p == path) {
                        self.edited_files.push(path.to_string());
                    }
                    if is_code_file(path) {
                        self.edited_code_file = true;
                    }
                }
                // Ada edit baru -> verifikasi lama tidak lagi berlaku.
                self.verified_after_last_edit = false;
                self.last_verify_passed = None;
            }
            "lsp_rename" => {
                let dry_run = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
                if dry_run
                    || is_error
                    || output.starts_with("Ditolak user")
                    || !output.contains("diterapkan")
                {
                    return;
                }
                self.edited_code_file = true;
                if !self.edited_files.iter().any(|p| p == "(lsp_rename)") {
                    self.edited_files.push("(lsp_rename)".to_string());
                }
                self.verified_after_last_edit = false;
                self.last_verify_passed = None;
            }
            "verify_project" => {
                self.verified_after_last_edit = true;
                if !is_error
                    && (output.contains("verifikasi proyek otomatis dilewati")
                        || output.starts_with("Ditolak user"))
                {
                    self.last_verify_passed = Some(true);
                    return;
                }
                let no_tests = output.contains("TIDAK ADA TEST") || output.contains("NO_TESTS");
                let timed_out = output.contains("TIMEOUT");
                let ok = !is_error && !no_tests && !timed_out && output.contains("LULUS");
                self.last_verify_passed = Some(ok);
            }
            "run_shell" => {
                if is_verification_shell_cmd(args) {
                    self.verified_after_last_edit = true;
                    let ok = !is_error && output.starts_with("exit code: 0");
                    self.last_verify_passed = Some(ok);
                }
            }
            _ => {}
        }
    }

    /// Evaluasi sebelum agent menyelesaikan giliran:
    /// - Bila file kode sudah diubah tapi BELUM diverifikasi sesudah edit terakhir,
    ///   atau verifikasi terakhir GAGAL, kembalikan instruksi perbaikan internal
    ///   (maks `MAX_REPAIR_ATTEMPTS` siklus).
    /// - Kembalikan `None` bila sudah terverifikasi, tidak ada file kode diubah,
    ///   atau batas percobaan perbaikan sudah tercapai (agar agent melaporkan kondisi
    ///   secara jujur, bukan looping selamanya).
    pub fn completion_gate(&mut self, remaining_steps: usize) -> Option<String> {
        let cap = self.max_attempts;
        if !self.edited_code_file || remaining_steps < 2 || self.repair_attempts >= cap {
            return None;
        }
        // Tahap 2: jika verifikasi sudah LULUS setelah edit terakhir, JANGAN ulangi!
        if self.verified_after_last_edit && self.last_verify_passed == Some(true) {
            return None;
        }
        let files = self.edited_files.join(", ");
        if !self.verified_after_last_edit {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            return Some(format!(
                "[Self-Correction Gate {attempt}/{cap}] Kamu baru saja mengubah file kode ({files}), \
                 tetapi belum menjalankan verifikasi sesudah edit terakhir. \
                 Jalankan 1 perintah verifikasi paling langsung (`verify_project` atau test yang sudah ada) SEKALI saja, lalu berhenti jika lulus."
            ));
        }
        if self.last_verify_passed == Some(false) {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            return Some(format!(
                "[Self-Correction Gate {attempt}/{cap}] Verifikasi terakhir pada perubahan ({files}) masih GAGAL. \
                 Analisis error secara teliti, perbaiki dengan perubahan seminimal mungkin, lalu verifikasi ulang sekali."
            ));
        }
        None
    }
}
