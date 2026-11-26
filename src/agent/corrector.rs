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
    /// Pola Hermes `tool_guardrails.py` (v0.5.14 Perbaikan 1 #4): panggilan `(tool, args, result)`
    /// identik berturut-turut sebanyak `>= 3` kali — langkah terbakar tanpa kemajuan.
    ConsecutiveIdenticalResult { name: String, count: usize },
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
            Self::ConsecutiveIdenticalResult { name, count } => Some(format!(
                "[PERINGATAN PENGULANGAN IDENTIK — {count}x beruntun] Panggilan `{name}` dengan argumen sama menghasilkan keluaran identik {count} kali berturut-turut. \
                 Kamu sedang berputar dan membakar sisa langkah tanpa kemajuan! Hentikan pengulangan ini, ganti pendekatan, atau langsung terapkan perbaikan sekarang."
            )),
            Self::Stagnation { consecutive_errors } => Some(format!(
                "[Self-Correction / Deteksi Stagnasi] Terjadi {consecutive_errors} kegagalan tool beruntun tanpa kemajuan. \
                 Jangan terus mencoba hal yang sama: baca ulang error secara teliti, periksa asumsi atau file terkait, atau laporkan kendala sebenarnya ke user."
            )),
        }
    }
}

/// Apakah tool ini merupakan poller/penunggu latar yang sah diulang (pengecualian poller ala Hermes)?
pub fn is_polling_tool(tool_name: &str, args: &Value) -> bool {
    match tool_name {
        "watch_job" => {
            let act = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("status")
                .to_ascii_lowercase();
            matches!(act.as_str(), "status" | "check" | "list" | "wait" | "poll")
        }
        "background_status" | "process_status" => true,
        _ => false,
    }
}

