# Laporan Penyelesaian SelenCode v0.5.19 — Disiplin Berhenti, Prompt Cache, & Status Dedup Verifikasi

- **Tag rilis:** `v0.5.19`
- **Basis:** `v0.5.18` (`b58e95c`)
- **Sumber referensi Hermes Agent:** `https://github.com/NousResearch/hermes-agent` (commit `ed1cbc8d`)
- **Jumlah butir:** 3

## Ringkasan Vonis (3 Masalah Utama)

- **Butir 1 — LULUS:** Disiplin berhenti dan verifikasi inkremental kini aktif penuh: perintah verifikasi majemuk SvelteKit/JS/TS (`npm run check && npm test && npm run build`, `svelte-check`, `vitest`, `jest`, `eslint`) dikenali resmi, edit kecil (`≤ 2` berkas) sesudah verifikasi hijau diarahkan ke verifikasi inkremental terbatas (tanpa mengulang suite penuh), pengulangan perintah verifikasi hijau dicegat dedup dengan dorongan `[DISIPLIN BERHENTI]` tanpa memicu alarm stagnasi palsu, dan pada pengulangan ke-3 sesi menutup bersih dengan status terverifikasi lulus.
- **Butir 2 — LULUS:** Dua akar masalah di sisi SelenCode yang membuat prompt cache tidak menempel dan selalu tercatat 0 kini diperbaiki tuntas: (a) mutasi prefiks bergeser per langkah (`prune_old_tool_results_with_archive(..., 20)`) dihapus dari sesi non-resume dan prune proaktif kini memakai landasan histeresis `proactive_prune_rearm_tokens` (pemicu 28.000 / min reclaim 4.096) serta penanda amplop `cache_control` selektif untuk rute Claude/Kimi/Qwen, dan (b) parser respons OpenAI/Anthropic/Gemini + skema SQLite `usage` + `/usage` kini mem-parsing, menyimpan, dan menampilkan `cache_read_tokens` & `cache_write_tokens` secara penuh.
- **Butir 3 — LULUS:** `SelfCorrectState::observe_tool_at_step` kini mengenali keluaran dedup dari perintah verifikasi yang baru saja lulus (`is_dedup_passed_verify_output`) untuk `run_shell` maupun `verify_project`, sehingga `last_verify_passed` tetap `Some(true)` dan status penutup melaporkan `LULUS` (tidak pernah terbalik menjadi `[Status Verifikasi Akhir — GAGAL]` maupun memicu putaran self-correction palsu), sementara kegagalan verifikasi nyata tetap ditahan dan dilaporkan jujur sebagai `GAGAL`.

---

## Rincian Per Masalah (Pelabelan Sumber Eksplisit)

### 1. Masalah 1 — Tidak ada disiplin berhenti (±1,7 juta token terbakar sesudah aplikasi selesai)

- **Akar masalah di v0.5.18 (dari data tes banding KASKU & audit kode):**
  1. `is_verification_shell_cmd` dan `verify_runner_family` di `src/agent/corrector.rs` tidak mengenali perintah `npm run check`, `svelte-check`, `vitest`, `npm run build`, maupun perintah majemuk `npm run check && npm test && npm run build`. Akibatnya, sesudah aplikasi lulus penuh, verifikasi ulang atau verifikasi inkremental tidak terklasifikasi dengan tepat.
  2. Ketika model mengulang perintah verifikasi identik sesudah lulus, `ToolDedupCache` mengembalikan stub `[Dedup: ... baru saja LULUS ...]` tanpa `exit code: 0` → `SelfCorrectState` salah menandai `last_verify_passed = Some(false)` → memicu 3 putaran Self-Correction Gate penuh + 5× deteksi loop/stagnasi + pengulangan `check+test+build` berulang kali.
  3. Tidak ada jalur verifikasi inkremental untuk edit kecil sesudah build+tes sudah hijau, dan tidak ada penghenti konvergensi bersih bila model berulang kali mencoba menjalankan ulang perintah verifikasi yang sudah lulus tanpa perubahan berkas baru.
