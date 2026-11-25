//! Plan mode (Fase 6, Blueprint Bagian 10).
//!
//! Di plan mode agent hanya BOLEH membaca/menjelajah: daftar tool yang
//! dikirim ke provider dibatasi ke tool baca-saja, dan bila model tetap
//! memanggil tool menulis, loop menolaknya tanpa menjalankan. Agent wajib
//! menutup giliran plan dengan rencana langkah bernomor di bawah judul
//! "## Rencana"; user menyetujui dulu, baru giliran eksekusi berjalan.

/// Mode 1 giliran agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    Execute,
    Plan,
}

/// Daftar tool BACA-SAJA terpusat — dipakai plan mode (filter specs +
/// penegakan di loop) dan registry anak subagent. `update_plan` SENGAJA
/// tidak ada di sini: di plan mode rencana masih berupa teks; checklist
/// update_plan adalah alat mode eksekusi. `delegate` juga tidak: subagent
/// tidak boleh mendelegasikan lagi, dan plan mode tidak menelurkan tugas.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "list_files",
    "read_file",
    "search_text",
    "outline",
    "retrieve_context",
    "lsp_diagnostics",
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "git_status",
    "git_diff",
    "fetch_url",
    "skills_list",
    "skill_view",
    "session_search",
    "ask_user",
];

pub fn is_read_only(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

/// Catatan sistem yang disisipkan di awal giliran plan.
pub const PLAN_NOTE: &str = "[MODE PLAN AKTIF — giliran ini baca-saja]\n\
Kamu sedang dalam plan mode. Tool yang tersedia untukmu HANYA tool baca-saja \
(membaca file, mencari, outline, git status/diff, fetch) — tool menulis, \
menjalankan perintah, dan commit TIDAK tersedia dan akan ditolak bila dicoba.\n\
Tugasmu giliran ini: jelajahi project seperlunya untuk memahami masalah, lalu \
tutup jawabanmu dengan rencana langkah bernomor di bawah judul persis \
\"## Rencana\". Jangan mencoba mengubah apa pun di giliran ini.";

/// Ekstrak teks rencana dari jawaban assistant giliran plan.
/// Aturan: bila ada penanda "## Rencana", ambil dari penanda itu sampai
/// akhir. Bila tidak ada penanda, seluruh jawaban dianggap rencana
/// (fungsi ini hanya dipanggil untuk giliran plan). Teks kosong -> None.
/// Fungsi murni, mudah dites.
pub fn extract_plan(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(idx) = text.find("## Rencana") {
        let plan = text[idx..].trim();
        if plan.is_empty() {
            return None;
        }
        return Some(plan.to_string());
    }
    Some(text.to_string())
}

/// Prompt giliran eksekusi sesudah rencana disetujui user.
pub fn approved_plan_prompt(plan: &str) -> String {
    format!("Rencana yang disetujui:\n{plan}\nJalankan langkah demi langkah.")
}
