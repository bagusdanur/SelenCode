> Catatan: project ini bernama SelenCode (binary: selen) sejak 24 Nov 2026.

# Hasil Fase 4 — SelenCode (TUI Ratatui)

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-3.md`. Trait `Provider` dan `Tool` (Blueprint Bagian 5)
TIDAK diubah. Tidak ada API key asli dipakai; tidak ada panggilan provider sungguhan.

## Yang dikerjakan

### 1. Arsitektur event & approval channel (refactor loop + approver)
- `src/agent/ui.rs` (baru): `UiEvent` — `Token`, `ToolStart{name,args ringkas}`,
  `ToolDone{name,summary,is_error}`, `Step{current,max,context_pct}`,
  `TurnDone{usage}`, `ModelChanged`, `Info`, `Error`. StreamEvent provider
  tetap internal loop, tidak bocor ke UI.
- `src/agent/loop.rs`: inti loop digabung ke `run_turn_inner(..., ui: Option<Sender<UiEvent>>)`.
  `run_turn` lama = sink `None` (cetak stdout/stderr persis seperti Fase 0-3);
  `run_turn_ui` = sink channel (stdout/stderr TIDAK disentuh). Ringkasan tool
  untuk TUI diambil dari hasil yang **sudah teredaksi** (maks 200 char, 1 baris).
  Usage diakumulasi per giliran dan dilaporkan sekali lewat `TurnDone`.
- `src/security.rs`: `Approver` kini enum dua varian — `Stdin` (perilaku lama,
  persis) dan `Channel{tx}` (Fase 4). `ask()` menjadi async: varian Channel
  mengirim `ApprovalRequest{title, detail, reply: oneshot}` dan menunggu
  jawaban UI; UI mati = jawaban false (tolak demi keamanan). auto_yes tidak
  mengirim apa pun di kedua varian. 5 titik panggil di tool (file x2, shell,
  git, web) ditambah `.await`; trait Tool tidak berubah. `Ctx` ditambah
  builder `with_approver`.

### 2. Modul TUI — `src/tui/` (baru, sesuai Blueprint Bagian 4)
- `app.rs`: state murni + reducer (tanpa terminal, bisa dites): transkrip
  berjenis (User/Assistant/Tool/Info/Error), token streaming menempel ke baris
  assistant yang berjalan, ToolStart membuat baris tool `Pending`, ToolDone
  mencocokkan baris pending terakhir bernama sama → `Ok`/`Err` + ringkasan.
  Status: langkah, konteks %, token sesi terakumulasi dari TurnDone.
  Popup approval disimpan sebagai state; `answer_approval` mengirim oneshot.
- `diff_view.rs`: diff → baris Ratatui berwarna + statistik (+n/-n).
  **Keputusan: TANPA syntect** (sesuai arahan Fase 4) — baris `+` hijau,
  `-` merah, header `diff`/`@@` sian tebal, konteks abu; header `+++`/`---`
  tidak dihitung. Badan popup memakai ini bila detail tampak seperti diff,
  selain itu teks apa adanya (mis. perintah shell).
- `mod.rs`: gambar frame + loop UI blocking.
  Layout persis Blueprint Bagian 8: header 1 baris
  (`SelenCode · project: <cwd> · model: <provider>/<model>`), transkrip
  scrollable dengan wrap, status bar 1 baris
  (`konteks N% · token sesi X in / Y out · langkah a/b` + `bekerja…` saat sibuk),
  input 3 baris berbingkai di bawah. Popup approval modal di tengah (70% x 60%)
  dengan petunjuk `[y] setuju [n] tolak`; selama popup tampil input terkunci.
  Tombol: **Enter** kirim, **Alt+Enter** baris baru (input multi-baris
  tui-textarea), **Ctrl+C** keluar, **PageUp/PageDown** gulir 1 halaman,
  **roda mouse** gulir 3 baris. Perintah garis miring Fase 3 (`/model`,
  `/model <nama>`, `/sessions`, `/resume` info sesi, `/exit`) bekerja dari
  input TUI memakai parser `repl.rs` yang sama.
  Terminal dijaga: raw mode + alternate screen + mouse capture dipasang di
  awal; dipulihkan oleh guard Drop di semua jalur keluar DAN oleh panic hook
  yang memulihkan dulu baru meneruskan panic.

### 3. Pemilihan mode — `src/main.rs`
- Mode interaktif memakai TUI bila stdin DAN stdout adalah TTY
  (`std::io::IsTerminal`); bukan TTY (pipa/tes) → REPL polos lama.
  Flag global baru **`--plain`** memaksa mode polos. `selen resume` juga
  masuk TUI bila TTY. `selen run` tidak berubah (selalu polos).
- Worker: Session dimiliki task Tokio `tui_worker` yang menerima
  `TuiCommand` (Prompt/ShowModels/SwitchModel/ShowSessions/ShowSessionInfo/
  Shutdown) dan mengirim `UiEvent`; UI berjalan di `spawn_blocking`.
  Giliran yang gagal mengirim `Error` + `TurnDone{None}` agar UI lepas sibuk.
- Refactor pendukung: `Session::print_models/print_sessions` kini dibangun
  di atas `models_text()/sessions_text()` (1 sumber teks untuk polos & TUI);
  catatan resume disimpan sebagai field `resumed_note` (mode polos mencetak,
  TUI menjadikannya baris Info awal) — tidak ada lagi println di tengah
  `Session::build`. Token sesi awal (resume) dibaca dari store untuk status bar.

### 4. Dependensi baru
`ratatui 0.29`, `crossterm 0.28`, `tui-textarea 0.7` (persis arahan).
Tidak ada dependensi lain yang ditambah.

## Hasil tes

```
cargo test → 35 passed, 0 failed  (28 lama tetap hijau + 7 baru)
  tests/tui_app.rs  7 lulus:
    - reducer: Token x2 menyatu 1 baris, ToolStart/ToolDone berpasangan (Ok),
      TurnDone mengakumulasi token sesi 100/40, langkah & konteks tercatat
    - reducer: ToolDone error -> keadaan Err; ModelChanged mengganti header
    - popup approval: receive -> tampil; jawab 'n' -> oneshot false & popup
      hilang; jawab 'y' -> true; answer tanpa popup aman (false)
    - approver channel roundtrip: ask() mengirim ApprovalRequest & menerima
      jawaban; auto_yes channel langsung setuju tanpa mengirim apa pun
    - render TestBackend 80x24: buffer memuat "SelenCode",
      "model: router/model-x", "token sesi 0 in / 0 out", "langkah 0/25"
    - render popup: buffer memuat "Persetujuan: edit_file a.rs" & "[y] setuju"
    - diff_view: + hijau / - merah tercek dari gaya span, hitung +2/-1 benar,
      header +++/--- tidak dihitung, looks_like_diff membedakan teks biasa
