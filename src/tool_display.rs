//! Format tampilan aksi tool terstruktur (`agent/display.py::prepare_tool_preview` &
//! `gateway/platforms/base.py::format_tool_event`) + blok ringkasan akhir SelenCode (Perbaikan 3 v0.5.8).
//!
//! - Tiap aksi dirender sebagai `{tool} — {pratinjau}` (atau `{tool}` saja jika tanpa argumen utama).
//! - Argumen utama dipetakan per tool (meniru `_PRIMARY_ARGS` SelenCode), diredaksi dulu dari rahasia/token,
//!   dijadikan 1 baris, dan dipotong ekor di `tool_preview_length` (default 40 karakter).
//! - Blok ringkasan akhir menggabungkan pengulangan berturut-turut dengan pratinjau identik
//!   (`run_shell ×6 — job_queue.py show …`), mempertahankan total aksi asli di judul,
//!   membatasi ≤ 10 baris dengan seluruh baris GAGAL selalu tampil beserta alasan singkatnya,
//!   serta tanpa emoji dekoratif (aturan v0.5.0).

use crate::security::redact_secrets;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

pub const DEFAULT_TOOL_PREVIEW_LENGTH: usize = 40;
pub const DEFAULT_ACTION_BLOCK_MAX_LINES: usize = 10;

/// Satu kejadian aksi tool yang terekam sepanjang giliran untuk blok ringkasan akhir & ringkasan penutup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActionEvent {
    pub name: String,
    pub preview: String,
    pub is_error: bool,
    pub error_reason: Option<String>,
}

/// Redaksi tambahan khusus argumen CLI/JSON sebelum ditampilkan ke pengguna
/// (melengkapi `redact_secrets` untuk pola `--token xxx`, `--password xxx`, `KEY=val`, `Bearer xxx`).
pub fn redact_tool_args_for_display(raw: &str) -> String {
    let mut s = redact_secrets(raw);
    static FLAG_RE: OnceLock<Regex> = OnceLock::new();
    let flag_re = FLAG_RE.get_or_init(|| {
        Regex::new(
            r"(?i)(--?(?:api[-_]?key|token|secret|password|passwd|auth|bearer)(?:=|\s+))([^\s'\x22`]+)",
        )
        .expect("valid flag regex")
    });
    s = flag_re.replace_all(&s, "${1}[REDACTED]").into_owned();

    static ENV_RE: OnceLock<Regex> = OnceLock::new();
    let env_re = ENV_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b([A-Z0-9_]*(?:TOKEN|SECRET|API_KEY|APIKEY|PASSWORD|PASSWD|AUTH_KEY)[A-Z0-9_]*)=([^\s'\x22`]+)",
        )
        .expect("valid env regex")
    });
    s = env_re.replace_all(&s, "${1}=[REDACTED]").into_owned();

    static TG_BOT_RE: OnceLock<Regex> = OnceLock::new();
    let tg_re = TG_BOT_RE
        .get_or_init(|| Regex::new(r"\b\d{7,12}:[A-Za-z0-9_-]{20,}\b").expect("valid tg regex"));
    s = tg_re.replace_all(&s, "[REDACTED]").into_owned();
    s
}

/// Ambil nilai argumen utama (peta argumen utama tool SelenCode).
fn extract_primary_arg(tool_name: &str, args: &Value) -> Option<String> {
    let get_str = |k: &str| -> Option<String> {
        args.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    match tool_name {
        "run_shell" | "bg_start" => get_str("command"),
        "read_file" | "write_file" | "edit_file" | "outline" | "send_file" | "lsp_diagnostics"
        | "lsp_definition" | "lsp_references" | "lsp_hover" | "lsp_rename" => get_str("path"),
        "list_files" => get_str("path").filter(|p| p != "."),
        "search_text" => get_str("pattern").or_else(|| get_str("query")),
        "skill_view" | "skill_create" | "skill_edit" | "manage_skill" => get_str("name"),
        "delegate" | "orchestrate" => get_str("goal").or_else(|| {
            args.get("tasks")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|t| t.get("prompt"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }),
        "fetch_url" => get_str("url"),
        "web_search" => get_str("query"),
        "manage_cron" => get_str("name").or_else(|| get_str("action")),
        "ask_user" => get_str("question"),
        "session_search" => get_str("query"),
        "watch_job" => get_str("label")
            .or_else(|| get_str("check_command"))
            .or_else(|| get_str("status_file")),
        "memory" | "manage_memory" => get_str("content")
            .or_else(|| get_str("selector"))
            .or_else(|| get_str("action")),
        "activate_toolset" => args.get("toolsets").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }).filter(|s| !s.is_empty()),
        "load_mcp_tools" => args.get("names").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }).filter(|s| !s.is_empty()),
        "verify_project" => get_str("scope"),
        "git_commit" => get_str("message"),
        "retrieve_context" => get_str("query"),
        "manage_task" => get_str("goal").or_else(|| get_str("action")),
        "bg_output" | "bg_status" | "bg_stop" => get_str("id"),
        _ => {
            // Untuk tool MCP/kustom: coba kunci paling umum (command/path/query/url/name).
            for k in ["command", "path", "query", "url", "name", "pattern", "goal"] {
                if let Some(v) = get_str(k) {
                    return Some(v);
                }
            }
            None
        }
    }
}

