use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "selen", version, about = "SelenCode — terminal AI coding agent (full Rust), persona Selena")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Auto-approve semua tool yang butuh persetujuan (mode interaktif)
    #[arg(long, global = true)]
    pub yes: bool,
    /// Nama provider di config (default: default_provider)
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// Paksa mode polos (tanpa TUI) walau di terminal interaktif
    #[arg(long, global = true)]
    pub plain: bool,
    /// Plan mode (Fase 6): giliran pertama baca-saja & diakhiri rencana;
    /// untuk `selen run --plan` berhenti sesudah rencana (tanpa eksekusi)
    #[arg(long, global = true)]
    pub plan: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Sekali jalan: jalankan 1 perintah lalu keluar
    Run {
        /// Perintah / prompt untuk agent
        prompt: Vec<String>,
    },
    /// Lanjutkan sesi terakhir untuk folder ini (mode interaktif)
    Resume,
    /// Jalankan Telegram Bot Gateway untuk chat & coding jarak jauh dari Telegram
    Telegram,
    /// Perbarui selen ke rilis terbaru dari GitHub Releases
    Update,
}
