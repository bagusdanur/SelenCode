//! Tool `ask_user` / clarify (v0.5.6 Bagian 2).
//!
//! Memungkinkan agent mengajukan pertanyaan terstruktur (0–4 pilihan berlabel pendek
//! atau pertanyaan terbuka) kepada pengguna di tengah tugas alih-alih menebak atau
//! berputar-putar melakukan pencarian berulang.
//!
//! Jalur ini 100% TERPISAH dari mekanisme persetujuan (`ApprovalRequest` / `Approver`):
//! jawaban pengguna di sini BUKAN approval untuk aksi berisiko.

use super::{schema, str_arg, Ctx, Tool};
use crate::providers::ToolSpec;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Permintaan klarifikasi yang dikirim dari tool `ask_user` ke antarmuka (TUI / Telegram).
#[derive(Debug)]
pub struct AskUserRequest {
    pub question: String,
    pub choices: Vec<String>,
    pub context: Option<String>,
    pub timeout_secs: u64,
    pub reply: oneshot::Sender<AskUserAnswer>,
}

/// Jawaban dari pengguna (atau penanda timeout / non-interaktif).
#[derive(Debug, Clone)]
pub struct AskUserAnswer {
    pub response: String,
    pub timed_out: bool,
    pub non_interactive: bool,
}

enum AskerBackend {
    /// Mode one-shot non-interaktif (`selen run` via pipe / tes): tidak pernah menjeda.
    NonInteractive,
    /// Mode CLI polos interaktif (TTY): tampilkan pilihan bernomor & baca stdin dengan timeout.
    Stdin,
    /// Mode TUI atau Telegram: kirim `AskUserRequest` ke kanal UI terpisah dari approval.
    Channel(mpsc::Sender<AskUserRequest>),
}

/// Pengelola permintaan klarifikasi pengguna (terpisah total dari `Approver`).
pub struct UserAsker {
    backend: AskerBackend,
}

impl UserAsker {
    pub fn non_interactive() -> Self {
        Self {
            backend: AskerBackend::NonInteractive,
        }
    }

    pub fn stdin() -> Self {
        Self {
            backend: AskerBackend::Stdin,
        }
    }

    pub fn channel(tx: mpsc::Sender<AskUserRequest>) -> Self {
        Self {
            backend: AskerBackend::Channel(tx),
        }
    }

    pub fn is_interactive(&self) -> bool {
        !matches!(self.backend, AskerBackend::NonInteractive)
    }

    pub async fn ask(
        &self,
        question: String,
        choices: Vec<String>,
        context: Option<String>,
        timeout_secs: u64,
    ) -> AskUserAnswer {
        let timeout = timeout_secs.max(1);
        match &self.backend {
            AskerBackend::NonInteractive => AskUserAnswer {
                response: String::new(),
                timed_out: false,
                non_interactive: true,
            },
            AskerBackend::Stdin => {
                let q = question.clone();
                let ch = choices.clone();
                let ctx_opt = context.clone();
                let read_handle = tokio::task::spawn_blocking(move || -> String {
                    eprintln!("\n[Pertanyaan dari Selena]");
                    if let Some(c) = ctx_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        eprintln!("  Konteks: {c}");
                    }
                    eprintln!("  {q}");
                    for (idx, opt) in ch.iter().enumerate() {
                        eprintln!("  {}. {opt}", idx + 1);
                    }
                    if ch.is_empty() {
                        eprint!("Jawabanmu: ");
                    } else {
                        eprint!("Pilih nomor (1-{}) atau ketik jawaban bebas: ", ch.len());
                    }
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                        return String::new();
                    }
                    let trimmed = line.trim();
                    if let Ok(num) = trimmed.parse::<usize>() {
                        if num >= 1 && num <= ch.len() {
                            return ch[num - 1].clone();
                        }
                    }
                    trimmed.to_string()
                });
                match tokio::time::timeout(Duration::from_secs(timeout), read_handle).await {
                    Ok(Ok(ans)) if !ans.trim().is_empty() => AskUserAnswer {
                        response: ans,
                        timed_out: false,
                        non_interactive: false,
                    },
                    Ok(Ok(_)) => AskUserAnswer {
                        response: String::new(),
                        timed_out: false,
                        non_interactive: true,
                    },
                    _ => AskUserAnswer {
                        response: String::new(),
                        timed_out: true,
                        non_interactive: false,
                    },
                }
            }
            AskerBackend::Channel(tx) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let req = AskUserRequest {
                    question,
                    choices,
                    context,
                    timeout_secs: timeout,
                    reply: reply_tx,
                };
                if tx.send(req).await.is_err() {
                    return AskUserAnswer {
                        response: String::new(),
                        timed_out: false,
                        non_interactive: true,
                    };
                }
                match tokio::time::timeout(Duration::from_secs(timeout), reply_rx).await {
                    Ok(Ok(ans)) => ans,
                    Ok(Err(_)) => AskUserAnswer {
                        response: String::new(),
                        timed_out: true,
                        non_interactive: false,
                    },
                    Err(_) => AskUserAnswer {
                        response: String::new(),
                        timed_out: true,
                        non_interactive: false,
                    },
                }
            }
        }
    }
}

