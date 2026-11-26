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

/// Klasifikasi ketat error yang LAYAK memicu fallback ke model/provider cadangan (v0.5.6 Bagian 1):
/// - HANYA: HTTP 429 (kuota/rate limit), HTTP 5xx provider, timeout/koneksi putus,
///   penolakan kredensial/kuota upstream (401/403), dan model tidak ditemukan/mati (404).
/// - TIDAK PERNAH: HTTP 400 / bad request / format rusak, error tool lokal, penolakan approval,
///   pembatalan pengguna, atau error ambigu yang tidak dikenal.
pub fn is_fallback_eligible_error(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    // 1. Tolak tegas HTTP 400 / format rusak / konteks melebihi batas / pembatalan lokal.
    //    Catatan: jangan menolak `"invalid_request"` sebelum mengecek 404/model_not_found/401/403,
    //    karena gateway OpenAI-compatible (termasuk 9Router) membungkus 404 model mati di dalam
    //    `{"type":"invalid_request_error","code":"model_not_found"}`.
    if contains_any(
        &low,
        &[
            "400",
            "bad request",
            "malformed",
            "context_length_exceeded",
            "maximum context length",
            "ditolak user",
            "dibatalkan",
            "cancelled",
            "canceled",
            "approval",
        ],
    ) {
        return false;
    }
    if is_retryable_error(&low) {
        return true;
    }
    // 2. Upstream auth/quota rejection (401/403), kredensial provider tidak aktif, atau model mati/tidak ditemukan (404).
    if contains_any(
        &low,
        &[
            "401",
            "403",
            "unauthorized",
            "forbidden",
            "insufficient_quota",
            "quota exceeded",
            "billing",
            "no active credentials",
            "404",
            "model_not_found",
            "model not found",
            "does not exist",
            "dns error",
            "failed to connect",
            "error sending request",
            "unexpected eof",
        ],
    ) {
        return true;
    }
    // 3. `invalid_request` murni tanpa status fallback yang sah tetap ditolak.
    if contains_any(&low, &["invalid_request", "invalid request"]) {
        return false;
    }
    false
}

/// Ringkas sebab kegagalan utama untuk pesan transparansi fallback 1 baris ke pengguna.
pub fn short_fallback_reason(err: &str) -> &'static str {
    let low = err.to_ascii_lowercase();
    if is_rate_limit_error(&low) || low.contains("quota") {
        "429 kuota"
    } else if contains_any(&low, &["500", "502", "503", "504", "overloaded", "unavailable", "bad gateway"]) {
        "5xx provider"
    } else if contains_any(&low, &["401", "403", "unauthorized", "forbidden"]) {
        "401/403 kredensial"
    } else if contains_any(
        &low,
        &[
            "404",
            "model_not_found",
            "model not found",
            "does not exist",
            "no active credentials",
        ],
    ) {
        "404 model tidak tersedia"
    } else if contains_any(&low, &["timeout", "timed out", "connection", "eof", "dns", "sending request"]) {
        "timeout/koneksi"
    } else {
        "error provider"
    }
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

/// Keputusan routing transparan untuk observabilitas (`/route` & `/route history`).
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub route_id: String,
    pub task: TaskKind,
    pub escalated: bool,
    pub score: i32,
    pub fallback_used: bool,
    pub reason: String,
}

/// Penyimpan metrik bersama lintas panggilan router (termasuk histori performa lintas sesi).
#[derive(Debug, Default)]
pub struct RouterTelemetry {
    pub routes: HashMap<String, RouteMetrics>,
    /// Performa historis lintas sesi & benchmark: `(route_id, task_kind)` -> `(successes, failures)`.
    pub historical: HashMap<(String, String), (u64, u64)>,
    pub last_selected_route: Option<String>,
    pub last_task: Option<TaskKind>,
    pub last_decision: Option<RoutingDecision>,
    pub recent_decisions: Vec<RoutingDecision>,
    pub total_fallbacks: u64,
    pub total_retries: u64,
    pub total_session_cost_usd: f64,
    /// Info panggilan terakhir untuk pelaporan usage & transparansi fallback (v0.5.6 Bagian 1).
    pub last_served_model: Option<String>,
    pub last_was_fallback: bool,
    pub pending_fallback_notice: Option<String>,
}

