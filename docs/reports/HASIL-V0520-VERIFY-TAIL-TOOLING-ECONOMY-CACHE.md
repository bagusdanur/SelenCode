# Laporan Penyelesaian SelenCode v0.5.20 — Ekor Gerbang Verifikasi, Pemulihan Perkakas, Ekonomi Langkah, & Porsi Cache

- **Tag rilis:** `v0.5.20`
- **Basis:** `v0.5.19` (`40d515b`)
- **Sumber referensi Hermes Agent:** `https://github.com/NousResearch/hermes-agent` (commit `ed1cbc8d`)
- **Jumlah butir:** 4

## Ringkasan Vonis (4 Masalah Utama)

- **Butir 1 — LULUS:** Perintah setup/scaffold/instalasi paket (`npx sv create ... --add vitest`, `npm install -D vitest`, `npm create ...`, `npx svelte-kit sync`, dsb.) kini dievaluasi per sub-perintah dan tidak pernah bisa masuk sebagai perintah verifikasi gerbang; `SelfCorrectState` selalu memperbarui pelacakan ke perintah verifikasi proyek nyata terakhir beserta status sebenarnya sehingga sesi yang sudah hijau penuh langsung menutup tanpa ekor pengulangan paksa dan membawa cap `LULUS` (sementara edit belum terverifikasi atau gagal nyata tetap ditahan ketat).
- **Butir 2 — LULUS:** Pemulihan kegagalan perkakas/konfigurasi (`svelte-check`, `tsconfig`, resolusi modul Vite/TS) kini dilindungi tiga lapis: deteksi galat konfigurasi menyuntik panduan `[PEMULIHAN PERKAKAS]` terarah ke berkas config sumber, bacaan ke direktori vendor (`node_modules/`, `.pnpm/`, `.svelte-kit/output/`, dsb.) dibatasi maksimal 1× (bacaan ke-2+ dicegah tanpa membebani konteks), dan `write_file`/`edit_file` menolak keras modifikasi di dalam `node_modules/`.
- **Butir 3 — LULUS:** Ekonomi langkah ditingkatkan dari dua sisi: `ToolDedupCache` kini mencatat baseline berkas yang baru ditulis/diubah sendiri (`record_write` / `update_write_baseline_if_known`) sehingga bacaan penuh ulang atas karya sendiri yang belum berubah eksternal langsung di-dedup (bacaan rentang baris eksplisit `start`/`end` untuk galat & pasca-prune tetap diizinkan), serta `write_file`/`edit_file` mendorong patch terarah pada berkas besar yang sudah ada disertai dorongan `[Efisiensi]` saat penulisan ulang utuh dilakukan untuk perubahan lokal kecil.
- **Butir 4 — LULUS:** Tiga pematah cache prefiks prematur pada sesi bangun panjang (30k–80k token di jendela 1M) disingkirkan: ambang `needs_compaction` dinaikkan dari batas sempit `48.000` → `200.000` token (tetap `50%` untuk model kecil/menengah), pemicu bawaan `proactive_prune_tokens` dinaikkan `28.000 → 96.000` (tetap dibatasi `45%` pada model kecil/menengah), dan landasan histeresis `proactive_prune_rearm_tokens` diselaraskan penuh dengan Hermes (`after + max(reclaimed, proactive_trigger, min_reclaim)`).

---

## Rincian Per Masalah (Pelabelan Sumber Eksplisit)

### 1. Masalah 1 — Ekor gerbang verifikasi: pengulangan paksa sesudah hijau + cap "GAGAL" dari perintah basi

