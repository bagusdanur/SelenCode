# Laporan Hasil Update SelenCode v0.5.9 — Tiga Utang dari Uji Penerimaan v0.5.7

- **Versi rilis:** `v0.5.9`
- **Commit implementasi:**
  - `b07c6f0` (`feat(v0.5.9): bound skill_view context cost, live CLI/TUI watch polling, unified persistent memory store + migration`)
  - `2721f76` (`perf(v0.5.9): tighten skill_view working copy (1500 char) & prune after 1 step`)
  - `c318b1d` (`fix(v0.5.9): keep up to 8 markdown headings in archived skill stub`)
- **Rilis publik GitHub:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.9
- **CI (`c318b1d`):** Run `#45805680818` — **100% HIJAU** (`Cargo Test & Visual Render (Linux)` + `Cargo Test & Check (Windows x64)`, seluruh 34 suite tes termasuk `v059_three_debts` + Clippy lulus).
- **Release workflow (`v0.5.9`):** Run `#45805695737` — **100% HIJAU**, 3 biner publik terbit:
  - `selen-linux-x64`: `16.855.152` byte (16,07 MB)
  - `selen-termux-arm64`: `15.156.288` byte (14,45 MB)
  - `selen-windows-x64.exe`: `15.557.120` byte (14,84 MB)
- **Catatan migrasi (changelog 1 baris):** Entri memori lama di tabel SQLite `memories` (`manage_memory`) dimigrasikan otomatis satu kali saat startup ke berkas persisten `memory.md` (menghormati pagu 2.200 karakter tanpa memotong diam-diam; entri berlebih ditahan dengan peringatan minta rangkum).

---

## Ringkasan Perubahan Per Komponen

| Area | Berkas Utama | Ringkasan |
|---|---|---|
| **Perbaikan 1 — Biaya Memuat Skill Terkendali** | `src/tools/skill.rs`, `src/agent/context.rs`, `src/agent/loop.rs` | `skill_view` untuk `SKILL.md` besar (> 4.500 char, mis. `systematic-debugging` 14,6 rb char) mengembalikan **salinan kerja prosedural ringkas** (target 1.500 char / ~450 token; menjaga Iron Law + Phase 1–4 + daftar bagian + opsi `full=true`). Di `loop.rs`, `prune_old_skill_results_with_archive(..., keep_recent_msgs = 2)` mengarsipkan hasil `skill_view` secara **byte-exact** ke `pruned/<sid>/*skill_*.txt` tepat sesudah 1 langkah berikutnya dan menggantinya dengan stub ~250 char berisi nama skill, daftar hingga 8 judul bagian, serta petunjuk muat ulang `skill_view(name, full=true)`. |
| **Perbaikan 2 — Pantauan `watch_job` Hidup di CLI Murni** | `src/watch.rs`, `src/store.rs`, `src/app/session.rs`, `src/app/plain_runner.rs`, `src/app/tui_runner.rs`, `src/main.rs`, `src/app/telegram_runner.rs`, `src/tools/watch.rs` | Satu implementasi polling bersama (`evaluate_and_poll_watch` + `persist_watch_transition` + `poll_active_watches_for_cli`) dipakai sekaligus oleh gateway Telegram dan pembukaan/penutupan bersih sesi CLI/TUI (`selen`, `selen --plain`, `selen run`). Status `last_stage`/`last_progress` & `delivered` disimpan persisten di SQLite `job_watches` sehingga setiap perubahan/selesai/gagal dilaporkan **tepat satu kali** (nol duplikat meski CLI ↔ gateway aktif bergantian; nol baris saat tidak ada perubahan; 0 token provider). |
| **Perbaikan 3 — Satu Store Memory Persisten + Migrasi Lama** | `src/tools/memory.rs`, `src/tools/mod.rs`, `src/tools/toolsets.rs`, `src/user_memory.rs`, `src/store.rs`, `src/app/session.rs`, `src/app/commands.rs`, `src/agent/context.rs`, `src/agent/loop.rs`, `src/agent/review.rs`, `src/doctor.rs` | `manage_memory` dipensiunkan total dari `default_registry` dan toolset `skills_mem` — hanya ada **satu** tool memori lintas-sesi aktif (`memory`). System prompt & aktivasi toolset diarahkan hanya ke `memory`. `/memory` membaca langsung berkas `profile.md` & `memory.md` sehingga tampilan `/memory` selalu 100% sama dengan berkas store. `review.rs` & `loop.rs` tidak lagi menulis/menginjeksi tabel SQLite lama. `migrate_legacy_sqlite_memories` otomatis memindahkan entri lama saat startup sambil menegakkan pagu keras tanpa memotong diam-diam. |
| **Tes otomatis baru** | `tests/v059_three_debts.rs` (plus pembaruan `tests/stage2_context_memory.rs` & `tests/v051_background_review.rs`) | Menutup seluruh syarat penerimaan Perbaikan 1, 2, dan 3 secara otomatis pada CI Linux & Windows x64. |

