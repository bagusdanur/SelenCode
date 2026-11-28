//! Selena Improvement — review belajar pasca-tugas untuk SelenCode (v0.5.24):
//! - Berjalan di latar (non-blocking) sesudah penutup terkirim, pada model utama sesi secara bawaan
//!   (atau override `[aux] review_model` / `compression_model` bila terisi), dengan awalan prompt beku
//!   sadar cache (`FROZEN_REVIEW_SYSTEM_PROMPT`) dan pagu keras <= 5.000 token/panggilan.
//! - Skill BARU karya review lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md` dengan
//!   penanda frontmatter `origin: selena-improvement`, terindeks lintas sesi dengan presedensi lebih
//!   rendah dari skill pengguna.
//! - Hanya skill berpenanda `origin: selena-improvement` yang boleh ditambal langsung (wajib
//!   read-before-write + cadangan putar balik di `review_backups`). Setiap perubahan atas skill karya
//!   pengguna (tambal, timpa, hapus) selalu ditahan ke staging `review_proposals` (`/review approve`).
//! - Format laporan tunggal resmi: `💾 Selena improvement: ...` (diam bila nol perubahan).

use crate::agent::context::{analyze_token_breakdown, prune_old_tool_results};
use crate::config::Config;
use crate::providers::{Block, Message, Provider, Role, StreamEvent, ToolSpec, Usage};
use crate::security::{is_secret_file, redact_secrets};
use crate::skills::{
    format_skills_index, improvement_skill_file_path, list_skills, parse_skill_frontmatter,
    remove_skill, render_skill_markdown, render_skill_markdown_with_origin, sanitize_skill_name,
    scan_skills_with_warnings, skill_file_path_for, user_skills_dir, IMPROVEMENT_SKILL_ORIGIN,
};
use crate::store::{normalize_memory_category, ReviewProposal, Store};
use crate::tools::skill::ManageSkill;
use crate::tools::{Ctx, Tool};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Jumlah minimum panggilan tool untuk helper review manual lama (kompatibilitas mundur tes).
pub const AUTO_REVIEW_MIN_TOOLS: usize = 6;
/// Pagu keras maksimum token input per panggilan review agar selalu <= 5.000 token/panggilan.
pub const MAX_REVIEW_CALL_INPUT_TOKENS: u64 = 5_000;

const MAX_DIGEST_BYTES: usize = 6 * 1024;
const MAX_REF_FILE_BYTES: usize = 4 * 1024;

/// Awalan system prompt BEKU (byte-identik lintas seluruh review & sesi) untuk prefix cache provider
/// (>= 1.024 token untuk OpenAI/Anthropic/Gemini langsung, sekaligus ramping ~1.150 token agar
/// total panggilan termasuk overhead gateway `ag/*` ~2.086 token tetap ketat <= 5.000 token/panggilan).
pub const FROZEN_REVIEW_SYSTEM_PROMPT: &str = "\
Kamu adalah modul Selena Improvement milik SelenCode (review belajar pasca-tugas di latar).\n\
Tugasmu: tinjau transkrip sesi, indeks & isi skill relevan yang baru dibaca segar, serta catatan memory, lalu keluarkan HANYA satu objek JSON valid tanpa teks lain:\n\
{\"actions\": [...]}\n\
\n\
SINYAL WAJIB TINDAK:\n\
1. Koreksi eksplisit pengguna atas path, flag, format, atau alur kerja (sinyal kelas-satu).\n\
2. Teknik/workaround non-trivial yang terbukti berhasil sesudah kegagalan terpulihkan.\n\
3. Skill termuat ternyata salah, bertentangan, atau usang dibanding kenyataan sesi.\n\
4. Kelas teknik baru yang berulang lintas-sesi walau belum ada skill relevan.\n\
\n\
URUTAN PREFERENSI (PERHATIKAN RELEVANSI TOPIK):\n\
(1) `patch_skill` pada skill termuat — HANYA JIKA skill itu RELEVAN secara topik. DILARANG memaksakan topik lain ke skill yang kebetulan terbuka.\n\
(2) `patch_skill` pada skill payung lain — HANYA JIKA relevan secara topik.\n\
(3) `add_skill_reference` (`references/`/`templates/`/`scripts/`) untuk rincian pendukung pada skill relevan.\n\
(4) `create_skill` — PILIHAN WAJIB DAN TEPAT bila pelajaran membentuk kelas teknik BARU lintas-sesi dan TIDAK ADA skill relevan secara topik di katalog (meskipun ada skill lain seperti `systematic-debugging`).\n\
(5) `add_memory` — hanya fakta/preferensi sempit 1 baris (bukan prosedur multi-langkah).\n\
\n\
ATURAN LAPISAN PELAJARAN & ANTI-KONTRADIKSI:\n\
- Tulis aturan imperatif tergeneralisasi + 1 klausa sebab (BUKAN narasi kejadian, tanpa nomor PR/tiket/tanggal).\n\
- ATURAN KERAS KOREKSI: jika skill punya baris/langkah keliru, WAJIB ganti DI TEMPATNYA dengan `old_snippet` (salin persis dari SKILL.md yang dibaca) dan `new_snippet` pengganti bersih. DILARANG membiarkan aturan keliru utuh lalu menempel bantahan/'UPDATE:'/'Catatan:' di bawahnya!\n\
- Pakai `append_section` HANYA untuk seksi baru yang tidak bertentangan dengan isi lama.\n\
\n\
JANGAN TANGKAP (kembalikan {\"actions\":[]}):\n\
- Gagal lingkungan sementara (binary/kredensial/paket hilang), klaim negatif tentang tool ('tool X rusak'), error sesaat tanpa pola ulang, narasi sekali jalan, kegagalan tak terselesaikan, rahasia/API key, atau sesi bersih tanpa pelajaran.\n\
\n\
CONTOH TERAPAN:\n\
- Contoh A (Koreksi di tempatnya — WAJIB vs DITOLAK):\n\
  Skill punya: `2. Kirim ke /tmp/staging.` Koreksi user: pakai `/var/lib/releases` karena `/tmp` hilang saat reboot.\n\
  SALAH (ditolak): `{\"actions\":[{\"action\":\"patch_skill\",\"skill\":\"deploy\",\"append_section\":\"Catatan: pakai /var/lib/releases.\",\"reason\":\"koreksi\"}]}`\n\
  BENAR: `{\"actions\":[{\"action\":\"patch_skill\",\"skill\":\"deploy\",\"old_snippet\":\"2. Kirim ke /tmp/staging.\",\"new_snippet\":\"2. Kirim ke /var/lib/releases karena direktori temporer hilang saat reboot.\",\"reason\":\"ganti path deploy di tempatnya\"}]}`\n\
- Contoh B (Teknik kelas-baru walau ada skill lain tak relevan -> `create_skill`):\n\
  `{\"actions\":[{\"action\":\"create_skill\",\"skill\":\"sqlite-safe-migration\",\"description\":\"Migrasi skema SQLite aman tanpa lock\",\"body\":\"# SQLite Safe Migration\\n\\n1. Setel PRAGMA busy_timeout=5000 sebelum DDL karena koneksi paralel langsung gagal bila timeout 0.\",\"tags\":[\"sqlite\",\"migration\"],\"reason\":\"prosedur migrasi SQLite lintas sesi\"}]}`\n\
\n\
SKEMA JSON:\n\
1. {\"action\":\"patch_skill\",\"skill\":\"<kebab>\",\"old_snippet\":\"<teks lama persis>\",\"new_snippet\":\"<teks baru>\",\"append_section\":\"<opsional non-koreksi>\",\"reason\":\"<=60 char\"}\n\
2. {\"action\":\"create_skill\",\"skill\":\"<kebab>\",\"description\":\"<=80 char\",\"body\":\"<markdown>\",\"tags\":[\"t\"],\"reason\":\"<=60 char\"}\n\
3. {\"action\":\"add_skill_reference\",\"skill\":\"<kebab>\",\"ref_path\":\"references/<x>.md\",\"content\":\"<md>\",\"reason\":\"<=60 char\"}\n\
4. {\"action\":\"add_memory\",\"category\":\"arch|commands|conventions|decisions|issues|tasks|general\",\"scope\":\"project|global\",\"content\":\"<fakta>\",\"reason\":\"<=60 char\"}\n\
5. {\"action\":\"delete_memory\",\"memory_id\":1,\"reason\":\"...\"}\n\
6. {\"action\":\"overwrite_memory\",\"memory_id\":1,\"category\":\"...\",\"new_content\":\"...\",\"reason\":\"...\"}\n\
7. {\"action\":\"delete_skill\",\"skill\":\"<kebab>\",\"reason\":\"...\"}\n\
8. {\"action\":\"overwrite_skill\",\"skill\":\"<kebab>\",\"body\":\"<md>\",\"reason\":\"...\"}\n\
Dilarang sentuh berkas proyek. Bila tak ada pelajaran konkret -> {\"actions\":[]}";

// ---------- Legacy manual helper (`/skills review` & tes kompatibilitas mundur) ----------

fn build_turn_digest(messages: &[Message]) -> String {
    let mut out = String::new();
    let tail_start = messages.len().saturating_sub(10);
    for m in &messages[tail_start..] {
        if m.role == Role::System {
            continue;
        }
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "SELENA",
            Role::Tool => "TOOL",
            Role::System => continue,
        };
        for b in &m.content {
            let line = match b {
                Block::Text(t) => {
                    let s: String = t.chars().take(400).collect();
                    format!("[{role}] {s}\n")
                }
                Block::Image { media_type, .. } => {
                    format!("[{role}] [image: {media_type}]\n")
                }
                Block::ToolUse { name, args, .. } => {
                    let a: String = args.to_string().chars().take(200).collect();
                    format!("[CALL] {name} {a}\n")
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let first = content.lines().next().unwrap_or("");
                    let s: String = first.chars().take(180).collect();
                    let tag = if *is_error { "ERR" } else { "OK" };
                    format!("[RESULT {tag}] {s}\n")
                }
            };
            if out.len() + line.len() > MAX_DIGEST_BYTES {
                break;
            }
            out.push_str(&line);
        }
    }
    out
}

