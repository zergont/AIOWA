//! Обновление приложения из окна (`tauri-plugin-updater`): проверка, загрузка с ходом, установка.
//!
//! Откуда: `plugins.updater.endpoints` в `tauri.conf.json` — `latest.json` последнего релиза GitHub
//! (его собирает CI по метке `v*`, `.github/workflows/release.yml`). Установщик проверяется подписью
//! ключом обновлений (minisign, открытый ключ — `plugins.updater.pubkey`), и подпись обязана нести
//! версию (`requireSignedVersion`): подменённый `latest.json` не подсунет старый подписанный выпуск
//! под видом нового. Загружать установщик разрешено только с того же хоста и из того же репозитория,
//! что и `latest.json` (`download_allowed`). Других сетевых адресатов у модуля нет.
//!
//! Установка на Windows: плагин пишет установщик во временную папку, вызывает `on_before_exit`,
//! запускает установщик NSIS (`/P /UPDATE /R`: только окно хода установки, затем запуск приложения)
//! и завершает процесс через `std::process::exit(0)` — `RunEvent::Exit` при этом не наступает.
//! Поэтому штатные шаги выхода (`MailServices::shutdown`: напоминания, канал уведомлений,
//! отложенные действия, синхронизация и logoff сессии OWA) выполняются в `on_before_exit`, до запуска
//! установщика. Если установщик после этого не запустился, службы уже остановлены — приложение
//! перезапускается.
//!
//! Настройки — в `sync_state` базы: `desktop.update_auto_check` (по умолчанию включено) и
//! `desktop.update_last_check_ms` (последняя удачная проверка). Фоновая проверка идёт только в режиме
//! почты: через минуту после старта и затем раз в сутки; найденная версия — событие
//! `update-available` окну. Неудачная фоновая проверка (GitHub закрыт, нет сети) повторяется с
//! нарастающей паузой — 6 ч, 12 ч, затем раз в сутки (`retry_after_ms`), — а в журнал попадает
//! предупреждением только первая неудача подряд. Ход загрузки и установки — событие
//! `update-progress`.
//!
//! Журнал: только версии и коды ошибок, без адресов и текста ответов.

use crate::{err, AppState, CmdResult};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use tracing::{debug, info, warn};
use url::Url;

pub(crate) const AUTO_CHECK_KEY: &str = "desktop.update_auto_check";
pub(crate) const LAST_CHECK_KEY: &str = "desktop.update_last_check_ms";

/// Событие окну: найдена новая версия (`UpdateInfoDto`).
const AVAILABLE_EVENT: &str = "update-available";
/// Событие окну: ход загрузки и установки (`ProgressDto`).
const PROGRESS_EVENT: &str = "update-progress";

/// Первая фоновая проверка — через минуту после старта: окно и первая синхронизация важнее.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
/// Как часто фоновая задача смотрит, не пора ли проверить снова (по часам, а не по таймеру сна:
/// после сна компьютера проверка не сдвигается на сутки).
const TICK: Duration = Duration::from_secs(60 * 60);
const HOUR_MS: i64 = 60 * 60 * 1000;
/// Повторная проверка — раз в сутки после последней удачной.
const CHECK_EVERY_MS: i64 = 24 * HOUR_MS;
/// Срок запроса `latest.json`.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Загрузка установщика без срока (медленный канал), но без данных дольше этого — отказ.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Как часто проверять, не встала ли загрузка.
const STALL_POLL: Duration = Duration::from_secs(5);
/// События хода загрузки — не чаще.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// «Что нового» длиннее — обрезается: это текст для окна, а не документ.
const NOTES_MAX_CHARS: usize = 4000;

/// Сведения о найденной версии для окна.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateInfoDto {
    version: String,
    /// Дата выпуска, мс от эпохи; `None` — в `latest.json` её нет.
    date_ms: Option<i64>,
    /// Что нового: текст из `latest.json` без разметки; `None` — пусто.
    notes: Option<String>,
}

/// Ответ проверки: текущая версия и новая, если есть.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateCheckDto {
    current: String,
    available: Option<UpdateInfoDto>,
    /// Время этой проверки, мс.
    checked_ms: i64,
}

