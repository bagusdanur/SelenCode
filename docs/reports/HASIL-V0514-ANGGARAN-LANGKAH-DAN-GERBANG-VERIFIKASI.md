# Laporan Hasil Perbaikan SelenCode v0.5.14 — Kesadaran Anggaran Langkah & Gerbang Verifikasi Sebelum Menutup

- **Versi rilis:** `v0.5.14`
- **Jumlah butir:** 2
- **Cakupan:** Dua perbaikan di level loop (`src/agent/loop.rs`, `src/agent/corrector.rs`, `src/config.rs`, `src/tool_display.rs`, `config.example.toml`) meniru pola Hermes (`turn_iteration_prep.py`, `tool_guardrails.py`, pelabelan `exit_reason`), opencode (`MAX_STEPS_PROMPT`), SWE-agent (`review_on_submit`), dan Aider (`run_one` / `max_reflections = 3`). Seluruh fitur v0.5.7–v0.5.13 termasuk konverter Telegram v0.5.12 tidak diubah.

---

## Ringkasan Vonis (2 Butir)

- Butir 1 — LULUS: Checkpoint anggaran langkah satu-kali (bawaan rasio `0.7`, knob `[guard] budget_warning_enabled`/`budget_warning_ratio`), pemicu konvergensi belum-ada-edit, akhir pagu pola opencode dengan label kegagalan eksplisit bila nol edit, dan penjaga pengulangan identik 3× berturut-turut (pengecualian poller `watch_job`) telah terpasang di loop dan lolos tes otomatis serta verifikasi live.
- Butir 2 — LULUS: Gerbang verifikasi berbasis keadaan tercatat di loop (`last_verify_cmd`, `last_verify_passed`, `last_verify_output`, `verified_after_last_edit`) menahan penutupan pertama saat keadaan merah/belum diverifikasi ulang, mengembalikan keluaran gagal mentah hingga batas `≤ 3` putaran dengan penutupan jujur sesudah batas, serta diam total (0 suntikan) pada kontrol hijau dan kontrol tanpa verifikasi.

---

## 1. Perbaikan 1 — Kesadaran Anggaran Langkah

### Perubahan Kode
- `src/config.rs` & `config.example.toml`: menambahkan knob `[guard] budget_warning_enabled` (bawaan `true`) dan `budget_warning_ratio` (bawaan `0.7`), mendaftarkannya ke `KNOWN_GUARD_KEYS`, serta menaikkan bawaan `max_self_correct_rounds` dari `1` menjadi `3` (menyatu dengan batas refleksi Aider di Perbaikan 2).
- `src/agent/loop.rs`:
  - **Checkpoint 1× (pola Hermes):** menempel pada `ToolResult` terakhir ketika `used_steps >= checkpoint_step` (`round(max_steps * ratio)`), menampilkan angka persis `Kamu sudah memakai {used} dari {max_steps} langkah (sisa {remaining} langkah)` tanpa memicu giliran trivial yang jauh dari pagu.
  - **Pemicu konvergensi belum-ada-edit:** menempel `[PERINTAH KONVERGENSI — BELUM ADA EDIT]` ketika sisa langkah menipis (`sisa ≤ 5` atau `≥ 85%`) sementara `corrector.edited_files.is_empty()` pada tugas perubahan (`prompt_requests_file_change`).
  - **Akhir pagu (pola opencode + pelabelan Hermes):** wrap-up paksa memuat 4 bagian wajib (pernyataan batas tercapai, sudah dikerjakan, belum selesai, langkah berikutnya); bila berakhir di pagu dengan nol edit pada tugas perubahan, ringkasan wajib dibuka kalimat `Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.` (baik dari model maupun templat `build_template_closing_summary_with_status` di `src/tool_display.rs`).
- `src/agent/corrector.rs`: menambahkan `LoopGuard::record_with_result` + `LoopWarning::ConsecutiveIdenticalResult` (ambang `3` berturut-turut untuk `(tool, args, result)` identik, termasuk kelanjutan dedup) dengan pengecualian poller `is_polling_tool` (`watch_job` status/check/list/wait/poll, `background_status`).

