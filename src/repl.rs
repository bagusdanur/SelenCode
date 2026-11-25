//! Parsing perintah garis miring (`/`) & inline shell (`!cmd`) mode interaktif
//! + katalog perintah tunggal untuk palette live `/` di TUI dan tampilan `/help`.
//! Perintah garis miring TIDAK pernah diteruskan ke provider LLM.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Bukan perintah (teks biasa untuk agent).
    NotCommand,
    /// `!perintah` — jalankan shell lokal cepat tanpa memanggil LLM (ala OpenCode).
    InlineShell(String),
    Exit,
    /// /model | /model <nomor> | /model <provider> | /model set <nomor|id> | /model add <id|no...> | /model rm <nomor|id|all> | /model clear | /model list [filter]
    Model(Option<String>),
    /// /models [filter] (intip katalog model dari API tanpa mengotori daftar tersimpan)
    Models(Option<String>),
    /// /provider | /provider add ... | /provider preset ... | /provider model ... | /provider default ... | /provider rm ...
    Provider(Option<String>),
    /// /setup — buka wizard interaktif untuk pasang provider, base URL, API key, & model.
    Setup,
    /// /telegram [setup <token> [user_id] | allow <id> | rm <id> | approve <mode> | off]
    Telegram(Option<String>),
    /// /undo — batalkan edit/tulis file terakhir oleh agent.
    Undo,
    /// /redo — terapkan ulang edit/tulis file yang baru di-undo.
    Redo,
    /// /checkpoint [label] — buat atau lihat daftar checkpoint transaksional.
    Checkpoint(Option<String>),
    /// /rollback [id] — pulihkan perubahan SelenCode ke checkpoint terpilih (aman dari konflik eksternal).
    Rollback(Option<String>),
    /// /worktree [new <label> | diff <id> | apply <id> | rm <id>] — Git Worktree transaksional (Fase 6).
    Worktree(Option<String>),
    /// /mcp — status server MCP & matriks dukungan protokol (Fase 7).
    Mcp,
    /// /doctor — diagnostik kesehatan instalasi, provider, Git, LSP, Sandbox, & Telegram.
    Doctor,
    /// /verify [check | test | lint | all] — jalankan validasi proyek otomatis & tampilkan diagnostik.
    Verify(Option<String>),
    /// /index [query] atau /context [query] — lihat indeks repositori inkremental & uji pencarian konteks relevan.
    Index(Option<String>),
    /// /jobs [output <id> | stop <id|all>] — kelola proses latar belakang milik SelenCode.
    Jobs(Option<String>),
    /// /route [auto on/off | fallback ... | set <tugas> <target> | retries <n>] — Dynamic Model Routing & telemetri.
    Route(Option<String>),
    /// /lsp [diag [path] | def <simbol> | refs <simbol> | hover <simbol>] — LSP diagnostics & navigasi simbol.
    Lsp(Option<String>),
    /// /task [new <goal> | show <id> | pause <id> | resume <id> | rm <id>] — Long-Horizon Task persisten lintas sesi.
    Task(Option<String>),
    /// /memory | /memory add [kat] <teks> | /memory update <id> [kat] <teks> | /memory global [kat] <teks> | /memory rm <id>
    Memory(Option<String>),
    /// /skills | /skills auto | /skills review | /skills show <nama> | /skills rm <nama>
    Skills(Option<String>),
    /// /review [pending | approve <id> | discard <id> | status] — kelola usulan background review (v0.5.1).
    Review(Option<String>),
    /// /skill <nama> — aktifkan instruksi skill ke sesi ini.
    Skill(Option<String>),
    /// /learn <nama> <instruksi> — simpan skill baru ke `.selencode/skills/`.
    Learn(Option<String>),
    /// /init — buat starter `AGENTS.md` sesuai stack project.
    Init,
    /// /export [nama.md] — ekspor percakapan saat ini ke file Markdown.
    Export(Option<String>),
    /// /approve [ask | edits | auto] — atur mode izin aksi sesi aktif.
    Approve(Option<String>),
    /// /sandbox [read-only | workspace-write | unrestricted | isolate ... | audit] — Security Sandbox v2.
    Sandbox(Option<String>),
    /// /details — toggle tampilan ringkas vs detail pada baris tool di TUI.
    Details,
    /// /sessions
    Sessions,
    /// /resume [nomor | info | rename <judul>] — lihat 10 sesi terakhir bernomor & lanjutkan sesi.
    Resume(Option<String>),
    /// /plan (giliran berikutnya plan mode baca-saja)
    Plan,
    /// /new atau /clear (mulai sesi percakapan baru yang bersih)
    New,
    /// /compact (ringkas riwayat konteks secara manual)
    Compact,
    /// /stop atau /cancel (hentikan semua task, tool, & giliran agent aktif)
    Stop,
    /// /help atau /?
    Help,
    /// Perintah garis miring yang tidak dikenal (nama tanpa '/').
    Unknown(String),
}

