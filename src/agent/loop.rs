use crate::providers::{Block, Message, Provider, Role, StreamEvent, Usage};
use crate::security::redact_secrets;
use crate::tools::{Ctx, Registry};
use anyhow::Result;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::context::{
    analyze_token_breakdown, auto_outline, build_folder_map, build_resume_snapshot,
    build_system_prompt, cap_dynamic_injection, compact_messages_hermes, estimate_tokens,
    expand_at_mentions, head_tail_truncate, needs_compaction,
    prune_old_skill_results_with_archive, prune_old_tool_results_with_archive,
    session_archive_dir, spillover_if_needed, spillover_with_preview,
};
use crate::tool_display::{
    build_template_closing_summary, format_tool_event_from_preview, prepare_tool_preview,
    short_error_reason, ToolActionEvent,
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
    let mut active_toolsets = if lean_resume {
        crate::tools::toolsets::select_resume_initial_toolsets(user_prompt, &ctx.cwd, has_mcp, mode)
    } else {
        let mut s =
            crate::tools::toolsets::select_initial_toolsets(user_prompt, &ctx.cwd, has_mcp, mode);
        if !installed_skills.is_empty() {
            s.insert("skills_view".to_string());
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

    // Helper lokal untuk menyusun System Prompt + Konteks Sesi (Snapshot Awal) yang identik untuk sesi baru
    // maupun penyegaran snapshot resume ramping.
    let build_fresh_system_prompt = |cur_tools: &[crate::providers::ToolSpec],
                                     cur_active: &std::collections::HashSet<String>,
                                     include_folder_map: bool|
     -> String {
        let mut sys = build_system_prompt(&ctx.cwd, cur_tools);
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
        let skills_idx = crate::skills::format_skills_index_budgeted(&installed_skills, dyn_cap);
        if !skills_idx.is_empty() {
            dynamic_snapshot.push_str(&skills_idx);
            dynamic_snapshot.push('\n');
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
        let sys = build_fresh_system_prompt(&tools, &active_toolsets, true);
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
    let mut corrector = SelfCorrectState::new(turn_checkpoint_id).with_max_attempts(sc_max);
    let sid_for_archive = ctx
        .session_id
        .clone()
        .unwrap_or_else(|| "ephemeral".to_string());
    let prune_dir = session_archive_dir("pruned", &sid_for_archive);
    let mut turn_tool_chars_used = 0usize;
    let scaled_turn_budget = (ctx.tool_budget.turn_budget_chars as u128
        * (ctx.context_limit_tokens.max(20_000) as u128)
        / 100_000)
        .clamp(24_000, 180_000) as usize;
    let mut last_step_deduped = 0usize;
    let mut last_announced_fallback_model: Option<String> = None;
    let mut unconfirmed_search_calls: usize = 0;
    let mut anti_spin_reminded = false;
    let mut turn_actions: Vec<ToolActionEvent> = Vec::new();
    let mut forced_stop_reason: Option<String> = None;

    for step in 0..max_steps {
        // Perbaikan 1 (v0.5.9) — Prune lebih dini hasil `skill_view` lama (di luar 2 pesan terakhir =
        // tepat sesudah model memakainya di 1 giliran berikutnya) ke stub ringkas + arsip byte-exact
        // agar isi skill tidak terbawa penuh di sisa sesi.
        let pruned_skills = prune_old_skill_results_with_archive(messages, &prune_dir, 2);
        // Tahap 1 — Prune hasil tool lama (>200 char di luar 20 pesan terakhir) + arsip ke file (TANPA LLM).
        let pruned_now = pruned_skills + prune_old_tool_results_with_archive(messages, &prune_dir, 20);
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
        }
        // Perbaikan 1 (v0.5.7): untuk sesi RESUME, bangun snapshot ramping (system prompt segar tanpa
        // folder_map ulang + ringkas riwayat tool giliran sebelumnya) tanpa mengubah riwayat mentah di DB!
        let mut snapshot = if lean_resume {
            let fresh_sys = build_fresh_system_prompt(&tools, &active_toolsets, false);
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
        let reply = handle_guard
            .0
            .take()
            .expect("handle provider tersedia")
            .await
            .map_err(|e| anyhow::anyhow!("task provider panik: {e}"))??;
        if printed {
            println!();
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
                if let Some(gate_msg) = corrector.completion_gate_with_checklist(
                    max_steps.saturating_sub(step + 1),
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
        let tool_msg = Message {
            role: Role::Tool,
            content: results,
        };
        persist_message(ctx, &tool_msg);
        messages.push(tool_msg);

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

        if step + 1 == max_steps {
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
        }
    }

    // Perbaikan 2 (v0.5.8): Giliran terpotong (max_steps / stagnasi) TIDAK BOLEH berakhir pada
    // pesan ToolResult kosong tanpa ringkasan penutup untuk pengguna!
    if let Some(stop_reason) = forced_stop_reason {
        let mut summary_msgs = messages.clone();
        summary_msgs.push(Message::text(
            Role::User,
            format!(
                "[Ringkasan Penutup Wajib — {stop_reason}] Giliran berhenti di tengah jalan. \
                 Tulis ringkasan penutup singkat untuk pengguna: (1) yang sudah dikerjakan & hasilnya, \
                 (2) yang sedang berjalan termasuk ID job/proses bila ada, (3) langkah berikutnya atau kendalanya. \
                 Jangan memanggil tool."
            ),
        ));
        let (stx, _srx) = mpsc::channel::<StreamEvent>(64);
        let p_close = provider.clone();
        let closing_res = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            p_close.stream_chat(&summary_msgs, &[], stx),
        )
        .await;
        let closing_text = match closing_res {
            Ok(Ok(rep)) if !rep.text.trim().is_empty() => {
                if let Some(u) = rep.usage {
                    persist_usage_kind(ctx, provider.model_name(), Some(u), "main");
                    turn_usage.input_tokens += u.input_tokens;
                    turn_usage.output_tokens += u.output_tokens;
                    have_usage = true;
                }
                rep.text.trim().to_string()
            }
            _ => build_template_closing_summary(&turn_actions, &stop_reason),
        };
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
