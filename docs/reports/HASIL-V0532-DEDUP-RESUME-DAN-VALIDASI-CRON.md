# Laporan Rilis SelenCode v0.5.32 — Dedup Baca-Ulang `read_file` Lintas Resume + Validasi Metakarakter Cron di Pintu Masuk

- **Versi rilis:** `v0.5.32`
- **Jumlah butir:** 2
- **Rujukan riset §0 (Hermes Agent, `NousResearch/hermes-agent`):**
  - `tools/file_tools.py:528-558` (`_dedup_stub_or_block`): mengembalikan stub ringkas `{"status": "unchanged", "dedup": true, "content_returned": false, ...}` pada pembacaan ulang identik pertama, lalu eskalasi penolakan keras `BLOCKED:` pada pengulangan `>= 2`.
  - `tools/file_tools.py:561-598` & `725-734` (`_record_successful_read`, titik dedup kunci `(path, offset, limit)`).
  - `tools/file_tools_read_tracking.py:205-266` (`_dedup_lookup`, `_note_read_served`, `reset_file_dedup`): generasi bacaan pasca-compaction agar bacaan pemulihan pertama sesudah konteks dipangkas tetap mengembalikan isi penuh.
  - `tools/file_tools_read_tracking.py:344-398` (`_file_version`, `_mark_full_write_baseline`, `_known_full_content_sha256`): sidik berkas berbasis SHA-256 byte aktual (tidak hanya `mtime` yang bisa dipertahankan editor).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Memasang pelacak dedup pembacaan berkas deterministik berbasis sidik SHA-256 (`ReadFileTracker` di `src/tools/read_tracker.rs` terpasang di `Ctx` & `ReadFile::run`) yang mengembalikan stub ringkas `{"status":"unchanged","dedup":true,"content_returned":false,...}` pada pengulangan pertama dan eskalasi `BLOCKED:` pada pengulangan `>= 2`, bertahan lintas `/resume` melalui dua lapis (tabel SQLite `session_read_files` di `src/store.rs` + rekonstruksi fallback dari riwayat `messages`), otomatis mendeteksi perubahan berkas di disk (kontrol kesegaran lewat perbandingan SHA-256 + invalidasi pada `WriteFile`/`EditFile`), mendukung reset generasi pasca-compaction/pruning, dan dilengkapi sinyal status berkas pada `build_resume_snapshot_with_read_status` serta panduan `build_resume_system_prompt`.