/// Definisi satu perintah `/` untuk live palette TUI & kartu `/help`.
#[derive(Debug, Clone, Copy)]
pub struct CommandDef {
    pub cmd: &'static str,
    pub args: &'static str,
    pub desc: &'static str,
    /// true = setelah Tab di palette, tambahkan spasi agar user langsung ketik argumen.
    pub takes_args: bool,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        cmd: "/setup",
        args: "",
        desc: "wizard mudah pasang provider, base URL, API key, & model",
        takes_args: false,
    },
    CommandDef {
        cmd: "/model",
        args: "[no / add / rm / clear]",
        desc: "pemilih model interaktif, tambah 1-per-1, atau hapus",
        takes_args: true,
    },
    CommandDef {
        cmd: "/models",
        args: "[cari]",
        desc: "intip katalog API (pilih 1-per-1 dengan /model add)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/provider",
        args: "[preset / add / default / rm]",
        desc: "kelola & pasang provider lain + API key",
        takes_args: true,
    },
    CommandDef {
        cmd: "/telegram",
        args: "[setup / allow / approve / off]",
        desc: "hubungkan bot Telegram Gateway (`selen telegram`)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/stop",
        args: "",
        desc: "hentikan paksa semua task, tool, & giliran agent aktif",
        takes_args: false,
    },
    CommandDef {
        cmd: "/plan",
        args: "",
        desc: "giliran berikutnya baca-saja & minta izin rencana",
        takes_args: false,
    },
    CommandDef {
        cmd: "/undo",
        args: "",
        desc: "batalkan perubahan file terakhir oleh agent",
        takes_args: false,
    },
    CommandDef {
        cmd: "/redo",
        args: "",
        desc: "terapkan ulang perubahan yang baru di-undo",
        takes_args: false,
    },
    CommandDef {
        cmd: "/checkpoint",
        args: "[label]",
        desc: "buat atau lihat daftar checkpoint transaksional",
        takes_args: true,
    },
    CommandDef {
        cmd: "/rollback",
        args: "[id]",
        desc: "pulihkan perubahan SelenCode ke checkpoint (aman konflik)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/worktree",
        args: "[new / diff / apply / rm]",
        desc: "isolasi refactor besar lewat Git Worktree & preview diff aman",
        takes_args: true,
    },
    CommandDef {
        cmd: "/mcp",
        args: "",
        desc: "lihat server MCP & fitur protokol yang didukung",
        takes_args: false,
    },
    CommandDef {
        cmd: "/doctor",
        args: "",
        desc: "cek kesehatan provider, SQLite, Git, LSP, & sandbox",
        takes_args: false,
    },
    CommandDef {
        cmd: "/verify",
        args: "[check / test / lint]",
        desc: "validasi otomatis proyek & ekstrak error terstruktur",
        takes_args: true,
    },
    CommandDef {
        cmd: "/index",
        args: "[query]",
        desc: "indeks repositori inkremental & uji smart context retrieval",
        takes_args: true,
    },
    CommandDef {
        cmd: "/jobs",
        args: "[output <id> / stop <id>]",
        desc: "kelola proses background SelenCode (server, watcher, build)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/route",
        args: "[auto / fallback / set / retries]",
        desc: "routing model dinamis, fallback provider, & metrik latensi/biaya",
        takes_args: true,
    },
    CommandDef {
        cmd: "/lsp",
        args: "[diag / def / refs <simbol>]",
        desc: "status LSP adapter, diagnostik bahasa, definisi, & referensi simbol",
        takes_args: true,
    },
    CommandDef {
        cmd: "/task",
        args: "[new / show / pause / resume / rm]",
        desc: "tugas panjang persisten lintas sesi & pemulihan checkpoint",
        takes_args: true,
    },
    CommandDef {
        cmd: "/memory",
        args: "[add / update / global / rm <id>]",
        desc: "memori proyek terstruktur lintas sesi (anti-rahasia)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/skills",
        args: "[auto / review / show / rm]",
        desc: "daftar skill, auto self-improvement, & review",
        takes_args: true,
    },
    CommandDef {
        cmd: "/review",
        args: "[pending / approve <id> / discard <id> / status]",
        desc: "lihat & putuskan usulan staging background review",
        takes_args: true,
    },
    CommandDef {
        cmd: "/skill",
        args: "<nama>",
        desc: "aktifkan skill ke percakapan saat ini",
        takes_args: true,
    },
    CommandDef {
        cmd: "/learn",
        args: "<nama> <instruksi>",
        desc: "simpan workflow/instruksi baru sebagai skill",
        takes_args: true,
    },
    CommandDef {
        cmd: "/init",
        args: "",
        desc: "buat file panduan AGENTS.md untuk project ini",
        takes_args: false,
    },
    CommandDef {
        cmd: "/export",
        args: "[nama.md]",
        desc: "ekspor riwayat percakapan ke Markdown",
        takes_args: true,
    },
    CommandDef {
        cmd: "/approve",
        args: "[ask / edits / auto]",
        desc: "atur mode persetujuan (kurangi popup izin)",
        takes_args: true,
    },
    CommandDef {
        cmd: "/sandbox",
        args: "[read-only / workspace-write / isolate / audit]",
        desc: "atur mode sandbox, isolasi OS, env guard, & audit log",
        takes_args: true,
    },
    CommandDef {
        cmd: "/details",
        args: "",
        desc: "toggle tampilan ringkas / detail hasil tool",
        takes_args: false,
    },
    CommandDef {
        cmd: "/new",
        args: "",
        desc: "mulai sesi baru (bersihkan transkrip & konteks)",
        takes_args: false,
    },
    CommandDef {
        cmd: "/compact",
        args: "",
        desc: "ringkas riwayat konteks secara manual",
        takes_args: false,
    },
    CommandDef {
        cmd: "/sessions",
        args: "",
        desc: "daftar 10 sesi terakhir di folder ini",
        takes_args: false,
    },
    CommandDef {
        cmd: "/resume",
        args: "[nomor / rm / info]",
        desc: "tampilkan 10 sesi terakhir bernomor & lanjutkan/hapus sesi",
        takes_args: true,
    },
    CommandDef {
        cmd: "/help",
        args: "",
        desc: "tampilkan semua perintah & cara pakai",
        takes_args: false,
    },
    CommandDef {
        cmd: "/exit",
        args: "",
        desc: "keluar dari SelenCode",
        takes_args: false,
    },
];

