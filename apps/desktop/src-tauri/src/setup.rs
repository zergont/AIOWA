//! Режим приложения и мастер первого запуска: `app_mode`, проверка входа (`setup_test_login`),
//! сохранение учётной записи (`setup_save`). Команды работают в обоих режимах.
//!
//! Режим первого запуска — приложение без базы и без состояний почты: окно показывает только
//! мастер. Мастер сохраняет пароль в диспетчере учётных данных Windows (служба `AIOWA`, ключ =
//! логин, как `aiowa set-password`), пишет `config.json` с блоком `initial` и перезапускает
//! приложение — оно поднимается в режиме почты с новой учётной записью.
//!
//! Проверка входа — отдельная сессия (не транспорт процесса): вход и два лёгких запроса
//! (иерархия папок и сведения о ящике, `sessiondata.ashx`) — не дольше 60 с, затем выход
//! (всегда, с отдельным пределом 5 с). Пароль живёт только в `SecretString` и в журнал не
//! попадает. Тексты ошибок — фиксированные фразы для пользователя.
//!
//! Одна база на ящик (`mailbox.rs`): после удачной проверки мастер ищет на диске базы этого
//! ящика и показывает их на шаге «Синхронизация» — «Продолжить с этой базой» (по умолчанию) или
//! «Начать новую» (каталог из адреса ящика). Выбор записывается в `config.json` (`db_path`).

use crate::config::{self, InitialConfig, MailBoot};
use crate::mailbox::{self, MatchKind};
use crate::ondemand::describe_transport;
use crate::sync::KEYRING_SERVICE;
use crate::CmdResult;
use aiowa_core::FolderChange;
use aiowa_transport::{Credentials, MailTransport, TransportError};
use aiowa_transport_owa::mailbox::MailboxInfo;
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, State};
use tracing::{info, warn};

/// Предел входа и запроса иерархии вместе; выход в него не входит.
const LOGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Предел выхода из проверочной сессии (после основного срока, даже если тот истёк).
const LOGOFF_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// Учётной записи нет: окно показывает мастер.
    Setup,
    Mail,
}

/// Режим процесса; управляется Tauri в обоих режимах.
pub(crate) struct SetupState {
    pub(crate) mode: Mode,
    pub(crate) host: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) config_dir: PathBuf,
    /// Учётная запись задана переменными `AIOWA_HOST`/`AIOWA_USER` (режим разработки).
    pub(crate) dev_env: bool,
    /// Запускать ли синхронизацию сразу при старте окна: `false` только на первом старте после
    /// мастера со снятой галочкой «Начать синхронизацию сразу».
    pub(crate) autostart_sync: bool,
    /// Учётная запись настроена, но база не открылась: текст ошибки для мастера.
    pub(crate) boot_error: Option<String>,
    /// Одна проверка входа за раз: двойной клик не открывает на сервере две сессии.
    login_lock: tokio::sync::Mutex<()>,
    /// Базы, предложенные последней удачной проверкой входа: `setup_save` принимает только их.
    db_choices: Mutex<Option<DbChoices>>,
}

/// Из каких баз мастер даёт выбрать (по последней удачной проверке входа).
#[derive(Debug, Clone)]
struct DbChoices {
    host: String,
    user: String,
    /// Найденные базы этого ящика.
    existing: Vec<PathBuf>,
    /// Где будет новая база.
    new_path: PathBuf,
}

impl SetupState {
    pub(crate) fn first_run(config_dir: PathBuf) -> Self {
        Self { mode: Mode::Setup, host: None, user: None, config_dir, dev_env: false, autostart_sync: false, boot_error: None, login_lock: tokio::sync::Mutex::new(()), db_choices: Mutex::new(None) }
    }

    pub(crate) fn mail(boot: &MailBoot, config_dir: PathBuf) -> Self {
        Self {
            mode: Mode::Mail,
            host: Some(boot.host.clone()),
            user: Some(boot.user.clone()),
            config_dir,
            dev_env: boot.dev_env,
            autostart_sync: boot.initial.is_none_or(|i| i.start_sync),
            boot_error: None,
            login_lock: tokio::sync::Mutex::new(()),
            db_choices: Mutex::new(None),
        }
    }

