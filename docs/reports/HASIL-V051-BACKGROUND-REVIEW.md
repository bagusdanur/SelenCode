# Hasil Update SelenCode v0.5.1 — Background Self-Improvement Review

- **Versi:** `v0.5.1` (dari `v0.5.0`)
- **Rilis Publik:** `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.1` (`draft=false`, 3 aset biner: `selen-linux-x64` 15.07 MB, `selen-termux-arm64` 13.59 MB, `selen-windows-x64.exe` 13.85 MB)
- **Commit Rilis:** `fe6a9be` (CI Linux & Windows x64 100% hijau)
- **Tanggal:** 26 November 2026

---

## 1. Rancangan Pemicu, Model Aux, Anggaran, & Keamanan

### Pemicu & Cooldown
- **Tidak pernah memblokir giliran aktif:** panggilan review sinkron lama di dalam `src/agent/loop.rs` dihapus dan diganti dengan mesin Background Self-Improvement Review (`src/agent/review.rs`) yang dijalankan di latar oleh runner CLI/TUI/Telegram.
- **Titik pemicu:**
  1. Setelah sesi berakhir (`/exit`, EOF, `/new`, `selen run` selesai).
  2. Setelah setiap **15 giliran tuntas** (`[review] turns_interval = 15`) dalam sesi panjang.
- **Syarat kerja substantif (`is_session_substantive`):** hanya aktif bila sesi memiliki minimal **5 panggilan tool** (`[review] min_tool_calls = 5`) ATAU terdeteksi **koreksi/penegasan prosedur** dari pengguna di tengah sesi. Sesi tanya-jawab trivial dilewati tanpa memanggil model.
- **Cooldown persisten:** maks **1 review per sesi per 30 menit** (`[review] cooldown_mins = 30`), dicatat pada tabel SQLite `review_runs`.
- **Anti-bentrok giliran aktif:** dijaga oleh `active_turn` (TUI) dan `busy_chats` (Telegram) agar tidak pernah berjalan bersamaan dengan giliran aktif.

### Model Aux & Anggaran Token
- **Slot model aux (`[aux]` di `src/config.rs`):**
  - `[aux] review_model` (opsional) → bila kosong, ikut `[aux] compression_model` → bila keduanya kosong, **review dilewati** (tidak pernah diam-diam memakai model utama yang mahal).
- **Anggaran per jalan (`[review] max_tokens`, default `6000`):**
  - Masukan dirakit dari transkrip **sesudah `prune_old_tool_results`** (tidak pernah memuat ulang tool result raksasa), indeks skill Tingkat-1 + isi maks 3 skill relevan, dan catatan memory aktif — semuanya melewati `redact_secrets`.
  - Sebelum memanggil provider aux, estimasi token input dicek terhadap `max_tokens` (wajib menyisakan ≥200 token output); bila melampaui anggaran atau provider/JSON gagal, review berhenti **diam-diam** (hanya dicatat ke log, tidak pernah mengganggu sesi berikutnya).
- **Pencatatan biaya terpisah di tabel `usage`:** kolom `kind = 'review'` dengan `session_id = "review:<orig_session_id>"` sehingga metrik token jalur utama (`WHERE session_id = <orig_session_id>`) tetap 100% bersih dan biaya review dapat diaudit terpisah.

### Batas Aksi Aman vs Staging Persetujuan
- **Boleh diterapkan langsung** (hanya pada folder skill + memory milik SelenCode, tidak pernah menyentuh kode/config/file proyek):
  1. `patch_skill`: patch bedah pada skill yang sudah ada (bila menghapus >50% baris lama, otomatis dialihkan ke staging `overwrite_skill`).
  2. `add_skill_reference`: menambah file referensi **baru** kecil (`≤4 KB`) di `references/`, `templates/`, atau `scripts/` dengan validasi anti-traversal & anti-secret-file (bila file sudah ada, otomatis dialihkan ke staging).
  3. `add_memory`: menambah catatan memory **baru** yang sempit & faktual (lewat validasi anti-rahasia).
