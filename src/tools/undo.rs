//! Riwayat snapshot edit file untuk `/undo` dan `/redo` (ala OpenCode).
//! Setiap `write_file` / `edit_file` yang berhasil menyimpan isi sebelum &
//! sesudah sehingga user bisa membatalkan atau mengulang perubahan dengan 1 perintah.

use anyhow::{Context, Result};
use std::path::PathBuf;

const MAX_UNDO_ENTRIES: usize = 50;

#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub rel: String,
    /// None berarti file belum ada sebelumnya (baru dibuat oleh write_file).
    pub before: Option<Vec<u8>>,
    pub after: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<FileSnapshot>,
    redo: Vec<FileSnapshot>,
}

impl UndoStack {
    pub fn record(&mut self, snap: FileSnapshot) {
        self.undo.push(snap);
        if self.undo.len() > MAX_UNDO_ENTRIES {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
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
