//! Progressive Disclosure & Tiered Toolsets (desain internal SelenCode).
//!
//! Menghemat 70–85% token schema tool per panggilan LLM dengan cara:
//! 1. Hanya mengirim **Core Toolset** (8 tool inti + meta-tool `activate_toolset`) secara default.
//! 2. Mengaktifkan **Extended Toolsets** otomatis bila prompt/workspace membutuhkannya
//!    (zero extra turn untuk intent yang jelas).
//! 3. Memungkinkan model mengaktifkan toolset tambahan kapan saja di tengah giliran
//!    lewat `activate_toolset(toolsets=[...])` tanpa kehilangan kemampuan apa pun.

use super::{schema, Registry};
use crate::agent::plan::{is_read_only, TurnMode};
use crate::providers::ToolSpec;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

pub const ACTIVATE_TOOLSET_NAME: &str = "activate_toolset";
pub const LOAD_MCP_TOOLS_NAME: &str = "load_mcp_tools";
/// Tahap 6: bila jumlah tool MCP terdaftar > 15, kirim indeks ringkas + pemuat skema on-demand!
pub const MCP_ON_DEMAND_THRESHOLD: usize = 15;

/// 6 tool paling esensial yang selalu aktif di setiap giliran Execute (+ `activate_toolset` = 7 tool).
/// `git_status`/`git_diff` otomatis aktif hanya bila folder adalah repo Git (`.git` ada) atau diminta,
/// sehingga tugas di folder polos hemat 2 skema tool (~400–600 token aktual) di setiap panggilan API!
pub const CORE_TOOLS: &[&str] = &[
    "list_files",
    "read_file",
    "write_file",
    "edit_file",
    "search_text",
    "run_shell",
];

/// Tool inti super-ramping untuk giliran RESUME (Perbaikan 1 v0.5.7):
/// menghemat ~2–4 skema tool pada panggilan pertama resume sambil tetap memungkinkan
/// pemuatan tool apa pun (`list_files`, `skill_view`, `session_search`, `watch_job`, dll.)
/// di giliran yang sama lewat `activate_toolset`.
pub const RESUME_CORE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "search_text",
    "run_shell",
];

/// Daftar paket toolset tambahan yang dapat diaktifkan secara dinamis.
pub const EXTENDED_TOOLSETS: &[(&str, &str, &[&str])] = &[
    ("fs_list", "daftar file workspace", &["list_files"]),
    ("git_read", "status/diff git", &["git_status", "git_diff"]),
    ("verify", "build/test/lint proyek", &["verify_project"]),
    ("git_write", "commit git", &["git_commit"]),
    ("code_nav", "outline AST & indeks repo", &["outline", "retrieve_context"]),
    (
        "lsp",
        "diagnostik/definisi/refs/rename LSP",
        &[
            "lsp_diagnostics",
            "lsp_definition",
            "lsp_references",
            "lsp_hover",
            "lsp_rename",
        ],
    ),
    (
        "bg",
        "proses latar (server/watcher)",
        &["bg_start", "bg_output", "bg_status", "bg_stop"],
    ),
    ("web", "ambil URL & cari web", &["fetch_url", "web_search"]),
    ("cron", "jadwal otomatis persisten", &["manage_cron"]),
    ("plan", "rencana & task panjang", &["update_plan", "manage_task"]),
    ("skills_view", "muat isi skill", &["skill_view"]),
    (
        "skills_mem",
        "skill & memori persisten/profil",
        &[
            "skills_list",
            "skill_view",
            "skill_create",
            "skill_edit",
            "manage_skill",
            "memory",
        ],
    ),
    (
        "multi_agent",
        "subagent paralel baca/worktree",
        &["delegate", "orchestrate"],
    ),
    ("clarify", "tanya klarifikasi user", &["ask_user"]),
    ("history", "cari riwayat sesi FTS5", &["session_search"]),
    ("watch", "pantau job asinkron hingga selesai", &["watch_job"]),
    ("tg_files", "kirim berkas lampiran ke Telegram", &["send_file"]),
];

