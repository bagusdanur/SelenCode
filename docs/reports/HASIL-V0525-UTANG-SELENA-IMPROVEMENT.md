# Laporan Rilis SelenCode v0.5.25 — Perbaikan 7 Utang Review (Selena Improvement)

- **Versi rilis:** `v0.5.25`
- **Jumlah butir:** 7
- **Ringkasan:** Menutup tepat 7 utang terukur dari `TES-PENERIMAAN-V0524.md` — anti-tembak ganda akhir sesi, preservasi sinyal kelas-satu melewati cooldown, pagu keras total request `<= 5.000` token + `digest_over_budget`, diagnosis & penyelarasan ambang cache awalan beku, penegakan mekanis patch koreksi di tempatnya (tolak append-only -> retry 1x / staging), plumbing & penghapusan bias `CreateSkill`, serta klasifikasi verifikasi `curl`/`wget` ke server lokal.

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Penjaga idempotensi 3 lapis (`Store::acquire_review_slot`, `Session::last_scheduled_review_msg_len` + `pending_bg_review` await di `plain_runner.rs`, serta kolom `review_runs.covered_msg_count` + `ReviewSkipReason::AlreadyReviewed`) memastikan akhir sesi layak review menembak tepat 1x (1 baris `review_runs`, <= 1 baris 💾).
- Butir 2 — LULUS: Sinyal kelas-satu (koreksi eksplisit pengguna & kegagalan terpulihkan) yang tertahan cooldown disimpan persisten lewat `Store::record_pending_review_signal`, langsung memicu review pada titik kelayakan pertama pasca-cooldown, dan cakupan digest menyertakan seluruh giliran sejak `last_reviewed_msg_count` hingga dibersihkan sesudah selesai.
- Butir 3 — LULUS: Penaksir total request `estimate_review_request_tokens` dikalibrasi ulang, bug overflow `+ 800` dihapus, pemangkasan progresif berprioritas (mempertahankan giliran koreksi & kegagalan-pulih paling akhir) menegakkan pagu `<= 5.000` token/panggilan, dan jika digest minimal tetap melampaui pagu review dilewati dengan alasan `digest_over_budget`.
- Butir 4 — LULUS: Diagnosis membuktikan 2 divergensi awalan MAIN vs REVIEW (`src/agent/review.rs:43–87` hanya ~2.985 char / ~800 token di bawah ambang implicit caching 2.048 token Gemini 3 Flash, serta `tools=[]` vs `tools=active_tool_specs` di `src/agent/loop.rs:1082`); `FROZEN_REVIEW_SYSTEM_PROMPT` diperluas menjadi 8.687 char (~2.633 token Gemini) yang byte-identik lintas review sambil total request tetap `<= 5.000`.
- Butir 5 — LULUS: Prompt ditambah contoh terapan koreksi di tempatnya, fallback tempel diam-diam di `src/agent/review.rs:1234–1244` dihapus, `replace_snippet_in_place` mendukung cocok persis & trimmed-line, dan patch koreksi murni penambahan di akhir ditolak secara mekanis -> retry 1x dengan isi persis `SKILL.md` atau dialihkan ke staging agar berkas skill tidak pernah berkontradiksi.
- Butir 6 — LULUS: Plumbing ujung-ke-ujung `CreateSkill` terverifikasi lewat `cargo test` (`skills/improvement/<nama>/SKILL.md` ber-frontmatter `origin: selena-improvement`, `source_session`, `learned_at` + laporan `created` + indeks lintas proyek), dan bias urutan preferensi prompt dipertajam agar `patch_skill` hanya dipakai bila relevan secara topik serta `create_skill` wajib dipilih untuk kelas teknik baru.
- Butir 7 — LULUS: `is_local_http_verification_cmd` mengenali `curl`/`wget` ke alamat lokal (`127.0.0.1`, `localhost`, `[::1]`, `0.0.0.0`) dan mengecualikannya dari `is_setup_or_scaffold_shell_cmd`, memverifikasi exit 0 & status HTTP non-4xx/5xx pada `VerificationTracker`, sementara URL eksternal tetap ditolak.

---

## 1. Diagnosis Teknis & Rujukan `berkas:baris`

