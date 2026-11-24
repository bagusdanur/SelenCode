# Blueprint — Terminal AI Agent Rust Maksimal untuk Termux Android

> Project: CLI AI coding agent seperti Claude Code, ditulis full Rust,
> jalan sebagai 1 binary di Termux Android, laptop Linux, dan Windows.
> Otak = API cloud (bukan model lokal), tangan = tool lokal.
> Nama final: **SelenCode** — persona agent **Selena**, binary/perintah `selen`, package `selencode`.
> Catatan: nama kerja awal project ini adalah "koda"; rename ke SelenCode disahkan fhr & dikerjakan 24 November 2026 (lihat HASIL-RENAME.md). Sebutan "koda" yang masih tersisa di isi teknis blueprint ini adalah sisa dokumen awal dan mengacu ke project yang sama.

Tanggal: 24 November 2026 · Untuk: fhr · Editor: VS Code / Antigravity di laptop

---

## 1. Keputusan inti (jangan diganti di tengah jalan)

| Hal | Keputusan | Alasan |
|---|---|---|
| Bahasa | Rust stable, edition 2021 | 1 binary, RAM satuan MB, tanpa runtime |
| Async | Tokio (full) | Streaming token + tool paralel |
| TLS | **rustls**, BUKAN OpenSSL/native-tls | Cross-compile ke Android/Termux jauh lebih gampang, tanpa lib sistem |
| HTTP | reqwest (rustls, stream, json) | Streaming SSE ke provider |
| TUI | Ratatui + Crossterm | Full-screen seperti Claude Code, jalan di Termux |
| Database | SQLite via rusqlite (bundled) | Riwayat sesi lokal, tanpa server DB |
| Provider | OpenAI-compatible PERTAMA, Anthropic & Gemini adapter berikutnya | 9Router / OpenRouter / endpoint OpenAI-compatible langsung jalan dari hari pertama |
| Distribusi | GitHub Actions build rilis | Laptop kentang & HP tidak compile rilis sendiri |

Prinsip: **semua provider diperlakukan sama lewat 1 trait.** Nambah provider baru
= 1 file baru, bukan ngutak-atik loop agent.

---

## 2. Arsitektur

```
┌─────────────────────────────────────────────────────┐
│  TUI (Ratatui)                                      │
│  chat stream · diff view · popup approval · status  │
└───────────────▲──────────────────┬──────────────────┘
                │ event (mpsc)     │ input user
┌───────────────┴──────────────────▼──────────────────┐
│  AGENT LOOP (core)                                  │
│  prompt → provider → tool_call? → jalanin tool →    │
│  hasil balik ke provider → ... sampai selesai       │
│  + plan mode, todo list, compaction konteks         │
└──────┬──────────────────┬──────────────────┬────────┘
       │                  │                  │
┌──────▼─────┐   ┌────────▼───────┐   ┌──────▼────────┐
│ PROVIDERS  │   │ TOOLS          │   │ MCP CLIENT    │
│ openai-    │   │ file, search,  │   │ hubungkan ke  │
│ compatible │   │ shell, git,    │   │ server MCP    │
│ anthropic  │   │ web fetch      │   │ eksternal     │
│ gemini     │   │                │   │ (fase akhir)  │
└────────────┘   └────────────────┘   └───────────────┘
       │                  │                  │
┌──────▼──────────────────▼──────────────────▼────────┐
│  STORAGE: SQLite (sesi, pesan, biaya) · config.toml │
│  KEAMANAN: approval gate, allowlist, sandbox folder │
└─────────────────────────────────────────────────────┘
```

Loop agent (pseudocode):

```
loop {
    respons = provider.chat_stream(pesan, definisi_tool).await
    tampilkan token sambil streaming
    if respons tidak minta tool { simpan; break }
    for setiap tool_call {
        if tool butuh izin { tampilkan diff/perintah; tunggu y/n/sekali/selalu }
        hasil = tool.jalankan(args).await   // timeout + batas ukuran output
        tambahkan hasil ke pesan
    }
    if token > 80% konteks { compaction: ringkas riwayat lama }
}
```

---

## 3. Stack lengkap (Cargo.toml)

```toml
[package]
name = "koda"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
reqwest = { version = "0.12", default-features = false,
            features = ["rustls-tls", "json", "stream"] }
eventsources-stream = "0.2"        # parsing SSE
futures = "0.3"
ratatui = "0.29"
crossterm = "0.28"
tui-textarea = "0.7"               # input multi-baris di TUI
syntect = "5"                      # syntax highlight
similar = "2"                      # diff
ignore = "0.4"                     # jalan folder hormati .gitignore
globset = "0.4"
tree-sitter = "0.24"               # + grammar per bahasa (fase 5)
tree-sitter-rust = "0.23"
tree-sitter-python = "0.23"
tree-sitter-javascript = "0.23"
tree-sitter-typescript = "0.23"
rusqlite = { version = "0.32", features = ["bundled"] }
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = "0.3"
dirs = "5"                         # folder config/data lintas OS
uuid = { version = "1", features = ["v4"] }
chrono = "0.4"
sha2 = "0.10"

[profile.release]
lto = true
codegen-units = 1
strip = true
panic = "abort"
opt-level = 3
```

