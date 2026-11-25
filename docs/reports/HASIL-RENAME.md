# Hasil Rename — koda → SelenCode

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**

Rename dari nama kerja "koda" ke skema nama final yang disahkan fhr:

## Pemetaan nama

| Aspek | Lama | Baru |
|---|---|---|
| Nama produk | koda | **SelenCode** |
| Persona agent | (implisit "koda" di system prompt) | **Selena** |
| Package / crate | `koda` | `selencode` |
| Library | `koda` (implisit) | `selencode` (dinyatakan eksplisit `[lib]`) |
| Binary / perintah CLI | `koda` | `selen` (`[[bin]] name = "selen"`) |
| Config per project | `./koda.toml` | `./selencode.toml` |
| Config global | `~/.config/koda/config.toml` | `~/.config/selencode/config.toml` |
| Env var contoh | `KODA_ROUTER_KEY` | `SELENCODE_ROUTER_KEY` |
| Sapaan mode interaktif | "koda — mode interaktif..." | "SelenCode — halo, aku Selena. Ketik perintah, /exit untuk keluar." |
| System prompt | "Kamu adalah koda, agent coding..." | "Kamu adalah Selena, agent coding dari SelenCode..." (instruksi kerja tidak diubah) |
| Prefix log loop | `[koda]` | `[selen]` |

## Yang diubah

- `Cargo.toml` — package `selencode`, `[lib] name = "selencode"`, `[[bin]] name = "selen"`, description SelenCode.
- `src/main.rs` — semua `use koda::` → `use selencode::`; Clap `name = "selen"`, about menyebut SelenCode & Selena; contoh prompt kosong `selen run "..."`; banner interaktif Selena.
- `src/config.rs` — pencarian `./selencode.toml` → `~/.config/selencode/config.toml`; pesan error "Config SelenCode tidak ditemukan" + contoh `SELENCODE_ROUTER_KEY`.
- `src/agent/context.rs` — 1 kalimat identitas di system prompt jadi Selena dari SelenCode; instruksi kerja lain tidak diubah.
- `src/agent/loop.rs` — prefix log `[selen]`.
- `tests/*.rs` — semua `koda::` → `selencode::`; prefix folder temp tes & fixture (`selencode-test-`, `tes@selencode.local`, dll) diselaraskan; logika tes tidak diubah.
- `config.example.toml` — header SelenCode, path config baru, `api_key_env = "SELENCODE_ROUTER_KEY"`.
- `README.md` — judul SelenCode, persona Selena, semua contoh perintah/path config diperbarui.
- `BLUEPRINT.md` — HANYA baris nama di bagian awal diganti ke nama final + catatan tanggal rename; isi teknis lain tidak diubah (sebutan "koda" yang tersisa di badan blueprint adalah sisa dokumen awal, sudah diberi catatan penjelasan di baris nama).
- `HASIL-FASE-0-1.md` & `HASIL-FASE-2.md` — TIDAK ditulis ulang (catatan sejarah); masing-masing hanya ditambah 1 baris catatan rename di paling atas.
- Folder project tetap `~/workspace/termux-agent-rust/` (tidak dipindah).

## Verifikasi

- `cargo test`: **19/19 lulus, 0 gagal** (fase2: 5, loop_mock: 3, tools_file: 4, tools_git: 2, tools_shell: 5) — semua tes lama tetap hijau.
- `cargo build --release`: hijau. Binary hasil: `target/release/selen` = **5.178.648 byte (≈4,9 MB)** — sama dengan ukuran Fase 2, rename tidak mengubah ukuran berarti.
- `selen --version` → `selen 0.1.0`
- Smoke test tanpa config (di folder kosong): error menyebut nama baru dengan benar —
  `Config SelenCode tidak ditemukan. Buat salah satu file ini: ./selencode.toml, ~/.config/selencode/config.toml`
- Artefak binary lama `target/release/koda` dihapus agar tidak membingungkan.

## Contoh pemakaian baru

```bash
cp config.example.toml selencode.toml     # per project, didahulukan
# atau: mkdir -p ~/.config/selencode && cp config.example.toml ~/.config/selencode/config.toml
export SELENCODE_ROUTER_KEY="key-kamu"
./target/release/selen --version
./target/release/selen run "jelaskan project ini"
./target/release/selen --yes run "perbaiki bug di src/main.rs"
./target/release/selen                    # mode interaktif, disapa Selena
```

## Sisa kata "koda" yang sengaja dipertahankan

- Badan `BLUEPRINT.md` (contoh Cargo.toml, struktur folder, workflow rilis, checklist) — dokumen perencanaan awal; hanya baris nama + catatan rename yang diubah sesuai instruksi.
- Isi `HASIL-FASE-0-1.md` & `HASIL-FASE-2.md` — catatan sejarah saat project masih bernama koda; sudah diberi baris catatan rename di atasnya.
- `Cargo.lock` diperbarui otomatis oleh cargo (nama package lokal sekarang `selencode`).
