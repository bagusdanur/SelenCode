> Catatan: project ini bernama SelenCode (binary: selen) sejak 24 Nov 2026.

# Hasil Fase 7 — SelenCode (klien MCP + rilis Termux) — FASE TERAKHIR

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-6.md` & `HASIL-9ROUTER.md`. Trait `Provider` dan `Tool`
(Blueprint Bagian 5) TIDAK diubah. Provider utama config contoh tetap
router/9Router (tidak disentuh). Tidak ada git init/push; tidak ada API key
asli dipakai (tes MCP 100% lokal).

## 1. Klien MCP — `src/mcp/client.rs` (baru)

- Transport stdio sesuai spesifikasi MCP: JSON-RPC 2.0 dibingkai newline
  (NDJSON). `encode_message` / `decode_message` adalah fungsi murni terpisah
  dan ter-unit-test.
- Handshake persis urutan protokol: spawn proses (command + args + env dari
  config, stdin/stdout piped, `kill_on_drop`) -> request `initialize`
  (protocolVersion `2024-11-05`, capabilities kosong, clientInfo
  `{name: "selencode", version}`) -> notifikasi `notifications/initialized`
  -> `tools/list`.
- Async Tokio: 1 task pembaca memiliki stdout anak (BufReader per baris);
  request diberi id menaik (AtomicU64); respons dicocokkan lewat map
  id -> oneshot. Pesan server yang bukan respons (notifikasi / request dari
  server) diabaikan di versi ini.
- Timeout per request dari config per server (`timeout_secs`, default 30).
- Proses server mati di tengah sesi: task pembaca melihat EOF -> flag `dead`
  diset + semua request yang menunggu digagalkan dengan error jelas;
  panggilan berikutnya langsung error "server MCP '...' sudah mati".
  Tidak ada jalur panik.
- `tools/call`: content[] bertipe `text` digabung newline; tipe lain jadi
  representasi JSON ringkas; `isError: true` dari server -> Err tool.
- Keputusan kecil yang dicatat: **stderr proses server dibuang (null)**
  agar output server eksternal tidak mengotori TUI; kegagalan server tetap
  terlihat sebagai error handshake/panggilan di sisi selen.

## 2. Pembungkus Tool — `src/mcp/mod.rs` (baru)

- `McpTool` mengimplementasikan trait `Tool` yang ada. Nama terekspos ke
  model: **`mcp__<server>__<tool>`** (pola umum MCP, anti-tabrakan).
  Skema argumen diteruskan apa adanya dari `inputSchema` server.
- **Approval (keputusan):** `needs_approval` = true secara default untuk
  semua tool MCP — server eksternal tidak dikenal sifatnya. Config per
  server `trust = true` mematikannya untuk server itu saja.
- Didaftarkan HANYA ke registry induk mode Execute di `Session::build`
  (yang kini `async` karena startup MCP async; 3 titik panggil disesuaikan).
  TIDAK masuk registry baca-saja plan mode / subagent `delegate`.
- Kegagalan start 1 server TIDAK menggagalkan start selen: `build_mcp_tools`
  menangkap Err per server, mencetak `[mcp] server '...' dilewati: ...` ke
  stderr, dan melanjutkan server lain. Terbukti di smoke test (lihat bawah).
- Hasil tool MCP mengalir lewat jalur ToolResult biasa -> tetap melewati
  `redact_secrets` terpusat di loop (agent/loop.rs), sama seperti tool lain.
- System prompt (agent/context.rs): bila ada tool `mcp__*` terdaftar,
  ditambah catatan bahwa tool itu berasal dari server eksternal pihak
  ketiga, bukan bawaan SelenCode.

## 3. Config

Section baru `[[mcp_servers]]` di `Config` (serde default = kosong, jadi
tanpa section ini perilaku selen persis Fase 6):

```toml
[[mcp_servers]]
name = "contoh"
command = "npx"
args = ["-y", "@contoh/mcp-server"]
# env = { KEY = "value" }        # opsional
# trust = false                  # true = tool server ini tanpa approval
# timeout_secs = 30
```

Contoh berkomentar ditambahkan ke config.example.toml; subbab MCP +
contoh server filesystem ditambahkan ke README.

## 4. Rilis Termux — `.github/workflows/release.yml` (baru)

Persis Blueprint Bagian 11, dengan nama artifact disesuaikan ke nama final:

- Trigger: push tag `v*` + `workflow_dispatch` manual.
- Job **android**: ubuntu-latest, `dtolnay/rust-toolchain@stable` dengan
  target `aarch64-linux-android`, `nttld/setup-ndk@v1`,
  `cargo install cargo-ndk --locked`, build
  `cargo ndk -t arm64-v8a build --release`, artifact **`selen-termux-arm64`**
  = `target/aarch64-linux-android/release/selen`.
- Job **linux**: `cargo build --release` biasa, artifact **`selen-linux-x64`**
  = `target/release/selen`.
- Job **release** (needs android + linux, `if` hanya ref tag, permissions
  `contents: write`): download kedua artifact, **ganti nama aset per
  platform** (`selen-termux-arm64` / `selen-linux-x64`) — perlu karena kedua
  artifact berisi file bernama sama `selen` dan aset rilis bernama sama akan
  saling menimpa — lalu `softprops/action-gh-release@v2` dengan
  `generate_release_notes: true`.

**Status validasi (jujur):** YAML diperiksa valid dengan
`yaml.safe_load` (Python) dan struktur job/trigger/artifact/needs/if
ditelusuri programatis — semua sesuai rancangan. **Workflow BELUM TERUJI
sungguhan**: GitHub Actions tidak bisa dijalankan dari sini, dan project
ini belum git init. Workflow baru terbukti saat user push tag `v*` pertama
ke repo GitHub-nya. Risiko yang belum bisa dicek dari sini: kompilasi
crate C (grammar tree-sitter, rusqlite bundled, ring dari rustls) di bawah
NDK lewat cargo-ndk — jalur standar yang umum dipakai, tapi tetap
pembuktian pertamanya adalah run Actions pertama itu.

## 5. TERMUX.md (baru, root project)

Panduan pasang di Termux mengikuti Blueprint Bagian 11 "Cara pakai di
Termux": `pkg install git ripgrep`, download aset `selen-termux-arm64`
dari Releases, `chmod +x` + `mv` ke `$PREFIX/bin/selen`, config di
`~/.config/selencode/config.toml` dengan contoh provider router 9Router,
`export SELENCODE_ROUTER_KEY=...` (disarankan di `~/.bashrc`), contoh
`selen run`, plus jalur cadangan compile di dalam Termux.

## Hasil tes

```
cargo test → 59 passed, 0 failed  (52 lama tetap hijau + 7 baru)
  src/mcp/client.rs (unit)  4 lulus:
    - encode_is_single_ndjson_line
    - decode_roundtrip_and_rejects_empty
    - extract_text_joins_text_blocks
    - parse_tools_list_defaults
  tests/mcp.rs  3 lulus:
    - framing_ndjson_roundtrip: encode/decode NDJSON 1 baris persis
    - mcp_echo_server_end_to_end: lawan fixture Python
      tests/fixtures/mcp_echo_server.py (NDJSON stdio: initialize,
      tools/list, tools/call) — tools/list memuat echo, McpTool bernama
      mcp__tes__echo, run({text:"halo"}) → "halo", needs_approval benar
      untuk trust=false dan salah untuk trust=true, jalur connect_server
      (yang dipakai Session::build) juga terbukti
    - mcp_dead_server_fails_gracefully: command "false" (langsung mati)
      dan command tidak ada → Err anggun, bukan panik
