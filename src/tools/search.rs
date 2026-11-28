use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::{is_secret_file, resolve_in_sandbox};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

pub const MAX_RESULTS: usize = 100;
const MAX_FILE_BYTES: u64 = 1_000_000;
const MAX_CONTEXT_LINES: usize = 5;

/// Bangun pencocok glob opsional untuk `search_text` dan `list_files`.
/// Pola tanpa pemisah `/` (mis. `"*.rs"`) dicocokkan terhadap nama file maupun path relatif.
pub(crate) fn compile_path_glob(raw_glob: Option<&str>) -> Result<Option<(globset::GlobMatcher, bool)>> {
    let Some(pat) = raw_glob.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let norm = pat.replace('\\', "/");
    let has_slash = norm.contains('/');
    let matcher = globset::Glob::new(&norm)
        .map_err(|e| anyhow!("pola glob '{pat}' tidak valid: {e}"))?
        .compile_matcher();
    Ok(Some((matcher, has_slash)))
}

pub(crate) fn path_matches_glob(
    rel: &std::path::Path,
    compiled: &Option<(globset::GlobMatcher, bool)>,
) -> bool {
    let Some((matcher, has_slash)) = compiled else {
        return true;
    };
    let rel_norm = rel.to_string_lossy().replace('\\', "/");
    if matcher.is_match(&rel_norm) {
        return true;
    }
    if !has_slash {
        if let Some(fname) = rel.file_name().and_then(|s| s.to_str()) {
            return matcher.is_match(fname);
        }
    }
    false
}

/// search_text — pencarian teks/regex di workspace (hormati .gitignore, maks 100 hasil),
/// mendukung `glob`, `case_insensitive`, `fixed_strings`, `context_lines`, dan `files_only` (v0.5.28).
pub struct SearchText;

#[async_trait]
impl Tool for SearchText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_text".into(),
            description: "Cari teks/regex di workspace (maks 100; dukung glob, case_insensitive, fixed_strings, context_lines, files_only).".into(),
            parameters: schema(
                json!({
                    "query": {"type": "string"},
                    "path": {"type": "string"},
                    "glob": {"type": "string"},
                    "case_insensitive": {"type": "boolean"},
                    "fixed_strings": {"type": "boolean"},
                    "context_lines": {"type": "integer"},
                    "files_only": {"type": "boolean"}
                }),
                &["query"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let query = str_arg(&args, "query")
            .or_else(|| str_arg(&args, "pattern"))
            .ok_or_else(|| anyhow!("argumen 'query' wajib"))?;
        let fixed_strings = args
            .get("fixed_strings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let case_insensitive = args
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let files_only = args
            .get("files_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let context_lines = args
            .get("context_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_CONTEXT_LINES as u64) as usize;

        let pat = if fixed_strings {
            regex::escape(query)
        } else {
            query.to_string()
        };
        let re = regex::RegexBuilder::new(&pat)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| anyhow!("regex tidak valid: {e}"))?;

        let sub = str_arg(&args, "path").unwrap_or(".");
        let root = resolve_in_sandbox(&ctx.cwd, std::path::Path::new(sub))?;
        let glob_filter = compile_path_glob(str_arg(&args, "glob"))?;

        let mut out: Vec<String> = Vec::new();
        let mut match_count = 0usize;

        'walk: for e in ignore::WalkBuilder::new(&root).require_git(false).build().flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = e.path();
            if is_secret_file(p) {
                continue;
            }
            let rel = p.strip_prefix(&ctx.cwd).unwrap_or(p);
            if !path_matches_glob(rel, &glob_filter) {
                continue;
            }
            if e.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(p) else { continue };
            if bytes.contains(&0) {
                continue; // biner, lewati
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let matching_idxs: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter_map(|(i, line)| if re.is_match(line) { Some(i) } else { None })
                .collect();
            if matching_idxs.is_empty() {
                continue;
            }

            if files_only {
                out.push(format!("{} ({} cocok)", rel.display(), matching_idxs.len()));
                match_count += 1;
                if match_count >= MAX_RESULTS {
                    break 'walk;
                }
                continue;
            }

            if context_lines == 0 {
                for idx in matching_idxs {
                    out.push(format!("{}:{}: {}", rel.display(), idx + 1, lines[idx].trim()));
                    match_count += 1;
                    if match_count >= MAX_RESULTS {
                        break 'walk;
                    }
                }
            } else {
                // Gabungkan rentang konteks yang tumpang tindih agar tidak mencetak baris ganda.
                let mut hit_set = std::collections::HashSet::with_capacity(matching_idxs.len());
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                for idx in matching_idxs {
                    hit_set.insert(idx);
                    match_count += 1;
                    let lo = idx.saturating_sub(context_lines);
                    let hi = (idx + context_lines).min(lines.len().saturating_sub(1));
                    if let Some(last) = ranges.last_mut() {
                        if lo <= last.1 + 1 {
                            last.1 = last.1.max(hi);
                        } else {
                            ranges.push((lo, hi));
                        }
                    } else {
                        ranges.push((lo, hi));
                    }
                    if match_count >= MAX_RESULTS {
                        break;
                    }
                }
                for (r_idx, (lo, hi)) in ranges.into_iter().enumerate() {
                    if r_idx > 0 {
                        out.push("--".to_string());
                    }
                    for i in lo..=hi {
                        let sep = if hit_set.contains(&i) { ':' } else { '-' };
                        out.push(format!("{}:{}{sep} {}", rel.display(), i + 1, lines[i].trim_end()));
                    }
                }
                if match_count >= MAX_RESULTS {
                    break 'walk;
                }
            }
        }
        if out.is_empty() {
            return Ok(format!("Tidak ada hasil untuk '{query}'."));
        }
        Ok(out.join("\n"))
    }
}