/// Spesifikasi meta-tool `activate_toolset` yang sangat ramping.
pub fn activate_toolset_spec() -> ToolSpec {
    ToolSpec {
        name: ACTIVATE_TOOLSET_NAME.to_string(),
        description: "Aktifkan toolset opsional: fs_list, git_read, verify, git_write, code_nav, lsp, bg, web, cron, plan, skills_view, skills_mem, multi_agent, clarify, history, watch, tg_files, mcp.".to_string(),
        parameters: schema(
            json!({
                "toolsets": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            }),
            &["toolsets"],
        ),
    }
}

/// Pilih toolset awal secara pintar berdasarkan intent prompt & penanda workspace
/// sehingga tugas ringan hanya mengirim ~9 tool (~700 token), sementara tugas kompleks
/// langsung mendapat tool yang dibutuhkan tanpa langkah ekstra.
pub fn select_initial_toolsets(
    prompt: &str,
    cwd: &Path,
    has_mcp: bool,
    mode: TurnMode,
) -> HashSet<String> {
    let mut set = HashSet::new();
    let lower = prompt.to_ascii_lowercase();

    if mode == TurnMode::Plan {
        set.insert("code_nav".to_string());
    }

    // `verify` hanya aktif otomatis bila workspace punya penanda proyek DAN prompt menyangkut kode/test.
    let is_code_task = [
        "fix", "bug", "perbaiki", "refactor", "test", "tes", "build", "compile", "cargo", "npm",
        "pytest", "lint", "verify", "verifikasi", "fitur", "feature", "implement", "ubah", "edit",
        "tambah", "update", "optim",
    ]
    .iter()
    .any(|kw| lower.contains(kw));
    if is_code_task && crate::tools::verify::has_project_markers(cwd) {
        set.insert("verify".to_string());
    }

    // `git_read` otomatis aktif bila folder benar-benar repo Git ATAU prompt menyebut git/diff/commit.
    if cwd.join(".git").exists()
        || ["git ", "git_", "commit", "diff", "branch", "staged"]
            .iter()
            .any(|kw| lower.contains(kw))
    {
        set.insert("git_read".to_string());
    }

    if ["commit", "git ", "git_commit", "staged", "branch"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        set.insert("git_write".to_string());
    }

    // JANGAN mengaktifkan `code_nav` atau `plan` hanya karena panjang prompt (prompt 180+ char
    // untuk tugas 1 file atau `/resume` tidak butuh 4 schema tambahan di setiap panggilan API!).
    if [
        "arsitektur",
        "struktur repo",
        "struktur proyek",
        "refactor besar",
        "outline",
        "retrieve_context",
        "seluruh repo",
        "codebase",
        "cari fungsi",
        "jelaskan project",
        "jelaskan proyek",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("code_nav".to_string());
    }

    if [
        "lsp",
        "rename",
        "referensi",
        "references",
        "definition",
        "definisi",
        "hover",
        "diagnostik",
        "diagnostics",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("lsp".to_string());
    }

    if [
        "background",
        "dev server",
        "watcher",
        "daemon",
        "bg_",
        "jalan di belakang",
        "latar belakang",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("bg".to_string());
    }

    if [
        "http://",
        "https://",
        "fetch_url",
        "web_search",
        "buka url",
        "ambil halaman",
        "website",
        "cari di web",
        "cari online",
        "search web",
        "googling",
        "dokumentasi terbaru",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("web".to_string());
    }

    if [
        "cron",
        "manage_cron",
        "jadwalkan",
        "jadwal rutin",
        "tiap jam",
        "setiap hari",
        "setiap pagi",
        "pengingat rutin",
        "schedule",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("cron".to_string());
    }

    if [
        "rencana",
        "update_plan",
        "manage_task",
        "long-horizon",
        "roadmap",
        "checklist",
        "pemulihan task",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("plan".to_string());
    }

    if [
        "skill",
        "playbook",
        "referensi permanen",
        "memori",
        "memory",
        "manage_skill",
        "manage_memory",
        "/learn",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("skills_mem".to_string());
    }

    if [
        "delegate",
        "subagent",
        "paralel",
        "bandingkan",
        "compare",
        "orchestrate",
        "multi-agent",
        "reviewer",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("multi_agent".to_string());
    }

    if [
        "cari ",
        "unduh",
        "download",
        "manga",
        "komik",
        "klarifikasi",
        "ask_user",
        "kandidat",
        "ambigu",
        "maksudnya",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("clarify".to_string());
    }

    if [
        "sesi lalu",
        "sesi sebelumnya",
        "sesi lain",
        "cari sesi",
        "session_search",
        "riwayat sesi",
        "pernah kita bahas",
        "percakapan lama",
        "history",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("history".to_string());
    }

    if [
        "pantau",
        "watch",
        "antrean",
        "queue",
        "comic-auto",
        "thunder",
        "kerjain",
        "job_queue",
        "status file",
        "status job",
        "cek job",
        "cek status",
        "udah belum",
        "sudah belum",
        "sampai mana",
        "gimana job",
        "asinkron",
        "proses panjang",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("watch".to_string());
    }

    if [
        "kirim berkas",
        "kirim file",
        "kirim zip",
        "kirim laporan",
        "kirim gambar",
        "kirim ke chat",
        "kirim ke telegram",
        "lampirkan",
        "send_file",
        "send file",
        "attach",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("tg_files".to_string());
    }

    if [
        "preferensi",
        "ingat ",
        "simpan ke memori",
        "simpan memori",
        "profil",
        "profile",
        "selalu simpan",
        "selalu pakai",
        "selalu gunakan",
        "mulai sekarang",
        "biasakan",
        "jangan lupa",
        "catat untuk sesi",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("skills_mem".to_string());
    }

    // Jika ada server MCP terpasang di mode Execute, aktifkan toolset `mcp` otomatis tanpa
    // mewajibkan user menulis kata kunci "mcp" (untuk >15 tool tetap super ramping lewat
    // mode indeks + `load_mcp_tools`, dan untuk <=15 tool langsung siap pakai tanpa mutasi array `tools`).
    if has_mcp && mode == TurnMode::Execute {
        set.insert("mcp".to_string());
    }

    set
}