- **Sumber & pelabelan:**
  - **Mekanisme sendiri (SelenCode):** klasifikasi perintah majemuk (`split_compound_shell_commands`), pengenalan runner JS/TS/SvelteKit (`npm_check`, `npm_build`, `npm_lint`, `vitest`, `svelte-check`), mode verifikasi inkremental sesudah verifikasi hijau (`has_passed_green_verify`, `edits_since_last_green_verify <= 2`) di `src/agent/corrector.rs`, aturan disiplin berhenti 1 baris pada `build_system_prompt` (`src/agent/context.rs`), serta konvergensi bersih `green_verify_dedup_hits` (`>= 2` suntik `[DISIPLIN BERHENTI]`, `>= 3` tutup bersih terverifikasi) di `src/agent/loop.rs` tanpa memberi penalti ke `LoopGuard`.
  - **Tetap mempertahankan pagar v0.5.15:** probe satu-baris (`python -c`, `node -e`) tetap ditolak keras sebagai verifikasi sah.
- **Bukti unit:** `tests/v0519_stop_discipline_prompt_cache_dedup_status.rs`:
  - `m1_compound_svelte_verify_and_incremental_verify_after_small_edit`
  - `m1_repeated_green_verify_converges_cleanly_without_stagnation_loop`

### 2. Masalah 2 — Prompt cache tidak menempel di Selen (0 cache read vs Hermes 85,6%)

- **Penyelidikan terpisah: (a) Bentuk request & pencatatan SelenCode vs (b) Jalur pengiriman (relay lokal `127.0.0.1:20129/v1` vs langsung 9Router):**
  1. **Terbukti dari kode — Bug #1 SelenCode (buta token cache):** `src/providers/openai.rs` (yang melayani 9Router / OpenAI-compatible) sebelumnya HANYA membaca `prompt_tokens` dan `completion_tokens` — kolom `prompt_tokens_details.cached_tokens`, `cache_read_input_tokens`, `prompt_cache_hit_tokens`, dan `cached_tokens` **tidak pernah di-parse**. Demikian pula `src/providers/anthropic.rs`, `src/providers/gemini.rs` (`cachedContentTokenCount`), `Usage` (`src/providers/mod.rs`), dan tabel SQLite `usage` (`src/store.rs`) tidak memiliki kolom `cache_read_tokens` / `cache_write_tokens`. **Artinya, walaupun Gemini/9Router melayani cache implisit, catatan `usage` Selen v0.5.18 secara matematis PASTI 0.**
  2. **Terbukti dari kode — Bug #2 SelenCode (perusakan prefiks tiap langkah):** Pada `src/agent/loop.rs:1262-1264` (v0.5.18), `prune_old_tool_results_with_archive(messages, &prune_dir, 20)` dijalankan pada **setiap langkah** begitu `messages.len() > 20` — menggeser batas pangkas maju 1–2 pesan setiap panggilan (panggilan #11 sampai #77), yang mengubah isi pesan awal dan **mematahkan prompt cache prefiks provider di setiap panggilan**! Selain itu, prune proaktif (pemicu lama `18.000`, tanpa landasan histeresis) menyala **16×** di larian KASKU, makin sering memotong prefiks dan memaksa model membaca ulang berkas (33 baca vs Hermes 9 baca).
  3. **Temuan source Hermes (`scratch/hermes-agent`, commit `ed1cbc8d`) untuk model `ag/gemini-4-hydrogen-medium`:**
     - Di `agent/agent_runtime_helpers.py:1322-1434` (`anthropic_prompt_cache_policy`), Hermes **TIDAK** mengirim marker `cache_control` untuk model Gemini pada rute OpenAI-wire (mengembalikan `(False, False)` agar relay/provider tidak menolak HTTP 400). Hermes meraih 85,6% cache read pada Gemini murni melalui **automatic implicit prefix caching** berkat **prefiks pesan yang stabil byte-identik** antar-panggilan (didukung histeresis `_proactive_prune_rearm_tokens` di `agent/context_compressor_prune.py:148-200`) + pencatatan `prompt_tokens_details.cached_tokens` (`agent/usage_pricing.py:687-693`).
     - Untuk model **Claude / Kimi / Qwen** pada gateway OpenAI-wire pendukung (OpenRouter/Nous/Alibaba/9Router), Hermes mengirim marker amplop `cache_control: {"type": "ephemeral"}` hingga 4 breakpoint (`agent/prompt_caching.py:503-538`).
  4. **Pemisahan jujur terhadap jalur relay `127.0.0.1:20129/v1` vs langsung 9Router:** Bukti kode di atas memastikan Bug #1 + Bug #2 di SelenCode sendiri sudah cukup untuk menyebabkan `cache_read = 0` di seluruh catatan `usage` v0.5.18 bahkan seandainya jalur jaringan identik. Apakah relay lokal VM di `127.0.0.1:20129/v1` meneruskan field `prompt_tokens_details.cached_tokens` pada respons SSE tanpa melucutinya tidak dapat dibuktikan secara lokal tanpa menyentuh infrastruktur VM Muse — hal tersebut kini dapat langsung diverifikasi secara transparan pada uji ulang live oleh Muse melalui kolom baru `usage.cache_read_tokens` dan tampilan `/usage`.
