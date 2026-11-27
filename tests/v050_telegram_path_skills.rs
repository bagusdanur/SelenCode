//! Tes penerimaan v0.5.0:
//! 1. Pengiriman Telegram & jeda progres saat approval (`TgApprovalHub::has_pending_for_chat`)
//! 2. PATH sesi `run_shell` (`enriched_shell_path` mencakup ~/.local/bin, ~/bin, $PREFIX/bin tanpa duplikat)
//! 3. Perintah "buat skill" saat skill sudah ada (`skill_edit(..., ref_path="references/...")` + diff + approval)
//! 4. Tampilan Telegram rapi & hemat emoji (maks 1 emoji/pesan, perintah approval 100% utuh tanpa dipotong)

use selencode::agent::context::build_system_prompt;
use selencode::sandbox::enriched_shell_path;
use selencode::security::{ApprovalMode, ApprovalPolicy, ApprovalRequest, Approver};
use selencode::telegram::{
    format_approval_card_html, format_telegram_progress_html, markdown_to_telegram_html,
    menu_markup_to_telegram_html, PendingTgApproval, TgApprovalHub, APPROVAL_TIMEOUT_SECS,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

fn count_decorative_emojis(s: &str) -> usize {
    const EMOJIS: &[char] = &[
        '🌙', '🔐', '✅', '❌', '⚡', '🔧', '📌', '💡', '✨', '💬', '🛑', '🔒', '⚙', '💭', '⏰',
    ];
    s.chars().filter(|c| EMOJIS.contains(c)).count()
}

#[test]
fn perbaikan1_and_4_telegram_clean_style_emoji_budget_and_full_approval_command() {
    // 1. Gelembung progres: 0 emoji dekoratif, format polos rapi.
    let p_tool = format_telegram_progress_html(3, 25, Some("run_shell"), 2, "");
    assert_eq!(count_decorative_emojis(&p_tool), 0, "{p_tool}");
    assert!(p_tool.contains("Sedang bekerja"));
    assert!(p_tool.contains("langkah 3/25"));
    assert!(p_tool.contains("<code>run_shell</code>"));

    let p_stream = format_telegram_progress_html(4, 25, None, 2, "Hasil ringkas <ok>");
    assert_eq!(count_decorative_emojis(&p_stream), 0, "{p_stream}");
    assert!(p_stream.contains("Mengetik..."));
    assert!(p_stream.contains("<blockquote>Hasil ringkas &lt;ok&gt;</blockquote>"));

    // 2. Kartu approval: perintah 2.500 karakter ditampilkan UTUH (TIDAK dipotong di 1.500 char!)
    //    dan hanya memakai maks 1 emoji penanda (⚠️).
    let long_cmd = format!(
        "run_shell: rawdl get \"Judul Panjang {}\" --all --end-marker-utuh",
        "A".repeat(2_200)
    );
    let (preamble, card_html, _plain) = format_approval_card_html(
        "run_shell",
        &long_cmd,
        "Menjalankan unduhan batch sesuai permintaan pengguna",
        APPROVAL_TIMEOUT_SECS,
    );
    assert!(preamble.is_empty(), "2500 char masih muat 1 kartu tanpa dipotong");
    assert!(card_html.contains("⚠️ Perlu persetujuan"));
    assert!(card_html.contains("--end-marker-utuh"), "ujung perintah TIDAK boleh terpotong!");
    assert!(card_html.contains("<b>Alasan:</b>"));
    assert!(card_html.contains("Batas waktu: 5 menit."));
    assert_eq!(count_decorative_emojis(&card_html), 0);

    // 3. Perintah raksasa (>4.000 char): tidak dipangkas diam-diam, melainkan dipecah ke preamble utuh!
    let huge_cmd = format!("run_shell: echo START_{}_END_RAKSASA", "Z".repeat(5_000));
    let (preamble2, card_html2, _) =
        format_approval_card_html("run_shell", &huge_cmd, "", APPROVAL_TIMEOUT_SECS);
    assert!(preamble2.len() >= 2, "harus dipecah ke pesan lanjutan utuh");
    let joined_pre = preamble2.join("\n");
    assert!(joined_pre.contains("START_"));
    assert!(joined_pre.contains("_END_RAKSASA"));
    assert!(card_html2.contains("pesan tepat di atas"));

    // 4. Markdown & menu markup Telegram: 0 emoji dekoratif (🌙 / 💡 / 📌 hilang).
    let md_html = markdown_to_telegram_html("# Judul Utama\n## Sub\n- butir satu");
    assert_eq!(count_decorative_emojis(&md_html), 0, "{md_html}");
    let menu_html = menu_markup_to_telegram_html("# Status\n* mode | ok\n! catatan");
    assert_eq!(count_decorative_emojis(&menu_html), 0, "{menu_html}");

    // 5. TgApprovalHub::has_pending_for_chat aktif saat menunggu dan bersih setelah dibatalkan.
    let hub = TgApprovalHub::new();
    let (tx, _rx) = oneshot::channel::<bool>();
    let pol = Arc::new(Mutex::new(ApprovalPolicy::new(ApprovalMode::Ask)));
    assert!(!hub.has_pending_for_chat(999));
    hub.register_for_test(
        999,
        PendingTgApproval {
            chat_id: 999,
            message_id: 10,
            base_text: "test".into(),
            category: "run_shell".into(),
            policy: pol,
            reply: tx,
        },
    );
    assert!(hub.has_pending_for_chat(999));
    assert!(!hub.has_pending_for_chat(888));
    assert_eq!(hub.cancel_chat_approvals(999), 1);
    assert!(!hub.has_pending_for_chat(999));
}

#[test]
fn perbaikan2_enriched_shell_path_adds_user_bins_without_duplicates() {
    let base_existing = PathBuf::from("/usr/bin");
    let home = PathBuf::from("/home/tester");
    let prefix = PathBuf::from("/data/data/com.termux/files/usr");

    let initial = std::env::join_paths([&base_existing, &home.join("bin")]).unwrap();
    let enriched = enriched_shell_path(Some(&initial), Some(&home), Some(&prefix));
    let parts: Vec<PathBuf> = std::env::split_paths(&enriched).collect();

    // Urutan awal dipertahankan
    assert_eq!(parts[0], base_existing);
    assert_eq!(parts[1], home.join("bin"));
    // ~/.local/bin dan $PREFIX/bin ditambahkan
    assert!(parts.contains(&home.join(".local").join("bin")));
    assert!(parts.contains(&prefix.join("bin")));
    // Tidak ada duplikat ~/bin
    let home_bin_count = parts.iter().filter(|p| *p == &home.join("bin")).count();
    assert_eq!(home_bin_count, 1, "PATH tidak boleh duplikat: {parts:?}");

    // Bila PATH kosong, tetap menambahkan direktori pengguna
    let from_empty = enriched_shell_path(Some(OsStr::new("")), Some(&home), None);
    let empty_parts: Vec<PathBuf> = std::env::split_paths(&from_empty).collect();
    assert!(empty_parts.contains(&home.join(".local").join("bin")));
}

#[tokio::test]
async fn perbaikan3_skill_edit_ref_path_writes_audit_reference_with_approval_and_diff() {
    let cwd = std::env::temp_dir().join(format!(
        "selen-v050-skillref-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let sk_dir = cwd.join(".selencode").join("skills").join("ops").join("raw-downloader");
    std::fs::create_dir_all(&sk_dir).unwrap();
    std::fs::write(
        sk_dir.join("SKILL.md"),
        "---\nname: raw-downloader\ndescription: Cara unduh RAW memakai rawdl\nversion: 1.0.0\n---\n\n# raw-downloader\nGunakan `rawdl sources` lalu `rawdl get`.\n",
    )
    .unwrap();

    // System prompt statis harus memuat aturan wajib tulis nyata + diff untuk perintah buat skill.
    let sys = build_system_prompt(&cwd, &[]);
    assert!(sys.contains("skill_edit"));
    assert!(sys.contains("ref_path"));
    assert!(sys.contains("diff ringkas"));

    let reg = default_registry();
    let edit_tool = reg.get("skill_edit").expect("skill_edit terdaftar");

    // Path traversal pada ref_path ditolak!
    let ctx_yes = Ctx::new(cwd.clone(), true);
    assert!(edit_tool
        .run(
            json!({
                "name": "raw-downloader",
                "ref_path": "../luar.md",
                "body": "tidak boleh"
            }),
            &ctx_yes
        )
        .await
        .is_err());
    assert!(edit_tool
        .run(
            json!({
                "name": "raw-downloader",
                "ref_path": "references/.env",
                "body": "tidak boleh"
            }),
            &ctx_yes
        )
        .await
        .is_err());

    // Penulisan file referensi sah WAJIB lewat persetujuan & menampilkan unified diff!
    let (tx, mut rx) = mpsc::channel::<ApprovalRequest>(4);
    let ctx_appr =
        Ctx::new(cwd.clone(), false).with_approver(Arc::new(Approver::channel(false, tx)));
    let appr_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("approval ref_path harus muncul");
        assert!(req.title.contains("skill_edit"));
        assert!(req.detail.contains("+Tanggal audit: 2026-11-26"));
        let _ = req.reply.send(true);
    });
    let out = edit_tool
        .run(
            json!({
                "name": "raw-downloader",
                "ref_path": "references/audit-rawdl.md",
                "body": "# Audit rawdl\nTanggal audit: 2026-11-26\nCakupan: verifikasi subperintah sources, search, get."
            }),
            &ctx_appr,
        )
        .await
        .unwrap();
    appr_task.await.unwrap();

    assert!(out.contains("berhasil disimpan"), "{out}");
    assert!(out.contains("+Tanggal audit: 2026-11-26"), "output harus memuat diff: {out}");
    let ref_file = sk_dir.join("references").join("audit-rawdl.md");
    assert!(ref_file.is_file());
    let saved = std::fs::read_to_string(&ref_file).unwrap();
    assert!(saved.contains("verifikasi subperintah sources"));

    // Pastikan `skill_view` Tingkat 3 langsung bisa membaca referensi yang baru ditulis!
    let view_tool = reg.get("skill_view").expect("skill_view terdaftar");
    let viewed = view_tool
        .run(
            json!({"name": "raw-downloader", "path": "references/audit-rawdl.md"}),
            &ctx_yes,
        )
        .await
        .unwrap();
    assert!(viewed.contains("Tanggal audit: 2026-11-26"));

    let _ = std::fs::remove_dir_all(&cwd);
}

#[cfg(unix)]
#[tokio::test]
async fn perbaikan2_run_shell_finds_executable_in_custom_path_and_scrubs_secrets() {
    use selencode::tools::shell::RunShell;
    use selencode::tools::Tool;
    use std::os::unix::fs::PermissionsExt;

    let tmp = std::env::temp_dir().join(format!(
        "selen-v050-path-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let fake_bin = tmp.join(".local").join("bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let script = fake_bin.join("selen_dummy_bin_v050");
    std::fs::write(&script, "#!/bin/sh\necho DUMMY_BIN_OK\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Verifikasi enriched_shell_path menyertakan fake_bin saat HOME = tmp.
    let ep = enriched_shell_path(Some(OsStr::new("/usr/bin:/bin")), Some(&tmp), None);
    let parts: Vec<PathBuf> = std::env::split_paths(&ep).collect();
    assert!(parts.contains(&fake_bin));

    // Uji eksekusi nyata lewat RunShell jika script ada di PATH yang diperkaya.
    let prev_path = std::env::var_os("PATH");
    let mut joined: Vec<PathBuf> = prev_path
        .as_deref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    joined.push(fake_bin.clone());
    unsafe { std::env::set_var("PATH", std::env::join_paths(&joined).unwrap()) };

    let ctx = Ctx::new(tmp.clone(), true);
    let out = RunShell::new(vec![], 30)
        .run(json!({"command": "selen_dummy_bin_v050"}), &ctx)
        .await
        .unwrap();
    if let Some(old) = prev_path {
        unsafe { std::env::set_var("PATH", old) };
    }
    assert!(out.contains("DUMMY_BIN_OK"), "{out}");

    let _ = std::fs::remove_dir_all(&tmp);
}
