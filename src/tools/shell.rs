use super::{run_timed, schema, str_arg, truncate_output, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::is_forbidden_command;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Batas aman tangkapan output mentah shell di memori (512 KB) agar output besar seperti
/// 60KB+ tetap utuh saat masuk ke tahap spillover arsip di `agent/loop.rs` tanpa pernah hilang.
const MAX_SHELL_CAPTURE: usize = 512 * 1024;

/// run_shell — jalankan perintah shell di cwd project.
/// Approval wajib KECUALI perintah cocok allowlist config [shell].
/// Pola destruktif (security::is_forbidden_command) selalu ditolak.
pub struct RunShell {
    pub allowlist: Vec<String>,
    pub timeout_secs: u64,
}

/// Perintah baca-saja / cek versi bawaan yang aman berjalan tanpa mengganggu
/// user dengan popup approval berulang (tetap menolak chaining/redirect/flag berbahaya!).
/// `cargo check`/`cargo test` SENGAJA tidak dimasukkan agar kode repo (`build.rs`/proc-macro/test)
/// tidak pernah berjalan tanpa persetujuan.
const SAFE_READONLY_CMDS: &[&str] = &[
    "ls",
    "dir",
    "pwd",
    "whoami",
    "uname",
    "git status",
    "git diff",
    "git log",
    "git branch",
    "git show",
    "rustc --version",
    "cargo --version",
    "node -v",
    "node --version",
    "python --version",
    "python3 --version",
];

/// Tolak flag/argumen berbahaya pada perintah baca-saja bawaan (seperti `git diff --no-index`,
/// `git diff --output=...`, `git branch -D`, `..`, atau ekspansi env `%VAR%`/`$VAR`).
fn has_dangerous_readonly_args(c: &str) -> bool {
    if c.contains('$') || c.contains('%') {
        return true;
    }
    let tokens: Vec<&str> = c.split_whitespace().collect();
    for tok in &tokens {
        let low = tok.to_ascii_lowercase();
        if low == ".."
            || low.starts_with("../")
            || low.contains("/../")
            || low.contains(r"\..\")
            || low == "--output"
            || low.starts_with("--output=")
            || low == "--no-index"
            || low == "--ext-diff"
            || low == "--textconv"
            || low == "--config"
            || low.starts_with("--config=")
            || low == "-c"
            || low == "--exec"
            || low.starts_with("--exec=")
            || low == "--upload-pack"
            || low.starts_with("--upload-pack=")
        {
            return true;
        }
    }
    if tokens.len() >= 2 && tokens[0].eq_ignore_ascii_case("git") && tokens[1].eq_ignore_ascii_case("branch") {
        for tok in &tokens[2..] {
            if matches!(
                *tok,
                "-d" | "-D" | "-f" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--force" | "--move" | "--copy" | "--set-upstream-to"
            ) || tok.starts_with("--delete=")
                || tok.starts_with("--move=")
            {
                return true;
            }
        }
    }
    false
}

fn matches_prefix(c: &str, entry: &str) -> bool {
    let a = entry.trim();
    !a.is_empty()
        && (c == a
            || c.strip_prefix(a)
                .map(|rest| rest.starts_with(|ch: char| ch.is_whitespace()))
                .unwrap_or(false))
}

impl RunShell {
    pub fn new(allowlist: Vec<String>, timeout_secs: u64) -> Self {
        Self {
            allowlist,
            timeout_secs: if timeout_secs == 0 { 120 } else { timeout_secs },
        }
    }

    /// Cocok allowlist = perintah sama persis dengan entri, atau diawali
    /// entri + spasi (mis. allowlist "git status" cocok "git status --short",
    /// tapi allowlist "ls" TIDAK cocok "lsof"). Perintah yang mengandung
    /// operator chaining/subshell/redirect (`;`, `|`, `&`, `` ` ``, `$(`,
    /// `>`, `<`, newline) TIDAK pernah lolos allowlist agar tidak bisa
    /// disusupi perintah tambahan tanpa approval.
    fn is_allowlisted(&self, command: &str) -> bool {
        let c = command.trim();
        if c.contains([';', '|', '&', '`', '>', '<', '\n', '\r']) || c.contains("$(") {
            return false;
        }
        if self.allowlist.iter().any(|a| matches_prefix(c, a)) {
            return true;
        }
        SAFE_READONLY_CMDS.iter().any(|a| matches_prefix(c, a)) && !has_dangerous_readonly_args(c)
    }
}

#[async_trait]
impl Tool for RunShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Jalankan perintah shell di workspace.".into(),
            parameters: schema(
                json!({"command": {"type": "string"}}),
                &["command"],
            ),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        match str_arg(args, "command") {
            Some(cmd) => !self.is_allowlisted(cmd),
            None => true,
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let command = str_arg(&args, "command")
            .ok_or_else(|| anyhow!("argumen 'command' wajib"))?
            .trim()
            .to_string();
        if command.is_empty() {
            return Err(anyhow!("perintah kosong"));
        }
        if is_forbidden_command(&command) {
            return Err(anyhow!(
                "perintah ditolak demi keamanan (pola terlarang, tidak bisa di-approve): {command}"
            ));
        }
        // Cek batas sandbox lebih dulu sebelum minta approval agar tombol `[a]` tidak bocor saat ditolak sandbox.
        let mut cmd = ctx
            .sandbox
            .prepare_shell_command("run_shell", &command, &ctx.cwd)?;
        if self.needs_approval(&args)
            && !ctx
                .approver
                .ask(&format!("run_shell: {command}"), "").await
        {
            return Ok(format!(
                "Ditolak user — perintah TIDAK dijalankan: {command}"
            ));
        }

        let (code, output) = run_timed(&mut cmd, self.timeout_secs)
            .await
            .map_err(|e| anyhow!("perintah '{command}': {e}"))?;
        let output = truncate_output(output, MAX_SHELL_CAPTURE);
        // Exit non-nol BUKAN error fatal: model perlu tahu (mis. test gagal).
        Ok(format!(
            "exit code: {}\n{output}",
            code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
        ))
    }
}
