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

/// Hasil lengkap eksekusi `run_verify_plan`.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub kind: ProjectKind,
    pub passed: bool,
    pub steps_summary: Vec<(String, bool, Option<i32>)>,
    pub diagnostics: Vec<Diagnostic>,
    pub raw_tail: String,
}

impl VerifyReport {
    pub fn format_for_agent(&self) -> String {
        let status = if self.passed { "LULUS ✅" } else { "GAGAL ❌" };
        let mut out = format!(
            "Verifikasi proyek [{}]: {status}\n",
            self.kind.label()
        );
        for (name, ok, code) in &self.steps_summary {
            let icon = if *ok { "✓" } else { "✖" };
            let c = code.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
            out.push_str(&format!("  {icon} {name} (exit {c})\n"));
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
        let status = if self.passed { "LULUS" } else { "GAGAL" };
        let mut out = format!("# Verifikasi Proyek ({} · {status})\n", self.kind.label());
        for (name, ok, code) in &self.steps_summary {
            let mark = if *ok { "*" } else { ">" };
            let c = code.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
            let tag = if *ok { "lulus" } else { "gagal" };
            out.push_str(&format!("{mark} {name} | exit {c} | {tag}\n"));
        }
        if !self.diagnostics.is_empty() {
            out.push_str("---\n");
            for d in self.diagnostics.iter().take(8) {
                let loc = format!("{}:{}", d.file, d.line);
                out.push_str(&format!("> {loc} | {} | {}\n", d.code.as_deref().unwrap_or("error"), d.message));
            }
        }
        out
    }
}

/// Jalankan `VerifyPlan` di `cwd` secara berurutan (berhenti di langkah pertama yang gagal
/// agar perbaikan terfokus pada akar masalah).
pub async fn run_verify_plan(cwd: &Path, scope: &str, timeout_secs: u64) -> Result<VerifyReport> {
    let plan = detect_project_plan(cwd, scope);
    if plan.steps.is_empty() {
        return Err(anyhow!(
            "tidak ada perintah validasi otomatis untuk proyek '{}' (scope='{scope}'). Jalankan `run_shell` secara eksplisit.",
            plan.kind.label()
        ));
    }

    let mut steps_summary = Vec::new();
    let mut diagnostics = Vec::new();
    let mut all_ok = true;
    let mut raw_tail = String::new();

    for step in &plan.steps {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &step.command]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &step.command]);
            c
        };
        cmd.current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);

        let (code, output) = run_timed(&mut cmd, timeout_secs.max(10))
            .await
            .map_err(|e| anyhow!("verifikasi '{}': {e}", step.name))?;
        let ok = code == Some(0);
        steps_summary.push((step.name.clone(), ok, code));

        if !ok {
            all_ok = false;
            diagnostics = parse_diagnostics(cwd, &output);
            let tail_lines: Vec<&str> = output.lines().rev().take(35).collect();
            let joined = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
            raw_tail = truncate_output(joined, MAX_TOOL_OUTPUT / 3);
            break;
        }
    }

    Ok(VerifyReport {
        kind: plan.kind,
        passed: all_ok,
        steps_summary,
        diagnostics,
        raw_tail,
    })
}

/// Tool `verify_project`: jalankan check/test/lint otomatis & kembalikan diagnostik terstruktur.
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
            description: "Deteksi otomatis toolchain proyek (Rust/Node/Python/Go/Make) lalu jalankan pemeriksaan aman (`check`, `test`, `lint`, atau `all`) dan ekstrak error terstruktur (file, baris, kode, snippet) untuk perbaikan cepat.".into(),
            parameters: schema(
                json!({
                    "scope": {
                        "type": "string",
                        "enum": ["check", "test", "lint", "all"],
                        "description": "Cakupan verifikasi: check (cepat), test, lint, atau all (default)."
                    }
                }),
                &[],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let scope = str_arg(&args, "scope").unwrap_or("all");
        let report = run_verify_plan(&ctx.cwd, scope, self.timeout_secs).await?;
        Ok(report.format_for_agent())
    }
}
