//! Tahap 2 — Smart Context Retrieval & Incremental Repository Index.
//!
//! Memelihara indeks repositori inkremental di memori (hanya mem-parse ulang
//! file yang `mtime`/`size` berubah, menghapus file yang terhapus, menghormati
//! `.gitignore` dan menolak file rahasia `.env` / `*.key` / `id_rsa`).
//! Memberi peringkat relevansi berdasarkan simbol Tree-sitter, nama/path file,
//! serta tetangga ketergantungan import, lalu mengekstrak potongan kode terikat
//! budget token yang sudah teredaksi.

use crate::outline::{extract_imports, extract_symbols, is_supported_ext, SymbolInfo};
use crate::security::{is_secret_file, redact_secrets, resolve_in_sandbox};
use crate::tools::file::MAX_READ_BYTES;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_INDEX_FILES: usize = 1500;
const MAX_SNIPPET_LINES_PER_SYM: usize = 18;
const MAX_FALLBACK_LINES: usize = 12;

/// Metadata & simbol hasil indeks 1 file dalam repositori.
#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub rel_path: String,
    pub mtime_secs: u64,
    pub size_bytes: u64,
    pub symbols: Vec<SymbolInfo>,
    pub imports: Vec<String>,
    pub stem_lower: String,
    pub path_lower: String,
}

/// Statistik hasil penyegaran indeks inkremental.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub total_files: usize,
    pub indexed_new_or_changed: usize,
    pub reused_cached: usize,
    pub removed_files: usize,
    pub total_symbols: usize,
}

/// Satu hasil pencarian konteks relevan yang sudah diberi skor & cuplikan kode.
#[derive(Debug, Clone)]
pub struct ContextHit {
    pub rel_path: String,
    pub score: i32,
    pub matched_symbols: Vec<SymbolInfo>,
    pub dependencies: Vec<String>,
    pub snippet: String,
}

/// Indeks repositori inkremental yang disimpan per sesi di `Ctx`.
#[derive(Debug)]
pub struct RepoIndex {
    cwd: PathBuf,
    files: HashMap<String, IndexedFile>,
}

