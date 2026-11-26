# Laporan Hasil Update SelenCode v0.5.15 — Hapus Pagu Langkah Bawaan + Tiga Utang v0.5.14

- **Versi rilis:** `v0.5.15`
- **Jumlah butir:** 4
- **Pembagian kerja (keputusan pemilik):** Pembangunan, `cargo test` (unit/mock/integrasi), dan rilis dikerjakan agent pengembang; seluruh pengujian penerimaan live & pengukuran angka regresi A/B/E + SWE-bench dijalankan oleh **Muse**.

---

## Ringkasan Vonis (4 Butir)

- Butir 1 — LULUS: `default_max_steps()` dihapus menjadi `0` (tanpa batas ala Hermes), rumus konvergensi mode berpagu disamakan tepat spesifikasi (`max_steps = 15` → langkah `10`, `max_steps = 25` → langkah `20`), mode tanpa pagu menyuntikkan satu notice konvergensi pada ambang bawaan `20` langkah tanpa menghentikan giliran, penjaga pengulangan 3× tetap aktif, dan tag langkah Telegram/TUI menyesuaikan (`langkah {n}` saat tanpa pagu).
- Butir 2 — LULUS: `enforce_zero_edit_failure_header` menegakkan baris pembuka `Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.` secara programatis pasca-generasi di depan teks model/templat apa pun untuk penutupan paksa nol-edit pada tugas perubahan, dan diam di luar kondisi itu.
- Butir 3 — LULUS: Integritas keadaan verifikasi di `SelfCorrectState` kini terikat pada perintah verifikasi mapan (`is_same_or_broader_verify_cmd`) sehingga probe ad-hoc (`python3 -c`/`node -e`) atau suite tes tak terkait tidak dapat menimpa status gagal, dan ringkasan penutup mengutip hasil perintah mapan (perintah + exit code + baris hasil kunci) atau menyatakan `belum diverifikasi`.
- Butir 4 — LULUS: Mutasi pada berkas tes lintas bahasa (`is_test_file_path`) dilacak di `SelfCorrectState::edited_test_files`, ditandai langsung di transkrip sesi (`[berkas-tes] diubah: <path>`), dan ditempelkan permanen di ringkasan penutup (`Catatan berkas tes diubah: <path>`).

---

## 1. Perbaikan 1 — Hapus Pagu Langkah Bawaan + Tag Langkah Menyesuaikan + Rumus Konvergensi Disamakan

- **Berkas & fungsi diubah:**
  - `src/config.rs` (`default_max_steps() -> 0`, `GuardCfg::no_edit_convergence_steps = 20`, `KNOWN_GUARD_KEYS`) & `config.example.toml` (`max_steps = 0` + komentar kosong/0 = tanpa batas ala Hermes).
  - `src/agent/loop.rs` (`compute_convergence_step`, loop `for step in 0usize..` mendukung `max_steps == 0` tanpa pagu tersembunyi, `remaining_steps = usize::MAX` di mode tanpa pagu, notice konvergensi 1× di ambang `no_edit_convergence_steps` untuk mode tanpa pagu yang bersifat tekanan bukan penghentian).
  - `src/telegram.rs` (`format_telegram_progress_html`: `langkah {step}/{max_steps}` saat berpagu, `langkah {step}` saat `max_steps == 0`) & `src/tui/moonlit.rs` (status bar TUI menyesuaikan).