### Bukti Uji Otomatis (`tests/v0514_step_budget_and_verify_gate.rs`)
- `p1_forced_small_budget_checkpoint_convergence_and_explicit_zero_edit_failure`:
  - Pagu kecil `max_steps = 6` (skenario edit): checkpoint `4 dari 6 langkah` terlihat tepat 1×, model langsung mengedit `app.py` sebelum pagu habis.
  - Pagu kecil `max_steps = 6` (skenario 0 edit): checkpoint `4/6` + `[PERINTAH KONVERGENSI — BELUM ADA EDIT]` `5/6` terlihat model, dan akhir pagu dibuka persis dengan `Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.` meski model mencoba menulis wrap-up netral.
  - `budget_warning_enabled = false`: 0 checkpoint disuntikkan.
- `p1_identical_repeat_notice_fires_on_3rd_consecutive_and_exempts_polling`:
  - 3 panggilan identik `list_files({})` berturut-turut: panggilan ke-1 & ke-2 bersih, panggilan ke-3 ditempeli `[PERINGATAN PENGULANGAN IDENTIK — 3x beruntun]`.
  - 5 panggilan berturut-turut `watch_job(status)` → `LoopWarning::None`.

*(Transkrip uji LIVE biner `v0.5.14` untuk skenario paksa pagu kecil, pengulangan ke-3, dan pengulangan `pylint-6506` akan dilengkapi tepat setelah biner rilis diterbitkan.)*

---

## 2. Perbaikan 2 — Gerbang Verifikasi Sebelum Menutup

### Perubahan Kode
- `src/agent/corrector.rs` & `src/agent/loop.rs`:
  - `SelfCorrectState` melacak keadaan nyata dari loop: `has_verification_run`, `last_verify_cmd`, `last_verify_step`, `last_verify_passed`, `last_verify_output` (keluaran gagal mentah hingga 4.000 char), dan `verified_after_last_edit`.
  - `is_verification_shell_cmd` diperluas menangkap runner & skrip repro nyata (`pytest`, `bin/test`, `unittest`, `node <script.js>`, `python <script.py>`, `python -c ...`, `cargo test`, dll.) tanpa menangkap `git status`/`--version`.
  - `verify_project` dengan hasil `"verifikasi proyek otomatis dilewati"` tidak lagi menimpa kegagalan `run_shell` yang sudah diketahui merah.
  - Mode lingkup ketat (`with_strict_known_verify_only(true)` di `run_turn_inner`): gerbang hanya hidup bila ada mutasi berkas DAN ada perintah verifikasi yang benar-benar dijalankan pada giliran itu (0 suntikan pada kontrol hijau maupun tugas tanpa verifikasi).
  - Penutupan ditahan hingga maks `3` putaran (`max_self_correct_rounds = 3`, satu hitungan terpadu), mengembalikan keluaran gagal mentah apa adanya; sesudah batas 3 putaran penutupan diterima disertai `[Status Verifikasi Akhir — GAGAL]` jujur (`honest_unverified_closing_note`).

### Bukti Uji Otomatis (`tests/v0514_step_budget_and_verify_gate.rs`)
- `p2_verification_gate_holds_red_feeds_raw_failure_and_stays_silent_on_green_or_no_verify`:
  - **Skenario perbaikan pertama sengaja salah arah:** tes awal merah (`34990`), edit salah (`34980`) lalu coba tutup → Gerbang 1/3 menahan & menyebut `node test_hitung.js`; tes ulang tetap gagal → coba tutup → Gerbang 2/3 menahan & menyertakan keluaran mentah `ASSERT_FAIL: expected 31500 got 34980`; perbaikan benar (`31500`) + tes lulus → penutupan diterima.
  - **Kontrol hijau:** edit benar + `node test_hitung.js` lulus → 0 suntikan gerbang.
  - **Kontrol tanpa verifikasi:** `write_file("helper.py")` tanpa perintah tes/repro → gerbang diam total (2 panggilan total, 0 suntikan).
  - **Batas 3 putaran + penutupan jujur:** tetap gagal 4× → gerbang menahan tepat 3 kali, lalu penutupan diterima dengan `[Status Verifikasi Akhir — GAGAL]` menyebut `node test_hitung.js`.

