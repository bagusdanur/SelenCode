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
use crate::providers::{Block, Message, Provider, Role, StreamEvent, Usage};
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

/// Awalan system prompt BEKU (byte-identik lintas seluruh review & sesi) agar panggilan Selena
/// improvement mengenai prefix prompt cache provider. Seluruh digest sesi ditaruh di ekor (pesan User).
pub const FROZEN_REVIEW_SYSTEM_PROMPT: &str = "\
Kamu adalah modul Selena Improvement milik SelenCode (tinjauan belajar pasca-tugas di latar).\n\
Tugasmu: tinjau transkrip sesi yang telah selesai, indeks skill & isi skill relevan, serta catatan memory saat ini.\n\
Keluarkan HANYA satu objek JSON valid tanpa teks pembuka/penutup:\n\
{\"actions\": [...]}\n\
\n\
SINYAL YANG MEWAJIBKAN TINDAKAN:\n\
1. Koreksi pengguna atas gaya, format, path, atau alur kerja (sinyal kelas-satu).\n\
2. Teknik, perintah, atau workaround non-trivial yang terbukti berhasil sesudah percobaan/kegagalan yang dipulihkan di sesi ini.\n\
3. Skill yang dimuat sesi ini ternyata salah, kurang, atau usang dibandingkan kenyataan sesi.\n\
\n\
URUTAN PREFERENSI TINDAKAN:\n\
(1) Tambal skill yang sedang dimuat pada sesi ini (`patch_skill`).\n\
(2) Tambal skill payung yang sudah ada (`patch_skill`).\n\
(3) Tambah berkas pendukung pada skill yang ada (`add_skill_reference`: `references/`, `templates/`, `scripts/`).\n\
(4) Buat skill payung kelas-baru (`create_skill`) untuk prosedur lintas-sesi yang benar-benar dapat dipakai ulang.\n\
(5) Simpan fakta preferensi/prosedur sempit ke memory (`add_memory`) bila memang preferensi/fakta tetap.\n\
\n\
BENTUK PELAJARAN (WAJIB):\n\
- Tulis prosedur + jebakan sebagai aturan imperatif yang dapat digeneralisasi + satu klausa mekanisme sebabnya (BUKAN narasi kejadian sesi).\n\
- DILARANG mencantumkan nomor PR, ID tiket, tanggal insiden, atau kutipan mentah percakapan sebagai isi aturan.\n\
- Pelajaran yang sama muncul dua kali = perkuat/perjelas satu aturan yang sudah ada, JANGAN gandakan.\n\
- Perbaiki kalimat keliru langsung di tempatnya (`old_snippet` -> `new_snippet`), JANGAN tempel 'UPDATE:' di bawahnya.\n\
\n\
DAFTAR JANGAN-TANGKAP (DILARANG DISIMPAN):\n\
- Kegagalan tergantung lingkungan (binary belum terpasang, kredensial belum diatur, paket hilang, path mismatch pasca-migrasi).\n\
- Klaim negatif tentang tool ('tool X rusak/tidak jalan') yang dapat mengeras menjadi penolakan permanen.\n\
- Error sesaat yang sudah pulih (pelajarannya adalah pola ulang/retry-nya, bukan kegagalan sesaatnya).\n\
- Narasi tugas sekali jalan yang tidak membentuk kelas prosedur berulang.\n\
- Kegagalan yang TIDAK terselesaikan (jangan pernah menulis percobaan gagal sebagai workflow andal).\n\
- Rahasia, token, password, API key, atau data sensitif apa pun.\n\
\n\
JENIS AKSI JSON YANG DIDUKUNG:\n\
1. {\"action\":\"patch_skill\",\"skill\":\"<nama-kebab>\",\"old_snippet\":\"<potongan lama persis dari SKILL.md yang baru dibaca>\",\"new_snippet\":\"<potongan baru>\",\"append_section\":\"<opsional jika menambah seksi baru>\",\"reason\":\"<alasan <=60 char>\"}\n\
2. {\"action\":\"create_skill\",\"skill\":\"<nama-kebab>\",\"description\":\"<ringkasan 1 baris <=80 char>\",\"body\":\"<markdown prosedur konkret>\",\"tags\":[\"tag1\"],\"reason\":\"<alasan <=60 char>\"}\n\
3. {\"action\":\"add_skill_reference\",\"skill\":\"<nama-kebab>\",\"ref_path\":\"references/<topik>.md\",\"content\":\"<markdown singkat>\",\"reason\":\"<alasan <=60 char>\"}\n\
4. {\"action\":\"add_memory\",\"category\":\"arch|commands|conventions|decisions|issues|tasks|general\",\"scope\":\"project|global\",\"content\":\"<fakta sempit & konkret tanpa rahasia>\",\"reason\":\"<alasan <=60 char>\"}\n\
5. {\"action\":\"delete_memory\",\"memory_id\":123,\"reason\":\"<alasan>\"}\n\
6. {\"action\":\"overwrite_memory\",\"memory_id\":123,\"category\":\"...\",\"new_content\":\"...\",\"reason\":\"<alasan>\"}\n\
7. {\"action\":\"delete_skill\",\"skill\":\"<nama-kebab>\",\"reason\":\"<alasan>\"}\n\
8. {\"action\":\"overwrite_skill\",\"skill\":\"<nama-kebab>\",\"body\":\"<isi lengkap>\",\"reason\":\"<alasan>\"}\n\
\n\
ATURAN PENUTUP:\n\
- DILARANG menyentuh berkas/config proyek pengguna — hanya skill & memory SelenCode.\n\
- Jika tidak ada pelajaran konkret & tahan lama di sesi ini -> kembalikan persis: {\"actions\":[]}";

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
}

