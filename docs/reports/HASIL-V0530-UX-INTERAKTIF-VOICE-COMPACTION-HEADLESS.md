# Laporan Rilis SelenCode v0.5.30 — UX Interaktif Penuh Telegram & TUI, Transkripsi Suara, Compaction Cerdas LLM & Fallback Headless `fetch_url`

- **Versi rilis:** `v0.5.30`
- **Jumlah butir:** 4
- **Catatan asal mekanisme:** Seluruh antarmuka inline keyboard Telegram, transkripsi audio Rust murni, modal `/resume` TUI, auto-probe context window latar, compaction ringkasan LLM, dan fallback headless Chrome pada `fetch_url` merupakan implementasi internal SelenCode (tanpa dependensi C tambahan).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Melengkapi UX Telegram interaktif penuh dengan tombol inline keyboard (`<= 64` byte) untuk `/resume` & `/sessions` (`res:s`, `res:new`, `res:x`), saklar 1-tap `/approve` & `/sandbox` (`cfg:ap`, `cfg:sb`), tombol cepat `/cron` (`crn:t` toggle & `crn:d` hapus), pratinjau `/skills` (`skl:v`), tombol `📡 Katalog API` langsung di dalam picker `/model` (`mdl:c` & `mdl:a`), mode `/plan` dengan tombol `[ ✅ Jalankan Rencana ]` / `[ ✕ Batalkan ]` (`pln:run`/`pln:x`), perintah pindah folder `/cd` lintas permukaan, serta ekstraksi & transkripsi pesan suara/audio Telegram (`extract_tg_voice_or_audio` + `transcribe_audio_bytes` untuk OpenAI `/audio/transcriptions` dan Gemini `inlineData`, sambil menjaga kompatibilitas penuh uji `v0.5.26`).
- Butir 2 — LULUS: Meningkatkan UX terminal TUI & Plain REPL dengan modal `/resume` interaktif bergaya Moonlit (`src/tui/resume_modal.rs` — filter pencarian judul live, `↑/↓`, `Enter` lanjut, `Ctrl+N` sesi baru, `Del`/`Ctrl+D` hapus), auto-probe latar kapasitas context window model dari API saat startup (non-blocking 0 ms cold start + tersimpan di cache SQLite), perintah `/cd` sinkron dengan re-scan `@file`, serta tampilan `/status` dengan bilah progres visual Unicode (`render_progress_bar`) dan metrik cache read/write.
- Butir 3 — LULUS: Menambahkan compaction terstruktur berbantuan LLM (`compact_messages_with_llm` di `src/agent/context.rs` dengan tenggat 15 detik, ringkasan 4 bagian terstruktur, penjagaan pasangan `ToolUse`/`ToolResult`, dan fallback deterministik aman) yang terhubung ke `/compact` di seluruh permukaan, serta deteksi shell SPA (`detect_spa_shell`) dan fallback opsional browser lokal headless Chrome/Chromium/Edge (`find_headless_chrome_binary` + `js_render` / `[web].headless_browser = true`) pada `fetch_url` setelah verifikasi anti-SSRF.
- Butir 4 — LULUS: Membersihkan warning kompilasi (`thiserror` tidak terpakai di `Cargo.toml` dan 7 impor tidak terpakai di file uji integrasi), memecah logika UI besar menjadi submodul terpisah (`src/app/telegram_ui.rs` dan `src/tui/resume_modal.rs`), serta menambahkan 4 uji integrasi baru di `tests/v0530_interactive_ui_voice_compaction_headless.rs` yang lulus 100% di CI Linux & Windows x64.

---

## Ringkasan Perubahan Kode

- `src/telegram.rs`, `src/app/telegram_ui.rs`, `src/app/telegram_runner.rs`: menambahkan seluruh parser callback & kartu inline baru (`mdl:c/a`, `res:`, `cfg:`, `crn:`, `skl:`, `pln:`), `TgUiSharedState`, `extract_tg_voice_or_audio`, `transcribe_audio_bytes` (dengan encoder base64 Rust murni), serta dukungan `/plan`, `/cd`, dan auto-probe context window latar.
- `src/tui/resume_modal.rs`, `src/tui/app.rs`, `src/tui/mod.rs`, `src/agent/ui.rs`, `src/app/tui_runner.rs`, `src/app/plain_runner.rs`, `src/app/session.rs`, `src/app/commands.rs`, `src/repl.rs`: menambahkan `SlashCommand::Cd`, `ResumePickerState` & modal `/resume` TUI, `UiEvent::ResumePicker` & `UiEvent::CwdChanged`, `Session::handle_cd`, `Session::compact_now_smart`, `Session::background_context_probe_params`, dan bilah progres visual pada `/status`.
- `src/agent/context.rs`: menambahkan `render_progress_bar` dan `compact_messages_with_llm`.
- `src/tools/web.rs` & `src/config.rs`: menambahkan `detect_spa_shell`, `find_headless_chrome_binary`, `fetch_via_headless_chrome`, argumen opsional `js_render` pada `fetch_url`, `WebCfg.headless_browser`, dan `TelegramCfg.voice_transcription`.
- `Cargo.toml`, `tests/*.rs`, `tests/v0530_interactive_ui_voice_compaction_headless.rs`, `.github/workflows/ci.yml`: membersihkan dependensi & impor tidak terpakai, menambah 4 uji integrasi v0.5.30, dan mendaftarkannya di CI Windows x64.

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.29) | Sesudah (v0.5.30) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 61 suite (278 tes lulus) | 62 suite (282 tes lulus, 0 gagal) | LULUS |
| Unit & integration test CI Windows x64 (`cargo test`) | lulus (61 suite) | lulus (62 suite, 0 gagal) | LULUS |
| Uji integrasi khusus v0.5.30 (`v0530_interactive_ui_voice_compaction_headless`) | 0 tes | 4 tes (4 lulus, 0 gagal) | LULUS |
| Jumlah perintah Telegram dengan kontrol tombol inline interaktif | 3 perintah (`/model`, `/provider`, `/review`) | 9 perintah (+`/resume`, `/sessions`, `/approve`, `/sandbox`, `/cron`, `/skills`, `/plan`, +Katalog API) | LULUS |
| Warning impor tidak terpakai di file tes lama & dependensi manifest (`thiserror`) | 8 warning | 0 warning (seluruh 8 dibersihkan) | LULUS |

- Tidak diukur — Pengukuran token live A+B / resume E: rilis v0.5.30 tidak mengubah prompt sistem maupun komposisi Core Toolset 6 tool sehingga anggaran token awal sesi ringan tetap terjaga.

---

## Tidak Diubah

- Komposisi Core Toolset (6 tool), Resume Core Toolset (5 tool), serta seluruh 36 tool bawaan tetap utuh tanpa menambah beban token definisi tool pada sesi baru.
- Pagar keamanan anti-SSRF pada `fetch_url` (`check_url_allowed` sebelum jaringan maupun sebelum headless Chrome), whitelist Telegram, sandbox workspace, dan isolasi seluruh prefix callback (`ap:`, `rv:`, `ask:`, `mdl:`, `res:`, `cfg:`, `crn:`, `skl:`, `pln:`) tetap ketat.
- Seluruh jaminan rilis `v0.5.7`–`v0.5.29` (termasuk perilaku `classify_tg_message` pada `v0.5.26`, skill shadow visibility `v0.5.27`, tools suite `v0.5.28`, dan pemilih `/model` + kartu `/new` `v0.5.29`) tetap lulus penuh tanpa regresi.
