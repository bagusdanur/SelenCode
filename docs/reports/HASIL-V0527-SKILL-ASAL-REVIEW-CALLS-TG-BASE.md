# Laporan Rilis SelenCode v0.5.27 — Perbaikan Utang: Skill Tulis-ke-Asal + Dua Utang Kecil

- **Versi rilis:** `v0.5.27`
- **Jumlah butir:** 3
- **Catatan asal mekanisme:** Seluruh perbaikan pada rilis ini merupakan mekanisme internal SelenCode sendiri (resolusi store skill di `src/skills.rs` & `src/tools/skill.rs`, pencatatan `review_runs` di `src/store.rs` & `src/agent/review.rs`, serta klien Bot API di `src/telegram.rs`).

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: `skill_edit` (baik edit `SKILL.md` maupun `ref_path`), `manage_skill(action="patch")`, dan jalur Selena improvement review kini menulis kembali langsung ke berkas ASAL tempat skill dimuat (store + path asalnya, tanpa menciptakan salinan bayangan di `.selencode/skills/`) sambil mempertahankan metadata frontmatter `origin`/`source_session`/`learned_at`; salinan timpa proyek hanya lahir dari permukaan eksplisit (`/skills override <nama>` / alias `/skills copy-to-project <nama>`, `skill_create`, dan `/learn`), keterlihatan bayangan proyek-atas-global ditampilkan eksplisit beserta kedua store asalnya di `/skills`, `skills_list`, dan `skill_view`, serta review atas salinan proyek yang sah menandai `(salinan proyek)` pada baris laporan `💾`, catatan cadangan, dan proposal staging.
- Butir 2 — LULUS: Tabel `review_runs` kini mencatat setiap panggilan provider review secara individual (`event_id`, `call_index`, `event_calls`, `call_kind`) lewat `Store::record_review_call` dengan migrasi skema otomatis aman, sehingga peristiwa multi-panggilan (panggilan awal + retry pagar mutu koreksi) tercatat sebagai baris terpisah yang masing-masing memperlihatkan `input_tokens <= 5.000` secara langsung tanpa rekonstruksi dan tanpa menduplikasi laporan `💾` (hanya baris panggilan akhir yang membawa `report_text`).
- Butir 3 — LULUS: Env override khusus pengujian `SELENCODE_TELEGRAM_API_BASE` ditambahkan dan dihormati seluruh jalur keluar `TelegramClient::new` — baik panggilan metode Bot API (`getMe`, `deleteWebhook`, `setMyCommands`, `getUpdates`, `getFile`, `sendMessage`, `sendDocument`, `sendPhoto`, dsb.) maupun unduhan berkas (`download_file_bytes` via `file_base_url`) — sementara saat env absen/kosong perilaku produksi tetap 100% sama ke `https://api.telegram.org`; ditandai eksplisit sebagai fasilitas uji di `config.example.toml` dan `README.md`.

---

## Ringkasan Perubahan Kode