Catatan: grammar tree-sitter ditambah bertahap — tarik semua grammar sekaligus
bikin binary & waktu compile membengkak.

---

## 4. Struktur folder

```
koda/
├── Cargo.toml
├── README.md
├── config.example.toml
├── .github/workflows/release.yml     # build Termux + Linux + Windows
├── src/
│   ├── main.rs            # Clap, pilih mode: TUI / sekali-jalan / resume
│   ├── config.rs          # baca config.toml + env (API key TIDAK di file biasa)
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── loop.rs        # loop agent inti (bagian terpenting)
│   │   ├── context.rs     # rakit system prompt, baca AGENT.md project, compaction
│   │   ├── plan.rs        # plan mode + todo list agent
│   │   └── subagent.rs    # task Tokio paralel (fase 6)
│   ├── providers/
│   │   ├── mod.rs         # trait Provider + tipe pesan/tool_call bersama
│   │   ├── openai.rs      # OpenAI-compatible: 9Router, OpenRouter, dll
│   │   ├── anthropic.rs   # Claude native (fase 3)
│   │   └── gemini.rs      # Gemini native (fase 3)
│   ├── tools/
│   │   ├── mod.rs         # trait Tool + registry + skema JSON per tool
│   │   ├── file.rs        # read_file, write_file, edit_file, list_files
│   │   ├── search.rs      # search_text (ignore + regex), outline (tree-sitter)
│   │   ├── shell.rs       # run_shell: timeout, batas output, allowlist
│   │   ├── git.rs         # git_status, git_diff, git_commit (lewat shell git)
│   │   └── web.rs         # fetch_url (teks saja, batas ukuran)
│   ├── tui/
│   │   ├── mod.rs         # Ratatui: layout, render
│   │   ├── app.rs         # state UI + event mpsc dari agent
│   │   └── diff_view.rs   # tampilan diff + popup approval
│   ├── store.rs           # SQLite: sesi, pesan, statistik biaya
│   ├── security.rs        # approval, path sandbox, redaksi rahasia di output
│   └── mcp/               # klien MCP (fase 7)
│       ├── mod.rs
│       └── client.rs      # transport stdio + JSON-RPC
└── tests/
    ├── loop_mock.rs       # loop diuji pakai provider bohongan (tanpa API)
    └── tools_file.rs
```

Aturan main.rs tipis: semua logika di modul. Kalau 1 file lewat ±400 baris,
pecah — Antigravity lebih akurat di file kecil.

---

## 5. Kontrak inti (tulis ini PERTAMA, sebelum fitur)

```rust
// providers/mod.rs — semua provider wajib tunduk ke bentuk ini
pub struct Message { pub role: Role, pub content: Vec<Block> }
pub enum Block { Text(String), ToolUse { id: String, name: String, args: Value },
                 ToolResult { id: String, content: String, is_error: bool } }

#[async_trait]
pub trait Provider {
    async fn stream_chat(&self, msgs: &[Message], tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>) -> Result<AssistantReply>;
}

// tools/mod.rs — semua tool wajib tunduk ke bentuk ini
#[async_trait]
pub trait Tool {
    fn spec(&self) -> ToolSpec;              // nama, deskripsi, skema JSON args
    fn needs_approval(&self, args: &Value) -> bool;
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String>;
}
```

Kenapa kontrak dulu: provider OpenAI / Anthropic / Gemini format tool-calling-nya
beda-beda. Kontrak bersama ini yang menerjemahkan semuanya ke 1 bentuk internal,
jadi loop agent tidak pernah tahu provider mana yang lagi dipakai.

---

## 6. Daftar tool bawaan (persis ini, jangan ditambah dulu)

| Tool | Izin | Batas |
|---|---|---|
| `list_files(path)` | bebas | hormati .gitignore, maks 500 entri |
| `read_file(path, start?, end?)` | bebas | maks 2000 baris / 100 KB per panggil |
| `search_text(query)` | bebas | regex, maks 100 hasil |
| `outline(path)` | bebas | struktur fungsi/class via tree-sitter (fase 5) |
| `write_file(path, content)` | **approval + diff** | hanya di dalam folder project |
| `edit_file(path, old, new)` | **approval + diff** | `old` wajib unik di file itu |
| `run_shell(command)` | **approval** (kecuali allowlist) | timeout 120 dtk, output dipotong 30 KB |
| `git_status` / `git_diff` | bebas | — |
| `git_commit(message)` | **approval** | tidak pernah push otomatis |
| `fetch_url(url)` | approval sekali per domain | teks saja, maks 100 KB, blokir IP privat |

