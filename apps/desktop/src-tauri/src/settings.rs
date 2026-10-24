//! Настройки окна почты (режим mail): чтение и частичное изменение, смена пароля, «забыть
//! учётную запись», открытие каталога базы.
//!
//! Значения живут в `sync_state` базы под ключами `desktop.*` (тосты — существующий ключ
//! `desktop.notify_toasts` канала уведомлений). Нет ключа или значение негодное — значение по
//! умолчанию. Синхронизация (`sync.rs`) берёт отсюда глубину тел, квоту кэша и лимит вложения;
//! окно — задержку автопометки, интервал страховочной синхронизации и тему.

use crate::config::{self, InitialConfig};
use crate::notify::{NotifyState, TOASTS_KEY};
use crate::setup::{self, SetupState};
use crate::sync::SyncState;
use crate::{err, AppState, CmdResult};
use aiowa_core::AccountId;
use aiowa_store::Store;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tracing::{info, warn};

pub(crate) const BODY_DAYS_KEY: &str = "desktop.body_days";
pub(crate) const AUTO_READ_KEY: &str = "desktop.auto_read_ms";
pub(crate) const THEME_KEY: &str = "desktop.theme";
pub(crate) const SYNC_INTERVAL_KEY: &str = "desktop.sync_interval_min";
pub(crate) const CACHE_MB_KEY: &str = "desktop.cache_mb";
pub(crate) const MAX_ATTACHMENT_MB_KEY: &str = "desktop.max_attachment_mb";

const MIB: u64 = 1024 * 1024;

/// Глубина загрузки тел, дней (0 — все письма).
const BODY_DAYS: RangeInclusive<u32> = 0..=3650;
/// Задержка автопометки прочитанным, мс (0 — выключено).
const AUTO_READ_MS: RangeInclusive<u32> = 0..=10_000;
/// Интервал страховочной синхронизации, мин.
const SYNC_INTERVAL_MIN: RangeInclusive<u32> = 1..=120;
/// Квота кэша вложений, МБ.
const CACHE_MB: RangeInclusive<u32> = 100..=102_400;
/// Лимит вложения, скачиваемого синхронизацией, МБ.
const MAX_ATTACHMENT_MB: RangeInclusive<u32> = 1..=200;

const THEMES: &[&str] = &["system", "light", "dark"];

/// Настройки в значениях; по умолчанию — прежнее поведение (90 дней, 1,5 с, тосты, 2 мин, 2 ГБ, 25 МБ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prefs {
    pub(crate) body_days: u32,
    pub(crate) auto_read_ms: u32,
    pub(crate) toasts: bool,
    pub(crate) theme: String,
    pub(crate) sync_interval_min: u32,
    pub(crate) cache_mb: u32,
    pub(crate) max_attachment_mb: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { body_days: 90, auto_read_ms: 1500, toasts: true, theme: "system".into(), sync_interval_min: 2, cache_mb: 2048, max_attachment_mb: 25 }
    }
}

fn parse_in(raw: Option<String>, range: &RangeInclusive<u32>, default: u32) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok()).filter(|v| range.contains(v)).unwrap_or(default)
}

impl Prefs {
    /// Из значений по ключам; негодное или отсутствующее значение — по умолчанию.
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        Self {
            body_days: parse_in(get(BODY_DAYS_KEY), &BODY_DAYS, d.body_days),
            auto_read_ms: parse_in(get(AUTO_READ_KEY), &AUTO_READ_MS, d.auto_read_ms),
            // Как в канале уведомлений: выключены только явным `0`.
            toasts: get(TOASTS_KEY).is_none_or(|v| v != "0"),
            theme: get(THEME_KEY).filter(|t| THEMES.contains(&t.as_str())).unwrap_or(d.theme),
            sync_interval_min: parse_in(get(SYNC_INTERVAL_KEY), &SYNC_INTERVAL_MIN, d.sync_interval_min),
            cache_mb: parse_in(get(CACHE_MB_KEY), &CACHE_MB, d.cache_mb),
            max_attachment_mb: parse_in(get(MAX_ATTACHMENT_MB_KEY), &MAX_ATTACHMENT_MB, d.max_attachment_mb),
        }
    }

    /// Из базы; ошибка чтения ключа — значение по умолчанию (с записью в журнал).
    pub(crate) fn load(store: &Store, account: AccountId) -> Self {
        Self::from_lookup(|key| match store.get_state(account, key) {
            Ok(v) => v,
            Err(e) => {
                warn!(key, error = %e, "не удалось прочитать настройку");
                None
            }
        })
    }

    /// Окно загрузки тел для `SyncOptions::body_days`: `None` — все письма.
    pub(crate) fn body_window(&self) -> Option<u32> {
        (self.body_days > 0).then_some(self.body_days)
    }

    pub(crate) fn attachment_cache_bytes(&self) -> u64 {
        u64::from(self.cache_mb) * MIB
    }

    pub(crate) fn max_attachment_bytes(&self) -> u64 {
        u64::from(self.max_attachment_mb) * MIB
    }
}

