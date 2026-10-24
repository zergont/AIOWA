//! Синхронизация из окна: один транспорт OWA на процесс (ленивый вход, logoff при выходе),
//! одна синхронизация одновременно, ход отправляется в окно событием `sync-progress`.
//!
//! Пароль берётся из диспетчера учётных данных Windows (служба `AIOWA`, ключ = логин),
//! как в `aiowa set-password`; формы ввода пароля в окне нет.

use aiowa_core::AccountId;
use aiowa_store::Store;
use aiowa_sync::{Engine, FolderSelection, SyncEvent, SyncOptions, SyncReport, SyncStage};
use aiowa_transport::{Credentials, MailTransport, TransportError};
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use secrecy::SecretString;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{info, warn};

const KEYRING_SERVICE: &str = "AIOWA";
/// Ключ в `sync_state` с временем последней успешной синхронизации из окна (мс UTC).
const LAST_OK_KEY: &str = "desktop.last_sync_ok_ms";
/// Сколько ждать завершения текущей пачки при выходе, прежде чем выйти из сессии.
const EXIT_GRACE: Duration = Duration::from_secs(8);
const LOGOFF_TIMEOUT: Duration = Duration::from_secs(5);

/// Этап для окна: строки, а не числа, чтобы фронтенд не зависел от порядка вариантов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageDto {
    /// Вход на сервер.
    Login,
    Hierarchy,
    Items,
    Bodies,
    Attachments,
}

impl From<SyncStage> for StageDto {
    fn from(s: SyncStage) -> Self {
        match s {
            SyncStage::Hierarchy => StageDto::Hierarchy,
            SyncStage::Items => StageDto::Items,
            SyncStage::Bodies => StageDto::Bodies,
            SyncStage::Attachments => StageDto::Attachments,
        }
    }
}

/// Итог последнего завершённого запуска, в числах для строки состояния.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportDto {
    pub folders_synced: usize,
    pub items_created: usize,
    pub items_updated: usize,
    pub items_deleted: usize,
    pub bodies_fetched: usize,
    pub bodies_failed: usize,
    pub attachments_fetched: usize,
    pub attachments_failed: usize,
    pub elapsed_ms: u64,
}

impl From<&SyncReport> for ReportDto {
    fn from(r: &SyncReport) -> Self {
        Self {
            folders_synced: r.folders_synced,
            items_created: r.items_created,
            items_updated: r.items_updated,
            items_deleted: r.items_deleted,
            bodies_fetched: r.bodies_fetched,
            bodies_failed: r.bodies_failed,
            attachments_fetched: r.attachments_fetched,
            attachments_failed: r.attachments_failed,
            elapsed_ms: r.elapsed.as_millis() as u64,
        }
    }
}

/// Состояние синхронизации для окна. То же самое приходит и событием `sync-progress`,
/// и ответом `sync_status`, поэтому фронтенд держит одну структуру.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncStatusDto {
    pub running: bool,
    /// Отмена запрошена, движок останавливается после текущей пачки.
    pub cancelling: bool,
    pub stage: Option<StageDto>,
    pub folder: Option<String>,
    /// Сделано на текущем этапе в текущей папке.
    pub done: u64,
    /// Ожидается на этапе, если известно.
    pub total: Option<u64>,
    /// Писем (метаданных) обработано за этот запуск по всем папкам.
    pub items_processed: u64,
    /// Папок с обработанными метаданными за этот запуск.
    pub folders_done: u64,
    pub started_ms: Option<i64>,
    pub last_ok_ms: Option<i64>,
    /// Ошибка, прервавшая последний запуск; сбрасывается при следующем успешном.
    pub last_error: Option<String>,
    /// Ошибки по отдельным письмам и вложениям за последний запуск (последние несколько).
    pub warnings: Vec<String>,
    pub warnings_total: u64,
    pub last_report: Option<ReportDto>,
}