/// Jalankan 1 kali review Selena improvement manual terhadap riwayat `messages` (`/skills review`).
/// Mengembalikan `Ok(Some("💾 Selena improvement: Skill '<nama>' created|patched"))`
/// bila ada skill yang dibuat/diperbarui, atau `Ok(None)` bila tidak perlu.
pub async fn run_self_improvement_review(
    provider: Arc<dyn Provider>,
    ctx: &Ctx,
    messages: &[Message],
) -> Result<Option<String>> {
    let digest = build_turn_digest(messages);
    if digest.trim().is_empty() {
        return Ok(None);
    }

    let existing = list_skills(&ctx.cwd);
    let catalog = if existing.is_empty() {
        "(belum ada skill)".to_string()
    } else {
        existing
            .iter()
            .take(20)
            .map(|s| format!("- {}: {}", s.name, s.summary))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let sys = "Kamu adalah modul Selena Improvement milik SelenCode.\n\
Tugasmu: tinjau singkat ringkasan pekerjaan yang baru saja selesai.\n\
Aturan KETAT:\n\
1. Jika pekerjaan barusan mengungkap prosedur reusable, workaround penting, pola scraper/API/build/deploy spesifik project, atau memperbaiki kekurangan pada skill yang sudah ada -> panggil tool `manage_skill` dengan `action: \"create\"` (skill baru, nama kebab-case deskriptif) atau `action: \"patch\"` (perbarui skill lama di katalog).\n\
2. Isi skill harus konkret, ringkas, berisi langkah nyata, jebakan yang harus dihindari, dan TANPA secret/API key.\n\
3. Jika percakapan hanya tanya-jawab biasa, edit trivial 1 baris, atau belum ada pola penting -> JANGAN panggil tool apa pun, cukup jawab persis: NONE";

    let user = format!(
        "Katalog skill saat ini:\n{catalog}\n\nRingkasan giliran terakhir:\n{digest}\n\nEvaluasi apakah perlu create/patch skill lewat `manage_skill`, atau jawab NONE."
    );

    let review_msgs = vec![
        Message::text(Role::System, sys),
        Message::text(Role::User, user),
    ];
    let skill_tool = ManageSkill;
    let specs = vec![skill_tool.spec()];
    let breakdown = analyze_token_breakdown(&review_msgs, &specs, 1);
    let (tx, _rx) = mpsc::channel::<StreamEvent>(64);
    let reply = provider.stream_chat(&review_msgs, &specs, tx).await?;

    if let (Some(store), Some(sid)) = (&ctx.store, &ctx.session_id) {
        let (act_in, act_out) = reply
            .usage
            .map(|u| (u.input_tokens, u.output_tokens))
            .unwrap_or((0, 0));
        if let Some(u) = reply.usage {
            let _ = store.record_usage(sid, provider.model_name(), u.input_tokens, u.output_tokens);
        }
        let _ = store.record_call_token_metric(&crate::store::CallTokenMetric {
            session_id: sid.clone(),
            model: provider.model_name().to_string(),
            est_system_tokens: breakdown.system_tokens,
            est_tools_tokens: breakdown.tools_tokens,
            est_history_tokens: breakdown.conversation_tokens,
            est_tool_results_tokens: breakdown.tool_results_tokens,
            actual_input_tokens: act_in,
            actual_output_tokens: act_out,
            active_tools_count: breakdown.active_tools,
            pruned_count: 0,
            deduped_count: 0,
        });
    }

    let mut events: Vec<String> = Vec::new();
    for call in &reply.tool_calls {
        if call.name != "manage_skill" {
            continue;
        }
        let action = call
            .args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !matches!(action, "create" | "patch") {
            continue;
        }
        if let Ok(out) = skill_tool.run(call.args.clone(), ctx).await {
            let short = out
                .split(" (")
                .next()
                .unwrap_or(&out)
                .trim_end_matches('.');
            events.push(short.to_string());
        }
    }

    if events.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "💾 Selena improvement: {}",
            events.join(" · ")
        )))
    }
}

// ---------- Selena Improvement Engine (v0.5.24) ----------

/// Alasan mengapa Selena improvement dilewati sebelum memanggil provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSkipReason {
    Disabled,
    NoAuxModelConfigured,
    CooldownActive,
    TrivialSession,
    BudgetExceeded,
    /// Snapshot `(session_id, messages.len())` sudah pernah ditinjau tuntas (anti-tembak ganda, Masalah 1 v0.5.25).
    AlreadyReviewed,
    /// Bahkan digest minimal sesudah pemangkasan tetap melampaui pagu total request <= 5.000 token (Masalah 3 v0.5.25).
    DigestOverBudget,
}

impl ReviewSkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoAuxModelConfigured => "no_model_configured",
            Self::CooldownActive => "cooldown_active",
            Self::TrivialSession => "trivial_session",
            Self::BudgetExceeded => "budget_exceeded",
            Self::AlreadyReviewed => "already_reviewed",
            Self::DigestOverBudget => "digest_over_budget",
        }
    }
}

/// Satu perubahan aman yang langsung diterapkan oleh Selena improvement.
#[derive(Debug, Clone)]
pub struct AppliedReviewChange {
    pub kind: String,
    pub target: String,
    pub reason: String,
    pub diff: Option<String>,
    pub origin: String,
    pub backup_id: Option<i64>,
    /// Cakupan store target perubahan (`"user"`, `"project"`, atau `"memory"`).
    pub scope: String,
}

/// Hasil satu kali jalan Selena improvement.
#[derive(Debug, Clone)]
pub struct BgReviewOutcome {
    pub applied: Vec<AppliedReviewChange>,
    pub staged_ids: Vec<i64>,
    /// Tepat satu pesan ringkas bergaya `💾 Selena improvement: ...` bila ada hasil,
    /// atau `None` bila tidak ada temuan (anti-spam).
    pub report_message: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Jumlah panggilan provider individual dalam peristiwa review ini (v0.5.27 Masalah 2).
    pub calls_count: usize,
    /// Token input terbesar dari satu panggilan individual dalam peristiwa ini (<= 5.000).
    pub max_call_input_tokens: u64,
    pub model: String,
    pub run_id: Option<i64>,
    pub backup_ids: Vec<i64>,
}

/// Hitung total panggilan tool (`Block::ToolUse`) pada riwayat sesi.
pub fn count_tool_calls_in_messages(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, Block::ToolUse { .. }))
        .count()
}

/// Hitung jumlah giliran pengguna tuntas dalam sesi.
pub fn count_user_turns_in_messages(messages: &[Message]) -> usize {
    messages.iter().filter(|m| m.role == Role::User).count()
}

/// Deteksi adanya koreksi atau penegasan prosedur dari pengguna di tengah sesi.
pub fn has_user_correction(messages: &[Message]) -> bool {
    const CORRECTION_CUES: &[&str] = &[
        "salah",
        "keliru",
        "bukan di",
        "bukan itu",
        "bukan begitu",
        "bukan pakai",
        "harusnya",
        "seharusnya",
        "yang benar",
        "path yang benar",
        "folder yang benar",
        "koreksi",
        "jangan pakai",
        "jangan gunakan",
        "selalu pakai",
        "selalu gunakan",
        "mulai sekarang",
        "sudah usang",
        "tidak dipakai lagi",
        "sudah tidak dipakai",
        "sudah pindah",
        "sudah diganti",
        "ganti jadi",
        "wrong",
        "not there",
        "instead of",
        "actually use",
        "correct path",
        "should be",
        "don't use",
        "do not use",
        "always use",
        "never use",
        "outdated",
        "obsolete",
        "no longer used",
    ];

    let user_texts: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    Block::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
        })
        .collect();

    for (idx, text) in user_texts.iter().enumerate() {
        let matches_cue = CORRECTION_CUES.iter().any(|cue| text.contains(cue));
        // Koreksi di giliran ke-2 dst. selalu dihitung; di giliran pertama hanya bila jelas
        // memuat penegasan koreksi / catatan usang / aturan permanen.
        if idx > 0 && matches_cue {
            return true;
        }
        if idx == 0
            && (text.contains("sudah usang")
                || text.contains("tidak dipakai lagi")
                || text.contains("outdated")
                || text.contains("obsolete")
                || text.contains("koreksi")
                || text.contains("yang benar")
                || text.contains("mulai sekarang")
                || text.contains("selalu pakai")
                || text.contains("jangan pakai"))
        {
            return true;
        }
    }
    false
}

/// Deteksi apakah sesi pernah mengalami kegagalan tool/shell yang kemudian dipulihkan
/// (ada hasil error diikuti hasil sukses di bagian berikutnya dari sesi).
pub fn has_recovered_failure(messages: &[Message]) -> bool {
    let mut saw_error = false;
    for m in messages {
        if m.role != Role::Tool {
            continue;
        }
        for b in &m.content {
            if let Block::ToolResult {
                content, is_error, ..
            } = b
            {
                let nonzero_exit = content
                    .lines()
                    .next()
                    .and_then(|l| l.trim().strip_prefix("exit code:"))
                    .and_then(|rest| rest.trim().parse::<i32>().ok())
                    .map(|c| c != 0)
                    .unwrap_or(false);
                if *is_error || nonzero_exit {
                    saw_error = true;
                } else if saw_error {
                    return true;
                }
            }
        }
    }
    false
}

/// Deteksi apakah sesi memakai atau mengubah skill (`skill_view`, `skill_create`, `skill_edit`, `manage_skill`).
pub fn has_skill_activity(messages: &[Message]) -> bool {
    messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.content.iter())
        .any(|b| match b {
            Block::ToolUse { name, .. } => matches!(
                name.as_str(),
                "skill_view" | "skill_create" | "skill_edit" | "manage_skill"
            ),
            _ => false,
        })
}

/// Sinyal bernilai tinggi yang pantas memicu Selena improvement segera sesudah giliran selesai:
/// koreksi pengguna, kegagalan yang dipulihkan di dalam sesi, atau pemakaian/pembuatan skill
/// pada tugas dengan kerja nyata.
pub fn has_valuable_review_signal(messages: &[Message], min_tool_calls: usize) -> bool {
    if has_user_correction(messages) {
        return true;
    }
    if has_recovered_failure(messages) {
        return true;
    }
    if has_skill_activity(messages) && count_tool_calls_in_messages(messages) >= 2 {
        return true;
    }
    // Tugas besar (>= max(10, min_tool_calls * 2) panggilan tool) juga layak ditinjau.
    count_tool_calls_in_messages(messages) >= min_tool_calls.saturating_mul(2).max(10)
}

/// Apakah sesi mengandung kerja substantif yang layak ditinjau Selena improvement:
/// minimal `min_tool_calls` panggilan tool, ATAU ada koreksi pengguna, ATAU ada kegagalan terpulihkan,
/// ATAU ada pemakaian skill dengan >= 2 panggilan tool.
pub fn is_session_substantive(messages: &[Message], min_tool_calls: usize) -> bool {
    let min_tc = min_tool_calls.max(1);
    let tc = count_tool_calls_in_messages(messages);
    has_user_correction(messages)
        || has_recovered_failure(messages)
        || (has_skill_activity(messages) && tc >= 2)
        || tc >= min_tc
}

/// Tentukan model efektif Selena improvement:
/// 1. `[aux] review_model` -> `[aux] compression_model` (override opsional),
/// 2. Fallback ke model utama sesi di `cfg` (default hidup tanpa syarat `[aux]`, v0.5.24).
pub fn resolve_effective_review_model(cfg: &Config) -> Option<String> {
    if let Some(aux_m) = cfg.aux.effective_review_model() {
        return Some(aux_m.to_string());
    }
    if let Ok((_, pcfg, _)) = cfg.resolve_provider(None) {
        let m = pcfg.model.trim();
        if !m.is_empty() && m != "(belum diatur)" {
            return Some(m.to_string());
        }
    }
    if !cfg.default_provider.trim().is_empty() {
        if let Some(p) = cfg.providers.get(cfg.default_provider.trim()) {
            let m = p.model.trim();
            if !m.is_empty() && m != "(belum diatur)" {
                return Some(m.to_string());
            }
        }
    }
    cfg.providers
        .values()
        .map(|p| p.model.trim())
        .find(|m| !m.is_empty() && *m != "(belum diatur)")
        .map(|m| m.to_string())
}

