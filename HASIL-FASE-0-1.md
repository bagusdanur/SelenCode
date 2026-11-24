> Catatan: project ini sekarang bernama SelenCode (binary: selen) sejak 24 Nov 2026.
# Hasil Fase 0 + Fase 1 — koda

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build` hijau**

## Struktur file jadi

```
termux-agent-rust/
├── BLUEPRINT.md            (dari sebelumnya, tidak diubah)
├── HASIL-FASE-0-1.md       (file ini)
├── Cargo.toml              deps Fase 0+1 saja (tanpa ratatui/tree-sitter/rusqlite/syntect)
├── Cargo.lock
├── config.example.toml
├── README.md
├── .gitignore
├── src/
│   ├── lib.rs              modul publik (dipakai tests)
│   ├── main.rs             Clap: `run`, mode interaktif polos, --yes, --provider, --version
│   ├── config.rs           ./koda.toml didahulukan, lalu ~/.config/koda/config.toml
│   ├── security.rs         sandbox path, tolak file rahasia, Approver (y/n / auto)
│   ├── providers/
│   │   ├── mod.rs          trait Provider + Message/Block/ToolSpec/StreamEvent (Bagian 5)
│   │   └── openai.rs       OpenAI-compatible, streaming SSE, tool_calls akumulatif per index
│   ├── tools/
│   │   ├── mod.rs          trait Tool + Ctx + Registry (Bagian 5)
│   │   ├── file.rs         list_files, read_file, write_file, edit_file (+diff similar)
│   │   └── search.rs       search_text (regex, hormati .gitignore)
│   └── agent/
│       ├── mod.rs
│       ├── context.rs      system prompt + sisip AGENT.md/AGENTS.md (maks 8KB)
│       └── loop.rs         loop agent (diekspos sebagai agent::run / run_turn)
└── tests/
    ├── tools_file.rs       4 tes
    └── loop_mock.rs        3 tes (provider bohongan in-memory)
```

## Hasil tes

```
cargo test  →  7 passed, 0 failed
  tests/loop_mock.rs   3 passed (loop 2 putaran + tool, teks langsung berhenti,
                                 error tool jadi ToolResult is_error bukan crash)
  tests/tools_file.rs  4 passed (write/read/edit/list/search roundtrip,
                                 edit menolak old tidak unik, sandbox menolak ../
                                 + baca .env ditolak + search tidak bocor .env,
                                 .gitignore dihormati)
cargo build → hijau (debug)
```

Catatan perbaikan selama pengerjaan: `ignore::WalkBuilder` secara default hanya
menghormati `.gitignore` di dalam repo git — diperbaiki dengan
`.require_git(false)` di `list_files` dan `search_text`.

## Ukuran binary

- Debug: `target/debug/koda` ≈ 110 MB (normal untuk debug, bukan angka rilis)
- Release: `target/release/koda` = **5.059.552 byte (≈4,8 MB)**, build 2 mnt 34 dtk
  di VM ini (profile: LTO + strip + codegen-units=1) — sesuai target blueprint "binary satuan MB"

Smoke test manual: `koda --version` → `koda 0.1.0`; `koda run "halo"` tanpa
config → error yang jelas berisi contoh config (sesuai desain). Tes ke provider
asli TIDAK dilakukan (tidak ada API key asli yang dipakai).

## Contoh config

Lihat `config.example.toml`. Inti:

```toml
default_provider = "router"

[providers.router]
kind = "openai"
base_url = "https://router.ryukomik.com/v1"
model = "isi-model-default-kamu"
api_key_env = "KODA_ROUTER_KEY"

[agent]
max_steps = 25
max_output_tokens = 8192
approval = "ask"

[shell]
allowlist = ["ls", "git status", "git diff", "cargo test"]
timeout_secs = 120
```

## Contoh pemakaian

```bash
export KODA_ROUTER_KEY="key-kamu"
cargo run -- run "jelaskan project ini"        # sekali jalan, streaming
cargo run -- --yes run "rapikan fungsi di src/lib.rs"  # auto-approve edit
cargo run                                       # interaktif: ketik perintah, /exit keluar
```

## Belum dikerjakan (Fase 2+)

- Fase 2: `run_shell` (allowlist config sudah dibaca, tool-nya belum), tool git,
  `fetch_url`, compaction konteks, redaksi rahasia di output tool
- Fase 3: provider Anthropic & Gemini (config `kind` selain `openai` saat ini
  ditolak dengan pesan jelas), sesi SQLite + `resume`, hitung token/biaya
- Fase 4: TUI Ratatui · Fase 5: tree-sitter/`outline` · Fase 6: plan mode &
  subagent · Fase 7: MCP
- Build rilis Termux `aarch64-linux-android` via GitHub Actions (belum dibuat,
  sesuai blueprint Bagian 11 dikerjakan saat rilis)
- Belum ada git repo (sengaja, sesuai instruksi)
