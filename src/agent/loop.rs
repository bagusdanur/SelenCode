use crate::providers::{Block, Message, Provider, Role, StreamEvent, Usage};
use crate::security::redact_secrets;
use crate::tools::{Ctx, Registry};
use anyhow::Result;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::context::{
    auto_outline, build_system_prompt, compact_messages, estimate_tokens, expand_at_mentions,
    format_memories_section, needs_compaction, prune_old_tool_results,
};
use super::corrector::{LoopGuard, SelfCorrectState};
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

fn persist_usage(ctx: &Ctx, model: &str, usage: Option<Usage>) {
    if let (Some(store), Some(sid), Some(u)) = (&ctx.store, &ctx.session_id, usage) {
        if let Err(e) = store.record_usage(sid, model, u.input_tokens, u.output_tokens) {
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
    // Progressive Disclosure (Hermes-style toolsets):
    // Hanya kirim Core Toolset + toolset yang cocok dengan intent/workspace, bukan seluruh 29 schema!
    // Model dapat membuka toolset lain kapan saja lewat `activate_toolset`.
    let has_mcp = registry.names().iter().any(|n| n.starts_with("mcp__"));
    let mut active_toolsets =
        crate::tools::toolsets::select_initial_toolsets(user_prompt, &ctx.cwd, has_mcp, mode);
    let mut tools = crate::tools::toolsets::active_tool_specs(registry, &active_toolsets, mode);
    if messages.is_empty() {
        let mut sys = build_system_prompt(&ctx.cwd, &tools);
        let hint = crate::tools::toolsets::inactive_toolsets_hint(registry, &active_toolsets);
        if !hint.is_empty() {
            sys.push('\n');
            sys.push_str(&hint);
            sys.push('\n');
        }
        if let Some(store) = &ctx.store {
            let cwd_str = ctx.cwd.to_string_lossy().to_string();
            if let Ok(mems) = store.list_memories(&cwd_str, 15) {
                let mem_sec = format_memories_section(&mems);
                if !mem_sec.is_empty() {
                    sys.push_str(&mem_sec);
                }
            }
        }
        let sys_msg = Message::text(Role::System, sys);
        persist_message(ctx, &sys_msg);
        messages.push(sys_msg);
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
    let mut loop_guard = LoopGuard::new();
    let mut corrector = SelfCorrectState::new(turn_checkpoint_id);

    for step in 0..max_steps {
        // Diet konteks (Live-Test P1): padatkan hasil tool lama sebelum mengirim ulang ke provider.
        prune_old_tool_results(messages, 2);
        // Compaction konteks: ringkas riwayat lama bila estimasi token melewati ambang batas.
        if needs_compaction(messages, ctx.context_limit_tokens) {
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::Info("compaction konteks dijalankan".into()))
                        .await;
                }
                None => eprintln!("[selen] compaction konteks dijalankan"),
            }
            *messages = compact_messages(messages);
        }
        if let Some(tx) = &ui {
            let est = estimate_tokens(messages);
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
        // Provider dipindah ke task agar StreamEvent bisa dikonsumsi sambil jalan.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(128);
        let p = provider.clone();
        let mut snapshot = messages.clone();
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
        let tools_c = tools.clone();
        let handle = tokio::spawn(async move { p.stream_chat(&snapshot, &tools_c, tx).await });
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
        let reply = handle
            .await
            .map_err(|e| anyhow::anyhow!("task provider panik: {e}"))??;
        if printed {
            println!();
        }
        persist_usage(ctx, provider.model_name(), reply.usage);
        if let Some(u) = reply.usage {
            turn_usage.input_tokens += u.input_tokens;
            turn_usage.output_tokens += u.output_tokens;
            have_usage = true;
        }
        last_text = reply.text.clone();

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
            // Self-Correction Gate (Tahap 1): bila file kode baru diubah tetapi belum
            // diverifikasi (atau verifikasi terakhir gagal), arahkan agent memverifikasi
            // & memperbaiki sebelum menyatakan selesai (maks MAX_REPAIR_ATTEMPTS siklus).
            if ctx.self_correct && mode == TurnMode::Execute {
                if let Some(gate_msg) = corrector.completion_gate(max_steps.saturating_sub(step + 1)) {
                    let info = format!(
                        "✦ Self-correction ({}/{}): memverifikasi perubahan sebelum selesai…",
                        corrector.repair_attempts,
                        super::corrector::MAX_REPAIR_ATTEMPTS
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
            // Self-improvement review otonom (ala Hermes Agent): bila giliran
            // kerja substansial (>= 3 tool call), evaluasi singkat apakah perlu
            // create/patch skill di `.selencode/skills/`.
            if ctx.auto_skills
                && mode == TurnMode::Execute
                && tool_calls_executed >= super::review::AUTO_REVIEW_MIN_TOOLS
                && !skill_touched_in_turn
            {
                if let Ok(Some(note)) =
                    super::review::run_self_improvement_review(provider.clone(), ctx, messages)
                        .await
                {
                    let msg = format!("✦ {note}");
                    match &ui {
                        Some(tx) => {
                            let _ = tx.send(UiEvent::Info(msg)).await;
                        }
                        None => eprintln!("[selen] {msg}"),
                    }
                }
            }
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
        let mut results: Vec<Block> = Vec::new();
        for call in &reply.tool_calls {
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::ToolStart {
                            name: call.name.clone(),
                            args: summarize(&call.args.to_string(), 120),
                        })
                        .await;
                }
                None => eprintln!("[tool] {} {}", call.name, call.args),
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
                    tools = crate::tools::toolsets::active_tool_specs(
                        registry,
                        &active_toolsets,
                        mode,
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
                } else if mode == TurnMode::Plan && !is_read_only(&call.name) {
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
                                    Ok(out) => (redact_secrets(&truncate(out)), false),
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
            corrector.observe_tool(&call.name, &call.args, &content, is_error);
            if let Some(warn) = loop_guard.record(&call.name, &call.args, is_error).hint_message() {
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
        let tool_msg = Message {
            role: Role::Tool,
            content: results,
        };
        persist_message(ctx, &tool_msg);
        messages.push(tool_msg);

        if step + 1 == max_steps {
            match &ui {
                Some(tx) => {
                    let _ = tx
                        .send(UiEvent::Info(format!(
                            "max_steps ({max_steps}) tercapai — berhenti."
                        )))
                        .await;
                }
                None => eprintln!("[selen] max_steps ({max_steps}) tercapai — berhenti."),
            }
        }
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
