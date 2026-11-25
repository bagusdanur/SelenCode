# Hasil Implementasi Paket Hemat Token ala Hermes (Tahap 0–6) + Bagian A (Ringan di Laptop Kentang)

- **Versi:** `v0.3.2` (dari baseline `v0.3.1`)
- **Tanggal:** 25 November 2026
- **Prinsip:** Pangkas tanpa LLM dulu, bekukan prefix system prompt untuk prompt caching, arsipkan hasil tool besar ke file, dedup panggilan identik, dan jadikan TUI event-driven agar ringan di laptop spek rendah tanpa mengubah desain Moonlit.

---

## 1. Ringkasan Implementasi per Tahap

### Tahap 0 — Instrumentasi Token per Panggilan API
- **File:** `src/store.rs`, `src/agent/loop.rs`, `src/app/commands.rs`
- Menambahkan tabel SQLite `call_token_metrics` untuk mencatat setiap panggilan provider: estimasi `system`, `tools`, `history`, `tool_results`, token aktual `input`/`output`, jumlah tool aktif, jumlah hasil yang di-prune, serta jumlah panggilan yang di-dedup.
- Menambahkan `Store::session_token_metrics()` dan menampilkannya di `/resume info`.

### Tahap 1 — Prune Hasil Tool Lama (Tanpa LLM) + Arsip ke File
- **File:** `src/agent/context.rs`, `src/agent/loop.rs`
- Menambahkan `prune_old_tool_results_with_archive(messages, archive_dir, 20)` sebelum setiap panggilan provider:
  - Hanya menyentuh `Block::ToolResult` di luar **20 pesan terakhir** dengan panjang `> 200` karakter.
  - Menulis isi utuh teredaksi rahasia ke `<data_dir>/selencode/pruned/<session_id>/<idx>_<tool>_<id>.txt`.
  - Mengganti isi di konteks menjadi stub 1 baris: `[Hasil <nama_tool> diringkas — arsip: <path>] <cuplikan maks 120 char>` (100% idempoten).
  - Melindungi pesan User, teks Assistant, dan 20 pesan terakhir.

### Tahap 2 — Dedup + Loop Guard + Rem Self-Correction
- **File:** `src/agent/corrector.rs`, `src/config.rs`, `src/agent/loop.rs`, `src/tools/mod.rs`
- Menambahkan `canonicalize_json()` & `canonical_tool_fingerprint()` (hash FNV-1a atas nama tool + JSON kunci terurut).
- Menambahkan `ToolDedupCache`:
  - `read_file`: melacak `(mtime, byte_len)` file; bila belum berubah, kembalikan stub dedup tanpa baca ulang; bila berubah, baca ulang beneran.
  - `run_shell` & `verify_project`: dedup perintah identik yang baru saja sukses pada giliran yang sama; otomatis diinvalidasi saat `write_file`/`edit_file`/`lsp_rename` berjalan.
- `LoopGuard`: peringatan ditempel ke `ToolResult` (tidak menyentuh system prompt) setelah 2× kegagalan identik atau 3× kegagalan tool sama; stagnasi beruntun langsung menghentikan giliran agar tidak membakar token.
- Rem Self-correction: dikendalikan lewat `[guard]` (`enabled = true`, `max_self_correct_rounds = 1`) dan dilewati bila verifikasi identik sudah lulus.

### Tahap 3 — Bekukan System Prompt + Prompt Caching
- **File:** `src/agent/context.rs`, `src/providers/anthropic.rs`, `src/agent/loop.rs`
- Prefix statis System Prompt dibekukan byte-identik di awal sesi (tanpa timestamp/ID acak di wilayah prefix); blok dinamis diletakkan di bagian akhir sebagai snapshot awal sesi (dibatasi cap Tahap 6).
- Adapter Anthropic: menambahkan `apply_prompt_caching()` pola Hermes `system_and_3` (`cache_control: {"type": "ephemeral"}` pada blok `system`, skema tool terakhir, dan hingga 3 pesan non-system terakhir).
- Ringkasan kompresi disisipkan sebagai pesan `User` agar prefix `System` tidak pernah berubah di tengah sesi.

### Tahap 4 — Anggaran Hasil Tool (Cap 40/60, Spillover, Budget Per Turn)
- **File:** `src/config.rs`, `src/agent/context.rs`, `src/agent/loop.rs`
- Menambahkan `[tool_budget]` (`shell_cap_chars = 12_000`, `spillover_chars = 50_000`, `turn_budget_chars = 60_000`, `dynamic_inject_max_tokens = 1_500`).
- `head_tail_truncate()`: khusus `run_shell`, mempertahankan **40% awal + 60% akhir** agar pesan error di ujung log tidak terpotong.
- `spillover_if_needed()`: hasil tunggal `> 50.000` char atau agregat per turn melebihi budget (proporsional terhadap kapasitas konteks) ditulis utuh ke `<data_dir>/selencode/spillover/<session_id>/...` dan konteks hanya menerima pratinjau kepala/ekor + path file.

