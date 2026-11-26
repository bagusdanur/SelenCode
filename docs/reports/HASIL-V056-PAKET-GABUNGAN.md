# Hasil Update SelenCode v0.5.6 — Paket Gabungan (Bagian 1–5)

- **Versi:** `v0.5.6` (satu bump langsung dari `v0.5.1`, tanpa rilis perantara `v0.5.2`–`v0.5.5`)
- **Rilis Publik:** `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.6` (`draft=false`, 3 aset biner: `selen-linux-x64` 15.67 MB, `selen-termux-arm64` 14.13 MB, `selen-windows-x64.exe` 14.45 MB)
- **Commit Rilis:** `ba995f7` (CI Linux & Windows x64 100% hijau — workflow CI `45728910001` & Release `45728923774`)
- **Rilis Pendahulu (Fase 1):** `v0.5.1` (`https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.1`, laporan terpisah di `docs/reports/HASIL-V051-BACKGROUND-REVIEW.md`)
- **Tanggal:** 26 November 2026

---

## 1. Ringkasan Perubahan per Bagian

### Bagian 1 — Fallback Provider / Model Otomatis
- **Konfigurasi:** menambah `ProviderCfg::fallback: Vec<String>` (urutan model/provider cadangan, default kosong) dan `[fallback]` (`max_models`, default `3`) di `src/config.rs`. Format entri mendukung `"nama-model"` (provider sama) maupun `"provider:model"` (lintas provider).
- **Klasifikasi error ketat (`is_fallback_eligible_error` di `src/providers/router.rs`):**
  - **LAYAK fallback:** HTTP `429` (kuota/rate limit), HTTP `5xx` provider, timeout/koneksi putus, penolakan kredensial/kuota upstream (`401`/`403`, `insufficient_quota`, `no active credentials`), dan model mati/tidak ditemukan (`404`, `model_not_found`).
  - **TIDAK PERNAH fallback:** HTTP `400` / bad request / format rusak / `context_length_exceeded`, error tool lokal, penolakan approval, pembatalan pengguna, serta error tak dikenal.
  - **Temuan & perbaikan uji live (`ba995f7`):** saat uji live pertama pada 9Router, gateway mengembalikan HTTP `404 Not Found` dengan JSON `{"type":"invalid_request_error","code":"model_not_found","message":"No active credentials for provider: ..."}`. Urutan pengecekan di `is_fallback_eligible_error` diperbaiki agar status `404`/`model_not_found`/`no active credentials` dievaluasi sebelum frasa generik `invalid_request`, sementara `400 Bad Request` tetap ditolak tegas.
- **Perilaku non-lengket & observabilitas:**
  - `non_sticky_manual = true`: sesi/giliran baru selalu mencoba model utama kembali.
  - Notifikasi transparan tepat **1 baris** (`Model utama gagal (<alasan>) — lanjut dengan <model>`), di Telegram dikirim sebagai pesan baru.
  - Pencatatan `usage` merekam **model yang benar-benar melayani** dengan `kind = 'fallback'` terpisah dari `kind = 'main'`.
- **File terkait:** `src/config.rs`, `src/providers/router.rs`, `src/app/mod.rs`, `src/agent/loop.rs`, `src/app/telegram_runner.rs`, `src/store.rs`.

### Bagian 2 — Tool `ask_user` / Clarify
- **Tool baru (`src/tools/ask.rs`):** `ask_user` menerima `question`, `choices` opsional (`0–4` pilihan singkat; selalu mendukung jawaban bebas), dan `context` opsional.
- **Jalur interaksi terpisah dari approval:**
  - **CLI plain (`src/app/plain_runner.rs`):** prompt terpisah dengan pilihan bernomor + teks bebas.
  - **TUI Moonlit (`src/tui/app.rs`, `src/tui/mod.rs`, `src/app/tui_runner.rs`):** modal `PendingAsk` terpisah dari kartu approval tool.
  - **Telegram (`src/telegram.rs`, `src/app/telegram_runner.rs`):** `TgAskHub` dengan callback prefix khusus `ak:` (terpisah total dari `ap:` approval & `rv:` review), tombol pilihan + dukungan balasan teks bebas.
  - **Non-interaktif (`selen run`):** langsung mengembalikan penanda `[NON-INTERACTIVE]` tanpa menggantung, menginstruksikan agen melanjutkan dengan asumsi eksplisit terbaik dan melaporkannya ke pengguna.