- **Dilarang tanpa persetujuan → wajib masuk staging (`review_proposals`):**
  - `delete_memory`, `overwrite_memory`, `delete_skill`, `overwrite_skill`.
  - Dikelola lewat `/review pending`, `/review approve <id>`, `/review discard <id>`, `/review status` di CLI/TUI/Telegram, subcommand CLI `selen review ...`, dan tombol inline Telegram (`Setujui #<id>` / `Buang #<id>`).
- **Laporan 1 baris:** hanya dikirim bila ada perubahan atau proposal staging (review tanpa temuan = 0 pesan). Di Telegram selalu dikirim sebagai **pesan baru**; di CLI/TUI ditampilkan di akhir sesi atau saat sesi berikutnya dimulai (`take_undelivered_review_reports`).

---

## 2. File yang Disentuh
- `Cargo.toml`, `Cargo.lock` (bump `0.5.1`)
- `src/config.rs` (`AuxCfg::review_model`, `AuxCfg::effective_review_model`, `ReviewCfg`)
- `src/store.rs` (migrasi aman `usage.kind`, `record_review_usage`, tabel `review_proposals` & `review_runs`, `get_memory`, `take_undelivered_review_reports`)
- `src/agent/review.rs`, `src/agent/mod.rs`, `src/agent/loop.rs` (mesin Background Self-Improvement Review + hapus review sinkron lama di loop utama)
- `src/app/mod.rs`, `src/app/session.rs`, `src/app/commands.rs`, `src/app/cli.rs`, `src/main.rs`, `src/app/plain_runner.rs`, `src/app/tui_runner.rs`, `src/app/telegram_runner.rs`, `src/repl.rs`, `src/telegram.rs`, `src/tui/mod.rs`
- `tests/v051_background_review.rs`, `tests/fase3.rs`, `.github/workflows/ci.yml`

---

## 3. Bukti 4 Uji Penerimaan & Regresi

### A. Tes Otomatis (`tests/v051_background_review.rs` — CI Linux & Windows x64 lulus 100%)
- `aux_model_resolution_and_trivial_control_eligibility`: verifikasi fallback `review_model -> compression_model -> skip`, sesi trivial ditolak, parser `/review` & callback Telegram `rv:<id>:a|d`.
- `scenario1_correction_patches_skill_records_separate_usage_and_enforces_cooldown`: verifikasi patch skill + diff + memory baru + pesan 1 baris + `usage` terpisah (`kind='review'`) + laporan tertunda CLI + cooldown 30 menit.
- `scenario2_delete_and_overwrite_are_staged_until_approved_or_discarded`: verifikasi 2 `delete_memory` + 1 `overwrite_skill` masuk staging (data utuh sebelum approve), `/review approve` menghapus memory #1, `/review discard` menjaga memory #2 tetap utuh.
- `scenario3_control_silent_failures_budget_and_safe_reference_guard`: verifikasi kontrol trivial (0 call), hasil kosong (0 pesan), kegagalan provider diam, penolakan `../escape.md`, dan auto-staging saat mencoba menimpa referensi yang sudah ada.

### B. Uji Live Biner Rilis `v0.5.1` (`selen-windows-x64.exe` pada 9Router `ag/gemini-4-neon-medium`)

#### 1) Skenario 1 — Koreksi prosedur di tengah sesi (`sc1-bg-patch`)
- **Alur:** agen utama membaca skill `raw-downloader` (`/tmp/raws-old`), menulis `run.sh`, lalu pengguna mengoreksi di giliran ke-2: *"Salah, folder kanonik yang benar adalah `~/data/raws-kanonik`, bukan `/tmp/raws-old`. Perbaiki file `run.sh` saja (jangan sentuh file lain)."* Agen utama hanya memperbaiki `run.sh`. Saat `/exit`, Background Review berjalan otomatis.
- **Pesan laporan nyata yang tercetak:**
  ```text
  💾 Self-improvement review: Skill 'raw-downloader' patched (Koreksi folder simpan kanonik ke ~/data/raws-kanonik)
  ```
- **Diff nyata pada `.selencode/skills/ops/raw-downloader/SKILL.md`:**
  ```diff
  -2. Simpan hasil unduhan ke folder `/tmp/raws-old`.
  +2. Simpan hasil unduhan ke folder `~/data/raws-kanonik`.
  ```
