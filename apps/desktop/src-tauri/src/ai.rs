//! ИИ-слой в приложении, шаг И1 (`docs/ai-layer-design.md` § 3): настройки ИИ, подключения к
//! моделям (профили), их ключи, проверка подключения и журнал обращений. Сами обращения — через
//! шлюз `aiowa-ai`; запись журнала — в базу учётной записи (`ai_calls`).
//!
//! Настройки — в `sync_state` под ключами `ai.*`, как остальные настройки окна; профили — одним
//! JSON. Ключ подключения — в диспетчере учётных данных Windows (служба «AIOWA», запись
//! `ai:<профиль>`): окно его передаёт один раз при сохранении и обратно получает только «ключ
//! задан». Ключ привязан к адресу: сменили адрес или протокол без нового ключа — старый удаляется,
//! чтобы не уйти на другой сервер.

use crate::sync::KEYRING_SERVICE;
use crate::{err, AppState, CmdResult};
use aiowa_ai::{Cancel, ChatRequest, Effort, Gateway, Message, ModelInfo, Place, PrivacyMode, Profile, ProviderKind, StopReason};
use aiowa_core::AccountId;
use aiowa_store::ai_log::{AiCall, AiUsage};
use aiowa_store::Store;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::State;
use tracing::{info, warn};

const ENABLED_KEY: &str = "ai.enabled";
const MODE_KEY: &str = "ai.mode";
const PROFILES_KEY: &str = "ai.profiles";
const ADVICE_KEY: &str = "ai.assign.advice";
const PARSE_KEY: &str = "ai.assign.parse";

/// Подключений самое большее (список в настройках, не каталог).
const PROFILES_MAX: usize = 20;
/// Предел длины ключа, символов: ключи провайдеров короче, длиннее — вставили не то.
const KEY_MAX: usize = 512;

/// Шлюз моделей и идущие проверки подключений (по профилю: новая проверка отменяет прежнюю).
pub(crate) struct AiState {
    pub(crate) gateway: Gateway,
    tests: Mutex<HashMap<String, Cancel>>,
    /// Идущие «Советы» по номеру просьбы окна: отмена из панели и при закрытии композера.
    advice: Mutex<HashMap<String, Cancel>>,
}

impl AiState {
    /// Шлюз с журналом в базу учётной записи.
    pub(crate) fn new(store: Arc<Store>, account: AccountId) -> Result<Self, String> {
        let journal = Arc::new(move |r: aiowa_ai::CallRecord| {
            let call = AiCall {
                at: r.at_ms,
                feature: r.feature,
                profile_id: r.profile_id,
                host: r.host,
                place: place_code(r.place).into(),
                model: r.model,
                input_tokens: r.input_tokens.map(|n| n as i64),
                output_tokens: r.output_tokens.map(|n| n as i64),
                duration_ms: r.duration_ms as i64,
                outcome: r.outcome,
                request_sha256: r.request_sha256,
            };
            if let Err(e) = store.ai_call_add(account, &call) {
                warn!(error = %e, "журнал обращений к моделям: запись не добавлена");
            }
        });
        let gateway = Gateway::new(&format!("AIOWA/{}", env!("CARGO_PKG_VERSION")), journal).map_err(|e| e.to_string())?;
        Ok(Self { gateway, tests: Mutex::new(HashMap::new()), advice: Mutex::new(HashMap::new()) })
    }

    /// Отменить идущую проверку профиля (и забыть её).
    /// Новый «Совет»: запомнить его отмену (прежний с тем же номером — отменить).
    pub(crate) fn start_advice(&self, request_id: &str) -> Cancel {
        let c = Cancel::new();
        if let Some(old) = self.advice.lock().unwrap_or_else(|p| p.into_inner()).insert(request_id.to_string(), c.clone()) {
            old.cancel();
        }
        c
    }

    pub(crate) fn finish_advice(&self, request_id: &str) {
        self.advice.lock().unwrap_or_else(|p| p.into_inner()).remove(request_id);
    }

    pub(crate) fn cancel_advice(&self, request_id: &str) {
        if let Some(c) = self.advice.lock().unwrap_or_else(|p| p.into_inner()).remove(request_id) {
            c.cancel();
        }
    }

    fn cancel_test(&self, id: &str) {
        if let Some(c) = self.tests.lock().unwrap_or_else(|p| p.into_inner()).remove(id) {
            c.cancel();
        }
    }
}

