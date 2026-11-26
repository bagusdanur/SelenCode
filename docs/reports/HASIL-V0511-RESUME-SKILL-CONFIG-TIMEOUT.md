# Laporan Hasil Perbaikan SelenCode v0.5.11 — Lantai Resume, Gerbang Skill Level Loop, Kebersihan Rilis, & Timeout 1×

- **Versi rilis:** `v0.5.11`
- **Rilis publik GitHub:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.11
- **Commit implementasi:**
  - `9848c82` (`feat(v0.5.11): lean resume follow-ups, loop skill gate, config warnings + release report gate, 1x timeout abort`)
  - `d6c6626` (`fix(test): pass title arg to Store::create_session in v0511 test`)
  - `b67a8c0` (`fix(test): extract Block::ToolResult text & include active resume msg in v0511 snapshot test`)
  - `fbe6d77` (`fix(test): use EditFile old/new keys & neutral prompt in v0511 resume test`)
  - `4784e97` (`docs(v0.5.11): add pre-tag release report (release gate compliance)`) — **di-commit SEBELUM tag `v0.5.11` dibuat**
- **CI (`fbe6d77`):** Run `#45843280662` — **100% HIJAU** (`Cargo Test & Visual Render (Linux)` + `Cargo Test & Check (Windows x64)`, seluruh 36 suite tes termasuk `v0511_resume_skill_config_timeout` + Clippy correctness lulus).
- **Release workflow (`v0.5.11`):** Run `#45844739339` — **100% HIJAU** (termasuk **Gerbang Laporan Rilis** baru yang memverifikasi berkas ini hadir di dalam tag `v0.5.11` sebelum kompilasi berjalan), 3 biner publik terbit:
  - `selen-linux-x64`: `16.829.584` byte (16,05 MB)
  - `selen-termux-arm64`: `15.160.080` byte (14,46 MB)
  - `selen-windows-x64.exe`: `15.528.448` byte (14,81 MB)

---

## 1. Perbaikan 1 — Lantai Resume Ramping di Seluruh Panggilan Giliran Resume (**LULUS**)

### 1A. Tabel Komposisi SEMUA Panggilan Giliran Resume — Sebelum (`v0.5.10`) vs Sesudah (`v0.5.11`)
Diukur pada skenario E identik persis (`kata.txt` berisi `sandi=BULAN-TERANG-7741` & konteks gudang selatan; giliran 1 baca tanpa mencetak ulang sandi; `/resume 1` meminta verifikasi ulang sehingga giliran resume mengeksekusi **2 panggilan API penuh**; seluruh angka dari `call_token_metrics` + `usage` SQLite dengan `ag/gemini-4-neon-medium`):

| Versi & Panggilan | Fase | `est_system` | `est_tools` | `active_tools` | `est_history` | `est_tool_res` | **Aktual Input (`usage`)** |
|---|---|---:|---:|---:|---:|---:|---:|
| `v0.5.10` Call #3 (Resume #1) | Panggil `read_file` | 1.655 | 517 | 6 | 117 | 0 | **4.427** |
| `v0.5.10` Call #4 (Resume #2) | Jawaban akhir | 1.655 | 517 | 6 | 146 | 41 | **4.202** |
| **Total Resume `v0.5.10` (Sebelum)** | **2 panggilan** | **3.310** | **1.034** | — | **263** | **41** | **8.629** *(kanonik v0.5.10: 7.959)* |
| `v0.5.11` Call #3 (Resume #1) | Panggil `read_file` | **433** | 517 | 6 | 117 | 0 | **3.477** *(−950)* |
| `v0.5.11` Call #4 (Resume #2) | Jawaban akhir | **433** | 517 | 6 | 147 | 41 | **3.294** *(−908)* |
| **Total Resume `v0.5.11` (Sesudah)** | **2 panggilan** | **866** *(−73,8%)* | **1.034** | — | **264** | **41** | **6.771** *(−1.858 / −21,5%, pagu ≤ 7.500)* |