Perintah yang SELALU ditolak walau di-approve: `rm -rf /`, penulisan di luar
folder project, membaca file berisi rahasia (`.env`, `*.key`, `id_rsa`) untuk
dikirim ke provider.

---

## 7. Config (config.toml di ~/.config/koda/)

```toml
default_provider = "router"

[providers.router]                 # contoh: 9Router kamu sendiri
kind = "openai"                    # openai | anthropic | gemini
base_url = "https://router.ryukomik.com/v1"
model = "isi-model-default-kamu"
api_key_env = "KODA_ROUTER_KEY"    # key dibaca dari env, BUKAN ditulis di sini

[providers.gemini]
kind = "gemini"
model = "isi-model-gemini"
api_key_env = "GEMINI_API_KEY"

[agent]
max_steps = 25                     # batas putaran tool per perintah
max_output_tokens = 8192
approval = "ask"                   # ask | auto-read (tulis tetap tanya)

[shell]
allowlist = ["ls", "git status", "git diff", "cargo test", "python -m py_compile"]
timeout_secs = 120
```

Di Termux: `export KODA_ROUTER_KEY=...` di `~/.bashrc`, atau file `~/.koda.env`
chmod 600 yang dibaca saat start. Jangan pernah commit key ke repo.

---

## 8. Tampilan TUI (Ratatui)

```
┌─ koda · project: ~/proyek · model: router/default ──────────────┐
│  > perbaiki fungsi login yang gagal test                        │
│                                                                 │
│  ● read_file src/auth.rs                                        │
│  ● search_text "fn login"                                       │
│  ◐ edit_file src/auth.rs   [diff ditampilkan, y/n?]             │
│  streaming jawaban model di sini...                             │
├─────────────────────────────────────────────────────────────────┤
│  konteks 34% · biaya sesi $0.012 · langkah 5/25                 │
│  > ketik perintah…                                    (Ctrl+C x2│
└─────────────────────────────────────────────────────────────────┘
```

Mode cadangan: `koda run "perintah"` tanpa TUI (sekali jalan, output polos) —
wajib ada, karena TUI di Termux sempit & mode polos enak buat pipa/script.

---

## 9. Skema SQLite (store.rs)

```sql
CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, provider TEXT,
                       model TEXT, created_at TEXT, title TEXT);
CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT,
                       role TEXT, content TEXT, ts TEXT);  -- content = JSON Block
CREATE TABLE usage (id INTEGER PRIMARY KEY, session_id TEXT, model TEXT,
                    input_tokens INT, output_tokens INT, ts TEXT);
```

Resume: `koda resume` memuat ulang messages sesi terakhir dari folder yang sama.

---

## 10. Roadmap fase (kerjakan berurutan, jangan loncat)

**Fase 0 — Kerangka (target: jalan kosong)**
Clap + config + `koda run "halo"` memanggil provider OpenAI-compatible TANPA
tool, streaming ke terminal polos (belum TUI). Tes: provider bohongan lokal.

**Fase 1 — Loop + tool file (MVP SEBENARNYA)**
Loop agent + `list_files`, `read_file`, `search_text`, `write_file`,
`edit_file` + approval diff di terminal polos. Titik ini agent sudah bisa
dipakai ngoding beneran di laptop.

**Fase 2 — Shell, git, keamanan**
`run_shell` (timeout, potong output, allowlist), tool git, sandbox path,
redaksi rahasia, batas langkah & compaction konteks.

**Fase 3 — Multi-provider + sesi**
Adapter Anthropic & Gemini, SQLite sesi + `resume`, hitung token/biaya,
ganti model dari dalam sesi (`/model`).

**Fase 4 — TUI Ratatui**
Layar full-screen, popup approval, panel diff, status bar. Mode polos
`koda run` tetap dipertahankan.

**Fase 5 — Tree-sitter**
Tool `outline` + konteks pintar: saat user menyebut file, agent otomatis
dikirim struktur fungsinya, bukan isi mentah — hemat token besar.

**Fase 6 — Plan mode & subagent**
Plan mode (agent menyusun rencana, user approve, baru eksekusi) + subagent
Tokio paralel untuk tugas yang bisa dipecah (mis. baca 5 file sekaligus).

**Fase 7 — MCP**
Klien MCP transport stdio: server MCP eksternal yang didaftarkan di config
tool-nya muncul sebagai tool biasa di registry. Ini fase terakhir & opsional.

