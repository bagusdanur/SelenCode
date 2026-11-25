//! Tahap 4 — LSP Diagnostics, Navigasi Simbol, Referensi, & Refactoring Aman.
//!
//! Arsitektur adapter modular yang mendukung Language Server Protocol (JSON-RPC 2.0
//! `Content-Length` stdio) secara bertahap untuk bahasa populer:
//! - Rust (`rust-analyzer`)
//! - TypeScript / JavaScript (`typescript-language-server --stdio`)
//! - Python (`pyright-langserver --stdio` atau `pylsp`)
//! - Go (`gopls`)
//!
//! Penambahan bahasa baru cukup menambah 1 entri `LspAdapter` tanpa mengubah
//! core agent. Bila binary language server belum terpasang di mesin pengguna
//! (mis. Termux Android atau lingkungan minimal), SelenCode otomatis jatuh ke
//! mesin hibrida berbasis Tree-sitter + pemindai simbol/referensi sandboxed
//! sehingga fitur tetap bekerja andal tanpa crash.

use crate::outline::{extract_symbols, is_supported_ext};
use crate::security::{is_secret_file, redact_secrets, resolve_in_sandbox};
use crate::tools::file::MAX_READ_BYTES;
use crate::tools::undo::{FileSnapshot, UndoStack};
use anyhow::{anyhow, Context, Result};
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tree_sitter::{Node, Parser};

const LSP_REQUEST_TIMEOUT_SECS: u64 = 8;
const MAX_SCAN_FILES: usize = 1500;
const MAX_REFERENCES_RETURNED: usize = 80;

/// Definisi satu adapter Language Server (mudah diperluas tanpa ubah core agent).
#[derive(Debug, Clone, Copy)]
pub struct LspAdapter {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Urutan binary yang dicari di `$PATH` (pertama yang ditemukan akan dipakai).
    pub binaries: &'static [&'static str],
    pub args: &'static [&'static str],
    pub extensions: &'static [&'static str],
}

pub const LSP_ADAPTERS: &[LspAdapter] = &[
    LspAdapter {
        id: "rust",
        display_name: "Rust (rust-analyzer)",
        binaries: &["rust-analyzer"],
        args: &[],
        extensions: &["rs"],
    },
    LspAdapter {
        id: "typescript",
        display_name: "TypeScript / JavaScript (typescript-language-server)",
        binaries: &["typescript-language-server"],
        args: &["--stdio"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs"],
    },
    LspAdapter {
        id: "python",
        display_name: "Python (pyright-langserver / pylsp)",
        binaries: &["pyright-langserver", "pylsp"],
        args: &["--stdio"],
        extensions: &["py"],
    },
    LspAdapter {
        id: "go",
        display_name: "Go (gopls)",
        binaries: &["gopls"],
        args: &[],
        extensions: &["go"],
    },
];

pub fn adapter_for_ext(ext: &str) -> Option<&'static LspAdapter> {
    let low = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    LSP_ADAPTERS
        .iter()
        .find(|a| a.extensions.contains(&low.as_str()))
}

pub fn language_id_for_ext(ext: &str) -> &'static str {
    match ext.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        _ => "plaintext",
    }
}

