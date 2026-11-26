# Laporan Hasil Penyelesaian SelenCode v0.5.13 — Opt-Out Gerbang Skill, Gerbang Laporan Terperiksa Mesin, & Kontrak Prune Skill

- **Versi rilis:** `v0.5.13`
- **Jumlah butir:** 3
- **Cakupan:** Penyelesaian dua butir v0.5.12 yang belum dikerjakan (opt-out gerbang skill + gerbang laporan terperiksa mesin) dan satu kontrak teruji (prune teks penuh `skill_view(full=true)` tanpa mengubah perilaku). Konverter tampilan Telegram v0.5.12 tidak disentuh sama sekali.
- **Konfirmasi gerbang laporan:** Berkas ini ditulis dalam format baru terperiksa mesin, di-commit ke `docs/reports/` sebelum tag `v0.5.13`, dan diverifikasi langsung oleh `scripts/check_release_report.py --tag v0.5.13`.

---

## Ringkasan Vonis (3 Butir)

- Butir 1 — LULUS: Instruksi penolakan skill eksplisit mematikan gerbang skill pada giliran berjalan (0 suntikan `skill_view` di DB), mencatat `[gerbang-skill] dilewati: instruksi pengguna` secara terlihat di log/UI/transkrip sesi, sementara giliran debugging tanpa larangan tetap menembak `gate_skill_step0` persis v0.5.11.
- Butir 2 — LULUS: Skrip pemeriksa mesin `scripts/check_release_report.py` terpasang di `.github/workflows/release.yml`, menolak contoh laporan yang kehilangan baris vonis atau memiliki sel angka kosong tanpa pernyataan eksplisit, dan meloloskan laporan lengkap termasuk laporan `v0.5.13` ini sendiri.
- Butir 3 — LULUS: Tes regresi `p3_skill_view_full_true_prune_contract_exact_1_call_then_stub_and_byte_exact_archive` memaku kontrak prune `skill_view(full=true)` tanpa mengubah kode prune (utuh di panggilan ke-1 sesudahnya, stub + penunjuk arsip mulai panggilan ke-2, dan arsip di disk byte-exact).

---

## 1. Perbaikan 1 — Opt-Out Gerbang Skill

### Perubahan Kode
- `src/agent/loop.rs` & `src/agent/mod.rs`:
  - Menambahkan `SKILL_GATE_SKIPPED_NOTE = "[gerbang-skill] dilewati: instruksi pengguna"`, `prompt_explicitly_forbids_skill`, dan `prompt_opts_out_of_skill_gate` (mendeteksi frasa penolakan eksplisit seperti `"jangan muat skill"`, `"tanpa skill"`, `"langsung saja kerjakan"`, `"skip skill"`, `"don't load skill"`, dll.).
  - Saat opt-out aktif pada giliran berjalan: `required_skill = None` (tidak ada suntikan `gate_skill_step0` maupun penahanan `edit_file`), `skills_view` dihapus dari `active_toolsets` giliran tersebut agar tidak dipicu hanya karena kata `"skill"` pada larangan pengguna, dan baris `[gerbang-skill] dilewati: instruksi pengguna` dikirim ke `UiEvent::Info` / `stderr` serta direkam di transkrip sesi SQLite.
  - Di luar instruksi eksplisit tersebut, perilaku gerbang v0.5.11 tidak berubah satu byte pun.

### Transkrip Dua Giliran & Urutan Tool dari DB (Skenario B: `hitung_total.js`, target `31500`)

#### A. Uji Terintegrasi Loop + SQLite DB (`tests/v0513_skill_optout_report_gate_prune.rs::p1_skill_gate_optout_suppresses_gate_logs_visibly_and_normal_turn_still_fires`)

1. **Giliran 1 — Berlarangan eksplisit:**
   - **Prompt:** `"Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Jangan muat skill apa pun, langsung saja perbaiki."`
   - **Baris log/transkrip terlihat (`UiEvent::Info` + tabel `messages` DB):**
     ```text
     [gerbang-skill] dilewati: instruksi pengguna
     ```
   - **Urutan tool dari tabel `messages` SQLite DB:**
     1. `c_read` → `read_file` (`hitung_total.js`)
     2. `c_edit` → `edit_file` (`const total = 35000 - 10;` → `const total = 35000 * 0.9;`)
   - **Jumlah suntikan `skill_view` / `gate_skill_*`:** `0`
   - **Hasil berkas `hitung_total.js`:** `const total = 35000 * 0.9;` (`31500`) — **LULUS**.

