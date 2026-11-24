/// Parsing perintah garis miring mode interaktif (Fase 3).
/// Fungsi murni terpisah dari REPL agar bisa dites: perintah garis miring
/// TIDAK pernah diteruskan ke provider.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Bukan perintah (teks biasa untuk agent).
    NotCommand,
    Exit,
    /// /model atau /model <nama-provider>
    Model(Option<String>),
    /// /sessions
    Sessions,
    /// /resume (di dalam sesi interaktif: tampilkan info sesi aktif)
    Resume,
    /// /plan (Fase 6: giliran berikutnya plan mode baca-saja)
    Plan,
    /// Perintah garis miring yang tidak dikenal (nama tanpa '/').
    Unknown(String),
}

pub fn parse_slash_command(line: &str) -> SlashCommand {
    let line = line.trim();
    if !line.starts_with('/') {
        return SlashCommand::NotCommand;
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts.next().map(|s| s.trim().to_string());
    match head {
        "/exit" | "/quit" => SlashCommand::Exit,
        "/model" => SlashCommand::Model(rest.filter(|s| !s.is_empty())),
        "/sessions" => SlashCommand::Sessions,
        "/resume" => SlashCommand::Resume,
        "/plan" => SlashCommand::Plan,
        other => SlashCommand::Unknown(other.trim_start_matches('/').to_string()),
    }
}
