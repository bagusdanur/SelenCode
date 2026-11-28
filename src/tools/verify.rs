//! Automatic Test & Repair Loop — Deteksi Toolchain, Validasi Proyek, &
//! Parser Diagnostik Error Terstruktur (Tahap 1).
//!
//! Tool `verify_project` mendeteksi jenis proyek (Rust, Node/TS, Python, Go,
//! Make), menjalankan pemeriksaan aman (tanpa menginstal dependency dan tanpa
//! perintah destruktif), lalu mengekstrak error terstruktur lengkap dengan
//! lokasi file, nomor baris, dan potongan kode terkait agar agent bisa
//! melakukan perbaikan tepat sasaran.

use super::{run_timed, schema, str_arg, truncate_output, Ctx, Tool, MAX_TOOL_OUTPUT};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 180;
const MAX_DIAGNOSTICS: usize = 12;

/// Jenis proyek yang terdeteksi secara otomatis dari struktur workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Rust,
    Node,
    Python,
    Go,
    Make,
    Unknown,
}

impl ProjectKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust (Cargo)",
            Self::Node => "Node.js / TypeScript",
            Self::Python => "Python",
            Self::Go => "Go",
            Self::Make => "Make",
            Self::Unknown => "Tidak terdeteksi",
        }
    }
}

/// Satu langkah validasi aman yang akan dijalankan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyStep {
    pub name: String,
    pub command: String,
}

/// Rencana pemeriksaan otomatis untuk sebuah workspace.
#[derive(Debug, Clone)]
pub struct VerifyPlan {
    pub kind: ProjectKind,
    pub steps: Vec<VerifyStep>,
}

/// Diagnostik error terstruktur yang diekstrak dari output build/test/linter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    pub line: usize,
    pub col: Option<usize>,
    pub code: Option<String>,
    pub message: String,
    pub snippet: Option<String>,
}

/// Cek cepat apakah workspace memiliki penanda proyek yang didukung `verify_project`
/// (`Cargo.toml`, `package.json`, `pyproject.toml`, `setup.py`, `pytest.ini`, `go.mod`, `Makefile`).
pub fn has_project_markers(cwd: &Path) -> bool {
    [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "pytest.ini",
        "go.mod",
        "Makefile",
        "makefile",
    ]
    .iter()
    .any(|m| cwd.join(m).exists())
}

