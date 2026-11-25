---
name: tinjau-rust
description: Checklist tinjauan kode Rust (keamanan, error, performa, tes)
version: 1.0.0
tags: [rust, code-review, security, performance]
---

# Playbook Tinjauan Kode Rust

Ikuti langkah terstruktur ini saat meninjau perubahan kode Rust sebelum merge atau rilis:

1. **Pahami ruang lingkup diff**: baca perubahan inti dengan `git_diff` atau `read_file` seperlunya tanpa membaca seluruh repo.
2. **Keamanan & batas sandbox**: periksa penanganan path (tolak `..`, path absolut di luar sandbox, symlink escape), redaksi secret, dan tidak ada perintah shell tak tersanitasi.
3. **Penanganan error & panic-safety**: pastikan tidak ada `unwrap()`/`expect()` baru pada input pengguna/jaringan/file; gunakan `?` dengan konteks `anyhow::Context`.
4. **Performa & alokasi**: hindari I/O sinkron di loop event/UI, hindari kloning besar di dalam loop panas, dan pastikan batas ukuran buffer/output terjaga.
5. **Kompatibilitas & pengujian**: pastikan kontrak trait/publik yang ada tidak pecah dan setiap cabang kritis punya unit/integration test yang jelas.

Untuk daftar periksa rinci tiap kategori, muat `references/checklist-rust.md` via `skill_view("tinjau-rust", "references/checklist-rust.md")`.
