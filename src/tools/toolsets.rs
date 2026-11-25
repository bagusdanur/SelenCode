//! Progressive Disclosure & Tiered Toolsets (terinspirasi Hermes Agent `toolsets.py`).
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

/// Daftar paket toolset tambahan yang dapat diaktifkan secara dinamis.
pub const EXTENDED_TOOLSETS: &[(&str, &str, &[&str])] = &[
    ("git_read", "status & diff repositori git", &["git_status", "git_diff"]),
    ("verify", "verifikasi build/test/lint proyek", &["verify_project"]),
    ("git_write", "commit perubahan git", &["git_commit"]),
    ("code_nav", "outline AST & Smart Context Index", &["outline", "retrieve_context"]),
    (
        "lsp",
        "diagnostik, definisi, referensi, hover, & rename simbol LSP",
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
        "proses latar belakang (dev server/watcher)",
        &["bg_start", "bg_output", "bg_status", "bg_stop"],
    ),
    ("web", "ambil isi halaman URL", &["fetch_url"]),
    (
        "plan",
        "checklist rencana & tugas panjang lintas sesi",
        &["update_plan", "manage_task"],
    ),
    (
        "skills_view",
        "muat isi skill & file referensi on-demand",
        &["skill_view"],
    ),
    (
        "skills_mem",
        "daftar, buat, edit skill & memori proyek persisten",
        &[
            "skills_list",
            "skill_view",
            "skill_create",
            "skill_edit",
            "manage_skill",
            "manage_memory",
        ],
    ),
    (
        "multi_agent",
        "subagent paralel & orkestrasi multi-peran",
        &["delegate", "orchestrate"],
    ),
];

/// Spesifikasi meta-tool `activate_toolset` yang sangat ramping (~75 token).
pub fn activate_toolset_spec() -> ToolSpec {
    ToolSpec {
        name: ACTIVATE_TOOLSET_NAME.to_string(),
        description: "Aktifkan toolset tambahan secara on-demand: git_read, verify, git_write, code_nav, lsp, bg, web, plan, skills_mem, multi_agent, mcp.".to_string(),
        parameters: schema(
            json!({
                "toolsets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Daftar nama toolset yang ingin diaktifkan untuk giliran ini."
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

    if ["http://", "https://", "fetch_url", "buka url", "ambil halaman", "website"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        set.insert("web".to_string());
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

    if has_mcp
        && (lower.contains("mcp")
            || lower.contains("resource")
            || lower.contains("prompt"))
    {
        set.insert("mcp".to_string());
    }

    set
}

/// Cek apakah nama tool termasuk dalam paket aktif saat ini.
pub fn is_tool_active(tool_name: &str, active_toolsets: &HashSet<String>) -> bool {
    if CORE_TOOLS.contains(&tool_name) {
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

/// Susun indeks 1 baris (nama + deskripsi maks 60 char) untuk tool MCP ketika jumlahnya > 15 (Tahap 6).
pub fn build_mcp_compact_index(registry: &Registry) -> Option<String> {
    let mcp_specs: Vec<ToolSpec> = registry.specs_where(|n| n.starts_with("mcp__"));
    if mcp_specs.len() <= MCP_ON_DEMAND_THRESHOLD {
        return None;
    }
    let mut out = format!(
        "Indeks MCP ({} tool — muat skema penuh lewat `{LOAD_MCP_TOOLS_NAME}` bila perlu):\n",
        mcp_specs.len()
    );
    for s in mcp_specs {
        let short_desc: String = s
            .description
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(60)
            .collect();
        out.push_str(&format!("- {}: {short_desc}\n", s.name));
    }
    Some(out.trim_end().to_string())
}

/// Susun daftar `ToolSpec` ramping yang benar-benar dikirim ke provider berdasarkan `active_toolsets`
/// dan daftar `loaded_mcp_tools` (untuk mode indeks MCP >15 tool).
pub fn active_tool_specs_with_mcp(
    registry: &Registry,
    active_toolsets: &HashSet<String>,
    loaded_mcp_tools: &HashSet<String>,
    mode: TurnMode,
) -> Vec<ToolSpec> {
    let mcp_count = registry
        .names()
        .iter()
        .filter(|n| n.starts_with("mcp__"))
        .count();
    let mcp_indexed_mode = mcp_count > MCP_ON_DEMAND_THRESHOLD;

    let mut specs = match mode {
        TurnMode::Plan => registry.specs_where(|n| is_read_only(n) && is_tool_active(n, active_toolsets)),
        TurnMode::Execute => registry.specs_where(|n| {
            if n.starts_with("mcp__") && mcp_indexed_mode {
                loaded_mcp_tools.contains(n)
            } else {
                is_tool_active(n, active_toolsets)
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

/// Susun daftar `ToolSpec` ramping yang benar-benar dikirim ke provider berdasarkan `active_toolsets`.
pub fn active_tool_specs(
    registry: &Registry,
    active_toolsets: &HashSet<String>,
    mode: TurnMode,
) -> Vec<ToolSpec> {
    active_tool_specs_with_mcp(registry, active_toolsets, &HashSet::new(), mode)
}

/// Ringkasan 1 baris toolset yang tersedia untuk disisipkan murah ke system prompt (~50 token).
pub fn inactive_toolsets_hint(registry: &Registry, active_toolsets: &HashSet<String>) -> String {
    let reg_names = registry.names();
    let mut avail = Vec::new();
    for (name, desc, members) in EXTENDED_TOOLSETS {
        if !active_toolsets.contains(*name)
            && members.iter().any(|m| reg_names.iter().any(|r| r == m))
        {
            avail.push(format!("{name} ({desc})"));
        }
    }
    if !active_toolsets.contains("mcp") && reg_names.iter().any(|n| n.starts_with("mcp__")) {
        avail.push("mcp (tool server MCP eksternal)".to_string());
    }
    if avail.is_empty() {
        String::new()
    } else {
        format!(
            "Toolset opsional (aktifkan via `{ACTIVATE_TOOLSET_NAME}` hanya bila perlu): {}.",
            avail.join("; ")
        )
    }
}