- **Timeout, anti-rahasia, anti-spinning, & toolset ramping:**
  - `[ask] timeout_secs` (default `600` detik).
  - Menolak pertanyaan yang meminta API key / password / token rahasia.
  - Guardrail anti-spinning di `src/agent/loop.rs`: setelah >5 panggilan pencarian beruntun tanpa kejelasan, agen diingatkan untuk berhenti berputar dan bertanya singkat lewat `ask_user`.
  - Toolset `clarify` di `src/tools/toolsets.rs` hanya dimuat secara selektif (hemat token pada tugas kode langsung).

### Bagian 3 — Cron / Tugas Terjadwal di Telegram Gateway
- **Mesin jadwal & keamanan (`src/cron.rs`, `src/store.rs`):**
  - Parser jadwal sederhana & ramah manusia: interval `every Nm`/`Nm` (menit), `every Nh`/`Nh` (jam), atau harian `daily HH:MM`/`HH:MM` (menolak ekspresi cron 5-bidang rumit).
  - Persistensi SQLite pada tabel `cron_jobs`; saat restart gateway, job yang terlewat dilewati aman (`skip_missed_jobs_on_startup`, tidak menumpuk backlog).
  - **Pagar keamanan default:** mode **baca-saja** (`allow_write = false`, `shell_allowlist = []`, `Approver::deny_all`), hanya bisa ditingkatkan secara eksplisit per-job saat pembuatan (daftar prefix shell allowlist dan/atau izin tulis).
  - Eksekusi serial di dalam loop gateway Telegram (`src/app/telegram_runner.rs`) agar tidak bentrok dengan chat aktif; hasil dikirim sebagai pesan baru ke chat pemilik; `usage` dicatat terpisah dengan `kind = 'cron'`.
- **Antarmuka CLI & Telegram:** subcommand `selen cron <add|list|pause|resume|remove>` (`src/app/cli.rs`, `src/app/commands.rs`) dan perintah `/cron` di CLI/TUI/Telegram. Selalu menampilkan catatan jujur bahwa job terjadwal hanya berjalan selama proses `selen telegram` hidup.

### Bagian 4A — Pencarian Lintas Sesi FTS5 (`session_search` & `/search`)
- **Indeks FTS5 (`src/store.rs`):** tabel bayangan `messages_fts` (`fts5`, `unicode61`) di atas pesan teredaksi di `messages`, dengan trigger sinkronisasi otomatis + backfill latar satu kali untuk sesi lama (`src/app/session.rs`).
- **Tool & perintah pencarian:**
  - Tool `session_search` (`src/tools/session_search.rs`, toolset `history` yang dimuat selektif): dibatasi maks **10 hasil** dan cuplikan maks **180 karakter** agar hemat konteks.
  - Perintah `/search <kueri>` di CLI/TUI/Telegram (`src/app/commands.rs`, `src/app/telegram_runner.rs`, `src/repl.rs`) serta CLI `selen search <kueri>` (`src/app/cli.rs`).

### Bagian 4B — Pelacakan Pengiriman & Timeout Approval Telegram
- **Pelacakan pengiriman (`src/telegram.rs`):** `TgApprovalHub` melacak status pengiriman kartu approval (`Sent` / `Failed`), melakukan **1x retry** bila pengiriman pertama gagal, lalu menolak aksi secara aman (`deny`) disertai notifikasi kegagalan ke pengguna (tidak pernah menggantung tanpa ujung).
- **Timeout approval terkonfigurasi:** `[telegram] approval_timeout_mins` di `src/config.rs` (default `10` menit; `0` = tanpa batas). Saat kedaluwarsa, kartu diedit menjadi `"Kedaluwarsa — aksi dibatalkan"`, ID dicatat pada `expired_ids`, dan klik tombol yang terlambat dibalas `"sudah kedaluwarsa"`.

