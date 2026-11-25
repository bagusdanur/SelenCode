# Hasil Implementasi SelenCode Skills System — v0.4.0

- **Versi:** `v0.4.0` (dari `v0.3.4`)
- **Tanggal:** 25 November 2026
- **Prinsip:** Playbook `SKILL.md` lintas-sesi berbasis Progressive Disclosure 3 tingkat milik SelenCode, tetap menjaga penuh sistem hemat token Tahap 0–6, Bagian A performa, spillover `run_shell`, plan mode, dan UX persetujuan yang sudah terverifikasi.

---

## 1. Perubahan per Tahap & File yang Disentuh

### Tahap 1 — Format & Penyimpanan
- **File:** `src/skills.rs`, `src/config.rs`, `.selencode/skills/dev/rilis-github/SKILL.md`, `.selencode/skills/dev/rilis-github/references/checklist-rilis.md`, `.selencode/skills/dev/tinjau-rust/SKILL.md`, `.selencode/skills/dev/tinjau-rust/references/checklist-rust.md`
- Satu skill = satu folder berisi `SKILL.md` wajib + subfolder referensi opsional (`references/`, `templates/`, `scripts/`).
- Parser frontmatter YAML (`parse_skill_frontmatter`): memvalidasi `name` wajib kebab-case unik, `description` wajib satu baris, serta opsional `version` dan `tags` (inline maupun daftar multiline).
- Dua lokasi dengan prioritas jelas:
  1. **Proyek** (menang bila nama sama): `<proyek>/.selencode/skills/[<kategori>/]<nama>/SKILL.md`
  2. **Pengguna**: `<data_dir>/selencode/skills/[<kategori>/]<nama>/SKILL.md` (menghormati `SELENCODE_DATA_DIR`, plus fallback baca lokasi lama bila ada).
- Menyertakan 2 skill bawaan siap pakai sejak hari pertama: **`rilis-github`** dan **`tinjau-rust`** (termasuk file referensi di `references/`), tersedia langsung di repo ini dan disemai otomatis ke `<data_dir>/selencode/skills/dev/` pada startup pertama lewat `seed_default_user_skills()`.
- Folder tidak valid (frontmatter rusak, `name` bukan kebab-case, duplikat nama dalam satu scope, atau folder kosong tanpa `SKILL.md`) dilewati dengan peringatan log, **tidak pernah menggagalkan startup**.

### Tahap 2 — Indeks di Snapshot Dinamis (Disiplin Token)
- **File:** `src/agent/context.rs`, `src/agent/loop.rs`, `src/config.rs`, `src/tools/mod.rs`, `src/app/session.rs`
- Memindahkan daftar skill **keluar dari wilayah statis `build_system_prompt()`** ke **snapshot dinamis awal sesi** di `run_turn_inner()`, dikelompokkan per `[kategori]` dengan deskripsi dipotong maks **60 karakter** (`format_skills_index`).
- Berbagi anggaran cap injeksi dinamis **1.500 token** (`tool_budget.dynamic_inject_max_tokens`), dan hanya dapat dinaikkan secara eksplisit lewat `[skills] index_max_tokens`.
- Menambahkan 1 baris aturan statis singkat di system prompt: periksa indeks skill sebelum mulai dan muat lewat `skill_view` bila cocok; setelah tugas multi-langkah yang polanya bisa berulang, tawarkan menyimpan langkahnya sebagai skill (hanya tulis lewat `skill_create`/`skill_edit` bila user setuju).
- Isi penuh `SKILL.md` **tidak pernah masuk konteks** sebelum dimuat lewat `skill_view`, `/skill`, atau config eksplisit `[skills] auto_load = [...]` (bawaan kosong).

### Tahap 3 — Tool Pemuat Progressive Disclosure 3 Tingkat
- **File:** `src/tools/skill.rs`, `src/tools/mod.rs`, `src/tools/toolsets.rs`, `src/agent/plan.rs`
- **`skills_list`** (read-only): mengembalikan indeks ringkas yang sama dengan snapshot prompt (berguna pasca-`/resume` atau kompresi).
- **`skill_view(name)`** (Tingkat 2, read-only): memuat isi penuh `SKILL.md` sebagai `ToolResult` biasa (ikut prune/spillover) + daftar nama file referensi yang tersedia tanpa langsung membaca isinya.
- **`skill_view(name, path)`** (Tingkat 3, read-only): memuat **1 file referensi** di dalam folder skill dengan validasi keamanan ketat (menolak path absolut, `..`, symlink escape di luar folder skill, dan file rahasia seperti `.env` / `*.key` / `id_rsa`, plus `redact_secrets`).
- Skrip di `scripts/` hanya dibaca sebagai teks, **tidak pernah dieksekusi otomatis** (eksekusi tetap via `run_shell` dengan persetujuan + sandbox).
- `skill_view` otomatis diaktifkan (toolset `skills_view`, ~65 token) hanya saat ada skill terpasang agar model bisa langsung memuat skill di langkah pertama tanpa menambah overhead bila belum ada skill; `skills_list` & `skill_view` juga diizinkan di Plan mode sementara tool tulis tetap diblokir.