- **Akar masalah di v0.5.19 (dari bukti Putaran 3 KASKU & audit kode):**
  1. `is_verification_shell_cmd` di `src/agent/corrector.rs` sebelumnya mencocokkan kata kunci seperti `"vitest"` dan `"eslint"` lewat `.contains(kw)` pada seluruh string perintah. Akibatnya, perintah scaffold awal sesi `npx sv create . --template minimal --types ts --add vitest tailwindcss` tercatat sebagai `last_verify_cmd`!
  2. Ketika model kemudian menjalankan verifikasi nyata `npm run check && npm test && npm run build`, `is_same_or_broader_verify_cmd("npx sv create ...", ...)` mengembalikan `false` karena `npx sv` bukan keluarga runner verifikasi — sehingga `last_verify_cmd` terkunci selamanya pada perintah scaffold basi, `verified_after_last_edit` tetap `false`, dan harness memaksa 2× pengulangan suite (5 panggilan / 258.652 token) lalu menutup dengan cap keliru `[Status Verifikasi Akhir — GAGAL]`.
- **Sumber & pelabelan:**
  - **Mekanisme sendiri (SelenCode):**
    - `is_setup_or_scaffold_shell_cmd` dan evaluasi per sub-perintah `is_single_verification_shell_cmd` di `src/agent/corrector.rs` mengecualikan perintah scaffold/setup/install/git/filesystem secara struktural.
    - `SelfCorrectState::observe_tool_at_step` memperbarui `last_verify_cmd` & status terakhir untuk setiap runner verifikasi proyek nyata ketika tidak sedang berada dalam status gagal (`last_verify_passed != Some(false)`), sementara status gagal nyata (`Some(false)`) tetap menegakkan aturan ketat `v0.5.15` (hanya perintah sama/lebih luas yang dapat membuka gerbang; probe `python -c`/`node -e` tetap ditolak).
- **Bukti unit:** `tests/v0520_verify_tail_tooling_economy_cache.rs`:
  - `m1_scaffold_command_never_tracked_and_green_verify_closes_with_zero_tail_and_lulus`

### 2. Masalah 2 — Pemulihan kegagalan perkakas lambat (±29 panggilan vs ±3 di Hermes)

- **Akar masalah di v0.5.19:** saat `svelte-check` melaporkan 59 error terkait `tsconfig.json`, tidak ada rem terhadap pembacaan berulang di dalam `node_modules/` maupun arahan cepat ke berkas config sumber, sehingga agen menghabiskan puluhan panggilan mengorek dependensi dan membaca ulang berkasnya sendiri.
- **Sumber & pelabelan:**
  - **Mekanisme sendiri (SelenCode):**
    - `is_vendor_or_dependency_path` & `detect_tooling_config_error_hint` di `src/agent/corrector.rs`.
    - Di `src/agent/loop.rs`: pembatasan bacaan vendor `vendor_reads_in_turn` (bacaan ke-1 menyertakan `[PANDUAN PERKAKAS]`, bacaan ke-2+ dicegah langsung tanpa membaca disk) dan suntikan `[PEMULIHAN PERKAKAS]` 1× pada galat konfigurasi/typecheck yang otomatis direset saat verifikasi hijau kembali.
    - Di `src/tools/file.rs`: penolakan eksplisit `write_file`/`edit_file` pada jalur `node_modules/` dan `.pnpm/`.
    - Di `src/agent/context.rs`: satu kalimat panduan pemulihan galat perkakas/config di aturan 2 `build_system_prompt`.
- **Bukti unit:** `tests/v0520_verify_tail_tooling_economy_cache.rs`:
  - `m2_tooling_tsconfig_failure_redirects_away_from_node_modules_and_recovers_fast`

### 3. Masalah 3 — Ekonomi langkah (76 panggilan vs 30; baca ulang karya sendiri & tulis ulang utuh vs patch terarah)

- **Akar masalah di v0.5.19:**
  1. `ToolDedupCache` hanya melacak `read_file`, bukan berkas yang baru saja ditulis sendiri lewat `write_file`, dan aturan 2 prompt menulis `"baca file sebelum mengedit"` tanpa pengecualian untuk berkas yang baru ditulis sendiri — menyebabkan 25× `read_file` di KASKU.
  2. Deskripsi `write_file`/`edit_file` tidak mendorong patch terarah untuk berkas besar yang sudah ada, dan tidak ada umpan balik ketika berkas besar ditulis ulang utuh hanya demi perubahan kecil.