- **Jawaban live (`v0.5.11`):** *"Kata sandi pada `kata.txt` adalah `BULAN-TERANG-7741` dengan konteks kode akses gudang selatan untuk audit malam."* (100% akurat).
- **Uji skenario E lanjutan (aktivasi tool malas pada sesi resume):** di sesi resume, prompt *"Cari di sesi lain frasa KODE-ARSIP-511 dan sebutkan kodenya."* berhasil memanggil `session_search` (`used_session_search = true`, `found_code = true`) pada giliran itu juga.
- **Ringkasan perubahan (`src/agent/context.rs`, `src/agent/loop.rs`):**
  1. `build_resume_system_prompt`: prompt sistem khusus giliran resume (~1.050 char vs 3.200 char pada sesi baru) tanpa mengubah sesi baru.
  2. Katalog 14 skill penuh hanya dikirim di giliran resume bila `skills_view`/`skills_mem` aktif; jika tidak, diganti 1 baris penunjuk.
  3. Riwayat pra-resume > 6 pesan diringkas menjadi `[Ringkasan checkpoint pra-resume]`, dan pada `step >= 2` giliran resume hasil tool besar (>200 char) di luar 2 pesan terakhir langsung diarsip ke stub.

---

## 2. Perbaikan 2 — Gerbang Skill di Level Loop + Konsistensi Muat-Ulang `skill_view` (**LULUS**)

### 2A. Mekanisme Penegakan di Loop Harness (`src/agent/loop.rs`)
Bukan tambahan kalimat prompt, melainkan gerbang deterministik dua titik berbasis klasifikasi jenis tugas (`classify_required_skill_for_turn`):
1. **Gerbang Pre-Mutate:** jika tugas tergolong debugging/TDD dan model mencoba `edit_file`/`write_file`/`lsp_rename` sebelum `skill_view` dimuat, harness menahan aksi pengubah (berkas di disk tidak tersentuh), mengeksekusi `skill_view` (salinan kerja ringkas ~1.900 char), menyimpan pasangan `ToolUse`/`ToolResult` sah ke SQLite DB, lalu mengembalikan kontrol ke model.
2. **Suntikan Pasca-Langkah-0 (nol-pemborosan):** jika pada langkah 0 model hanya membaca berkas (`read_file`) tanpa memanggil `skill_view`, harness menyuntikkan salinan kerja `skill_view` segera sesudah langkah 0 (0 panggilan LLM terbuang, `skill_view` 100% tercatat di DB sebelum `edit_file` pertama).
- Pilihan ini dipilih karena menjamin 100% kepatuhan lintas redaksi prompt sekaligus menghindari pemborosan 1 giliran LLM penuh akibat penolakan mentah.

### 2B. Bukti Live dari SQLite `messages` (`selen 0.5.11`, `ag/gemini-4-neon-medium`)

| Skenario Uji Live | Larian | Urutan Tool di DB (`messages`) | `skill_view` Sebelum Edit? | Output Verifikasi | Total Input (`usage`) |
|---|---|---|---:|---|---:|
| **Kanonik B-DBG1** (`hitung_total.js`, *"seharusnya 31500.0"*) | Run 1 | `skill_view → read_file → run_shell → edit_file → run_shell → activate_toolset → verify_project` | **Ya (1/3)** | **`31500`** | 44.818 (8 call) |
| **Kanonik B-DBG1** | Run 2 | `skill_view → read_file → edit_file → run_shell → activate_toolset → verify_project` | **Ya (2/3)** | **`31500`** | 38.102 (7 call) |
| **Kanonik B-DBG1** | Run 3 | `skill_view → read_file → edit_file → run_shell → activate_toolset → verify_project` | **Ya (3/3)** | **`31500`** | 37.265 (7 call) |
| **Kanonik B-DBG2 Pajak** (`pajak.js`, *"seharusnya 34965"*) | Run 1 | `skill_view → read_file → list_files → run_shell → edit_file → run_shell → activate_toolset → verify_project` | **Ya (1/3)** | **`34965`** | 49.267 (9 call) |
| **Kanonik B-DBG2 Pajak** | Run 2 | `skill_view → read_file → run_shell → edit_file → run_shell → activate_toolset → list_files → verify_project` | **Ya (2/3)** | **`34965`** | 43.353 (8 call) |
| **Kanonik B-DBG2 Pajak** | Run 3 | `skill_view → read_file → edit_file → run_shell → list_files → activate_toolset → verify_project` | **Ya (3/3)** | **`34965`** | 37.029 (7 call) |
| **Bug Terbuka** (*"temukan akar penyebabnya"*) | Run 1–2 | Panggilan pertama = **`skill_view`** di **2/2 larian** | **Ya (2/2)** | **`31500`** | — |
| **TDD tanpa menyebut "skill"** (*"Tulis fitur formatRupiah ... lengkap dengan tesnya"*) | Run 1 | `skill_view(test-driven-development) → list_files → run_shell → write_file → ...` | **Ya (`test-driven-development`)** | Tes lulus | — |
| **Kontrol Trivial** (*"Balas ok saja."*) | Run 1 | `[]` (0 tool) | **0 skill (LULUS)** | `ok` | — |
| **Kontrol Proyek A (`rekap.js`) & Proyek B (`calc.js`)** | Run 1 | `loaded_skill = false` pada keduanya (meski Proyek B memuat *"cetak error ke stderr"*) | **0 skill (LULUS)** | Benar | — |

