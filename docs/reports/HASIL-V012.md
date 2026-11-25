# HASIL — SelenCode v0.1.2 (TUI Moonlit + `selen update`)

Tanggal: 24 November 2026 · Status: **SELESAI** — `cargo test` 68/68 hijau,
`cargo build --release` hijau tanpa warning, binary 12.807.224 byte
(±12,2 MiB; v0.1.1: ±12,1 MB — nyaris tidak berubah, tanpa dependensi baru).

Catatan versi: Cargo.toml sebelumnya masih `0.1.0` (rilis GitHub v0.1.1
kemarin tidak menaikkan versi crate), jadi versi dilompati langsung ke
`0.1.2` sesuai perintah — `selen --version` kini menjawab `selen 0.1.2`.

## A. TUI Moonlit — modul baru `src/tui/moonlit.rs`

Semua fungsi murni & terpisah dari logika gambar:

- **Palet persis mockup** sebagai konstanta `Color::Rgb`: BG, VIOLET, DEEP,
  LAV, SILVER, DIM, FAINT, GOLD (design/moonlit_mockup.py).
- **`moon_art(cols, rows) -> Vec<String>`** — bulan sabit karakter braille:
  grid titik 2x4 per sel; titik menyala bila di dalam lingkaran besar dan
  di luar lingkaran geser (rumus sama dengan mockup). Sel kosong memakai
  U+2800 agar lebar konsisten. Bintang ✦ ditambahkan sebagai span teks
  terpisah berwarna GOLD di celah kanan-atas bulan (bukan di dalam art).
- **`wordmark() -> Vec<String>`** — "SELENCODE" font piksel 5x7 persis
  tabel PIX mockup, 7 baris x 53 kolom.
- **`welcome_lines(width, info) -> Vec<Line>`** — layar sambutan berbingkai
  sudut membulat (FAINT):
  - **width >= 100 (lebar):** pojok kanan atas "SelenCode v0.1.2 · edisi
    Moonlit"; kiri bulan 22x13; kanan wordmark (baris 0-2 & 5-6 VIOLET,
    3-4 LAV, semua tebal), tagline DIM, baris Selena, baris
    Provider/Model; panel dua kolom "✦ Tools" / "✦ Perintah" (judul GOLD,
    label DEEP tebal, isi SILVER) dengan isi persis mockup; catatan
    "{N} tool aktif · sandbox: folder ini saja" dengan **N dihitung nyata**
    dari `registry.names().len()` di titik wiring (main.rs), bukan
    hardcode; baris penutup SILVER.
  - **width < 100 (HP):** tata letak mockup HP — bulan kecil 14x8 +
    "SELENCODE" teks LAV tebal, info menumpuk (Selena / Provider /
    Folder / Sesi), perintah inti /plan /model /help.
  - Semua baris dijamin <= width (fungsi `fit`/`framed` memotong aman;
    dites pada width 4/10/30/52/99/100/116 tanpa panik/overflow).
- **Status bar Moonlit** (`status_line`): glyph fase bulan dari persen
  konteks — `phase_glyph`: ○ (0-12%), ◔ (13-37%), ◑ (38-62%), ◕ (63-87%),
  ● (88-100%) — VIOLET tebal + label "konteks" + bar 10 sel ▓ (DEEP) /
  ░ (FAINT) + persen; token sesi diformat gaya id-ID (12.480), langkah,
  cwd (LAV), indikator "bekerja…" GOLD.
- **Kotak input:** bingkai digambar sebagai Block luar berwarna DEEP,
  prompt "❯" VIOLET tebal di sel kiri dalam bingkai, placeholder textarea
  "Tanya apa saja…" berwarna DIM.
- **Popup approval:** border + judul GOLD; warna badan diff TETAP
  hijau/merah.
- Warna transkrip diselaraskan: prefix user VIOLET, checklist plan LAV,
  info DIM (sebelumnya Cyan/Magenta/DarkGray).

