---
name: rilis-github
description: Prosedur rilis versi SelenCode, verifikasi CI, & tag GitHub
version: 1.0.0
tags: [release, github, ci, rust]
---

# Playbook Rilis GitHub SelenCode

Gunakan langkah ini setiap kali menyiapkan rilis versi baru di repository SelenCode:

1. **Verifikasi kondisi kerja**: jalankan `git status` dan pastikan tidak ada perubahan tak terduga atau file rahasia (`.env`, API key) yang ikut ter-stage.
2. **Bump versi**: perbarui `version` di `Cargo.toml` dan entri paket `selencode` di `Cargo.lock` secara seragam (SemVer: `0.x.y`).
3. **Commit & push ke `main`**: buat commit ringkas (`chore(release): bump version to vX.Y.Z`) lalu push ke `origin main`.
4. **Wajib tunggu CI hijau**: pastikan workflow CI pada **Linux x64** dan **Windows x64** berstatus `success` sebelum membuat tag rilis.
5. **Buat & push tag rilis**: buat annotated tag `git tag -a vX.Y.Z -m "..."` lalu `git push origin vX.Y.Z` untuk memicu workflow Release (Linux x64, Termux arm64, Windows x64).
6. **Verifikasi aset rilis**: cek halaman GitHub Release memastikan ketiga binary terbit utuh, lalu jalankan `selen update` untuk memastikan self-updater bekerja.

Lihat `references/checklist-rilis.md` lewat `skill_view("rilis-github", "references/checklist-rilis.md")` untuk daftar periksa detail sebelum publikasi.
