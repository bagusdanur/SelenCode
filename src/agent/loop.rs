use crate::providers::{Block, Message, Provider, Role, StreamEvent, Usage};
use crate::security::redact_secrets;
use crate::tools::{Ctx, Registry};
use anyhow::Result;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::context::{
    analyze_token_breakdown, auto_outline, build_folder_map, build_resume_snapshot,
    build_resume_system_prompt, build_system_prompt, cap_dynamic_injection,
    compact_messages_hermes, estimate_tokens, expand_at_mentions, head_tail_truncate,
    needs_compaction, proactive_prune_context_with_archive,
    prune_old_skill_results_with_archive, prune_old_tool_results_with_archive, session_archive_dir,
    spillover_if_needed, spillover_with_preview,
};
use crate::tool_display::{
    build_template_closing_summary_with_status, format_tool_event_from_preview,
    prepare_tool_preview, short_error_reason, ToolActionEvent,
};
use super::corrector::{LoopGuard, SelfCorrectState, ToolDedupCache};
use super::plan::{is_read_only, TurnMode, PLAN_NOTE};
use super::ui::UiEvent;

/// Simpan 1 pesan ke store sesi bila persistensi aktif (Fase 3).
/// Urutan penting: pesan ToolResult yang sampai ke titik simpan ini sudah
/// melewati redact_secrets di bawah, jadi yang tersimpan di DB adalah versi
/// teredaksi — tidak ada rahasia mentah / API key di DB.
fn persist_message(ctx: &Ctx, msg: &Message) {
    if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
        if let Err(e) = store.save_message(sid, msg) {
            eprintln!("[selen] gagal menyimpan pesan sesi: {e}");
        }
    }
}

fn persist_usage_kind(ctx: &Ctx, model: &str, usage: Option<Usage>, kind: &str) {
    if let (Some(store), Some(sid), Some(u)) = (&ctx.store, &ctx.session_id, usage) {
        if let Err(e) = store.record_usage_with_kind(sid, model, u.input_tokens, u.output_tokens, kind) {
            eprintln!("[selen] gagal mencatat usage: {e}");
        }
    }
}

/// Jalankan 1 giliran (mode polos Fase 0-3): token dicetak langsung ke
/// stdout, aktivitas tool ke stderr. Perilaku TIDAK berubah dari sebelumnya.
pub async fn run_turn(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
) -> Result<String> {
    Ok(run_turn_inner(
        provider,
        registry,
        ctx,
        messages,
        user_prompt,
        max_steps,
        None,
        TurnMode::Execute,
    )
    .await?
    .0)
}

/// Jalankan 1 giliran dengan mode eksplisit (Fase 6: plan mode).
pub async fn run_turn_mode(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
    mode: TurnMode,
) -> Result<String> {
    Ok(run_turn_inner(provider, registry, ctx, messages, user_prompt, max_steps, None, mode)
        .await?
        .0)
}

/// Jalankan 1 giliran (mode TUI Fase 4): semua keluaran dikirim sebagai
/// UiEvent lewat `ui` — stdout/stderr tidak disentuh sama sekali.
pub async fn run_turn_ui(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
    ui: mpsc::Sender<UiEvent>,
) -> Result<String> {
    Ok(run_turn_inner(
        provider,
        registry,
        ctx,
        messages,
        user_prompt,
        max_steps,
        Some(ui),
        TurnMode::Execute,
    )
    .await?
    .0)
}

/// Varian TUI dengan mode eksplisit (Fase 6: plan mode di TUI).
pub async fn run_turn_ui_mode(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
    ui: mpsc::Sender<UiEvent>,
    mode: TurnMode,
) -> Result<String> {
    Ok(run_turn_inner(provider, registry, ctx, messages, user_prompt, max_steps, Some(ui), mode)
        .await?
        .0)
}

/// Jalankan 1 giliran TANPA mencetak apa pun ke terminal (UiEvent dibuang)
/// dan kembalikan juga usage giliran. Dipakai subagent (Fase 6): banyak
/// anak berjalan paralel — keluaran mereka tidak boleh mengacak stdout.
pub async fn run_turn_counted(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
) -> Result<(String, Option<Usage>)> {
    let (tx, rx) = mpsc::channel::<UiEvent>(256);
    drop(rx); // semua send di loop gagal diam-diam (diabaikan dengan let _)
    run_turn_inner(
        provider,
        registry,
        ctx,
        messages,
        user_prompt,
        max_steps,
        Some(tx),
        TurnMode::Execute,
    )
    .await
}

/// Catatan terlihat yang direkam di transkrip & log sesi saat pengguna menonaktifkan gerbang skill
/// secara eksplisit pada giliran berjalan (Perbaikan 1 v0.5.13).
pub const SKILL_GATE_SKIPPED_NOTE: &str = "[gerbang-skill] dilewati: instruksi pengguna";

fn normalize_ws_ascii_lower(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn contains_word_phrase(haystack: &str, phrase: &str) -> bool {
    let mut start = 0usize;
    while let Some(pos) = haystack[start..].find(phrase) {
        let abs = start + pos;
        let end = abs + phrase.len();
        let before_ok = abs == 0
            || !haystack[..abs]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        let after_ok = end >= haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + phrase.len();
    }
    false
}

/// Deteksi apakah pengguna secara eksplisit melarang pemuatan/pemakaian skill pada giliran berjalan.
pub fn prompt_explicitly_forbids_skill(prompt: &str) -> bool {
    let norm = normalize_ws_ascii_lower(prompt);
    if norm.is_empty() {
        return false;
    }
    const EXPLICIT_SKILL_OPTOUTS: &[&str] = &[
        "jangan muat skill",
        "jangan memuat skill",
        "jangan load skill",
        "jangan pakai skill",
        "jangan memakai skill",
        "jangan gunakan skill",
        "jangan menggunakan skill",
        "jangan panggil skill",
        "jangan buka skill",
        "jangan aktifkan skill",
        "jangan jalankan skill",
        "jangan pakai gerbang skill",
        "tidak usah muat skill",
        "tak usah muat skill",
        "gak usah muat skill",
        "ga usah muat skill",
        "nggak usah muat skill",
        "tidak usah pakai skill",
        "tak usah pakai skill",
        "gak usah pakai skill",
        "ga usah pakai skill",
        "nggak usah pakai skill",
        "tidak perlu muat skill",
        "tidak perlu memuat skill",
        "tak perlu muat skill",
        "tak perlu memuat skill",
        "tidak perlu pakai skill",
        "tidak perlu skill",
        "tak perlu skill",
        "tanpa muat skill",
        "tanpa memuat skill",
        "tanpa pakai skill",
        "tanpa memakai skill",
        "tanpa menggunakan skill",
        "tanpa skill",
        "tanpa gerbang skill",
        "lewati skill",
        "abaikan skill",
        "matikan gerbang skill",
        "don't load skill",
        "dont load skill",
        "do not load skill",
        "don't load any skill",
        "do not load any skill",
        "don't use skill",
        "dont use skill",
        "do not use skill",
        "don't use any skill",
        "do not use any skill",
        "without skill",
        "without any skill",
        "without loading skill",
        "without loading any skill",
        "skip skill",
        "skip skills",
        "disable skill gate",
        "bypass skill gate",
    ];
    if EXPLICIT_SKILL_OPTOUTS.iter().any(|p| norm.contains(p)) {
        return true;
    }
    contains_word_phrase(&norm, "no skill") || contains_word_phrase(&norm, "no skills")
}

/// Deteksi apakah instruksi pengguna pada giliran berjalan meminta melewati gerbang skill
/// (baik penolakan eksplisit skill maupun instruksi langsung eksekusi seperti "langsung saja kerjakan").
pub fn prompt_opts_out_of_skill_gate(prompt: &str) -> bool {
    if prompt_explicitly_forbids_skill(prompt) {
        return true;
    }
    let norm = normalize_ws_ascii_lower(prompt);
    const DIRECT_EXEC_OPTOUTS: &[&str] = &[
        "langsung saja kerjakan",
        "kerjakan langsung saja",
        "langsung kerjakan saja",
        "langsung saja perbaiki",
        "langsung perbaiki saja",
        "langsung eksekusi saja",
    ];
    DIRECT_EXEC_OPTOUTS.iter().any(|p| norm.contains(p))
}

/// Klasifikasi jenis tugas deterministik di level harness (Perbaikan 2 v0.5.11 + Perbaikan 1 v0.5.13):
/// menentukan apakah giliran ini memerlukan penegakan pemuatan skill sebelum aksi pengubah berkas
/// (`systematic-debugging` untuk bug/perbaikan/kegagalan; `test-driven-development` untuk fitur+tes/TDD).
/// - Berdasarkan jenis tugas, BUKAN redaksi prompt tertentu (berlaku baik untuk redaksi terbuka maupun kanonik).
/// - Tugas trivial, tanya-jawab, dan pembuatan skrip sederhana (mis. `rekap.py`, `calc.js` dengan
///   spesifikasi `"cetak error ke stderr"`) mengembalikan `None` (zero spam skill).
/// - Instruksi penolakan eksplisit dari pengguna (`prompt_opts_out_of_skill_gate`) selalu menang
///   dan mengembalikan `None`.
pub fn classify_required_skill_for_turn(
    prompt: &str,
    installed_skills: &[crate::skills::SkillInfo],
) -> Option<String> {
    if prompt_opts_out_of_skill_gate(prompt) {
        return None;
    }
    classify_required_skill_for_turn_raw(prompt, installed_skills)
}

fn classify_required_skill_for_turn_raw(
    prompt: &str,
    installed_skills: &[crate::skills::SkillInfo],
) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() || installed_skills.is_empty() {
        return None;
    }
    let has_skill = |target: &str| -> bool {
        installed_skills
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(target))
    };
    let low = trimmed.to_ascii_lowercase();

    // 1. Fitur baru lengkap dengan tes / TDD -> `test-driven-development`
    if has_skill("test-driven-development") {
        let explicit_tdd = [
            "tdd",
            "test-driven",
            "test driven",
            "tes dulu",
            "test first",
        ]
        .iter()
        .any(|kw| low.contains(kw));
        let has_feature_verb = [
            "fitur",
            "feature",
            "implement",
            "tulis ",
            "buat fungsi",
            "buat modul",
            "buat kelas",
            "buat class",
            "tambah fungsi",
            "tambah fitur",
            "tambahkan ",
        ]
        .iter()
        .any(|kw| low.contains(kw));
        let has_test_req = [
            "lengkap dengan tes",
            "lengkap dengan test",
            "beserta tes",
            "beserta test",
            "dengan tesnya",
            "dengan testnya",
            "dengan tes",
            "dengan test",
            "unit test",
            "integration test",
            "tes otomatis",
            "test otomatis",
            "tulis tes",
            "tulis test",
            "buat tes",
            "buat test",
            "tesnya",
            "testnya",
        ]
        .iter()
        .any(|kw| low.contains(kw));
        if explicit_tdd || (has_feature_verb && has_test_req) {
            return Some("test-driven-development".to_string());
        }
    }

    // 2. Bug / perbaikan / error / diagnosis+jawaban benar -> `systematic-debugging`
    if has_skill("systematic-debugging") {
        // Saring frasa spesifikasi error-handling pada tugas pembuatan fitur baru (mis. "div 0 cetak error ke stderr")
        // agar proyek greenfield biasa tidak salah dikira laporan bug.
        let mut sanitized = low.clone();
        for benign in [
            "cetak error",
            "pesan error",
            "tampilkan error",
            "kembalikan error",
            "lempar error",
            "throw error",
            "return error",
            "error handling",
            "handle error",
            "handling error",
            "error message",
            "on error",
            "kalau error",
            "jika error",
            "bila error",
        ] {
            sanitized = sanitized.replace(benign, " ");
        }

        let is_trivial_qa = [
            "jelaskan ",
            "apa itu ",
            "ringkas ",
            "balas ",
            "tulis satu kalimat",
            "sapa ",
            "sebutkan kata sandi",
            "apa kode rahasia",
        ]
        .iter()
        .any(|p| low.starts_with(p));
        let is_pure_status_query = [
            "cek status",
            "cek lagi status",
            "udah belum",
            "sudah belum",
            "sampai mana",
        ]
        .iter()
        .any(|p| low.starts_with(p))
            && !["perbaiki", "benerin", "retry", "ulang", "fix ", "bug"]
                .iter()
                .any(|kw| low.contains(kw));
        if is_pure_status_query {
            return None;
        }

        let direct_debug = [
            "bug",
            "debug",
            "diagnos",
            "akar masalah",
            "akar penyebab",
            "perbaiki",
            "benerin",
            "fix ",
            "fixing",
            "traceback",
            "stack trace",
            "panic",
            "crash",
            "gagal",
            "failed",
            "salah hitung",
            "hasilnya salah",
            "keliru",
            "tidak sesuai",
            "broken",
            "wrong output",
            "wrong result",
            "error",
        ]
        .iter()
        .any(|kw| sanitized.contains(kw));

        let mismatch_diagnosis = ["seharusnya", "harusnya", "expected", "bukannya", "malah"]
            .iter()
            .any(|kw| low.contains(kw))
            && [
                ".js", ".py", ".rs", ".ts", ".go", "hitung", "fungsi", "skrip", "script", "kode",
                "output", "hasil", "koreksi", "ubah",
            ]
            .iter()
            .any(|kw| low.contains(kw));

        if (!is_trivial_qa && direct_debug) || mismatch_diagnosis {
            return Some("systematic-debugging".to_string());
        }
    }

    None
}