### Keputusan tata letak (diminta dicatat)

1. **Header lama Fase 4 DIHAPUS total.** Sambutan Moonlit adalah konten
   awal transkrip (dibuat sekali pada frame pertama saat lebar terminal
   diketahui, disimpan di `App.welcome`, ikut scroll seperti baris lain).
   Provider/model pindah ke sambutan; cwd pindah ke status bar. Status
   bar tetap selalu terlihat.
2. **Resume tidak menampilkan sambutan penuh** — hanya 1 baris ringkas
   `resume_lines`: "☾ SelenCode — sesi dilanjutkan · {provider}/{model}".
   Catatan resume lama (judul sesi, tanggal, jumlah pesan) tetap tampil
   sebagai baris Info sesudahnya.
3. Sambutan dibuat dari lebar frame PERTAMA; bila terminal diubah
   ukurannya sesudah itu, sambutan tidak digambar ulang (transkrip lain
   tetap responsif). Diterima untuk v0.1.2.
4. Provider di sambutan lebar tampil sebagai label "nama (host)" —
   mis. `router (router.ryukomik.com)` — dihitung main.rs dari
   `base_url` config (`App.provider_label`); tanpa base_url, nama saja.

### Mode polos

Banner diganti `moonlit::plain_banner`: bulan braille kecil (10x5,
ANSI truecolor violet) + "SelenCode — halo, aku Selena..." + baris
perintah/provider seperti sebelumnya. Sengaja sederhana.

### Bukti visual (wajib, terpasang)

Harness `examples/render_welcome.rs` menggambar frame TUI ASLI ke
TestBackend dan men-dump semua sel (simbol+warna+bold);
`design/render_bukti.py` merender dump itu ke PNG (sel braille digambar
sebagai titik). Hasil:

- `design/bukti-moonlit-wide.png` (116 kolom) — cocok dengan mockup wide:
  sabit titik + bintang, wordmark dua nada, panel Tools/Perintah, status
  bar fase bulan, kotak input ❯.
- `design/bukti-moonlit-hp.png` (52 kolom) — cocok dengan mockup HP.

## B. `selen update` — `src/update.rs` + subcommand clap `Update`

- Fungsi murni (dites): `parse_version` (terima "v0.1.2"/"0.1.2"/sufiks
  "-rc", tolak format aneh), `is_newer` (perbandingan tuple semver),
  `asset_name_for(os, arch)`.
- Alur: GET `https://api.github.com/repos/bagusdanur/SelenCode/releases/latest`
  (reqwest rustls yang sudah ada; User-Agent "selencode", Accept
  `application/vnd.github+json`) → bandingkan `tag_name` dengan
  `env!("CARGO_PKG_VERSION")`. Tidak lebih baru → cetak
  `Sudah versi terbaru (vX).` Lebih baru → unduh aset platform dari
  `browser_download_url` ke `selen.tmp` DI SEBELAH exe aktif
  (`current_exe().with_extension("tmp")`), chmod 0o755 (unix), lalu
  rename menimpa exe aktif (aman di Linux/Termux untuk binary yang
  sedang jalan). Cetak `Diperbarui ke vX — jalankan \`selen\` lagi.`
- Semua error jadi pesan jelas tanpa panic: offline/gagal hubungi
  GitHub, tag tak dikenal, aset tidak ada, platform tidak didukung,
  unduhan putus, gagal tulis/ganti file.
- Repo konstanta `bagusdanur/SelenCode`, bisa dioverride env
  `SELENCODE_REPO` (tes/dev).
- `selen --help` menampilkan subcommand `update`. README dapat bagian
  "## Update" (perintah + alternatif skrip update-selen); TERMUX.md
  menyebut `selen update` sebagai cara update utama mulai v0.1.2.

### Keputusan pemetaan aset (dicatat karena brief ambigu)