/// Состояние для вкладки «О программе» и строки состояния, без сети.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdatesStatusDto {
    current: String,
    auto_check: bool,
    /// Последняя удачная проверка, мс; `None` — ещё не проверялось.
    last_check_ms: Option<i64>,
    /// Найденная последней проверкой версия (если проверка уже была).
    available: Option<UpdateInfoDto>,
    installing: bool,
    /// Настройки хранятся в базе учётной записи: без неё (мастер первого запуска) — только по умолчанию.
    can_configure: bool,
}

/// Ход установки: `download` — загрузка (`total` — размер, если сервер его сообщил), `install` —
/// загружено и проверено, идут шаги выхода и запуск установщика.
#[derive(Debug, Clone, Serialize)]
struct ProgressDto {
    stage: &'static str,
    downloaded: u64,
    total: Option<u64>,
}

#[derive(Default)]
struct Inner {
    /// Найденное последней проверкой обновление (с адресом и подписью для установки).
    available: Option<Update>,
    /// Последняя удачная проверка, когда базы нет (режим первого запуска).
    last_check_ms: Option<i64>,
    /// Неудачные фоновые проверки подряд; любая удачная проверка сбрасывает.
    backoff: Option<Backoff>,
}

/// Неудачные фоновые проверки подряд: когда была последняя и сколько их.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    last_ms: i64,
    count: u32,
}

#[derive(Default)]
pub(crate) struct UpdatesState {
    inner: Mutex<Inner>,
    /// Проверки по одной: фоновая и по кнопке не идут параллельно.
    check_lock: tokio::sync::Mutex<()>,
    /// Идёт установка (загрузка и запуск установщика): вторая не начинается.
    installing: AtomicBool,
    /// Шаги выхода перед установщиком уже выполнены: службы почты остановлены.
    stopped: AtomicBool,
}

impl UpdatesState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

// --- настройки -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdatePrefs {
    auto_check: bool,
    last_check_ms: Option<i64>,
}

impl UpdatePrefs {
    /// Автопроверка выключена только явным `0`; время — целое положительное число мс.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            auto_check: get(AUTO_CHECK_KEY).is_none_or(|v| v.trim() != "0"),
            last_check_ms: get(LAST_CHECK_KEY).and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0),
        }
    }
}

fn load_prefs(app: &AppHandle) -> UpdatePrefs {
    match app.try_state::<AppState>() {
        Some(st) => UpdatePrefs::from_lookup(|key| match st.store.get_state(st.account, key) {
            Ok(v) => v,
            Err(e) => {
                warn!(key, error = %e, "не удалось прочитать настройку обновлений");
                None
            }
        }),
        None => UpdatePrefs { auto_check: true, last_check_ms: app.state::<UpdatesState>().lock().last_check_ms },
    }
}

fn record_check(app: &AppHandle, at_ms: i64) {
    app.state::<UpdatesState>().lock().last_check_ms = Some(at_ms);
    if let Some(st) = app.try_state::<AppState>()
        && let Err(e) = st.store.set_state(st.account, LAST_CHECK_KEY, Some(&at_ms.to_string()))
    {
        warn!(error = %e, "не удалось записать время проверки обновлений");
    }
}

/// Пора ли проверке по последней удачной: её ещё не было, прошли сутки или часы переведены назад.
fn due(last_ok_ms: Option<i64>, now_ms: i64) -> bool {
    last_ok_ms.is_none_or(|t| now_ms < t || now_ms - t >= CHECK_EVERY_MS)
}

/// Пауза после `count` неудачных фоновых проверок подряд: 6 ч, 12 ч, затем сутки. Сервер, закрытый
/// сейчас (GitHub недоступен из сети предприятия), через час скорее всего закрыт так же.
fn retry_after_ms(count: u32) -> i64 {
    match count {
        0 => 0,
        1 => 6 * HOUR_MS,
        2 => 12 * HOUR_MS,
        _ => CHECK_EVERY_MS,
    }
}

