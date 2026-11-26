# Hasil Update SelenCode v0.5.7 — 3 Perbaikan + 5 Fitur Lanjutan

- **Versi:** `v0.5.7` (satu rilis utuh dari `v0.5.6`)
- **Rilis Publik:** `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.7` (`draft=false`, 3 aset biner: `selen-linux-x64` 15.97 MB, `selen-termux-arm64` 14.37 MB, `selen-windows-x64.exe` 14.74 MB)
- **Commit Rilis:** `4235711` (CI Linux & Windows x64 100% hijau — workflow CI `45783607561` & Release `45784977203`)
- **Suite Tes Otomatis Baru:** `tests/v057_improvements.rs` (seluruh 32 suite tes lulus pada Linux & Windows x64)
- **Tanggal:** 26 November 2026

---

## 1. Perbaikan 1 — Turunkan Lantai Token Resume (≤ 7.500)

### A. Pengukuran Komposisi Panggilan Pertama Resume (Sebelum vs Sesudah)
Diukur langsung dari `call_token_metrics` dan `usage` pada Skenario E identik (tanam sandi `BULAN-PERAK-42` + `SelenCode` di `kata.txt` pada giliran 1, lalu `selen resume --yes --plain` menanyakan satu kalimat, model `ag/gemini-4-neon-medium`):

| Komponen Panggilan #1 Resume | `v0.5.6` (Sebelum) | `v0.5.7` (Sesudah) | Selisih |
| :--- | :---: | :---: | :---: |
| `est_system_tokens` (prompt statis + snapshot dinamis) | `1.429` | **`1.060`** | **`-369`** |
| `est_tools_tokens` (skema tool aktif) | `875` (`8` tool) | **`517`** (`6` tool) | **`-358`** |
| `est_history_tokens` (riwayat percakapan) | `209` | **`175`** | **`-34`** |
| `est_tool_results_tokens` (hasil tool lama) | `27` | **`0`** | **`-27`** |
| **Input Token Aktual (`usage`) — Panggilan #1 Resume** | **`4.254`** | **`3.621`** | **`-633` (-14,9%)** |
| **Estimasi Giliran Resume 2-Panggilan (mis. + `read_file`)** | `8.147` (GAGAL > 7.500) | **`~7.250`** (**LULUS ≤ 7.500**) | **`-897`** |

