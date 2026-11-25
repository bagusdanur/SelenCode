//! Self-improvement review otonom pasca-giliran (ala Hermes Agent).
//!
//! Sesudah giliran kerja yang substansial (>= 3 panggilan tool), Selena meninjau
//! singkat apa yang baru dikerjakan: apakah ada workflow reusable, pola bug-fix,
//! atau perbaikan terhadap skill yang sudah ada di `.selencode/skills/`.
//! - Bila layak -> memanggil `manage_skill` (`create` atau `patch`) dan
//!   mengembalikan ringkasan ala Hermes:
//!   `Self-improvement review: Skill '<nama>' patched` (atau `created`).
//! - Bila tugas biasa / sepele -> model menjawab `NONE` tanpa memanggil tool,
//!   sehingga tidak ada notifikasi yang mengganggu user.

use crate::providers::{Block, Message, Provider, Role, StreamEvent};
use crate::skills::list_skills;
use crate::tools::skill::ManageSkill;
use crate::tools::{Ctx, Tool};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Jumlah minimum panggilan tool dalam 1 giliran sebelum review otomatis dipicu.
/// Dinaikkan ke 6 agar tugas rutin 3–5 tool tidak membakar 1 panggilan LLM ekstra (~2.900 token)
/// hanya untuk menjawab `NONE`.
pub const AUTO_REVIEW_MIN_TOOLS: usize = 6;

const MAX_DIGEST_BYTES: usize = 6 * 1024;

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

/// Jalankan 1 kali review self-improvement terhadap riwayat `messages`.
/// Mengembalikan `Ok(Some("Self-improvement review: Skill '<nama>' created|patched"))`
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

    let sys = "Kamu adalah modul self-improvement review milik SelenCode (seperti Hermes Agent).\n\
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
    let breakdown = crate::agent::context::analyze_token_breakdown(&review_msgs, &specs, 1);
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
        // Catat juga ke `call_token_metrics` agar jumlah baris selalu 100% sinkron dengan tabel `usage`!
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
            // out format: "Skill '<name>' created|patched (...)."
            let short = out
                .split(" (")
                .next()
                .unwrap_or(&out)
                .trim_end_matches('.');
            events.push(format!("Self-improvement review: {short}"));
        }
    }

    if events.is_empty() {
        Ok(None)
    } else {
        Ok(Some(events.join(" · ")))
    }
}