fn is_file_mutating_tool(name: &str) -> bool {
    matches!(name, "edit_file" | "write_file" | "lsp_rename")
}

/// Baris pembuka kegagalan baku saat giliran berakhir paksa di pagu tanpa satu pun mutasi berkas
/// (Perbaikan 2 v0.5.15 — ditegakkan secara programatis pasca-generasi, tidak boleh bergantung pada model).
pub const ZERO_EDIT_FAILURE_HEADER: &str =
    "Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.";

/// Hitung langkah (1-based) pemicu konvergensi belum-ada-edit untuk mode berpagu (`max_steps > 0`):
/// - Rumus inti (v0.5.15): sisa <= 5 langkah ATAU pemakaian >= 85%, mana yang tercapai lebih dulu
///   (`max_steps = 15` -> langkah `10`; `max_steps = 25` -> langkah `20`; `max_steps = 100` -> langkah `85`).
/// - Lantai eksplorasi bermakna (Perbaikan 3 v0.5.16): minimal 50% pagu dan minimal 3 langkah
///   sebelum konvergensi boleh menembak (dibatasi `max_steps - 1`), sehingga pada pagu kecil (<= 6)
///   notice tidak pernah menembak prematur di langkah 1–2 (`max_steps = 6` -> langkah `3`).
pub fn compute_convergence_step(max_steps: usize) -> usize {
    if max_steps == 0 {
        return 0;
    }
    let by_remaining = max_steps.saturating_sub(5);
    let by_ratio = ((max_steps as f64) * 0.85).ceil() as usize;
    let raw = by_remaining.min(by_ratio);
    let min_explore = (max_steps / 2).max(3);
    let cap = max_steps.saturating_sub(1).max(1);
    raw.max(min_explore).min(cap)
}

/// Tegakkan kalimat kegagalan baku secara programatis PASCA-GENERASI (Perbaikan 2 v0.5.15):
/// bila `zero_edit_failure` bernilai `true`, kalimat baku SELALU ditempel di baris pembuka
/// di depan teks model/templat apa pun; di luar kondisi itu teks tidak disentuh.
pub fn enforce_zero_edit_failure_header(closing_text: &str, zero_edit_failure: bool) -> String {
    let trimmed = closing_text.trim();
    if !zero_edit_failure {
        return trimmed.to_string();
    }
    if trimmed.starts_with(ZERO_EDIT_FAILURE_HEADER) {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        ZERO_EDIT_FAILURE_HEADER.to_string()
    } else {
        format!("{ZERO_EDIT_FAILURE_HEADER}\n\n{trimmed}")
    }
}

/// Deteksi apakah giliran pengguna meminta perubahan kode/berkas (Perbaikan 1 v0.5.14 +
/// Perbaikan 2 v0.5.15 + Perbaikan 2 v0.5.16: mengenali bentuk verba, pasif, maupun nomina
/// Indonesia seperti "ada perbaikan yang harus dilakukan", "perlu perubahan", "butuh penambahan",
/// sambil menolak kontrol negatif diskusi/penjelasan seperti "apa perbaikan terbaik...?",
/// "jelaskan perubahan...", dan "kenapa perbaikan kemarin gagal?").
pub fn prompt_requests_file_change(prompt: &str) -> bool {
    let norm = normalize_ws_ascii_lower(prompt);
    if norm.is_empty() {
        return false;
    }
    let explicitly_readonly = [
        "jangan ubah",
        "tanpa mengubah",
        "read-only",
        "read only",
        "baca saja",
        "hanya baca",
    ]
    .iter()
    .any(|kw| norm.contains(kw));
    if explicitly_readonly {
        return false;
    }

    // Verba aksi imperatif / pasif langsung yang menandakan permintaan eksekusi perubahan nyata
    // (tetap menang walau diawali pertanyaan, mis. "Kenapa salah? Tolong perbaiki sekalian.").
    let imperative_action_verbs = [
        "perbaiki",
        "diperbaiki",
        "benerin",
        "dibenerin",
        "betulkan",
        "dibetulkan",
        "ubah ",
        "diubah",
        "ganti ",
        "diganti",
        "edit ",
        "diedit",
        "tulis ",
        "ditulis",
        "buat ",
        "buatkan ",
        "dibuat",
        "tambah ",
        "tambahkan ",
        "ditambah",
        "ditambahkan",
        "hapus ",
        "dihapus",
        "sesuaikan",
        "disesuaikan",
        "perbarui",
        "diperbarui",
        "selesaikan",
        "diselesaikan",
        "lengkapi",
        "dilengkapi",
        "rapikan",
        "dirapikan",
        "koreksi",
        "dikoreksi",
        "terapkan",
        "diterapkan",
        "refactor",
        "implement",
        "fix",
        "patch",
        "resolve",
        "solve ",
        "update ",
        "modify ",
        "change ",
        "write ",
        "create ",
        "add ",
        "remove ",
        "delete ",
    ];
    let has_imperative_action = imperative_action_verbs.iter().any(|kw| norm.contains(kw));

    // Kontrol negatif wajib (Perbaikan 2 #2 v0.5.16): pertanyaan diskusi/penjelasan/analisis tentang
    // perubahan ("apa perbaikan terbaik...?", "jelaskan perubahan...", "kenapa perbaikan kemarin gagal?")
    // BUKAN tugas perubahan selama tidak disertai instruksi aksi perubahan eksplisit.
    let discussion_prefixes = [
        "apa ",
        "apakah ",
        "kenapa ",
        "mengapa ",
        "bagaimana ",
        "gimana ",
        "jelaskan ",
        "ceritakan ",
        "uraikan ",
        "ringkas ",
        "rangkum ",
        "bandingkan ",
        "sebutkan ",
        "balas ",
        "tulis satu kalimat",
        "sapa ",
        "cek status",
        "cek lagi status",
        "udah belum",
        "sudah belum",
        "sampai mana",
        "daftar file",
        "menurutmu ",
        "kira-kira ",
        "what is ",
        "what are ",
        "why did ",
        "why is ",
        "how does ",
        "explain ",
        "summarize ",
        "describe ",
    ];
    if discussion_prefixes.iter().any(|p| norm.starts_with(p)) && !has_imperative_action {
        return false;
    }

    if has_imperative_action {
        return true;
    }

    // Bentuk nomina perubahan Indonesia + konstruksi kebutuhan/permintaan yang lazim (Perbaikan 2 #1 v0.5.16):
    // mis. "ada perbaikan yang harus dilakukan", "perlu perubahan", "butuh penambahan", "sekalian perbaikannya".
    let change_nouns = [
        "perbaikan",
        "perubahan",
        "penambahan",
        "penghapusan",
        "pembaruan",
        "penyesuaian",
        "pembenahan",
        "perapian",
        "pembetulan",
        "penggantian",
        "pembuatan",
        "penerapan",
        "penyelesaian",
        "revisi",
        "modifikasi",
    ];
    let nominal_demand_cues = [
        "ada ",
        "perlu ",
        "butuh ",
        "harus ",
        "wajib ",
        "minta ",
        "tolong ",
        "mohon ",
        "bantu ",
        "coba ",
        "silakan ",
        "sekalian ",
        "lakukan ",
        "dilakukan",
        "kerjakan ",
        "dikerjakan",
        "eksekusi ",
        "diperlukan",
        "dibutuhkan",
        "ingin ",
        "mau ",
    ];
    let has_nominal_request = change_nouns.iter().any(|n| norm.contains(n))
        && nominal_demand_cues.iter().any(|c| norm.contains(c));
    if has_nominal_request {
        // Jika seluruh prompt murni pertanyaan tanpa permintaan tindakan (berakhir `?` tanpa cue permintaan/kewajiban), tolak.
        let is_pure_question = norm.ends_with('?')
            && !["harus ", "tolong ", "mohon ", "bantu ", "sekalian ", "lakukan ", "kerjakan ", "dilakukan"]
                .iter()
                .any(|c| norm.contains(c));
        if !is_pure_question {
            return true;
        }
    }

    // Sinyal laporan bug / issue teknis (mis. problem_statement SWE-bench Lite tanpa verba imperatif langsung).
    let problem_signals = [
        "bug",
        "error",
        "crash",
        "traceback",
        "stack trace",
        "exception",
        "raises",
        "fail",
        "gagal",
        "salah hitung",
        "seharusnya",
        "expected",
        "instead of",
        "incorrect",
        "wrong",
        "should ",
        "broken",
        "issue",
        "problem",
        "regression",
    ];
    let is_pure_question_without_action = norm.ends_with('?') && !has_nominal_request;
    if !is_pure_question_without_action && problem_signals.iter().any(|kw| norm.contains(kw)) {
        return true;
    }

    false
}

/// Batas rentetan stream macet beruntun lintas langkah sebelum circuit breaker berhenti bersih
/// (diporting dari Hermes Agent `agent/chat_completion_helpers.py`: `_consecutive_stale_streams` / `stale_giveup`).
pub const STALE_STREAM_GIVEUP_LIMIT: usize = 3;

/// Klasifikasi kegagalan panggilan API dalam giliran aktif untuk menentukan jalur pemulihan
/// (diporting dari Hermes Agent `agent/error_classifier.py`: `FailoverReason` & `classify_api_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnApiErrorKind {
    /// Stream berhenti mengalir / terputus di tengah / timeout jaringan — layak dipulihkan dengan
    /// prune ringan + retry + circuit breaker rentetan macet.
    StreamStaleOrDrop,
    /// HTTP 429 rate-limit atau 5xx / server overload sementara — retry dengan jittered backoff.
    TransientServerOrRateLimit,
    /// HTTP 413 / konteks melebihi batas provider — kompresi darurat lalu retry di giliran yang sama.
    ContextOverflow,
    /// Error deterministik (400 format, 401/403 auth, 404 model mati, batal user, lokal) — hentikan segera.
    NonRetryable,
}