    /// База учётной записи не открылась: режим мастера с текстом ошибки; хост и логин —
    /// подстановкой в поля мастера.
    pub(crate) fn boot_failed(boot: &MailBoot, config_dir: PathBuf, error: String) -> Self {
        Self { host: Some(boot.host.clone()), user: Some(boot.user.clone()), dev_env: boot.dev_env, boot_error: Some(error), ..Self::first_run(config_dir) }
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        config::config_path(&self.config_dir)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AppModeDto {
    mode: Mode,
    host: Option<String>,
    user: Option<String>,
    config_path: String,
    dev_env: bool,
    autostart_sync: bool,
    /// Режим setup из-за того, что база не открылась (учётная запись в конфиге есть).
    boot_error: Option<String>,
}

/// Итог проверки входа. `ok = false` — текст в `error`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LoginCheckDto {
    pub(crate) ok: bool,
    /// Сборка сервера с названием версии Exchange, если сервер её сообщил.
    pub(crate) server_version: Option<String>,
    /// Почтовых папок в ящике.
    pub(crate) folders: Option<i64>,
    pub(crate) error: Option<String>,
    /// Адрес сервера после нормализации (`host[:port]`), с которым шла проверка.
    pub(crate) host: Option<String>,
    /// Ящик, в который выполнен вход, если сервер сообщил (`sessiondata.ashx`).
    pub(crate) mailbox: Option<MailboxDto>,
    /// Базы этого ящика на диске: сначала полнее. Мастер предлагает продолжить с первой.
    pub(crate) stores: Vec<CandidateDto>,
    /// Путь новой базы, если начать с нуля (каталог из адреса ящика).
    pub(crate) new_db_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct MailboxDto {
    pub(crate) email: Option<String>,
    pub(crate) display_name: Option<String>,
}

/// Найденная база ящика для шага «Синхронизация».
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CandidateDto {
    path: String,
    /// Почему база подходит: `guid` — тот же ящик точно, `email` — совпал адрес, `login` — логин.
    kind: MatchKind,
    items: i64,
    last_sync_ms: Option<i64>,
    /// База с журналами и кэшем вложений.
    total_bytes: u64,
}

impl From<&mailbox::Candidate> for CandidateDto {
    fn from(c: &mailbox::Candidate) -> Self {
        Self { path: c.path.display().to_string(), kind: c.kind, items: c.summary.items, last_sync_ms: c.summary.last_sync_ms, total_bytes: c.summary.total_bytes() }
    }
}

impl LoginCheckDto {
    fn failed(host: Option<String>, error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), host, ..Self::default() }
    }
}

// --- пароль в диспетчере учётных данных ---------------------------------------------------

fn keyring_entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| format!("Диспетчер учётных данных Windows недоступен: {e}"))
}

/// Сохранить пароль (заменяет прежний). Блокирующий вызов.
pub(crate) fn save_password(user: &str, password: &SecretString) -> Result<(), String> {
    keyring_entry(user)?.set_password(password.expose_secret()).map_err(|e| format!("Не удалось сохранить пароль в диспетчере учётных данных Windows: {e}"))
}

/// Удалить пароль; если его нет — не ошибка. Блокирующий вызов.
pub(crate) fn delete_password(user: &str) -> Result<(), String> {
    match keyring_entry(user)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Не удалось удалить пароль из диспетчера учётных данных Windows: {e}")),
    }
}

/// Есть ли сохранённый пароль. Значение сразу отбрасывается. Блокирующий вызов.
pub(crate) fn has_password(user: &str) -> bool {
    keyring_entry(user).is_ok_and(|e| e.get_password().map(SecretString::from).is_ok())
}

// --- проверка входа ---------------------------------------------------------------------

/// Версия Exchange по сборке сервера: `15.2.1748.39` → `Exchange 2019 (15.2.1748.39)`.
pub(crate) fn describe_build(build: &str) -> String {
    let mut parts = build.split('.');
    let major = parts.next().and_then(|p| p.trim().parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.trim().parse::<u32>().ok());
    let name = match (major, minor) {
        (Some(15), Some(2)) => Some("Exchange 2019"),
        (Some(15), Some(1)) => Some("Exchange 2016"),
        (Some(15), Some(0)) => Some("Exchange 2013"),
        (Some(14), _) => Some("Exchange 2010"),
        (Some(8), _) => Some("Exchange 2007"),
        _ => None,
    };
    match name {
        Some(n) => format!("{n} ({build})"),
        None => build.to_string(),
    }
}

/// Текст цепочки ошибки (`source()` до конца) в нижнем регистре.
fn error_chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(" | ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out.to_lowercase()
}

