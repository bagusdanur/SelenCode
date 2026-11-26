# Laporan Hasil Update SelenCode v0.5.8 — Disiplin Riset & Verifikasi (Hermes 3 Lapis + `folder_map`), Ringkasan Penutup Wajib + Checklist Coding, & Tampilan Aksi Tool

- **Versi rilis:** `v0.5.8`
- **Commit implementasi:** `68f19fe` (`feat(v0.5.8): Hermes 3-layer discipline + folder_map, guaranteed closing summary + SWE checklist, Hermes tool_display action block`)
- **Rilis publik GitHub:** https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.8
- **CI (`68f19fe`):** Run `#45790053260` — **100% HIJAU** (`Cargo Test & Visual Render (Linux)` + `Cargo Test & Check (Windows x64)`, seluruh 33 suite tes + Clippy lulus).
- **Release workflow (`v0.5.8`):** Run `#45791301158` — **100% HIJAU**, 3 biner publik terbit:
  - `selen-linux-x64`: `16.840.336` byte (16,06 MB)
  - `selen-termux-arm64`: `15.150.608` byte (14,45 MB)
  - `selen-windows-x64.exe`: `15.541.760` byte (14,82 MB)
- **Lingkungan uji penerimaan live:** Biner rilis `selen-windows-x64.exe` `0.5.8` (`selen --version` → `selen 0.5.8`), model `ag/gemini-4-neon-medium` via 9Router, DB & data terisolasi (`SELENCODE_DB` + `SELENCODE_DATA_DIR` temporer).

---

## Ringkasan Perubahan Per Komponen

| Area | Berkas Utama | Ringkasan |
|---|---|---|
| **Perbaikan 1 — Disiplin Riset & Verifikasi (Lapis A/A2/B/C)** | `src/agent/context.rs`, `src/agent/loop.rs`, `src/config.rs`, `assets/builtin_skills/dev/systematic-debugging/SKILL.md`, `src/skills.rs` | System prompt 3 lapis (grounding, batch paralel baca-saja tanpa penalti anti-spin, `--help` dulu untuk CLI asing, re-read target sesudah mutasi eksternal, guard file asing opencode, disiplin resume/requeue job gagal); harness `build_folder_map` via Tree-sitter berpagu ketat ~300 token (`[context] folder_map = true`, kosong pada folder kosong, tidak diinjeksi ulang saat resume); deskripsi indeks `systematic-debugging` dipertajam (`WAJIB untuk bug/error/job gagal/ulang: debug 4 fase`, 51 char). |
| **Perbaikan 2 — Ringkasan Penutup Wajib + SWE-agent Checklist** | `src/agent/loop.rs`, `src/agent/corrector.rs`, `src/tool_display.rs`, `src/app/telegram_runner.rs` | Jika giliran terpotong oleh `max_steps` atau stagnasi, loop melakukan 1 panggilan penutup (`tools = []`) dengan fallback deterministik `build_template_closing_summary`, dan selalu menyimpan pesan `Role::Assistant` terakhir di DB (tidak pernah berhenti di `Role::Tool` kosong). Dilengkapi safety net gateway Telegram dan `SelfCorrectState::completion_gate_with_checklist` (verifikasi pasca-edit terakhir, pemeriksaan berkas sampah/repro, penyebutan file diubah). |
| **Perbaikan 3 — Tampilan Aksi Tool ala Hermes** | `src/tool_display.rs`, `src/agent/loop.rs`, `src/telegram.rs`, `src/app/telegram_runner.rs`, `src/config.rs` | `prepare_tool_preview` + `format_tool_event` meredaksi rahasia terlebih dahulu, mengambil argumen utama tool, 1 baris, potong ekor di `[display] tool_preview_length` (default `40`). Gelembung progres live Telegram menampilkan `<code>tool</code> — <code>pratinjau</code>`; blok akhir `Aksi Tool (N)` menyimpan total `N`, meruntuhkan pengulangan sukses identik berurutan (`run_shell ×8 — ...`), dibatasi ≤10 baris dengan seluruh baris `GAGAL` (+ alasan singkat) selalu terjaga, nol emoji dekoratif. |
| **Tes otomatis baru** | `tests/v058_hermes_discipline.rs` | 18 tes baru mencakup seluruh skenario Perbaikan 1, 2, dan 3 (termasuk 31 aksi diruntuhkan ≤10 baris, fallback templat, checklist coding, `folder_map` pagu ~300 token & folder kosong, redaksi rahasia argumen). |