/// Pilih toolset awal super-ramping khusus untuk giliran RESUME (Perbaikan 1 v0.5.7):
/// - Tidak menyalakan `git_read` / `verify` / `skills_view` hanya karena penanda folder `.git` atau adanya skill;
///   hanya menyalakan toolset yang benar-benar diminta prompt saat ini!
/// - Semua tool lain tetap dapat diaktifkan di giliran yang sama lewat `activate_toolset`.
pub fn select_resume_initial_toolsets(
    prompt: &str,
    cwd: &Path,
    has_mcp: bool,
    mode: TurnMode,
) -> HashSet<String> {
    let mut set = select_initial_toolsets(prompt, cwd, has_mcp, mode);
    let lower = prompt.to_ascii_lowercase();
    // Pada sesi resume, jangan otomatis membawa `git_read` hanya karena `.git` ada di folder
    // kecuali prompt benar-benar menyebut git/diff/commit/branch!
    if !["git ", "git_", "commit", "diff", "branch", "staged"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        set.remove("git_read");
    }
    // `verify` tidak perlu di pertanyaan singkat resume kecuali prompt menyentuh build/test/fix.
    if lower.chars().count() < 60
        && !["test", "tes", "build", "compile", "cargo", "npm", "pytest", "verify", "verifikasi", "fix", "bug"]
            .iter()
            .any(|kw| lower.contains(kw))
    {
        set.remove("verify");
    }
    // Muat `skills_view` di resume bila prompt menyangkut debugging/TDD/review/desain/inspeksi/skill.
    if [
        "bug",
        "error",
        "salah",
        "debug",
        "perbaiki",
        "fix",
        "tdd",
        "review",
        "tinjau",
        "desain",
        "rencana",
        "inspeksi",
        "skill",
        "docker",
        "sitasi",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        set.insert("skills_view".to_string());
    }
    if ["daftar file", "list_files", "isi folder", "file apa saja"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        set.insert("fs_list".to_string());
    }
    set
}

