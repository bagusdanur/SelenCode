//! Submodul UI interaktif Telegram untuk SelenCode (`v0.5.30`):
//! memisahkan seluruh builder kartu inline keyboard dan handler callback (`mdl:`, `res:`,
//! `cfg:`, `crn:`, `skl:`, `pln:`) agar `telegram_runner.rs` tetap ramping dan mudah dirawat.

use super::session::Session;
use anyhow::Result;
use selencode::agent::context::{format_context_tokens_exact, infer_model_context_limit};
use selencode::config;
use selencode::providers::{anthropic, gemini, openai};
use selencode::security::ApprovalMode;
use selencode::store::Store;
use selencode::telegram::{
    build_tg_approve_picker_card, build_tg_catalog_picker_card, build_tg_cron_picker_card,
    build_tg_model_picker_card, build_tg_provider_picker_card, build_tg_resume_picker_card,
    build_tg_sandbox_picker_card, build_tg_skills_picker_card, escape_telegram_html,
    format_tg_model_switched_html, format_tg_new_session_html, TelegramClient, TgCallbackQuery,
    TgConfigCallback, TgCronCallback, TgCronItem, TgInlineCard, TgModelCallback, TgModelEntry,
    TgModelProviderItem, TgResumeCallback, TgResumeItem, TgSkillCallback, TgSkillItem,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// State ringan bersama antar callback inline Telegram & task chat per-`chat_id`.
#[derive(Default)]
pub(crate) struct TgUiSharedState {
    /// Cache daftar ID model katalog API terakhir per `(chat_id, provider_idx)` dengan TTL 60 detik.
    remote_catalogs: Mutex<HashMap<(i64, usize), (Instant, Vec<String>)>>,
    /// Chat yang mengaktifkan `/plan` untuk giliran pesan berikutnya.
    plan_next_chats: Mutex<HashSet<i64>>,
    /// Teks rencana hasil giliran `/plan` yang menunggu tombol `pln:run` / `pln:x`.
    pending_plans: Mutex<HashMap<i64, String>>,
    /// Daftar nama skill terakhir per `chat_id` untuk tombol detail `skl:v:<idx>`.
    last_skills_list: Mutex<HashMap<i64, Vec<String>>>,
}

impl TgUiSharedState {
    pub(crate) fn set_remote_catalog(&self, chat_id: i64, prov_idx: usize, ids: Vec<String>) {
        if let Ok(mut g) = self.remote_catalogs.lock() {
            g.insert((chat_id, prov_idx), (Instant::now(), ids));
        }
    }

    pub(crate) fn get_remote_catalog(&self, chat_id: i64, prov_idx: usize) -> Option<Vec<String>> {
        self.remote_catalogs
            .lock()
            .ok()
            .and_then(|g| g.get(&(chat_id, prov_idx)).map(|(_, ids)| ids.clone()))
    }

    pub(crate) fn get_fresh_remote_catalog(
        &self,
        chat_id: i64,
        prov_idx: usize,
        ttl: Duration,
    ) -> Option<Vec<String>> {
        self.remote_catalogs.lock().ok().and_then(|g| {
            g.get(&(chat_id, prov_idx))
                .filter(|(ts, _)| ts.elapsed() <= ttl)
                .map(|(_, ids)| ids.clone())
        })
    }

    pub(crate) fn set_plan_next(&self, chat_id: i64, enabled: bool) {
        if let Ok(mut g) = self.plan_next_chats.lock() {
            if enabled {
                g.insert(chat_id);
            } else {
                g.remove(&chat_id);
            }
        }
    }

    pub(crate) fn take_plan_next(&self, chat_id: i64) -> bool {
        self.plan_next_chats
            .lock()
            .ok()
            .map(|mut g| g.remove(&chat_id))
            .unwrap_or(false)
    }

    pub(crate) fn set_pending_plan(&self, chat_id: i64, plan: String) {
        if let Ok(mut g) = self.pending_plans.lock() {
            g.insert(chat_id, plan);
        }
    }

    pub(crate) fn take_pending_plan(&self, chat_id: i64) -> Option<String> {
        self.pending_plans
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&chat_id))
    }

    pub(crate) fn set_skills_list(&self, chat_id: i64, names: Vec<String>) {
        if let Ok(mut g) = self.last_skills_list.lock() {
            g.insert(chat_id, names);
        }
    }

    pub(crate) fn get_skill_name(&self, chat_id: i64, idx: usize) -> Option<String> {
        self.last_skills_list
            .lock()
            .ok()
            .and_then(|g| g.get(&chat_id).and_then(|v| v.get(idx).cloned()))
    }

    /// Bersihkan seluruh cache UI idle milik `chat_id` saat sesi dikeluarkan dari RAM gateway.
    pub(crate) fn evict_chat(&self, chat_id: i64) {
        if let Ok(mut g) = self.remote_catalogs.lock() {
            g.retain(|&(cid, _), _| cid != chat_id);
        }
        if let Ok(mut g) = self.plan_next_chats.lock() {
            g.remove(&chat_id);
        }
        if let Ok(mut g) = self.pending_plans.lock() {
            g.remove(&chat_id);
        }
        if let Ok(mut g) = self.last_skills_list.lock() {
            g.remove(&chat_id);
        }
    }
}