Smoke test (manual, tercatat di sini):
  - Non-TTY: `printf '/model\n/sessions\n/exit\n' | selen --plain` DAN tanpa
    --plain (pipa) -> REPL polos menjawab /model & /sessions, /exit bersih (0).
  - PTY sungguhan (python pty, winsize 80x24): TUI menggambar header
    "SelenCode" + status "token sesi", mengetik /exit + Enter keluar status 0
    dan terminal pulih. Catatan: PTY berukuran 0x0 menggambar frame kosong
    (wajar — tidak ada area), input tetap bekerja.
cargo build --release → hijau, tanpa warning.
```

## Ukuran binary

- Release: `target/release/selen` = **7.592.280 byte (≈7,2 MB)** — naik dari
  7.239.544 byte (Fase 3); kenaikan = ratatui + crossterm + tui-textarea.
  Masih 1 binary tanpa runtime.

## Bug / refactor berisiko yang ditemukan

- Tidak ada bug perilaku yang ditemukan di kode lama; semua 28 tes lama
  lulus tanpa diubah logikanya (yang berubah hanya `.await` di titik panggil
  `ask` dan literal yang memang terdampak refactor internal).
- Titik rawan yang dijaga tes: pasangan ToolStart/ToolDone (dicocokkan dari
  belakang berdasarkan nama + keadaan Pending) dan pemulihan terminal
  (guard Drop + panic hook, diverifikasi lewat smoke PTY keluar bersih).
- `Approver::ask` menjadi async adalah perubahan tanda tangan internal
  (bukan trait publik Blueprint) — disengaja agar varian channel bisa
  menunggu jawaban UI tanpa memblokir thread.

## Cara mencoba

```bash
./target/release/selen            # di terminal beneran -> TUI
./target/release/selen resume     # TUI + sesi terakhir folder ini
./target/release/selen --plain    # paksa REPL polos
printf '/exit\n' | ./target/release/selen   # pipa -> otomatis polos
```

## Belum dikerjakan (Fase 5+)

- Fase 5: tree-sitter / tool `outline` · Fase 6: plan mode & subagent ·
  Fase 7: MCP
- Build rilis Termux `aarch64-linux-android` via GitHub Actions (belum)
- Pewarnaan sintaks kode di transkrip (syntect) — ditunda; diff sudah cukup
  dengan gaya Ratatui
- Approval `fetch_url` per domain per sesi (masih per panggilan)
- Tes ke provider sungguhan (belum — tanpa API key; TUI diuji lewat
  reducer/render/smoke, loop diuji provider bohongan seperti fase sebelumnya)
- Belum ada git repo (sengaja, sesuai instruksi)