/// Частичное изменение: заданы только меняемые поля. Неизвестное поле — ошибка (опечатка в окне
/// не должна молча теряться).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsPatch {
    pub(crate) body_days: Option<u32>,
    pub(crate) auto_read_ms: Option<u32>,
    pub(crate) toasts: Option<bool>,
    pub(crate) theme: Option<String>,
    pub(crate) sync_interval_min: Option<u32>,
    #[serde(alias = "cache_mb")]
    pub(crate) attachment_cache_mb: Option<u32>,
    pub(crate) max_attachment_mb: Option<u32>,
}

/// Что записать по проверенному патчу: ключи `sync_state` и тосты (через канал уведомлений).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlannedWrites {
    pub(crate) values: Vec<(&'static str, String)>,
    pub(crate) toasts: Option<bool>,
}

fn check_range(what: &str, v: u32, range: &RangeInclusive<u32>) -> Result<u32, String> {
    if range.contains(&v) { Ok(v) } else { Err(format!("{what}: допустимо от {} до {}, получено {v}", range.start(), range.end())) }
}

pub(crate) fn check_body_days(v: u32) -> Result<u32, String> {
    check_range("Глубина загрузки тел (дней)", v, &BODY_DAYS)
}

/// Проверка всего патча до записи: либо всё годное, либо ошибка и ничего не меняется.
pub(crate) fn plan(patch: &SettingsPatch) -> Result<PlannedWrites, String> {
    let mut out = PlannedWrites { toasts: patch.toasts, ..PlannedWrites::default() };
    if let Some(v) = patch.body_days {
        out.values.push((BODY_DAYS_KEY, check_body_days(v)?.to_string()));
    }
    if let Some(v) = patch.auto_read_ms {
        out.values.push((AUTO_READ_KEY, check_range("Задержка автопометки (мс)", v, &AUTO_READ_MS)?.to_string()));
    }
    if let Some(t) = &patch.theme {
        if !THEMES.contains(&t.as_str()) {
            return Err(format!("Тема: допустимо system, light или dark, получено {t}"));
        }
        out.values.push((THEME_KEY, t.clone()));
    }
    if let Some(v) = patch.sync_interval_min {
        out.values.push((SYNC_INTERVAL_KEY, check_range("Интервал синхронизации (мин)", v, &SYNC_INTERVAL_MIN)?.to_string()));
    }
    if let Some(v) = patch.attachment_cache_mb {
        out.values.push((CACHE_MB_KEY, check_range("Кэш вложений (МБ)", v, &CACHE_MB)?.to_string()));
    }
    if let Some(v) = patch.max_attachment_mb {
        out.values.push((MAX_ATTACHMENT_MB_KEY, check_range("Лимит вложения (МБ)", v, &MAX_ATTACHMENT_MB)?.to_string()));
    }
    Ok(out)
}