/// Deteksi sinyal belajar kelas-satu (koreksi eksplisit pengguna atau kegagalan terpulihkan)
/// pada potongan pesan mulai dari `start_idx` (Masalah 2 v0.5.25).
pub fn detect_first_class_signal_in_slice(
    messages: &[Message],
    start_idx: usize,
) -> Option<&'static str> {
    let idx = start_idx.min(messages.len());
    let slice = &messages[idx..];
    if slice.is_empty() {
        return None;
    }
    if has_user_correction(slice) {
        return Some("user_correction");
    }
    if has_recovered_failure(slice) {
        return Some("recovered_failure");
    }
    None
}

/// Catat sinyal belajar kelas-satu secara persisten pada `store` apabila muncul sejak review terakhir
/// yang selesai DAN sesi sedang berada dalam cooldown, sehingga cooldown hanya menunda eksekusi dan
/// tidak pernah membuang sinyal (Masalah 2 v0.5.25).
pub fn record_pending_signal_if_present(
    store: &Store,
    session_id: &str,
    messages: &[Message],
) {
    let since_idx = store
        .last_reviewed_msg_count(session_id)
        .unwrap_or(0)
        .min(messages.len());
    if let Some(kind) = detect_first_class_signal_in_slice(messages, since_idx) {
        let _ = store.record_pending_review_signal(session_id, kind, since_idx);
    }
}

/// Cek semua prasyarat pemicu Selena improvement sebelum memanggil provider.
/// Setiap alasan lewati dicatat ke `store` (`last_review_skip`) agar terdiagnosis di `/doctor` & `/review status`.
/// Masalah 1 & 2 (v0.5.25): menolak snapshot yang sudah tercakup (`AlreadyReviewed`) dan selalu
/// menyimpan sinyal kelas-satu secara persisten ketika tertahan oleh cooldown.
pub fn check_review_eligibility(
    cfg: &Config,
    store: &Store,
    session_id: &str,
    messages: &[Message],
) -> std::result::Result<String, ReviewSkipReason> {
    if !cfg.review.enabled {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::Disabled.as_str(),
            "[review] enabled = false",
        );
        return Err(ReviewSkipReason::Disabled);
    }
    let Some(model) = resolve_effective_review_model(cfg) else {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::NoAuxModelConfigured.as_str(),
            "belum ada provider utama maupun [aux] review_model",
        );
        return Err(ReviewSkipReason::NoAuxModelConfigured);
    };
    let last_covered = store.last_reviewed_msg_count(session_id).unwrap_or(0);
    if !messages.is_empty() && last_covered >= messages.len() {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::AlreadyReviewed.as_str(),
            &format!(
                "snapshot sesi ({} pesan) sudah tercakup review sebelumnya",
                messages.len()
            ),
        );
        return Err(ReviewSkipReason::AlreadyReviewed);
    }
    let in_cooldown = store
        .is_review_in_cooldown(session_id, cfg.review.cooldown_mins)
        .unwrap_or(false);
    // Hanya tahan ke pending_review_signal bila benar-benar terblokir oleh cooldown (Masalah 2).
    if in_cooldown {
        record_pending_signal_if_present(store, session_id, messages);
    }
    let pending_sig = store
        .get_pending_review_signal(session_id)
        .ok()
        .flatten()
        .filter(|sig| messages.len() > sig.first_msg_idx);
    let substantive =
        pending_sig.is_some() || is_session_substantive(messages, cfg.review.min_tool_calls);
    if !substantive {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::TrivialSession.as_str(),
            "sesi trivial tanpa sinyal belajar (tanpa koreksi/kegagalan pulih/tool substantif)",
        );
        return Err(ReviewSkipReason::TrivialSession);
    }
    if in_cooldown {
        let detail = match &pending_sig {
            Some(sig) => format!(
                "cooldown {} menit masih aktif — sinyal tertunda ({}) disimpan untuk ditindaklanjuti pasca-cooldown",
                cfg.review.cooldown_mins, sig.signal_kind
            ),
            None => format!(
                "cooldown {} menit masih aktif untuk sesi ini",
                cfg.review.cooldown_mins
            ),
        };
        let _ = store.set_last_review_skip(ReviewSkipReason::CooldownActive.as_str(), &detail);
        return Err(ReviewSkipReason::CooldownActive);
    }
    Ok(model)
}

fn build_unified_diff(title: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("diff {title}\n");
    let mut shown = 0usize;
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        if shown < 100 {
            out.push_str(sign);
            out.push_str(change.value());
            if !change.value().ends_with('\n') {
                out.push('\n');
            }
            shown += 1;
        }
    }
    out
}

fn strip_frontmatter(raw: &str) -> &str {
    let text = raw.trim_start_matches('\u{feff}');
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(after_first_nl) = rest.find('\n').map(|i| &rest[i + 1..]) {
            if let Some(end_idx) = after_first_nl.find("\n---") {
                let after = &after_first_nl[end_idx + 4..];
                return after.trim_start_matches(['\r', '\n']);
            }
        }
    }
    text
}

/// Taksiran overhead token tetap yang ditambahkan relay/gateway tertentu (mis. rute `ag/*` di 9Router
/// menyuntikkan ~2.086 token system prompt dasar tersembunyi ke setiap request — terukur empiris 2.096
/// token untuk payload 10 token). Menyisihkan overhead ini menjaga `review_runs.input_tokens` aktual
/// tetap ketat `<= 5.000` token/panggilan (Masalah 3 v0.5.25).
pub fn provider_base_overhead_tokens(model_name: &str) -> u64 {
    let low = model_name.trim().to_ascii_lowercase();
    if low.starts_with("ag/") || low.contains("/ag/") {
        2_150
    } else {
        0
    }
}

/// Hitung taksiran token konservatif atas TOTAL request review yang benar-benar dikirim ke provider
/// (awalan system prompt beku + seluruh pesan user/digest + skema tool bila ada + overhead framing JSON).
/// Dikalibrasi agar tidak pernah meremehkan tokenizer Gemini/OpenAI/Anthropic pada teks campuran
/// Bahasa Indonesia + Markdown + JSON (Masalah 3 v0.5.25).
pub fn estimate_review_request_tokens(messages: &[Message], tools: &[ToolSpec]) -> u64 {
    fn estimate_str_tokens(s: &str) -> u64 {
        let chars = s.chars().count() as u64;
        let bytes = s.len() as u64;
        let by_chars = chars.saturating_mul(10).div_ceil(28);
        let by_bytes = bytes.saturating_mul(10).div_ceil(30);
        by_chars.max(by_bytes)
    }

    let mut total: u64 = 24; // envelope JSON request (`model`, `stream`, `messages`, dsb.)
    for m in messages {
        total = total.saturating_add(16); // framing per pesan (`role`, `content`)
        for b in &m.content {
            match b {
                Block::Text(t) => total = total.saturating_add(estimate_str_tokens(t)),
                Block::Image { media_type, .. } => {
                    total = total.saturating_add(estimate_str_tokens(media_type).saturating_add(80))
                }
                Block::ToolUse { name, args, .. } => {
                    total = total
                        .saturating_add(estimate_str_tokens(name))
                        .saturating_add(estimate_str_tokens(&args.to_string()))
                        .saturating_add(16);
                }
                Block::ToolResult { content, .. } => {
                    total = total
                        .saturating_add(estimate_str_tokens(content))
                        .saturating_add(12);
                }
            }
        }
    }
    for t in tools {
        total = total
            .saturating_add(estimate_str_tokens(&t.name))
            .saturating_add(estimate_str_tokens(&t.description))
            .saturating_add(estimate_str_tokens(&t.parameters.to_string()))
            .saturating_add(24);
    }
    total
}

/// Rakit masukan review yang hemat token, teredaksi, & sadar-cache:
/// - Awalan `Role::System` memakai `FROZEN_REVIEW_SYSTEM_PROMPT` (byte-identik lintas sesi agar hit cache).
/// - Ekor `Role::User` dipangkas progresif sampai TOTAL request tunduk pada `min(max_tokens, 5.000)` token/panggilan.
/// - Mengembalikan `(system_prompt, user_prompt)` (kompatibel API lama).
pub fn build_pruned_review_prompt(
    cwd: &Path,
    store: &Store,
    messages: &[Message],
    max_tokens: u64,
) -> (String, String) {
    let (sys, user, _) = build_pruned_review_prompt_with_reads(cwd, store, messages, max_tokens);
    (sys, user)
}

/// Sama seperti `build_pruned_review_prompt`, sekaligus mengembalikan himpunan path skill yang benar-benar
/// baru dibaca segar pada jalan review ini (penjaga read-before-write).
pub fn build_pruned_review_prompt_with_reads(
    cwd: &Path,
    store: &Store,
    messages: &[Message],
    max_tokens: u64,
) -> (String, String, HashSet<PathBuf>) {
    build_pruned_review_prompt_with_reads_for_session(cwd, store, None, messages, max_tokens)
}

struct TranscriptItem {
    line: String,
    priority: bool,
    is_tool_result: bool,
}

