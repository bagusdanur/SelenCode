> Catatan: project ini bernama SelenCode (binary: selen) sejak 24 Nov 2026.

# Hasil Fase 5 — SelenCode (tree-sitter, tool outline, konteks pintar)

Tanggal: 24 November 2026 · Status: **SELESAI, `cargo test` + `cargo build --release` hijau**
Melanjutkan `HASIL-FASE-4.md`. Trait `Provider` dan `Tool` (Blueprint Bagian 5)
TIDAK diubah. Tidak ada API key asli dipakai; tidak ada panggilan provider sungguhan.

## Versi dependensi final (persis saran Blueprint Bagian 3 — tanpa penyesuaian)

| Crate | Versi terkunci (Cargo.lock) |
|---|---|
| tree-sitter (core) | 0.24.7 |
| tree-sitter-rust | 0.23.3 |
| tree-sitter-python | 0.23.6 |
| tree-sitter-javascript | 0.23.1 |
| tree-sitter-typescript | 0.23.2 |
| tree-sitter-language (transitif) | 0.1.9 |

Kombinasi core 0.24 + grammar 0.23 compile bersama tanpa konflik `LanguageFn`
(keduanya memakai tree-sitter-language 0.1.9). Grammar typescript menyediakan
dua bahasa: `LANGUAGE_TYPESCRIPT` untuk .ts, `LANGUAGE_TSX` untuk .tsx.
Grammar dipilih per ekstensi saat file diproses — tidak ada parse lintas bahasa.

## Yang dikerjakan

### 1. Modul murni `src/outline.rs` (baru)
- `outline_source(source: &str, ext: &str) -> Result<String>` — tanpa sentuh
  filesystem, mudah dites. Sumber kosong -> `Ok("")`. Ekstensi tak didukung ->
  error "ekstensi .X belum didukung outline, pakai read_file".
- Tree ditelusuri generik: node yang cocok pola item bahasa menjadi Item,
  anaknya dicari di dalam node itu (fn di dalam impl/class jadi bersarang,
  indentasi 2 spasi per level); node lain diteruskan telusurannya.
- **Keamanan tanda tangan:** label dibangun dari teks node HANYA sampai awal
  body (field `body`) — isi body tidak pernah masuk outline. Akibatnya
  outline tidak membocorkan isi kode maupun nilai literal (mis. nilai const).
- Parse tidak sempurna (`root.has_error()`): outline parsial tetap keluar +
  baris catatan `(parse tidak sempurna)`; bukan gagal total.

Format keluaran (stabil, 1 item per baris):

```
fn load() -> Config — baris 13-15
struct Config — baris 3-5
impl Config
  fn save(&self) — baris 17-18
```

Tanda tangan diringkas 1 baris dan dipotong maks ~100 karakter.

Item per bahasa:
- **Rust** (.rs): `fn` (termasuk di dalam impl & trait, termasuk
  `function_signature_item`), `struct`, `enum`, `trait`, `impl` (target tipe,
  atau `impl Trait for Type`), `const`, `static`, `type`, `mod`.
  Label const/static/type/mod hanya nama (tanpa nilai/RHS).
- **Python** (.py): `def`, `async def` (method di dalam class bersarang di
  bawahnya), `class` (termasuk basis dalam tanda kurung).
- **JavaScript** (.js/.mjs/.jsx): function declaration (termasuk generator),
  `class` + method-nya, `const`/`let`/`var` yang diisi arrow function atau
  function expression (label `const nama = (args) =>`). Destrukturisasi
  dilewati.
- **TypeScript/TSX** (.ts/.tsx): semua item JS + `interface`, `type` (nama
  saja), `enum`, abstract class.

### 2. Tool `outline` — `src/tools/outline.rs` (baru)
- Args `{path}`, `needs_approval = false`. Deskripsi tool menyebut kapan
  memakainya (struktur sebelum membaca isi) + system prompt (context.rs
  langkah 1) kini mengarahkan: file kode besar -> outline dulu, baru
  read_file bagian yang perlu.
- Batas memakai ulang read_file persis: `resolve_in_sandbox`, penolakan
  file rahasia (`is_secret_file`), batas 100 KB (`MAX_READ_BYTES` dari
  file.rs). File tanpa ekstensi didukung -> error yang menyebut read_file.
- Terdaftar di registry setelah `search_text`; otomatis muncul di daftar
  tool system prompt.