/// Внутреннее состояние; всё под одним мьютексом, так как меняется редко.
#[derive(Default)]
struct Inner {
    status: SyncStatusDto,
    cancel: Option<Arc<AtomicBool>>,
    transport: Option<OwaTransport>,
    /// Поколение транспорта: растёт с каждым входом. Долгоживущий вызывающий (канал уведомлений)
    /// сбрасывает транспорт только своего поколения, чтобы не разлогинить сессию, которую другой
    /// вызывающий уже успел открыть заново.
    transport_gen: u64,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    /// Запрос канала уведомлений, пришедший во время синхронизации: запускается по её концу.
    pending: Option<PendingSync>,
}

/// Что синхронизировать за один запуск.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Дерево папок и все почтовые папки.
    AllMail,
    /// Дерево папок и только эти папки (ключи локальной базы, не пусто).
    Folders(Vec<i64>),
    /// Только дерево папок (`SyncFolderHierarchy`): после создания, переименования и удаления
    /// папки из окна, когда письма перечитывать не нужно.
    HierarchyOnly,
}

impl Scope {
    /// Из аргумента команды окна: `None` или пустой список — все почтовые папки.
    fn from_folders(folders: Option<Vec<i64>>) -> Self {
        match folders {
            Some(pks) if !pks.is_empty() => Scope::Folders(pks),
            _ => Scope::AllMail,
        }
    }
}

/// Отложенная синхронизация: всё, список ключей папок без повторов и/или только дерево.
#[derive(Debug, Default)]
struct PendingSync {
    all: bool,
    folders: Vec<i64>,
    hierarchy: bool,
}

impl PendingSync {
    fn merge(&mut self, scope: Scope) {
        match scope {
            Scope::AllMail => self.all = true,
            Scope::HierarchyOnly => self.hierarchy = true,
            Scope::Folders(pks) => {
                for pk in pks {
                    if !self.folders.contains(&pk) {
                        self.folders.push(pk);
                    }
                }
            }
        }
    }

    /// Объём следующего запуска: всё перекрывает список папок, список — одно дерево
    /// (прогон по папкам и так начинается с дерева).
    fn into_scope(self) -> Scope {
        if self.all {
            Scope::AllMail
        } else if !self.folders.is_empty() {
            Scope::Folders(self.folders)
        } else {
            Scope::HierarchyOnly
        }
    }
}

pub struct SyncState {
    inner: Mutex<Inner>,
    /// Фаза входа на сервер: один вход за раз, остальные ждут и берут готовый транспорт.
    /// Иначе синхронизация и команда «по требованию» войдут одновременно и займут две сессии.
    connect_lock: tokio::sync::Mutex<()>,
    /// Приложение закрывается: транспорт отдан на logoff, новых входов не делать.
    closing: AtomicBool,
    store: Arc<Store>,
    account: AccountId,
    host: String,
    user: String,
}

const MAX_WARNINGS: usize = 5;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn load_password(user: &str) -> Result<SecretString, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| format!("keyring: {e}"))?;
    let pw = entry.get_password().map_err(|e| format!("пароль для {user} не найден в диспетчере учётных данных ({e}); выполните в консоли: aiowa set-password"))?;
    Ok(SecretString::from(pw))
}

/// Почему транспорт процесса не получен. `Display` даёт текст для пользователя.
#[derive(Debug)]
pub(crate) enum ConnectError {
    /// В диспетчере учётных данных нет пароля (подробность уже в журнале).
    NoPassword,
    /// Приложение закрывается.
    Closing,
    /// Вход на сервер не удался.
    Connect(TransportError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoPassword => f.write_str("Нет сохранённого пароля: выполните aiowa set-password"),
            ConnectError::Closing => f.write_str("Приложение закрывается"),
            // Текст про отклонённый вход уже начинается со слова «Вход»; остальным нужен контекст.
            ConnectError::Connect(e @ TransportError::AuthFailed(_)) => f.write_str(&crate::ondemand::describe_transport(e)),
            ConnectError::Connect(e) => write!(f, "Вход не удался: {}", crate::ondemand::describe_transport(e)),
        }
    }
}