impl RouterTelemetry {
    /// Ambil `(actual_model, was_fallback, one_time_notice)` dari panggilan terakhir.
    pub fn take_last_call_info(&mut self) -> (Option<String>, bool, Option<String>) {
        (
            self.last_served_model.clone(),
            self.last_was_fallback,
            self.pending_fallback_notice.take(),
        )
    }
    /// Muat data performa historis dari SQLite (`model_perf`) agar router belajar lintas sesi.
    pub fn seed_from_perf_records(&mut self, records: &[crate::store::ModelPerfRecord]) {
        for r in records {
            self.historical.insert(
                (r.route_id.clone(), r.task_kind.clone()),
                (r.successes, r.failures),
            );
        }
    }

    pub fn record_decision(&mut self, decision: RoutingDecision) {
        self.last_decision = Some(decision.clone());
        self.recent_decisions.push(decision);
        if self.recent_decisions.len() > 15 {
            self.recent_decisions.remove(0);
        }
    }

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
                self.total_session_cost_usd += cost;
            }
        }
        let hist = self
            .historical
            .entry((route.id.clone(), task.as_str().to_string()))
            .or_insert((0, 0));
        hist.0 += 1;
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
        let hist = self
            .historical
            .entry((route.id.clone(), task.as_str().to_string()))
            .or_insert((0, 0));
        hist.1 += 1;
    }

    pub fn snapshot_routes(&self) -> Vec<RouteMetrics> {
        let mut out: Vec<RouteMetrics> = self.routes.values().cloned().collect();
        out.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.route_id.cmp(&b.route_id)));
        out
    }
}

