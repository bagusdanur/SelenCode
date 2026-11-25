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

use serde_json::Value;
use std::collections::HashMap;

/// Batas pengulangan panggilan tool dengan argumen persis sama sebelum peringatan loop.
pub const MAX_IDENTICAL_CALLS: usize = 3;
/// Batas error tool beruntun tanpa kemajuan sebelum peringatan stagnasi.
pub const MAX_CONSECUTIVE_ERRORS: usize = 4;
/// Batas siklus perbaikan/verifikasi otomatis per giliran agar tidak infinite loop.
pub const MAX_REPAIR_ATTEMPTS: usize = 3;

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

/// Pendeteksi pengulangan panggilan tool & stagnasi error dalam satu giliran agent.
#[derive(Debug, Default)]
pub struct LoopGuard {
    call_counts: HashMap<String, usize>,
    consecutive_errors: usize,
}

impl LoopGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Catat satu eksekusi tool dan periksa apakah terjadi loop / stagnasi.
    pub fn record(&mut self, tool_name: &str, args: &Value, is_error: bool) -> LoopWarning {
        let sig = format!("{tool_name}::{}", args);
        let count = self.call_counts.entry(sig).or_insert(0);
        *count += 1;
        let current_count = *count;

        if is_error {
            self.consecutive_errors += 1;
        } else if matches!(tool_name, "edit_file" | "write_file" | "verify_project") {
            // Perubahan file atau verifikasi sukses mereset hitungan stagnasi.
            self.consecutive_errors = 0;
        } else {
            self.consecutive_errors = 0;
        }

        if current_count >= MAX_IDENTICAL_CALLS {
            return LoopWarning::RepeatedCall {
                name: tool_name.to_string(),
                count: current_count,
            };
        }
        if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            return LoopWarning::Stagnation {
                consecutive_errors: self.consecutive_errors,
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
#[derive(Debug, Default)]
pub struct SelfCorrectState {
    pub edited_files: Vec<String>,
    pub edited_code_file: bool,
    pub verified_after_last_edit: bool,
    pub last_verify_passed: Option<bool>,
    pub repair_attempts: usize,
    pub checkpoint_id: Option<usize>,
}

impl SelfCorrectState {
    pub fn new(checkpoint_id: Option<usize>) -> Self {
        Self {
            checkpoint_id,
            ..Self::default()
        }
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
                let ok = !is_error && output.contains("LULUS");
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
        if !self.edited_code_file || remaining_steps < 2 || self.repair_attempts >= MAX_REPAIR_ATTEMPTS {
            return None;
        }
        let files = self.edited_files.join(", ");
        if !self.verified_after_last_edit {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            return Some(format!(
                "[Self-Correction Gate {attempt}/{MAX_REPAIR_ATTEMPTS}] Kamu baru saja mengubah file kode ({files}), \
                 tetapi belum menjalankan verifikasi sesudah edit terakhir. \
                 Jalankan `verify_project` (atau `run_shell` test/check yang relevan) sekarang untuk memastikan perubahan tidak menimbulkan error, \
                 lalu perbaiki jika ada kegagalan sebelum melaporkan hasil akhir."
            ));
        }
        if self.last_verify_passed == Some(false) {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            return Some(format!(
                "[Self-Correction Gate {attempt}/{MAX_REPAIR_ATTEMPTS}] Verifikasi terakhir pada perubahan ({files}) masih GAGAL. \
                 Analisis error secara teliti, perbaiki dengan perubahan seminimal mungkin, lalu jalankan ulang verifikasi. \
                 Jika masalah memang tidak dapat diselesaikan di lingkungan ini, jelaskan penyebab sebenarnya secara jujur tanpa mengklaim berhasil."
            ));
        }
        None
    }
}