---

## Bukti Uji Penerimaan Live (Biner Rilis `selen 0.5.8`)

### Perbaikan 1 — Disiplin Riset, `folder_map`, & Verifikasi

#### 1A. Miniatur Antrean Job Gagal (`p1a-comic-queue` — `job-6c33`, halaman 1–4 selesai, halaman 5 gagal kualitas V5)
- **`folder_map` aktif di snapshot sesi awal:** `has_folder_map = true` (tercatat langsung di pesan `Role::System` DB).
- **Riset ter-batch SEBELUM aksi edit/eksekusi:**
  - Batch 1 (1 giliran assistant paralel — 6 panggilan baca-saja sekaligus):
    `["skill_view(systematic-debugging)", "read_file(README.md)", "read_file(job_queue.js)", "read_file(queue_worker.js)", "read_file(jobs/job-6c33/status.json)", "list_files"]`
  - Batch 2 (1 giliran assistant paralel — 3 panggilan baca-saja sekaligus):
    `["read_file(jobs/job-6c33/metadata/page-04/pipeline-status.json)", "read_file(jobs/job-6c33/metadata/page-05/pipeline-status.json)", "run_shell(node job_queue.js --help && node job_queue.js show job-6c33)"]`
- **Tindakan & pemakaian `requeue` pada job & folder YANG SAMA:**
  - Agent mengidentifikasi akar masalah (`V5_QUALITY_THRESHOLD = 0.99` menolak halaman 5 skor `0.91`, sekaligus menangani UTF-8 BOM pada JSON metadata halaman 1–4).
  - Agent mengedit `queue_worker.js` lalu menjalankan **`node job_queue.js requeue job-6c33`** (BUKAN `enqueue` baru).