/// Klasifikasi deterministik pesan error panggilan model pada giliran berjalan.
pub fn classify_turn_api_error(err_msg: &str) -> TurnApiErrorKind {
    let low = err_msg.to_ascii_lowercase();
    if [
        "ditolak user",
        "dibatalkan",
        "cancelled",
        "canceled",
        "belum dikonfigurasi",
        "batas biaya routing",
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "404",
        "model_not_found",
        "model not found",
        "insufficient_quota",
        "billing",
    ]
    .iter()
    .any(|kw| low.contains(kw))
    {
        return TurnApiErrorKind::NonRetryable;
    }
    if [
        "413",
        "payload too large",
        "request entity too large",
        "request_too_large",
        "request exceeds the maximum size",
        "context_length_exceeded",
        "context length exceeded",
        "maximum context length",
        "too many tokens",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|kw| low.contains(kw))
    {
        return TurnApiErrorKind::ContextOverflow;
    }
    if [
        "ketidakaktifan",
        "tidak ada data selama",
        "stream provider macet",
        "stream anthropic macet",
        "stream gemini macet",
        "stream terputus",
        "stream provider terputus",
        "stream anthropic terputus",
        "stream gemini terputus",
        "unexpected eof",
        "connection reset",
        "broken pipe",
        "timeout total panggilan",
        "menunggu respons awal",
        "tidak merespons",
        "timeout",
        "timed out",
    ]
    .iter()
    .any(|kw| low.contains(kw))
    {
        return TurnApiErrorKind::StreamStaleOrDrop;
    }
    if [
        "429",
        "rate limit",
        "rate_limit",
        "too many requests",
        "500",
        "502",
        "503",
        "504",
        "overloaded",
        "temporarily unavailable",
        "bad gateway",
        "service unavailable",
        "connection refused",
        "error sending request",
    ]
    .iter()
    .any(|kw| low.contains(kw))
    {
        return TurnApiErrorKind::TransientServerOrRateLimit;
    }
    TurnApiErrorKind::NonRetryable
}

