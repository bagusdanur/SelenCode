# Laporan Penyelesaian SelenCode v0.5.17 — Porting Mekanisme Hermes Agent (Stale-Stream Recovery, Proactive Context Pruning & Pembebasan Pagu Warisan)

- **Tag rilis:** `v0.5.17`
- **Basis:** `v0.5.16` (`4428f65`)
- **Sumber referensi Hermes Agent:** `https://github.com/NousResearch/hermes-agent` (commit `ed1cbc8d`)
- **Jumlah butir:** 3

## Ringkasan Vonis (3 Masalah Utama)

- **Butir 1 — LULUS:** Mekanisme skalasi `stream_idle` berbasis ukuran konteks (`agent/chat_completion_helpers.py`) serta loop pemulihan error panggilan API di dalam giliran dengan klasifikasi error (`agent/error_classifier.py`), jittered exponential backoff (`agent/retry_utils.py`), diet darurat pra-retry, dan circuit breaker rentetan macet (`STALE_STREAM_GIVEUP_LIMIT = 3`, `agent/turn_api_error.py`) telah diporting ke Rust dan lolos uji unit/integrasi otomatis.
- **Butir 2 — LULUS:** Mekanisme batas ganda bacaan berkas di sumber dengan pemotongan akhir baris utuh + pagination hint (`tools/file_tools.py`), pemotongan per baris 2.000 char (`tools/tool_output_limits.py`), serta pemangkasan proaktif 3-pass deterministik (`agent/context_compressor_prune.py`, `agent/context_compressor.py`) + reset cache dedup bacaan (`agent/turn_preflight.py`) dan pagu `scaled_turn_budget` tetap (tidak lagi menggelembung ke 180k pada model 1M) telah diporting dan lolos uji end-to-end (`pruned_count > 0` tercatat di `call_token_metrics`).
- **Butir 3 — LULUS:** Pagu warisan `[agent] max_steps = 25` dari config pra-v0.5.15 kini otomatis dibebaskan menjadi tanpa batas (`0`) oleh loader config disertai pemberitahuan terlihat (stderr + banner TUI/Plain REPL), nilai pagu eksplisit lain (`15`, `30`, `40`, `50`, dst.) tetap dihormati apa adanya, dan `selen doctor` melaporkan pagu efektif beserta sumbernya (`Batas Langkah`).

---

## Rincian Per Masalah (Mekanisme Hermes → Porting SelenCode → Bukti Unit)

### 1. Masalah 1 — Panggilan model mati diam membunuh giliran

- **Mekanisme Hermes Agent yang diambil:**
  - `agent/chat_completion_helpers.py` (`_stream_env_stale_base`, `_scale_stale_timeout_for_context`, `_consecutive_stale_streams`, `_check_stale_giveup`): skalasi kesabaran stream berdasarkan estimasi token konteks (`>16k` → `180s`, `>50k` → `240s`, `>100k` → `300s`) dan circuit breaker lintas langkah agar provider yang benar-benar mati berhenti bersih setelah 3× macet beruntun, lalu direset pada setiap respons sukses.
  - `agent/error_classifier.py` (`FailoverReason`, `classify_api_error`): pemisahan kelas kegagalan stream macet/putus, overload/429/5xx, context overflow (413/token limit), dan error deterministik non-retryable (400/401/403/404/batal).
  - `agent/turn_api_error.py` (`handle_api_error`, `settle_unrecovered_error`) & `agent/retry_utils.py` (`jittered_backoff`): loop retry di dalam satu giliran dengan jittered exponential backoff + pemulihan overflow tanpa memutus giliran kerja pengguna.
- **Perubahan di SelenCode:**
  - `src/providers/mod.rs`: `ProviderTimeouts::effective_stream_idle(est_context_tokens)` & `ProviderTimeouts::effective_total(est_context_tokens)` (menjaga tenggat uji pendek `< 30s` agar tes v0.5.10/v0.5.11 tetap instan).
  - `src/providers/openai.rs`, `src/providers/anthropic.rs`, `src/providers/gemini.rs`: memakai `effective_stream_idle` & `effective_total` berbasis `estimate_tokens(msgs)` pada setiap panggilan streaming.
  - `src/agent/loop.rs` & `src/agent/mod.rs`: `STALE_STREAM_GIVEUP_LIMIT`, `TurnApiErrorKind`, `classify_turn_api_error`, `jittered_backoff_ms`, serta loop pemulihan panggilan API di `run_turn_inner` yang menjalankan diet darurat proaktif sebelum retry stream macet, mencatat pemberitahuan retry terlihat ke UI/stderr, dan menerapkan circuit breaker 3× macet beruntun.
  - `src/tools/mod.rs`: `Ctx.api_max_retries` (default 3), `Ctx.api_retry_base_delay_ms` (default 2.000 ms), dan builder `Ctx::with_api_retry`.
