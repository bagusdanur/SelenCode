# Checklist Rinci Review Kode Rust

## Keamanan
- Path user dikanonisasi dan diverifikasi tetap di dalam direktori yang diizinkan.
- File rahasia (`.env`, `*.key`, `id_rsa`, token) ditolak dan output diredaksi.
- Timeout jaringan dan proses eksternal selalu terpasang.

## Keandalan & Error
- Operasi UTF-8 memakai `chars()` atau `is_char_boundary`, tidak memotong byte di tengah karakter multibyte.
- `Instant::checked_sub` digunakan alih-alih pengurangan langsung yang bisa panik.

## Pengujian
- Jalankan 1 kali perintah verifikasi (`cargo test` / `cargo clippy`) dan hentikan jika lulus.