/// Hitung jeda exponential backoff + uniform jitter `[0, delay/2]` dalam milidetik
/// (diporting dari Hermes Agent `agent/retry_utils.py`: `jittered_backoff`).
/// `attempt` bersifat 1-based (`1, 2, 3, ...`).
pub fn jittered_backoff_ms(attempt: usize, base_ms: u64, max_ms: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    if base_ms == 0 || max_ms == 0 {
        return 0;
    }
    static JITTER_TICK: AtomicU64 = AtomicU64::new(0);
    let tick = JITTER_TICK.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let shift = attempt.saturating_sub(1).min(16) as u32;
    let delay = base_ms.saturating_mul(1u64 << shift).min(max_ms);
    let half = delay / 2;
    if half == 0 {
        return delay;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let seed = (nanos ^ tick.wrapping_mul(0x9E37_79B9)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let jitter = seed % (half + 1);
    delay.saturating_add(jitter)
}

/// Loop (Blueprint Bagian 2): provider → tool_call? → jalankan → ToolResult →
/// ulangi, sampai tidak ada tool_call atau max_steps tercapai.
/// Error tool menjadi ToolResult is_error=true — tidak pernah crash.
async fn run_turn_inner(
    provider: Arc<dyn Provider>,
    registry: &Registry,
    ctx: &Ctx,
    messages: &mut Vec<Message>,
    user_prompt: &str,
    max_steps: usize,
    ui: Option<mpsc::Sender<UiEvent>>,
    mode: TurnMode,
) -> Result<(String, Option<Usage>)> {
    // Progressive Disclosure (tiered toolsets + MCP >15 on-demand index + Skills System + Lean Resume v0.5.7):
    // Pada sesi RESUME (`ctx.is_resumed_session && !messages.is_empty()`), pakai toolset awal super-ramping
    // agar lantai token resume turun drastis sementara seluruh tool tetap dapat diaktifkan di giliran yang sama.
    let has_mcp = registry.names().iter().any(|n| n.starts_with("mcp__"));
    let installed_skills = crate::skills::list_skills(&ctx.cwd);
    let lean_resume = ctx.is_resumed_session && !messages.is_empty();
    // Perbaikan 2 (v0.5.11) + Perbaikan 1 (v0.5.13): klasifikasi jenis tugas di level harness
    // untuk gerbang skill; instruksi eksplisit pengguna pada giliran berjalan selalu menang.
    let raw_required_skill = if mode == TurnMode::Execute && ctx.guard.enabled {
        classify_required_skill_for_turn_raw(user_prompt, &installed_skills)
    } else {
        None
    };
    let skill_gate_opted_out = if mode == TurnMode::Execute && ctx.guard.enabled {
        prompt_explicitly_forbids_skill(user_prompt)
            || (raw_required_skill.is_some() && prompt_opts_out_of_skill_gate(user_prompt))
    } else {
        false
    };
    let required_skill = if skill_gate_opted_out {
        None
    } else {
        raw_required_skill
    };
    let mut active_toolsets = if lean_resume {
        let mut s = crate::tools::toolsets::select_resume_initial_toolsets(
            user_prompt,
            &ctx.cwd,
            has_mcp,
            mode,
        );
        if required_skill.is_some() {
            s.insert("skills_view".to_string());
        }
        if skill_gate_opted_out {
            s.remove("skills_view");
        }
        s
    } else {
        let mut s =
            crate::tools::toolsets::select_initial_toolsets(user_prompt, &ctx.cwd, has_mcp, mode);
        if !installed_skills.is_empty() && !skill_gate_opted_out {
            s.insert("skills_view".to_string());
        }
        if skill_gate_opted_out {
            s.remove("skills_view");
        }
        s
    };
    let mut loaded_mcp_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tools = crate::tools::toolsets::active_tool_specs_for_turn(
        registry,
        &active_toolsets,
        &loaded_mcp_tools,
        mode,
        lean_resume,
    );

    // Helper lokal untuk menyusun System Prompt + Konteks Sesi (Snapshot Awal).
    // Pada sesi baru (`is_resume_call == false`): perilaku identik 100% dengan v0.5.10.
    // Pada giliran RESUME (`is_resume_call == true`, Perbaikan 1 v0.5.11): pakai prompt resume padat
    // (`build_resume_system_prompt`) dan hanya kirim katalog 14 skill penuh bila toolset skill aktif
    // (menghemat ~1.000+ token di SETIAP panggilan giliran resume tanpa mengurangi kemampuan apa pun!).
    let build_fresh_system_prompt = |cur_tools: &[crate::providers::ToolSpec],
                                     cur_active: &std::collections::HashSet<String>,
                                     include_folder_map: bool,
                                     is_resume_call: bool|
     -> String {
        let mut sys = if is_resume_call {
            build_resume_system_prompt(&ctx.cwd, cur_tools)
        } else {
            build_system_prompt(&ctx.cwd, cur_tools)
        };
        // Sentuhan prompt ringan khusus konteks Telegram (CLI/TUI/plain tidak tersentuh sama sekali!).
        if ctx.telegram_chat_id.is_some() {
            sys.push_str(
                "Format Telegram: jawaban dirender ke HTML Telegram untuk layar HP — tulis ringkas, hindari tabel lebar, dan blok kode panjang otomatis dikirim sebagai berkas.\n",
            );
        }
        let mut dynamic_snapshot = String::new();
        let hint = crate::tools::toolsets::inactive_toolsets_hint(registry, cur_active);
        if !hint.is_empty() {
            dynamic_snapshot.push_str(&hint);
            dynamic_snapshot.push('\n');
        }
        let dyn_cap = ctx
            .skills
            .index_max_tokens
            .unwrap_or(ctx.tool_budget.dynamic_inject_max_tokens);
        // Perbaikan 1 Lapis A2 (v0.5.8): peta folder kerja + simbol kunci (pagu ~300 token)
        // disiapkan oleh harness pada awal sesi baru bila `[context] folder_map = true`.
        if include_folder_map && ctx.context_cfg.folder_map {
            let fmap = build_folder_map(&ctx.cwd, ctx.context_cfg.folder_map_max_tokens);
            if !fmap.is_empty() {
                dynamic_snapshot.push_str(&fmap);
                dynamic_snapshot.push('\n');
            }
        }
        let include_full_skills_idx = !is_resume_call
            || cur_active.contains("skills_view")
            || cur_active.contains("skills_mem");
        if include_full_skills_idx {
            let skills_idx = crate::skills::format_skills_index_budgeted(&installed_skills, dyn_cap);
            if !skills_idx.is_empty() {
                dynamic_snapshot.push_str(&skills_idx);
                dynamic_snapshot.push('\n');
            }
        } else if !installed_skills.is_empty() {
            dynamic_snapshot.push_str(&format!(
                "Skills terpasang ({}): aktifkan `skills_view`/`skills_mem` bila butuh.\n",
                installed_skills.len()
            ));
        }
        if let Some(mcp_idx) = crate::tools::toolsets::build_mcp_compact_index(registry) {
            dynamic_snapshot.push_str(&mcp_idx);
            dynamic_snapshot.push('\n');
        }
        // Perbaikan 5 (v0.5.7) & Perbaikan 3 (v0.5.9): SATU sumber kebenaran memori persisten —
        // hanya snapshot beku wadah `memory` + `profile` yang dimuat 1x di awal sesi (tanpa store SQLite ganda).
        if let Some(frozen) = &ctx.frozen_user_memory {
            let frozen_block = frozen.format_for_prompt();
            if !frozen_block.is_empty() {
                dynamic_snapshot.push_str(&frozen_block);
            }
        }
        if !dynamic_snapshot.trim().is_empty() {
            let capped = cap_dynamic_injection(dynamic_snapshot.trim(), dyn_cap);
            sys.push_str("\n--- Konteks Sesi (Snapshot Awal) ---\n");
            sys.push_str(&capped);
            sys.push('\n');
        }
        sys
    };

    // Tahap 3 & Tahap 6 & Skills Tahap 2: Bekukan System Prompt statis di awal sesi (byte-identik),
    // diikuti snapshot blok dinamis (toolsets + peta folder kerja + indeks skills + indeks MCP + memori beku).
    let mut auto_loaded_skill_msgs: Vec<Message> = Vec::new();
    if messages.is_empty() {
        let sys = build_fresh_system_prompt(&tools, &active_toolsets, true, false);
        let sys_msg = Message::text(Role::System, sys);
        persist_message(ctx, &sys_msg);
        messages.push(sys_msg);

        // Tahap 5: Auto-load HANYA aktif jika user secara eksplisit mengonfigurasi `[skills] auto_load = [...]`
        // (bawaan kosong = nol skill dimuat otomatis!).
        for sk_name in &ctx.skills.auto_load {
            let trimmed = sk_name.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok((info, content)) = crate::skills::load_skill(&ctx.cwd, trimmed) {
                auto_loaded_skill_msgs.push(Message::text(
                    Role::User,
                    format!("[Skill auto_load: {}]\n{content}", info.name),
                ));
            }
        }
        for sm in auto_loaded_skill_msgs.drain(..) {
            persist_message(ctx, &sm);
            messages.push(sm);
        }
    }
    // Lampiran eksplisit `@path/to/file` (ala OpenCode) + konteks pintar auto-outline
    // (hanya untuk prompt substantif >= 25 karakter agar pertanyaan singkat `/resume` super hemat!).
    let mut user_text = user_prompt.to_string();
    let (at_block, attached_files) = expand_at_mentions(&ctx.cwd, user_prompt);
    if !at_block.is_empty() {
        user_text.push_str("\n\n");
        user_text.push_str(&at_block);
        let note = format!("Melampirkan @file: {}", attached_files.join(", "));
        match &ui {
            Some(tx) => {
                let _ = tx.send(UiEvent::Info(note)).await;
            }
            None => eprintln!("[selen] {note}"),
        }
    } else if user_prompt.trim().chars().count() >= 25 {
        let auto = auto_outline(&ctx.cwd, user_prompt);
        if !auto.is_empty() {
            user_text.push_str("\n\n");
            user_text.push_str(&auto);
        } else if ctx.self_correct {
            // Tahap 2: Smart Context Retrieval otomatis (hanya saat skor simbol/path tinggi >= 35,
            // budget ketat 2 KB agar hemat token & tidak mengganggu sapaan biasa).
            let hits = ctx
                .repo_index
                .lock()
                .ok()
                .map(|mut idx| idx.query(user_prompt, 3, 2048))
                .unwrap_or_default();
            if hits.first().map(|h| h.score >= 35).unwrap_or(false) {
                let smart_block = crate::context_index::format_hits_for_prompt(&hits, 2048);
                if !smart_block.is_empty() {
                    user_text.push_str("\n\n[Konteks relevan otomatis (Smart Index)]\n");
                    user_text.push_str(&smart_block);
                }
            }
        }
    }
    let (image_blocks, attached_images) = extract_at_images(&ctx.cwd, user_prompt);
    if !attached_images.is_empty() {
        let note = format!("Melampirkan gambar: {}", attached_images.join(", "));
        match &ui {
            Some(tx) => {
                let _ = tx.send(UiEvent::Info(note)).await;
            }
            None => eprintln!("[selen] {note}"),
        }
    }
    if skill_gate_opted_out {
        match &ui {
            Some(tx) => {
                let _ = tx
                    .send(UiEvent::Info(SKILL_GATE_SKIPPED_NOTE.to_string()))
                    .await;
            }
            None => eprintln!("{SKILL_GATE_SKIPPED_NOTE}"),
        }
        user_text.push_str("\n\n");
        user_text.push_str(SKILL_GATE_SKIPPED_NOTE);
    }
    let mut user_blocks = vec![Block::Text(user_text)];
    user_blocks.extend(image_blocks);
    let user_msg = Message {
        role: Role::User,
        content: user_blocks,
    };
    persist_message(ctx, &user_msg);
    messages.push(user_msg);
    let mut last_text = String::new();
    let mut turn_usage = Usage::default();
    let mut have_usage = false;
    let mut tool_calls_executed = 0usize;
    let mut skill_touched_in_turn = false;
    let turn_checkpoint_id = if ctx.self_correct && mode == TurnMode::Execute {
        let short_label: String = user_prompt.trim().chars().take(40).collect();
        let label = if short_label.is_empty() {
            "giliran agent".to_string()
        } else {
            format!("giliran: {short_label}")
        };
        ctx.undo_stack.lock().ok().map(|mut st| st.begin_checkpoint(label))
    } else {
        None
    };
    let mut loop_guard = if ctx.guard.enabled {
        LoopGuard::with_hermes_guards()
    } else {
        LoopGuard::new()
    };
    let mut dedup = ToolDedupCache::new();
    let sc_max = if ctx.guard.enabled {
        ctx.guard.max_self_correct_rounds
    } else {
        super::corrector::MAX_REPAIR_ATTEMPTS
    };
    let mut corrector = SelfCorrectState::new(turn_checkpoint_id)
        .with_max_attempts(sc_max)
        .with_strict_known_verify_only(true);
    let sid_for_archive = ctx
        .session_id
        .clone()
        .unwrap_or_else(|| "ephemeral".to_string());
    let prune_dir = session_archive_dir("pruned", &sid_for_archive);
    let mut turn_tool_chars_used = 0usize;
    // v0.5.17 Masalah 2: JANGAN menggelembungkan pagu output tool per giliran hingga 180.000 char
    // hanya karena model di atas kertas berjendela 1M (mis. Gemini di 9Router). Skala hanya turun
    // untuk model kecil (<100k), dan patok batas atas pada `turn_budget_chars` (bawaan 60.000 char).
    let scaled_turn_budget = if ctx.context_limit_tokens < 100_000 {
        ((ctx.tool_budget.turn_budget_chars as u128)
            * (ctx.context_limit_tokens.max(20_000) as u128)
            / 100_000)
            .clamp(20_000, ctx.tool_budget.turn_budget_chars.max(20_000) as u128) as usize
    } else {
        ctx.tool_budget.turn_budget_chars.clamp(24_000, 60_000)
    };
    let mut last_step_deduped = 0usize;
    let mut last_announced_fallback_model: Option<String> = None;
    let mut unconfirmed_search_calls: usize = 0;
    let mut anti_spin_reminded = false;
    let mut turn_actions: Vec<ToolActionEvent> = Vec::new();
    let mut forced_stop_reason: Option<String> = None;
    let mut loaded_skills_in_turn: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut skill_gate_fired = false;
    // v0.5.17 Masalah 1 (Hermes `agent/chat_completion_helpers.py`): pelacak rentetan stream macet
    // lintas langkah — direset ke 0 setiap kali panggilan model berhasil.
    let mut consecutive_stale_streams: usize = 0;
    // v0.5.17 Masalah 2 (Hermes `agent/usage_anchor.py`): jangkar token aktual terakhir dari provider
    // agar pemangkasan proaktif menyala berdasarkan ukuran riil, bukan hanya perkiraan kasar.
    let mut last_provider_prompt_tokens: usize = 0;
    let mut last_est_tokens_at_call: usize = 0;
    // Perbaikan 1 (v0.5.14 + v0.5.15): kesadaran anggaran langkah.
    // `max_steps == 0` berarti TANPA BATAS ala Hermes (tidak ada pagu tersembunyi);
    // `max_steps > 0` berlaku sebagai pagu keras dengan checkpoint rasio + konvergensi tepat spesifikasi.
    let has_step_cap = max_steps > 0;
    let budget_ratio = ctx.guard.budget_warning_ratio.clamp(0.1, 0.95);
    let max_before_last = max_steps.saturating_sub(1).max(1);
    let checkpoint_step = if has_step_cap {
        ((max_steps as f64) * budget_ratio)
            .round()
            .clamp(1.0, max_before_last as f64) as usize
    } else {
        0
    };
    let convergence_step = if has_step_cap {
        compute_convergence_step(max_steps)
    } else {
        ctx.guard.no_edit_convergence_steps.max(1)
    };
    let mut budget_checkpoint_injected = false;
    let mut convergence_notice_injected = false;
    let turn_requests_edit = mode == TurnMode::Execute && prompt_requests_file_change(user_prompt);

    for step in 0usize.. {
        if has_step_cap && step >= max_steps {
            break;
        }
        // Perbaikan 1 (v0.5.9) — Prune lebih dini hasil `skill_view` lama (di luar 2 pesan terakhir =
        // tepat sesudah model memakainya di 1 giliran berikutnya) ke stub ringkas + arsip byte-exact
        // agar isi skill tidak terbawa penuh di sisa sesi.
        let pruned_skills = prune_old_skill_results_with_archive(messages, &prune_dir, 2);
        // Tahap 1 & Perbaikan 1 (v0.5.11): pada giliran resume step lanjutan (`lean_resume && step >= 2`),
        // pangkas hasil tool besar (>200 char) di luar 2 pesan terakhir ke stub arsip agar tidak menempel
        // antar-panggilan; sesi biasa mempertahankan jendela 20 pesan terakhir.
        let tool_keep_tail = if lean_resume && step >= 2 { 2 } else { 20 };
        let mut pruned_now =
            pruned_skills + prune_old_tool_results_with_archive(messages, &prune_dir, tool_keep_tail);
        if pruned_now > 0 {
            dedup.clear_file_reads();
        }
        // v0.5.17 Masalah 2 (diporting dari Hermes `agent/turn_preflight.py` + `agent/context_compressor_prune.py`):
        // Pemangkasan proaktif 3-pass (dedup hasil tool identik + arsip ToolResult lama + ringkas argumen
        // ToolUse raksasa seperti `write_file` 16KB) berdasarkan tekanan token riil, TANPA menunggu
        // `messages.len() > 20` atau 80% jendela 1M!
        if ctx.tool_budget.proactive_prune_tokens > 0 {
            let est_pre = analyze_token_breakdown(messages, &tools, registry.names().len()).total_tokens();
            let anchored_tokens = if last_provider_prompt_tokens > 0 {
                est_pre.max(
                    last_provider_prompt_tokens
                        .saturating_add(est_pre.saturating_sub(last_est_tokens_at_call)),
                )
            } else {
                est_pre
            };
            let proactive_trigger = ctx
                .tool_budget
                .proactive_prune_tokens
                .min((ctx.context_limit_tokens * 45 / 100).max(4_000));
            if anchored_tokens >= proactive_trigger {
                let mut pres = proactive_prune_context_with_archive(
                    messages,
                    &prune_dir,
                    anchored_tokens,
                    proactive_trigger,
                    ctx.tool_budget.proactive_prune_min_reclaim_tokens,
                    4,
                    false,
                );
                if pres.pruned_count == 0 && anchored_tokens >= proactive_trigger.saturating_mul(3) / 2 {
                    pres = proactive_prune_context_with_archive(
                        messages,
                        &prune_dir,
                        anchored_tokens,
                        proactive_trigger,
                        ctx.tool_budget.proactive_prune_min_reclaim_tokens,
                        2,
                        false,
                    );
                }
                if pres.pruned_count > 0 {
                    pruned_now += pres.pruned_count;
                    dedup.clear_file_reads();
                    let note = format!(
                        "✦ Prune proaktif konteks: {} item diringkas (~{} token dibebaskan, arsip tersimpan)",
                        pres.pruned_count, pres.reclaimed_tokens
                    );
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(note)).await;
                        }
                        None => eprintln!("[selen] {note}"),
                    }
                }
            }
        }
        // Kompresi dua tahap: hanya ringkas jika MASIH > 50% sesudah Tahap 1 prune!
        if needs_compaction(messages, ctx.context_limit_tokens) {
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::Info("compaction dua tahap dijalankan".into()))
                        .await;
                }
                None => eprintln!("[selen] compaction dua tahap dijalankan"),
            }
            *messages = compact_messages_hermes(messages);
            dedup.clear_file_reads();
        }
        // Provider dipindah ke task agar StreamEvent bisa dikonsumsi sambil jalan.
        // Bungkus dalam `AbortOnDrop` (v0.5.10 Perbaikan 1) agar saat giliran dibatalkan
        // (timeout / `/cancel` / interupsi), soket HTTPS provider LANGSUNG ditutup, bukan dibiarkan
        // menggantung ESTABLISHED di latar belakang!
        struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);
        impl<T> Drop for AbortOnDrop<T> {
            fn drop(&mut self) {
                if let Some(h) = self.0.take() {
                    h.abort();
                }
            }
        }
        // v0.5.17 Masalah 1 (diporting dari Hermes `agent/turn_api_error.py`, `agent/retry_utils.py`,
        // `agent/chat_completion_helpers.py`): loop pemulihan panggilan API di dalam giliran agar
        // stream macet (>90s/180s), koneksi putus tengah jalan, 429/5xx sementara, atau 413 overflow
        // TIDAK langsung membunuh giliran kerja pengguna!
        let mut api_retry_count: usize = 0;
        let mut overflow_compactions: usize = 0;
        let (reply, breakdown) = loop {
            // Perbaikan 1 (v0.5.7 & v0.5.11): untuk sesi RESUME, bangun snapshot ramping (system prompt
            // resume padat tanpa folder_map/katalog skill penuh + ringkas riwayat pra-resume) tanpa mengubah DB!
            let mut snapshot = if lean_resume {
                let fresh_sys = build_fresh_system_prompt(&tools, &active_toolsets, false, true);
                build_resume_snapshot(messages, Some(&fresh_sys))
            } else {
                messages.clone()
            };
            // PLAN_NOTE hanya disisipkan sementara pada snapshot giliran Plan (tidak
            // disimpan ke riwayat/DB) agar giliran Execute berikutnya tidak ikut
            // terlarang menulis.
            if mode == TurnMode::Plan {
                let pos = if snapshot.first().map(|m| m.role == Role::System).unwrap_or(false) {
                    1
                } else {
                    0
                };
                snapshot.insert(pos, Message::text(Role::System, PLAN_NOTE));
            }
            // Tahap 0 — Instrumentasi rincian estimasi token per komponen dari snapshot AKTUAL yang dikirim!
            let breakdown = analyze_token_breakdown(&snapshot, &tools, registry.names().len());
            if api_retry_count == 0 && overflow_compactions == 0 {
                if let Some(tx) = &ui {
                    let est = breakdown.total_tokens();
                    let pct = calc_context_pct(est, ctx.context_limit_tokens);
                    let _ = tx
                        .send(UiEvent::ContextInfo {
                            used_tokens: est,
                            limit_tokens: ctx.context_limit_tokens,
                        })
                        .await;
                    let _ = tx
                        .send(UiEvent::Step {
                            current: step + 1,
                            max: max_steps,
                            context_pct: pct,
                        })
                        .await;
                }
            }
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(128);
            let p = provider.clone();
            let tools_c = tools.clone();
            let mut handle_guard = AbortOnDrop(Some(tokio::spawn(async move {
                p.stream_chat(&snapshot, &tools_c, tx).await
            })));
            let mut printed = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Token(t) => match &ui {
                        Some(utx) => {
                            let _ = utx.send(UiEvent::Token(t)).await;
                        }
                        None => {
                            print!("{t}");
                            let _ = std::io::stdout().flush();
                            printed = true;
                        }
                    },
                }
            }
            let call_res = handle_guard
                .0
                .take()
                .expect("handle provider tersedia")
                .await
                .map_err(|e| anyhow::anyhow!("task provider panik: {e}"))
                .and_then(|r| r);
            if printed {
                println!();
            }
            match call_res {
                Ok(r) => {
                    consecutive_stale_streams = 0;
                    break (r, breakdown);
                }
                Err(err) => {
                    let err_str = err.to_string();
                    match classify_turn_api_error(&err_str) {
                        TurnApiErrorKind::NonRetryable => return Err(err),
                        TurnApiErrorKind::ContextOverflow => {
                            if overflow_compactions >= 2 {
                                return Err(err);
                            }
                            overflow_compactions += 1;
                            let em = proactive_prune_context_with_archive(
                                messages,
                                &prune_dir,
                                usize::MAX,
                                1,
                                0,
                                2,
                                true,
                            );
                            pruned_now += em.pruned_count;
                            if em.pruned_count == 0 {
                                *messages = compact_messages_hermes(messages);
                            }
                            dedup.clear_file_reads();
                            let note = format!(
                                "✦ Konteks melebihi batas provider — meringkas {} item & mencoba ulang otomatis ({overflow_compactions}/2)…",
                                em.pruned_count
                            );
                            match &ui {
                                Some(tx) => {
                                    let _ = tx.send(UiEvent::Info(note)).await;
                                }
                                None => eprintln!("[selen] {note}"),
                            }
                            continue;
                        }
                        TurnApiErrorKind::StreamStaleOrDrop => {
                            consecutive_stale_streams += 1;
                            if consecutive_stale_streams >= STALE_STREAM_GIVEUP_LIMIT {
                                return Err(anyhow::anyhow!(
                                    "Provider tidak merespons / stream macet {consecutive_stale_streams}x beruntun — panggilan dihentikan agar tidak macet tanpa akhir (penyebab terakhir: {err_str})"
                                ));
                            }
                            if api_retry_count >= ctx.api_max_retries {
                                return Err(err);
                            }
                            api_retry_count += 1;
                            // Diet darurat sebelum retry agar payload ulang lebih ringan bagi proxy/9Router!
                            let em = proactive_prune_context_with_archive(
                                messages,
                                &prune_dir,
                                breakdown.total_tokens(),
                                1,
                                0,
                                2,
                                true,
                            );
                            if em.pruned_count > 0 {
                                pruned_now += em.pruned_count;
                                dedup.clear_file_reads();
                            }
                            let delay_ms = jittered_backoff_ms(
                                api_retry_count,
                                ctx.api_retry_base_delay_ms,
                                30_000,
                            );
                            let prune_suffix = if em.pruned_count > 0 {
                                format!(", {} item konteks diringkas", em.pruned_count)
                            } else {
                                String::new()
                            };
                            let note = format!(
                                "✦ Stream provider macet/terputus — mencoba ulang otomatis ({api_retry_count}/{}{prune_suffix}) dalam {:.1} dtk…",
                                ctx.api_max_retries,
                                delay_ms as f64 / 1000.0
                            );
                            match &ui {
                                Some(tx) => {
                                    let _ = tx.send(UiEvent::Info(note)).await;
                                }
                                None => eprintln!("[selen] {note}"),
                            }
                            if delay_ms > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            }
                            continue;
                        }
                        TurnApiErrorKind::TransientServerOrRateLimit => {
                            if api_retry_count >= ctx.api_max_retries {
                                return Err(err);
                            }
                            api_retry_count += 1;
                            let delay_ms = jittered_backoff_ms(
                                api_retry_count,
                                ctx.api_retry_base_delay_ms,
                                30_000,
                            );
                            let note = format!(
                                "✦ Provider sibuk/sementara gagal — mencoba ulang otomatis ({api_retry_count}/{}) dalam {:.1} dtk…",
                                ctx.api_max_retries,
                                delay_ms as f64 / 1000.0
                            );
                            match &ui {
                                Some(tx) => {
                                    let _ = tx.send(UiEvent::Info(note)).await;
                                }
                                None => eprintln!("[selen] {note}"),
                            }
                            if delay_ms > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            }
                            continue;
                        }
                    }
                }
            }
        };
        if let Some(u) = reply.usage {
            if u.input_tokens > 0 {
                last_provider_prompt_tokens = u.input_tokens as usize;
                last_est_tokens_at_call = breakdown.total_tokens();
            }
        }
        // BAGIAN 1 (#4 & #5): catat model aktual yang melayani panggilan & beri tahu user 1x per perpindahan fallback.
        let (served_model_opt, was_fallback, fallback_notice) = ctx
            .router_telemetry
            .as_ref()
            .and_then(|t| t.lock().ok().map(|mut g| g.take_last_call_info()))
            .unwrap_or((None, false, None));
        let actual_model = served_model_opt
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| provider.model_name());
        if was_fallback {
            if let Some(notice) = fallback_notice {
                if last_announced_fallback_model.as_deref() != Some(actual_model) {
                    last_announced_fallback_model = Some(actual_model.to_string());
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(notice)).await;
                        }
                        None => eprintln!("[selen] {notice}"),
                    }
                }
            }
        }
        let usage_kind = if was_fallback { "fallback" } else { "main" };
        persist_usage_kind(ctx, actual_model, reply.usage, usage_kind);
        // Tahap 0 — Catat rincian token per panggilan API ke SQLite `call_token_metrics`.
        if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
            let (act_in, act_out) = reply
                .usage
                .map(|u| (u.input_tokens, u.output_tokens))
                .unwrap_or((0, 0));
            let _ = store.record_call_token_metric(&crate::store::CallTokenMetric {
                session_id: sid.clone(),
                model: actual_model.to_string(),
                est_system_tokens: breakdown.system_tokens,
                est_tools_tokens: breakdown.tools_tokens,
                est_history_tokens: breakdown.conversation_tokens,
                est_tool_results_tokens: breakdown.tool_results_tokens,
                actual_input_tokens: act_in,
                actual_output_tokens: act_out,
                active_tools_count: breakdown.active_tools,
                pruned_count: pruned_now,
                deduped_count: last_step_deduped,
            });
        }
        last_step_deduped = 0;
        if let Some(u) = reply.usage {
            turn_usage.input_tokens += u.input_tokens;
            turn_usage.output_tokens += u.output_tokens;
            have_usage = true;
        }
        last_text = reply.text.clone();

        // Perbaikan 2 (v0.5.11) — Gerbang Skill di level loop harness (bukan sekadar kalimat prompt!):
        // Bila tugas tergolong debugging/TDD dan model mencoba menjalankan aksi pengubah berkas
        // (`edit_file`/`write_file`/`lsp_rename`) SEBELUM memuat skill yang cocok, tahan aksi pengubah
        // (disk belum tersentuh), jalankan `skill_view` salinan kerja ringkas di harness, rekam pasangan
        // `ToolUse`/`ToolResult` yang sah ke riwayat + DB, lalu lanjutkan langkah berikutnya.
        if let Some(req_sk) = required_skill.as_deref() {
            let attempts_mutation = reply
                .tool_calls
                .iter()
                .any(|c| is_file_mutating_tool(&c.name));
            let loads_req_in_batch = reply.tool_calls.iter().any(|c| {
                c.name == "skill_view"
                    && c.args
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|n| n.eq_ignore_ascii_case(req_sk))
                        .unwrap_or(false)
            });
            if attempts_mutation
                && !loads_req_in_batch
                && !loaded_skills_in_turn.contains(req_sk)
                && !skill_gate_fired
            {
                if let Some(sv_tool) = registry.get("skill_view") {
                    skill_gate_fired = true;
                    let gate_call_id = format!("gate_skill_{}", step + 1);
                    let gate_args = serde_json::json!({ "name": req_sk });
                    let preview_arg = prepare_tool_preview(
                        "skill_view",
                        &gate_args,
                        ctx.tool_preview_length,
                    );
                    match &ui {
                        Some(tx) => {
                            let _ = tx
                                .send(UiEvent::ToolStart {
                                    name: "skill_view".into(),
                                    args: preview_arg.clone(),
                                })
                                .await;
                        }
                        None => eprintln!(
                            "[tool] {}",
                            format_tool_event_from_preview("skill_view", &preview_arg)
                        ),
                    }
                    let (mut sv_out, sv_err) = match sv_tool.run(gate_args.clone(), ctx).await {
                        Ok(out) => (redact_secrets(&out), false),
                        Err(e) => (redact_secrets(&format!("ERROR: {e}")), true),
                    };
                    if !sv_err {
                        loaded_skills_in_turn.insert(req_sk.to_string());
                        sv_out.push_str(
                            "\n\n[Gerbang Skill Harness] Aksi ubah berkas ditahan sebelum skill ini dimuat. \
                             Ikuti prosedur investigasi/tes di atas terlebih dahulu, lalu lanjutkan perubahan berkas.",
                        );
                    }
                    let gate_asst = Message {
                        role: Role::Assistant,
                        content: vec![Block::ToolUse {
                            id: gate_call_id.clone(),
                            name: "skill_view".into(),
                            args: gate_args,
                        }],
                    };
                    persist_message(ctx, &gate_asst);
                    messages.push(gate_asst);
                    let gate_tool = Message {
                        role: Role::Tool,
                        content: vec![Block::ToolResult {
                            id: gate_call_id,
                            content: sv_out.clone(),
                            is_error: sv_err,
                        }],
                    };
                    persist_message(ctx, &gate_tool);
                    messages.push(gate_tool);
                    turn_actions.push(ToolActionEvent {
                        name: "skill_view".into(),
                        preview: preview_arg,
                        is_error: sv_err,
                        error_reason: sv_err.then(|| short_error_reason(&sv_out)),
                    });
                    tool_calls_executed += 1;
                    if let Some(tx) = &ui {
                        let _ = tx
                            .send(UiEvent::ToolDone {
                                name: "skill_view".into(),
                                summary: summarize(&sv_out, 200),
                                is_error: sv_err,
                            })
                            .await;
                    }
                    continue;
                }
            }
        }

        // Rekonstruksi pesan assistant (teks + tool_use).
        let mut blocks: Vec<Block> = Vec::new();
        if !reply.text.is_empty() {
            blocks.push(Block::Text(reply.text.clone()));
        }
        for c in &reply.tool_calls {
            blocks.push(Block::ToolUse {
                id: c.id.clone(),
                name: c.name.clone(),
                args: c.args.clone(),
            });
        }
        if !blocks.is_empty() {
            let asst_msg = Message {
                role: Role::Assistant,
                content: blocks,
            };
            persist_message(ctx, &asst_msg);
            messages.push(asst_msg);
        }

        if reply.tool_calls.is_empty() {
            // Self-Correction Gate (Tahap 1) + Daftar Periksa Penutup ala SWE-agent (Perbaikan 2 #4 v0.5.8):
            // verifikasi terakhir WAJIB sesudah edit terakhir, berkas sampah/repro dicek, & ringkasan menyebut file diubah.
            if ctx.self_correct && mode == TurnMode::Execute {
                let rem_for_gate = if has_step_cap {
                    max_steps.saturating_sub(step + 1)
                } else {
                    usize::MAX
                };
                if let Some(gate_msg) = corrector.completion_gate_with_checklist(
                    rem_for_gate,
                    &last_text,
                    &ctx.cwd,
                ) {
                    let info = format!(
                        "✦ Self-correction ({}/{}): memeriksa verifikasi & daftar periksa penutup…",
                        corrector.repair_attempts.max(1),
                        corrector.max_attempts.max(1)
                    );
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(info)).await;
                        }
                        None => eprintln!("[selen] {info}"),
                    }
                    messages.push(Message::text(Role::User, gate_msg));
                    continue;
                }
                // Perbaikan 2 #3 (v0.5.14) + Perbaikan 3 #2 & Perbaikan 4 (v0.5.15):
                // tempelkan baris faktual penutup secara programatis (status gagal jujur sesudah batas,
                // kutipan hasil perintah verifikasi mapan / "belum diverifikasi", & catatan berkas tes diubah).
                let mut footer_lines: Vec<String> = Vec::new();
                if let Some(honest_note) = corrector.honest_unverified_closing_note(&last_text) {
                    footer_lines.push(honest_note.trim().to_string());
                }
                if let Some(ver_line) = corrector.closing_verification_summary_line(turn_requests_edit)
                {
                    if !last_text.contains(&ver_line) {
                        footer_lines.push(ver_line);
                    }
                }
                if let Some(test_line) = corrector.closing_test_files_note() {
                    if !last_text.contains(&test_line) {
                        footer_lines.push(test_line);
                    }
                }
                if !footer_lines.is_empty() {
                    let footer = format!("\n\n{}", footer_lines.join("\n"));
                    last_text.push_str(&footer);
                    let note_msg = Message::text(Role::Assistant, footer.trim().to_string());
                    persist_message(ctx, &note_msg);
                    messages.push(note_msg);
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Token(footer)).await;
                        }
                        None => println!("{}", footer.trim()),
                    }
                }
            }
            if let Some(cp_id) = turn_checkpoint_id {
                if corrector.edited_files.is_empty() {
                    if let Ok(mut st) = ctx.undo_stack.lock() {
                        st.discard_empty_checkpoint(cp_id);
                    }
                } else {
                    let note = format!(
                        "✦ Checkpoint #{cp_id} tersimpan ({} file diubah · /rollback {cp_id} untuk batalkan)",
                        corrector.edited_files.len()
                    );
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(note)).await;
                        }
                        None => eprintln!("[selen] {note}"),
                    }
                }
            }
            // Background Self-Improvement Review (v0.5.1) kini dijalankan di latar
            // sesudah sesi berakhir atau setiap 15 giliran tuntas oleh runner (CLI/TUI/Telegram)
            // memakai model aux (`[aux] review_model` / `compression_model`), tidak lagi
            // memblokir giliran aktif ataupun memakai model utama secara diam-diam.
            let _ = (tool_calls_executed, skill_touched_in_turn);
            if let Some(tx) = &ui {
                let reported = reply
                    .usage
                    .map(|u| (u.input_tokens + u.output_tokens) as usize)
                    .unwrap_or(0);
                let used = estimate_tokens(messages).max(reported);
                let pct = calc_context_pct(used, ctx.context_limit_tokens);
                let _ = tx
                    .send(UiEvent::ContextInfo {
                        used_tokens: used,
                        limit_tokens: ctx.context_limit_tokens,
                    })
                    .await;
                let _ = tx
                    .send(UiEvent::Step {
                        current: step + 1,
                        max: max_steps,
                        context_pct: pct,
                    })
                    .await;
                let _ = tx
                    .send(UiEvent::TurnDone {
                        usage: have_usage.then_some(turn_usage),
                    })
                    .await;
            }
            return Ok((last_text, have_usage.then_some(turn_usage)));
        }

        // Jalankan tool satu per satu; error -> ToolResult is_error.
        // Catat apakah giliran ini melakukan pencarian ter-batch (maks +1 terhadap guard anti-berputar
        // per giliran assistant agar riset ter-batch paralel TIDAK dihukum — Perbaikan 1 Lapis A #3).
        let mut results: Vec<Block> = Vec::new();
        let mut step_had_search = false;
        let mut step_asked_user = false;
        for call in &reply.tool_calls {
            let preview_arg = prepare_tool_preview(&call.name, &call.args, ctx.tool_preview_length);
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::ToolStart {
                            name: call.name.clone(),
                            args: preview_arg.clone(),
                        })
                        .await;
                }
                None => eprintln!(
                    "[tool] {}",
                    format_tool_event_from_preview(&call.name, &preview_arg)
                ),
            }
            // Redaksi rahasia diterapkan di SATU titik pusat ini (Fase 2):
            // semua output tool — sekarang dan yang akan datang — teredaksi
            // sebelum masuk ToolResult yang dikirim ke provider. Output tool
            // tidak pernah ditampilkan mentah ke user lokal oleh loop (hanya
            // nama tool di stderr / ringkasan teredaksi di TUI), jadi redaksi
            // di sini murni melindungi sisi provider.
            let (mut content, is_error) =
                if call.name == crate::tools::toolsets::ACTIVATE_TOOLSET_NAME {
                    let requested: Vec<String> = call
                        .args
                        .get("toolsets")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.trim().to_ascii_lowercase()))
                                .filter(|s| !s.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut activated = Vec::new();
                    for ts in requested {
                        if active_toolsets.insert(ts.clone()) {
                            activated.push(ts);
                        }
                    }
                    tools = crate::tools::toolsets::active_tool_specs_for_turn(
                        registry,
                        &active_toolsets,
                        &loaded_mcp_tools,
                        mode,
                        lean_resume,
                    );
                    let active_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
                    (
                        format!(
                            "Toolset diaktifkan: {:?}. Tool aktif sekarang ({}): {}",
                            activated,
                            active_names.len(),
                            active_names.join(", ")
                        ),
                        false,
                    )
                } else if call.name == crate::tools::toolsets::LOAD_MCP_TOOLS_NAME {
                    let req_names: Vec<String> = call
                        .args
                        .get("names")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                                .filter(|s| !s.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut loaded = Vec::new();
                    for nm in req_names {
                        if registry.get(&nm).is_some() && loaded_mcp_tools.insert(nm.clone()) {
                            loaded.push(nm);
                        }
                    }
                    tools = crate::tools::toolsets::active_tool_specs_for_turn(
                        registry,
                        &active_toolsets,
                        &loaded_mcp_tools,
                        mode,
                        lean_resume,
                    );
                    (
                        format!(
                            "Skema penuh MCP dimuat ({}): {}",
                            loaded.len(),
                            loaded.join(", ")
                        ),
                        false,
                    )
                } else if mode == TurnMode::Plan
                    && !is_read_only(&call.name)
                    && call.name != "ask_user"
                {
                    // Penegakan plan mode (Fase 6): model tetap memanggil
                    // tool menulis (halusinasi) -> tolak TANPA menjalankan.
                    (
                        format!(
                            "ERROR: tool '{}' tidak tersedia di plan mode (giliran ini baca-saja). \
                             Selesaikan penjelajahanmu dan tutup dengan rencana di bawah judul \"## Rencana\".",
                            call.name
                        ),
                        true,
                    )
                } else if ctx.guard.enabled {
                    if let Some(dedup_stub) = dedup.check_hit(&call.name, &call.args, &ctx.cwd) {
                        last_step_deduped += 1;
                        (dedup_stub, false)
                    } else {
                        let args_str = call.args.to_string();
                        let pre_res = if !ctx.hooks.is_empty() {
                            ctx.hooks
                                .run_pre_tool(&call.name, &args_str, &ctx.cwd, &ctx.sandbox)
                                .await
                        } else {
                            Ok(Vec::new())
                        };
                        match pre_res {
                            Err(block_err) => (redact_secrets(&format!("ERROR: {block_err}")), true),
                            Ok(_pre_notes) => {
                                let (mut out_txt, err_flag) = match registry.get(&call.name) {
                                    Some(tool) => match tool.run(call.args.clone(), ctx).await {
                                        Ok(out) => {
                                            let redacted = redact_secrets(&out);
                                            let shaped = shape_tool_output(
                                                &sid_for_archive,
                                                &call.id,
                                                &call.name,
                                                redacted,
                                                &ctx.tool_budget,
                                                &mut turn_tool_chars_used,
                                                scaled_turn_budget,
                                            );
                                            (shaped, false)
                                        }
                                        Err(e) => (redact_secrets(&format!("ERROR: {e}")), true),
                                    },
                                    None => (format!("ERROR: tool '{}' tidak dikenal", call.name), true),
                                };
                                if !err_flag {
                                    if matches!(
                                        call.name.as_str(),
                                        "write_file" | "edit_file" | "lsp_rename"
                                    ) && !out_txt.starts_with("Ditolak user")
                                    {
                                        let p = call.args.get("path").and_then(|v| v.as_str());
                                        dedup.invalidate_on_edit(p);
                                    } else {
                                        dedup.record_success(
                                            &call.name,
                                            &call.args,
                                            &out_txt,
                                            &ctx.cwd,
                                        );
                                    }
                                }
                                if !ctx.hooks.is_empty() {
                                    let post_notes = ctx
                                        .hooks
                                        .run_post_tool(
                                            &call.name,
                                            &args_str,
                                            err_flag,
                                            &ctx.cwd,
                                            &ctx.sandbox,
                                        )
                                        .await;
                                    if !post_notes.is_empty() {
                                        out_txt.push_str("\n\n");
                                        out_txt.push_str(&post_notes.join("\n"));
                                    }
                                }
                                (out_txt, err_flag)
                            }
                        }
                    }
                } else {
                    let args_str = call.args.to_string();
                    let pre_res = if !ctx.hooks.is_empty() {
                        ctx.hooks
                            .run_pre_tool(&call.name, &args_str, &ctx.cwd, &ctx.sandbox)
                            .await
                    } else {
                        Ok(Vec::new())
                    };
                    match pre_res {
                        Err(block_err) => (redact_secrets(&format!("ERROR: {block_err}")), true),
                        Ok(_pre_notes) => {
                            let (mut out_txt, err_flag) = match registry.get(&call.name) {
                                Some(tool) => match tool.run(call.args.clone(), ctx).await {
                                    Ok(out) => {
                                        let redacted = redact_secrets(&out);
                                        let shaped = shape_tool_output(
                                            &sid_for_archive,
                                            &call.id,
                                            &call.name,
                                            redacted,
                                            &ctx.tool_budget,
                                            &mut turn_tool_chars_used,
                                            scaled_turn_budget,
                                        );
                                        (shaped, false)
                                    }
                                    Err(e) => (redact_secrets(&format!("ERROR: {e}")), true),
                                },
                                None => (format!("ERROR: tool '{}' tidak dikenal", call.name), true),
                            };
                            if !ctx.hooks.is_empty() {
                                let post_notes = ctx
                                    .hooks
                                    .run_post_tool(
                                        &call.name,
                                        &args_str,
                                        err_flag,
                                        &ctx.cwd,
                                        &ctx.sandbox,
                                    )
                                    .await;
                                if !post_notes.is_empty() {
                                    out_txt.push_str("\n\n");
                                    out_txt.push_str(&post_notes.join("\n"));
                                }
                            }
                            (out_txt, err_flag)
                        }
                    }
                };
            let prev_test_count = corrector.edited_test_files.len();
            corrector.observe_tool_at_step(&call.name, &call.args, &content, is_error, step + 1);
            // Perbaikan 4 (v0.5.15): tandai langsung di transkrip sesi ketika berkas tes ikut diubah.
            if corrector.edited_test_files.len() > prev_test_count {
                if let Some(tpath) = corrector.edited_test_files.last() {
                    let tnote = format!("[berkas-tes] diubah: {tpath}");
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(tnote)).await;
                        }
                        None => eprintln!("{tnote}"),
                    }
                }
            }
            if let Some(warn) = loop_guard
                .record_with_result(&call.name, &call.args, &content, is_error)
                .hint_message()
            {
                content.push_str("\n\n");
                content.push_str(&warn);
                let note = format!("✦ Loop/stagnasi terdeteksi pada `{}` — meminta strategi baru…", call.name);
                match &ui {
                    Some(tx) => {
                        let _ = tx.send(UiEvent::Info(note)).await;
                    }
                    None => eprintln!("[selen] {note}"),
                }
            }
            // Rekam aksi teredaksi untuk ringkasan penutup (Perbaikan 2) & blok aksi (Perbaikan 3).
            turn_actions.push(ToolActionEvent {
                name: call.name.clone(),
                preview: preview_arg.clone(),
                is_error,
                error_reason: is_error.then(|| short_error_reason(&content)),
            });
            // BAGIAN 2 (#4) & Perbaikan 1 Lapis A (#3): hitung pencarian per giliran assistant (bukan per tool)
            // agar riset ter-batch (mis. 4 baca paralel dalam 1 giliran) tidak memicu false-positive anti-spin.
            if call.name == "ask_user" {
                step_asked_user = true;
            } else if matches!(
                call.name.as_str(),
                "search_text" | "list_files" | "run_shell" | "fetch_url"
            ) {
                step_had_search = true;
            }
            if let Some(tx) = &ui {
                let _ = tx
                    .send(UiEvent::ToolDone {
                        name: call.name.clone(),
                        summary: summarize(&content, 200),
                        is_error,
                    })
                    .await;
            }
            // update_plan (Fase 6): checklist ditampilkan ke user setiap
            // berhasil diperbarui — polos dicetak, TUI jadi blok khusus.
            tool_calls_executed += 1;
            if call.name == "skill_view" && !is_error {
                if let Some(sk) = call.args.get("name").and_then(|v| v.as_str()) {
                    loaded_skills_in_turn.insert(sk.trim().to_ascii_lowercase());
                }
            }
            if matches!(call.name.as_str(), "skill_create" | "skill_edit")
                && !is_error
                && !content.starts_with("Ditolak user")
            {
                skill_touched_in_turn = true;
            }
            if call.name == "manage_skill" && !is_error {
                let act = call
                    .args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if matches!(act, "create" | "patch") {
                    skill_touched_in_turn = true;
                    let short = content
                        .split(" (")
                        .next()
                        .unwrap_or(&content)
                        .trim_end_matches('.');
                    let note = format!("✦ Self-improvement review: {short}");
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(note)).await;
                        }
                        None => eprintln!("[selen] {note}"),
                    }
                }
            }
            if call.name == "update_plan" && !is_error {
                match &ui {
                    Some(tx) => {
                        let _ = tx.send(UiEvent::PlanUpdated(content.clone())).await;
                    }
                    None => println!("\n{content}"),
                }
            }
            results.push(Block::ToolResult {
                id: call.id.clone(),
                content,
                is_error,
            });
        }
        // Perbarui counter anti-berputar per giliran (bukan per tool paralel).
        if step_asked_user {
            unconfirmed_search_calls = 0;
            anti_spin_reminded = false;
        } else if step_had_search {
            unconfirmed_search_calls += 1;
            if unconfirmed_search_calls >= 5
                && !anti_spin_reminded
                && registry.get("ask_user").is_some()
            {
                anti_spin_reminded = true;
                if active_toolsets.insert("clarify".to_string()) {
                    tools = crate::tools::toolsets::active_tool_specs_for_turn(
                        registry,
                        &active_toolsets,
                        &loaded_mcp_tools,
                        mode,
                        lean_resume,
                    );
                }
                if let Some(Block::ToolResult { content, .. }) = results.last_mut() {
                    content.push_str(
                        "\n\n[GUARD ANTI-BERPUTAR] Kamu sudah melakukan 5 giliran pencarian/eksplorasi tanpa kandidat terkonfirmasi. \
                         DILARANG melanjutkan pencarian buta — berhenti sekarang dan panggil `ask_user` dengan kandidat terbaik yang sudah ditemukan sebagai pilihan (maks 4).",
                    );
                }
            }
        }
        // Perbaikan 1 #1, #2, #3 (v0.5.14 + v0.5.15):
        // - Mode berpagu (`has_step_cap`): checkpoint 1x pada rasio (bawaan 0.7) + konvergensi belum-ada-edit
        //   tepat di `compute_convergence_step(max_steps)` (sisa <= 5 ATAU >= 85%, mana yang tercapai lebih dulu).
        // - Mode tanpa pagu (`!has_step_cap`, bawaan Hermes): tidak ada checkpoint pagu, tetapi menyuntikkan
        //   SATU notice konvergensi keras saat langkah tanpa satu pun mutasi melewati ambang (bawaan 20) —
        //   tekanan saja, BUKAN penghentian.
        let used_steps = step + 1;
        if ctx.guard.enabled && ctx.guard.budget_warning_enabled {
            if has_step_cap {
                let remaining_steps = max_steps.saturating_sub(used_steps);
                if max_steps >= 3 && remaining_steps > 0 {
                    if !budget_checkpoint_injected && used_steps >= checkpoint_step {
                        budget_checkpoint_injected = true;
                        let cp_notice = format!(
                            "[PEMBERITAHUAN ANGGARAN LANGKAH — checkpoint] Kamu sudah memakai {used_steps} dari {max_steps} langkah (sisa {remaining_steps} langkah). \
                             Amankan kemajuan nyata sekarang lalu lanjutkan tugas hingga tuntas; jangan berhenti hanya karena pemberitahuan ini."
                        );
                        if let Some(Block::ToolResult { content, .. }) = results.last_mut() {
                            content.push_str("\n\n");
                            content.push_str(&cp_notice);
                        }
                        let log_line = format!(
                            "[anggaran-langkah] checkpoint: {used_steps}/{max_steps} langkah terpakai (sisa {remaining_steps})"
                        );
                        match &ui {
                            Some(tx) => {
                                let _ = tx.send(UiEvent::Info(log_line)).await;
                            }
                            None => eprintln!("{log_line}"),
                        }
                    }
                    if !convergence_notice_injected
                        && turn_requests_edit
                        && corrector.edited_files.is_empty()
                        && max_steps >= 4
                        && used_steps >= convergence_step
                    {
                        convergence_notice_injected = true;
                        let conv_notice = format!(
                            "[PERINTAH KONVERGENSI — BELUM ADA EDIT] Kamu sudah memakai {used_steps} dari {max_steps} langkah (sisa {remaining_steps} langkah) \
                             dan BELUM melakukan perubahan berkas apa pun. HENTIKAN eksplorasi sekarang! Langsung terapkan perbaikan terbaik yang sudah kamu identifikasi \
                             ke berkas target pada langkah berikutnya, atau nyatakan secara eksplisit bahwa masalah tidak dapat diselesaikan beserta alasannya."
                        );
                        if let Some(Block::ToolResult { content, .. }) = results.last_mut() {
                            content.push_str("\n\n");
                            content.push_str(&conv_notice);
                        }
                        let log_line = format!(
                            "[anggaran-langkah] konvergensi: {used_steps}/{max_steps} langkah tanpa edit — memaksa eksekusi perbaikan"
                        );
                        match &ui {
                            Some(tx) => {
                                let _ = tx.send(UiEvent::Info(log_line)).await;
                            }
                            None => eprintln!("{log_line}"),
                        }
                    }
                }
            } else if !convergence_notice_injected
                && turn_requests_edit
                && corrector.edited_files.is_empty()
                && used_steps >= convergence_step
            {
                convergence_notice_injected = true;
                let conv_notice = format!(
                    "[PERINTAH KONVERGENSI — BELUM ADA EDIT] Kamu sudah berjalan {used_steps} langkah dan BELUM melakukan perubahan berkas apa pun. \
                     HENTIKAN eksplorasi sekarang! Langsung terapkan perbaikan terbaik yang sudah kamu identifikasi ke berkas target, \
                     atau nyatakan secara eksplisit bahwa masalah tidak dapat diselesaikan beserta alasannya."
                );
                if let Some(Block::ToolResult { content, .. }) = results.last_mut() {
                    content.push_str("\n\n");
                    content.push_str(&conv_notice);
                }
                let log_line = format!(
                    "[anggaran-langkah] konvergensi: {used_steps} langkah tanpa edit — memaksa eksekusi perbaikan"
                );
                match &ui {
                    Some(tx) => {
                        let _ = tx.send(UiEvent::Info(log_line)).await;
                    }
                    None => eprintln!("{log_line}"),
                }
            }
        }
        let tool_msg = Message {
            role: Role::Tool,
            content: results,
        };
        persist_message(ctx, &tool_msg);
        messages.push(tool_msg);

        // Perbaikan 2 (v0.5.11) — Suntikan nol-pemborosan pasca-langkah-pertama:
        // Bila pada langkah 0 model hanya membaca/memeriksa berkas tanpa memanggil `skill_view`
        // pada tugas debugging/TDD, muat salinan kerja skill sekarang juga di harness sebelum
        // langkah 1 dikirim ke LLM (0 panggilan LLM terbuang, 100% tercatat sebelum edit pertama!).
        if step == 0 && !skill_gate_fired {
            if let Some(req_sk) = required_skill.as_deref() {
                if !loaded_skills_in_turn.contains(req_sk) {
                    if let Some(sv_tool) = registry.get("skill_view") {
                        skill_gate_fired = true;
                        let gate_call_id = "gate_skill_step0".to_string();
                        let gate_args = serde_json::json!({ "name": req_sk });
                        let preview_arg = prepare_tool_preview(
                            "skill_view",
                            &gate_args,
                            ctx.tool_preview_length,
                        );
                        match &ui {
                            Some(tx) => {
                                let _ = tx
                                    .send(UiEvent::ToolStart {
                                        name: "skill_view".into(),
                                        args: preview_arg.clone(),
                                    })
                                    .await;
                            }
                            None => eprintln!(
                                "[tool] {}",
                                format_tool_event_from_preview("skill_view", &preview_arg)
                            ),
                        }
                        let (mut sv_out, sv_err) = match sv_tool.run(gate_args.clone(), ctx).await {
                            Ok(out) => (redact_secrets(&out), false),
                            Err(e) => (redact_secrets(&format!("ERROR: {e}")), true),
                        };
                        if !sv_err {
                            loaded_skills_in_turn.insert(req_sk.to_string());
                            sv_out.push_str(
                                "\n\n[Gerbang Skill Harness] Skill dimuat otomatis sebelum langkah edit. \
                                 Ikuti prosedur investigasi/verifikasi di atas.",
                            );
                        }
                        let gate_asst = Message {
                            role: Role::Assistant,
                            content: vec![Block::ToolUse {
                                id: gate_call_id.clone(),
                                name: "skill_view".into(),
                                args: gate_args,
                            }],
                        };
                        persist_message(ctx, &gate_asst);
                        messages.push(gate_asst);
                        let gate_tool = Message {
                            role: Role::Tool,
                            content: vec![Block::ToolResult {
                                id: gate_call_id,
                                content: sv_out.clone(),
                                is_error: sv_err,
                            }],
                        };
                        persist_message(ctx, &gate_tool);
                        messages.push(gate_tool);
                        turn_actions.push(ToolActionEvent {
                            name: "skill_view".into(),
                            preview: preview_arg,
                            is_error: sv_err,
                            error_reason: sv_err.then(|| short_error_reason(&sv_out)),
                        });
                        tool_calls_executed += 1;
                        if let Some(tx) = &ui {
                            let _ = tx
                                .send(UiEvent::ToolDone {
                                    name: "skill_view".into(),
                                    summary: summarize(&sv_out, 200),
                                    is_error: sv_err,
                                })
                                .await;
                        }
                    }
                }
            }
        }

        // Tahap 2: deteksi stagnasi sebagai pemicu berhenti (bukan sekadar log) bila guard aktif.
        if ctx.guard.enabled && loop_guard.is_stagnated() {
            let stop_note = "Stagnasi beruntun terdeteksi — giliran dihentikan agar tidak memboroskan token.";
            match &ui {
                Some(tx) => {
                    let _ = tx.send(UiEvent::Info(stop_note.into())).await;
                }
                None => eprintln!("[selen] {stop_note}"),
            }
            forced_stop_reason = Some("stagnasi beruntun".to_string());
            break;
        }

        if has_step_cap && step + 1 == max_steps {
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::Info(format!(
                            "max_steps ({max_steps}) tercapai — menyusun ringkasan penutup…"
                        )))
                        .await;
                }
                None => eprintln!("[selen] max_steps ({max_steps}) tercapai — menyusun ringkasan penutup…"),
            }
            forced_stop_reason = Some(format!("batas langkah ({max_steps}) tercapai"));
            break;
        }
    }

    // Perbaikan 2 (v0.5.8) + Perbaikan 1 #3 (v0.5.14) + Perbaikan 2/3/4 (v0.5.15):
    // Giliran terpotong memakai struktur wajib opencode `MAX_STEPS_PROMPT` + pelabelan jujur Hermes.
    // Kalimat kegagalan nol-edit ditegakkan PASCA-GENERASI secara programatis — tidak bergantung kepatuhan model!
    if let Some(stop_reason) = forced_stop_reason {
        let zero_edit_failure = turn_requests_edit && corrector.edited_files.is_empty();
        let mut summary_msgs = messages.clone();
        summary_msgs.push(Message::text(
            Role::User,
            format!(
                "[BATAS LANGKAH TERCAPAI — {stop_reason}] Tool dimatikan hingga input pengguna berikutnya. \
                 Jawab dengan teks saja (DILARANG memanggil tool). Jawaban WAJIB memuat: \
                 (1) pernyataan eksplisit bahwa batas langkah tercapai, \
                 (2) ringkasan yang sudah dikerjakan, \
                 (3) daftar yang belum selesai, \
                 (4) rekomendasi langkah berikutnya."
            ),
        ));
        let (stx, _srx) = mpsc::channel::<StreamEvent>(64);
        let p_close = provider.clone();
        let closing_res = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            p_close.stream_chat(&summary_msgs, &[], stx),
        )
        .await;
        let raw_closing = match closing_res {
            Ok(Ok(rep)) if !rep.text.trim().is_empty() => {
                if let Some(u) = rep.usage {
                    persist_usage_kind(ctx, provider.model_name(), Some(u), "main");
                    turn_usage.input_tokens += u.input_tokens;
                    turn_usage.output_tokens += u.output_tokens;
                    have_usage = true;
                }
                rep.text.trim().to_string()
            }
            _ => build_template_closing_summary_with_status(
                &turn_actions,
                &stop_reason,
                zero_edit_failure,
            ),
        };
        let mut closing_text = enforce_zero_edit_failure_header(&raw_closing, zero_edit_failure);
        if let Some(ver_line) = corrector.closing_verification_summary_line(turn_requests_edit) {
            if !closing_text.contains(&ver_line) {
                closing_text.push_str("\n\n");
                closing_text.push_str(&ver_line);
            }
        }
        if let Some(test_line) = corrector.closing_test_files_note() {
            if !closing_text.contains(&test_line) {
                closing_text.push_str("\n");
                closing_text.push_str(&test_line);
            }
        }
        let closing_msg = Message::text(Role::Assistant, closing_text.clone());
        persist_message(ctx, &closing_msg);
        messages.push(closing_msg);
        match &ui {
            Some(tx) => {
                let _ = tx.send(UiEvent::Token(closing_text.clone())).await;
            }
            None => println!("{closing_text}"),
        }
        last_text = closing_text;
    }
    if let Some(cp_id) = turn_checkpoint_id {
        if corrector.edited_files.is_empty() {
            if let Ok(mut st) = ctx.undo_stack.lock() {
                st.discard_empty_checkpoint(cp_id);
            }
        }
    }
    if let Some(tx) = &ui {
        let _ = tx
            .send(UiEvent::TurnDone {
                usage: have_usage.then_some(turn_usage),
            })
            .await;
    }
    Ok((last_text, have_usage.then_some(turn_usage)))
}

