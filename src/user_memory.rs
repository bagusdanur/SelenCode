//! Memory pengguna persisten terstruktur (Perbaikan 5 v0.5.7):
//! Dua wadah terstruktur dengan pagu karakter keras & snapshot beku di awal sesi:
//! 1. `memory`  (`<data_dir>/selencode/memory/memory.md`,  pagu default 2.200 char):
//!    fakta, prosedur, dan preferensi yang dipelajari lintas sesi.
//! 2. `profile` (`<data_dir>/selencode/memory/profile.md`, pagu default 1.375 char):
//!    profil pemilik (kebiasaan, folder kanonik, preferensi kerja).
//!
//! Disiplin:
//! - Penulisan yang melebihi pagu karakter DITOLAK dengan pesan yang meminta merangkum dulu.
//! - Snapshot keduanya dibekukan saat sesi dimulai dan dimuat ke blok dinamis prompt apa adanya.
//! - Perintah `/memory` menampilkan isi PERSIS seperti yang termuat di prompt beserta rasio pagunya.
//! - Background review v0.5.1 menulis ke wadah yang sama ini (satu sumber kebenaran).

use crate::security::redact_secrets;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_MEMORY_CHARS: usize = 2_200;
pub const DEFAULT_MAX_PROFILE_CHARS: usize = 1_375;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryStoreKind {
    Memory,
    Profile,
}

impl MemoryStoreKind {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "memory" | "memori" | "notes" | "catatan" => Ok(Self::Memory),
            "profile" | "profil" | "user" | "pemilik" => Ok(Self::Profile),
            other => Err(anyhow!(
                "target '{other}' tidak dikenal — pilih 'memory' atau 'profile'"
            )),
        }
    }

    pub fn filename(self) -> &'static str {
        match self {
            Self::Memory => "memory.md",
            Self::Profile => "profile.md",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Profile => "profile",
        }
    }
}

/// Snapshot beku memori & profil yang dimuat sekali saat sesi dimulai.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrozenUserMemory {
    pub memory_entries: Vec<String>,
    pub profile_entries: Vec<String>,
    pub max_memory_chars: usize,
    pub max_profile_chars: usize,
}

impl FrozenUserMemory {
    pub fn memory_text(&self) -> String {
        render_entries(&self.memory_entries)
    }

    pub fn profile_text(&self) -> String {
        render_entries(&self.profile_entries)
    }

    pub fn is_empty(&self) -> bool {
        self.memory_entries.is_empty() && self.profile_entries.is_empty()
    }

    /// Format teks PERSIS yang disisipkan ke snapshot dinamis prompt (dan ditampilkan oleh `/memory`).
    pub fn format_for_prompt(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if !self.profile_entries.is_empty() {
            out.push_str("\n--- Profil Pemilik (Beku) ---\n");
            out.push_str(&self.profile_text());
            out.push('\n');
        }
        if !self.memory_entries.is_empty() {
            out.push_str("\n--- Memori Persisten (Beku) ---\n");
            out.push_str(&self.memory_text());
            out.push('\n');
        }
        out
    }
}

/// Direktori penyimpanan wadah memori persisten (`<data_dir>/selencode/memory`).
pub fn user_memory_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("SELENCODE_DATA_DIR") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("selencode").join("memory");
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("selencode")
        .join("memory")
}

fn contains_secret_like(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    let redacted = redact_secrets(text);
    if redacted != text {
        return true;
    }
    for pat in [
        "sk-",
        "ghp_",
        "gho_",
        "xoxb-",
        "aiza",
        "-----begin ",
        "password=",
        "api_key=",
        "bot_token=",
    ] {
        if low.contains(pat) {
            return true;
        }
    }
    false
}