/// Cari binary language server di `$PATH` secara lintas-platform (Windows & Unix).
pub fn find_binary_on_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let direct = dir.join(bin);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for ext in [".exe", ".cmd", ".bat"] {
            let cand = dir.join(format!("{bin}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

pub fn resolve_adapter_binary(adapter: &LspAdapter) -> Option<(&'static str, PathBuf)> {
    for &bin in adapter.binaries {
        if let Some(p) = find_binary_on_path(bin) {
            return Some((bin, p));
        }
    }
    None
}

// ---------- Framing JSON-RPC 2.0 LSP (`Content-Length`) ----------

/// Bungkus pesan JSON-RPC 2.0 dengan header standar LSP `Content-Length`.
pub fn encode_lsp_message(payload: &Value) -> Vec<u8> {
    let body = payload.to_string();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut bytes = Vec::with_capacity(header.len() + body.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// Baca 1 pesan JSON-RPC 2.0 ber-header `Content-Length` dari stream LSP.
pub async fn read_lsp_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value> {
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("stream LSP ditutup sebelum header lengkap"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_len = Some(
                    v.trim()
                        .parse::<usize>()
                        .context("nilai Content-Length LSP tidak valid")?,
                );
            }
        }
    }
    let len = content_len.ok_or_else(|| anyhow!("header Content-Length tidak ditemukan"))?;
    if len > 8 * 1024 * 1024 {
        return Err(anyhow!("pesan LSP terlalu besar ({len} byte)"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let v: Value = serde_json::from_slice(&buf).context("payload JSON LSP tidak valid")?;
    Ok(v)
}

/// Konversi path lokal ke URI `file://` (lintas-platform Unix & Windows).
pub fn path_to_file_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let raw = abs.to_string_lossy().replace('\\', "/");
    let stripped = raw.strip_prefix("//?/").unwrap_or(&raw);
    let encoded = stripped.replace(' ', "%20");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// Konversi URI `file://` kembali ke `PathBuf`.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = rest.replace("%20", " ");
    #[cfg(windows)]
    let clean = {
        let s = decoded.strip_prefix('/').unwrap_or(&decoded);
        s.replace('/', "\\")
    };
    #[cfg(not(windows))]
    let clean = decoded;
    Some(PathBuf::from(clean))
}

// ---------- Tipe Data Hasil LSP & Hibrida ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub rel_path: String,
    pub line: usize, // 1-based
    pub col: usize,  // 1-based
    pub severity: String,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    pub rel_path: String,
    pub line: usize, // 1-based
    pub col: usize,  // 1-based
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEdit {
    pub start_line: usize, // 0-based LSP
    pub start_col: usize,  // 0-based UTF-16/char
    pub end_line: usize,
    pub end_col: usize,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsReport {
    pub backend: String,
    pub diagnostics: Vec<LspDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct SymbolLocationsReport {
    pub backend: String,
    pub symbol: String,
    pub locations: Vec<SymbolLocation>,
}

#[derive(Debug, Clone)]
pub struct HoverReport {
    pub backend: String,
    pub symbol: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct RenameFileResult {
    pub rel_path: String,
    pub replacements: usize,
}

#[derive(Debug, Clone)]
pub struct RenameReport {
    pub backend: String,
    pub old_name: String,
    pub new_name: String,
    pub dry_run: bool,
    pub files: Vec<RenameFileResult>,
    pub total_replacements: usize,
}

// ---------- Klien LSP Stdio Aktif ----------

struct LspSession {
    adapter_name: String,
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    opened_uris: HashMap<String, i32>,
    diagnostics: HashMap<String, Vec<LspDiagnostic>>,
}

impl LspSession {
    async fn start(adapter: &LspAdapter, bin_name: &str, bin_path: &Path, cwd: &Path) -> Result<Self> {
        let mut cmd = Command::new(bin_path);
        // `pylsp` tidak butuh `--stdio`, sedangkan `pyright-langserver` butuh `--stdio`.
        if bin_name != "pylsp" {
            cmd.args(adapter.args);
        }
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("gagal menjalankan language server {}", adapter.display_name))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin LSP tidak tersedia"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("stdout LSP tidak tersedia"))?;

        let mut sess = Self {
            adapter_name: format!("LSP ({bin_name})"),
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            opened_uris: HashMap::new(),
            diagnostics: HashMap::new(),
        };

        let root_uri = path_to_file_uri(cwd);
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {},
                    "definition": { "linkSupport": false },
                    "references": {},
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "rename": { "prepareSupport": false }
                }
            }
        });
        let _ = sess.request("initialize", init_params, cwd).await?;
        sess.notify("initialized", json!({})).await?;
        Ok(sess)
    }

    async fn send_raw(&mut self, msg: &Value) -> Result<()> {
        let bytes = encode_lsp_message(msg);
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
        .await
    }

    async fn request(&mut self, method: &str, params: Value, cwd: &Path) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;

        let deadline = Duration::from_secs(LSP_REQUEST_TIMEOUT_SECS);
        tokio::time::timeout(deadline, async {
            loop {
                let msg = read_lsp_message(&mut self.stdout).await?;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    if let Some(err) = msg.get("error") {
                        return Err(anyhow!("LSP error ({method}): {err}"));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                self.handle_notification(&msg, cwd);
            }
        })
        .await
        .map_err(|_| anyhow!("timeout {LSP_REQUEST_TIMEOUT_SECS}s menunggu respons LSP '{method}'"))?
    }

    fn handle_notification(&mut self, msg: &Value, cwd: &Path) {
        if msg.get("method").and_then(|v| v.as_str()) != Some("textDocument/publishDiagnostics") {
            return;
        }
        let Some(params) = msg.get("params") else {
            return;
        };
        let Some(uri) = params.get("uri").and_then(|v| v.as_str()) else {
            return;
        };
        let rel = file_uri_to_path(uri)
            .and_then(|p| rel_display(cwd, &p))
            .unwrap_or_else(|| uri.to_string());
        let mut list = Vec::new();
        if let Some(arr) = params.get("diagnostics").and_then(|v| v.as_array()) {
            for d in arr {
                let line = d
                    .pointer("/range/start/line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize
                    + 1;
                let col = d
                    .pointer("/range/start/character")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize
                    + 1;
                let sev = match d.get("severity").and_then(|v| v.as_u64()).unwrap_or(1) {
                    1 => "error",
                    2 => "warning",
                    3 => "info",
                    _ => "hint",
                };
                let text = d
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let source = d
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&self.adapter_name)
                    .to_string();
                if !text.is_empty() {
                    list.push(LspDiagnostic {
                        rel_path: rel.clone(),
                        line,
                        col,
                        severity: sev.to_string(),
                        message: redact_secrets(&text),
                        source,
                    });
                }
            }
        }
        self.diagnostics.insert(uri.to_string(), list);
    }

    async fn sync_document(&mut self, abs_path: &Path, text: &str, ext: &str) -> Result<String> {
        let uri = path_to_file_uri(abs_path);
        if let Some(ver) = self.opened_uris.get_mut(&uri) {
            *ver += 1;
            let version = *ver;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            )
            .await?;
        } else {
            self.opened_uris.insert(uri.clone(), 1);
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id_for_ext(ext),
                        "version": 1,
                        "text": text
                    }
                }),
            )
            .await?;
        }
        Ok(uri)
    }

    async fn collect_diagnostics_briefly(&mut self, uri: &str, cwd: &Path) -> Vec<LspDiagnostic> {
        // Beri jendela singkat 500ms agar server mengirim `publishDiagnostics` untuk file yang dibuka.
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                match read_lsp_message(&mut self.stdout).await {
                    Ok(msg) => {
                        self.handle_notification(&msg, cwd);
                        if self.diagnostics.contains_key(uri) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .await;
        self.diagnostics.get(uri).cloned().unwrap_or_default()
    }
}

// ---------- Manajer LSP Hibrida per Sesi ----------

/// Manajer LSP sesi: memakai Language Server nyata bila tersedia di `$PATH`,
/// dan jatuh mulus ke mesin Tree-sitter + workspace symbol/reference bila belum terpasang.
pub struct LspManager {
    cwd: PathBuf,
    sessions: tokio::sync::Mutex<HashMap<String, LspSession>>,
}

impl LspManager {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn ensure_session_for_ext(&self, ext: &str) -> Option<String> {
        let adapter = adapter_for_ext(ext)?;
        {
            let guard = self.sessions.lock().await;
            if guard.contains_key(adapter.id) {
                return Some(adapter.id.to_string());
            }
        }
        let (bin_name, bin_path) = resolve_adapter_binary(adapter)?;
        match LspSession::start(adapter, bin_name, &bin_path, &self.cwd).await {
            Ok(sess) => {
                let mut guard = self.sessions.lock().await;
                guard.insert(adapter.id.to_string(), sess);
                Some(adapter.id.to_string())
            }
            Err(_) => None,
        }
    }

    /// Ambil diagnostik untuk 1 file (`rel_path = Some(...)`) atau seluruh file kode workspace.
    pub async fn diagnostics(&self, rel_path: Option<&str>) -> Result<DiagnosticsReport> {
        if let Some(rel) = rel_path.map(str::trim).filter(|s| !s.is_empty()) {
            let abs = resolve_in_sandbox(&self.cwd, Path::new(rel))?;
            if !abs.is_file() {
                return Err(anyhow!("file tidak ditemukan: {rel}"));
            }
            if is_secret_file(&abs) {
                return Err(anyhow!("file rahasia ditolak: {rel}"));
            }
            let text = read_safe_text(&abs)?;
            let ext = ext_of(&abs);

            if let Some(key) = self.ensure_session_for_ext(&ext).await {
                let mut guard = self.sessions.lock().await;
                if let Some(sess) = guard.get_mut(&key) {
                    if let Ok(uri) = sess.sync_document(&abs, &text, &ext).await {
                        let diags = sess.collect_diagnostics_briefly(&uri, &self.cwd).await;
                        let mut combined = diags;
                        // Tambahkan error sintaks Tree-sitter bila LSP belum selesai mengindeks proyek.
                        if combined.is_empty() && is_supported_ext(&ext) {
                            combined = tree_sitter_syntax_diagnostics(&text, &ext, rel);
                        }
                        return Ok(DiagnosticsReport {
                            backend: sess.adapter_name.clone(),
                            diagnostics: combined,
                        });
                    }
                }
            }

            let diags = if is_supported_ext(&ext) {
                tree_sitter_syntax_diagnostics(&text, &ext, rel)
            } else {
                Vec::new()
            };
            return Ok(DiagnosticsReport {
                backend: "Tree-sitter Fallback".to_string(),
                diagnostics: diags,
            });
        }

        // Diagnostik cepat lintas workspace via Tree-sitter untuk semua file yang didukung.
        let mut all = Vec::new();
        for (rel, abs) in walk_workspace_source_files(&self.cwd) {
            let ext = ext_of(&abs);
            if !is_supported_ext(&ext) {
                continue;
            }
            if let Ok(text) = read_safe_text(&abs) {
                all.extend(tree_sitter_syntax_diagnostics(&text, &ext, &rel));
                if all.len() >= 50 {
                    break;
                }
            }
        }
        Ok(DiagnosticsReport {
            backend: "Tree-sitter Workspace Check".to_string(),
            diagnostics: all,
        })
    }

    /// Go-to-definition: pakai `textDocument/definition` bila LSP aktif, fallback ke Tree-sitter.
    pub async fn definition(
        &self,
        rel_path: Option<&str>,
        line: Option<usize>,
        col: Option<usize>,
        symbol: Option<&str>,
    ) -> Result<SymbolLocationsReport> {
        let mut resolved_symbol = symbol
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(rel) = rel_path.map(str::trim).filter(|s| !s.is_empty()) {
            let abs = resolve_in_sandbox(&self.cwd, Path::new(rel))?;
            if !abs.is_file() || is_secret_file(&abs) {
                return Err(anyhow!("file tidak valid atau rahasia: {rel}"));
            }
            let text = read_safe_text(&abs)?;
            let ext = ext_of(&abs);
            if resolved_symbol.is_none() {
                if let (Some(l), Some(c)) = (line, col) {
                    resolved_symbol = extract_identifier_at(&text, l, c);
                }
            }

            if let (Some(l), Some(c)) = (line, col) {
                if let Some(key) = self.ensure_session_for_ext(&ext).await {
                    let mut guard = self.sessions.lock().await;
                    if let Some(sess) = guard.get_mut(&key) {
                        if let Ok(uri) = sess.sync_document(&abs, &text, &ext).await {
                            let params = json!({
                                "textDocument": { "uri": uri },
                                "position": {
                                    "line": l.saturating_sub(1),
                                    "character": c.saturating_sub(1)
                                }
                            });
                            if let Ok(res) = sess.request("textDocument/definition", params, &self.cwd).await {
                                let locs = parse_lsp_locations(&self.cwd, &res);
                                if !locs.is_empty() {
                                    return Ok(SymbolLocationsReport {
                                        backend: sess.adapter_name.clone(),
                                        symbol: resolved_symbol.unwrap_or_else(|| format!("{rel}:{l}:{c}")),
                                        locations: locs,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let sym = resolved_symbol.ok_or_else(|| {
            anyhow!("berikan argumen 'symbol' atau posisi ('path', 'line', 'col') yang menunjuk identifier")
        })?;
        let locs = fallback_definitions(&self.cwd, &sym, rel_path);
        Ok(SymbolLocationsReport {
            backend: "Tree-sitter Symbol Fallback".to_string(),
            symbol: sym,
            locations: locs,
        })
    }

    /// Find references: pakai `textDocument/references` bila LSP aktif, fallback ke pemindai batas kata workspace.
    pub async fn references(
        &self,
        rel_path: Option<&str>,
        line: Option<usize>,
        col: Option<usize>,
        symbol: Option<&str>,
    ) -> Result<SymbolLocationsReport> {
        let mut resolved_symbol = symbol
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(rel) = rel_path.map(str::trim).filter(|s| !s.is_empty()) {
            let abs = resolve_in_sandbox(&self.cwd, Path::new(rel))?;
            if !abs.is_file() || is_secret_file(&abs) {
                return Err(anyhow!("file tidak valid atau rahasia: {rel}"));
            }
            let text = read_safe_text(&abs)?;
            let ext = ext_of(&abs);
            if resolved_symbol.is_none() {
                if let (Some(l), Some(c)) = (line, col) {
                    resolved_symbol = extract_identifier_at(&text, l, c);
                }
            }

            if let (Some(l), Some(c)) = (line, col) {
                if let Some(key) = self.ensure_session_for_ext(&ext).await {
                    let mut guard = self.sessions.lock().await;
                    if let Some(sess) = guard.get_mut(&key) {
                        if let Ok(uri) = sess.sync_document(&abs, &text, &ext).await {
                            let params = json!({
                                "textDocument": { "uri": uri },
                                "position": {
                                    "line": l.saturating_sub(1),
                                    "character": c.saturating_sub(1)
                                },
                                "context": { "includeDeclaration": true }
                            });
                            if let Ok(res) = sess.request("textDocument/references", params, &self.cwd).await {
                                let locs = parse_lsp_locations(&self.cwd, &res);
                                if !locs.is_empty() {
                                    return Ok(SymbolLocationsReport {
                                        backend: sess.adapter_name.clone(),
                                        symbol: resolved_symbol.unwrap_or_else(|| format!("{rel}:{l}:{c}")),
                                        locations: locs,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let sym = resolved_symbol.ok_or_else(|| {
            anyhow!("berikan argumen 'symbol' atau posisi ('path', 'line', 'col') yang menunjuk identifier")
        })?;
        let locs = fallback_references(&self.cwd, &sym);
        Ok(SymbolLocationsReport {
            backend: "Workspace Reference Fallback".to_string(),
            symbol: sym,
            locations: locs,
        })
    }

    /// Hover / info tipe & dokumentasi simbol.
    pub async fn hover(
        &self,
        rel_path: Option<&str>,
        line: Option<usize>,
        col: Option<usize>,
        symbol: Option<&str>,
    ) -> Result<HoverReport> {
        let mut resolved_symbol = symbol
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(rel) = rel_path.map(str::trim).filter(|s| !s.is_empty()) {
            let abs = resolve_in_sandbox(&self.cwd, Path::new(rel))?;
            if !abs.is_file() || is_secret_file(&abs) {
                return Err(anyhow!("file tidak valid atau rahasia: {rel}"));
            }
            let text = read_safe_text(&abs)?;
            let ext = ext_of(&abs);
            if resolved_symbol.is_none() {
                if let (Some(l), Some(c)) = (line, col) {
                    resolved_symbol = extract_identifier_at(&text, l, c);
                }
            }

            if let (Some(l), Some(c)) = (line, col) {
                if let Some(key) = self.ensure_session_for_ext(&ext).await {
                    let mut guard = self.sessions.lock().await;
                    if let Some(sess) = guard.get_mut(&key) {
                        if let Ok(uri) = sess.sync_document(&abs, &text, &ext).await {
                            let params = json!({
                                "textDocument": { "uri": uri },
                                "position": {
                                    "line": l.saturating_sub(1),
                                    "character": c.saturating_sub(1)
                                }
                            });
                            if let Ok(res) = sess.request("textDocument/hover", params, &self.cwd).await {
                                if let Some(doc) = parse_lsp_hover(&res) {
                                    return Ok(HoverReport {
                                        backend: sess.adapter_name.clone(),
                                        symbol: resolved_symbol.unwrap_or_else(|| format!("{rel}:{l}:{c}")),
                                        content: redact_secrets(&doc),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let sym = resolved_symbol.ok_or_else(|| {
            anyhow!("berikan argumen 'symbol' atau posisi ('path', 'line', 'col') untuk hover")
        })?;
        let doc = fallback_hover(&self.cwd, &sym, rel_path);
        Ok(HoverReport {
            backend: "Tree-sitter Hover Fallback".to_string(),
            symbol: sym,
            content: doc,
        })
    }

    /// Rename simbol aman lintas workspace (menggunakan `textDocument/rename` bila LSP aktif,
    /// atau fallback batas-identifier ketat). Selalu memvalidasi nama identifier, menolak file
    /// rahasia/di luar sandbox, mendukung `dry_run`, dan mencatat `FileSnapshot` ke `UndoStack`.
    pub async fn rename_symbol(
        &self,
        rel_path: Option<&str>,
        line: Option<usize>,
        col: Option<usize>,
        old_name: Option<&str>,
        new_name: &str,
        dry_run: bool,
        undo_stack: &Mutex<UndoStack>,
    ) -> Result<RenameReport> {
        let new_clean = new_name.trim();
        if !is_valid_identifier(new_clean) {
            return Err(anyhow!(
                "nama simbol baru '{new_clean}' bukan identifier valid (gunakan huruf/underscore + alfanumerik)"
            ));
        }

        let mut resolved_old = old_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(rel) = rel_path.map(str::trim).filter(|s| !s.is_empty()) {
            let abs = resolve_in_sandbox(&self.cwd, Path::new(rel))?;
            if !abs.is_file() || is_secret_file(&abs) {
                return Err(anyhow!("file tidak valid atau rahasia: {rel}"));
            }
            let text = read_safe_text(&abs)?;
            let ext = ext_of(&abs);
            if resolved_old.is_none() {
                if let (Some(l), Some(c)) = (line, col) {
                    resolved_old = extract_identifier_at(&text, l, c);
                }
            }

            if let (Some(l), Some(c)) = (line, col) {
                if let Some(key) = self.ensure_session_for_ext(&ext).await {
                    let mut guard = self.sessions.lock().await;
                    if let Some(sess) = guard.get_mut(&key) {
                        if let Ok(uri) = sess.sync_document(&abs, &text, &ext).await {
                            let params = json!({
                                "textDocument": { "uri": uri },
                                "position": {
                                    "line": l.saturating_sub(1),
                                    "character": c.saturating_sub(1)
                                },
                                "newName": new_clean
                            });
                            if let Ok(res) = sess.request("textDocument/rename", params, &self.cwd).await {
                                let edit_map = parse_lsp_workspace_edit(&res);
                                if !edit_map.is_empty() {
                                    let mut files = Vec::new();
                                    let mut total = 0usize;
                                    for (target_path, edits) in edit_map {
                                        let Ok(safe_abs) = resolve_in_sandbox(&self.cwd, &target_path) else {
                                            continue;
                                        };
                                        if !safe_abs.is_file() || is_secret_file(&safe_abs) {
                                            continue;
                                        }
                                        let Ok(orig_bytes) = std::fs::read(&safe_abs) else {
                                            continue;
                                        };
                                        let orig_str = String::from_utf8_lossy(&orig_bytes).into_owned();
                                        let updated = apply_text_edits(&orig_str, &edits);
                                        if updated == orig_str {
                                            continue;
                                        }
                                        let rel_disp = rel_display(&self.cwd, &safe_abs)
                                            .unwrap_or_else(|| safe_abs.to_string_lossy().to_string());
                                        if !dry_run {
                                            std::fs::write(&safe_abs, updated.as_bytes())?;
                                            if let Ok(mut st) = undo_stack.lock() {
                                                st.record(FileSnapshot {
                                                    path: safe_abs,
                                                    rel: rel_disp.clone(),
                                                    before: Some(orig_bytes),
                                                    after: updated.into_bytes(),
                                                });
                                            }
                                        }
                                        total += edits.len();
                                        files.push(RenameFileResult {
                                            rel_path: rel_disp,
                                            replacements: edits.len(),
                                        });
                                    }
                                    return Ok(RenameReport {
                                        backend: sess.adapter_name.clone(),
                                        old_name: resolved_old.unwrap_or_else(|| format!("{rel}:{l}:{c}")),
                                        new_name: new_clean.to_string(),
                                        dry_run,
                                        files,
                                        total_replacements: total,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let old = resolved_old.ok_or_else(|| {
            anyhow!("berikan 'old_name' atau posisi ('path', 'line', 'col') simbol yang akan di-rename")
        })?;
        if !is_valid_identifier(&old) {
            return Err(anyhow!("nama simbol lama '{old}' bukan identifier valid"));
        }
        if old == new_clean {
            return Err(anyhow!("nama simbol baru sama dengan nama lama ('{old}')"));
        }

        let mut files = Vec::new();
        let mut total = 0usize;
        for (rel, abs) in walk_workspace_source_files(&self.cwd) {
            if is_secret_file(&abs) {
                continue;
            }
            let Ok(orig_bytes) = std::fs::read(&abs) else {
                continue;
            };
            if orig_bytes.contains(&0) || orig_bytes.len() > MAX_READ_BYTES {
                continue;
            }
            let orig_str = String::from_utf8_lossy(&orig_bytes).into_owned();
            let (updated, count) = safe_rename_identifier_in_text(&orig_str, &old, new_clean);
            if count == 0 {
                continue;
            }
            if !dry_run {
                std::fs::write(&abs, updated.as_bytes())?;
                if let Ok(mut st) = undo_stack.lock() {
                    st.record(FileSnapshot {
                        path: abs,
                        rel: rel.clone(),
                        before: Some(orig_bytes),
                        after: updated.into_bytes(),
                    });
                }
            }
            total += count;
            files.push(RenameFileResult {
                rel_path: rel,
                replacements: count,
            });
        }

        Ok(RenameReport {
            backend: "Safe Identifier Rename Fallback".to_string(),
            old_name: old,
            new_name: new_clean.to_string(),
            dry_run,
            files,
            total_replacements: total,
        })
    }

    /// Format kartu menu status adapter LSP untuk perintah `/lsp`.
    pub fn format_status_card(&self) -> String {
        let mut out = String::from("# LSP Diagnostics & Adapter Bahasa\n");
        let mut installed = 0usize;
        for adapter in LSP_ADAPTERS {
            match resolve_adapter_binary(adapter) {
                Some((bin, _path)) => {
                    installed += 1;
                    out.push_str(&format!(
                        "* {} | siap ({bin}) | ekstensi: .{}\n",
                        adapter.id,
                        adapter.extensions.join(", .")
                    ));
                }
                None => {
                    out.push_str(&format!(
                        "> {} | Tree-sitter fallback | pasang `{}` untuk LSP penuh\n",
                        adapter.id,
                        adapter.binaries.join("` / `")
                    ));
                }
            }
        }
        out.push_str("---\n");
        out.push_str(&format!(
            "! Adapter LSP terdeteksi: {installed}/{} (mesin Tree-sitter fallback selalu siap bila server belum terpasang)\n",
            LSP_ADAPTERS.len()
        ));
        out.push_str("! Cek diagnostik: /lsp diag [path]   ·   Definisi: /lsp def <simbol>   ·   Referensi: /lsp refs <simbol>");
        out
    }
}

// ---------- Fungsi Murni: TextEdit, Tree-sitter Diagnostics, & Fallback ----------

/// Terapkan daftar `LspTextEdit` (0-based line/col) secara aman dalam urutan terbalik
/// agar pergeseran indeks tidak merusak edit lain di file yang sama.
pub fn apply_text_edits(source: &str, edits: &[LspTextEdit]) -> String {
    if edits.is_empty() {
        return source.to_string();
    }
    let mut line_starts: Vec<usize> = vec![0];
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(idx + 1);
        }
    }
    let to_offset = |line: usize, col: usize| -> usize {
        let start = *line_starts.get(line).unwrap_or(&source.len());
        let line_slice = &source[start..];
        let mut offset = start;
        for (i, ch) in line_slice.chars().enumerate() {
            if i == col || ch == '\n' {
                break;
            }
            offset += ch.len_utf8();
        }
        offset.min(source.len())
    };

    let mut indexed: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| (to_offset(e.start_line, e.start_col), to_offset(e.end_line, e.end_col), e.new_text.as_str()))
        .collect();
    indexed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out = source.to_string();
    for (s, e, repl) in indexed {
        if s <= e && e <= out.len() && out.is_char_boundary(s) && out.is_char_boundary(e) {
            out.replace_range(s..e, repl);
        }
    }
    out
}

/// Deteksi error sintaks kode langsung via AST Tree-sitter (`ERROR` / `MISSING` nodes).
pub fn tree_sitter_syntax_diagnostics(source: &str, ext: &str, rel_path: &str) -> Vec<LspDiagnostic> {
    let language: tree_sitter::Language = match ext.to_ascii_lowercase().as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "js" | "mjs" | "jsx" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return Vec::new(),
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    if !tree.root_node().has_error() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_ts_errors(tree.root_node(), source, rel_path, &mut out);
    out
}

fn collect_ts_errors(node: Node, source: &str, rel_path: &str, out: &mut Vec<LspDiagnostic>) {
    if out.len() >= 25 {
        return;
    }
    if node.is_missing() {
        let pos = node.start_position();
        out.push(LspDiagnostic {
            rel_path: rel_path.to_string(),
            line: pos.row + 1,
            col: pos.column + 1,
            severity: "error".to_string(),
            message: format!("token hilang: diharapkan `{}`", node.kind()),
            source: "tree-sitter".to_string(),
        });
        return;
    }
    if node.is_error() {
        let pos = node.start_position();
        let snippet: String = node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(60)
            .collect();
        let msg = if snippet.is_empty() {
            "kesalahan sintaks (unexpected token)".to_string()
        } else {
            format!("kesalahan sintaks dekat `{}`", redact_secrets(&snippet))
        };
        out.push(LspDiagnostic {
            rel_path: rel_path.to_string(),
            line: pos.row + 1,
            col: pos.column + 1,
            severity: "error".to_string(),
            message: msg,
            source: "tree-sitter".to_string(),
        });
        return;
    }
    if node.has_error() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_ts_errors(child, source, rel_path, out);
        }
    }
}

pub fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Ekstrak identifier pada baris & kolom 1-based dari teks sumber.
pub fn extract_identifier_at(source: &str, line_1based: usize, col_1based: usize) -> Option<String> {
    let line = source.lines().nth(line_1based.saturating_sub(1))?;
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let mut idx = col_1based.saturating_sub(1).min(chars.len().saturating_sub(1));
    if !is_ident_char(chars[idx]) && idx > 0 && is_ident_char(chars[idx - 1]) {
        idx -= 1;
    }
    if !is_ident_char(chars[idx]) {
        return None;
    }
    let mut start = idx;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    let ident: String = chars[start..end].iter().collect();
    is_valid_identifier(&ident).then_some(ident)
}

/// Ganti seluruh kemunculan `old_name` dengan `new_name` HANYA pada batas identifier penuh
/// (tidak pernah merusak `old_name_extra` atau `my_old_name`).
pub fn safe_rename_identifier_in_text(
    source: &str,
    old_name: &str,
    new_name: &str,
) -> (String, usize) {
    if old_name.is_empty() || !source.contains(old_name) {
        return (source.to_string(), 0);
    }
    let bytes = source.as_bytes();
    let needle = old_name.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut last_copy = 0usize;
    let mut count = 0usize;

    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let left_ok = i == 0 || !is_ident_char(bytes[i - 1] as char);
            let right_ok =
                i + needle.len() == bytes.len() || !is_ident_char(bytes[i + needle.len()] as char);
            if left_ok && right_ok {
                out.push_str(&source[last_copy..i]);
                out.push_str(new_name);
                i += needle.len();
                last_copy = i;
                count += 1;
                continue;
            }
        }
        i += 1;
    }
    if count == 0 {
        return (source.to_string(), 0);
    }
    out.push_str(&source[last_copy..]);
    (out, count)
}

fn fallback_definitions(cwd: &Path, symbol: &str, prefer_rel: Option<&str>) -> Vec<SymbolLocation> {
    let mut out = Vec::new();
    for (rel, abs) in walk_workspace_source_files(cwd) {
        let ext = ext_of(&abs);
        if !is_supported_ext(&ext) {
            continue;
        }
        let Ok(text) = read_safe_text(&abs) else {
            continue;
        };
        let Ok(syms) = extract_symbols(&text, &ext) else {
            continue;
        };
        for s in syms {
            if s.name == symbol {
                out.push(SymbolLocation {
                    rel_path: rel.clone(),
                    line: s.start_line,
                    col: 1,
                    preview: format!("[{}] {}", s.kind, redact_secrets(&s.signature)),
                });
            }
        }
    }
    if let Some(pref) = prefer_rel {
        out.sort_by_key(|loc| if loc.rel_path == pref { 0 } else { 1 });
    }
    out
}

fn fallback_references(cwd: &Path, symbol: &str) -> Vec<SymbolLocation> {
    let mut out = Vec::new();
    if symbol.is_empty() {
        return out;
    }
    for (rel, abs) in walk_workspace_source_files(cwd) {
        if out.len() >= MAX_REFERENCES_RETURNED {
            break;
        }
        let Ok(text) = read_safe_text(&abs) else {
            continue;
        };
        if !text.contains(symbol) {
            continue;
        }
        for (line_idx, line) in text.lines().enumerate() {
            let bytes = line.as_bytes();
            let needle = symbol.as_bytes();
            let mut i = 0usize;
            while i + needle.len() <= bytes.len() {
                if &bytes[i..i + needle.len()] == needle {
                    let left_ok = i == 0 || !is_ident_char(bytes[i - 1] as char);
                    let right_ok = i + needle.len() == bytes.len()
                        || !is_ident_char(bytes[i + needle.len()] as char);
                    if left_ok && right_ok {
                        let col = line[..i].chars().count() + 1;
                        let preview: String =
                            redact_secrets(line.trim()).chars().take(110).collect();
                        out.push(SymbolLocation {
                            rel_path: rel.clone(),
                            line: line_idx + 1,
                            col,
                            preview,
                        });
                        if out.len() >= MAX_REFERENCES_RETURNED {
                            return out;
                        }
                        i += needle.len();
                        continue;
                    }
                }
                i += 1;
            }
        }
    }
    out
}

fn fallback_hover(cwd: &Path, symbol: &str, prefer_rel: Option<&str>) -> String {
    let mut matches: Vec<(bool, String)> = Vec::new();
    for (rel, abs) in walk_workspace_source_files(cwd) {
        let ext = ext_of(&abs);
        if !is_supported_ext(&ext) {
            continue;
        }
        let Ok(text) = read_safe_text(&abs) else {
            continue;
        };
        let Ok(syms) = extract_symbols(&text, &ext) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for s in syms {
            if s.name == symbol {
                let mut doc_lines = Vec::new();
                let mut idx = s.start_line.saturating_sub(1);
                while idx > 0 {
                    idx -= 1;
                    let t = lines[idx].trim();
                    if t.starts_with("///")
                        || t.starts_with("//!")
                        || t.starts_with("# ")
                        || t.starts_with('*')
                        || t.starts_with("/**")
                        || t.starts_with("*/")
                    {
                        doc_lines.push(t.to_string());
                    } else if t.is_empty() {
                        break;
                    } else {
                        break;
                    }
                    if doc_lines.len() >= 8 {
                        break;
                    }
                }
                doc_lines.reverse();
                let mut block = format!(
                    "{rel}:L{}-L{}\n{}",
                    s.start_line,
                    s.end_line,
                    redact_secrets(&s.signature)
                );
                if !doc_lines.is_empty() {
                    block.push_str("\n---\n");
                    block.push_str(&redact_secrets(&doc_lines.join("\n")));
                }
                let is_pref = prefer_rel.map(|p| p == rel).unwrap_or(false);
                matches.push((is_pref, block));
            }
        }
    }
    matches.sort_by_key(|(pref, _)| if *pref { 0 } else { 1 });
    if matches.is_empty() {
        format!("Tidak ditemukan definisi simbol '{symbol}' di workspace.")
    } else {
        matches
            .into_iter()
            .take(3)
            .map(|(_, b)| b)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn parse_lsp_locations(cwd: &Path, res: &Value) -> Vec<SymbolLocation> {
    let mut out = Vec::new();
    let items: Vec<&Value> = if let Some(arr) = res.as_array() {
        arr.iter().collect()
    } else if res.is_object() {
        vec![res]
    } else {
        Vec::new()
    };
    for item in items.into_iter().take(MAX_REFERENCES_RETURNED) {
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(|v| v.as_str());
        let range = item.get("range").or_else(|| item.get("targetSelectionRange"));
        let (Some(uri), Some(range)) = (uri, range) else {
            continue;
        };
        let Some(abs) = file_uri_to_path(uri) else {
            continue;
        };
        if is_secret_file(&abs) {
            continue;
        }
        let Ok(safe) = resolve_in_sandbox(cwd, &abs) else {
            continue;
        };
        let rel = rel_display(cwd, &safe).unwrap_or_else(|| safe.to_string_lossy().to_string());
        let line = range
            .pointer("/start/line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize
            + 1;
        let col = range
            .pointer("/start/character")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize
            + 1;
        let preview = std::fs::read_to_string(&safe)
            .ok()
            .and_then(|txt| txt.lines().nth(line.saturating_sub(1)).map(|l| l.trim().to_string()))
            .map(|l| redact_secrets(&l).chars().take(110).collect())
            .unwrap_or_default();
        out.push(SymbolLocation {
            rel_path: rel,
            line,
            col,
            preview,
        });
    }
    out
}

fn parse_lsp_hover(res: &Value) -> Option<String> {
    let contents = res.get("contents")?;
    if let Some(s) = contents.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(v) = contents.get("value").and_then(|x| x.as_str()) {
        let t = v.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(arr) = contents.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_string())
                    .or_else(|| item.get("value").and_then(|v| v.as_str()).map(|s| s.trim().to_string()))
            })
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n\n"));
        }
    }
    None
}

fn parse_lsp_workspace_edit(res: &Value) -> Vec<(PathBuf, Vec<LspTextEdit>)> {
    let mut out = Vec::new();
    if let Some(changes) = res.get("changes").and_then(|v| v.as_object()) {
        for (uri, edits_val) in changes {
            if let (Some(path), Some(arr)) = (file_uri_to_path(uri), edits_val.as_array()) {
                let edits = parse_text_edits_array(arr);
                if !edits.is_empty() {
                    out.push((path, edits));
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(doc_changes) = res.get("documentChanges").and_then(|v| v.as_array()) {
            for dc in doc_changes {
                let uri = dc.pointer("/textDocument/uri").and_then(|v| v.as_str());
                let arr = dc.get("edits").and_then(|v| v.as_array());
                if let (Some(u), Some(a)) = (uri, arr) {
                    if let Some(path) = file_uri_to_path(u) {
                        let edits = parse_text_edits_array(a);
                        if !edits.is_empty() {
                            out.push((path, edits));
                        }
                    }
                }
            }
        }
    }
    out
}

fn parse_text_edits_array(arr: &[Value]) -> Vec<LspTextEdit> {
    arr.iter()
        .filter_map(|e| {
            let start_line = e.pointer("/range/start/line")?.as_u64()? as usize;
            let start_col = e.pointer("/range/start/character")?.as_u64()? as usize;
            let end_line = e.pointer("/range/end/line")?.as_u64()? as usize;
            let end_col = e.pointer("/range/end/character")?.as_u64()? as usize;
            let new_text = e.get("newText")?.as_str()?.to_string();
            Some(LspTextEdit {
                start_line,
                start_col,
                end_line,
                end_col,
                new_text,
            })
        })
        .collect()
}

fn walk_workspace_source_files(cwd: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(cwd)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(12))
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy();
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

    for entry in walker.flatten() {
        if out.len() >= MAX_SCAN_FILES {
            break;
        }
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if is_secret_file(path) {
            continue;
        }
        let ext = ext_of(path);
        if !matches!(
            ext.as_str(),
            "rs" | "py" | "js" | "mjs" | "jsx" | "ts" | "tsx" | "go" | "java" | "kt" | "c" | "h" | "cpp"
        ) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(cwd) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path.to_path_buf()));
        }
    }
    out
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn rel_display(cwd: &Path, abs: &Path) -> Option<String> {
    let cwd_can = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let abs_can = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    abs_can
        .strip_prefix(&cwd_can)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn read_safe_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) {
        return Err(anyhow!("file biner dilewati"));
    }
    if bytes.len() > MAX_READ_BYTES {
        return Err(anyhow!("file melebihi batas ukuran baca"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