- **Bukti unit:** `tests/v0517_hermes_resilience_pruning_legacy_steps.rs`:
  - `m1_context_scaled_stream_idle_and_backoff_match_hermes`
  - `m1_mid_turn_stale_stream_recovers_and_circuit_breaker_stops_infinite_hang`

### 2. Masalah 2 — Konteks meledak tanpa ada yang mengendalikan (`4.351 → 38.449` token, `pruned_count = 0`)

- **Mekanisme Hermes Agent yang diambil:**
  - `tools/file_tools.py` (`_truncate_to_char_budget`, `_apply_char_budget`) & `tools/tool_output_limits.py` (`DEFAULT_MAX_LINE_LENGTH = 2000`): batas ganda pada `read_file` (batas baris bawaan + pagu karakter yang memotong rapi di baris utuh terakhir disertai petunjuk lanjutan `start`/`end`) dan pemotongan baris tunggal super panjang.
  - `agent/context_compressor_prune.py` (`ProactivePruneMixin.prune_tool_results_only`, `_align_boundary_backward`, `proactive_prune_min_reclaim_tokens`) & `agent/context_compressor.py` (`_prune_old_tool_results`, `_dedupe_tool_results`): pemangkasan proaktif 3-pass deterministik tanpa LLM — Pass 1 dedup hasil tool identik, Pass 2 arsip+stub `ToolResult` lama di luar ekor terlindungi, Pass 3 arsip+ringkas argumen `ToolUse` raksasa (seperti `write_file.content` 16 KB) — dipicu berdasarkan tekanan token praktis + gerbang `min_reclaim_tokens` agar tidak memecah prompt cache sia-sia.
  - `agent/turn_preflight.py` (`_proactive_tool_result_prune`, `_reset_read_dedup_caches`) & `agent/usage_anchor.py`: jangkar tagihan token aktual terakhir dari provider dan reset cache dedup bacaan setiap kali prune di-commit.
- **Perubahan di SelenCode:**
  - `src/tools/file.rs`: `DEFAULT_MAX_LINE_LENGTH` (2.000), `DEFAULT_FILE_READ_LINES` (250), `DEFAULT_FILE_READ_MAX_CHARS` (16.000), `truncate_lines_to_char_budget`, dan pagination hint pada `ReadFile::run` (berkas kecil tetap 100% utuh).
  - `src/agent/context.rs`: `ProactivePruneResult` & `proactive_prune_context_with_archive` (3-pass deterministik + arsip byte-exact + gerbang `min_reclaim_tokens` + penyelarasan ekor agar pasangan `ToolUse`/`ToolResult` tidak terpotong).
  - `src/agent/corrector.rs`: `ToolDedupCache::clear_file_reads()` agar berkas yang baru di-prune tetap bisa dibaca ulang.
  - `src/agent/loop.rs`: memicu `proactive_prune_context_with_archive` berbasis jangkar token aktual provider + reset `dedup.clear_file_reads()` + mematok `scaled_turn_budget` maksimum 60.000 char (tidak lagi menggelembung ke 180.000 char pada jendela 1M).
  - `src/config.rs`: 4 kunci baru di `[tool_budget]` (`file_read_max_chars`, `file_read_default_lines`, `proactive_prune_tokens`, `proactive_prune_min_reclaim_tokens`) lengkap di `validate_toml_keys`.
- **Bukti unit:** `tests/v0517_hermes_resilience_pruning_legacy_steps.rs`:
  - `m2_read_file_dual_cap_pagination_and_per_line_clamp`
  - `m2_proactive_prune_controls_context_explosion_archives_and_resets_dedup`

### 3. Masalah 3 — Pagu warisan `max_steps = 25` tetap membatasi diam-diam