### Tahap 4 — Pembuatan & Pengelolaan Skill dengan Persetujuan Wajib
- **File:** `src/tools/skill.rs`, `src/app/commands.rs`, `src/agent/loop.rs`
- **`skill_create(name, description, body, ...)`** & **`skill_edit(name, ...)`**: `needs_approval() = true`, selalu menghasilkan unified diff (`similar::TextDiff`) dan meminta persetujuan pengguna lewat `ctx.approver.ask(...)` (popup TUI, `y/n` mode polos, tombol inline Telegram). Konten selalu diredaksi dari secret sebelum ditulis.
- **`/skills`** menampilkan daftar skill lengkap (scope, kategori, versi, deskripsi ≤60 char) serta peringatan bila ada folder skill rusak; **`/skill <nama>`** memuat paksa skill ke percakapan sebagai pesan `Role::User` (tidak merusak prefix System Prompt yang dibekukan) sekaligus menampilkan daftar file referensinya.

### Tahap 5 — Pencocokan
- Pencocokan murni oleh model berdasarkan `description` di indeks (tanpa embedding/vektor, nol biaya tambahan).
- Tidak ada auto-load bawaan; dukungan opsional hanya aktif jika pengguna secara eksplisit mengisi `[skills] auto_load = [...]` di `selencode.toml`.

---

## 2. Hasil Pengujian & Bukti DB

### A. Tes Otomatis (`tests/skills_system.rs` + Seluruh Suite Lama pada CI Linux & Windows x64)
- `tahap1_frontmatter_parsing_valid_invalid_and_priority_project_over_user`: memverifikasi parsing YAML valid/rusak, validasi kebab-case, prioritas proyek > pengguna, warning tanpa crash untuk skill rusak/duplikat, serta validitas 2 skill bawaan (`rilis-github`, `tinjau-rust`).
- `tahap2_index_truncated_60_chars_and_grouped_by_category`: memverifikasi pemotongan deskripsi indeks ≤60 char dan pengelompokan kategori.
- `tahap3_and_tahap4_skill_view_security_and_approval_gated_create_edit`: memverifikasi Tingkat 2 & Tingkat 3, penolakan path traversal (`../`, path absolut) dan `.env`, serta verifikasi bahwa penolakan approval membatalkan penulisan `skill_create` sedangkan persetujuan menulis `SKILL.md` teredaksi + menampilkan unified diff.
- `tahap6_db_token_proof_10_skills_lazy_load_and_single_skill_view`: memasang **10 skill** masing-masing berisi body unik >3,5 KB, menjalankan giliran dengan pencatatan ke SQLite (`usage` + `call_token_metrics`):
  - Panggilan 1: **0 dari 10** isi penuh skill masuk konteks (hanya indeks ringkas); `usage` & `call_token_metrics` mencatat panggilan 1 (`1.200 in`).
  - Sesudah `skill_view("skill-03")`: panggilan 2 memuat **hanya `skill-03`** (9 skill lainnya tetap tidak masuk konteks); total tercatat di `usage`/`call_token_metrics` tepat 2 panggilan (`3.550 in`).
- `tahap6_end_to_end_create_skill_with_approval_then_new_session_follows_it`: Sesi 1 membuat skill `format-laporan-kpi` lewat `skill_create` + persetujuan `true` → keluar → Sesi 2 (riwayat kosong baru) melihat indeks `format-laporan-kpi` → memanggil `skill_view` → menghasilkan `laporan_kpi.txt` yang mengikuti aturan playbook (`=== LAPORAN KPI RESMI ===` & `VERIFIED-BY-SKILL`).

### B. Catatan Kejujuran Uji Live Provider (9Router `ag/gemini-3-flash`)
- Lingkungan build/pengembang ini tidak mengekspor API key live `SELENCODE_ROUTER_KEY` untuk panggilan provider nyata, sehingga **angka tabel `usage` dari panggilan live 9Router (skenario A/B/E dan skenario live buat-skill → sesi-baru)** belum dijalankan dari shell ini dan **tidak diklaim** di laporan ini.
- Seluruh mekanisme Tahap 1–5, bukti isolasi 10 skill di SQLite `usage`/`call_token_metrics`, skenario end-to-end 2 sesi dengan approval nyata, serta regresi seluruh test suite lama (`tests/fase*`, `tests/stage*`, `tests/hermes_full_and_perf.rs`, `tests/token_efficiency_hermes.rs`) diverifikasi hijau pada CI Linux x64 & Windows x64.
