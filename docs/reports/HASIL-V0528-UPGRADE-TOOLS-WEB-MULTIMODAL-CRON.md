# Laporan Rilis SelenCode v0.5.28 — Upgrade Suite 3 Besar Tool (Web v2, Core Search/Edit, Multimodal Read & Manage Cron)

- **Versi rilis:** `v0.5.28`
- **Jumlah butir:** 4
- **Catatan asal mekanisme:** Seluruh peningkatan tool di rilis ini merupakan implementasi internal SelenCode (Rust murni tanpa dependensi C tambahan; memakai `flate2` backend `rust_backend` untuk dekompresi stream `FlateDecode` PDF).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Menambahkan tool `web_search` (0 API key bawaan via DuckDuckGo HTML, GitHub Search, Hacker News Algolia, Wikipedia, plus dukungan backend opsional `[web]` SearXNG/Tavily/Brave/Firecrawl) dan meningkatkan `fetch_url` v2 dengan konversi HTML ke Markdown bersih (`html_to_markdown`), jalur cepat API per-domain (GitHub repo/issue/PR/blob, Wikipedia, Hacker News, RSS/Atom), dukungan batch 1–4 URL, serta pagu karakter deterministik 15.000 char dengan spill-to-disk ke `.selencode/web_cache/` yang dapat dibaca bertahap via `read_file` sambil mempertahankan seluruh pagar anti-SSRF.
- Butir 2 — LULUS: Meningkatkan `search_text` dengan parameter opsional `glob`, `case_insensitive`, `fixed_strings`, `context_lines` (`0..=5`, penggabungan rentang tumpang tindih), dan `files_only`; menambahkan filter `glob` pada `list_files`; serta menambahkan dukungan `replace_all` (tingkat atas maupun per-item di `edits[]`) pada `edit_file` dengan unified diff, persetujuan, dan `undo_stack`.
- Butir 3 — LULUS: Meningkatkan `read_file` agar mendukung berkas multimodal — gambar (PNG/JPEG/WebP/GIF dengan ekstraksi dimensi piksel & penyuntikan `Block::Image` lewat pesan `Role::User` di `src/agent/loop.rs` agar kompatibel dengan OpenAI/9Router, Anthropic, maupun Gemini) serta dokumen PDF (ekstraktor teks murni Rust untuk stream uncompressed maupun `/FlateDecode` via `flate2` dengan batas aman 4 MB/stream, ditambah fallback opsional `pdftotext` dan paginasi `start`/`end`).
- Butir 4 — LULUS: Membuka mesin penjadwalan persisten SQLite (`src/cron.rs`) ke agent melalui tool baru `manage_cron` (`list`, `add`, `enable`, `disable`, `remove`) pada toolset opsional `cron` — `list` bersifat baca-saja tanpa approval, seluruh mutasi mewajibkan persetujuan eksplisit pengguna, serta otomatis mewarisi `telegram_chat_id` aktif.

---

## Ringkasan Perubahan Kode

- `Cargo.toml`, `src/config.rs`, `config.example.toml`, `src/app/session.rs`: menambah crate murni Rust `flate2` (`rust_backend`), konfigurasi opsional `[web]` (`WebCfg`: `extract_backend`, `search_backend`, `searxng_url`, `api_key_env`, `max_extract_chars = 15000`), dan meneruskannya ke `Ctx` (termasuk `subagent.rs` & `orchestrator.rs`).
- `src/tools/web.rs`: mengimplementasikan `html_to_markdown`, `decode_html_entities`, `parse_rss_or_atom_feed`, `detect_fast_path`, `spill_web_output_if_needed`, `parse_ddg_html_results`, `FetchUrl` v2 (batch 1–4 URL + fast-path + optional `jina`/`firecrawl` + 15k spill-to-disk), dan tool `WebSearch`.
- `src/tools/search.rs` & `src/tools/file.rs`: menambah `compile_path_glob`/`path_matches_glob`, opsi `glob`/`case_insensitive`/`fixed_strings`/`context_lines`/`files_only` pada `SearchText`, `glob` pada `ListFiles`, `replace_all` (`replace_all_occurrences`) pada `EditFile`, serta pembacaan gambar (`extract_image_dimensions`, `load_read_file_image_block`) dan PDF (`extract_pdf_text_bytes` + fallback `pdftotext`) pada `ReadFile`.
- `src/agent/loop.rs`: menyisipkan pesan `Role::User` berisi `Block::Image` tepat sesudah `Role::Tool` apabila `read_file` berhasil memuat berkas gambar sehingga seluruh provider vision dapat melihat gambar secara langsung.
- `src/tools/cron.rs`, `src/tools/mod.rs`, `src/agent/plan.rs`, `src/tools/toolsets.rs`, `src/tool_display.rs`: menambah tool `ManageCron`, mendaftarkan `web_search` (termasuk `READ_ONLY_TOOLS` & `read_only_registry`) dan `manage_cron` (toolset `cron`), serta pratinjau argumen tampilan.
- `tests/v0528_tools_upgrade_suite.rs` & `.github/workflows/ci.yml`: menambah 4 uji integrasi mencakup seluruh 4 butir rilis dan mendaftarkan suite `v0.5.26`–`v0.5.28` di CI Windows x64.

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.27) | Sesudah (v0.5.28) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 59 suite (269 tes lulus) | 60 suite (273 tes lulus, 0 gagal) | LULUS |
| Unit & integration test khusus v0.5.28 (`v0528_tools_upgrade_suite`) | 0 tes | 4 tes (4 lulus, 0 gagal) | LULUS |
| Total tool bawaan terdaftar (Progressive Disclosure tetap 6 Core + `activate_toolset`) | 34 tool | 36 tool (+`web_search`, +`manage_cron`; Core tetap 6) | LULUS |
| Pagu default keluaran langsung `fetch_url` sebelum spill ke `.selencode/web_cache/` | 102400 byte (tanpa spill) | 15000 char (+ spill head/tail ke disk) | LULUS |
| Uji penerimaan live & regresi token A+B <= 5.000 / resume E <= 7.500 | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |

---

## Tidak Diubah

- Jumlah dan struktur Core Toolset (6 tool) & Resume Core Toolset (5 tool) tetap utuh sehingga anggaran token awal sesi ringan tidak membengkak (`web_search` dan `manage_cron` berada di extended toolset on-demand).
- Pagar anti-SSRF ketat pada `fetch_url` (`is_forbidden_ip`, cek DNS setiap hop redirect, persetujuan pengguna) tidak dilonggarkan.
- Cakupan v0.5.7–v0.5.27 (dua store skill + tulis-ke-asal + keterlihatan bayangan `/skills`, `review_runs` per-panggilan, `SELENCODE_TELEGRAM_API_BASE`, Selena improvement, penutup tunggal, gerbang verifikasi) tetap utuh tanpa regresi.