fn fnv1a_hash_str(seed: u64, s: &str) -> u64 {
    let mut hash = seed;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Pendeteksi pengulangan panggilan tool, pengulangan hasil identik 3x beruntun (Hermes),
/// kegagalan berulang, & stagnasi error dalam satu giliran agent.
#[derive(Debug, Default)]
pub struct LoopGuard {
    call_counts: HashMap<u64, usize>,
    identical_fail_counts: HashMap<u64, usize>,
    tool_fail_counts: HashMap<String, usize>,
    consecutive_errors: usize,
    stagnated: bool,
    /// Batas kegagalan tool yang sama sebelum peringatan (aktif di runtime Tahap 2 = `Some(3)`).
    tool_fail_limit: Option<usize>,
    /// Fingerprint `(tool, args)` terakhir untuk mendeteksi rentetan identik berturut-turut.
    last_call_fp: Option<u64>,
    /// Fingerprint `(tool, args, result)` terakhir (pola Hermes `tool_guardrails.py`).
    last_result_fp: Option<u64>,
    /// Jumlah panggilan identik berturut-turut dengan hasil yang sama (ambang = 3).
    consecutive_identical_streak: usize,
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

    /// Catat satu eksekusi tool (API lama tetap utuh untuk kompatibilitas tes).
    pub fn record(&mut self, tool_name: &str, args: &Value, is_error: bool) -> LoopWarning {
        self.record_internal(tool_name, args, None, is_error)
    }

    /// Catat satu eksekusi tool beserta keluarannya (v0.5.14 Perbaikan 1 #4, pola Hermes `tool_guardrails.py`):
    /// menempelkan peringatan tepat pada pengulangan `(tool, args, result)` identik ke-3 berturut-turut,
    /// sambil mengecualikan tool polling/penunggu yang sah (`watch_job`).
    pub fn record_with_result(
        &mut self,
        tool_name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
    ) -> LoopWarning {
        self.record_internal(tool_name, args, Some(output), is_error)
    }

    fn record_internal(
        &mut self,
        tool_name: &str,
        args: &Value,
        output: Option<&str>,
        is_error: bool,
    ) -> LoopWarning {
        if is_polling_tool(tool_name, args) {
            // Tool polling sah (mis. `watch_job` status/wait) tidak boleh memicu peringatan berputar.
            if !is_error {
                self.consecutive_errors = 0;
            }
            return LoopWarning::None;
        }

        let fp = canonical_tool_fingerprint(tool_name, args);
        let count = self.call_counts.entry(fp).or_insert(0);
        *count += 1;
        let current_count = *count;

        // Lacak rentetan `(tool, args, result)` identik berturut-turut (ambang Hermes/opencode = 3).
        if let Some(out) = output {
            let trimmed = out.trim();
            let is_dedup_stub = trimmed.starts_with("[Dedup:");
            let res_fp = fnv1a_hash_str(fp, trimmed);
            let same_as_prev = if is_dedup_stub {
                // Stub dedup membuktikan `(tool, args)` sama dengan hasil dasar yang belum berubah.
                self.last_call_fp == Some(fp)
            } else {
                self.last_call_fp == Some(fp) && self.last_result_fp == Some(res_fp)
            };
            if same_as_prev {
                self.consecutive_identical_streak += 1;
            } else {
                self.last_call_fp = Some(fp);
                if !is_dedup_stub {
                    self.last_result_fp = Some(res_fp);
                }
                self.consecutive_identical_streak = 1;
            }
        } else {
            self.last_call_fp = Some(fp);
            self.last_result_fp = None;
            self.consecutive_identical_streak = 1;
        }

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
            if output.is_some() && self.consecutive_identical_streak >= MAX_IDENTICAL_CALLS {
                return LoopWarning::ConsecutiveIdenticalResult {
                    name: tool_name.to_string(),
                    count: self.consecutive_identical_streak,
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

        if output.is_some() && self.consecutive_identical_streak >= MAX_IDENTICAL_CALLS {
            return LoopWarning::ConsecutiveIdenticalResult {
                name: tool_name.to_string(),
                count: self.consecutive_identical_streak,
            };
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

/// Apakah perintah shell ini merupakan aksi verifikasi (test / build / check / lint / skrip repro)?
pub fn is_verification_shell_cmd(args: &Value) -> bool {
    let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Abaikan cek versi/bantuan/instalasi paket semata.
    if [
        "--version",
        " -v",
        " -V",
        "--help",
        "pip install",
        "pip list",
        "npm install",
        "git status",
        "git diff",
        "git log",
    ]
    .iter()
    .any(|excl| lower.ends_with(excl) || lower.starts_with(excl))
    {
        return false;
    }
    let runner_hit = [
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
        "flake8",
        "mypy",
        "pylint",
        "bin/test",
        "runtests.py",
        "manage.py test",
        "tox",
        "nox",
        "node --test",
        "node --check",
        "python -m unittest",
        "python3 -m unittest",
        "python -m pytest",
        "python3 -m pytest",
        "rspec",
        "phpunit",
        "mvn test",
        "gradle test",
        "dotnet test",
    ]
    .iter()
    .any(|kw| lower.contains(kw));
    if runner_hit {
        return true;
    }
    // Skrip repro / eksekusi tes langsung (`python repro.py`, `python -c "assert ..."`, `node hitung_total.js`, dll.)
    let starts_runtime = lower.starts_with("python ")
        || lower.starts_with("python3 ")
        || lower.starts_with("node ")
        || lower.contains(" && python ")
        || lower.contains(" && python3 ")
        || lower.contains(" && node ")
        || lower.contains("; python ")
        || lower.contains("; python3 ")
        || lower.contains("; node ");
    if starts_runtime {
        let runs_script_or_eval = lower.contains(".py")
            || lower.contains(".js")
            || lower.contains(".mjs")
            || lower.contains(".cjs")
            || lower.contains(".ts")
            || lower.contains(" -c ")
            || lower.contains(" -e ");
        if runs_script_or_eval {
            return true;
        }
    }
    false
}

/// Apakah jalur berkas tampak sebagai berkas tes lintas bahasa (Perbaikan 4 v0.5.15)?
/// Mencakup pola umum: `test_*.py`, `*_test.*`, `*.test.*`, `*.spec.*`, `*_spec.*`,
/// direktori `tests/` / `test/` / `__tests__/` / `spec/`, serta `*Test.java`/`*Tests.cs`, dsb.
pub fn is_test_file_path(path: &str) -> bool {
    let norm = path.trim().replace('\\', "/").to_ascii_lowercase();
    if norm.is_empty() {
        return false;
    }
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return false;
    }
    // Direktori tes umum di jalur induk
    if segs[..segs.len().saturating_sub(1)]
        .iter()
        .any(|d| matches!(*d, "tests" | "test" | "__tests__" | "spec" | "specs"))
    {
        return true;
    }
    let fname = *segs.last().unwrap_or(&"");
    fname.starts_with("test_")
        || fname.contains("_test.")
        || fname.contains(".test.")
        || fname.contains(".spec.")
        || fname.contains("_spec.")
        || fname.ends_with("test.java")
        || fname.ends_with("tests.java")
        || fname.ends_with("test.kt")
        || fname.ends_with("tests.kt")
        || fname.ends_with("test.cs")
        || fname.ends_with("tests.cs")
        || fname == "tests.py"
        || fname == "test.py"
        || fname == "test.js"
        || fname == "tests.js"
}

fn normalize_verify_cmd_tokens(cmd: &str) -> Vec<String> {
    cmd.split_whitespace()
        .map(|tok| {
            let clean = tok.trim_matches(|c| c == '"' || c == '\'');
            let low = clean.to_ascii_lowercase();
            match low.as_str() {
                "python3" | "py" => "python".to_string(),
                "./bin/test" => "bin/test".to_string(),
                _ => low,
            }
        })
        .collect()
}

fn verify_runner_family(tokens: &[String]) -> Option<&'static str> {
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] == "verify_project" {
        return Some("verify_project");
    }
    if tokens[0] == "pytest"
        || (tokens.len() >= 3 && tokens[0] == "python" && tokens[1] == "-m" && tokens[2] == "pytest")
    {
        return Some("pytest");
    }
    if tokens[0] == "bin/test" || tokens.iter().any(|t| t.ends_with("/bin/test")) {
        return Some("bin/test");
    }
    if tokens.len() >= 2 && tokens[0] == "cargo" && matches!(tokens[1].as_str(), "test" | "check" | "clippy" | "build") {
        return Some("cargo");
    }
    if tokens.len() >= 3 && tokens[0] == "python" && tokens[1] == "-m" && tokens[2] == "unittest" {
        return Some("unittest");
    }
    if tokens.len() >= 2 && tokens[0] == "go" && tokens[1] == "test" {
        return Some("go_test");
    }
    if tokens.len() >= 2 && matches!(tokens[0].as_str(), "npm" | "pnpm" | "yarn") && tokens.iter().any(|t| t == "test") {
        return Some("npm_test");
    }
    if tokens.len() >= 2 && tokens[0] == "node" && tokens[1] == "--test" {
        return Some("node_test");
    }
    None
}

fn extract_runner_targets(tokens: &[String]) -> Vec<String> {
    let mut skip_next = false;
    let mut start = 1usize;
    if tokens.len() >= 3 && tokens[0] == "python" && tokens[1] == "-m" {
        start = 3;
    } else if tokens.len() >= 2 && matches!(tokens[0].as_str(), "cargo" | "go" | "npm" | "pnpm" | "yarn") {
        start = 2;
    } else if tokens.len() >= 2 && tokens[0] == "node" && tokens[1] == "--test" {
        start = 2;
    }
    let mut targets = Vec::new();
    for tok in tokens.iter().skip(start) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(tok.as_str(), "-k" | "-m" | "--test" | "-p" | "--package" | "-o") {
            skip_next = true;
            continue;
        }
        if tok.starts_with('-') || tok == "run" {
            continue;
        }
        targets.push(tok.trim_start_matches("./").to_string());
    }
    targets
}

/// Apakah perintah `candidate` merupakan larian ulang dari perintah verifikasi mapan `established`
/// atau perluasan eksplisitnya (mis. filter lebih luas pada berkas/suite tes yang sama) — Perbaikan 3 v0.5.15?
/// Probe ad-hoc (mis. `python3 -c ...`), skrip lain, atau berkas tes tak terkait mengembalikan `false`.
pub fn is_same_or_broader_verify_cmd(established: &str, candidate: &str) -> bool {
    let est_toks = normalize_verify_cmd_tokens(established);
    let cand_toks = normalize_verify_cmd_tokens(candidate);
    if est_toks.is_empty() || cand_toks.is_empty() {
        return false;
    }
    if est_toks == cand_toks {
        return true;
    }
    // Probe ad-hoc (`python -c ...`, `node -e ...`) tidak pernah boleh menimpa perintah mapan lain!
    if cand_toks.iter().any(|t| t == "-c" || t == "-e") {
        return false;
    }
    let Some(est_fam) = verify_runner_family(&est_toks) else {
        return false;
    };
    let Some(cand_fam) = verify_runner_family(&cand_toks) else {
        return false;
    };
    if est_fam != cand_fam {
        return false;
    }
    let est_targets = extract_runner_targets(&est_toks);
    let cand_targets = extract_runner_targets(&cand_toks);
    // Menjalankan seluruh suite runner yang sama tanpa penyempitan -> perluasan sah (mis. `pytest` / `cargo test`).
    if cand_targets.is_empty() {
        return true;
    }
    if est_targets.is_empty() {
        // Mapan adalah suite penuh, candidate menyempit ke 1 file lain -> bukan perluasan!
        return false;
    }
    // Setiap target mapan harus tercakup oleh minimal satu target candidate
    // (identik, berkas yang sama tanpa `::test_fn`, atau direktori induknya).
    est_targets.iter().all(|et| {
        let et_file = et.split("::").next().unwrap_or(et);
        cand_targets.iter().any(|ct| {
            let ct_clean = ct.trim_end_matches('/');
            et == ct
                || et_file == ct_clean
                || et_file.starts_with(&format!("{ct_clean}/"))
        })
    })
}

fn parse_exit_code(output: &str) -> Option<i32> {
    let first = output.lines().next()?.trim();
    let rest = first.strip_prefix("exit code:")?;
    rest.split_whitespace().next()?.parse::<i32>().ok()
}

fn extract_verify_key_line(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("exit code:"))
        .collect();
    if lines.is_empty() {
        return "(tanpa keluaran teks)".to_string();
    }
    let interesting = lines.iter().rev().find(|l| {
        let low = l.to_ascii_lowercase();
        low.contains("passed")
            || low.contains("failed")
            || low.contains("error")
            || low.contains("assert")
            || low.contains("lulus")
            || low.contains("gagal")
            || low.contains("ok")
            || low.contains("test result:")
    });
    let chosen = interesting.copied().unwrap_or_else(|| *lines.last().unwrap());
    let clean: String = chosen.chars().take(160).collect();
    clean
}

/// Apakah nama file terlihat seperti skrip sampah/repro sementara ciptaan sesi?
fn is_scratch_repro_file(rel: &str) -> bool {
    let fname = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    fname.starts_with("repro")
        || fname.starts_with("tmp_")
        || fname.starts_with("_tmp")
        || fname.starts_with("debug_tmp")
        || fname.ends_with(".tmp")
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

fn cap_raw_verify_output(output: &str, max_chars: usize) -> String {
    let trimmed = output.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    let head_len = max_chars / 3;
    let tail_len = max_chars.saturating_sub(head_len);
    let head: String = trimmed.chars().take(head_len).collect();
    let tail: String = trimmed
        .chars()
        .skip(count.saturating_sub(tail_len))
        .collect();
    format!("{head}\n… [dipotong tengah agar fokus ke error akhir] …\n{tail}")
}

/// State pelacakan self-correction & gerbang verifikasi dalam satu giliran eksekusi agent
/// (Tahap 1 + Perbaikan 2 v0.5.8 + Perbaikan 2 v0.5.14: SWE-agent `review_on_submit` + Aider reflection).
#[derive(Debug)]
pub struct SelfCorrectState {
    pub edited_files: Vec<String>,
    /// File baru yang dibuat via `write_file` sepanjang giliran ini (untuk daftar periksa sampah/repro SWE-agent).
    pub created_files: Vec<String>,
    /// Berkas tes yang ikut diubah pada giliran ini (Perbaikan 4 v0.5.15 — visibilitas permanen di ringkasan akhir).
    pub edited_test_files: Vec<String>,
    pub edited_code_file: bool,
    pub verified_after_last_edit: bool,
    pub last_verify_passed: Option<bool>,
    /// True bila minimal satu perintah verifikasi nyata pernah dijalankan pada giliran ini (v0.5.14).
    pub has_verification_run: bool,
    /// Perintah verifikasi mapan untuk giliran ini (`run_shell` / `verify_project`, Perbaikan 3 v0.5.15).
    pub last_verify_cmd: Option<String>,
    /// Nomor langkah (1-based) saat perintah verifikasi mapan terakhir dijalankan.
    pub last_verify_step: Option<usize>,
    /// Exit code terakhir dari perintah verifikasi mapan (bila dari `run_shell`).
    pub last_verify_exit_code: Option<i32>,
    /// Baris hasil kunci dari larian terakhir perintah verifikasi mapan.
    pub last_verify_key_line: Option<String>,
    /// Keluaran mentah kegagalan verifikasi terakhir untuk umpan balik refleksi ala Aider.
    pub last_verify_output: Option<String>,
    /// Lingkup ketat v0.5.14: gerbang hanya hidup bila ada mutasi berkas DAN ada perintah verifikasi yang dijalankan.
    pub strict_known_verify_only: bool,
    pub repair_attempts: usize,
    pub max_attempts: usize,
    pub checkpoint_id: Option<usize>,
    /// True sesudah daftar periksa penutup SWE-agent (`review_on_submit`) dijalankan sekali.
    pub checklist_checked: bool,
}

impl Default for SelfCorrectState {
    fn default() -> Self {
        Self {
            edited_files: Vec::new(),
            created_files: Vec::new(),
            edited_test_files: Vec::new(),
            edited_code_file: false,
            verified_after_last_edit: false,
            last_verify_passed: None,
            has_verification_run: false,
            last_verify_cmd: None,
            last_verify_step: None,
            last_verify_exit_code: None,
            last_verify_key_line: None,
            last_verify_output: None,
            strict_known_verify_only: false,
            repair_attempts: 0,
            max_attempts: MAX_REPAIR_ATTEMPTS,
            checkpoint_id: None,
            checklist_checked: false,
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

    /// Aktifkan lingkup ketat v0.5.14 (Perbaikan 2 #4): gerbang hanya menegakkan kasus verifikasi
    /// yang benar-benar pernah dijalankan pada giliran ini (diam total pada tugas tanpa tes).
    pub fn with_strict_known_verify_only(mut self, strict: bool) -> Self {
        self.strict_known_verify_only = strict;
        self
    }

    /// Perbarui state berdasarkan hasil eksekusi 1 tool (kompatibel API lama).
    pub fn observe_tool(&mut self, name: &str, args: &Value, output: &str, is_error: bool) {
        self.observe_tool_at_step(name, args, output, is_error, 0);
    }

    /// Perbarui state berdasarkan hasil eksekusi 1 tool beserta nomor langkah 1-based (v0.5.14).
    pub fn observe_tool_at_step(
        &mut self,
        name: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        step_1based: usize,
    ) {
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
                    if name == "write_file" && !self.created_files.iter().any(|p| p == path) {
                        self.created_files.push(path.to_string());
                    }
                    if is_code_file(path) {
                        self.edited_code_file = true;
                    }
                    if is_test_file_path(path)
                        && !self.edited_test_files.iter().any(|p| p == path)
                    {
                        self.edited_test_files.push(path.to_string());
                    }
                }
                // Ada edit baru -> verifikasi lama tidak lagi berlaku sesudah edit terakhir,
                // namun tetap simpan `last_verify_cmd` / `last_verify_passed` agar gerbang tahu konteks gagalnya.
                self.verified_after_last_edit = false;
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
            }
            "verify_project" => {
                if !is_error
                    && (output.contains("verifikasi proyek otomatis dilewati")
                        || output.starts_with("Ditolak user"))
                {
                    // Perbaikan 3 (v0.5.15): bila sudah ada perintah verifikasi mapan dari `run_shell`,
                    // `verify_project` yang dilewati TIDAK boleh menimpa keadaannya!
                    if self.last_verify_cmd.is_none() {
                        self.verified_after_last_edit = true;
                        self.last_verify_passed = Some(true);
                    }
                    return;
                }
                // Bila sudah ada perintah verifikasi mapan berbeda (mis. `python uji.py`), jangan timpa!
                if let Some(ref est) = self.last_verify_cmd {
                    if !is_same_or_broader_verify_cmd(est, "verify_project") {
                        return;
                    }
                } else {
                    self.last_verify_cmd = Some("verify_project".to_string());
                }
                self.has_verification_run = true;
                self.verified_after_last_edit = true;
                self.last_verify_step = (step_1based > 0).then_some(step_1based);
                let no_tests = output.contains("TIDAK ADA TEST") || output.contains("NO_TESTS");
                let timed_out = output.contains("TIMEOUT");
                let ok = !is_error && !no_tests && !timed_out && output.contains("LULUS");
                self.last_verify_exit_code = Some(if ok { 0 } else { 1 });
                self.last_verify_key_line = Some(extract_verify_key_line(output));
                self.last_verify_passed = Some(ok);
                self.last_verify_output = if ok {
                    None
                } else {
                    Some(cap_raw_verify_output(output, 4_000))
                };
            }
            "run_shell" => {
                let cmd = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if cmd.is_empty() {
                    return;
                }
                // Perbaikan 3 (v0.5.15): integritas keadaan verifikasi — terikat pada perintah mapan.
                // Perintah lain (probe `python3 -c ...`, skrip bantu, suite tak terkait) TIDAK menimpa keadaan!
                let should_update = match self.last_verify_cmd.as_deref() {
                    None => is_verification_shell_cmd(args),
                    Some(est) => is_same_or_broader_verify_cmd(est, &cmd),
                };
                if should_update {
                    if self.last_verify_cmd.is_none() {
                        self.last_verify_cmd = Some(cmd);
                    }
                    self.has_verification_run = true;
                    self.verified_after_last_edit = true;
                    self.last_verify_step = (step_1based > 0).then_some(step_1based);
                    let ec = parse_exit_code(output);
                    self.last_verify_exit_code = ec;
                    self.last_verify_key_line = Some(extract_verify_key_line(output));
                    let ok = !is_error && (ec == Some(0) || output.starts_with("exit code: 0"));
                    self.last_verify_passed = Some(ok);
                    self.last_verify_output = if ok {
                        None
                    } else {
                        Some(cap_raw_verify_output(output, 4_000))
                    };
                }
            }
            _ => {}
        }
    }

    /// Evaluasi sebelum agent menyelesaikan giliran (pola SWE-agent `review_on_submit` + refleksi Aider):
    /// - Pada mode ketat (`strict_known_verify_only`), gerbang hanya hidup bila ada mutasi berkas
    ///   DAN ada perintah verifikasi yang benar-benar dijalankan pada giliran ini.
    /// - Bila verifikasi terakhir lulus sesudah edit terakhir -> `None` (0 suntikan).
    /// - Bila ada edit sesudah verifikasi terakhir ATAU verifikasi terakhir GAGAL -> tahan penutupan
    ///   hingga `max_attempts` (bawaan 3), menyertakan perintah tercatat dan keluaran gagal mentah.
    pub fn completion_gate(&mut self, remaining_steps: usize) -> Option<String> {
        let cap = self.max_attempts;
        let has_mutation = self.edited_code_file || !self.edited_files.is_empty();
        if !has_mutation || remaining_steps < 2 || self.repair_attempts >= cap {
            return None;
        }
        if self.strict_known_verify_only && !self.has_verification_run {
            return None;
        }
        // Jika verifikasi sudah LULUS setelah edit terakhir, JANGAN ulangi!
        if self.verified_after_last_edit && self.last_verify_passed == Some(true) {
            return None;
        }
        let files = self.edited_files.join(", ");
        let step_note = self
            .last_verify_step
            .map(|s| format!(" pada langkah {s}"))
            .unwrap_or_default();

        if !self.verified_after_last_edit {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            if let Some(cmd) = self.last_verify_cmd.as_deref() {
                let prev_status = match self.last_verify_passed {
                    Some(false) => "GAGAL",
                    Some(true) => "LULUS",
                    None => "belum terkonfirmasi",
                };
                return Some(format!(
                    "[Self-Correction Gate {attempt}/{cap} — Penutupan Ditahan] \
                     Keadaan tercatat: perintah verifikasi terakhir `{cmd}` dijalankan{step_note} (status: {prev_status}), \
                     dan setelah itu ada perubahan berkas ({files}) yang belum diverifikasi ulang. \
                     Jalankan ulang `{cmd}` sekarang untuk membuktikan perubahanmu; bila masih gagal, perbaiki hingga lulus, lalu tutup kembali."
                ));
            }
            return Some(format!(
                "[Self-Correction Gate {attempt}/{cap}] Kamu baru saja mengubah file kode ({files}), \
                 tetapi belum menjalankan verifikasi sesudah edit terakhir. \
                 Jalankan 1 perintah verifikasi paling langsung (`verify_project` atau test yang sudah ada) SEKALI saja, lalu berhenti jika lulus."
            ));
        }
        if self.last_verify_passed == Some(false) {
            self.repair_attempts += 1;
            let attempt = self.repair_attempts;
            let cmd = self
                .last_verify_cmd
                .as_deref()
                .unwrap_or("verifikasi terakhir");
            let raw_block = self
                .last_verify_output
                .as_deref()
                .map(|raw| format!("\n\nKeluaran kegagalan mentah dari `{cmd}`:\n{raw}"))
                .unwrap_or_default();
            return Some(format!(
                "[Self-Correction Gate {attempt}/{cap} — Penutupan Ditahan] \
                 Verifikasi terakhir `{cmd}`{step_note} pada perubahan ({files}) masih GAGAL.{raw_block}\n\n\
                 Analisis keluaran kegagalan mentah di atas, perbaiki penyebabnya dengan perubahan terarah, \
                 lalu jalankan ulang `{cmd}` untuk membuktikan hasil lulus sebelum menutup."
            ));
        }
        None
    }

    /// Daftar periksa penutup tugas coding ala SWE-agent `review_on_submit` (Perbaikan 2 #4 v0.5.8):
    /// (a) verifikasi terakhir (tes/repro/bukti) WAJIB dijalankan SETELAH perubahan terakhir;
    /// (b) berkas sampah/repro sementara ciptaan sesi dibersihkan atau disebutkan eksplisit;
    /// (c) jawaban akhir menyebut berkas yang diubah sebagai ringkasan perubahan.
    pub fn completion_gate_with_checklist(
        &mut self,
        remaining_steps: usize,
        final_reply: &str,
        cwd: &Path,
    ) -> Option<String> {
        if let Some(base_gate) = self.completion_gate(remaining_steps) {
            return Some(base_gate);
        }
        // Pada lingkup ketat v0.5.14, tugas coding tanpa perintah verifikasi diam total (0 suntikan).
        if self.strict_known_verify_only && !self.has_verification_run {
            return None;
        }
        if !self.edited_code_file || self.checklist_checked || remaining_steps < 2 {
            return None;
        }
        // Cari berkas sampah/repro yang dibuat sesi ini, masih ada di disk, dan belum disebut di jawaban.
        let lingering_scratch: Vec<String> = self
            .created_files
            .iter()
            .filter(|rel| {
                is_scratch_repro_file(rel)
                    && cwd.join(rel).exists()
                    && !final_reply.contains(rel.as_str())
            })
            .cloned()
            .collect();
        // Pada lingkup ketat v0.5.14: bila verifikasi sudah HIJAU setelah edit terakhir dan tidak ada
        // berkas sampah repro tertinggal, langsung terima penutupan tanpa suntikan tambahan!
        if self.strict_known_verify_only
            && self.verified_after_last_edit
            && self.last_verify_passed == Some(true)
            && lingering_scratch.is_empty()
        {
            self.checklist_checked = true;
            return None;
        }
        // Cek apakah jawaban akhir sudah menyebut minimal satu berkas kode yang diubah.
        let mentions_edited = self.edited_files.iter().any(|f| {
            let base = Path::new(f)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(f.as_str());
            final_reply.contains(f.as_str()) || final_reply.contains(base)
        });

        if lingering_scratch.is_empty() && mentions_edited {
            self.checklist_checked = true;
            return None;
        }
        self.checklist_checked = true;
        let mut items = Vec::new();
        if !lingering_scratch.is_empty() {
            items.push(format!(
                "berkas sementara/repro masih tersisa ({}) — hapus jika tidak diperlukan atau sebutkan secara eksplisit",
                lingering_scratch.join(", ")
            ));
        }
        if !mentions_edited {
            items.push(format!(
                "sebutkan ringkasan perubahan beserta daftar berkas yang diubah ({})",
                self.edited_files.join(", ")
            ));
        }
        Some(format!(
            "[Daftar Periksa Penutup] Sebelum menyatakan selesai: {}. \
             Jangan menjalankan ulang tes yang sudah lulus bila tidak ada edit baru.",
            items.join("; ")
        ))
    }

    /// Catatan status jujur (pola Hermes + Perbaikan 2 #3 v0.5.14): bila penutupan diterima
    /// sesudah batas putaran refleksi tercapai sementara verifikasi terakhir masih GAGAL atau
    /// belum dijalankan ulang sesudah edit terakhir, kembalikan blok status jujur untuk ditempel
    /// pada ringkasan penutup agar tidak pernah menyamar sebagai sukses.
    pub fn honest_unverified_closing_note(&self, final_reply: &str) -> Option<String> {
        let has_mutation = self.edited_code_file || !self.edited_files.is_empty();
        if !has_mutation || !self.has_verification_run {
            return None;
        }
        if self.verified_after_last_edit && self.last_verify_passed == Some(true) {
            return None;
        }
        if final_reply.contains("[Status Verifikasi Akhir") {
            return None;
        }
        let cmd = self
            .last_verify_cmd
            .as_deref()
            .unwrap_or("verifikasi terakhir");
        let status_desc = if !self.verified_after_last_edit {
            match self.last_verify_passed {
                Some(false) => "masih GAGAL dan belum dijalankan ulang setelah edit terakhir",
                _ => "belum dijalankan ulang setelah edit terakhir",
            }
        } else {
            "masih GAGAL"
        };
        let snippet = self
            .last_verify_output
            .as_deref()
            .and_then(|out| {
                out.lines()
                    .rev()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(|l| l.chars().take(160).collect::<String>())
            })
            .map(|s| format!(" Ringkasan error terakhir: {s}"))
            .unwrap_or_default();
        Some(format!(
            "\n\n[Status Verifikasi Akhir — GAGAL] Perintah `{cmd}` {status_desc} \
             (putaran gerbang verifikasi: {}/{}).{snippet}",
            self.repair_attempts, self.max_attempts
        ))
    }

    /// Baris ringkasan verifikasi spesifik yang dikutip langsung dari keadaan tercatat loop
    /// (Perbaikan 3 #2 v0.5.15: perintah mapan + exit code + baris hasil kunci, atau `belum diverifikasi`).
    pub fn closing_verification_summary_line(&self, turn_requests_edit: bool) -> Option<String> {
        let has_mutation = self.edited_code_file || !self.edited_files.is_empty();
        if !has_mutation {
            return None;
        }
        if self.has_verification_run {
            let cmd = self
                .last_verify_cmd
                .as_deref()
                .unwrap_or("verifikasi");
            let ec_str = self
                .last_verify_exit_code
                .map(|c| format!("exit code {c}"))
                .unwrap_or_else(|| "exit code tidak tercatat".to_string());
            let status = if !self.verified_after_last_edit {
                match self.last_verify_passed {
                    Some(false) => "GAGAL / BASI: belum dijalankan ulang sesudah edit terakhir",
                    Some(true) => "BASI: belum dijalankan ulang sesudah edit terakhir",
                    None => "BASI",
                }
            } else if self.last_verify_passed == Some(true) {
                "LULUS"
            } else {
                "GAGAL"
            };
            let key_line = self
                .last_verify_key_line
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("(tanpa keluaran teks)");
            return Some(format!(
                "Verifikasi: `{cmd}` — {ec_str} ({status}) — {key_line}"
            ));
        }
        if turn_requests_edit {
            return Some(
                "Verifikasi: belum diverifikasi (tidak ada perintah verifikasi yang dijalankan pada giliran ini)."
                    .to_string(),
            );
        }
        None
    }

    /// Baris catatan wajib bila ada berkas tes yang ikut diubah selama giliran (Perbaikan 4 v0.5.15).
    pub fn closing_test_files_note(&self) -> Option<String> {
        if self.edited_test_files.is_empty() {
            return None;
        }
        Some(format!(
            "Catatan berkas tes diubah: {}",
            self.edited_test_files.join(", ")
        ))
    }
}