/// Deteksi jenis proyek & susun langkah validasi aman sesuai `scope`:
/// `"check"` (cepat: type-check/compile), `"test"` (unit/integration tests),
/// `"lint"` (clippy/lint), atau `"all"` (check + test).
pub fn detect_project_plan(cwd: &Path, scope: &str) -> VerifyPlan {
    let scope = scope.trim().to_ascii_lowercase();
    let scope = if scope.is_empty() { "all" } else { scope.as_str() };

    if cwd.join("Cargo.toml").exists() {
        let mut steps = Vec::new();
        match scope {
            "check" => steps.push(VerifyStep {
                name: "cargo check".into(),
                command: "cargo check --tests".into(),
            }),
            "lint" => steps.push(VerifyStep {
                name: "cargo clippy".into(),
                command: "cargo clippy --all-targets".into(),
            }),
            "test" => steps.push(VerifyStep {
                name: "cargo test".into(),
                command: "cargo test".into(),
            }),
            _ => {
                steps.push(VerifyStep {
                    name: "cargo check".into(),
                    command: "cargo check --tests".into(),
                });
                steps.push(VerifyStep {
                    name: "cargo test".into(),
                    command: "cargo test".into(),
                });
            }
        }
        return VerifyPlan {
            kind: ProjectKind::Rust,
            steps,
        };
    }

    if cwd.join("package.json").exists() {
        let pkg_text = std::fs::read_to_string(cwd.join("package.json")).unwrap_or_default();
        let pkg: Value = serde_json::from_str(&pkg_text).unwrap_or(Value::Null);
        let has_script = |name: &str| {
            pkg.get("scripts")
                .and_then(|s| s.get(name))
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        };
        let has_ts = cwd.join("tsconfig.json").exists();
        let runner = if cwd.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if cwd.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        let run_script = |s: &str| {
            if runner == "npm" {
                format!("npm run {s}")
            } else {
                format!("{runner} {s}")
            }
        };

        let mut steps = Vec::new();
        if matches!(scope, "check" | "all") {
            if has_script("typecheck") {
                steps.push(VerifyStep {
                    name: "typecheck".into(),
                    command: run_script("typecheck"),
                });
            } else if has_ts {
                steps.push(VerifyStep {
                    name: "tsc".into(),
                    command: "npx --no-install tsc --noEmit".into(),
                });
            } else if has_script("build") {
                steps.push(VerifyStep {
                    name: "build".into(),
                    command: run_script("build"),
                });
            }
        }
        if matches!(scope, "lint") && has_script("lint") {
            steps.push(VerifyStep {
                name: "lint".into(),
                command: run_script("lint"),
            });
        }
        if matches!(scope, "test" | "all") && has_script("test") {
            steps.push(VerifyStep {
                name: "test".into(),
                command: if runner == "npm" {
                    "npm test".into()
                } else {
                    format!("{runner} test")
                },
            });
        }
        return VerifyPlan {
            kind: ProjectKind::Node,
            steps,
        };
    }

    if cwd.join("go.mod").exists() {
        let mut steps = Vec::new();
        match scope {
            "check" => steps.push(VerifyStep {
                name: "go build".into(),
                command: "go build ./...".into(),
            }),
            "lint" => steps.push(VerifyStep {
                name: "go vet".into(),
                command: "go vet ./...".into(),
            }),
            "test" => steps.push(VerifyStep {
                name: "go test".into(),
                command: "go test ./...".into(),
            }),
            _ => {
                steps.push(VerifyStep {
                    name: "go vet".into(),
                    command: "go vet ./...".into(),
                });
                steps.push(VerifyStep {
                    name: "go test".into(),
                    command: "go test ./...".into(),
                });
            }
        }
        return VerifyPlan {
            kind: ProjectKind::Go,
            steps,
        };
    }

    if cwd.join("pyproject.toml").exists()
        || cwd.join("pytest.ini").exists()
        || cwd.join("setup.py").exists()
        || cwd.join("requirements.txt").exists()
    {
        let mut steps = Vec::new();
        match scope {
            "lint" => steps.push(VerifyStep {
                name: "ruff".into(),
                command: "ruff check .".into(),
            }),
            "check" => steps.push(VerifyStep {
                name: "py_compile".into(),
                command: "python3 -m compileall -q .".into(),
            }),
            _ => steps.push(VerifyStep {
                name: "pytest".into(),
                command: "pytest -q".into(),
            }),
        }
        return VerifyPlan {
            kind: ProjectKind::Python,
            steps,
        };
    }

    if cwd.join("Makefile").exists() {
        let mk = std::fs::read_to_string(cwd.join("Makefile")).unwrap_or_default();
        let mut steps = Vec::new();
        if mk.lines().any(|l| l.starts_with("test:")) {
            steps.push(VerifyStep {
                name: "make test".into(),
                command: "make test".into(),
            });
        } else if mk.lines().any(|l| l.starts_with("check:")) {
            steps.push(VerifyStep {
                name: "make check".into(),
                command: "make check".into(),
            });
        }
        return VerifyPlan {
            kind: ProjectKind::Make,
            steps,
        };
    }

    VerifyPlan {
        kind: ProjectKind::Unknown,
        steps: Vec::new(),
    }
}