- Butir 2 — LULUS: Memasang validasi pintu masuk metakarakter shell cron (`validate_cron_shell_command` & `validate_cron_allowlist` di `src/cron.rs`) yang menolak `;`, `&`, `|`, `` ` ``, `$(`, `${`, `>`, `<`, newline (`\n`/`\r`), dan NUL (`\0`) secara all-or-nothing (tanpa tersimpan sebagian) pada CLI `selen cron add`, tool `manage_cron`, lapisan penyimpanan `Store::add_cron_job`, serta validasi store-level `Store::add_job_watch`, sambil mempertahankan pertahanan lapisan kedua sisi-eksekusi (`is_shell_command_allowlisted`) dan menjaga perintah polos seperti `echo ok` tetap diterima.

---

## Ringkasan Desain & Cakupan

### Masalah 1 — Mekanisme Dedup `read_file` & Kelangsungan Lintas Resume

- **Mekanisme yang dipilih:** `ReadFileTracker` (`src/tools/read_tracker.rs`) yang dimiliki langsung oleh `Ctx` (`Arc<Mutex<ReadFileTracker>>`) dan dijalankan di dalam `ReadFile::run` (`src/tools/file.rs`).
- **Kunci & sidik isi:**
  - Kunci dedup: `(canonical_path, start, end)` (membedakan bacaan penuh dan bacaan rentang baris spesifik, meniru `(path, offset, limit)` Hermes).
  - Sidik utama: `content_sha256` (SHA-256 murni Rust FIPS 180-4 via `crate::update::sha256_hex` atas byte mentah berkas di disk) ditambah metadata `mtime_ns` & `file_size`.
  - Sidik fallback: `rendered_sha256` atas teks keluaran `read_file` ketika pelacak direkonstruksi dari riwayat `messages` sesi lama pra-v0.5.32 atau jalur tanpa SQLite.
  - Eskalasi: hit ke-1 -> stub JSON `"status":"unchanged"`; hit ke-2+ -> penolakan keras `BLOCKED:`. Tidak pernah mengembalikan isi penuh selama sidik cocok pada generasi konteks aktif.
  - Kesegaran: setiap perbedaan SHA-256 (bahkan bila panjang byte sama) langsung menginvalidasi entri lama, menjalankan bacaan segar penuh, dan memperbarui sidik di memori + SQLite. `WriteFile` dan `EditFile` juga langsung menginvalidasi path yang diubah.
  - Pasca-compaction/pruning: `reset_generation()` di `src/agent/loop.rs` mengizinkan 1 kali bacaan pemulihan penuh setelah konteks dipangkas.
- **Cara tracker selamat dari resume:**
  1. Persistensi SQLite pada tabel `session_read_files` (`upsert_session_read_file`, `load_session_read_files`, `delete_session_read_file_path`, dibersihkan saat `delete_session`).
  2. Rekonstruksi fallback dari `messages` (`hydrate_from_messages`) di awal `run_turn_inner` yang menelusuri kronologi `read_file` / `write_file` / `edit_file` dan melewati hasil yang sudah di-prune.
  3. Pemeriksaan kesegaran awal giliran resume (`verify_freshness_for_resume`) yang langsung menginvalidasi berkas yang berubah di disk dan memberi status `Unchanged` vs `Modified` ke `build_resume_snapshot_with_read_status` (`src/agent/context.rs`).
- **Cakupan jalur & batas jujur:**
  - **Terjangkau:** seluruh jalur sesi yang melalui `ReadFile` dan `run_turn_inner` — TUI, Plain REPL, headless (`selen -p` / `selen run`), Telegram gateway, serta subagent (`Ctx` terisolasi per subagent).
  - **Tidak dijangkau:** pembacaan isi berkas yang dilakukan lewat `run_shell` (mis. `cat`/`head`) dan pembacaan multimodal gambar/PDF pada `read_file` tidak masuk tabel dedup teks `session_read_files`.

### Masalah 2 — Daftar Permukaan Masuk Cron & Watch yang Tervalidasi

- **Permukaan masuk yang kini tervalidasi di pintu masuk (menolak `;`, `&`, `|`, `` ` ``, `$(`, `${`, `>`, `<`, `\n`, `\r`, `\0` sebelum simpan / approval):**
  1. **CLI `selen cron add --allow-shell ...`** (`src/main.rs`) — memanggil `validate_cron_allowlist` sebelum `Store::add_cron_job`; keluar dengan kode non-zero & pesan jelas; 0 baris tersimpan saat ditolak.
  2. **Tool agent `manage_cron(action="add", allowed_shell=[...])`** (`src/tools/cron.rs`) — memanggil `validate_cron_allowlist` sebelum `ctx.approver.ask` dan sebelum `Store::add_cron_job`.
  3. **Lapisan penyimpanan `Store::add_cron_job`** (`src/store.rs`) — memanggil `validate_cron_allowlist` sebelum `INSERT INTO cron_jobs` sebagai gerbang sentral all-or-nothing.
  4. **Perintah sesi `/cron add ...`** (`src/app/commands.rs`, dipakai bersama oleh TUI, Plain REPL, dan Telegram `/cron add` di `src/app/telegram_runner.rs`) — hanya membuat job baca-saja (`allowed_shell = []`) dan tetap melewati `Store::add_cron_job`.
  5. **Tool `watch_job(action="register", check_command=...)`** (`src/tools/watch.rs`) & **`Store::add_job_watch`** (`src/store.rs`) — memvalidasi perintah shell lewat `validate_readonly_watch_command` sebelum approval dan sebelum `INSERT INTO job_watches`.
  6. **Pertahanan lapisan kedua sisi-eksekusi** (`src/cron.rs::is_shell_command_allowlisted` & `AllowlistedShellTool`) — tetap dipertahankan utuh tanpa dihapus.
- **Permukaan yang tidak membuat entri cron/shell (tidak memerlukan form input `allowed_shell`):**
  - Tombol inline Telegram `/cron` (`crn:t`, `crn:d`, `crn:x` di `src/app/telegram_ui.rs`) hanya melakukan toggle aktif/jeda dan hapus job yang sudah ada.
  - CLI `selen watch` & perintah sesi `/watch` hanya menyediakan `list` dan `cancel <id>`.
  - Modifikasi manual berkas SQLite secara langsung di luar biner `selen` (tetap ditahan oleh pertahanan lapisan kedua `is_shell_command_allowlisted` saat eksekusi).

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.31) | Sesudah (v0.5.32) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 64 suite (294 tes lulus) | 65 suite (301 tes lulus, 0 gagal) | LULUS |
| Unit & integration test CI Windows x64 (`cargo test`) | 54 suite (265 tes lulus) | 55 suite (lulus, 0 gagal) | LULUS |
| Uji integrasi khusus v0.5.32 (`v0532_read_dedup_and_cron_entry`) | 0 tes | 7 tes (7 lulus, 0 gagal; mencakup CLI binary `selen cron add`) | LULUS |
| Resume E live (token giliran resume ≤ 7.500 & 0 baca-ulang) | 8.544 token (baca ulang) | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Kontrol kesegaran live (berkas diubah di antara giliran) | belum diuji | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |
| Regresi A+B live (rata-rata ≤ 5.000 token/panggilan) | baseline v0.5.31 | pengukuran penerimaan oleh Muse | Menunggu uji live Muse |

- Tidak diukur — Resume E live (token giliran resume ≤ 7.500 & 0 baca-ulang): pengukuran penerimaan oleh Muse (sesuai pembagian kerja rilis, uji live tidak dijalankan oleh pengembang).
- Tidak diukur — Kontrol kesegaran live (berkas diubah di antara giliran): pengukuran penerimaan oleh Muse.
- Tidak diukur — Regresi A+B live (rata-rata ≤ 5.000 token/panggilan): pengukuran penerimaan oleh Muse.

---

## Tidak Diubah

- Sistem review / Selena improvement (`src/agent/review.rs`), gerbang verifikasi (`src/tools/verify.rs`), dan perilaku orchestrator (`src/agent/orchestrator.rs`) tidak diubah.
- Komposisi Core Toolset (6 tool), Resume Core Toolset (5 tool), serta system prompt sesi baru (`build_system_prompt`) tidak diubah sehingga anggaran token Regresi A+B tetap terjaga.
- Pertahanan sisi-eksekusi cron (`is_shell_command_allowlisted`), otorisasi Telegram, verifikasi SHA-256 self-update, SSRF `web.rs`, isolasi `bwrap`, serta fitur v0.5.28–v0.5.31 di luar cakupan brief tidak disentuh.
