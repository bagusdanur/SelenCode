use super::{run_timed, schema, str_arg, truncate_output, Ctx, Tool, MAX_TOOL_OUTPUT};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

const GIT_TIMEOUT: u64 = 30;

/// Jalankan git dengan argumen terpisah (BUKAN string shell — aman dari
/// injeksi lewat argumen seperti pesan commit), env sensitif di-scrub, serta
/// menonaktifkan hook/fsmonitor/diff.external repo tak tepercaya.
async fn git_cmd_with_opts(
    ctx: &Ctx,
    args: &[&str],
    disable_repo_hooks: bool,
) -> Result<(Option<i32>, String)> {
    let mut cmd = tokio::process::Command::new("git");
    if disable_repo_hooks {
        cmd.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.external=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.pager=cat",
        ]);
    }
    cmd.args(args)
        .current_dir(&ctx.cwd)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    if ctx.sandbox.scrub_env_enabled() {
        crate::sandbox::scrub_tokio_command(&mut cmd);
    }
    run_timed(&mut cmd, GIT_TIMEOUT)
        .await
        .map_err(|e| anyhow!("git {}: {e}", args.first().copied().unwrap_or("")))
}

async fn git_cmd(ctx: &Ctx, args: &[&str]) -> Result<(Option<i32>, String)> {
    git_cmd_with_opts(ctx, args, true).await
}

/// Pastikan cwd berada di dalam repo git; error jelas bila bukan.
async fn ensure_repo(ctx: &Ctx) -> Result<()> {
    let (code, out) = git_cmd(ctx, &["rev-parse", "--is-inside-work-tree"]).await?;
    if code != Some(0) || !out.trim().starts_with("true") {
        return Err(anyhow!(
            "bukan repo git: folder {} tidak berada di dalam repo git (info saja — JANGAN jalankan `git init` kecuali diminta user)",
            ctx.cwd.display()
        ));
    }
    Ok(())
}

// ---------- git_status ----------
pub struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_status".into(),
            description: "Status repo git (git status --short --branch). Tanpa persetujuan.".into(),
            parameters: schema(json!({}), &[]),
        }
    }
    async fn run(&self, _args: Value, ctx: &Ctx) -> Result<String> {
        ensure_repo(ctx).await?;
        let (_code, out) = git_cmd(ctx, &["status", "--short", "--branch"]).await?;
        let out = out.trim_end().to_string();
        if out.lines().count() <= 1 {
            return Ok(format!("{out}\n(tidak ada perubahan)"));
        }
        Ok(out)
    }
}

// ---------- git_diff ----------
pub struct GitDiff;

#[async_trait]
impl Tool for GitDiff {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_diff".into(),
            description: "Diff perubahan belum commit + yang sudah staged (maks 30KB). Tanpa persetujuan.".into(),
            parameters: schema(json!({}), &[]),
        }
    }
    async fn run(&self, _args: Value, ctx: &Ctx) -> Result<String> {
        ensure_repo(ctx).await?;
        let (_c1, unstaged) = git_cmd(ctx, &["diff", "--no-ext-diff", "--no-textconv"]).await?;
        let (_c2, staged) =
            git_cmd(ctx, &["diff", "--staged", "--no-ext-diff", "--no-textconv"]).await?;
        if unstaged.trim().is_empty() && staged.trim().is_empty() {
            return Ok("Tidak ada perubahan (diff kosong).".into());
        }
        let combined = format!(
            "=== diff (belum staged) ===\n{unstaged}\n=== diff --staged ===\n{staged}"
        );
        Ok(truncate_output(combined, MAX_TOOL_OUTPUT))
    }
}

// ---------- git_commit ----------
pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git_commit".into(),
            description: "git add -A lalu commit dengan pesan yang diberikan. Butuh persetujuan user. TIDAK PERNAH push.".into(),
            parameters: schema(
                json!({"message": {"type": "string", "description": "pesan commit"}}),
                &["message"],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let message = str_arg(&args, "message")
            .ok_or_else(|| anyhow!("argumen 'message' wajib"))?
            .to_string();
        ensure_repo(ctx).await?;
        ctx.sandbox.check_write_allowed("git_commit", &message)?;
        if !ctx
            .approver
            .ask(&format!("git_commit: {message}"), "git add -A && git commit").await
        {
            return Ok("Ditolak user — commit TIDAK dibuat.".into());
        }
        let (acode, aout) = git_cmd(ctx, &["add", "-A"]).await?;
        if acode != Some(0) {
            return Ok(format!("git add gagal (exit code: {acode:?}):\n{aout}"));
        }
        // Argumen terpisah: pesan commit tidak pernah lewat string shell.
        let (code, out) = git_cmd(ctx, &["commit", "-m", &message]).await?;
        Ok(format!(
            "exit code: {}\n{}",
            code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
            truncate_output(out, MAX_TOOL_OUTPUT)
        ))
    }
}