/// Baca potongan kecil (maks 3 baris) di sekitar baris `line_num` (1-based)
/// dengan menghormati sandbox & menolak file rahasia (`.env`, `*.key`, dll.).
fn read_code_snippet(cwd: &Path, rel_file: &str, line_num: usize) -> Option<String> {
    if line_num == 0 {
        return None;
    }
    let clean = rel_file.trim().trim_matches('"');
    let p = resolve_in_sandbox(cwd, Path::new(clean)).ok()?;
    if is_secret_file(&p) || !p.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&p).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    if line_num > lines.len() {
        return None;
    }
    let start = line_num.saturating_sub(2).max(1);
    let end = (line_num + 1).min(lines.len());
    let mut out = String::new();
    for i in start..=end {
        let mark = if i == line_num { ">" } else { " " };
        out.push_str(&format!("  {mark} {i:>4} | {}\n", lines[i - 1]));
    }
    Some(out.trim_end().to_string())
}

/// Ekstrak error terstruktur (file, baris, kolom, kode, pesan, snippet kode)
/// dari output Rust (`rustc`/`cargo`), TypeScript (`tsc`), Python (`pytest`/traceback),
/// dan Go (`go build`/`go test`).
pub fn parse_diagnostics(cwd: &Path, output: &str) -> Vec<Diagnostic> {
    static RUST_ERR: OnceLock<Regex> = OnceLock::new();
    static RUST_LOC: OnceLock<Regex> = OnceLock::new();
    static TS_ERR: OnceLock<Regex> = OnceLock::new();
    static PY_LOC: OnceLock<Regex> = OnceLock::new();
    static GO_ERR: OnceLock<Regex> = OnceLock::new();

    let re_rust_err = RUST_ERR.get_or_init(|| {
        Regex::new(r"^error(?:\[(E\d+)\])?:\s+(.+)$").expect("regex rust_err valid")
    });
    let re_rust_loc = RUST_LOC.get_or_init(|| {
        Regex::new(r"^\s*-->\s+([^:]+):(\d+):(\d+)").expect("regex rust_loc valid")
    });
    let re_ts_err = TS_ERR.get_or_init(|| {
        Regex::new(r"^([^\s(]+)\((\d+),(\d+)\):\s+error\s+(TS\d+):\s+(.+)$")
            .expect("regex ts_err valid")
    });
    let re_py_loc = PY_LOC.get_or_init(|| {
        Regex::new(r#"^\s*File\s+"([^"]+)",\s+line\s+(\d+)"#).expect("regex py_loc valid")
    });
    let re_go_err = GO_ERR.get_or_init(|| {
        Regex::new(r"^([^\s:]+\.go):(\d+)(?::(\d+))?:\s+(.+)$").expect("regex go_err valid")
    });

    let mut out: Vec<Diagnostic> = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0usize;

    while i < lines.len() && out.len() < MAX_DIAGNOSTICS {
        let raw = lines[i].trim_end();

        // 1. Rust: `error[E0308]: mismatched types` diikuti ` --> src/a.rs:12:5`
        if let Some(cap) = re_rust_err.captures(raw.trim_start()) {
            let code = cap.get(1).map(|m| m.as_str().to_string());
            let message = cap[2].trim().to_string();
            // Cari baris `--> file:line:col` dalam 3 baris berikutnya.
            let mut found_loc = None;
            for look in 1..=3 {
                if let Some(next_line) = lines.get(i + look) {
                    if let Some(lcap) = re_rust_loc.captures(next_line) {
                        let file = lcap[1].trim().to_string();
                        let line = lcap[2].parse::<usize>().unwrap_or(0);
                        let col = lcap[3].parse::<usize>().ok();
                        found_loc = Some((file, line, col));
                        break;
                    }
                }
            }
            if let Some((file, line, col)) = found_loc {
                let snippet = read_code_snippet(cwd, &file, line);
                out.push(Diagnostic {
                    file,
                    line,
                    col,
                    code,
                    message,
                    snippet,
                });
            }
            i += 1;
            continue;
        }

        // 2. TypeScript: `src/a.ts(12,5): error TS2322: ...`
        if let Some(cap) = re_ts_err.captures(raw.trim_start()) {
            let file = cap[1].to_string();
            let line = cap[2].parse::<usize>().unwrap_or(0);
            let col = cap[3].parse::<usize>().ok();
            let code = Some(cap[4].to_string());
            let message = cap[5].trim().to_string();
            let snippet = read_code_snippet(cwd, &file, line);
            out.push(Diagnostic {
                file,
                line,
                col,
                code,
                message,
                snippet,
            });
            i += 1;
            continue;
        }

        // 3. Python traceback: `File "app.py", line 42, in ...`
        if let Some(cap) = re_py_loc.captures(raw) {
            let file = cap[1].to_string();
            let line = cap[2].parse::<usize>().unwrap_or(0);
            let msg = lines
                .get(i + 2)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Python exception".to_string());
            let snippet = read_code_snippet(cwd, &file, line);
            out.push(Diagnostic {
                file,
                line,
                col: None,
                code: None,
                message: msg,
                snippet,
            });
            i += 1;
            continue;
        }

        // 4. Go compiler/vet: `main.go:18:9: undefined: foo`
        if let Some(cap) = re_go_err.captures(raw.trim_start()) {
            let file = cap[1].to_string();
            let line = cap[2].parse::<usize>().unwrap_or(0);
            let col = cap.get(3).and_then(|m| m.as_str().parse::<usize>().ok());
            let message = cap[4].trim().to_string();
            let snippet = read_code_snippet(cwd, &file, line);
            out.push(Diagnostic {
                file,
                line,
                col,
                code: None,
                message,
                snippet,
            });
            i += 1;
            continue;
        }

        i += 1;
    }

    out
}

/// Status verifikasi terstruktur yang membedakan secara jujur antara lulus,
/// gagal, tidak ada test yang ditemukan/dijalankan, timeout, dan belum dijalankan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyStatus {
    Passed,
    Failed,
    NoTestsFound,
    TimedOut,
    NotRun,
}