/// Блок мастера на первом старте в режиме почты: глубина тел — в базу, затем блок убирается из
/// `config.json`. Не записалось в базу — блок остаётся до следующего старта.
pub(crate) fn apply_initial(store: &Store, account: AccountId, config_dir: &Path, initial: InitialConfig) {
    let days = initial.body_days.clamp(*BODY_DAYS.start(), *BODY_DAYS.end());
    if let Err(e) = store.set_state(account, BODY_DAYS_KEY, Some(&days.to_string())) {
        warn!(error = %e, "не удалось применить настройки первого запуска");
        return;
    }
    info!(body_days = days, start_sync = initial.start_sync, "настройки первого запуска применены");
    if let Err(e) = config::clear_initial(config_dir) {
        warn!(error = %e, "не удалось убрать настройки первого запуска из конфигурации");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SettingsDto {
    host: String,
    user: String,
    /// 0 — все письма.
    body_days: u32,
    /// 0 — автопометка выключена.
    auto_read_ms: u32,
    toasts: bool,
    /// `system` | `light` | `dark`.
    theme: String,
    sync_interval_min: u32,
    attachment_cache_mb: u32,
    max_attachment_mb: u32,
    db_path: String,
    db_size_bytes: u64,
    cache_size_bytes: u64,
    password_saved: bool,
    /// Учётная запись задана переменными окружения: «забыть» из окна нельзя.
    dev_env: bool,
    config_path: String,
}

/// Снимок настроек вместе с размерами на диске и наличием пароля. Блокирующий: диск и
/// диспетчер учётных данных.
fn snapshot(store: &Store, account: AccountId, host: &str, user: &str, dev_env: bool, config_path: &Path) -> SettingsDto {
    let p = Prefs::load(store, account);
    SettingsDto {
        host: host.to_string(),
        user: user.to_string(),
        body_days: p.body_days,
        auto_read_ms: p.auto_read_ms,
        toasts: p.toasts,
        theme: p.theme,
        sync_interval_min: p.sync_interval_min,
        attachment_cache_mb: p.cache_mb,
        max_attachment_mb: p.max_attachment_mb,
        db_path: store.path().display().to_string(),
        db_size_bytes: store.disk_size_bytes(),
        cache_size_bytes: store.attachment_cache_bytes(),
        password_saved: setup::has_password(user),
        dev_env,
        config_path: config_path.display().to_string(),
    }
}

async fn snapshot_async(state: &AppState, setup: &SetupState) -> CmdResult<SettingsDto> {
    let (store, account, host, user, dev_env, config_path) = (state.store.clone(), state.account, state.host.clone(), state.user.clone(), setup.dev_env, setup.config_path());
    tauri::async_runtime::spawn_blocking(move || snapshot(&store, account, &host, &user, dev_env, &config_path)).await.map_err(err)
}

// --- команды Tauri -----------------------------------------------------------

#[tauri::command]
pub(crate) async fn settings_get(state: State<'_, AppState>, setup: State<'_, SetupState>) -> CmdResult<SettingsDto> {
    snapshot_async(&state, &setup).await
}

/// Частичное изменение настроек: сначала проверка всего патча, затем запись. Возвращает новый
/// снимок и рассылает его окну событием `settings-changed`.
#[tauri::command]
pub(crate) async fn settings_set(patch: SettingsPatch, app: AppHandle, state: State<'_, AppState>, setup: State<'_, SetupState>, notify: State<'_, Arc<NotifyState>>) -> CmdResult<SettingsDto> {
    let planned = plan(&patch)?;
    for (key, value) in &planned.values {
        state.store.set_state(state.account, key, Some(value)).map_err(|e| format!("Не удалось сохранить настройку: {e}"))?;
    }
    if let Some(enabled) = planned.toasts {
        notify.set_toasts(&app, enabled)?;
    }
    if !planned.values.is_empty() || planned.toasts.is_some() {
        info!(changed = ?planned.values.iter().map(|(k, _)| *k).collect::<Vec<_>>(), toasts = ?planned.toasts, "настройки изменены");
    }
    let dto = snapshot_async(&state, &setup).await?;
    if let Err(e) = app.emit("settings-changed", dto.clone()) {
        warn!(error = %e, "не удалось отправить событие settings-changed");
    }
    Ok(dto)
}

/// Смена пароля: проверка входа с новым паролем (отдельная сессия); удачно — пароль в
/// диспетчер учётных данных и сброс транспорта процесса (следующее обращение войдёт с новым
/// паролем). Неудачно — ошибка, сохранённый пароль не меняется.
#[tauri::command]
pub(crate) async fn settings_change_password(password: String, state: State<'_, AppState>, setup: State<'_, SetupState>, sync: State<'_, Arc<SyncState>>) -> CmdResult<()> {
    let password = SecretString::from(password);
    if password.expose_secret().is_empty() {
        return Err("Введите пароль".into());
    }
    let check = setup::check_login_locked(&setup, &state.host, &state.user, password.clone()).await;
    if !check.ok {
        return Err(check.error.unwrap_or_else(|| "Вход не удался".into()));
    }
    let user = state.user.clone();
    tauri::async_runtime::spawn_blocking(move || setup::save_password(&user, &password)).await.map_err(err)??;
    sync.reset_transport().await;
    info!(user = %state.user, "пароль заменён");
    Ok(())
}

/// Забыть учётную запись: пароль из диспетчера учётных данных и учётная запись из `config.json`
/// удаляются (база остаётся на диске), затем перезапуск в мастер; выход из сессии на сервере
/// делает обработчик выхода. Окно спрашивает подтверждение до вызова.
#[tauri::command]
pub(crate) async fn settings_forget_account(app: AppHandle, state: State<'_, AppState>, setup: State<'_, SetupState>) -> CmdResult<()> {
    if setup.dev_env {
        return Err("Учётная запись задана переменными окружения AIOWA_HOST и AIOWA_USER: забыть её из окна нельзя".into());
    }
    let user = state.user.clone();
    let dir = setup.config_dir.clone();
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<()> {
        config::update(&dir, |c| {
            c.account = None;
            c.initial = None;
        })
        .map_err(|e| format!("Не удалось записать конфигурацию {}: {e}", config::config_path(&dir).display()))?;
        // Конфиг уже без учётной записи; повторное «Забыть» снова попробует удалить пароль.
        setup::delete_password(&user).map_err(|e| format!("Учётная запись убрана из конфигурации, но пароль остался. {e}. Повторите или удалите запись «AIOWA» в диспетчере учётных данных Windows"))
    })
    .await
    .map_err(err)??;
    info!(db = %state.store.path().display(), "учётная запись забыта, база осталась на диске");
    setup::restart(&app);
    Ok(())
}

/// Открыть каталог базы в Проводнике (только его).
#[tauri::command]
pub(crate) async fn settings_open_data_dir(state: State<'_, AppState>) -> CmdResult<()> {
    let dir = state.store.path().parent().map(Path::to_path_buf).ok_or_else(|| "Каталог базы не определён".to_string())?;
    tauri::async_runtime::spawn_blocking(move || open::that_detached(&dir)).await.map_err(err)?.map_err(|e| {
        warn!(error = %e, "Проводник не открыл каталог базы");
        "Не удалось открыть каталог базы".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn defaults_match_previous_behaviour() {
        let p = Prefs::from_lookup(|_| None);
        assert_eq!(p, Prefs::default());
        let base = aiowa_sync::SyncOptions::default();
        assert_eq!(p.body_window(), base.body_days);
        assert_eq!(p.attachment_cache_bytes(), base.attachment_cache_bytes);
        assert_eq!(p.max_attachment_bytes(), base.max_attachment_bytes);
        assert_eq!((p.auto_read_ms, p.sync_interval_min, p.toasts, p.theme.as_str()), (1500, 2, true, "system"));
    }

    #[test]
    fn stored_values_are_parsed_tolerantly() {
        let p = Prefs::from_lookup(lookup(&[
            (BODY_DAYS_KEY, "0"),
            (AUTO_READ_KEY, "5000"),
            (TOASTS_KEY, "0"),
            (THEME_KEY, "dark"),
            (SYNC_INTERVAL_KEY, "999"),
            (CACHE_MB_KEY, "мусор"),
            (MAX_ATTACHMENT_MB_KEY, " 50 "),
        ]));
        assert_eq!(p.body_days, 0);
        assert_eq!(p.body_window(), None, "0 — все письма");
        assert_eq!(p.auto_read_ms, 5000);
        assert!(!p.toasts);
        assert_eq!(p.theme, "dark");
        assert_eq!(p.sync_interval_min, 2, "вне диапазона — по умолчанию");
        assert_eq!(p.cache_mb, 2048, "не число — по умолчанию");
        assert_eq!(p.max_attachment_bytes(), 50 * MIB);
        let q = Prefs::from_lookup(lookup(&[(THEME_KEY, "neon"), (TOASTS_KEY, "1")]));
        assert_eq!(q.theme, "system");
        assert!(q.toasts);
    }

    #[test]
    fn patch_ranges_are_checked() {
        let ok = |p: SettingsPatch| plan(&p).is_ok();
        assert!(ok(SettingsPatch { body_days: Some(0), ..Default::default() }));
        assert!(ok(SettingsPatch { body_days: Some(3650), ..Default::default() }));
        assert!(!ok(SettingsPatch { body_days: Some(3651), ..Default::default() }));
        assert!(ok(SettingsPatch { auto_read_ms: Some(0), ..Default::default() }));
        assert!(ok(SettingsPatch { auto_read_ms: Some(10_000), ..Default::default() }));
        assert!(!ok(SettingsPatch { auto_read_ms: Some(10_001), ..Default::default() }));
        assert!(!ok(SettingsPatch { sync_interval_min: Some(0), ..Default::default() }));
        assert!(ok(SettingsPatch { sync_interval_min: Some(1), ..Default::default() }));
        assert!(ok(SettingsPatch { sync_interval_min: Some(120), ..Default::default() }));
        assert!(!ok(SettingsPatch { sync_interval_min: Some(121), ..Default::default() }));
        assert!(!ok(SettingsPatch { attachment_cache_mb: Some(99), ..Default::default() }));
        assert!(ok(SettingsPatch { attachment_cache_mb: Some(100), ..Default::default() }));
        assert!(ok(SettingsPatch { attachment_cache_mb: Some(102_400), ..Default::default() }));
        assert!(!ok(SettingsPatch { attachment_cache_mb: Some(102_401), ..Default::default() }));
        assert!(!ok(SettingsPatch { max_attachment_mb: Some(0), ..Default::default() }));
        assert!(ok(SettingsPatch { max_attachment_mb: Some(200), ..Default::default() }));
        assert!(!ok(SettingsPatch { max_attachment_mb: Some(201), ..Default::default() }));
        assert!(ok(SettingsPatch { theme: Some("light".into()), ..Default::default() }));
        assert!(!ok(SettingsPatch { theme: Some("Light".into()), ..Default::default() }));
        // Одно негодное поле — отказ всему патчу.
        assert!(plan(&SettingsPatch { body_days: Some(30), max_attachment_mb: Some(500), ..Default::default() }).is_err());
        let p = plan(&SettingsPatch { body_days: Some(30), toasts: Some(false), attachment_cache_mb: Some(4096), ..Default::default() }).unwrap();
        assert_eq!(p.values, vec![(BODY_DAYS_KEY, "30".to_string()), (CACHE_MB_KEY, "4096".to_string())]);
        assert_eq!(p.toasts, Some(false));
        assert_eq!(plan(&SettingsPatch::default()).unwrap(), PlannedWrites::default());
    }

    #[test]
    fn patch_json_shape() {
        let p: SettingsPatch = serde_json::from_str(r#"{"body_days":365,"theme":"dark","cache_mb":1024}"#).unwrap();
        assert_eq!(p, SettingsPatch { body_days: Some(365), theme: Some("dark".into()), attachment_cache_mb: Some(1024), ..Default::default() });
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"bodyDays":365}"#).is_err(), "неизвестное поле — ошибка");
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"body_days":-1}"#).is_err());
    }

    #[test]
    fn initial_block_is_applied_once() {
        let dir = std::env::temp_dir().join(format!("aiowa-desktop-settings-initial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("db").join("mail.db")).unwrap();
        let account = store.account("mail.x.ru", "ivanov@x.ru").unwrap();
        let cfg_dir = dir.join("cfg");
        let cfg = config::AppConfig {
            account: Some(config::AccountConfig { host: "mail.x.ru".into(), user: "ivanov@x.ru".into() }),
            initial: Some(InitialConfig { body_days: 365, start_sync: false }),
            ..config::AppConfig::default()
        };
        config::save(&cfg_dir, &cfg).unwrap();
        let boot = match config::resolve(&config::EnvAccount::default(), config::load(&cfg_dir).as_ref()) {
            config::Boot::Mail(m) => m,
            config::Boot::Setup => panic!("ожидался режим почты"),
        };
        assert_eq!(boot.initial, Some(InitialConfig { body_days: 365, start_sync: false }));
        apply_initial(&store, account, &cfg_dir, boot.initial.unwrap());
        assert_eq!(Prefs::load(&store, account).body_days, 365);
        let after = config::load(&cfg_dir).unwrap();
        assert_eq!(after.initial, None, "блок убран из файла");
        assert_eq!(after.account, cfg.account, "учётная запись осталась");
        match config::resolve(&config::EnvAccount::default(), Some(&after)) {
            config::Boot::Mail(m) => assert_eq!(m.initial, None, "на следующем старте применять нечего"),
            config::Boot::Setup => panic!("ожидался режим почты"),
        }
        // «Всё» (0) тоже применяется.
        apply_initial(&store, account, &cfg_dir, InitialConfig { body_days: 0, start_sync: true });
        assert_eq!(Prefs::load(&store, account).body_window(), None);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
