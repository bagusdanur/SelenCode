# Laporan Rilis SelenCode `v0.5.23` — Perbaikan MCP Menyeluruh, Header Moonlit Penuh, & Rust Edition 2024

- **Versi rilis:** `v0.5.23`
- **Jumlah butir:** 5
- **CI terverifikasi:** [Run 46033464838](https://github.com/bagusdanur/SelenCode/actions/runs/46033464838) (`Cargo Test & Visual Render (Linux)` = `success`, `Jalankan tes di Windows` = `success`)

## Ringkasan Vonis

- Butir 1 — LULUS: Indeks MCP dipisahkan dari pagu `cap_dynamic_injection` 1.500 token di `src/agent/loop.rs` dan dikelompokkan per server di `src/tools/toolsets.rs` saat >30 tool sehingga 58/58 nama tool MCP lintas server selalu terlihat utuh tanpa terpotong, diverifikasi oleh `tests/v0523_mcp_discovery_guard_and_reconnect.rs`.
- Butir 2 — LULUS: `LoopGuard::record_internal` di `src/agent/corrector.rs` kini mengecualikan pemanggilan batch MCP yang sukses dengan keluaran berbeda dari peringatan `RepeatedCall` palsu sambil tetap menangkap keluaran identik 3x beruntun dan kegagalan berulang, diverifikasi oleh `tests/v0523_mcp_discovery_guard_and_reconnect.rs`.
- Butir 3 — LULUS: Toolset `mcp` aktif otomatis saat `has_mcp == true` pada mode Execute di `src/tools/toolsets.rs` tanpa mewajibkan kata kunci `"mcp"` di prompt, dan `McpClient::reconnect` di `src/mcp/client.rs` mencetak notifikasi pemulihan otomatis serta melacak `reconnect_count()`, diverifikasi oleh `tests/v0523_mcp_discovery_guard_and_reconnect.rs`.
- Butir 4 — LULUS: Header sambutan Moonlit penuh pada `src/tui/mod.rs` kini selalu tampil baik pada sesi baru maupun saat auto-resume / `/resume` (dengan catatan sesi dilanjutkan tetap muncul di bawahnya), diverifikasi oleh `tui_keeps_full_moonlit_welcome_header_even_when_resumed`.
- Butir 5 — LULUS: Paket `selencode` di `Cargo.toml` ditingkatkan ke Rust `edition = "2024"` dengan penyesuaian match ergonomics di `src/agent/corrector.rs` dan pembungkusan `unsafe` untuk `std::env::set_var`/`remove_var` pada berkas tes, lulus seluruh unit & integration tests di Linux dan Windows x64.

## Perbaikan 1 — Indeks MCP Anti-Terpotong Lintas Banyak Server

- `src/agent/loop.rs`: blok `build_mcp_compact_index(registry)` kini disisipkan pada bagian khusus `--- Indeks MCP ---` di luar pemotongan `cap_dynamic_injection(..., dyn_cap)` agar peta folder, indeks skill, dan memori tidak pernah memotong ekor daftar tool MCP.
- `src/tools/toolsets.rs`: menambahkan mode terkelompok per server (`MCP_GROUPED_INDEX_THRESHOLD = 30`) yang menampilkan nama penuh `mcp__<server>__<tool>` secara super ringkas (<2.500 karakter untuk 58 tool).

## Perbaikan 2 — `LoopGuard` Ramah Pekerjaan Batch MCP yang Sah

- `src/agent/corrector.rs`: pada panggilan tool `mcp__*` yang berhasil (`!is_error`) dengan keluaran baru yang berbeda dari panggilan sebelumnya (`!same_as_prev && !is_dedup_stub`), `LoopGuard` tidak lagi menembakkan `LoopWarning::RepeatedCall` palsu saat membuat banyak item berturut-turut (mis. 7 slide PowerPoint).

## Perbaikan 3 — Aktivasi MCP Tanpa Kata Kunci & Notifikasi Auto-Reconnect

- `src/tools/toolsets.rs`: `select_initial_toolsets` mengaktifkan `"mcp"` otomatis di mode `Execute` bila `has_mcp == true` (untuk >15 tool tetap ramping lewat `load_mcp_tools`, untuk <=15 tool langsung siap pakai tanpa mutasi array `tools`).
- `src/mcp/client.rs`: saat `reconnect()` berhasil menyambungkan ulang server yang terputus di tengah sesi, mencetak 1 baris info `[mcp] server '<nama>' terputus — tersambung ulang otomatis` dan menaikkan `reconnect_count`.

## Perbaikan 4 — Header Moonlit Penuh Tetap Tampil Saat Auto-Resume

- `src/tui/mod.rs`: `draw()` selalu membangun kartu sambutan penuh `moonlit::welcome_lines(area.width, &info)` meskipun `app.resumed == true`, sehingga tampilan header atas Moonlit tidak lagi menghilang menjadi 1 baris saat membuka ulang `selen`.

## Perbaikan 5 — Upgrade ke Rust Edition 2024

- `Cargo.toml`: `edition = "2024"`.
- `src/agent/corrector.rs` & 7 berkas tes integrasi: penyesuaian pola pinjam implisit (`Some(f)`) dan blok `unsafe` untuk mutasi variabel lingkungan di tes sesuai standar Rust 2024.

## Tabel Regresi

| Metrik | Sebelum (`v0.5.22`) | Sesudah (`v0.5.23`) | Status |
| :--- | ---: | ---: | :--- |
| Tool MCP terlihat pada indeks gabungan 58 tool (2 server) | 50 | 58 | LULUS |
| False-positive LoopGuard pada 7x `pptx_add_slide` sukses | 5 | 0 | LULUS |
| Aktivasi toolset MCP tanpa kata kunci `"mcp"` di prompt | 0 | 1 | LULUS |
| Baris header sambutan Moonlit saat auto-resume TUI | 1 | 10 | LULUS |
| Edisi Rust paket `Cargo.toml` | 2021 | 2024 | LULUS |

## Tidak Diubah

- Persistensi sesi CLI & Telegram lintas restart serta penghapusan peringatan `max_steps` dari `v0.5.22` tidak diubah.
- Kebijakan approval default `trust = false` untuk server MCP eksternal dan redaksi rahasia terpusat tetap berlaku penuh.
- Array `tools` beku untuk stabilitas prompt cache serta seluruh mekanisme `v0.5.21` tidak diubah.