Setiap fase selesai = commit + tag (`v0.1-fase1`, dst). Kalau fase rusak,
rollback ke tag terakhir, bukan debug di atas kode rusak.

---

## 11. Build & rilis Termux (bagian yang paling sering salah)

Fakta teknis yang menentukan cara build:

1. Termux memakai **bionic libc (Android)**, bukan glibc Linux biasa.
   Binary Linux glibc standar TIDAK jalan di Termux.
2. Target Rust yang benar: **`aarch64-linux-android`** (HP Android umumnya ARM64).
3. Target itu butuh **Android NDK** buat linker — berat di laptop kentang,
   makanya build rilis diserahkan ke GitHub Actions (gratis, runner kuat).

### GitHub Actions (.github/workflows/release.yml) — garis besarnya

```yaml
name: release
on: { push: { tags: ["v*"] } }
jobs:
  android:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: aarch64-linux-android }
      - uses: nttld/setup-ndk@v1        # sediakan NDK + linker aarch64
      - run: cargo install cargo-ndk
      - run: cargo ndk -t arm64-v8a build --release
      - uses: actions/upload-artifact@v4
        with: { name: koda-termux-arm64, path: target/aarch64-linux-android/release/koda }
  linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --release
      - uses: actions/upload-artifact@v4
        with: { name: koda-linux-x64, path: target/release/koda }
```

### Cara pakai di Termux (hasil akhirnya semudah ini)

```bash
pkg install git ripgrep python nodejs   # tool yang mungkin dipakai agent
# download binary 'koda' dari GitHub Releases project-mu, lalu:
chmod +x koda && mv koda $PREFIX/bin/
export KODA_ROUTER_KEY="key-kamu"       # taruh di ~/.bashrc
cd ~/project-kamu && koda
```

Jalur cadangan kalau build NDK bermasalah: compile DI DALAM Termux sekali saja
(`pkg install rust && cargo build --release`) — lambat (bisa 20-40 menit di HP),
tapi terbukti bisa untuk hampir semua crate murni-Rust; rustls (bukan OpenSSL)
adalah alasan jalur cadangan ini realistis.

Di laptop buat pengembangan harian: `cargo run` biasa (target Linux/Windows),
tidak perlu NDK sama sekali. NDK hanya buat rilis Termux.

---

## 12. Prompt siap tempel ke Antigravity

Tempel blueprint ini (BLUEPRINT.md) ke folder project, lalu pakai prompt ini
satu per satu — JANGAN minta semua fase sekaligus.

**Prompt mulai (Fase 0):**
> Baca BLUEPRINT.md di folder ini dan kerjakan HANYA Fase 0. Buat Cargo.toml
> persis seperti Bagian 3 (tapi tree-sitter, ratatui, dan rusqlite JANGAN
> dimasukkan dulu — itu fase berikutnya). Buat src/main.rs, src/config.rs,
> src/providers/mod.rs, src/providers/openai.rs. Perintah `koda run "halo"`
> harus streaming jawaban dari provider OpenAI-compatible yang diatur di
> config.toml. Selesai Fase 0, berhenti dan tunjukkan cara mengujinya.

**Prompt lanjut (dipakai ulang tiap fase, ganti nomornya):**
> Baca BLUEPRINT.md. Fase sebelumnya sudah selesai dan ter-commit. Kerjakan
> HANYA Fase N sekarang, ikuti struktur folder di Bagian 4 dan kontrak di
> Bagian 5. Jangan ubah trait Provider dan Tool. Tambahkan tes untuk fitur
> baru, jalankan `cargo test`, dan berhenti sesudahnya.

**Prompt jaga-jaga (kalau Antigravity mulai ngarang):**
> Berhenti menambah fitur. Tunjukkan file mana saja yang kamu ubah, kenapa,
> dan hasil `cargo test` terakhir. Jangan lanjut sebelum aku bilang gas.

---

## 13. Yang TIDAK dikerjakan (sengaja)

- Model lokal / inference di HP — bukan tujuan; otak selalu API.
- GUI / IDE — ini agent terminal, titik. Antigravity tetap editor kamu.
- Auto-push, auto-deploy — agent berhenti di commit; push keputusan user.
- Plugin marketplace, akun, server pusat — semua lokal, semua file sendiri.

---

## 14. Ukuran keberhasilan

- [ ] `koda run "jelaskan project ini"` menjawab benar dari isi folder
- [ ] Agent memperbaiki 1 bug nyata di project kecil, diff-nya kamu approve
- [ ] Binary Termux dari GitHub Actions jalan di HP: `koda --version`
- [ ] Sesi bisa di-resume setelah Termux ditutup paksa
- [ ] Biaya 1 tugas ngoding nyata tampil di status bar dan masuk akal