### Bagian 5 — Katalog 14 Skill Bawaan Resmi + Atribusi MIT
- **Embedding & penyemaian otomatis (`src/builtin_skills.rs`, `src/skills.rs`, `assets/builtin_skills/`):** 42 file aset (12 skill adaptasi MIT + referensi/template/skrip + `NOTICE`) disematkan langsung ke biner lewat `include_str!` dan disemai otomatis bersama 2 skill asli SelenCode pada instalasi bersih.
- **0 kebocoran nama tool pihak lain:** seluruh instruksi skill diadaptasi ke tool native SelenCode (`run_shell`, `read_file`, `edit_file`, `write_file`, `search_files`, `grep_text`, `delegate`, `ask_user`, `skill_view`, `manage_memory`, dll.).
- **Pagu indeks ketat:** `format_skills_index_budgeted` menjaga pagu indeks gabungan dengan urutan pangkas **`bawaan -> user -> proyek`**. Ukuran terukur indeks 14 skill bawaan = **`370` token** (memenuhi batas ≤ 400 token).

---

## 2. Bukti Uji Penerimaan per Bagian

### A. Suite Tes Otomatis (`tests/v056_combined_package.rs` — CI Linux & Windows x64 lulus 100%)
Seluruh 31 suite cargo test di CI lulus pada commit `ba995f7`, termasuk 6 skenario khusus di `tests/v056_combined_package.rs`:
1. `bagian1_fallback_dead_model_400_no_fallback_and_non_sticky`: klasifikasi ketat 429/5xx/401/403/404 (termasuk format 9Router `404 Not Found` + `invalid_request_error`/`model_not_found`), tolak `400 Bad Request` & `invalid_request_error` murni, fallback ke model cadangan + tepat 1 notifikasi + `usage` atas nama model cadangan (`kind='fallback'`), serta non-sticky pada giliran berikutnya.
2. `bagian2_ask_user_choices_non_interactive_anti_secret_and_lean_toolset`: pilihan kandidat, non-interaktif tidak menggantung, penolakan permintaan secret, timeout, dan toolset `clarify` hanya aktif saat relevan.
3. `bagian3_cron_parser_persistence_skip_missed_and_readonly_guard`: parser `every Nm/Nh` & `daily HH:MM`, penolakan 5-bidang, persistensi SQLite, `skip_missed_jobs_on_startup`, pagar `read-only` & shell allowlist, eksekusi nyata `run_cron_job_once`, serta `usage` `kind='cron'`.
4. `bagian4a_fts5_cross_session_search_backfill_and_caps`: backfill sesi lama ke `messages_fts`, `session_search` mengembalikan sesi yang tepat, pagu ≤ 10 hasil & cuplikan ≤ 180 karakter.
5. `bagian4b_telegram_approval_expiry_and_ask_callback_isolation`: `expired_ids`, balasan `"sudah kedaluwarsa"`, pemisahan callback `ak:` vs `ap:`, serta mekanisme retry-once & safe-deny.
6. `bagian5_14_builtin_skills_notice_no_hermes_tools_and_budget`: penyemaian 14 skill + `NOTICE`, 0 sisa nama tool Hermes, indeks bawaan `370` token (≤ 400), serta urutan pangkas `bawaan -> user -> proyek`.

### B. Bukti Uji Live pada Biner Rilis Publik `v0.5.6` (`selen-windows-x64.exe`, 9Router `ag/gemini-4-neon-medium`)

#### 1) Bagian 1 — Fallback Aktif (Model Utama Mati `404` → Cadangan Hidup) & Non-Lengket
- **Konfigurasi Uji 1:** `model = "nonexistent/dead-model-v056-test"`, `fallback = ["ag/gemini-4-neon-medium"]`.
- **Output nyata:**
  ```text
  Hasil 17 dikali 3 adalah 51.
  [selen] Model utama gagal (404 model tidak tersedia) — lanjut dengan ag/gemini-4-neon-medium
  EXIT1=0
  ```
- **Baris SQLite `usage` Uji 1 (model aktual + `kind` terpisah):**
  ```json
  {"folder":"b1-dead-primary","session_model":"nonexistent/dead-model-v056-test","usage":[{"model":"ag/gemini-4-neon-medium","input_tokens":3982,"output_tokens":56,"kind":"fallback"}]}
  ```