cargo build --release → hijau (70 dtk, CARGO_BUILD_JOBS=2), tanpa warning baru.
Smoke start: config berisi 2 server MCP — "mati" (command false) dan
"echo" (fixture) — `selen --plain` start normal (exit 0), stderr memuat
"[mcp] server 'mati' dilewati: handshake initialize ... berhenti di
tengah sesi", server echo terdaftar tanpa peringatan.
```

## Ukuran binary

- Release: `target/release/selen` = **12.712.824 byte (±12,1 MB)** — naik
  ±103 KB dari Fase 6 (12.609.816 byte). Klien MCP murni tokio + serde_json
  yang sudah ada; tanpa dependensi baru.

## Kesulitan yang ditemui

- Praktis tidak ada di protokol: server bohongan + klien lulus sekali
  jalan. Satu-satunya penyesuaian struktural: `Session::build` menjadi
  `async` karena startup MCP memang async (3 titik panggil ditambah
  `.await`).
- Catatan parser YAML: PyYAML membaca kunci `on:` sebagai boolean True
  (kekhasan YAML 1.1) — itu artefak parser pemeriksa, bukan kesalahan
  workflow; GitHub mem-parse `on:` dengan benar.

## Ringkasan status keseluruhan project — Fase 0–7 TUNTAS

| Fase | Isi | Status |
|---|---|---|
| 0+1 | Kerangka CLI + provider OpenAI-compatible + loop agent + tool file + approval diff | selesai (tes live 9Router lulus, HASIL-9ROUTER.md) |
| 2 | run_shell, tool git, sandbox, redaksi rahasia, compaction konteks | selesai |
| 3 | Adapter Anthropic & Gemini native, sesi SQLite + resume, hitung token/biaya, fetch_url anti-SSRF | selesai |
| 4 | TUI Ratatui full-screen: transkrip, popup approval diff berwarna, status bar, fallback polos | selesai |
| 5 | Tree-sitter: tool outline + konteks pintar auto-outline | selesai |
| 6 | Plan mode (dua lapis) + update_plan + subagent delegate paralel baca-saja | selesai |
| 7 | Klien MCP stdio + workflow rilis Termux/Linux + TERMUX.md | selesai (workflow menunggu pembuktian tag pertama) |

SelenCode hari ini: 1 binary Rust ±12,1 MB, 59 tes hijau, 3 provider
(OpenAI-compatible/Anthropic/Gemini), 14 tool bawaan + tool MCP dinamis,
plan mode, subagent paralel, sesi SQLite yang bisa di-resume, dan jalur
rilis Termux `aarch64-linux-android` siap di GitHub Actions.

Sisa di luar blueprint (bukan utang fase, hanya belum dikerjakan):
- Push repo ke GitHub + tag `v*` pertama untuk membuktikan workflow rilis
  dan menghasilkan binary Termux yang bisa dipasang di HP (keputusan &
  akun GitHub user).
- Tes live adapter Anthropic/Gemini native ke API aslinya masing-masing
  (OpenAI-compatible via 9Router sudah lulus live).
- Request dari server MCP (sampling, roots) belum didukung klien —
  diabaikan; mayoritas server tool-only tidak membutuhkannya.
