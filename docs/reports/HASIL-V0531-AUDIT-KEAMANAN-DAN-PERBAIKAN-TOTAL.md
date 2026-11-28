# Laporan Rilis SelenCode v0.5.31 — Audit Keamanan Total, Perbaikan `/model`, Telegram, Terminal/TUI, Sandbox, SSRF/MCP & Self-Update SHA-256

- **Versi rilis:** `v0.5.31`
- **Jumlah butir:** 4
- **Catatan asal mekanisme:** Seluruh hardening keamanan (sanitasi config lokal, isolasi sandbox/bwrap, redaksi secret, anti-SSRF, verifikasi SHA-256 Rust murni pada self-update, guard modal TUI, dan uji regresi keamanan) merupakan implementasi internal SelenCode tanpa dependensi eksternal tambahan.

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Memperbaiki UX & kebenaran `/model` dan Telegram lintas sesi (`0a8f90c`, `5535823`, `448c14c`), yaitu menolak target `/model` tak dikenal alih-alih membuat model palsu, membuka picker `/model` Telegram langsung pada daftar provider terlebih dahulu, menyinkronkan `default_provider` persisten, mendukung ID model Ollama bertanda titik dua, mengikat tombol `/resume` Telegram pada awalan ID sesi yang stabil, menyertakan `/plan` & `/cd` di menu bot, serta menghormati `/cd` pada media/callback dan mencegah kartu `/plan` ganda.
- Butir 2 — LULUS: Menuntaskan Wave 1 audit keamanan inti (`97d5af8`) pada konfigurasi, sandbox, shell, git/worktree, `send_file`, `delegate`, dan TUI: sanitasi key berbahaya dari `./selencode.toml` untrusted, menghapus mirroring secret global ke config lokal, penyimpanan atomik `0o600`, isolasi `bwrap` (`--unshare-pid/ipc/uts` + `--tmpfs` mask direktori sensitif) & scrubbing env subprocess, hardening `is_secret_file` (termasuk Windows NTFS ADS `:`) dan `resolve_in_sandbox`, penghapusan `cargo check`/`test` dari allowlist readonly, gating approval `send_file` & `run_shell` worktree, validasi argumen `verify`/`git`, dan perbaikan panic UTF-8 pada `parse_porcelain_paths`, `format_sliced_text`, serta `extract_at_query`.
- Butir 3 — LULUS: Menuntaskan Wave 2 audit jaringan, Telegram, supply-chain, dan provider (`f316e43`): pengetatan otorisasi group chat Telegram (`user_id` wajib di `allowed_users`), pembatasan perintah admin `/telegram` dari chat, cooldown anti-flood unauthorized, fix cross-chat IDOR `/cron` & `/review`, gating `!cmd` lewat approval, batas ukuran unduhan & redaksi token bot; perluasan blokir IP SSRF & gating `js_render` headless Chrome pada `web.rs`, batas 8 MiB & non-redirect pada klien MCP, anti-argument-injection & symlink containment pada `skill_installer.rs`, verifikasi host HTTPS GitHub + checksum `SHA256SUMS` Rust murni pada `update.rs` (serta publikasi `SHA256SUMS` di `.github/workflows/release.yml`), dan perbaikan batas 4 breakpoint Anthropic caching, buffer SSE `Vec<u8>` anti-potong UTF-8, disambiguasi `ToolCall.id` Gemini, serta deteksi HTTP `400` di router.
- Butir 4 — LULUS: Menuntaskan Wave 3 audit cron/watch, skills/memory/export, store/LSP, watchdog, Session `/cd` sandbox, TUI modals, dan suite regresi keamanan (`c7b51b3`–`6809eba`): blokir metakarakter & inline shell di cron/watch, approval & sandbox `ref_path` pada skill/memory, redaksi secret sebelum simpan ke SQLite/FTS5 & prompt history, `LIKE` escape & scoping `SessionSearch` ke `cwd`, penguncian `/cd` ke `workspace_root`, guard modal `resume_picker`/`pending_ask` TUI, jam monotonik & bounded recovery pada watchdog, serta penambahan `tests/v0531_full_security_audit.rs` yang lulus 100% di CI Linux & Windows x64.

