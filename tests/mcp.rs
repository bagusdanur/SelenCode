//! Tes Fase 7 — klien MCP stdio melawan server bohongan Python
//! (tests/fixtures/mcp_echo_server.py). Semua lokal, tanpa jaringan,
//! tanpa API key. Tes yang butuh python3 dilewati (dengan pesan) bila
//! python3 tidak tersedia di mesin.

use selencode::config::McpServerCfg;
use selencode::mcp::client::{decode_message, encode_message, McpClient};
use selencode::mcp::{connect_server, exposed_name, McpTool};
use selencode::tools::{Ctx, Tool};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/mcp_echo_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn echo_cfg(trust: bool) -> McpServerCfg {
    McpServerCfg {
        name: "tes".to_string(),
        command: "python3".to_string(),
        args: vec![fixture_path()],
        env: HashMap::new(),
        trust,
        timeout_secs: 10,
    }
}

#[test]
fn framing_ndjson_roundtrip() {
    let msg = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}});
    let line = encode_message(&msg);
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1, "NDJSON = tepat 1 baris");
    assert_eq!(decode_message(&line).unwrap(), msg);
    assert!(decode_message("").is_err());
    assert!(decode_message("{rusak").is_err());
}

#[tokio::test]
async fn mcp_echo_server_end_to_end() {
    if !python3_available() {
        eprintln!("python3 tidak ada — tes MCP end-to-end dilewati");
        return;
    }
    let cfg = echo_cfg(false);
    let (client, infos) = McpClient::start(&cfg)
        .await
        .expect("handshake ke server bohongan harus berhasil");
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "echo");

    // Nama terekspos mengikuti pola mcp__<server>__<tool>.
    assert_eq!(exposed_name("tes", "echo"), "mcp__tes__echo");
    let tool = McpTool::new("tes", &infos[0], client.clone(), false);
    assert_eq!(tool.spec().name, "mcp__tes__echo");
    // Server tidak di-trust -> approval wajib, dan penolakan membatalkan panggilan.
    assert!(tool.needs_approval(&json!({"text": "x"})));
    let (atx, mut arx) = tokio::sync::mpsc::channel(4);
    let deny_ctx = Ctx::new(PathBuf::from("."), false)
        .with_approver(std::sync::Arc::new(selencode::security::Approver::channel(false, atx)));
    let deny_handle = tokio::spawn(async move {
        if let Some(req) = arx.recv().await {
            let _ = req.reply.send(false);
        }
    });
    let denied = tool
        .run(json!({"text": "jangan"}), &deny_ctx)
        .await
        .expect("penolakan approval mengembalikan pesan, bukan Err");
    deny_handle.await.unwrap();
    assert!(denied.contains("Ditolak"), "{denied}");

    let ctx = Ctx::new(PathBuf::from("."), true);
    let out = tool
        .run(json!({"text": "halo"}), &ctx)
        .await
        .expect("tools/call echo harus berhasil");
    assert_eq!(out, "halo");

    // Lewat jalur connect_server (yang dipakai Session::build).
    let tools = connect_server(&echo_cfg(true)).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec().name, "mcp__tes__echo");
    // trust = true -> tanpa approval.
    assert!(!tools[0].needs_approval(&json!({"text": "x"})));
    let out = tools[0].run(json!({"text": "lewat-connect"}), &ctx).await.unwrap();
    assert_eq!(out, "lewat-connect");
}

#[tokio::test]
async fn mcp_dead_server_fails_gracefully() {
    // `false` langsung keluar: handshake harus gagal sebagai Err, bukan panik.
    let cfg = McpServerCfg {
        name: "mati".to_string(),
        command: "false".to_string(),
        args: vec![],
        env: HashMap::new(),
        trust: false,
        timeout_secs: 5,
    };
    let r = McpClient::start(&cfg).await;
    assert!(r.is_err(), "server yang langsung mati harus menghasilkan Err");

    // Command yang tidak ada sama sekali juga Err anggun.
    let cfg2 = McpServerCfg {
        command: "selen-perintah-yang-tidak-ada-xyz".to_string(),
        ..cfg
    };
    assert!(McpClient::start(&cfg2).await.is_err());
}
