# Laporan Hasil Perbaikan SelenCode v0.5.10 — Ketahanan Gateway Anti-Gantung & Status Pekerjaan Segar

- **Versi rilis:** `v0.5.10`
- **Commit implementasi:**
  - `42b6be3` (`feat(v0.5.10): gateway resilience against silent hangs, memory bounds, & fresh job status grounding`)
  - `efde3d1` (`fix(test): add timeouts field to Config literal in tests/fase3.rs`)
  - `fd59244` (`fix(test): align v0.5.10 test calls with OpenAiProvider/DynamicRouterProvider/UndoStack APIs`)
  - `7c757b0` (`test(v0.5.10): cover both partial-token mid-stream abort and zero-token SSE stall fallback`)
  - `8ef181b` (`fix(providers): avoid 'dibatalkan' in timeout error strings so router fallback triggers cleanly`)
- **Rilis publik GitHub:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.10
- **CI (`8ef181b`):** Run `#45813422520` — **100% HIJAU** (`Cargo Test & Visual Render (Linux)` + `Cargo Test & Check (Windows x64)`, seluruh 35 suite tes termasuk `v0510_gateway_and_status` + Clippy correctness lulus).
- **Release workflow (`v0.5.10`):** Run `#45814838545` — **100% HIJAU**, 3 biner publik terbit:
  - `selen-linux-x64`: `17.000.688` byte (16,21 MB)
  - `selen-termux-arm64`: `15.295.480` byte (14,59 MB)
  - `selen-windows-x64.exe`: `15.691.264` byte (14,96 MB)

---

## 1. Temuan Akar Masalah (Investigasi Kejadian Nyata)

### 1A. Mengapa Gateway Menggantung Diam 12+ Menit dengan 2 Soket HTTPS `ESTABLISHED`
1. **Task provider terlepas saat giliran dibatalkan (`src/agent/loop.rs`):** `tokio::spawn(p.stream_chat(...))` sebelumnya tidak dibungkus guard pembatalan (`AbortOnDrop`), sehingga saat giliran dibatalkan/diinterupsi/timeout, task provider di latar tetap hidup menahan soket TCP HTTPS `ESTABLISHED`.
2. **Batas waktu HTTP provider tidak lengkap & tidak dapat diatur:** sebelumnya tidak ada batas total per panggilan (SSE keepalive dapat menahan koneksi selamanya), pembacaan body error non-2xx (`resp.text()`) tidak punya timeout, dan nilai timeout tidak bisa dikonfigurasi di `selencode.toml`.
3. **Tabrakan filter fallback pada pesan timeout (`src/providers/router.rs`):** `is_retryable_error` belum mengenali frasa timeout ketidakaktifan stream, dan pesan error yang mengandung kata `"dibatalkan"` sempat ditolak oleh filter pembatalan user di `is_fallback_eligible_error` (ditangkap oleh tes integrasi dan diperbaiki di `8ef181b`).
4. **Tidak ada pengaman giliran & kelangsungan eksternal di gateway (`src/app/telegram_runner.rs`):** belum ada `turn_timeout_secs`, belum ada pemberitahuan macet per episode ala Hermes (`stall_notice_secs`), belum ada watchdog di luar Tokio runtime (`poll_watchdog_secs`), dan startup selalu menjalankan `skip_backlog()` sehingga pesan tertahan bisa terbuang sesudah restart paksa.

