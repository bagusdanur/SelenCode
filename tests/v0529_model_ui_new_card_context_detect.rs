//! Tes integrasi v0.5.29:
//! 1. Deteksi kapasitas context window model AI lintas keluarga + tag ukuran berbasis batas kata
//!    + format eksak dengan koma (`format_context_tokens_exact`).
//! 2. Parser metadata `/models` multi-provider (OpenAI-compatible vLLM/LiteLLM/OpenRouter,
//!    Gemini native `/v1beta/models`, Anthropic `/v1/models`).
//! 3. Persistensi cache context window di SQLite `Store` (`upsert_model_context_cache` / `load_model_context_cache`).
//! 4. Kartu pemilih inline `/model` & `/provider` Telegram, parser callback `mdl:...`,
//!    konfirmasi ganti model ringkas, dan kartu `/new` dengan info model + konteks.
//! 5. Pembaruan tampilan terminal TUI (`App::model_contexts`, `UiEvent::ModelContexts`,
//!    `WelcomeInfo::with_context_limit`, `plain_banner_with_context`).

use selencode::agent::context::{
    format_context_compact, format_context_tokens_exact, format_tokens_with_commas,
    infer_model_context_limit,
};
use selencode::agent::ui::UiEvent;
use selencode::providers::{anthropic, gemini, openai};
use selencode::store::Store;
use selencode::telegram::{
    build_tg_model_picker_card, build_tg_provider_picker_card, format_tg_model_switched_html,
    format_tg_new_session_html, parse_callback_model, TgModelCallback, TgModelEntry,
    TgModelProviderItem,
};
use selencode::tui::app::App;
use selencode::tui::moonlit::{
    plain_banner_with_context, resume_lines, welcome_lines, WelcomeInfo,
};
use serde_json::json;
use std::collections::HashMap;
use tempfile::tempdir;

#[test]
fn deteksi_context_window_lintas_keluarga_dan_anti_false_positive() {
    // Gemini keluarga 1M & 1.5 Pro 2M
    assert_eq!(infer_model_context_limit("ag/gemini-4-hydrogen-medium"), 1_048_576);
    assert_eq!(infer_model_context_limit("ag/gemini-3-flash"), 1_048_576);
    assert_eq!(infer_model_context_limit("google/gemini-1.5-pro"), 2_000_000);

    // Claude default 200k vs tag eksplisit `-1m`
    assert_eq!(infer_model_context_limit("anthropic/claude-sonnet-4-6"), 200_000);
    assert_eq!(infer_model_context_limit("anthropic/claude-sonnet-4-1m"), 1_000_000);

    // Anti false-positive: `v1mini` mengandung substring `1m` tetapi BUKAN tag ukuran 1M!
    assert_eq!(infer_model_context_limit("custom/gpt-4o-v1mini"), 128_000);
    assert_eq!(infer_model_context_limit("custom/model-r1medium"), 128_000);

    // Model modern lintas ekosistem
    assert_eq!(infer_model_context_limit("openai/gpt-4.1"), 1_047_576);
    assert_eq!(infer_model_context_limit("openai/gpt-5"), 400_000);
    assert_eq!(infer_model_context_limit("openai/gpt-oss-120b"), 131_072);
    assert_eq!(infer_model_context_limit("moonshot/kimi-k2.5"), 262_144);
    assert_eq!(infer_model_context_limit("qwen/qwen3-coder-plus"), 262_144);
    assert_eq!(infer_model_context_limit("zai/glm-5"), 200_000);
    assert_eq!(infer_model_context_limit("minimax/minimax-m2"), 204_800);
    assert_eq!(infer_model_context_limit("mistral/devstral-small"), 256_000);
    assert_eq!(infer_model_context_limit("xiaomi/mimo-v2"), 131_072);

    // Format ribuan & eksak
    assert_eq!(format_tokens_with_commas(1_048_576), "1,048,576");
    assert_eq!(format_tokens_with_commas(200_000), "200,000");
    assert_eq!(
        format_context_tokens_exact(1_048_576),
        "1,048,576 token (1M)"
    );
    assert_eq!(
        format_context_tokens_exact(200_000),
        "200,000 token (200k)"
    );
    assert_eq!(format_context_compact(262_144), "262k");
}