/// Rakit masukan review untuk `session_id` tertentu (Masalah 2 & 3 v0.5.25):
/// - Mencakup SEMUA giliran sejak review terakhir yang selesai (termasuk koreksi yang tertunda cooldown).
/// - Mempertahankan giliran koreksi pengguna & kegagalan-terpulihkan pada prioritas tertinggi saat pemangkasan.
/// - Menjamin ukuran TOTAL request (system beku + ekor dinamis) diukur utuh dan dipangkas progresif
///   sampai muat dalam pagu `min(max_tokens, MAX_REVIEW_CALL_INPUT_TOKENS)`.
pub fn build_pruned_review_prompt_with_reads_for_session(
    cwd: &Path,
    store: &Store,
    session_id: Option<&str>,
    messages: &[Message],
    max_tokens: u64,
) -> (String, String, HashSet<PathBuf>) {
    // Tentukan indeks awal transkrip sejak review terakhir yang selesai (atau sejak sinyal tertunda pertama).
    let start_idx = if let Some(sid) = session_id {
        let mut idx = store.last_reviewed_msg_count(sid).unwrap_or(0);
        if let Ok(Some(sig)) = store.get_pending_review_signal(sid) {
            idx = idx.min(sig.first_msg_idx);
        }
        if idx > 0 && idx < messages.len() {
            idx
        } else {
            0
        }
    } else {
        0
    };

    let mut pruned = messages[start_idx..].to_vec();
    let _ = prune_old_tool_results(&mut pruned, 2);

    // Kumpulkan indeks pesan prioritas (koreksi eksplisit pengguna & rangkaian kegagalan terpulihkan)
    // agar tidak pernah terbuang lebih dulu saat pemangkasan anggaran token.
    let mut priority_msg_idxs: HashSet<usize> = HashSet::new();
    let mut saw_error_recently = false;
    for (i, m) in pruned.iter().enumerate() {
        if m.role == Role::User && has_user_correction(std::slice::from_ref(m)) {
            priority_msg_idxs.insert(i);
            if i > 0 {
                priority_msg_idxs.insert(i - 1);
            }
            if i + 1 < pruned.len() {
                priority_msg_idxs.insert(i + 1);
            }
        }
        for b in &m.content {
            if let Block::ToolResult { is_error, .. } = b {
                if *is_error {
                    saw_error_recently = true;
                    priority_msg_idxs.insert(i);
                    if i > 0 {
                        priority_msg_idxs.insert(i - 1);
                    }
                } else if saw_error_recently {
                    priority_msg_idxs.insert(i);
                    if i > 0 {
                        priority_msg_idxs.insert(i - 1);
                    }
                    if i + 1 < pruned.len() {
                        priority_msg_idxs.insert(i + 1);
                    }
                    saw_error_recently = false;
                }
            }
        }
    }

    let mut mentioned_skills: HashSet<String> = HashSet::new();
    // Pindai juga seluruh `messages` untuk skill yang pernah dibuka di giliran awal sesi (sebelum start_idx)
    // agar skill termuat di giliran 1 tetap dibaca segar saat koreksi datang di giliran 2+!
    for m in messages {
        for b in &m.content {
            if let Block::ToolUse { name, args, .. } = b {
                if name == "skill_view" || name == "skill_edit" || name == "manage_skill" {
                    if let Some(sname) = args.get("name").and_then(|v| v.as_str()) {
                        mentioned_skills.insert(sname.to_ascii_lowercase());
                    }
                }
            }
        }
    }

    let mut items: Vec<TranscriptItem> = Vec::new();
    for (i, m) in pruned.iter().enumerate() {
        if m.role == Role::System {
            continue;
        }
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "SELENA",
            Role::Tool => "TOOL",
            Role::System => continue,
        };
        let is_prio = priority_msg_idxs.contains(&i);
        for b in &m.content {
            let (raw_line, is_res) = match b {
                Block::Text(t) => {
                    let cap = if is_prio { 420 } else { 300 };
                    let s: String = t.chars().take(cap).collect();
                    (format!("[{role}] {s}\n"), false)
                }
                Block::Image { media_type, .. } => (format!("[{role}] [image:{media_type}]\n"), false),
                Block::ToolUse { name, args, .. } => {
                    if name == "skill_view" || name == "skill_edit" || name == "manage_skill" {
                        if let Some(sname) = args.get("name").and_then(|v| v.as_str()) {
                            mentioned_skills.insert(sname.to_ascii_lowercase());
                        }
                    }
                    let a: String = args.to_string().chars().take(180).collect();
                    (format!("[CALL] {name} {a}\n"), false)
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let first_two: String = content
                        .lines()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" | ")
                        .chars()
                        .take(160)
                        .collect();
                    let tag = if *is_error { "ERR" } else { "OK" };
                    (format!("[RESULT {tag}] {first_two}\n"), true)
                }
            };
            items.push(TranscriptItem {
                line: redact_secrets(&raw_line),
                priority: is_prio,
                is_tool_result: is_res,
            });
        }
    }

    let full_transcript_lower: String = items
        .iter()
        .map(|it| it.line.as_str())
        .collect::<Vec<_>>()
        .join("")
        .to_ascii_lowercase();

    let scan = scan_skills_with_warnings(cwd);
    let raw_index = if scan.skills.is_empty() {
        "(belum ada skill)".to_string()
    } else {
        redact_secrets(&format_skills_index(&scan.skills))
    };

    // Baca segar hingga 3 skill paling relevan (prioritaskan skill yang eksplisit dibuka lewat skill_view).
    let mut read_skill_paths: HashSet<PathBuf> = HashSet::new();
    let mut relevant_skill_blocks: Vec<(bool, String, String, String)> = Vec::new();
    for s in &scan.skills {
        let sname_low = s.name.to_ascii_lowercase();
        let explicit = mentioned_skills.contains(&sname_low);
        let is_relevant = explicit
            || full_transcript_lower.contains(&sname_low)
            || s.tags
                .iter()
                .any(|t| t.len() >= 3 && full_transcript_lower.contains(&t.to_ascii_lowercase()));
        if !is_relevant {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&s.path) {
            let canon = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
            read_skill_paths.insert(canon);
            read_skill_paths.insert(s.path.clone());
            let safe = redact_secrets(&raw);
            let origin_tag = if s.is_improvement_skill() {
                "selena-improvement".to_string()
            } else {
                s.scope.to_string()
            };
            relevant_skill_blocks.push((explicit, s.name.clone(), origin_tag, safe));
        }
    }
    relevant_skill_blocks.sort_by_key(|(explicit, _, _, _)| if *explicit { 0u8 } else { 1u8 });
    relevant_skill_blocks.truncate(3);

    // Kumpulkan daftar memori.
    let cwd_str = cwd.to_string_lossy().to_string();
    let mem_dir = crate::user_memory::user_memory_dir();
    let persistent_mems = crate::user_memory::load_entries_from(
        &mem_dir,
        crate::user_memory::MemoryStoreKind::Memory,
    );
    let legacy_mems = store.list_memories(&cwd_str, 20).unwrap_or_default();
    let mut mem_lines: Vec<String> = Vec::new();
    for (idx, content) in persistent_mems.iter().enumerate() {
        mem_lines.push(redact_secrets(&format!("- #{} [memory] {}\n", idx + 1, content)));
    }
    for m in &legacy_mems {
        let scope = if m.scope == "global" { "global" } else { "project" };
        mem_lines.push(redact_secrets(&format!(
            "- #{} [{} | {}] {}\n",
            m.id, m.category, scope, m.content
        )));
    }

    let call_cap = max_tokens.min(MAX_REVIEW_CALL_INPUT_TOKENS);
    // Target internal dengan margin aman terhadap tokenizer provider agar `review_runs.input_tokens`
    // selalu <= 5.000 token pada model Gemini/OpenAI/Claude.
    let target_budget = if call_cap >= 1_500 {
        call_cap.saturating_sub(200)
    } else {
        call_cap
    };

    let render_user = |idx_cap: usize,
                       skill_body_cap: usize,
                       max_skills: usize,
                       mem_cap: usize,
                       transcript_cap: usize,
                       drop_non_prio_results: bool,
                       only_prio_and_tail: bool|
     -> String {
        let mut skills_section: String = raw_index.chars().take(idx_cap).collect();
        let mut bodies = String::new();
        for (_, name, origin_tag, safe) in relevant_skill_blocks.iter().take(max_skills) {
            let cut: String = safe.chars().take(skill_body_cap).collect();
            bodies.push_str(&format!("\n--- SKILL `{name}` ({origin_tag}) ---\n{cut}\n"));
        }
        if !bodies.is_empty() {
            skills_section.push_str("\nIsi skill relevan:");
            skills_section.push_str(&bodies);
        }

        let mut mem_section = String::new();
        if mem_lines.is_empty() || mem_cap == 0 {
            mem_section.push_str("(belum ada catatan memory)");
        } else {
            for line in &mem_lines {
                if mem_section.chars().count() + line.chars().count() > mem_cap {
                    break;
                }
                mem_section.push_str(line);
            }
            if mem_section.is_empty() {
                mem_section.push_str("(ringkas karena pagu token)");
            }
        }

        // Seleksi baris transkrip: selalu pertahankan item prioritas (koreksi & kegagalan pulih),
        // lalu isi sisa kuota dari item terbaru ke terlama agar konteks akhir & koreksi utuh.
        let filtered: Vec<(usize, &TranscriptItem)> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                if only_prio_and_tail {
                    it.priority
                } else if drop_non_prio_results && it.is_tool_result && !it.priority {
                    false
                } else {
                    true
                }
            })
            .collect();

        let mut chosen_idxs: HashSet<usize> = HashSet::new();
        let mut used_chars = 0usize;
        // Pass 1: semua item prioritas lebih dulu.
        for &(orig_idx, it) in &filtered {
            if it.priority {
                let len = it.line.chars().count();
                if used_chars + len <= transcript_cap {
                    chosen_idxs.insert(orig_idx);
                    used_chars += len;
                }
            }
        }
        // Pass 2: item non-prioritas dari terbaru ke terlama (buang keluaran lama lebih dulu!).
        if !only_prio_and_tail {
            for &(orig_idx, it) in filtered.iter().rev() {
                if chosen_idxs.contains(&orig_idx) {
                    continue;
                }
                let len = it.line.chars().count();
                if used_chars + len <= transcript_cap {
                    chosen_idxs.insert(orig_idx);
                    used_chars += len;
                }
            }
        }
        let mut transcript = String::new();
        for (orig_idx, it) in items.iter().enumerate() {
            if chosen_idxs.contains(&orig_idx) {
                transcript.push_str(&it.line);
            }
        }
        if transcript.is_empty() && !items.is_empty() {
            // Minimal ambil item prioritas terakhir (atau item terakhir) dipotong ke transcript_cap.
            let fallback = items
                .iter()
                .rev()
                .find(|it| it.priority)
                .or_else(|| items.last());
            if let Some(it) = fallback {
                transcript = it.line.chars().take(transcript_cap.max(160)).collect();
            }
        }

        format!(
            "=== INDEKS & SKILL RELEVAN ===\n{skills_section}\n\n\
             === CATATAN MEMORY SAAT INI ===\n{mem_section}\n\n\
             === TRANSKRIP SESI (PRUNED) ===\n{transcript}\n\n\
             Analisa sesi ini dan kembalikan HANYA JSON {{\"actions\":[...]}}."
        )
    };

    // Tahapan pemangkasan progresif sampai TOTAL request (System beku + User) <= target_budget.
    let stages: &[(usize, usize, usize, usize, usize, bool, bool)] = &[
        (900, 1200, 3, 700, 2600, false, false),
        (750, 1100, 2, 550, 2100, true, false),
        (600, 1000, 2, 400, 1650, true, false),
        (450, 900, 1, 250, 1250, true, false),
        (350, 800, 1, 150, 950, true, true),
        (220, 650, 1, 0, 600, true, true),
    ];

    let mut chosen_user = String::new();
    for &(idx_cap, skill_cap, max_sk, mem_cap, tr_cap, drop_res, only_prio) in stages {
        let candidate = render_user(
            idx_cap,
            skill_cap,
            max_sk,
            mem_cap,
            tr_cap,
            drop_res,
            only_prio,
        );
        let probe_msgs = [
            Message::text(Role::System, FROZEN_REVIEW_SYSTEM_PROMPT),
            Message::text(Role::User, candidate.as_str()),
        ];
        let est = estimate_review_request_tokens(&probe_msgs, &[]);
        chosen_user = candidate;
        if est <= target_budget {
            break;
        }
    }

    (
        FROZEN_REVIEW_SYSTEM_PROMPT.to_string(),
        chosen_user,
        read_skill_paths,
    )
}