/// Siapkan pratinjau 1 baris teredaksi dari argumen utama tool, dipotong ekor di `max_len` char.
pub fn prepare_tool_preview(tool_name: &str, args: &Value, max_len: usize) -> String {
    let Some(raw) = extract_primary_arg(tool_name, args) else {
        return String::new();
    };
    let redacted = redact_tool_args_for_display(&raw);
    let one_line: String = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_tail(&one_line, max_len.max(12))
}

/// Format 1 kejadian tool terstruktur (`{tool} — {pratinjau}` atau `{tool}` saja jika tanpa pratinjau).
pub fn format_tool_event(tool_name: &str, args: &Value, max_len: usize) -> String {
    let preview = prepare_tool_preview(tool_name, args, max_len);
    format_tool_event_from_preview(tool_name, &preview)
}

/// Format 1 kejadian tool dari pratinjau yang sudah disiapkan & diredaksi.
pub fn format_tool_event_from_preview(tool_name: &str, preview: &str) -> String {
    let p = preview.trim();
    if p.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} — {p}")
    }
}

/// Ringkas alasan error singkat (1 baris teredaksi, maks ~60 char) untuk baris gagal di blok aksi.
pub fn short_error_reason(output: &str) -> String {
    let clean = redact_tool_args_for_display(output);
    let first = clean
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("gagal");
    let stripped = first
        .strip_prefix("ERROR:")
        .or_else(|| first.strip_prefix("Error:"))
        .unwrap_or(first)
        .trim();
    truncate_tail(stripped, 60)
}

#[derive(Debug, Clone)]
struct CollapsedRow {
    name: String,
    preview: String,
    count: usize,
    is_error: bool,
    error_reason: Option<String>,
}

impl CollapsedRow {
    fn render(&self) -> String {
        let head = if self.count > 1 {
            format!("{} ×{}", self.name, self.count)
        } else {
            self.name.clone()
        };
        let mut line = if self.preview.is_empty() {
            head
        } else {
            format!("{head} — {}", self.preview)
        };
        if self.is_error {
            let reason = self
                .error_reason
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("error");
            line.push_str(&format!(" (gagal: {reason})"));
        }
        line
    }
}

/// Susun baris-baris blok ringkasan akhir `Aksi Tool (N)` (Perbaikan 3 v0.5.8):
/// - Giliran kecil (`events.len() <= 3`): tampilkan semua baris utuh tanpa penggabungan paksa.
/// - Giliran > 3 aksi: gabungkan pengulangan berturut-turut yang sukses dengan `(name, preview)` identik
///   menjadi `{tool} ×N — {pratinjau}`.
/// - Batas `max_lines` (default 10): baris GAGAL selalu dipertahankan dengan alasan singkatnya;
///   bila masih melebihi batas, pengulangan/baris sukses di tengah diringkas menjadi 1 baris sisa.
/// - Mengembalikan `(total_actions, rendered_lines)` bebas emoji dekoratif.
pub fn build_action_summary_lines(
    events: &[ToolActionEvent],
    max_lines: usize,
) -> (usize, Vec<String>) {
    let total = events.len();
    if total == 0 {
        return (0, Vec::new());
    }
    let limit = max_lines.max(3);

    // Giliran kecil <= 3 aksi: tampil apa adanya tanpa penggabungan paksa.
    if total <= 3 {
        let rows: Vec<String> = events
            .iter()
            .map(|e| {
                CollapsedRow {
                    name: e.name.clone(),
                    preview: e.preview.clone(),
                    count: 1,
                    is_error: e.is_error,
                    error_reason: e.error_reason.clone(),
                }
                .render()
            })
            .collect();
        return (total, rows);
    }

    // Gabungkan pengulangan berturut-turut yang sukses dengan (name, preview) identik.
    let mut collapsed: Vec<CollapsedRow> = Vec::new();
    for ev in events {
        if !ev.is_error {
            if let Some(last) = collapsed.last_mut() {
                if !last.is_error && last.name == ev.name && last.preview == ev.preview {
                    last.count += 1;
                    continue;
                }
            }
        }
        collapsed.push(CollapsedRow {
            name: ev.name.clone(),
            preview: ev.preview.clone(),
            count: 1,
            is_error: ev.is_error,
            error_reason: ev.error_reason.clone(),
        });
    }

    if collapsed.len() <= limit {
        return (total, collapsed.iter().map(CollapsedRow::render).collect());
    }

    // Jika masih > limit: PERTAHANKAN semua baris gagal, lalu isi sisa slot dengan baris sukses
    // terawal & terakhir (korbankan pengulangan sukses di tengah), ditutup baris "... dan N aksi sukses lainnya".
    let err_indices: Vec<usize> = collapsed
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.is_error.then_some(i))
        .collect();

    // Bila error sendiri >= limit, tampilkan hingga `limit` error pertama (terakhir diprioritaskan).
    if err_indices.len() >= limit {
        let take_n = limit.saturating_sub(1);
        let mut out: Vec<String> = err_indices
            .iter()
            .take(take_n)
            .map(|&i| collapsed[i].render())
            .collect();
        let omitted_actions: usize = collapsed
            .iter()
            .enumerate()
            .filter(|(i, _)| !err_indices.iter().take(take_n).any(|&k| k == *i))
            .map(|(_, r)| r.count)
            .sum();
        out.push(format!("… dan {omitted_actions} aksi lainnya"));
        return (total, out);
    }

    // Slot tersisa untuk baris sukses (sisakan 1 slot untuk baris ringkasan "... dan X aksi sukses lainnya").
    let ok_slots = limit.saturating_sub(err_indices.len()).saturating_sub(1);
    let ok_indices: Vec<usize> = collapsed
        .iter()
        .enumerate()
        .filter_map(|(i, r)| (!r.is_error).then_some(i))
        .collect();

    let mut kept_set = std::collections::HashSet::new();
    for &i in &err_indices {
        kept_set.insert(i);
    }
    if ok_slots > 0 && !ok_indices.is_empty() {
        let head_n = (ok_slots + 1) / 2;
        let tail_n = ok_slots.saturating_sub(head_n);
        for &i in ok_indices.iter().take(head_n) {
            kept_set.insert(i);
        }
        for &i in ok_indices.iter().rev().take(tail_n) {
            kept_set.insert(i);
        }
    }

    let mut out = Vec::with_capacity(limit);
    let mut omitted_ok_actions = 0usize;
    let mut omitted_inserted = false;
    for (idx, row) in collapsed.iter().enumerate() {
        if kept_set.contains(&idx) {
            out.push(row.render());
        } else {
            omitted_ok_actions += row.count;
            if !omitted_inserted {
                // Tandai posisi penyisipan ringkasan tengah.
                out.push("__OMITTED_PLACEHOLDER__".to_string());
                omitted_inserted = true;
            }
        }
    }
    for line in &mut out {
        if line == "__OMITTED_PLACEHOLDER__" {
            *line = format!("… dan {omitted_ok_actions} aksi sukses lainnya");
        }
    }
    (total, out)
}