#[test]
fn parser_models_multi_provider_openai_gemini_anthropic() {
    // OpenAI-compatible dengan berbagai skema field context window (OpenRouter, vLLM, LiteLLM, camelCase)
    let openai_json = json!({
        "data": [
            { "id": "router/model-vllm", "max_model_len": 131072 },
            { "id": "router/model-litellm", "limits": { "context": 262144 } },
            { "id": "router/model-camel", "inputTokenLimit": 1048576 },
            { "id": "router/model-openrouter", "top_provider": { "context_length": 200000 } },
            { "id": "router/model-no-ctx" }
        ]
    });
    let parsed_oai: HashMap<String, Option<usize>> = openai::parse_openai_models_response(&openai_json)
        .into_iter()
        .collect();
    assert_eq!(parsed_oai.get("router/model-vllm"), Some(&Some(131_072)));
    assert_eq!(parsed_oai.get("router/model-litellm"), Some(&Some(262_144)));
    assert_eq!(parsed_oai.get("router/model-camel"), Some(&Some(1_048_576)));
    assert_eq!(parsed_oai.get("router/model-openrouter"), Some(&Some(200_000)));
    assert_eq!(parsed_oai.get("router/model-no-ctx"), Some(&None));

    // Gemini native `/v1beta/models` (membuang prefix `models/` & menyaring model non-generateContent)
    let gemini_json = json!({
        "models": [
            {
                "name": "models/gemini-2.5-pro",
                "inputTokenLimit": 1048576,
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            },
            {
                "name": "models/text-embedding-004",
                "inputTokenLimit": 2048,
                "supportedGenerationMethods": ["embedContent"]
            }
        ]
    });
    let parsed_gem = gemini::parse_gemini_models_response(&gemini_json);
    assert_eq!(parsed_gem.len(), 1);
    assert_eq!(parsed_gem[0], ("gemini-2.5-pro".to_string(), Some(1_048_576)));

    // Anthropic `/v1/models`
    let anthropic_json = json!({
        "data": [
            { "id": "claude-sonnet-4-6", "context_window": 200000 },
            { "id": "claude-haiku-4-5" }
        ]
    });
    let parsed_ant: HashMap<String, Option<usize>> =
        anthropic::parse_anthropic_models_response(&anthropic_json)
            .into_iter()
            .collect();
    assert_eq!(parsed_ant.get("claude-sonnet-4-6"), Some(&Some(200_000)));
    assert_eq!(parsed_ant.get("claude-haiku-4-5"), Some(&None));
}

#[test]
fn cache_context_window_persisten_lintas_restart_sqlite() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("selencode.db");
    {
        let st = Store::open_at(&db).expect("open store");
        let merged = st
            .upsert_model_context_cache(&[
                ("custom/private-llm".to_string(), 320_000),
                ("ag/gemini-4-hydrogen-medium".to_string(), 1_048_576),
                ("invalid-small".to_string(), 100), // < 1024 harus diabaikan
            ])
            .expect("upsert cache");
        assert_eq!(merged.get("custom/private-llm"), Some(&320_000));
        assert_eq!(merged.get("invalid-small"), None);
    }
    // Buka ulang DB (simulasi restart TUI/CLI/Telegram) — cache harus langsung terbaca!
    let st2 = Store::open_at(&db).expect("reopen store");
    let loaded = st2.load_model_context_cache();
    assert_eq!(loaded.get("custom/private-llm"), Some(&320_000));
    assert_eq!(loaded.get("ag/gemini-4-hydrogen-medium"), Some(&1_048_576));
}

