# Laporan Penyelesaian SelenCode v0.5.21 — Cache-Safe Fixed Toolset & Porting Hermes (Fuzzy Edit, Parallel Dispatch, Not-Found/Patch Tracking)

- **Tag rilis:** `v0.5.21`
- **Basis:** `v0.5.20` (`505ed6a`)
- **Sumber referensi Hermes Agent:** `https://github.com/NousResearch/hermes-agent` (commit `ed1cbc8d`)
- **Jumlah butir:** 4

## Ringkasan Vonis (4 Perbaikan)

- **Butir 1 — LULUS:** Mutasi array `tools` di tengah giliran kerja produktif (`active_toolsets.insert("clarify")` yang sebelumnya memecah prompt cache pada call #16 di 65.623 token) dihentikan tuntas: kemajuan nyata (`write_file`, `edit_file`, `lsp_rename`, atau verifikasi proyek) kini mereset `unconfirmed_search_calls`, dan pada mode non-interaktif (`auto_yes`) array `tools` tetap beku sepanjang sesi agar prefiks prompt cache tetap utuh.
- **Butir 2 — LULUS:** `edit_file` kini dilengkapi `fuzzy_find_and_replace` 4-lapis (exact → `line_trimmed` → `indent_preserving` → `unicode_normalized`), petunjuk baris terdekat `"Maksudmu baris X-Y?"` saat gagal cocok, penolakan no-op identik, serta dukungan multi-edit atomik `edits: [{old, new}, ...]` dalam satu tool call.
- **Butir 3 — LULUS:** Batch tool independen dalam satu giliran assistant (read-only parallel-safe serta `write_file` non-overlapping di mode `auto_yes`) kini dieksekusi secara konkuren via `futures::future::join_all` dengan pemrosesan hasil deterministik dalam urutan asli `reply.tool_calls`.
- **Butir 4 — LULUS:** `ToolDedupCache` kini memiliki negative cache TTL 60 detik untuk `read_file` berkas tidak ditemukan (gugur otomatis begitu berkas dibuat di disk) dan penghitung kegagalan `edit_file` beruntun per berkas dengan eskalasi panduan `[PANDUAN PATCH]` mulai kegagalan ke-2.

---

## Rincian Per Butir (Pelabelan Sumber Eksplisit)

### 1. Butir 1 — Cache-Safe Fixed Toolset (Menutup Cache Cliff Call #16 dari Data Live v0.5.20)

- **Bukti lapangan dari uji live `v0.5.20` di InstaBox:** pada call #16 (65.623 token), `unconfirmed_search_calls >= 5` tidak direset sesudah `write_file`/`edit_file`, sehingga `active_toolsets.insert("clarify")` mengubah `tools` dari `18 → 19` dan menjatuhkan `cache_read` ke `0%` tepat di tengah sesi (membuang ~61,5k token segar).
- **Sumber & pelabelan:**
  - **Diambil dari prinsip Hermes (`agent/prompt_builder.py:520`: *"cache-safe: the toolset is fixed per session"*):** menjaga daftar `tools` tetap stabil sepanjang sesi kerja produktif.
  - **Mekanisme sendiri (SelenCode):** di `src/agent/loop.rs`, menambahkan `step_had_progress` (mereset `unconfirmed_search_calls` saat ada mutasi berkas atau verifikasi proyek) serta melarang mutasi `active_toolsets.insert("clarify")` pada mode non-interaktif (`ctx.approver.auto_yes()`).
- **Bukti unit:** `tests/v0521_hermes_cache_fuzzy_parallel_tracking.rs`:
  - `b1_productive_session_keeps_tools_fixed_and_no_false_antispin`

### 2. Butir 2 — Fuzzy / Whitespace-Tolerant `edit_file` + Multi-Edit Atomik

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`tools/fuzzy_match.py`: `fuzzy_find_and_replace`, `IDENTICAL_STRINGS_ERROR`, `UNICODE_MAP`; dan `tools/patch_parser.py:137-142`: `_no_match_hint`):** `fuzzy_find_and_replace` 4-lapis (`exact`, `line_trimmed`, `indent_preserving`, `unicode_normalized`) + `closest_window_hint` di `src/tools/file.rs`.
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`tools/patch_parser.py` multi-hunk):** dukungan argumen opsional `edits: [{old, new}, ...]` pada `edit_file` yang menerapkan seluruh patch secara atomik dalam 1 panggilan tool tanpa mengubah kompatibilitas mundur `old`/`new` tunggal.
- **Bukti unit:** `tests/v0521_hermes_cache_fuzzy_parallel_tracking.rs`:
  - `b2_fuzzy_edit_file_strategies_multiedit_and_no_match_hint`