/// Susun ringkasan penutup templat ketika giliran berhenti mendadak & model tidak dapat dipanggil lagi
/// (Perbaikan 2 v0.5.8 + Perbaikan 1 v0.5.14 — selaras `MAX_STEPS_PROMPT` opencode & pelabelan jujur SelenCode).
pub fn build_template_closing_summary(events: &[ToolActionEvent], stop_reason: &str) -> String {
    build_template_closing_summary_with_status(events, stop_reason, false)
}

/// Varian ringkasan penutup templat dengan penanda eksplisit kegagalan nol-edit di akhir pagu (Perbaikan 1 #3 v0.5.14).
pub fn build_template_closing_summary_with_status(
    events: &[ToolActionEvent],
    stop_reason: &str,
    zero_edit_failure: bool,
) -> String {
    let total = events.len();
    let ok_count = events.iter().filter(|e| !e.is_error).count();
    let fail_count = total.saturating_sub(ok_count);

    let mut out = String::new();
    if zero_edit_failure {
        out.push_str("Gagal: pagu langkah habis sebelum ada perubahan apa pun dibuat.\n\n");
    }
    out.push_str(&format!(
        "Ringkasan penutup ({stop_reason}):\n\
         - Sudah dikerjakan: {total} aksi tool ({ok_count} berhasil, {fail_count} gagal).\n"
    ));
    let (_, lines) = build_action_summary_lines(events, 6);
    if !lines.is_empty() {
        out.push_str("- Aksi utama:\n");
        for l in lines {
            out.push_str(&format!("  • {l}\n"));
        }
    }
    if zero_edit_failure {
        out.push_str(
            "- Belum selesai: belum ada satu pun berkas yang diubah atau diverifikasi sebelum pagu langkah habis.\n",
        );
    } else if fail_count > 0 {
        if let Some(last_err) = events.iter().rev().find(|e| e.is_error) {
            out.push_str(&format!(
                "- Belum selesai / kendala terakhir: {} ({})\n",
                format_tool_event_from_preview(&last_err.name, &last_err.preview),
                last_err.error_reason.as_deref().unwrap_or("error")
            ));
        }
    } else {
        out.push_str("- Belum selesai: tugas terpotong sebelum penutupan normal selesai.\n");
    }
    out.push_str(
        "- Langkah berikutnya: lanjutkan sesi ini dengan instruksi spesifik atau naikkan batas langkah bila tugas belum selesai.",
    );
    out
}

fn truncate_tail(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return s.chars().take(max_chars).collect();
    }
    let head: String = s.chars().take(max_chars - 1).collect();
    format!("{head}…")
}