### 1B. Temuan Investigasi Puncak Memori 4,6 GB (`MemoryPeak`) & Batas yang Diterapkan
Dua faktor berkontribusi:
1. **Atribusi cgroup `systemd`:** `MemoryPeak` pada unit `systemd` menghitung seluruh cgroup layanan, termasuk subproses berat (seperti worker Python/OCR/inpainting Comic-auto) yang dijalankan lewat `run_shell`.
2. **Tiga jalur retensi memori tak terbatas di dalam proses SelenCode sendiri** — ketiganya kini dibatasi keras:
   - **Capture output subproses (`src/tools/mod.rs::run_timed`):** sebelumnya memakai `cmd.output()` yang menampung `stdout`/`stderr` tanpa batas ukuran. Kini diganti pembacaan pipe streaming berbatas **`MAX_RUN_TIMED_PIPE_BYTES = 2 MB` per pipe** (`stdout` & `stderr`, sisa stream tetap dikuras agar proses anak tidak deadlock tanpa menaikkan RAM).
   - **Snapshot edit file (`src/tools/undo.rs::UndoStack`):** sebelumnya hanya membatasi jumlah entri (`200`) tanpa pagu byte. Kini dibatasi keras **`MAX_UNDO_TOTAL_BYTES = 32 MB`** (membuang snapshot tertua dengan penyesuaian indeks checkpoint yang aman).
   - **Cache sesi & riwayat pesan RAM gateway (`src/gateway_health.rs` & `src/app/telegram_runner.rs`):** sebelumnya `sessions` HashMap menyimpan setiap chat selamanya dan `s.messages` menahan payload `Block::Image` base64 (bisa 6–24 MB per giliran foto) tanpa pemangkasan. Kini `trim_gateway_session_memory` otomatis melepas base64 gambar di luar 4 pesan terbaru dan memadatkan salinan RAM saat melewati **80 pesan atau 2 MB** (riwayat penuh tetap 100% utuh di SQLite), ditambah eviksi berkala sesi chat idle (> 2 jam atau > 16 chat, melewati chat yang sedang sibuk).

### 1C. Mengapa `job_watches` Membeku & Jawaban Status Menjadi Basi
1. `parse_watch_output` sebelumnya hanya membaca JSON datar & nama kunci sempit sehingga JSON bersarang (`{"job": {...}}`) atau kunci seperti `current_stage`, `completed_pages`/`total_pages`, `error_message`, `failure_reason`, dan error validasi seperti `PIXEL_OUTSIDE_ALLOWED_MASK` tidak terdeteksi utuh.
2. Belum ada aksi cek segar langsung pada `watch_job` (`action="check"`) dan daftar `/watch` belum menampilkan cap waktu pemeriksaan terakhir (`last_checked_at`).
3. Aturan prompt belum mewajibkan pendaftaran `watch_job` saat mengulang job gagal dan belum menegaskan aturan **status segar pada giliran yang sama** beserta pengakuan jujur bila perintah cek gagal.

---

## 2. Ringkasan Perubahan & Konfigurasi Baru (`v0.5.10`)