pub fn parse_entries(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("- ").unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn render_entries(entries: &[String]) -> String {
    entries
        .iter()
        .map(|e| format!("- {}", e.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_char_count(entries: &[String]) -> usize {
    render_entries(entries).chars().count()
}

pub fn load_entries_from(dir: &Path, kind: MemoryStoreKind) -> Vec<String> {
    let path = dir.join(kind.filename());
    match std::fs::read_to_string(&path) {
        Ok(raw) => parse_entries(&raw),
        Err(_) => Vec::new(),
    }
}

pub fn load_frozen_snapshot_from(
    dir: &Path,
    max_memory_chars: usize,
    max_profile_chars: usize,
) -> FrozenUserMemory {
    FrozenUserMemory {
        memory_entries: load_entries_from(dir, MemoryStoreKind::Memory),
        profile_entries: load_entries_from(dir, MemoryStoreKind::Profile),
        max_memory_chars: max_memory_chars.max(200),
        max_profile_chars: max_profile_chars.max(200),
    }
}

pub fn load_frozen_snapshot(max_memory_chars: usize, max_profile_chars: usize) -> FrozenUserMemory {
    load_frozen_snapshot_from(&user_memory_dir(), max_memory_chars, max_profile_chars)
}

fn save_entries_to(dir: &Path, kind: MemoryStoreKind, entries: &[String], max_chars: usize) -> Result<usize> {
    let rendered = render_entries(entries);
    let len = rendered.chars().count();
    if len > max_chars {
        return Err(anyhow!(
            "pagu wadah '{}' terlampaui ({len}/{max_chars} karakter). Rangkum atau hapus catatan lama terlebih dahulu dengan `memory(action=\"replace\"|\"remove\")` sebelum menambah catatan baru.",
            kind.label()
        ));
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("gagal membuat folder memori {}", dir.display()))?;
    let path = dir.join(kind.filename());
    let content = if rendered.is_empty() {
        String::new()
    } else {
        format!("{rendered}\n")
    };
    std::fs::write(&path, content)
        .with_context(|| format!("gagal menyimpan {}", path.display()))?;
    Ok(len)
}

/// Tambah 1 entri baru ke wadah `memory` atau `profile` dengan pagu karakter keras.
pub fn add_entry_in(
    dir: &Path,
    kind: MemoryStoreKind,
    text: &str,
    max_chars: usize,
) -> Result<(usize, usize)> {
    let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return Err(anyhow!("isi catatan tidak boleh kosong"));
    }
    if contains_secret_like(&clean) {
        return Err(anyhow!(
            "catatan ditolak: terdeteksi kredensial/API key/rahasia — dilarang menyimpan rahasia di memory/profile"
        ));
    }
    let mut entries = load_entries_from(dir, kind);
    if entries.iter().any(|e| e.eq_ignore_ascii_case(&clean)) {
        let chars = entry_char_count(&entries);
        return Ok((entries.len(), chars));
    }
    entries.push(clean);
    let chars = save_entries_to(dir, kind, &entries, max_chars)?;
    Ok((entries.len(), chars))
}

/// Ganti entri berdasarkan nomor 1-based ATAU substring lama (`old_text`) dengan `new_text`.
pub fn replace_entry_in(
    dir: &Path,
    kind: MemoryStoreKind,
    selector: &str,
    new_text: &str,
    max_chars: usize,
) -> Result<(usize, usize)> {
    let clean_new: String = new_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean_new.is_empty() {
        return Err(anyhow!("isi pengganti tidak boleh kosong"));
    }
    if contains_secret_like(&clean_new) {
        return Err(anyhow!(
            "catatan ditolak: terdeteksi kredensial/API key/rahasia"
        ));
    }
    let mut entries = load_entries_from(dir, kind);
    let idx = find_entry_index(&entries, selector).ok_or_else(|| {
        anyhow!(
            "entri '{}' tidak ditemukan pada wadah '{}'",
            selector,
            kind.label()
        )
    })?;
    entries[idx] = clean_new;
    let chars = save_entries_to(dir, kind, &entries, max_chars)?;
    Ok((idx + 1, chars))
}

/// Hapus entri berdasarkan nomor 1-based ATAU substring cocok.
pub fn remove_entry_in(
    dir: &Path,
    kind: MemoryStoreKind,
    selector: &str,
    max_chars: usize,
) -> Result<(String, usize)> {
    let mut entries = load_entries_from(dir, kind);
    let idx = find_entry_index(&entries, selector).ok_or_else(|| {
        anyhow!(
            "entri '{}' tidak ditemukan pada wadah '{}'",
            selector,
            kind.label()
        )
    })?;
    let removed = entries.remove(idx);
    let chars = save_entries_to(dir, kind, &entries, max_chars)?;
    Ok((removed, chars))
}

fn find_entry_index(entries: &[String], selector: &str) -> Option<usize> {
    let sel = selector.trim().trim_start_matches('#');
    if let Ok(num) = sel.parse::<usize>() {
        if num >= 1 && num <= entries.len() {
            return Some(num - 1);
        }
    }
    let low = sel.to_ascii_lowercase();
    if low.is_empty() {
        return None;
    }
    entries
        .iter()
        .position(|e| e.to_ascii_lowercase().contains(&low))
}

/// Laporan hasil migrasi satu kali dari tabel SQLite lama (`memories` / `manage_memory`)
/// ke store persisten tunggal (`memory.md`, Perbaikan 3 v0.5.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyMemoryMigrationReport {
    pub migrated: usize,
    pub skipped_duplicates: usize,
    pub skipped_secrets: usize,
    pub overflow_remaining: usize,
    pub warning: Option<String>,
}

/// Pindahkan seluruh entri lama di tabel SQLite `memories` ke berkas persisten `dir/memory.md`.
/// - Menghormati pagu keras `max_memory_chars` (default 2.200 char): jika gabungannya meluber,
///   entri berlebih TIDAK dipotong diam-diam melainkan ditahan utuh dengan peringatan minta rangkum.
/// - Menolak & membersihkan entri lama yang mengandung kredensial/rahasia.
/// - Idempoten: entri yang sudah berhasil dipindahkan atau sudah ada di `memory.md` dihapus dari tabel lama.
pub fn migrate_legacy_sqlite_memories_in(
    store: &crate::store::Store,
    dir: &Path,
    max_memory_chars: usize,
) -> Result<LegacyMemoryMigrationReport> {
    let legacy_rows = store.list_all_legacy_memories()?;
    if legacy_rows.is_empty() {
        return Ok(LegacyMemoryMigrationReport::default());
    }
    let limit = max_memory_chars.max(200);
    let mut report = LegacyMemoryMigrationReport::default();
    let mut entries = load_entries_from(dir, MemoryStoreKind::Memory);
    let mut changed_file = false;
    let mut migrated_ids: Vec<i64> = Vec::new();

    for row in legacy_rows {
        let clean: String = row.content.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            migrated_ids.push(row.id);
            continue;
        }
        if contains_secret_like(&clean) {
            report.skipped_secrets += 1;
            migrated_ids.push(row.id);
            continue;
        }
        if entries.iter().any(|e| e.eq_ignore_ascii_case(&clean)) {
            report.skipped_duplicates += 1;
            migrated_ids.push(row.id);
            continue;
        }
        let mut candidate = entries.clone();
        candidate.push(clean.clone());
        let next_chars = entry_char_count(&candidate);
        if next_chars > limit {
            // Hormati pagu: JANGAN potong diam-diam; tahan di tabel lama agar tidak ada data hilang.
            report.overflow_remaining += 1;
            continue;
        }
        entries = candidate;
        changed_file = true;
        report.migrated += 1;
        migrated_ids.push(row.id);
    }

    if changed_file {
        save_entries_to(dir, MemoryStoreKind::Memory, &entries, limit)?;
    }
    for id in migrated_ids {
        let _ = store.remove_memory(id);
    }
    if report.overflow_remaining > 0 {
        report.warning = Some(format!(
            "Migrasi memori lama: {} entri dipindahkan ke memory.md; {} entri tertahan karena pagu {limit} karakter penuh — rangkum atau hapus catatan lama lewat `/memory` terlebih dahulu (tidak dipotong diam-diam).",
            report.migrated, report.overflow_remaining
        ));
    }
    Ok(report)
}

/// Versi default `migrate_legacy_sqlite_memories_in` yang memakai `user_memory_dir()`.
pub fn migrate_legacy_sqlite_memories(
    store: &crate::store::Store,
    max_memory_chars: usize,
) -> Result<LegacyMemoryMigrationReport> {
    migrate_legacy_sqlite_memories_in(store, &user_memory_dir(), max_memory_chars)
}
