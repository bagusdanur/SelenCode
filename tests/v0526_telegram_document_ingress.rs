//! Uji otomatis SelenCode v0.5.26 — Gateway Telegram: Terima Dokumen/Berkas Masuk:
//! 1. Parsing JSON `TgMessage` dengan `document`, `photo`, `text`, serta jenis belum didukung
//!    (`voice`, `sticker`, `video`, `audio`, `location`, `contact`) menghasilkan balasan 1 baris.
//! 2. Dokumen teks disimpan aman di `.selencode/tg_uploads/tg_<id>_<nama>` & dibaca agent via tool `read_file`.
//! 3. Dokumen gambar (`image/*`) diperlakukan sejajar jalur foto (`@<path>` -> dimuat sebagai `Block::Image`).
//! 4. Berkas > 20 MB ditolak sebelum disimpan, menghasilkan balasan batas ukuran, 0 berkas ditulis.
//! 5. Nama berkas berbahaya (`../../etc/passwd`, `..\evil.sh`, `C:\win\a.txt`, `.env`) disanitasi aman
//!    di dalam `.selencode/tg_uploads/` tanpa pernah menulis ke luar folder itu.

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::run_turn;
use selencode::providers::{
    AssistantReply, Block, Message, Provider, Role, StreamEvent, ToolCall, ToolSpec,
};
use selencode::telegram::{
    classify_tg_message, format_tg_document_too_large_reply, format_tg_document_turn_prompt,
    is_tg_image_document, sanitize_tg_document_rel_path, save_tg_document_bytes,
    TgIngressClassification, TgMessage, DEFAULT_TG_DOCUMENT_CAPTION, DEFAULT_TG_PHOTO_CAPTION,
    TG_DOCUMENT_MAX_BYTES,
};
use selencode::tools::{default_registry, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "selen-v0526-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn kriteria1_parse_document_photo_and_unsupported_messages_never_dropped_silently() {
    // 1. Pesan dokumen dengan caption kosong -> caption bawaan netral!
    let doc_json = json!({
        "message_id": 42,
        "chat": { "id": 1001 },
        "from": { "id": 777, "first_name": "Bagus" },
        "document": {
            "file_id": "BQACAgIAAxkBAAIB",
            "file_name": "laporan.txt",
            "mime_type": "text/plain",
            "file_size": 1280
        }
    });
    let doc_msg: TgMessage = serde_json::from_value(doc_json).unwrap();
    match classify_tg_message(&doc_msg) {
        TgIngressClassification::Document {
            file_id,
            message_id,
            file_name,
            mime_type,
            file_size,
            caption,
        } => {
            assert_eq!(file_id, "BQACAgIAAxkBAAIB");
            assert_eq!(message_id, 42);
            assert_eq!(file_name.as_deref(), Some("laporan.txt"));
            assert_eq!(mime_type.as_deref(), Some("text/plain"));
            assert_eq!(file_size, Some(1280));
            assert_eq!(caption, DEFAULT_TG_DOCUMENT_CAPTION);
        }
        other => panic!("harus Document, dapat: {other:?}"),
    }

    // 2. Pesan foto tetap berperilaku identik (regresi kriteria 7)!
    let photo_json = json!({
        "message_id": 43,
        "chat": { "id": 1001 },
        "from": { "id": 777, "first_name": "Bagus" },
        "photo": [
            { "file_id": "small", "width": 100, "height": 100 },
            { "file_id": "large", "width": 800, "height": 600 }
        ]
    });
    let photo_msg: TgMessage = serde_json::from_value(photo_json).unwrap();
    match classify_tg_message(&photo_msg) {
        TgIngressClassification::Photo {
            file_id,
            message_id,
            caption,
        } => {
            assert_eq!(file_id, "large");
            assert_eq!(message_id, 43);
            assert_eq!(caption, DEFAULT_TG_PHOTO_CAPTION);
        }
        other => panic!("harus Photo, dapat: {other:?}"),
    }

    // 3. Pesan suara, stiker, video -> masing-masing menghasilkan balasan 1 baris "belum didukung"!
    for (field, expected_label) in [
        ("voice", "suara"),
        ("sticker", "stiker"),
        ("video", "video"),
        ("audio", "audio"),
    ] {
        let raw = json!({
            "message_id": 44,
            "chat": { "id": 1001 },
            "from": { "id": 777, "first_name": "Bagus" },
            field: { "file_id": "abc" }
        });
        let msg: TgMessage = serde_json::from_value(raw).unwrap();
        match classify_tg_message(&msg) {
            TgIngressClassification::Unsupported { kind, reply } => {
                assert_eq!(kind, expected_label);
                assert!(reply.contains("belum didukung"), "{reply}");
                assert!(!reply.contains('\n'), "balasan harus tepat 1 baris: {reply}");
            }
            other => panic!("field {field} harus Unsupported, dapat: {other:?}"),
        }
    }
}

