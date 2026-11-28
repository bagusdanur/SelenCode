# Laporan Rilis SelenCode v0.5.29 — Pemilih `/model` Interaktif Telegram, Kartu `/new` Lintas Permukaan & Deteksi Context Window Persisten

- **Versi rilis:** `v0.5.29`
- **Jumlah butir:** 3
- **Catatan asal mekanisme:** Seluruh antarmuka inline keyboard Telegram, tampilan terminal TUI/Plain, kartu sesi `/new`, parser katalog `/models` multi-provider, serta cache SQLite context window merupakan implementasi internal SelenCode (Rust murni).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Merancang ulang tampilan `/model` dan `/provider` di Telegram menjadi kartu HTML bersih dengan tombol inline keyboard interaktif (`mdl:p:<page>`, `mdl:m:<pidx>:<page>`, `mdl:s:<pidx>:<midx>`, `mdl:x`, `mdl:noop`, seluruh payload <= 16 byte), penanda `✓` pada model & provider aktif, paginasi in-place via `edit_html_message_with_keyboard`, konfirmasi ganti model ringkas (`format_tg_model_switched_html`), serta merapikan `models_menu()` dan modal pemilih model TUI agar model aktif dan kapasitas konteks setiap model langsung terlihat jelas.
- Butir 2 — LULUS: Memperbarui perintah `/new` di seluruh permukaan — Telegram (`format_tg_new_session_html`), TUI terminal (`UiEvent::SessionReset` + `UiEvent::ContextInfo` + `UiEvent::Menu(s.new_session_card(&sid))`), dan Plain REPL (`new_session_card`) — agar langsung menampilkan kartu informasi sesi baru berisi ID sesi, model aktif, provider, kapasitas context window terdeteksi (`1,048,576 token (1M)`), dan folder kerja sehingga pengguna di terminal maupun Telegram langsung tahu sesi sudah berpindah.
- Butir 3 — LULUS: Meningkatkan deteksi kapasitas context window model AI lintas permukaan (TUI, Plain REPL, Telegram, compaction, dan router): penanda ukuran token berbasis batas kata (`has_context_size_tag`, bebas salah cocok seperti `v1mini`), cakupan keluarga model modern (`gemini`, `claude`, `gpt-4.1`, `gpt-5`, `gpt-oss`, `kimi-k2.5`, `qwen3-coder`, `glm-5`, `minimax-m2`, `devstral`, `mimo`), parser `/models` multi-provider (OpenAI-compatible `max_model_len`/`limits.context`/`inputTokenLimit`/`top_provider.context_length`, Gemini native `/v1beta/models`, dan Anthropic `/v1/models`), persistensi cache di SQLite `Store` (`load_model_context_cache` / `upsert_model_context_cache`), serta tampilan kapasitas konteks di layar sambutan TUI (`WelcomeInfo`), modal `/model` TUI, dan banner Plain REPL.

---

## Ringkasan Perubahan Kode

- `src/agent/context.rs`: menambahkan `has_context_size_tag`, memperluas `infer_model_context_limit`, serta menambahkan `format_tokens_with_commas` dan `format_context_tokens_exact`.
- `src/providers/openai.rs`, `src/providers/gemini.rs`, `src/providers/anthropic.rs`: menambahkan parser murni dan penarikan katalog model + context window (`parse_openai_models_response`, `parse_gemini_models_response`, `parse_anthropic_models_response`, `gemini::fetch_models_with_context`, `anthropic::fetch_models_with_context`).
- `src/store.rs`: menambahkan `load_model_context_cache`, `save_model_context_cache`, dan `upsert_model_context_cache` pada tabel `gateway_state` (pagu 512 entri).
- `src/telegram.rs` & `src/app/telegram_runner.rs`: menambahkan `TgModelCallback`, `parse_callback_model`, `build_tg_model_picker_card`, `build_tg_provider_picker_card`, `format_tg_model_switched_html`, `format_tg_new_session_html`, `TelegramClient::edit_html_message_with_keyboard`, handler callback `handle_tg_model_callback`, serta integrasi `/model`, `/provider`, `/new`, dan `/start`.
- `src/agent/ui.rs`, `src/tui/app.rs`, `src/tui/mod.rs`, `src/tui/moonlit.rs`, `src/app/tui_runner.rs`, `src/app/plain_runner.rs`, `src/app/session.rs`: menambahkan `UiEvent::ModelContexts`, `App::model_contexts` & `context_limit_for_model`, `WelcomeInfo::with_context_limit`, `plain_banner_with_context`, `Session::new_session_card`, tampilan kapasitas konteks pada modal `/model` TUI, serta kartu konfirmasi `/new` di TUI & Plain REPL.
- `tests/v0529_model_ui_new_card_context_detect.rs` & `.github/workflows/ci.yml`: menambahkan 5 uji integrasi mencakup seluruh 3 butir rilis dan mendaftarkannya pada CI Windows x64.

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.28) | Sesudah (v0.5.29) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 60 suite (273 tes lulus) | 61 suite (278 tes lulus, 0 gagal) | LULUS |
| Unit & integration test CI Windows x64 (`cargo test`) | lulus (60 suite) | lulus (61 suite, 0 gagal) | LULUS |
| Uji integrasi khusus v0.5.29 (`v0529_model_ui_new_card_context_detect`) | 0 tes | 5 tes (5 lulus, 0 gagal) | LULUS |
| Panjang maksimum `callback_data` tombol `/model` Telegram (batas Bot API 64 byte) | 0 byte (belum ada inline picker) | 16 byte (aman <= 64 byte) | LULUS |
| Jumlah permukaan menampilkan kartu info `/new` (model + konteks + sesi) | 0 permukaan | 3 permukaan (Telegram, TUI, Plain REPL) | LULUS |

- Tidak diukur — Pengukuran token live A+B / resume E: rilis v0.5.29 berfokus pada UI `/model` & `/new` serta deteksi context window tanpa mengubah prompt sistem maupun Core Toolset 6 tool.

---

## Tidak Diubah

- Jumlah dan komposisi Core Toolset (6 tool), Resume Core Toolset (5 tool), serta seluruh 36 tool bawaan dari `v0.5.28` tetap utuh.
- Pagar keamanan sandbox, approval tool, anti-SSRF `fetch_url`, whitelist Telegram, dan isolasi callback (`ap:`, `rv:`, `ask:`) tidak dilonggarkan; prefix baru `mdl:` terpisah total.
- Seluruh jaminan rilis `v0.5.7`–`v0.5.28` (dua store skill + tulis-ke-asal + bayangan `/skills`, `review_runs` per-panggilan, `SELENCODE_TELEGRAM_API_BASE`, dokumen Telegram, tool web v2 + multimodal `read_file` + `manage_cron`) tetap utuh tanpa regresi.