fn place_code(p: Place) -> &'static str {
    match p {
        Place::Local => "local",
        Place::OwnServer => "own_server",
        Place::Cloud => "cloud",
    }
}

// --- настройки -----------------------------------------------------------------------------

/// Настройки ИИ в значениях.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiConfig {
    pub(crate) enabled: bool,
    pub(crate) mode: PrivacyMode,
    pub(crate) profiles: Vec<Profile>,
    pub(crate) advice: Option<String>,
    parse: Option<String>,
}

impl AiConfig {
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let profiles: Vec<Profile> = get(PROFILES_KEY).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let known = |id: Option<String>| id.filter(|id| profiles.iter().any(|p| &p.id == id));
        Self {
            enabled: get(ENABLED_KEY).is_some_and(|v| v == "1"),
            mode: match get(MODE_KEY).as_deref() {
                Some("own_only") => PrivacyMode::OwnOnly,
                _ => PrivacyMode::Any,
            },
            advice: known(get(ADVICE_KEY)),
            parse: known(get(PARSE_KEY)),
            profiles,
        }
    }

    pub(crate) fn load(store: &Store, account: AccountId) -> Self {
        Self::from_lookup(|key| match store.get_state(account, key) {
            Ok(v) => v,
            Err(e) => {
                warn!(key, error = %e, "не удалось прочитать настройку ИИ");
                None
            }
        })
    }

    fn save(&self, store: &Store, account: AccountId) -> CmdResult<()> {
        let profiles = serde_json::to_string(&self.profiles).map_err(err)?;
        let mode = match self.mode {
            PrivacyMode::Any => "any",
            PrivacyMode::OwnOnly => "own_only",
        };
        store.set_state(account, ENABLED_KEY, Some(if self.enabled { "1" } else { "0" })).map_err(err)?;
        store.set_state(account, MODE_KEY, Some(mode)).map_err(err)?;
        store.set_state(account, PROFILES_KEY, Some(&profiles)).map_err(err)?;
        store.set_state(account, ADVICE_KEY, self.advice.as_deref()).map_err(err)?;
        store.set_state(account, PARSE_KEY, self.parse.as_deref()).map_err(err)?;
        Ok(())
    }

    pub(crate) fn profile(&self, id: &str) -> CmdResult<&Profile> {
        self.profiles.iter().find(|p| p.id == id).ok_or_else(|| "Подключение не найдено".to_string())
    }
}

// --- ключи ---------------------------------------------------------------------------------

fn key_entry(id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("ai:{id}")).map_err(|e| format!("Диспетчер учётных данных Windows недоступен: {e}"))
}

pub(crate) fn key_get(id: &str) -> Option<SecretString> {
    key_entry(id).ok()?.get_password().ok().map(SecretString::from)
}

fn key_set(id: &str, key: &str) -> CmdResult<()> {
    key_entry(id)?.set_password(key).map_err(|e| format!("Не удалось сохранить ключ в диспетчере учётных данных Windows: {e}"))
}

fn key_delete(id: &str) {
    match key_entry(id).map(|e| e.delete_credential()) {
        Ok(Ok(())) | Ok(Err(keyring::Error::NoEntry)) => {}
        Ok(Err(e)) => warn!(error = %e, "ключ подключения не удалён из диспетчера учётных данных"),
        Err(e) => warn!(error = %e, "ключ подключения не удалён"),
    }
}

// --- DTO ------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ProfileDto {
    #[serde(flatten)]
    profile: Profile,
    has_key: bool,
    place: Place,
    /// Адрес API, как к нему пойдёт шлюз (с `/v1` у OpenAI-совместимых); пустой — адрес неверный.
    api_base: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AiSettingsDto {
    enabled: bool,
    mode: PrivacyMode,
    profiles: Vec<ProfileDto>,
    advice_profile: Option<String>,
    parse_profile: Option<String>,
}

fn dto(cfg: &AiConfig) -> AiSettingsDto {
    AiSettingsDto {
        enabled: cfg.enabled,
        mode: cfg.mode,
        profiles: cfg
            .profiles
            .iter()
            .map(|p| ProfileDto { has_key: key_get(&p.id).is_some(), place: p.place(), api_base: p.api_base().map(|u| u.to_string()).unwrap_or_default(), profile: p.clone() })
            .collect(),
        advice_profile: cfg.advice.clone(),
        parse_profile: cfg.parse.clone(),
    }
}

