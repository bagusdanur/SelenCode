# Laporan Penyelesaian SelenCode v0.5.18 — Delta Utang v0.5.17 (Fallback Mati-Diam, Akhir Jujur `max_output_tokens`, Dasar 180s, & Prune Pasca-Tool)

- **Tag rilis:** `v0.5.18`
- **Basis:** `v0.5.17` (`c72be22`)
- **Sumber referensi Hermes Agent:** `https://github.com/NousResearch/hermes-agent` (commit `ed1cbc8d`)
- **Jumlah butir:** 4

## Ringkasan Vonis (4 Masalah Utama)

- **Butir 1 — LULUS:** Jalur fallback `DynamicRouterProvider` kini mengalihkan kelas kegagalan stream mati-diam maupun macet di tengah stream (meski sudah sempat mengirim token parsial) ke rute fallback sehat di dalam giliran yang sama disertai pemberitahuan pengalihan terlihat, sementara circuit breaker tetap menjadi jaring terakhir bila seluruh fallback juga mati.
- **Butir 2 — LULUS:** Generasi yang terpotong `max_output_tokens` (`finish_reason = "length" | "max_tokens" | "MAX_TOKENS"`) atau menghasilkan argumen tool call terpotong (`_raw`) kini TIDAK pernah berakhir diam maupun menerapkan penulisan sebagian, melainkan membatalkan tool call terpotong dan menulis + menyimpan pesan penutup jujur (`[GENERASI TERPOTONG BATAS TOKEN KELUARAN]`) yang menyebut apa yang sudah dan belum diterapkan.
- **Butir 3 — LULUS:** Kesabaran dasar `stream_idle` disamakan dengan dasar Hermes menjadi **180 detik** (`ProviderTimeouts::default()`, `ProviderTimeoutsCfg::default()`, `config.example.toml`) dan `total` dasar menjadi **600 detik**, dengan tier skalasi konteks `> 50k → 240s` dan `> 100k → 300s` tepat seperti Hermes.
- **Butir 4 — LULUS:** Gerbang evaluasi tekanan konteks pasca-tool kini dijalankan segera sesudah hasil tool satu langkah mendarat (di giliran yang sama, sebelum panggilan model berikutnya), memangkas hasil lama serta batch penelanan raksasa yang melewati ambang absolut ke arsip byte-exact + pratinjau kepala/ekor dan langsung mencatat `pruned_count > 0` pada baris metrik giliran penelanan tersebut.

---

## Rincian Per Masalah (Pelabelan Sumber Eksplisit)

### 1. Masalah 1 — Fallback chain tidak menyala untuk kelas mati-diam

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`agent/turn_api_error.py`: `settle_unrecovered_error` → `_try_activate_fallback`):** ketika rute utama gagal karena error transport/stream macet, giliran dialihkan ke rantai fallback di dalam giliran yang sama sebelum menyerah.
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`agent/chat_completion_helpers.py`: `_consecutive_stale_streams`, `agent/chat_completion_local.py`: `stale_giveup()` bawaan `5`):** SelenCode memakai `STALE_STREAM_GIVEUP_LIMIT = 3` (lebih ketat untuk interaktivitas terminal/HP) sebagai jaring terakhir ketika semua rute (termasuk fallback) sama-sama macet.
- **Akar masalah di v0.5.17 & perubahan di SelenCode:**
  - `src/providers/router.rs` (`DynamicRouterProvider::stream_chat`, `is_retryable_error`, `is_timeout_error`): sebelumnya blok `if emitted_any { return Err("stream terputus di tengah jawaban..."); }` langsung memutus loop kandidat sebelum rute fallback ke-2 pernah dicoba (sehingga fallback mendapat 0 request dan retry loop hanya mengulang rute utama hingga breaker). Kini bila `fallback_eligible && has_more_candidates`, kegagalan mid-stream stall langsung `break` ke kandidat fallback berikutnya dan menyetel `primary_fail_short = Some("stream macet di tengah")` sehingga `pending_fallback_notice` tampil jelas ke user.
  - `src/app/mod.rs` (`make_routed_provider`): `[routing] fallbacks` kini juga memakai `push_fallback_spec` (mendukung format `"provider:model"`, `"provider"`, maupun `"model-id"`).
- **Bukti unit:** `tests/v0518_fallback_truncation_base180_posttool_prune.rs`:
  - `m1_dead_silent_and_midstream_primary_falls_back_with_notice_and_breaker_when_all_dead`

