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
    /// Lanjutkan sesi terakhir atau nomor sesi tertentu (mis. `selen resume` atau `selen resume 2`)
    Resume {
        /// Nomor urut sesi (1-10) atau prefix ID sesi (opsional; default sesi terbaru)
        target: Option<String>,
    },
    /// Jalankan Telegram Bot Gateway untuk chat & coding jarak jauh dari Telegram
    Telegram,
    /// Jalankan Agent Benchmark Engine untuk mengukur & membandingkan kualitas model
    Bench {
        /// File JSON suite benchmark kustom (opsional; default memakai suite bawaan 5 kategori)
        #[arg(long)]
        suite: Option<String>,
        /// Filter task berdasarkan ID atau kategori (coding/bug_fix/refactoring/test_gen/tool_usage)
        #[arg(long)]
        filter: Option<String>,
        /// Simpan laporan hasil benchmark ke file JSON (opsional)
        #[arg(long)]
        out: Option<String>,
        /// Tampilkan riwayat perbandingan benchmark tersimpan tanpa menjalankan benchmark baru
        #[arg(long)]
        history: bool,
    },
    /// Cek kesehatan instalasi, provider, SQLite, Git, LSP, Sandbox, & Telegram
    Doctor,
    /// Kelola usulan Background Self-Improvement Review (`selen review pending|approve <id>|discard <id>|status`)
    Review {
        /// Subperintah: pending, approve <id>, discard <id>, status
        args: Vec<String>,
    },
    /// Perbarui selen ke rilis terbaru dari GitHub Releases
    Update,
}