---

## Ringkasan Perubahan Kode

- `src/config.rs`, `src/security.rs`, `src/sandbox.rs`: sanitasi config lokal untrusted, penyimpanan atomik `0o600`, validasi key TOML, perluasan deteksi file secret (termasuk NTFS ADS `:`), canonical sandbox containment, scoping kategori approval MCP, perluasan `redact_secrets` & `is_forbidden_command`, isolasi namespace/tmpfs `bwrap`, dan env scrubbing subprocess.
- `src/tools/{shell,send_file,delegate,orchestrate,verify,git,file,search,bg,undo,mod,web,watch,memory,skill,session_search}.rs`, `src/worktree.rs`, `src/hooks.rs`, `src/agent/{subagent,context,loop}.rs`: hardening approval, sandbox, argument injection, pipe timeout, SSRF, headless Chrome gating, dan clamp `token_budget`.
- `src/telegram.rs`, `src/app/{telegram_ui,telegram_runner,session,commands,tui_runner}.rs`, `src/tui/{app,mod}.rs`, `src/repl.rs`: perbaikan UX `/model` & `/resume`, hardening otorisasi/IDOR/flood Telegram, penguncian `/cd` ke `workspace_root`, sanitasi pesan interrupted pada `/resume`, dan guard modal TUI.
- `src/providers/{anthropic,openai,gemini,router}.rs`, `src/mcp/client.rs`, `src/skill_installer.rs`, `src/update.rs`, `src/cron.rs`, `src/watch.rs`, `src/skills.rs`, `src/user_memory.rs`, `src/store.rs`, `src/lsp.rs`, `src/gateway_health.rs`: hardening SSE/caching/fallback provider, MCP, installer skill, self-update `SHA256SUMS`, cron/watch, redaksi store SQLite, LSP, dan watchdog.
- `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `tests/v0531_full_security_audit.rs`: bump versi ke `0.5.31`, generator aset `SHA256SUMS` di workflow rilis, dan pendaftaran suite regresi keamanan baru.

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.30) | Sesudah (v0.5.31) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 62 suite (282 tes lulus) | 64 suite (294 tes lulus, 0 gagal) | LULUS |
| Unit & integration test CI Windows x64 (`cargo test`) | 53 suite lulus | 54 suite (265 tes lulus, 0 gagal) | LULUS |
| Uji integrasi keamanan khusus v0.5.31 (`v0531_full_security_audit`) | 0 tes | 1 suite regresi keamanan penuh (lulus di Linux & Windows x64) | LULUS |
| Temuan audit keamanan & bug kritis/tinggi (P0+P1) yang terverifikasi tertutup | 22 temuan terbuka | 0 temuan terbuka (seluruh 22 P0+P1 + 27 P2/P3 ditangani) | LULUS |
| Integritas aset self-update rilis (`SHA256SUMS` + cek header ELF/PE) | 0 verifikasi checksum | 3 binary platform diverifikasi via `SHA256SUMS` | LULUS |

- Tidak diukur — Pengukuran token live A+B / resume E: rilis v0.5.31 berfokus pada hardening keamanan, keandalan, dan perbaikan bug lintas permukaan tanpa menambah tool baru maupun mengubah anggaran token prompt dasar Core Toolset 6 tool.

---

## Tidak Diubah

- Komposisi Core Toolset (6 tool), Resume Core Toolset (5 tool), serta seluruh kontrak perilaku tool publik dan format perintah pengguna yang sudah ada tetap dipertahankan tanpa regresi.
- Kompatibilitas kontrak uji rilis sebelumnya (`v0.5.7`–`v0.5.30`, termasuk `v051_background_review`, `v057_improvements`, `phase3_verify`, `stage5_autonomy`, `v0526`–`v0530`) tetap dijaga lulus penuh di Linux dan Windows x64.
- Persona Selena, skema tema TUI Moonlit, dan seluruh prefix callback inline Telegram (`ap:`, `rv:`, `ask:`, `mdl:`, `res:`, `cfg:`, `crn:`, `skl:`, `pln:`) tetap konsisten.
