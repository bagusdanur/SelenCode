//! Tahap 3 — Dynamic Model Routing (vendor-independen) + Fallback + Retry + Telemetry.
//!
//! Lapisan routing ini mengimplementasikan trait `Provider` sehingga loop agent
//! inti tetap 100% independen dari vendor tertentu:
//! - Klasifikasi jenis tugas otomatis (`coding`, `debugging`, `reasoning`, `review`, `summary`, `general`)
//! - Pemilihan kandidat berdasarkan aturan tugas, kemampuan tool calling, kecocokan konteks,
//!   biaya, latensi, dan kesehatan provider (TIDAK berasumsi konteks terbesar selalu terbaik)
//! - Tetap menghormati pilihan model manual pengguna (saat `auto = false`, primary selalu dicoba dulu)
//! - Retry terbatas untuk error sementara (429 rate-limit, timeout, 5xx) + fallback otomatis ke provider cadangan
//! - Pemantauan metrik latensi, biaya, token, retry, fallback, & error per rute (`/route`).

use crate::agent::context::{estimate_tokens, infer_model_context_limit};
use crate::providers::{AssistantReply, Message, Provider, Role, StreamEvent, ToolSpec, Usage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Jenis tugas hasil klasifikasi permintaan aktif.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Coding,
    Debugging,
    Reasoning,
    Review,
    Summary,
    General,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Coding => "coding",
            TaskKind::Debugging => "debugging",
            TaskKind::Reasoning => "reasoning",
            TaskKind::Review => "review",
            TaskKind::Summary => "summary",
            TaskKind::General => "general",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "coding" | "code" | "kode" => Some(TaskKind::Coding),
            "debugging" | "debug" | "fix" => Some(TaskKind::Debugging),
            "reasoning" | "reason" | "plan" | "arch" => Some(TaskKind::Reasoning),
            "review" | "audit" => Some(TaskKind::Review),
            "summary" | "summarize" | "ringkas" => Some(TaskKind::Summary),
            "general" | "umum" => Some(TaskKind::General),
            _ => None,
        }
    }
}

/// Klasifikasi deterministik jenis tugas dari pesan terakhir & keberadaan tool.
pub fn classify_task(messages: &[Message], _tools: &[ToolSpec]) -> TaskKind {
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.plain_text().to_ascii_lowercase())
        .unwrap_or_default();

    if last_user.is_empty() {
        return TaskKind::General;
    }

    // Prioritas 1: Debugging / perbaikan error / test gagal
    if contains_any(
        &last_user,
        &[
            "error",
            "panic",
            "failed",
            "gagal",
            "bug",
            "debug",
            "perbaiki",
            "benerin",
            "traceback",
            "stack trace",
            "cannot borrow",
            "exception",
            "crash",
            "fix ",
        ],
    ) {
        return TaskKind::Debugging;
    }

    // Prioritas 2: Review / audit
    if contains_any(
        &last_user,
        &[
            "review",
            "tinjau",
            "audit",
            "cek keamanan",
            "security check",
            "code smell",
            "inspeksi diff",
        ],
    ) {
        return TaskKind::Review;
    }

    // Prioritas 3: Ringkasan / penjelasan singkat
    if contains_any(
        &last_user,
        &[
            "ringkas",
            "rangkum",
            "summarize",
            "summary",
            "tl;dr",
            "jelaskan singkat",
            "singkat saja",
        ],
    ) {
        return TaskKind::Summary;
    }

    // Prioritas 4: Reasoning / arsitektur / perencanaan
    if contains_any(
        &last_user,
        &[
            "arsitektur",
            "architecture",
            "desain",
            "design",
            "rencana",
            "plan",
            "analisis",
            "bandingkan",
            "tradeoff",
            "strategi",
            "roadmap",
        ],
    ) {
        return TaskKind::Reasoning;
    }

    // Prioritas 5: Implementasi / coding
    if contains_any(
        &last_user,
        &[
            "buat ",
            "bikin ",
            "tambahkan",
            "tambah ",
            "implement",
            "refactor",
            "ubah ",
            "edit ",
            "tulis ",
            "kode",
            "fungsi",
            "struct",
            "class",
            "endpoint",
            "fitur",
            "test",
            "kerjakan",
        ],
    ) {
        return TaskKind::Coding;
    }

    TaskKind::General
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Deteksi apakah pesan error API termasuk kegagalan sementara yang layak di-retry / fallback.
pub fn is_retryable_error(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    contains_any(
        &low,
        &[
            "429",
            "rate limit",
            "rate_limit",
            "too many requests",
            "timeout",
            "timed out",
            "500",
            "502",
            "503",
            "504",
            "overloaded",
            "temporarily unavailable",
            "connection reset",
            "connection refused",
            "broken pipe",
            "bad gateway",
            "service unavailable",
        ],
    )
}