### 2. Masalah 2 — Giliran berakhir diam saat generasi terpotong `max_output_tokens` di tengah tool call raksasa

- **Sumber & pelabelan:**
  - **Mekanisme sendiri (SelenCode):** penanganan penutupan jujur & pembatalan aman tool call terpotong di `run_turn_inner`, dipadukan dengan deteksi `finish_reason = "length" | "max_tokens" | "MAX_TOKENS"` dan pendeteksian argumen JSON rusak (`_raw`) dari parser SSE.
- **Perubahan di SelenCode:**
  - `src/agent/loop.rs` & `src/agent/mod.rs`: `OUTPUT_TRUNCATION_HEADER`, `is_output_length_truncated`, `has_truncated_tool_call`, dan `build_output_truncation_closing_message`.
  - Di `run_turn_inner`: jika `has_truncated_tool_call(&reply)` atau `reply.tool_calls.is_empty() && (is_output_length_truncated(...) || reply.text.trim().is_empty())`, seluruh tool call terpotong DIBATALKAN (nol penulisan sebagian ke disk, nol `ToolUse` yatim di DB), lalu pesan penutup jujur yang menyebut penyebab `max_output_tokens`, berkas yang sudah diterapkan (`corrector.edited_files`), dan aksi yang belum diterapkan disimpan ke SQLite (`persist_message`), dikirim ke UI/stdout, dan dikembalikan sebagai hasil giliran.
- **Bukti unit:** `tests/v0518_fallback_truncation_base180_posttool_prune.rs`:
  - `m2_max_output_tokens_truncation_is_visible_persisted_and_cancels_partial_tool_call`

### 3. Masalah 3 — Kesabaran dasar sesi kecil 90s → 180s (selaras dasar Hermes)

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`agent/chat_completion_helpers.py`: `_stream_env_stale_base` = `180.0`s dasar; `_scale_stale_timeout_for_context`: `> 50_000` → `240.0`s, `> 100_000` → `300.0`s):** nilai dasar `stream_idle` kini tepat `180` detik dan tier di atasnya tepat `> 50k → 240s`, `> 100k → 300s` (tier buatan `> 16k → 180s` dari v0.5.17 dihapus karena `180s` sudah menjadi nilai dasar).
  - **Angka sendiri, terinspirasi Hermes:** `total` dasar dinaikkan `300s → 600s` agar memberi ruang aman di atas `stream_idle` 180s–300s, dan nilai uji pendek (`stream_idle < 30s`) tetap tidak diskalakan agar unit test timeout berjalan cepat.
- **Perubahan di SelenCode:**
  - `src/providers/mod.rs` (`ProviderTimeouts::default`, `effective_stream_idle`), `src/config.rs` (`default_provider_stream_idle_secs = 180`, `default_provider_total_secs = 600`), `config.example.toml`, serta penyelarasan assertion default di `tests/v0510_gateway_and_status.rs`, `tests/v0511_resume_skill_config_timeout.rs`, dan `tests/v0517_hermes_resilience_pruning_legacy_steps.rs`.
- **Bukti unit:** `tests/v0518_fallback_truncation_base180_posttool_prune.rs`:
  - `m3_base_stream_idle_is_180s_and_tiers_scale_correctly`

### 4. Masalah 4 — Gerbang prune pasca-tool di giliran penelanan itu sendiri