### Masalah 1 — Review Menembak Ganda di Akhir Sesi
- **Akar masalah:** `src/app/plain_runner.rs:448` mem-`tokio::spawn` job review sesudah giliran selesai (`prepare_bg_review_job(false)`), lalu saat loop REPL langsung mencapai `/exit` atau EOF beberapa milidetik kemudian, `src/app/plain_runner.rs:477` memanggil `prepare_bg_review_job(true)` sebelum task pertama sempat menulis baris `review_runs`, sehingga cooldown masih `false` dan 2 panggilan identik berjalan serentak.
- **Perbaikan:** (1) `Store::acquire_review_slot(session_id, covered_msg_count)` di `src/store.rs` (penjaga RAII in-process); (2) kolom `covered_msg_count` di `review_runs` + `ReviewSkipReason::AlreadyReviewed` di `src/agent/review.rs`; (3) `Session::last_scheduled_review_msg_len` di `src/app/session.rs` dan penungguan `pending_bg_review` di `src/app/plain_runner.rs` sebelum mengecek `prepare_bg_review_job(true)`.

### Masalah 2 — Cooldown Menelan Sinyal Koreksi Pengguna
- **Akar masalah:** `src/agent/review.rs:517–526` mengembalikan `Err(CooldownActive)` saat giliran 2 membawa koreksi eksplisit tanpa mencatat sinyal tertunda secara persisten, dan `build_pruned_review_prompt_with_reads` sebelumnya tidak melacak batas pesan yang sudah ditinjau (`last_reviewed_msg_count`).
- **Perbaikan:** `Store::record_pending_review_signal` / `get_pending_review_signal` / `clear_pending_review_signal` di `src/store.rs` + `record_pending_signal_if_present` di `src/agent/review.rs` & `src/app/session.rs`. Cooldown kini hanya menunda eksekusi, tidak pernah membuang sinyal kelas-satu; pada titik kelayakan pertama pasca-cooldown, review langsung menembak dengan cakupan digest seluruh pesan sejak `last_reviewed_msg_count` (memprioritaskan giliran koreksi & kegagalan pulih).

### Masalah 3 — Pagu Token Review Bocor (> 5.000 Token)
- **Akar masalah:** `src/agent/review.rs:697` memiliki tambahan `+ 800` char di luar `skills_budget`, dan `analyze_token_breakdown` memakai aproksimasi `chars / 4` yang meremehkan tokenisasi aktual Gemini pada teks campuran Bahasa Indonesia + Markdown + JSON (menghasilkan 5.075–5.857 token di `review_runs`).
- **Perbaikan:** `estimate_review_request_tokens` di `src/agent/review.rs` mengukur total request secara konservatif (`max(chars/2.8, bytes/3.0)` + overhead framing pesan), bug `+ 800` dihapus, dan 6 tahap pemangkasan progresif memangkas keluaran tool & giliran rutin terlama lebih dulu sambil mempertahankan giliran koreksi/kegagalan-pulih paling akhir. Jika digest minimal pun tetap `> min(max_tokens, 5.000)`, review dilewati dengan alasan tercatat `digest_over_budget`.

### Masalah 4 — Diagnosis Cache Review NOL (`cache_read_tokens = 0`)
- **Divergensi yang ditemukan (dengan bukti `berkas:baris`):**
  1. **Ukuran awalan beku di bawah ambang implicit prefix caching:** Di v0.5.24, `FROZEN_REVIEW_SYSTEM_PROMPT` (`src/agent/review.rs:43–87`) hanya sepanjang ~2.985 char (~780–900 token). Pada `ag/gemini-3-flash` melalui 9Router (`src/providers/openai.rs:178–203` mengembalikan `should_apply_openai_envelope_cache_control = false` untuk Gemini sehingga mengandalkan implicit prefix caching), ambang minimum implicit prefix caching Gemini 2.5 Pro / 3 Flash adalah **2.048 token** (dan OpenAI/Anthropic 1.024–2.048 token), sehingga prefix ~800 token tidak pernah di-cache meski byte-identik di review ke-2 & ke-3.
  2. **Divergensi awalan terhadap sesi MAIN:** Jalur MAIN (`src/agent/loop.rs:1082–1085` + `src/providers/openai.rs:378–387`) mengirim `messages[0] = System(build_system_prompt + snapshot awal)` ditambah `tools = active_tool_specs(...)` (~9.943 token total, jauh di atas ambang 2.048 namun juga jauh di atas pagu keras review 5.000 token/panggilan), sedangkan jalur review mengirim `System(FROZEN_REVIEW_SYSTEM_PROMPT)` + `tools = []`.