impl SyncState {
    pub fn new(store: Arc<Store>, account: AccountId, host: String, user: String) -> Self {
        let last_ok_ms = store.get_state(account, LAST_OK_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        let status = SyncStatusDto { last_ok_ms, ..SyncStatusDto::default() };
        Self { inner: Mutex::new(Inner { status, ..Inner::default() }), connect_lock: tokio::sync::Mutex::new(()), closing: AtomicBool::new(false), store, account, host, user }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Отравленный мьютекс здесь не страшен: внутри только статус и клоны Arc.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn status(&self) -> SyncStatusDto {
        self.lock().status.clone()
    }

    /// Меняет статус и рассылает его в окно.
    fn update(&self, app: &AppHandle, f: impl FnOnce(&mut SyncStatusDto)) {
        let snapshot = {
            let mut g = self.lock();
            f(&mut g.status);
            g.status.clone()
        };
        if let Err(e) = app.emit("sync-progress", snapshot) {
            warn!(error = %e, "не удалось отправить событие sync-progress");
        }
    }

    /// Запускает синхронизацию, если она не идёт. Возвращает статус сразу после запуска.
    /// `folders` — только эти папки (ключи локальной базы); `None` или пусто — все почтовые.
    pub fn start(self: &Arc<Self>, app: AppHandle, days: Option<u32>, folders: Option<Vec<i64>>) -> SyncStatusDto {
        self.launch(app, days, Scope::from_folders(folders), false)
    }

    /// Запрос от канала уведомлений: как `start`, но если синхронизация уже идёт, папки
    /// запоминаются и запускаются сразу по её концу (объединяясь с другими такими запросами).
    pub fn request(self: &Arc<Self>, app: AppHandle, folders: Option<Vec<i64>>) {
        self.launch(app, None, Scope::from_folders(folders), true);
    }

    /// Запрос только дерева папок после операции с папкой из окна (`folders.rs`): сервер уже
    /// ответил, локальная строка поправлена, иерархия сверит имена, родителей и счётчики. Если
    /// синхронизация идёт, дерево обновится сразу по её концу. По завершении — событие
    /// `folders-changed`.
    pub fn request_hierarchy(self: &Arc<Self>, app: AppHandle) {
        self.launch(app, None, Scope::HierarchyOnly, true);
    }

    fn launch(self: &Arc<Self>, app: AppHandle, days: Option<u32>, scope: Scope, queue_if_busy: bool) -> SyncStatusDto {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut g = self.lock();
            if g.status.running || self.closing.load(Ordering::SeqCst) {
                if queue_if_busy && g.status.running {
                    g.pending.get_or_insert_with(PendingSync::default).merge(scope);
                }
                return g.status.clone();
            }
            g.cancel = Some(cancel.clone());
            g.status = SyncStatusDto {
                running: true,
                cancelling: false,
                stage: Some(StageDto::Login),
                folder: None,
                done: 0,
                total: None,
                items_processed: 0,
                folders_done: 0,
                started_ms: Some(now_ms()),
                last_ok_ms: g.status.last_ok_ms,
                last_error: None,
                warnings: Vec::new(),
                warnings_total: 0,
                last_report: g.status.last_report.clone(),
            };
        }
        let this = self.clone();
        let app_for_task = app.clone();
        let handle = tauri::async_runtime::spawn(async move {
            this.run(app_for_task, days, scope, cancel).await;
        });
        let mut g = self.lock();
        g.task = Some(handle);
        let snapshot = g.status.clone();
        drop(g);
        if let Err(e) = app.emit("sync-progress", snapshot.clone()) {
            warn!(error = %e, "не удалось отправить событие sync-progress");
        }
        snapshot
    }

    /// Просит остановиться после текущей пачки. Если ничего не идёт, ничего не делает.
    pub fn cancel(&self, app: &AppHandle) -> SyncStatusDto {
        let flag = {
            let g = self.lock();
            if !g.status.running {
                return g.status.clone();
            }
            g.cancel.clone()
        };
        if let Some(c) = flag {
            c.store(true, Ordering::Relaxed);
        }
        self.update(app, |s| s.cancelling = true);
        self.status()
    }

    /// Транспорт процесса, если вход уже был (без входа и ожидания): для завершения цикла
    /// канала уведомлений при выходе.
    pub(crate) fn current_transport(&self) -> Option<OwaTransport> {
        self.lock().transport.clone()
    }

    /// Транспорт процесса: вход при первом обращении, дальше та же сессия. Вход защищён
    /// замком: параллельные вызывающие (синхронизация и команды окна) ждут один вход.
    pub(crate) async fn transport(&self) -> Result<OwaTransport, ConnectError> {
        if let Some(t) = self.lock().transport.clone() {
            return Ok(t);
        }
        let _connecting = self.connect_lock.lock().await;
        if self.closing.load(Ordering::SeqCst) {
            return Err(ConnectError::Closing);
        }
        // Пока ждали замок, кто-то мог уже войти.
        if let Some(t) = self.lock().transport.clone() {
            return Ok(t);
        }
        let password = load_password(&self.user).map_err(|detail| {
            warn!(error = %detail, "пароль не найден");
            ConnectError::NoPassword
        })?;
        let creds = Credentials { host: self.host.clone(), user: self.user.clone(), password };
        let t = OwaTransport::connect(creds, OwaConfig::default()).await.map_err(ConnectError::Connect)?;
        // Пока шёл вход, приложение могли начать закрывать: такую сессию не сохраняем, а сразу
        // завершаем, иначе её никто не разлогинит. (Окно между проверкой и записью закрывает
        // `shutdown`: он ждёт замок входа и забирает транспорт ещё раз.)
        if self.closing.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await;
            return Err(ConnectError::Closing);
        }
        let mut g = self.lock();
        g.transport = Some(t.clone());
        g.transport_gen += 1;
        Ok(t)
    }

