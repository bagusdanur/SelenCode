# Hasil — 9Router Jadi Provider Utama + Tes Live Pertama SelenCode

> Catatan: project ini bernama SelenCode (binary: `selen`) sejak 24 Nov 2026.
Tanggal: 24 November 2026 · Status: **SELESAI — tes live pertama LULUS (chat + tool calling)**

## 1. Default provider baru

`config.example.toml` diperbarui (kode Rust tidak diubah sama sekali):

- `default_provider = "router"`
- `[providers.router]`: kind `openai`, base_url `https://router.ryukomik.com/v1`,
  model **`ag/gemini-3-flash`**, api_key_env `SELENCODE_ROUTER_KEY`
- Contoh anthropic/gemini dipertahankan sebagai alternatif sekunder (berkomentar, bukan default).
- README diperiksa: tidak menyebut default provider/model lain (sudah konsisten, tidak diubah).

## 2. Jalur tes di VM ini

VM Muse tidak dijamin egress langsung, jadi tes resmi memakai relay lokal XianScan:

- Service `xianscan-relay` ditemukan **inactive** (unit hilang dari /etc, sisa reset VM).
  Dipulihkan dari `~/workspace/xianscan/systemd-backup/xianscan-relay.service` dengan
  ExecStart disesuaikan ke `~/workspace/xianscan/venv-relay/bin/python` (persis resep
  RESTORE.md), `daemon-reload`, lalu `systemctl start xianscan-relay` → **active**,
  port 127.0.0.1:20129 menerima koneksi. Service `xianscan` TIDAK disentuh.
- Base URL tes (relay): `http://127.0.0.1:20129/v1`, model `ag/gemini-3-flash`.
- Key: key dari DB XianScan (tidak ditampilkan). Diambil saat runtime oleh peluncur
  Python di /tmp dari tabel **`ai_providers`** (kolom `api_key`, baris `id='custom' AND is_default=1`)
  di `~/.local/share/xianscan/data/xianscan.db` — pola yang sama seperti Mode 2.
  Key hanya hidup di memori proses peluncur → env proses `selen`; tidak ditulis ke file mana pun.
- Folder tes `/tmp/selen-live/` (halo.txt + selencode.toml lokal + test.db sesi tes,
  `SELENCODE_DB` diarahkan ke sana agar DB sesi asli tidak kotor). Sesudah tes,
  folder diperiksa: **tidak ada file yang mengandung pola key**.

## 3. Hasil tes live

### Tes A — chat polos ✅
`selen run "Jawab persis satu kata: PONG"` → jawaban model: **PONG** (±2,8 detik, exit 0).

### Tes B — tool calling live ✅ (percobaan pertama)
`selen run "Baca file halo.txt dan sebutkan isinya persis"` (±4,8 detik, exit 0).
Output aktivitas tool yang ditampilkan selen:

```
[tool] list_files {}
[tool] read_file {"path":"halo.txt"}
```

Jawaban model memuat isi file **persis**: `isi-rahasia-tes-123`.
Terbukti model benar-benar memanggil tool, bukan menebak.

### Tes C — jalur langsung tanpa relay ✅ (opsional, 1x)
Config sama tapi base_url `https://router.ryukomik.com/v1` langsung dari VM:
jawaban **PONG** (±2,1 detik, exit 0). Artinya saat tes ini jalur langsung dari VM
**hidup** — kabar baik untuk pemakaian di laptop/Termux user (di sana jalur langsung
memang jalurnya). Relay tetap jalur resmi VM yang terbukti andal; hasil Tes C bisa
berubah mengikuti keadaan egress VM.

## 4. Tes unit

`cargo test` (CARGO_BUILD_JOBS=2): **52/52 lulus, 0 gagal** — perubahan config contoh
tidak merusak apa pun, sesuai status Fase 6.

## 5. Kesimpulan & sisa

- SelenCode kini terhubung ke provider yang biasa dipakai fhr (9Router +
  ag/gemini-3-flash) sebagai default, dan untuk pertama kalinya terbukti jalan
  end-to-end ke provider sungguhan: streaming chat, tool calling file, sesi SQLite.
- Belum diuji live: adapter Anthropic & Gemini native (Fase 3) terhadap API aslinya
  masing-masing, mode TUI terhadap provider live, dan subagent delegate live —
  semua itu pekerjaan berikutnya bila diinginkan, bukan syarat fase ini.