### 3. Konteks pintar — `auto_outline` di `src/agent/context.rs`
- Fungsi murni `auto_outline(cwd, prompt) -> String`: token prompt dipecah
  per spasi, tanda baca di ujung dibuang; token yang ekstensinya didukung
  outline jadi kandidat.
- Maks **3 file** pertama yang benar-benar ada sebagai file di dalam sandbox
  cwd, bukan file rahasia, dan <= 100 KB yang outline-nya terhitung.
  Yang tidak ada / di luar sandbox / rahasia **dilewati diam-diam**.
- Blok hasil: header `[Outline otomatis untuk file yang kamu sebut]` +
  `--- <path> ---` + outline per file; total dipotong maks **6 KB**.
- **Wiring (keputusan):** blok dilampirkan ke **pesan user giliran itu**
  di `run_turn_inner` (loop.rs) — bukan system prompt, karena system prompt
  dibuat sekali di awal sesi sedangkan outline bersifat per-giliran.
  Berlaku untuk mode polos & TUI sekaligus. Pesan yang tersimpan di sesi
  SQLite adalah versi yang sama persis dengan yang dilihat provider.
- **Redact (keputusan):** blok TIDAK melewati `redact_secrets`. Alasannya:
  outline hanya berisi tanda tangan deklarasi (body tidak pernah diambil)
  dan file rahasia sudah ditolak di hulu — tidak ada permukaan rahasia baru.
  (Hasil tool tetap teredaksi terpusat di loop seperti sebelumnya.)

### 4. README
Tabel tool ditambah `outline`; penjelasan konteks pintar; daftar
"Belum dikerjakan" kini Fase 6-7. `config.example.toml` tidak berubah
(tidak ada config baru yang perlu).

## Hasil tes

```
cargo test → 43 passed, 0 failed  (35 lama tetap hijau + 8 baru)
  tests/outline.rs  8 lulus:
    - Rust: const/struct/enum/impl + 2 fn bersarang di impl + fn main —
      nama DAN rentang baris persis
    - Python: class + method + async method bersarang + fungsi bebas
      tidak berindentasi
    - JavaScript: function declaration, class + method, const arrow
    - TypeScript: interface, type, enum, function bertipe
    - ekstensi tak didukung -> error menyebut read_file; sumber kosong -> Ok("")
    - sumber dengan sintaks rusak sebagian -> outline parsial keluar,
      fungsi valid tetap ditemukan, tidak panik
    - tool via registry: hasil benar, needs_approval=false, .env ditolak,
      ../ ditolak, .md error menyebut read_file
    - auto_outline: file disebut & ada -> blok memuat nama fungsi;
      file tidak ada / path ../ / tanpa penyebutan -> blok kosong
cargo build --release → hijau (1m 18s, CARGO_BUILD_JOBS=2).
```

## Ukuran binary

- Release: `target/release/selen` = **12.530.040 byte (≈12,0 MB)** — naik
  dari 7.592.280 byte (Fase 4). Kenaikan ≈4,9 MB adalah 4 parser grammar
  tree-sitter (kode C: rust, python, javascript, typescript+tsx) yang
  ter-link statis. Masih 1 binary tanpa runtime.

## Kesulitan versi/compile yang ditemui

- **Tidak ada.** Kombinasi versi saran blueprint (core 0.24 + grammar 0.23)
  ter-resolve & compile hijau sekali jalan di jobs=2; compile grammar C
  pertama (debug) ±20 detik, build release penuh 78 detik.
- Satu-satunya kegagalan proses: `/usr/bin/time` tidak ada di VM sehingga
  percobaan build pertama tidak jalan sama sekali (bukan error cargo) —
  diulang dengan timer `date` biasa.

## Belum dikerjakan (Fase 6+)

- Fase 6: plan mode & subagent · Fase 7: MCP
- Build rilis Termux `aarch64-linux-android` via GitHub Actions (belum)
- Query tree-sitter (`.scm`) — ekstraksi saat ini penelusuran node manual
  per bahasa; bahasa baru berarti menambah fungsi label baru di outline.rs
- Outline untuk bahasa lain (Go, Java, C, dll) — belum, ekstensi di luar
  daftar ditolak sopan ke read_file
- Tes ke provider sungguhan (belum — tanpa API key, sesuai instruksi)