    /// Транспорт процесса вместе с его поколением — для `invalidate_on_generation`.
    pub(crate) async fn transport_with_generation(&self) -> Result<(OwaTransport, u64), ConnectError> {
        let t = self.transport().await?;
        let g = self.lock();
        // Между входом и чтением поколения транспорт могли сбросить: тогда отдаём то, что есть,
        // а поколение — текущее (сброс по нему никого не заденет).
        Ok((g.transport.clone().unwrap_or(t), g.transport_gen))
    }

    /// Сбрасывает транспорт процесса, если ошибка означает мёртвую сессию: истёкшая сессия, которую
    /// транспорт не смог восстановить повторным входом, отказ CSRF или отклонённый вход. Следующее
    /// обращение войдёт заново. Прочие ошибки транспорт не трогают.
    pub(crate) async fn invalidate_on(&self, e: &TransportError) {
        self.invalidate_on_generation(e, None).await;
    }

    /// То же, но только если транспорт процесса всё ещё поколения `generation`: иначе ошибка
    /// пришла от уже заменённой сессии, и текущую трогать нельзя.
    pub(crate) async fn invalidate_on_generation(&self, e: &TransportError, generation: Option<u64>) {
        if !(e.needs_reauth() || matches!(e, TransportError::AuthFailed(_))) {
            return;
        }
        let old = {
            let mut g = self.lock();
            if generation.is_some_and(|wanted| wanted != g.transport_gen) { None } else { g.transport.take() }
        };
        if let Some(t) = old {
            warn!(error = %e, "сессия OWA непригодна, транспорт сброшен");
            let _ = tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await;
        }
    }