/// Filter daftar perintah untuk live `/` palette saat user mengetik.
/// Aktif bila input 1 baris, dimulai dengan `/`, dan belum mengandung spasi.
pub fn filter_commands(input: &str) -> Vec<&'static CommandDef> {
    if !input.starts_with('/') || input.contains('\n') || input.contains(' ') {
        return Vec::new();
    }
    let needle = input.to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|c| c.cmd.starts_with(&needle))
        .collect()
}

pub fn parse_slash_command(line: &str) -> SlashCommand {
    let line = line.trim();
    if let Some(cmd) = line.strip_prefix('!') {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            return SlashCommand::InlineShell(cmd.to_string());
        }
        return SlashCommand::NotCommand;
    }
    if !line.starts_with('/') {
        return SlashCommand::NotCommand;
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match head {
        "/exit" | "/quit" => SlashCommand::Exit,
        "/model" => SlashCommand::Model(rest),
        "/models" => SlashCommand::Models(rest),
        "/provider" | "/connect" => SlashCommand::Provider(rest),
        "/setup" | "/onboard" => SlashCommand::Setup,
        "/telegram" | "/tg" => SlashCommand::Telegram(rest),
        "/undo" => SlashCommand::Undo,
        "/redo" => SlashCommand::Redo,
        "/checkpoint" | "/cp" => SlashCommand::Checkpoint(rest),
        "/rollback" => SlashCommand::Rollback(rest),
        "/worktree" | "/wt" => SlashCommand::Worktree(rest),
        "/mcp" => SlashCommand::Mcp,
        "/doctor" => SlashCommand::Doctor,
        "/verify" => SlashCommand::Verify(rest),
        "/index" | "/context" => SlashCommand::Index(rest),
        "/jobs" | "/bg" => SlashCommand::Jobs(rest),
        "/route" | "/routing" => SlashCommand::Route(rest),
        "/lsp" => SlashCommand::Lsp(rest),
        "/task" | "/tasks" => SlashCommand::Task(rest),
        "/memory" => SlashCommand::Memory(rest),
        "/skills" => SlashCommand::Skills(rest),
        "/review" => SlashCommand::Review(rest),
        "/skill" => SlashCommand::Skill(rest),
        "/learn" => SlashCommand::Learn(rest),
        "/init" => SlashCommand::Init,
        "/export" => SlashCommand::Export(rest),
        "/approve" => SlashCommand::Approve(rest),
        "/sandbox" => SlashCommand::Sandbox(rest),
        "/details" => SlashCommand::Details,
        "/sessions" => SlashCommand::Sessions,
        "/resume" => SlashCommand::Resume(rest),
        "/plan" => SlashCommand::Plan,
        "/new" | "/clear" => SlashCommand::New,
        "/compact" => SlashCommand::Compact,
        "/stop" | "/cancel" => SlashCommand::Stop,
        "/help" | "/?" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.trim_start_matches('/').to_string()),
    }
}

