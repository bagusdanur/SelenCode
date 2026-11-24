use crate::providers::{Block, Message, Provider, Role, StreamEvent, Usage};
use crate::security::redact_secrets;
use crate::tools::{Ctx, Registry};
use anyhow::Result;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::context::{
    auto_outline, build_system_prompt, compact_messages, estimate_tokens, needs_compaction,
};
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
    // Plan mode (Fase 6): specs yang dikirim ke provider HANYA tool
    // baca-saja. Penegakan kedua ada di titik eksekusi tool di bawah.
    let tools = match mode {
        TurnMode::Plan => registry.specs_where(is_read_only),
        TurnMode::Execute => registry.specs(),
    };
    if messages.is_empty() {
        let sys = build_system_prompt(&ctx.cwd, &tools);
        let sys_msg = Message::text(Role::System, sys);
        persist_message(ctx, &sys_msg);
        messages.push(sys_msg);
    }
    // Konteks pintar (Fase 5): bila user menyebut path file kode yang ada,
    // lampirkan outline-nya ke pesan user giliran ini (bukan system prompt —
    // system prompt dibuat sekali; konteks per-giliran menempel di giliran).
    // Blok ikut tersimpan di sesi persis seperti yang dilihat provider.
    let mut user_text = user_prompt.to_string();
    let auto = auto_outline(&ctx.cwd, user_prompt);
    if !auto.is_empty() {
        user_text.push_str("\n\n");
        user_text.push_str(&auto);
    }
    let user_msg = Message::text(Role::User, user_text);
    persist_message(ctx, &user_msg);
    messages.push(user_msg);
    let mut last_text = String::new();
    let mut turn_usage = Usage::default();
    let mut have_usage = false;

    for step in 0..max_steps {
        // Compaction konteks (Fase 2): ringkas riwayat lama bila estimasi
        // token melewati 80% batas, sebelum dikirim ke provider.
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
            let pct = if ctx.context_limit_tokens > 0 {
                (estimate_tokens(messages) * 100 / ctx.context_limit_tokens).min(100) as u8
            } else {
                0
            };
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
            if let Some(tx) = &ui {
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
            let (content, is_error) =
                if mode == TurnMode::Plan && !is_read_only(&call.name) {
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
                    match registry.get(&call.name) {
                        Some(tool) => match tool.run(call.args.clone(), ctx).await {
                            Ok(out) => (redact_secrets(&truncate(out)), false),
                            Err(e) => (redact_secrets(&format!("ERROR: {e}")), true),
                        },
                        None => (format!("ERROR: tool '{}' tidak dikenal", call.name), true),
                    }
                };
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
    if let Some(tx) = &ui {
        let _ = tx
            .send(UiEvent::TurnDone {
                usage: have_usage.then_some(turn_usage),
            })
            .await;
    }
    Ok((last_text, have_usage.then_some(turn_usage)))
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

/// Potong hasil tool agar konteks tidak meledak (30KB, aman UTF-8).
fn truncate(s: String) -> String {
    const MAX: usize = 30 * 1024;
    if s.len() <= MAX {
        s
    } else {
        let mut end = MAX;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut cut = s;
        cut.truncate(end);
        cut.push_str("\n... (hasil dipotong 30KB)");
        cut
    }
}
