//! Pelacak deduplikasi pembacaan berkas per sesi (`ReadFileTracker`, v0.5.32).
//!
//! Meniru mekanisme deterministik Hermes Agent:
//! - `tools/file_tools.py:528-558` (`_dedup_stub_or_block`): stub `"status": "unchanged"`
//!   pada pengulangan pertama dan eskalasi penolakan keras (`BLOCKED:`) pada pengulangan >= 2.
//! - `tools/file_tools_read_tracking.py:205-266` (`_dedup_lookup`, `reset_file_dedup`):
//!   generasi bacaan pasca-compaction agar bacaan pemulihan pertama tetap mengembalikan isi penuh.
//! - `tools/file_tools_read_tracking.py:344-398` (`_file_version`, `_known_full_content_sha256`):
//!   sidik isi berbasis SHA-256 byte aktual (tidak hanya mtime yang bisa dipertahankan editor).
//!
//! Bertahan lintas `/resume` lewat dua lapis:
//! 1. Persistensi SQLite pada tabel `session_read_files` (sidik SHA-256 mentah + mtime + ukuran).
//! 2. Rekonstruksi fallback dari riwayat `messages` sesi (sidik SHA-256 keluaran `read_file`)
//!    untuk sesi lama pra-v0.5.32 atau jalur uji in-memory tanpa SQLite.

use crate::providers::{Block, Message};
use crate::security::{normalize_sandbox_path, resolve_in_sandbox};
use crate::store::{PersistedReadFile, Store};
use crate::update::sha256_hex;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Batas maksimum entri pelacakan bacaan per sesi agar memori tetap terkendali.
pub const MAX_TRACKED_READS: usize = 512;

/// Penanda yang menunjukkan keluaran `read_file` adalah stub dedup (bukan isi berkas asli).
pub fn is_read_dedup_stub(output: &str) -> bool {
    let t = output.trim_start();
    t.starts_with("BLOCKED:")
        || t.starts_with("[Dedup:")
        || (t.starts_with('{') && t.contains("\"status\":\"unchanged\"") && t.contains("\"dedup\":true"))
}

/// Penanda bahwa hasil `ToolResult` di riwayat sudah dipangkas/diarsipkan oleh pruner.
fn is_pruned_tool_result(content: &str) -> bool {
    let t = content.trim_start();
    t.contains("diringkas — arsip:")
        || t.starts_with("[Hasil tool dipangkas")
        || t.starts_with("[Ringkasan")
        || t.starts_with("[arsip:")
}

/// Bentuk kunci dedup deterministik untuk `(canonical_path, start, end)`.
pub fn make_dedup_key(canonical_path: &str, start: Option<usize>, end: Option<usize>) -> String {
    let s = start
        .map(|v| v.to_string())
        .unwrap_or_else(|| "*".to_string());
    let e = end
        .map(|v| v.to_string())
        .unwrap_or_else(|| "*".to_string());
    format!("{canonical_path}|{s}|{e}")
}

/// Normalisasi path kanonis lintas platform (menghapus prefiks verbatim `\\?\` Windows).
pub fn canonical_key_path(path: &Path) -> String {
    normalize_sandbox_path(path).to_string_lossy().to_string()
}

/// Ambil `(mtime_ns, file_size)` berkas di disk secara best-effort.
pub fn file_mtime_and_size(path: &Path) -> (u128, u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (mtime_ns, meta.len())
}

#[derive(Debug, Clone)]
pub struct TrackedReadEntry {
    pub dedup_key: String,
    pub rel_path: String,
    pub canonical_path: String,
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// Sidik SHA-256 byte mentah berkas di disk (penegas utama).
    pub content_sha256: Option<String>,
    /// Sidik SHA-256 teks hasil `read_file` yang pernah dikirim ke model (fallback rekonstruksi riwayat).
    pub rendered_sha256: Option<String>,
    pub mtime_ns: u128,
    pub file_size: u64,
    /// True bila seluruh berkas dibaca utuh tanpa terpotong.
    pub full_file: bool,
    /// True selama isi berkas masih ada di generasi konteks aktif (direset saat compaction/prune).
    pub in_current_generation: bool,
    /// Jumlah pengulangan dedup sejak pembacaan asli (1 = stub unchanged, >=2 = BLOCKED).
    pub dedup_hits: usize,
    last_seq: u64,
}

/// Status kesegaran satu berkas yang sebelumnya dibaca pada sesi yang di-resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeReadStatus {
    Unchanged {
        rel_path: String,
        canonical_path: String,
    },
    Modified {
        rel_path: String,
        canonical_path: String,
    },
}