#[test]
fn telegram_model_picker_callback_dan_kartu_new_bersih() {
    // 1. Parser callback `mdl:...`
    assert_eq!(parse_callback_model("mdl:p:0"), Some(TgModelCallback::ProviderPage(0)));
    assert_eq!(
        parse_callback_model("mdl:m:2:1"),
        Some(TgModelCallback::ModelPage {
            provider_idx: 2,
            page: 1
        })
    );
    assert_eq!(
        parse_callback_model("mdl:s:0:4"),
        Some(TgModelCallback::SelectModel {
            provider_idx: 0,
            model_idx: 4
        })
    );
    assert_eq!(parse_callback_model("mdl:x"), Some(TgModelCallback::Close));
    assert_eq!(parse_callback_model("mdl:noop"), Some(TgModelCallback::Noop));
    assert_eq!(parse_callback_model("ap:1:y"), None);
    assert_eq!(parse_callback_model("mdl:s:0:1:extra"), None);

    // 2. Kartu Model Picker dengan 10 model (paginasi 2 halaman, 1 kolom untuk ID panjang)
    let models: Vec<TgModelEntry> = (0..10)
        .map(|i| TgModelEntry {
            id: format!("ag/gemini-4-hydrogen-variant-{i}"),
            context_limit: 1_048_576,
            is_active: i == 1,
        })
        .collect();
    let card = build_tg_model_picker_card(
        "router",
        "openai",
        "ag/gemini-4-hydrogen-variant-1",
        1_048_576,
        0,
        "router",
        "openai",
        2,
        &models,
        0,
    );
    assert!(card.html.contains("Konfigurasi Model"));
    assert!(card.html.contains("1,048,576 token (1M)"));
    assert!(card.html.contains("ag/gemini-4-hydrogen-variant-1"));
    // Pastikan tombol aktif ditandai `✓` dan semua callback_data <= 64 byte
    let flat_btns: Vec<&(String, String)> = card.rows.iter().flat_map(|r| r.iter()).collect();
    assert!(
        flat_btns
            .iter()
            .any(|(lbl, cb)| lbl.starts_with('✓') && cb == "mdl:s:0:1"),
        "tombol model aktif harus bertanda ✓: {flat_btns:?}"
    );
    assert!(flat_btns.iter().any(|(_, cb)| cb == "mdl:m:0:1"), "ada tombol Next ▶");
    assert!(flat_btns.iter().any(|(_, cb)| cb == "mdl:p:0"), "ada tombol pindah provider");
    assert!(flat_btns.iter().any(|(_, cb)| cb == "mdl:x"), "ada tombol Tutup");
    for (_, cb) in &flat_btns {
        assert!(cb.len() <= 64, "callback_data terlalu panjang: {cb}");
    }

    // 3. Kartu Provider Picker 2-kolom dengan 12 provider (paginasi 10/halaman -> 2 halaman)
    let providers: Vec<TgModelProviderItem> = (0..12)
        .map(|i| TgModelProviderItem {
            name: format!("prov-{i}"),
            kind: "openai".to_string(),
            active_model: "gpt-4.1".to_string(),
            model_count: i + 1,
            is_active: i == 0,
            key_ready: true,
        })
        .collect();
    let pcard = build_tg_provider_picker_card(
        "prov-0",
        "openai",
        "gpt-4.1",
        1_047_576,
        &providers,
        0,
    );
    assert!(pcard.html.contains("Pilih provider (1–10 dari 12)"));
    assert_eq!(pcard.rows[0].len(), 2, "provider picker harus 2 kolom");
    assert!(pcard.rows[0][0].0.starts_with('✓'), "provider aktif ditandai ✓");
    for row in &pcard.rows {
        for (_, cb) in row {
            assert!(cb.len() <= 64);
        }
    }

    // 4. Konfirmasi ganti model & kartu /new (termasuk HTML escaping aman)
    let (sw_html, sw_plain) = format_tg_model_switched_html(
        "router",
        "openai",
        "ag/gemini-4-neon-medium",
        1_048_576,
    );
    assert!(sw_html.contains("ag/gemini-4-neon-medium"));
    assert!(sw_html.contains("1,048,576 token (1M)"));
    assert!(sw_plain.contains("Provider: router (openai)"));

    let (new_html, new_plain) = format_tg_new_session_html(
        "db2b54ae-f123-4567",
        "router",
        "openai",
        "ag/gemini-4-hydrogen-medium",
        1_048_576,
        "C:\\Users\\demo\\<proj>",
    );
    assert!(new_html.contains("db2b54ae-f"));
    assert!(new_html.contains("ag/gemini-4-hydrogen-medium"));
    assert!(new_html.contains("1,048,576 token (1M)"));
    assert!(new_html.contains("&lt;proj&gt;"), "HTML karakter khusus harus di-escape");
    assert!(new_plain.contains("Model: ag/gemini-4-hydrogen-medium"));
}

#[test]
fn terminal_tui_dan_plain_menampilkan_context_window_dan_menjaga_cache_saat_new() {
    let mut app = App::new(
        "/tmp/demo".into(),
        "router".into(),
        "custom/private-llm".into(),
        0,
        0,
        0,
    );
    // Awalnya heuristik default 128k
    assert_eq!(app.context_limit, 128_000);

    // Terima pembaruan cache dari API `/models`
    let mut cache = HashMap::new();
    cache.insert("custom/private-llm".to_string(), 320_000);
    app.apply_event(UiEvent::ModelContexts(cache));
    assert_eq!(app.context_limit, 320_000);
    assert_eq!(app.context_limit_for_model("custom/private-llm"), 320_000);

    // Saat /new (SessionReset), TUI harus tetap mempertahankan limit API 320k, bukan jatuh balik ke 128k!
    app.apply_event(UiEvent::SessionReset {
        session_id: "sess-baru-9999".into(),
        provider: "router".into(),
        model: "custom/private-llm".into(),
    });
    assert_eq!(app.context_limit, 320_000);

    // Layar sambutan & resume & banner polos menampilkan info context window
    let info = WelcomeInfo::new(
        "router".into(),
        "router".into(),
        "custom/private-llm".into(),
        "/tmp/demo".into(),
        10,
    )
    .with_context_limit(320_000);
    let wide_txt: String = welcome_lines(116, &info)
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(wide_txt.contains("ctx 320k"), "welcome wide harus tampilkan ctx: {wide_txt}");

    let res_txt: String = resume_lines(&info)
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(res_txt.contains("ctx 320k"), "resume line harus tampilkan ctx: {res_txt}");

    let banner = plain_banner_with_context("router", "custom/private-llm", 320_000);
    assert!(banner.contains("320,000 token (320k)"));
}