### Tahap 5 — Kompresi Dua Tahap ala Hermes
- **File:** `src/agent/context.rs`, `src/config.rs`, `src/agent/loop.rs`
- Pemicu diturunkan ke **50%** budget konteks; selalu menjalankan Tahap 1 prune dulu sebelum meringkas.
- Melindungi **3 pesan pertama + 20 pesan terakhir** dengan sanitasi batas pasangan `ToolUse`/`ToolResult`.
- Format ringkasan menggunakan template tetap Hermes (`Goal`, `Constraints & Preferences`, `Progress`, `Key Decisions`, `Relevant Files`, `Next Steps`, `Critical Context`), anggaran 20% (`2.000..=12.000` token), dan memperbarui ringkasan lama secara inkremental.
- Menambahkan `[aux] compression_model` dan `AuxCooldownState` (cooldown bertingkat `60 → 300 → 900` detik).

### Tahap 6 — Indeks On-Demand MCP (>15 Tool) + Cap Injeksi Dinamis
- **File:** `src/tools/toolsets.rs`, `src/agent/context.rs`, `src/agent/loop.rs`
- Bila server MCP mendaftarkan `> 15` tool, hanya indeks 1 baris (nama + deskripsi maks 60 char) dan meta-tool `load_mcp_tools` yang dikirim; skema penuh dimuat on-demand saat model memintanya.
- Seluruh blok injeksi dinamis awal sesi dibatasi oleh `cap_dynamic_injection(..., 1_500)` (~6.000 char) sebagai snapshot awal sesi.

---

## 2. Bagian A — Ringan di Laptop Kentang (Performa TUI & Build Rilis)

- **File:** `Cargo.toml`, `src/tui/app.rs`, `src/tui/mod.rs`, `src/app/tui_runner.rs`, `src/agent/ui.rs`
- **Event-Driven Dirty-Flag Loop (`src/tui/mod.rs`):** mengganti redraw paksa 20 fps tanpa henti menjadi render **hanya saat state berubah (`dirty = true`)**. Saat idle: poll 200ms dengan **0 redraw/detik (~0% CPU)**. Saat sibuk/streaming: coalesce seluruh token antre dalam 1 frame (maks ~12 fps streaming / ~6 fps spinner), sementara ketikan keyboard/Setup Wizard langsung redraw instan.
- **Per-Message Render Cache + Viewport Slicing (`src/tui/app.rs`, `src/tui/mod.rs`):** setiap `TranscriptLine` menyimpan cache layout Markdown/wrapping (`cached_lines`, `cached_wrapped_rows`) sehingga ketikan/scroll pada 150+ pesan tidak mem-parse ulang Markdown seluruh riwayat, dan `Paragraph` hanya menerima potongan baris di viewport aktif.
- **Non-Blocking Startup (`src/app/tui_runner.rs`):** pemindaian `@file` workspace (1.500 file) dipindahkan ke `tokio::task::spawn_blocking` di latar belakang dan dikirim via `UiEvent::WorkspaceFilesLoaded`.
- **Profil Rilis Optimal (`Cargo.toml`):** `opt-level = 3`, `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`.

---

## 3. Pengukuran Performa & Verifikasi

| Metrik | Baseline `v0.3.1` | Sesudah `v0.3.2` | Catatan |
|---|---:|---:|---|
| Ukuran binary Windows (`selen.exe`) | 13,12 MB (13.758.976 B) | *(diukur setelah rilis v0.3.2)* | Tambah `strip = true`, `codegen-units = 1`, `panic = "abort"` |
| Startup `--plain` (warm) | 70,1 ms | *(diukur setelah rilis v0.3.2)* | Target < 300 ms |
| Idle TUI redraws / detik | 20 redraw/dtk (50ms loop) | **0 redraw/dtk** (dirty-flag) | CPU idle mendekati 0% |
| Redraw 150 pesan (cache hit, CI) | Re-parse 150 pesan tiap frame | **100% cache hit + viewport slice** | Diverifikasi di `tests/hermes_full_and_perf.rs` |
| Overhead awal tugas ringan (sistem + tool) | ~5.500+ token (v0.3.0) | **< 2.000 token** (~9 tool aktif) | Diverifikasi di `tests/token_efficiency_hermes.rs` |

### Catatan Kejujuran Uji Regresi Live LLM (Skenario A / B / E)
- Lingkungan build/pengembang ini tidak memiliki API key live `SELENCODE_ROUTER_KEY` (key milik pemilik tidak diekspor ke shell ini), sehingga **angka regresi live terhadap 9Router (`ag/gemini-3-flash`) belum dijalankan dari mesin ini** dan TIDAK diklaim sebelum diuji langsung oleh pemilik/Muse dengan binary rilis `v0.3.2`.
- Seluruh mekanisme Tahap 0–6 & Bagian A (instrumentasi SQLite, prune+arsip idempoten, fingerprint/dedup, rem Self-correction maks 1 putaran, cache_control `system_and_3`, cap shell 40/60, spillover, template kompresi Hermes, cooldown aux 60→300→900s, indeks MCP >15, cap injeksi dinamis, dan cache render TUI 150 pesan) telah diverifikasi lewat tes otomatis di `tests/hermes_full_and_perf.rs` pada CI Linux & Windows x64.
