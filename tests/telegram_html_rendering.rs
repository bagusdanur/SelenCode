//! Uji rendering Telegram HTML (draf belum diberi versi — tanpa markdown mentah, rapi di layar HP):
//! 1. 6 contoh nyata sebelum/sesudah (bold `**`/`__` termasuk membungkus `` `kode` ``, backtick URL & status,
//!    tabel Markdown -> tumpukan kunci-nilai HP, header dengan spasi baris kosong, blok kode panjang -> lampiran,
//!    dan escape HTML `&`, `<`, `>` penuh).
//! 2. Jalur fallback anti-gagal-parse (HTTP 400 "can't parse entities" -> kirim ulang teks polos bersih tanpa `**`).
//! 3. Ekstraksi & unggah dokumen otomatis (`sendDocument`) untuk blok kode panjang (`>= 25` baris / `>= 1.500` char).
//! 4. Sentuhan prompt 1 baris HANYA pada konteks Telegram (CLI/plain tidak tersentuh).

use anyhow::Result;
use async_trait::async_trait;
use selencode::agent::run_turn_counted;
use selencode::providers::{AssistantReply, Message, Provider, StreamEvent, ToolSpec, Usage};
use selencode::telegram::{
    extract_long_code_blocks_for_telegram, format_telegram_progress_html,
    markdown_to_clean_plain_text, markdown_to_telegram_html, TelegramClient, TgAskHub,
};
use selencode::tools::{default_registry, Ctx};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[test]
fn telegram_html_six_real_world_samples_no_raw_markdown() {
    // Contoh 1: **bold** & __bold__ termasuk membungkus backtick & italic + coret
    let s1 = "**Status akhir:** **`FAILED`** pada __tahap inpaint__ — cek *halaman 7* (~~lama~~ baru).";
    let h1 = markdown_to_telegram_html(s1);
    assert!(!h1.contains("**"), "tidak boleh ada ** mentah: {h1}");
    assert!(!h1.contains("__"), "tidak boleh ada __ mentah: {h1}");
    assert!(!h1.contains("~~"), "tidak boleh ada ~~ mentah: {h1}");
    assert!(h1.contains("<b>Status akhir:</b>"));
    // Status `FAILED` tidak dibungkus <code> monospace kaku
    assert!(h1.contains("<b>FAILED</b>"), "got: {h1}");
    assert!(h1.contains("<b>tahap inpaint</b>"));
    assert!(h1.contains("<i>halaman 7</i>"));
    assert!(h1.contains("<s>lama</s>"));

    // Contoh 2: Backtick pada URL & istilah teknis pendek
    let s2 = "Rilis tersedia di `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11` atau [halaman rilis](https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11).\nJalankan `cargo test` di `src/telegram.rs` untuk cek `#6c33`.";
    let h2 = markdown_to_telegram_html(s2);
    assert!(
        !h2.contains("<code>https://"),
        "URL TIDAK BOLEH dibungkus <code> agar bisa disentuh di HP: {h2}"
    );
    assert!(h2.contains("https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11"));
    assert!(h2.contains("<a href=\"https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11\">halaman rilis</a>"));
    assert!(h2.contains("<code>cargo test</code>"));
    assert!(h2.contains("<code>src/telegram.rs</code>"));
    assert!(h2.contains("<code>#6c33</code>"));

    // Contoh 3: Tabel Markdown -> tumpukan kunci-nilai rapi untuk layar HP (0 pipa `|`)
    let s3 = "\
| Job | Status | Progres |
| --- | --- | --- |
| `#6c33` | `SELESAI` | 22/22 |
| `#7d11` | `FAILED` | 4/12 |";
    let h3 = markdown_to_telegram_html(s3);
    assert!(!h3.contains('|'), "tabel harus diubah jadi tumpukan kunci-nilai: {h3}");
    assert!(h3.contains("• <b>Job:</b> <code>#6c33</code>"));
    assert!(h3.contains("– <b>Status:</b> SELESAI"));
    assert!(h3.contains("– <b>Progres:</b> 22/22"));
    assert!(h3.contains("• <b>Job:</b> <code>#7d11</code>"));

    // Contoh 4: Header `#`/`##`/`###` dengan jarak baris kosong sebelum/sesudah
    let s4 = "Pembuka singkat.\n## Hasil Verifikasi\n- Baris satu\n- Baris dua";
    let h4 = markdown_to_telegram_html(s4);
    assert!(!h4.contains("##"));
    assert!(
        h4.contains("Pembuka singkat.\n\n<b>Hasil Verifikasi</b>\n\n• Baris satu"),
        "header harus berjarak baris kosong sebelum & sesudah: {h4:?}"
    );

    // Contoh 5: Blok kode panjang (>=25 baris) diekstrak menjadi berkas lampiran + 1 baris ringkasan
    let mut long_code = String::from("Berikut skrip lengkapnya:\n```python\n");
    for i in 1..=30 {
        long_code.push_str(&format!("print('baris {i}')\n"));
    }
    long_code.push_str("```\nSelesai.");
    let (short_md, extracted) = extract_long_code_blocks_for_telegram(&long_code);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].filename, "kode_1.py");
    assert_eq!(extracted[0].lines, 30);
    assert!(short_md.contains("kode_1.py"));
    assert!(short_md.contains("30 baris"));
    assert!(!short_md.contains("print('baris 1')"));

    // Contoh 6: Escape HTML penuh pada teks biasa, hasil tool, kartu `ask_user`, & preview mengetik
    let s6 = "Output tool: `if (a < b && c > 0)` dan teks <script>alert(1)</script> & **aman**.";
    let h6 = markdown_to_telegram_html(s6);
    assert!(h6.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; <b>aman</b>"));
    assert!(h6.contains("<code>if (a &lt; b &amp;&amp; c &gt; 0)</code>"));

    let (ask_html, ask_plain) = TgAskHub::format_ask_card_html(
        "Pilih target **deploy** (`prod` <utama>)?",
        &["Ya".into()],
        Some("Konteks: **rilis** & verifikasi"),
    );
    assert!(!ask_html.contains("**"));
    assert!(ask_html.contains("<b>deploy</b>"));
    assert!(ask_html.contains("&lt;utama&gt;"));
    assert!(!ask_plain.contains("**"));

    let prog = format_telegram_progress_html(2, 10, None, 1, "Mengecek **berkas** <a&b>");
    assert!(!prog.contains("**"));
    assert!(prog.contains("<b>berkas</b> &lt;a&amp;b&gt;"));
}

