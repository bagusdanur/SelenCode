//! Parsing perintah garis miring mode interaktif + katalog perintah tunggal
//! untuk palette live `/` di TUI dan tampilan `/help`.
//! Perintah garis miring TIDAK pernah diteruskan ke provider LLM.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Bukan perintah (teks biasa untuk agent).
    NotCommand,
    Exit,
    /// /model | /model <provider> | /model set <model> | /model <provider>:<model> | /model list
    Model(Option<String>),
    /// /models (pintasan /model list)
    Models,
    /// /provider | /provider add ... | /provider model ... | /provider default ... | /provider rm ...
    Provider(Option<String>),
    /// /sessions
    Sessions,
    /// /resume (di dalam sesi interaktif: tampilkan info sesi aktif)
    Resume,
    /// /plan (giliran berikutnya plan mode baca-saja)
    Plan,
    /// /new atau /clear (mulai sesi percakapan baru yang bersih)
    New,
    /// /compact (ringkas riwayat konteks secara manual)
    Compact,
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
        cmd: "/model",
        args: "[provider | set <id> | list]",
        desc: "lihat, ganti provider, atau input model manual",
        takes_args: true,
    },
    CommandDef {
        cmd: "/models",
        args: "",
        desc: "tarik daftar model dari API provider aktif",
        takes_args: false,
    },
    CommandDef {
        cmd: "/provider",
        args: "[add | model | default | rm]",
        desc: "kelola & pasang provider lain + API key",
        takes_args: true,
    },
    CommandDef {
        cmd: "/plan",
        args: "",
        desc: "giliran berikutnya baca-saja & minta izin rencana",
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
        args: "",
        desc: "tampilkan detail sesi aktif",
        takes_args: false,
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
    if !line.starts_with('/') {
        return SlashCommand::NotCommand;
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match head {
        "/exit" | "/quit" => SlashCommand::Exit,
        "/model" => SlashCommand::Model(rest),
        "/models" => SlashCommand::Models,
        "/provider" | "/connect" => SlashCommand::Provider(rest),
        "/sessions" => SlashCommand::Sessions,
        "/resume" => SlashCommand::Resume,
        "/plan" => SlashCommand::Plan,
        "/new" | "/clear" => SlashCommand::New,
        "/compact" => SlashCommand::Compact,
        "/help" | "/?" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.trim_start_matches('/').to_string()),
    }
}

/// Aksi hasil parsing argumen `/model <arg>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    /// `/model list` -> tarik daftar model dari provider aktif.
    FetchList,
    /// `/model set <model_id>` -> set model manual pada provider aktif & simpan.
    SetCurrentModel(String),
    /// `/model <provider>:<model_id>` -> pindah provider + set model manual & simpan.
    SwitchAndSetModel { provider: String, model: String },
    /// `/model <target>` -> ganti ke provider bernama `<target>` (atau set model bila bukan nama provider).
    SwitchOrModel(String),
}

pub fn parse_model_action(arg: &str) -> ModelAction {
    let arg = arg.trim();
    if arg.eq_ignore_ascii_case("list") || arg.eq_ignore_ascii_case("ls") {
        return ModelAction::FetchList;
    }
    if let Some(rest) = arg.strip_prefix("set ").or_else(|| arg.strip_prefix("SET ")) {
        let model = rest.trim();
        if !model.is_empty() {
            return ModelAction::SetCurrentModel(model.to_string());
        }
    }
    if let Some((prov, model)) = arg.split_once(':') {
        let prov = prov.trim();
        let model = model.trim();
        // Hindari memotong model yang memang memakai ':' (mis. Ollama `qwen2.5:7b`)
        // bila bagian kiri mengandung '/' atau titik.
        if !prov.is_empty() && !model.is_empty() && !prov.contains('/') && !prov.contains('.') {
            return ModelAction::SwitchAndSetModel {
                provider: prov.to_string(),
                model: model.to_string(),
            };
        }
    }
    ModelAction::SwitchOrModel(arg.to_string())
}

/// Spesifikasi kunci API dari `/provider add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySpec {
    EnvVar(String),
    DirectKey(String),
}

/// Klasifikasi argumen API key: nama env var (mis. `OPENROUTER_API_KEY` atau
/// `env:MY_KEY`) vs API key mentah (mis. `sk-or-v1-...`).
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

/// Aksi hasil parsing `/provider [subcommand ...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    List,
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
        "default" if tokens.len() == 2 => ProviderAction::SetDefault(tokens[1].to_string()),
        "rm" | "remove" | "del" if tokens.len() == 2 => {
            ProviderAction::Remove(tokens[1].to_string())
        }
        "model" if tokens.len() >= 3 => ProviderAction::SetModel {
            name: tokens[1].to_string(),
            model: tokens[2].to_string(),
        },
        "add" => {
            // Format 1 (openai/custom): /provider add <nama> <kind> <model> <base_url> <ENV_atau_key>
            // Format 2 (anthropic/gemini default URL): /provider add <nama> <kind> <model> <ENV_atau_key>
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
            } else {
                // 5 token: tokens[4] = key_spec (untuk anthropic/gemini dengan base_url default)
                if kind == "anthropic" || kind == "gemini" {
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
        }
        _ => ProviderAction::Usage,
    }
}

/// Kartu bantuan `/help` memakai markup menu terstruktur (`#`, `>`, `---`, `!`)
/// agar tampil berwarna & rapi di TUI, dan dibersihkan otomatis di mode polos.
pub fn help_menu_markup() -> String {
    let mut out = String::from("# Daftar Perintah SelenCode\n");
    for c in COMMANDS {
        out.push_str(&format!("> {} | {} | {}\n", c.cmd, c.args, c.desc));
    }
    out.push_str("---\n");
    out.push_str("! Pasang model manual:  /model set <id-model>   atau   /model <provider>:<id-model>\n");
    out.push_str("! Pasang provider baru: /provider add <nama> <openai|anthropic|gemini> <model> [base_url] <ENV|api_key>\n");
    out.push_str("! Shortcut keyboard:    Ketik / + ↑↓ + Tab untuk palette · Alt+Enter baris baru · Ctrl+C keluar");
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