- **Biaya token (`usage`):** jalur utama `5/5` calls (`23.376` in, avg `4.675`); jalur review terpisah (`kind='review'`): **`3.375` in / `851` out** (`4.226` total ≤ `6.000`).

#### 2) Skenario 2 — Memory lama terbukti usang (`sc2-bg-staging`)
- **Alur:** 2 memory lama ditanam (`#1`: `legacy-sync.internal:8080`, `#2`: `old-backup-2021.sh`). Pengguna menulis `DEPLOY.md` yang menyatakan keduanya sudah usang, tanpa memanggil `manage_memory`. Saat `/exit`, Background Review berjalan.
- **Pesan laporan nyata:**
  ```text
  💾 Self-improvement review: 2 usulan hapus/timpa menunggu persetujuanmu — /review pending
  ```
- **Isi staging (`selen review pending`):**
  ```text
  === Usulan Review Tertunda (2) ===
    * #2  ·  overwrite_memory (2)  ·  Timpa memory #2 (Skrip backup lama sudah usang)
      lama: Skrip backup lama: old-backup-2021.sh
      baru: Skrip backup aktif: backup-2026.sh (menggantikan old-backup-2021.sh)
    * #1  ·  overwrite_memory (1)  ·  Timpa memory #1 (Server sync lama sudah usang)
      lama: Server sync lama: legacy-sync.internal:8080
      baru: Server sync aktif: sync-v2.internal:9000 (menggantikan legacy-sync.internal:8080)
  ```
- **Verifikasi staging:** sebelum approve, kedua memory masih **100% utuh** (`legacy-sync.internal:8080` & `old-backup-2021.sh`). Sesudah `selen review approve 1` → memory `#1` diperbarui (`status: approved`). Sesudah `selen review discard 2` → memory `#2` **tetap utuh** (`status: discarded`). (Jalur `delete_memory` penuh juga terverifikasi di tes otomatis `scenario2_delete_and_overwrite_are_staged_until_approved_or_discarded`.)
- **Biaya token review (`usage`, `kind='review'`):** **`3.190` in / `839` out** (`4.029` total ≤ `6.000`).

#### 3) Skenario 3 — Kontrol trivial (`sc3-kontrol`)
- Prompt: *"Apa ibu kota Jepang? Jawab satu kalimat singkat saja."* (dengan `[aux] review_model` aktif).
- Hasil: `review_usage = []`, `review_runs = []`, **0 panggilan model review, 0 perubahan file, 0 pesan**.

#### 4) Skenario 4 — Regresi Token Jalur Utama A/B/E (tabel `usage`, `v0.5.1`)

| Skenario | Calls (`usage` / `metrics`) | Avg Tools | Input Token per Panggilan (`usage`) | Rata-rata In/Call | Total In / Out | Status |
| :--- | :---: | :---: | :--- | :---: | :---: | :--- |
| **Proyek A (`rekap.js`)** | `6 / 6` | `8` | `3.611 → 3.814 → 4.259 → 4.350 → 4.513 → 4.687` | `4.206` | `25.234 / 924` | **LULUS** |
| **Proyek B (`hitung.js`)** | `7 / 7` | `8` | `3.624 → 3.746 → 4.450 → 4.761 → 4.924 → 5.234 → 5.491` | `4.604` | `32.230 / 1.847` | **LULUS** |
| **Gabungan A + B** | `13 / 13` | `8` | 13 panggilan (`57.464` total in) | **`4.420`** | `57.464 / 2.771` | **LULUS** (≤ 5.000) |
| **Proyek E + Resume (`kata.txt`)** | `5 / 5` | `8` | Turn 1: `3.593 → 3.756` · **Resume Turn 2:** `3.829 → 3.948 → 4.067` | **`3.839`** | `19.193 / 396` | **LULUS** (≤ 7.500) |

---

## 4. Yang Sengaja Tidak Diubah
Seluruh perbaikan v0.5.0 (pengiriman & tampilan Telegram, PATH sesi, perilaku menulis skill), sistem hemat token v0.3.2–v0.4.0, Skills System, plan mode, sesi/checkpoint, delegate, TUI Moonlit, `selen update`, trait `Provider`/`Tool`, sandbox, serta default provider/model (`9Router`, `ag/gemini-3-flash`).