/// Tolak pertanyaan yang mencoba meminta rahasia, kredensial, token, atau password pengguna.
fn asks_for_secret(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    [
        "api key",
        "apikey",
        "api_key",
        "password",
        "kata sandi",
        "secret key",
        "private key",
        "bot token",
        "access token",
        "bearer token",
        "otp",
        "pin rahasia",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ajukan pertanyaan klarifikasi singkat ke pengguna (opsional 0-4 pilihan pendek) saat menemukan kandidat mirip/ambigu atau >5 pencarian tanpa konfirmasi. BUKAN untuk meminta rahasia/password dan BUKAN persetujuan aksi berisiko.".into(),
            parameters: schema(
                json!({
                    "question": {
                        "type": "string",
                        "description": "Pertanyaan singkat & jelas untuk pengguna"
                    },
                    "choices": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 4,
                        "description": "Opsional: 0 sampai 4 pilihan berlabel pendek (kosong = pertanyaan terbuka)"
                    },
                    "context": {
                        "type": "string",
                        "description": "Opsional: ringkasan singkat kandidat atau alasan ambiguitas"
                    }
                }),
                &["question"],
            ),
        }
    }

    async fn run(&self, args: Value, ctx: &Ctx) -> Result<String> {
        let question = str_arg(&args, "question")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("argumen 'question' wajib diisi"))?;

        if asks_for_secret(question) {
            return Err(anyhow!(
                "ditolak: `ask_user` tidak boleh digunakan untuk meminta API key, password, token, atau rahasia lainnya."
            ));
        }

        let choices: Vec<String> = args
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .take(4)
                    .collect()
            })
            .unwrap_or_default();

        let context = str_arg(&args, "context")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let timeout_secs = ctx.ask_timeout_secs.max(1);
        let ans = ctx
            .asker
            .ask(question.to_string(), choices, context, timeout_secs)
            .await;

        if ans.non_interactive {
            return Ok(
                "[NON-INTERACTIVE] Tidak ada pengguna interaktif (mode pipe/one-shot). \
                 Lanjutkan dengan asumsi terbaikmu dan NYATAKAN asumsi tersebut secara eksplisit di jawaban akhirmu."
                    .to_string(),
            );
        }
        if ans.timed_out {
            let mins = timeout_secs.div_ceil(60);
            return Ok(format!(
                "[TIMEOUT] Tidak ada jawaban pengguna dalam batas waktu ({timeout_secs} dtk / ~{mins} menit). \
                 Lanjutkan dengan asumsi terbaikmu dan NYATAKAN secara eksplisit di jawabanmu: \
                 \"tidak ada jawaban dalam {mins} menit — kulanjutkan dengan asumsi ...\"."
            ));
        }

        let safe_resp = crate::security::redact_secrets(ans.response.trim());
        Ok(format!(
            "Jawaban pengguna: {safe_resp}\n\
             (Catatan: ini adalah klarifikasi niat pengguna, BUKAN persetujuan untuk aksi berisiko — aksi berisiko tetap melewati jalur approval seperti biasa.)"
        ))
    }
}
