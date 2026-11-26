# Laporan Hasil Update SelenCode v0.5.16 — Perbaikan Kebocoran Task Mengetik Telegram + Klasifier Nomina "Perbaikan" + Konvergensi Pagu Kecil

- **Versi rilis:** `v0.5.16`
- **Jumlah butir:** 3
- **Pembagian kerja (keputusan pemilik):** Pembangunan, `cargo test` (unit/mock/regresi), dan rilis dikerjakan agent pengembang; seluruh pengujian penerimaan live & pengukuran angka regresi A/B/E dijalankan oleh **Muse**.

---

## Ringkasan Vonis (3 Butir)

- Butir 1 — LULUS: Task indikator mengetik Telegram (`TypingLoopGuard`) dan `collector` gelembung progres di `src/telegram.rs` + `src/app/telegram_runner.rs` kini memakai sinyal berhenti teringat (`tokio::sync::watch`) serta `JoinHandle::abort()` eksplisit + `Drop` guard, dan lulus uji lomba sinyal berhenti di tengah `send_typing()` lambat tanpa panggilan tersisa.
- Butir 2 — LULUS: Klasifier `prompt_requests_file_change` di `src/agent/loop.rs` mengenali konstruksi nomina & verba Indonesia ("ada perbaikan yang harus dilakukan", "perlu perubahan", "butuh penambahan", "sekalian perbaikannya", "tolong diperbaiki") sekaligus menolak kontrol negatif diskusi/penjelasan ("apa perbaikan terbaik…?", "jelaskan perubahan…", "kenapa perbaikan kemarin gagal?").
- Butir 3 — LULUS: `compute_convergence_step` di `src/agent/loop.rs` menerapkan lantai eksplorasi bermakna (`max(max_steps / 2, 3)` dibatasi `max_steps - 1`) sehingga pagu 6 menembak di langkah `3` (tidak prematur di langkah 1–2), sedangkan pagu 15 (`10`) dan pagu 25 (`20`) tetap persis `v0.5.15`.

---

## 1. Perbaikan 1 — Perbaikan Kebocoran Task Pengetik Telegram & Audit Task Per-Giliran

- **Berkas & fungsi diubah:**
  - `src/telegram.rs` (`TypingLoopGuard::spawn`, `TypingLoopGuard::spawn_for_client`, `TypingLoopGuard::stop_and_join`, `Drop for TypingLoopGuard`).
  - `src/app/telegram_runner.rs` (`AbortTaskOnDrop`, penggantian `typing_stop: Notify` + `typing_task` ter-detach dengan `TypingLoopGuard`, pengawalan `typing_init_guard`, serta penambahan sinyal berhenti `watch` + `abort()` eksplisit pada `collector` gelembung progres setelah tenggang 3 detik).
- **Bukti unit uji lomba (`tests/v0516_typing_leak_classifier_small_budget.rs::p1_typing_loop_stop_signal_during_slow_send_never_lost_or_leaked`):**
  - Simulasi `send_typing()` lambat (600 ms) dengan interval 80 ms: sinyal `stop_and_join` dikirim TEPAT saat task berada di dalam `send_typing().await` → loop langsung berhenti dalam `< 250 ms`, dan sesudah menunggu 250 ms (> 3× interval) tercatat **`0` panggilan tambahan**.
  - Simulasi `Drop` mendadak (tanpa `stop_and_join`, mewakili jalur batal/timeout/galat/panik): menjatuhkan `TypingLoopGuard` di tengah `send_typing()` langsung meng-abort task dan menghasilkan **`0` panggilan tambahan** sesudahnya.

---

## 2. Perbaikan 2 — Klasifier Nomina & Verba Indonesia (`prompt_requests_file_change`)

- **Berkas & fungsi diubah:** `src/agent/loop.rs` (`prompt_requests_file_change`).
- **Daftar kalimat uji unit (`p2_indonesian_nominal_verbal_and_negative_discussion_classifier`, semua HIJAU):**
  - **Positif nomina (`true`):**
    - `"Ada perbaikan yang harus dilakukan pada berkas hitung_total.js."` → `true`
    - `"Perlu perubahan pada konfigurasi parser di src/config.rs."` → `true`
    - `"Butuh penambahan validasi input di modul autentikasi."` → `true`
    - `"Sekalian perbaikannya untuk fungsi diskon di calc.js."` → `true`
    - `"Ada penghapusan field lama yang belum dikerjakan di schema.sql."` → `true`
    - `"Mohon penyesuaian format tanggal di laporan bulanan."` → `true`
    - `"Lakukan pembaruan dokumentasi pada config.example.toml."` → `true`
    - `"Bantu pembenahan kode di handler telegram."` → `true`
  - **Positif verba aktif/pasif (`true`):**
    - `"Tolong diperbaiki fungsi pembagian nol di calc.js."` → `true`
    - `"Perbaiki bug diskon di hitung_total.js agar hasilnya 31500."` → `true`
    - `"Ubah nilai default timeout di src/config.rs."` → `true`
    - `"Tambahkan unit test untuk kasus kosong."` → `true`
    - `"Hapus kode duplikat di src/agent/loop.rs."` → `true`
    - `"Kenapa hitung_total.js salah? Tolong perbaiki sekalian."` → `true`
  - **Kontrol negatif diskusi/penjelasan (`false`):**
    - `"apa perbaikan terbaik untuk fungsi ini?"` → `false`
    - `"jelaskan perubahan di berkas ini"` → `false`
    - `"kenapa perbaikan kemarin gagal?"` → `false`
    - `"Bagaimana cara kerja penambahan fitur ini?"` → `false`
    - `"Sebutkan daftar perubahan di commit terakhir."` → `false`
    - `"Ringkas perbaikan yang dilakukan pada rilis kemarin."` → `false`
    - `"Apakah perubahan ini aman untuk produksi?"` → `false`
    - `"Jelaskan apa itu borrow checker."` → `false`
    - `"Menurutmu apa penyesuaian yang paling bersih?"` → `false`

