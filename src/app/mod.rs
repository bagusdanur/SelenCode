pub mod cli;
pub mod commands;
pub mod plain_runner;
pub mod session;
pub mod telegram_runner;
pub mod tui_runner;

use anyhow::Result;
use selencode::config::{Config, ProviderCfg};
use selencode::providers::anthropic::AnthropicProvider;
use selencode::providers::gemini::GeminiProvider;
use selencode::providers::openai::OpenAiProvider;
use selencode::providers::router::{DynamicRouterProvider, ModelRoute, RouterTelemetry};
use selencode::providers::Provider;
use std::sync::Arc;
/// Label tampil provider untuk sambutan: nama + host base_url bila ada,
/// mis. "router (router.ryukomik.com)".
pub(crate) fn provider_label(name: &str, pcfg: &ProviderCfg) -> String {
    match &pcfg.base_url {
        Some(u) => {
            let rest = u
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            let host = rest.split('/').next().unwrap_or(rest);
            if host.is_empty() {
                name.to_string()
            } else {
                format!("{name} ({host})")
            }
        }
        None => name.to_string(),
    }
}

/// Buat instance provider dari config sesuai kind:
/// openai (base_url wajib) | anthropic | gemini (base_url default resmi).
pub(crate) fn make_provider(
    cfg: &Config,
    name: &str,
    pcfg: &ProviderCfg,
    api_key: &str,
) -> Result<Arc<dyn Provider>> {
    let max_tokens = cfg.agent.max_output_tokens;
    match pcfg.kind.as_str() {
        "openai" => {
            let base_url = pcfg
                .base_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("base_url wajib untuk provider '{name}' kind=openai"))?;
            Ok(Arc::new(OpenAiProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        "anthropic" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::anthropic::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(AnthropicProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        "gemini" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::gemini::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(GeminiProvider::new(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
            )))
        }
        other => anyhow::bail!(
            "provider kind '{other}' tidak dikenal (pilihan: openai, anthropic, gemini)"
        ),
    }
}

/// Rakit `DynamicRouterProvider` vendor-independen (primary + fallback + task routes + telemetri).
pub(crate) fn make_routed_provider(
    cfg: &Config,
    primary_name: &str,
    primary_cfg: &ProviderCfg,
    primary_key: &str,
    telemetry: Arc<std::sync::Mutex<RouterTelemetry>>,
) -> Result<Arc<dyn Provider>> {
    let mut routes: Vec<ModelRoute> = Vec::new();
    let primary_prov = make_provider(cfg, primary_name, primary_cfg, primary_key)?;
    routes.push(ModelRoute::new(
        primary_name.to_string(),
        primary_cfg.model.clone(),
        primary_prov,
        primary_cfg.context_limit,
        primary_cfg.price_in,
        primary_cfg.price_out,
        true,
    ));

    let push_spec = |routes: &mut Vec<ModelRoute>, spec: &str| {
        let spec = spec.trim();
        if spec.is_empty() {
            return;
        }
        let (pname, model_override) = match spec.split_once(':') {
            Some((p, m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
                (p.trim(), Some(m.trim()))
            }
            _ => (spec, None),
        };
        let Ok((resolved_name, mut pcfg, key)) = cfg.resolve_provider(Some(pname)) else {
            return;
        };
        if let Some(m) = model_override {
            pcfg.model = m.to_string();
        }
        let id = format!("{resolved_name}:{}", pcfg.model);
        if routes.iter().any(|r| r.id == id) {
            return;
        }
        if let Ok(prov) = make_provider(cfg, &resolved_name, &pcfg, &key) {
            routes.push(ModelRoute::new(
                resolved_name,
                pcfg.model,
                prov,
                pcfg.context_limit,
                pcfg.price_in,
                pcfg.price_out,
                false,
            ));
        }
    };

    for fb in &cfg.routing.fallbacks {
        push_spec(&mut routes, fb);
    }
    for target in cfg.routing.task_routes.values() {
        push_spec(&mut routes, target);
    }
    if cfg.routing.auto {
        // Tambahkan model tersimpan lain di provider aktif agar auto-routing bisa membedakan
        // model cepat/hemat untuk ringkasan vs model kuat untuk coding/debugging.
        for m in primary_cfg.effective_models() {
            if m != primary_cfg.model {
                push_spec(&mut routes, &format!("{primary_name}:{m}"));
            }
        }
        let mut other_provs: Vec<&String> = cfg.providers.keys().collect();
        other_provs.sort();
        for pname in other_provs {
            if pname != primary_name {
                push_spec(&mut routes, pname);
            }
        }
    }

    Ok(Arc::new(DynamicRouterProvider::new(
        routes,
        cfg.routing.auto,
        cfg.routing.task_routes.clone(),
        cfg.routing.max_retries,
        telemetry,
    )))
}