/// Сетевая ошибка входа по тексту её цепочки (reqwest → hyper → Schannel/ОС, сообщения ОС бывают
/// по-русски). Адреса и имя сервера из текста убираются, чтобы `ssl.example.ru` не сошёл за TLS.
pub(crate) fn classify_network(chain: &str, host: &str) -> &'static str {
    let mut text = chain.to_lowercase();
    let host = host.to_lowercase();
    if !host.is_empty() {
        text = text.replace(&host, " ");
    }
    let text: String = text.split_whitespace().filter(|w| !w.contains("://")).collect::<Vec<_>>().join(" ");
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["certificate", "сертификат", "tls", "ssl", "schannel", "sec_e_", "handshake", "os error -2146", "общего алгоритма", "algorithm"]) {
        "Не удалось установить защищённое соединение (TLS): сертификат сервера не принят или сервер не поддерживает нужный протокол"
    } else if has(&["dns error", "no such host", "failed to lookup", "name or service not known", "хост неизвестен", "os error 11001", "os error 11004"]) {
        "Сервер не найден: проверьте адрес (имя не находится в DNS)"
    } else if has(&["refused", "отверг", "os error 10061"]) {
        "Сервер недоступен: подключение отклонено (проверьте адрес и порт)"
    } else if has(&["timed out", "timeout", "os error 10060", "отклик", "таймаут"]) {
        "Сервер не отвечает: проверьте адрес и подключение к сети (VPN)"
    } else {
        "Сервер недоступен: нет связи"
    }
}

/// Текст ошибки входа для мастера и смены пароля.
pub(crate) fn describe_login_error(e: &TransportError, host: &str) -> String {
    match e {
        TransportError::AuthFailed(_) => "Неверный логин или пароль: сервер отклонил вход".into(),
        TransportError::SessionLimit => "Исчерпан лимит сессий на ящик: закройте лишние окна OWA или подождите несколько минут".into(),
        TransportError::Network(inner) => classify_network(&error_chain_text(inner.as_ref()), host).into(),
        TransportError::Protocol { status: 404, .. } => "По этому адресу нет OWA (HTTP 404): проверьте адрес сервера".into(),
        TransportError::Protocol { op, status: 0, .. } if op == "config" => "Не похоже на адрес сервера".into(),
        TransportError::SessionExpired | TransportError::CsrfRejected => "Сервер отклонил сессию сразу после входа: повторите попытку".into(),
        other => describe_transport(other),
    }
}

/// Отдельная сессия: вход, иерархия папок, выход. `host` уже нормализован. Ошибки — в DTO.
///
/// Срок `LOGIN_CHECK_TIMEOUT` общий для входа и запроса иерархии; выход — вне его, со своим
/// пределом: сессия, открытая входом, закрывается и тогда, когда иерархия не уложилась в срок
/// (иначе каждая повисшая проверка оставляла бы на сервере живую сессию). Остаётся узкая щель:
/// срок может истечь внутри `connect` уже после удачного auth.owa (на редиректах) — транспорта
/// ещё нет, выйти нечем; закрыть её можно, только если `connect` отдаст транспорт до входа.
pub(crate) async fn check_login(host: &str, user: &str, password: SecretString) -> LoginCheckDto {
    check_login_with_mailbox(host, user, password).await.0
}