2. **Giliran 2 — Tanpa larangan (kontrol v0.5.11):**
   - **Prompt:** `"Di hitung_total.js ada bug diskon 10% dari 35000, seharusnya 31500.0 tapi sekarang 34990. Perbaiki."`
   - **Baris penekanan gerbang:** tidak ada (0 kemunculan `[gerbang-skill] dilewati`).
   - **Urutan tool dari tabel `messages` SQLite DB:**
     1. `c_read` → `read_file` (`hitung_total.js`)
     2. `gate_skill_step0` → `skill_view` (`{"name": "systematic-debugging"}`)
     3. `c_edit` → `edit_file` (`const total = 35000 - 10;` → `const total = 35000 * 0.9;`)
   - **Hasil berkas `hitung_total.js`:** `const total = 35000 * 0.9;` (`31500`) — **LULUS** (identik dengan v0.5.11).

*(Catatan: transkrip uji LIVE pada biner rilis `selen-windows-x64.exe` `v0.5.13` ditambahkan setelah biner rilis diterbitkan.)*

---

## 2. Perbaikan 2 — Gerbang Laporan Terperiksa Mesin

### Perubahan Kode & Workflow
- `scripts/check_release_report.py`: pemeriksa mesin mandiri (Python 3 stdlib) yang memvalidasi:
  1. Keberadaan tag versi target (`vX.Y.Z`) di dalam laporan.
  2. Deklarasi `Jumlah butir: N` (dan mencocokkan dengan berkas prompt bila diberikan lewat `--prompt-file` atau ditemukan di `docs/prompts/`).
  3. Tepat 1 baris vonis per butir `1..=N` berpola `Butir <n> — <LULUS|SEBAGIAN|GAGAL>: <satu kalimat bukti>` (menolak butir hilang, duplikat, di luar rentang, atau bukti kosong).
  4. Bagian `Tabel Regresi` dengan minimal 1 baris metrik di mana setiap sel angka wajib terisi angka **atau** metrik tersebut memiliki pernyataan eksplisit `Tidak diukur — <metrik>: <alasan>`.
  5. Bagian `Tidak Diubah` / `Jangan Diubah` tidak kosong.
- `.github/workflows/release.yml`: langkah `Gerbang Laporan Rilis Terperiksa Mesin (v0.5.13 Perbaikan 2)` menjalankan `python3 scripts/check_release_report.py --tag "${GITHUB_REF_NAME}"` sebelum build & publikasi rilis.

### Keluaran Pemeriksaan atas Tiga Contoh Buatan (`p2_release_report_gate_rejects_missing_verdict_and_empty_cells_and_accepts_complete`)

1. **Contoh buatan 1 — Satu baris vonis hilang (`Butir 2` hilang dari `Jumlah butir: 3`):**
   - **Exit code:** `1` (**DITOLAK**)
   - **Pesan pemeriksaan:**
     ```text
     GERBANG LAPORAN RILIS DITOLAK (contoh1_hilang_butir.md):
     ::error::Gerbang laporan GAGAL (contoh1_hilang_butir.md): Baris vonis hilang untuk: Butir 2 (diharapkan 3 butir: Butir 1..3, ditemukan: [1, 3]).
     ```
2. **Contoh buatan 2 — Sel angka kosong/placeholder (`-`) tanpa pernyataan eksplisit `Tidak diukur`:**
   - **Exit code:** `1` (**DITOLAK**)
   - **Pesan pemeriksaan:**
     ```text
     GERBANG LAPORAN RILIS DITOLAK (contoh2_sel_kosong.md):
     ::error::Gerbang laporan GAGAL (contoh2_sel_kosong.md): Sel angka kosong/belum diisi tanpa pernyataan eksplisit 'Tidak diukur — Proyek A (usage): <alasan>' pada baris 'Proyek A (usage)' kolom 'Nilai v0.5.13' (isi: '-').
     ```
3. **Contoh buatan 3 — Laporan lengkap (3 baris vonis + sel angka terisi + pernyataan `Tidak diukur` eksplisit):**
   - **Exit code:** `0` (**LOLOS**)
   - **Pesan pemeriksaan:**
     ```text
     GERBANG LAPORAN RILIS LULUS: contoh3_lengkap.md (...)
       - Vonis butir terverifikasi: 3/3 (Butir 1=LULUS, Butir 2=LULUS, Butir 3=LULUS)
       - Tabel regresi terverifikasi: 2 baris metrik, 1 pernyataan 'Tidak diukur' eksplisit.
     ```
4. **Cek tambahan:**
   - Cek silang `--prompt-file` (4 butir) vs laporan (3 butir) → **DITOLAK** (`Ketidakcocokan jumlah butir` + `Butir 4`).
   - Laporan `docs/reports/HASIL-V0512-TAMPILAN-TELEGRAM-HTML.md` lama diperiksa dengan `--expected-items 4` → **DITOLAK** (membuktikan celah gerbang v0.5.11/v0.5.12 benar-benar tertutup).
   - Laporan `v0.5.13` ini sendiri diperiksa dengan `--tag v0.5.13 --expected-items 3` → **LOLOS**.

