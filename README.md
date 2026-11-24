# SelenCode — Terminal AI Coding Agent (full Rust)

Agent coding di terminal seperti Claude Code, ditulis full Rust, jalan sebagai
1 binary bernama `selen` di laptop dan Termux Android. Personanya bernama
**Selena**. Otak = API cloud (OpenAI-compatible),
tangan = tool lokal (file, search, shell, git).

Blueprint lengkap: lihat `BLUEPRINT.md`. Status: **Fase 0–7 selesai — blueprint tuntas**
(lihat `HASIL-FASE-0-1.md` s/d `HASIL-FASE-7.md`, plus `HASIL-9ROUTER.md` untuk
tes live pertama ke provider 9Router). Cara pasang di Termux: lihat `TERMUX.md`.

## Pakai

```bash
# 1. Siapkan config (pilih salah satu lokasi)
cp config.example.toml selencode.toml          # per project, didahulukan
# atau: mkdir -p ~/.config/selencode && cp config.example.toml ~/.config/selencode/config.toml

# 2. Set API key (nama env mengikuti api_key_env di config)
export SELENCODE_ROUTER_KEY="key-kamu"

# 3. Jalan
cargo run -- run "jelaskan project ini"   # sekali jalan
cargo run -- --yes run "perbaiki bug di src/main.rs"  # auto-approve edit
cargo run                                  # mode interaktif: TUI bila TTY, /exit keluar
cargo run -- --plain                       # paksa mode interaktif polos (tanpa TUI)
cargo run -- resume                        # lanjutkan sesi terakhir folder ini
./target/release/selen --version
```

Mode interaktif di terminal beneran memakai **TUI Ratatui** (Fase 4):
header + transkrip scrollable + status bar (konteks %, token sesi, langkah)
+ input multi-baris. Enter = kirim, Alt+Enter = baris baru, Ctrl+C = keluar,
PageUp/PageDown & roda mouse = gulir. Persetujuan tool tampil sebagai popup
modal — diff `edit_file`/`write_file` berwarna (+ hijau / - merah) —
jawab dengan `y`/`n`. Bila stdin/stdout bukan TTY (pipa/script) atau ada
flag `--plain`, mode interaktif jatuh ke REPL polos seperti sebelumnya.

Provider dipilih dari config lewat `kind`: `openai` (OpenAI-compatible,
base_url wajib), `anthropic` (Claude native), `gemini` (Gemini native) —
base_url anthropic/gemini opsional (default endpoint resmi). Ganti provider
saat jalan dengan flag `--provider <nama>`.

Di mode interaktif tersedia perintah garis miring (tidak dikirim ke provider):

| Perintah | Fungsi |
|---|---|
| `/plan` | giliran berikutnya plan mode: baca-saja, diakhiri rencana; kamu ditanya `Jalankan rencana ini? [y/N]` (TUI: ketik `y`/`n`) sebelum eksekusi |
| `/model` | tampilkan provider/model aktif + daftar provider di config |
| `/model <nama>` | ganti provider aktif untuk giliran berikutnya |
| `/sessions` | daftar 10 sesi terakhir folder ini |
| `/exit` | keluar |

Semua sesi tersimpan di SQLite `~/.local/share/selencode/selen.db`
(override: env `SELENCODE_DB`) — pesan (versi teredaksi) + usage token.
Sesudah tiap giliran interaktif ditampilkan `token sesi ini: X in / Y out`,
plus estimasi biaya bila provider di config mengisi `price_in`/`price_out`
(per 1M token); tanpa harga, biaya tidak ditebak.

Di mode interaktif & `run`, tool tulis (`write_file`/`edit_file`) selalu
menampilkan diff dan minta persetujuan `y/N`, kecuali flag `--yes` atau
`approval = "auto"` di config.

## Tool

