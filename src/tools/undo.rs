//! Transactional Coding, Checkpoint, & Rollback untuk SelenCode (Tahap 1).
//!
//! Setiap `write_file` / `edit_file` yang berhasil mencatat `FileSnapshot`
//! (isi sebelum & sesudah). Selain `/undo` dan `/redo` per-file, modul ini
//! menyediakan checkpoint transaksional (`/checkpoint`, `/rollback`) yang:
//! - mencatat titik aman sebelum perubahan besar atau awal giliran agent,
//! - memulihkan HANYA perubahan milik SelenCode sejak checkpoint terpilih,
//! - menolak menimpa file yang sudah diedit manual oleh user setelah SelenCode
//!   menyentuhnya (proteksi konflik eksternal).

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

const MAX_UNDO_ENTRIES: usize = 200;
const MAX_CHECKPOINTS: usize = 30;

#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub rel: String,
    /// None berarti file belum ada sebelumnya (baru dibuat oleh write_file).
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

/// Titik checkpoint transaksional pada `UndoStack`.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub id: usize,
    pub label: String,
    pub created_at: String,
    /// Indeks `undo.len()` saat checkpoint dibuat.
    pub undo_mark: usize,
}

/// Laporan hasil rollback ke suatu checkpoint.
#[derive(Debug, Clone)]
pub struct RollbackReport {
    pub checkpoint_id: usize,
    pub label: String,
    /// File yang berhasil dipulihkan / dihapus kembali ke keadaan checkpoint.
    pub restored: Vec<String>,
    /// File yang dilewati karena berubah secara eksternal oleh user (tidak ditimpa!).
    pub skipped_conflicts: Vec<String>,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<FileSnapshot>,
    redo: Vec<FileSnapshot>,
    checkpoints: Vec<Checkpoint>,
    next_cp_id: usize,
}

impl UndoStack {
    pub fn record(&mut self, snap: FileSnapshot) {
        self.undo.push(snap);
        if self.undo.len() > MAX_UNDO_ENTRIES {
            let overflow = self.undo.len() - MAX_UNDO_ENTRIES;
            self.undo.drain(0..overflow);
            for cp in &mut self.checkpoints {
                cp.undo_mark = cp.undo_mark.saturating_sub(overflow);
            }
        }
        self.redo.clear();
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Buat checkpoint baru pada posisi `undo` saat ini.
    pub fn begin_checkpoint(&mut self, label: impl Into<String>) -> usize {
        self.next_cp_id += 1;
        let id = self.next_cp_id;
        let label = label.into();
        let created_at = chrono::Local::now().format("%H:%M:%S").to_string();
        self.checkpoints.push(Checkpoint {
            id,
            label,
            created_at,
            undo_mark: self.undo.len(),
        });
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            self.checkpoints.remove(0);
        }
        id
    }

    /// Daftar semua checkpoint beserta jumlah perubahan file sejak checkpoint itu.
    pub fn list_checkpoints(&self) -> Vec<(Checkpoint, usize)> {
        self.checkpoints
            .iter()
            .map(|cp| {
                let count = self.undo.len().saturating_sub(cp.undo_mark);
                (cp.clone(), count)
            })
            .collect()
    }