- **Bukti unit/mock (`tests/v0515_unbounded_steps_integrity_test_notice.rs::p1_unbounded_default_convergence_formula_and_telegram_step_tag`):**
  - `AgentCfg::default().max_steps == 0` dan `GuardCfg::default().no_edit_convergence_steps == 20`.
  - Titik tembak rumus konvergensi `compute_convergence_step` (`min(max_steps - 5, ceil(0.85 * max_steps))`):
    - **Pagu 15:** `compute_convergence_step(15) == 10` (sisa 5 langkah tercapai lebih dulu daripada 85% = 13; terverifikasi juga pada mock loop `max_steps = 15` menembak tepat di **langkah 10**, bukan 12).
    - **Pagu 25:** `compute_convergence_step(25) == 20` (sisa 5 langkah tercapai lebih dulu daripada 85% = 22).
    - **Pagu 100:** `compute_convergence_step(100) == 85` (85% tercapai lebih dulu daripada sisa 5 = 95).
  - Mode tanpa pagu (`max_steps = 0`, ambang uji `4`): notice konvergensi menembak 1× di langkah 4, `0` checkpoint pagu, giliran tetap lanjut hingga selesai di langkah 6.
  - Tag Telegram: `format_telegram_progress_html(7, 0, ...)` → `langkah 7` (tanpa `/0`); `format_telegram_progress_html(3, 25, ...)` → `langkah 3/25`.
- **Temuan jalur lari-liar:** Tidak dipasang pagu pengganti diam-diam; perlindungan lapisan luar tetap mengandalkan timeout giliran gateway 900 dtk, timeout provider (`connect`/`response`/`stream_idle`/`total`), penjaga pengulangan 3× & stagnasi 4× error beruntun, serta pagu token keluaran.

---

## 2. Perbaikan 2 — Penegakan Pasca-Generasi Kalimat Kegagalan Nol-Edit

- **Berkas & fungsi diubah:**
  - `src/agent/loop.rs` (`ZERO_EDIT_FAILURE_HEADER`, `enforce_zero_edit_failure_header`, diterapkan pasca-generasi di blok `forced_stop_reason` tepat sesudah respons model atau fallback templat diterima).
  - `prompt_requests_file_change` diperluas mencakup kata kerja perubahan ID/EN umum (`ganti`, `betulkan`, `selesaikan`, `update`, `modify`, `raises`, `exception`, `instead of`, dll.).
- **Bukti unit/mock (`p2_zero_edit_failure_header_enforced_post_generation_unconditionally`):**
  - Contoh masukan model netral: `"Proses dihentikan sementara karena batas langkah; analisis sudah selesai."`
  - Keluaran pasca-generasi (`zero_edit_failure = true`):
    ```text
    Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.

    Proses dihentikan sementara karena batas langkah; analisis sudah selesai.
    ```
  - Contoh masukan model `"Gagal menyelesaikan tepat waktu..."` (bukan kalimat baku) tetap ditempel `Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.` di baris pembuka.
  - Pada tugas baca-saja (`"Jelaskan isi folder ini tanpa mengubah apa pun."`) atau bila ada mutasi berkas (`zero_edit_failure = false`): tidak ada penempelan apa pun.

---

## 3. Perbaikan 3 — Integritas Keadaan Verifikasi & Kutipan Spesifik Penutup

- **Berkas & fungsi diubah:**
  - `src/agent/corrector.rs` (`is_same_or_broader_verify_cmd`, `parse_exit_code`, `extract_verify_key_line`, `SelfCorrectState::observe_tool_at_step`, `SelfCorrectState::closing_verification_summary_line`).
  - `src/agent/loop.rs` (menempelkan `closing_verification_summary_line` secara programatis pada ringkasan penutup giliran perubahan).
- **Bukti unit/mock (`p3_verification_state_bound_to_established_cmd_not_overwritten_by_probes`):**
  - Keadaan tidak tertimpa probe: `node uji.js` gagal (`exit code 1`) → edit salah → probe `node -e "console.log('probe ok')"` (`exit code 0`) → `is_same_or_broader_verify_cmd("node uji.js", "node -e ...") == false` → penutupan tetap ditahan gerbang sambil menyebut `node uji.js`!
  - Suite tak terkait juga ditolak: `is_same_or_broader_verify_cmd("pytest tests/test_prefixes.py", "pytest tests/test_other.py") == false`, sementara perluasan sah (`pytest tests/test_prefixes.py::test_mul` → `pytest tests/test_prefixes.py` / `pytest`) bernilai `true`.
  - Contoh keluaran penutup sesudah `node uji.js` lulus:
    ```text
    Verifikasi: `node uji.js` — exit code 0 (LULUS) — PASS_UJI: 31500
    ```
  - Contoh keluaran penutup tugas perubahan tanpa verifikasi (gerbang tetap diam, 0 panggilan ekstra):
    ```text
    Verifikasi: belum diverifikasi (tidak ada perintah verifikasi yang dijalankan pada giliran ini).
    ```