#[tokio::test]
async fn telegram_html_parse_error_400_falls_back_to_clean_plain_text_and_uploads_long_code() {
    // Mock server Telegram Bot API lokal:
    // - Menolak `sendMessage` bila `parse_mode == "HTML"` mengandung `<invalid-tag>` dengan HTTP 400 "can't parse entities"
    // - Menerima retry teks polos dan `sendDocument` multipart
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let rec_srv = recorded.clone();

    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let rec = rec_srv.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut expected_total: Option<usize> = None;
                loop {
                    let Ok(n) = sock.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if expected_total.is_none() {
                        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            let cl = head
                                .lines()
                                .find_map(|l| {
                                    let low = l.to_ascii_lowercase();
                                    low.strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            expected_total = Some(pos + 4 + cl);
                        }
                    }
                    if let Some(tot) = expected_total {
                        if raw.len() >= tot {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let first_line = text.lines().next().unwrap_or("").to_string();
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                rec.lock().unwrap().push((first_line.clone(), body.clone()));

                let (status, resp_json) = if first_line.contains("/sendMessage")
                    && body.contains("\"parse_mode\":\"HTML\"")
                    && body.contains("<unclosed")
                {
                    (
                        "400 Bad Request",
                        r#"{"ok":false,"description":"Bad Request: can't parse entities: Unsupported start tag \"unclosed\""}"#,
                    )
                } else {
                    (
                        "200 OK",
                        r#"{"ok":true,"result":{"message_id":42,"chat":{"id":12345}}}"#,
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_json}",
                    resp_json.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let tg = TelegramClient::with_base_url("test-token", &format!("http://{addr}")).unwrap();

    // 1. Paksa HTML tidak valid -> Telegram balas 400 "can't parse entities" -> fallback kirim TEKS POLOS BERSIH (tanpa `**`!)
    let mid = tg
        .send_html_message(
            12345,
            "<b>Halo</b> <unclosed>",
            "**Halo** dunia\n- Baris `aman` & bersih",
        )
        .await
        .unwrap();
    assert_eq!(mid, 42);

    // 2. Uji `send_long_markdown_with_code_files`: 1 pesan HTML ringkas + 1 `sendDocument` berkas kode
    let mut big_md = String::from("## Skrip Lengkap\n```rust\n");
    for i in 1..=28 {
        big_md.push_str(&format!("let v{i} = {i};\n"));
    }
    big_md.push_str("```");
    tg.send_long_markdown_with_code_files(12345, &big_md)
        .await
        .unwrap();

    let calls = recorded.lock().unwrap().clone();
    assert_eq!(calls.len(), 4, "harus ada: 1 HTML gagal + 1 fallback polos + 1 HTML ringkasan + 1 sendDocument");
    // Panggilan ke-2 = fallback teks polos bersih (tanpa parse_mode, tanpa `**`, baris tetap utuh!)
    assert!(calls[1].0.contains("/sendMessage"));
    assert!(!calls[1].1.contains("parse_mode"));
    assert!(!calls[1].1.contains("**"), "fallback polos tidak boleh membawa ** mentah: {}", calls[1].1);
    assert!(calls[1].1.contains("Halo dunia"));
    assert!(calls[1].1.contains("• Baris aman & bersih"));
    // Panggilan ke-3 = HTML ringkasan
    assert!(calls[2].0.contains("/sendMessage"));
    assert!(calls[2].1.contains("\"parse_mode\":\"HTML\""));
    assert!(calls[2].1.contains("kode_1.rs"));
    // Panggilan ke-4 = sendDocument berisi skrip lengkap
    assert!(calls[3].0.contains("/sendDocument"));
    assert!(calls[3].1.contains("kode_1.rs"));
    assert!(calls[3].1.contains("let v28 = 28;"));

    // Pastikan helper bersih juga bebas bintang/backtick mentah
    let clean = markdown_to_clean_plain_text("**Tebal** `https://example.com`");
    assert_eq!(clean, "Tebal https://example.com");

    server.abort();
}

#[tokio::test]
async fn telegram_prompt_hint_only_appears_in_telegram_context_not_cli() {
    struct CaptureSysProv(Arc<Mutex<String>>);
    #[async_trait]
    impl Provider for CaptureSysProv {
        fn model_name(&self) -> &str {
            "mock-sys-capture"
        }
        async fn stream_chat(
            &self,
            msgs: &[Message],
            _tools: &[ToolSpec],
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<AssistantReply> {
            *self.0.lock().unwrap() = msgs.first().map(|m| m.plain_text()).unwrap_or_default();
            Ok(AssistantReply {
                text: "siap".into(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 5,
                }),
            })
        }
    }

    let dir = std::env::temp_dir().join(format!("selencode_tg_hint_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let reg = default_registry();

    // CLI (telegram_chat_id = None): TIDAK boleh ada baris "Format Telegram:"
    let cli_sys = Arc::new(Mutex::new(String::new()));
    let mut cli_msgs = Vec::new();
    let cli_ctx = Ctx::new(dir.clone(), true);
    let _ = run_turn_counted(
        Arc::new(CaptureSysProv(cli_sys.clone())),
        &reg,
        &cli_ctx,
        &mut cli_msgs,
        "halo",
        2,
    )
    .await
    .unwrap();
    assert!(!cli_sys.lock().unwrap().contains("Format Telegram:"));

    // Telegram (telegram_chat_id = Some(999)): WAJIB ada 1 baris petunjuk ringan Telegram
    let tg_sys = Arc::new(Mutex::new(String::new()));
    let mut tg_msgs = Vec::new();
    let mut tg_ctx = Ctx::new(dir.clone(), true);
    tg_ctx.telegram_chat_id = Some(999);
    let _ = run_turn_counted(
        Arc::new(CaptureSysProv(tg_sys.clone())),
        &reg,
        &tg_ctx,
        &mut tg_msgs,
        "halo",
        2,
    )
    .await
    .unwrap();
    assert!(tg_sys.lock().unwrap().contains("Format Telegram:"));

    let _ = std::fs::remove_dir_all(&dir);
}