/// То же, плюс сведения о ящике (адрес и GUID) для поиска его базы: запрашиваются после
/// иерархии в том же сроке; не пришли — `None`, проверка входа от этого не проваливается.
async fn check_login_with_mailbox(host: &str, user: &str, password: SecretString) -> (LoginCheckDto, Option<MailboxInfo>) {
    let creds = Credentials { host: host.to_string(), user: user.to_string(), password };
    let timed_out = || {
        warn!(host, "проверка входа не уложилась в {} с", LOGIN_CHECK_TIMEOUT.as_secs());
        LoginCheckDto::failed(Some(host.to_string()), format!("Сервер не ответил за {} с: проверьте адрес и подключение к сети", LOGIN_CHECK_TIMEOUT.as_secs()))
    };
    let failed = |e: TransportError| {
        warn!(host, error = %e, "проверка входа не удалась");
        LoginCheckDto::failed(Some(host.to_string()), describe_login_error(&e, host))
    };
    let deadline = tokio::time::Instant::now() + LOGIN_CHECK_TIMEOUT;
    let t = match tokio::time::timeout_at(deadline, OwaTransport::connect(creds, OwaConfig::default())).await {
        Err(_) => return (timed_out(), None),
        Ok(Err(e)) => return (failed(e), None),
        Ok(Ok(t)) => t,
    };
    let hierarchy = tokio::time::timeout_at(deadline, t.sync_folder_hierarchy(None)).await;
    let info = match &hierarchy {
        Ok(Ok(_)) => match tokio::time::timeout_at(deadline, t.mailbox_info()).await {
            Ok(Ok(info)) => Some(info),
            Ok(Err(e)) => {
                warn!(error = %e, "сведения о ящике при проверке входа не получены");
                None
            }
            Err(_) => {
                warn!("сведения о ящике при проверке входа не уложились в срок");
                None
            }
        },
        _ => None,
    };
    let build = t.server_info().build;
    // Выход — всегда, при любом исходе запроса иерархии.
    match tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "выход из проверочной сессии не удался"),
        Err(_) => warn!("выход из проверочной сессии не уложился в {} с", LOGOFF_TIMEOUT.as_secs()),
    }
    match hierarchy {
        Err(_) => (timed_out(), None),
        Ok(Err(e)) => (failed(e), None),
        Ok(Ok(changes)) => {
            let folders = changes.changes.iter().filter(|c| matches!(c, FolderChange::Create(f) if f.is_mail())).count() as i64;
            info!(host, build = build.as_deref().unwrap_or("?"), folders, mailbox_email = info.as_ref().is_some_and(|i| i.email.is_some()), mailbox_guid = info.as_ref().is_some_and(|i| i.mailbox_guid.is_some()), "проверка входа прошла");
            let mailbox = info.as_ref().map(|i| MailboxDto { email: i.email.clone(), display_name: i.display_name.clone() });
            (LoginCheckDto { ok: true, server_version: build.as_deref().map(describe_build), folders: Some(folders), error: None, host: Some(host.to_string()), mailbox, ..LoginCheckDto::default() }, info)
        }
    }
}

/// Проверка входа под замком процесса (одна за раз).
pub(crate) async fn check_login_locked(setup: &SetupState, host: &str, user: &str, password: SecretString) -> LoginCheckDto {
    let _one = setup.login_lock.lock().await;
    check_login(host, user, password).await
}

/// Базы ящика на диске и путь новой базы: базы этого сервера под `%LOCALAPPDATA%\AIOWA` и путь
/// из `config.json` (после «Забыть учётную запись» он остаётся), если там есть база. Блокирующий.
fn find_db_choices(config_dir: &Path, host: &str, user: &str, info: &MailboxInfo) -> (Vec<mailbox::Candidate>, PathBuf) {
    let root = aiowa_store::identity::data_root();
    let mut paths = mailbox::host_databases(&root, host);
    if let Some(p) = config::load(config_dir).and_then(|c| c.db_path).filter(|p| p.is_file())
        && !paths.iter().any(|x| crate::data::same_path(x, &p))
    {
        paths.push(p);
    }
    let found = mailbox::find_candidates(&paths, host, user, info);
    info!(databases = paths.len(), matching = found.len(), by_guid = found.iter().filter(|c| c.kind == MatchKind::Guid).count(), "поиск базы ящика");
    (found, mailbox::new_db_path(&root, host, user, info))
}

/// Какой путь к базе записать в конфиг: выбранный в мастере (только из предложенных последней
/// проверкой входа с этими сервером и логином) или новый. Проверки не было — путь по логину,
/// как раньше.
fn chosen_db_path(choices: Option<&DbChoices>, host: &str, user: &str, wanted: Option<&str>) -> Result<PathBuf, String> {
    let choices = choices.filter(|c| c.host == host && c.user == user);
    match (choices, wanted) {
        (Some(c), Some(p)) => {
            let p = Path::new(p);
            c.existing.iter().chain(std::iter::once(&c.new_path)).find(|x| crate::data::same_path(x, p)).cloned().ok_or_else(|| "Выбранной базы нет среди найденных: проверьте вход ещё раз".to_string())
        }
        (Some(c), None) => Ok(c.new_path.clone()),
        (None, Some(_)) => Err("Сначала проверьте вход: база выбирается после него".into()),
        (None, None) => Ok(config::default_db_path(host, user)),
    }
}

