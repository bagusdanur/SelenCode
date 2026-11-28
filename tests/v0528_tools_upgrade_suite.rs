//! Tes integrasi v0.5.28 — Upgrade Suite 3 Besar Tool SelenCode:
//! 1. `web_search` + `fetch_url` v2 (HTML->Markdown bersih, fast-path GitHub/Wikipedia/HN, RSS/Atom,
//!    parser DuckDuckGo 0-key, pagu 15.000 char + spill-to-disk `.selencode/web_cache/`).
//! 2. `search_text` (`glob`, `case_insensitive`, `fixed_strings`, `context_lines`, `files_only`),
//!    `list_files` (`glob`), dan `edit_file` (`replace_all`).
//! 3. `read_file` multimodal (Gambar PNG/JPEG/GIF/WebP + injeksi `Block::Image` di `run_turn`,
//!    serta ekstraktor teks PDF murni Rust uncompressed & `FlateDecode`).
//! 4. Tool agent `manage_cron` (CRUD jadwal SQLite + approval + pewarisan `telegram_chat_id` + toolset `cron`).

use anyhow::Result;
use async_trait::async_trait;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use selencode::agent::plan::{TurnMode, READ_ONLY_TOOLS};
use selencode::agent::run_turn;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};
use selencode::security::{ApprovalRequest, Approver};
use selencode::store::Store;
use selencode::tools::file::{extract_image_dimensions, extract_pdf_text_bytes};
use selencode::tools::toolsets::select_initial_toolsets;
use selencode::tools::web::{
    detect_fast_path, html_to_markdown, parse_ddg_html_results, parse_rss_or_atom_feed,
    spill_web_output_if_needed, WebFastPath,
};
use selencode::tools::{default_registry, read_only_registry, Ctx};
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v0528-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Bangun PNG minimal valid berukuran `w x h` untuk pengujian `read_file` multimodal.
fn make_minimal_png(w: u32, h: u32) -> Vec<u8> {
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&w.to_be_bytes());
    png.extend_from_slice(&h.to_be_bytes());
    png.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth 8, RGB
    png.extend_from_slice(&[0, 0, 0, 0]); // CRC dummy (cukup untuk header dimensi + MIME)
    png
}

/// Bangun PDF minimal dengan stream konten tak terkompresi atau `/FlateDecode`.
fn make_minimal_pdf(content_stream: &[u8], compress: bool) -> Vec<u8> {
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n2 0 obj\n");
    if compress {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content_stream).unwrap();
        let compressed = enc.finish().unwrap();
        pdf.extend_from_slice(
            format!(
                "<< /Length {} /Filter /FlateDecode >>\nstream\n",
                compressed.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&compressed);
    } else {
        pdf.extend_from_slice(
            format!("<< /Length {} >>\nstream\n", content_stream.len()).as_bytes(),
        );
        pdf.extend_from_slice(content_stream);
    }
    pdf.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");
    pdf
}

