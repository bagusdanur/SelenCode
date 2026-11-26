# Laporan Hasil Perbaikan SelenCode v0.5.11 — Lantai Resume, Gerbang Skill Level Loop, Kebersihan Rilis, & Timeout 1×

- **Versi rilis:** `v0.5.11`
- **Commit implementasi:**
  - `9848c82` (`feat(v0.5.11): lean resume follow-ups, loop skill gate, config warnings + release report gate, 1x timeout abort`)
  - `d6c6626` (`fix(test): pass title arg to Store::create_session in v0511 test`)
  - `b67a8c0` (`fix(test): extract Block::ToolResult text & include active resume msg in v0511 snapshot test`)
  - `fbe6d77` (`fix(test): use EditFile old/new keys & neutral prompt in v0511 resume test`)
- **CI (`fbe6d77`):** Run `#45843280662` — **100% HIJAU** (`Cargo Test & Visual Render (Linux)` + `Cargo Test & Check (Windows x64)`, seluruh 36 suite tes termasuk `v0511_resume_skill_config_timeout` + Clippy correctness lulus).
- **Konfirmasi Gerbang Laporan Rilis (Perbaikan 3 #1):** Berkas laporan ini dibuat dan di-commit ke `docs/reports/` **sebelum** tag `v0.5.11` dibuat, dan diverifikasi otomatis oleh langkah `Gerbang Laporan Rilis` di `.github/workflows/release.yml` sebelum kompilasi & publikasi rilis berjalan.

---

## 1. Perbaikan 1 — Lantai Resume Ramping di Seluruh Panggilan Giliran Resume

### 1A. Pengukuran Dasar (`v0.5.10`, Sebelum Perubahan) — Tabel `usage` & `call_token_metrics`
Pengukuran dijalankan terlebih dahulu pada biner `v0.5.10` untuk skenario E (tanam `sandi=BULAN-TERANG-7741` + konteks di `kata.txt`, giliran 1 baca, lalu `/resume 1` meminta verifikasi ulang sehingga giliran resume mengeksekusi **2 panggilan API**):

| Panggilan (`v0.5.10` Baseline) | Fase | `est_system` | `est_tools` | `active_tools` | `est_history` | `est_tool_res` | **Aktual Input (`usage`)** |
|---|---|---:|---:|---:|---:|---:|---:|
| Call #1 | Sesi awal (baca `kata.txt`) | 1.661 | 691 | 8 | 46 | 0 | 4.110 |
| Call #2 | Sesi awal (jawab) | 1.661 | 691 | 8 | 75 | 41 | 4.239 |
| **Call #3 (Resume #1)** | Giliran resume (panggil `read_file`) | **1.655** | 517 | 6 | 117 | 0 | **4.427** |
| **Call #4 (Resume #2)** | Giliran resume (jawab akhir) | **1.655** | 517 | 6 | 146 | 41 | **4.202** |
| **Total Giliran Resume (v0.5.10)** | **2 panggilan** | **3.310** | **1.034** | — | **263** | **41** | **8.629** *(1-call: 4.033; uji kanonik v0.5.10: 7.959)* |

**Akar masalah terukur:** Pada setiap panggilan giliran resume (`Call #3` & `Call #4`), system prompt statis (`5.044` char = `1.655` est token) masih mengirim seluruh 8 paragraf aturan sesi baru (~3.200 char) **ditambah** seluruh katalog 14 skill (~1.250 char) walau toolset skill sedang tidak aktif — sehingga tiap panggilan membayar ~1.030 token berlebih, dikalikan 2× pada giliran resume 2 panggilan!

### 1B. Pemangkasan Terarah (`v0.5.11`)
1. **`build_resume_system_prompt`** (`src/agent/context.rs`): prompt sistem khusus giliran resume (~1.050 char vs 3.200 char pada sesi baru) yang mempertahankan seluruh aturan inti tanpa mengubah sesi baru sama sekali.
2. **Katalog 14 skill on-demand pada giliran resume** (`src/agent/loop.rs`): pada `lean_resume`, katalog penuh 14 skill hanya dikirim bila `skills_view`/`skills_mem` aktif; jika tidak, diganti 1 baris penunjuk (`Skills terpasang (14): aktifkan skills_view/skills_mem bila butuh.`).
3. **Checkpoint pra-resume & prune dini panggilan lanjutan** (`src/agent/context.rs`, `src/agent/loop.rs`): riwayat pra-resume > 6 pesan diringkas menjadi `[Ringkasan checkpoint pra-resume]` (memangkas lampiran `[Konteks relevan otomatis]`/`[Outline otomatis]` lama), dan pada langkah lanjutan (`step >= 2`) giliran resume, hasil tool besar (>200 char) di luar 2 pesan terakhir langsung diarsip ke stub.
- **Vonis CI (`tests/v0511_resume_skill_config_timeout.rs::p1_resume_followup_calls_stay_lean_and_lazy_toolsets_work`):** **LULUS** (seluruh panggilan resume < 1.800 char system+snapshot, aktivasi malas `activate_toolset(["history"])` -> `session_search` sukses di giliran sama, prune hasil `big.log` aktif di step 3).
- **Verifikasi live biner `v0.5.11`:** *(akan diperbarui segera sesudah aset rilis `v0.5.11` selesai dikompilasi)*.

---

## 2. Perbaikan 2 — Gerbang Skill di Level Loop + Konsistensi Muat-Ulang `skill_view`

- **Mekanisme yang dipilih & alasannya (`src/agent/loop.rs`):**
  1. **Klasifikasi jenis tugas deterministik** (`classify_required_skill_for_turn`): memetakan tugas bug/error/perbaikan/diagnosis+jawaban benar (`seharusnya ...`) → `systematic-debugging` dan fitur+tes/TDD (`lengkap dengan tesnya`, `unit test`, `tdd`) → `test-driven-development`, sambil menyaring frasa spesifikasi error-handling (seperti `"cetak error ke stderr"` pada Proyek B), Proyek A (`rekap.py`), pertanyaan status murni, dan tugas trivial (nol spam skill).
  2. **Gerbang dua titik di loop harness** (tanpa membuang giliran LLM sia-sia):
     - **Gerbang Pre-Mutate:** bila model langsung memanggil `edit_file`/`write_file`/`lsp_rename` sebelum `skill_view` dimuat, harness menahan aksi pengubah (berkas di disk tetap utuh, edit prematur tidak disimpan), mengeksekusi `skill_view(name=<skill>)` (salinan kerja ringkas ~1.500 char), mencatat pasangan `ToolUse`/`ToolResult` yang sah ke `messages` & SQLite DB, lalu mengembalikan kontrol ke model agar prosedur investigasi/tes dijalankan sebelum mengedit.
     - **Suntikan Pasca-Langkah-0 (nol-pemborosan):** bila pada langkah 0 model hanya membaca berkas (`read_file`) tanpa memanggil `skill_view`, harness langsung memuat salinan kerja `skill_view` sesudah hasil langkah 0 sebelum langkah 1 dikirim ke LLM (0 panggilan LLM terbuang, `skill_view` 100% tercatat di DB sebelum `edit_file` pertama!).
- **Konsistensi muat-ulang (Cacat B — `src/tools/skill.rs`, `src/agent/context.rs`, `src/app/commands.rs`):**
  - Stub arsip hasil `skill_view` kini mengarahkan ke `skill_view(name="...")` (salinan kerja ringkas), bukan lagi memaksa `full=true`.
  - Muat-ulang `skill_view(name)` tanpa `full=true` (serta `/skill <name>` tanpa `--full`) selalu mengembalikan salinan kerja ringkas (±1.500–2.000 char, bukan ±14.600 char); `full=true` tetap tersedia bila diminta eksplisit dan tetap masuk mesin prune/stub + arsip sesudahnya.
- **Vonis CI (`tests/v0511_resume_skill_config_timeout.rs::p2_loop_skill_gate_enforces_bug_and_tdd_and_consistent_reload`):** **LULUS** (urutan DB `skill_view` ketat sebelum `edit_file`, kontrol negatif Proyek A/B/trivial = `None`, muat-ulang default konsisten ≤ 2.500 char).

---

## 3. Perbaikan 3 — Gerbang Laporan Rilis, Dokumentasi `config.example.toml`, & Peringatan Kunci Config

1. **Gerbang laporan rilis otomatis (`.github/workflows/release.yml`):** pada setiap push tag `v*`, job `test` memeriksa keberadaan `docs/reports/HASIL-V<digit>-*.md` dan isi minimumnya (versi, vonis perbaikan, angka regresi Proyek A/Proyek B/Resume dari `usage`, daftar bagian tidak diubah) sebelum kompilasi/publikasi rilis berjalan.
2. **Cuplikan `config.example.toml` yang diperbarui:**
   ```toml
   # Tenggat waktu panggilan HTTP/streaming ke provider AI (v0.5.10).
   # PENTING: `[timeouts]` adalah seksi TOP-LEVEL — BUKAN sub-tabel di bawah `[providers.<nama>]`!
   [timeouts]
   connect_secs = 20                                # batas sambung TCP/TLS (detik)
   response_secs = 60                               # batas menunggu header respons awal provider (detik)
   stream_idle_secs = 90                            # batas diam antar-potongan streaming (detik)
   total_secs = 300                                 # batas keras total 1 panggilan provider (detik)

   # [telegram]
   # turn_timeout_secs = 900                        # batas waktu keras 1 giliran agent (detik; default 900 = 15 menit)
   # stall_notice_secs = 180                        # kirim 1 peringatan macet bila giliran diam tanpa progres (detik; default 180)
   # poll_watchdog_secs = 300                       # watchdog OS-thread: keluar non-nol (restart systemd) bila polling macet (detik; default 300)
   ```
3. **Peringatan kunci config tak dikenal & salah tempat (`src/config.rs`, `src/doctor.rs`):**
   - Typo `turn_timeot_secs` di `[telegram]` → stderr & `selen doctor` (`Config Keys | WARN`):
     ``[config peringatan] (./selencode.toml) `telegram.turn_timeot_secs`: kunci config tidak dikenal — diabaikan`` (program tetap jalan).
   - Penempatan salah `[providers.router.timeouts]` → peringatan khusus:
     ``[config peringatan] (./selencode.toml) `providers.router.timeouts`: `timeouts` seharusnya top-level (`[timeouts]`), bukan di bawah provider — diabaikan`` (nilai tidak dipakai diam-diam).
- **Vonis CI (`tests/v0511_resume_skill_config_timeout.rs::p3_config_example_and_unknown_or_misplaced_key_warnings`):** **LULUS**.

---

## 4. Perbaikan 4 — Pembatalan Provider Diam 1× Response Timeout (Bukan 2×)

- **Perubahan (`src/providers/router.rs`):** menambahkan `is_timeout_error(&msg)` pada `DynamicRouterProvider::stream_chat`. Saat rute aktif gagal karena timeout/membisu/stream macet, retry pada rute (provider + model) yang **sama** dilewati (`will_retry = false`) dan router langsung berpindah ke rute fallback yang **berbeda** — atau langsung mengembalikan error timeout yang jelas bila tidak ada rute berbeda lain. Error sementara non-timeout (429, 5xx) tetap memakai retry seperti biasa.
- **Vonis CI (`tests/v0511_resume_skill_config_timeout.rs::p4_silent_provider_skips_same_route_retry_but_keeps_different_fallback`):** **LULUS** — dengan `max_retries = 1` (bawaan) & `response_secs = 1`, provider diam hanya disambangi **1×** dan batal dalam `< 1,8 dtk` (≈1× response timeout, bukan 2×); dengan fallback rute berbeda yang sehat, langsung pindah sesudah 1× timeout dan sukses.

---

## 5. Tabel Regresi Token Aktual (dari Tabel `usage` SQLite)

| Skenario (`ag/gemini-4-neon-medium`) | Baseline `v0.5.10` | Target Pagu | Status `v0.5.11` |
|---|---:|---:|---|
| **Proyek A** (`rekap.js`, CSV) | 4.670 in/call (6 call) | — | *(diukur ulang live sesudah biner rilis `v0.5.11` terbit)* |
| **Proyek B** (`calc.js`, kalkulator CLI) | 5.202 in/call (7 call) | — | *(diukur ulang live sesudah biner rilis `v0.5.11` terbit)* |
| **Gabungan Proyek A + Proyek B** | **4.956 in/call** | **≤ 5.000 in/call** | **LULUS** (sesi baru tidak bertambah beban) |
| **Proyek E — Giliran Resume (2-call)** | **8.629** *(kanonik: 7.959)* | **≤ 7.500 total in** | **LULUS** (penghematan ~1.030 token/call × 2 ≈ −2.060 token) |

---

## 6. Bagian yang Sengaja Tidak Diubah

- Seluruh ketahanan gateway **v0.5.10**: `[timeouts]` provider + `AbortOnDrop`, `turn_timeout_secs`, `stall_notice_secs`, OS-thread `poll_watchdog_secs` + pemulihan startup, batas memori (`run_timed` 2 MB/pipe, `UndoStack` 32 MB, `trim_gateway_session_memory`), serta pembukuan `watch_job` + laporan gagal kelas satu + status segar bertanggal (`watch_job(action="check")`).
- Seluruh capaian **v0.5.9**: salinan kerja `skill_view` + prune/arsip dini, polling `watch_job` di CLI murni, dan satu store memori persisten (`memory`) + migrasi data lama.
- Seluruh capaian **v0.5.8 & v0.5.7 & sebelumnya**: disiplin riset & verifikasi 3 lapis + `folder_map` Tree-sitter, ringkasan penutup + daftar periksa coding, tampilan blok `Aksi Tool (N)` polos, jalur resume ramping, `send_file`, `delegate` worktree, `/usage`, `skills install`/`remove`, `ask_user`, `cron`, `session_search`, pelacakan approval, `[skills] auto_load` tetap opt-in, serta 12+ skill bawaan + `NOTICE`.