/// Пора ли фоновой проверке: по последней удачной (`due`) и, если перед этим были неудачи подряд,
/// выдержана пауза после последней из них (часы переведены назад — пора).
fn background_due(last_ok_ms: Option<i64>, backoff: Option<Backoff>, now_ms: i64) -> bool {
    due(last_ok_ms, now_ms) && backoff.is_none_or(|b| now_ms < b.last_ms || now_ms - b.last_ms >= retry_after_ms(b.count))
}

/// Записать неудачу фоновой проверки; возвращает, сколько их теперь подряд.
fn note_background_failure(st: &UpdatesState, at_ms: i64) -> u32 {
    let mut inner = st.lock();
    let count = inner.backoff.map_or(0, |b| b.count).saturating_add(1);
    inner.backoff = Some(Backoff { last_ms: at_ms, count });
    count
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn current_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

// --- ошибки ----------------------------------------------------------------------------------

/// Отказ проверки или установки: код для журнала и текст для окна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    /// HTTP-статус ответа, если отказал сервер.
    status: Option<u16>,
}

impl Failure {
    const fn of(code: &'static str) -> Self {
        Self { code, status: None }
    }

    fn from_error(e: &tauri_plugin_updater::Error) -> Self {
        use tauri_plugin_updater::Error as E;
        match e {
            // 404 на `latest.json` (релиза ещё нет или репозиторий закрыт) плагин сводит сюда.
            E::ReleaseNotFound => Self::of("release_not_found"),
            E::Reqwest(r) if r.is_timeout() => Self::of("timeout"),
            E::Reqwest(r) => Self { code: "network", status: r.status().map(|s| s.as_u16()) },
            E::Network(text) => Self { code: "http_status", status: status_in(text) },
            E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) => Self::of("signature"),
            E::SignedVersionMismatch { .. } => Self::of("signed_version_mismatch"),
            E::MissingSignedVersion => Self::of("signed_version_missing"),
            E::TargetNotFound(_) | E::TargetsNotFound(_) => Self::of("no_windows_build"),
            E::Serialization(_) | E::Semver(_) | E::UrlParse(_) => Self::of("bad_manifest"),
            E::InvalidUpdaterFormat | E::BinaryNotFoundInArchive => Self::of("bad_package"),
            E::Io(_) | E::TempDirNotFound => Self::of("io"),
            E::InsecureTransportProtocol => Self::of("insecure_endpoint"),
            _ => Self::of("other"),
        }
    }

    fn text(&self) -> String {
        match self.code {
            "release_not_found" => "На сервере обновлений нет опубликованной версии.".into(),
            "timeout" | "network" => "Сервер обновлений недоступен. Проверьте подключение к интернету и доступ к github.com.".into(),
            "http_status" => match self.status {
                Some(s) => format!("Сервер обновлений ответил ошибкой {s}."),
                None => "Сервер обновлений ответил ошибкой.".into(),
            },
            "signature" | "signed_version_mismatch" | "signed_version_missing" => "Подпись обновления не прошла проверку: обновление не установлено.".into(),
            "no_windows_build" => "В опубликованной версии нет сборки для Windows.".into(),
            "bad_manifest" => "Сервер обновлений вернул неожиданный ответ.".into(),
            "bad_package" => "Загруженный файл не является установщиком: обновление не установлено.".into(),
            "io" => "Не удалось сохранить установщик во временную папку.".into(),
            "stalled" => "Загрузка обновления остановилась: больше минуты нет данных.".into(),
            "foreign_host" => "Установщик предлагается загрузить не с сервера обновлений: загрузка отменена.".into(),
            "insecure_endpoint" => "Адрес сервера обновлений не защищён (нужен https).".into(),
            "installer" => "Установщик не запустился.".into(),
            _ => "Не удалось выполнить обновление.".into(),
        }
    }

    fn log(&self, what: &str) {
        warn!(code = self.code, status = ?self.status, "обновление: {what} не удалась");
    }
}