impl RepoIndex {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            files: HashMap::new(),
        }
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn get(&self, rel_path: &str) -> Option<&IndexedFile> {
        self.files.get(rel_path)
    }

    /// Segarkan indeks secara inkremental: hanya file baru/berubah yang diparse ulang.
    /// File rahasia (`.env`, `*.key`, `id_rsa`, dll.) dan folder artefak build dilewati.
    pub fn refresh(&mut self) -> IndexStats {
        let mut stats = IndexStats::default();
        let mut seen: HashSet<String> = HashSet::new();

        let walker = WalkBuilder::new(&self.cwd)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .max_depth(Some(12))
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    return !matches!(
                        name.as_ref(),
                        ".git"
                            | "target"
                            | "node_modules"
                            | "dist"
                            | "build"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                            | ".next"
                            | ".selencode"
                    );
                }
                true
            })
            .build();

        let mut scanned = 0usize;
        for entry in walker.flatten() {
            if scanned >= MAX_INDEX_FILES {
                break;
            }
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            if is_secret_file(path) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&self.cwd) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.is_empty() || !is_indexable_path(&rel_str) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size_bytes = meta.len();
            if size_bytes as usize > MAX_READ_BYTES {
                continue;
            }
            let mtime_secs = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            scanned += 1;
            seen.insert(rel_str.clone());

            // Cek cache inkremental: jika mtime & size identik, langsung pakai ulang!
            if let Some(existing) = self.files.get(&rel_str) {
                if existing.mtime_secs == mtime_secs && existing.size_bytes == size_bytes {
                    stats.reused_cached += 1;
                    continue;
                }
            }

            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if bytes.contains(&0) {
                continue; // lewati file biner
            }
            let text = String::from_utf8_lossy(&bytes);
            let ext = Path::new(&rel_str)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();

            let symbols = if is_supported_ext(&ext) {
                extract_symbols(&text, &ext).unwrap_or_default()
            } else {
                Vec::new()
            };
            let imports = extract_imports(&text, &ext);
            let stem_lower = Path::new(&rel_str)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let path_lower = rel_str.to_ascii_lowercase();

            self.files.insert(
                rel_str.clone(),
                IndexedFile {
                    rel_path: rel_str,
                    mtime_secs,
                    size_bytes,
                    symbols,
                    imports,
                    stem_lower,
                    path_lower,
                },
            );
            stats.indexed_new_or_changed += 1;
        }

        // Hapus entri file yang sudah dihapus dari disk.
        let before = self.files.len();
        self.files.retain(|k, _| seen.contains(k));
        stats.removed_files = before.saturating_sub(self.files.len());
        stats.total_files = self.files.len();
        stats.total_symbols = self.files.values().map(|f| f.symbols.len()).sum();
        stats
    }

    /// Cari file, simbol, dan potongan kode paling relevan untuk `query` dengan
    /// peringkat berbobot + boost ketergantungan import + batas budget byte.
    pub fn query(&mut self, query: &str, max_results: usize, max_bytes: usize) -> Vec<ContextHit> {
        let _ = self.refresh();
        let terms = tokenize_query(query);
        if terms.is_empty() || max_results == 0 || max_bytes == 0 {
            return Vec::new();
        }

        // Pass 1: skor langsung per file (simbol, signature, path, import).
        let mut direct_scores: HashMap<String, (i32, Vec<SymbolInfo>)> = HashMap::new();
        for (rel, file) in &self.files {
            let mut score: i32 = 0;
            let mut matched_terms: HashSet<&str> = HashSet::new();
            let mut matched_syms: Vec<SymbolInfo> = Vec::new();

            for term in &terms {
                let mut term_hit = false;
                if file.stem_lower == *term {
                    score += 36;
                    term_hit = true;
                } else if file.path_lower.contains(term.as_str()) {
                    score += 18;
                    term_hit = true;
                }

                for imp in &file.imports {
                    if imp == term || imp.contains(term.as_str()) {
                        score += 10;
                        term_hit = true;
                        break;
                    }
                }

                for sym in &file.symbols {
                    let sym_low = sym.name.to_ascii_lowercase();
                    let sig_low = sym.signature.to_ascii_lowercase();
                    let sym_score = if sym_low == *term {
                        60
                    } else if sym_low.contains(term.as_str()) {
                        26
                    } else if sig_low.contains(term.as_str()) {
                        12
                    } else {
                        0
                    };
                    if sym_score > 0 {
                        score += sym_score;
                        term_hit = true;
                        if !matched_syms
                            .iter()
                            .any(|s| s.name == sym.name && s.start_line == sym.start_line)
                        {
                            matched_syms.push(sym.clone());
                        }
                    }
                }

                if term_hit {
                    matched_terms.insert(term.as_str());
                }
            }

            if matched_terms.len() > 1 {
                score += (matched_terms.len() as i32) * 12;
            }
            if score > 0 {
                direct_scores.insert(rel.clone(), (score, matched_syms));
            }
        }

        // Pass 2: boost ketergantungan antar-modul (caller & callee dari file skor tinggi).
        let high_stems: HashSet<String> = direct_scores
            .iter()
            .filter(|(_, (sc, _))| *sc >= 30)
            .filter_map(|(rel, _)| self.files.get(rel).map(|f| f.stem_lower.clone()))
            .filter(|st| st.len() >= 3 && !matches!(st.as_str(), "mod" | "lib" | "main" | "index"))
            .collect();

        let high_imports: HashSet<String> = direct_scores
            .iter()
            .filter(|(_, (sc, _))| *sc >= 30)
            .filter_map(|(rel, _)| self.files.get(rel))
            .flat_map(|f| f.imports.iter().cloned())
            .filter(|st| st.len() >= 3 && !matches!(st.as_str(), "mod" | "lib" | "main" | "index"))
            .collect();

        if !high_stems.is_empty() || !high_imports.is_empty() {
            for (rel, file) in &self.files {
                let imports_high = file.imports.iter().any(|imp| {
                    high_stems.contains(imp) || high_stems.iter().any(|s| imp.ends_with(s.as_str()))
                });
                let is_imported_by_high = !matches!(
                    file.stem_lower.as_str(),
                    "" | "mod" | "lib" | "main" | "index"
                ) && high_imports.contains(&file.stem_lower);

                if imports_high || is_imported_by_high {
                    let entry = direct_scores.entry(rel.clone()).or_insert((0, Vec::new()));
                    entry.0 += 14;
                }
            }
        }

        let mut ranked: Vec<(String, i32, Vec<SymbolInfo>)> = direct_scores
            .into_iter()
            .map(|(rel, (sc, syms))| (rel, sc, syms))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.len().cmp(&a.2.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked.truncate(max_results);

        let per_hit_budget = (max_bytes / ranked.len().max(1)).max(320);
        let mut total_used = 0usize;
        let mut hits = Vec::new();

        for (rel, score, matched_symbols) in ranked {
            if total_used >= max_bytes {
                break;
            }
            let Some(file) = self.files.get(&rel) else {
                continue;
            };
            let remaining = max_bytes.saturating_sub(total_used);
            let snippet_budget = per_hit_budget.min(remaining);
            let snippet = extract_safe_snippet(&self.cwd, file, &matched_symbols, snippet_budget);
            let deps = file.imports.iter().take(8).cloned().collect::<Vec<_>>();
            total_used += snippet.len() + rel.len() + 64;
            hits.push(ContextHit {
                rel_path: rel,
                score,
                matched_symbols,
                dependencies: deps,
                snippet,
            });
        }
        hits
    }

    /// Ringkas status indeks (atau hasil pencarian bila `query` diisi) sebagai kartu menu Moonlit.
    pub fn format_status_card(&mut self, query: Option<&str>) -> String {
        let stats = self.refresh();
        if let Some(q) = query.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let hits = self.query(q, 6, 6144);
            let mut out = format!(
                "# Smart Context Index — Cari: {q}\n\
                 > indeks | {} file · {} simbol | {} baru/ubah · {} cache\n",
                stats.total_files,
                stats.total_symbols,
                stats.indexed_new_or_changed,
                stats.reused_cached
            );
            if hits.is_empty() {
                out.push_str("---\n! Tidak ditemukan file/simbol relevan untuk kueri tersebut.");
                return out;
            }
            out.push_str("---\n");
            for h in &hits {
                let syms = if h.matched_symbols.is_empty() {
                    "(kecocokan path/import)".to_string()
                } else {
                    h.matched_symbols
                        .iter()
                        .take(3)
                        .map(|s| format!("{} (L{})", s.name, s.start_line))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                out.push_str(&format!(
                    "* {} | skor {} | {}\n",
                    h.rel_path, h.score, syms
                ));
            }
            out.push_str("---\n! Pakai `@path/file` di prompt atau biarkan Selena memanggil `retrieve_context` otomatis.");
            return out;
        }

        let mut out = format!(
            "# Indeks Repositori Inkremental\n\
             * file terindeks | {} file | {} simbol Tree-sitter\n\
             > sinkronisasi | {} diperbarui | {} cache dipakai ulang · {} dihapus\n",
            stats.total_files,
            stats.total_symbols,
            stats.indexed_new_or_changed,
            stats.reused_cached,
            stats.removed_files
        );
        let mut by_syms: Vec<&IndexedFile> = self.files.values().collect();
        by_syms.sort_by(|a, b| {
            b.symbols
                .len()
                .cmp(&a.symbols.len())
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });
        if !by_syms.is_empty() {
            out.push_str("---\n");
            for f in by_syms.iter().take(8) {
                out.push_str(&format!(
                    "> {} | {} simbol | {} import\n",
                    f.rel_path,
                    f.symbols.len(),
                    f.imports.len()
                ));
            }
        }
        out.push_str("---\n! Uji pencarian konteks relevan: /index <kata_kunci_atau_nama_fungsi>");
        out
    }
}

