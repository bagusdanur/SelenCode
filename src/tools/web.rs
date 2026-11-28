use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use crate::security::redact_secrets;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::OnceLock;

pub const DEFAULT_WEB_MAX_CHARS: usize = 15_000;
const MIN_WEB_MAX_CHARS: usize = 1_000;
const MAX_WEB_MAX_CHARS: usize = 60_000;
const MAX_RAW: usize = 1024 * 1024;
const TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;
const MAX_BATCH_URLS: usize = 4;
const USER_AGENT: &str = "SelenCode/0.5 (fetch_url)";

/// Apakah IP ini tujuan terlarang (SSRF): privat, loopback, link-local,
/// unspecified, multicast, broadcast, CGNAT (100.64/10), 0.0.0.0/8, 192.0.0.0/24,
/// 198.18.0.0/15, 240.0.0.0/4, IPv4-mapped/compatible, NAT64 (64:ff9b::/96), fec0::/10.
pub fn is_forbidden_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || o[0] == 0 // 0.0.0.0/8 ("this network", routes locally on Linux)
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 IETF protocol
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18.0.0/15 benchmark
                || o[0] >= 240 // 240.0.0.0/4 reserved
        }
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            let extract_v4 = || {
                Ipv4Addr::new(
                    (segs[6] >> 8) as u8,
                    segs[6] as u8,
                    (segs[7] >> 8) as u8,
                    segs[7] as u8,
                )
            };
            // IPv4-mapped (::ffff:0:0/96), IPv4-compatible (::/96), atau NAT64 (64:ff9b::/96)
            let embedded_v4_forbidden =
                (segs[0..5] == [0, 0, 0, 0, 0] && (segs[5] == 0xffff || segs[5] == 0))
                    || (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6] == [0, 0, 0, 0]);
            let mapped_forbidden =
                embedded_v4_forbidden && is_forbidden_ip(&IpAddr::V4(extract_v4()));
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segs[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (segs[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || (segs[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
                || *v6 == Ipv6Addr::LOCALHOST
                || mapped_forbidden
        }
    }
}

/// Baca body respons HTTP secara terikat memori (maks `max_bytes`, cegah OOM DoS).
async fn read_response_text_bounded(mut resp: reqwest::Response, max_bytes: usize) -> Result<String> {
    if let Some(len) = resp.content_length() {
        if len as usize > max_bytes.saturating_mul(2) {
            return Err(anyhow!("respons HTTP terlalu besar ({len} byte)"));
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let rem = max_bytes.saturating_sub(buf.len());
        if rem == 0 {
            break;
        }
        let take = chunk.len().min(rem);
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= max_bytes {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Validasi URL untuk fetch_url: skema http/https saja, host bukan IP
/// privat/loopback, dan SEMUA hasil resolusi DNS host lolos cek yang sama.
/// Mengembalikan Url yang sudah divalidasi.
pub async fn check_url_allowed(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|e| anyhow!("URL tidak valid: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(anyhow!("skema '{other}' ditolak: hanya http/https")),
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("URL tanpa host"))?
        .to_string();
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        if is_forbidden_ip(&ip) {
            return Err(anyhow!(
                "host {host} ditolak: IP privat/loopback/link-local tidak boleh di-fetch (SSRF)"
            ));
        }
        return Ok(url);
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| anyhow!("gagal resolusi DNS {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(anyhow!("host {host} tidak menghasilkan alamat apa pun"));
    }
    for a in &addrs {
        if is_forbidden_ip(&a.ip()) {
            return Err(anyhow!(
                "host {host} ditolak: beresolusi ke IP privat/loopback {} (SSRF)",
                a.ip()
            ));
        }
    }
    Ok(url)
}

fn is_textual_content_type(ct: &str) -> bool {
    let ct = ct.to_lowercase();
    ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("javascript")
        || ct.contains("rss")
        || ct.contains("atom")
}

/// Dekode entity HTML dasar & numerik (`&#NNN;`, `&#xHHH;`).
pub fn decode_html_entities(input: &str) -> String {
    let mut s = input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&copy;", "©");
    static RE_NUM_ENT: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE_NUM_ENT.get_or_init(|| regex::Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());
    s = re
        .replace_all(&s, |caps: &regex::Captures| {
            let code_str = &caps[1];
            let cp = if let Some(hex) = code_str.strip_prefix('x').or_else(|| code_str.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                code_str.parse::<u32>().ok()
            };
            cp.and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned();
    s
}

/// Ekstrak teks kasar dari HTML: buang script/style/komentar, buang tag,
/// rapikan entity dasar & whitespace. Dipertahankan untuk kompatibilitas mundur.
pub fn html_to_text(html: &str) -> String {
    static RE_SCRIPT: OnceLock<regex::Regex> = OnceLock::new();
    static RE_STYLE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_COMMENT: OnceLock<regex::Regex> = OnceLock::new();
    static RE_TAG: OnceLock<regex::Regex> = OnceLock::new();
    static RE_WS: OnceLock<regex::Regex> = OnceLock::new();
    let re_script = RE_SCRIPT
        .get_or_init(|| regex::Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>").unwrap());
    let re_style =
        RE_STYLE.get_or_init(|| regex::Regex::new(r"(?is)<style\b[^>]*>.*?</style\s*>").unwrap());
    let re_comment =
        RE_COMMENT.get_or_init(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
    let re_tag = RE_TAG.get_or_init(|| regex::Regex::new(r"(?s)<[^>]+>").unwrap());
    let re_ws = RE_WS.get_or_init(|| regex::Regex::new(r"\s+").unwrap());

    let s = re_script.replace_all(html, " ");
    let s = re_style.replace_all(&s, " ");
    let s = re_comment.replace_all(&s, " ");
    let s = re_tag.replace_all(&s, " ");
    let s = decode_html_entities(&s);
    re_ws.replace_all(&s, " ").trim().to_string()
}

/// Konversi HTML ke Markdown bersih (v0.5.28):
/// - Membuang `<script>`, `<style>`, `<noscript>`, `<svg>`, `<nav>`, `<header>`, `<footer>`, `<aside>`.
/// - Memilih isi `<article>` / `<main>` bila cukup substantif (>= 200 char).
/// - Mempertahankan struktur heading (`#..######`), blok kode (`<pre>`), kode sebaris (`<code>`),
///   daftar (`<li>`), paragraf, dan tautan `[teks](https://...)`.
pub fn html_to_markdown(html: &str) -> String {
    static RE_TITLE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_NOISE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_CHROME: OnceLock<regex::Regex> = OnceLock::new();
    static RE_ARTICLE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_MAIN: OnceLock<regex::Regex> = OnceLock::new();
    static RE_PRE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_CODE: OnceLock<regex::Regex> = OnceLock::new();
    static RE_HEADING: OnceLock<regex::Regex> = OnceLock::new();
    static RE_LINK: OnceLock<regex::Regex> = OnceLock::new();
    static RE_LI_OPEN: OnceLock<regex::Regex> = OnceLock::new();
    static RE_BR: OnceLock<regex::Regex> = OnceLock::new();
    static RE_BLOCK_END: OnceLock<regex::Regex> = OnceLock::new();
    static RE_TAG: OnceLock<regex::Regex> = OnceLock::new();
    static RE_SPACES: OnceLock<regex::Regex> = OnceLock::new();
    static RE_BLANKS: OnceLock<regex::Regex> = OnceLock::new();

    let re_title = RE_TITLE.get_or_init(|| regex::Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").unwrap());
    let re_noise = RE_NOISE.get_or_init(|| {
        regex::Regex::new(
            r"(?is)<!--.*?-->|<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<noscript\b[^>]*>.*?</noscript\s*>|<svg\b[^>]*>.*?</svg\s*>|<iframe\b[^>]*>.*?</iframe\s*>|<template\b[^>]*>.*?</template\s*>",
        )
        .unwrap()
    });
    let re_chrome = RE_CHROME.get_or_init(|| {
        regex::Regex::new(
            r"(?is)<nav\b[^>]*>.*?</nav\s*>|<footer\b[^>]*>.*?</footer\s*>|<header\b[^>]*>.*?</header\s*>|<aside\b[^>]*>.*?</aside\s*>",
        )
        .unwrap()
    });
    let re_article = RE_ARTICLE.get_or_init(|| regex::Regex::new(r"(?is)<article\b[^>]*>(.*?)</article\s*>").unwrap());
    let re_main = RE_MAIN.get_or_init(|| regex::Regex::new(r"(?is)<main\b[^>]*>(.*?)</main\s*>").unwrap());
    let re_pre = RE_PRE.get_or_init(|| regex::Regex::new(r"(?is)<pre\b[^>]*>(.*?)</pre\s*>").unwrap());
    let re_code = RE_CODE.get_or_init(|| regex::Regex::new(r"(?is)<code\b[^>]*>(.*?)</code>").unwrap());
    let re_heading = RE_HEADING.get_or_init(|| regex::Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").unwrap());
    let re_link = RE_LINK.get_or_init(|| {
        regex::Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>(.*?)</a\s*>"#).unwrap()
    });
    let re_li_open = RE_LI_OPEN.get_or_init(|| regex::Regex::new(r"(?is)<li\b[^>]*>").unwrap());
    let re_br = RE_BR.get_or_init(|| regex::Regex::new(r"(?is)<br\s*/?>").unwrap());
    let re_block_end = RE_BLOCK_END.get_or_init(|| {
        regex::Regex::new(r"(?is)</(?:p|div|section|article|main|li|tr|blockquote|ul|ol|table|dl|dt|dd)\s*>").unwrap()
    });
    let re_tag = RE_TAG.get_or_init(|| regex::Regex::new(r"(?s)<[^>]+>").unwrap());
    let re_spaces = RE_SPACES.get_or_init(|| regex::Regex::new(r"[ \t]{2,}").unwrap());
    let re_blanks = RE_BLANKS.get_or_init(|| regex::Regex::new(r"\n{3,}").unwrap());

    let title = re_title
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| decode_html_entities(&re_tag.replace_all(m.as_str(), " ")))
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());

    let cleaned = re_noise.replace_all(html, " ").into_owned();

    // Pilih <article> atau <main> bila mengandung isi substantif (>= 200 char setelah strip tag).
    let focused = re_article
        .captures(&cleaned)
        .or_else(|| re_main.captures(&cleaned))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|inner| re_tag.replace_all(inner, " ").trim().chars().count() >= 200)
        .unwrap_or_else(|| {
            let without_chrome = re_chrome.replace_all(&cleaned, " ").into_owned();
            if re_tag.replace_all(&without_chrome, " ").trim().chars().count() >= 120 {
                without_chrome
            } else {
                cleaned
            }
        });

    // Lindungi blok <pre> dengan placeholder agar spasi & baris kode utuh.
    let mut code_blocks: Vec<String> = Vec::new();
    let with_pre_tokens = re_pre
        .replace_all(&focused, |caps: &regex::Captures| {
            let raw_code = re_tag.replace_all(&caps[1], "");
            let decoded = decode_html_entities(&raw_code);
            let idx = code_blocks.len();
            code_blocks.push(format!("\n```\n{}\n```\n", decoded.trim_matches('\n')));
            format!("\n@@SELEN_CODE_BLOCK_{idx}@@\n")
        })
        .into_owned();

    let s = re_heading
        .replace_all(&with_pre_tokens, |caps: &regex::Captures| {
            let level = caps[1].parse::<usize>().unwrap_or(2).clamp(1, 6);
            let inner = decode_html_entities(&re_tag.replace_all(&caps[2], " "));
            let text = inner.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                String::new()
            } else {
                format!("\n\n{} {text}\n", "#".repeat(level))
            }
        })
        .into_owned();

    let s = re_code
        .replace_all(&s, |caps: &regex::Captures| {
            let inner = decode_html_entities(&re_tag.replace_all(&caps[1], ""));
            let clean = inner.trim();
            if clean.is_empty() || clean.contains('\n') {
                inner
            } else {
                format!("`{clean}`")
            }
        })
        .into_owned();

    let s = re_link
        .replace_all(&s, |caps: &regex::Captures| {
            let href = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str().trim())
                .unwrap_or("");
            let inner = decode_html_entities(&re_tag.replace_all(&caps[3], " "));
            let label = inner.split_whitespace().collect::<Vec<_>>().join(" ");
            if label.is_empty() {
                String::new()
            } else if href.starts_with("http://") || href.starts_with("https://") {
                format!("[{label}]({href})")
            } else {
                label
            }
        })
        .into_owned();

    let s = re_li_open.replace_all(&s, "\n- ").into_owned();
    let s = re_br.replace_all(&s, "\n").into_owned();
    let s = re_block_end.replace_all(&s, "\n").into_owned();
    let s = re_tag.replace_all(&s, " ").into_owned();
    let s = decode_html_entities(&s);

    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        let collapsed = re_spaces.replace_all(line.trim(), " ").into_owned();
        lines.push(collapsed);
    }
    let mut joined = re_blanks.replace_all(&lines.join("\n"), "\n\n").trim().to_string();

    for (idx, block) in code_blocks.into_iter().enumerate() {
        joined = joined.replace(&format!("@@SELEN_CODE_BLOCK_{idx}@@"), &block);
    }
    joined = re_blanks.replace_all(&joined, "\n\n").trim().to_string();

    if let Some(t) = title {
        if !joined.starts_with(&format!("# {t}")) {
            return format!("# {t}\n\n{joined}");
        }
    }
    joined
}

/// Parse RSS 2.0 / Atom XML menjadi daftar Markdown ringkas (maks 20 entri).
pub fn parse_rss_or_atom_feed(xml: &str) -> Option<String> {
    let head: String = xml.chars().take(600).collect();
    let low = head.to_ascii_lowercase();
    if !(low.contains("<rss") || low.contains("<feed") || low.contains("<rdf:rdf")) {
        return None;
    }
    static RE_ENTRY: OnceLock<regex::Regex> = OnceLock::new();
    static RE_LINK_HREF: OnceLock<regex::Regex> = OnceLock::new();
    static RE_CDATA: OnceLock<regex::Regex> = OnceLock::new();

    let re_entry = RE_ENTRY.get_or_init(|| {
        regex::Regex::new(r"(?is)<(?:item|entry)\b[^>]*>(.*?)</(?:item|entry)\s*>").unwrap()
    });
    let re_link_href = RE_LINK_HREF.get_or_init(|| {
        regex::Regex::new(r#"(?is)<link\b[^>]*href\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*/?>"#).unwrap()
    });
    let re_cdata = RE_CDATA.get_or_init(|| regex::Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").unwrap());

    let extract_tag = |src: &str, tag: &str| -> Option<String> {
        let pat = format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}\s*>");
        let re = regex::Regex::new(&pat).ok()?;
        let raw = re.captures(src)?.get(1)?.as_str();
        let uncdata = re_cdata.replace_all(raw, "$1");
        let clean = html_to_text(&uncdata);
        if clean.is_empty() { None } else { Some(clean) }
    };

    let entries: Vec<&str> = re_entry
        .captures_iter(xml)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    if entries.is_empty() {
        return None;
    }

    let feed_title = extract_tag(&head, "title").unwrap_or_else(|| "Feed".to_string());
    let mut out = format!("# Feed: {feed_title} ({} entri)\n\n", entries.len());
    for entry in entries.into_iter().take(20) {
        let title = extract_tag(entry, "title").unwrap_or_else(|| "(tanpa judul)".to_string());
        let link = re_link_href
            .captures(entry)
            .and_then(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().trim().to_string()))
            .or_else(|| extract_tag(entry, "link"))
            .unwrap_or_default();
        let date = extract_tag(entry, "pubDate")
            .or_else(|| extract_tag(entry, "updated"))
            .or_else(|| extract_tag(entry, "published"))
            .unwrap_or_default();
        let desc = extract_tag(entry, "description")
            .or_else(|| extract_tag(entry, "summary"))
            .map(|d| d.chars().take(220).collect::<String>())
            .unwrap_or_default();
        if date.is_empty() {
            out.push_str(&format!("- **{title}**\n"));
        } else {
            out.push_str(&format!("- **{title}** ({date})\n"));
        }
        if !link.is_empty() {
            out.push_str(&format!("  {link}\n"));
        }
        if !desc.is_empty() {
            out.push_str(&format!("  {desc}\n"));
        }
    }
    Some(out.trim_end().to_string())
}

/// Jenis jalur cepat per-domain tanpa API key untuk `fetch_url` (v0.5.28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebFastPath {
    GitHubRepo { owner: String, repo: String },
    GitHubIssueOrPr { owner: String, repo: String, number: String, is_pr: bool },
    GitHubBlob { owner: String, repo: String, git_ref: String, file_path: String },
    Wikipedia { lang: String, title: String },
    HackerNewsItem { id: String },
}

/// Deteksi apakah URL cocok dengan jalur API resmi yang lebih bersih & hemat token.
pub fn detect_fast_path(url: &reqwest::Url) -> Option<WebFastPath> {
    let raw_host = url.host_str()?.to_ascii_lowercase();
    let host = raw_host
        .strip_prefix("www.")
        .or_else(|| raw_host.strip_prefix("m."))
        .unwrap_or(&raw_host);
    let segs: Vec<&str> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    if host == "github.com" && segs.len() >= 2 {
        let owner = segs[0].to_string();
        let repo = segs[1].trim_end_matches(".git").to_string();
        if segs.len() >= 4
            && (segs[2] == "issues" || segs[2] == "pull")
            && segs[3].chars().all(|c| c.is_ascii_digit())
        {
            return Some(WebFastPath::GitHubIssueOrPr {
                owner,
                repo,
                number: segs[3].to_string(),
                is_pr: segs[2] == "pull",
            });
        }
        if segs.len() >= 5 && segs[2] == "blob" {
            return Some(WebFastPath::GitHubBlob {
                owner,
                repo,
                git_ref: segs[3].to_string(),
                file_path: segs[4..].join("/"),
            });
        }
        if segs.len() == 2 {
            return Some(WebFastPath::GitHubRepo { owner, repo });
        }
    }

    if let Some(lang) = host.strip_suffix(".wikipedia.org") {
        if !lang.is_empty() && !lang.contains('.') && segs.len() >= 2 && segs[0] == "wiki" {
            return Some(WebFastPath::Wikipedia {
                lang: lang.to_string(),
                title: segs[1..].join("/"),
            });
        }
    }

    if host == "news.ycombinator.com" && segs.first().copied() == Some("item") {
        for (k, v) in url.query_pairs() {
            if k == "id" && !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
                return Some(WebFastPath::HackerNewsItem { id: v.into_owned() });
            }
        }
    }

    None
}

fn fnv1a_hex12(input: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:012x}")
}

/// Terapkan pagu karakter deterministik (default 15.000 char) dengan spill-to-disk (head + tail)
/// ke `.selencode/web_cache/web_<hash>.md` agar halaman besar tidak membanjiri konteks LLM
/// namun seluruh isinya tetap dapat dibaca bertahap via `read_file` (v0.5.28).
pub fn spill_web_output_if_needed(
    cwd: &Path,
    url: &str,
    content: &str,
    max_chars: usize,
) -> String {
    let safe = redact_secrets(content);
    let budget = max_chars.clamp(MIN_WEB_MAX_CHARS, MAX_WEB_MAX_CHARS);
    let total_chars = safe.chars().count();
    if total_chars <= budget {
        return safe;
    }

    let hash = fnv1a_hex12(url);
    let rel_cache = format!(".selencode/web_cache/web_{hash}.md");
    let cache_abs = cwd.join(&rel_cache);
    let saved_ok = cache_abs
        .parent()
        .and_then(|p| std::fs::create_dir_all(p).ok())
        .and_then(|_| std::fs::write(&cache_abs, &safe).ok())
        .is_some();

    let total_lines = safe.lines().count().max(1);
    let head_cap = (budget * 65 / 100).max(400);
    let tail_cap = (budget * 22 / 100).max(200);
    let head: String = safe.chars().take(head_cap).collect();
    let tail: String = safe
        .chars()
        .rev()
        .take(tail_cap)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let note = if saved_ok {
        format!(
            "[Konten dipotong di pagu {budget} char (total {total_chars} char / {total_lines} baris). \
             Isi penuh tersimpan di `{rel_cache}` — gunakan `read_file(path=\"{rel_cache}\", start=..., end=...)` \
             untuk membaca bagian tengah yang terlewati.]"
        )
    } else {
        format!(
            "[Konten dipotong di pagu {budget} char (total {total_chars} char / {total_lines} baris).]"
        )
    };
    format!("{head}\n\n... {note} ...\n\n{tail}")
}

async fn fetch_checked_http(
    client: &reqwest::Client,
    initial_url: reqwest::Url,
    extra_accept: Option<&str>,
) -> Result<(String, String)> {
    let mut url = initial_url;
    let mut hops = 0usize;
    loop {
        let mut req = client.get(url.clone()).header("user-agent", USER_AGENT);
        if let Some(acc) = extra_accept {
            req = req.header("accept", acc);
        }
        let resp = req.send().await.map_err(|e| anyhow!("fetch gagal: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            if hops >= MAX_REDIRECTS {
                return Err(anyhow!("terlalu banyak redirect (maks {MAX_REDIRECTS})"));
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect {status} tanpa header Location"))?;
            let next = url
                .join(loc)
                .map_err(|e| anyhow!("URL redirect tidak valid: {e}"))?;
            url = check_url_allowed(next.as_str()).await?;
            hops += 1;
            continue;
        }
        if !status.is_success() {
            return Err(anyhow!("server membalas {status}"));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !ct.is_empty() && !is_textual_content_type(&ct) {
            return Err(anyhow!(
                "content-type '{ct}' ditolak: fetch_url hanya mengambil teks"
            ));
        }
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("baca body gagal: {e}"))?;
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_RAW {
                break;
            }
        }
        return Ok((ct, String::from_utf8_lossy(&buf).into_owned()));
    }
}

async fn fetch_json_checked(client: &reqwest::Client, raw_url: &str) -> Result<Value> {
    let url = check_url_allowed(raw_url).await?;
    let (_, body) = fetch_checked_http(client, url, Some("application/json")).await?;
    serde_json::from_str(&body).map_err(|e| anyhow!("respons JSON tidak valid: {e}"))
}

async fn try_fast_path_extract(
    client: &reqwest::Client,
    fp: &WebFastPath,
) -> Result<String> {
    match fp {
        WebFastPath::GitHubRepo { owner, repo } => {
            let api = format!("https://api.github.com/repos/{owner}/{repo}");
            let meta = fetch_json_checked(client, &api).await?;
            let full_name = meta["full_name"].as_str().unwrap_or(repo);
            let desc = meta["description"].as_str().unwrap_or("");
            let stars = meta["stargazers_count"].as_u64().unwrap_or(0);
            let lang = meta["language"].as_str().unwrap_or("-");
            let license = meta["license"]["spdx_id"].as_str().unwrap_or("-");
            let pushed: String = meta["pushed_at"]
                .as_str()
                .map(|s| s.chars().take(10).collect())
                .unwrap_or_else(|| "?".to_string());
            let readme_url = check_url_allowed(&format!("{api}/readme")).await?;
            let readme = fetch_checked_http(
                client,
                readme_url,
                Some("application/vnd.github.raw"),
            )
            .await
            .map(|(_, b)| b)
            .unwrap_or_default();
            let mut out = format!(
                "# GitHub: {full_name}\n{desc}\n\n- Bintang: {stars} · Bahasa: {lang} · Lisensi: {license} · Diperbarui: {pushed}\n"
            );
            if !readme.trim().is_empty() {
                out.push_str("\n## README\n\n");
                out.push_str(readme.trim());
            }
            Ok(out)
        }
        WebFastPath::GitHubIssueOrPr {
            owner,
            repo,
            number,
            is_pr,
        } => {
            let base = format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}");
            let item = fetch_json_checked(client, &base).await?;
            let kind = if *is_pr { "PR" } else { "Issue" };
            let title = item["title"].as_str().unwrap_or("(tanpa judul)");
            let state = item["state"].as_str().unwrap_or("?");
            let author = item["user"]["login"].as_str().unwrap_or("?");
            let body = item["body"].as_str().unwrap_or("");
            let mut out = format!("# GitHub {kind} #{number} ({state}): {title}\nOleh: @{author}\n\n{body}\n");
            if let Ok(Value::Array(comments)) =
                fetch_json_checked(client, &format!("{base}/comments?per_page=20")).await
            {
                if !comments.is_empty() {
                    out.push_str("\n## Komentar\n");
                    for c in comments {
                        let who = c["user"]["login"].as_str().unwrap_or("?");
                        let txt = c["body"].as_str().unwrap_or("").trim();
                        if !txt.is_empty() {
                            out.push_str(&format!("\n### @{who}\n{txt}\n"));
                        }
                    }
                }
            }
            Ok(out)
        }
        WebFastPath::GitHubBlob {
            owner,
            repo,
            git_ref,
            file_path,
        } => {
            let raw_url = format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/{file_path}"
            );
            let url = check_url_allowed(&raw_url).await?;
            let (_, body) = fetch_checked_http(client, url, Some("text/plain")).await?;
            Ok(format!("# GitHub File: {owner}/{repo}@{git_ref}/{file_path}\n\n{body}"))
        }
        WebFastPath::Wikipedia { lang, title } => {
            let enc_title = percent_encode_query(title);
            let api = format!(
                "https://{lang}.wikipedia.org/w/api.php?action=query&prop=extracts&explaintext=1&format=json&formatversion=2&titles={enc_title}"
            );
            let data = fetch_json_checked(client, &api).await?;
            let page = data["query"]["pages"]
                .as_array()
                .and_then(|a| a.first())
                .ok_or_else(|| anyhow!("artikel Wikipedia tidak ditemukan"))?;
            if page["missing"].as_bool().unwrap_or(false) {
                return Err(anyhow!("artikel Wikipedia tidak ditemukan"));
            }
            let ptitle = page["title"].as_str().unwrap_or(title);
            let extract = page["extract"].as_str().unwrap_or("");
            Ok(format!("# Wikipedia ({lang}): {ptitle}\n\n{extract}"))
        }
        WebFastPath::HackerNewsItem { id } => {
            let data = fetch_json_checked(
                client,
                &format!("https://hn.algolia.com/api/v1/items/{id}"),
            )
            .await?;
            let title = data["title"].as_str().unwrap_or("(komentar)");
            let points = data["points"].as_u64().unwrap_or(0);
            let author = data["author"].as_str().unwrap_or("?");
            let link = data["url"].as_str().unwrap_or("");
            let mut out = format!("# Hacker News: {title}\nPoin: {points} · Oleh: {author}");
            if !link.is_empty() {
                out.push_str(&format!(" · {link}"));
            }
            out.push('\n');
            if let Some(txt) = data["text"].as_str().filter(|s| !s.is_empty()) {
                out.push('\n');
                out.push_str(&html_to_markdown(txt));
                out.push('\n');
            }
            fn collect_hn_comments(node: &Value, depth: usize, out: &mut Vec<String>) {
                if let Some(children) = node["children"].as_array() {
                    for ch in children {
                        if out.len() >= 25 {
                            return;
                        }
                        if let Some(txt) = ch["text"].as_str().filter(|s| !s.is_empty()) {
                            let who = ch["author"].as_str().unwrap_or("?");
                            let indent = "  ".repeat(depth);
                            out.push(format!("{indent}- **{who}**: {}", html_to_text(txt)));
                        }
                        if depth < 2 {
                            collect_hn_comments(ch, depth + 1, out);
                        }
                    }
                }
            }
            let mut comments = Vec::new();
            collect_hn_comments(&data, 0, &mut comments);
            if !comments.is_empty() {
                out.push_str("\n## Diskusi\n");
                out.push_str(&comments.join("\n"));
            }
            Ok(out)
        }
    }
}

async fn try_optional_extract_backend(
    client: &reqwest::Client,
    backend: &str,
    api_key_env: Option<&str>,
    target_url: &str,
) -> Option<String> {
    match backend.trim().to_ascii_lowercase().as_str() {
        "jina" => {
            let jina_url = check_url_allowed(&format!("https://r.jina.ai/{target_url}")).await.ok()?;
            let resp = client
                .get(jina_url)
                .header("user-agent", USER_AGENT)
                .header("x-return-format", "markdown")
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let text = read_response_text_bounded(resp, MAX_RAW).await.ok()?;
            let trimmed = text.trim();
            if trimmed.len() > 80 {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        "firecrawl" => {
            let env_name = api_key_env.unwrap_or("FIRECRAWL_API_KEY");
            let key = std::env::var(env_name).ok().filter(|v| !v.trim().is_empty())?;
            let resp = client
                .post("https://api.firecrawl.dev/v1/scrape")
                .header("user-agent", USER_AGENT)
                .bearer_auth(key.trim())
                .json(&json!({ "url": target_url, "formats": ["markdown"] }))
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let raw = read_response_text_bounded(resp, MAX_RAW).await.ok()?;
            let v: Value = serde_json::from_str(&raw).ok()?;
            v["data"]["markdown"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Deteksi apakah dokumen HTML adalah shell SPA (React/Vue/Next/Nuxt/Angular) yang hampir
/// kosong tanpa eksekusi JavaScript (sehingga layak dicoba dengan fallback headless Chrome).
pub fn detect_spa_shell(html: &str) -> bool {
    let low = html.to_ascii_lowercase();
    let has_spa_marker = low.contains("id=\"root\"")
        || low.contains("id='root'")
        || low.contains("id=\"app\"")
        || low.contains("id='app'")
        || low.contains("id=\"__next\"")
        || low.contains("id=\"__nuxt\"")
        || low.contains("enable javascript")
        || low.contains("javascript is required")
        || low.contains("need to enable javascript");
    if !has_spa_marker {
        return false;
    }
    let visible = html_to_markdown(html);
    visible.trim().chars().count() < 180
}

/// Temukan binary browser Chromium/Chrome/Edge lokal untuk rendering JS opsional (`fetch_url`).
pub fn find_headless_chrome_binary() -> Option<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "chrome.exe",
            "msedge.exe",
            "chromium.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        ]
    } else {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let path_dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path_var).collect();
    for cand in candidates {
        let p = std::path::Path::new(cand);
        if p.is_absolute() && p.is_file() {
            return Some((*cand).to_string());
        }
        for dir in &path_dirs {
            let full = dir.join(cand);
            if full.is_file() {
                return Some(full.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Render halaman SPA via browser lokal headless (`--headless=new --dump-dom`, batas waktu 15 detik).
/// WAJIB hanya dipanggil bila `ctx.web_cfg.headless_browser == true` DAN URL lulus `check_url_allowed`.
async fn fetch_via_headless_chrome(url: &str) -> Option<String> {
    let bin = find_headless_chrome_binary()?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--virtual-time-budget=4500")
        .arg("--dump-dom")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::sandbox::scrub_tokio_command(&mut cmd);
    let (code, dom) = super::run_timed(&mut cmd, 15).await.ok()?;
    if code != Some(0) || dom.trim().is_empty() {
        return None;
    }
    let md = html_to_markdown(&dom);
    if md.trim().chars().count() > 60 {
        Some(md)
    } else {
        None
    }
}

async fn fetch_single_url_markdown(
    client: &reqwest::Client,
    url: reqwest::Url,
    ctx: &Ctx,
    want_raw: bool,
    js_render: bool,
) -> Result<String> {
    // Keamanan SSRF: headless Chrome HANYA boleh dipakai bila diizinkan eksplisit di `[web] headless_browser = true`!
    let allow_headless = ctx.web_cfg.headless_browser;
    let url_str = url.to_string();
    if !want_raw {
        if js_render && allow_headless {
            if let Some(rendered) = fetch_via_headless_chrome(&url_str).await {
                return Ok(rendered);
            }
        }
        if let Some(fp) = detect_fast_path(&url) {
            if let Ok(fast) = try_fast_path_extract(client, &fp).await {
                if !fast.trim().is_empty() {
                    return Ok(fast);
                }
            }
        }
        if !ctx.web_cfg.extract_backend.eq_ignore_ascii_case("direct") {
            if let Some(md) = try_optional_extract_backend(
                client,
                &ctx.web_cfg.extract_backend,
                ctx.web_cfg.api_key_env.as_deref(),
                url.as_str(),
            )
            .await
            {
                return Ok(md);
            }
        }
    }

    let (content_type, body) = fetch_checked_http(client, url, None).await?;
    if want_raw {
        return Ok(body);
    }
    if let Some(feed_md) = parse_rss_or_atom_feed(&body) {
        return Ok(feed_md);
    }
    let is_html = content_type.to_lowercase().contains("html")
        || body.trim_start().starts_with('<');
    if is_html {
        if allow_headless && detect_spa_shell(&body) {
            if let Some(rendered) = fetch_via_headless_chrome(&url_str).await {
                return Ok(rendered);
            }
        }
        Ok(html_to_markdown(&body))
    } else {
        Ok(body)
    }
}

// ---------- fetch_url ----------
pub struct FetchUrl;

#[async_trait]
impl Tool for FetchUrl {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fetch_url".into(),
            description: "Ambil isi bersih (Markdown/API GitHub/Wikipedia/HN/RSS, plus fallback opsional headless Chrome untuk SPA) dari 1–4 URL http/https. Anti-SSRF, pagu default 15.000 char (sisa di-cache ke .selencode/web_cache/).".into(),
            parameters: schema(
                json!({
                    "url": {"type": "string", "description": "URL http/https lengkap"},
                    "urls": {"type": "array", "items": {"type": "string"}, "description": "Opsional: daftar 1-4 URL sekaligus"},
                    "max_chars": {"type": "integer", "description": "Opsional: pagu karakter keluaran (default 15000)"},
                    "raw": {"type": "boolean", "description": "Opsional: kembalikan teks mentah tanpa konversi Markdown"},
                    "js_render": {"type": "boolean", "description": "Opsional: render halaman SPA memakai Chrome/Chromium headless lokal bila tersedia (default: false)"}
                }),
                &[],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let mut raw_urls: Vec<String> = Vec::new();
        if let Some(u) = str_arg(&args, "url").map(str::trim).filter(|s| !s.is_empty()) {
            raw_urls.push(u.to_string());
        }
        if let Some(arr) = args.get("urls").and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    if !raw_urls.iter().any(|x| x == s) {
                        raw_urls.push(s.to_string());
                    }
                }
            }
        }
        if raw_urls.is_empty() {
            return Err(anyhow!("argumen 'url' (atau 'urls') wajib diisi"));
        }
        raw_urls.truncate(MAX_BATCH_URLS);

        // Validasi SSRF seluruh URL SEBELUM approval & sebelum jaringan disentuh.
        let mut validated: Vec<reqwest::Url> = Vec::with_capacity(raw_urls.len());
        for raw in &raw_urls {
            validated.push(check_url_allowed(raw).await?);
        }

        let approval_title = if validated.len() == 1 {
            format!("fetch_url: {}", validated[0].host_str().unwrap_or("?"))
        } else {
            format!("fetch_url ({} URL)", validated.len())
        };
        let js_render = args
            .get("js_render")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && ctx.web_cfg.headless_browser;
        let mut approval_detail = validated
            .iter()
            .map(|u| u.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if js_render {
            approval_detail.push_str("\n[Mode: headless Chrome JS render aktif]");
        }
        if !ctx.approver.ask(&approval_title, &approval_detail).await {
            return Ok("Ditolak user — URL tidak di-fetch.".into());
        }

        // Validasi ulang DNS pasca-approval untuk memperkecil celah DNS rebinding TOCTOU.
        let mut revalidated: Vec<reqwest::Url> = Vec::with_capacity(raw_urls.len());
        for raw in &raw_urls {
            revalidated.push(check_url_allowed(raw).await?);
        }
        let mut validated = revalidated;

        let want_raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
        let base_budget = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(ctx.web_cfg.max_extract_chars)
            .clamp(MIN_WEB_MAX_CHARS, MAX_WEB_MAX_CHARS);
        let per_url_budget = (base_budget / validated.len().max(1)).max(MIN_WEB_MAX_CHARS);

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()?;

        if validated.len() == 1 {
            let url = validated.remove(0);
            let url_str = url.to_string();
            let extracted = fetch_single_url_markdown(&client, url, ctx, want_raw, js_render).await?;
            return Ok(spill_web_output_if_needed(
                &ctx.cwd,
                &url_str,
                &extracted,
                per_url_budget,
            ));
        }

        let mut sections: Vec<String> = Vec::new();
        for url in validated {
            let url_str = url.to_string();
            match fetch_single_url_markdown(&client, url, ctx, want_raw, js_render).await {
                Ok(extracted) => {
                    let body = spill_web_output_if_needed(
                        &ctx.cwd,
                        &url_str,
                        &extracted,
                        per_url_budget,
                    );
                    sections.push(format!("=== {url_str} ===\n{body}"));
                }
                Err(e) => {
                    sections.push(format!("=== {url_str} ===\n[GAGAL] {e}"));
                }
            }
        }
        Ok(sections.join("\n\n"))
    }
}

// ---------- web_search ----------

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 2);
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse hasil HTML ringan dari DuckDuckGo (`html.duckduckgo.com/html/`) tanpa API key.
/// Mengembalikan `Vec<(title, url, snippet)>`.
pub fn parse_ddg_html_results(html: &str, limit: usize) -> Vec<(String, String, String)> {
    static RE_RESULT_LINK: OnceLock<regex::Regex> = OnceLock::new();
    static RE_SNIPPET: OnceLock<regex::Regex> = OnceLock::new();
    let re_link = RE_RESULT_LINK.get_or_init(|| {
        regex::Regex::new(
            r#"(?is)<a\b[^>]*class\s*=\s*"[^"]*result__a[^"]*"[^>]*href\s*=\s*"([^"]+)"[^>]*>(.*?)</a\s*>"#,
        )
        .unwrap()
    });
    let re_snip = RE_SNIPPET.get_or_init(|| {
        regex::Regex::new(
            r#"(?is)<(?:a|div|span)\b[^>]*class\s*=\s*"[^"]*result__snippet[^"]*"[^>]*>(.*?)</(?:a|div|span)\s*>"#,
        )
        .unwrap()
    });

    let snippets: Vec<String> = re_snip
        .captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| html_to_text(m.as_str())))
        .collect();

    let mut results = Vec::new();
    for (idx, caps) in re_link.captures_iter(html).enumerate() {
        if results.len() >= limit {
            break;
        }
        let raw_href = decode_html_entities(&caps[1]);
        let target_url = if let Some(pos) = raw_href.find("uddg=") {
            let after = &raw_href[pos + 5..];
            let end = after.find('&').unwrap_or(after.len());
            percent_decode(&after[..end])
        } else if raw_href.starts_with("//") {
            format!("https:{raw_href}")
        } else {
            raw_href
        };
        let title = html_to_text(&caps[2]);
        if title.is_empty() || target_url.is_empty() {
            continue;
        }
        let snip = snippets.get(idx).cloned().unwrap_or_default();
        results.push((title, target_url, snip));
    }
    results
}

fn format_search_results(
    source_label: &str,
    query: &str,
    items: &[(String, String, String)],
) -> String {
    if items.is_empty() {
        return format!("Tidak ada hasil pencarian di {source_label} untuk '{query}'.");
    }
    let mut out = format!("Hasil pencarian ({source_label}) untuk \"{query}\":\n\n");
    for (i, (title, url, snip)) in items.iter().enumerate() {
        out.push_str(&format!("{}. **{title}**\n   {url}\n", i + 1));
        if !snip.trim().is_empty() {
            let short: String = snip.chars().take(240).collect();
            out.push_str(&format!("   {short}\n"));
        }
    }
    redact_secrets(out.trim_end())
}

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Cari informasi di web (default tanpa API key: DuckDuckGo/GitHub/HN/Wikipedia; dukung backend opsional [web] SearXNG/Tavily/Brave/Firecrawl).".into(),
            parameters: schema(
                json!({
                    "query": {"type": "string", "description": "Kata kunci pencarian"},
                    "source": {"type": "string", "enum": ["web", "github", "hackernews", "wikipedia"], "description": "Sumber (default: web)"},
                    "limit": {"type": "integer", "description": "Jumlah hasil 1-10 (default: 5)"},
                    "lang": {"type": "string", "description": "Kode bahasa Wikipedia (id/en, default: en)"}
                }),
                &["query"],
            ),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let query = str_arg(&args, "query")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'query' wajib diisi"))?;
        let source = str_arg(&args, "source")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "web".to_string());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;
        let lang = str_arg(&args, "lang")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic()))
            .unwrap_or_else(|| "en".to_string());

        if !ctx
            .approver
            .ask(&format!("web_search ({source})"), query)
            .await
        {
            return Ok("Ditolak user — pencarian web tidak dijalankan.".into());
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()?;
        let enc_q = percent_encode_query(query);

        match source.as_str() {
            "github" => {
                let data = fetch_json_checked(
                    &client,
                    &format!("https://api.github.com/search/repositories?q={enc_q}&per_page={limit}"),
                )
                .await?;
                let items: Vec<(String, String, String)> = data["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(limit)
                    .map(|it| {
                        let name = it["full_name"].as_str().unwrap_or("?").to_string();
                        let url = it["html_url"].as_str().unwrap_or("").to_string();
                        let stars = it["stargazers_count"].as_u64().unwrap_or(0);
                        let lang = it["language"].as_str().unwrap_or("-");
                        let desc = it["description"].as_str().unwrap_or("");
                        (format!("{name} ({stars}★ · {lang})"), url, desc.to_string())
                    })
                    .collect();
                Ok(format_search_results("GitHub", query, &items))
            }
            "hackernews" => {
                let data = fetch_json_checked(
                    &client,
                    &format!("https://hn.algolia.com/api/v1/search?tags=story&hitsPerPage={limit}&query={enc_q}"),
                )
                .await?;
                let items: Vec<(String, String, String)> = data["hits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(limit)
                    .map(|it| {
                        let title = it["title"].as_str().unwrap_or("?").to_string();
                        let oid = it["objectID"].as_str().unwrap_or("");
                        let url = format!("https://news.ycombinator.com/item?id={oid}");
                        let pts = it["points"].as_u64().unwrap_or(0);
                        let cmts = it["num_comments"].as_u64().unwrap_or(0);
                        let ext = it["url"].as_str().unwrap_or("");
                        let snip = if ext.is_empty() {
                            format!("{pts} poin · {cmts} komentar")
                        } else {
                            format!("{pts} poin · {cmts} komentar · {ext}")
                        };
                        (title, url, snip)
                    })
                    .collect();
                Ok(format_search_results("Hacker News", query, &items))
            }
            "wikipedia" => {
                let data = fetch_json_checked(
                    &client,
                    &format!(
                        "https://{lang}.wikipedia.org/w/api.php?action=query&list=search&format=json&srlimit={limit}&srsearch={enc_q}"
                    ),
                )
                .await?;
                let items: Vec<(String, String, String)> = data["query"]["search"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(limit)
                    .map(|it| {
                        let title = it["title"].as_str().unwrap_or("?").to_string();
                        let slug = percent_encode_query(&title.replace(' ', "_"));
                        let url = format!("https://{lang}.wikipedia.org/wiki/{slug}");
                        let snip = html_to_text(it["snippet"].as_str().unwrap_or(""));
                        (title, url, snip)
                    })
                    .collect();
                Ok(format_search_results(&format!("Wikipedia ({lang})"), query, &items))
            }
            _ => {
                // Backend pencarian web umum sesuai `[web].search_backend` (default tanpa key: DuckDuckGo HTML).
                let backend = ctx.web_cfg.search_backend.trim().to_ascii_lowercase();
                if backend == "searxng" {
                    if let Some(base) = ctx
                        .web_cfg
                        .searxng_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
                    {
                        let url = format!(
                            "{}/search?q={enc_q}&format=json",
                            base.trim_end_matches('/')
                        );
                        // Blokir alamat metadata cloud (169.254.169.254 / link-local) pada searxng_url.
                        if let Ok(parsed_sx) = reqwest::Url::parse(&url) {
                            let host_ok = parsed_sx
                                .host_str()
                                .map(|h| !h.contains("169.254.169.254") && !h.eq_ignore_ascii_case("metadata.google.internal"))
                                .unwrap_or(false);
                            if !host_ok {
                                return Err(anyhow!("searxng_url ditolak demi keamanan (SSRF metadata)"));
                            }
                        }
                        let resp = client
                            .get(&url)
                            .header("user-agent", USER_AGENT)
                            .send()
                            .await
                            .map_err(|e| anyhow!("SearXNG gagal: {e}"))?;
                        if resp.status().is_success() {
                            let raw = read_response_text_bounded(resp, MAX_RAW).await?;
                            let data: Value = serde_json::from_str(&raw)?;
                            let items: Vec<(String, String, String)> = data["results"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .take(limit)
                                .map(|r| {
                                    (
                                        r["title"].as_str().unwrap_or("?").to_string(),
                                        r["url"].as_str().unwrap_or("").to_string(),
                                        r["content"].as_str().unwrap_or("").to_string(),
                                    )
                                })
                                .collect();
                            return Ok(format_search_results("SearXNG", query, &items));
                        }
                    }
                } else if backend == "tavily" {
                    let env_k = ctx.web_cfg.api_key_env.as_deref().unwrap_or("TAVILY_API_KEY");
                    if let Ok(key) = std::env::var(env_k) {
                        if !key.trim().is_empty() {
                            let resp = client
                                .post("https://api.tavily.com/search")
                                .header("user-agent", USER_AGENT)
                                .json(&json!({
                                    "api_key": key.trim(),
                                    "query": query,
                                    "max_results": limit
                                }))
                                .send()
                                .await
                                .map_err(|e| anyhow!("Tavily gagal: {e}"))?;
                            if resp.status().is_success() {
                                let raw = read_response_text_bounded(resp, MAX_RAW).await?;
                                let data: Value = serde_json::from_str(&raw)?;
                                let items: Vec<(String, String, String)> = data["results"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .take(limit)
                                    .map(|r| {
                                        (
                                            r["title"].as_str().unwrap_or("?").to_string(),
                                            r["url"].as_str().unwrap_or("").to_string(),
                                            r["content"].as_str().unwrap_or("").to_string(),
                                        )
                                    })
                                    .collect();
                                return Ok(format_search_results("Tavily", query, &items));
                            }
                        }
                    }
                } else if backend == "brave" {
                    let env_k = ctx.web_cfg.api_key_env.as_deref().unwrap_or("BRAVE_API_KEY");
                    if let Ok(key) = std::env::var(env_k) {
                        if !key.trim().is_empty() {
                            let resp = client
                                .get(format!(
                                    "https://api.search.brave.com/res/v1/web/search?q={enc_q}&count={limit}"
                                ))
                                .header("user-agent", USER_AGENT)
                                .header("X-Subscription-Token", key.trim())
                                .send()
                                .await
                                .map_err(|e| anyhow!("Brave Search gagal: {e}"))?;
                            if resp.status().is_success() {
                                let raw = read_response_text_bounded(resp, MAX_RAW).await?;
                                let data: Value = serde_json::from_str(&raw)?;
                                let items: Vec<(String, String, String)> = data["web"]["results"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .take(limit)
                                    .map(|r| {
                                        (
                                            r["title"].as_str().unwrap_or("?").to_string(),
                                            r["url"].as_str().unwrap_or("").to_string(),
                                            r["description"].as_str().unwrap_or("").to_string(),
                                        )
                                    })
                                    .collect();
                                return Ok(format_search_results("Brave", query, &items));
                            }
                        }
                    }
                } else if backend == "firecrawl" {
                    let env_k = ctx
                        .web_cfg
                        .api_key_env
                        .as_deref()
                        .unwrap_or("FIRECRAWL_API_KEY");
                    if let Ok(key) = std::env::var(env_k) {
                        if !key.trim().is_empty() {
                            let resp = client
                                .post("https://api.firecrawl.dev/v1/search")
                                .header("user-agent", USER_AGENT)
                                .bearer_auth(key.trim())
                                .json(&json!({ "query": query, "limit": limit }))
                                .send()
                                .await
                                .map_err(|e| anyhow!("Firecrawl Search gagal: {e}"))?;
                            if resp.status().is_success() {
                                let raw = read_response_text_bounded(resp, MAX_RAW).await?;
                                let data: Value = serde_json::from_str(&raw)?;
                                let items: Vec<(String, String, String)> = data["data"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .take(limit)
                                    .map(|r| {
                                        (
                                            r["title"].as_str().unwrap_or("?").to_string(),
                                            r["url"].as_str().unwrap_or("").to_string(),
                                            r["description"].as_str().unwrap_or("").to_string(),
                                        )
                                    })
                                    .collect();
                                return Ok(format_search_results("Firecrawl", query, &items));
                            }
                        }
                    }
                }

                // Default 0-API-key: DuckDuckGo HTML lightweight endpoint.
                let ddg_url = check_url_allowed(&format!(
                    "https://html.duckduckgo.com/html/?q={enc_q}"
                ))
                .await?;
                let (_, body) = fetch_checked_http(&client, ddg_url, Some("text/html")).await?;
                let items = parse_ddg_html_results(&body, limit);
                Ok(format_search_results("DuckDuckGo", query, &items))
            }
        }
    }
}