/// Перезапуск процесса через штатный выход: сработает `RunEvent::Exit` (канал уведомлений,
/// отложенные действия, logoff), затем Tauri запустит процесс заново.
pub(crate) fn restart(app: &AppHandle) {
    info!("перезапуск приложения");
    app.request_restart();
}

/// Записать учётную запись, путь к базе и блок первого запуска в конфиг.
pub(crate) fn write_account(dir: &Path, host: &str, user: &str, db_path: &Path, initial: InitialConfig) -> std::io::Result<()> {
    config::update(dir, |c| {
        c.account = Some(config::AccountConfig { host: host.to_string(), user: user.to_string() });
        c.db_path = Some(db_path.to_path_buf());
        c.initial = Some(initial);
    })
    .map(|_| ())
}

// --- команды Tauri -----------------------------------------------------------

#[tauri::command]
pub(crate) fn app_mode(setup: State<'_, SetupState>) -> AppModeDto {
    AppModeDto {
        mode: setup.mode,
        host: setup.host.clone(),
        user: setup.user.clone(),
        config_path: setup.config_path().display().to_string(),
        dev_env: setup.dev_env,
        autostart_sync: setup.autostart_sync,
        boot_error: setup.boot_error.clone(),
    }
}

/// Адрес сервера после нормализации — подсказка мастера под полем ввода.
#[tauri::command]
pub(crate) fn setup_normalize_host(host: String) -> CmdResult<String> {
    config::normalize_host(&host)
}

/// Проверка входа с введёнными данными. Ошибка входа — не ошибка команды, а `ok = false`.
#[tauri::command]
pub(crate) async fn setup_test_login(host: String, user: String, password: String, setup: State<'_, SetupState>) -> CmdResult<LoginCheckDto> {
    let password = SecretString::from(password);
    let host = match config::normalize_host(&host) {
        Ok(h) => h,
        Err(e) => return Ok(LoginCheckDto::failed(None, e)),
    };
    let user = match config::normalize_user(&user) {
        Ok(u) => u,
        Err(e) => return Ok(LoginCheckDto::failed(Some(host), e)),
    };
    if password.expose_secret().is_empty() {
        return Ok(LoginCheckDto::failed(Some(host), "Введите пароль"));
    }
    let (mut dto, info) = {
        let _one = setup.login_lock.lock().await;
        check_login_with_mailbox(&host, &user, password).await
    };
    if !dto.ok {
        return Ok(dto);
    }
    // Базы этого ящика на диске: шаг «Синхронизация» предложит продолжить с найденной.
    let (dir, h, u) = (setup.config_dir.clone(), host.clone(), user.clone());
    let info = info.unwrap_or_default();
    let (found, new_path) = tauri::async_runtime::spawn_blocking(move || find_db_choices(&dir, &h, &u, &info)).await.map_err(crate::err)?;
    dto.stores = found.iter().map(CandidateDto::from).collect();
    dto.new_db_path = Some(new_path.display().to_string());
    *setup.db_choices.lock().unwrap_or_else(|p| p.into_inner()) = Some(DbChoices { host, user, existing: found.into_iter().map(|c| c.path).collect(), new_path });
    Ok(dto)
}

