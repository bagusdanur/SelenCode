//! Ketahanan Gateway Telegram SelenCode (Perbaikan 1 v0.5.10):
//! 1. `StallTracker` (mekanisme internal SelenCode):
//!    melacak kemajuan giliran aktif; mengirim TEPAT SATU peringatan macet per episode
//!    diam melewati ambang `stall_notice_secs`, lalu menutup episode secara diam-diam saat
//!    giliran bergerak lagi.
//! 2. `GatewayHealthClock` + OS-thread watchdog (mekanisme internal SelenCode):
//!    memantau detak jantung loop utama dan waktu keberhasilan polling Telegram di luar runtime
//!    Tokio; bila macet melewati `poll_watchdog_secs`, mencatat marker pemulihan di SQLite,
//!    menulis log bermakna ke stderr, lalu keluar non-nol agar systemd me-restart otomatis.
//! 3. `trim_gateway_session_memory`: membatasi retensi memori sesi jangka panjang di gateway
//!    (memangkas lampiran gambar base64 lama & memadatkan riwayat in-memory di atas pagu
//!    sambil menjaga riwayat penuh tetap aman di SQLite).

use crate::agent::context::{compact_messages_structured, prune_old_tool_results_with_archive};
use crate::providers::{Block, Message};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Batas jumlah pesan yang dipertahankan di RAM per sesi gateway setelah giliran selesai.
pub const MAX_GATEWAY_IN_MEM_MSGS: usize = 80;
/// Batas ukuran byte total pesan yang dipertahankan di RAM per sesi gateway (2 MB).
pub const MAX_GATEWAY_IN_MEM_BYTES: usize = 2 * 1024 * 1024;
/// Jumlah pesan terbaru yang masih mempertahankan lampiran `Block::Image` base64 di RAM.
pub const KEEP_RECENT_IMAGE_MSGS: usize = 4;
/// Batas maksimum sesi chat yang di-cache sekaligus di RAM gateway (sisanya dimuat ulang dari DB).
pub const MAX_CACHED_GATEWAY_SESSIONS: usize = 16;
/// Umur idle sesi chat sebelum dikeluarkan dari cache RAM gateway (2 jam).
pub const GATEWAY_SESSION_IDLE_SECS: u64 = 2 * 3600;

/// Format durasi manusiawi Bahasa Indonesia untuk pesan peringatan macet.
pub fn format_duration_id(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{mins} menit")
        } else {
            format!("{mins} menit {rem} detik")
        }
    } else {
        format!("{secs} detik")
    }
}

/// Pelacak kemacetan giliran aktif internal SelenCode.
/// - Mengirim TEPAT SATU peringatan per episode macet.
/// - `record_progress()` menutup episode secara diam-diam saat giliran bergerak lagi.
/// - Dapat dijeda (`paused = true`) saat sedang menunggu tombol approval / `ask_user` dari pengguna.
#[derive(Debug, Clone)]
pub struct StallTracker {
    threshold: Duration,
    last_progress: Instant,
    notified_in_episode: bool,
    paused: bool,
}

impl StallTracker {
    pub fn new(threshold: Duration, now: Instant) -> Self {
        Self {
            threshold,
            last_progress: now,
            notified_in_episode: false,
            paused: false,
        }
    }

    /// Catat adanya kemajuan (token streaming / mulai tool / selesai tool / langkah baru).
    pub fn record_progress(&mut self, now: Instant) {
        self.last_progress = now;
        self.notified_in_episode = false;
    }

    /// Jeda/lanjutkan pelacakan (mis. saat menunggu persetujuan tombol atau `ask_user`).
    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        if self.paused && !paused {
            // Baru kembali dari jeda menunggu user -> mulai hitung ulang dari sekarang.
            self.last_progress = now;
        }
        self.paused = paused;
    }

    pub fn is_notified(&self) -> bool {
        self.notified_in_episode
    }

    /// Evaluasi apakah giliran diam melewati ambang. Mengembalikan `Some(pesan)` TEPAT 1x per episode.
    pub fn check_stall(&mut self, now: Instant) -> Option<String> {
        if self.paused || self.threshold.is_zero() || self.notified_in_episode {
            return None;
        }
        let elapsed = now.saturating_duration_since(self.last_progress);
        if elapsed < self.threshold {
            return None;
        }
        self.notified_in_episode = true;
        Some(format!(
            "Giliran ini belum menunjukkan kemajuan selama {} — masih menunggu respons provider/tool.",
            format_duration_id(elapsed)
        ))
    }
}

/// Cap waktu milidetik MONOTONIK (tahan terhadap loncatan jam NTP/VM/Termux ke depan maupun ke belakang).
/// Berawal dari baseline 10^12 ms agar operasi pengurangan `saturating_sub` di uji deterministik aman.
pub fn now_epoch_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let base = START.get_or_init(Instant::now);
    const MONO_OFFSET_MS: u64 = 1_000_000_000_000;
    MONO_OFFSET_MS.saturating_add(base.elapsed().as_millis() as u64)
}

