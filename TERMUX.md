# SelenCode di Termux (Android)

Cara memasang binary `selen` hasil GitHub Actions di Termux dan menjalankan
Selena di HP. Sesuai Blueprint Bagian 11: Termux memakai bionic libc, jadi
yang dipakai adalah binary **aarch64-linux-android** dari GitHub Releases —
bukan binary Linux biasa.

## 1. Persiapan di Termux

```bash
pkg update
pkg install git ripgrep          # tool yang mungkin dipakai agent
```

## 2. Pasang binary dari GitHub Releases

### Cara cepat (disarankan — 1 perintah)

```bash
curl -fsSL https://raw.githubusercontent.com/bagusdanur/SelenCode/main/install.sh | sh
```

Skrip otomatis mendeteksi Termux arm64, mengunduh `selen-termux-arm64` dari rilis terbaru GitHub, memasangnya ke `$PREFIX/bin/selen`, dan memverifikasi `selen --version`.

### Cara manual

1. Buka halaman **Releases** repo SelenCode di GitHub dari HP.
2. Download aset **`selen-termux-arm64`** (jangan yang `selen-linux-x64`).
3. Di Termux, dari folder tempat file itu terdownload (biasanya
   `~/storage/downloads` — jalankan `termux-setup-storage` dulu bila belum):

```bash
chmod +x selen-termux-arm64
mv selen-termux-arm64 $PREFIX/bin/selen
selen --version
```

## 3. Config + API key

Salin contoh config dari repo ini (`config.example.toml`) ke config global:

```bash
mkdir -p ~/.config/selencode
# salin isi config.example.toml ke file ini (edit sesuai kebutuhan):
nano ~/.config/selencode/config.toml
```

Config bawaan contoh memakai provider utama **router (9Router)**:

```toml
default_provider = "router"

[providers.router]
kind = "openai"
base_url = "https://router.ryukomik.com/v1"
model = "ag/gemini-3-flash"
api_key_env = "SELENCODE_ROUTER_KEY"
```

API key TIDAK ditulis di config — dibaca dari environment variable:

```bash
export SELENCODE_ROUTER_KEY="key-kamu"
# agar permanen, taruh baris yang sama di ~/.bashrc:
echo 'export SELENCODE_ROUTER_KEY="key-kamu"' >> ~/.bashrc
```

## 4. Pakai

```bash
cd ~/folder-project-kamu
selen run "jelaskan project ini"   # sekali jalan, output polos
selen                               # mode interaktif (TUI; /exit untuk keluar)
selen resume                        # lanjutkan sesi terakhir folder ini
```

## 5. Update

Mulai v0.1.2 cara update utama cukup satu perintah (tidak perlu download
manual dari browser lagi):

```bash
selen update
```

Ia mengambil rilis terbaru dari GitHub, dan bila versinya lebih baru dari
yang terpasang, binary `selen` ditimpa otomatis. Jalankan `selen` lagi
sesudahnya untuk memakai versi baru.

## Catatan

- Sesi tersimpan di `~/.local/share/selencode/selen.db` (SQLite lokal) —
  resume tetap jalan sesudah Termux ditutup paksa.
- Jalur cadangan bila suatu saat binary rilis bermasalah: compile di dalam
  Termux langsung — `pkg install rust` lalu `cargo build --release` di
  folder project ini. Lambat (bisa 20–40 menit di HP), tapi realistis
  karena SelenCode memakai rustls, bukan OpenSSL.
