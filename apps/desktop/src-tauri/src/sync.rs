//! Синхронизация из окна: один транспорт OWA на процесс (ленивый вход, logoff при выходе),
//! одна синхронизация одновременно, ход отправляется в окно событием `sync-progress`.
//!
//! Пароль берётся из диспетчера учётных данных Windows (служба `AIOWA`, ключ = логин),
//! как в `aiowa set-password`; задаётся мастером первого запуска и «Сменить пароль…» в настройках
//! (`setup.rs`, `settings.rs`). Глубина тел, квота кэша и лимит вложения — из настроек окна.
//!
//! После входа транспорт процесса сверяет ящик с базой (`mailbox::verify_login`): в базе записан
//! GUID другого ящика — транспорт не выдаётся (`ConnectError::WrongMailbox`) до перезапуска.

use aiowa_core::{AccountId, FolderId, FolderRef, ItemId};
use aiowa_store::{ContactApplyStats, Store};
use aiowa_sync::{Engine, FolderSelection, SyncEvent, SyncOptions, SyncReport, SyncStage};
use aiowa_transport::{Credentials, MailTransport, TransportError, WriteResult};
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use secrecy::SecretString;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, info, warn};

/// Событие окну `contacts-changed`: сколько личных контактов добавилось, изменилось и ушло.
#[derive(Debug, Clone, Serialize)]
struct ContactsChanged {
    created: usize,
    updated: usize,
    deleted: usize,
}

pub(crate) const KEYRING_SERVICE: &str = "AIOWA";
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
    /// Свежие письма папки, синхронизируемой впервые.
    Recent,
    Items,
    Bodies,
    Attachments,
}

impl From<SyncStage> for StageDto {
    fn from(s: SyncStage) -> Self {
        match s {
            SyncStage::Hierarchy => StageDto::Hierarchy,
            SyncStage::Recent => StageDto::Recent,
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
    /// База чужого ящика (сверка при входе, `mailbox.rs`): транспорт не выдаётся до перезапуска.
    identity: crate::mailbox::IdentityGuard,
    /// Живое окно: события о записи в базу по ходу прогона (`live.rs`).
    live: Arc<crate::live::LiveEvents>,
}

const MAX_WARNINGS: usize = 5;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn load_password(user: &str) -> Result<SecretString, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| format!("keyring: {e}"))?;
    let pw = entry.get_password().map_err(|e| format!("пароль для {user} не найден в диспетчере учётных данных ({e}); задайте его в настройках или выполните aiowa set-password"))?;
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
    /// В базе записан GUID другого ящика: синхронизировать в неё нельзя.
    WrongMailbox,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoPassword => f.write_str("Нет сохранённого пароля: задайте его в настройках («Сменить пароль…»)"),
            ConnectError::Closing => f.write_str("Приложение закрывается"),
            // Текст про отклонённый вход уже начинается со слова «Вход»; остальным нужен контекст.
            ConnectError::Connect(e @ TransportError::AuthFailed(_)) => f.write_str(&crate::ondemand::describe_transport(e)),
            ConnectError::Connect(e) => write!(f, "Вход не удался: {}", crate::ondemand::describe_transport(e)),
            ConnectError::WrongMailbox => f.write_str(crate::mailbox::WRONG_MAILBOX),
        }
    }
}

