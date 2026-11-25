> Catatan: project ini bernama SelenCode (binary: selen) sejak 24 Nov 2026.

# Hasil Fase 6 — SelenCode (plan mode & subagent)

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-5.md`. Trait `Provider` dan `Tool` (Blueprint Bagian 5)
TIDAK diubah. Tidak ada API key asli dipakai; tidak ada panggilan provider sungguhan.

## 1. Plan mode — `src/agent/plan.rs` (baru)

- `TurnMode { Execute, Plan }` kini parameter loop. Daftar tool baca-saja
  terpusat `READ_ONLY_TOOLS` = list_files, read_file, search_text, outline,
  git_status, git_diff, fetch_url. `update_plan` & `delegate` SENGAJA di
  luar daftar (lihat keputusan di bawah).
- Penegakan DUA lapis di `run_turn_inner` (loop.rs):
  1. ToolSpec yang dikirim ke provider di mode Plan = `registry.specs_where(is_read_only)`
     — tool menulis bahkan tidak ditawarkan.
  2. Bila model tetap memanggil tool menulis (halusinasi), loop menolaknya
     dengan ToolResult error "tool X tidak tersedia di plan mode" TANPA
     menjalankannya (registry tidak disentuh).
- Setiap giliran plan menyisipkan pesan sistem `PLAN_NOTE`: agent wajib
  menjelajah seperlunya lalu menutup dengan rencana langkah bernomor di
  bawah judul persis `## Rencana`.
- `extract_plan(text) -> Option<String>` (murni): dari penanda `## Rencana`
  sampai akhir; tanpa penanda -> seluruh jawaban dianggap rencana; teks
  kosong -> None.
- Cara masuk:
  - Flag global `--plan`: giliran pertama mode interaktif polos, dan
    `selen run --plan "..."` = 1 giliran plan lalu berhenti, tanpa eksekusi.
    (Perilaku di TUI: lihat catatan di bawah.)
  - Perintah `/plan` (SlashCommand::Plan baru di repl.rs): giliran
    berikutnya mode Plan. Sesudah giliran plan selesai & rencana
    terekstrak, user ditanya persis `Jalankan rencana ini? [y/N]`:
    - polos: dibaca dari stdin sesudah giliran plan.
    - TUI: worker mengirim `UiEvent::PlanReady(plan)`; App menyimpan
      `pending_plan` dan input berikutnya harus `y`/`n` di kotak input
      (`y` -> `TuiCommand::ExecutePlan`, `n` -> buang; teks lain ditolak
      dengan pesan supaya konfirmasi tidak terlewat diam-diam).
  - Bila disetujui, giliran eksekusi berjalan dengan prompt
    `Rencana yang disetujui:\n<rencana>\nJalankan langkah demi langkah.`
    (`approved_plan_prompt`, fungsi murni). Bila ditolak, rencana dibuang.

Catatan TUI + `--plan`: flag `--plan` pada mode interaktif TUI tidak
memaksa giliran pertama (worker TUI mulai dari perintah user); di TUI
gunakan `/plan`. Di mode polos flag itu aktif penuh.

## 2. Tool `update_plan` — `src/tools/plan.rs` (baru)

- Args `{steps: [{text, status}]}` — daftar menggantikan state sebelumnya
  secara penuh; status divalidasi pending|in_progress|done (default pending).
- State bersama di `Ctx.plan_state: Arc<Mutex<Vec<PlanStep>>>` (field baru
  di Ctx; Ctx tetap Clone dengan state yang sama untuk seluruh sesi).
- Mengembalikan checklist ter-render: `[x]` done, `[>]` in_progress,
  `[ ]` pending (`render_plan`, fungsi murni).
- Loop menampilkan checklist setiap update berhasil: polos dicetak ke
  stdout; TUI lewat `UiEvent::PlanUpdated` -> baris `LineKind::Plan`
  (magenta) di transkrip. needs_approval = false.
- Hanya mode eksekusi: tidak ditawarkan & ditolak di plan mode (di plan
  mode rencana masih berupa teks bebas di bawah `## Rencana`).

## 3. Subagent & tool `delegate`

- `src/agent/subagent.rs` (baru): `run_subagent(provider, registry, ctx, task)`.
  Anak = konteks BARU: system prompt khusus subagent baca-saja, Ctx anak
  tanpa store/sesi (anak tidak menulis sesi sendiri), approver auto yang
  tidak pernah ditanya (registry anak tak punya tool ber-approval),
  max_steps anak 10. Loop anak memakai jalur UiEvent dengan receiver
  dibuang (`run_turn_counted` baru di loop.rs — varian yang juga
  mengembalikan usage giliran), jadi keluaran anak paralel tidak pernah
  mengacak stdout induk.
- `src/tools/delegate.rs` (baru): tool `delegate {tasks: [{prompt}]}`,
  1-5 tugas (0 / >5 ditolak sebagai error tool). Konstruktor publik
  `Delegate::new(provider, child_registry, max_parallel)` agar bisa
  dibangun & dites terpisah dari main.