- **Uji 2 (Non-lengket — sesi baru dengan primary sehat `ag/gemini-4-neon-medium`):** langsung dijawab tanpa notifikasi fallback (`EXIT2=0`), baris `usage` tercatat `kind="main"` (`3.981` in / `54` out).

#### 2) Bagian 2 — Tool `ask_user` / Clarify (Mode Non-Interaktif `selen run`)
- **Skenario:** `catalog.txt` berisi 3 judul manga mirip (`Rebirth of Legendary Doctor (MangaDex)`, `Legendary Doctor Returns (ComicK)`, `Modern Doctor Chronicles`); prompt meminta mencari judul persis `'The Rise of a Legendary Doctor'`.
- **Pemanggilan tool & output nyata:**
  ```text
  [tool] read_file {"path":"catalog.txt"}
  [tool] ask_user {"choices":["Rebirth of Legendary Doctor (MangaDex)","Legendary Doctor Returns (ComicK)","Modern Doctor Chronicles"],"context":"Judul 'The Rise of a Legendary Doctor' tidak ditemukan persis di catalog.txt...","question":"Judul mana yang ingin Anda tulis ke target.txt?"}
  [tool] write_file {"content":"Rebirth of Legendary Doctor (MangaDex)\n","path":"target.txt"}
  ```
  > *"Karena sesi berjalan dalam mode non-interaktif (`ask_user` tidak dapat menerima input pengguna), saya melanjutkan dengan **asumsi terbaik** memilih kandidat terdekat secara frasa dan makna, yaitu **`Rebirth of Legendary Doctor (MangaDex)`**, dan telah menuliskannya ke `target.txt`."*
- **Metrik:** `4/4` calls, `avg_tools = 9`, `used_ask_user = true`, tidak menggantung.

#### 3) Bagian 3 — Cron / Tugas Terjadwal (`selen cron` CLI + Tes Eksekusi Otomatis)
- **Output nyata `selen cron` CLI (`add`, `list`, `pause`, `resume`, `remove`):**
  ```text
  Job terjadwal #1 'uji-cron-v056' ditambahkan (setiap 2 menit). Jadwal berikut: 2026-11-26T00:24:29Z
  Catatan: job terjadwal hanya berjalan selama proses gateway Telegram (`selen telegram`) hidup.

  === Daftar Job Terjadwal (1) ===
    * #1 uji-cron-v056  ·  setiap 2 menit · aktif · izin: baca-saja  ·  terakhir: belum pernah
  ----------------------------------------
    * Catatan: job terjadwal hanya berjalan selama proses gateway Telegram (`selen telegram`) hidup.

  Job 'uji-cron-v056' dijeda.
  Job 'uji-cron-v056' diaktifkan kembali (berikut: 2026-11-26T00:24:29Z).
  Job 'uji-cron-v056' berhasil dihapus.
  ```
- **Eksekusi job & pencatatan `kind='cron'`:** diverifikasi end-to-end pada `bagian3_cron_parser_persistence_skip_missed_and_readonly_guard` (menguji `run_cron_job_once`, penolakan tulis di mode baca-saja, allowlist shell, skip-missed pasca restart, dan baris `usage` `kind='cron'`). *(Catatan jujur: pengiriman laporan cron ke Telegram Bot API live tidak dijalankan di mesin uji karena memerlukan token bot & chat ID pemilik.)*

#### 4) Bagian 4A — Pencarian Lintas Sesi FTS5 (`selen search` + Tool `session_search`)
- **Skenario:** menanam frasa `KODE-UNIK-7391` pada sesi pertama (`7bedf7cb-65f4-4c54-9824-0bb902015400`), lalu mencarinya dari sesi baru dan CLI.
- **Output nyata `selen search KODE-UNIK-7391`:**
  ```text
  === Hasil Pencarian Sesi (1 cocok untuk "KODE-UNIK-7391") ===
    * #1  ·  Catat bahwa kode audit proyek hari ini adalah… (2026-11-26T00:22:30)  ·  id: 7bedf7cb-6
    > cuplikan | Tercatat. Kode audit proyek hari ini adalah **KODE-UNIK-7391**. |
  ```
