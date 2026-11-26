# Laporan Hasil Perbaikan SelenCode v0.5.12 — Tampilan Telegram HTML Rapi di Layar HP

- **Versi rilis:** `v0.5.12`
- **Cakupan:** Hanya lapisan pengiriman/rendering Telegram gateway (`src/telegram.rs`, `src/app/telegram_runner.rs`, sentuhan prompt 1 baris khusus Telegram di `src/agent/loop.rs`); CLI/plain tidak berubah sama sekali.
- **Konfirmasi Gerbang Laporan Rilis:** Berkas laporan ini di-commit ke `docs/reports/` **sebelum** tag `v0.5.12` dibuat (memenuhi gerbang rilis otomatis `v0.5.11`).

---

## 1. Perbaikan 1 — Konverter Markdown → Telegram HTML + Jalur Kirim Tunggal + Fallback Anti-Gagal-Parse (**LULUS**)

### Ringkasan Perubahan
| Komponen | Berkas | Perubahan |
|---|---|---|
| **Konverter Markdown → Telegram HTML** | `src/telegram.rs` | Ekstraksi placeholder backtick sebelum parsing `**`/`__`/`*`/`~~` sehingga pola seperti ``**`FAILED`**`` atau ``**cek [link](...)**`` tidak pernah membocorkan bintang `**` mentah; `__tebal__` didukung; backtick URL **tidak pernah** dibungkus `<code>` (dibiarkan polos agar bisa disentuh di HP) dan kata status biasa (`SELESAI`, `FAILED`, `PROCESSING`, dll.) tidak dibungkus monospace kaku; tabel Markdown dikonversi menjadi tumpukan kunci-nilai (`• Kunci: nilai`); header `#`/`##`/`###` diberi jarak baris kosong sebelum/sesudah; escape HTML (`&`, `<`, `>`) penuh. |
| **Blok Kode Panjang → Lampiran Berkas** | `src/telegram.rs`, `src/app/telegram_runner.rs` | `extract_long_code_blocks_for_telegram` mengekstrak blok kode `>= 25` baris atau `>= 1.500` karakter menjadi `sendDocument` (`send_document_bytes`, nama berkas sesuai bahasa mis. `kode_1.py`/`kode_1.rs`) dengan 1 baris ringkasan di chat. |
| **Fallback Anti-Gagal-Parse** | `src/telegram.rs` | `send_html_message`, `send_html_message_with_keyboard`, & `edit_html_message` bila ditolak Telegram (HTTP 400 `"can't parse entities"`) mencatat `[telegram][render-fallback]` ke log dan mengirim ulang melalui `markdown_to_clean_plain_text` (nol `**`/backtick mentah, struktur baris utuh). |
| **Jalur Kirim Tunggal Gateway** | `src/app/telegram_runner.rs`, `src/telegram.rs` | Seluruh teks keluar Telegram — jawaban akhir, ringkasan penutup/timeout, laporan `watch_job`, laporan `cron`, error giliran, preview mengetik (`format_telegram_progress_html`), dan kartu `ask_user` (`TgAskHub::format_ask_card_html`) — melalui konverter HTML yang sama. |
| **Sentuhan Prompt Ringan Khusus Telegram** | `src/agent/loop.rs` | 1 baris petunjuk ringan (`Format Telegram: ...`) HANYA ditambahkan saat `ctx.telegram_chat_id.is_some()`. CLI/TUI/plain **100% tidak tersentuh**. |

---

## 2. Kumpulan 6 Contoh Sebelum / Sesudah (Persis Seperti yang Diterima Telegram)

Seluruh 6 contoh diverifikasi otomatis di `tests/telegram_html_rendering.rs::telegram_html_six_real_world_samples_no_raw_markdown` dengan `parse_mode: "HTML"`:

1. **Bold `**`/`__` membungkus backtick/status, italic, & coret:**
   - *Sebelum:* ``**Status akhir:** **`FAILED`** pada __tahap inpaint__ — cek *halaman 7* (~~lama~~ baru).``
   - *Sesudah (`parse_mode: "HTML"`):* `<b>Status akhir:</b> <b>FAILED</b> pada <b>tahap inpaint</b> — cek <i>halaman 7</i> (<s>lama</s> baru).`
2. **Backtick pada URL (dapat diklik) vs istilah teknis pendek (`<code>`):**
   - *Sebelum:* ``Rilis tersedia di `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11` atau [halaman rilis](https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11). Jalankan `cargo test` di `src/telegram.rs` untuk cek `#6c33`.``
   - *Sesudah (`parse_mode: "HTML"`):* `Rilis tersedia di https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11 atau <a href="https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11">halaman rilis</a>. Jalankan <code>cargo test</code> di <code>src/telegram.rs</code> untuk cek <code>#6c33</code>.`