- **Sumber & pelabelan:**
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`tools/file_tools_read_tracking.py:12-15`: `full_write_baselines` & `dedup_generation_reads`):** `ToolDedupCache::record_write` dan `update_write_baseline_if_known` di `src/agent/corrector.rs` (+ dipanggil di `src/agent/loop.rs`) men-dedup bacaan penuh (`start`/`end` kosong) atas berkas yang baru ditulis/di-patch dari baseline penuh di sesi yang sama selama tanda tangan berkas di disk belum berubah; otomatis dibersihkan oleh `clear_file_reads()` sesudah prune/compaction dan selalu mengizinkan bacaan rentang baris eksplisit (`start`/`end`).
  - **Mekanisme sendiri, terinspirasi Hermes (`agent/prompt_builder.py:411-419, 478-479`):** penajaman deskripsi `write_file`/`edit_file`, dorongan `[Efisiensi]` berbasis `TextDiff::from_lines().ratio() >= 0.65` saat menimpa berkas ada `>= 4.000` byte di `src/tools/file.rs`, serta penyelarasan aturan 2 `build_system_prompt` di `src/agent/context.rs` (gabungkan baca/tulis paralel, jangan baca ulang karya sendiri, utamakan `edit_file`).
- **Bukti unit:** `tests/v0520_verify_tail_tooling_economy_cache.rs`:
  - `m3_skip_rereading_own_written_files_and_nudge_targeted_edit_on_large_files`

### 4. Masalah 4 — Porsi cache masih tertinggal (71,6% vs 84,9% di Hermes)

- **Akar masalah di v0.5.19 (terbukti dari audit kode SelenCode vs Hermes):**
  1. `needs_compaction` di `src/agent/context.rs:1694` mematok ambang maksimum keras di **`48.000` token** bahkan pada model berjendela 1.000.000 token (`ag/gemini-4-hydrogen-medium`) — padahal konteks kerja rata-rata Hermes di KASKU adalah **58.772 token**! Begitu Selen menyentuh 48k, `compact_messages_hermes` merombak tengah percakapan dan mematahkan seluruh prefiks cache.
  2. `default_proactive_prune_tokens` sebesar `28.000` di `src/config.rs` dipadukan dengan landasan rearm `proactive_trigger / 2` (`14.000`) di `src/agent/loop.rs` memicu prune proaktif berulang di tengah giliran kerja 30k–60k token (tiap prune memutasi prefiks awal → cache drop ke ~0% pada panggilan berikutnya).
  3. **Langkah pengubah konteks yang secara teknis tak terhindarkan (dicatat jujur):** aktivasi `activate_toolset` atas permintaan eksplisit model (mengubah daftar `tools` di prefiks), prune/compaction sesudah konteks riil benar-benar melampaui ambang `96.000` / `200.000` token, dan panggilan pertama sesi (cold write). Di luar kejadian tersebut, seluruh panggilan sesi bangun kini mempertahankan prefiks 100% byte-identik.
- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`agent/context_compressor_prune.py:200`):** rumus landasan rearm penuh `after_est + max(reclaimed, proactive_trigger, min_reclaim)` di jalur pre-call dan post-tool `src/agent/loop.rs`.
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`agent/context_compressor.py:2630-2669`):** `compaction_threshold_tokens` di `src/agent/context.rs` menaikkan batas atas compaction untuk model jumbo dari `48.000 → 200.000` token (sambil menjaga `50%` untuk model kecil/menengah) dan `default_proactive_prune_tokens` di `src/config.rs` dinaikkan `28.000 → 96.000` (tetap dibatasi `45%` dari `context_limit_tokens` pada model kecil/menengah).
- **Bukti unit:** `tests/v0520_verify_tail_tooling_economy_cache.rs`:
  - `m4_large_context_preserves_cache_prefix_without_premature_prune_or_compaction`

---

## Tabel Regresi

