# Hasil Update SelenCode v0.5.0 — Pengiriman Telegram, PATH Sesi, & Perilaku Menulis Skill

- **Versi:** `v0.5.0` (dari `v0.4.0`)
- **Rilis Publik:** `https://github.com/bagusdanur/SelenCode/releases/tag/v0.5.0` (`draft=false`, 3 aset biner: `selen-linux-x64` 14.82 MB, `selen-termux-arm64` 13.38 MB, `selen-windows-x64.exe` 13.6 MB)
- **Tanggal:** 26 November 2026
- **Fokus:** Empat perbaikan terarah untuk gateway Telegram, lingkungan `PATH` pada `run_shell`, disiplin penulisan nyata pada perintah pembuatan skill, dan keterbacaan pesan Telegram yang hemat emoji — tanpa mengubah mekanisme hemat token Tahap 0–6, Skills System Tahap 1–6, maupun desain TUI Moonlit.

---

## 1. Perubahan per Perbaikan & File yang Disentuh

### Perbaikan 1 — Pengiriman Telegram: Hasil Akhir Selalu Pesan Baru + Jeda Approval
- **File:** `src/app/telegram_runner.rs`, `src/telegram.rs`
- **Jawaban akhir selalu pesan baru (`sendMessage`):** menghapus seluruh jalur `edit_html_message` pada `progress_msg_id` saat giliran selesai. Baik giliran dengan tool, tanpa tool, dengan approval, maupun jawaban panjang (`>4050` char / multi-chunk), semuanya dikirim sebagai pesan baru di bagian paling bawah chat sehingga urutan kronologis selalu benar dan memicu notifikasi Telegram (termasuk giliran panjang ≥30 detik tanpa pesan ganda).
- **Penanganan gelembung progres sesudah selesai:** sesudah pesan jawaban baru terkirim, gateway mencoba `delete_message_checked(chat_id, mid)`; bila penghapusan gagal, gelembung di-edit menjadi **1 baris status singkat** (`Selesai — N aksi tool · X dtk` atau `Selesai — X dtk`) tanpa isi jawaban.
- **Jeda progres saat approval diminta:** melacak `active_progress` per `chat_id` dan `TgApprovalHub::has_pending_for_chat(chat_id)`. Saat `approval_rx` menerima permintaan persetujuan, gelembung progres di-edit sekali menjadi `"Menunggu persetujuan — lihat pesan di bawah."` dan collector berhenti mengeditnya selama menunggu tombol ditekan; sesudah keputusan diterima, progres otomatis berlanjut.
- **`/cancel` & interupsi:** gelembung progres dibersihkan/diperbarui tanpa meninggalkan pesan jawaban yatim.

### Perbaikan 2 — `PATH` Sesi `run_shell` Setara Shell Pengguna
- **File:** `src/sandbox.rs`
- Menambahkan `enriched_shell_path(current_path, home_dir, termux_prefix)` yang mempertahankan urutan `PATH` proses saat ini lalu menambahkan (tanpa duplikat) `~/.local/bin`, `~/bin`, `$PREFIX/bin` (Termux), dan `/usr/local/bin` (Unix).
- Dipasang langsung di `SandboxController::prepare_shell_command(...)` sehingga berlaku seragam untuk CLI, gateway Telegram (`selen telegram`, termasuk bila berjalan sebagai service `systemd`), dan `verify_project`.
- Penyaringan/scrubbing env sensitif (`scrub_env`) tetap berlaku penuh tanpa perubahan.

### Perbaikan 3 — Perintah "Buat Skill" Wajib Berujung Tulisan Nyata + Diff Ringkas
- **File:** `src/tools/skill.rs`, `src/agent/context.rs`, `src/tools/toolsets.rs`
- Memperluas tool `skill_edit` dengan argumen opsional `ref_path` (wajib relatif di bawah `references/`, `templates/`, atau `scripts/`, menolak `..`, path absolut, dan file rahasia) agar agen dapat menambah/memperbarui file referensi (termasuk catatan audit) di dalam folder skill yang sudah ada — **tetap lewat persetujuan pengguna + unified diff + redaksi secret**, serta mengembalikan unified diff pada output tool.
- Memperbarui aturan statis #8 di `build_system_prompt`: bila pengguna **eksplisit** meminta membuat/menulis skill dan skill sejenis sudah ada, agen wajib mengaudit lalu mengakhiri dengan tulisan nyata (`skill_edit` pada `SKILL.md` atau `ref_path="references/..."`) dan melaporkan diff ringkasnya — tidak boleh berhenti di ringkasan "sudah lengkap". Perintah biasa (kontrol) tetap tidak pernah menulis skill tanpa diminta.