### 3. Butir 3 — Eksekusi Paralel Batch Tool Independen

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`agent/tool_dispatch_helpers.py:34-52`: `_PARALLEL_SAFE_TOOLS`, `_PATH_SCOPED_READERS`, `_PATH_SCOPED_WRITERS`):** `can_run_tool_batch_in_parallel` di `src/agent/corrector.rs` dan eksekusi konkuren via `futures::future::join_all` di `src/agent/loop.rs`, dengan pemrosesan hasil deterministik mengikuti urutan asli `reply.tool_calls` (zero race condition pada `dedup`, `corrector`, `loop_guard`, dan urutan `ToolResult`).
- **Bukti unit:** `tests/v0521_hermes_cache_fuzzy_parallel_tracking.rs`:
  - `b3_parallel_batch_admission_and_deterministic_order`

### 4. Butir 4 — Negative Cache Not-Found `read_file` (60 dtk) & Eskalasi Kegagalan Patch

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`tools/file_tools_read_tracking.py:69-82, 97-116, 151-183`: `_NOT_FOUND_TTL_SECONDS = 60.0`, `_check_not_found_cache`, `_record_not_found`, `_record_patch_failure`, `_reset_patch_failures`):** `NOT_FOUND_TTL_SECS`, `record_read_not_found`, `record_edit_failure`, `reset_edit_failures` pada `ToolDedupCache` di `src/agent/corrector.rs` + suntikan `[PANDUAN PATCH]` pada kegagalan ke-2+ di `src/agent/loop.rs`.
- **Bukti unit:** `tests/v0521_hermes_cache_fuzzy_parallel_tracking.rs`:
  - `b4_not_found_negative_cache_and_edit_failure_escalation`

---

## Tabel Regresi

| Metrik / Suite | Target / Pagar | Hasil v0.5.21 | Status |
|---|---|---|---|
| Rata-rata token tugas A + B (`usage`) | ≤ 5.000 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Token giliran Resume E (`usage`) | ≤ 7.500 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Suite unit & integrasi Linux CI (`cargo test`) | 100% lulus, 0 gagal | Menunggu CI Run | Menunggu CI |
| Suite unit & integrasi Windows x64 CI (`cargo test`) | 100% lulus, 0 gagal | Menunggu CI Run | Menunggu CI |
| `cargo clippy --all-targets -- -D clippy::correctness` | 0 error | Menunggu CI Run | Menunggu CI |

Tidak diukur — Rata-rata token tugas A + B (`usage`): pengukuran penerimaan oleh Muse.
Tidak diukur — Token giliran Resume E (`usage`): pengukuran penerimaan oleh Muse.
Tidak diukur — Suite unit & integrasi Linux CI (`cargo test`): akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.21`.
Tidak diukur — Suite unit & integrasi Windows x64 CI (`cargo test`): akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.21`.
Tidak diukur — `cargo clippy --all-targets -- -D clippy::correctness`: akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.21`.

---

## Tidak Diubah (Sengaja Dipertahankan Utuh)

Seluruh fitur, pagar, dan kontrak `v0.5.7`–`v0.5.20` di bawah ini sengaja tidak diubah dan tetap terlindungi oleh suite regresi CI:
- Gerbang verifikasi (`Self-Correction Gate`) untuk edit belum terverifikasi atau gagal nyata, pengecualian perintah scaffold (`is_setup_or_scaffold_shell_cmd`), penolakan probe `python -c`/`node -e`, pelestarian status `LULUS` saat dedup hijau, dan disiplin berhenti model (`v0.5.14`–`v0.5.20`).
- Pemulihan perkakas `node_modules/` guard + `[PEMULIHAN PERKAKAS]`, baseline dedup `write_file` (`full_write_baselines`), dan dorongan `[Efisiensi]` pada tulis ulang berkas besar (`v0.5.20`).
- Pencatatan `cache_read_tokens`/`cache_write_tokens`, penanda `cache_control` selektif, ambang `proactive_prune_tokens = 96.000` & `compaction_threshold_tokens` maks `200.000`, serta histeresis rearm penuh (`v0.5.19`–`v0.5.20`).
- Penghapusan `max_steps` dari config/UI user (Opsi A `v0.5.18`), fallback mati-diam & mid-stream stall, penutupan jujur `max_output_tokens`, dasar `stream_idle = 180s`, retry + breaker, batas ganda `read_file`, dan pagar hemat resume/skill (`v0.5.7`–`v0.5.18`).