/// Статус из текста плагина «Download request failed with status: 404 Not Found».
fn status_in(text: &str) -> Option<u16> {
    let rest = &text[text.find("status: ")? + "status: ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|s| (100..600).contains(s))
}

// --- источник обновлений ---------------------------------------------------------------------

/// Адреса `latest.json` из `plugins.updater.endpoints` конфигурации.
fn endpoints(app: &AppHandle) -> Vec<Url> {
    endpoints_of(app.config().plugins.0.get("updater"))
}

fn endpoints_of(updater: Option<&serde_json::Value>) -> Vec<Url> {
    updater
        .and_then(|v| v.get("endpoints"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|s| Url::parse(s).ok()).collect())
        .unwrap_or_default()
}

/// Первые два сегмента пути (владелец и репозиторий на GitHub), в нижнем регистре.
fn repo_of(u: &Url) -> Option<(String, String)> {
    let mut seg = u.path_segments()?.filter(|s| !s.is_empty());
    Some((seg.next()?.to_ascii_lowercase(), seg.next()?.to_ascii_lowercase()))
}

/// Установщик загружается только по `https`, без `user@`, с того же хоста и порта и из того же
/// репозитория, что и один из адресов `latest.json`. Переадресацию на хранилище файлов GitHub
/// (`*.githubusercontent.com`) делает сам сервер, исходный адрес остаётся на `github.com`.
pub(crate) fn download_allowed(endpoints: &[Url], download: &Url) -> bool {
    if download.scheme() != "https" || !download.username().is_empty() || download.password().is_some() {
        return false;
    }
    let Some(repo) = repo_of(download) else { return false };
    endpoints.iter().any(|e| {
        e.scheme() == "https"
            && e.host_str().is_some()
            && e.host_str().map(str::to_ascii_lowercase) == download.host_str().map(str::to_ascii_lowercase)
            && e.port_or_known_default() == download.port_or_known_default()
            && repo_of(e).as_ref() == Some(&repo)
    })
}

fn info_of(u: &Update) -> UpdateInfoDto {
    UpdateInfoDto { version: u.version.clone(), date_ms: u.date.map(|d| (d.unix_timestamp_nanos() / 1_000_000) as i64), notes: clean_notes(u.body.as_deref()) }
}

/// «Что нового» для окна: без пустого текста, не длиннее `NOTES_MAX_CHARS` символов.
fn clean_notes(raw: Option<&str>) -> Option<String> {
    let text = raw?.replace("\r\n", "\n");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match text.char_indices().nth(NOTES_MAX_CHARS) {
        Some((cut, _)) => Some(format!("{}…", text[..cut].trim_end())),
        None => Some(text.to_string()),
    }
}

/// Модуль обновления с нашими шагами выхода перед установщиком (вместо `cleanup_before_exit`
/// по умолчанию — он вызывается последним).
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, tauri_plugin_updater::Error> {
    let handle = app.clone();
    app.updater_builder().timeout(CHECK_TIMEOUT).on_before_exit(move || before_installer(&handle)).build()
}

/// Вызывается плагином после записи установщика на диск и до его запуска; после этого процесс
/// завершается без `RunEvent::Exit`. Выполняется в блокирующем потоке (`updates_install`), поэтому
/// шаги выхода могут ждать сеть.
fn before_installer(app: &AppHandle) {
    info!("обновление: штатный выход перед запуском установщика");
    app.state::<UpdatesState>().stopped.store(true, Ordering::SeqCst);
    if let Some(services) = app.try_state::<crate::MailServices>() {
        services.shutdown(app);
    }
    app.cleanup_before_exit();
}

fn emit_progress(app: &AppHandle, stage: &'static str, downloaded: u64, total: Option<u64>) {
    if let Err(e) = app.emit(PROGRESS_EVENT, ProgressDto { stage, downloaded, total }) {
        warn!(error = %e, "не удалось отправить событие {PROGRESS_EVENT}");
    }
}

// --- проверка --------------------------------------------------------------------------------

/// Запрос `latest.json`. Удачная проверка запоминается (время — в базе) и сбрасывает счёт
/// фоновых неудач; найденная версия заменяет прежнюю и уходит окну событием. Неудача фоновой
/// проверки идёт в счёт пауз (`retry_after_ms`), и предупреждение в журнал пишется только при
/// первой подряд; неудача по кнопке — всегда.
async fn check(app: &AppHandle, background: bool) -> Result<Option<UpdateInfoDto>, Failure> {
    let st = app.state::<UpdatesState>();
    let _one = st.check_lock.lock().await;
    let current = current_version(app);
    let found = match updater(app) {
        Ok(u) => u.check().await,
        Err(e) => Err(e),
    };
    let found = found.map_err(|e| {
        let f = Failure::from_error(&e);
        if background {
            let failures = note_background_failure(&st, now_ms());
            let retry_h = retry_after_ms(failures) / HOUR_MS;
            if failures == 1 {
                warn!(code = f.code, status = ?f.status, retry_h, "обновление: фоновая проверка не удалась, следующая — с паузой");
            } else {
                debug!(code = f.code, status = ?f.status, failures, retry_h, "обновление: фоновая проверка снова не удалась");
            }
        } else {
            f.log("проверка");
        }
        f
    })?;
    if let Some(b) = st.lock().backoff.take() {
        info!(failures = b.count, background, "обновление: проверка снова удалась");
    }
    record_check(app, now_ms());
    let info = found.as_ref().map(info_of);
    match &info {
        Some(i) => info!(current, available = %i.version, background, "обновление: найдена новая версия"),
        None => info!(current, background, "обновление: установлена последняя версия"),
    }
    st.lock().available = found;
    if let Some(i) = &info
        && let Err(e) = app.emit(AVAILABLE_EVENT, i.clone())
    {
        warn!(error = %e, "не удалось отправить событие {AVAILABLE_EVENT}");
    }
    Ok(info)
}

/// Фоновая проверка (только режим почты): через минуту после старта, если включена, затем раз в
/// час смотрит, прошли ли сутки с последней удачной. Неудачная проверка повторится не раньше
/// чем через 6 ч, следующая — через 12 ч, дальше — раз в сутки (`background_due`).
pub(crate) fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        let mut first = true;
        loop {
            let prefs = load_prefs(&app);
            let (busy, backoff) = {
                let st = app.state::<UpdatesState>();
                let backoff = st.lock().backoff;
                (st.installing.load(Ordering::SeqCst), backoff)
            };
            if prefs.auto_check && !busy && (first || background_due(prefs.last_check_ms, backoff, now_ms())) {
                // Отказ уже учтён в `check` (журнал и пауза); окну фоновая ошибка не нужна.
                let _ = check(&app, true).await;
            }
            first = false;
            tokio::time::sleep(TICK).await;
        }
    });
}