/// Завершение мастера: пароль — в диспетчер учётных данных, учётная запись, путь к базе и
/// настройки первого старта — в `config.json`, затем перезапуск в режим почты. `db_path` — одна
/// из баз, найденных проверкой входа («Продолжить с этой базой»); `None` — новая база.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn setup_save(host: String, user: String, password: String, body_days: u32, start_sync: bool, db_path: Option<String>, app: AppHandle, setup: State<'_, SetupState>) -> CmdResult<()> {
    let password = SecretString::from(password);
    let host = config::normalize_host(&host)?;
    let user = config::normalize_user(&user)?;
    if password.expose_secret().is_empty() {
        return Err("Введите пароль".into());
    }
    crate::settings::check_body_days(body_days)?;
    let db = chosen_db_path(setup.db_choices.lock().unwrap_or_else(|p| p.into_inner()).as_ref(), &host, &user, db_path.as_deref())?;
    let existing = db.is_file();
    let dir = setup.config_dir.clone();
    let (h, u) = (host.clone(), user.clone());
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<()> {
        save_password(&u, &password)?;
        write_account(&dir, &h, &u, &db, InitialConfig { body_days, start_sync }).map_err(|e| format!("Не удалось записать конфигурацию {}: {e}", config::config_path(&dir).display()))
    })
    .await
    .map_err(crate::err)??;
    info!(host, user, body_days, start_sync, existing_db = existing, "учётная запись сохранена");
    restart(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_build_names() {
        assert_eq!(describe_build("15.2.1748.39"), "Exchange 2019 (15.2.1748.39)");
        assert_eq!(describe_build("15.1.2507.6"), "Exchange 2016 (15.1.2507.6)");
        assert_eq!(describe_build("15.0.1497.2"), "Exchange 2013 (15.0.1497.2)");
        assert_eq!(describe_build("14.3.123.4"), "Exchange 2010 (14.3.123.4)");
        assert_eq!(describe_build("99.1"), "99.1");
        assert_eq!(describe_build("непонятно"), "непонятно");
    }

    #[test]
    fn network_errors_are_classified() {
        let tls = "error sending request for url (https://mail.x.ru/owa/auth/logon.aspx) | client error (connect) | the certificate chain was issued by an authority that is not trusted. (os error -2146893019)";
        assert!(classify_network(tls, "mail.x.ru").contains("TLS"));
        let tls_ru = "error sending request | Цепочка сертификатов выпущена центром сертификации, не имеющим доверия";
        assert!(classify_network(tls_ru, "mail.x.ru").contains("TLS"));
        let dns = "error sending request for url (https://ssl.x.ru/owa/auth/logon.aspx) | client error (connect) | dns error | failed to lookup address information: no such host is known. (os error 11001)";
        assert!(classify_network(dns, "ssl.x.ru").contains("не найден"), "имя сервера с «ssl» не путает классификацию");
        let refused = "error sending request | tcp connect error | подключение не установлено, т.к. конечный компьютер отверг запрос на подключение. (os error 10061)";
        assert!(classify_network(refused, "h").contains("отклонено"));
        let timeout = "error sending request | operation timed out";
        assert!(classify_network(timeout, "h").contains("не отвечает"));
        assert_eq!(classify_network("something else", "h"), "Сервер недоступен: нет связи");
    }

    #[test]
    fn db_choice_is_limited_to_found_databases() {
        let choices = DbChoices { host: "mail.x.ru".into(), user: "savvidi".into(), existing: vec![PathBuf::from(r"C:\A\mail.x.ru\savvidi_ngenergo\mail.db")], new_path: PathBuf::from(r"C:\A\mail.x.ru\savvidi_x.ru\mail.db") };
        let c = Some(&choices);
        assert_eq!(chosen_db_path(c, "mail.x.ru", "savvidi", Some(r"C:\A\mail.x.ru\savvidi_ngenergo\mail.db")).unwrap(), choices.existing[0]);
        assert_eq!(chosen_db_path(c, "mail.x.ru", "savvidi", Some(r"c:\a\MAIL.X.RU\savvidi_ngenergo\mail.db")).unwrap(), choices.existing[0], "регистр пути не важен");
        assert_eq!(chosen_db_path(c, "mail.x.ru", "savvidi", None).unwrap(), choices.new_path, "новая база — по адресу ящика");
        assert!(chosen_db_path(c, "mail.x.ru", "savvidi", Some(r"C:\Windows\mail.db")).is_err(), "произвольный путь не принимается");
        // Проверка была с другим логином — выбор не годится.
        assert!(chosen_db_path(c, "mail.x.ru", "petrov", Some(r"C:\A\mail.x.ru\savvidi_ngenergo\mail.db")).is_err());
        assert_eq!(chosen_db_path(c, "mail.x.ru", "petrov", None).unwrap(), config::default_db_path("mail.x.ru", "petrov"));
        assert_eq!(chosen_db_path(None, "mail.x.ru", "savvidi", None).unwrap(), config::default_db_path("mail.x.ru", "savvidi"));
    }

    #[test]
    fn login_errors_have_fixed_texts() {
        let auth = describe_login_error(&TransportError::AuthFailed("reason=2".into()), "h");
        assert!(auth.contains("Неверный логин или пароль") && !auth.contains("reason"), "{auth}");
        assert!(describe_login_error(&TransportError::SessionLimit, "h").contains("лимит сессий"));
        assert!(describe_login_error(&TransportError::Protocol { op: "auth.owa".into(), status: 404, raw: "<html>".into() }, "h").contains("404"));
        let net = describe_login_error(&TransportError::Network(Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out"))), "h");
        assert!(net.contains("не отвечает"), "{net}");
    }
}