- **Verifikasi keadaan luar (re-read sesudah aksi):**
  - Batch pasca-requeue: `["run_shell(node job_queue.js show job-6c33)", "list_files(jobs)"]`.
  - Self-correction coding checklist (Perbaikan 2 #4) berjalan otomatis: `node --check queue_worker.js && node --check job_queue.js` (`exit code 0`).
- **Hasil akhir di disk (terverifikasi langsung):**
  - Folder di `jobs/`: **`job-6c33`** (hanya 1 folder, tidak ada `job-new-*`).
  - `jobs/job-6c33/status.json`:
    ```json
    {
      "job_id": "job-6c33",
      "title": "Hold On",
      "status": "COMPLETED",
      "total_pages": 8,
      "reused": [1, 2, 3, 4],
      "processed": [5, 6, 7, 8]
    }
    ```
  - Halaman 1–4 **tidak diproses ulang** (`reused: [1,2,3,4]`), hanya halaman 5–8 yang diproses (`processed: [5,6,7,8]`).

#### 1B. CLI Asing (`p1b-cli-asing` — `weird_tool.js`)
- Batch 1: `["run_shell(node weird_tool.js --help)", "read_file(weird_tool.js)"]` (**cek `--help` dulu sebelum menebak flag**).
- Batch 2: `node weird_tool.js --op=mirror --dest=out_sync --confirm-yes` → langsung berhasil pada percobaan pertama (**0 `ARG_ERROR`**).
- Batch 3 (verifikasi keadaan luar): `["list_files(out_sync)", "read_file(out_sync/synced.txt)"]` → isi `MIRROR_OK`, `P1B_OUT_EXISTS = True`.

#### 1C. Kontrol Job Tanpa Mekanisme Resume (`p1c-no-resume` — `simple_runner.js`)
- Batch 1: `["skill_view(systematic-debugging)", "read_file(simple_runner.js)", "read_file(run_state.json)", "run_shell(node simple_runner.js --help)"]`.
- Jawaban agent secara eksplisit menyatakan hasil pemeriksaan: *"`simple_runner.js` tidak memiliki fitur `resume` maupun `requeue`. Satu-satunya perintah yang didukung adalah `node simple_runner.js run <job_id>`"*, lalu menjalankan ulang dari awal dengan alasan tertulis, dan membaca ulang `run_state.json` (`"status":"DONE","steps_run":[1,2,3]`).

#### 1D. Pagu `folder_map` (Lapis A2)
- Terverifikasi lewat `tests/v058_hermes_discipline.rs`:
  - `folder_map_stays_within_300_token_cap_and_extracts_symbols`: pada workspace besar berisi 50 berkas kode, `build_folder_map(&dir, 300)` mengekstrak simbol fungsi/struct Tree-sitter dan memotong berkas kurang relevan sehingga tetap di bawah pagu ~300 token (`(+N file lain)`).
  - `folder_map_is_empty_on_empty_dir`: folder kosong menghasilkan string kosong `""` (0 token).
  - Dapat dimatikan lewat `[context] folder_map = false` di `selencode.toml`.

---

### Perbaikan 2 — Ringkasan Penutup Wajib & Daftar Periksa Coding (SWE-agent)

#### 2A. Uji Live Paksa Tabrak `max_steps = 2` (`p2-max-steps`)
- Konfigurasi: `[agent] max_steps = 2`, 5 file (`doc_1.txt` s.d. `doc_5.txt`), perintah baca satu per satu.
- Transkrip aktual:
  ```text
  [tool] read_file — doc_1.txt
  [tool] read_file — doc_2.txt
  [selen] max_steps (2) tercapai — menyusun ringkasan penutup…
  **Ringkasan Penutup (Batas Langkah Tercapai)**

  1. **Yang sudah dikerjakan & hasilnya:**
     - Membaca `doc_1.txt`: `nilai=100`
     - Membaca `doc_2.txt`: `nilai=200`
     - Subtotal sementara: **300**

  2. **Yang sedang berjalan:**
     - Tidak ada job atau proses latar belakang yang sedang berjalan.

  3. **Langkah berikutnya / kendala:**
     - Eksekusi terhenti karena batas langkah (`max_steps = 2`) tercapai saat membaca berkas satu per satu.
     - Langkah selanjutnya yang tersisa: membaca `doc_3.txt`, `doc_4.txt`, dan `doc_5.txt`, menghitung total nilai keseluruhan, lalu menulis hasilnya ke `total.txt`.
  ```
- Bukti persistensi SQLite DB: `last_role = "assistant"` (bukan `Role::Tool` kosong).
- Bukti fallback deterministik saat provider gagal/kosong di panggilan penutup: terverifikasi lulus di `tests/v058_hermes_discipline.rs::max_steps_fallback_template_when_closing_call_fails`.
- Bukti giliran normal (tidak menabrak batas): tidak ada ringkasan ganda (`regresi-trivial`, `regresi-a`, `regresi-b`, `p1b-cli-asing`).
- Bukti daftar periksa coding ala SWE-agent (`review_on_submit`): terlihat langsung pada `p1a-comic-queue` dan `regresi-b` saat `[selen] ✦ Self-correction (1/1): memeriksa verifikasi & daftar periksa penutup…` memicu verifikasi akhir pasca-edit terakhir, plus uji unit `swe_coding_checklist_flags_unverified_edit_and_scratch_file`.

---

### Perbaikan 3 — Tampilan Aksi Tool ala Hermes & Redaksi Rahasia

#### 3A. Bukti Live Pratinjau Argumen Utama & Redaksi Rahasia (`p3-redact`)
- **Sebelum v0.5.8:** baris aksi polos hanya menampilkan nama tool (`[tool] run_shell` / `run_shell selesai`) atau potongan argumen mentah yang tidak konsisten.
- **Sesudah v0.5.8 (keluaran live aktual `p3-redact`):**
  ```text
  [tool] run_shell — echo test --token [REDACTED]
  ```
  Nilai rahasia pada `--token ...` **teredaksi sebelum ditampilkan** menjadi `[REDACTED]`.
- Format pratinjau konsisten untuk seluruh tool di keluaran live lain:
  - `[tool] skill_view — systematic-debugging`
  - `[tool] read_file — jobs/job-6c33/metadata/page-04/pipeline…` (terpotong rapi di batas 40 char)
  - `[tool] run_shell — node job_queue.js requeue job-6c33`

#### 3B. Contoh Sebelum vs Sesudah Blok Aksi Telegram (31 Aksi)
- **Sebelum v0.5.8:** 31 baris mentah `run_shell` berulang tanpa argumen yang jelas dan membengkakkan balon Telegram hingga >30 baris.
- **Sesudah v0.5.8 (terverifikasi otomatis di `tests/v058_hermes_discipline.rs::action_summary_collapses_31_actions_to_max_10_lines_and_keeps_all_failures`):**
  ```text
  Aksi Tool (31)
  - read_file — Cargo.toml
  - run_shell ×8 — node status.js
  - run_shell ×1 — node broken.js (GAGAL: exit code 1 — lint error)
  - run_shell ×8 — node worker.js
  - edit_file — src/main.rs (GAGAL: teks target tidak ditemukan)
  - … dan 12 aksi sukses lainnya
  ```
  - Total aksi sebenarnya **`(31)`** tetap utuh.
  - Pengulangan sukses identik berurutan diruntuhkan (`run_shell ×8 — ...`).
  - Seluruh baris `GAGAL` beserta alasan singkatnya **selalu dipertahankan**; baris sukses di tengah yang dikorbankan (`… dan N aksi sukses lainnya`) sehingga total daftar **≤10 baris**.
  - **Nol emoji dekoratif** (mematuhi keputusan v0.5.0).
  - Giliran kecil (≤3 aksi) tetap tampil lengkap tanpa pemaksaan peruntuhan.

---

## Tabel Regresi Token Aktual (dari Tabel `usage` SQLite — `folder_map = true`)

| Skenario | Panggilan API | Rata-rata Input / Call | Total Input | Total Output | Perilaku & Verifikasi |
|---|---:|---:|---:|---:|---|
| **Trivial** (`regresi-trivial`) | 1 | 3.793 | 3.793 | 39 | 0 tool, 0 riset, 0 skill — jawab langsung `"Ibu kota Prancis adalah Paris."` |
| **Proyek A** (`regresi-a`, CSV rekap) | 7 | **4.472** | 31.303 | 965 | `Total nilai: 240`, `Rata-rata nilai: 80`, verifikasi lulus |
| **Proyek B** (`regresi-b`, kalkulator CLI) | 6 | **4.703** | 28.216 | 1.432 | `add/sub/mul/div` + bagi 0 exit 1, verifikasi pasca-edit lulus |
| **Gabungan A + B (bobot panggilan)** | **13** | **4.578** | **59.519** | **2.397** | **LULUS (pagu ≤ 5.000 in/call; margin -422 token)** |
| **Proyek E — Awal + Resume** (`regresi-e`) | 3 (call #3 = resume) | **3.802** *(call resume)* | 11.609 | 307 | **LULUS (pagu Resume ≤ 7.500 in; margin -3.698 token)**, sandi `BULAN-PERAK-42` + proyek `SelenCode` akurat |

> **Catatan observasi token (konteks untuk v0.5.9):** Pada `p1a-comic-queue` (10 call) dan `p1c-no-resume` (4 call), `skill_view(systematic-debugging)` bekerja sesuai desain (terpanggil di batch pertama), namun isi skill ~14,6 rb karakter ikut terbawa pada panggilan-panggilan berikutnya (`est_tool_results_tokens` naik ke ~6.259). Hal ini tercatat jelas sebagai fokus perbaikan berikutnya di `v0.5.9`.

---

## Daftar Bagian yang Sengaja Tidak Diubah

- Jalur **resume ramping** v0.5.7 (`build_resume_snapshot`, `RESUME_CORE_TOOLS`, `select_resume_initial_toolsets`) — tidak diinjeksi ulang `folder_map` saat resume.
- **Tool `send_file`** (lampiran Telegram) dan **`delegate` `mode="work"`** (subagent worktree terisolasi).
- **Perintah `/usage`** (laporan langsung dari tabel `usage` SQLite) dan **`selen skills install`** (staging + provenance).
- **Background Self-Improvement Review** v0.5.1 (model aux + staging `/review`).
- **Mesin prune tahap-1 & spillover** v0.5.0/v0.5.6, **Cron scheduler**, **FTS5 `session_search`**, **Router fallback non-sticky**, **klarifikasi `ask_user`**, dan **format balon Telegram tanpa emoji dekoratif**.