    /// Daftar path relatif unik yang berubah sejak checkpoint `id`.
    pub fn files_changed_since(&self, id: usize) -> Vec<String> {
        let Some(cp) = self.checkpoints.iter().find(|c| c.id == id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for snap in self.undo.iter().skip(cp.undo_mark) {
            if !out.contains(&snap.rel) {
                out.push(snap.rel.clone());
            }
        }
        out
    }

    /// Hapus checkpoint bila tidak ada file yang berubah (menjaga daftar tetap bersih).
    pub fn discard_empty_checkpoint(&mut self, id: usize) {
        if let Some(pos) = self.checkpoints.iter().position(|c| c.id == id) {
            if self.checkpoints[pos].undo_mark == self.undo.len() {
                self.checkpoints.remove(pos);
            }
        }
    }

    /// Rollback semua perubahan SelenCode mundur ke checkpoint `id` (atau checkpoint
    /// terbaru bila `id` adalah `None`). File yang sudah diubah secara eksternal oleh
    /// user TIDAK akan ditimpa dan dilaporkan di `skipped_conflicts`.
    pub fn rollback_to(&mut self, id: Option<usize>) -> Result<RollbackReport> {
        let pos = match id {
            Some(target) => self
                .checkpoints
                .iter()
                .position(|c| c.id == target)
                .ok_or_else(|| anyhow!("checkpoint #{target} tidak ditemukan"))?,
            None => self
                .checkpoints
                .len()
                .checked_sub(1)
                .ok_or_else(|| anyhow!("belum ada checkpoint tersimpan untuk di-rollback"))?,
        };
        let cp = self.checkpoints[pos].clone();
        let mut restored = Vec::new();
        let mut skipped_conflicts = Vec::new();

        while self.undo.len() > cp.undo_mark {
            let Some(snap) = self.undo.pop() else {
                break;
            };
            // Proteksi konflik: hanya pulihkan jika isi file di disk masih cocok
            // dengan isi terakhir yang ditulis SelenCode (`snap.after`).
            let current_disk = std::fs::read(&snap.path).ok();
            let modified_externally = match &current_disk {
                Some(bytes) => bytes != &snap.after,
                None => !snap.after.is_empty(),
            };
            if modified_externally {
                skipped_conflicts.push(format!(
                    "{} (diubah di luar SelenCode — dilewati agar aman)",
                    snap.rel
                ));
                continue;
            }
            let detail = match &snap.before {
                Some(old_bytes) => {
                    if let Some(parent) = snap.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&snap.path, old_bytes)
                        .with_context(|| format!("gagal memulihkan {}", snap.rel))?;
                    format!("{} (dipulihkan)", snap.rel)
                }
                None => {
                    if snap.path.exists() {
                        std::fs::remove_file(&snap.path)
                            .with_context(|| format!("gagal menghapus file baru {}", snap.rel))?;
                    }
                    format!("{} (file baru dihapus)", snap.rel)
                }
            };
            restored.push(detail);
            self.redo.push(snap);
        }

        // Buang checkpoint yang lebih baru dari titik rollback.
        self.checkpoints.truncate(pos + 1);
        Ok(RollbackReport {
            checkpoint_id: cp.id,
            label: cp.label,
            restored,
            skipped_conflicts,
        })
    }

    /// Batalkan perubahan file terakhir. Mengembalikan deskripsi singkat atau None bila kosong.
    pub fn undo(&mut self) -> Result<Option<String>> {
        let Some(snap) = self.undo.pop() else {
            return Ok(None);
        };
        let msg = match &snap.before {
            Some(old_bytes) => {
                if let Some(parent) = snap.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&snap.path, old_bytes)
                    .with_context(|| format!("gagal memulihkan {}", snap.rel))?;
                format!("dipulihkan ke isi sebelumnya ({} byte)", old_bytes.len())
            }
            None => {
                if snap.path.exists() {
                    std::fs::remove_file(&snap.path)
                        .with_context(|| format!("gagal menghapus file baru {}", snap.rel))?;
                }
                "file baru dihapus kembali".to_string()
            }
        };
        let rel = snap.rel.clone();
        self.redo.push(snap);
        Ok(Some(format!("{rel} — {msg}")))
    }

    /// Ulangi perubahan yang baru saja di-undo.
    pub fn redo(&mut self) -> Result<Option<String>> {
        let Some(snap) = self.redo.pop() else {
            return Ok(None);
        };
        if let Some(parent) = snap.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&snap.path, &snap.after)
            .with_context(|| format!("gagal menerapkan ulang {}", snap.rel))?;
        let rel = snap.rel.clone();
        let len = snap.after.len();
        self.undo.push(snap);
        Ok(Some(format!("{rel} — diterapkan ulang ({len} byte)")))
    }
}
