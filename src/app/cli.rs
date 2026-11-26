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
    /// Kelola tugas terjadwal (`selen cron add|list|remove|pause|resume`) — berjalan selama gateway Telegram hidup
    Cron {
        #[command(subcommand)]
        action: CronCliAction,
    },
    /// Cari percakapan lintas sesi tersimpan (FTS5)
    Search {
        /// Kata kunci atau frasa yang dicari
        query: Vec<String>,
    },
    /// Kelola pemantauan tugas asinkron (`selen watch list|cancel <id>`)
    Watch {
        #[command(subcommand)]
        action: WatchCliAction,
    },
    /// Pasang/copot/lihat skill pengguna (`selen skills install <url>|remove <nama>|list`)
    Skills {
        #[command(subcommand)]
        action: SkillsCliAction,
    },
    /// Perbarui selen ke rilis terbaru dari GitHub Releases
    Update,
}

#[derive(Subcommand)]
pub enum WatchCliAction {
    /// Tampilkan daftar pemantauan tugas asinkron
    List,
    /// Batalkan pemantauan aktif berdasarkan ID
    Cancel { id: i64 },
}

#[derive(Subcommand)]
pub enum SkillsCliAction {
    /// Pasang skill dari URL Git atau URL SKILL.md langsung (lewat staging, validasi, & persetujuan)
    Install {
        /// URL repositori Git (opsional `#subpath` atau `/tree/...`) atau URL `SKILL.md` langsung
        url: String,
        /// Ganti nama skill (kebab-case) bila terjadi bentrok nama
        #[arg(long)]
        rename: Option<String>,
    },
    /// Copot skill hasil `skills install` (tidak berlaku untuk bawaan atau skill proyek)
    Remove { name: String },
    /// Tampilkan daftar skill terpasang
    List,
}

#[derive(Subcommand)]
pub enum CronCliAction {
    /// Tambah job terjadwal baru (interval `every 5m`/`every 2h` atau harian `daily 08:30`)
    Add {
        /// Nama unik job
        #[arg(long)]
        name: String,
        /// Jadwal sederhana: `every 5m`, `every 2h`, atau `daily 08:30`
        #[arg(long)]
        schedule: String,
        /// Prompt tugas yang dijalankan agent
        #[arg(long)]
        prompt: String,
        /// Folder kerja job (default: folder saat ini)
        #[arg(long)]
        cwd: Option<String>,
        /// Allowlist perintah shell eksplisit per job (boleh diulang); kosong = hanya tool baca
        #[arg(long = "allow-shell")]
        allow_shell: Vec<String>,
        /// Izinkan tulis/edit file di dalam folder kerja job (default: false / baca saja)
        #[arg(long)]
        allow_write: bool,
        /// Target Chat ID Telegram untuk laporan (opsional; default whitelist pertama)
        #[arg(long)]
        chat_id: Option<i64>,
    },
    /// Tampilkan semua job terjadwal
    List,
    /// Hapus job berdasarkan nama atau ID
    Remove { target: String },
    /// Jeda (nonaktifkan sementara) job berdasarkan nama atau ID
    Pause { target: String },
    /// Aktifkan kembali job berdasarkan nama atau ID
    Resume { target: String },
}