---

## 4. Perbaikan 4 — Visibilitas Mutasi Berkas Tes

- **Berkas & fungsi diubah:**
  - `src/agent/corrector.rs` (`is_test_file_path`, `SelfCorrectState::edited_test_files`, `SelfCorrectState::closing_test_files_note`).
  - `src/agent/loop.rs` (mencatat `[berkas-tes] diubah: <path>` ke `UiEvent::Info`/`stderr` saat mutasi terjadi & menempelkan `Catatan berkas tes diubah: <daftar>` pada ringkasan penutup).
- **Bukti unit/mock (`p4_test_file_mutations_visible_in_transcript_and_closing_and_v0515_report_passes`):**
  - Pola terdeteksi: `test_discount.py`, `pkg/tests/test_lint.py`, `src/__tests__/app.js`, `calc.spec.ts`, `calc.test.tsx`, `util_test.go`, `foo_test.rs`, `tests/v0515.rs`, `src/test/java/AppTest.java`; bukan berkas tes: `src/main.rs`, `calc.js`, `pylint/config.py`, `README.md`.
  - Contoh penanda transkrip saat terjadi: `[berkas-tes] diubah: tests/test_app.py`.
  - Contoh baris wajib di ringkasan penutup:
    ```text
    Catatan berkas tes diubah: tests/test_app.py
    ```

---

## Tabel Regresi

| Metrik | Nilai v0.5.15 | Status |
| --- | --- | --- |
| Titik tembak konvergensi berpagu (unit) | Pagu 15 → langkah 10; pagu 25 → langkah 20; pagu 100 → langkah 85 | LULUS |
| Mode tanpa pagu bawaan (unit/mock) | `max_steps = 0`, ambang notice 20 langkah, 0 checkpoint pagu | LULUS |
| Penegakan pasca-generasi nol-edit (unit/mock) | 100% ditempel di depan teks netral/kustom, 0 di baca-saja | LULUS |
| Integritas keadaan verifikasi & catatan tes (unit/mock) | Probe `exit 0` ditolak; kutipan `exit code 0 (LULUS)` & `Catatan berkas tes diubah:` muncul | LULUS |
| Proyek A (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Proyek B (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Resume E (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Uji live SWE-bench & penerimaan v0.5.15 | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |

---

## Bagian yang Sengaja Tidak Diubah

- Mekanisme checkpoint rasio 0,7, panggilan anugerah, & isi wrap-up paksa `v0.5.14` (selain penyamaan rumus konvergensi di Butir 1 & penegakan pasca-generasi di Butir 2).
- Gerbang verifikasi sebelum menutup `v0.5.14` di luar integritas perintah mapan & kutipan penutup (Butir 3).
- Konverter tampilan Telegram HTML + fallback anti-gagal-parse (`v0.5.12`, hanya menambahkan cabang `langkah {n}` saat `max_steps == 0` di `format_telegram_progress_html` sesuai Butir 1 #5).
- Seluruh hasil `v0.5.7`–`v0.5.13`: gerbang skill + opt-out, gerbang laporan rilis terperiksa mesin (ditambah penerimaan pernyataan `"pengukuran penerimaan oleh Muse"` sesuai pembagian kerja baru), timeout & watchdog gateway + trim memori sesi, `watch_job` + status segar + polling watch CLI, store memory tunggal, jalur resume ramping, salinan kerja `skill_view` + prune teks penuh `full=true`, peringatan config + `selen doctor`, pembatalan provider ~1×, `send_file`, `delegate_work`, `/usage`, `skills install`, fallback rute berbeda, `ask_user`, `cron`, `session_search`, pelacakan approval, serta 12 skill bawaan + `NOTICE`.