---

## 3. Perbaikan 3 — Lantai Eksplorasi Konvergensi Pagu Kecil (`≤ 6`)

- **Berkas & fungsi diubah:** `src/agent/loop.rs` (`compute_convergence_step`).
- **Aturan:** `min(max_steps - 5, ceil(0.85 * max_steps)).max(max(max_steps / 2, 3)).min(max_steps - 1)`.
- **Titik tembak terverifikasi (`p3_small_budget_convergence_not_premature_and_v0516_report_passes_gate`):**
  - **Pagu 6:** langkah **`3`** (langkah 1–2 bebas eksplorasi, tidak prematur).
  - **Pagu 15:** langkah **`10`** (persis `v0.5.15`, tidak bergeser).
  - **Pagu 25:** langkah **`20`** (persis `v0.5.15`, tidak bergeser).
  - **Pagu 100:** langkah **`85`** (persis `v0.5.15`).
  - **Tanpa pagu (`0`):** `0` (pakai ambang `no_edit_convergence_steps = 20`).

---

## Tabel Regresi

| Metrik | Nilai v0.5.16 | Status |
| --- | --- | --- |
| Uji lomba `TypingLoopGuard` (sinyal di tengah HTTP lambat + Drop) | 0 panggilan `sendChatAction` setelah berhenti (< 250 ms) | LULUS |
| Klasifier Indonesia (8 nomina + 6 verba + 9 negatif diskusi) | 23/23 tepat (14 `true`, 9 `false`) | LULUS |
| Titik tembak konvergensi berpagu (6 / 15 / 25) | Pagu 6 → langkah 3; pagu 15 → langkah 10; pagu 25 → langkah 20 | LULUS |
| Proyek A (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Proyek B (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Resume E (`usage` LIVE) | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |
| Uji penerimaan live v0.5.16 | pengukuran penerimaan oleh Muse | Diserahkan ke Muse |

---

## Bagian yang Sengaja Tidak Diubah

- Penghapusan pagu bawaan (`max_steps = 0`) + notice tanpa-pagu ambang 20 + tag langkah Telegram/TUI (`v0.5.15`), penempelan pasca-generasi kalimat kegagalan nol-edit (`v0.5.15`), integritas keadaan verifikasi mapan + kutipan penutup (`v0.5.15`), catatan berkas tes diubah (`v0.5.15`).
- Checkpoint rasio 0,7 + panggilan anugerah + isi wrap-up paksa + penjaga pengulangan 3× (`v0.5.14`), konverter tampilan Telegram HTML + fallback anti-gagal-parse (`v0.5.12`), gerbang skill + opt-out (`v0.5.11`/`v0.5.13`), gerbang laporan rilis terperiksa mesin (`v0.5.13`/`v0.5.15`).
- Seluruh hasil `v0.5.7`–`v0.5.11`: timeout & watchdog gateway + trim memori sesi, `watch_job` + status segar + polling watch CLI, store memory tunggal, jalur resume ramping, salinan kerja `skill_view` + prune teks penuh `full=true`, peringatan config + `selen doctor`, pembatalan provider ~1×, `send_file`, `delegate_work`, `/usage`, `skills install`, fallback rute berbeda, `ask_user`, `cron`, `session_search`, pelacakan approval, serta 12 skill bawaan + `NOTICE`.

---

## Bukti CI & Rilis Publik `v0.5.16`

- **CI GitHub Actions (Linux + Windows x64):** Run `#45902924285` pada commit `a2919b6` — **LULUS** (`Cargo Test & Visual Render (Linux)` & `Cargo Test & Check (Windows x64)` keduanya `success`, termasuk seluruh tes `v0516_typing_leak_classifier_small_budget`).
- **Release Workflow & Gerbang Laporan Rilis Mesin:** Run `#45904268616` pada tag `v0.5.16` — **LULUS** (`scripts/check_release_report.py --tag v0.5.16` lulus untuk 3 baris vonis + pernyataan `pengukuran penerimaan oleh Muse`; 5/5 job `success`).
- **Halaman Rilis Publik:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.16 (`draft = false`, 3 aset lengkap):
  - `selen-linux-x64` (`16.951.600` byte)
  - `selen-termux-arm64` (`15.260.600` byte)
  - `selen-windows-x64.exe` (`15.645.184` byte)