/// Изменение настроек; `Some(None)` у назначения — снять назначение.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct AiSettingsPatch {
    enabled: Option<bool>,
    mode: Option<PrivacyMode>,
    #[serde(default, with = "double_option")]
    advice_profile: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    parse_profile: Option<Option<String>>,
}

/// `null` в JSON — `Some(None)`, отсутствие поля — `None`.
mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// Профиль из окна: без `id` — новый.
#[derive(Debug, Deserialize)]
pub(crate) struct ProfileInput {
    id: Option<String>,
    name: String,
    kind: ProviderKind,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    timeout_s: Option<u32>,
    #[serde(default)]
    own_server: bool,
    #[serde(default)]
    extra: String,
}

// --- команды --------------------------------------------------------------------------------

#[tauri::command]
pub(crate) fn ai_settings(state: State<'_, AppState>) -> CmdResult<AiSettingsDto> {
    Ok(dto(&AiConfig::load(&state.store, state.account)))
}

#[tauri::command]
pub(crate) fn ai_settings_set(patch: AiSettingsPatch, state: State<'_, AppState>) -> CmdResult<AiSettingsDto> {
    let mut cfg = AiConfig::load(&state.store, state.account);
    if let Some(v) = patch.enabled {
        cfg.enabled = v;
    }
    if let Some(m) = patch.mode {
        cfg.mode = m;
    }
    for (slot, value) in [(&mut cfg.advice, patch.advice_profile), (&mut cfg.parse, patch.parse_profile)] {
        if let Some(v) = value {
            if let Some(id) = &v
                && !cfg.profiles.iter().any(|p| &p.id == id)
            {
                return Err("Подключение не найдено".into());
            }
            *slot = v;
        }
    }
    cfg.save(&state.store, state.account)?;
    info!(enabled = cfg.enabled, mode = ?cfg.mode, advice = cfg.advice.is_some(), parse = cfg.parse.is_some(), "настройки ИИ изменены");
    Ok(dto(&cfg))
}

/// Сохранить подключение (новое или изменённое). `key` — новый ключ (пустой — не менять),
/// `clear_key` — убрать сохранённый.
#[tauri::command]
pub(crate) fn ai_profile_save(profile: ProfileInput, key: Option<String>, clear_key: bool, state: State<'_, AppState>, ai: State<'_, AiState>) -> CmdResult<AiSettingsDto> {
    let mut cfg = AiConfig::load(&state.store, state.account);
    let key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    if key.as_ref().is_some_and(|k| k.chars().count() > KEY_MAX || k.chars().any(char::is_control)) {
        return Err("Это не похоже на ключ: проверьте, что вставлен ключ целиком и без лишнего".into());
    }
    let existing = profile.id.as_deref().and_then(|id| cfg.profiles.iter().position(|p| p.id == id));
    if profile.id.is_some() && existing.is_none() {
        return Err("Подключение не найдено".into());
    }
    if existing.is_none() && cfg.profiles.len() >= PROFILES_MAX {
        return Err(format!("Подключений не больше {PROFILES_MAX}"));
    }
    let id = profile.id.clone().unwrap_or_else(aiowa_transport::new_guid);
    let next = Profile {
        id: id.clone(),
        name: profile.name.trim().to_string(),
        kind: profile.kind,
        base_url: profile.base_url.trim().to_string(),
        model: profile.model.trim().to_string(),
        timeout_s: profile.timeout_s.unwrap_or(aiowa_ai::profile::DEFAULT_TIMEOUT_S),
        own_server: profile.own_server,
        extra: if profile.kind == ProviderKind::OpenAi { profile.extra.trim().to_string() } else { String::new() },
    };
    next.validate().map_err(|e| e.0)?;
    // Ключ привязан к адресу: другой сервер без нового ключа старого не получает.
    let moved = existing.is_some_and(|i| {
        let old = &cfg.profiles[i];
        old.kind != next.kind || old.api_base().ok().map(|u| u.origin()) != next.api_base().ok().map(|u| u.origin())
    });
    ai.cancel_test(&id);
    if clear_key || (moved && key.is_none()) {
        key_delete(&id);
    }
    if let Some(k) = &key {
        key_set(&id, k)?;
    }
    match existing {
        Some(i) => cfg.profiles[i] = next,
        None => cfg.profiles.push(next),
    }
    cfg.save(&state.store, state.account)?;
    info!(new = existing.is_none(), key_changed = key.is_some() || clear_key || moved, "подключение к модели сохранено");
    Ok(dto(&cfg))
}