### B. Perubahan yang Dibuat
1. **Toolset Resume Super-Ramping (`src/tools/toolsets.rs`, `src/agent/loop.rs`):** sesi resume (`ctx.is_resumed_session && !messages.is_empty()`) menggunakan `RESUME_CORE_TOOLS` (`read_file`, `write_file`, `edit_file`, `search_text`, `run_shell` + `activate_toolset` = **6 tool** pada panggilan #1), menunda `list_files`, `git_read`, `verify`, `skill_view`, `session_search`, `watch_job`, `send_file`, `delegate`, dan MCP sampai benar-benar dibutuhkan oleh prompt atau diaktifkan lewat `activate_toolset` di giliran yang sama.
2. **Snapshot Resume Ringkas (`build_resume_snapshot` di `src/agent/context.rs`):** menyegarkan pesan System di snapshot pengiriman API (tanpa memutasi DB) dan meringkas pasangan `ToolUse`/`ToolResult` giliran-giliran sebelumnya menjadi ringkasan 1 baris sambil menjaga 100% pesan User dan jawaban Assistant.
3. **Perampingan Prompt & Hint (`src/agent/context.rs`, `src/tools/toolsets.rs`, `src/tools/{file,shell,search,skill}.rs`):** memadatkan `inactive_toolsets_hint` (~250 → ~60 token) dan deskripsi skema tool inti tanpa mengubah nama tool/parameter atau perilaku sesi baru.

### C. Bukti Uji Penerimaan Live (`v0.5.7`)
- **Skenario E identik:**
  - Jawaban: `Sandi rahasia yang disimpan di kata.txt adalah BULAN-PERAK-42 untuk proyek SelenCode.`
  - Total input giliran resume (tabel `usage`): **`3.621` token** (**LULUS ≤ 7.500**).
- **Skenario E lanjutan (sesi resume yang sama):** prompt kedua meminta mencari kode `AUDIT-V057-881` di sesi lain → `session_search` aktif & langsung terpanggil pada giliran tersebut (`[tool] session_search {"query":"AUDIT-V057-881"}`), menemukan sesi `220aa8ec-b06e-4564-b0eb-75fccc1b8682` (`3.979 → 4.361` in).

---

## 2. Perbaikan 2 — Kebiasaan Memuat Skill Bawaan yang Cocok

### Perubahan yang Dibuat
1. Menambahkan aturan perilaku tegas di `build_system_prompt` (`src/agent/context.rs`): cek indeks skill di snapshot **SEBELUM** aksi tool pertama; bila jenis tugas jelas cocok (debugging, TDD, code review, wawancara desain, inspeksi repo, riset bersitasi), WAJIB panggil `skill_view(name)` dulu (1 skill cukup); JANGAN memuat skill pada tugas trivial. `[skills] auto_load` tetap opt-in (default kosong).
2. Menajamkan deskripsi indeks (≤ 60 char) pada 4 skill bawaan di `assets/builtin_skills/dev/` (`systematic-debugging`, `test-driven-development`, `requesting-code-review`, `grill-me`) serta penyegaran aman pada upgrade versi di `seed_builtin_skills_to` (`src/skills.rs`) tanpa menimpa skill yang dimodifikasi pengguna.

### Bukti Uji Penerimaan Live (`v0.5.7`, dari DB `messages` & `usage`)

| Skenario Uji (Tanpa Menyebut Nama Skill) | `skill_view` Terpanggil (DB) | Hasil Akhir | Total In / Avg In (`usage`) | Status |
| :--- | :--- | :--- | :---: | :--- |
| **Debug 1: Diskon ganda (`pricing.js`)** | `skill_view(systematic-debugging)` (#1 sebelum tool lain) | `TOTAL: 31500` (benar) | `72.629` / `8.070` (`9` call) | **LULUS** |
| **Debug 2: Pajak sebelum diskon (`tax.js`)** | `skill_view(systematic-debugging)` (#1 sebelum tool lain) | `FINAL: 99900` (benar) | `71.632` / `7.959` (`9` call) | **LULUS** |
| **TDD: `parseRupiah` (`rupiah.js` + tes)** | `skill_view(test-driven-development)` (#1 sebelum tool lain) | 3 siklus RED→GREEN→REFACTOR, **4/4 tes lulus** | `142.093` / `8.881` (`16` call) | **LULUS** |
| **Kontrol Trivial** (*ibu kota Jepang*) | `[]` (**0 skill**) | `Ibu kota Jepang adalah Tokyo.` | `3.600` / `3.600` (`1` call) | **LULUS** |
| **Kontrol Skenario A (`rekap.js`)** | `[]` (**0 skill**) | `Total nilai: 240`, `Rata-rata nilai: 80` | `25.188` / `4.198` (`6` call) | **LULUS** |

### Perbandingan Token Skenario B (`hitung.js`): `v0.5.6` vs `v0.5.7`
- **`v0.5.6`:** `8` panggilan, `38.623` total in, rata-rata `4.828` in/call.
- **`v0.5.7`:** `7` panggilan, **`32.108` total in**, rata-rata **`4.587` in/call** (**turun -16,9% total in** / **-5,0% avg/call**, jauh di bawah batas ±10%).

---

## 3. Perbaikan 3 — Pemantau Tugas Asinkron (`watch_job` + Loop Gateway + `/watch`)

### Perubahan yang Dibuat
- **Mesin pemantau deterministik (`src/watch.rs`, `src/store.rs` tabel `job_watches`):** mendukung metode pemeriksaan berkas status (`status_file`) maupun perintah shell baca-saja (`check_command`, menolak chaining/mutasi/perintah terlarang), interval default `60` dtk (min `30` dtk), batas umur default `6` jam, maks `5` pantauan aktif per chat.
- **Loop gateway persisten (`src/app/telegram_runner.rs`):** berjalan setiap 5 dtk memeriksa pantauan jatuh tempo dari SQLite (otomatis dilanjutkan sesudah gateway restart), **0 panggilan LLM / 0 token provider**, mengirim pesan BARU Telegram (format bersih maks 1 emoji) hanya saat tahap berubah / selesai / gagal (termasuk gagal periksa 3x berturut) / kedaluwarsa, dan **0 pesan saat tidak berubah** (anti-spam).
- **Tool & kendali pengguna:** `watch_job` (`src/tools/watch.rs`), aturan wajib di system prompt agar agent mendaftarkan `watch_job` setiap mengantrekan job asinkron, `/watch list` & `/watch cancel <id>` (Telegram + CLI/TUI), CLI `selen watch list|cancel <id>`, serta laporan penutup CLI tertunda saat sesi berikutnya dibuka (`take_undelivered_watch_reports`).

### Bukti Uji Penerimaan
- **Tes otomatis end-to-end (`perbaikan3_watch_job_lifecycle_antispam_failure_restart_and_zero_tokens`):**
  1. Tahap `S1` → 1 pesan progres (`⏳ Progres 'Comic-auto Hold On': S1 (1/3)`).
  2. Poll ulang saat tahap belum berubah → `notification = None` (**0 pesan, anti-spam**).
  3. Simulasi gateway restart (buka ulang `Store::open_at`) → pantauan aktif tetap dilanjutkan otomatis.
  4. Tahap `S2` → 1 pesan progres; lalu `SELESAI` → laporan akhir (`✅ Tugas 'Comic-auto Hold On' (#1) selesai. ... Lokasi: out/hold-on.zip`) & berhenti.
  5. Status `FAILED` dan gagal poll `3x` berturut → laporan kegagalan berisi teks error & berhenti.
  6. Query tabel `usage` sesudah seluruh siklus pantauan → **0 baris / 0 token provider**.
- **Uji live biner `v0.5.7`:** agent memanggil `[tool] watch_job {"interval_secs":30,"label":"Comic-auto Hold On","status_file":"job_status.txt"}`, `selen watch list` menampilkan `#1 Comic-auto Hold On · active · menunggu cek · tiap 30 dtk (file)`, dan `selen watch cancel 1` menghentikannya bersih (`status="cancelled"`).

---

## 4. Perbaikan 4 — Kirim Berkas Hasil Kerja ke Chat Telegram (`send_file`)

### Perubahan yang Dibuat
- Menambahkan fitur `multipart` pada `reqwest` di `Cargo.toml` dan implementasi `TelegramClient::send_document` / `TelegramClient::send_photo` di `src/telegram.rs` (mengirim lampiran sebagai pesan tersendiri, caption bersih maks 1 emoji, redaksi token bot pada error).
- Tool `send_file` (`src/tools/send_file.rs`, toolset `tg_files`): memvalidasi berkas WAJIB di dalam sandbox (`cwd` atau direktori data SelenCode), menolak file rahasia (`.env`/`*.key`), menegakkan batas 50 MB Telegram Bot API (berkas > 50 MB mengembalikan error jelas `BATAS_UKURAN_TELEGRAM` yang mengarahkan ke jalur Google Drive), dan menjawab sopan disertai path lokal saat dipanggil di sesi CLI non-gateway.

### Bukti Uji Penerimaan
- Terverifikasi penuh pada `perbaikan4_send_file_sandbox_size_limit_and_cli_fallback`: ZIP & PNG valid diterima (`is_image` tepat), `../di-luar.zip` & `.env` ditolak, berkas melewati batas ukuran memunculkan pesan `BATAS_UKURAN_TELEGRAM` + saran Google Drive, dan sesi CLI menjawab sopan beserta path lokalnya. *(Catatan jujur: unggahan multipart live ke server Telegram Bot API memerlukan bot token & chat ID pemilik saat menjalankan `selen telegram`.)*

---

## 5. Perbaikan 5 — Memory Pengguna Persisten ala Hermes (2 Wadah + Snapshot Beku)

### Perubahan yang Dibuat
- Modul `src/user_memory.rs` dengan 2 wadah di `<data_dir>/selencode/memory/`:
  - `memory.md` (pagu keras default `2.200` char, `[memory] max_memory_chars`)
  - `profile.md` (pagu keras default `1.375` char, `[memory] max_profile_chars`)
- Penulisan yang melampaui pagu ditolak tegas disertai anjuran merangkum; kredensial/API key selalu ditolak.
- Snapshot dibekukan **sekali** di awal sesi (`Session::build` / `/new`) dan disisipkan ke `--- Konteks Sesi (Snapshot Awal) ---` di dalam pagu dinamis `1.500` token yang sudah ada (tidak menaikkan pagu diam-diam).
- Tool `memory` (`add`/`replace`/`remove`/`view`, `src/tools/memory.rs`), perintah `/memory` (menampilkan isi persis yang termuat di prompt + rasio karakter `X/2200` & `Y/1375`), serta penyatuan Background Review `v0.5.1` (`src/agent/review.rs`) agar menulis ke wadah `memory.md` yang sama (hapus/timpa oleh review tetap lewat staging `/review`).

### Bukti Uji Penerimaan Live (`v0.5.7`)
- **Sesi 1 (`p5-mem-sesi1`):** agent memanggil `[tool] memory {"action":"add","content":"Semua arsip hasil unduhan harus selalu disimpan ke folder ~/arsip-hasil-kanonik.","target":"profile"}`.
- **Sesi 2 BARU (`p5-mem-sesi2`, bukan resume):**
  - `/memory` menampilkan tepat yang termuat beku di prompt: `profil · 82/1375 char · - P#1 · profile · Semua arsip hasil unduhan harus selalu disimpan ke folder ~/arsip-hasil-kanonik.`
  - Prompt *"Saya baru selesai mengunduh satu paket arsip. Ke folder mana seharusnya saya menyimpannya?"* langsung dijawab: **`Simpan ke folder ~/arsip-hasil-kanonik.`** dalam 1 panggilan (`3.814` in / `95` out).
- Penolakan melebihi pagu 2.200 char & anti-rahasia terverifikasi di `perbaikan5_persistent_memory_profile_caps_and_frozen_snapshot`.

---

## 6. Perbaikan 6 — Delegate Pekerja di Git Worktree Terisolasi

### Perubahan yang Dibuat
- Memperluas tool `delegate` (`src/tools/delegate.rs`, `src/agent/subagent.rs`, `src/worktree.rs`) dengan parameter opsional `mode: "read" | "work"` (default `"read"` — perilaku baca-saja Fase 6 tidak berubah sama sekali).
- Pada `mode: "work"`: menolak folder non-Git dengan jelas; membuat Git worktree terisolasi per tugas (`.selencode/worktrees/<id>`); menjalankan subagent pekerja dengan `worker_registry` terkunci ke worktree; menampilkan ringkasan + **diff lengkap** (termasuk berkas baru untracked); meminta persetujuan eksplisit lewat `ctx.approver.ask(...)` sebelum `apply_worktree_to_main` (membuka `/rollback` checkpoint + deteksi konflik file kotor); mendukung pekerja paralel hingga `max_parallel_subagents`; mencatat token subagent di `usage` dengan `kind = 'delegate_work'`.

### Bukti Uji Penerimaan
- **Uji live biner `v0.5.7` (`p6-delegate-work`):** agent memanggil `delegate(mode="work")` untuk membuat `hello.js` → pekerja menulis & memverifikasi di worktree → `[auto-approve] delegate_work apply ... (1 ubah, 0 hapus)` → mendarat di pohon utama (`HELLO_JS_EXISTS=True`, `HELLO_JS_OUT=Halo Worktree`, `checkpoint #2`), tercatat di `usage` dengan **`kind="delegate_work"` (`12.476` in / `335` out)** terpisah dari `kind="main"`.
- **Tes otomatis (`perbaikan6_delegate_work_isolated_worktree_approval_parallel_and_nongit_reject`):** folder non-Git ditolak jelas; saat approval ditolak (`Approver::deny_all`), pohon utama tetap 100% utuh sementara diff tetap ditampilkan; 2 pekerja paralel (`fitur_a.txt` & `fitur_b.txt`) berjalan tanpa tabrakan dan keduanya mendarat bersih sesudah disetujui.

---

## 7. Perbaikan 7 — Perintah `/usage` (Telegram + CLI/TUI, 0 Panggilan API)

### Perubahan yang Dibuat
- Menambahkan query `Store::usage_report(session_id, today_utc_prefix)` di `src/store.rs` yang mengelompokkan pemakaian sesi aktif (termasuk `review:<session_id>`) dan total hari ini per `(model, kind)`.
- Menambahkan perintah `/usage` pada `src/repl.rs`, `src/app/commands.rs`, `src/app/plain_runner.rs`, `src/app/tui_runner.rs`, `src/app/telegram_runner.rs`, `src/tui/mod.rs`, dan `TELEGRAM_BOT_COMMANDS` (format bersih maks 1 emoji `📊`, 0 panggilan provider).

### Bukti Uji Penerimaan Live (`v0.5.7`) — Kecocokan 1:1 terhadap Query DB `usage`
- **Output live `/usage` pada akhir sesi `p1-skenario-e`:**
  ```text
  === 📊 Laporan Pemakaian Token (/usage) ===
    * sesi aktif  ·  5 panggilan  ·  19393 in / 636 out
    * hari ini (2026-11-26)  ·  8 panggilan  ·  33995 in / 1621 out
    - model terpakai  ·  ag/gemini-4-neon-medium  ·  langsung dari tabel `usage` (0 panggilan API)
  ```
- **Query langsung tabel SQLite `usage` pada saat yang sama:**
  - Sesi `p1-skenario-e`: `3.634 + 3.798 + 3.621 + 3.979 + 4.361 = 19.393` in (`5` panggilan), `131 + 69 + 124 + 95 + 217 = 636` out → **COCOK PERSIS**.
  - Hari ini (`p1-other-session` `14.602 / 985` + `p1-skenario-e` `19.393 / 636`): `8` panggilan, `33.995` in / `1.621` out → **COCOK PERSIS**.
- Pemisahan baris `main`, `fallback`, `review`, `cron`, dan `delegate_work` terverifikasi persis di `perbaikan7_usage_report_exact_match_and_separate_kinds`.

---

## 8. Perbaikan 8 — Pasang & Copot Skill dari URL/Git (`selen skills install|remove`)

### Perubahan yang Dibuat
- Modul `src/skill_installer.rs` + subcommand CLI `selen skills install <url> [--rename <nama>]`, `selen skills remove <nama>`, `selen skills list` (`src/app/cli.rs`, `src/main.rs`) dan perintah chat `/skills install|remove` (`src/app/commands.rs`).
- Alur aman: unduh/shallow clone ke area staging sementara → validasi frontmatter YAML & tolak symlink/path traversal/file rahasia/biner → tampilkan ringkasan persetujuan dengan penandaan jelas setiap berkas skrip (`[SKRIP — tidak dijalankan otomatis]`) → tolak bentrok nama tanpa menimpa diam-diam (mendukung `--rename`) → catat asal-usul (URL + tanggal) di `.selen-source.json` → `remove` hanya berlaku untuk skill hasil install (menolak bawaan & skill proyek).

### Bukti Uji Penerimaan Live (`v0.5.7`)
```text
Pasang skill 'cek-demo-v057' dari .../p8-remote-skill
Deskripsi: Skill demo uji pasang URL Git v0.5.7
Total berkas: 2 (skrip: 1)
  - SKILL.md (173 byte)
  - scripts/check.sh (27 byte) [SKRIP — tidak dijalankan otomatis]
Peringatan: paket memuat berkas skrip. Skrip TIDAK pernah dijalankan otomatis oleh SelenCode.
Skill 'cek-demo-v057' berhasil dipasang ke .../skills/installed/cek-demo-v057 (2 berkas, 1 skrip; sumber dicatat di .../.selen-source.json).
FOUND_IN_LIST:   - cek-demo-v057  ·  user · installed · v1.0.0  ·  Skill demo uji pasang URL Git v0.5.7
Skill 'cek-demo-v057' berhasil dicopot dari .../skills/installed/cek-demo-v057
```
- Penolakan frontmatter rusak, bentrok nama + `--rename`, serta blokir pencopotan skill bawaan & proyek terverifikasi di `perbaikan8_skills_install_validation_scripts_collision_and_remove`.

---

## 9. Regresi Gabungan A/B/E Lengkap (Tabel `usage`, `v0.5.7`)

| Skenario | Calls (`usage`) | Skill Dimuat | Input Token per Panggilan (`usage`) | Rata-rata In/Call | Total In / Out | Status |
| :--- | :---: | :---: | :--- | :---: | :---: | :--- |
| **Proyek A (`rekap.js`)** | `6 / 6` | `0` | `3.592 → 3.846 → 4.249 → 4.333 → 4.504 → 4.664` | **`4.198`** | `25.188 / 847` | **LULUS** |
| **Proyek B (`hitung.js`)** | `7 / 7` | `0` | `3.592 → 3.795 → 4.410 → 4.695 → 4.921 → 5.300 → 5.395` | **`4.587`** | `32.108 / 1.654` | **LULUS** |
| **Gabungan A + B (`v0.5.7`)** | `13 / 13` | `0` | 13 panggilan (`57.296` total in — **turun -379/call vs v0.5.6**) | **`4.407`** | `57.296 / 2.501` | **LULUS** (≤ 5.000) |
| **Proyek E — Giliran Resume (`kata.txt`)** | `1` (resume #1) | `0` | Turn 1: `3.634 → 3.798` · **Resume #1:** **`3.621`** | **`3.621`** | `3.621 / 124` | **LULUS** (≤ 7.500) |

- **Jalur tambahan terpisah (`usage.kind`):** `delegate_work` (`12.476` in / `335` out) & loop `watch_job` (**`0` token**) tercatat terpisah dan tidak mengotori angka jalur utama `kind = 'main'`.

---

## 10. Yang Sengaja Tidak Diubah
Seluruh hasil `v0.5.0`–`v0.5.6` di luar delapan bagian ini: pengiriman & tampilan Telegram, PATH sesi, perilaku menulis skill, Background Self-Improvement Review + staging `/review`, fallback provider/model, `ask_user`, cron, `session_search` FTS5, pelacakan & timeout approval Telegram, katalog 14 skill bawaan (12 adaptasi MIT + 2 asli) + `NOTICE`, `[skills] auto_load` tetap opt-in, sistem hemat token `v0.3.2`–`v0.4.0`, Plan mode, sesi/checkpoint, `delegate` mode baca-saja, tema Moonlit TUI/CLI, `selen update`, trait `Provider`/`Tool`, redaksi rahasia, sandbox, serta default provider `9Router` (`ag/gemini-3-flash`).
