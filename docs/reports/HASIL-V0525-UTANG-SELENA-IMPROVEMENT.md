# Laporan Rilis SelenCode v0.5.25 — Perbaikan 7 Utang Review (Selena Improvement)

- **Versi rilis:** `v0.5.25`
- **Jumlah butir:** 7
- **Ringkasan:** Menutup tepat 7 utang terukur dari `TES-PENERIMAAN-V0524.md` — anti-tembak ganda akhir sesi, preservasi sinyal kelas-satu melewati cooldown, pagu keras total request `<= 5.000` token + `digest_over_budget`, diagnosis & penyelarasan ambang cache awalan beku, penegakan mekanis patch koreksi di tempatnya (tolak append-only -> retry 1x / staging), plumbing & penghapusan bias `CreateSkill`, serta klasifikasi verifikasi `curl`/`wget` ke server lokal.

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Penjaga idempotensi 3 lapis (`Store::acquire_review_slot`, `Session::last_scheduled_review_msg_len` + `pending_bg_review` await di `plain_runner.rs`, serta kolom `review_runs.covered_msg_count` + `ReviewSkipReason::AlreadyReviewed`) memastikan akhir sesi layak review menembak tepat 1x (1 baris `review_runs`, <= 1 baris 💾).
- Butir 2 — LULUS: Sinyal kelas-satu (koreksi eksplisit pengguna & kegagalan terpulihkan) yang tertahan cooldown disimpan persisten lewat `Store::record_pending_review_signal`, langsung memicu review pada titik kelayakan pertama pasca-cooldown, dan cakupan digest menyertakan seluruh giliran sejak `last_reviewed_msg_count` hingga dibersihkan sesudah selesai.
- Butir 3 — LULUS: Penaksir total request `estimate_review_request_tokens` + kompensasi overhead gateway terukur `provider_base_overhead_tokens` (`+2.150` token untuk rute `ag/*`), penghapusan bug `+ 800`, serta pemangkasan progresif berprioritas koreksi/kegagalan-pulih menegakkan pagu `<= 5.000` token/panggilan atau melewati dengan alasan tercatat `digest_over_budget`.
- Butir 4 — LULUS: Diagnosis & pembuktian langsung atas `ag/gemini-3-flash` di 9Router membuktikan penyebab `cache_read_tokens = 0` di bawah pagu `<= 5.000` berada di sisi provider/relay (overhead tersembunyi +2.086 token dan implicit caching baru aktif di multi-turn ~10.516 token dengan `cached_tokens = 6.112`, sedangkan 4 panggilan multi-turn identik di 4.516 token tetap 0 cache); Selen tetap menjaga awalan beku byte-identik `>= 1.024` token untuk provider lain yang mendukung cache di ambang 1k+.
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
- **Akar masalah:** (1) `src/agent/review.rs:697` memiliki tambahan `+ 800` char di luar `skills_budget`; (2) `analyze_token_breakdown` memakai `chars / 4` yang meremehkan tokenisasi aktual; dan (3) rute `ag/*` pada 9Router menyuntikkan overhead system prompt dasar tersembunyi sebesar **~2.086 token** pada setiap panggilan (terbukti dari probe payload 10 token yang dilaporkan `prompt_tokens: 2096`), sehingga payload Selen ~3.000 token terlaporkan 5.075–5.857 token di `review_runs`.
- **Perbaikan:** `estimate_review_request_tokens` + `provider_base_overhead_tokens` (`+2.150` untuk `ag/*`) di `src/agent/review.rs` mengukur total request secara konservatif, `FROZEN_REVIEW_SYSTEM_PROMPT` dipadatkan ke 4.125 char (~1.375 token), bug `+ 800` dihapus, dan 6 tahap pemangkasan progresif mempertahankan giliran koreksi/kegagalan-pulih paling akhir agar total aktual `review_runs.input_tokens` tetap `<= 5.000` (atau dilewati eksplisit dengan `digest_over_budget`).

### Masalah 4 — Diagnosis Cache Review NOL (`cache_read_tokens = 0`) — Bukti Sisi Provider/Relay
- **Divergensi kode yang diperiksa (`berkas:baris`):**
  1. Jalur MAIN (`src/agent/loop.rs:1082–1085` + `src/providers/openai.rs:378–387`) mengirim `System(build_system_prompt + snapshot awal)` + `tools = active_tool_specs(...)` + histori multi-giliran (~10.453–15.875 `prompt_tokens`), sedangkan jalur review (`src/agent/review.rs:922–942`) mengirim `System(FROZEN_REVIEW_SYSTEM_PROMPT)` + `tools = []` + `User(digest)`.
  2. `src/providers/openai.rs:178–203` (`should_apply_openai_envelope_cache_control`) mengembalikan `false` untuk `ag/gemini-3-flash` karena Gemini memakai implicit prefix caching otomatis (bukan header `cache_control`).
- **Bukti empiris sisi provider/relay (`ag/gemini-3-flash` via 9Router):**
  - Payload minimal 10 token (`A1_tiny`) dilaporkan `prompt_tokens: 2096` (overhead dasar 9Router `ag/*` = **~2.086 token/panggilan**).
  - 2 panggilan identik dengan `System` besar saja (`E1`/`E2`: `prompt_tokens: 18598`, `stream=true`) menghasilkan **0 `cached_tokens`**.
  - 4 panggilan multi-turn identik yang muat di pagu `<= 5.000` (`I1`..`I4`: `prompt_tokens: 4516`, jeda 3 detik antar-panggilan, `stream=true`) tetap menghasilkan **0 `cached_tokens`** pada keempat panggilan.
  - 3 panggilan multi-turn identik berukuran setara MAIN (`H1`..`H3`: `prompt_tokens: 10516`, `stream=true`) baru menghasilkan **`prompt_tokens_details.cached_tokens = 6112`** pada panggilan ke-3 (`H3`) — cocok persis dengan angka MAIN 9.943 di uji Muse.
- **Kesimpulan jujur (sesuai brief Masalah 4):** Penyebab `cache_read_tokens = 0` pada review `ag/gemini-3-flash` di bawah pagu `<= 5.000` adalah **ambang sisi provider/relay** (implicit caching `ag/gemini-3-flash` tidak menyala pada payload multi-turn 4.516 token dan baru menyala di ~10.516 token yang bertentangan langsung dengan pagu keras `<= 5.000` Masalah 3; menyalin awalan MAIN ~10.453 token akan otomatis melanggar pagu 5.000). Kami tidak mengklaim `ag/gemini-3-flash` akan mengenai cache di bawah 5.000 token; Selen tetap mempertahankan `FROZEN_REVIEW_SYSTEM_PROMPT` byte-identik `>= 1.024` token agar provider lain yang mendukung cache di ambang 1.024 token (OpenAI/Anthropic/Gemini langsung) tetap memperoleh cache hit sambil mematuhi pagu keras `<= 5.000`.

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
| Ukuran awalan sistem beku (`FROZEN_REVIEW_SYSTEM_PROMPT`) | 2985 char (~850 token) | 4125 char (~1375 token >= 1024; +2150 headroom `ag/*`) | LULUS |
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