/// Cek apakah nama tool termasuk dalam paket aktif saat ini (mode normal).
pub fn is_tool_active(tool_name: &str, active_toolsets: &HashSet<String>) -> bool {
    is_tool_active_for_turn(tool_name, active_toolsets, false)
}

/// Cek apakah nama tool termasuk dalam paket aktif saat ini (`lean_resume = true` memakai `RESUME_CORE_TOOLS`).
pub fn is_tool_active_for_turn(
    tool_name: &str,
    active_toolsets: &HashSet<String>,
    lean_resume: bool,
) -> bool {
    if lean_resume {
        if RESUME_CORE_TOOLS.contains(&tool_name) {
            return true;
        }
    } else if CORE_TOOLS.contains(&tool_name) {
        return true;
    }
    if tool_name.starts_with("mcp__") {
        return active_toolsets.contains("mcp");
    }
    for (set_name, _, members) in EXTENDED_TOOLSETS {
        if members.contains(&tool_name) {
            return active_toolsets.contains(*set_name);
        }
    }
    // Tool kustom lain yang tidak terdaftar di tabel extended tetap diizinkan aktif.
    true
}

/// Meta-tool pemuat skema MCP on-demand bila server MCP mendaftarkan >15 tool (Tahap 6).
pub fn load_mcp_tools_spec() -> ToolSpec {
    ToolSpec {
        name: LOAD_MCP_TOOLS_NAME.to_string(),
        description: "Muat skema JSON penuh untuk tool MCP tertentu dari indeks MCP saat dibutuhkan.".to_string(),
        parameters: schema(
            json!({
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Daftar nama tool MCP (contoh: [\"mcp__github__create_issue\"])."
                }
            }),
            &["names"],
        ),
    }
}

/// Batas jumlah tool MCP sebelum beralih dari daftar 1-baris-berdeskripsi ke daftar
/// terkelompok per server agar 100% nama tool (mis. 58–200+ tool) tetap terlihat utuh tanpa terpotong.
pub const MCP_GROUPED_INDEX_THRESHOLD: usize = 30;

/// Susun indeks ringkas untuk tool MCP ketika jumlahnya > 15 (Tahap 6 + perbaikan v0.5.23).
/// - 16..=30 tool: 1 baris per tool (`- mcp__srv__tool: deskripsi <=55 char`).
/// - >30 tool: dikelompokkan per server dengan nama penuh `mcp__<server>__<tool>` agar super hemat
///   token dan 100% nama tool dari seluruh server selalu terlihat oleh model (tidak pernah terpotong ekornya).
pub fn build_mcp_compact_index(registry: &Registry) -> Option<String> {
    let mcp_specs: Vec<ToolSpec> = registry.specs_where(|n| n.starts_with("mcp__"));
    if mcp_specs.len() <= MCP_ON_DEMAND_THRESHOLD {
        return None;
    }
    let total = mcp_specs.len();
    let mut out = format!(
        "Indeks MCP ({total} tool — muat skema penuh lewat `{LOAD_MCP_TOOLS_NAME}` bila perlu):\n"
    );
    if total <= MCP_GROUPED_INDEX_THRESHOLD {
        for s in mcp_specs {
            let short_desc: String = s
                .description
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(55)
                .collect();
            out.push_str(&format!("- {}: {short_desc}\n", s.name));
        }
    } else {
        let mut by_server: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for s in mcp_specs {
            let server = s
                .name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split("__").next())
                .unwrap_or("mcp")
                .to_string();
            by_server.entry(server).or_default().push(s.name);
        }
        for (server, mut names) in by_server {
            names.sort();
            out.push_str(&format!("- {server} ({}): {}\n", names.len(), names.join(", ")));
        }
    }
    Some(out.trim_end().to_string())
}