impl VerifyStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "LULUS ✅",
            Self::Failed => "GAGAL ❌",
            Self::NoTestsFound => "TIDAK ADA TEST ⚠️",
            Self::TimedOut => "TIMEOUT ⏱",
            Self::NotRun => "BELUM DIJALANKAN",
        }
    }

    pub fn card_tag(self) -> &'static str {
        match self {
            Self::Passed => "LULUS",
            Self::Failed => "GAGAL",
            Self::NoTestsFound => "NO_TESTS",
            Self::TimedOut => "TIMEOUT",
            Self::NotRun => "NOT_RUN",
        }
    }
}

/// Bukti hasil 1 langkah verifikasi (perintah, status, exit code, durasi, & jumlah test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEvidence {
    pub name: String,
    pub command: String,
    pub status: VerifyStatus,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub tests_passed: Option<usize>,
}

/// Deteksi status langkah dari kode keluar & output runner (membedakan `NoTestsFound` vs `Passed`).
pub fn classify_step_output(step_name: &str, exit_code: Option<i32>, output: &str) -> (VerifyStatus, Option<usize>) {
    let low = output.to_ascii_lowercase();
    let is_test_step = step_name.to_ascii_lowercase().contains("test")
        || step_name.to_ascii_lowercase().contains("pytest");

    // Pytest exit code 5 = no tests collected.
    if is_test_step && exit_code == Some(5) && low.contains("no tests ran") {
        return (VerifyStatus::NoTestsFound, Some(0));
    }

    if exit_code != Some(0) {
        return (VerifyStatus::Failed, None);
    }

    if is_test_step {
        // Rust cargo test: hitung total `N passed` di seluruh baris `test result:`.
        if low.contains("test result:") {
            static RE_RUST_PASSED: OnceLock<Regex> = OnceLock::new();
            let re = RE_RUST_PASSED.get_or_init(|| {
                Regex::new(r"test result:\s+\w+\.\s+(\d+)\s+passed").expect("regex rust passed")
            });
            let mut total_passed = 0usize;
            let mut saw_any = false;
            for cap in re.captures_iter(&low) {
                saw_any = true;
                total_passed += cap[1].parse::<usize>().unwrap_or(0);
            }
            if saw_any {
                if total_passed == 0 {
                    return (VerifyStatus::NoTestsFound, Some(0));
                }
                return (VerifyStatus::Passed, Some(total_passed));
            }
        }
        // Pytest / Go / Node zero-test indicators
        if low.contains("collected 0 items")
            || low.contains("no tests ran")
            || low.contains("[no test files]")
            || low.contains("[no tests to run]")
            || low.contains("no tests found")
            || low.contains("0 passing")
        {
            return (VerifyStatus::NoTestsFound, Some(0));
        }
    }

    (VerifyStatus::Passed, None)
}

