//! Custom Hooks (`PreToolUse` & `PostToolUse`) ala Claude Code.
//!
//! Memungkinkan pengguna memasang hook di `selencode.toml`:
//! ```toml
//! [[hooks]]
//! event = "post_tool"
//! matcher = "edit_file"
//! command = "cargo fmt"
//! ```
//! Setiap hook berjalan di bawah `SandboxController` (scrub env var sensitif,
//! timeout ketat, `kill_on_drop`), dan menerima konteks via env:
//! - `SELENCODE_HOOK_EVENT` (`pre_tool` / `post_tool`)
//! - `SELENCODE_TOOL_NAME`
//! - `SELENCODE_TOOL_ARGS` (ringkas, teredaksi rahasia)

use crate::config::HookCfg;
use crate::sandbox::SandboxController;
use crate::security::redact_secrets;
use anyhow::{anyhow, Result};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct HookEngine {
    hooks: Vec<HookCfg>,
}

impl HookEngine {
    pub fn from_config(hooks: &[HookCfg]) -> Self {
        Self {
            hooks: hooks.to_vec(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    fn matching(&self, event: &str, tool_name: &str) -> Vec<&HookCfg> {
        self.hooks
            .iter()
            .filter(|h| {
                h.event.eq_ignore_ascii_case(event)
                    && !h.command.trim().is_empty()
                    && matcher_hits(&h.matcher, tool_name)
            })
            .collect()
    }

    /// Jalankan seluruh hook `pre_tool` yang cocok.
    /// Jika sebuah hook memiliki `block_on_error = true` dan gagal (exit != 0),
    /// kembalikan `Err(...)` agar eksekusi tool diblokir aman.
    pub async fn run_pre_tool(
        &self,
        tool_name: &str,
        args_json: &str,
        cwd: &Path,
        sandbox: &SandboxController,
    ) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for h in self.matching("pre_tool", tool_name) {
            let (ok, output) = run_single_hook(h, "pre_tool", tool_name, args_json, false, cwd, sandbox).await;
            if !ok && h.block_on_error {
                return Err(anyhow!(
                    "diblokir oleh hook pre_tool ('{}'): {}",
                    h.command,
                    output.trim()
                ));
            }
            if !output.trim().is_empty() {
                notes.push(format!(
                    "[hook pre_tool {}] {}",
                    if ok { "OK" } else { "WARN" },
                    output.trim()
                ));
            }
        }
        Ok(notes)
    }

    /// Jalankan seluruh hook `post_tool` yang cocok (mis. auto-format setelah `edit_file`).
    pub async fn run_post_tool(
        &self,
        tool_name: &str,
        args_json: &str,
        tool_is_error: bool,
        cwd: &Path,
        sandbox: &SandboxController,
    ) -> Vec<String> {
        let mut notes = Vec::new();
        for h in self.matching("post_tool", tool_name) {
            let (ok, output) =
                run_single_hook(h, "post_tool", tool_name, args_json, tool_is_error, cwd, sandbox)
                    .await;
            if !output.trim().is_empty() {
                notes.push(format!(
                    "[hook post_tool {}] {}",
                    if ok { "OK" } else { "WARN" },
                    output.trim()
                ));
            }
        }
        notes
    }
}

/// Cocokkan pola matcher hook: `*` (semua), nama persis, daftar pisah koma (`edit_file,write_file`),
/// atau prefix wildcard (`mcp__*`).
pub fn matcher_hits(matcher: &str, tool_name: &str) -> bool {
    let m = matcher.trim();
    if m.is_empty() || m == "*" {
        return true;
    }
    for part in m.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if part == "*" || part.eq_ignore_ascii_case(tool_name) {
            return true;
        }
        if let Some(prefix) = part.strip_suffix('*') {
            if tool_name
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
            {
                return true;
            }
        }
    }
    false
}

async fn run_single_hook(
    hook: &HookCfg,
    event: &str,
    tool_name: &str,
    args_json: &str,
    tool_is_error: bool,
    cwd: &Path,
    sandbox: &SandboxController,
) -> (bool, String) {
    // Jangan patahkan tool baca-saja saat sandbox dalam mode `read-only` hanya karena shell hook aktif.
    if sandbox.mode() == crate::sandbox::SandboxMode::ReadOnly {
        return (true, "hook dilewati saat sandbox mode read-only".to_string());
    }
    let mut cmd = match sandbox.prepare_shell_command(&format!("hook:{event}"), &hook.command, cwd) {
        Ok(c) => c,
        Err(e) => return (false, format!("sandbox menolak hook: {e}")),
    };
    let safe_args: String = redact_secrets(args_json).chars().take(1024).collect();
    cmd.env("SELENCODE_HOOK_EVENT", event)
        .env("SELENCODE_TOOL_NAME", tool_name)
        .env("SELENCODE_TOOL_ARGS", safe_args)
        .env(
            "SELENCODE_TOOL_IS_ERROR",
            if tool_is_error { "1" } else { "0" },
        );
    let timeout_secs = hook.timeout_secs.clamp(1, 120);
    match crate::tools::run_timed(&mut cmd, timeout_secs).await {
        Ok((code, combined)) => {
            let safe: String = redact_secrets(&combined).chars().take(500).collect();
            (code == Some(0), safe)
        }
        Err(e) => (false, format!("gagal menjalankan hook: {e}")),
    }
}