---

## Bukti Uji Penerimaan Live (Biner Rilis `selen 0.5.9`)

### Perbaikan 1 — Biaya Memuat Skill (`skill_view` Working Copy + Prune/Arsip Dini)

#### 1A. Dua Skenario Debugging v0.5.7 Diulang Persis + Perbandingan Dengan vs Tanpa Skill (`c318b1d`)

| Skenario (Tabel `usage` SQLite) | Skill Dimuat Sebelum Edit? | Repro Sebelum Edit? | Hasil Akhir | Panggilan API | Rata-rata Input/Call | Total Input | Delta per-Call vs Tanpa Skill | Delta 7-Call Setara vs Tanpa Skill |
|---|---|---|---:|---:|---:|---:|---:|---:|
| **B-DBG1 Tanpa Skill** (`b-dbg1-no-skill`, baseline rilis ini) | Tidak | — | `31500` | 7 | **4.563** | **31.943** | baseline | baseline |
| **B-DBG1 Dengan Skill** (`b-dbg1-with-skill`, diskon 10% dari 35000) | **Ya** (`systematic-debugging`) | **Ya** (`node hitung_total.js` → `34990`) | **`31500`** | 8 *(+1 call repro)* | **5.237** | **41.895** *(7 call pertama: 35.856)* | **+14,8%** *(≤ +15%)* | **+12,2%** *(≤ +15%)* |
| **B-DBG2 Varian Pajak** (`b-dbg2-pajak-skill`, 35000 − 10% + pajak 11%) | **Ya** (`systematic-debugging`) | **Ya** (`node pajak.js` → `35350`) | **`34965`** | 8 *(+1 call repro)* | **5.122** | **40.974** *(7 call pertama: 35.028)* | **+12,2%** *(≤ +15%)* | **+9,7%** *(≤ +15%)* |
| *Pembanding historis v0.5.7* (`TES-PENERIMAAN-V057.md`) | Ya (`14.612 char` terbawa terus) | Ya | `31500` | — | *5.549 (A+B)* | *56.171 (+107%)* | — | — |

> **Catatan transparan pengukuran:** Dibanding v0.5.7 (`56.171` token / `+107%`), total skenario debugging turun **−14.276 token (−25,4%)**. Overhead membawa skill sendiri kini hanya **+9,7% hingga +12,2%** pada jumlah panggilan setara (7 call) dan **+12,2% hingga +14,8%** rata-rata per panggilan (keduanya di bawah pagu **+15%**). Selisih total 8 call (`41.895` vs `31.943`, `+31,1%`) murni berasal dari **1 langkah repro wajib tambahan** (`node hitung_total.js` sebelum edit) yang membuktikan metode `systematic-debugging` benar-benar diikuti, bukan diabaikan.

#### 1B. Bukti Stub/Arsip Isi Skill di Sesi Panjang (`p1-long-prune` & `call_token_metrics`)
- Transkrip `p1-long-prune` (`pipe.js` → `mod1.js` → `mod2.js` → `mod3.js`, bug `17` → `27`):
  - Call 1: `skill_view(systematic-debugging)` + baca paralel `pipe.js`, `mod1.js`, `mod2.js`, `mod3.js`.
  - Call 2: `est_tool_results_tokens = 734` (`actual_in = 5.104` — salinan kerja ringkas, turun ~86% dari ~5.200 token isi penuh!).
  - **Call 3 (`pruned_count = 1`):** `est_tool_results_tokens` **langsung anjlok dari `734` → `303` (−59%)**, isi hasil `skill_view` diarsipkan byte-exact ke:
    `pruned/<sid>/0003_skill_systematic-debugging_call_14948801104567527444.txt`
  - Call 4 s.d. Call 8: `est_tool_results_tokens` bertahan kecil di kisaran `326–502` token (berbentuk stub arsip, BUKAN 14,6 rb karakter utuh), dan output akhir `node pipe.js` = **`27`**.
