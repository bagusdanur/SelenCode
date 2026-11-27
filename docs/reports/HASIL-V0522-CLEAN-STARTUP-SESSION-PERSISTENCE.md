# Laporan Rilis SelenCode `v0.5.22` — Startup Bersih & Persistensi Sesi CLI/Telegram

- **Versi rilis:** `v0.5.22`
- **Jumlah butir:** 3
- **CI terverifikasi:** [Run 46025701879](https://github.com/bagusdanur/SelenCode/actions/runs/46025701879) (`Cargo Test & Visual Render (Linux)` = `success`, `Cargo Test & Check (Windows x64)` = `success`)

## Ringkasan Vonis

- Butir 1 — LULUS: Peringatan terminal `[agent] max_steps` warisan dihapus dari stderr dan `startup_notices` di `src/config.rs` sambil tetap menetapkan `agent.max_steps = 0` dan melaporkannya di `selen doctor`, diverifikasi oleh `tests/v0522_session_persistence_and_clean_startup.rs` dan `tests/v0517_resilience_pruning_legacy_steps.rs`.
- Butir 2 — LULUS: Membuka ulang `selen` di terminal (TUI dan `--plain`) kini otomatis melanjutkan sesi aktif terakhir per folder kerja lewat pointer persisten SQLite `cli_last_session` (dengan flag `--new` dan `/new` untuk sesi baru bersih), diverifikasi di `tests/v0522_session_persistence_and_clean_startup.rs`.
- Butir 3 — LULUS: Gateway Telegram (`selen telegram`) kini memulihkan sesi aktif terakhir per `chat_id` dari SQLite `tg_chat_session` sesudah restart proses/server maupun idle eviction, lengkap dengan sanitasi `ToolUse` menggantung pasca-crash via `sanitize_interrupted_messages`, diverifikasi di `tests/v0522_session_persistence_and_clean_startup.rs`.

## Perbaikan 1 — Startup Terminal Bersih Tanpa Peringatan `max_steps` Warisan

- `src/config.rs`: menghapus `emit_legacy_max_steps_notice_once` dan tidak lagi mendorong teks peringatan `Kunci [agent] max_steps = ...` ke `cfg.startup_notices`.
- Config lama (mis. `max_steps = 25` di Termux) tetap diabaikan otomatis menjadi `0` (tanpa batas langkah), tidak ditulis ulang saat `save()`, dan tetap tercatat pada `MaxStepsSource::DeprecatedIgnored` untuk `selen doctor`.

## Perbaikan 2 — Auto-Resume Sesi Terakhir di Terminal (`selen`)

- `src/main.rs` & `src/app/cli.rs`: mode interaktif default (`selen`) otomatis melanjutkan sesi terakhir (`!cli.new`), dengan tambahan flag `--new` untuk memulai sesi baru secara eksplisit.
- `src/store.rs`, `src/app/session.rs`, `src/app/tui_runner.rs`, `src/app/plain_runner.rs`: menyimpan pointer `cli_last_session:<cwd>` di tabel `gateway_state` SQLite saat turn berjalan, saat `/resume` pindah sesi, dan menyimpan penanda `__new__` saat `/new` atau `--new` agar restart sebelum pesan pertama tetap menghormati sesi baru; fallback ke `latest_session(cwd)` bila pointer belum ada atau sesi target telah dihapus.

## Perbaikan 3 — Persistensi Sesi Telegram Gateway Lintas Restart & Idle Eviction

- `src/store.rs`, `src/app/session.rs`, `src/app/telegram_runner.rs`: menyimpan pointer `tg_chat_session:<chat_id>` di SQLite `gateway_state` dan memuat ulang sesi persisten via `Session::build_for_telegram_chat` setiap kali `chat_id` belum ada di cache RAM (pasca-restart gateway/InstaBox atau sesudah `evict_idle_gateway_sessions`).
- Memanggil `sanitize_interrupted_messages` saat pemulihan sesi agar pesan `ToolUse` yang terpotong karena server mati mendadak tidak memicu error API 400.

## Tabel Regresi

| Metrik | Sebelum (`v0.5.21`) | Sesudah (`v0.5.22`) | Status |
| :--- | ---: | ---: | :--- |
| Jumlah notice terminal untuk `max_steps = 25` | 1 | 0 | LULUS |
| Sesi CLI dipulihkan saat buka ulang `selen` | 0 | 1 | LULUS |
| Sesi Telegram per `chat_id` dipulihkan pasca-restart | 0 | 1 | LULUS |
| Kasus uji integrasi baru `v0522` lulus | 0 | 3 | LULUS |
| Job CI hijau (Linux + Windows x64) | 2 | 2 | LULUS |

## Tidak Diubah

- Mode sekali jalan `selen run "..."` serta perintah utilitas (`doctor`, `skills`, `review`, `search`, `watch`, `cron`, `bench`) tetap berjalan terisolasi dan tidak mengubah pointer sesi interaktif CLI maupun Telegram.
- Mekanisme `LoopGuard`, konvergensi, timeout, toolset beku, fuzzy `edit_file`, dan pemrosesan paralel batch dari `v0.5.21` tidak diubah.
- Konfigurasi pengguna dan database SQLite lama tetap kompatibel 100% tanpa migrasi destruktif.