---

## 3. Perbaikan 3 — Kontrak Teruji Prune Teks Penuh `skill_view(full=true)` (Tanpa Mengubah Perilaku)

- **Nama tes regresi:** `tests/v0513_skill_optout_report_gate_prune.rs::p3_skill_view_full_true_prune_contract_exact_1_call_then_stub_and_byte_exact_archive`
- **Perubahan pada mesin prune:** **0 baris** (perilaku mesin prune `src/agent/context.rs` dan pemanggilan `prune_old_skill_results_with_archive(..., 2)` di `src/agent/loop.rs` tidak diubah sama sekali).
- **Kontrak yang dipaku & diverifikasi:**
  - **(a) Panggilan LLM ke-1 sesudah `skill_view(name="systematic-debugging", full=true)`:** hasil teks penuh (`> 10.000` karakter, utuh tanpa stub `isi penuh terarsip:`) hadir persis untuk dikonsumsi model.
  - **(b) Mulai panggilan LLM ke-2 sesudahnya:** hasil yang sama telah berubah menjadi stub ringkas (`< 1.000` karakter) memuat `[Skill \`systematic-debugging\` aktif — isi penuh terarsip: <path>]`.
  - **(c) Arsip di disk byte-exact & dapat diambil ulang:** berkas di `<path>` benar-benar ada di disk, `std::fs::read(&archive_path) == full_on_call1.as_bytes()` (identik byte-per-byte), dapat dibaca ulang utuh, dan snapshot resume berikutnya (`build_resume_snapshot`) tetap ramping (`< 3.000` karakter).
- **Temuan perbedaan perilaku aktual vs kontrak terukur:** **Tidak ada** — perilaku aktual cocok 100% dengan kontrak terukur.

---

## Tabel Regresi

| Metrik | Nilai v0.5.13 | Status |
| --- | --- | --- |
| Skenario B opt-out (`hitung_total.js`) | 31500 (0 suntikan `skill_view`, 1 catatan `[gerbang-skill] dilewati`) | LULUS |
| Skenario B kontrol tanpa larangan | 31500 (1 `gate_skill_step0` sebelum `edit_file`) | LULUS |
| Kontrak prune `skill_view(full=true)` | 1 panggilan penuh (>10.000 char) → panggilan ke-2 stub (<1.000 char) + arsip byte-exact | LULUS |
| Pemeriksa laporan mesin (contoh buatan) | 2 ditolak (hilang `Butir 2`, sel kosong) + 1 lolos (lengkap) | LULUS |
| Proyek A (`usage` penuh) | Tidak diukur | Uji terarah |
| Proyek B (`usage` penuh) | Tidak diukur | Uji terarah |
| Resume E (`usage` penuh) | Tidak diukur | Uji terarah |

- Tidak diukur — Proyek A (`usage` penuh): Prompt rilis v0.5.13 secara eksplisit meminta uji terarah tanpa baterai A/B/E penuh; prompt/tool/loop non-opt-out identik dengan baseline v0.5.11 (`4.825` token).
- Tidak diukur — Proyek B (`usage` penuh): Prompt rilis v0.5.13 secara eksplisit meminta uji terarah tanpa baterai A/B/E penuh; baseline v0.5.11 (`5.246` token) tidak tersentuh.
- Tidak diukur — Resume E (`usage` penuh): Prompt rilis v0.5.13 secara eksplisit meminta uji terarah tanpa baterai A/B/E penuh; jalur resume baseline v0.5.11 (`6.013`–`6.771` token) tidak diubah dan tetap diuji otomatis.

---

## Bagian yang Sengaja Tidak Diubah

- Konverter tampilan Telegram HTML + fallback anti-gagal-parse (`src/telegram.rs`, `src/app/telegram_runner.rs`, v0.5.12 LULUS).
- Perilaku gerbang skill di luar jalur opt-out Perbaikan 1 (jaminan termuat sebelum aksi pengubah pertama, pengecualian tugas trivial, dan biaya terukur v0.5.11).
- Mesin prune/stub + arsip (`src/agent/context.rs`) di luar penambahan tes regresi kontrak Perbaikan 3.
- Seluruh hasil v0.5.7–v0.5.11: timeout & watchdog gateway + batas memori, `watch_job` + status segar, polling watch CLI, store memory tunggal, jalur resume ramping (`6.013`), salinan kerja `skill_view`, peringatan config + `selen doctor`, pembatalan provider ~1×, `send_file`, `delegate_work`, `/usage`, `skills install`, fallback rute berbeda, `ask_user`, `cron`, `session_search`, pelacakan approval, serta 12 skill bawaan + `NOTICE`.