/// Format hasil `ContextHit` menjadi blok prompt ringkas dengan batas byte/token ketat.
pub fn format_hits_for_prompt(hits: &[ContextHit], max_bytes: usize) -> String {
    if hits.is_empty() || max_bytes == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (idx, h) in hits.iter().enumerate() {
        let mut sec = format!("### {}. {} (skor: {})\n", idx + 1, h.rel_path, h.score);
        if !h.matched_symbols.is_empty() {
            let syms: Vec<String> = h
                .matched_symbols
                .iter()
                .take(4)
                .map(|s| format!("{} [L{}-L{}]", s.signature, s.start_line, s.end_line))
                .collect();
            sec.push_str(&format!("Simbol cocok: {}\n", syms.join(" ; ")));
        }
        if !h.dependencies.is_empty() {
            sec.push_str(&format!("Import/ketergantungan: {}\n", h.dependencies.join(", ")));
        }
        if !h.snippet.is_empty() {
            sec.push_str("```\n");
            sec.push_str(&h.snippet);
            if !h.snippet.ends_with('\n') {
                sec.push('\n');
            }
            sec.push_str("```\n");
        }
        if out.len() + sec.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(out.len());
            if remaining > 80 {
                let mut cut = remaining;
                while cut > 0 && !sec.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push_str(&sec[..cut]);
                out.push_str("\n... [konteks dipotong sesuai budget token]\n");
            }
            break;
        }
        out.push_str(&sec);
    }
    out.trim_end().to_string()
}

