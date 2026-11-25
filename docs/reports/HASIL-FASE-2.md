> Catatan: project ini sekarang bernama SelenCode (binary: selen) sejak 24 Nov 2026.
# Hasil Fase 2 — koda (shell, git, keamanan)

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-0-1.md`. Trait `Provider` dan `Tool` (Blueprint Bagian 5) TIDAK diubah.

## Yang dikerjakan

### 1. Tool `run_shell` — `src/tools/shell.rs` (baru)
- Perintah lewat `sh -c` (Unix/Termux/Linux); Windows `cmd /C`. cwd = folder project.
- Timeout dari `[shell].timeout_secs` config (default 120 dtk): proses dibunuh
  (`kill_on_drop`), error berbunyi jelas "timeout setelah N detik".
- Output stdout+stderr digabung, dipotong maks 30 KB dengan penanda `[output dipotong]`.
- Approval: wajib KECUALI perintah cocok `[shell].allowlist` (cocok = sama persis
  atau diawali entri + spasi, jadi allowlist `ls` tidak meloloskan `lsof`).
- Exit code non-nol BUKAN error fatal: dikembalikan sebagai teks
  `exit code: N` + output, supaya model tahu test/build gagal.
- Pola terlarang selalu ditolak sebelum jalan (lihat poin 3).

### 2. Tool git — `src/tools/git.rs` (baru)
- `git_status` (bebas): `git status --short --branch`; repo bersih ditandai eksplisit.
- `git_diff` (bebas): `git diff` + `git diff --staged`, gabungan maks 30 KB.
- `git_commit` (approval wajib): `git add -A` lalu `git commit -m <message>` —
  semua pemanggilan git memakai argumen `Command` terpisah, BUKAN string shell,
  jadi pesan commit tidak bisa dipakai injeksi. TIDAK PERNAH push.
- Folder bukan repo → error jelas "bukan repo git" (cek `git rev-parse`).

### 3. Keamanan — `src/security.rs` (ditambah)
- `is_forbidden_command(cmd)`: menolak `rm -rf /`, `rm -rf ~` (termasuk varian
  bendera terpisah `-r -f` dan diselipkan di rantai `;`/`&&`/`|`), `mkfs*`,
  `dd if=`, fork bomb `:(){`, penulisan `/dev/sd*`, `shutdown`/`reboot`/`poweroff`/`halt`.
  Penolakan berlaku walau user `--yes`/approve.
- `redact_secrets(text)`: mengganti dengan `[REDACTED]` pola: blok
  `-----BEGIN ... PRIVATE KEY-----`, `Authorization: Bearer ...`, Bearer token lepas,
  `sk-...`, `ghp_...`, `github_pat_...`, `AKIA...` (AWS), dan pasangan
  `NAMA=VALUE` / `"nama": "value"` yang namanya mengandung KEY/TOKEN/SECRET/PASSWORD
  (nilai ≥ 8 karakter). Konservatif: nama seperti `monkey=` pun ikut teredaksi
  (mengandung "key") — disengaja, aman lebih penting.
- **Keputusan redaksi (dicatat sesuai tugas):** redaksi diterapkan di SATU titik
  pusat — `agent/loop.rs` saat membungkus hasil tool menjadi `ToolResult`
  (hasil Ok maupun error) — jadi tool baru otomatis ikut. Dalam arsitektur saat
  ini output tool TIDAK pernah ditampilkan mentah ke user lokal (loop hanya
  mencetak nama tool ke stderr; yang dibaca user adalah teks jawaban model),
  sehingga redaksi ini murni melindungi sisi provider tanpa mengurangi yang
  dilihat user.

### 4. Compaction konteks — `src/agent/context.rs` (ditambah) + hook di `loop.rs`
- `estimate_tokens`: total karakter semua blok / 4.
- `needs_compaction`: memicu bila estimasi > 80% `[agent].context_limit_tokens`
  (field config BARU, default 100.000). Dicek di loop sebelum tiap panggilan provider.
- `compact_messages` (murni, deterministik, TANPA panggilan provider):
  system prompt + pesan user pertama + 6 pesan terakhir dipertahankan utuh;
  bagian tengah diganti 1 pesan ringkasan berisi daftar tool yang pernah
  dipanggil + hasil tool lama dipotong maks 200 karakter per hasil dengan
  penanda `[hasil lama diringkas]`. Awal ekor dijaga tidak mulai di pesan Tool
  agar pasangan ToolUse/ToolResult tidak yatim.

### 5. Integrasi
- `Ctx` ditambah field `context_limit_tokens` (default 100.000, builder
  `with_context_limit`); `main.rs` mengisinya dari config dan memakai
  `default_registry_with_shell(&cfg.shell)` agar allowlist/timeout config
  benar-benar dipakai `run_shell`. `default_registry()` lama tetap ada
  (config shell bawaan) — tes lama tidak diubah.
- System prompt menyebut `run_shell` + alur git (cek status/diff sebelum commit,
  tidak pernah push).
- `config.example.toml`: field baru `context_limit_tokens = 100000`.
- Perbaikan sampingan: `ShellCfg::default()` kini manual — sebelumnya turunan
  `Default` membuat `timeout_secs = 0` bila section `[shell]` absen; sekarang 120.

## Hasil tes

```
cargo test → 19 passed, 0 failed
  tests/loop_mock.rs    3 lulus (Fase 1, tidak berubah)
  tests/tools_file.rs   4 lulus (Fase 1, tidak berubah)
  tests/tools_shell.rs  5 lulus — echo + output benar; exit 3 dilaporkan sebagai
                        teks exit code; sleep 5 dengan timeout 1 dtk → error
                        timeout; 5 pola terlarang ditolak tanpa dijalankan;
                        allowlist mengontrol needs_approval (termasuk kasus lsof)
  tests/tools_git.rs    2 lulus — repo temp sungguhan: status bersih → ubah file
                        → status/diff memuat perubahan → commit via tool tercatat
                        di git log → status bersih lagi; di luar repo → error
                        "bukan repo git"
  tests/fase2.rs        5 lulus — redaksi (sk-, Bearer, API_KEY=, password JSON,
                        AKIA, ghp_ semuanya teredaksi; teks normal utuh);
                        compaction (menyusut, kepala & ekor utuh, ada ringkasan);
                        loop: isi file berisi API_KEY=... sampai ke provider
                        sudah teredaksi
cargo build --release → hijau (55 dtk di VM ini)
```

## Ukuran binary

- Release: `target/release/koda` = **5.178.648 byte (≈4,9 MB)** — naik tipis dari
  Fase 0-1 (5.059.552 byte). `koda --version` → `koda 0.1.0`.

## Contoh pemakaian tool baru

```bash
export KODA_ROUTER_KEY="key-kamu"
cargo run -- run "jalankan cargo test dan perbaiki yang gagal"   # run_shell (approval per perintah di luar allowlist)
cargo run -- run "cek status git, lalu commit dengan pesan 'fix: perbaiki parser'"
cargo run -- --yes run "cek git diff terakhir"                    # auto-approve
```

Config relevan (`koda.toml`):

```toml
[agent]
context_limit_tokens = 100000   # compaction di 80%

[shell]
allowlist = ["ls", "git status", "git diff", "cargo test"]
timeout_secs = 120
```

## Bug / temuan selama pengerjaan

- `ShellCfg` turunan `Default` memberi `timeout_secs = 0` bila section `[shell]`
  tidak ada di config — diperbaiki dengan `Default` manual (120 dtk). Di Fase 1
  tidak berdampak karena tool shell belum ada.
- Tidak ada bug lain; semua tes lama lulus tanpa diubah.

## Belum dikerjakan (Fase 3+)

- `fetch_url` (ada di daftar tool Blueprint Bagian 6; di roadmap menempel Fase 2,
  belum dikerjakan di putaran ini)
- Fase 3: provider Anthropic & Gemini, sesi SQLite + `resume`, hitung token/biaya,
  ganti model `/model`
- Fase 4: TUI Ratatui · Fase 5: tree-sitter/`outline` · Fase 6: plan mode &
  subagent · Fase 7: MCP
- Build rilis Termux `aarch64-linux-android` via GitHub Actions (belum)
- Belum ada git repo (sengaja, sesuai instruksi)