impl ReviewSkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoAuxModelConfigured => "no_model_configured",
            Self::CooldownActive => "cooldown_active",
            Self::TrivialSession => "trivial_session",
            Self::BudgetExceeded => "budget_exceeded",
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

/// Cek semua prasyarat pemicu Selena improvement sebelum memanggil provider.
/// Setiap alasan lewati dicatat ke `store` (`last_review_skip`) agar terdiagnosis di `/doctor` & `/review status`.
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
    if !is_session_substantive(messages, cfg.review.min_tool_calls) {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::TrivialSession.as_str(),
            "sesi trivial tanpa sinyal belajar (tanpa koreksi/kegagalan pulih/tool substantif)",
        );
        return Err(ReviewSkipReason::TrivialSession);
    }
    if store
        .is_review_in_cooldown(session_id, cfg.review.cooldown_mins)
        .unwrap_or(false)
    {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::CooldownActive.as_str(),
            &format!("cooldown {} menit masih aktif untuk sesi ini", cfg.review.cooldown_mins),
        );
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

/// Rakit masukan review yang hemat token, teredaksi, & sadar-cache:
/// - Awalan `Role::System` memakai `FROZEN_REVIEW_SYSTEM_PROMPT` (byte-identik lintas sesi agar hit cache).
/// - Ekor `Role::User` dibatasi agar total panggilan selalu tunduk pada `min(max_tokens, 5.000)` token/panggilan.
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
    let mut pruned = messages.to_vec();
    let _ = prune_old_tool_results(&mut pruned, 2);

    // Pagu input per panggilan tunduk pada min(max_tokens - 400, MAX_REVIEW_CALL_INPUT_TOKENS - 300)
    // sehingga total input panggilan review tidak pernah melewati 5.000 token/panggilan.
    let call_input_cap = max_tokens
        .saturating_sub(400)
        .min(MAX_REVIEW_CALL_INPUT_TOKENS.saturating_sub(300))
        .max(1_000) as usize;
    let sys_est_tokens = FROZEN_REVIEW_SYSTEM_PROMPT.chars().count().div_ceil(3);
    let user_token_budget = call_input_cap.saturating_sub(sys_est_tokens).max(600);
    let total_char_budget = user_token_budget.saturating_mul(3).clamp(1_600, 10_500);
    let transcript_budget = total_char_budget * 55 / 100;
    let skills_budget = total_char_budget * 28 / 100;
    let mem_budget = total_char_budget * 17 / 100;

    let mut read_skill_paths: HashSet<PathBuf> = HashSet::new();
    let mut transcript = String::new();
    let mut mentioned_skills: HashSet<String> = HashSet::new();
    for m in &pruned {
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
            let raw_line = match b {
                Block::Text(t) => {
                    let s: String = t.chars().take(480).collect();
                    format!("[{role}] {s}\n")
                }
                Block::Image { media_type, .. } => format!("[{role}] [image:{media_type}]\n"),
                Block::ToolUse { name, args, .. } => {
                    if name == "skill_view" || name == "skill_edit" || name == "manage_skill" {
                        if let Some(sname) = args.get("name").and_then(|v| v.as_str()) {
                            mentioned_skills.insert(sname.to_ascii_lowercase());
                        }
                    }
                    let a: String = args.to_string().chars().take(220).collect();
                    format!("[CALL] {name} {a}\n")
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
                        .take(200)
                        .collect();
                    let tag = if *is_error { "ERR" } else { "OK" };
                    format!("[RESULT {tag}] {first_two}\n")
                }
            };
            let safe_line = redact_secrets(&raw_line);
            if transcript.chars().count() + safe_line.chars().count() > transcript_budget {
                break;
            }
            transcript.push_str(&safe_line);
        }
    }

    let transcript_lower = transcript.to_ascii_lowercase();
    let scan = scan_skills_with_warnings(cwd);
    let mut skills_section = if scan.skills.is_empty() {
        "(belum ada skill)".to_string()
    } else {
        let mut idx_txt = redact_secrets(&format_skills_index(&scan.skills));
        if idx_txt.chars().count() > skills_budget / 2 {
            idx_txt = idx_txt.chars().take(skills_budget / 2).collect();
        }
        idx_txt
    };

    // Tambahkan isi skill relevan (yang dibuka atau disebut di transkrip, maks 3 skill)
    // sekaligus menandai path-nya sebagai baru dibaca segar (read-before-write).
    let mut relevant_bodies = String::new();
    let mut loaded = 0usize;
    for s in &scan.skills {
        if loaded >= 3 {
            break;
        }
        let sname_low = s.name.to_ascii_lowercase();
        let is_relevant = mentioned_skills.contains(&sname_low)
            || transcript_lower.contains(&sname_low)
            || s.tags
                .iter()
                .any(|t| t.len() >= 3 && transcript_lower.contains(&t.to_ascii_lowercase()));
        if !is_relevant {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&s.path) {
            let canon = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
            read_skill_paths.insert(canon);
            read_skill_paths.insert(s.path.clone());
            let safe = redact_secrets(&raw);
            let cut: String = safe.chars().take(1200).collect();
            let origin_tag = if s.is_improvement_skill() {
                "selena-improvement"
            } else {
                s.scope
            };
            let block = format!("\n--- SKILL `{}` ({origin_tag}) ---\n{}\n", s.name, cut);
            if skills_section.chars().count() + relevant_bodies.chars().count() + block.chars().count()
                <= skills_budget + 800
            {
                relevant_bodies.push_str(&block);
                loaded += 1;
            }
        }
    }
    if !relevant_bodies.is_empty() {
        skills_section.push_str("\nIsi skill relevan:");
        skills_section.push_str(&relevant_bodies);
    }

    let cwd_str = cwd.to_string_lossy().to_string();
    let mem_dir = crate::user_memory::user_memory_dir();
    let persistent_mems = crate::user_memory::load_entries_from(
        &mem_dir,
        crate::user_memory::MemoryStoreKind::Memory,
    );
    let legacy_mems = store.list_memories(&cwd_str, 30).unwrap_or_default();
    let mut mem_section = String::new();
    if persistent_mems.is_empty() && legacy_mems.is_empty() {
        mem_section.push_str("(belum ada catatan memory)");
    } else {
        for (idx, content) in persistent_mems.iter().enumerate() {
            let line = redact_secrets(&format!("- #{} [memory] {}\n", idx + 1, content));
            if mem_section.chars().count() + line.chars().count() > mem_budget {
                break;
            }
            mem_section.push_str(&line);
        }
        for m in &legacy_mems {
            let scope = if m.scope == "global" {
                "global"
            } else {
                "project"
            };
            let line = redact_secrets(&format!(
                "- #{} [{} | {}] {}\n",
                m.id, m.category, scope, m.content
            ));
            if mem_section.chars().count() + line.chars().count() > mem_budget {
                break;
            }
            mem_section.push_str(&line);
        }
    }

    let user = format!(
        "=== INDEKS & SKILL RELEVAN ===\n{skills_section}\n\n\
         === CATATAN MEMORY SAAT INI ===\n{mem_section}\n\n\
         === TRANSKRIP SESI (PRUNED) ===\n{transcript}\n\n\
         Analisa sesi ini dan kembalikan HANYA JSON {{\"actions\":[...]}}."
    );

    (FROZEN_REVIEW_SYSTEM_PROMPT.to_string(), user, read_skill_paths)
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
        skill: String,
        #[serde(default)]
        description: String,
        body: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        reason: String,
    },
    PatchSkill {
        skill: String,
        #[serde(default)]
        old_snippet: Option<String>,
        #[serde(default)]
        new_snippet: Option<String>,
        #[serde(default)]
        append_section: Option<String>,
        #[serde(default)]
        reason: String,
    },
    AddSkillReference {
        skill: String,
        ref_path: String,
        content: String,
        #[serde(default)]
        reason: String,
    },
    AddMemory {
        #[serde(default = "default_general_cat")]
        category: String,
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

/// Jalankan Selena Improvement (v0.5.24) secara aman, sadar-cache, & non-blocking.
/// - Tidak pernah mengembalikan error fatal ke pengguna.
/// - Memakai awalan system prompt beku (`FROZEN_REVIEW_SYSTEM_PROMPT`) + mencatat `cache_read_tokens`.
/// - Skill BARU lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md` dengan `origin: selena-improvement`.
/// - Hanya skill milik Selena improvement yang boleh ditambal langsung (wajib read-before-write + backup putar balik).
/// - Setiap perubahan atas skill karya pengguna (tambal/timpa/hapus) SELALU ditahan ke staging `review_proposals`.
pub async fn run_background_review(
    provider: Arc<dyn Provider>,
    cfg: &Config,
    store: &Store,
    cwd: &Path,
    session_id: &str,
    messages: &[Message],
    mark_delivered: bool,
) -> Result<Option<BgReviewOutcome>> {
    if check_review_eligibility(cfg, store, session_id, messages).is_err() {
        return Ok(None);
    }

    let max_tokens = cfg.review.max_tokens.max(1000);
    let (sys, user, read_skill_paths) =
        build_pruned_review_prompt_with_reads(cwd, store, messages, max_tokens);
    let review_msgs = vec![
        Message::text(Role::System, sys),
        Message::text(Role::User, user),
    ];

    // Guard anggaran token sebelum memanggil provider: patuhi pagu per-review DAN 5.000 token/panggilan.
    let est = analyze_token_breakdown(&review_msgs, &[], 0).total_tokens() as u64;
    let call_cap = max_tokens.min(MAX_REVIEW_CALL_INPUT_TOKENS);
    if est > call_cap || est.saturating_add(200) > max_tokens {
        let _ = store.set_last_review_skip(
            ReviewSkipReason::BudgetExceeded.as_str(),
            &format!("estimasi input {est} > pagu {call_cap} (max_tokens={max_tokens})"),
        );
        eprintln!(
            "[selena-improvement] dilewati: estimasi input ({est}) melampaui pagu ({call_cap})"
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

    let model_name = provider.model_name().to_string();
    let usage = reply.usage.unwrap_or(Usage {
        input_tokens: est,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    });
    let (in_tok, out_tok, cache_read, cache_write) = (
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );

    // Catat biaya review TERPISAH di tabel `usage` (`kind='review'`, `session_id="review:<sid>"`) lengkap dengan cache!
    let _ = store.record_review_usage_full(session_id, &model_name, usage);

    // Jika total token aktual melewati anggaran keras (> 125% max_tokens), catat run & berhenti diam-diam.
    let cwd_str = cwd.to_string_lossy().to_string();
    if in_tok.saturating_add(out_tok) > max_tokens.saturating_mul(5) / 4 {
        eprintln!(
            "[selena-improvement] anggaran token terlampaui ({} > {}), menghentikan evaluasi diam-diam",
            in_tok + out_tok,
            max_tokens
        );
        let _ = store.record_review_run_full(
            session_id,
            &cwd_str,
            &model_name,
            "",
            true,
            in_tok,
            out_tok,
            cache_read,
            cache_write,
        );
        return Ok(None);
    }

    let Some(json_str) = extract_json_object(&reply.text) else {
        eprintln!("[selena-improvement] output model bukan JSON valid (diam)");
        let _ = store.record_review_run_full(
            session_id,
            &cwd_str,
            &model_name,
            "",
            true,
            in_tok,
            out_tok,
            cache_read,
            cache_write,
        );
        return Ok(None);
    };

    let parsed: ReviewPlanResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[selena-improvement] gagal parse JSON review (diam): {e}");
            let _ = store.record_review_run_full(
                session_id,
                &cwd_str,
                &model_name,
                "",
                true,
                in_tok,
                out_tok,
                cache_read,
                cache_write,
            );
            return Ok(None);
        }
    };

    let mut applied: Vec<AppliedReviewChange> = Vec::new();
    let mut staged_ids: Vec<i64> = Vec::new();
    let mut staged_kinds: Vec<String> = Vec::new();
    let mut backup_ids: Vec<i64> = Vec::new();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

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
                            "path": ex.path.to_string_lossy(),
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
                            &format!("Perbarui skill pengguna '{clean_name}' ({rsn})"),
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
                    let bid = store
                        .record_review_backup(
                            session_id,
                            &cwd_str,
                            &clean_name,
                            &ex.path.to_string_lossy(),
                            &old_raw,
                            &diff,
                            &rsn,
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
                        });
                    }
                    continue;
                }

                // Skill BARU karya Selena improvement wajib lahir di `<user_skills_dir>/improvement/<nama>/SKILL.md`
                // dengan frontmatter `origin: selena-improvement`, sesi sumber, & tanggal (lintas sesi!).
                let base_root = user_skills_dir()
                    .unwrap_or_else(|| crate::skills::project_skills_dir(cwd));
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

                if let (Some(old_sn), Some(new_sn)) = (
                    old_snippet.as_deref().map(str::trim).filter(|s| !s.is_empty()),
                    new_snippet.as_deref().map(str::trim),
                ) {
                    if old_body.contains(old_sn) {
                        let safe_new = redact_secrets(new_sn);
                        let candidate = old_body.replacen(old_sn, &safe_new, 1);
                        let old_lines = old_body.lines().count().max(1);
                        let new_lines = candidate.lines().count();
                        // Guard: menghapus >50% baris isi skill lama dianggap timpa/destruktif -> STAGING!
                        if new_lines * 2 < old_lines {
                            destructive_overwrite = true;
                            patched_body = Some(candidate);
                        } else if candidate != old_body {
                            patched_body = Some(candidate);
                        }
                    } else if !new_sn.is_empty() {
                        if append_section
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .is_none()
                        {
                            let safe_append = redact_secrets(new_sn);
                            patched_body = Some(format!("{old_body}\n\n{safe_append}"));
                        }
                    }
                }

                if patched_body.is_none() {
                    if let Some(app) = append_section
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        let safe_app = redact_secrets(app);
                        if !old_body.contains(&safe_app) {
                            patched_body = Some(format!("{old_body}\n\n{safe_app}"));
                        }
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

                // Perlindungan skill karya pengguna (3.4 #2) + read-before-write:
                // - Skill BUKAN milik `selena-improvement` -> SELALU tahan ke staging `/review approve`!
                // - Skill belum dibaca segar di review ini atau destruktif (>50% hilang) -> tahan ke staging!
                let must_stage =
                    !info.is_improvement_skill() || destructive_overwrite || !was_freshly_read;
                if must_stage {
                    let payload = serde_json::json!({
                        "skill": clean_name,
                        "path": info.path.to_string_lossy(),
                        "new_markdown": new_md,
                        "diff": diff,
                        "reason": rsn,
                        "read_before_write": was_freshly_read,
                        "user_owned": !info.is_improvement_skill(),
                    })
                    .to_string();
                    let label = if !info.is_improvement_skill() {
                        format!("Perbarui skill pengguna '{clean_name}' ({rsn})")
                    } else {
                        format!("Timpa skill '{clean_name}' ({rsn})")
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
                    let bid = store
                        .record_review_backup(
                            session_id,
                            &cwd_str,
                            &clean_name,
                            &info.path.to_string_lossy(),
                            &old_raw,
                            &diff,
                            &rsn,
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
                    let payload = serde_json::json!({
                        "skill": clean_name,
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
                        &format!("Referensi '{norm_ref}' pada skill '{clean_name}' ({rsn})"),
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
                let payload = serde_json::json!({
                    "skill": clean_name,
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
                    &format!("Timpa skill '{clean_name}' ({rsn})"),
                    &payload,
                ) {
                    staged_ids.push(pid);
                    staged_kinds.push("timpa skill".to_string());
                }
            }
        }
    }

    let report_message = format_review_report(&applied, &staged_ids, &staged_kinds);
    let run_id = store
        .record_review_run_full(
            session_id,
            &cwd_str,
            &model_name,
            report_message.as_deref().unwrap_or(""),
            mark_delivered,
            in_tok,
            out_tok,
            cache_read,
            cache_write,
        )
        .ok();

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
        match ch.kind.as_str() {
            "create_skill" => {
                parts.push(format!("Skill '{}' created", ch.target));
            }
            "patch_skill" => {
                parts.push(format!("Skill '{}' patched", ch.target));
            }
            "add_skill_reference" => {
                let sname = ch.target.split('/').next().unwrap_or(&ch.target);
                parts.push(format!("Skill '{sname}' patched"));
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