Brief menulis `("android"|"linux", "aarch64") -> "selen-termux-arm64"`.
Diimplementasikan sebagai: **("android","aarch64") -> termux**,
**("linux","x86_64") -> linux-x64**, selain itu **None**. Alasan:
binary Termux itu bionic — di Linux glibc aarch64 (mis. Raspberry Pi)
binary itu TIDAK jalan, dan memang tidak ada aset glibc ARM yang
dipublikasikan; mengunduh aset Termux ke sana akan memasang binary
rusak. Di Termux sendiri `std::env::consts::OS` bernilai "android",
jadi jalur nyatanya tidak terpengaruh.

### Batas `selen update`

- Hanya memeriksa rilis **latest** yang stabil (bukan pre-release /
  daftar semua rilis), dan tidak ada verifikasi checksum/tanda tangan —
  aset dipercaya karena datang dari repo sendiri via HTTPS GitHub.
- Tidak bisa mengupdate bila binary berada di lokasi yang tidak bisa
  ditulis user (rename akan gagal dengan pesan error yang jelas).
- Belum dites live ke GitHub dari VM ini (fungsi-fungsi murni teruji;
  jalur jaringan mengikuti pola reqwest yang sama dengan provider).

## C. Tes

- Baru: `tests/moonlit.rs` (6 tes) — moon_art (jumlah baris, lebar
  konsisten, ada sel menyala & kosong, sabit minoritas sel, tepi kiri
  tengah menyala, pojok kanan atas kosong), wordmark (7 baris, lebar
  sama 53), welcome 116 & 52 (semua baris <= width via `Line::width()`,
  memuat SELENCODE/Selena/model/panel/jumlah tool), width ekstrem tidak
  panik, fase bulan 0/25/50/75/100.
- Baru: `tests/update.rs` (3 tes) — parse_version (termasuk "v1.2.3"
  & "1.2.3"), is_newer, asset_name_for (android aarch64, linux x86_64,
  dan yang tidak didukung).
- Disesuaikan: `tests/tui_app.rs::render_satu_frame_memuat_sambutan_dan_status`
  (nama & maksud sama — dulu "header", kini sambutan Moonlit): assert
  SelenCode/SELENCODE/model/status tetap tampil dalam 1 frame.
- **Total: 68/68 lulus** (59 lama tetap hijau + 9 baru). Trait Provider
  & Tool tidak diubah; logika agent/provider/tools tidak disentuh.

## File baru / berubah

- Baru: `src/tui/moonlit.rs`, `src/update.rs`, `tests/moonlit.rs`,
  `tests/update.rs`, `examples/render_welcome.rs`,
  `design/render_bukti.py`, `design/bukti-moonlit-wide.png`,
  `design/bukti-moonlit-hp.png`, `HASIL-V012.md`.
- Berubah: `Cargo.toml` (versi 0.1.2), `src/lib.rs` (mod update),
  `src/main.rs` (subcommand Update, wiring sambutan TUI, banner polos),
  `src/tui/mod.rs` (mod moonlit, draw tanpa header, status/input/popup
  Moonlit), `src/tui/app.rs` (field sambutan + placeholder),
  `tests/tui_app.rs` (1 tes disesuaikan), `README.md`, `TERMUX.md`.

## Kesulitan yang ditemui

1. `str::splitlines()` di Python ikut memotong pada karakter \x1e
   (separator sel dump) — render bukti pertama keluar nyaris kosong;
   diperbaiki dengan split hanya pada \n.
2. Versi Cargo.toml ternyata masih 0.1.0 (bukan 0.1.1 seperti dugaan
   brief) — dilompati ke 0.1.2; konsekuensi: binary v0.1.1 yang sudah
   terpasang user akan melihat v0.1.2 sebagai lebih baru. Normal.
3. Tidak ada kesulitan protokol/kompilasi berarti — `cargo check`
   bersih sejak draf pertama selain hal di atas.