pub fn is_rate_limit_error(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("429") || low.contains("rate limit") || low.contains("rate_limit") || low.contains("too many requests")
}

/// Satu kandidat rute (provider + model + kapabilitas + harga opsional).
#[derive(Clone)]
pub struct ModelRoute {
    pub id: String,
    pub provider_name: String,
    pub model: String,
    pub provider: Arc<dyn Provider>,
    pub supports_tools: bool,
    pub context_limit: usize,
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub is_primary: bool,
}

impl ModelRoute {
    pub fn new(
        provider_name: impl Into<String>,
        model: impl Into<String>,
        provider: Arc<dyn Provider>,
        context_limit: Option<usize>,
        price_in: Option<f64>,
        price_out: Option<f64>,
        is_primary: bool,
    ) -> Self {
        let p = provider_name.into();
        let m = model.into();
        let ctx = context_limit.unwrap_or_else(|| infer_model_context_limit(&m));
        Self {
            id: format!("{p}:{m}"),
            provider_name: p,
            model: m,
            provider,
            supports_tools: true,
            context_limit: ctx,
            price_in,
            price_out,
            is_primary,
        }
    }
}

/// Metrik latensi, biaya, token, dan error untuk satu rute.
#[derive(Debug, Clone, Default)]
pub struct RouteMetrics {
    pub route_id: String,
    pub calls: u64,
    pub successes: u64,
    pub errors: u64,
    pub rate_limits: u64,
    pub retries: u64,
    pub fallbacks_used: u64,
    pub total_latency_ms: u64,
    pub last_latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub has_cost_data: bool,
    pub last_error: Option<String>,
    pub last_task: Option<String>,
    pub cooldown_until: Option<Instant>,
}

impl RouteMetrics {
    pub fn avg_latency_ms(&self) -> u64 {
        if self.successes == 0 {
            0
        } else {
            self.total_latency_ms / self.successes
        }
    }

    pub fn in_cooldown(&self) -> bool {
        self.cooldown_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }
}

/// Penyimpan metrik bersama lintas panggilan router.
#[derive(Debug, Default)]
pub struct RouterTelemetry {
    pub routes: HashMap<String, RouteMetrics>,
    pub last_selected_route: Option<String>,
    pub last_task: Option<TaskKind>,
    pub total_fallbacks: u64,
    pub total_retries: u64,
}

impl RouterTelemetry {
    pub fn record_success(
        &mut self,
        route: &ModelRoute,
        task: TaskKind,
        latency_ms: u64,
        usage: Option<Usage>,
        was_fallback: bool,
    ) {
        let m = self
            .routes
            .entry(route.id.clone())
            .or_insert_with(|| RouteMetrics {
                route_id: route.id.clone(),
                ..Default::default()
            });
        m.calls += 1;
        m.successes += 1;
        m.total_latency_ms += latency_ms;
        m.last_latency_ms = latency_ms;
        m.last_task = Some(task.as_str().to_string());
        m.cooldown_until = None;
        if was_fallback {
            m.fallbacks_used += 1;
            self.total_fallbacks += 1;
        }
        if let Some(u) = usage {
            m.input_tokens += u.input_tokens;
            m.output_tokens += u.output_tokens;
            if let (Some(pin), Some(pout)) = (route.price_in, route.price_out) {
                let cost = (u.input_tokens as f64 / 1_000_000.0) * pin
                    + (u.output_tokens as f64 / 1_000_000.0) * pout;
                m.estimated_cost_usd += cost;
                m.has_cost_data = true;
            }
        }
        self.last_selected_route = Some(route.id.clone());
        self.last_task = Some(task);
    }