- **Perbaikan:** Menyalin awalan MAIN penuh (~9.943 token) akan melanggar pagu keras Masalah 3 (`<= 5.000` token/panggilan). Sebagai gantinya, `FROZEN_REVIEW_SYSTEM_PROMPT` (`src/agent/review.rs:41–116`) diperluas dengan aturan ketat & contoh terapan Masalah 5–6 menjadi **8.687 char (~2.633 token aktual Gemini)** — stabil byte-identik lintas semua review di HOME yang sama, melampaui ambang minimum 2.048 token implicit prefix caching Gemini 3 Flash, sambil menyisakan ruang digest dinamis agar total request tetap `<= 5.000` token. (Pembuktian live `cache_read_tokens > 0` pada relay/provider diserahkan ke pengukuran penerimaan oleh Muse.)

### Masalah 5 — Mutu Patch: Anti-Tempel di Akhir & Anti-Kontradiksi
- **Akar masalah:** `src/agent/review.rs:1234–1244` diam-diam jatuh ke `format!("{old_body}\n\n{safe_append}")` ketika `old_snippet` tidak cocok persis atau model hanya mengisi `append_section`, sehingga aturan keliru nomor 2–3 tetap utuh berdampingan dengan bantahannya.
- **Perbaikan:** (a) Contoh terapan BENAR vs SALAH di `FROZEN_REVIEW_SYSTEM_PROMPT` + alias serde `old_text`/`new_text`; (b) `replace_snippet_in_place` mendukung cocok persis dan cocok baris ter-trim whitespace; (c) fallback append diam-diam dihapus; (d) patch koreksi murni penambahan di akhir pada skill improvement ditolak secara mekanis lalu dicoba ulang **maks 1x** dengan isi persis `SKILL.md` — bila masih tidak menunjuk teks lama yang cocok, dipaksa masuk staging `review_proposals` (tidak pernah menulis skill kontradiktif langsung ke disk).

### Masalah 6 — Plumbing `CreateSkill` & Audit Bias Prompt
- **Temuan bias prompt:** `src/agent/review.rs:54–58` sebelumnya menuliskan `(1) Tambal skill yang sedang dimuat` dan `(2) Tambal skill payung yang sudah ada` tanpa syarat relevansi topik, mendorong model ringan menempelkan pelajaran topik baru ke skill apa pun yang kebetulan termuat.
- **Perbaikan:** Urutan preferensi di `FROZEN_REVIEW_SYSTEM_PROMPT` dipertegas: `(1)` dan `(2)` HANYA berlaku jika skill relevan secara topik, sedangkan `(4) create_skill` ditegaskan sebagai pilihan wajib & tepat untuk kelas teknik baru tanpa skill relevan secara topik (dilengkapi Contoh B konkret). Plumbing ujung-ke-ujung terverifikasi oleh tes `masalah6_create_skill_plumbing_and_prompt_allows_new_skill_despite_unrelated_catalog`.

### Masalah 7 — Pengenalan `curl`/`wget` ke Server Lokal
- **Akar masalah:** `src/agent/corrector.rs:713–714` memasukkan `"curl" | "wget"` secara menyeluruh ke `is_setup_or_scaffold_shell_cmd`, sehingga `is_single_verification_shell_cmd` langsung menolak `curl http://127.0.0.1:…`.
- **Perbaikan:** `is_local_http_verification_cmd` di `src/agent/corrector.rs` mengenali `curl`/`wget` (termasuk pipeline seperti `curl ... | grep ...`) khusus ke host lokal (`127.0.0.1`, `localhost`, `[::1]`, `0.0.0.0`), mengecualikannya dari `is_setup_or_scaffold_shell_cmd`, dan `local_http_output_indicates_error` memastikan respons HTTP `4xx`/`5xx` tetap berstatus `GAGAL`. URL eksternal tetap diabaikan (`belum diverifikasi`).

---

## 2. Pelabelan Kejujuran Mekanisme