#[derive(Debug, Deserialize)]
struct ReviewPlanResponse {
    #[serde(default)]
    actions: Vec<ReviewAction>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ReviewAction {
    CreateSkill {
        #[serde(alias = "name")]
        skill: String,
        #[serde(default)]
        description: String,
        #[serde(alias = "content")]
        body: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        reason: String,
    },
    PatchSkill {
        #[serde(alias = "name")]
        skill: String,
        #[serde(default, alias = "old_text")]
        old_snippet: Option<String>,
        #[serde(default, alias = "new_text")]
        new_snippet: Option<String>,
        #[serde(default)]
        append_section: Option<String>,
        #[serde(default)]
        reason: String,
    },
    AddSkillReference {
        #[serde(alias = "name")]
        skill: String,
        ref_path: String,
        content: String,
        #[serde(default)]
        reason: String,
    },
    AddMemory {
        #[allow(dead_code)]
        #[serde(default = "default_general_cat")]
        category: String,
        #[allow(dead_code)]
        #[serde(default = "default_project_scope")]
        scope: String,
        content: String,
        #[serde(default)]
        reason: String,
    },
    DeleteMemory {
        memory_id: i64,
        #[serde(default)]
        reason: String,
    },
    OverwriteMemory {
        memory_id: i64,
        #[serde(default)]
        category: Option<String>,
        new_content: String,
        #[serde(default)]
        reason: String,
    },
    DeleteSkill {
        skill: String,
        #[serde(default)]
        reason: String,
    },
    OverwriteSkill {
        skill: String,
        body: String,
        #[serde(default)]
        reason: String,
    },
}

fn default_general_cat() -> String {
    "general".to_string()
}
fn default_project_scope() -> String {
    "project".to_string()
}

/// Ekstrak JSON dari jawaban model (tahan terhadap blok pagar ```json ... ```).
fn extract_json_object(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return Some(&trimmed[start..=end]);
            }
        }
    }
    None
}

fn short_reason(raw: &str, fallback: &str) -> String {
    let clean = redact_secrets(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if clean.is_empty() {
        return fallback.to_string();
    }
    let count = clean.chars().count();
    if count <= 64 {
        clean
    } else {
        let head: String = clean.chars().take(63).collect();
        format!("{head}…")
    }
}

fn validate_safe_ref_path(raw_ref: &str) -> Result<String> {
    let norm = raw_ref.trim().replace('\\', "/");
    if norm.is_empty() {
        anyhow::bail!("ref_path kosong");
    }
    let p = Path::new(&norm);
    if p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("ref_path tidak aman: {raw_ref}");
    }
    if !(norm.starts_with("references/")
        || norm.starts_with("templates/")
        || norm.starts_with("scripts/"))
    {
        anyhow::bail!("ref_path harus di bawah references/, templates/, atau scripts/");
    }
    if is_secret_file(p) {
        anyhow::bail!("ref_path file rahasia ditolak");
    }
    Ok(norm)
}

/// Ganti potongan lama DI TEMPATNYA (exact match dulu, lalu fallback cocok-baris ter-trim whitespace).
/// Tidak pernah menempelkan di akhir berkas — mengembalikan `None` bila teks lama tidak ditemukan
/// (Masalah 5 v0.5.25).
fn replace_snippet_in_place(old_body: &str, old_sn: &str, new_sn: &str) -> Option<String> {
    if old_body.contains(old_sn) {
        return Some(old_body.replacen(old_sn, new_sn, 1));
    }
    let needle_lines: Vec<&str> = old_sn
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if needle_lines.is_empty() {
        return None;
    }
    let body_lines: Vec<&str> = old_body.lines().collect();
    if needle_lines.len() > body_lines.len() {
        return None;
    }
    for start in 0..=body_lines.len() - needle_lines.len() {
        let mut matched = true;
        for (offset, nl) in needle_lines.iter().enumerate() {
            if body_lines[start + offset].trim() != *nl {
                matched = false;
                break;
            }
        }
        if matched {
            let mut out: Vec<String> = Vec::with_capacity(body_lines.len());
            for l in &body_lines[..start] {
                out.push((*l).to_string());
            }
            out.push(new_sn.to_string());
            for l in &body_lines[start + needle_lines.len()..] {
                out.push((*l).to_string());
            }
            return Some(out.join("\n"));
        }
    }
    None
}