### Perbaikan 4 — Tampilan Pesan Telegram Rapi & Hemat Emoji
- **File:** `src/telegram.rs`, `src/app/telegram_runner.rs`
- **Anggaran emoji ≤1 per pesan:** menghapus seluruh emoji dekoratif (`🌙`, `🔐`, `✅`, `❌`, `⚡`, `🔧`, `📌`, `💡`, `✨`, `💬`, `🛑`, `🔒`) dari gelembung progres, tombol approval, blok Aksi Tool, markdown heading, kartu menu, `/start`, `/new`, `/resume`, `/id`, `/cancel`, dan pesan error. Hanya kartu approval memakai 1 penanda `⚠️` di judul.
- **Gelembung progres polos:** `Sedang bekerja — langkah 3/25 · run_shell` dan `Mengetik...` dalam `<blockquote>` seperlunya.
- **Kartu approval utuh tanpa potong diam-diam (`format_approval_card_html`):** judul `⚠️ Perlu persetujuan`, perintah ditampilkan **utuh** dalam `<pre><code>` (tidak lagi dipotong di 1.500 char; bila melebihi batas pesan Telegram, seluruh perintah/detail utuh dikirim dulu sebagai pesan lanjutan bernomor), baris `Alasan:`, tombol polos (`Setuju`, `Tolak`, `Selalu untuk <kategori> — sesi ini`), dan baris batas waktu (`Batas waktu: 5 menit.`).
- **Blok Aksi Tool:** `<blockquote expandable><b>Aksi Tool (N)</b>` dengan baris ringkas `nama_tool — ringkasan` (atau `nama_tool — gagal: ringkasan` saat gagal), tanpa ikon `✓`/`✗`.

---

## 2. Daftar yang Sengaja Tidak Diubah
- Sistem hemat token Tahap 0–6 (prune + arsip, dedup/loop guard, rem Self-correction, pembekuan system prompt statis, spillover `read_file`/`run_shell`, kompresi dua tahap, indeks MCP >15, cap dinamis 1.500 token).
- Skills System Tahap 1–6 (format folder `SKILL.md` YAML, prioritas proyek > pengguna, `skill_view` 3 tingkat, `skill_create` + persetujuan, 2 skill bawaan `rilis-github` & `tinjau-rust`).
- Desain TUI Moonlit di terminal, Plan mode, approval UX TUI/CLI, sesi/checkpoint/rollback, `delegate`/`orchestrate`, `selen update`, kontrak trait `Provider`/`Tool`, redaksi rahasia, sandbox mode & env scrubbing, default provider/model (`9Router`, `ag/gemini-3-flash`).

---

## 3. Bukti Uji Penerimaan & Regresi

### A. Tes Otomatis (`tests/v050_telegram_path_skills.rs` + Seluruh Suite di CI Linux & Windows x64)
- `perbaikan1_and_4_telegram_clean_style_emoji_budget_and_full_approval_command`: memverifikasi 0 emoji dekoratif pada progres/menu/markdown, maks 1 `⚠️` pada kartu approval, perintah 2.500 char tampil 100% utuh, perintah >5.000 char dipecah ke pesan lanjutan utuh tanpa kehilangan karakter, dan status `TgApprovalHub::has_pending_for_chat`.
- `perbaikan2_enriched_shell_path_adds_user_bins_without_duplicates` & `perbaikan2_run_shell_finds_executable_in_custom_path_and_scrubs_secrets`: memverifikasi penambahan `~/.local/bin`, `~/bin`, `$PREFIX/bin` tanpa duplikat serta eksekusi biner dummy dengan nama pendek.
- `perbaikan3_skill_edit_ref_path_writes_audit_reference_with_approval_and_diff`: memverifikasi penolakan path traversal & `.env`, persetujuan wajib + diff unified saat menulis `references/audit-rawdl.md`, serta pembacaan balik lewat `skill_view` Tingkat 3.

### B. Uji Live Biner Rilis `v0.5.0` (`selen-windows-x64.exe` pada 9Router `ag/gemini-4-neon-medium`)

Seluruh angka token di bawah diambil langsung dari tabel SQLite **`usage`** (dan diverifikasi 1:1 terhadap `call_token_metrics`), bukan dari estimator. Sesi uji sementara dibersihkan dari database sesudah pengukuran.