/// Daftar nama provider terurut deterministik (dipakai konsisten antara render tombol & callback `mdl:`).
pub(crate) fn tg_sorted_providers(cfg: &config::Config) -> Vec<String> {
    let mut names: Vec<String> = cfg.providers.keys().cloned().collect();
    names.sort();
    names
}

/// Deteksi kapasitas context window satu model pada Telegram gateway (prioritas sama dengan `Session`).
pub(crate) fn tg_resolve_model_ctx(
    cfg: &config::Config,
    pcfg: &config::ProviderCfg,
    model_id: &str,
    cache: &HashMap<String, usize>,
) -> usize {
    if model_id == pcfg.model {
        if let Some(n) = pcfg.context_limit.filter(|&n| n > 0) {
            return n;
        }
    }
    if let Some(&n) = cache.get(model_id) {
        if n >= 1024 {
            return n;
        }
    }
    if cfg.agent.context_limit_tokens != 100_000 && cfg.agent.context_limit_tokens > 0 {
        return cfg.agent.context_limit_tokens;
    }
    infer_model_context_limit(model_id)
}

/// Bangun kartu pemilih model Telegram untuk provider indeks `view_pidx` halaman `page`.
pub(crate) fn tg_build_model_picker_for_provider(
    cfg: &config::Config,
    active_prov: &str,
    active_model: &str,
    active_ctx: usize,
    view_pidx: usize,
    page: usize,
    cache: &HashMap<String, usize>,
) -> Option<TgInlineCard> {
    let names = tg_sorted_providers(cfg);
    let view_name = names.get(view_pidx)?;
    let view_pcfg = cfg.providers.get(view_name)?;
    let active_kind = cfg
        .providers
        .get(active_prov)
        .map(|p| p.kind.as_str())
        .unwrap_or("openai");
    let models: Vec<TgModelEntry> = view_pcfg
        .effective_models()
        .into_iter()
        .map(|id| {
            let is_active = view_name == active_prov && id == active_model;
            let context_limit = tg_resolve_model_ctx(cfg, view_pcfg, &id, cache);
            TgModelEntry {
                id,
                context_limit,
                is_active,
            }
        })
        .collect();
    Some(build_tg_model_picker_card(
        active_prov,
        active_kind,
        active_model,
        active_ctx,
        view_pidx,
        view_name,
        &view_pcfg.kind,
        names.len(),
        &models,
        page,
    ))
}

/// Bangun kartu pemilih provider 2-kolom Telegram untuk halaman `page`.
pub(crate) fn tg_build_provider_picker(
    cfg: &config::Config,
    active_prov: &str,
    active_model: &str,
    active_ctx: usize,
    page: usize,
) -> TgInlineCard {
    let names = tg_sorted_providers(cfg);
    let active_kind = cfg
        .providers
        .get(active_prov)
        .map(|p| p.kind.as_str())
        .unwrap_or("openai");
    let items: Vec<TgModelProviderItem> = names
        .iter()
        .filter_map(|name| {
            let p = cfg.providers.get(name)?;
            Some(TgModelProviderItem {
                name: name.clone(),
                kind: p.kind.clone(),
                active_model: p.model.clone(),
                model_count: p.effective_models().len(),
                is_active: name == active_prov,
                key_ready: cfg.is_key_ready(name),
            })
        })
        .collect();
    build_tg_provider_picker_card(
        active_prov,
        active_kind,
        active_model,
        active_ctx,
        &items,
        page,
    )
}

/// Bangun kartu pemilih sesi `/resume` interaktif dari `Session` aktif.
pub(crate) fn tg_build_resume_picker(s: &Session) -> Result<TgInlineCard> {
    let cwd_str = s.ctx.cwd.to_string_lossy().to_string();
    let _ = s.store.prune_empty_sessions(&cwd_str, &s.session_id);
    let list = s.store.list_sessions(&cwd_str, 10)?;
    let items: Vec<TgResumeItem> = list
        .into_iter()
        .enumerate()
        .map(|(i, info)| {
            let msg_count = s.store.count_messages(&info.id).unwrap_or(0);
            let is_active = info.id == s.session_id;
            TgResumeItem {
                num: i + 1,
                id_short: info.id.chars().take(10).collect(),
                title: info.title,
                msg_count,
                model: info.model,
                is_active,
            }
        })
        .collect();
    Ok(build_tg_resume_picker_card(&cwd_str, &items))
}