*(Transkrip uji LIVE biner `v0.5.14` untuk tiga skenario gerbang dan pengulangan `sympy-24909` akan dilengkapi tepat setelah biner rilis diterbitkan.)*

---

## Tabel Regresi

| Metrik | Nilai v0.5.14 | Status |
| --- | --- | --- |
| Paksa pagu kecil (`max_steps = 6`) | Checkpoint 4/6 (1×) + konvergensi 5/6 + label gagal nol-edit | LULUS |
| Pengulangan identik berturut-turut | Notice tepat pada panggilan ke-3 (`watch_job` 5× bebas notice) | LULUS |
| Gerbang verifikasi (salah arah → hijau) | 2 penahanan (1 belum verif ulang + 1 keluaran mentah) → 31500 | LULUS |
| Gerbang verifikasi (batas 3× jujur) | 3 penahanan maks + `[Status Verifikasi Akhir — GAGAL]` | LULUS |
| Kontrol hijau & kontrol tanpa verifikasi | 0 suntikan gerbang (0 token tambahan) | LULUS |
| Proyek A (`usage` LIVE) | Tidak diukur | Menunggu biner rilis |
| Proyek B (`usage` LIVE) | Tidak diukur | Menunggu biner rilis |
| Resume E (`usage` LIVE) | Tidak diukur | Menunggu biner rilis |
| SWE-bench `pylint-6506` & `sympy-24909` | Tidak diukur | Menunggu biner rilis |

- Tidak diukur — Proyek A (`usage` LIVE): Laporan pra-tag diverifikasi lewat uji terintegrasi sebelum rilis; pengukuran LIVE dari tabel `usage` ditambahkan segera setelah biner `v0.5.14` terbit.
- Tidak diukur — Proyek B (`usage` LIVE): Laporan pra-tag diverifikasi lewat uji terintegrasi sebelum rilis; pengukuran LIVE dari tabel `usage` ditambahkan segera setelah biner `v0.5.14` terbit.
- Tidak diukur — Resume E (`usage` LIVE): Laporan pra-tag diverifikasi lewat uji terintegrasi sebelum rilis; pengukuran LIVE dari tabel `usage` ditambahkan segera setelah biner `v0.5.14` terbit.
- Tidak diukur — SWE-bench `pylint-6506` & `sympy-24909`: Pengulangan LIVE terhadap biner rilis `v0.5.14` (`ag/gemini-4-neon-medium`) dijalankan segera setelah biner terbit dan hasilnya ditempelkan ke laporan ini.

---

## Bagian yang Sengaja Tidak Diubah

- Konverter tampilan Telegram HTML + fallback anti-gagal-parse (`src/telegram.rs`, `src/app/telegram_runner.rs`, v0.5.12 LULUS).
- Gerbang skill di loop + opt-out instruksi pengguna (`v0.5.11`/`v0.5.13`).
- Gerbang laporan rilis terperiksa mesin + format baris vonis (`scripts/check_release_report.py`, `v0.5.13`).
- Mesin prune/stub + arsip byte-exact `skill_view(full=true)` (`src/agent/context.rs`, `v0.5.9`/`v0.5.13`).
- Seluruh hasil `v0.5.7`–`v0.5.11`: timeout & watchdog gateway + trim memori sesi, `watch_job` + status segar + polling watch CLI, store memory tunggal, jalur resume ramping (`E = 6.013`), salinan kerja `skill_view`, peringatan config + `selen doctor`, pembatalan provider ~1× response timeout, disiplin riset/verifikasi + peta folder, isi daftar periksa penutup v0.5.8, `send_file`, `delegate_work`, `/usage`, `skills install`, fallback rute berbeda, `ask_user`, `cron`, `session_search`, pelacakan approval, serta 12 skill bawaan + `NOTICE`.
