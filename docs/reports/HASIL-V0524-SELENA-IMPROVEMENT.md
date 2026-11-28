# Laporan Rilis SelenCode v0.5.24 — Selena Improvement & Penghapusan Self-Correction Penutup

- **Versi rilis:** `v0.5.24`
- **Jumlah butir:** 6
- **Ringkasan satu kalimat:** Fitur belajar pasca-tugas yang sebelumnya mati senyap sejak v0.5.1 kini diaudit dengan bukti kode langsung, dihidupkan secara default pada model utama sesi secara sadar-cache dengan aturan tangkap/jangan-tangkap dan provenans dari Hermes, diisolasi ke `skills/improvement/` dengan proteksi staging & putar balik, di-rebrand penuh ke `💾 Selena improvement: …`, serta menggantikan loop refleksi self-correction penutup (`corrector.rs`) yang dihapus total tanpa pernah menahan atau mengulang penutupan tugas.

---

## Ringkasan Vonis Butir

- Butir 1 — LULUS: Riset ulang langsung atas source Hermes (`HEAD ae2a81ccc0`) dan SelenCode mengonfirmasi 4 penyebab mati senyap review latar dengan bukti `berkas:baris` konkret beserta perbedaan eksplisit terhadap peta awal prompt.
- Butir 2 — LULUS: Selena improvement kini hidup secara default pada model utama sesi tanpa syarat `[aux]`, menulis skill lintas sesi di direktori skill pengguna, memakai awalan beku `FROZEN_REVIEW_SYSTEM_PROMPT` byte-identik, serta mencatat `cache_read_tokens` & `cache_write_tokens` di `review_runs` dan `usage (kind='review')`.
- Butir 3 — LULUS: Prompt keputusan diperkuat dengan sinyal belajar (koreksi pengguna kelas-satu, kegagalan terpulihkan, workaround non-trivial, skill keliru), urutan preferensi 4 tingkat, aturan lapisan pelajaran (imperatif + 1 klausa sebab tanpa narasi kejadian), daftar jangan-tangkap, dan berhenti diam untuk `{"actions":[]}`.
- Butir 4 — LULUS: Skill baru lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md` ber-frontmatter `origin: selena-improvement`, `source_session`, `learned_at` dengan presedensi di bawah skill pengguna, badge `/skills`, read-before-write, staging wajib untuk semua perubahan skill pengguna, dan putar balik patch langsung via `review_backups` (`/review undo [id]`).
- Butir 5 — LULUS: Pagar keras ditegakkan: berjalan latar pasca-penutup, dibatalkan saat giliran live baru masuk (`bg_review_cancel`/`gen_counter`), pagu input `<= 5.000` token/panggilan & `6.000` token/review, `ReviewSkipReason` tercatat ke SQLite dan tampil di `/doctor` & `/review status`, `delivered=1` ditandai sesudah kirim sukses, serta rebrand penuh ke `💾 Selena improvement: …`.
- Butir 6 — LULUS: Loop refleksi self-correction penutup (`SelfCorrectState`, `completion_gate`, `Self-correction (n/3)`, `[guard] max_self_correct_rounds`) dihapus total dan diganti `VerificationTracker` deterministik (0 panggilan LLM tambahan, jawaban akhir terkirim tepat 1x) sambil mempertahankan catatan status verifikasi dan berkas sementara.

---

## 1. Daftar Berkas Sumber yang Dibaca Langsung (Riset Ulang Wajib)

### Source Hermes (`~/.hermes/hermes-agent`, HEAD `ae2a81ccc0`)
1. `agent/background_review.py` (1.468 baris) — fork cache-parity, `load_background_review_settings`, `_SKILL_REVIEW_PROMPT`, `_LESSON_LAYER_BLOCK`, `_DO_NOT_CAPTURE_BLOCK`, `_digest_history`, `cancel_background_review_for_live_turn`, `summarize_background_review_actions`, `_record_review_usage_to_parent`.
2. `agent/turn_finalizer.py` (926 baris) — `_mirror_turn_to_memory`, penjadwalan pasca-delivery, pengecualian `skip_background_review`.
3. `agent/review_signals.py` (64 baris) — `skill_review_due`, ambang iterasi tool & sinyal belajar.
4. `tools/skill_provenance.py` (44 baris) — ContextVar `foreground` vs `background_review`.
5. `tools/skill_manager_guards.py` (288 baris) — penjaga read-before-write untuk tulisan review & validasi target.

### Source SelenCode (`c:\Users\kanim\Desktop\SelenCode`, dasar `v0.5.23`)
1. `src/agent/review.rs` — `run_background_review`, `run_self_improvement_review`, `check_review_eligibility`, `build_pruned_review_prompt`, `ReviewAction`, `format_review_report`.
2. `src/agent/corrector.rs` — `SelfCorrectState`, `completion_gate`, `completion_gate_with_checklist`, `LoopGuard`, `ToolDedupCache`.
3. `src/agent/loop.rs` — loop refleksi self-correction (`baris 1848–1872`), catatan verifikasi penutup, dan event UI `manage_skill` (`baris 2476`).
4. `src/app/session.rs` & `src/app/mod.rs` — `Session::prepare_bg_review_job`, `make_aux_provider`, `turns_since_review`.
5. `src/app/tui_runner.rs`, `src/app/telegram_runner.rs`, `src/app/plain_runner.rs` — penjadwalan job latar, `busy_chats`, delivery & flag `delivered`.
6. `src/app/commands.rs`, `src/app/cli.rs`, `src/repl.rs` — perintah `/review`, `/skills`, `/help`.
7. `src/skills.rs`, `src/tools/skill.rs` — pemindaian 2 tingkat proyek/pengguna, frontmatter YAML, `ManageSkill`.
8. `src/store.rs`, `src/config.rs`, `src/doctor.rs`, `src/providers/router.rs` — tabel `review_runs`/`review_proposals`/`usage`, `AuxCfg`/`ReviewCfg`/`GuardCfg`, pemeriksaan `run_doctor`.

### Perbedaan Eksplisit terhadap Peta Awal Prompt (Bagian 0)
- **Jalur penulisan patch latar:** Bagian 0 prompt menyatakan patch aman diterapkan *"lewat tool `ManageSkill` (`src/tools/skill.rs`)"*. Pembacaan langsung `src/agent/review.rs` (sebelum v0.5.24, baris `784` dan `892`) membuktikan `run_background_review` **tidak** memanggil `ManageSkill` sama sekali — ia menulis langsung ke disk lewat `std::fs::write(&info.path, &new_md)` tanpa pemeriksaan read-before-write, tanpa pembedaan skill karya pengguna vs review, dan `enum ReviewAction` (`baris 566–618`) bahkan **belum memiliki varian `CreateSkill`**. Di v0.5.24 celah ini ditutup penuh dengan `CreateSkill`, read-before-write, proteksi staging skill pengguna, dan `review_backups`.

---

## 2. Hasil Audit Butir 3.1 (Bukti `berkas:baris` Mengapa Review Mati di Lapangan)

1. **Terkonfirmasi — Kematian senyap via `NoAuxModelConfigured` (penyebab utama):**
   - `src/config.rs:391` (`AuxCfg::effective_review_model`) mengembalikan `None` saat `[aux] review_model` dan `compression_model` kosong (terkonfirmasi tidak terisi pada config produksi pemilik).
   - `src/agent/review.rs:328–330` langsung mengembalikan `Err(ReviewSkipReason::NoAuxModelConfigured)`, sementara `src/app/session.rs:1048–1068` dan `src/app/mod.rs:237–245` membatalkan job diam-diam tanpa log atau peringatan.
2. **Terkonfirmasi — Ambang pemicu terlalu tinggi & tidak cocok dengan pola kerja nyata:**
   - `src/config.rs:407–420` (`ReviewCfg` lama: `turns_interval = 15`, `cooldown_mins = 30`, `min_tool_calls = 5`) dan `src/app/session.rs:1049–1054` hanya memicu review periodik setelah 15 giliran pengguna penuh. Pada tugas coding modern (1–3 giliran pengguna dengan 10–30 tool call, dan bot Telegram jarang menjalankan `/exit`), pemicu periodik hampir tidak pernah tercapai.
   - Penyetelan ulang v0.5.24: `turns_interval = 5`, `cooldown_mins = 10`, `min_tool_calls = 4`, plus pemicu langsung pasca-giliran bila `has_valuable_review_signal` terpenuhi (koreksi pengguna, kegagalan yang dipulihkan di dalam sesi, aktivitas skill ≥ 2 tool, atau tugas besar).
3. **Terkonfirmasi — Celah jalur delivery & flag `delivered` pada semua permukaan:**
   - `src/app/plain_runner.rs:448` memanggil `job.run(false)`, sehingga laporan tidak pernah tercetak pada sesi aktif.
   - `src/app/telegram_runner.rs:1688–1705` dan `src/app/tui_runner.rs:101–113` meneruskan `mark_delivered = true` **sebelum** pengiriman benar-benar berhasil (jika kirim gagal sementara, laporan hilang permanen) dan belum punya pembatalan review latar saat giliran live baru masuk.
   - Diperbaiki di v0.5.24: `run_background_review` mencatat `delivered = 0` lebih dulu, lalu `store.mark_review_run_delivered(run_id)` dipanggil **hanya sesudah** pesan benar-benar terkirim ke TUI/Telegram; giliran live baru langsung membatalkan job latar (`bg_review_cancel` di TUI, `gen_counter` di Telegram).
4. **Terkonfirmasi — `ReviewSkipReason` tak terlihat di mana pun:**
   - `src/doctor.rs` (326 baris sebelum v0.5.24) tidak memiliki pemeriksaan untuk review latar sama sekali, dan `src/agent/review.rs:703` membuang `Err(ReviewSkipReason)` tanpa menyimpannya.
   - Diperbaiki di v0.5.24: setiap `ReviewSkipReason` dicatat ke `store.set_last_review_skip(...)` dan ditampilkan di `/doctor` (bagian `Selena Improvement`) serta `/review status`.

---

## 3. Audit Perlindungan Unik `SelfCorrectState` yang Dihapus (Butir 3.6)

Saat menghapus `SelfCorrectState`, `completion_gate`, `completion_gate_with_checklist`, tag `"✦ Self-correction (n/3): …"`, dan `[guard] max_self_correct_rounds`, ditemukan tepat **2 perlindungan** yang sebelumnya hanya dijalankan lewat putaran refleksi model di dalam giliran:

1. **Memaksa model mengulang verifikasi ketika tes terakhir GAGAL atau basi sesudah edit terakhir.**
   - *Pengganti deterministik (0 panggilan LLM tambahan):* `VerificationTracker::honest_unverified_closing_note` dan `closing_verification_summary_line` di `src/agent/corrector.rs` tetap menempelkan ringkasan faktual `Verifikasi: <perintah> — exit code <N> (LULUS/GAGAL/BASI)` dan blok `[Status Verifikasi Akhir — GAGAL]` pada penutup tunggal tanpa menahan atau mengulang giliran.
2. **Mengingatkan penghapusan/penyebutan berkas sementara (`repro*`, `tmp_*`, `_tmp*`, `debug_tmp*`, `*.tmp`) ciptaan sesi yang tertinggal di disk.**
   - *Pengganti deterministik (0 panggilan LLM tambahan):* `VerificationTracker::closing_scratch_files_note` di `src/agent/corrector.rs` memeriksa daftar `created_files` di disk secara deterministik dan melampirkan 1 baris `Catatan berkas sementara masih ada di disk: …` apabila berkas tersebut masih ada dan belum disebut pada jawaban akhir.

---

## 4. Pelabelan Kejujuran Mekanisme & Mekanisme Putar Balik

| Mekanisme di SelenCode v0.5.24 | Pelabelan Asal | Berkas Implementasi SelenCode |
|---|---|---|
| Default berjalan pada provider/model utama sesi (`[aux]` opsional) | **diambil dari Hermes (`agent/background_review.py`)** | `src/agent/review.rs`, `src/app/mod.rs` |
| Awalan sistem beku sadar-cache (`FROZEN_REVIEW_SYSTEM_PROMPT`) + ekor dinamis + pencatatan `cache_read_tokens` | **diambil dari Hermes (`agent/background_review.py::build_cache_parity_fork`)** | `src/agent/review.rs`, `src/store.rs` |
| Daftar sinyal belajar, urutan preferensi 4 tingkat, aturan lapisan pelajaran, & daftar jangan-tangkap | **diambil dari Hermes (`agent/background_review.py::_SKILL_REVIEW_PROMPT`, `_LESSON_LAYER_BLOCK`, `_DO_NOT_CAPTURE_BLOCK`)** | `src/agent/review.rs` |
| Pemicu berbasis sinyal & iterasi tool pasca-delivery + pembatalan saat giliran live baru masuk | **diambil dari Hermes (`agent/turn_finalizer.py`, `agent/review_signals.py`, `agent/background_review.py::cancel_background_review_for_live_turn`)** | `src/app/session.rs`, `src/app/tui_runner.rs`, `src/app/telegram_runner.rs` |
| Penjaga read-before-write (wajib baca segar sebelum menambal skill) | **diambil dari Hermes (`tools/skill_manager_guards.py`)** | `src/agent/review.rs` |
| Penanda asal provenans (`origin: selena-improvement`, `source_session`, `learned_at`) & atribusi biaya `usage(kind='review')` | **diambil dari Hermes (`tools/skill_provenance.py`, `agent/background_review.py::_record_review_usage_to_parent`)** | `src/skills.rs`, `src/agent/review.rs`, `src/store.rs` |
| Subfolder fisik `skills/improvement/<nama>/SKILL.md` berpresedensi lebih rendah + badge `/skills` | **mekanisme sendiri, terinspirasi Hermes** (atas perintah pemilik 28 Nov) | `src/skills.rs`, `src/agent/review.rs`, `src/app/commands.rs` |
| Staging wajib `review_proposals` untuk semua perubahan skill karya pengguna (`/review approve|discard`) | **mekanisme sendiri, terinspirasi Hermes** | `src/agent/review.rs`, `src/store.rs`, `src/app/commands.rs` |
| Putar balik patch langsung via tabel `review_backups` & perintah `/review undo [id]` | **mekanisme sendiri, terinspirasi Hermes** | `src/store.rs`, `src/agent/review.rs`, `src/app/commands.rs` |
| Identitas laporan satu baris `💾 Selena improvement: …` & diagnostik `ReviewSkipReason` di `/doctor` | **mekanisme sendiri, terinspirasi Hermes** | `src/agent/review.rs`, `src/doctor.rs`, `src/app/commands.rs` |

### Penjelasan Mekanisme Putar Balik (`/review undo [id]`)
Setiap kali Selena improvement menerapkan patch langsung ke skill ber-origin `selena-improvement`, isi penuh berkas sebelum patch bersama `file_path`, `diff`, `session_id`, dan `reason` disimpan terlebih dahulu ke tabel SQLite `review_backups` (`Store::record_review_backup`). Pemilik dapat memulihkan isi sebelumnya kapan saja lewat `/review undo` (putar balik patch langsung terbaru) atau `/review undo <id>` (memanggil `rollback_direct_review_patch`), yang menulis kembali `previous_content` persis seperti semula dan menandai baris cadangan `rolled_back = 1`.

---

## Tabel Regresi

| Metrik | Sebelum (v0.5.23) | Sesudah (v0.5.24) | Status |
|---|---:|---:|---|
| Unit & integration test CI Linux (`cargo test`) | 55 suite (251 tes lulus) | 56 suite (256 tes lulus, 0 gagal) | LULUS |
| Unit & integration test khusus v0.5.24 (`v0524_selena_improvement_and_no_self_correction`) | 0 tes | 5 tes (5 lulus, 0 gagal) | LULUS |
| Pagu keras input per panggilan review (`MAX_REVIEW_CALL_INPUT_TOKENS`) | 6000 token | 5000 token | LULUS |
| Pagu default per-review (`[review] max_tokens`) | 6000 token | 6000 token | LULUS |
| Putaran refleksi self-correction penutup (`Self-correction (n/3)`) | 3 putaran | 0 putaran | LULUS |
| Kriteria penerimaan 1 — kegagalan terpulihkan -> patch skill + 1 baris `💾 Selena improvement` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 2 — tugas bersih -> diam (0 laporan) | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 3 — biaya & token `review_runs` dalam pagu <= 5.000 token/panggilan | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 4 — proteksi staging skill pengguna, diff, & rollback | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 5 — kasus catatan-keuangan menutup tepat 1x tanpa `Self-correction (n/3)` | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 6 — skill baru lintas sesi & `cache_read_tokens > 0` pada review ke-2+ | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Kriteria penerimaan 7 — `skills/improvement/`, `origin: selena-improvement`, badge `/skills`, & presedensi skill pengguna | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Pagar regresi skenario A+B rata-rata token/panggilan (<= 5.000) | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |
| Pagar regresi skenario resume E rata-rata token/panggilan (<= 7.500) | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse | pengukuran penerimaan oleh Muse |

---

## Tidak Diubah

- Jalur eksekusi tool inti, `LoopGuard` (deteksi loop/stagnasi), `ToolDedupCache` (dedup hasil read/shell), dan pagu anggaran alat/konteks (v0.5.14–v0.5.23) tetap utuh.
- Protokol MCP, aktivasi dinamis tool MCP, dan indeks kompak anti-terpotong (v0.5.23) tidak diubah.
- Persistensi sesi lintas restart CLI & Telegram (v0.5.22) dan tampilan penuh Moonlit TUI header (v0.5.23) tidak diubah.
- Kurator siklus hidup lapis lambat (`agent/curator.py` di Hermes) sengaja tidak masuk cakupan v0.5.24 sesuai spesifikasi prompt.
- Pengujian penerimaan live (kriteria 1–7 & regresi skenario A+B / resume E) sengaja tidak dijalankan oleh pengembang dan diserahkan sepenuhnya kepada Muse.