| Area | Berkas Utama | Ringkasan |
|---|---|---|
| **P1 #1 — Batas waktu HTTP provider + Fallback** | `src/config.rs`, `src/providers/{mod,openai,anthropic,gemini,router}.rs`, `src/app/mod.rs`, `src/agent/loop.rs` | Bagian konfigurasi baru `[timeouts]` (`connect_secs = 20`, `response_secs = 60`, `stream_idle_secs = 90`, `total_secs = 300`), TCP keepalive 15s, batas baca error body 15s, `AbortOnDrop` pada task `stream_chat` agar soket langsung tertutup saat batal/timeout, serta pemetaan timeout/macet ke `is_retryable_error` & `short_fallback_reason` sehingga rantai fallback otomatis terpanggil. |
| **P1 #2 & #3 — Turn Timeout & Stall Notice Hermes** | `src/gateway_health.rs`, `src/app/telegram_runner.rs` | Konfigurasi `[telegram] turn_timeout_secs = 900` dan `stall_notice_secs = 180`. `StallTracker` mengirim **tepat 1** peringatan per episode macet (otomatis jeda saat menunggu tombol approval/`ask_user`, lalu tutup diam-diam saat ada progres). Bila giliran melampaui `turn_timeout_secs`, giliran dibatalkan bersih, task provider langsung di-abort, ringkasan penutup templat + daftar aksi tool dikirim, dan gateway langsung melayani pesan berikutnya. |
| **P1 #4 — Watchdog OS-Thread & Pemulihan Startup** | `src/gateway_health.rs`, `src/store.rs`, `src/app/telegram_runner.rs` | Konfigurasi `[telegram] poll_watchdog_secs = 300`. Thread OS terpisah di luar Tokio memantau `GatewayHealthClock` (sukses `getUpdates` + detak jantung loop utama); bila macet melewati ambang, menulis log `[watchdog] KRITIS`, menyimpan marker `pending_recovery` ke tabel SQLite `gateway_state`, lalu keluar dengan `exit(2)` agar `systemd Restart=on-failure` me-restart otomatis. Saat bangun, gateway melewati `skip_backlog()` dan melanjutkan dari `last_update_offset` tersimpan (pesan tertahan tetap diproses), mengirim **1** catatan pemulihan singkat, lalu menghapus marker (anti-loop). |
| **P1 #5 — Batas Retensi Memori** | `src/tools/mod.rs`, `src/tools/undo.rs`, `src/gateway_health.rs`, `src/app/telegram_runner.rs` | Pagu 2 MB/pipe pada `run_timed`, 32 MB pada `UndoStack`, pelepasan base64 gambar lama & pemadatan RAM > 80 pesan / > 2 MB per sesi gateway, serta eviksi sesi idle (> 2 jam / > 16 chat). |
| **P2 — Pembukuan Watch, Gagal Kelas Satu, & Status Segar** | `src/watch.rs`, `src/tools/watch.rs`, `src/app/commands.rs`, `src/agent/context.rs`, `src/tools/toolsets.rs` | `parse_watch_output` mendukung JSON bersarang, alias `current_stage`/`completed_pages`/`total_pages`/`error_message`/`failure_reason`, dan deteksi gagal kelas satu (`FAILED`/`ERROR`/`PIXEL_OUTSIDE_ALLOWED_MASK`). Tool `watch_job` kini mendukung `action="register"` (default), `action="check"` (cek segar 0-token bertanggal yang juga memperbarui SQLite & mengaku jujur saat sumber error), dan `action="list"` (bertanggal). `/watch` menampilkan `dicek <timestamp> UTC`. System prompt (Aturan 5 & 7) mewajibkan `watch_job` baru pada retry job gagal, pengakuan jujur bila job lama tanpa watch, aturan status segar di giliran yang sama (semua antarmuka), dan larangan mengarang saat cek error. Keyword `"udah belum"`, `"sudah belum"`, `"status job"`, `"sampai mana"` mengaktifkan toolset `watch` otomatis. |
| **Suite Tes Integrasi v0.5.10** | `tests/v0510_gateway_and_status.rs`, `.github/workflows/ci.yml` | 7 tes terotomasi pada CI Linux & Windows x64 mencakup mock provider membisu, macet tengah stream + fallback, `AbortOnDrop` turn timeout, `StallTracker`, OS-thread watchdog + pemulihan + persistensi `job_watches`/`cron_jobs`, batas memori `UndoStack` & sesi gateway, serta pembukuan watch, gagal kelas satu, dan `watch_job(action="check")`. |

---

## 3. Bukti Uji Penerimaan Nyata (Biner Rilis `selen 0.5.10`)

### 3A. Perbaikan 1 — Provider Membisu & Macet Tengah Stream (Live terhadap Mock HTTP Server Lokal)
Dijalankan langsung pada biner publik `selen-windows-x64.exe` (`selen 0.5.10`) dengan `[timeouts]` pendek (`connect_secs=2`, `response_secs=2`, `stream_idle_secs=2`, `total_secs=6`):