/// Susun daftar `ToolSpec` ramping yang benar-benar dikirim ke provider berdasarkan `active_toolsets`,
/// daftar `loaded_mcp_tools` (untuk mode indeks MCP >15 tool), dan mode `lean_resume` (Perbaikan 1 v0.5.7).
pub fn active_tool_specs_for_turn(
    registry: &Registry,
    active_toolsets: &HashSet<String>,
    loaded_mcp_tools: &HashSet<String>,
    mode: TurnMode,
    lean_resume: bool,
) -> Vec<ToolSpec> {
    let mcp_count = registry
        .names()
        .iter()
        .filter(|n| n.starts_with("mcp__"))
        .count();
    let mcp_indexed_mode = mcp_count > MCP_ON_DEMAND_THRESHOLD;

    let mut specs = match mode {
        TurnMode::Plan => registry.specs_where(|n| {
            is_read_only(n) && is_tool_active_for_turn(n, active_toolsets, lean_resume)
        }),
        TurnMode::Execute => registry.specs_where(|n| {
            if n.starts_with("mcp__") && mcp_indexed_mode {
                loaded_mcp_tools.contains(n)
            } else {
                is_tool_active_for_turn(n, active_toolsets, lean_resume)
            }
        }),
    };
    if mode == TurnMode::Execute && mcp_indexed_mode {
        specs.push(load_mcp_tools_spec());
    }
    // Tambahkan meta-tool `activate_toolset` bila masih ada toolset non-MCP tersembunyi di registry.
    let total_eligible = match mode {
        TurnMode::Plan => registry.specs_where(is_read_only).len(),
        TurnMode::Execute => registry
            .names()
            .iter()
            .filter(|n| !mcp_indexed_mode || !n.starts_with("mcp__"))
            .count(),
    };
    if specs.len() < total_eligible {
        specs.push(activate_toolset_spec());
    }
    specs
}

/// Susun daftar `ToolSpec` ramping yang benar-benar dikirim ke provider berdasarkan `active_toolsets`
/// dan daftar `loaded_mcp_tools` (untuk mode indeks MCP >15 tool).
pub fn active_tool_specs_with_mcp(
    registry: &Registry,
    active_toolsets: &HashSet<String>,
    loaded_mcp_tools: &HashSet<String>,
    mode: TurnMode,
) -> Vec<ToolSpec> {
    active_tool_specs_for_turn(registry, active_toolsets, loaded_mcp_tools, mode, false)
}

/// Susun daftar `ToolSpec` ramping yang benar-benar dikirim ke provider berdasarkan `active_toolsets`.
pub fn active_tool_specs(
    registry: &Registry,
    active_toolsets: &HashSet<String>,
    mode: TurnMode,
) -> Vec<ToolSpec> {
    active_tool_specs_with_mcp(registry, active_toolsets, &HashSet::new(), mode)
}

/// Ringkasan 1 baris ultra-padat toolset opsional (~60 token; hemat ~190 token/call dibanding deskripsi panjang).
pub fn inactive_toolsets_hint(registry: &Registry, active_toolsets: &HashSet<String>) -> String {
    let reg_names = registry.names();
    let mut avail = Vec::new();
    for (name, _, members) in EXTENDED_TOOLSETS {
        if !active_toolsets.contains(*name)
            && members.iter().any(|m| reg_names.iter().any(|r| r == m))
        {
            avail.push((*name).to_string());
        }
    }
    if !active_toolsets.contains("mcp") && reg_names.iter().any(|n| n.starts_with("mcp__")) {
        avail.push("mcp".to_string());
    }
    if avail.is_empty() {
        String::new()
    } else {
        format!(
            "Toolset opsional (`{ACTIVATE_TOOLSET_NAME}`): {}.",
            avail.join(", ")
        )
    }
}