// --- установка -------------------------------------------------------------------------------

/// Загрузка установщика с событиями хода; подпись проверяет плагин в конце загрузки. Без данных
/// дольше `STALL_TIMEOUT` — отказ (у загрузки нет общего срока: канал может быть медленным).
async fn download(app: &AppHandle, update: &Update) -> Result<Vec<u8>, Failure> {
    let started = Instant::now();
    // Время последнего куска, мс от `started`.
    let last_chunk = Arc::new(AtomicU64::new(0));
    let mut downloaded: u64 = 0;
    let mut last_emit: Option<Instant> = None;
    let on_chunk = {
        let last_chunk = last_chunk.clone();
        move |len: usize, total: Option<u64>| {
            downloaded += len as u64;
            last_chunk.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            if last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                last_emit = Some(Instant::now());
                emit_progress(app, "download", downloaded, total);
            }
        }
    };
    let fut = update.download(on_chunk, || {});
    tokio::pin!(fut);
    loop {
        tokio::select! {
            res = &mut fut => {
                return res.map_err(|e| {
                    let f = Failure::from_error(&e);
                    f.log("загрузка");
                    f
                });
            }
            _ = tokio::time::sleep(STALL_POLL) => {
                let idle = started.elapsed().saturating_sub(Duration::from_millis(last_chunk.load(Ordering::Relaxed)));
                if idle >= STALL_TIMEOUT {
                    let f = Failure::of("stalled");
                    f.log("загрузка");
                    return Err(f);
                }
            }
        }
    }
}