/// Persentase pemakaian konteks (0..=100), dibulatkan ke atas agar pemakaian
/// kecil yang nyata (>0 token) tidak tampil sebagai 0%.
fn calc_context_pct(tokens: usize, limit: usize) -> u8 {
    if limit == 0 || tokens == 0 {
        return 0;
    }
    ((tokens.saturating_mul(100) + limit - 1) / limit).min(100) as u8
}

/// Ringkasan 1 baris: ambil baris pertama non-kosong, potong di batas char.
fn summarize(s: &str, max: usize) -> String {
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max {
        out.push('…');
    }
    out
}

/// Tahap 4 — Terapkan anggaran hasil tool:
/// 1. Khusus `run_shell`: bila melebihi `shell_cap_chars` (default 12.000) atau `spillover_chars` (50.000),
///    SELALU arsipkan output utuh ke file spillover lebih dulu agar tidak ada baris yang hilang, lalu
///    kembalikan pratinjau 40% kepala / 60% ekor + path arsipnya.
/// 2. Tool lain: spillover ke file bila hasil tunggal > `spillover_chars` (default 50.000 char).
/// 3. Budget agregat per turn: bila `turn_tool_chars_used` akan melampaui `turn_budget`, spill hasil besar duluan.
/// 4. Tool lain <= batas: potong aman di batas 16KB.
fn shape_tool_output(
    session_id: &str,
    call_id: &str,
    tool_name: &str,
    redacted: String,
    budget: &crate::config::ToolBudgetCfg,
    turn_tool_chars_used: &mut usize,
    turn_budget: usize,
) -> String {
    let char_len = redacted.chars().count();
    if tool_name == "run_shell" {
        let cap = budget.shell_cap_chars.max(1_000);
        if char_len > budget.spillover_chars && budget.spillover_chars > 0 {
            if let Some(spilled) = spillover_with_preview(
                session_id,
                call_id,
                tool_name,
                &redacted,
                budget.spillover_chars,
                2_400,
            ) {
                *turn_tool_chars_used += spilled.chars().count();
                return spilled;
            }
        }
        if char_len > cap {
            if let Some(spilled) =
                spillover_with_preview(session_id, call_id, tool_name, &redacted, cap, cap)
            {
                *turn_tool_chars_used += spilled.chars().count();
                return spilled;
            }
            let cut = head_tail_truncate(&redacted, cap);
            *turn_tool_chars_used += cut.chars().count();
            return cut;
        }
    }
    if let Some(spilled) = spillover_if_needed(
        session_id,
        call_id,
        tool_name,
        &redacted,
        budget.spillover_chars,
    ) {
        *turn_tool_chars_used += spilled.chars().count();
        return spilled;
    }
    if *turn_tool_chars_used + char_len > turn_budget && char_len > 2_000 {
        if let Some(spilled) = spillover_if_needed(session_id, call_id, tool_name, &redacted, 1) {
            *turn_tool_chars_used += spilled.chars().count();
            return spilled;
        }
    }
    let shaped = if tool_name == "run_shell" {
        head_tail_truncate(&redacted, budget.shell_cap_chars.max(1_000))
    } else {
        truncate(redacted)
    };
    *turn_tool_chars_used += shaped.chars().count();
    shaped
}