/// Bangun kartu saklar `/approve` dari `Session` aktif.
pub(crate) fn tg_build_approve_picker(s: &Session) -> TgInlineCard {
    let policy = s.ctx.approver.policy();
    let (mode_lbl, mut cats) = {
        let p = policy.lock().unwrap_or_else(|e| e.into_inner());
        let cats: Vec<String> = p.session_allowed.iter().cloned().collect();
        (p.mode.label().to_string(), cats)
    };
    cats.sort();
    let cats_str = if cats.is_empty() {
        "(belum ada)".to_string()
    } else {
        cats.join(", ")
    };
    build_tg_approve_picker_card(&mode_lbl, &cats_str)
}

/// Bangun kartu saklar `/sandbox` dari `Session` aktif.
pub(crate) fn tg_build_sandbox_picker(s: &Session) -> TgInlineCard {
    build_tg_sandbox_picker_card(
        s.ctx.sandbox.mode().as_str(),
        &s.ctx.sandbox.backend().label(),
    )
}

/// Bangun kartu daftar `/cron` milik `chat_id` (atau job global `chat_id == 0`) dengan tombol toggle & hapus.
pub(crate) fn tg_build_cron_picker(store: &Store, chat_id: i64) -> Result<TgInlineCard> {
    let jobs = store.list_cron_jobs()?;
    let items: Vec<TgCronItem> = jobs
        .into_iter()
        .filter(|j| j.chat_id == chat_id || j.chat_id == 0)
        .map(|j| TgCronItem {
            id: j.id,
            name: j.name,
            schedule_human: selencode::cron::format_schedule_human(&j.schedule_spec),
            enabled: j.enabled,
        })
        .collect();
    Ok(build_tg_cron_picker_card(&items))
}

/// Bangun kartu `/skills` dengan tombol lihat detail 2-kolom dan simpan urutan nama skill di `tg_ui`.
pub(crate) fn tg_build_skills_picker(
    s: &Session,
    menu_markup: &str,
    chat_id: i64,
    tg_ui: &TgUiSharedState,
) -> TgInlineCard {
    let skills = selencode::skills::list_skills(&s.ctx.cwd);
    let names: Vec<String> = skills.iter().map(|sk| sk.name.clone()).collect();
    tg_ui.set_skills_list(chat_id, names);
    let items: Vec<TgSkillItem> = skills
        .into_iter()
        .map(|sk| TgSkillItem {
            name: sk.name,
            scope: sk.scope.to_string(),
        })
        .collect();
    build_tg_skills_picker_card(menu_markup, &items)
}

async fn send_or_edit_card(
    tg: &TelegramClient,
    chat_id: i64,
    msg_id: Option<i64>,
    card: TgInlineCard,
) {
    if let Some(mid) = msg_id {
        let _ = tg
            .edit_html_message_with_keyboard(chat_id, mid, &card.html, &card.plain, card.rows)
            .await;
    } else {
        let _ = tg
            .send_html_message_with_keyboard(chat_id, &card.html, &card.plain, card.rows)
            .await;
    }
}

