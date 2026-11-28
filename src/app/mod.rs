pub mod bench_runner;
pub mod cli;
pub mod commands;
pub mod plain_runner;
pub mod session;
pub mod telegram_runner;
pub mod telegram_ui;
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
    let timeouts = cfg.timeouts.to_durations();
    match pcfg.kind.as_str() {
        "openai" => {
            let base_url = pcfg
                .base_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("base_url wajib untuk provider '{name}' kind=openai"))?;
            Ok(Arc::new(OpenAiProvider::with_timeouts(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
                timeouts,
            )))
        }
        "anthropic" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::anthropic::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(AnthropicProvider::with_timeouts(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
                timeouts,
            )))
        }
        "gemini" => {
            let base_url = pcfg
                .base_url
                .clone()
                .unwrap_or_else(|| selencode::providers::gemini::DEFAULT_BASE_URL.to_string());
            Ok(Arc::new(GeminiProvider::with_timeouts(
                base_url,
                api_key.to_string(),
                pcfg.model.clone(),
                max_tokens,
                timeouts,
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

    let push_fallback_spec = |routes: &mut Vec<ModelRoute>, spec: &str| {
        let spec = spec.trim();
        if spec.is_empty() {
            return;
        }
        // Dukung baik `"provider:model"` maupun `"model-id"` (pada provider aktif).
        let (pname, model_override) = match spec.split_once(':') {
            Some((p, m))
                if !p.trim().is_empty()
                    && !m.trim().is_empty()
                    && cfg.providers.contains_key(p.trim()) =>
            {
                (p.trim(), Some(m.trim()))
            }
            _ if cfg.providers.contains_key(spec) => (spec, None),
            _ => (primary_name, Some(spec)),
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

    // BAGIAN 1 (v0.5.6): Prioritas daftar fallback terurut pada provider aktif, lalu `[fallback]` global, lalu `[routing].fallbacks`.
    let has_explicit_fallback =
        !primary_cfg.fallback.is_empty() || !cfg.fallback.models.is_empty();
    for fb in &primary_cfg.fallback {
        push_fallback_spec(&mut routes, fb);
    }
    for fb in &cfg.fallback.models {
        push_fallback_spec(&mut routes, fb);
    }
    for fb in &cfg.routing.fallbacks {
        push_fallback_spec(&mut routes, fb);
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

    let mut router = DynamicRouterProvider::new(
        routes,
        cfg.routing.auto,
        cfg.routing.task_routes.clone(),
        cfg.routing.max_retries,
        telemetry,
    )
    .with_max_fallback_models(cfg.fallback.max_attempts)
    .with_non_sticky_manual(has_explicit_fallback)
    .with_max_cost_usd(cfg.routing.max_cost_usd);
    if let Ok(store) = selencode::store::Store::open() {
        router = router.with_store(Arc::new(store));
    }
    Ok(Arc::new(router))
}

/// Bangun provider untuk Selena Improvement (v0.5.24):
/// - Bila `[aux] review_model` atau `[aux] compression_model` diisi -> pakai model aux override tersebut.
/// - Bila keduanya kosong -> pakai provider & model utama sesi itu sendiri (seperti fork latar pada
///   model utama agar awalan prompt beku mengenai cache awalan provider, tetap dalam pagu anggaran).
pub(crate) fn make_review_provider(
    cfg: &Config,
    primary_name: &str,
    primary_cfg: &ProviderCfg,
    primary_provider: Arc<dyn Provider>,
) -> Option<Arc<dyn Provider>> {
    if let Some(aux) = make_aux_review_provider(cfg, primary_name, primary_cfg) {
        return Some(aux);
    }
    if primary_cfg.model.trim().is_empty() || primary_cfg.model == "(belum diatur)" {
        return None;
    }
    // Bangun handle provider baru pada model utama dengan batas output konservatif bila tersedia,
    // atau gunakan `primary_provider` sesi langsung.
    if let Ok((n, p, k)) = cfg.resolve_provider(Some(primary_name)) {
        let mut rev_cfg = cfg.clone();
        rev_cfg.agent.max_output_tokens = (cfg.review.max_tokens.min(1200).max(256)) as u32;
        if let Ok(prov) = make_provider(&rev_cfg, &n, &p, &k) {
            return Some(prov);
        }
    }
    Some(primary_provider)
}

/// Bangun provider khusus Selena Improvement memakai `[aux] review_model`
/// (atau fallback `[aux] compression_model`). Mengembalikan `None` bila keduanya kosong.
pub(crate) fn make_aux_review_provider(
    cfg: &Config,
    primary_name: &str,
    primary_cfg: &ProviderCfg,
) -> Option<Arc<dyn Provider>> {
    let spec = cfg.aux.effective_review_model()?.trim();
    if spec.is_empty() {
        return None;
    }
    let (target_prov, mut target_cfg, key) = match spec.split_once(':') {
        Some((pname, model_id))
            if !pname.trim().is_empty()
                && !model_id.trim().is_empty()
                && cfg.providers.contains_key(pname.trim()) =>
        {
            let Ok((n, mut p, k)) = cfg.resolve_provider(Some(pname.trim())) else {
                return None;
            };
            p.model = model_id.trim().to_string();
            (n, p, k)
        }
        _ => {
            let Ok((n, mut p, k)) = cfg.resolve_provider(Some(primary_name)) else {
                let Ok((n2, mut p2, k2)) = cfg.resolve_provider(None) else {
                    return None;
                };
                p2.model = spec.to_string();
                let mut rev_cfg = cfg.clone();
                rev_cfg.agent.max_output_tokens =
                    (cfg.review.max_tokens.min(1200).max(256)) as u32;
                return make_provider(&rev_cfg, &n2, &p2, &k2).ok();
            };
            p.base_url = primary_cfg.base_url.clone().or(p.base_url);
            p.model = spec.to_string();
            (n, p, k)
        }
    };
    let mut rev_cfg = cfg.clone();
    rev_cfg.agent.max_output_tokens = (cfg.review.max_tokens.min(1200).max(256)) as u32;
    if target_cfg.model.trim().is_empty() {
        target_cfg.model = spec.to_string();
    }
    make_provider(&rev_cfg, &target_prov, &target_cfg, &key).ok()
}