/// Aksi hasil parsing argumen `/model <arg>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    /// `/model list [filter]` -> tarik daftar model dari API & simpan ke daftar provider.
    FetchList(Option<String>),
    /// `/model <nomor>` atau `/model set <nomor>` -> pilih model berdasarkan nomor urut (1-based).
    SelectIndex(usize),
    /// `/model set <model_id>` -> set model manual & tambahkan ke daftar tersimpan.
    SetCurrentModel(String),
    /// `/model add <m1> [m2 ...]` -> tambah model (ID manual atau nomor dari `/models`) ke daftar bernomor provider aktif.
    AddModels(Vec<String>),
    /// `/model rm <nomor_atau_id>` -> hapus 1 model dari daftar bernomor provider aktif.
    RemoveModel(String),
    /// `/model clear` / `/model rm all` -> hapus semua model tersimpan (sisakan 1 model aktif saja).
    ClearModels,
    /// `/model <provider>:<nomor_atau_id>` -> pindah provider + pilih/set modelnya.
    SwitchAndSetModel { provider: String, model: String },
    /// `/model <target>` -> ganti ke provider `<target>`, cocokkan substring di daftar, atau set manual.
    SwitchOrModel(String),
}

pub fn parse_model_action(arg: &str) -> ModelAction {
    let arg = arg.trim();
    if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("reset") {
        return ModelAction::ClearModels;
    }
    if arg.eq_ignore_ascii_case("list") || arg.eq_ignore_ascii_case("ls") {
        return ModelAction::FetchList(None);
    }
    if let Some(rest) = arg
        .strip_prefix("list ")
        .or_else(|| arg.strip_prefix("LIST "))
        .or_else(|| arg.strip_prefix("ls "))
    {
        let f = rest.trim();
        return ModelAction::FetchList((!f.is_empty()).then(|| f.to_string()));
    }
    if let Ok(idx) = arg.parse::<usize>() {
        if idx >= 1 {
            return ModelAction::SelectIndex(idx);
        }
    }
    if let Some(rest) = arg.strip_prefix("set ").or_else(|| arg.strip_prefix("SET ")) {
        let val = rest.trim();
        if let Ok(idx) = val.parse::<usize>() {
            if idx >= 1 {
                return ModelAction::SelectIndex(idx);
            }
        }
        if !val.is_empty() {
            return ModelAction::SetCurrentModel(val.to_string());
        }
    }
    if let Some(rest) = arg.strip_prefix("add ").or_else(|| arg.strip_prefix("ADD ")) {
        let items: Vec<String> = rest
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if !items.is_empty() {
            return ModelAction::AddModels(items);
        }
    }
    if let Some(rest) = arg
        .strip_prefix("rm ")
        .or_else(|| arg.strip_prefix("RM "))
        .or_else(|| arg.strip_prefix("remove "))
        .or_else(|| arg.strip_prefix("del "))
    {
        let target = rest.trim();
        if target.eq_ignore_ascii_case("all") || target == "*" {
            return ModelAction::ClearModels;
        }
        if !target.is_empty() {
            return ModelAction::RemoveModel(target.to_string());
        }
    }
    if let Some((prov, model)) = arg.split_once(':') {
        let prov = prov.trim();
        let model = model.trim();
        if !prov.is_empty() && !model.is_empty() && !prov.contains('/') && !prov.contains('.') {
            return ModelAction::SwitchAndSetModel {
                provider: prov.to_string(),
                model: model.to_string(),
            };
        }
    }
    ModelAction::SwitchOrModel(arg.to_string())
}

