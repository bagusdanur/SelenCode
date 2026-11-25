# Checklist Detail Rilis GitHub

- [ ] `Cargo.toml` dan `Cargo.lock` sudah memakai nomor versi baru yang identik.
- [ ] Seluruh unit test & integration test lulus di CI Linux x64 & Windows x64.
- [ ] `cargo clippy --all-targets -- -D clippy::correctness` bebas error.
- [ ] Tidak ada API key, token Telegram, atau data pribadi di commit diff.
- [ ] Ketiga aset rilis terunggah: `selen-linux-x64`, `selen-termux-arm64`, `selen-windows-x64.exe`.
- [ ] Laporan rilis / changelog mencatat angka pengujian nyata secara jujur.