/// Tangani callback tombol inline `/model` & `/provider` (termasuk Katalog API `mdl:c` & `mdl:a`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_tg_model_callback(
    tg: &TelegramClient,
    cb: &TgCallbackQuery,
    chat_id: i64,
    action: TgModelCallback,
    fallback_cfg: &config::Config,
    gw_store: &Option<Arc<Store>>,
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
    tg_ui: &Arc<TgUiSharedState>,
) {
    let msg_id = cb.message.as_ref().map(|m| m.message_id);
    let mut cur_cfg = config::load().unwrap_or_else(|_| fallback_cfg.clone());
    let mut cache: HashMap<String, usize> = gw_store
        .as_ref()
        .map(|st| st.load_model_context_cache())
        .unwrap_or_default();

    let active_prov = cur_cfg.default_provider.clone();
    let (active_model, active_ctx) = cur_cfg
        .providers
        .get(&active_prov)
        .map(|p| {
            let m = p.model.clone();
            let c = tg_resolve_model_ctx(&cur_cfg, p, &m, &cache);
            (m, c)
        })
        .unwrap_or_else(|| ("(belum diatur)".to_string(), 128_000));

    match action {
        TgModelCallback::Noop => {
            let _ = tg.answer_callback_query(&cb.id, None).await;
        }
        TgModelCallback::Close => {
            let _ = tg.answer_callback_query(&cb.id, Some("Ditutup")).await;
            let ctx_s = format_context_tokens_exact(active_ctx);
            let html = format!(
                "<i>Pengaturan model ditutup.</i>\nModel aktif: <code>{}</code> ({})",
                escape_telegram_html(&active_model),
                escape_telegram_html(&ctx_s)
            );
            let plain = format!("Pengaturan model ditutup.\nModel aktif: {active_model} ({ctx_s})");
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                    .await;
            }
        }
        TgModelCallback::ProviderPage(page) => {
            let _ = tg.answer_callback_query(&cb.id, None).await;
            let card =
                tg_build_provider_picker(&cur_cfg, &active_prov, &active_model, active_ctx, page);
            send_or_edit_card(tg, chat_id, msg_id, card).await;
        }
        TgModelCallback::ModelPage { provider_idx, page } => {
            let _ = tg.answer_callback_query(&cb.id, None).await;
            if let Some(card) = tg_build_model_picker_for_provider(
                &cur_cfg,
                &active_prov,
                &active_model,
                active_ctx,
                provider_idx,
                page,
                &cache,
            ) {
                send_or_edit_card(tg, chat_id, msg_id, card).await;
            } else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Daftar provider berubah, ketik /model lagi."))
                    .await;
            }
        }
        TgModelCallback::CatalogPage { provider_idx, page } => {
            let names = tg_sorted_providers(&cur_cfg);
            let Some(view_name) = names.get(provider_idx).cloned() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Provider tidak ditemukan."))
                    .await;
                return;
            };
            let Some(view_pcfg) = cur_cfg.providers.get(&view_name).cloned() else {
                return;
            };
            let cached_ids = if page > 0 {
                tg_ui.get_remote_catalog(chat_id, provider_idx)
            } else {
                tg_ui.get_fresh_remote_catalog(chat_id, provider_idx, Duration::from_secs(60))
            };
            let remote_ids = if let Some(ids) = cached_ids {
                let _ = tg.answer_callback_query(&cb.id, None).await;
                ids
            } else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Memuat katalog API..."))
                    .await;
                let Ok((_, pcfg, api_key)) = cur_cfg.resolve_provider(Some(&view_name)) else {
                    let _ = tg
                        .send_message(chat_id, &format!("API key untuk '{view_name}' belum siap."))
                        .await;
                    return;
                };
                let fetched = tokio::time::timeout(Duration::from_secs(15), async {
                    match pcfg.kind.as_str() {
                        "openai" => {
                            let u = pcfg
                                .base_url
                                .as_deref()
                                .ok_or_else(|| anyhow::anyhow!("base_url kosong"))?;
                            openai::fetch_models_with_context(u, &api_key).await
                        }
                        "gemini" => {
                            let u = pcfg
                                .base_url
                                .as_deref()
                                .unwrap_or(gemini::DEFAULT_BASE_URL);
                            gemini::fetch_models_with_context(u, &api_key).await
                        }
                        "anthropic" => {
                            let u = pcfg
                                .base_url
                                .as_deref()
                                .unwrap_or(anthropic::DEFAULT_BASE_URL);
                            anthropic::fetch_models_with_context(u, &api_key).await
                        }
                        other => anyhow::bail!("kind '{other}' belum mendukung katalog API"),
                    }
                })
                .await;
                match fetched {
                    Ok(Ok(items)) => {
                        let mut ids = Vec::with_capacity(items.len());
                        let mut discovered = Vec::new();
                        for (id, lim) in items {
                            if let Some(n) = lim {
                                cache.insert(id.clone(), n);
                                discovered.push((id.clone(), n));
                            }
                            ids.push(id);
                        }
                        if !discovered.is_empty() {
                            if let Some(st) = gw_store {
                                if let Ok(merged) = st.upsert_model_context_cache(&discovered) {
                                    cache = merged;
                                }
                            }
                        }
                        tg_ui.set_remote_catalog(chat_id, provider_idx, ids.clone());
                        ids
                    }
                    Ok(Err(e)) => {
                        let safe = tg.sanitize_error(&e.to_string());
                        let _ = tg
                            .send_message(chat_id, &format!("Gagal tarik katalog {view_name}: {safe}"))
                            .await;
                        return;
                    }
                    Err(_) => {
                        let _ = tg
                            .send_message(chat_id, "Timeout memuat katalog API (15 dtk).")
                            .await;
                        return;
                    }
                }
            };
            let entries: Vec<TgModelEntry> = remote_ids
                .into_iter()
                .map(|id| {
                    let is_active = view_name == active_prov && id == active_model;
                    let context_limit = tg_resolve_model_ctx(&cur_cfg, &view_pcfg, &id, &cache);
                    TgModelEntry {
                        id,
                        context_limit,
                        is_active,
                    }
                })
                .collect();
            let card = build_tg_catalog_picker_card(
                &active_prov,
                &active_model,
                provider_idx,
                &view_name,
                &view_pcfg.kind,
                &entries,
                page,
            );
            send_or_edit_card(tg, chat_id, msg_id, card).await;
        }
        TgModelCallback::AddAndSelectCatalog {
            provider_idx,
            remote_idx,
        } => {
            let names = tg_sorted_providers(&cur_cfg);
            let Some(target_prov) = names.get(provider_idx).cloned() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Provider tidak ditemukan."))
                    .await;
                return;
            };
            let Some(chosen_model) = tg_ui
                .get_remote_catalog(chat_id, provider_idx)
                .and_then(|v| v.get(remote_idx).cloned())
            else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Katalog kedaluwarsa, buka Katalog API lagi."))
                    .await;
                return;
            };
            let (chosen_kind, chosen_ctx) = {
                let Some(p) = cur_cfg.providers.get_mut(&target_prov) else {
                    return;
                };
                p.models = p.effective_models();
                p.push_model_unique(&chosen_model);
                p.model = chosen_model.clone();
                let kind = p.kind.clone();
                let snap = p.clone();
                (kind, tg_resolve_model_ctx(&cur_cfg, &snap, &snap.model, &cache))
            };
            cur_cfg.default_provider = target_prov.clone();
            if let Err(e) = config::save(&cur_cfg) {
                let _ = tg
                    .answer_callback_query(&cb.id, Some(&format!("Gagal simpan: {e}")))
                    .await;
                return;
            }
            let live_session = { sessions.lock().await.get(&chat_id).cloned() };
            let mut final_ctx = chosen_ctx;
            if let Some(s_arc) = live_session {
                if let Ok(mut s_guard) = s_arc.try_lock() {
                    s_guard.model_context_cache = cache.clone();
                    s_guard.cfg = cur_cfg.clone();
                    if s_guard.switch_provider(&target_prov).is_ok() {
                        final_ctx = s_guard.ctx.context_limit_tokens;
                    }
                }
            }
            let toast: String = chosen_model.chars().take(40).collect();
            let _ = tg
                .answer_callback_query(&cb.id, Some(&format!("✓ Disimpan & aktif: {toast}")))
                .await;
            let (html, plain) =
                format_tg_model_switched_html(&target_prov, &chosen_kind, &chosen_model, final_ctx);
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                    .await;
            } else {
                let _ = tg.send_html_message(chat_id, &html, &plain).await;
            }
        }
        TgModelCallback::SelectModel {
            provider_idx,
            model_idx,
        } => {
            let names = tg_sorted_providers(&cur_cfg);
            let Some(target_prov) = names.get(provider_idx).cloned() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Provider tidak ditemukan, ketik /model lagi."))
                    .await;
                return;
            };
            let (chosen_model, chosen_kind, chosen_ctx) = {
                let Some(p) = cur_cfg.providers.get_mut(&target_prov) else {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some("Provider tidak ditemukan."))
                        .await;
                    return;
                };
                let list = p.effective_models();
                let Some(chosen) = list.get(model_idx).cloned() else {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some("Daftar model berubah, ketik /model lagi."))
                        .await;
                    return;
                };
                p.models = list;
                p.model = chosen.clone();
                let kind = p.kind.clone();
                let p_snap = p.clone();
                (
                    chosen,
                    kind,
                    tg_resolve_model_ctx(&cur_cfg, &p_snap, &p_snap.model, &cache),
                )
            };
            cur_cfg.default_provider = target_prov.clone();
            if let Err(e) = config::save(&cur_cfg) {
                let _ = tg
                    .answer_callback_query(&cb.id, Some(&format!("Gagal simpan: {e}")))
                    .await;
                return;
            }
            let live_session = { sessions.lock().await.get(&chat_id).cloned() };
            let mut final_ctx = chosen_ctx;
            if let Some(s_arc) = live_session {
                if let Ok(mut s_guard) = s_arc.try_lock() {
                    s_guard.model_context_cache = cache.clone();
                    s_guard.cfg = cur_cfg.clone();
                    if s_guard.switch_provider(&target_prov).is_ok() {
                        final_ctx = s_guard.ctx.context_limit_tokens;
                    }
                }
            }
            let toast_model: String = chosen_model.chars().take(40).collect();
            let _ = tg
                .answer_callback_query(&cb.id, Some(&format!("✓ {toast_model}")))
                .await;
            let (html, plain) =
                format_tg_model_switched_html(&target_prov, &chosen_kind, &chosen_model, final_ctx);
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                    .await;
            } else {
                let _ = tg.send_html_message(chat_id, &html, &plain).await;
            }
        }
    }
}