- Paralel: `futures::future::join_all` + `tokio::sync::Semaphore`
  ([agent] max_parallel_subagents, default 3). Hasil digabung dengan
  urutan SESUAI urutan input (bukan urutan selesai):
  `### Tugas N: <prompt singkat>` + jawaban anak. Kegagalan 1 anak menjadi
  bagian `ERROR: subagent gagal: ...` di slotnya; tugas lain tetap jalan.
- Registry anak = `tools::read_only_registry()` (persis READ_ONLY_TOOLS;
  ada tes yang mengunci kesetaraannya) — tanpa delegate & update_plan,
  jadi rekursi mustahil secara konstruksi.
- Agregasi usage (keputusan): usage tiap anak dicatat oleh tool delegate
  ke store sesi induk (`record_usage` atas session_id aktif) — total sesi
  & ringkasan token induk ikut menghitung anak. Di luar sesi persisten
  (tes / tanpa store) usage anak tidak dicatat ke mana pun.
- Wiring (keputusan): delegate dibangun di `Session::build` dan
  didaftarkan ke registry induk untuk SEMUA jalur (run, polos, TUI).
  Provider yang dipakai delegate adalah provider sesi SAAT BUILD —
  mengganti provider dengan `/model` di tengah sesi tidak mengganti
  provider subagent (baru berlaku di sesi berikutnya). Dicatat apa adanya.
- System prompt induk ditambah 2 arahan: update_plan untuk pekerjaan
  besar, dan delegate untuk penjelajahan independen paralel (bukan untuk
  menulis).

## 4. Config & dokumentasi

- `[agent] max_parallel_subagents = 3` (default bila absen) di
  config.example.toml; AgentCfg diperbarui.
- README: baris perintah `/plan`, tool update_plan & delegate, subbab
  plan mode, daftar "Belum dikerjakan" kini Fase 7.

## Hasil tes

```
cargo test → 52 passed, 0 failed  (43 lama tetap hijau + 9 baru)
  tests/fase6.rs  9 lulus:
    - plan_mode_filters_specs_and_blocks_writes: provider bohongan
      mencatat specs — write_file/edit_file/run_shell/git_commit/
      update_plan/delegate TIDAK pernah ditawarkan; panggilan write_file
      model ditolak dengan ToolResult "tidak tersedia di plan mode" dan
      file benar-benar TIDAK terbuat di disk
    - extract_plan_rules: penanda / tanpa penanda / kosong
    - update_plan_renders_and_updates_state: checklist [x]/[>]/[ ]
      benar, state tersimpan, panggilan kedua memperbarui
    - delegate_runs_parallel_in_input_order: 3 tugas, hasil gabungan
      memuat ketiga temuan dengan urutan input
    - delegate_children_are_read_only: anak yang mencoba write_file
      ditolak (tool tak dikenal di registry anak) & file tidak terbuat
    - delegate_rejects_bad_task_counts: 0 tugas & 6 tugas ditolak
    - read_only_registry_matches_plan_list: registry anak == READ_ONLY_TOOLS
    - slash_plan_parses: /plan -> SlashCommand::Plan
    - tui_reducer_plan_events: PlanUpdated -> baris LineKind::Plan;
      PlanReady -> pending_plan terisi & busy lepas
cargo build --release → hijau (66 dtk, CARGO_BUILD_JOBS=2), tanpa warning.
Smoke non-TTY: REPL polos dengan config dummy menjawab /plan dengan
"Mode plan aktif untuk giliran berikutnya", /exit keluar 0.
```

## Ukuran binary

- Release: `target/release/selen` = **12.609.816 byte (≈12,0 MB)** — naik
  hanya ±80 KB dari Fase 5 (12.530.040 byte); plan/subagent murni logika
  Rust tanpa dependensi baru (futures & tokio semaphore sudah ada).

## Kesulitan wiring yang ditemui

- Loop lama mengembalikan String saja; subagent butuh usage per giliran
  tanpa mencetak ke terminal -> `run_turn_inner` kini mengembalikan
  `(String, Option<Usage>)` secara internal dan semua varian publik lama
  mempertahankan tanda tangan persis (tes lama tidak berubah).
- Satu-satunya error compile: lupa import `Registry` di delegate.rs —
  diperbaiki sebaris. Selebihnya hijau sekali jalan.

## Belum dikerjakan (Fase 7)

- Fase 7: MCP (klien stdio) + rilis Termux `aarch64-linux-android` via
  GitHub Actions (belum)
- Konfirmasi rencana di TUI memakai input y/n di kotak input, bukan
  popup modal khusus (mengikuti mekanisme yang ada sesuai scope)
- Provider subagent delegate mengikuti provider saat sesi dibangun
  (bukan provider aktif bila diganti di tengah sesi)
- Tes ke provider sungguhan (belum — tanpa API key, sesuai instruksi)