/// Susun rencana verifikasi tertarget berdasarkan `changed_files` (Fase 3: Advanced Verification).
/// Memilih test yang paling relevan dengan file/modul yang berubah, sambil tetap
/// menjalankan type-check/compile cepat agar regresi lintas modul tertangkap.
pub fn detect_targeted_plan(cwd: &Path, scope: &str, changed_files: &[String]) -> VerifyPlan {
    let norm_scope = scope.trim().to_ascii_lowercase();
    if changed_files.is_empty() || matches!(norm_scope.as_str(), "check" | "lint" | "format") {
        if norm_scope == "format" && cwd.join("Cargo.toml").exists() {
            return VerifyPlan {
                kind: ProjectKind::Rust,
                steps: vec![VerifyStep {
                    name: "cargo fmt".into(),
                    command: "cargo fmt --check".into(),
                }],
            };
        }
        return detect_project_plan(cwd, scope);
    }

    // Validasi token target agar nama file/paket dari LLM atau repo tidak pernah menyisipkan metakarakter shell.
    let is_safe_token = |s: &str| -> bool {
        !s.is_empty()
            && s.len() <= 160
            && !s.starts_with('-')
            && !s.contains("..")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    };

    // Rust: cocokkan file `tests/<name>.rs` atau `src/<stem>.rs` -> `tests/*<stem>*.rs`
    if cwd.join("Cargo.toml").exists() {
        let mut targeted_tests: Vec<String> = Vec::new();
        for raw in changed_files {
            let rel = raw.replace('\\', "/");
            if let Some(rest) = rel.strip_prefix("tests/") {
                if let Some(stem) = rest.strip_suffix(".rs") {
                    if !stem.contains('/')
                        && is_safe_token(stem)
                        && !targeted_tests.iter().any(|t| t == stem)
                    {
                        targeted_tests.push(stem.to_string());
                    }
                }
            } else if let Some(stem) = Path::new(&rel)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
            {
                if stem.len() >= 3 && is_safe_token(&stem) {
                    if let Ok(entries) = std::fs::read_dir(cwd.join("tests")) {
                        for ent in entries.flatten() {
                            let fname = ent.file_name().to_string_lossy().to_string();
                            if let Some(tstem) = fname.strip_suffix(".rs") {
                                if is_safe_token(tstem)
                                    && tstem.to_ascii_lowercase().contains(&stem)
                                    && !targeted_tests.iter().any(|t| t == tstem)
                                {
                                    targeted_tests.push(tstem.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if !targeted_tests.is_empty() {
            let mut steps = Vec::new();
            if norm_scope != "test" {
                steps.push(VerifyStep {
                    name: "cargo check".into(),
                    command: "cargo check --tests".into(),
                });
            }
            let flags = targeted_tests
                .iter()
                .take(4)
                .map(|t| format!("--test {t}"))
                .collect::<Vec<_>>()
                .join(" ");
            steps.push(VerifyStep {
                name: format!("cargo test (targeted: {})", targeted_tests.join(", ")),
                command: format!("cargo test {flags}"),
            });
            return VerifyPlan {
                kind: ProjectKind::Rust,
                steps,
            };
        }
    }

    // Python: cocokkan file test yang berubah atau `tests/test_<stem>.py`
    if cwd.join("pyproject.toml").exists()
        || cwd.join("pytest.ini").exists()
        || cwd.join("setup.py").exists()
        || cwd.join("requirements.txt").exists()
    {
        let mut py_targets: Vec<String> = Vec::new();
        for raw in changed_files {
            let rel = raw.replace('\\', "/");
            if !rel.ends_with(".py") || !is_safe_token(&rel) {
                continue;
            }
            let fname = Path::new(&rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if fname.starts_with("test_") || fname.ends_with("_test.py") {
                if cwd.join(&rel).is_file() && !py_targets.contains(&rel) {
                    py_targets.push(rel);
                }
            } else if let Some(stem) = Path::new(&rel).file_stem().and_then(|s| s.to_str()) {
                if is_safe_token(stem) {
                    for cand in [format!("tests/test_{stem}.py"), format!("test_{stem}.py")] {
                        if cwd.join(&cand).is_file() && !py_targets.contains(&cand) {
                            py_targets.push(cand);
                        }
                    }
                }
            }
        }
        if !py_targets.is_empty() {
            return VerifyPlan {
                kind: ProjectKind::Python,
                steps: vec![VerifyStep {
                    name: format!("pytest (targeted: {})", py_targets.join(", ")),
                    command: format!("pytest -q {}", py_targets.join(" ")),
                }],
            };
        }
    }

    // Go: jalankan `go test` khusus pada paket direktori yang berubah
    if cwd.join("go.mod").exists() {
        let mut go_pkgs: Vec<String> = Vec::new();
        for raw in changed_files {
            let rel = raw.replace('\\', "/");
            if !rel.ends_with(".go") || !is_safe_token(&rel) {
                continue;
            }
            let dir = Path::new(&rel)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            if !dir.is_empty() && !is_safe_token(dir) {
                continue;
            }
            let pkg = if dir.is_empty() {
                ".".to_string()
            } else {
                format!("./{dir}")
            };
            if !go_pkgs.contains(&pkg) {
                go_pkgs.push(pkg);
            }
        }
        if !go_pkgs.is_empty() {
            return VerifyPlan {
                kind: ProjectKind::Go,
                steps: vec![VerifyStep {
                    name: format!("go test (targeted: {})", go_pkgs.join(", ")),
                    command: format!("go test {}", go_pkgs.join(" ")),
                }],
            };
        }
    }

    detect_project_plan(cwd, scope)
}

/// Hasil lengkap eksekusi `run_verify_plan`.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub kind: ProjectKind,
    pub status: VerifyStatus,
    pub passed: bool,
    pub steps_summary: Vec<(String, bool, Option<i32>)>,
    pub evidence: Vec<StepEvidence>,
    pub diagnostics: Vec<Diagnostic>,
    pub raw_tail: String,
}

impl VerifyReport {
    pub fn format_for_agent(&self) -> String {
        let status = self.status.label();
        let mut out = format!(
            "Verifikasi proyek [{}]: {status}\n",
            self.kind.label()
        );
        for ev in &self.evidence {
            let icon = match ev.status {
                VerifyStatus::Passed => "✓",
                VerifyStatus::NoTestsFound => "⚠",
                VerifyStatus::TimedOut => "⏱",
                _ => "✖",
            };
            let c = ev
                .exit_code
                .map(|x| x.to_string())
                .unwrap_or_else(|| "?".into());
            let test_note = ev
                .tests_passed
                .map(|n| format!(" · {n} test"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {icon} {} (exit {c} · {}ms{test_note}) [{}]\n",
                ev.name,
                ev.elapsed_ms,
                ev.status.card_tag()
            ));
        }
        if self.status == VerifyStatus::NoTestsFound {
            out.push_str(
                "\nCatatan bukti verifikasi: 0 test dijalankan (NO_TESTS). Jangan mengklaim test lulus bila tidak ada test yang benar-benar dijalankan.\n",
            );
        }
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "\nDiagnostik terstruktur ({} temuan):\n",
                self.diagnostics.len()
            ));
            for (idx, d) in self.diagnostics.iter().enumerate() {
                let loc = match d.col {
                    Some(c) => format!("{}:{}:{}", d.file, d.line, c),
                    None => format!("{}:{}", d.file, d.line),
                };
                let code_tag = d
                    .code
                    .as_deref()
                    .map(|c| format!("[{c}] "))
                    .unwrap_or_default();
                out.push_str(&format!("{}. {loc} — {code_tag}{}\n", idx + 1, d.message));
                if let Some(sn) = &d.snippet {
                    out.push_str(&format!("{sn}\n"));
                }
            }
        }
        if !self.passed && !self.raw_tail.trim().is_empty() {
            out.push_str("\nPotongan output terakhir:\n");
            out.push_str(&self.raw_tail);
        }
        out
    }

    pub fn format_menu_card(&self) -> String {
        let status = self.status.card_tag();
        let mut out = format!("# Verifikasi Proyek ({} · {status})\n", self.kind.label());
        for ev in &self.evidence {
            let mark = if ev.status == VerifyStatus::Passed { "*" } else { ">" };
            let c = ev
                .exit_code
                .map(|x| x.to_string())
                .unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "{mark} {} | exit {c} · {}ms | {}\n",
                ev.name,
                ev.elapsed_ms,
                ev.status.card_tag()
            ));
        }
        if !self.diagnostics.is_empty() {
            out.push_str("---\n");
            for d in self.diagnostics.iter().take(8) {
                let loc = format!("{}:{}", d.file, d.line);
                out.push_str(&format!(
                    "> {loc} | {} | {}\n",
                    d.code.as_deref().unwrap_or("error"),
                    d.message
                ));
            }
        }
        out
    }
}

/// Jalankan `VerifyPlan` di `cwd` secara berurutan (kompatibel penuh dengan API lama).
pub async fn run_verify_plan(cwd: &Path, scope: &str, timeout_secs: u64) -> Result<VerifyReport> {
    let default_sb = crate::sandbox::SandboxController::default();
    run_verify_plan_with_sandbox(cwd, scope, &[], timeout_secs, &default_sb).await
}

/// Jalankan verifikasi tertarget/bertingkat melalui `SandboxController` (env scrubbed + audit log)
/// dengan bukti lengkap per langkah (`VerifyStatus::Passed / Failed / NoTestsFound / TimedOut`).
pub async fn run_verify_plan_with_sandbox(
    cwd: &Path,
    scope: &str,
    changed_files: &[String],
    timeout_secs: u64,
    sandbox: &crate::sandbox::SandboxController,
) -> Result<VerifyReport> {
    let plan = detect_targeted_plan(cwd, scope, changed_files);
    if plan.steps.is_empty() {
        return Err(anyhow!(
            "tidak ada perintah validasi otomatis untuk proyek '{}' (scope='{scope}'). Jalankan `run_shell` secara eksplisit.",
            plan.kind.label()
        ));
    }

    let mut steps_summary = Vec::new();
    let mut evidence = Vec::new();
    let mut diagnostics = Vec::new();
    let mut overall = VerifyStatus::Passed;
    let mut raw_tail = String::new();

    for step in &plan.steps {
        let started = std::time::Instant::now();
        let mut cmd = sandbox.prepare_shell_command("verify_project", &step.command, cwd)?;

        match run_timed(&mut cmd, timeout_secs.max(10)).await {
            Ok((code, output)) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let (step_status, tests_passed) = classify_step_output(&step.name, code, &output);
                let ok = step_status == VerifyStatus::Passed;
                steps_summary.push((step.name.clone(), ok, code));
                evidence.push(StepEvidence {
                    name: step.name.clone(),
                    command: step.command.clone(),
                    status: step_status,
                    exit_code: code,
                    elapsed_ms,
                    tests_passed,
                });

                match step_status {
                    VerifyStatus::Passed => {}
                    VerifyStatus::NoTestsFound => {
                        if overall == VerifyStatus::Passed {
                            overall = VerifyStatus::NoTestsFound;
                        }
                    }
                    _ => {
                        overall = VerifyStatus::Failed;
                        diagnostics = parse_diagnostics(cwd, &output);
                        let tail_lines: Vec<&str> = output.lines().rev().take(35).collect();
                        let joined = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
                        raw_tail = truncate_output(joined, MAX_TOOL_OUTPUT / 3);
                        break;
                    }
                }
            }
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let msg = e.to_string();
                let is_timeout = msg.contains("timeout");
                let st = if is_timeout {
                    VerifyStatus::TimedOut
                } else {
                    VerifyStatus::Failed
                };
                overall = st;
                steps_summary.push((step.name.clone(), false, None));
                evidence.push(StepEvidence {
                    name: step.name.clone(),
                    command: step.command.clone(),
                    status: st,
                    exit_code: None,
                    elapsed_ms,
                    tests_passed: None,
                });
                raw_tail = msg;
                break;
            }
        }
    }

    Ok(VerifyReport {
        kind: plan.kind,
        status: overall,
        passed: overall == VerifyStatus::Passed,
        steps_summary,
        evidence,
        diagnostics,
        raw_tail,
    })
}

/// Tool `verify_project`: jalankan check/test/lint/targeted otomatis & kembalikan bukti verifikasi terstruktur.
pub struct VerifyProject {
    pub timeout_secs: u64,
}

impl VerifyProject {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs: if timeout_secs == 0 {
                DEFAULT_VERIFY_TIMEOUT_SECS
            } else {
                timeout_secs
            },
        }
    }
}