/// Tangani callback inline `/resume` (`res:s:<num>`, `res:new`, `res:x`).
pub(crate) async fn handle_tg_resume_callback(
    tg: &TelegramClient,
    cb: &TgCallbackQuery,
    chat_id: i64,
    action: TgResumeCallback,
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
) {
    let msg_id = cb.message.as_ref().map(|m| m.message_id);
    match action {
        TgResumeCallback::Close => {
            let _ = tg.answer_callback_query(&cb.id, Some("Ditutup")).await;
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(
                        chat_id,
                        mid,
                        "<i>Daftar sesi ditutup.</i>",
                        "Daftar sesi ditutup.",
                        Vec::new(),
                    )
                    .await;
            }
        }
        TgResumeCallback::NewSession => {
            let live = { sessions.lock().await.get(&chat_id).cloned() };
            let Some(s_arc) = live else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Ketik /new untuk memulai sesi baru."))
                    .await;
                return;
            };
            let Ok(mut s) = s_arc.try_lock() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Tunggu giliran aktif selesai dulu."))
                    .await;
                return;
            };
            match s.reset_session() {
                Ok(sid) => {
                    let _ = s
                        .store
                        .set_telegram_chat_session(chat_id, Store::SESSION_POINTER_NEW);
                    let (html, plain) = format_tg_new_session_html(
                        &sid,
                        &s.provider_name,
                        &s.pcfg.kind,
                        &s.pcfg.model,
                        s.ctx.context_limit_tokens,
                        &s.ctx.cwd.display().to_string(),
                    );
                    let _ = tg
                        .answer_callback_query(&cb.id, Some("✨ Sesi baru dimulai"))
                        .await;
                    if let Some(mid) = msg_id {
                        let _ = tg
                            .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                            .await;
                    } else {
                        let _ = tg.send_html_message(chat_id, &html, &plain).await;
                    }
                }
                Err(e) => {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("Gagal: {e}")))
                        .await;
                }
            }
        }
        TgResumeCallback::Select(num) => {
            let live = { sessions.lock().await.get(&chat_id).cloned() };
            let Some(s_arc) = live else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some(&format!("Ketik /resume {num}.")))
                    .await;
                return;
            };
            let Ok(mut s) = s_arc.try_lock() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Tunggu giliran aktif selesai dulu."))
                    .await;
                return;
            };
            match s.resume_by_target(&num.to_string()) {
                Ok(data) => {
                    let _ = s.store.set_telegram_chat_session(chat_id, &data.session_id);
                    let short: String = data.session_id.chars().take(10).collect();
                    let ctx_s = format_context_tokens_exact(data.context_limit);
                    let html = format!(
                        "<b>Melanjutkan Sesi #{num}</b>\n\n\
                         • <b>Judul:</b> {}\n\
                         • <b>Sesi:</b> <code>{short}</code> ({} pesan)\n\
                         • <b>Model:</b> <code>{}</code> ({ctx_s})",
                        escape_telegram_html(&data.title),
                        data.total_messages,
                        escape_telegram_html(&data.model),
                    );
                    let plain = format!(
                        "Melanjutkan Sesi #{num}: {} ({short}) — {} pesan · {}",
                        data.title, data.total_messages, data.model
                    );
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("✓ Sesi #{num} dimuat")))
                        .await;
                    if let Some(mid) = msg_id {
                        let _ = tg
                            .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                            .await;
                    } else {
                        let _ = tg.send_html_message(chat_id, &html, &plain).await;
                    }
                }
                Err(e) => {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("Gagal: {e}")))
                        .await;
                }
            }
        }
        TgResumeCallback::SelectId(id_prefix) => {
            let live = { sessions.lock().await.get(&chat_id).cloned() };
            let Some(s_arc) = live else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some(&format!("Ketik /resume {id_prefix}.")))
                    .await;
                return;
            };
            let Ok(mut s) = s_arc.try_lock() else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Tunggu giliran aktif selesai dulu."))
                    .await;
                return;
            };
            match s.resume_by_target(&id_prefix) {
                Ok(data) => {
                    let _ = s.store.set_telegram_chat_session(chat_id, &data.session_id);
                    let short: String = data.session_id.chars().take(10).collect();
                    let ctx_s = format_context_tokens_exact(data.context_limit);
                    let html = format!(
                        "<b>Melanjutkan Sesi</b>\n\n\
                         • <b>Judul:</b> {}\n\
                         • <b>Sesi:</b> <code>{short}</code> ({} pesan)\n\
                         • <b>Model:</b> <code>{}</code> ({ctx_s})",
                        escape_telegram_html(&data.title),
                        data.total_messages,
                        escape_telegram_html(&data.model),
                    );
                    let plain = format!(
                        "Melanjutkan Sesi: {} ({short}) — {} pesan · {}",
                        data.title, data.total_messages, data.model
                    );
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("✓ Sesi {short} dimuat")))
                        .await;
                    if let Some(mid) = msg_id {
                        let _ = tg
                            .edit_html_message_with_keyboard(chat_id, mid, &html, &plain, Vec::new())
                            .await;
                    } else {
                        let _ = tg.send_html_message(chat_id, &html, &plain).await;
                    }
                }
                Err(e) => {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("Gagal: {e}")))
                        .await;
                }
            }
        }
    }
}