/// Hasil evaluasi kesehatan loop utama & polling Telegram oleh watchdog eksternal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogVerdict {
    Healthy,
    PollStalled { silent_secs: u64 },
    LoopUnresponsive { silent_secs: u64 },
}

impl WatchdogVerdict {
    pub fn reason_text(&self) -> Option<String> {
        match self {
            Self::Healthy => None,
            Self::PollStalled { silent_secs } => Some(format!(
                "polling Telegram macet/gagal selama {} detik",
                silent_secs
            )),
            Self::LoopUnresponsive { silent_secs } => Some(format!(
                "loop utama gateway tidak responsif selama {} detik",
                silent_secs
            )),
        }
    }
}

/// Jam detak jantung bebas-lock yang diperbarui oleh loop utama gateway dan dipantau oleh
/// thread OS watchdog terpisah (tidak bergantung pada scheduler Tokio).
#[derive(Debug)]
pub struct GatewayHealthClock {
    last_poll_ok_ms: AtomicU64,
    last_loop_heartbeat_ms: AtomicU64,
    last_active_chat_id: AtomicI64,
    shutdown: AtomicBool,
}

impl GatewayHealthClock {
    pub fn new(initial_chat_id: i64) -> Self {
        let now = now_epoch_ms();
        Self {
            last_poll_ok_ms: AtomicU64::new(now),
            last_loop_heartbeat_ms: AtomicU64::new(now),
            last_active_chat_id: AtomicI64::new(initial_chat_id),
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn mark_poll_ok(&self) {
        let now = now_epoch_ms();
        self.last_poll_ok_ms.store(now, Ordering::SeqCst);
        self.last_loop_heartbeat_ms.store(now, Ordering::SeqCst);
    }

    pub fn mark_loop_heartbeat(&self) {
        self.last_loop_heartbeat_ms
            .store(now_epoch_ms(), Ordering::SeqCst);
    }

    pub fn set_last_active_chat(&self, chat_id: i64) {
        if chat_id != 0 {
            self.last_active_chat_id.store(chat_id, Ordering::SeqCst);
        }
    }

    pub fn last_active_chat(&self) -> i64 {
        self.last_active_chat_id.load(Ordering::SeqCst)
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Khusus uji unit/integrasi: mundurkan cap waktu terakhir agar kondisi macet teruji deterministik.
    pub fn force_timestamps_for_test(&self, poll_ok_ms: u64, loop_hb_ms: u64) {
        self.last_poll_ok_ms.store(poll_ok_ms, Ordering::SeqCst);
        self.last_loop_heartbeat_ms.store(loop_hb_ms, Ordering::SeqCst);
    }

    /// Evaluasi kesehatan gateway terhadap ambang keras `threshold_secs`.
    pub fn evaluate_at(&self, now_ms: u64, threshold_secs: u64) -> WatchdogVerdict {
        if threshold_secs == 0 {
            return WatchdogVerdict::Healthy;
        }
        let threshold_ms = threshold_secs.saturating_mul(1000);
        let poll_age = now_ms.saturating_sub(self.last_poll_ok_ms.load(Ordering::SeqCst));
        if poll_age >= threshold_ms {
            return WatchdogVerdict::PollStalled {
                silent_secs: poll_age / 1000,
            };
        }
        let hb_age = now_ms.saturating_sub(self.last_loop_heartbeat_ms.load(Ordering::SeqCst));
        if hb_age >= threshold_ms {
            return WatchdogVerdict::LoopUnresponsive {
                silent_secs: hb_age / 1000,
            };
        }
        WatchdogVerdict::Healthy
    }
}

/// Jalankan pengawas kelangsungan di thread OS terpisah (di luar Tokio runtime).
/// Jika polling macet / loop utama tidak responsif melewati `threshold_secs`:
/// 1. Catat marker pemulihan ke SQLite (`gateway_state`),
/// 2. Tulis log diagnostik jelas ke stderr,
/// 3. Panggil `on_unhealthy` (di produksi: `std::process::exit(2)` agar systemd me-restart otomatis).
pub fn spawn_os_watchdog<F>(
    clock: Arc<GatewayHealthClock>,
    store: Arc<crate::store::Store>,
    threshold_secs: u64,
    check_interval: Duration,
    on_unhealthy: F,
) -> std::thread::JoinHandle<()>
where
    F: FnOnce(WatchdogVerdict) + Send + 'static,
{
    std::thread::spawn(move || {
        let interval = check_interval.max(Duration::from_millis(50));
        loop {
            std::thread::sleep(interval);
            if clock.is_shutdown() {
                return;
            }
            let verdict = clock.evaluate_at(now_epoch_ms(), threshold_secs);
            if let Some(reason) = verdict.reason_text() {
                let chat_id = clock.last_active_chat();
                eprintln!(
                    "[watchdog] KRITIS: {reason} (ambang {threshold_secs}s, chat_id={chat_id}) — menyimpan status pemulihan & keluar agar supervisor me-restart gateway."
                );
                // Rekam marker pemulihan dengan batas waktu 2 detik agar mutex Store yang deadlock
                // tidak pernah menahan watchdog untuk memanggil `on_unhealthy` (`exit(2)`).
                let store_rec = store.clone();
                let reason_rec = reason.clone();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let _ = std::thread::spawn(move || {
                    let _ = store_rec.record_gateway_recovery(chat_id, &reason_rec);
                    let _ = done_tx.send(());
                });
                let _ = done_rx.recv_timeout(Duration::from_secs(2));
                on_unhealthy(verdict);
                return;
            }
        }
    })
}

/// Format catatan singkat yang dikirim sekali ke chat aktif terakhir setelah gateway pulih dari restart watchdog.
pub fn format_recovery_notice(reason: &str, crashed_at: &str) -> String {
    let ts_short: String = crashed_at.chars().take(19).collect();
    if ts_short.is_empty() {
        format!(
            "Gateway sempat berhenti merespons ({reason}) dan baru saja pulih lewat restart otomatis. Pesan yang tertahan diproses sekarang."
        )
    } else {
        format!(
            "Gateway sempat berhenti merespons ({reason}, pukul {ts_short} UTC) dan baru saja pulih lewat restart otomatis. Pesan yang tertahan diproses sekarang."
        )
    }
}

/// Estimasi ukuran memori (byte) daftar `Message` yang disimpan di RAM.
pub fn estimate_messages_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            64 + m
                .content
                .iter()
                .map(|b| match b {
                    Block::Text(t) => t.len(),
                    Block::Image {
                        media_type,
                        data_base64,
                    } => media_type.len() + data_base64.len(),
                    Block::ToolUse { id, name, args } => {
                        id.len() + name.len() + args.to_string().len()
                    }
                    Block::ToolResult { id, content, .. } => id.len() + content.len(),
                })
                .sum::<usize>()
        })
        .sum()
}