#[async_trait]
impl Tool for VerifyProject {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verify_project".into(),
            description: "Deteksi otomatis toolchain proyek (Rust/Node/Python/Go/Make) lalu jalankan pemeriksaan aman (`check`, `test`, `lint`, `format`, `targeted`, atau `all`), pilih test relevan untuk file yang berubah, dan kembalikan bukti verifikasi terstruktur (LULUS / GAGAL / NO_TESTS / TIMEOUT).".into(),
            parameters: schema(
                json!({
                    "scope": {
                        "type": "string",
                        "enum": ["check", "test", "lint", "format", "targeted", "all"],
                        "description": "Cakupan verifikasi: check (cepat), test, lint, format, targeted (khusus file berubah), atau all (default)."
                    },
                    "changed_files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Opsional: daftar path file yang baru diubah untuk pemilihan test tertarget."
                    }
                }),
                &[],
            ),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        // P0.2: verify_project menjalankan script/command milik proyek (cargo/npm/pytest/make)
        // sehingga WAJIB melewati gerbang persetujuan user (kecuali auto-approve aktif).
        true
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let scope = str_arg(&args, "scope").unwrap_or("all");
        let mut changed: Vec<String> = args
            .get("changed_files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // P1.7: bila model lupa mengirim `changed_files`, lengkapi otomatis dari `UndoStack` sesi.
        if changed.is_empty() {
            if let Ok(st) = ctx.undo_stack.lock() {
                for rel in st.recent_edited_files(12) {
                    if !changed.contains(&rel) {
                        changed.push(rel);
                    }
                }
            }
        }
        let plan = detect_targeted_plan(&ctx.cwd, scope, &changed);
        // Live-test P3: di folder polos tanpa penanda proyek, lewati verifikasi secara netral
        // tanpa menjalankan shell atau memicu percobaan perbaikan palsu.
        if plan.steps.is_empty() {
            return Ok(
                "Info: tidak ada penanda proyek (Cargo.toml/package.json/pyproject.toml/go.mod/Makefile) di folder ini — verifikasi proyek otomatis dilewati (tidak perlu membuat file test atau menjalankan git init)."
                    .to_string(),
            );
        }
        // P0.2: minta persetujuan eksplisit menampilkan daftar perintah proyek yang akan dieksekusi.
        let detail = plan
            .steps
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.command))
            .collect::<Vec<_>>()
            .join("\n");
        if !ctx
            .approver
            .ask(
                &format!("verify_project ({}, scope={scope})", plan.kind.label()),
                &detail,
            )
            .await
        {
            return Ok("Ditolak user — verifikasi proyek TIDAK dijalankan.".into());
        }
        let report = run_verify_plan_with_sandbox(
            &ctx.cwd,
            scope,
            &changed,
            self.timeout_secs,
            &ctx.sandbox,
        )
        .await?;
        Ok(report.format_for_agent())
    }
}