| Skenario | Calls (`usage` / `metrics`) | Avg Tools | Input Token per Panggilan (`usage`) | Rata-rata In/Call | Total In / Out | Status |
| :--- | :---: | :---: | :--- | :---: | :---: | :--- |
| **Perbaikan 3 — Buat skill (existing `raw-downloader`)** | `3 / 3` | `20` | `5.729 → 6.123 → 7.692` | `6.515` | `19.544 / 1.688` | **LULUS** — `has_full_skill_in_initial_system_prompt=false`, panggil `skill_view` + `skill_edit`, ubah `SKILL.md` (`253 B → 968 B`, hash berubah) + tampilkan hasil audit & diff ringkas |
| **Perbaikan 3 — Kontrol (tugas biasa)** | `2 / 2` | `8` | `3.804 → 4.140` | `3.972` | `7.944 / 524` | **LULUS** — 0 pemanggilan `skill_view`/`skill_edit`/`skill_create`, 0 perubahan file skill (`HASH_UNCHANGED=True`) |
| **Regresi Proyek A (`rekap.js`)** | `6 / 6` | `8` | `3.611 → 3.770 → 4.294 → 4.378 → 4.514 → 4.654` | `4.204` | `25.221 / 861` | **LULUS** (≤ 5.000) |
| **Regresi Proyek B (`hitung.js`)** | `7 / 7` | `8` | `3.624 → 3.732 → 4.294 → 4.496 → 4.604 → 4.892 → 5.070` | `4.387` | `30.712 / 1.447` | **LULUS** (≤ 5.000) |
| **Gabungan Regresi A + B** | `13 / 13` | `8` | 13 panggilan (`55.933` total in) | **`4.303`** | `55.933 / 2.308` | **LULUS** (target ≤ 5.000 in/call) |
| **Regresi Proyek E + Resume (`kata.txt`)** | `5 / 5` | `8` | Turn 1: `3.593 → 3.779` · **Resume Turn 2:** `3.852 → 3.972 → 4.091` | **`3.857`** | `19.287 / 466` | **LULUS** (panggilan resume `3.852–4.091` in, jauh di bawah batas ≤ 7.500; `kata.txt` berisi 5 buah + `TOTAL: 5`) |

---

## 4. Contoh Format Pesan Telegram Baru (`v0.5.0`)

1. **Gelembung progres sementara (0 emoji):**
   - Sedang berjalan: `Sedang bekerja — langkah 2/25 · run_shell`
   - Saat menunggu approval: `Menunggu persetujuan — lihat pesan di bawah.`
   - Fallback bila hapus gagal sesudah jawaban baru terkirim: `Selesai — 3 aksi tool · 14 dtk`
2. **Kartu approval (maks 1 emoji `⚠️`, perintah utuh tanpa potong, tombol polos):**
   - Judul: `⚠️ Perlu persetujuan`
   - Isi: perintah lengkap dalam `<pre><code>...</code></pre>` (bila >3.800 char HTML, seluruh perintah utuh dikirim lebih dulu sebagai pesan lanjutan bernomor) + `Alasan: Menjalankan perintah shell di dalam ruang kerja.` + `Batas waktu: 5 menit.`
   - Tombol: `[Setuju]` `[Tolak]` / `[Selalu untuk run_shell — sesi ini]`
3. **Pesan jawaban akhir (selalu `sendMessage` baru di posisi paling bawah chat, 0 emoji dekoratif):**
   - Blok lipat opsional:
     `<blockquote expandable><b>Aksi Tool (2)</b>`
     `run_shell — rawdl sources`
     `skill_edit — .selencode/skills/ops/raw-downloader/SKILL.md`
     `</blockquote>`
   - Diikuti isi jawaban utama yang rapi.

---

## 5. Catatan Jujur Ruang Lingkup Uji
- **Terverifikasi penuh secara otomatis di CI Linux & Windows x64 (`4d3f56c`) + workflow rilis `v0.5.0`:** seluruh tes baru (`tests/v050_telegram_path_skills.rs`), tes Telegram & sesi (`tests/telegram_doctor_sessions.rs`), tes Skills System (`tests/skills_system.rs`), serta seluruh suite existing lulus 100%.
- **Terverifikasi live pada biner rilis `v0.5.0` + 9Router:** Perbaikan 3 (existing skill write + diff ringkas, kontrol tanpa tulisan), lazy loading skill, serta regresi token A/B/E + resume pada tabel `usage`.
- **Menunggu verifikasi pemilik di lingkungan produksi:** uji chat Telegram live (notifikasi pesan baru, jeda progres saat approval, urutan kronologis) dan `rawdl sources` di box pemilik belum dijalankan dari mesin pengembang karena token bot Telegram & instalasi `rawdl` hanya ada di perangkat pemilik. Pemilik dapat langsung menjalankan `selen update` ke `v0.5.0` untuk menguji kedua poin tersebut secara langsung.
