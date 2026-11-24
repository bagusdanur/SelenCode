//! Event UI dari loop agent (Fase 4).
//!
//! Mode polos (Fase 0-3) mencetak token langsung ke stdout. Mode TUI tidak
//! bisa berbagi stdout, jadi loop menyediakan varian `run_turn_ui` yang
//! mengirim semua yang tadinya dicetak sebagai `UiEvent` lewat mpsc.
//! StreamEvent provider tetap internal loop — tidak bocor ke lapisan UI.

use crate::providers::Usage;

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Potongan teks jawaban assistant yang sedang streaming.
    Token(String),
    /// Tool mulai dijalankan (args = ringkasan 1 baris).
    ToolStart { name: String, args: String },
    /// Tool selesai; summary = potongan hasil/error (sudah teredaksi).
    ToolDone {
        name: String,
        summary: String,
        is_error: bool,
    },
    /// Langkah provider ke-`current` dari `max`; context_pct = estimasi
    /// pemakaian konteks dalam persen (0-100).
    Step {
        current: usize,
        max: usize,
        context_pct: u8,
    },
    /// Giliran selesai. usage = total usage provider selama giliran ini
    /// (None bila provider tidak melaporkan / giliran gagal di tengah).
    TurnDone { usage: Option<Usage> },
    /// Provider aktif berubah (perintah /model dari UI).
    ModelChanged { provider: String, model: String },
    /// Checklist update_plan diperbarui (Fase 6) — dirender sebagai blok
    /// khusus di transkrip.
    PlanUpdated(String),
    /// Giliran plan selesai & rencana diekstrak (Fase 6): UI menyimpan
    /// rencana ini sebagai menunggu konfirmasi y/n dari user.
    PlanReady(String),
    /// Pesan informasi (ringkasan token, daftar sesi, compaction, dll).
    Info(String),
    /// Giliran gagal dengan error.
    Error(String),
}
