# Langkah Rilis GitHub — SelenCode v0.1.0

Semua perintah dijalankan di laptop, di dalam folder project `termux-agent-rust`
(folder yang berisi `Cargo.toml` dan `.github/`).

Sudah disiapkan & diverifikasi oleh Muse (24 Nov 2026):

- `.gitignore` menutup `/target` (6,8 GB hasil build — JANGAN sampai ikut ter-commit),
  `selencode.toml` (config lokalmu), dan `*.env`.
- Tidak ada file `.env`/`.key`/`.pem` di folder project — aman di-commit semua sisanya.
- `config.example.toml` aman: isinya cuma NAMA env (`SELENCODE_ROUTER_KEY`), bukan key-nya.
- Workflow `.github/workflows/release.yml` sudah valid secara struktur.

## Langkah 1 — Buat repo kosong di GitHub

1. Buka https://github.com/new
2. Owner: **bagusdanur**, nama repo: **selencode**
3. Publik atau privat — bebas (Actions jalan di keduanya; repo privat punya kuota menit gratis lebih kecil).
4. JANGAN centang "Add a README", "Add .gitignore", atau lisensi — repo harus benar-benar kosong agar push pertama tidak konflik.
5. Klik **Create repository**.

## Langkah 2 — Push pertama (git CLI)

```bash
cd /path/ke/termux-agent-rust

git init -b main
git add .
git status        # CEK DULU: tidak boleh ada folder target/ di daftar!
git commit -m "SelenCode v0.1.0 — agent terminal Rust untuk Termux (Fase 0-7)"
git remote add origin https://github.com/bagusdanur/selencode.git
git push -u origin main
```

Bila `git status` menunjukkan `target/` akan ikut, BERHENTI — berarti `.gitignore`
tidak terbaca (salah folder). Perbaiki dulu sebelum commit.

Alternatif Langkah 1+2 bila punya GitHub CLI (`gh`):

```bash
cd /path/ke/termux-agent-rust
git init -b main
git add .
git commit -m "SelenCode v0.1.0 — agent terminal Rust untuk Termux (Fase 0-7)"
gh repo create bagusdanur/selencode --public --source=. --push
```

## Langkah 3 — Tag pemicu rilis

```bash
git tag v0.1.0
git push origin v0.1.0
```

Tag `v*` inilah yang menyalakan workflow rilis.

## Langkah 4 — Pantau Actions

1. Buka https://github.com/bagusdanur/selencode/actions
2. Workflow **Release** akan jalan: 3 job — `android`, `linux`, lalu `release`.
3. Perkiraan kasar run pertama: 10-25 menit (NDK di-download + `cargo-ndk` di-compile
   dari nol + tree-sitter C di-compile). Ini estimasi, bukan janji — run berikutnya
   lebih cepat karena cache toolchain.
4. Sukses = job `release` hijau. Gagal paling mungkin di langkah NDK/cargo-ndk —
   salin pesan error job yang merah, kirim ke Muse untuk diperbaiki workflow-nya.

## Langkah 5 — Ambil hasil rilis

1. Buka https://github.com/bagusdanur/selencode/releases
2. Release **v0.1.0** berisi dua aset:
   - `selen-termux-arm64` — untuk HP Android (Termux)
   - `selen-linux-x64` — untuk laptop Linux
3. Install di HP: ikuti `TERMUX.md` di repo (download → `chmod +x` →
   pindah ke `$PREFIX/bin/selen` → buat `selencode.toml` → `export SELENCODE_ROUTER_KEY=...`).

## Rilis berikutnya

Naikkan `version` di `Cargo.toml`, commit, lalu:

```bash
git tag v0.1.1
git push origin v0.1.1
```

Selesai — workflow mengurus sisanya.