    pub fn record_error(
        &mut self,
        route: &ModelRoute,
        task: TaskKind,
        err: &str,
        is_retry: bool,
    ) {
        let m = self
            .routes
            .entry(route.id.clone())
            .or_insert_with(|| RouteMetrics {
                route_id: route.id.clone(),
                ..Default::default()
            });
        m.calls += 1;
        m.errors += 1;
        if is_retry {
            m.retries += 1;
            self.total_retries += 1;
        }
        if is_rate_limit_error(err) {
            m.rate_limits += 1;
            m.cooldown_until = Some(Instant::now() + Duration::from_secs(45));
        } else if is_retryable_error(err) {
            m.cooldown_until = Some(Instant::now() + Duration::from_secs(15));
        }
        m.last_error = Some(err.chars().take(120).collect());
        m.last_task = Some(task.as_str().to_string());
    }

    pub fn snapshot_routes(&self) -> Vec<RouteMetrics> {
        let mut out: Vec<RouteMetrics> = self.routes.values().cloned().collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.route_id.cmp(&b.route_id)));
        out
    }
}

/// Skor kecocokan heuristik satu kandidat rute terhadap `task` (tanpa bias konteks terbesar!).
fn score_candidate(
    route: &ModelRoute,
    task: TaskKind,
    needed_tokens: usize,
    needs_tools: bool,
    metrics: Option<&RouteMetrics>,
) -> i32 {
    let mut score = 0i32;

    if route.is_primary {
        score += 20;
    }

    // Kecocokan tool calling & kebutuhan konteks nyata (bukan makin besar makin bagus).
    if needs_tools && !route.supports_tools {
        score -= 100;
    }
    if needed_tokens > 0 && route.context_limit < needed_tokens {
        score -= 60;
    }

    let m = route.model.to_ascii_lowercase();
    let is_fast_cheap = contains_any(&m, &["mini", "flash", "haiku", "nano", "small", "lite", "instant"]);
    let is_deep_reason = contains_any(&m, &["o1", "o3", "r1", "reasoner", "thinking", "opus", "pro", "sonnet"]);
    let is_coder = contains_any(&m, &["coder", "codex", "sonnet", "claude", "gpt-4", "gpt-5", "deepseek", "qwen"]);

    match task {
        TaskKind::Summary => {
            if is_fast_cheap {
                score += 30;
            }
            if let Some(pin) = route.price_in {
                if pin <= 1.0 {
                    score += 12;
                }
            }
        }
        TaskKind::Reasoning | TaskKind::Review => {
            if is_deep_reason {
                score += 28;
            }
            if is_fast_cheap {
                score -= 10;
            }
        }
        TaskKind::Coding | TaskKind::Debugging => {
            if is_coder {
                score += 25;
            }
            if is_deep_reason {
                score += 12;
            }
        }
        TaskKind::General => {}
    }

    // Kesehatan & latensi nyata dari telemetri: rute dalam cooldown/error tinggi diturunkan!
    if let Some(met) = metrics {
        if met.in_cooldown() {
            score -= 80;
        }
        if met.errors > met.successes && met.calls >= 2 {
            score -= 25;
        }
        let avg = met.avg_latency_ms();
        if avg > 0 && avg < 2500 {
            score += 6;
        } else if avg > 15_000 {
            score -= 10;
        }
    }

    score
}

/// Provider wrapper yang mengorkestrasi Dynamic Model Routing, Retry terbatas, & Fallback.
pub struct DynamicRouterProvider {
    pub routes: Vec<ModelRoute>,
    pub auto_routing: bool,
    pub task_routes: HashMap<String, String>,
    pub max_retries: u8,
    pub retry_backoff_ms: u64,
    pub telemetry: Arc<Mutex<RouterTelemetry>>,
}

impl DynamicRouterProvider {
    pub fn new(
        routes: Vec<ModelRoute>,
        auto_routing: bool,
        task_routes: HashMap<String, String>,
        max_retries: u8,
        telemetry: Arc<Mutex<RouterTelemetry>>,
    ) -> Self {
        Self {
            routes,
            auto_routing,
            task_routes,
            max_retries: max_retries.min(3),
            retry_backoff_ms: 400,
            telemetry,
        }
    }

    pub fn with_backoff_ms(mut self, ms: u64) -> Self {
        self.retry_backoff_ms = ms;
        self
    }