/// Tangani callback saklar `/approve` & `/sandbox` (`cfg:ap:*`, `cfg:sb:*`, `cfg:x`).
pub(crate) async fn handle_tg_config_callback(
    tg: &TelegramClient,
    cb: &TgCallbackQuery,
    chat_id: i64,
    action: TgConfigCallback,
    fallback_cfg: &config::Config,
    sessions: &Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<Session>>>>>,
) {
    let msg_id = cb.message.as_ref().map(|m| m.message_id);
    match action {
        TgConfigCallback::Close => {
            let _ = tg.answer_callback_query(&cb.id, Some("Ditutup")).await;
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(
                        chat_id,
                        mid,
                        "<i>Pengaturan ditutup.</i>",
                        "Pengaturan ditutup.",
                        Vec::new(),
                    )
                    .await;
            }
        }
        TgConfigCallback::SetApprove(mode_str) => {
            let new_mode = match mode_str.as_str() {
                "ask" => ApprovalMode::Ask,
                "edits" => ApprovalMode::AutoEdits,
                "auto" => ApprovalMode::AutoAll,
                _ => return,
            };
            let mut cur_cfg = config::load().unwrap_or_else(|_| fallback_cfg.clone());
            cur_cfg.telegram.approval = mode_str.clone();
            let _ = config::save(&cur_cfg);

            let live = { sessions.lock().await.get(&chat_id).cloned() };
            let card = if let Some(s_arc) = live {
                if let Ok(s) = s_arc.try_lock() {
                    let pol = s.ctx.approver.policy();
                    if let Ok(mut p) = pol.lock() {
                        p.mode = new_mode;
                        if matches!(new_mode, ApprovalMode::Ask) {
                            p.session_allowed.clear();
                        }
                    }
                    tg_build_approve_picker(&s)
                } else {
                    build_tg_approve_picker_card(new_mode.label(), "(sedang berjalan)")
                }
            } else {
                build_tg_approve_picker_card(new_mode.label(), "(belum ada)")
            };
            let _ = tg
                .answer_callback_query(&cb.id, Some(&format!("✓ Approval: {mode_str}")))
                .await;
            send_or_edit_card(tg, chat_id, msg_id, card).await;
        }
        TgConfigCallback::SetSandbox(mode_str) => {
            let Some(sb_mode) = selencode::sandbox::SandboxMode::parse(&mode_str) else {
                return;
            };
            let mut cur_cfg = config::load().unwrap_or_else(|_| fallback_cfg.clone());
            cur_cfg.sandbox.mode = sb_mode.as_str().to_string();
            let _ = config::save(&cur_cfg);

            let live = { sessions.lock().await.get(&chat_id).cloned() };
            let card = if let Some(s_arc) = live {
                if let Ok(mut s) = s_arc.try_lock() {
                    s.ctx.sandbox.set_mode(sb_mode);
                    s.cfg.sandbox.mode = sb_mode.as_str().to_string();
                    tg_build_sandbox_picker(&s)
                } else {
                    build_tg_sandbox_picker_card(sb_mode.as_str(), "aktif")
                }
            } else {
                build_tg_sandbox_picker_card(sb_mode.as_str(), "aktif")
            };
            let _ = tg
                .answer_callback_query(&cb.id, Some(&format!("✓ Sandbox: {}", sb_mode.as_str())))
                .await;
            send_or_edit_card(tg, chat_id, msg_id, card).await;
        }
    }
}