#[tauri::command]
pub(crate) fn ai_profile_delete(id: String, state: State<'_, AppState>, ai: State<'_, AiState>) -> CmdResult<AiSettingsDto> {
    let mut cfg = AiConfig::load(&state.store, state.account);
    let before = cfg.profiles.len();
    cfg.profiles.retain(|p| p.id != id);
    if cfg.profiles.len() == before {
        return Err("Подключение не найдено".into());
    }
    for slot in [&mut cfg.advice, &mut cfg.parse] {
        if slot.as_deref() == Some(id.as_str()) {
            *slot = None;
        }
    }
    ai.cancel_test(&id);
    key_delete(&id);
    cfg.save(&state.store, state.account)?;
    info!("подключение к модели удалено");
    Ok(dto(&cfg))
}

/// Модели подключения — для выбора модели в настройках.
#[tauri::command]
pub(crate) async fn ai_profile_models(id: String, state: State<'_, AppState>, ai: State<'_, AiState>) -> CmdResult<Vec<ModelInfo>> {
    let cfg = AiConfig::load(&state.store, state.account);
    let profile = cfg.profile(&id)?.clone();
    let key = key_get(&id);
    ai.gateway.list_models(&profile, key.as_ref(), cfg.mode, &Cancel::new()).await.map_err(|e| e.to_string())
}

/// Итог проверки подключения: список моделей и короткий ответ выбранной модели.
#[derive(Debug, Serialize, Default)]
pub(crate) struct AiTestDto {
    models: Vec<ModelInfo>,
    models_error: Option<String>,
    /// Выбранная модель есть в списке сервера; `None` — список не получен или модель не выбрана.
    model_listed: Option<bool>,
    /// Ответ модели (обрезан до 200 символов); `None` — до ответа не дошло.
    reply: Option<String>,
    reply_error: Option<String>,
    /// Время до первого куска текста и до конца ответа, мс.
    first_ms: Option<u64>,
    total_ms: Option<u64>,
    stop: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Модель, которую назвал сервер.
    served_model: Option<String>,
}

/// «Проверить»: список моделей и одно короткое обращение к выбранной модели (в журнал — как
/// «проверка»). Явное действие автора: работает и при выключенных ИИ-функциях, но по политике
/// приватности (облако в режиме «только свои» не проверяется).
#[tauri::command]
pub(crate) async fn ai_profile_test(id: String, state: State<'_, AppState>, ai: State<'_, AiState>) -> CmdResult<AiTestDto> {
    let cfg = AiConfig::load(&state.store, state.account);
    let profile = cfg.profile(&id)?.clone();
    let key = key_get(&id);
    ai.cancel_test(&id);
    let cancel = Cancel::new();
    ai.tests.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), cancel.clone());

    let mut out = AiTestDto::default();
    match ai.gateway.list_models(&profile, key.as_ref(), cfg.mode, &cancel).await {
        Ok(models) => {
            if !profile.model.is_empty() {
                out.model_listed = Some(models.iter().any(|m| m.id == profile.model));
            }
            out.models = models;
        }
        Err(aiowa_ai::AiError::Policy) => {
            out.models_error = Some(aiowa_ai::AiError::Policy.to_string());
            ai.cancel_test(&id);
            return Ok(out);
        }
        Err(e) => out.models_error = Some(e.to_string()),
    }
    if profile.model.is_empty() {
        out.reply_error = Some("Модель не выбрана: выберите её из списка и сохраните подключение".into());
        ai.cancel_test(&id);
        return Ok(out);
    }
    let req = ChatRequest {
        feature: "проверка",
        system: "Это проверка подключения почтового клиента. Ответь по-русски одним словом.".into(),
        messages: vec![Message::user("Ты на связи? Ответь одним словом: «готов».")],
        max_tokens: 1024,
        effort: (profile.kind == ProviderKind::Anthropic).then_some(Effort::Low),
    };
    let started = Instant::now();
    let mut first: Option<u64> = None;
    let mut on_text = |_: &str| {
        if first.is_none() {
            first = Some(started.elapsed().as_millis() as u64);
        }
    };
    let result = ai.gateway.chat(&profile, key.as_ref(), cfg.mode, &req, &mut on_text, &cancel).await;
    out.first_ms = first;
    out.total_ms = Some(started.elapsed().as_millis() as u64);
    match result {
        Ok(o) => {
            out.stop = Some(
                match &o.stop {
                    StopReason::End => "end",
                    StopReason::MaxTokens => "max_tokens",
                    StopReason::Refusal => "refusal",
                    StopReason::Other(_) => "other",
                }
                .into(),
            );
            let text = o.text.trim();
            out.reply = Some(text.chars().take(200).collect());
            if text.is_empty() {
                out.reply_error = Some(match o.stop {
                    StopReason::MaxTokens => "Модель не успела ответить в отведённые 1024 токена (долго размышляет)".into(),
                    StopReason::Refusal => "Модель отказалась отвечать".into(),
                    _ => "Модель ответила пустым текстом".into(),
                });
            }
            out.input_tokens = o.input_tokens;
            out.output_tokens = o.output_tokens;
            out.served_model = o.model;
        }
        Err(e) => out.reply_error = Some(e.to_string()),
    }
    ai.cancel_test(&id);
    Ok(out)
}