| Tool | Izin |
|---|---|
| `list_files` | bebas — hormati .gitignore, maks 500 |
| `read_file` | bebas — maks 2000 baris / 100KB, file rahasia ditolak |
| `search_text` | bebas — regex, hormati .gitignore, maks 100 hasil |
| `outline` | bebas — struktur file kode via tree-sitter (fn/class/struct/impl/interface + rentang baris) tanpa membaca isi; bahasa rs, py, js, mjs, jsx, ts, tsx |
| `write_file` | approval + diff |
| `edit_file` | approval + diff, `old` wajib unik |
| `run_shell` | approval, kecuali cocok `[shell].allowlist` — timeout dari config, output maks 30KB, pola destruktif (`rm -rf /`, `mkfs`, fork bomb, dll) selalu ditolak |
| `git_status` / `git_diff` | bebas |
| `git_commit` | approval — `git add -A` + commit, tidak pernah push |
| `fetch_url` | approval per panggilan — teks saja maks 100KB, HTML diubah ke teks, IP privat/loopback/link-local ditolak (anti-SSRF, redirect divalidasi ulang per hop) |
| `update_plan` | bebas — catat/perbarui checklist langkah agent (`[x]`/`[>]`/`[ ]`); hanya mode eksekusi |
| `delegate` | bebas — 1-5 tugas penjelajahan ke subagent baca-saja paralel (batas `[agent].max_parallel_subagents`, default 3); subagent tidak bisa menulis |

**Plan mode (Fase 6):** flag `--plan` (giliran pertama interaktif; untuk
`selen run --plan "..."` berhenti sesudah rencana) atau perintah `/plan`.
Di plan mode hanya tool baca-saja yang ditawarkan ke model, dan panggilan
tool menulis ditolak loop tanpa dijalankan. Agent menutup dengan rencana
di bawah judul `## Rencana`; eksekusi hanya berjalan sesudah kamu setujui.

Semua path dikunci di dalam folder project (sandbox); `../` keluar ditolak.
Semua output tool **diredaksi** dari pola rahasia (`sk-...`, Bearer token,
`API_KEY=...`, blok private key → `[REDACTED]`) sebelum dikirim ke provider.
Konteks panjang otomatis di-compact: riwayat lama diringkas saat estimasi
token melewati 80% `[agent].context_limit_tokens` (default 100.000).

**Konteks pintar (Fase 5):** bila kamu menyebut path file kode di prompt
(mis. "perbaiki fungsi hitung di src/contoh.rs") dan file itu ada di folder
project, outline strukturnya otomatis dilampirkan ke pesanmu untuk giliran
itu (maks 3 file, blok maks 6KB) — Selena langsung tahu peta fungsinya
tanpa membaca isi file penuh. File yang tidak ada / di luar folder / rahasia
dilewati diam-diam.

## Server MCP (Fase 7)

Server MCP eksternal (transport stdio) bisa didaftarkan di config; tool-nya
muncul di registry mode eksekusi sebagai `mcp__<server>__<tool>` dan dipakai
Selena seperti tool biasa. Daftarkan sebanyak yang dibutuhkan:

```toml
[[mcp_servers]]
name = "contoh"
command = "npx"
args = ["-y", "@contoh/mcp-server"]
# env = { KEY = "value" }        # opsional: env tambahan untuk proses server
# trust = false                  # true = tool server ini jalan TANPA approval
# timeout_secs = 30              # timeout per request ke server ini
```

Keputusan keamanan: panggilan tool MCP **selalu minta approval** secara
default (server eksternal tidak dikenal sifatnya) — `trust = true` hanya
untuk server yang benar-benar kamu percaya. Tool MCP tidak tersedia di
plan mode maupun untuk subagent `delegate`. Server yang gagal start
dilewati dengan peringatan di stderr; `selen` tetap jalan tanpa tool-nya.
Hasil tool MCP tetap melewati redaksi rahasia yang sama seperti tool lain.

Contoh nyata (server filesystem resmi MCP):

```toml
[[mcp_servers]]
name = "fs"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/folder"]
```

## Tes

```bash
cargo test
```

## Belum dikerjakan

Tidak ada — Fase 0–7 blueprint selesai. Workflow rilis Termux
(`.github/workflows/release.yml`) baru terbukti saat tag `v*` pertama
di-push ke repo GitHub (project ini belum di-git-init; lihat
`HASIL-FASE-7.md`).