/// Tangani callback tombol cepat `/cron` (`crn:t:<id>`, `crn:d:<id>`, `crn:x`).
pub(crate) async fn handle_tg_cron_callback(
    tg: &TelegramClient,
    cb: &TgCallbackQuery,
    chat_id: i64,
    action: TgCronCallback,
    gw_store: &Option<Arc<Store>>,
) {
    let msg_id = cb.message.as_ref().map(|m| m.message_id);
    if matches!(action, TgCronCallback::Close) {
        let _ = tg.answer_callback_query(&cb.id, Some("Ditutup")).await;
        if let Some(mid) = msg_id {
            let _ = tg
                .edit_html_message_with_keyboard(
                    chat_id,
                    mid,
                    "<i>Daftar cron ditutup.</i>",
                    "Daftar cron ditutup.",
                    Vec::new(),
                )
                .await;
        }
        return;
    }
    let Some(st) = gw_store else {
        let _ = tg
            .answer_callback_query(&cb.id, Some("Store SQLite tidak tersedia."))
            .await;
        return;
    };
    let toast = match action {
        TgCronCallback::Toggle(id) => {
            let id_s = id.to_string();
            match st.get_cron_job(&id_s) {
                Ok(Some(job)) if job.chat_id == chat_id || job.chat_id == 0 => {
                    let next_en = !job.enabled;
                    let next_run = if next_en {
                        selencode::cron::compute_next_run_after(
                            &job.schedule_spec,
                            chrono::Utc::now(),
                        )
                        .ok()
                    } else {
                        None
                    };
                    let _ = st.set_cron_job_enabled(&id_s, next_en, next_run.as_deref());
                    if next_en {
                        format!("▶ Cron #{id} diaktifkan")
                    } else {
                        format!("⏸ Cron #{id} dijeda")
                    }
                }
                Ok(Some(_)) => "Ditolak: jadwal milik chat lain.".to_string(),
                _ => format!("Cron #{id} tidak ditemukan"),
            }
        }
        TgCronCallback::Delete(id) => {
            let id_s = id.to_string();
            match st.get_cron_job(&id_s) {
                Ok(Some(job)) if job.chat_id == chat_id || job.chat_id == 0 => {
                    match st.remove_cron_job(&id_s) {
                        Ok(true) => format!("🗑 Cron #{id} dihapus"),
                        _ => format!("Cron #{id} tidak ditemukan"),
                    }
                }
                Ok(Some(_)) => "Ditolak: jadwal milik chat lain.".to_string(),
                _ => format!("Cron #{id} tidak ditemukan"),
            }
        }
        TgCronCallback::Close => unreachable!(),
    };
    let _ = tg.answer_callback_query(&cb.id, Some(&toast)).await;
    if let Ok(card) = tg_build_cron_picker(st, chat_id) {
        send_or_edit_card(tg, chat_id, msg_id, card).await;
    }
}