/// Spesifikasi kunci API dari `/provider add` / `/provider preset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    EnvVar(String),
    DirectKey(String),
}

pub fn classify_key_spec(raw: &str) -> KeySpec {
    let raw = raw.trim();
    if let Some(env_name) = raw.strip_prefix("env:").or_else(|| raw.strip_prefix("ENV:")) {
        return KeySpec::EnvVar(env_name.trim().to_string());
    }
    let looks_like_env = raw.len() >= 3
        && raw.contains('_')
        && raw
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if looks_like_env {
        KeySpec::EnvVar(raw.to_string())
    } else {
        KeySpec::DirectKey(raw.to_string())
    }
}

/// Preset provider populer agar user tidak perlu menghafal `base_url`.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub kind: &'static str,
    pub base_url: Option<&'static str>,
    pub default_model: &'static str,
    pub default_env: &'static str,
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openrouter",
        kind: "openai",
        base_url: Some("https://openrouter.ai/api/v1"),
        default_model: "anthropic/claude-3.7-sonnet",
        default_env: "OPENROUTER_API_KEY",
    },
    ProviderPreset {
        id: "9router",
        kind: "openai",
        base_url: Some("https://router.ryukomik.com/v1"),
        default_model: "ag/gemini-3-flash",
        default_env: "SELENCODE_ROUTER_KEY",
    },
    ProviderPreset {
        id: "groq",
        kind: "openai",
        base_url: Some("https://api.groq.com/openai/v1"),
        default_model: "llama-3.3-70b-versatile",
        default_env: "GROQ_API_KEY",
    },
    ProviderPreset {
        id: "deepseek",
        kind: "openai",
        base_url: Some("https://api.deepseek.com/v1"),
        default_model: "deepseek-chat",
        default_env: "DEEPSEEK_API_KEY",
    },
    ProviderPreset {
        id: "ollama",
        kind: "openai",
        base_url: Some("http://localhost:11434/v1"),
        default_model: "qwen2.5-coder:7b",
        default_env: "",
    },
    ProviderPreset {
        id: "anthropic",
        kind: "anthropic",
        base_url: None,
        default_model: "claude-sonnet-4-20250514",
        default_env: "ANTHROPIC_API_KEY",
    },
    ProviderPreset {
        id: "gemini",
        kind: "gemini",
        base_url: None,
        default_model: "gemini-2.5-pro",
        default_env: "GEMINI_API_KEY",
    },
];

pub fn find_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(id))
}

/// Aksi hasil parsing `/provider [subcommand ...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    List,
    ListPresets,
    Add {
        name: String,
        kind: String,
        model: String,
        base_url: Option<String>,
        key_spec: KeySpec,
    },
    SetModel {
        name: String,
        model: String,
    },
    SetDefault(String),
    Remove(String),
    Usage,
}