#[derive(Debug, Serialize)]
pub(crate) struct AiCallDto {
    at_ms: i64,
    feature: String,
    profile_id: String,
    host: String,
    place: String,
    model: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    duration_ms: i64,
    outcome: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AiUsageDto {
    profile_id: String,
    model: String,
    place: String,
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
}

#[derive(Debug, Serialize)]
pub(crate) struct AiJournalDto {
    recent: Vec<AiCallDto>,
    /// Итоги за последние 30 дней по подключению и модели.
    month: Vec<AiUsageDto>,
}

/// Журнал обращений: последние 30 и итоги за 30 дней.
#[tauri::command]
pub(crate) fn ai_journal(state: State<'_, AppState>) -> CmdResult<AiJournalDto> {
    let recent = state.store.ai_calls_recent(state.account, 30).map_err(err)?;
    let since = jiff::Timestamp::now().as_millisecond() - 30 * 24 * 3600 * 1000;
    let month = state.store.ai_usage_since(state.account, since).map_err(err)?;
    Ok(AiJournalDto {
        recent: recent
            .into_iter()
            .map(|c| AiCallDto { at_ms: c.at, feature: c.feature, profile_id: c.profile_id, host: c.host, place: c.place, model: c.model, input_tokens: c.input_tokens, output_tokens: c.output_tokens, duration_ms: c.duration_ms, outcome: c.outcome })
            .collect(),
        month: month
            .into_iter()
            .map(|u: AiUsage| AiUsageDto { profile_id: u.profile_id, model: u.model, place: u.place, calls: u.calls, input_tokens: u.input_tokens, output_tokens: u.output_tokens })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_dangling_assignments() {
        let empty = AiConfig::from_lookup(|_| None);
        assert!(!empty.enabled, "ИИ по умолчанию выключен");
        assert_eq!(empty.mode, PrivacyMode::Any);
        assert!(empty.profiles.is_empty());

        let profiles = r#"[{"id":"a","name":"Облако","kind":"anthropic","base_url":"","model":"claude-opus-5-5","timeout_s":120,"own_server":false}]"#;
        let cfg = AiConfig::from_lookup(|k| match k {
            ENABLED_KEY => Some("1".into()),
            MODE_KEY => Some("own_only".into()),
            PROFILES_KEY => Some(profiles.into()),
            ADVICE_KEY => Some("a".into()),
            PARSE_KEY => Some("gone".into()),
            _ => None,
        });
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, PrivacyMode::OwnOnly);
        assert_eq!(cfg.advice.as_deref(), Some("a"));
        assert_eq!(cfg.parse, None, "назначение на удалённое подключение снимается");
        assert_eq!(cfg.profiles[0].kind, ProviderKind::Anthropic);
    }

    #[test]
    fn patch_null_clears_assignment_missing_keeps() {
        let p: AiSettingsPatch = serde_json::from_str(r#"{"advice_profile":null}"#).unwrap();
        assert_eq!(p.advice_profile, Some(None));
        assert_eq!(p.parse_profile, None);
        let p: AiSettingsPatch = serde_json::from_str(r#"{"mode":"own_only","parse_profile":"x"}"#).unwrap();
        assert_eq!(p.mode, Some(PrivacyMode::OwnOnly));
        assert_eq!(p.parse_profile, Some(Some("x".into())));
    }
}