/// Tangani callback tombol detail `/skills` (`skl:v:<idx>`, `skl:x`).
pub(crate) async fn handle_tg_skill_callback(
    tg: &TelegramClient,
    cb: &TgCallbackQuery,
    chat_id: i64,
    action: TgSkillCallback,
    cwd: &Path,
    tg_ui: &TgUiSharedState,
) {
    let msg_id = cb.message.as_ref().map(|m| m.message_id);
    match action {
        TgSkillCallback::Close => {
            let _ = tg.answer_callback_query(&cb.id, Some("Ditutup")).await;
            if let Some(mid) = msg_id {
                let _ = tg
                    .edit_html_message_with_keyboard(
                        chat_id,
                        mid,
                        "<i>Daftar skills ditutup.</i>",
                        "Daftar skills ditutup.",
                        Vec::new(),
                    )
                    .await;
            }
        }
        TgSkillCallback::View(idx) => {
            let Some(name) = tg_ui.get_skill_name(chat_id, idx) else {
                let _ = tg
                    .answer_callback_query(&cb.id, Some("Daftar berubah, ketik /skills lagi."))
                    .await;
                return;
            };
            match selencode::skills::load_skill(cwd, &name) {
                Ok((info, body)) => {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("📖 {name}")))
                        .await;
                    let preview: String = body.chars().take(1800).collect();
                    let html = format!(
                        "📖 <b>Skill: <code>{}</code></b> ({})\n<i>{}</i>\n\n<pre>{}</pre>",
                        escape_telegram_html(&info.name),
                        escape_telegram_html(info.scope),
                        escape_telegram_html(&info.summary),
                        escape_telegram_html(&preview),
                    );
                    let plain = format!("Skill {} ({}):\n\n{preview}", info.name, info.scope);
                    let _ = tg.send_html_message(chat_id, &html, &plain).await;
                }
                Err(e) => {
                    let _ = tg
                        .answer_callback_query(&cb.id, Some(&format!("Gagal muat: {e}")))
                        .await;
                }
            }
        }
    }
}