#[tokio::test]
async fn kriteria2_and_3_text_document_read_by_agent_and_image_document_loaded_as_image() {
    let dir = temp_dir("k2-k3");

    // --- Kriteria 2: dokumen teks kecil disimpan di tg_uploads & dibaca agent via read_file ---
    let content = b"KODE_RAHASIA_UJI=SELENA-2026\nTotal transaksi: 42\n";
    let rel_txt = save_tg_document_bytes(
        &dir,
        101,
        Some("catatan bulanan.txt"),
        Some("text/plain"),
        None,
        content,
    )
    .unwrap();
    assert!(rel_txt.starts_with(".selencode/tg_uploads/tg_101_"));
    assert!(dir.join(&rel_txt).is_file());
    assert!(!rel_txt.contains(' '), "path tersimpan tidak boleh mengandung spasi");

    let prompt_txt = format_tg_document_turn_prompt(
        "catatan bulanan.txt",
        &rel_txt,
        Some("text/plain"),
        content.len(),
        "Berapa total transaksi di berkas ini?",
    );
    assert!(prompt_txt.contains(&rel_txt));
    assert!(prompt_txt.contains("catatan bulanan.txt"));

    struct ReadDocMock {
        step: AtomicUsize,
        rel_path: String,
    }
    #[async_trait]
    impl Provider for ReadDocMock {
        fn model_name(&self) -> &str {
            "mock-doc-reader"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(AssistantReply {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "r1".into(),
                        name: "read_file".into(),
                        args: json!({"path": self.rel_path}),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: None,
                })
            } else {
                // Pastikan hasil read_file (`Block::ToolResult`) benar-benar berisi teks dokumen yang diunggah!
                let saw_file_body = msgs.iter().any(|m| {
                    m.content.iter().any(|b| match b {
                        Block::ToolResult {
                            content,
                            is_error: false,
                            ..
                        } => content.contains("Total transaksi: 42"),
                        _ => false,
                    })
                });
                assert!(saw_file_body, "agent harus menerima isi dokumen lewat read_file");
                let ans = "Total transaksi di catatan bulanan.txt adalah 42.".to_string();
                let _ = tx.send(StreamEvent::Token(ans.clone())).await;
                Ok(AssistantReply {
                    text: ans,
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                })
            }
        }
    }

    let reg = default_registry();
    let ctx = Ctx::new(dir.clone(), true);
    let mut msgs = Vec::new();
    let ans = run_turn(
        Arc::new(ReadDocMock {
            step: AtomicUsize::new(0),
            rel_path: rel_txt.clone(),
        }),
        &reg,
        &ctx,
        &mut msgs,
        &prompt_txt,
        5,
    )
    .await
    .unwrap();
    assert!(ans.contains("42"), "agent harus menjawab berdasar isi dokumen: {ans}");

    // --- Kriteria 3: dokumen gambar (`image/png` dikirim sebagai file tanpa ekstensi) -> @<path> + Block::Image ---
    let png_bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let rel_img = save_tg_document_bytes(
        &dir,
        102,
        Some("screenshot_tanpa_ekstensi"),
        Some("image/png"),
        None,
        png_bytes,
    )
    .unwrap();
    assert!(rel_img.ends_with(".png"), "MIME image/png harus mendapat ekstensi .png: {rel_img}");
    assert!(is_tg_image_document(Some("image/png"), Some(&rel_img)));
    let prompt_img = format_tg_document_turn_prompt(
        "screenshot_tanpa_ekstensi",
        &rel_img,
        Some("image/png"),
        png_bytes.len(),
        "Jelaskan isi gambar ini.",
    );
    assert!(
        prompt_img.starts_with(&format!("@{rel_img} ")),
        "dokumen gambar harus memakai format @{rel_img} sejajar jalur foto: {prompt_img}"
    );

    struct ImageBlockCaptureMock {
        saw_image_block: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Provider for ImageBlockCaptureMock {
        fn model_name(&self) -> &str {
            "mock-vision"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            for m in msgs {
                for b in &m.content {
                    if let Block::Image { media_type, .. } = b {
                        if media_type == "image/png" {
                            *self.saw_image_block.lock().unwrap() = true;
                        }
                    }
                }
            }
            let reply = "Gambar PNG terbaca.".to_string();
            let _ = tx.send(StreamEvent::Token(reply.clone())).await;
            Ok(AssistantReply {
                text: reply,
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: None,
            })
        }
    }

    let saw_img = Arc::new(Mutex::new(false));
    let mut img_msgs = Vec::new();
    let _ = run_turn(
        Arc::new(ImageBlockCaptureMock {
            saw_image_block: saw_img.clone(),
        }),
        &reg,
        &ctx,
        &mut img_msgs,
        &prompt_img,
        3,
    )
    .await
    .unwrap();
    assert!(
        *saw_img.lock().unwrap(),
        "dokumen gambar harus otomatis dilampirkan sebagai Block::Image persis jalur foto"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kriteria4_and_5_oversized_20mb_rejected_and_malicious_filenames_sanitized_inside_tg_uploads() {
    let dir = temp_dir("k4-k5");

    // --- Kriteria 4: berkas > 20 MB ditolak, 0 berkas ditulis ---
    let over_bytes = vec![0u8; (TG_DOCUMENT_MAX_BYTES as usize) + 1];
    let err = save_tg_document_bytes(
        &dir,
        201,
        Some("video-mentah.zip"),
        Some("application/zip"),
        None,
        &over_bytes,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("20 MB"),
        "pesan penolakan harus menyebut batas 20 MB: {err}"
    );
    assert!(
        !dir.join(".selencode/tg_uploads").exists(),
        "berkas > 20 MB tidak boleh meninggalkan file apa pun di disk"
    );
    let notice = format_tg_document_too_large_reply(Some("big.pdf"), 25 * 1024 * 1024);
    assert!(notice.contains("20 MB") && notice.contains("big.pdf"));

    // --- Kriteria 5: nama berkas traversal / berbahaya selalu terkurung di `.selencode/tg_uploads/` ---
    let evil_names = [
        "../../../etc/passwd",
        "..\\..\\Windows\\System32\\cmd.exe",
        "/etc/shadow",
        "C:\\Users\\admin\\secret.txt",
        ".env",
        "...",
        "foo/bar/../../notes.md",
    ];
    let uploads_dir = dir.join(".selencode/tg_uploads");
    for (idx, evil) in evil_names.iter().enumerate() {
        let mid = 300 + idx as i64;
        let rel = sanitize_tg_document_rel_path(mid, Some(evil), Some("text/plain"), None);
        assert!(
            rel.starts_with(&format!(".selencode/tg_uploads/tg_{mid}_")),
            "hasil sanitasi untuk '{evil}' harus diawali tg_{mid}_: {rel}"
        );
        assert!(
            !rel.contains(".."),
            "hasil sanitasi untuk '{evil}' dilarang memuat '..': {rel}"
        );
        let saved = save_tg_document_bytes(&dir, mid, Some(evil), Some("text/plain"), None, b"uji")
            .unwrap();
        let full = dir.join(&saved);
        assert_eq!(
            full.parent(),
            Some(uploads_dir.as_path()),
            "berkas untuk '{evil}' wajib berada ketat di dalam .selencode/tg_uploads/"
        );
        assert!(full.is_file());
    }

    let _ = std::fs::remove_dir_all(&dir);
}