fn is_indexable_path(rel: &str) -> bool {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "rs" | "py"
            | "js"
            | "mjs"
            | "jsx"
            | "ts"
            | "tsx"
            | "go"
            | "java"
            | "kt"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "swift"
            | "sh"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "md"
            | "sql"
    )
}

/// Pecah query menjadi token lowercase (mendukung snake_case & camelCase), buang stopword.
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut raw_tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let flush = |buf: &mut String, out: &mut Vec<String>| {
        if buf.len() >= 2 {
            out.push(buf.to_ascii_lowercase());
        }
        buf.clear();
    };

    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() {
            // Pisah batas camelCase (mis. `LoopGuard` -> `loop`, `guard`)
            if ch.is_ascii_uppercase()
                && !current.is_empty()
                && current
                    .chars()
                    .last()
                    .map(|p| p.is_ascii_lowercase())
                    .unwrap_or(false)
            {
                flush(&mut current, &mut raw_tokens);
            }
            current.push(ch);
        } else {
            flush(&mut current, &mut raw_tokens);
        }
    }
    flush(&mut current, &mut raw_tokens);

    // Juga simpan kata majemuk snake_case utuh bila ada (agar exact match simbol `verify_project` bernilai tinggi).
    for word in query.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        let low = word.trim_matches('_').to_ascii_lowercase();
        if low.contains('_') && low.len() >= 3 {
            raw_tokens.push(low);
        }
    }

    let mut out = Vec::new();
    for tok in raw_tokens {
        if is_stopword(&tok) || out.iter().any(|x| x == &tok) {
            continue;
        }
        out.push(tok);
        if out.len() >= 16 {
            break;
        }
    }
    out
}

fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "yang"
            | "dan"
            | "di"
            | "ke"
            | "dari"
            | "ini"
            | "itu"
            | "untuk"
            | "dengan"
            | "pada"
            | "ada"
            | "tidak"
            | "bisa"
            | "tolong"
            | "coba"
            | "bagian"
            | "cara"
            | "buat"
            | "cek"
            | "lihat"
            | "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "from"
            | "into"
            | "how"
            | "what"
            | "where"
            | "when"
            | "please"
            | "fix"
            | "check"
            | "file"
            | "code"
            | "halo"
            | "hai"
            | "kamu"
            | "apa"
            | "aja"
    )
}

fn extract_safe_snippet(
    cwd: &Path,
    file: &IndexedFile,
    matched_symbols: &[SymbolInfo],
    max_bytes: usize,
) -> String {
    let Ok(abs) = resolve_in_sandbox(cwd, Path::new(&file.rel_path)) else {
        return String::new();
    };
    if !abs.is_file() || is_secret_file(&abs) {
        return String::new();
    }
    let Ok(bytes) = std::fs::read(&abs) else {
        return String::new();
    };
    if bytes.contains(&0) || bytes.len() > MAX_READ_BYTES {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    if matched_symbols.is_empty() {
        for (i, line) in lines.iter().take(MAX_FALLBACK_LINES).enumerate() {
            let formatted = format!("L{}: {}\n", i + 1, line);
            if out.len() + formatted.len() > max_bytes {
                break;
            }
            out.push_str(&formatted);
        }
    } else {
        for (idx, sym) in matched_symbols.iter().take(2).enumerate() {
            if idx > 0 {
                out.push_str("...\n");
            }
            let start = sym.start_line.saturating_sub(1);
            let end = sym
                .end_line
                .min(sym.start_line + MAX_SNIPPET_LINES_PER_SYM - 1)
                .min(lines.len());
            for i in start..end {
                let formatted = format!("L{}: {}\n", i + 1, lines[i]);
                if out.len() + formatted.len() > max_bytes {
                    break;
                }
                out.push_str(&formatted);
            }
            if out.len() >= max_bytes {
                break;
            }
        }
    }
    redact_secrets(out.trim_end())
}