impl SyncState {
    pub fn new(store: Arc<Store>, account: AccountId, host: String, user: String) -> Self {
        let last_ok_ms = store.get_state(account, LAST_OK_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        let status = SyncStatusDto { last_ok_ms, ..SyncStatusDto::default() };
        Self { inner: Mutex::new(Inner { status, ..Inner::default() }), connect_lock: tokio::sync::Mutex::new(()), closing: AtomicBool::new(false), store, account, host, user, identity: Default::default(), live: Arc::default() }
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
        if self.identity.blocked() {
            return Err(ConnectError::WrongMailbox);
        }
        let password = load_password(&self.user).map_err(|detail| {
            warn!(error = %detail, "пароль не найден");
            ConnectError::NoPassword
        })?;
        let creds = Credentials { host: self.host.clone(), user: self.user.clone(), password };
        let t = OwaTransport::connect(creds, OwaConfig::default()).await.map_err(ConnectError::Connect)?;
        // Чей ящик: GUID в базе не записан — записать; записан другой — в эту базу не синхронизировать.
        if crate::mailbox::verify_login(&self.store, self.account, &t).await.is_err() {
            self.identity.block();
            let _ = tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await;
            return Err(ConnectError::WrongMailbox);
        }
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

    /// Сбрасывает транспорт процесса безусловно (пароль сменился): сессия закрывается на сервере,
    /// следующее обращение войдёт заново уже с новым паролем. Синхронизация, идущая на старой
    /// сессии, закончится ошибкой; следующий запуск войдёт заново.
    pub(crate) async fn reset_transport(&self) {
        // Вход, который идёт прямо сейчас, — ещё со старым паролем: дождаться его и сбросить тоже.
        let _connecting = self.connect_lock.lock().await;
        let old = self.lock().transport.take();
        if let Some(t) = old {
            info!("транспорт сброшен после смены пароля");
            let _ = tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await;
        }
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
        // Окно по концу прогона перечитывает всё само: накопленные живые события больше не нужны.
        self.live.finish();
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
        let engine = Engine::new(Arc::new(transport.clone()), self.store.clone(), self.account, cancel.clone()).on_progress(progress);
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
        // Настройки окна: глубина тел (если окно не передало свою), квота кэша и лимит вложения.
        let prefs = crate::settings::Prefs::load(&self.store, self.account);
        let opts = SyncOptions {
            folders,
            body_days: match days {
                Some(0) => None,
                Some(d) => Some(d),
                None => prefs.body_window(),
            },
            max_attachment_bytes: prefs.max_attachment_bytes(),
            attachment_cache_bytes: prefs.attachment_cache_bytes(),
            ..SyncOptions::default()
        };
        let body_days = opts.body_days;
        let report = engine.run(&opts).await.map_err(Self::failed)?;
        self.repair_change_keys(&transport, &cancel).await;
        self.backfill_verbs(&transport, body_days, &cancel).await;
        self.sync_contacts(app, &transport, &cancel).await;
        // Остановка на дозапросах или контактах прерывает только их: почта к этому моменту уже
        // синхронизирована, и отчёт прогона — удачный (счётчик непрочитанных, запуск канала
        // уведомлений); недоделанное продолжит следующий прогон.
        if cancel.load(Ordering::Relaxed) {
            info!("синхронизация: остановлена на дозапросах или контактах, почта готова");
        }
        Ok(report)
    }

    /// Личные контакты (docs/obkatka.md п. 8): после почты — `SyncFolderItems` формой
    /// `AllProperties` по папкам класса ровно `IPF.Contact` (служебные `IPF.Contact.*` не
    /// трогаются), состояние — в `folders.contact_sync_state`, отдельно от почтового. Полная
    /// выгрузка (состояния нет или сервер его отверг) по концу убирает контакты папки, которых
    /// сервер не вернул: удаления, случившиеся до сброса, иначе не придут. Отказ сервера по папке
    /// не прерывает прогон: папка ждёт следующего. Об изменениях окну сообщает событие
    /// `contacts-changed`.
    async fn sync_contacts(&self, app: &AppHandle, transport: &OwaTransport, cancel: &AtomicBool) {
        const PAGE: u32 = 100;
        const MAX_PAGES: u32 = 100;
        let folders = match self.store.contact_folders(self.account) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "папки контактов не выбраны");
                return;
            }
        };
        if folders.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let mut stats = ContactApplyStats::default();
        let mut failed = 0usize;
        for f in &folders {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let folder = FolderRef::Id(FolderId { id: f.server_id.clone(), change_key: None });
            let mut state = match self.store.contact_sync_state(f.folder_pk) {
                Ok(s) => s,
                Err(e) => {
                    warn!(folder_pk = f.folder_pk, error = %e, "состояние папки контактов не прочитано");
                    failed += 1;
                    continue;
                }
            };
            if state.is_none()
                && let Err(e) = self.store.begin_contact_full_sync(f.folder_pk)
            {
                warn!(folder_pk = f.folder_pk, error = %e, "контакты папки не помечены к сверке с полной выгрузкой");
            }
            let mut reset_once = false;
            let mut pages = 0u32;
            while pages < MAX_PAGES && !cancel.load(Ordering::Relaxed) {
                let changes = match tokio::time::timeout(Duration::from_secs(60), transport.sync_contact_items(&folder, state.as_ref(), PAGE)).await {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) if e.invalidates_sync_state() && !reset_once => {
                        warn!(folder_pk = f.folder_pk, "состояние папки контактов недействительно, синхронизация заново");
                        reset_once = true;
                        state = None;
                        if let Err(e) = self.store.begin_contact_full_sync(f.folder_pk) {
                            warn!(folder_pk = f.folder_pk, error = %e, "состояние папки контактов не сброшено");
                        }
                        continue;
                    }
                    Ok(Err(e)) => {
                        warn!(folder_pk = f.folder_pk, error = %e, pages, "контакты папки не синхронизированы");
                        failed += 1;
                        break;
                    }
                    Err(_) => {
                        warn!(folder_pk = f.folder_pk, pages, "контакты папки не синхронизированы: сервер не ответил за 60 с");
                        failed += 1;
                        break;
                    }
                };
                pages += 1;
                let applied = match self.store.apply_contact_batch(self.account, f.folder_pk, &changes.changes, &changes.state, changes.includes_last) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(folder_pk = f.folder_pk, error = %e, "пачка контактов не записана");
                        failed += 1;
                        break;
                    }
                };
                stats.created += applied.created;
                stats.updated += applied.updated;
                stats.deleted += applied.deleted;
                let empty = changes.changes.is_empty();
                state = Some(changes.state);
                if changes.includes_last || empty {
                    break;
                }
            }
        }
        let changed = stats.created + stats.updated + stats.deleted;
        if changed > 0 || failed > 0 {
            info!(folders = folders.len(), created = stats.created, updated = stats.updated, deleted = stats.deleted, failed, elapsed_ms = started.elapsed().as_millis() as u64, "личные контакты синхронизированы");
        } else {
            debug!(folders = folders.len(), elapsed_ms = started.elapsed().as_millis() as u64, "личные контакты: изменений нет");
        }
        if changed > 0
            && let Err(e) = app.emit("contacts-changed", ContactsChanged { created: stats.created, updated: stats.updated, deleted: stats.deleted })
        {
            warn!(error = %e, "событие contacts-changed не отправлено");
        }
    }

    /// Письма окна тел, скачанные до того, как клиент стал запрашивать последнее действие
    /// (ответ, пересылка; docs/obkatka.md п. 20): дозапрос `GetItem` формой списка пачками по 25,
    /// до 1000 писем за прогон; письмо, по которому сервер отказал, считается проверенным.
    /// Остановка синхронизации прерывает дозапрос перед следующей пачкой.
    async fn backfill_verbs(&self, transport: &OwaTransport, body_days: Option<u32>, cancel: &AtomicBool) {
        const BATCH: usize = 25;
        const PER_RUN: usize = 1000;
        let days = body_days.unwrap_or(365);
        let since = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * i64::from(days));
        let rows = match self.store.items_to_check_verbs(self.account, since, PER_RUN) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "письма без сведений о последнем действии не выбраны");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let (mut checked, mut with_verb) = (0usize, 0usize);
        for chunk in rows.chunks(BATCH) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let ids: Vec<ItemId> = chunk.iter().map(|(_, id)| id.clone()).collect();
            let results = match tokio::time::timeout(Duration::from_secs(60), transport.get_items(&ids, aiowa_transport::ItemFetchShape::Meta)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!(error = %e.chain(), checked, "дозапрос последнего действия прерван отказом сервера");
                    return;
                }
                Err(_) => {
                    warn!(checked, "дозапрос последнего действия прерван: сервер не ответил за 60 с");
                    return;
                }
            };
            for ((pk, _), r) in chunk.iter().zip(results) {
                let (verb, at) = match r {
                    aiowa_transport::ItemResult::Ok(m) => (m.meta.last_verb, m.meta.last_verb_at),
                    aiowa_transport::ItemResult::Failed { code, .. } => {
                        info!(item_pk = pk, code, "последнее действие не получено, письмо считается проверенным");
                        (None, None)
                    }
                };
                if verb.is_some() {
                    with_verb += 1;
                }
                match self.store.set_last_verb(*pk, verb, at) {
                    Ok(()) => checked += 1,
                    Err(e) => warn!(item_pk = pk, error = %e, "последнее действие не записано"),
                }
            }
        }
        info!(checked, with_verb, total = rows.len(), elapsed_ms = started.elapsed().as_millis() as u64, "последнее действие дозапрошено по скачанным письмам");
    }

    /// Письма с усечённым ключом изменений (8 символов из ответа `UpdateItem`, записанные до
    /// второго патча; docs/obkatka.md п. 19) перечитываются пачками `GetItem` только с
    /// идентификаторами, полный ключ ложится в базу. Пачка, которую сервер не отдал, ждёт
    /// следующего прогона; письма, которых нет на сервере, уберёт синхронизация. Остановка
    /// синхронизации прерывает починку перед следующей пачкой.
    async fn repair_change_keys(&self, transport: &OwaTransport, cancel: &AtomicBool) {
        const BATCH: usize = 25;
        const PER_RUN: usize = 1000;
        let rows = match self.store.items_with_short_change_keys(self.account, ItemId::MIN_FULL_CHANGE_KEY, PER_RUN) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "письма с усечёнными ключами не выбраны");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        let (mut fixed, mut failed) = (0usize, 0usize);
        for chunk in rows.chunks(BATCH) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let ids: Vec<ItemId> = chunk.iter().map(|(_, id)| id.clone()).collect();
            let results = match tokio::time::timeout(Duration::from_secs(60), transport.refresh_item_ids(&ids)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!(error = %e, fixed, "починка ключей прервана отказом сервера");
                    return;
                }
                Err(_) => {
                    warn!(fixed, "починка ключей прервана: сервер не ответил за 60 с");
                    return;
                }
            };
            for ((pk, _), r) in chunk.iter().zip(results) {
                match r {
                    WriteResult::Ok { id } if id.has_full_change_key() => match self.store.update_item_id(*pk, &id) {
                        Ok(()) => fixed += 1,
                        Err(e) => warn!(item_pk = pk, error = %e, "полный ключ не записан"),
                    },
                    WriteResult::Ok { id } => {
                        failed += 1;
                        warn!(item_pk = pk, ck_len = id.change_key_len(), "сервер снова отдал усечённый ключ");
                    }
                    WriteResult::Failed { code, .. } => {
                        failed += 1;
                        info!(item_pk = pk, code, "ключ письма не перечитан");
                    }
                }
            }
        }
        info!(fixed, failed, total = rows.len(), "усечённые ключи изменений перечитаны");
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
            // Живое окно: дерево и письма видны по ходу прогона, а не только по его концу.
            SyncEvent::FoldersApplied { .. } => self.live.folders_applied(app),
            SyncEvent::ItemsApplied { folder_pk, .. } => self.live.items_applied(app, *folder_pk),
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