/// Potong hasil tool agar konteks tidak meledak (16KB, aman UTF-8).
fn truncate(s: String) -> String {
    const MAX: usize = 16 * 1024;
    if s.len() <= MAX {
        s
    } else {
        let mut end = MAX;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut cut = s;
        cut.truncate(end);
        cut.push_str("\n... (hasil dipotong 16KB)");
        cut
    }
}

/// Ekstrak lampiran gambar multimodal `@path/ke/gambar.(png|jpg|jpeg|webp|gif)` dari prompt user.
/// Maksimal 4 gambar & 6 MB per gambar; menolak file rahasia.
fn extract_at_images(cwd: &std::path::Path, prompt: &str) -> (Vec<Block>, Vec<String>) {
    const MAX_IMAGES: usize = 4;
    const MAX_IMAGE_BYTES: u64 = 6 * 1024 * 1024;
    let mut blocks = Vec::new();
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for raw_tok in prompt.split_whitespace() {
        if blocks.len() >= MAX_IMAGES {
            break;
        }
        let tok = raw_tok.trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '"' | '\'' | '`'));
        let Some(rel) = tok.strip_prefix('@') else {
            continue;
        };
        if rel.is_empty() || !seen.insert(rel.to_string()) {
            continue;
        }
        let cand = std::path::Path::new(rel);
        let Some(mime) = crate::providers::detect_image_mime(cand) else {
            continue;
        };
        let resolved = if cand.is_absolute() {
            cand.to_path_buf()
        } else {
            match crate::security::resolve_in_sandbox(cwd, cand) {
                Ok(p) => p,
                Err(_) => continue,
            }
        };
        if crate::security::is_secret_file(&resolved) || !resolved.is_file() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&resolved) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&resolved) {
            blocks.push(Block::Image {
                media_type: mime.to_string(),
                data_base64: crate::providers::encode_base64(&bytes),
            });
            names.push(rel.to_string());
        }
    }
    (blocks, names)
}