    async fn run(self: Arc<Self>, app: AppHandle, days: Option<u32>, scope: Scope, cancel: Arc<AtomicBool>) {
        let hierarchy_only = scope == Scope::HierarchyOnly;
        let outcome = self.run_inner(&app, days, scope, cancel).await;
        match &outcome {
            // Прогон по одному дереву время «последней синхронизации» и итог не меняет: письма не читались.
            Ok(report) if hierarchy_only => {
                info!(elapsed_ms = report.elapsed.as_millis() as u64, folders = report.folders_in_hierarchy, "дерево папок обновлено по запросу окна");
                if let Err(e) = app.emit("folders-changed", ()) {
                    warn!(error = %e, "не удалось отправить событие folders-changed");
                }
            }
            Ok(report) => {
                let ms = now_ms();
                if let Err(e) = self.store.set_state(self.account, LAST_OK_KEY, Some(&ms.to_string())) {
                    warn!(error = %e, "не удалось сохранить время синхронизации");
                }
                info!(elapsed_s = report.elapsed.as_secs_f32(), created = report.items_created, updated = report.items_updated, bodies = report.bodies_fetched, attachments = report.attachments_fetched, "синхронизация из окна завершена");
            }
            Err(Outcome::Cancelled) => info!("синхронизация остановлена пользователем"),
            Err(Outcome::Failed { message, cause }) => {
                warn!(error = %message, "синхронизация из окна прервана");
                if let Some(e) = cause {
                    self.invalidate_on(e).await;
                }
            }
        }
        let finished_ms = now_ms();
        self.update(&app, |s| {
            s.running = false;
            s.cancelling = false;
            s.stage = None;
            s.folder = None;
            s.done = 0;
            s.total = None;
            match &outcome {
                Ok(_) if hierarchy_only => s.last_error = None,
                Ok(report) => {
                    s.last_ok_ms = Some(finished_ms);
                    s.last_error = None;
                    s.last_report = Some(ReportDto::from(report));
                }
                Err(Outcome::Cancelled) => s.last_error = Some("остановлено".to_string()),
                Err(Outcome::Failed { message, .. }) => s.last_error = Some(message.clone()),
            }
        });
        // После удачного прогона: счётчик непрочитанных в заголовке и (один раз) запуск канала
        // уведомлений — транспорт уже есть, подписки есть на что делать.
        if outcome.is_ok()
            && let Some(notify) = app.try_state::<Arc<crate::notify::NotifyState>>()
        {
            notify.on_sync_ok(&app);
        }
        let pending = {
            let mut g = self.lock();
            g.task = None;
            g.cancel = None;
            g.pending.take()
        };
        // Папки, о которых канал сообщил во время прогона (или дерево, которое окно поправило):
        // сразу следующий прогон (не при выходе).
        if let Some(p) = pending
            && !self.closing.load(Ordering::SeqCst)
        {
            let scope = p.into_scope();
            info!(?scope, "синхронизация по отложенному запросу");
            self.launch(app, None, scope, false);
        }
    }