fn reason_looks_like_correction(reason: &str) -> bool {
    let low = reason.to_ascii_lowercase();
    [
        "koreksi",
        "ganti",
        "perbaiki",
        "ubah",
        "salah",
        "keliru",
        "bukan",
        "usang",
        "replace",
        "fix",
        "correct",
        "wrong",
        "update rule",
        "aturan",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

/// Jalankan Selena Improvement (v0.5.25) secara aman, idempoten, sadar-cache, & non-blocking.
/// - Penjaga slot eksklusif in-process + `covered_msg_count` di DB mencegah tembak ganda (Masalah 1).
/// - Sinyal kelas-satu tertunda tidak pernah ditelan cooldown & dibersihkan sesudah tercakup (Masalah 2).
/// - Pagu total request diukur utuh <= `min(max_tokens, 5.000)`; bila minimal pun tak muat -> `digest_over_budget` (Masalah 3).
/// - Memakai awalan system prompt beku (`FROZEN_REVIEW_SYSTEM_PROMPT` >= 2.048 token) + mencatat `cache_read_tokens` (Masalah 4).
/// - Patch koreksi wajib mengganti teks lama di tempatnya (tolak + retry 1x / staging bila murni tempel di akhir, Masalah 5).
/// - Skill BARU lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md` dengan `origin: selena-improvement` (Masalah 6).
pub async fn run_background_review(
    provider: Arc<dyn Provider>,
    cfg: &Config,
    store: &Store,
    cwd: &Path,
    session_id: &str,
    messages: &[Message],
    mark_delivered: bool,
) -> Result<Option<BgReviewOutcome>> {
    // Masalah 1: penjaga idempotensi serentak untuk `(session_id, messages.len())`.
    let covered_msg_count = messages.len();
    let Some(_in_flight_guard) = store.acquire_review_slot(session_id, covered_msg_count) else {
        return Ok(None);
    };

    if check_review_eligibility(cfg, store, session_id, messages).is_err() {
        return Ok(None);
    }

    let max_tokens = cfg.review.max_tokens.max(1000);
    let call_cap = max_tokens.min(MAX_REVIEW_CALL_INPUT_TOKENS);
    let model_name = provider.model_name().to_string();
    let gateway_overhead = provider_base_overhead_tokens(&model_name);
    let effective_selen_cap = call_cap.saturating_sub(gateway_overhead);
    let (sys, user, read_skill_paths) = build_pruned_review_prompt_with_reads_for_session(
        cwd,
        store,
        Some(session_id),
        messages,
        effective_selen_cap,
    );
    let review_msgs = vec![
        Message::text(Role::System, sys),
        Message::text(Role::User, user),
    ];

    // Masalah 3: ukur ukuran TOTAL request (payload Selen + overhead gateway terukur) sebelum mengirim —
    // jika bahkan digest minimal melampaui pagu, lewati dengan alasan eksplisit `digest_over_budget`.
    let est = estimate_review_request_tokens(&review_msgs, &[]).saturating_add(gateway_overhead);
    if est > call_cap {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::DigestOverBudget.as_str(),
            &format!(
                "total request minimal ({est} token termasuk overhead gateway {gateway_overhead}) melampaui pagu {call_cap} token/panggilan"
            ),
        );
        eprintln!(
            "[selena-improvement] dilewati (digest_over_budget): estimasi total ({est}) > pagu ({call_cap})"
        );
        return Ok(None);
    }

    let (tx, _rx) = mpsc::channel::<StreamEvent>(32);
    let reply = match provider.stream_chat(&review_msgs, &[], tx).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[selena-improvement] gagal memanggil model review (diam): {e}");
            return Ok(None);
        }
    };

    let usage = reply.usage.unwrap_or(Usage {
        input_tokens: est,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    });
    let (mut in_tok, mut out_tok, mut cache_read, mut cache_write) = (
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
    let event_id = uuid::Uuid::new_v4().simple().to_string();
    let mut call_usages: Vec<(&'static str, Usage)> = vec![("initial", usage)];

    // Catat biaya review TERPISAH di tabel `usage` (`kind='review'`, `session_id="review:<sid>"`) lengkap dengan cache!
    let _ = store.record_review_usage_full(session_id, &model_name, usage);

    let cwd_str = cwd.to_string_lossy().to_string();
    // Jika total token aktual melewati anggaran keras (> 125% max_tokens), catat run & berhenti diam-diam.
    if in_tok.saturating_add(out_tok) > max_tokens.saturating_mul(5) / 4 {
        eprintln!(
            "[selena-improvement] anggaran token terlampaui ({} > {}), menghentikan evaluasi diam-diam",
            in_tok + out_tok,
            max_tokens
        );
        let _ = store.record_review_call(
            session_id,
            &cwd_str,
            &model_name,
            "",
            true,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            covered_msg_count,
            &event_id,
            1,
            1,
            "initial",
        );
        let _ = store.clear_pending_review_signal(session_id);
        return Ok(None);
    }

    let Some(json_str) = extract_json_object(&reply.text) else {
        eprintln!("[selena-improvement] output model bukan JSON valid (diam)");
        let _ = store.record_review_call(
            session_id,
            &cwd_str,
            &model_name,
            "",
            true,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            covered_msg_count,
            &event_id,
            1,
            1,
            "initial",
        );
        let _ = store.clear_pending_review_signal(session_id);
        return Ok(None);
    };

    let parsed: ReviewPlanResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[selena-improvement] gagal parse JSON review (diam): {e}");
            let _ = store.record_review_call(
                session_id,
                &cwd_str,
                &model_name,
                "",
                true,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                covered_msg_count,
                &event_id,
                1,
                1,
                "initial",
            );
            let _ = store.clear_pending_review_signal(session_id);
            return Ok(None);
        }
    };

    let mut applied: Vec<AppliedReviewChange> = Vec::new();
    let mut staged_ids: Vec<i64> = Vec::new();
    let mut staged_kinds: Vec<String> = Vec::new();
    let mut backup_ids: Vec<i64> = Vec::new();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let session_has_correction = has_user_correction(messages);
    let mut correction_retry_used = false;

    for action in parsed.actions.into_iter().take(6) {
        match action {
            ReviewAction::CreateSkill {
                skill,
                description,
                body,
                tags,
                reason,
            } => {
                let Ok(clean_name) = sanitize_skill_name(&skill) else {
                    continue;
                };
                let safe_body = redact_secrets(body.trim());
                if safe_body.is_empty() {
                    continue;
                }
                let desc = redact_secrets(description.trim());
                let desc = if desc.is_empty() {
                    format!("Prosedur belajar Selena untuk {clean_name}")
                } else {
                    desc.lines().next().unwrap_or("").trim().to_string()
                };
                let rsn = short_reason(&reason, "skill baru hasil belajar sesi");

                // Aturan anti-tabrakan (3.4 #1): bila nama sudah dipakai oleh skill pengguna/proyek,
                // DILARANG membuat duplikat bayangan di `skills/improvement/`; arahkan ke staging!
                let existing = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean_name);
                if let Some(ex) = existing {
                    if !ex.is_improvement_skill() {
                        let old_raw = std::fs::read_to_string(&ex.path).unwrap_or_default();
                        let Ok(new_md) = render_skill_markdown(
                            &clean_name,
                            &desc,
                            &safe_body,
                            ex.version.as_deref(),
                            &tags,
                        ) else {
                            continue;
                        };
                        let diff = build_unified_diff(&ex.path.display().to_string(), &old_raw, &new_md);
                        let payload = serde_json::json!({
                            "skill": clean_name,
                            "scope": ex.scope,
                            "path": ex.path.to_string_lossy(),
                            "new_markdown": new_md,
                            "diff": diff,
                            "reason": rsn,
                        })
                        .to_string();
                        let scope_note = if ex.scope == "project" {
                            " [salinan proyek]"
                        } else {
                            ""
                        };
                        if let Ok(pid) = store.create_review_proposal(
                            session_id,
                            &cwd_str,
                            "overwrite_skill",
                            &clean_name,
                            &format!("Perbarui skill pengguna '{clean_name}'{scope_note} ({rsn})"),
                            &payload,
                        ) {
                            staged_ids.push(pid);
                            staged_kinds.push("perbarui skill pengguna".to_string());
                        }
                        continue;
                    }
                    // Jika sudah ada sebagai skill milik Selena improvement sendiri, perlakukan seperti update dengan backup.
                    let old_raw = std::fs::read_to_string(&ex.path).unwrap_or_default();
                    let Ok(new_md) = render_skill_markdown_with_origin(
                        &clean_name,
                        &desc,
                        &safe_body,
                        Some("1.0.0"),
                        &tags,
                        Some(IMPROVEMENT_SKILL_ORIGIN),
                        Some(session_id),
                        Some(&today),
                    ) else {
                        continue;
                    };
                    if new_md == old_raw {
                        continue;
                    }
                    let diff = build_unified_diff(&ex.path.display().to_string(), &old_raw, &new_md);
                    let backup_reason = if ex.scope == "project" {
                        format!("{rsn} [salinan proyek]")
                    } else {
                        rsn.clone()
                    };
                    let bid = store
                        .record_review_backup(
                            session_id,
                            &cwd_str,
                            &clean_name,
                            &ex.path.to_string_lossy(),
                            &old_raw,
                            &diff,
                            &backup_reason,
                        )
                        .ok();
                    if let Some(id) = bid {
                        backup_ids.push(id);
                    }
                    if std::fs::write(&ex.path, &new_md).is_ok() {
                        applied.push(AppliedReviewChange {
                            kind: "patch_skill".to_string(),
                            target: clean_name,
                            reason: rsn,
                            diff: Some(diff),
                            origin: IMPROVEMENT_SKILL_ORIGIN.to_string(),
                            backup_id: bid,
                            scope: ex.scope.to_string(),
                        });
                    }
                    continue;
                }

                // Skill BARU karya Selena improvement wajib lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md`
                // dengan frontmatter `origin: selena-improvement`, sesi sumber, & tanggal (lintas sesi!).
                let (base_root, new_scope) = match user_skills_dir() {
                    Some(u) => (u, "user"),
                    None => (crate::skills::project_skills_dir(cwd), "project"),
                };
                let Ok(target_path) = improvement_skill_file_path(&base_root, &clean_name) else {
                    continue;
                };
                let Ok(new_md) = render_skill_markdown_with_origin(
                    &clean_name,
                    &desc,
                    &safe_body,
                    Some("1.0.0"),
                    &tags,
                    Some(IMPROVEMENT_SKILL_ORIGIN),
                    Some(session_id),
                    Some(&today),
                ) else {
                    continue;
                };
                if let Some(parent) = target_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(&target_path, &new_md).is_ok() {
                    let diff = build_unified_diff(
                        &target_path.display().to_string(),
                        "",
                        &new_md,
                    );
                    applied.push(AppliedReviewChange {
                        kind: "create_skill".to_string(),
                        target: clean_name,
                        reason: rsn,
                        diff: Some(diff),
                        origin: IMPROVEMENT_SKILL_ORIGIN.to_string(),
                        backup_id: None,
                        scope: new_scope.to_string(),
                    });
                }
            }
            ReviewAction::PatchSkill {
                skill,
                old_snippet,
                new_snippet,
                append_section,
                reason,
            } => {
                let Ok(clean_name) = sanitize_skill_name(&skill) else {
                    continue;
                };
                let existing = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean_name);
                let Some(info) = existing else {
                    continue;
                };
                // Penjaga read-before-write (diambil dari pola skill_manager_guards):
                // skill wajib sudah dibaca segar pada jalan review ini sebelum ditambal langsung.
                let canon_path = std::fs::canonicalize(&info.path)
                    .unwrap_or_else(|_| info.path.clone());
                let was_freshly_read = read_skill_paths.contains(&canon_path)
                    || read_skill_paths.contains(&info.path);
                let Ok(old_raw) = std::fs::read_to_string(&info.path) else {
                    continue;
                };
                let old_fm = parse_skill_frontmatter(&old_raw).ok();
                let old_body = strip_frontmatter(&old_raw).trim().to_string();
                let desc = old_fm
                    .as_ref()
                    .map(|f| f.description.clone())
                    .unwrap_or_else(|| info.summary.clone());
                let ver = old_fm.as_ref().and_then(|f| f.version.clone());
                let tags = old_fm
                    .as_ref()
                    .map(|f| f.tags.clone())
                    .unwrap_or_default();

                let mut patched_body: Option<String> = None;
                let mut destructive_overwrite = false;
                let mut is_append_only_patch = false;

                let old_sn_opt = old_snippet
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let new_sn_opt = new_snippet
                    .as_deref()
                    .map(str::trim)
                    .map(|s| s.to_string());
                let app_opt = append_section
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let is_correction_ctx = session_has_correction
                    || reason_looks_like_correction(&reason)
                    || old_sn_opt.is_some();

                if let (Some(old_sn), Some(new_sn)) = (old_sn_opt.as_deref(), new_sn_opt.as_deref())
                {
                    let safe_new = redact_secrets(new_sn);
                    if let Some(candidate) = replace_snippet_in_place(&old_body, old_sn, &safe_new)
                    {
                        let old_lines = old_body.lines().count().max(1);
                        let new_lines = candidate.lines().count();
                        // Guard: menghapus >50% baris isi skill lama dianggap timpa/destruktif -> STAGING!
                        if new_lines * 2 < old_lines {
                            destructive_overwrite = true;
                            patched_body = Some(candidate);
                        } else if candidate != old_body {
                            patched_body = Some(candidate);
                        }
                    }
                    // Masalah 5 (v0.5.25): DILARANG jatuh diam-diam ke append di akhir berkas ketika
                    // `old_snippet` tidak cocok — itu menyebabkan aturan keliru tetap utuh & kontradiktif!
                }

                if patched_body.is_none() {
                    if let Some(app) = app_opt.as_deref() {
                        let safe_app = redact_secrets(app);
                        if !old_body.contains(&safe_app) {
                            patched_body = Some(format!("{old_body}\n\n{safe_app}"));
                            is_append_only_patch = true;
                        }
                    } else if let Some(new_sn) = new_sn_opt.as_deref().filter(|s| !s.is_empty()) {
                        let safe_app = redact_secrets(new_sn);
                        if !old_body.contains(&safe_app) {
                            patched_body = Some(format!("{old_body}\n\n{safe_app}"));
                            is_append_only_patch = true;
                        }
                    }
                }

                // Masalah 5 (v0.5.25): untuk pelajaran KOREKSI pada skill improvement, patch murni
                // penambahan di akhir berkas DITOLAK -> coba ulang 1x dengan isi persis SKILL.md
                // agar model menunjuk `old_snippet` yang diganti di tempatnya; jika tetap gagal,
                // paksa masuk STAGING (tidak pernah menulis skill kontradiktif langsung ke disk!).
                let mut force_stage_unanchored_correction = false;
                if is_append_only_patch && is_correction_ctx && info.is_improvement_skill() {
                    let mut repaired = false;
                    if !correction_retry_used {
                        correction_retry_used = true;
                        let proposed_text = new_sn_opt
                            .as_deref()
                            .or(app_opt.as_deref())
                            .unwrap_or("")
                            .to_string();
                        let repair_user = format!(
                            "=== PERBAIKAN PATCH KOREKSI (WAJIB GANTI DI TEMPATNYA) ===\n\
                             Patch sebelumnya untuk skill `{clean_name}` DITOLAK karena hanya menempel teks baru \
                             di akhir berkas tanpa mengganti kalimat keliru yang sudah ada.\n\
                             Alasan koreksi: {reason}\n\
                             Usulan teks baru sebelumnya: {proposed_text}\n\n\
                             Isi PERSIS `SKILL.md` `{clean_name}` saat ini:\n\
                             ```markdown\n\
                             {old_body}\n\
                             ```\n\n\
                             Kembalikan HANYA JSON `patch_skill` dengan `old_snippet` yang DISALIN PERSIS dari isi di atas \
                             (mencakup baris/nomor aturan keliru yang harus diganti) dan `new_snippet` penggantinya yang bersih:\n\
                             {{\"actions\":[{{\"action\":\"patch_skill\",\"skill\":\"{clean_name}\",\"old_snippet\":\"...\",\"new_snippet\":\"...\",\"reason\":\"...\"}}]}}"
                        );
                        let retry_msgs = vec![
                            Message::text(Role::System, FROZEN_REVIEW_SYSTEM_PROMPT),
                            Message::text(Role::User, repair_user),
                        ];
                        let retry_est = estimate_review_request_tokens(&retry_msgs, &[])
                            .saturating_add(gateway_overhead);
                        if retry_est <= call_cap {
                            let (rtx, _rrx) = mpsc::channel::<StreamEvent>(16);
                            if let Ok(r_reply) = provider.stream_chat(&retry_msgs, &[], rtx).await {
                                let ru = r_reply.usage.unwrap_or(Usage {
                                    input_tokens: retry_est,
                                    output_tokens: 0,
                                    cache_read_tokens: 0,
                                    cache_write_tokens: 0,
                                });
                                call_usages.push(("correction_retry", ru));
                                in_tok = in_tok.saturating_add(ru.input_tokens);
                                out_tok = out_tok.saturating_add(ru.output_tokens);
                                cache_read = cache_read.saturating_add(ru.cache_read_tokens);
                                cache_write = cache_write.saturating_add(ru.cache_write_tokens);
                                let _ = store.record_review_usage_full(
                                    session_id,
                                    &model_name,
                                    ru,
                                );
                                if let Some(r_json) = extract_json_object(&r_reply.text) {
                                    if let Ok(r_plan) =
                                        serde_json::from_str::<ReviewPlanResponse>(r_json)
                                    {
                                        for r_act in r_plan.actions {
                                            if let ReviewAction::PatchSkill {
                                                old_snippet: Some(ro),
                                                new_snippet: Some(rn),
                                                ..
                                            } = r_act
                                            {
                                                let ro_t = ro.trim();
                                                let rn_t = redact_secrets(rn.trim());
                                                if !ro_t.is_empty() {
                                                    if let Some(cand) = replace_snippet_in_place(
                                                        &old_body, ro_t, &rn_t,
                                                    ) {
                                                        if cand != old_body {
                                                            let old_l =
                                                                old_body.lines().count().max(1);
                                                            let new_l = cand.lines().count();
                                                            destructive_overwrite =
                                                                new_l * 2 < old_l;
                                                            patched_body = Some(cand);
                                                            repaired = true;
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !repaired {
                        force_stage_unanchored_correction = true;
                    }
                }

                let Some(new_body) = patched_body else {
                    continue;
                };
                let new_md_res = if info.is_improvement_skill() {
                    render_skill_markdown_with_origin(
                        &clean_name,
                        &desc,
                        &new_body,
                        ver.as_deref(),
                        &tags,
                        Some(IMPROVEMENT_SKILL_ORIGIN),
                        old_fm
                            .as_ref()
                            .and_then(|f| f.source_session.as_deref())
                            .or(Some(session_id)),
                        Some(&today),
                    )
                } else {
                    render_skill_markdown(&clean_name, &desc, &new_body, ver.as_deref(), &tags)
                };
                let Ok(new_md) = new_md_res else {
                    continue;
                };
                if new_md == old_raw {
                    continue;
                }
                let rel_display = info
                    .path
                    .strip_prefix(cwd)
                    .unwrap_or(&info.path)
                    .display()
                    .to_string();
                let diff = build_unified_diff(&rel_display, &old_raw, &new_md);
                let rsn = short_reason(&reason, "perbaikan dari sesi");

                // Perlindungan skill karya pengguna (3.4 #2) + read-before-write + anti-kontradiksi (Masalah 5):
                // - Skill BUKAN milik `selena-improvement` -> SELALU tahan ke staging `/review approve`!
                // - Skill belum dibaca segar, destruktif (>50% hilang), atau koreksi tanpa `old_snippet` cocok -> STAGING!
                let must_stage = !info.is_improvement_skill()
                    || destructive_overwrite
                    || !was_freshly_read
                    || force_stage_unanchored_correction;
                if must_stage {
                    let scope_note = if info.scope == "project" {
                        " [salinan proyek]"
                    } else {
                        ""
                    };
                    let payload = serde_json::json!({
                        "skill": clean_name,
                        "scope": info.scope,
                        "path": info.path.to_string_lossy(),
                        "new_markdown": new_md,
                        "diff": diff,
                        "reason": rsn,
                        "read_before_write": was_freshly_read,
                        "user_owned": !info.is_improvement_skill(),
                        "unanchored_correction": force_stage_unanchored_correction,
                    })
                    .to_string();
                    let label = if !info.is_improvement_skill() {
                        format!("Perbarui skill pengguna '{clean_name}'{scope_note} ({rsn})")
                    } else if force_stage_unanchored_correction {
                        format!("Tinjau koreksi skill '{clean_name}'{scope_note} tanpa old_snippet persis ({rsn})")
                    } else {
                        format!("Timpa skill '{clean_name}'{scope_note} ({rsn})")
                    };
                    if let Ok(pid) = store.create_review_proposal(
                        session_id,
                        &cwd_str,
                        "overwrite_skill",
                        &clean_name,
                        &label,
                        &payload,
                    ) {
                        staged_ids.push(pid);
                        staged_kinds.push(if !info.is_improvement_skill() {
                            "perbarui skill pengguna".to_string()
                        } else {
                            "timpa skill".to_string()
                        });
                    }
                } else {
                    // Simpan cadangan isi sebelumnya agar setiap patch langsung dapat diputar balik (`/review undo`).
                    let backup_reason = if info.scope == "project" {
                        format!("{rsn} [salinan proyek]")
                    } else {
                        rsn.clone()
                    };
                    let bid = store
                        .record_review_backup(
                            session_id,
                            &cwd_str,
                            &clean_name,
                            &info.path.to_string_lossy(),
                            &old_raw,
                            &diff,
                            &backup_reason,
                        )
                        .ok();
                    if let Some(id) = bid {
                        backup_ids.push(id);
                    }
                    if std::fs::write(&info.path, &new_md).is_ok() {
                        applied.push(AppliedReviewChange {
                            kind: "patch_skill".to_string(),
                            target: clean_name,
                            reason: rsn,
                            diff: Some(diff),
                            origin: IMPROVEMENT_SKILL_ORIGIN.to_string(),
                            backup_id: bid,
                            scope: info.scope.to_string(),
                        });
                    }
                }
            }
            ReviewAction::AddSkillReference {
                skill,
                ref_path,
                content,
                reason,
            } => {
                let Ok(clean_name) = sanitize_skill_name(&skill) else {
                    continue;
                };
                let Ok(norm_ref) = validate_safe_ref_path(&ref_path) else {
                    continue;
                };
                let safe_content = format!("{}\n", redact_secrets(content.trim()));
                if safe_content.trim().is_empty() || safe_content.len() > MAX_REF_FILE_BYTES {
                    continue;
                }
                let Some(info) = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean_name)
                else {
                    continue;
                };
                let target = info.dir.join(&norm_ref);
                let rsn = short_reason(&reason, "tambah file referensi");
                if target.exists() || !info.is_improvement_skill() {
                    let old_ref = std::fs::read_to_string(&target).unwrap_or_default();
                    if old_ref == safe_content {
                        continue;
                    }
                    let diff = build_unified_diff(&norm_ref, &old_ref, &safe_content);
                    let scope_note = if info.scope == "project" {
                        " [salinan proyek]"
                    } else {
                        ""
                    };
                    let payload = serde_json::json!({
                        "skill": clean_name,
                        "scope": info.scope,
                        "ref_path": norm_ref,
                        "path": target.to_string_lossy(),
                        "new_content": safe_content,
                        "diff": diff,
                        "reason": rsn,
                    })
                    .to_string();
                    if let Ok(pid) = store.create_review_proposal(
                        session_id,
                        &cwd_str,
                        "overwrite_skill",
                        &format!("{clean_name}/{norm_ref}"),
                        &format!("Referensi '{norm_ref}' pada skill '{clean_name}'{scope_note} ({rsn})"),
                        &payload,
                    ) {
                        staged_ids.push(pid);
                        staged_kinds.push("referensi skill".to_string());
                    }
                } else {
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&target, &safe_content).is_ok() {
                        let diff = build_unified_diff(&norm_ref, "", &safe_content);
                        applied.push(AppliedReviewChange {
                            kind: "add_skill_reference".to_string(),
                            target: format!("{clean_name}/{norm_ref}"),
                            reason: rsn,
                            diff: Some(diff),
                            origin: IMPROVEMENT_SKILL_ORIGIN.to_string(),
                            backup_id: None,
                            scope: info.scope.to_string(),
                        });
                    }
                }
            }
            ReviewAction::AddMemory {
                category: _,
                scope: _,
                content,
                reason,
            } => {
                let safe_content = redact_secrets(content.trim());
                if safe_content.is_empty() || safe_content.chars().count() > 320 {
                    continue;
                }
                let mem_dir = crate::user_memory::user_memory_dir();
                let existing = crate::user_memory::load_entries_from(
                    &mem_dir,
                    crate::user_memory::MemoryStoreKind::Memory,
                );
                if existing
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&safe_content))
                {
                    continue;
                }
                if let Ok((idx, _)) = crate::user_memory::add_entry_in(
                    &mem_dir,
                    crate::user_memory::MemoryStoreKind::Memory,
                    &safe_content,
                    cfg.memory.max_memory_chars,
                ) {
                    applied.push(AppliedReviewChange {
                        kind: "add_memory".to_string(),
                        target: format!("M#{idx}"),
                        reason: short_reason(&reason, &safe_content),
                        diff: None,
                        origin: IMPROVEMENT_SKILL_ORIGIN.to_string(),
                        backup_id: None,
                        scope: "memory".to_string(),
                    });
                }
            }
            ReviewAction::DeleteMemory { memory_id, reason } => {
                let Ok(Some(mem)) = store.get_memory(memory_id) else {
                    continue;
                };
                let rsn = short_reason(&reason, "catatan memory usang");
                let payload = serde_json::json!({
                    "memory_id": mem.id,
                    "scope": mem.scope,
                    "category": mem.category,
                    "old_content": mem.content,
                    "reason": rsn,
                })
                .to_string();
                if let Ok(pid) = store.create_review_proposal(
                    session_id,
                    &cwd_str,
                    "delete_memory",
                    &mem.id.to_string(),
                    &format!("Hapus memory #{} ({rsn})", mem.id),
                    &payload,
                ) {
                    staged_ids.push(pid);
                    staged_kinds.push("hapus memory".to_string());
                }
            }
            ReviewAction::OverwriteMemory {
                memory_id,
                category,
                new_content,
                reason,
            } => {
                let Ok(Some(mem)) = store.get_memory(memory_id) else {
                    continue;
                };
                let safe_new = redact_secrets(new_content.trim());
                if safe_new.is_empty() || safe_new == mem.content {
                    continue;
                }
                let rsn = short_reason(&reason, "perbarui isi memory");
                let payload = serde_json::json!({
                    "memory_id": mem.id,
                    "old_category": mem.category,
                    "new_category": category.as_deref().map(normalize_memory_category).unwrap_or(&mem.category),
                    "old_content": mem.content,
                    "new_content": safe_new,
                    "reason": rsn,
                })
                .to_string();
                if let Ok(pid) = store.create_review_proposal(
                    session_id,
                    &cwd_str,
                    "overwrite_memory",
                    &mem.id.to_string(),
                    &format!("Timpa memory #{} ({rsn})", mem.id),
                    &payload,
                ) {
                    staged_ids.push(pid);
                    staged_kinds.push("timpa memory".to_string());
                }
            }
            ReviewAction::DeleteSkill { skill, reason } => {
                let Ok(clean_name) = sanitize_skill_name(&skill) else {
                    continue;
                };
                let Some(info) = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean_name)
                else {
                    continue;
                };
                let rsn = short_reason(&reason, "skill sudah tidak relevan");
                let payload = serde_json::json!({
                    "skill": clean_name,
                    "scope": info.scope,
                    "path": info.path.to_string_lossy(),
                    "reason": rsn,
                })
                .to_string();
                if let Ok(pid) = store.create_review_proposal(
                    session_id,
                    &cwd_str,
                    "delete_skill",
                    &clean_name,
                    &format!("Hapus skill '{clean_name}' ({rsn})"),
                    &payload,
                ) {
                    staged_ids.push(pid);
                    staged_kinds.push("hapus skill".to_string());
                }
            }
            ReviewAction::OverwriteSkill {
                skill,
                body,
                reason,
            } => {
                let Ok(clean_name) = sanitize_skill_name(&skill) else {
                    continue;
                };
                let Some(info) = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean_name)
                else {
                    continue;
                };
                let old_raw = std::fs::read_to_string(&info.path).unwrap_or_default();
                let old_fm = parse_skill_frontmatter(&old_raw).ok();
                let desc = old_fm
                    .as_ref()
                    .map(|f| f.description.clone())
                    .unwrap_or_else(|| info.summary.clone());
                let ver = old_fm.as_ref().and_then(|f| f.version.clone());
                let tags = old_fm.as_ref().map(|f| f.tags.clone()).unwrap_or_default();
                let safe_body = redact_secrets(body.trim());
                if safe_body.is_empty() {
                    continue;
                }
                let Ok(new_md) =
                    render_skill_markdown(&clean_name, &desc, &safe_body, ver.as_deref(), &tags)
                else {
                    continue;
                };
                if new_md == old_raw {
                    continue;
                }
                let rel_display = info
                    .path
                    .strip_prefix(cwd)
                    .unwrap_or(&info.path)
                    .display()
                    .to_string();
                let diff = build_unified_diff(&rel_display, &old_raw, &new_md);
                let rsn = short_reason(&reason, "tulis ulang isi skill");
                let scope_note = if info.scope == "project" {
                    " [salinan proyek]"
                } else {
                    ""
                };
                let payload = serde_json::json!({
                    "skill": clean_name,
                    "scope": info.scope,
                    "path": info.path.to_string_lossy(),
                    "new_markdown": new_md,
                    "diff": diff,
                    "reason": rsn,
                })
                .to_string();
                if let Ok(pid) = store.create_review_proposal(
                    session_id,
                    &cwd_str,
                    "overwrite_skill",
                    &clean_name,
                    &format!("Timpa skill '{clean_name}'{scope_note} ({rsn})"),
                    &payload,
                ) {
                    staged_ids.push(pid);
                    staged_kinds.push("timpa skill".to_string());
                }
            }
        }
    }

    let report_message = format_review_report(&applied, &staged_ids, &staged_kinds);
    let event_calls = call_usages.len().max(1);
    let max_call_input_tokens = call_usages
        .iter()
        .map(|(_, u)| u.input_tokens)
        .max()
        .unwrap_or(in_tok);
    let mut run_id: Option<i64> = None;
    for (idx, (call_kind, cu)) in call_usages.iter().enumerate() {
        let call_index = idx + 1;
        let is_last = call_index == event_calls;
        let row_report = if is_last {
            report_message.as_deref().unwrap_or("")
        } else {
            ""
        };
        let row_delivered = if is_last { mark_delivered } else { true };
        if let Ok(id) = store.record_review_call(
            session_id,
            &cwd_str,
            &model_name,
            row_report,
            row_delivered,
            cu.input_tokens,
            cu.output_tokens,
            cu.cache_read_tokens,
            cu.cache_write_tokens,
            covered_msg_count,
            &event_id,
            call_index,
            event_calls,
            call_kind,
        ) {
            if is_last {
                run_id = Some(id);
            }
        }
    }
    // Sinyal tertunda pada sesi ini sudah resmi ditindaklanjuti oleh review tuntas.
    let _ = store.clear_pending_review_signal(session_id);

    if report_message.is_none() {
        return Ok(None);
    }

    Ok(Some(BgReviewOutcome {
        applied,
        staged_ids,
        report_message,
        input_tokens: in_tok,
        output_tokens: out_tok,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        calls_count: event_calls,
        max_call_input_tokens,
        model: model_name,
        run_id,
        backup_ids,
    }))
}

fn format_review_report(
    applied: &[AppliedReviewChange],
    staged_ids: &[i64],
    staged_kinds: &[String],
) -> Option<String> {
    if applied.is_empty() && staged_ids.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut saw_memory_update = false;
    for ch in applied {
        let project_suffix = if ch.scope == "project" {
            " (salinan proyek)"
        } else {
            ""
        };
        match ch.kind.as_str() {
            "create_skill" => {
                parts.push(format!("Skill '{}' created{project_suffix}", ch.target));
            }
            "patch_skill" => {
                parts.push(format!("Skill '{}' patched{project_suffix}", ch.target));
            }
            "add_skill_reference" => {
                let sname = ch.target.split('/').next().unwrap_or(&ch.target);
                parts.push(format!("Skill '{sname}' patched{project_suffix}"));
            }
            "add_memory" => {
                if !saw_memory_update {
                    parts.push("Memory updated".to_string());
                    saw_memory_update = true;
                }
            }
            _ => {
                parts.push(format!("{} ({})", ch.target, ch.reason));
            }
        }
    }
    if !staged_ids.is_empty() {
        let label = if staged_ids.len() == 1 {
            format!(
                "1 usulan {} menunggu persetujuanmu — /review pending",
                staged_kinds
                    .first()
                    .map(|s| s.as_str())
                    .unwrap_or("perubahan")
            )
        } else {
            format!(
                "{} usulan menunggu persetujuanmu — /review pending",
                staged_ids.len()
            )
        };
        parts.push(label);
    }
    Some(format!("💾 Selena improvement: {}", parts.join(" · ")))
}

/// Putar balik satu patch langsung karya Selena improvement berdasarkan `backup_id`
/// (atau cadangan terbaru yang belum diputar balik bila `backup_id == None`).
pub fn rollback_direct_review_patch(store: &Store, backup_id: Option<i64>) -> Result<String> {
    let backup = match backup_id {
        Some(id) => store
            .get_review_backup(id)?
            .ok_or_else(|| anyhow!("cadangan patch #{id} tidak ditemukan"))?,
        None => store
            .list_review_backups(20)?
            .into_iter()
            .find(|b| !b.rolled_back)
            .ok_or_else(|| anyhow!("belum ada patch langsung Selena improvement yang dapat diputar balik"))?,
    };
    if backup.rolled_back {
        anyhow::bail!("cadangan patch #{} sudah pernah diputar balik sebelumnya", backup.id);
    }
    let path = PathBuf::from(&backup.file_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &backup.previous_content)
        .with_context(|| format!("gagal memulihkan {}", path.display()))?;
    store.mark_review_backup_rolled_back(backup.id)?;
    Ok(format!(
        "Patch #{} pada skill '{}' berhasil diputar balik ke isi sebelumnya ({}).",
        backup.id,
        backup.skill_name,
        path.display()
    ))
}

/// Terapkan 1 proposal staging yang disetujui pengguna (`/review approve <id>`).
pub fn apply_staged_proposal(store: &Store, cwd: &Path, proposal_id: i64) -> Result<String> {
    let prop = store
        .get_review_proposal(proposal_id)?
        .ok_or_else(|| anyhow!("proposal #{proposal_id} tidak ditemukan"))?;
    if prop.status != "pending" {
        anyhow::bail!(
            "proposal #{proposal_id} sudah diproses (status: {})",
            prop.status
        );
    }
    let payload: serde_json::Value =
        serde_json::from_str(&prop.payload_json).unwrap_or_else(|_| serde_json::json!({}));

    let result_msg = match prop.kind.as_str() {
        "delete_memory" => {
            let mid = payload
                .get("memory_id")
                .and_then(|v| v.as_i64())
                .or_else(|| prop.target.parse::<i64>().ok())
                .ok_or_else(|| anyhow!("memory_id tidak valid pada proposal #{proposal_id}"))?;
            let removed = store.remove_memory(mid)?;
            if removed {
                format!("Proposal #{proposal_id} disetujui: memory #{mid} berhasil dihapus.")
            } else {
                format!("Proposal #{proposal_id} ditandai disetujui (memory #{mid} sudah tidak ada).")
            }
        }
        "overwrite_memory" => {
            let mid = payload
                .get("memory_id")
                .and_then(|v| v.as_i64())
                .or_else(|| prop.target.parse::<i64>().ok())
                .ok_or_else(|| anyhow!("memory_id tidak valid pada proposal #{proposal_id}"))?;
            let new_content = payload
                .get("new_content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("new_content hilang pada proposal #{proposal_id}"))?;
            let new_cat = payload.get("new_category").and_then(|v| v.as_str());
            let updated = store.update_memory(mid, new_cat, new_content)?;
            if updated {
                format!("Proposal #{proposal_id} disetujui: memory #{mid} berhasil diperbarui.")
            } else {
                format!("Proposal #{proposal_id} ditandai disetujui (memory #{mid} sudah tidak ada).")
            }
        }
        "delete_skill" => {
            let sname = payload
                .get("skill")
                .and_then(|v| v.as_str())
                .unwrap_or(&prop.target);
            let removed = remove_skill(cwd, sname)?;
            if removed {
                format!("Proposal #{proposal_id} disetujui: skill '{sname}' dihapus.")
            } else {
                format!("Proposal #{proposal_id} ditandai disetujui (skill proyek '{sname}' tidak ditemukan).")
            }
        }
        "overwrite_skill" => {
            let sname = payload
                .get("skill")
                .and_then(|v| v.as_str())
                .unwrap_or(&prop.target);
            if let (Some(ref_path), Some(new_content)) = (
                payload.get("ref_path").and_then(|v| v.as_str()),
                payload.get("new_content").and_then(|v| v.as_str()),
            ) {
                let norm_ref = validate_safe_ref_path(ref_path)?;
                let clean = sanitize_skill_name(sname)?;
                let skill_dir = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean)
                    .map(|s| s.dir)
                    .ok_or_else(|| anyhow!("skill '{clean}' tidak ditemukan"))?;
                let target = skill_dir.join(&norm_ref);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, redact_secrets(new_content))
                    .with_context(|| format!("gagal menulis {}", target.display()))?;
                format!(
                    "Proposal #{proposal_id} disetujui: referensi '{norm_ref}' pada skill '{clean}' diperbarui."
                )
            } else if let Some(new_md) = payload.get("new_markdown").and_then(|v| v.as_str()) {
                let clean = sanitize_skill_name(sname)?;
                let target = scan_skills_with_warnings(cwd)
                    .skills
                    .into_iter()
                    .find(|s| s.name == clean)
                    .map(|s| s.path)
                    .unwrap_or_else(|| {
                        skill_file_path_for(cwd, &clean, None)
                            .unwrap_or_else(|_| cwd.join(".selencode/skills").join(&clean).join("SKILL.md"))
                    });
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, redact_secrets(new_md))
                    .with_context(|| format!("gagal menulis {}", target.display()))?;
                format!("Proposal #{proposal_id} disetujui: skill '{clean}' berhasil diperbarui.")
            } else {
                anyhow::bail!("payload overwrite_skill tidak lengkap pada proposal #{proposal_id}");
            }
        }
        other => anyhow::bail!("jenis proposal tidak dikenal: '{other}'"),
    };

    store.resolve_review_proposal(proposal_id, "approved")?;
    Ok(result_msg)
}