| Skenario | Waktu Selesai | Exit Code | Keluaran Aktual |
|---|---:|---:|---|
| **1a. Provider membisu total** (`/silent/v1`, tanpa fallback) | **2,13 dtk** | `1` | `Error: timeout 2 detik menunggu respons awal provider di http://127.0.0.1:.../silent/v1/chat/completions — provider tidak merespons` |
| **1b. Provider membisu + fallback sehat** (`fallback = ["healthy:mock-healthy"]`) | **2,18 dtk** | `0` | stderr: `[selen] Model utama gagal (timeout/koneksi) — lanjut dengan mock-healthy`<br>stdout: `Halo dari fallback lokal yang sehat!` |
| **1c. Provider macet di tengah stream** (`/midstream/v1`, kirim 1 chunk lalu diam) | **2,14 dtk** | `1` | stdout: `sebagian`<br>stderr: `Error: stream terputus di tengah jawaban (midstream:mock-midstream): timeout ketidakaktifan stream provider (tidak ada data selama 2 detik) — koneksi dihentikan` |
| **1d. Permintaan berikutnya sesudah abort** (`/healthy/v1`) | **0,13 dtk** | `0` | stdout: `Halo dari fallback lokal yang sehat!` |

> **Verifikasi tambahan terotomasi (CI Linux & Windows x64, `tests/v0510_gateway_and_status.rs`):**
> - `p1_turn_timeout_drops_and_aborts_provider_task_and_next_turn_works`: memverifikasi `AbortOnDrop` langsung membatalkan task provider saat turn timeout sehingga tidak ada soket bocor, dan giliran berikutnya berhasil.
> - `p1_stall_tracker_emits_once_pauses_for_approval_and_resets_silently`: memverifikasi tepat 1 peringatan per episode, jeda saat menunggu persetujuan/klarifikasi, dan reset diam-diam saat progres berlanjut.
> - `p1_os_watchdog_triggers_records_recovery_and_preserves_watches_and_cron`: memverifikasi OS-thread watchdog memicu saat polling macet, mencatat `pending_recovery` (sekali pakai, anti-loop), menjaga `last_update_offset` agar pesan tertahan tidak dibuang, serta menjaga seluruh `job_watches` & `cron_jobs` tetap utuh melintasi restart.
> - `p1_memory_bounds_cap_undo_stack_and_gateway_session_ram`: memverifikasi `UndoStack` ≤ 32 MB dan `trim_gateway_session_memory` melepas gambar base64 lama (turun dari > 2 MB menjadi < 100 KB).

### 3B. Perbaikan 2 — Pembukuan Watch, Gagal Kelas Satu, Retry + Watch, Status Segar, & Kejujuran Error (Live `9router` / `ag/gemini-4-neon-medium`)

1. **Kasus job gagal di tengah pantauan (`test_p2a`, JSON bersarang):**
   - Sesi 1 mendaftarkan `#1` (`Comic-auto #6c33`) atas `job_status.json` → DB langsung mencatat `last_stage = "S1"`, `last_progress = "1/3"`, `last_checked_at = "2026-11-26T12:09:00Z"`.
   - Tahap berubah menjadi `S2 (2/3)` → tercetak otomatis `⏳ Progres 'Comic-auto #6c33': S2 (2/3)` dan DB diperbarui `last_stage = "S2"`, `last_progress = "2/3"`.
   - File diubah menjadi `FAILED` (`PIXEL_OUTSIDE_ALLOWED_MASK on page 7`) di luar sesi → sesi CLI berikutnya dibuka dan **langsung melaporkan kegagalan kelas satu tanpa ditanya**:
     ```text
     ❌ Tugas 'Comic-auto #6c33' (#1) gagal.
     Error: PIXEL_OUTSIDE_ALLOWED_MASK on page 7
     ```
   - DB diperbarui `status = "failed"`, `delivered = 1`; pembukaan sesi berikutnya **sunyi total** (nol laporan dobel).