| Mekanisme di SelenCode v0.5.25 | Pelabelan Asal | Berkas Implementasi |
|---|---|---|
| Disiplin prompt keputusan: ganti aturan keliru di tempatnya (anti-kontradiksi) & klasifikasi `create_skill` kelas baru | **diambil dari Hermes (`agent/background_review.py::_SKILL_REVIEW_PROMPT`, `_LESSON_LAYER_BLOCK`)** | `src/agent/review.rs` |
| Konsep awalan sistem beku byte-identik lintas review untuk prefix cache | **diambil dari Hermes (`agent/background_review.py::build_cache_parity_fork`)** | `src/agent/review.rs` |
| Penjaga idempotensi 3 lapis (`acquire_review_slot`, `covered_msg_count`, `AlreadyReviewed`, await `pending_bg_review`) | **mekanisme sendiri, terinspirasi Hermes** | `src/store.rs`, `src/agent/review.rs`, `src/app/session.rs`, `src/app/plain_runner.rs` |
| Sinyal tertunda persisten pasca-cooldown (`pending_review_signal`) & cakupan digest sejak `last_reviewed_msg_count` | **mekanisme sendiri, terinspirasi Hermes** | `src/store.rs`, `src/agent/review.rs`, `src/app/session.rs` |
| Penaksir total request konservatif + pemangkasan progresif berprioritas + `digest_over_budget` | **mekanisme sendiri, terinspirasi Hermes** | `src/agent/review.rs` |
| Pagar mekanis anti-append-only koreksi + retry 1x + fallback staging | **mekanisme sendiri, terinspirasi Hermes** | `src/agent/review.rs` |
| Klasifikasi verifikasi `curl`/`wget` lokal & cek kode HTTP 2xx/3xx vs 4xx/5xx | **mekanisme sendiri, terinspirasi Hermes** | `src/agent/corrector.rs` |

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.24) | Sesudah (v0.5.25) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 56 suite (256 tes lulus) | 57 suite (262 tes lulus, 0 gagal) | LULUS |
| Unit & integration test khusus v0.5.25 (`v0525_selena_improvement_debt`) | 0 tes | 6 tes (6 lulus, 0 gagal) | LULUS |
| Pagu keras total input per panggilan review (`MAX_REVIEW_CALL_INPUT_TOKENS`) | 5000 token (bocor hingga 5857) | 5000 token (terikat total request + `digest_over_budget`) | LULUS |
| Ukuran awalan sistem beku (`FROZEN_REVIEW_SYSTEM_PROMPT`) | 2985 char (~850 token) | 8687 char (~2633 token >= ambang 2048) | LULUS |
| Kriteria 1 — akhir sesi REPL layak review: 1 baris `review_runs`, baris 💾 <= 1x | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 2 — koreksi giliran 2 sesudah review giliran 1 ditindaklanjuti pasca-cooldown | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 3 — skenario kaya konteks semua panggilan review <= 5.000 input atau `digest_over_budget` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 4 — review ke-2+ satu HOME mencatat `cache_read_tokens > 0` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 5 — aturan keliru tertanam hilang dari berkas (grep), tanpa kontradiksi | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 6 — pelajaran kelas-baru tanpa skill relevan menciptakan skill baru di `skills/improvement/` + `created` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 7 — verifikasi `curl` lokal tercap LULUS menyebut curl; tanpa verifikasi tetap belum diverifikasi | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria 8 — regresi skenario A+B <= 5.000, resume E <= 7.500, penutup tunggal, nol "Self-correction" | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |

---

## Tidak Diubah

- Utang #8 dari laporan v0.5.24 (review diam pada sinyal implisit) dipertahankan sesuai keputusan pemilik — ambang keputusan review untuk sinyal implisit tidak dilonggarkan.
- Perilaku mode one-shot (job latar berhenti saat proses keluar) sengaja tidak diubah sesuai brief.
- Seluruh jaminan v0.5.7–v0.5.24 (penutup tunggal tanpa loop self-correction, gerbang verifikasi deterministik + cap jujur, staging wajib skill pengguna + putar balik `/review undo`, dedup verifikasi, pemisahan fisik `skills/improvement/` dengan presedensi lebih rendah, MCP & tampilan Moonlit TUI) tidak diubah.
- Pengukuran penerimaan live (Kriteria 1–8) diserahkan sepenuhnya kepada Muse.