- **Perbandingan token B-DBG1 dengan-skill vs baseline tanpa-skill (`v0.5.9` baseline 7 call = `31.943` / `4.563 in/call`):** pada `v0.5.11`, larian 7-call kanonik DBG1 (`37.265`–`38.102`, rata-rata `5.323` in/call) dan DBG2 (`37.029`, `5.290` in/call) tetap jauh di bawah `v0.5.7` (`56.171`, **−33,7%**) berkat salinan kerja ringkas + prune dini.
- **Bukti live Cacat B — muat-ulang `skill_view` konsisten (`p2_reload_live`, DB `messages`):**
  - Muatan pertama (`msg_id: 4`): **`1.900` karakter** (salinan kerja ringkas)
  - Muat-ulang di tengah sesi tanpa `full=true` (`msg_id: 8`): **`1.900` karakter** (identik 100%, BUKAN `14.616` karakter!). Permintaan eksplisit `full=true` tetap mengembalikan teks penuh (>10.000 char) dan masuk mesin prune/arsip sesudahnya (`tests/v0511_resume_skill_config_timeout.rs`).

---

## 3. Perbaikan 3 — Gerbang Laporan Rilis, `config.example.toml`, & Peringatan Kunci Config (**LULUS**)

1. **Konfirmasi gerbang laporan rilis (`.github/workflows/release.yml`):**
   - Berkas `docs/reports/HASIL-V0511-RESUME-SKILL-CONFIG-TIMEOUT.md` di-commit pada `4784e97` **sebelum** tag `v0.5.11` dibuat, dan langsung lulus langkah `Gerbang Laporan Rilis` pada Release Run `#45844739339`.
2. **Cuplikan `config.example.toml` yang diperbarui:**
   ```toml
   # Tenggat waktu panggilan HTTP/streaming ke provider AI (v0.5.10).
   # PENTING: `[timeouts]` adalah seksi TOP-LEVEL — BUKAN sub-tabel di bawah `[providers.<nama>]`!
   [timeouts]
   connect_secs = 20                                # batas sambung TCP/TLS (detik)
   response_secs = 60                               # batas menunggu header respons awal provider (detik)
   stream_idle_secs = 90                            # batas diam antar-potongan streaming (detik)
   total_secs = 300                                 # batas keras total 1 panggilan provider (detik)

   # Telegram Bot Gateway (`selen telegram`) — opsional.
   # [telegram]
   # turn_timeout_secs = 900                        # batas waktu keras 1 giliran agent (detik; default 900 = 15 menit)
   # stall_notice_secs = 180                        # kirim 1 peringatan macet bila giliran diam tanpa progres (detik; default 180)
   # poll_watchdog_secs = 300                       # watchdog OS-thread: keluar non-nol (restart systemd) bila polling macet (detik; default 300)
   ```
3. **Bukti live peringatan kunci tak dikenal & salah tempat (`selen 0.5.11 doctor`, exit code `0` — start tetap jalan):**
   - **Stderr startup:**
     ```text
     [config peringatan] (./selencode.toml) `providers.9router.timeouts`: `timeouts` seharusnya top-level (`[timeouts]`), bukan di bawah provider — diabaikan
     [config peringatan] (./selencode.toml) `telegram.turn_timeot_secs`: kunci config tidak dikenal — diabaikan
     ```
   - **Baris `selen doctor`:**
     ```text
       - Config Keys  ·  WARN  ·  2 peringatan: `providers.9router.timeouts`: `timeouts` seharusnya top-level (`[timeouts]`), bukan di bawah provider — diabaikan; `telegram.turn_timeot_secs`: kunci config tidak dikenal — diabaikan
     ```

