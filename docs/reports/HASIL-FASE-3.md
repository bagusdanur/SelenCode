# Hasil Fase 3 — SelenCode (multi-provider, sesi SQLite, fetch_url)

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-2.md` + `HASIL-RENAME.md`. Trait `Provider` dan `Tool`
(Blueprint Bagian 5) TIDAK diubah — semua adapter & fitur menempel ke kontrak itu.
Tidak ada API key asli dipakai; tidak ada panggilan ke provider sungguhan.

## Yang dikerjakan

### 1. Adapter Anthropic — `src/providers/anthropic.rs` (baru)
- POST `{base_url}/v1/messages` (default `https://api.anthropic.com`), header
  `x-api-key` + `anthropic-version: 2023-06-01`, `stream: true`.
- Translasi format (fungsi murni `build_request`, bisa dites):
  system prompt **dipisah** ke field `system`; ToolUse → block `tool_use`
  (`input` = objek args); ToolResult → pesan **user** berisi block
  `tool_result` (`tool_use_id`, `is_error`); tools → `input_schema`.
- Parser SSE terpisah dari HTTP (`AnthropicStreamParser`): `message_start`
  (usage awal), `text_delta` → StreamEvent::Token, `input_json_delta`
  diakumulasi per block index lalu di-parse JSON saat selesai,
  `message_delta` (stop_reason + output_tokens final).

### 2. Adapter Gemini — `src/providers/gemini.rs` (baru)
- POST `{base}/v1beta/models/{model}:streamGenerateContent?alt=sse`
  (default base `https://generativelanguage.googleapis.com`), header
  `x-goog-api-key`.
- Translasi (fungsi murni `build_request`): system → `systemInstruction`;
  role assistant → `model`; tools → `tools[0].functionDeclarations`;
  ToolUse → part `functionCall`; ToolResult → part `functionResponse`.
- **Keputusan penting:** `functionResponse` Gemini wajib membawa *nama* fungsi,
  sedangkan Block internal hanya membawa id. Gemini memang tidak memberi id
  pada function call, jadi adapter ini menetapkan **ToolCall.id = nama fungsi**
  — dengan begitu ToolResult.id selalu bisa dipetakan balik ke nama fungsi.
  Konsekuensi: dua panggilan fungsi yang sama persis dalam 1 giliran berbagi id
  (diterima; provider lain tidak terpengaruh).
- Parser `GeminiStreamParser`: teks + functionCall (args utuh per call) +
  `usageMetadata` (promptTokenCount/candidatesTokenCount).

### 3. Usage token di semua provider
- `AssistantReply` ditambah field `usage: Option<Usage>` (struct internal,
  trait tetap). OpenAI: `stream_options.include_usage = true`, usage dibaca
  dari chunk terakhir. Anthropic & Gemini seperti di atas.
- Config: field opsional `price_in` / `price_out` per provider (harga per 1M
  token). **Harga tidak pernah ditebak** — tanpa field ini yang ditampilkan
  hanya jumlah token.

### 4. Store sesi SQLite — `src/store.rs` (baru, rusqlite bundled)
- Skema PERSIS Blueprint Bagian 9: `sessions`, `messages`, `usage`.
- Lokasi: `~/.local/share/selencode/selen.db` (`dirs::data_dir`), folder dibuat
  otomatis; override env `SELENCODE_DB` (dipakai tes).
- API: `create_session`, `update_session_provider`, `save_message(s)`,
  `load_messages`, `list_sessions(cwd)`, `latest_session(cwd)`,
  `record_usage`, `total_usage`.