/// Deteksi apakah giliran ini memerlukan eskalasi ke model yang lebih kuat
/// (mis. pasca kegagalan verifikasi / Self-Correction Gate atau `>= 2` error tool terbaru).
pub fn detect_escalation_needed(msgs: &[Message]) -> bool {
    let mut recent_tool_errors = 0usize;
    for m in msgs.iter().rev().take(6) {
        for b in &m.content {
            match b {
                crate::providers::Block::Text(t) => {
                    if t.contains("[Self-Correction Gate")
                        || t.contains("[Self-Correction / Deteksi")
                    {
                        return true;
                    }
                }
                crate::providers::Block::ToolResult { content, is_error, .. } => {
                    if *is_error {
                        recent_tool_errors += 1;
                    }
                    if content.contains("GAGAL ❌")
                        || content.contains("TIMEOUT ⏱")
                        || content.contains("NO_TESTS")
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    recent_tool_errors >= 2
}

/// Skor kecocokan satu kandidat rute berbasis performa aktual historis, eskalasi,
/// budget biaya, kebutuhan konteks/tool, & heuristik tugas (mengembalikan skor + alasan).
fn score_candidate(
    route: &ModelRoute,
    task: TaskKind,
    needed_tokens: usize,
    needs_tools: bool,
    escalated: bool,
    budget_tight: bool,
    metrics: Option<&RouteMetrics>,
    historical: Option<(u64, u64)>,
) -> (i32, String) {
    let mut score = 0i32;
    let mut reasons: Vec<String> = Vec::new();

    if route.is_primary {
        score += 20;
    }

    if needs_tools && !route.supports_tools {
        score -= 100;
        reasons.push("tanpa tool-calling".into());
    }
    if needed_tokens > 0 && route.context_limit < needed_tokens {
        score -= 60;
        reasons.push("konteks sempit".into());
    }

    let m = route.model.to_ascii_lowercase();
    let is_fast_cheap = contains_any(&m, &["mini", "flash", "haiku", "nano", "small", "lite", "instant"]);
    let is_deep_reason = contains_any(&m, &["o1", "o3", "r1", "reasoner", "thinking", "opus", "pro", "sonnet"]);
    let is_coder = contains_any(&m, &["coder", "codex", "sonnet", "claude", "gpt-4", "gpt-5", "deepseek", "qwen"]);

    // 1. Performa aktual historis dari SQLite / benchmark (bobot lebih tinggi dari tebakan nama!).
    if let Some((succ, fail)) = historical {
        let total = succ + fail;
        if total >= 2 {
            let rate = succ as f64 / total as f64;
            let hist_bonus = ((rate - 0.5) * 90.0).round() as i32; // -45 .. +45
            score += hist_bonus;
            reasons.push(format!("histori {:.0}% ({succ}/{total})", rate * 100.0));
        }
    }

    // 2. Eskalasi pasca-verifikasi gagal: hindari model kecil/cepat, promosikan model kuat.
    if escalated {
        if is_deep_reason || is_coder {
            score += 35;
            reasons.push("eskalasi model kuat".into());
        }
        if is_fast_cheap {
            score -= 35;
        }
    }

    // 3. Budget biaya ketat: prioritaskan rute ekonomis.
    if budget_tight {
        if let Some(pin) = route.price_in {
            if pin <= 1.0 {
                score += 28;
                reasons.push("hemat budget".into());
            } else if pin >= 3.0 {
                score -= 30;
            }
        }
    }

    match task {
        TaskKind::Summary => {
            if is_fast_cheap {
                score += 30;
                reasons.push("ringkasan cepat".into());
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
                reasons.push("reasoning/review".into());
            }
            if is_fast_cheap {
                score -= 10;
            }
        }
        TaskKind::Coding | TaskKind::Debugging => {
            if is_coder {
                score += 25;
                reasons.push("coding/debug".into());
            }
            if is_deep_reason {
                score += 12;
            }
        }
        TaskKind::General => {}
    }

    if let Some(met) = metrics {
        if met.in_cooldown() {
            score -= 80;
            reasons.push("cooldown".into());
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

    let reason = if reasons.is_empty() {
        format!("tugas={}", task.as_str())
    } else {
        format!("tugas={} · {}", task.as_str(), reasons.join(", "))
    };
    (score, reason)
}

/// Provider wrapper yang mengorkestrasi Intelligent Model Routing, Learning, Escalation, Retry, & Fallback.
pub struct DynamicRouterProvider {
    pub routes: Vec<ModelRoute>,
    pub auto_routing: bool,
    pub task_routes: HashMap<String, String>,
    pub max_retries: u8,
    pub retry_backoff_ms: u64,
    pub max_cost_usd: Option<f64>,
    /// Maksimum total model/rute yang dicoba per giliran termasuk model utama (default 3, v0.5.6).
    pub max_fallback_models: usize,
    /// True = mode fallback non-lengket: giliran/sesi baru selalu mulai dari model utama lagi.
    pub non_sticky_manual: bool,
    pub store: Option<Arc<crate::store::Store>>,
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
            max_cost_usd: None,
            max_fallback_models: 3,
            non_sticky_manual: false,
            store: None,
            telemetry,
        }
    }

    pub fn with_max_fallback_models(mut self, max_models: usize) -> Self {
        self.max_fallback_models = max_models.max(1);
        self
    }

    pub fn with_non_sticky_manual(mut self, non_sticky: bool) -> Self {
        self.non_sticky_manual = non_sticky;
        self
    }

    pub fn with_backoff_ms(mut self, ms: u64) -> Self {
        self.retry_backoff_ms = ms;
        self
    }

    pub fn with_max_cost_usd(mut self, max_cost_usd: Option<f64>) -> Self {
        self.max_cost_usd = max_cost_usd.filter(|c| *c > 0.0);
        self
    }

    pub fn with_store(mut self, store: Arc<crate::store::Store>) -> Self {
        if let Ok(records) = store.load_model_perf() {
            if let Ok(mut tel) = self.telemetry.lock() {
                tel.seed_from_perf_records(&records);
            }
        }
        self.store = Some(store);
        self
    }

    /// Tentukan urutan kandidat rute beserta skor & alasan transparan.
    pub fn plan_candidates_explained(
        &self,
        msgs: &[Message],
        tools: &[ToolSpec],
    ) -> (TaskKind, bool, Vec<(ModelRoute, i32, String)>) {
        let task = classify_task(msgs, tools);
        let escalated = detect_escalation_needed(msgs);
        let needed_tokens = estimate_tokens(msgs).saturating_add(4096);
        let needs_tools = !tools.is_empty();

        if self.routes.len() <= 1 {
            let list = self
                .routes
                .iter()
                .map(|r| (r.clone(), 0, format!("rute tunggal (tugas={})", task.as_str())))
                .collect();
            return (task, escalated, list);
        }

        let tel = self.telemetry.lock().ok();
        let budget_tight = self
            .max_cost_usd
            .zip(tel.as_ref().map(|t| t.total_session_cost_usd))
            .map(|(cap, used)| used >= cap * 0.75)
            .unwrap_or(false);

        if !self.auto_routing {
            if self.non_sticky_manual {
                // Fallback TIDAK lengket (v0.5.6 Bagian 1): sesi/giliran baru selalu mulai dari model utama lagi.
                let ordered = self
                    .routes
                    .iter()
                    .map(|r| {
                        let lbl = if r.is_primary {
                            format!("pilihan manual primary (tugas={})", task.as_str())
                        } else {
                            format!("fallback terurut (tugas={})", task.as_str())
                        };
                        (r.clone(), if r.is_primary { 20 } else { 0 }, lbl)
                    })
                    .collect();
                return (task, escalated, ordered);
            }
            let mut healthy = Vec::new();
            let mut cooled = Vec::new();
            for r in &self.routes {
                let in_cd = tel
                    .as_ref()
                    .and_then(|t| t.routes.get(&r.id))
                    .map(|m| m.in_cooldown())
                    .unwrap_or(false);
                if in_cd {
                    cooled.push((r.clone(), -80, "mode manual (cooldown)".to_string()));
                } else {
                    let lbl = if r.is_primary {
                        format!("pilihan manual primary (tugas={})", task.as_str())
                    } else {
                        format!("fallback manual (tugas={})", task.as_str())
                    };
                    healthy.push((r.clone(), if r.is_primary { 20 } else { 0 }, lbl));
                }
            }
            healthy.extend(cooled);
            return (task, escalated, healthy);
        }

        let explicit_target = self.task_routes.get(task.as_str()).map(|s| s.trim());
        let mut scored: Vec<(i32, usize, ModelRoute, String)> = self
            .routes
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let met = tel.as_ref().and_then(|t| t.routes.get(&r.id));
                let hist = tel
                    .as_ref()
                    .and_then(|t| t.historical.get(&(r.id.clone(), task.as_str().to_string())).copied());
                let (mut sc, mut reason) = score_candidate(
                    r,
                    task,
                    needed_tokens,
                    needs_tools,
                    escalated,
                    budget_tight,
                    met,
                    hist,
                );
                if let Some(target) = explicit_target {
                    if r.id.eq_ignore_ascii_case(target)
                        || r.provider_name.eq_ignore_ascii_case(target)
                        || r.model.eq_ignore_ascii_case(target)
                    {
                        sc += 200;
                        reason = format!("aturan eksplisit {} · {reason}", task.as_str());
                    }
                }
                (sc, idx, r.clone(), reason)
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        (
            task,
            escalated,
            scored
                .into_iter()
                .map(|(sc, _, r, reason)| (r, sc, reason))
                .collect(),
        )
    }

    /// Kompatibel penuh dengan API Tahap 3.
    pub fn plan_candidates(&self, msgs: &[Message], tools: &[ToolSpec]) -> (TaskKind, Vec<ModelRoute>) {
        let (task, _, explained) = self.plan_candidates_explained(msgs, tools);
        (task, explained.into_iter().map(|(r, _, _)| r).collect())
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
        if let Some(cap) = self.max_cost_usd {
            let used = self
                .telemetry
                .lock()
                .map(|t| t.total_session_cost_usd)
                .unwrap_or(0.0);
            if used >= cap {
                return Err(anyhow!(
                    "batas biaya routing sesi (${cap:.4}) telah tercapai (terpakai ${used:.4}). Naikkan atau hapus batas lewat `/route budget clear`."
                ));
            }
        }

        let (task, escalated, candidates) = self.plan_candidates_explained(msgs, tools);
        if candidates.is_empty() {
            return Err(anyhow!("tidak ada provider/rute yang tersedia di DynamicRouterProvider"));
        }

        let mut errors: Vec<String> = Vec::new();
        let mut primary_error: Option<String> = None;
        let mut primary_fail_short: Option<&'static str> = None;
        let max_models_to_try = self.max_fallback_models.max(1);
        let mut tried_route_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut models_tried = 0usize;

        for (cand_idx, (route, score, reason)) in candidates.iter().enumerate() {
            if models_tried >= max_models_to_try {
                break;
            }
            if !tried_route_ids.insert(route.id.clone()) {
                continue;
            }
            models_tried += 1;
            let max_attempts = (self.max_retries as usize) + 1;
            for attempt in 0..max_attempts {
                let started = Instant::now();
                let (inner_tx, mut inner_rx) = mpsc::channel::<StreamEvent>(128);
                let out_tx = tx.clone();

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
                        let used_tokens = reply
                            .usage
                            .map(|u| u.input_tokens + u.output_tokens)
                            .unwrap_or(0);
                        let call_cost = match (reply.usage, route.price_in, route.price_out) {
                            (Some(u), Some(pin), Some(pout)) => {
                                (u.input_tokens as f64 / 1_000_000.0) * pin
                                    + (u.output_tokens as f64 / 1_000_000.0) * pout
                            }
                            _ => 0.0,
                        };
                        let used_fallback = cand_idx > 0 && primary_error.is_some();
                        if let Ok(mut tel) = self.telemetry.lock() {
                            tel.record_success(route, task, elapsed_ms, reply.usage, cand_idx > 0);
                            tel.last_served_model = Some(route.model.clone());
                            tel.last_was_fallback = used_fallback;
                            if used_fallback {
                                let cause = primary_fail_short.unwrap_or("error provider");
                                tel.pending_fallback_notice = Some(format!(
                                    "Model utama gagal ({cause}) — lanjut dengan {}",
                                    route.model
                                ));
                            } else {
                                tel.pending_fallback_notice = None;
                            }
                            tel.record_decision(RoutingDecision {
                                route_id: route.id.clone(),
                                task,
                                escalated,
                                score: *score,
                                fallback_used: cand_idx > 0,
                                reason: if cand_idx > 0 {
                                    format!("fallback · {reason}")
                                } else {
                                    reason.clone()
                                },
                            });
                        }
                        if let Some(st) = &self.store {
                            let _ = st.record_model_outcome(
                                &route.id,
                                task.as_str(),
                                true,
                                elapsed_ms,
                                used_tokens,
                                call_cost,
                            );
                        }
                        return Ok(reply);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if primary_error.is_none() {
                            primary_error = Some(msg.clone());
                            primary_fail_short = Some(short_fallback_reason(&msg));
                        }
                        let retryable = is_retryable_error(&msg);
                        let fallback_eligible = is_fallback_eligible_error(&msg);
                        let will_retry = retryable && !emitted_any && attempt + 1 < max_attempts;
                        if let Ok(mut tel) = self.telemetry.lock() {
                            tel.record_error(route, task, &msg, will_retry);
                        }
                        if !will_retry {
                            if let Some(st) = &self.store {
                                let _ = st.record_model_outcome(
                                    &route.id,
                                    task.as_str(),
                                    false,
                                    elapsed_ms,
                                    0,
                                    0.0,
                                );
                            }
                        }
                        errors.push(format!("{} (percobaan {}): {}", route.id, attempt + 1, msg));
                        if will_retry {
                            if self.retry_backoff_ms > 0 {
                                let delay = self.retry_backoff_ms * (attempt as u64 + 1);
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                            }
                            continue;
                        }
                        if emitted_any {
                            return Err(anyhow!(
                                "stream terputus di tengah jawaban ({}): {}",
                                route.id,
                                msg
                            ));
                        }
                        // BAGIAN 1 (#2): JANGAN fallback pada error 400 / permintaan rusak / lokal / ambigu!
                        if !fallback_eligible {
                            return Err(e);
                        }
                        break;
                    }
                }
            }
        }

        // BAGIAN 1 (#3): Bila semua cadangan habis, kembalikan error model utama (paling informatif).
        if let Some(first_err) = primary_error {
            if errors.len() > 1 {
                return Err(anyhow!(
                    "{first_err}\n(Fallback dicoba {} rute dan semua gagal: {})",
                    models_tried,
                    errors.join("; ")
                ));
            }
            return Err(anyhow!("{first_err}"));
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