---

## 4. Perbaikan 4 — Pembatalan Provider Diam 1× Response Timeout (**LULUS**)

Diukur live pada biner rilis `selen 0.5.11` terhadap mock server HTTP diam lokal dengan **config routing bawaan (`max_retries = 1`)** dan `response_secs = 3`:

| Skenario (`max_retries = 1` bawaan, `response_secs = 3`) | Kunjungan ke Provider Diam (`silent_hits`) | Waktu Total | Exit Code | Keluaran Aktual |
|---|---:|---:|---:|---|
| **Provider diam tunggal** (tanpa rute berbeda) | **1×** *(bukan 2×!)* | **3,52 dtk** *(≈1× 3 dtk, turun separuh dari 2×)* | `1` | `Error: timeout 3 detik menunggu respons awal provider ... — provider tidak merespons` |
| **Provider diam + fallback rute BERBEDA sehat** | **1×** | **3,23 dtk** | `0` | stderr: `[selen] Model utama gagal (timeout/koneksi) — lanjut dengan mock-healthy`<br>stdout: `Halo dari fallback berbeda yang sehat!` |

---

## 5. Tabel Regresi Token Aktual v0.5.11 (dari Tabel `usage` SQLite)

| Skenario (`ag/gemini-4-neon-medium`) | Panggilan API | Rata-rata Input / Call | Total Input | Total Output | Catatan & Status |
|---|---:|---:|---:|---:|---|
| **Proyek E — Giliran Resume (2-call)** | 2 (`#3`, `#4`) | **3.386** / call | **6.771** *(turun dari 8.629 di v0.5.10)* | 277 | **LULUS** *(pagu ≤ 7.500 total in; margin −729 token)* |
| **Proyek A** (`rekap.js`, 0 skill) | 7 | **4.825** | 33.776 | 1.021 | `Total: 240`, `Rata-rata: 80` (call #6–7 opsional `verify_project` oleh model) |
| **Proyek B** (`calc.js`, 0 skill) | 7 | **5.246** | 36.725 | 1.787 | `add/sub/mul/div` + div 0 exit 1 |
| **Gabungan Proyek A + Proyek B** | **14** | **5.036** *(v0.5.10: 4.956 di 13 call)* | **70.501** | **2.808** | **Catatan transparan:** +36 token di atas 5.000 murni berasal dari 1 panggilan tambahan stokastik model pada Proyek A (7 call vs 6 call di v0.5.10; `est_system` sesi baru identik persis `1.661`, `est_tools` hanya `+8` token deskripsi `skill_view`). |

---

## 6. Bagian yang Sengaja Tidak Diubah

- Seluruh ketahanan gateway **v0.5.10**: `[timeouts]` provider + `AbortOnDrop`, `turn_timeout_secs`, `stall_notice_secs`, OS-thread `poll_watchdog_secs` + pemulihan startup, batas memori (`run_timed` 2 MB/pipe, `UndoStack` 32 MB, `trim_gateway_session_memory`), serta pembukuan `watch_job` + laporan gagal kelas satu + status segar bertanggal (`watch_job(action="check")`).
- Seluruh capaian **v0.5.9**: salinan kerja `skill_view` + prune/arsip dini, polling `watch_job` di CLI murni, dan satu store memori persisten (`memory`) + migrasi data lama.
- Seluruh capaian **v0.5.8, v0.5.7, & sebelumnya**: disiplin riset & verifikasi 3 lapis + `folder_map` Tree-sitter, ringkasan penutup + daftar periksa coding, tampilan blok `Aksi Tool (N)` polos, jalur resume ramping, `send_file`, `delegate` worktree, `/usage`, `skills install`/`remove`, `ask_user`, `cron`, `session_search`, pelacakan approval, `[skills] auto_load` tetap opt-in, serta 12+ skill bawaan + `NOTICE`.