#[tokio::test]
async fn upgrade1_web_v2_markdown_fast_paths_rss_ddg_and_spill_to_disk() {
    let dir = temp_dir("web-v2");

    // 1A. HTML -> Markdown bersih (buang nav/footer/script, fokus <article>, jaga heading/kode/list/link).
    let html = r#"<!doctype html>
<html>
<head><title>Panduan Rust Async</title><style>body{color:red}</style></head>
<body>
  <nav><a href="/home">MENU_NAV_SAMPAH</a></nav>
  <article>
    <h1>Memulai Tokio</h1>
    <p>Gunakan <code>#[tokio::main]</code> untuk menjalankan runtime asinkron dengan aman dan cepat di aplikasi jaringan modern Rust.</p>
    <p>Dokumentasi resmi tersedia di <a href="https://tokio.rs/tokio/tutorial">Tutorial Tokio</a> untuk referensi lengkap termasuk saluran mpsc dan task spawning.</p>
    <ul>
      <li>Langkah pertama pasang crate</li>
      <li>Langkah kedua jalankan cargo test</li>
    </ul>
    <pre><code class="language-rust">#[tokio::main]
async fn main() {
    println!("halo");
}</code></pre>
  </article>
  <footer>FOOTER_SAMPAH_2026</footer>
  <script>alert("xss")</script>
</body>
</html>"#;
    let md = html_to_markdown(html);
    assert!(md.contains("# Panduan Rust Async"), "{md}");
    assert!(md.contains("# Memulai Tokio"), "{md}");
    assert!(md.contains("`#[tokio::main]`"), "{md}");
    assert!(
        md.contains("[Tutorial Tokio](https://tokio.rs/tokio/tutorial)"),
        "{md}"
    );
    assert!(md.contains("- Langkah pertama pasang crate"), "{md}");
    assert!(md.contains("```\n#[tokio::main]"), "{md}");
    assert!(!md.contains("MENU_NAV_SAMPAH"), "nav harus dibuang: {md}");
    assert!(!md.contains("FOOTER_SAMPAH_2026"), "footer harus dibuang: {md}");
    assert!(!md.contains("alert("), "script harus dibuang: {md}");

    // 1B. Deteksi jalur cepat per-domain (GitHub, Wikipedia, HN).
    assert_eq!(
        detect_fast_path(&reqwest::Url::parse("https://github.com/bagusdanur/SelenCode").unwrap()),
        Some(WebFastPath::GitHubRepo {
            owner: "bagusdanur".into(),
            repo: "SelenCode".into()
        })
    );
    assert_eq!(
        detect_fast_path(
            &reqwest::Url::parse("https://github.com/bagusdanur/SelenCode/pull/42").unwrap()
        ),
        Some(WebFastPath::GitHubIssueOrPr {
            owner: "bagusdanur".into(),
            repo: "SelenCode".into(),
            number: "42".into(),
            is_pr: true
        })
    );
    assert_eq!(
        detect_fast_path(
            &reqwest::Url::parse("https://github.com/bagusdanur/SelenCode/blob/main/src/lib.rs")
                .unwrap()
        ),
        Some(WebFastPath::GitHubBlob {
            owner: "bagusdanur".into(),
            repo: "SelenCode".into(),
            git_ref: "main".into(),
            file_path: "src/lib.rs".into()
        })
    );
    assert_eq!(
        detect_fast_path(
            &reqwest::Url::parse("https://id.wikipedia.org/wiki/Bahasa_pemrograman_Rust").unwrap()
        ),
        Some(WebFastPath::Wikipedia {
            lang: "id".into(),
            title: "Bahasa_pemrograman_Rust".into()
        })
    );
    assert_eq!(
        detect_fast_path(
            &reqwest::Url::parse("https://news.ycombinator.com/item?id=12345678").unwrap()
        ),
        Some(WebFastPath::HackerNewsItem {
            id: "12345678".into()
        })
    );

    // 1C. Parser RSS/Atom.
    let rss = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Berita Rust</title>
    <item><title>Rilis 1.90</title><link>https://blog.rust-lang.org/1.90</link><pubDate>2026-11-01</pubDate><description>Peningkatan kecepatan kompilasi.</description></item>
    </channel></rss>"#;
    let feed = parse_rss_or_atom_feed(rss).expect("RSS harus terdeteksi");
    assert!(feed.contains("Berita Rust"), "{feed}");
    assert!(feed.contains("Rilis 1.90"), "{feed}");
    assert!(feed.contains("https://blog.rust-lang.org/1.90"), "{feed}");

    // 1D. Parser DuckDuckGo HTML (0 API key, dekode uddg=).
    let ddg_html = r#"
    <div class="result">
      <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=1">Tokio Docs</a>
      <a class="result__snippet">Runtime asinkron untuk bahasa Rust.</a>
    </div>
    <div class="result">
      <a class="result__a" href="https://rust-lang.org">Rust Lang</a>
      <div class="result__snippet">Bahasa pemrograman sistem yang aman.</div>
    </div>"#;
    let ddg_res = parse_ddg_html_results(ddg_html, 5);
    assert_eq!(ddg_res.len(), 2);
    assert_eq!(ddg_res[0].0, "Tokio Docs");
    assert_eq!(ddg_res[0].1, "https://docs.rs/tokio");
    assert!(ddg_res[0].2.contains("Runtime asinkron"));

    // 1E. Pagu deterministik 15.000 char + spill-to-disk ke `.selencode/web_cache/` yang terbaca oleh `read_file`!
    let mut huge = String::new();
    for i in 1..=600 {
        huge.push_str(&format!("Baris dokumentasi nomor {i:03} — isi panjang untuk pengujian spill cache web.\n"));
    }
    assert!(huge.chars().count() > 25_000);
    let spilled = spill_web_output_if_needed(&dir, "https://docs.example.com/big", &huge, 4_000);
    assert!(spilled.contains("Baris dokumentasi nomor 001"), "head wajib hadir");
    assert!(spilled.contains("Baris dokumentasi nomor 600"), "tail wajib hadir");
    assert!(spilled.contains(".selencode/web_cache/web_"), "path cache wajib disebut: {spilled}");
    assert!(
        spilled.chars().count() < 5_000,
        "output spill harus ringkas sesuai pagu, dapat {} char",
        spilled.chars().count()
    );
    // Pastikan berkas cache benar-benar ada dan bisa dibaca lewat `read_file`!
    let cache_entries: Vec<PathBuf> = std::fs::read_dir(dir.join(".selencode/web_cache"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(cache_entries.len(), 1);
    let rel_cache = cache_entries[0]
        .strip_prefix(&dir)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let reg = default_registry();
    let read_tool = reg.get("read_file").unwrap();
    let ctx = Ctx::new(dir.clone(), true);
    let mid = read_tool
        .run(json!({"path": rel_cache, "start": 300, "end": 305}), &ctx)
        .await
        .unwrap();
    assert!(mid.contains("Baris dokumentasi nomor 300"), "{mid}");

    // 1F. Registrasi `web_search` di registry penuh, read-only, dan toolset `web`.
    assert!(reg.get("web_search").is_some());
    assert!(read_only_registry().get("web_search").is_some());
    assert!(READ_ONLY_TOOLS.contains(&"web_search"));
    let ts_web = select_initial_toolsets("tolong cari di web dokumentasi terbaru rust", &dir, false, TurnMode::Execute);
    assert!(ts_web.contains("web"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn upgrade2_search_text_list_files_and_edit_file_replace_all() {
    let dir = temp_dir("core-tools");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();

    std::fs::write(
        dir.join("src/app.rs"),
        "fn alpha() {\n    let old_val = items[0].get(a+b);\n    println!(\"{old_val}\");\n}\n\nfn beta() {\n    let x = OLD_VAL;\n    let y = old_val + old_val;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("docs/note.md"),
        "Catatan tentang old_val di dokumentasi.\n",
    )
    .unwrap();

    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true);

    // 2A. `list_files` dengan filter `glob`.
    let list_tool = reg.get("list_files").unwrap();
    let only_rs = list_tool.run(json!({"glob": "*.rs"}), &ctx).await.unwrap();
    assert!(only_rs.contains("app.rs"), "{only_rs}");
    assert!(!only_rs.contains("note.md"), "{only_rs}");

    // 2B. `search_text` dengan `glob`, `case_insensitive`, `fixed_strings`, `context_lines`, & `files_only`.
    let search_tool = reg.get("search_text").unwrap();
    let fixed = search_tool
        .run(
            json!({"query": "items[0].get(a+b)", "fixed_strings": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(fixed.contains("app.rs:2:"), "fixed_strings wajib cocok literal: {fixed}");

    let ci_glob = search_tool
        .run(
            json!({"query": "old_val", "glob": "*.rs", "case_insensitive": true, "files_only": true}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(ci_glob.contains("app.rs (4 cocok)"), "{ci_glob}");
    assert!(!ci_glob.contains("note.md"), "{ci_glob}");

    let with_ctx = search_tool
        .run(
            json!({"query": "OLD_VAL", "glob": "*.rs", "context_lines": 1}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(with_ctx.contains("app.rs:6- fn beta()"), "{with_ctx}");
    assert!(with_ctx.contains("app.rs:7:     let x = OLD_VAL;"), "{with_ctx}");
    assert!(with_ctx.contains("app.rs:8-     let y = old_val"), "{with_ctx}");

    // 2C. `edit_file` dengan `replace_all: true` mengganti semua kemunculan sekaligus + tercatat di undo_stack!
    let edit_tool = reg.get("edit_file").unwrap();
    let edit_out = edit_tool
        .run(
            json!({
                "path": "src/app.rs",
                "old": "old_val",
                "new": "new_val",
                "replace_all": true
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(edit_out.contains("4 penggantian"), "{edit_out}");
    let after = std::fs::read_to_string(dir.join("src/app.rs")).unwrap();
    assert!(!after.contains("old_val"), "{after}");
    assert_eq!(after.matches("new_val").count(), 4);
    assert!(after.contains("OLD_VAL"), "case-sensitive exact replace_all tidak mengubah OLD_VAL");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn upgrade3a_read_file_multimodal_image_and_pdf_with_vision_loop() {
    let dir = temp_dir("multimodal-read");
    let png_bytes = make_minimal_png(320, 180);
    assert_eq!(extract_image_dimensions(&png_bytes), Some((320, 180)));
    std::fs::write(dir.join("mockup.png"), &png_bytes).unwrap();

    // PDF 1: tak terkompresi
    let pdf_plain = make_minimal_pdf(b"BT /F1 12 Tf (Halo PDF SelenCode) Tj ET", false);
    std::fs::write(dir.join("plain.pdf"), &pdf_plain).unwrap();
    assert!(extract_pdf_text_bytes(&pdf_plain)
        .unwrap()
        .contains("Halo PDF SelenCode"));

    // PDF 2: terkompresi FlateDecode dengan TJ array
    let pdf_flate = make_minimal_pdf(
        b"BT /F1 12 Tf [(Laporan ) -220 (Terkompresi)] TJ T* (Baris Kedua) Tj ET",
        true,
    );
    std::fs::write(dir.join("flate.pdf"), &pdf_flate).unwrap();

    let reg = default_registry();
    let read_tool = reg.get("read_file").unwrap();
    let ctx = Ctx::new(dir.clone(), true);

    let img_out = read_tool.run(json!({"path": "mockup.png"}), &ctx).await.unwrap();
    assert!(img_out.contains("320x180"), "{img_out}");
    assert!(img_out.contains("image/png"), "{img_out}");

    let pdf1_out = read_tool.run(json!({"path": "plain.pdf"}), &ctx).await.unwrap();
    assert!(pdf1_out.contains("Halo PDF SelenCode"), "{pdf1_out}");

    let pdf2_out = read_tool.run(json!({"path": "flate.pdf"}), &ctx).await.unwrap();
    assert!(pdf2_out.contains("Laporan Terkompresi"), "{pdf2_out}");
    assert!(pdf2_out.contains("Baris Kedua"), "{pdf2_out}");

    // Uji penuh di `run_turn`: panggilan 1 memanggil `read_file("mockup.png")` ->
    // panggilan 2 WAJIB menerima `Block::Image { media_type: "image/png", .. }` di pesan user!
    struct VisionCheckProvider {
        calls: AtomicUsize,
        saw_image_block: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for VisionCheckProvider {
        fn model_name(&self) -> &str {
            "mock-vision"
        }
        async fn stream_chat(
            &self,
            messages: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "rf_img".into(),
                        name: "read_file".into(),
                        args: json!({"path": "mockup.png"}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: Some(Usage {
                        input_tokens: 800,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                })
            } else {
                let has_img = messages.iter().any(|m| {
                    m.content.iter().any(|b| {
                        matches!(b, Block::Image { media_type, data_base64 } if media_type == "image/png" && !data_base64.is_empty())
                    })
                });
                *self.saw_image_block.lock().unwrap() = has_img;
                let ans = "Gambar mockup.png (320x180) sudah saya lihat.";
                let _ = tx.send(StreamEvent::Token(ans.into())).await;
                Ok(AssistantReply {
                    text: ans.into(),
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 25,
                        ..Default::default()
                    }),
                })
            }
        }
    }

    let saw_img = Arc::new(Mutex::new(false));
    let prov = Arc::new(VisionCheckProvider {
        calls: AtomicUsize::new(0),
        saw_image_block: saw_img.clone(),
    });
    let mut msgs = Vec::new();
    let ans = run_turn(
        prov,
        &reg,
        &ctx,
        &mut msgs,
        "Periksa gambar mockup.png di folder ini.",
        10,
    )
    .await
    .unwrap();
    assert!(ans.contains("mockup.png"), "{ans}");
    assert!(
        *saw_img.lock().unwrap(),
        "run_turn WAJIB menyisipkan Block::Image ke pesan berikutnya sesudah read_file memuat gambar!"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn upgrade3b_manage_cron_agent_tool_crud_approval_and_toolset() {
    let dir = temp_dir("manage-cron");
    let store = Arc::new(Store::open_at(&dir.join("selen.db")).unwrap());
    let sid = store
        .create_session(&dir.to_string_lossy(), "router", "m", "uji cron")
        .unwrap();

    // Toolset `cron` aktif saat prompt menyebut jadwal, dan TIDAK aktif pada tugas ringan.
    let ts_cron = select_initial_toolsets(
        "jadwalkan cek server setiap pagi jam 07:30",
        &dir,
        false,
        TurnMode::Execute,
    );
    assert!(ts_cron.contains("cron"));
    let ts_simple = select_initial_toolsets(
        "buat file hello.txt",
        &dir,
        false,
        TurnMode::Execute,
    );
    assert!(!ts_simple.contains("cron"));
    assert!(!ts_simple.contains("web"));

    let reg = default_registry();
    let cron_tool = reg.get("manage_cron").expect("manage_cron terdaftar");
    assert!(!cron_tool.needs_approval(&json!({"action": "list"})));
    assert!(cron_tool.needs_approval(&json!({"action": "add"})));

    // 1. Jika user MENOLAK approval `add` -> tidak ada job tersimpan!
    let (tx_deny, mut rx_deny) = mpsc::channel::<ApprovalRequest>(4);
    let ctx_deny = Ctx::new(dir.clone(), false)
        .with_approver(Arc::new(Approver::channel(false, tx_deny)))
        .with_store(store.clone(), sid.clone());
    let h_deny = tokio::spawn(async move {
        let req = rx_deny.recv().await.expect("approval add harus muncul");
        assert!(req.title.contains("manage_cron add"));
        let _ = req.reply.send(false);
    });
    let denied = cron_tool
        .run(
            json!({
                "action": "add",
                "name": "cek-pagi",
                "schedule": "daily 07:30",
                "prompt": "cek disk dan memori"
            }),
            &ctx_deny,
        )
        .await
        .unwrap();
    h_deny.await.unwrap();
    assert!(denied.contains("Ditolak user"), "{denied}");
    assert!(store.list_cron_jobs().unwrap().is_empty());

    // 2. Dengan approval + chat_id Telegram -> tersimpan lengkap di SQLite, bisa disable/enable/remove!
    let mut ctx_ok = Ctx::new(dir.clone(), true).with_store(store.clone(), sid);
    ctx_ok.telegram_chat_id = Some(987654);

    let added = cron_tool
        .run(
            json!({
                "action": "add",
                "name": "cek-pagi",
                "schedule": "daily 07:30",
                "prompt": "cek disk dan memori",
                "allowed_shell": ["df -h"]
            }),
            &ctx_ok,
        )
        .await
        .unwrap();
    assert!(added.contains("berhasil dibuat"), "{added}");

    let jobs = store.list_cron_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "cek-pagi");
    assert_eq!(jobs[0].schedule_spec, "daily:07:30");
    assert_eq!(jobs[0].chat_id, 987654);
    assert!(!jobs[0].allow_write);
    assert_eq!(jobs[0].allowed_shell, vec!["df -h"]);

    let listed = cron_tool.run(json!({"action": "list"}), &ctx_ok).await.unwrap();
    assert!(listed.contains("cek-pagi"), "{listed}");
    assert!(listed.contains("07:30"), "{listed}");

    let disabled = cron_tool
        .run(json!({"action": "disable", "name": "cek-pagi"}), &ctx_ok)
        .await
        .unwrap();
    assert!(disabled.contains("JEDA"), "{disabled}");
    assert!(!store.list_cron_jobs().unwrap()[0].enabled);

    let enabled = cron_tool
        .run(json!({"action": "enable", "name": "cek-pagi"}), &ctx_ok)
        .await
        .unwrap();
    assert!(enabled.contains("AKTIF"), "{enabled}");
    assert!(store.list_cron_jobs().unwrap()[0].enabled);

    let removed = cron_tool
        .run(json!({"action": "remove", "name": "cek-pagi"}), &ctx_ok)
        .await
        .unwrap();
    assert!(removed.contains("berhasil dihapus"), "{removed}");
    assert!(store.list_cron_jobs().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