async fn install(app: &AppHandle) -> CmdResult<()> {
    let st = app.state::<UpdatesState>();
    let stored = st.lock().available.clone();
    let update = match stored {
        Some(u) => u,
        None => {
            check(app, false).await.map_err(|f| f.text())?;
            st.lock().available.clone().ok_or_else(|| "Новой версии нет: установлена последняя.".to_string())?
        }
    };
    if !download_allowed(&endpoints(app), &update.download_url) {
        let f = Failure::of("foreign_host");
        f.log("загрузка");
        return Err(f.text());
    }
    let current = current_version(app);
    info!(current, version = %update.version, "обновление: загрузка установщика");
    emit_progress(app, "download", 0, None);
    let bytes = download(app, &update).await.map_err(|f| f.text())?;
    let len = bytes.len() as u64;
    info!(version = %update.version, bytes = len, "обновление: установщик загружен, подпись верна");
    emit_progress(app, "install", len, Some(len));
    let u = update.clone();
    match tauri::async_runtime::spawn_blocking(move || u.install(bytes)).await.map_err(err)? {
        // На Windows сюда не попадаем: установщик запущен, процесс завершён. На других системах
        // новая версия уже на месте — перезапуск.
        Ok(()) => {
            info!(version = %update.version, "обновление установлено, перезапуск");
            app.request_restart();
            Ok(())
        }
        Err(e) => {
            let f = match &e {
                tauri_plugin_updater::Error::Io(_) if st.stopped.load(Ordering::SeqCst) => Failure::of("installer"),
                _ => Failure::from_error(&e),
            };
            f.log("установка");
            if st.stopped.load(Ordering::SeqCst) {
                // Службы почты уже остановлены, окно скрыто: работать дальше нельзя — перезапуск.
                warn!("обновление: установщик не запустился после остановки служб, перезапуск");
                app.request_restart();
                return Err(format!("{} Приложение перезапустится.", f.text()));
            }
            Err(f.text())
        }
    }
}

// --- команды Tauri ---------------------------------------------------------------------------

/// Версия, настройки и найденное обновление, без сети.
#[tauri::command]
pub(crate) async fn updates_status(app: AppHandle, st: State<'_, UpdatesState>) -> CmdResult<UpdatesStatusDto> {
    let prefs = load_prefs(&app);
    let available = st.lock().available.as_ref().map(info_of);
    Ok(UpdatesStatusDto {
        current: current_version(&app),
        auto_check: prefs.auto_check,
        last_check_ms: prefs.last_check_ms,
        available,
        installing: st.installing.load(Ordering::SeqCst),
        can_configure: app.try_state::<AppState>().is_some(),
    })
}

/// Проверить обновления сейчас (кнопка «Проверить обновления»).
#[tauri::command]
pub(crate) async fn updates_check(app: AppHandle) -> CmdResult<UpdateCheckDto> {
    let available = check(&app, false).await.map_err(|f| f.text())?;
    Ok(UpdateCheckDto { current: current_version(&app), available, checked_ms: now_ms() })
}

/// Включить или выключить автоматическую проверку.
#[tauri::command]
pub(crate) async fn updates_set_auto(enabled: bool, app: AppHandle) -> CmdResult<()> {
    let st = app.try_state::<AppState>().ok_or_else(|| "Настройка доступна после подключения учётной записи".to_string())?;
    st.store.set_state(st.account, AUTO_CHECK_KEY, Some(if enabled { "1" } else { "0" })).map_err(|e| format!("Не удалось сохранить настройку: {e}"))?;
    info!(enabled, "автопроверка обновлений изменена");
    Ok(())
}