- `src/skills.rs`: menambah `ShadowedSkillInfo` & field `shadows` pada `SkillInfo` beserta `shadows_user_store()` dan perekaman bayangan pada `scan_skills_in_dirs` / `scan_skills_with_warnings` (tanpa mengubah presedensi pemuatan proyek-di-atas-pengguna); menambah penanda `(project; menimpa user)` pada `format_skills_index` khusus skill yang menimpa; mengekstrak `format_skills_list_card` untuk tampilan `/skills` lengkap dengan penanda `MENIMPA` + path kedua store; menambah `patch_resolved_skill` (tulis-ke-asal) dan permukaan eksplisit `copy_user_skill_to_project`.
- `src/tools/skill.rs`: memperbaiki `SkillEdit::run` (`SKILL.md` dan `ref_path`) agar selalu menulis ke `existing.path` / `existing.dir` asal dan mempertahankan frontmatter provenans via `render_skill_markdown_with_origin` (menghapus pembuatan salinan bayangan diam-diam); memperbarui `ManageSkill` `action="patch"` ke `patch_resolved_skill`; menandai override user pada `SkillCreate` dan header `SkillView`.
- `src/app/commands.rs`: memanggil `format_skills_list_card` pada `/skills`, menambah perintah eksplisit `/skills override <nama>` (alias `/skills copy-to-project <nama>`) dengan persetujuan pengguna, menampilkan catatan bayangan di `/skills show <nama>`, serta menampilkan tag `[call i/n <kind>]` pada `/review status`.
- `src/store.rs` & `src/agent/review.rs`: menambah kolom `event_id`, `call_index`, `event_calls`, `call_kind` pada `review_runs` (beserta migrasi `ALTER TABLE` aman di `migrate_review_schema`) dan `Store::record_review_call`; mencatat tiap panggilan review (awal & retry koreksi) sebagai baris individual di `review_runs` dengan `report_text` hanya pada panggilan terakhir; menambah field `scope` pada `AppliedReviewChange` serta label `(salinan proyek)` / `[salinan proyek]` pada laporan `💾`, cadangan `/review undo`, dan proposal staging bila target review adalah salinan proyek eksplisit.
- `src/telegram.rs`, `src/app/telegram_runner.rs`, `config.example.toml`, `README.md`: menambah konstanta `TELEGRAM_API_BASE_ENV = "SELENCODE_TELEGRAM_API_BASE"` & `resolve_telegram_api_urls`, menyimpan `file_base_url` pada `TelegramClient` agar `download_file_bytes` menghormati override yang sama dengan metode Bot API, mencetak baris diagnostik `API Base` saat startup gateway hanya bila env uji diset, serta mendokumentasikannya sebagai fasilitas uji khusus (bukan pemakaian produksi normal).
- `tests/v0527_skill_origin_review_calls_tg_base.rs`: 4 uji integrasi otomatis untuk Masalah 1 (skenario A5 `skill_edit` + `ref_path` + review tulis ke store pengguna tanpa bayangan proyek, serta override eksplisit + keterlihatan `/skills` & `skills_list` + penanda `(salinan proyek)` pada review), Masalah 2 (peristiwa 2-panggilan 3.750 + 3.958 token tercatat 2 baris individual `<= 5.000` dengan 1 baris laporan `💾`), dan Masalah 3 (`SELENCODE_TELEGRAM_API_BASE` melawan server Bot API palsu lokal untuk `getMe`, `getUpdates`, `getFile`, `download_file_bytes`, simpan ke `.selencode/tg_uploads/`, & `sendMessage`, lalu kembali ke `https://api.telegram.org` saat env dihapus).

### Permukaan Eksplisit Salinan Proyek & Env Override
- **Permukaan eksplisit pembuat salinan timpa proyek (override):**
  1. `/skills override <nama>` (alias `/skills copy-to-project <nama>`) — menyalin skill global ke `.selencode/skills/` sesudah persetujuan pengguna.
  2. Tool `skill_create` — membuat skill baru cakupan proyek di `.selencode/skills/` lewat persetujuan + diff (menandai eksplisit di judul persetujuan bila menimpa skill user).
  3. `/learn <nama> <instruksi>` — membuat skill proyek baru secara eksplisit.
- **Nama env override Telegram khusus uji:** `SELENCODE_TELEGRAM_API_BASE` (contoh: `http://127.0.0.1:18081`).

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.26) | Sesudah (v0.5.27) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 58 suite (265 tes lulus) | 59 suite (269 tes lulus, 0 gagal) | LULUS |
| Unit & integration test khusus v0.5.27 (`v0527_skill_origin_review_calls_tg_base`) | 0 tes | 4 tes (4 lulus, 0 gagal) | LULUS |
| Pencatatan `review_runs` pada peristiwa 2 panggilan (3.750 + 3.958 in) | 1 baris agregat (7.708 in) | 2 baris per panggilan (maks 3.958 in <= 5.000) | LULUS |
| Kriteria 1 — skenario A5: aturan keliru hilang di store pengguna, 0 salinan bayangan proyek | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 2 — `/skills` menandai salinan proyek yang menimpa global (penanda + kedua store terlihat) | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 3 — peristiwa review multi-panggilan tercatat per panggilan, masing-masing <= 5.000 | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 4 — gateway dengan `SELENCODE_TELEGRAM_API_BASE` melawan Bot API lokal (dokumen teks tersimpan & dijawab) + normal tanpa env | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 5 — regresi A+B <= 5.000, resume E <= 7.500, presedensi proyek > pengguna, penutup tunggal, nol "Self-correction" | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |

---

## Tidak Diubah

- Utang cache review vs pagu 5.000 token sengaja tidak disentuh (menunggu keputusan pemilik sesuai arahan brief v0.5.27).
- Arsitektur dua store skill dan presedensi pemuatan proyek-di-atas-pengguna tetap utuh seperti v0.5.7–v0.5.26.
- Perilaku produksi gateway Telegram saat `SELENCODE_TELEGRAM_API_BASE` tidak diset tetap mengarah ke `https://api.telegram.org` tanpa perubahan.
- Cakupan v0.5.7–v0.5.26 (Selena improvement + staging + `skills/improvement/`, penutup tunggal, gerbang verifikasi + `curl` lokal, jalur foto & dokumen Telegram) tidak dirusak.
- Uji penerimaan live (Kriteria 1–5) tidak dijalankan oleh pengembang dan diserahkan sepenuhnya kepada Muse dari binary rilis `v0.5.27`.