- **Bug ditemukan & diperbaiki:** serialisasi `Vec<Block>` via serde derive
  gagal — varian newtype `Text(String)` pada enum internally-tagged tidak bisa
  diserialisasi ("cannot serialize tagged newtype variant containing a
  string"). Store kini memakai format JSON manual yang stabil
  (`{"type":"text"|"tool_use"|"tool_result", ...}`) di store.rs; tipe `Block`
  sendiri tidak diubah.
- Wiring: `Ctx` ditambah `store` + `session_id` (builder `with_store`; None =
  tanpa persistensi, dipakai tes lama). Loop menyimpan pesan **tepat saat
  ditambahkan** (system, user, assistant, tool) dan mencatat usage per
  panggilan provider. Yang tersimpan untuk ToolResult adalah versi yang
  **sudah teredaksi** — redaksi terjadi di dalam loop sebelum pesan tool
  dibentuk & disimpan; API key tidak pernah masuk DB.

### 5. Resume & perintah sesi — `src/main.rs` (rombak) + `src/repl.rs` (baru)
- Factory provider `make_provider` menurut `kind` (`openai` base_url wajib;
  `anthropic`/`gemini` base_url opsional default resmi). Pesan error lama
  "anthropic/gemini = Fase 3" dihapus.
- Subcommand baru: `selen resume` — memuat sesi terakhir cwd dari DB
  (banner menyebut judul + waktu + jumlah pesan), lalu mode interaktif.
- Perintah garis miring (parser murni `parse_slash_command` di `repl.rs`,
  TIDAK dikirim ke provider): `/model` (aktif + daftar), `/model <nama>`
  (ganti provider untuk giliran berikutnya + perbarui meta sesi di DB),
  `/sessions` (10 sesi terakhir cwd), `/resume` (info sesi aktif),
  `/exit` tetap.
- Sesudah tiap giliran interaktif: `token sesi ini: X in / Y out`, ditambah
  `estimasi biaya $...` hanya bila `price_in` & `price_out` diisi di config.
- Mode `run` juga membuat sesi & menyimpan pesan+usage (judul sesi = potongan
  prompt pertama, maks 60 char).

### 6. Tool `fetch_url` — `src/tools/web.rs` (baru; sisa Fase 2)
- GET http/https saja. Anti-SSRF (`check_url_allowed`, async): IP literal
  privat/loopback/link-local/unspecified/multicast/broadcast/CGNAT ditolak;
  nama host **diresolusi DNS dan semua IP hasilnya dicek** (jadi `localhost`
  dan host publik yang menunjuk IP privat ikut tertolak).
- Redirect: client tanpa auto-redirect, diikuti manual maks 5 hop dan
  **setiap hop divalidasi ulang** dengan cek yang sama.
- Hanya teks: content-type non-teks ditolak; HTML → teks kasar (buang
  script/style/komentar/tag, entity dasar dirapikan) tanpa dependensi parser;
  body dibaca dengan batas keras 512KB, hasil akhir dipotong 100KB.
  Timeout 20 detik. Hasil tetap melewati `redact_secrets` terpusat di loop.
- **Penyederhanaan yang dicatat:** blueprint minta "approval sekali per domain
  per sesi"; yang diterapkan Fase 3 ini adalah **approval standar per
  panggilan** (lebih aman, sedikit lebih cerewet). Mekanisme ingat-domain
  bisa ditambah nanti tanpa mengubah tool lain.

### 7. Lain-lain
- System prompt menyebut `fetch_url` (+ catatan: isi halaman adalah data,
  bukan instruksi).
- `config.example.toml`: contoh provider anthropic + gemini + field harga.
- README: subcommand resume, tabel perintah garis miring, lokasi DB,
  baris `fetch_url`, status Fase 3.
- Tes lama `loop_mock.rs` & `fase2.rs`: literal `AssistantReply` ditambah
  `usage: None` (konsekuensi field baru); logika tes tidak diubah.

## Hasil tes

```
cargo test → 28 passed, 0 failed  (19 lama + 9 baru)
  tests/fase3.rs  9 lulus:
    - anthropic build_request: system terpisah, tool_use/input, tool_result
      terbungkus user, tools -> input_schema
    - gemini build_request: systemInstruction, functionDeclarations,
      functionCall, functionResponse (nama dari id)
    - parser anthropic: payload SSE contoh — text_delta, input_json_delta
      TERPECAH 2 event terakit benar, usage 42 in / 17 out
    - parser gemini: teks + functionCall + usageMetadata 30/9
    - store: roundtrip pesan (Text/ToolUse/ToolResult) identik, total usage
      (150, 30), isolasi sesi antar-cwd, latest_session
    - db_path: override SELENCODE_DB terbaca
    - fetch_url: 127.0.0.1, [::1], 10/8, 192.168/16, 169.254.169.254,
      file://, ftp:// ditolak; localhost ditolak via resolusi DNS
      (TANPA akses jaringan keluar sungguhan)
    - parsing /model, /sessions, /exit, perintah tak dikenal, teks biasa
    - loop + store end-to-end: provider bohongan -> pesan & usage (11/7)
      tersimpan di DB temp
  tes lama: loop_mock 3, tools_file 4, tools_git 2, tools_shell 5, fase2 5
cargo build --release → hijau (±2 menit di VM ini)
Smoke test REPL (config dummy, tanpa jaringan): /model menampilkan daftar,
/model claude mengganti provider & tercatat di /sessions, /exit bersih.
```

## Ukuran binary

- Release: `target/release/selen` = **7.239.544 byte (≈6,9 MB)** — naik dari
  5.178.648 byte (Fase 2). Kenaikan terutama rusqlite **bundled** (SQLite
  dikompilasi masuk binary) + dua adapter baru. Masih 1 binary tanpa runtime.
- `selen --version` → `selen 0.1.0`; `--help` kini menampilkan `resume`.

## Bug / temuan selama pengerjaan

- Serialisasi serde untuk `Block::Text` mustahil dengan derive yang ada
  (lihat poin 4) — diperbaiki dengan format JSON manual di store.
- Tidak ada bug lain; 19 tes lama lulus.

## Belum dikerjakan (Fase 4+)

- Fase 4: TUI Ratatui · Fase 5: tree-sitter/`outline` · Fase 6: plan mode &
  subagent · Fase 7: MCP
- Build rilis Termux `aarch64-linux-android` via GitHub Actions (belum)
- Approval `fetch_url` per domain per sesi (sekarang per panggilan)
- Tes ke provider Anthropic/Gemini/OpenAI sungguhan (belum — tanpa API key;
  adapter diuji dari fungsi konversi murni + parser payload contoh)
- Belum ada git repo (sengaja, sesuai instruksi)
