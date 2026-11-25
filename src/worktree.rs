//! Fase 6 — Transactional Git Worktree & Safe Rollback.
//!
//! Menyediakan isolasi kerja besar menggunakan Git Worktree di
//! `.selencode/worktrees/<id>` dengan branch khusus `selen/wt-<id>` sehingga:
//! - Perubahan pengguna yang belum disimpan di workspace utama TIDAK tersentuh.
//! - Agent/user dapat meninjau preview diff & mendeteksi konflik file kotor
//!   pengguna sebelum menerapkan (`apply`) ke workspace utama.
//! - Penerapan (`apply`) menolak menimpa file yang sedang diubah manual oleh
//!   pengguna, serta membuka `Checkpoint` pada `UndoStack` sehingga seluruh
//!   perubahan worktree tetap bisa di-`/rollback` kapan saja.

use crate::security::{is_secret_file, redact_secrets, resolve_in_sandbox};
use crate::tools::undo::{FileSnapshot, UndoStack};
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

const WORKTREE_BRANCH_PREFIX: &str = "selen/wt-";
const MAX_DIFF_PREVIEW_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub id: String,
    pub branch: String,
    pub path: PathBuf,
    pub base_commit: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeDiffPreview {
    pub id: String,
    pub branch: String,
    pub changed_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub user_dirty_conflicts: Vec<String>,
    pub diff_text: String,
}

#[derive(Debug, Clone)]
pub struct ApplyWorktreeReport {
    pub id: String,
    pub checkpoint_id: usize,
    pub applied_files: Vec<String>,
    pub deleted_files: Vec<String>,
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("gagal menjalankan `git {}`", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    Ok((out.status.success(), text))
}

/// Cek apakah `cwd` berada dalam repository Git yang valid dan memiliki minimal 1 commit.
pub fn is_git_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .map(|(ok, out)| ok && out.trim() == "true")
        .unwrap_or(false)
}

fn sanitize_label(label: &str) -> String {
    let mut s: String = label
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let trimmed = s.trim_matches('-');
    let short: String = trimmed.chars().take(24).collect();
    if short.is_empty() {
        let uid = &uuid::Uuid::new_v4().simple().to_string()[..6];
        format!("task-{uid}")
    } else {
        short
    }
}

fn worktrees_root(cwd: &Path) -> PathBuf {
    cwd.join(".selencode").join("worktrees")
}

/// Buat Git Worktree terisolasi baru untuk refactor/tugas besar.
pub fn create_isolated_worktree(cwd: &Path, label: &str) -> Result<WorktreeInfo> {
    if !is_git_repo(cwd) {
        return Err(anyhow!(
            "folder ini bukan repository Git — Git Worktree membutuhkan repository Git aktif"
        ));
    }
    let (ok_head, head_out) = run_git(cwd, &["rev-parse", "HEAD"])?;
    if !ok_head {
        return Err(anyhow!(
            "repository Git belum memiliki commit HEAD (buat commit awal dulu sebelum memakai worktree)"
        ));
    }
    let base_commit = head_out.trim().to_string();
    let slug = sanitize_label(label);
    let uid = &uuid::Uuid::new_v4().simple().to_string()[..4];
    let id = format!("{slug}-{uid}");
    let branch = format!("{WORKTREE_BRANCH_PREFIX}{id}");
    let wt_dir = worktrees_root(cwd).join(&id);
    if let Some(parent) = wt_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let wt_str = wt_dir.to_string_lossy().to_string();
    let (ok, out) = run_git(cwd, &["worktree", "add", "-b", &branch, &wt_str, "HEAD"])?;
    if !ok {
        return Err(anyhow!("gagal membuat Git Worktree '{id}': {}", out.trim()));
    }
    Ok(WorktreeInfo {
        id,
        branch,
        path: wt_dir,
        base_commit,
    })
}