/// Laporan hasil pemangkasan retensi memori sesi gateway di RAM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayMemoryTrimReport {
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub images_stripped: usize,
    pub tool_results_pruned: usize,
    pub compacted: bool,
}

/// Batasi retensi memori `messages` sesi gateway di RAM sesudah giliran selesai (Perbaikan 1 #5 v0.5.10):
/// 1. Ganti payload `Block::Image` base64 di luar `KEEP_RECENT_IMAGE_MSGS` pesan terakhir menjadi stub teks ringan.
/// 2. Bila jumlah pesan > `MAX_GATEWAY_IN_MEM_MSGS` atau ukuran > `MAX_GATEWAY_IN_MEM_BYTES`,
///    jalankan `prune_old_tool_results_with_archive` lalu `compact_messages_structured` pada salinan RAM
///    (riwayat mentah di SQLite tetap 100% utuh).
pub fn trim_gateway_session_memory(
    messages: &mut Vec<Message>,
    archive_dir: &Path,
) -> GatewayMemoryTrimReport {
    trim_gateway_session_memory_with_limits(
        messages,
        archive_dir,
        MAX_GATEWAY_IN_MEM_MSGS,
        MAX_GATEWAY_IN_MEM_BYTES,
    )
}

pub fn trim_gateway_session_memory_with_limits(
    messages: &mut Vec<Message>,
    archive_dir: &Path,
    max_msgs: usize,
    max_bytes: usize,
) -> GatewayMemoryTrimReport {
    let bytes_before = estimate_messages_bytes(messages);
    let mut images_stripped = 0usize;

    // 1. Lepas payload base64 gambar dari giliran lama (bisa 6-24 MB per giliran foto!).
    if messages.len() > KEEP_RECENT_IMAGE_MSGS {
        let img_cutoff = messages.len() - KEEP_RECENT_IMAGE_MSGS;
        for m in &mut messages[..img_cutoff] {
            for b in &mut m.content {
                if let Block::Image { media_type, .. } = b {
                    let mt = media_type.clone();
                    *b = Block::Text(format!(
                        "[lampiran gambar {mt} dari giliran sebelumnya — dilepas dari RAM]"
                    ));
                    images_stripped += 1;
                }
            }
        }
    }

    let mut tool_results_pruned = 0usize;
    let mut compacted = false;

    if messages.len() > max_msgs || estimate_messages_bytes(messages) > max_bytes {
        tool_results_pruned = prune_old_tool_results_with_archive(messages, archive_dir, 12);
    }
    if messages.len() > max_msgs || estimate_messages_bytes(messages) > max_bytes {
        *messages = compact_messages_structured(messages);
        compacted = true;
    }

    let bytes_after = estimate_messages_bytes(messages);
    GatewayMemoryTrimReport {
        bytes_before,
        bytes_after,
        images_stripped,
        tool_results_pruned,
        compacted,
    }
}
