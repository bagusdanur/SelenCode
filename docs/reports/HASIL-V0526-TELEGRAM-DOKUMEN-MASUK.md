# Laporan Rilis SelenCode v0.5.26 — Gateway Telegram: Terima Dokumen/Berkas Masuk

- **Versi rilis:** `v0.5.26`
- **Jumlah butir:** 8
- **Catatan asal mekanisme:** Fitur ini merupakan mekanisme SelenCode sendiri (perluasan jalur foto Telegram SelenCode yang sudah ada di `src/telegram.rs` dan `src/app/telegram_runner.rs`).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Struct `TgDocument` (`file_id`, `file_name`, `mime_type`, `file_size`) dan field `document` (serta field penanda media belum didukung) ditambahkan ke `TgMessage` di `src/telegram.rs` beserta `file_size` opsional pada `TgFileInfo`.
- Butir 2 — LULUS: Varian `IncomingTgPayload::Document` ditambahkan sejajar `Photo` di `src/app/telegram_runner.rs` dengan pemeriksaan whitelist `allowed_users` identik dan caption bawaan netral `DEFAULT_TG_DOCUMENT_CAPTION`.
- Butir 3 — LULUS: Unduhan dokumen dijalankan di task chat (bukan loop polling) dan disimpan lewat `save_tg_document_bytes` ke `.selencode/tg_uploads/tg_<message_id>_<nama_aman>` dengan sanitasi penghapusan traversal (`..`, `/`, `\`, prefix drive, dotfile rahasia) dan ekstensi MIME bawaan.
- Butir 4 — LULUS: Pagu keras 20 MB (`TG_DOCUMENT_MAX_BYTES`) ditegakkan baik sebelum `getFile` maupun sesudah unduh byte; berkas > 20 MB atau galat unduh/simpan dibalas satu baris singkat tanpa menyimpan berkas dan tanpa menjalankan giliran agent.
- Butir 5 — LULUS: Dokumen bergambar (`image/*`) dibentuk menjadi `@<path> <caption>` agar dimuat sebagai `Block::Image` sejajar jalur foto, sedangkan dokumen non-gambar dibentuk dengan nama asli, path relatif, arahan baca tool, dan caption (terbukti dibaca agent lewat `read_file` pada uji otomatis).
- Butir 6 — LULUS: Berkas dokumen masuk disimpan pada folder tunggal `.selencode/tg_uploads/` yang sama dengan foto sehingga ikut mekanisme retensi & pemangkasan gateway v0.5.10 tanpa folder/antrean kedua.
- Butir 7 — LULUS: Pagar keamanan ditegakkan — unduhan hanya dilakukan sesudah lolos whitelist, target simpan diverifikasi ketat berada di dalam `.selencode/tg_uploads/`, dan gateway tidak pernah mengeksekusi berkas secara otomatis (berkas diperlakukan murni sebagai data).
- Butir 8 — LULUS: Pesan masuk jenis belum didukung (`voice`, `audio`, `video`, `video_note`, `sticker`, `animation`, `location`, `contact`, dll.) tidak lagi dibuang diam-diam melainkan dibalas satu baris penjelas singkat lewat `classify_tg_message`.

---

## Ringkasan Perubahan Kode

- `src/telegram.rs`: menambah `TgDocument`, field `document` + penanda media belum didukung pada `TgMessage`, `file_size` pada `TgFileInfo`, konstanta `TG_DOCUMENT_MAX_BYTES` (20 MB) & `DEFAULT_TG_DOCUMENT_CAPTION`, serta fungsi murni `classify_tg_message`, `is_tg_image_document`, `sanitize_tg_document_rel_path`, `save_tg_document_bytes`, `format_tg_document_too_large_reply`, `format_tg_document_turn_prompt`, dan `format_unsupported_tg_message_reply`.
- `src/app/telegram_runner.rs`: menambah `IncomingTgPayload::Document`, memindahkan pencocokan pesan masuk ke `classify_tg_message`, menegakkan whitelist + cek awal 20 MB sebelum spawn task chat, membalas 1 baris untuk jenis belum didukung, serta mengunduh & menyimpan dokumen di task chat (dengan balasan 1 baris jika gagal/melebihi 20 MB).
- `tests/v0526_telegram_document_ingress.rs`: uji otomatis untuk kriteria 1–5 (parsing `TgDocument`/`Photo`/`Unsupported`, baca isi dokumen teks oleh agent via `read_file`, dokumen `image/png` dimuat sebagai `Block::Image`, penolakan > 20 MB dengan 0 berkas ditulis, serta sanitasi nama berkas berbahaya `../../etc/passwd`, `..\cmd.exe`, `C:\...`, `.env`).

---

## Tabel Regresi

- Tidak diukur — Kriteria 6 (berkas nyata ke @SelenCodeBot di produksi): verifikasi pemilik di produksi sesudah box naik ke v0.5.26.

| Metrik | Sebelum (v0.5.25) | Sesudah (v0.5.26) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 57 suite (262 tes lulus) | 58 suite (265 tes lulus, 0 gagal) | LULUS |
| Unit & integration test khusus v0.5.26 (`v0526_telegram_document_ingress`) | 0 tes | 3 tes (3 lulus, 0 gagal) | LULUS |
| Batas unduhan dokumen Telegram (`TG_DOCUMENT_MAX_BYTES`) | 0 (dibuang senyap) | 20971520 byte (20 MB) | LULUS |
| Kriteria 1 — parse Document + balasan 1 baris pesan suara/stiker belum didukung | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 2 — dokumen teks tersimpan di `.selencode/tg_uploads/` & isi dibaca agent | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 3 — dokumen gambar (foto sebagai file) ditanggapi sebagai gambar | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 4 — berkas > 20 MB dibalas batas ukuran, 0 berkas tersimpan, 0 giliran agent | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 5 — nama berkas `../` / pemisah path tersimpan aman di dalam `tg_uploads` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 6 (berkas nyata ke @SelenCodeBot di produksi) | verifikasi pemilik di produksi | verifikasi pemilik di produksi | verifikasi pemilik di produksi |
| Kriteria 7 — regresi jalur foto, A+B <= 5.000, resume E <= 7.500, penutup tunggal | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |

---

## Tidak Diubah

- Jalur foto Telegram yang sudah ada (`IncomingTgPayload::Photo`, batas 6 MB, format `@.selencode/tg_uploads/tg_<id>.<ext>`) tidak diubah perilakunya.
- Cakupan v0.5.7–v0.5.25 (penutup tunggal tanpa loop self-correction, gerbang verifikasi + `curl` lokal, Selena improvement + staging + `skills/improvement/` + anti-ganda + sinyal cooldown + pagu 5.000, dedup verifikasi, MCP, Moonlit TUI) tidak disentuh.
- Penanganan pemrosesan pesan suara, video, stiker, serta album/media-group sengaja tidak dibangun sesuai batasan cakupan brief v0.5.26.
- Pengujian penerimaan live (kriteria 1–5 & 7) diserahkan ke Muse, dan verifikasi produksi nyata di @SelenCodeBot (kriteria 6) diserahkan ke pemilik.