    /// Tentukan urutan kandidat rute untuk giliran saat ini.
    pub fn plan_candidates(&self, msgs: &[Message], tools: &[ToolSpec]) -> (TaskKind, Vec<ModelRoute>) {
        let task = classify_task(msgs, tools);
        let needed_tokens = estimate_tokens(msgs).saturating_add(4096);
        let needs_tools = !tools.is_empty();

        if self.routes.len() <= 1 {
            return (task, self.routes.clone());
        }

        let tel = self.telemetry.lock().ok();

        // Mode manual (`auto_routing == false`): hormati urutan primary lalu fallback,
        // hanya menunda kandidat yang sedang cooldown rate-limit jika ada cadangan sehat.
        if !self.auto_routing {
            let mut healthy = Vec::new();
            let mut cooled = Vec::new();
            for r in &self.routes {
                let in_cd = tel
                    .as_ref()
                    .and_then(|t| t.routes.get(&r.id))
                    .map(|m| m.in_cooldown())
                    .unwrap_or(false);
                if in_cd {
                    cooled.push(r.clone());
                } else {
                    healthy.push(r.clone());
                }
            }
            healthy.extend(cooled);
            return (task, healthy);
        }

        // Mode dinamis (`auto_routing == true`):
        // 1. Cek aturan eksplisit `task_routes[task]` terlebih dulu.
        let explicit_target = self.task_routes.get(task.as_str()).map(|s| s.trim());
        let mut scored: Vec<(i32, usize, ModelRoute)> = self
            .routes
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let met = tel.as_ref().and_then(|t| t.routes.get(&r.id));
                let mut sc = score_candidate(r, task, needed_tokens, needs_tools, met);
                if let Some(target) = explicit_target {
                    if r.id.eq_ignore_ascii_case(target)
                        || r.provider_name.eq_ignore_ascii_case(target)
                        || r.model.eq_ignore_ascii_case(target)
                    {
                        sc += 200;
                    }
                }
                (sc, idx, r.clone())
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        (task, scored.into_iter().map(|(_, _, r)| r).collect())
    }
}

#[async_trait]
impl Provider for DynamicRouterProvider {
    async fn stream_chat(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<AssistantReply> {
        let (task, candidates) = self.plan_candidates(msgs, tools);
        if candidates.is_empty() {
            return Err(anyhow!("tidak ada provider/rute yang tersedia di DynamicRouterProvider"));
        }

        let mut errors: Vec<String> = Vec::new();

        for (cand_idx, route) in candidates.iter().enumerate() {
            let max_attempts = (self.max_retries as usize) + 1;
            for attempt in 0..max_attempts {
                let started = Instant::now();
                let (inner_tx, mut inner_rx) = mpsc::channel::<StreamEvent>(128);
                let out_tx = tx.clone();

                // Teruskan token streaming secara langsung & catat apakah sudah ada token terkirim.
                let forwarder = tokio::spawn(async move {
                    let mut emitted = false;
                    while let Some(ev) = inner_rx.recv().await {
                        emitted = true;
                        let _ = out_tx.send(ev).await;
                    }
                    emitted
                });

                let res = route.provider.stream_chat(msgs, tools, inner_tx).await;
                let emitted_any = forwarder.await.unwrap_or(false);
                let elapsed_ms = started.elapsed().as_millis() as u64;

                match res {
                    Ok(reply) => {
                        if let Ok(mut tel) = self.telemetry.lock() {
                            tel.record_success(route, task, elapsed_ms, reply.usage, cand_idx > 0);
                        }
                        return Ok(reply);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let retryable = is_retryable_error(&msg);
                        let will_retry = retryable && !emitted_any && attempt + 1 < max_attempts;
                        if let Ok(mut tel) = self.telemetry.lock() {
                            tel.record_error(route, task, &msg, will_retry);
                        }
                        errors.push(format!("{} (percobaan {}): {}", route.id, attempt + 1, msg));
                        if will_retry {
                            if self.retry_backoff_ms > 0 {
                                let delay = self.retry_backoff_ms * (attempt as u64 + 1);
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                            }
                            continue;
                        }
                        // Bila token sudah terlanjur mengalir ke UI, jangan tumpuk jawaban ganda dari fallback.
                        if emitted_any {
                            return Err(anyhow!(
                                "stream terputus di tengah jawaban ({}): {}",
                                route.id,
                                msg
                            ));
                        }
                        break; // pindah ke provider fallback berikutnya!
                    }
                }
            }
        }

        Err(anyhow!(
            "semua rute provider gagal (tugas: {}):\n- {}",
            task.as_str(),
            errors.join("\n- ")
        ))
    }

    fn model_name(&self) -> &str {
        self.routes
            .iter()
            .find(|r| r.is_primary)
            .or_else(|| self.routes.first())
            .map(|r| r.model.as_str())
            .unwrap_or("")
    }
}