3. **Tabel Markdown → tumpukan kunci-nilai untuk layar HP (0 pipa `|`):**
   - *Sebelum:*
     ```markdown
     | Job | Status | Progres |
     | --- | --- | --- |
     | `#6c33` | `SELESAI` | 22/22 |
     | `#7d11` | `FAILED` | 4/12 |
     ```
   - *Sesudah (`parse_mode: "HTML"`):*
     ```html
     • <b>Job:</b> <code>#6c33</code>
       – <b>Status:</b> SELESAI
       – <b>Progres:</b> 22/22

     • <b>Job:</b> <code>#7d11</code>
       – <b>Status:</b> FAILED
       – <b>Progres:</b> 4/12
     ```
4. **Header `#`/`##`/`###` berjarak baris kosong sebelum & sesudah:**
   - *Sebelum:* `Pembuka singkat.\n## Hasil Verifikasi\n- Baris satu\n- Baris dua`
   - *Sesudah (`parse_mode: "HTML"`):* `Pembuka singkat.\n\n<b>Hasil Verifikasi</b>\n\n• Baris satu\n• Baris dua`
5. **Blok kode panjang (`>= 25` baris / `>= 1.500` char) → dokumen lampiran + 1 baris ringkasan:**
   - *Sebelum:* blok Python 30 baris di dalam ```` ```python ... ``` ````
   - *Sesudah:* pesan chat `Lampiran berkas <code>kode_1.py</code> (30 baris, python) — dikirim sebagai dokumen di bawah.` + 1 permintaan `sendDocument` mengunggah `kode_1.py` utuh.
6. **Escape HTML penuh pada teks, hasil tool, kartu `ask_user`, & preview mengetik:**
   - *Sebelum:* ``Output tool: `if (a < b && c > 0)` dan teks <script>alert(1)</script> & **aman**.``
   - *Sesudah (`parse_mode: "HTML"`):* `Output tool: <code>if (a &lt; b &amp;&amp; c &gt; 0)</code> dan teks &lt;script&gt;alert(1)&lt;/script&gt; &amp; <b>aman</b>.`

---

## 3. Bukti Jalur Fallback Anti-Gagal-Parse & Cek Asap Regresi Ringan (**LULUS**)

- **Fallback HTTP 400 `"can't parse entities"` (`tests/telegram_html_rendering.rs::telegram_html_parse_error_400_falls_back_to_clean_plain_text_and_uploads_long_code`):**
  - Mock Telegram Bot API menolak `<b>Halo</b> <unclosed>` dengan HTTP 400 `Bad Request: can't parse entities: Unsupported start tag "unclosed"`.
  - Gateway mencatat `[telegram][render-fallback] HTML ditolak Telegram (...) — mengirim ulang sebagai teks polos bersih.` dan langsung mengirim ulang tanpa `parse_mode`: `Halo dunia\n• Baris aman & bersih` (nol `**` mentah, baris tetap rapi).
- **Cek asap CLI/plain & regresi token (uji ringan sesuai perintah pemilik — tidak menjalankan suite A/B/E penuh):**
  - `tests/telegram_html_rendering.rs::telegram_prompt_hint_only_appears_in_telegram_context_not_cli` membuktikan `Format Telegram:` **tidak** muncul di CLI (`telegram_chat_id = None`), sehingga ekonomi token CLI (`usage` baseline `v0.5.11`: Proyek A `4.825`, Proyek B `5.246`, Resume E `6.771` ≤ `7.500`) **tidak terpengaruh sama sekali** (0 perubahan pada prompt CLI/tool/loop).
  - Cek asap live 1 giliran tanya-jawab pada biner rilis `v0.5.12` memastikan alur normal tetap utuh.

---

## 4. Bagian yang Sengaja Tidak Diubah

- Seluruh struktur pesan baru & kartu persetujuan Telegram (`v0.5.0`), timeout & watchdog gateway + batas memori (`v0.5.10`), pantauan `watch_job` & status segar (`v0.5.10`), gerbang skill level loop & konsistensi muat-ulang (`v0.5.11`), store memory tunggal & migrasi (`v0.5.9`), lantai resume ramping (`v0.5.7`/`v0.5.11`), peringatan kunci config & gerbang laporan rilis (`v0.5.11`), pembatalan timeout 1× (`v0.5.11`), serta tampilan Markdown terminal CLI/plain apa adanya.

---

## 5. Bukti CI, Rilis Publik `v0.5.12`, & Smoke Check Biner Live (**LULUS**)

- **CI GitHub Actions (Linux + Windows x64):** Run `#45858670196` pada commit `1c8731d` — **LULUS** (`Cargo Test & Visual Render (Linux)` & `Cargo Test & Check (Windows x64)` keduanya `success`, termasuk 3 tes baru di `tests/telegram_html_rendering.rs`).
- **Release Workflow & Gerbang Laporan Rilis:** Run `#45860194831` pada tag `v0.5.12` — **LULUS** (5/5 job `success`: `Unit & Integration Tests`, `Linux x64`, `Termux (aarch64-linux-android)`, `Windows x64`, `GitHub Release`).
- **Halaman Rilis Publik:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.12 (`draft = false`, 3 aset lengkap):
  - `selen-linux-x64` (`16.868.688` byte)
  - `selen-termux-arm64` (`15.186.728` byte)
  - `selen-windows-x64.exe` (`15.565.312` byte)
- **Smoke Check Ringan Biner Rilis (`selen-windows-x64.exe` `v0.5.12`):**
  - `selen --version` → `selen 0.5.12`
  - 1 giliran tanya-jawab normal (`selen --yes run "Jawab satu kalimat singkat: apa fungsi HTML escape pada Telegram?"`) → menjawab akurat dalam 1 kalimat dengan Markdown CLI normal (`<`, `>`, `&` tetap dalam backtick tanpa tag HTML bocor ke CLI).