- **Sumber & pelabelan perubahan di SelenCode:**
  - **Diambil dari Hermes (`agent/usage_pricing.py:687-693`, `_CHAT_USAGE_SHAPE`):** `parse_openai_usage` di `src/providers/openai.rs` mengekstrak `prompt_tokens_details.cached_tokens`, `cache_read_input_tokens`, `prompt_cache_hit_tokens`, `cached_tokens`, serta varian `cache_write_tokens` / `cache_creation_input_tokens`; ditambah `cachedContentTokenCount` di `src/providers/gemini.rs` dan `cache_read_input_tokens` / `cache_creation_input_tokens` di `src/providers/anthropic.rs`.
  - **Diambil dari Hermes (`agent/agent_runtime_helpers.py:1322-1434` & `agent/prompt_caching.py:503-538`):** `should_apply_openai_envelope_cache_control` dan `apply_openai_envelope_cache_control` di `src/providers/openai.rs` (maks 4 breakpoint `ephemeral` hanya untuk Claude/Kimi/Qwen pada gateway pendukung; nonaktif pada Gemini/OpenAI/localhost).
  - **Diambil dari Hermes (`agent/context_compressor_prune.py:148-200` & `agent/context_compressor.py:2669, 3178`):** histeresis landasan `proactive_prune_rearm_tokens` (termasuk reset ke `0` sesudah `compact_messages_hermes`) di `src/agent/loop.rs`, serta penaikan `proactive_prune_min_reclaim_tokens` bawaan ke `4.096` di `src/config.rs`.
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`agent/context_compressor_prune.py`):** penghapusan rolling prune `len - 20` pada sesi non-resume (mempertahankan `len - 2` khusus `lean_resume && step >= 2` agar pagar Resume E ≤ 7.500 tetap aman), penaikan pemicu bawaan `proactive_prune_tokens` dari `18.000 → 28.000` (memberi ruang stabil bagi jendela kerja tanpa thrashing 16×), serta migrasi otomatis kolom `cache_read_tokens` / `cache_write_tokens` pada tabel SQLite `usage` (`src/store.rs`) dan tampilan rasio cache hit di `/usage` (`src/app/commands.rs`).
- **Bukti unit:** `tests/v0519_stop_discipline_prompt_cache_dedup_status.rs`:
  - `m2_provider_cache_token_parsing_and_sqlite_persistence`
  - `m2_openai_envelope_cache_control_policy_and_markers`
  - `m2_prefix_stability_across_long_session_and_rearm_hysteresis`

### 3. Masalah 3 — Status gerbang verifikasi salah melaporkan "GAGAL" karena dedup