/// Daftar seluruh Git Worktree aktif milik SelenCode (`selen/wt-*`).
pub fn list_worktrees(cwd: &Path) -> Result<Vec<WorktreeInfo>> {
    if !is_git_repo(cwd) {
        return Ok(Vec::new());
    }
    let (ok, out) = run_git(cwd, &["worktree", "list", "--porcelain"])?;
    if !ok {
        return Ok(Vec::new());
    }
    let mut list = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head = String::new();
    let mut cur_branch = String::new();

    let flush = |p: &mut Option<PathBuf>,
                 head: &mut String,
                 branch: &mut String,
                 out_vec: &mut Vec<WorktreeInfo>| {
        if let Some(path) = p.take() {
            let short_branch = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch.as_str())
                .to_string();
            if let Some(id) = short_branch.strip_prefix(WORKTREE_BRANCH_PREFIX) {
                out_vec.push(WorktreeInfo {
                    id: id.to_string(),
                    branch: short_branch,
                    path,
                    base_commit: head.clone(),
                });
            }
        }
        head.clear();
        branch.clear();
    };

    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            flush(&mut cur_path, &mut cur_head, &mut cur_branch, &mut list);
            continue;
        }
        if let Some(rest) = l.strip_prefix("worktree ") {
            flush(&mut cur_path, &mut cur_head, &mut cur_branch, &mut list);
            cur_path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = l.strip_prefix("HEAD ") {
            cur_head = rest.trim().to_string();
        } else if let Some(rest) = l.strip_prefix("branch ") {
            cur_branch = rest.trim().to_string();
        }
    }
    flush(&mut cur_path, &mut cur_head, &mut cur_branch, &mut list);
    Ok(list)
}

fn find_worktree(cwd: &Path, id: &str) -> Result<WorktreeInfo> {
    let clean = id.trim();
    list_worktrees(cwd)?
        .into_iter()
        .find(|w| w.id == clean || w.branch == clean)
        .ok_or_else(|| anyhow!("worktree SelenCode '{clean}' tidak ditemukan"))
}

fn parse_porcelain_paths(status_out: &str) -> (Vec<String>, Vec<String>) {
    let mut modified_or_new = Vec::new();
    let mut deleted = Vec::new();
    for raw in status_out.lines() {
        if raw.len() < 4 {
            continue;
        }
        let code = &raw[..2];
        let path_part = raw[3..].trim();
        let rel = path_part
            .rsplit(" -> ")
            .next()
            .unwrap_or(path_part)
            .trim_matches('"')
            .replace('\\', "/");
        if rel.is_empty() || rel.starts_with(".selencode/") {
            continue;
        }
        if code.contains('D') {
            if !deleted.contains(&rel) {
                deleted.push(rel);
            }
        } else if !modified_or_new.contains(&rel) {
            modified_or_new.push(rel);
        }
    }
    (modified_or_new, deleted)
}

/// Pratinjau diff perubahan pada worktree `id` + deteksi konflik dengan file yang sedang
/// diedit manual oleh pengguna di workspace utama.
pub fn preview_diff(cwd: &Path, id: &str) -> Result<WorktreeDiffPreview> {
    let wt = find_worktree(cwd, id)?;

    // Perubahan di dalam worktree (belum commit maupun sudah commit di branch worktree).
    let (_, wt_status) = run_git(&wt.path, &["status", "--porcelain"])?;
    let (mut changed_files, mut deleted_files) = parse_porcelain_paths(&wt_status);

    // Juga cek commit yang mungkin dibuat di branch worktree terhadap HEAD utama.
    let (ok_nm, nm_out) = run_git(&wt.path, &["diff", "--name-status", "HEAD"])?;
    if ok_nm {
        for line in nm_out.lines() {
            let mut parts = line.splitn(2, '\t');
            let st = parts.next().unwrap_or("").trim();
            let rel = parts.next().unwrap_or("").trim().replace('\\', "/");
            if rel.is_empty() {
                continue;
            }
            if st.starts_with('D') {
                if !deleted_files.contains(&rel) {
                    deleted_files.push(rel);
                }
            } else if !changed_files.contains(&rel) {
                changed_files.push(rel);
            }
        }
    }

    // Cek perubahan pengguna yang belum di-commit di workspace utama.
    let (_, main_status) = run_git(cwd, &["status", "--porcelain"])?;
    let (main_mod, main_del) = parse_porcelain_paths(&main_status);
    let main_dirty: HashSet<String> = main_mod.into_iter().chain(main_del).collect();

    let mut conflicts = Vec::new();
    for f in changed_files.iter().chain(deleted_files.iter()) {
        if main_dirty.contains(f) && !conflicts.contains(f) {
            conflicts.push(f.clone());
        }
    }

    let (_, raw_diff) = run_git(&wt.path, &["diff"])?;
    let mut diff_text = redact_secrets(&raw_diff);
    if diff_text.len() > MAX_DIFF_PREVIEW_BYTES {
        let mut cut = MAX_DIFF_PREVIEW_BYTES;
        while cut > 0 && !diff_text.is_char_boundary(cut) {
            cut -= 1;
        }
        diff_text.truncate(cut);
        diff_text.push_str("\n... [diff dipotong 24KB]");
    }

    Ok(WorktreeDiffPreview {
        id: wt.id,
        branch: wt.branch,
        changed_files,
        deleted_files,
        user_dirty_conflicts: conflicts,
        diff_text,
    })
}