/// Tolak/buang 1 proposal staging tanpa menyentuh memory/skill (`status = 'discarded'`).
pub fn discard_staged_proposal(store: &Store, proposal_id: i64) -> Result<String> {
    let prop = store
        .get_review_proposal(proposal_id)?
        .ok_or_else(|| anyhow!("proposal #{proposal_id} tidak ditemukan"))?;
    if prop.status != "pending" {
        anyhow::bail!(
            "proposal #{proposal_id} sudah diproses (status: {})",
            prop.status
        );
    }
    store.resolve_review_proposal(proposal_id, "discarded")?;
    Ok(format!(
        "Proposal #{proposal_id} ({} `{}`) dibuang — data asli tetap utuh.",
        prop.kind, prop.target
    ))
}

/// Format satu proposal menjadi baris ringkas yang mudah dibaca di CLI/TUI/Telegram.
pub fn format_proposal_detail(prop: &ReviewProposal) -> String {
    let payload: serde_json::Value =
        serde_json::from_str(&prop.payload_json).unwrap_or_else(|_| serde_json::json!({}));
    let mut out = format!(
        "#{} [{}] target `{}` — {}",
        prop.id, prop.kind, prop.target, prop.summary
    );
    if let Some(old_c) = payload.get("old_content").and_then(|v| v.as_str()) {
        let short: String = old_c.chars().take(120).collect();
        out.push_str(&format!("\n  lama: {short}"));
    }
    if let Some(new_c) = payload.get("new_content").and_then(|v| v.as_str()) {
        let short: String = new_c.chars().take(120).collect();
        out.push_str(&format!("\n  baru: {short}"));
    }
    if let Some(diff) = payload.get("diff").and_then(|v| v.as_str()) {
        let snippet: String = diff.lines().take(8).collect::<Vec<_>>().join("\n  ");
        out.push_str(&format!("\n  {snippet}"));
    }
    out
}