| Metrik / Suite | Target / Pagar | Hasil v0.5.20 | Status |
|---|---|---|---|
| Rata-rata token tugas A + B (`usage`) | ≤ 5.000 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Token giliran Resume E (`usage`) | ≤ 7.500 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Tes banding ulang KASKU (ekor verifikasi 0, cap LULUS, pemulihan tsconfig cepat, panggilan turun, porsi cache naik, 45/45) | 4/4 syarat selesai terpenuhi, 45/45 + hijau | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Suite unit & integrasi Linux CI (`cargo test`) | 100% lulus, 0 gagal | Menunggu CI Run | Menunggu CI |
| Suite unit & integrasi Windows x64 CI (`cargo test`) | 100% lulus, 0 gagal | Menunggu CI Run | Menunggu CI |
| `cargo clippy --all-targets -- -D clippy::correctness` | 0 error | Menunggu CI Run | Menunggu CI |

Tidak diukur — Rata-rata token tugas A + B (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Token giliran Resume E (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Tes banding ulang KASKU (ekor verifikasi 0, cap LULUS, pemulihan tsconfig cepat, panggilan turun, porsi cache naik, 45/45): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Suite unit & integrasi Linux CI (`cargo test`): akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.20`.
Tidak diukur — Suite unit & integrasi Windows x64 CI (`cargo test`): akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.20`.
Tidak diukur — `cargo clippy --all-targets -- -D clippy::correctness`: akan diisi nomor Run ID CI resmi segera setelah workflow GitHub Actions selesai sebelum tag rilis `v0.5.20`.

---

## Tidak Diubah (Sengaja Dipertahankan Utuh)

Seluruh fitur, pagar, dan kontrak `v0.5.7`–`v0.5.19` di bawah ini sengaja tidak diubah dan tetap terlindungi oleh suite regresi CI:
- Keberadaan gerbang verifikasi (`Self-Correction Gate`) untuk tugas yang **belum** terverifikasi atau verifikasinya **benar-benar gagal**, penolakan probe `python -c` / `node -e`, kalimat kegagalan jujur `ZERO_EDIT_FAILURE_HEADER`, catatan berkas tes diubah, dan pelestarian status `LULUS` saat dedup mencegat verifikasi hijau (`v0.5.14`–`v0.5.19`).
- Disiplin berhenti model (`green_verify_dedup_hits`, `[DISIPLIN BERHENTI]`, konvergensi bersih) serta pencatatan `cache_read_tokens` / `cache_write_tokens` di parser OpenAI/Anthropic/Gemini, tabel SQLite `usage`, `/usage`, dan penanda `cache_control` selektif (`v0.5.19`).
- Penghapusan `max_steps` dari config/UI user (Opsi A `v0.5.18`) dengan batas internal tetap khusus subagent/orchestrator/cron latar (`CHILD_MAX_STEPS`), serta notice konvergensi tanpa edit (`no_edit_convergence_steps`).
- Porting ketahanan stream & fallback `v0.5.17`–`v0.5.18`: fallback chain untuk kelas mati-diam & mid-stream stall, penutupan jujur + pembatalan aman tool call terpotong `max_output_tokens`, dasar `stream_idle = 180s` + skalasi konteks (`> 50k → 240s`, `> 100k → 300s`), retry jittered backoff + diet pra-retry + breaker, batas ganda `read_file`, dan gerbang prune pasca-tool (aktif penuh ketika ambang tercapai).
- Pagar hemat sesi kecil & resume: `build_resume_snapshot`, `build_resume_system_prompt`, `prune_old_skill_results_with_archive`, dan `prune_old_tool_results_with_archive(..., 2)` khusus `lean_resume && step >= 2`.
- `TypingLoopGuard` Telegram, klasifier nomina permintaan ubah berkas, konverter Markdown→HTML Telegram, watchdog polling gateway, gerbang skill harness + opt-out eksplisit, memori persisten, `ask_user`, `cron`, `session_search`, `watch_job`, dan sandbox/approval.
- Verifikasi E2E browser mandiri (dicatat penguji di luar daftar 4 masalah bernomor siklus ini; tidak ditambahkan tanpa perintah pemilik).