- **Output nyata tool `session_search` dari sesi baru (`9b910cd9-...`):**
  ```text
  [tool] session_search {"query":"KODE-UNIK-7391"}
  Kode audit KODE-UNIK-7391 ditemukan pada riwayat sesi sebelumnya:
  - ID Sesi: 7bedf7cb-65f4-4c54-9824-0bb902015400
  - Cuplikan: Tercatat. Kode audit proyek hari ini adalah **KODE-UNIK-7391**.
  ```
  (`2/2` calls, `avg_in = 4.447`, `used_session_search = true`.)

#### 5) Bagian 4B — Pelacakan & Timeout Approval Telegram
- Diverifikasi penuh oleh `bagian4b_telegram_approval_expiry_and_ask_callback_isolation` di CI Linux & Windows x64: pengiriman kartu melacak status `Sent`/`Failed` dengan retry 1x + tolak aman jika gagal, kartu kedaluwarsa diubah menjadi `"Kedaluwarsa — aksi dibatalkan"`, dan klik tombol terlambat dibalas `"sudah kedaluwarsa"`. *(Catatan jujur: uji interaksi tombol live Telegram Bot API dilakukan oleh pemilik dengan bot token aktif masing-masing.)*

#### 6) Bagian 5 — Daftar 14 Skill Bawaan, `NOTICE`, Pagu Indeks, & Uji Live Tanpa Menyebut Nama Skill
- **Verifikasi instalasi bersih:** `CLEAN_INSTALL_SEEDED_SKILLS=14`, `NOTICE_EXISTS=True`, ukuran indeks 14 skill bawaan = **`370` token** (≤ 400 token).
- **Atribusi MIT (`assets/builtin_skills/NOTICE`, ikut disemai ke direktori skills):** mencantumkan lisensi MIT penuh `Copyright (c) 2025 Nous Research` untuk 12 skill adaptasi dari `https://github.com/NousResearch/hermes-agent` beserta kredit penulis hulu masing-masing.
- **Daftar lengkap 14 skill bawaan resmi:**

| # | Kategori / Nama | Asal | Ringkasan Peran |
| :-: | :--- | :--- | :--- |
| 1 | `dev/rilis-github` | Asli SelenCode | Prosedur rilis tag & GitHub Actions SelenCode |
| 2 | `dev/tinjau-rust` | Asli SelenCode | Checklist peninjauan kode Rust |
| 3 | `dev/systematic-debugging` | Adaptasi MIT (`obra/superpowers`, Hermes Agent) | Investigasi akar masalah 4-fase sebelum menambal kode |
| 4 | `dev/test-driven-development` | Adaptasi MIT (`obra/superpowers`, Hermes Agent) | Siklus Red-Green-Refactor (tes gagal dulu sebelum kode) |
| 5 | `dev/requesting-code-review` | Adaptasi MIT (`obra/superpowers` + `MorAlekss`, Hermes Agent) | Peninjauan diff terstruktur sebelum merge/rilis |
| 6 | `dev/simplify-code` | Adaptasi MIT (Hermes Agent, Nous Research) | Penyederhanaan kode tanpa mengubah perilaku |
| 7 | `dev/github` | Adaptasi MIT (`Ben Barclay`, Hermes Agent) | Alur kerja `gh` CLI (PR, issue, CI, commit konvensional) |
| 8 | `dev/spike` | Adaptasi MIT (Hermes Agent, Nous Research) | Eksperimen prototipe terukur sebelum implementasi penuh |
| 9 | `dev/ast-grep` | Adaptasi MIT (`Yeongyu Kim`, Hermes Agent) | Pencarian & refactor struktural berbasis AST |
| 10 | `dev/grill-me` | Adaptasi MIT (`Rafael Zendron` + `Matt Pocock`, Hermes Agent) | Wawancara desain mendalam untuk menguji asumsi rencana |
| 11 | `dev/codebase-inspection` | Adaptasi MIT (Hermes Agent, Nous Research) | Analisis cepat ukuran repo, bahasa, dan LOC (`pygount`/fallback) |
| 12 | `devops/docker-management` | Adaptasi MIT (`sprmn24`, Hermes Agent) | Manajemen & troubleshooting container/compose Docker |
| 13 | `riset/grounded-citations` | Adaptasi MIT (Hermes Agent, Nous Research) | Riset dengan sitasi terverifikasi & pelacakan sumber |
| 14 | `web/blocked-page-recovery` | Adaptasi MIT (Hermes Agent, Nous Research) | Pemulihan akses halaman web terblokir/terbatas secara sah |