- **Sumber & pelabelan:**
  - **Diambil dari Hermes (`agent/turn_preflight.py`: `compress_after_tool_results` → `_proactive_tool_result_prune` & `_reset_read_dedup_caches`):** evaluasi tekanan konteks dan pemangkasan proaktif segera sesudah hasil tool satu langkah mendarat (sebelum panggilan model berikutnya di giliran yang sama) beserta reset cache dedup bacaan.
  - **Angka/mekanisme sendiri, terinspirasi Hermes (`agent/context_compressor.py` #61932 pengecualian pending-round besar):** `prune_oversized_pending_tool_batch_with_archive` (ambang hasil tunggal `> 4.000` char, pratinjau kepala/ekor `2.400` char + arsip byte-exact) dan `Store::add_latest_call_pruned_count` agar batch penelanan raksasa di langkah pertama sekalipun langsung dipangkas sebelum panggilan berikutnya dan `pruned_count > 0` tercatat pada baris metrik panggilan penelanan itu sendiri.
- **Perubahan di SelenCode:**
  - `src/agent/context.rs`: `prune_oversized_pending_tool_batch_with_archive`.
  - `src/store.rs`: `Store::add_latest_call_pruned_count`.
  - `src/agent/loop.rs`: gerbang pasca-tool segera setelah `messages.push(tool_msg)` menjalankan `proactive_prune_context_with_archive` + bila masih melewati ambang menjalankan `prune_oversized_pending_tool_batch_with_archive`, mereset `dedup.clear_file_reads()`, mencatat ke `call_token_metrics`, dan mengirim pemberitahuan terlihat.
- **Bukti unit:** `tests/v0518_fallback_truncation_base180_posttool_prune.rs`:
  - `m4_post_tool_prune_fires_in_same_ingestion_turn_before_next_call`

### Catatan Tambahan (Permintaan Pemilik — Opsi A Penghapusan `max_steps` dari Config/UI User)

- **Diambil dari pola Hermes (`run_agent.py`: `TURN_LIMIT_UNLIMITED`, `agent/iteration_budget.py` untuk subagent):** `max_steps` dihapus dari `config.example.toml`, tidak lagi ditulis saat `save_to`, dan seluruh runner giliran utama (`src/main.rs`, `src/app/plain_runner.rs`, `src/app/tui_runner.rs`, `src/app/telegram_runner.rs`) selalu berjalan tanpa batas (`0`). Kunci `max_steps` lama di config pengguna diabaikan otomatis dengan pemberitahuan terlihat + laporan di `selen doctor`, sementara batas internal tetap dipertahankan khusus untuk subagent/orchestrator/cron latar agar tidak runaway.

---

## Tabel Regresi

| Metrik / Suite | Target / Pagar | Hasil v0.5.18 | Status |
|---|---|---|---|
| Rata-rata token tugas A + B (`usage`) | ≤ 5.000 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Token giliran Resume E (`usage`) | ≤ 7.500 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Uji penerimaan delta v0.5.18 (fallback mati-diam, trunc output, dasar 180s, prune pasca-tool) | 4/4 syarat selesai terpenuhi | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Suite unit & integrasi Linux CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45949219251`, job `140340899910`) | LULUS |
| Suite unit & integrasi Windows x64 CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45949219251`, job `140340900312`) | LULUS |
| `cargo clippy --all-targets -- -D clippy::correctness` | 0 error | 0 error (CI Run `45949219251`) | LULUS |

Tidak diukur — Rata-rata token tugas A + B (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Token giliran Resume E (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Uji penerimaan delta v0.5.18 (fallback mati-diam, trunc output, dasar 180s, prune pasca-tool): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).

---

## Tidak Diubah (Sengaja Dipertahankan Utuh)

Seluruh fitur dan kontrak `v0.5.7`–`v0.5.17` di bawah ini sengaja tidak diubah dan tetap terlindungi oleh suite regresi CI:
- Porting v0.5.17 yang sudah lulus: skalasi stale konteks besar (`> 50k → 240s`, `> 100k → 300s`), retry jittered backoff + diet pra-retry + breaker, batas ganda `read_file` (250 baris / 16.000 char + paging + clamp 2.000 char/baris), dan prune 3-pass dengan arsip byte-exact.
- Checkpoint anggaran langkah & batas internal subagent/orchestrator (`CHILD_MAX_STEPS`), notice konvergensi tanpa edit (`no_edit_convergence_steps`), gerbang verifikasi & kalimat kegagalan pasca-generasi (`ZERO_EDIT_FAILURE_HEADER`), serta catatan berkas tes diubah (`v0.5.14`–`v0.5.16`).
- `TypingLoopGuard` Telegram, klasifier nomina permintaan ubah berkas, konverter Markdown→HTML Telegram, dan watchdog polling gateway (`v0.5.10`–`v0.5.16`).
- Gerbang skill harness + opt-out eksplisit, `skill_view` salinan kerja ringkas & prune dini skill, serta `build_resume_snapshot` / `build_resume_system_prompt` (`v0.5.7`–`v0.5.13`).
- Kontrak `DynamicRouterProvider` v0.5.11 (timeout header respons awal ≈1× `response_secs` pada rute tunggal dan perpindahan cepat ke rute berbeda).
- Store memori persisten tunggal (`memory.md` / `profile.md`), `ask_user`, `cron`, `session_search`, `watch_job`, dan mekanisme approval/sandbox.
