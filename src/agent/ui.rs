//! Event UI dari loop agent (Fase 4).
//!
//! Mode polos (Fase 0-3) mencetak token langsung ke stdout. Mode TUI tidak
//! bisa berbagi stdout, jadi loop menyediakan varian `run_turn_ui` yang
//! mengirim semua yang tadinya dicetak sebagai `UiEvent` lewat mpsc.
//! StreamEvent provider tetap internal loop — tidak bocor ke lapisan UI.

use crate::providers::Usage;

/// Snapshot data untuk modal pemilih model interaktif (ala Hermes) di TUI.
#[derive(Debug, Clone)]
pub struct ModelPickerData {
    pub provider: String,
    pub providers: Vec<String>,
    pub active_model: String,
    pub saved_models: Vec<String>,
    pub remote_models: Vec<String>,
    /// true = langsung buka tab Katalog API (mis. setelah tekan `f`).
    pub open_catalog: bool,
    pub status: Option<String>,
}

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
    /// Pemakaian & kapasitas context window model AI aktif (dalam token).
    ContextInfo {
        used_tokens: usize,
        limit_tokens: usize,
    },
    /// Checklist update_plan diperbarui (Fase 6) — dirender sebagai blok
    /// khusus di transkrip.
    PlanUpdated(String),
    /// Giliran plan selesai & rencana diekstrak (Fase 6): UI menyimpan
    /// rencana ini sebagai menunggu konfirmasi y/n dari user.
    PlanReady(String),
    /// Pesan informasi (ringkasan token, compaction, dll).
    Info(String),
    /// Kartu menu terstruktur berwarna (output /help, /model, /provider, /sessions).
    Menu(String),
    /// Buka atau perbarui modal pemilih model interaktif di TUI.
    ModelPicker(ModelPickerData),
    /// Sesi baru dimulai (/new): bersihkan transkrip & reset statistik sesi.
    SessionReset {
        session_id: String,
        provider: String,
        model: String,
    },
    /// Berpindah/melanjutkan sesi tersimpan (/resume <nomor>): muat ulang transkrip ringkas & metrik sesi.
    SessionResumed {
        session_id: String,
        title: String,
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        context_used: usize,
        context_limit: usize,
        preview_lines: Vec<(String, String)>,
        total_messages: usize,
    },
    /// Daftar file workspace selesai dipindai di latar belakang untuk `@file` autocomplete (non-blocking startup).
    WorkspaceFilesLoaded(Vec<String>),
    /// Giliran gagal dengan error.
    Error(String),
}