pub fn parse_provider_action(arg: Option<&str>) -> ProviderAction {
    let Some(arg) = arg.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return ProviderAction::List;
    };
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    match tokens.first().copied().unwrap_or("") {
        "list" | "ls" => ProviderAction::List,
        "presets" => ProviderAction::ListPresets,
        "preset" => {
            if tokens.len() == 1 {
                return ProviderAction::ListPresets;
            }
            let Some(p) = find_preset(tokens[1]) else {
                return ProviderAction::ListPresets;
            };
            let model = tokens
                .get(2)
                .copied()
                .unwrap_or(p.default_model)
                .to_string();
            let key_spec = match tokens.get(3).copied() {
                Some(raw) => classify_key_spec(raw),
                None if p.id == "ollama" => KeySpec::DirectKey("ollama".to_string()),
                None => KeySpec::EnvVar(p.default_env.to_string()),
            };
            ProviderAction::Add {
                name: p.id.to_string(),
                kind: p.kind.to_string(),
                model,
                base_url: p.base_url.map(|s| s.to_string()),
                key_spec,
            }
        }
        "default" if tokens.len() == 2 => ProviderAction::SetDefault(tokens[1].to_string()),
        "rm" | "remove" | "del" if tokens.len() == 2 => {
            ProviderAction::Remove(tokens[1].to_string())
        }
        "model" if tokens.len() >= 3 => ProviderAction::SetModel {
            name: tokens[1].to_string(),
            model: tokens[2].to_string(),
        },
        "add" => {
            if tokens.len() < 5 {
                return ProviderAction::Usage;
            }
            let name = tokens[1].to_string();
            let kind = tokens[2].to_ascii_lowercase();
            let model = tokens[3].to_string();
            if tokens.len() >= 6 {
                let raw_url = tokens[4];
                let base_url = if raw_url == "-" || raw_url.eq_ignore_ascii_case("default") {
                    None
                } else {
                    Some(raw_url.to_string())
                };
                let key_spec = classify_key_spec(tokens[5]);
                ProviderAction::Add {
                    name,
                    kind,
                    model,
                    base_url,
                    key_spec,
                }
            } else if kind == "anthropic" || kind == "gemini" {
                ProviderAction::Add {
                    name,
                    kind,
                    model,
                    base_url: None,
                    key_spec: classify_key_spec(tokens[4]),
                }
            } else {
                ProviderAction::Usage
            }
        }
        _ => ProviderAction::Usage,
    }
}

/// Kartu bantuan `/help` memakai markup menu terstruktur (`#`, `>`, `---`, `!`).
pub fn help_menu_markup() -> String {
    let mut out = String::from("# Daftar Perintah SelenCode\n");
    for c in COMMANDS {
        out.push_str(&format!("> {} | {} | {}\n", c.cmd, c.args, c.desc));
    }
    out.push_str("---\n");
    out.push_str("# Panduan Cepat Kelola Model\n");
    out.push_str("* /model | interaktif | ↑↓ pilih · Enter pakai · a tambah manual · f katalog API · d hapus · c reset\n");
    out.push_str("> /model add | <id_model> | tambah 1 model manual (mis. /model add ag/claude-sonnet-4)\n");
    out.push_str("> /model 2 | atau kata | ganti cepat ke nomor #2 atau potongan nama\n");
    out.push_str("> /model rm 2 | /model clear | hapus 1 model (#2) atau bersihkan semua daftar\n");
    out.push_str("> /models | [kata_cari] | intip katalog API lalu ambil 1-per-1: /model add 1 3\n");
    out.push_str("---\n");
    out.push_str("! Interupsi cepat: ketik pesan baru + Enter saat Selena bekerja, atau /stop / Esc.\n");
    out.push_str("! Telegram Bot:  /telegram setup <TOKEN> <USER_ID> lalu jalankan `selen telegram`.\n");
    out.push_str("! File & shell:  @src/main.rs (lampirkan file) · !cargo test (shell instan).\n");
    out.push_str("! Keyboard TUI:  / + ↑↓ + Tab palette · Alt+Enter baris baru · Ctrl+C keluar.");
    out
}

/// Teks bantuan polos (mode `--plain`), dibersihkan dari penanda kartu menu.
pub fn strip_menu_markup(markup: &str) -> String {
    let mut lines = Vec::new();
    for raw in markup.lines() {
        if let Some(t) = raw.strip_prefix("# ") {
            lines.push(format!("=== {t} ==="));
        } else if raw.trim() == "---" {
            lines.push("----------------------------------------".to_string());
        } else if let Some(rest) = raw.strip_prefix("* ").or_else(|| raw.strip_prefix("> ")) {
            let mark = if raw.starts_with("* ") { "*" } else { "-" };
            let parts: Vec<&str> = rest.split('|').map(|p| p.trim()).collect();
            let joined: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
            lines.push(format!("  {mark} {}", joined.join("  ·  ")));
        } else if let Some(tip) = raw.strip_prefix("! ") {
            lines.push(format!("  * {tip}"));
        } else {
            lines.push(raw.to_string());
        }
    }
    lines.join("\n")
}