- **Akar masalah di v0.5.18:** `ToolDedupCache::check_hit` mengembalikan stub `"[Dedup: perintah '...' baru saja LULUS tanpa perubahan berkas baru ...]"` (`is_error = false`). Karena stub tidak mengandung `"exit code: 0"`, `SelfCorrectState::observe_tool_at_step` menghitung `parse_exit_code(output) == None` → `ok = false` → menimpa `last_verify_passed = Some(false)`, sehingga gerbang penutup mencetak `[Status Verifikasi Akhir — GAGAL]` meski perbaikan benar dan sudah terverifikasi lulus.
- **Sumber & pelabelan:**
  - **Mekanisme sendiri (SelenCode):** fungsi `is_dedup_passed_verify_output` di `src/agent/corrector.rs` dipakai di cabang `"verify_project"` dan `"run_shell"` pada `SelfCorrectState::observe_tool_at_step` untuk mempertahankan `last_verify_passed = Some(true)`, `verified_after_last_edit = true`, `last_verify_exit_code = self.last_verify_exit_code.or(Some(0))`, dan `last_verify_output = None`, serta tidak menganggap dedup hijau sebagai kegagalan di `LoopGuard`. Kegagalan verifikasi nyata (`exit code != 0` / `is_error == true`) tetap tercatat `Some(false)` dan tetap ditahan oleh `Self-Correction Gate`.
- **Bukti unit:** `tests/v0519_stop_discipline_prompt_cache_dedup_status.rs`:
  - `m3_dedup_on_passed_verify_preserves_lulus_status_and_never_reports_gagal`

---

## Tabel Regresi

| Metrik / Suite | Target / Pagar | Hasil v0.5.19 | Status |
|---|---|---|---|
| Rata-rata token tugas A + B (`usage`) | ≤ 5.000 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Token giliran Resume E (`usage`) | ≤ 7.500 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Tes banding ulang KASKU (total token, panggilan, berhenti sendiri, 45/45, `cache_read_tokens`) | Turun drastis dari 2,55 juta, selesai sendiri sebelum pagu, 45/45 + build hijau, `cache_read_tokens` tercatat | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Suite unit & integrasi Linux CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45984429127`, job `140456286171`) | LULUS |
| Suite unit & integrasi Windows x64 CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45984429127`, job `140456286775`) | LULUS |
| `cargo clippy --all-targets -- -D clippy::correctness` | 0 error | 0 error (CI Run `45984429127`) | LULUS |

Tidak diukur — Rata-rata token tugas A + B (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Token giliran Resume E (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Tes banding ulang KASKU (total token, panggilan, berhenti sendiri, 45/45, `cache_read_tokens`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).

---

## Tidak Diubah (Sengaja Dipertahankan Utuh)

Seluruh fitur, pagar, dan kontrak `v0.5.7`–`v0.5.18` di bawah ini sengaja tidak diubah dan tetap terlindungi oleh suite regresi CI:
- Keberadaan gerbang verifikasi (`Self-Correction Gate`) untuk tugas yang **belum** terverifikasi atau verifikasinya **benar-benar gagal**, penolakan probe `python -c` / `node -e`, kalimat kegagalan jujur `ZERO_EDIT_FAILURE_HEADER`, dan catatan berkas tes diubah (`v0.5.14`–`v0.5.16`).
- Penghapusan `max_steps` dari config/UI user (Opsi A `v0.5.18`) dengan batas internal tetap khusus subagent/orchestrator/cron latar (`CHILD_MAX_STEPS`), serta notice konvergensi tanpa edit (`no_edit_convergence_steps`).
- Porting ketahanan stream & fallback `v0.5.17`–`v0.5.18`: fallback chain untuk kelas mati-diam & mid-stream stall, penutupan jujur + pembatalan aman tool call terpotong `max_output_tokens`, dasar `stream_idle = 180s` + skalasi konteks (`> 50k → 240s`, `> 100k → 300s`), retry jittered backoff + diet pra-retry + breaker, batas ganda `read_file`, dan gerbang prune pasca-tool.
- Pagar hemat sesi kecil & resume: `build_resume_snapshot`, `build_resume_system_prompt`, `prune_old_skill_results_with_archive` (prune dini skill), dan `prune_old_tool_results_with_archive(..., 2)` khusus `lean_resume && step >= 2`.
- `TypingLoopGuard` Telegram, klasifier nomina permintaan ubah berkas, konverter Markdown→HTML Telegram, watchdog polling gateway, gerbang skill harness + opt-out eksplisit, memori persisten, `ask_user`, `cron`, `session_search`, `watch_job`, dan sandbox/approval.