- **Mekanisme & perubahan di SelenCode:**
  - `src/config.rs`: `LEGACY_DEFAULT_MAX_STEPS = 25`, enum `MaxStepsSource` (`DefaultUnbounded`, `Legacy25Liberated`, `ExplicitConfig`), field `#[serde(skip)]` `Config.max_steps_source` & `Config.startup_notices`, fungsi `extract_explicit_max_steps`, `apply_max_steps_policy`, `emit_legacy_max_steps_notice_once`, dan `parse_config_str_with_policy`. Nilai warisan `25` otomatis dibebaskan menjadi `0` dengan pemberitahuan terlihat; nilai pagu eksplisit lain (mis. `15`, `30`, `40`, `50`) dihormati apa adanya.
  - `src/app/tui_runner.rs` & `src/app/plain_runner.rs`: menampilkan `cfg.startup_notices` langsung di antarmuka TUI (`app.info`) dan Plain REPL (`println!`).
  - `src/doctor.rs`: menambahkan pemeriksaan `Batas Langkah` di `run_doctor` yang melaporkan pagu efektif (`tanpa batas (0)` atau `N langkah`) beserta sumbernya (`bawaan tanpa batas` / `warisan max_steps = 25 dari <path> dibebaskan otomatis` / `eksplisit di <path>`).
- **Bukti unit:** `tests/v0517_hermes_resilience_pruning_legacy_steps.rs`:
  - `m3_legacy_max_steps_25_liberated_explicit_respected_and_doctor_reports`

---

## Tabel Regresi

| Metrik / Suite | Target / Pagar | Hasil v0.5.17 | Status |
|---|---|---|---|
| Rata-rata token tugas A + B (`usage`) | ≤ 5.000 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Token giliran Resume E (`usage`) | ≤ 7.500 token | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Uji penerimaan proyek nyata (Termux HP / Svelte) | Tidak mati di tengah stream & `pruned_count > 0` | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Suite unit & integrasi Linux CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45943710080`, job `140323504133`) | LULUS |
| Suite unit & integrasi Windows x64 CI (`cargo test`) | 100% lulus, 0 gagal | 100% lulus, 0 gagal (CI Run `45943710080`, job `140323503401`) | LULUS |
| `cargo clippy --all-targets -- -D clippy::correctness` | 0 error | 0 error (CI Run `45943710080`) | LULUS |

Tidak diukur — Rata-rata token tugas A + B (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Token giliran Resume E (`usage`): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).
Tidak diukur — Uji penerimaan proyek nyata (Termux HP / Svelte): pengukuran penerimaan oleh Muse (sesuai pembagian tugas, pengujian live tidak dijalankan oleh pengembang).

---

## Tidak Diubah (Sengaja Dipertahankan Utuh)

Seluruh fitur dan kontrak `v0.5.7`–`v0.5.16` di bawah ini sengaja tidak diubah dan tetap terlindungi oleh suite regresi CI:
- Checkpoint anggaran langkah (`[guard] budget_warning_ratio`), pemicu konvergensi belum-ada-edit (`no_edit_convergence_steps`), gerbang verifikasi & kalimat kegagalan pasca-generasi (`ZERO_EDIT_FAILURE_HEADER`), serta catatan berkas tes diubah (`v0.5.14`–`v0.5.16`).
- `TypingLoopGuard` Telegram, klasifier nomina permintaan ubah berkas (`prompt_requests_file_change`), konverter Markdown→HTML Telegram, dan watchdog polling gateway (`v0.5.10`–`v0.5.16`).
- Gerbang skill harness + opt-out eksplisit pengguna, `skill_view` salinan kerja ringkas & prune dini skill, serta `build_resume_snapshot` / `build_resume_system_prompt` (`v0.5.7`–`v0.5.13`).
- Kontrak `DynamicRouterProvider` v0.5.11 (timeout header respons awal ≈1× `response_secs` pada rute tunggal dan fallback langsung ke rute berbeda) serta default `[timeouts]` (`stream_idle_secs = 90`, `total_secs = 300`).
- Store memori persisten tunggal (`memory.md` / `profile.md`), `ask_user`, `cron`, `session_search`, `watch_job`, dan mekanisme approval/sandbox.
