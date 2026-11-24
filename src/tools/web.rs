use super::{schema, str_arg, truncate_output, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const MAX_TEXT: usize = 100 * 1024;
const MAX_RAW: usize = 512 * 1024;
const TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;

/// Apakah IP ini tujuan terlarang (SSRF): privat, loopback, link-local,
/// unspecified, multicast, broadcast, CGNAT (100.64/10).
pub fn is_forbidden_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])) // CGNAT
                || *v4 == Ipv4Addr::new(192, 0, 0, 0)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || *v6 == Ipv6Addr::LOCALHOST
        }
    }
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
    // IP literal: cek langsung, tanpa DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(&ip) {
            return Err(anyhow!(
                "host {host} ditolak: IP privat/loopback/link-local tidak boleh di-fetch (SSRF)"
            ));
        }
        return Ok(url);
    }
    // Nama host: resolusi DNS, cek semua alamat hasil.
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
}

/// Ekstrak teks kasar dari HTML: buang script/style/komentar, buang tag,
/// rapikan entity dasar & whitespace. Tanpa dependensi parser HTML.
pub fn html_to_text(html: &str) -> String {
    use std::sync::OnceLock;
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
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    re_ws.replace_all(&s, " ").trim().to_string()
}

// ---------- fetch_url ----------
pub struct FetchUrl;

#[async_trait]
impl Tool for FetchUrl {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fetch_url".into(),
            description: "Ambil isi teks sebuah URL http/https (HTML diubah jadi teks). \
IP privat/loopback ditolak (anti-SSRF), hasil maks 100KB. Butuh persetujuan user."
                .into(),
            parameters: schema(
                json!({"url": {"type": "string", "description": "URL http/https lengkap"}}),
                &["url"],
            ),
        }
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        // Blueprint: approval sekali per domain per sesi. Penyederhanaan Fase 3:
        // approval standar per panggilan (lihat HASIL-FASE-3.md).
        true
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let raw = str_arg(&args, "url").ok_or_else(|| anyhow!("argumen 'url' wajib"))?;
        // Validasi SSRF SEBELUM approval & sebelum jaringan disentuh.
        let mut url = check_url_allowed(raw).await?;
        if !ctx.approver
            .ask(
                &format!("fetch_url: {}", url.host_str().unwrap_or("?")),
                &url.to_string(),
            )
            .await
        {
            return Ok("Ditolak user — URL tidak di-fetch.".into());
        }

        // Redirect manual: client tanpa auto-redirect, tiap hop divalidasi ulang.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()?;
        let mut hops = 0usize;
        let (content_type, body) = loop {
            let resp = client
                .get(url.clone())
                .header("user-agent", "SelenCode/0.1 (fetch_url)")
                .send()
                .await
                .map_err(|e| anyhow!("fetch gagal: {e}"))?;
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
            // Baca body dengan batas keras agar tidak menelan file raksasa.
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
            break (ct, String::from_utf8_lossy(&buf).into_owned());
        };

        let text = if content_type.to_lowercase().contains("html") || body.trim_start().starts_with('<') {
            html_to_text(&body)
        } else {
            body
        };
        Ok(truncate_output(text, MAX_TEXT))
    }
}