/// Terapkan perubahan dari worktree `id` ke workspace utama secara transaksional:
/// - Menolak jika ada konflik dengan file kotor pengguna (tidak menimpa pekerjaan user!).
/// - Membuka `Checkpoint` pada `UndoStack` & mencatat snapshot setiap file yang diterapkan.
pub fn apply_worktree_to_main(
    cwd: &Path,
    id: &str,
    undo_stack: &Mutex<UndoStack>,
) -> Result<ApplyWorktreeReport> {
    let wt = find_worktree(cwd, id)?;
    let preview = preview_diff(cwd, &wt.id)?;
    if !preview.user_dirty_conflicts.is_empty() {
        return Err(anyhow!(
            "penerapan worktree '{}' dibatalkan demi keamanan: file berikut juga memiliki perubahan belum tersimpan oleh pengguna di workspace utama: {}",
            wt.id,
            preview.user_dirty_conflicts.join(", ")
        ));
    }
    if preview.changed_files.is_empty() && preview.deleted_files.is_empty() {
        return Err(anyhow!(
            "tidak ada perubahan di worktree '{}' untuk diterapkan",
            wt.id
        ));
    }

    let cp_id = undo_stack
        .lock()
        .expect("undo_stack poisoned")
        .begin_checkpoint(format!("apply worktree {}", wt.id));

    let mut applied_files = Vec::new();
    for rel in &preview.changed_files {
        let src = resolve_in_sandbox(&wt.path, Path::new(rel))?;
        let dst = resolve_in_sandbox(cwd, Path::new(rel))?;
        if is_secret_file(&src) || is_secret_file(&dst) || !src.is_file() {
            continue;
        }
        let after = std::fs::read(&src)?;
        let before = std::fs::read(&dst).ok();
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, &after)?;
        if let Ok(mut st) = undo_stack.lock() {
            st.record(FileSnapshot {
                path: dst,
                rel: rel.clone(),
                before,
                after,
            });
        }
        applied_files.push(rel.clone());
    }

    let mut deleted_files = Vec::new();
    for rel in &preview.deleted_files {
        let dst = resolve_in_sandbox(cwd, Path::new(rel))?;
        if is_secret_file(&dst) || !dst.is_file() {
            continue;
        }
        let before = std::fs::read(&dst).ok();
        std::fs::remove_file(&dst)?;
        if let Ok(mut st) = undo_stack.lock() {
            st.record(FileSnapshot {
                path: dst,
                rel: rel.clone(),
                before,
                after: Vec::new(),
            });
        }
        deleted_files.push(rel.clone());
    }

    Ok(ApplyWorktreeReport {
        id: wt.id,
        checkpoint_id: cp_id,
        applied_files,
        deleted_files,
    })
}

/// Hapus worktree terisolasi milik SelenCode beserta branch sementaranya (`selen/wt-*`).
pub fn discard_worktree(cwd: &Path, id: &str) -> Result<()> {
    let wt = find_worktree(cwd, id)?;
    let path_str = wt.path.to_string_lossy().to_string();
    let (ok, out) = run_git(cwd, &["worktree", "remove", "--force", &path_str])?;
    if !ok {
        // Fallback: bersihkan folder + prune worktree metadata bila folder sudah hilang.
        let _ = std::fs::remove_dir_all(&wt.path);
        let _ = run_git(cwd, &["worktree", "prune"]);
        if wt.path.exists() {
            return Err(anyhow!(
                "gagal menghapus worktree '{}': {}",
                wt.id,
                out.trim()
            ));
        }
    }
    let _ = run_git(cwd, &["branch", "-D", &wt.branch]);
    Ok(())
}

/// Jalankan verifikasi proyek (build / test / lint) secara langsung DI DALAM worktree terisolasi
/// sebelum perubahan diterapkan ke workspace utama.
pub async fn verify_worktree(
    cwd: &Path,
    id: &str,
    scope: &str,
    timeout_secs: u64,
    sandbox: &crate::sandbox::SandboxController,
) -> Result<crate::tools::verify::VerifyReport> {
    let wt = find_worktree(cwd, id)?;
    let prev = preview_diff(cwd, &wt.id)?;
    crate::tools::verify::run_verify_plan_with_sandbox(
        &wt.path,
        scope,
        &prev.changed_files,
        timeout_secs,
        sandbox,
    )
    .await
}