    async fn run_inner(self: &Arc<Self>, app: &AppHandle, days: Option<u32>, scope: Scope, cancel: Arc<AtomicBool>) -> Result<SyncReport, Outcome> {
        let transport = self.transport().await.map_err(|e| Outcome::Failed { message: e.to_string(), cause: None })?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Outcome::Cancelled);
        }
        let progress: aiowa_sync::ProgressFn = {
            let app = app.clone();
            let st = self.clone();
            Arc::new(move |e: &SyncEvent| st.on_event(&app, e))
        };
        let engine = Engine::new(Arc::new(transport.clone()), self.store.clone(), self.account, cancel).on_progress(progress);
        let folders = match scope {
            Scope::AllMail => FolderSelection::AllMail,
            Scope::Folders(pks) => FolderSelection::Ids(pks),
            Scope::HierarchyOnly => {
                let started = std::time::Instant::now();
                return match engine.sync_hierarchy().await {
                    Ok(n) => Ok(SyncReport { folders_in_hierarchy: n, elapsed: started.elapsed(), ..SyncReport::default() }),
                    Err(e) => Err(Self::failed(e)),
                };
            }
        };
        let opts = SyncOptions {
            folders,
            body_days: match days {
                Some(0) => None,
                Some(d) => Some(d),
                None => SyncOptions::default().body_days,
            },
            ..SyncOptions::default()
        };
        engine.run(&opts).await.map_err(Self::failed)
    }

    /// Ошибка движка в исход запуска: транспортная сохраняет причину для `invalidate_on`.
    fn failed(e: aiowa_sync::SyncError) -> Outcome {
        match e {
            aiowa_sync::SyncError::Cancelled => Outcome::Cancelled,
            aiowa_sync::SyncError::Transport(e) => Outcome::Failed { message: e.to_string(), cause: Some(e) },
            e => Outcome::Failed { message: e.to_string(), cause: None },
        }
    }

    fn on_event(&self, app: &AppHandle, e: &SyncEvent) {
        match e {
            SyncEvent::Stage { stage, folder } => {
                let stage = *stage;
                let folder = folder.clone();
                self.update(app, |s| {
                    s.stage = Some(stage.into());
                    s.folder = folder;
                    s.done = 0;
                    s.total = None;
                    // Папок обработано: по началу этапа «метаданные» каждой папки.
                    if stage == SyncStage::Items {
                        s.folders_done += 1;
                    }
                });
            }
            SyncEvent::Progress { stage, folder, done, total } => {
                let (stage, folder, done, total) = (*stage, folder.clone(), *done, *total);
                self.update(app, |s| {
                    // `done` в событии накопительный по папке; в общий счётчик добавляем приращение.
                    let prev_done = if s.stage == Some(stage.into()) && s.folder == folder { s.done } else { 0 };
                    s.stage = Some(stage.into());
                    s.folder = folder;
                    s.done = done;
                    s.total = total;
                    if stage == SyncStage::Items {
                        s.items_processed += done.saturating_sub(prev_done);
                    }
                });
            }
            SyncEvent::Warning { message, folder, .. } => {
                let text = match folder {
                    Some(f) => format!("{f}: {message}"),
                    None => message.clone(),
                };
                self.update(app, |s| {
                    s.warnings_total += 1;
                    s.warnings.push(text);
                    if s.warnings.len() > MAX_WARNINGS {
                        let extra = s.warnings.len() - MAX_WARNINGS;
                        s.warnings.drain(0..extra);
                    }
                });
            }
        }
    }

    /// При выходе: остановить синхронизацию, дождаться конца пачки и выйти из сессии на сервере,
    /// чтобы на нём не копились сессии. Вызывается с главного потока, поэтому блокирует.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let (cancel, task) = {
            let mut g = self.lock();
            (g.cancel.take(), g.task.take())
        };
        if let Some(c) = cancel {
            c.store(true, Ordering::Relaxed);
        }
        tauri::async_runtime::block_on(async move {
            if let Some(t) = task
                && tokio::time::timeout(EXIT_GRACE, t).await.is_err()
            {
                warn!("синхронизация не остановилась за {} с, выхожу из сессии без ожидания", EXIT_GRACE.as_secs());
            }
            // Вход, начатый до закрытия (синхронизацией или командой окна), мог ещё идти: ждём замок
            // входа, чтобы забрать и его сессию. Новых входов не будет — `closing` уже выставлен.
            let _connecting = match tokio::time::timeout(LOGOFF_TIMEOUT, self.connect_lock.lock()).await {
                Ok(guard) => Some(guard),
                Err(_) => {
                    warn!("вход на сервер не завершился за {} с, выхожу из того, что есть", LOGOFF_TIMEOUT.as_secs());
                    None
                }
            };
            let transport = self.lock().transport.take();
            if let Some(t) = transport {
                match tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await {
                    Ok(Ok(())) => info!("выход из сессии OWA выполнен"),
                    Ok(Err(e)) => warn!(error = %e, "выход из сессии OWA не удался"),
                    Err(_) => warn!("выход из сессии OWA не уложился в {} с", LOGOFF_TIMEOUT.as_secs()),
                }
            }
        });
    }
}

enum Outcome {
    Cancelled,
    /// `cause` — ошибка транспорта, по которой `invalidate_on` решает, сбрасывать ли сессию.
    Failed { message: String, cause: Option<TransportError> },
}

// --- команды Tauri -----------------------------------------------------------

#[tauri::command]
pub fn sync_start(app: AppHandle, state: tauri::State<'_, Arc<SyncState>>, days: Option<u32>, folders: Option<Vec<i64>>) -> SyncStatusDto {
    state.inner().start(app, days, folders)
}

#[tauri::command]
pub fn sync_status(state: tauri::State<'_, Arc<SyncState>>) -> SyncStatusDto {
    state.status()
}

#[tauri::command]
pub fn sync_cancel(app: AppHandle, state: tauri::State<'_, Arc<SyncState>>) -> SyncStatusDto {
    state.cancel(&app)
}