- **Bukti muat ulang (`full=true`):** Terverifikasi otomatis di `tests/v059_three_debts.rs::p1_long_session_prunes_skill_result_to_stub_archives_byte_exact_and_allows_reload` — sesudah hasil `skill_view` pertama berubah menjadi stub arsip, pemanggilan ulang `skill_view(name="systematic-debugging", full=true)` di giliran akhir kembali memuat isi penuh (> 10.000 karakter) tanpa terblokir dedup cache.

---

### Perbaikan 2 — Pantauan `watch_job` Hidup di CLI Murni (Tanpa Gateway)

#### 2A. Transkrip Live Kasus Selesai (`p2-watch-done`)
1. **Sesi 1 (CLI `selen --yes run`):** Agent mendaftarkan pantauan `#1` (`Job CLI V059`) atas `job_status.txt` (`STAGE=S1`, `PROGRESS=1/3`). Saat sesi 1 ditutup bersih, 1 putaran polling berjalan otomatis:
   ```text
   ⏳ Progres 'Job CLI V059': S1 (1/3)
   ```
2. **Di luar sesi (tanpa gateway):** `job_status.txt` diubah menjadi `STATUS=SELESAI` / `SUMMARY=22 halaman selesai dirender` / `OUTPUT=out/cli-v059.zip`.
3. **Sesi 2 (`"/exit" | selen --plain` di folder yang sama) — laporan selesai tercetak persis 1x di awal sesi:**
   ```text
   ✅ Tugas 'Job CLI V059' (#1) selesai.
   Hasil: 22 halaman selesai dirender
   Lokasi: out/cli-v059.zip
   SelenCode — halo, aku Selena. Ketik perintah, /help untuk bantuan, /exit untuk keluar.
   ```
4. **Sesi 3 (`"/exit" | selen --plain` di folder yang sama) — TIDAK ada laporan ulang:**
   ```text
   SelenCode — halo, aku Selena. Ketik perintah, /help untuk bantuan, /exit untuk keluar.
   ```

#### 2B. Transkrip Live Kasus Gagal (`p2-watch-fail`) & Kontrol Sunyi (`p2-watch-quiet`)
- **Gagal di tengah (`STATUS=FAILED` / `ERROR=memori habis pada halaman 7`):**
  - Sesi 2 dibuka:
    ```text
    ❌ Tugas 'Job Gagal V059' (#2) gagal.
    Error: memori habis pada halaman 7
    ```
  - Sesi 3 dibuka lagi: **sunyi total** (pantauan berhenti otomatis di status `failed`).
- **Kontrol tanpa pantauan aktif (`p2-watch-quiet`):** pembukaan sesi CLI **tidak menampilkan sepatah kata pun** soal pantauan; biaya token = **0**.
- **Pemakaian bergantian CLI ↔ gateway atas pantauan yang sama:** Terverifikasi otomatis di `tests/v059_three_debts.rs::p2_cli_watch_polling_completion_failure_gateway_interleave_and_quiet_control` — fungsi tunggal `evaluate_and_poll_watch` + `persist_watch_transition` menjamin nol laporan dobel saat gateway dan CLI bergantian memeriksa tahap `S1`, `S2`, dan `SELESAI` (0 baris `usage`).

---

### Perbaikan 3 — Satu Store Memory Persisten + Migrasi Data Lama

#### 3A. Bukti Live Preferensi Pemilik → Store Persisten Tunggal (`p3-mem-pref` → `p3-mem-other-proj`)
- **Daftar tool aktif (terverifikasi di `tests/v059_three_debts.rs::p3_single_memory_tool_in_registry_toolsets_and_prompt`):** hanya ada **`memory`**; **`manage_memory` sudah tidak ada** di `default_registry()` maupun `EXTENDED_TOOLSETS`.
- **Sesi 1 (`p3-mem-pref`):** Pemilik meminta menyimpan preferensi tutup ringkasan dengan `[KANONIK-V059]`.
  - Tool yang dipanggil agent: `[tool] memory — Preferensi kerja: setiap kali menulis r…`
  - Isi berkas store `profile.md` di disk:
    ```text
    - Preferensi kerja: setiap kali menulis ringkasan singkat, selalu tutup dengan penanda [KANONIK-V059].
    ```
  - Keluaran `/memory` (100% identik dengan berkas store):
    ```text
    === Memori Persisten & Profil Pemilik ===
      * profil  ·  102/1375 char  ·  ...\p3-data\selencode\memory\profile.md
      - P#1  ·  profile  ·  Preferensi kerja: setiap kali menulis ringkasan singkat, selalu tutup dengan penanda [KANONIK-V059].
      * memory  ·  0/2200 char  ·  ...\p3-data\selencode\memory\memory.md
      * (wadah memory masih kosong)
    ```
  - Jumlah baris di tabel SQLite lama `memories`: **`P3A_SQLITE_MEMORIES_COUNT=0`**.