- **Uji Live 5A — `systematic-debugging` (TANPA menyebut nama skill di prompt):**
  - Prompt hanya meminta mencari penyebab `node pricing.js` mencetak `VIP total: 243000` (seharusnya `270000`).
  - Agen otomatis memanggil `[tool] skill_view {"name":"systematic-debugging"}` (`used_systematic_debugging = true`), mereproduksi bug, melacak diskon VIP ganda (`calcItemPrice` dan `calcCartTotal` sama-sama mengali `0.9`), memperbaiki `pricing.js`, lalu memverifikasi `node --check pricing.js && node pricing.js` → `VIP total: 270000`.
- **Uji Live 5B — `codebase-inspection` (TANPA menyebut nama skill di prompt):**
  - Prompt hanya meminta inspeksi ukuran repositori kecil (3 file).
  - Agen otomatis memanggil `[tool] skill_view {"name":"codebase-inspection"}` (`used_codebase_inspection = true`), mencoba `pygount --format=summary --folders-to-skip=...` sesuai instruksi skill, lalu jatuh kembali secara bersih ke perhitungan langsung: 3 file, 2 bahasa, 21 baris fisik, 16 LOC, 4 komentar, 1 baris kosong (`3/3` calls).

---

## 3. Regresi Gabungan Jalur Utama A/B/E (Tabel `usage`, `v0.5.6` dengan 14 Skill Bawaan Aktif)

| Skenario | Calls (`usage` / `metrics`) | Avg Tools | Input Token per Panggilan (`usage`) | Rata-rata In/Call | Total In / Out | Status |
| :--- | :---: | :---: | :--- | :---: | :---: | :--- |
| **Proyek A (`rekap.js`)** | `6 / 6` | `8` | `3.997 → 4.214 → 4.860 → 4.944 → 5.106 → 5.266` | `4.731` | `28.387 / 1.090` | **LULUS** |
| **Proyek B (`hitung.js`)** | `8 / 8` | `8.1` | `4.010 → 4.118 → 4.649 → 4.803 → 4.981 → 5.067 → 5.324 → 5.671` | `4.828` | `38.623 / 1.404` | **LULUS** |
| **Gabungan A + B (jalur utama)** | `14 / 14` | `8.1` | 14 panggilan (`67.010` total in, 14 skill bawaan aktif) | **`4.786`** | `67.010 / 2.494` | **LULUS** (≤ 5.000) |
| **Proyek E + Resume (`kata.txt`)** | `5 / 5` | `8` | Turn 1: `3.979 → 4.221` · **Resume Turn 2:** `4.288 → 4.429 → 4.532` | **`4.290`** | `21.449 / 503` | **LULUS** (≤ 7.500) |

### Jalur Tambahan (Terpisah di Kolom `usage.kind`, Tidak Mencemari Jalur Utama)
- **Fallback (`kind = 'fallback'`):** `1` panggilan pada uji live B1 (`ag/gemini-4-neon-medium`, `3.982` in / `56` out).
- **Cron (`kind = 'cron'`):** terisolasi di kolom `kind = 'cron'` pada `Store::record_usage_with_kind` (diverifikasi di `bagian3_cron_parser_persistence_skip_missed_and_readonly_guard`).

---

## 4. Yang Sengaja Tidak Diubah
Seluruh hasil hemat token `v0.3.2`–`v0.4.0`, inti Skills System Tahap 1–6 di luar tambahan katalog & pagu Bagian 5, paket `v0.5.0` (pengiriman & tampilan Telegram, PATH sesi, perilaku menulis skill), paket `v0.5.1` (Background Self-Improvement Review + staging `/review`), Plan mode, sesi/checkpoint & `/rollback`, `delegate`, tema Moonlit TUI/CLI, `selen update`, trait `Provider`/`Tool`, redaksi rahasia, sandbox, serta default provider `9Router` (`ag/gemini-3-flash`) dengan daftar `fallback` default kosong.
