//! Tes Fase 2: tool git (status/diff/commit) di repo temp sungguhan.
//! Bila git tidak tersedia di PATH, tes dilewati dengan pesan (bukan gagal).
use selencode::tools::git::{GitCommit, GitDiff, GitStatus};
use selencode::tools::{Ctx, Tool};
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &PathBuf, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} gagal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selencode-git-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[tokio::test]
async fn git_tools_roundtrip() {
    if !git_available() {
        eprintln!("git tidak tersedia di PATH — tes git dilewati");
        return;
    }
    let dir = temp_dir("roundtrip");
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "tes@selencode.local"]);
    git(&dir, &["config", "user.name", "Selencode Test"]);
    std::fs::write(dir.join("a.txt"), "satu\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "awal"]);

    let ctx = Ctx::new(dir.clone(), true);

    // Status bersih setelah commit awal
    let st = GitStatus.run(json!({}), &ctx).await.unwrap();
    assert!(st.contains("tidak ada perubahan"), "{st}");
    assert!(!st.contains("a.txt"), "{st}");

    // Ubah file -> status & diff memuat perubahan
    std::fs::write(dir.join("a.txt"), "satu\ndua\n").unwrap();
    let st2 = GitStatus.run(json!({}), &ctx).await.unwrap();
    assert!(st2.contains("a.txt"), "{st2}");
    let diff = GitDiff.run(json!({}), &ctx).await.unwrap();
    assert!(diff.contains("dua"), "{diff}");

    // Commit lewat tool -> tercatat di log, status bersih lagi
    let res = GitCommit
        .run(json!({"message": "tambah baris dua"}), &ctx)
        .await
        .unwrap();
    assert!(res.contains("exit code: 0"), "{res}");
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("tambah baris dua"), "{log}");
    let st3 = GitStatus.run(json!({}), &ctx).await.unwrap();
    assert!(st3.contains("tidak ada perubahan"), "{st3}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn git_tools_error_outside_repo() {
    if !git_available() {
        eprintln!("git tidak tersedia di PATH — tes git dilewati");
        return;
    }
    let dir = temp_dir("norepo");
    let ctx = Ctx::new(dir.clone(), true);
    let err = GitStatus.run(json!({}), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("bukan repo git"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