- **Sesi 2 di proyek berbeda (`p3-mem-other-proj`, tanpa diingatkan):**
  - Prompt: `"Tulis satu kalimat ringkasan singkat tentang bahasa Rust."`
  - Jawaban: `"Rust adalah bahasa pemrograman sistem yang menekankan keamanan memori, konkurensi yang aman, dan performa tinggi tanpa garbage collector. [KANONIK-V059]"` — **ditaati otomatis**.

#### 3B. Bukti Live Migrasi Data Lama (`p3-mig-proj`)
- **Sebelum menjalankan `v0.5.9`:** `SEEDED_LEGACY_ROWS=1` di tabel SQLite `memories` (`"Setiap sapaan singkat wajib menyertakan kode warisan [WARISAN-88]."`), berkas `memory.md` belum ada.
- **Sesudah membuka `v0.5.9` (`/memory`):**
  ```text
  === Memori Persisten & Profil Pemilik ===
    * profil  ·  0/1375 char  ·  ...\p3-mig-data\selencode\memory\profile.md
    * memory  ·  68/2200 char  ·  ...\p3-mig-data\selencode\memory\memory.md
    - M#1  ·  memory  ·  Setiap sapaan singkat wajib menyertakan kode warisan [WARISAN-88].
  ```
  - Isi berkas `memory.md` di disk: `- Setiap sapaan singkat wajib menyertakan kode warisan [WARISAN-88].`
  - Sisa baris di tabel SQLite lama: **`REMAINING_LEGACY_ROWS=0`**.
- **Sesi baru menaatinya:** Prompt `"Sapa saya dengan satu kalimat singkat."` → `"Halo, selamat datang dan semoga harimu menyenangkan [WARISAN-88]!"`.
- **Uji pagu meluber saat migrasi (tidak dipotong diam-diam):** Terverifikasi di `tests/v059_three_debts.rs::p3_legacy_sqlite_memory_migration_respects_cap_without_silent_truncation` — saat gabungan melebihi pagu, entri berlebih ditahan utuh dan peringatan minta rangkum dikembalikan.

---

## Tabel Regresi Token Aktual v0.5.9 (dari Tabel `usage` SQLite — `c318b1d`)

| Skenario | Panggilan API | Rata-rata Input / Call | Total Input | Total Output | Status vs Pagu |
|---|---:|---:|---:|---:|---|
| **Proyek A** (`regresi-a`, CSV rekap) | 7 | **4.606** | 32.239 | 925 | Total `240`, rata-rata `80` |
| **Proyek B** (`regresi-b`, kalkulator CLI) | 6 | **5.173** | 31.038 | 1.801 | `add/sub/mul/div` + bagi nol exit 1 |
| **Gabungan A + B (bobot panggilan)** | **13** | **4.867** | **63.277** | **2.726** | **LULUS (pagu ≤ 5.000 in/call; turun dari 5.549 di v0.5.7)** |
| **Proyek E — Awal + Resume** (`regresi-e`) | 3 (call #3 = resume) | **3.851** *(call resume)* | 11.834 | 298 | **LULUS (pagu Resume ≤ 7.500 in; margin −3.649 token)** |

---

## Daftar Bagian yang Sengaja Tidak Diubah

- **Hasil v0.5.8:** disiplin riset & verifikasi 3 lapis termasuk `folder_map` Tree-sitter (~300 token), ringkasan penutup wajib (`max_steps`/stagnasi + fallback templat) & daftar periksa coding SWE-agent (`SelfCorrectState`), serta tampilan aksi tool ala Hermes (`src/tool_display.rs`, blok `Aksi Tool (N)` tanpa emoji dekoratif).
- **Bagian v0.5.7 yang lulus penuh:** jalur resume ramping (`build_resume_snapshot`, `RESUME_CORE_TOOLS`), `send_file` Telegram, `delegate` `mode="work"` (worktree terisolasi), `/usage` (langsung dari tabel `usage`), dan `selen skills install` / `remove`.
- **Kebiasaan memuat skill (v0.5.7 Perbaikan 2):** aturan memuat skill yang cocok sebelum aksi tool pertama dipertahankan persis — v0.5.9 hanya memangkas biaya membawanya di konteks.