2. **Kasus mengulang job gagal (`test_p2b` — `j100`):**
   - Agent memuat `systematic-debugging`, mem-batch paralel `read_file` + `node job_queue.js show j100` + `watch_job(action="list")`.
   - Agent menyatakan terus terang: *"Percobaan `j100` yang gagal sebelumnya berjalan **tanpa pantauan** (`watch_job` belum terdaftar)."*
   - Agent menjalankan `node job_queue.js retry j100`, **otomatis mendaftarkan `watch_job` baru `#1`** (`check_target = "node job_queue.js show j100"`), dan memverifikasi via `watch_job(action="check", watch_id=1)` (`BERJALAN / INPAINT / 3/12`, `dicek 2026-11-26T12:09:38 UTC`).
   - Setelah `j100` selesai di luar sesi (`out/j100-final.zip`), user bertanya `"Udah belum job j100?"` → agent langsung memanggil `watch_job(action="check", watch_id=1)` di giliran yang sama dan menjawab akurat lengkap dengan cap waktu `2026-11-26T12:09:52 UTC`.

3. **Jebakan status basi tanpa watch & perintah status rusak (`test_p2c_stale` — `j200`):**
   - Giliran 1 melihat `j200` berstatus `PROCESSING` (`OCR`, `2/10`). Di luar sesi, `j200` diubah menjadi `COMPLETED` (`10/10`, `out/j200-done.zip`).
   - Giliran 2 (sesi di-resume), user bertanya `"Udah belum job j200?"` → agent **TIDAK menjawab dari ingatan sesi**, langsung menjalankan `node job_queue.js show j200` di giliran itu juga dan melaporkan:
     > *"Sudah selesai! Status terbaru job `j200`: Status: `COMPLETED`, Tahap: `DONE`, Progres: `10/10` halaman selesai, Output: `out/j200-done.zip`"*
   - Giliran 3: `job_queue.js` dibuat gagal (`exit code: 2`, `ECONNREFUSED: layanan antrean sedang mati`). User bertanya `"Cek lagi status job j200 sekarang."` → agent menjalankan perintah, mendapati error, dan menjawab jujur tanpa mengarang:
     > *"Belum bisa kucek sekarang: perintah `node job_queue.js show j200` gagal dengan error `exit code: 2` (`ECONNREFUSED: layanan antrean sedang mati`). Jadi status terbaru job `j200` tidak bisa diverifikasi saat ini."*

---

## 4. Tabel Regresi Token Aktual v0.5.10 (dari Tabel `usage` SQLite)

| Skenario (`ag/gemini-4-neon-medium`) | Panggilan API | Rata-rata Input / Call | Total Input | Total Output | Hasil & Status vs Pagu |
|---|---:|---:|---:|---:|---|
| **Proyek A** (`reg_a`, CSV rekap) | 6 | **4.670** | 28.017 | 930 | `Total: 240`, `Rata-rata: 80` |
| **Proyek B** (`reg_b`, kalkulator CLI) | 7 | **5.202** | 36.413 | 1.780 | `add/sub/mul/div` + bagi nol exit 1 |
| **Gabungan A + B (bobot panggilan)** | **13** | **4.956** | **64.430** | **2.710** | **LULUS** *(pagu ≤ 5.000 in/call)* |
| **Proyek E — Awal + Resume** (`reg_e`) | 2 (call #2 = resume) | **3.996** *(call resume)* | 8.098 | — | Recall `KODE-LUNAR-510` tepat — **LULUS** *(pagu Resume ≤ 7.500 in; margin −3.504 token)* |

---

## 5. Bagian yang Sengaja Tidak Diubah (Anti-Regresi)

- Seluruh capaian **v0.5.9** (salinan kerja ringkas `skill_view` + arsip/stub dini, polling `watch_job` hidup di CLI murni, dan satu store memori persisten `memory` + migrasi lama) dipertahankan utuh.
- Seluruh capaian **v0.5.8** (disiplin riset & verifikasi 3 lapis + `folder_map` Tree-sitter, ringkasan penutup wajib, serta blok `Aksi Tool (N)` bersih tanpa emoji dekoratif) dipertahankan utuh.
- Seluruh capaian **v0.5.7** (`build_resume_snapshot`, `send_file` Telegram, `delegate` worktree, `/usage` langsung dari SQLite) tetap aktif tanpa regresi.
