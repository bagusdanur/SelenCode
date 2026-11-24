use super::{run_timed, schema, str_arg, truncate_output, Ctx, Tool, MAX_TOOL_OUTPUT};
use crate::providers::ToolSpec;
use crate::security::is_forbidden_command;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// run_shell — jalankan perintah shell di cwd project.
/// Approval wajib KECUALI perintah cocok allowlist config [shell].
/// Pola destruktif (security::is_forbidden_command) selalu ditolak.
pub struct RunShell {
    pub allowlist: Vec<String>,
    pub timeout_secs: u64,
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
    /// tapi allowlist "ls" TIDAK cocok "lsof").
    fn is_allowlisted(&self, command: &str) -> bool {
        let c = command.trim();
        self.allowlist.iter().any(|a| {
            let a = a.trim();
            !a.is_empty()
                && (c == a
                    || c.strip_prefix(a)
                        .map(|rest| rest.starts_with(|ch: char| ch.is_whitespace()))
                        .unwrap_or(false))
        })
    }
}

#[async_trait]
impl Tool for RunShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".into(),
            description: "Jalankan perintah shell di folder project (timeout sesuai config, output maks 30KB). Perintah di luar allowlist butuh persetujuan user; exit code non-nol dilaporkan apa adanya.".into(),
            parameters: schema(
                json!({"command": {"type": "string", "description": "perintah shell lengkap"}}),
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
        if self.needs_approval(&args)
            && !ctx
                .approver
                .ask(&format!("run_shell: {command}"), "").await
        {
            return Ok(format!(
                "Ditolak user — perintah TIDAK dijalankan: {command}"
            ));
        }

        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &command]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &command]);
            c
        };
        cmd.current_dir(&ctx.cwd).kill_on_drop(true);

        let (code, output) = run_timed(&mut cmd, self.timeout_secs)
            .await
            .map_err(|e| anyhow!("perintah '{command}': {e}"))?;
        let output = truncate_output(output, MAX_TOOL_OUTPUT);
        // Exit non-nol BUKAN error fatal: model perlu tahu (mis. test gagal).
        Ok(format!(
            "exit code: {}\n{output}",
            code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
        ))
    }
}