/// Скачать найденное обновление (событие `update-progress`) и поставить: на Windows процесс
/// завершается, установщик запускает новую версию. Окно до вызова спрашивает о несохранённом
/// письме в композере. Ошибка — текст для окна.
#[tauri::command]
pub(crate) async fn updates_install(app: AppHandle, st: State<'_, UpdatesState>) -> CmdResult<()> {
    if st.installing.swap(true, Ordering::SeqCst) {
        return Err("Обновление уже устанавливается".into());
    }
    let res = install(&app).await;
    if res.is_err() {
        st.installing.store(false, Ordering::SeqCst);
    }
    res
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
    fn prefs_default_and_parsing() {
        assert_eq!(UpdatePrefs::from_lookup(|_| None), UpdatePrefs { auto_check: true, last_check_ms: None });
        let p = UpdatePrefs::from_lookup(lookup(&[(AUTO_CHECK_KEY, "0"), (LAST_CHECK_KEY, " 1761390000000 ")]));
        assert_eq!(p, UpdatePrefs { auto_check: false, last_check_ms: Some(1_761_390_000_000) });
        let q = UpdatePrefs::from_lookup(lookup(&[(AUTO_CHECK_KEY, "1"), (LAST_CHECK_KEY, "мусор")]));
        assert_eq!(q, UpdatePrefs { auto_check: true, last_check_ms: None });
        assert_eq!(UpdatePrefs::from_lookup(lookup(&[(LAST_CHECK_KEY, "-5")])).last_check_ms, None);
    }

    #[test]
    fn check_is_due_once_a_day() {
        let now = 1_761_390_000_000;
        assert!(due(None, now), "ещё не проверялось");
        assert!(!due(Some(now - 1000), now));
        assert!(!due(Some(now - CHECK_EVERY_MS + 1), now));
        assert!(due(Some(now - CHECK_EVERY_MS), now));
        assert!(due(Some(now + 60_000), now), "часы переведены назад");
    }

    /// Расписание фоновых проверок при закрытом сервере: фоновая задача просыпается раз в час
    /// (`TICK`), а повтор после неудачи — через 6 ч, затем 12 ч, затем раз в сутки; удачная
    /// проверка сбрасывает счёт.
    #[test]
    fn failed_background_checks_back_off() {
        let st = UpdatesState::default();
        let t0 = 1_761_390_000_000;
        let tick = TICK.as_millis() as i64;
        // Удачной проверки ещё не было, сервер недоступен: моменты фоновых проверок за трое суток.
        let mut attempts = vec![t0];
        assert_eq!(note_background_failure(&st, t0), 1, "первая проверка после старта не удалась");
        let mut now = t0;
        while now < t0 + 3 * CHECK_EVERY_MS {
            now += tick;
            if background_due(None, st.lock().backoff, now) {
                attempts.push(now);
                note_background_failure(&st, now);
            }
        }
        let gaps_h: Vec<i64> = attempts.windows(2).map(|w| (w[1] - w[0]) / HOUR_MS).collect();
        assert_eq!(gaps_h, vec![6, 12, 24, 24], "пауза нарастает до суток, а не каждый час");
        assert_eq!(st.lock().backoff.map(|b| b.count), Some(5));

        // Пауза считается от последней неудачи; до неё — рано, ровно в срок — пора.
        let b = Backoff { last_ms: t0, count: 1 };
        assert!(!background_due(None, Some(b), t0 + 6 * HOUR_MS - 1));
        assert!(background_due(None, Some(b), t0 + 6 * HOUR_MS));
        assert!(background_due(None, Some(b), t0 - 1), "часы переведены назад");
        // Без неудач — как раньше: раз в сутки после удачной.
        assert!(background_due(None, None, t0));
        assert!(!background_due(Some(t0), None, t0 + HOUR_MS));
        // После удачной проверки сутки ждут и при старом счёте неудач.
        assert!(!background_due(Some(t0), Some(Backoff { last_ms: t0 - CHECK_EVERY_MS, count: 3 }), t0 + HOUR_MS));
        assert_eq!((retry_after_ms(0), retry_after_ms(1), retry_after_ms(2), retry_after_ms(9)), (0, 6 * HOUR_MS, 12 * HOUR_MS, CHECK_EVERY_MS));
    }

    #[test]
    fn download_only_from_update_repo() {
        let eps = vec![Url::parse("https://github.com/zergont/AIOWA/releases/latest/download/latest.json").unwrap()];
        let ok = |s: &str| download_allowed(&eps, &Url::parse(s).unwrap());
        assert!(ok("https://github.com/zergont/AIOWA/releases/download/v0.2.0/AIOWA_0.2.0_x64-setup.exe"));
        assert!(ok("https://GitHub.com/Zergont/aiowa/releases/download/v0.2.0/x.exe"), "регистр хоста и репозитория не важен");
        assert!(!ok("http://github.com/zergont/AIOWA/releases/download/v0.2.0/x.exe"), "только https");
        assert!(!ok("https://github.com/someone/AIOWA/releases/download/v0.2.0/x.exe"), "чужой владелец");
        assert!(!ok("https://github.com/zergont/other/releases/download/v0.2.0/x.exe"), "чужой репозиторий");
        assert!(!ok("https://evil.example/zergont/AIOWA/releases/download/v0.2.0/x.exe"), "чужой хост");
        assert!(!ok("https://github.com:8443/zergont/AIOWA/x.exe"), "чужой порт");
        assert!(!ok("https://user@github.com/zergont/AIOWA/x.exe"), "user@ в адресе");
        assert!(!ok("https://github.com/zergont"), "нет репозитория");
        assert!(!download_allowed(&[], &Url::parse("https://github.com/zergont/AIOWA/x.exe").unwrap()), "без адресов — нельзя");
    }

    #[test]
    fn notes_are_trimmed_and_limited() {
        assert_eq!(clean_notes(None), None);
        assert_eq!(clean_notes(Some("  \r\n ")), None);
        assert_eq!(clean_notes(Some("- первое\r\n- второе\r\n")), Some("- первое\n- второе".into()));
        let long = "ж".repeat(NOTES_MAX_CHARS + 10);
        let cut = clean_notes(Some(&long)).unwrap();
        assert_eq!(cut.chars().count(), NOTES_MAX_CHARS + 1, "обрезано по символам, не по байтам");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn failures_map_to_codes() {
        use tauri_plugin_updater::Error as E;
        assert_eq!(Failure::from_error(&E::ReleaseNotFound).code, "release_not_found");
        assert_eq!(Failure::from_error(&E::MissingSignedVersion).code, "signed_version_missing");
        assert_eq!(Failure::from_error(&E::TargetsNotFound(vec!["windows-x86_64".into()])).code, "no_windows_build");
        assert_eq!(Failure::from_error(&E::InvalidUpdaterFormat).code, "bad_package");
        let net = Failure::from_error(&E::Network("Download request failed with status: 404 Not Found".into()));
        assert_eq!((net.code, net.status), ("http_status", Some(404)));
        assert!(net.text().contains("404"));
        assert_eq!(status_in("без статуса"), None);
        assert_eq!(status_in("status: 9999"), None);
        for code in ["release_not_found", "timeout", "network", "signature", "no_windows_build", "bad_manifest", "bad_package", "io", "stalled", "foreign_host", "installer", "other"] {
            assert!(!Failure::of(code).text().is_empty());
        }
    }

    /// Конфигурация обновлений: версия совпадает с Cargo, артефакты обновления включены, адрес — `https`
    /// и годится для `download_allowed`, подпись обязана нести версию, установка — пассивная.
    #[test]
    fn updater_config_is_consistent() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["version"].as_str(), Some(env!("CARGO_PKG_VERSION")), "версия в tauri.conf.json и Cargo.toml одна");
        assert_eq!(conf["bundle"]["createUpdaterArtifacts"], serde_json::Value::Bool(true));
        let updater = &conf["plugins"]["updater"];
        assert!(updater["pubkey"].as_str().is_some_and(|k| !k.is_empty()));
        assert_eq!(updater["requireSignedVersion"], serde_json::Value::Bool(true));
        assert_eq!(updater["windows"]["installMode"].as_str(), Some("passive"));
        let eps = endpoints_of(Some(updater));
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].scheme(), "https");
        assert!(eps[0].path().ends_with("/releases/latest/download/latest.json"));
        let repo = repo_of(&eps[0]).unwrap();
        let asset = Url::parse(&format!("https://{}/{}/{}/releases/download/v1.0.0/AIOWA_1.0.0_x64-setup.exe", eps[0].host_str().unwrap(), repo.0, repo.1)).unwrap();
        assert!(download_allowed(&eps, &asset), "установщик релиза того же репозитория разрешён");
    }
}