#[derive(Debug, Default)]
pub struct ReadFileTracker {
    entries: HashMap<String, TrackedReadEntry>,
    hydrated_session_id: Option<String>,
    seq: u64,
}

impl ReadFileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Kosongkan seluruh state pelacak (dipakai saat `/new` atau pindah sesi via `/resume`).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.hydrated_session_id = None;
        self.seq = 0;
    }

    /// Jumlah entri aktif di pelacak (untuk inspeksi/uji).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Majukan generasi pasca-compaction / pruning (meniru `reset_file_dedup` Hermes):
    /// - Menandai `in_current_generation = false` & mereset `dedup_hits = 0` agar bacaan
    ///   pemulihan pertama sesudah konteks dipangkas mengembalikan isi penuh.
    /// - Metadata tetap dipertahankan untuk diagnostik.
    pub fn reset_generation(&mut self) {
        for entry in self.entries.values_mut() {
            entry.in_current_generation = false;
            entry.dedup_hits = 0;
        }
    }

    /// Hapus seluruh entri untuk satu berkas kanonis (saat berkas ditulis/diedit atau berubah di disk).
    pub fn invalidate_path(&mut self, canonical_path: &str) {
        self.entries
            .retain(|_, e| e.canonical_path != canonical_path);
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn enforce_cap(&mut self) {
        while self.entries.len() > MAX_TRACKED_READS {
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_seq)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            } else {
                break;
            }
        }
    }

    /// Cek apakah pembacaan `(canonical_path, start, end)` dengan sidik saat ini merupakan
    /// pengulangan identik yang isinya sudah ada di konteks sesi.
    ///
    /// - Mengembalikan `Some(stub_atau_blokir)` jika berkas TIDAK berubah (sidik SHA-256 sama)
    ///   dan masih berada di generasi konteks aktif:
    ///   - Hit ke-1: stub JSON ringkas `{"status":"unchanged","dedup":true,...}`
    ///   - Hit ke-2+: eskalasi penolakan keras `BLOCKED: ...`
    /// - Mengembalikan `None` jika berkas belum pernah dibaca, sudah berubah di disk (sidik beda —
    ///   entri usang otomatis dihapus!), atau baru saja melewati compaction (`!in_current_generation`).
    pub fn check_dedup_or_invalidate(
        &mut self,
        canonical_path: &str,
        rel_path: &str,
        start: Option<usize>,
        end: Option<usize>,
        current_raw_sha256: &str,
        current_rendered_sha256: &str,
    ) -> Option<String> {
        let key = make_dedup_key(canonical_path, start, end);
        let matches_fingerprint = {
            let entry = self.entries.get(&key)?;
            if let Some(saved_raw) = entry.content_sha256.as_deref() {
                saved_raw == current_raw_sha256
            } else if let Some(saved_ren) = entry.rendered_sha256.as_deref() {
                saved_ren == current_rendered_sha256
            } else {
                false
            }
        };

        if !matches_fingerprint {
            // Berkas berubah di disk -> hapus seluruh entri lama untuk berkas ini agar bacaan berjalan segar!
            self.invalidate_path(canonical_path);
            return None;
        }

        let seq = self.next_seq();
        let entry = self.entries.get_mut(&key)?;
        // Upgrade entri hasil rekonstruksi riwayat agar menyimpan SHA-256 mentah juga.
        if entry.content_sha256.is_none() {
            entry.content_sha256 = Some(current_raw_sha256.to_string());
        }
        entry.last_seq = seq;

        if !entry.in_current_generation {
            // Pasca-compaction/prune: izinkan 1 kali bacaan pemulihan penuh!
            return None;
        }

        entry.dedup_hits = entry.dedup_hits.saturating_add(1);
        let hits = entry.dedup_hits;
        if hits >= 2 {
            Some(format!(
                "BLOCKED: Kamu sudah memanggil read_file untuk '{rel_path}' {} kali pada rentang yang sama dan berkas TIDAK berubah (SHA-256 identik). \
                 HENTIKAN panggilan read_file untuk berkas ini — isi dari pembacaan sebelumnya di sesi ini masih berlaku di konteks. \
                 Lanjutkan tugasmu menggunakan informasi yang sudah ada.",
                hits + 1
            ))
        } else {
            Some(
                json!({
                    "status": "unchanged",
                    "dedup": true,
                    "content_returned": false,
                    "path": rel_path,
                    "message": "Berkas tidak berubah sejak dibaca di sesi ini; isi sudah ada di konteks percakapan. Gunakan isi yang sudah ada tanpa membaca ulang."
                })
                .to_string(),
            )
        }
    }

    /// Catat pembacaan berkas yang berhasil (non-stub, non-error) beserta sidik SHA-256 isinya.
    #[allow(clippy::too_many_arguments)]
    pub fn record_read(
        &mut self,
        canonical_path: String,
        rel_path: String,
        start: Option<usize>,
        end: Option<usize>,
        content_sha256: String,
        rendered_sha256: String,
        mtime_ns: u128,
        file_size: u64,
        full_file: bool,
    ) {
        let key = make_dedup_key(&canonical_path, start, end);
        let seq = self.next_seq();
        self.entries.insert(
            key.clone(),
            TrackedReadEntry {
                dedup_key: key,
                rel_path,
                canonical_path,
                start,
                end,
                content_sha256: Some(content_sha256),
                rendered_sha256: Some(rendered_sha256),
                mtime_ns,
                file_size,
                full_file,
                in_current_generation: true,
                dedup_hits: 0,
                last_seq: seq,
            },
        );
        self.enforce_cap();
    }

    /// Hidrasi pelacak dari baris persisten SQLite `session_read_files` untuk sesi yang dilanjutkan.
    pub fn hydrate_from_store(&mut self, store: &Store, session_id: &str) {
        if self.hydrated_session_id.as_deref() == Some(session_id) {
            return;
        }
        if let Ok(rows) = store.load_session_read_files(session_id) {
            for row in rows {
                self.insert_persisted_row(row);
            }
        }
        self.hydrated_session_id = Some(session_id.to_string());
    }

    fn insert_persisted_row(&mut self, row: PersistedReadFile) {
        let seq = self.next_seq();
        let full_file = row.start_line.is_none() && row.end_line.is_none();
        self.entries
            .entry(row.dedup_key.clone())
            .or_insert(TrackedReadEntry {
                dedup_key: row.dedup_key,
                rel_path: row.rel_path,
                canonical_path: row.canonical_path,
                start: row.start_line,
                end: row.end_line,
                content_sha256: Some(row.content_sha256),
                rendered_sha256: None,
                mtime_ns: row.mtime_ns,
                file_size: row.file_size,
                full_file,
                in_current_generation: true,
                dedup_hits: 0,
                last_seq: seq,
            });
        self.enforce_cap();
    }

    /// Rekonstruksi pelacakan bacaan dari riwayat `messages` yang dimuat (selamat dari resume
    /// bahkan untuk sesi pra-v0.5.32 atau jalur uji in-memory tanpa SQLite).
    pub fn hydrate_from_messages(&mut self, cwd: &Path, messages: &[Message]) {
        // Kumpulkan semua panggilan tool dari pesan Assistant berdasarkan `id`.
        let mut tool_calls: HashMap<String, (&str, &serde_json::Value)> = HashMap::new();
        for msg in messages {
            for block in &msg.content {
                if let Block::ToolUse { id, name, args } = block {
                    tool_calls.insert(id.clone(), (name.as_str(), args));
                }
            }
        }

        // Telusuri hasil tool secara kronologis agar urutan read -> write/edit -> read akurat.
        for msg in messages {
            for block in &msg.content {
                let Block::ToolResult {
                    id,
                    content,
                    is_error,
                } = block
                else {
                    continue;
                };
                if *is_error {
                    continue;
                }
                let Some(&(name, args)) = tool_calls.get(id) else {
                    continue;
                };
                match name {
                    "write_file" | "edit_file" => {
                        if content.starts_with("Ditolak user") {
                            continue;
                        }
                        if let Some(rel) = args.get("path").and_then(|v| v.as_str()) {
                            if let Ok(resolved) = resolve_in_sandbox(cwd, Path::new(rel)) {
                                let canon = canonical_key_path(&resolved);
                                self.invalidate_path(&canon);
                            }
                        }
                    }
                    "read_file" => {
                        let Some(rel) = args.get("path").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let start = args
                            .get("start")
                            .and_then(|v| v.as_u64())
                            .map(|v| v.max(1) as usize);
                        let end = args
                            .get("end")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize);
                        let Ok(resolved) = resolve_in_sandbox(cwd, Path::new(rel)) else {
                            continue;
                        };
                        let canon = canonical_key_path(&resolved);
                        let key = make_dedup_key(&canon, start, end);

                        if is_pruned_tool_result(content) {
                            // Hasil lama sudah dipangkas -> tandai keluar dari generasi aktif agar bisa dibaca pemulihan.
                            if let Some(e) = self.entries.get_mut(&key) {
                                e.in_current_generation = false;
                            }
                            continue;
                        }
                        if is_read_dedup_stub(content)
                            || content.starts_with("[Gambar dimuat:")
                            || content.starts_with("[PDF")
                            || content.contains("\n... [dipotong di ")
                            || content.contains("\n... (hasil dipotong")
                        {
                            continue;
                        }
                        let rendered_hash = sha256_hex(content.as_bytes());
                        let seq = self.next_seq();
                        if let Some(existing) = self.entries.get_mut(&key) {
                            if existing.rendered_sha256.is_none() {
                                existing.rendered_sha256 = Some(rendered_hash);
                            }
                            existing.in_current_generation = true;
                            existing.last_seq = seq;
                        } else {
                            let (mtime_ns, file_size) = file_mtime_and_size(&resolved);
                            self.entries.insert(
                                key.clone(),
                                TrackedReadEntry {
                                    dedup_key: key,
                                    rel_path: rel.to_string(),
                                    canonical_path: canon,
                                    start,
                                    end,
                                    content_sha256: None,
                                    rendered_sha256: Some(rendered_hash),
                                    mtime_ns,
                                    file_size,
                                    full_file: start.is_none() && end.is_none(),
                                    in_current_generation: true,
                                    dedup_hits: 0,
                                    last_seq: seq,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        self.enforce_cap();
    }

    /// Evaluasi kesegaran berkas yang terlacak pada awal giliran resume terhadap isi aktual di disk.
    /// Berkas yang sudah berubah di disk langsung diinvalidasi dari pelacak (dan dari SQLite bila
    /// `store` + `session_id` tersedia) sehingga pembacaan berikutnya berjalan segar!
    pub fn verify_freshness_for_resume(
        &mut self,
        cwd: &Path,
        store: Option<&Store>,
        session_id: Option<&str>,
    ) -> Vec<ResumeReadStatus> {
        let snapshot: Vec<(String, String, Option<String>, Option<String>, Option<usize>, Option<usize>)> = self
            .entries
            .values()
            .filter(|e| e.in_current_generation)
            .map(|e| {
                (
                    e.rel_path.clone(),
                    e.canonical_path.clone(),
                    e.content_sha256.clone(),
                    e.rendered_sha256.clone(),
                    e.start,
                    e.end,
                )
            })
            .collect();

        let mut seen_canon = std::collections::HashSet::new();
        let mut statuses = Vec::new();
        let mut stale_canons = Vec::new();

        for (rel_path, canonical_path, saved_raw, saved_rendered, start, end) in snapshot {
            if !seen_canon.insert(canonical_path.clone()) {
                continue;
            }
            let disk_path = resolve_in_sandbox(cwd, Path::new(&rel_path))
                .ok()
                .filter(|p| canonical_key_path(p) == canonical_path);
            let Some(p) = disk_path else {
                stale_canons.push(canonical_path.clone());
                statuses.push(ResumeReadStatus::Modified {
                    rel_path,
                    canonical_path,
                });
                continue;
            };
            let Ok(bytes) = std::fs::read(&p) else {
                stale_canons.push(canonical_path.clone());
                statuses.push(ResumeReadStatus::Modified {
                    rel_path,
                    canonical_path,
                });
                continue;
            };
            let unchanged = if let Some(ref raw_sha) = saved_raw {
                sha256_hex(&bytes) == *raw_sha
            } else if let Some(ref ren_sha) = saved_rendered {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().collect();
                let s = start.unwrap_or(1).max(1);
                let e = end.unwrap_or(lines.len()).min(lines.len());
                let slice = if s <= lines.len() && e >= s {
                    lines[s - 1..e].join("\n")
                } else {
                    String::new()
                };
                sha256_hex(slice.as_bytes()) == *ren_sha
            } else {
                false
            };

            if unchanged {
                statuses.push(ResumeReadStatus::Unchanged {
                    rel_path,
                    canonical_path,
                });
            } else {
                stale_canons.push(canonical_path.clone());
                statuses.push(ResumeReadStatus::Modified {
                    rel_path,
                    canonical_path,
                });
            }
        }

        for canon in stale_canons {
            self.invalidate_path(&canon);
            if let (Some(st), Some(sid)) = (store, session_id) {
                let _ = st.delete_session_read_file_path(sid, &canon);
            }
        }

        statuses
    }
}
