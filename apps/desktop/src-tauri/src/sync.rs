//! Синхронизация из окна: один транспорт OWA на процесс (ленивый вход, logoff при выходе),
//! одна синхронизация одновременно, ход отправляется в окно событием `sync-progress`.
//!
//! Пароль берётся из диспетчера учётных данных Windows (служба `AIOWA`, ключ = логин),
//! как в `aiowa set-password`; формы ввода пароля в окне нет.

use aiowa_core::AccountId;
use aiowa_store::Store;
use aiowa_sync::{Engine, FolderSelection, SyncEvent, SyncOptions, SyncReport, SyncStage};
use aiowa_transport::{Credentials, MailTransport};
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use secrecy::SecretString;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
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
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

pub struct SyncState {
    inner: Mutex<Inner>,
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

impl SyncState {
    pub fn new(store: Arc<Store>, account: AccountId, host: String, user: String) -> Self {
        let last_ok_ms = store.get_state(account, LAST_OK_KEY).ok().flatten().and_then(|v| v.parse::<i64>().ok());
        let status = SyncStatusDto { last_ok_ms, ..SyncStatusDto::default() };
        Self { inner: Mutex::new(Inner { status, ..Inner::default() }), store, account, host, user }
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
    pub fn start(self: &Arc<Self>, app: AppHandle, days: Option<u32>) -> SyncStatusDto {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut g = self.lock();
            if g.status.running {
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
            this.run(app_for_task, days, cancel).await;
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

    /// Транспорт процесса: вход при первом обращении, дальше та же сессия.
    async fn transport(&self) -> Result<OwaTransport, String> {
        if let Some(t) = self.lock().transport.clone() {
            return Ok(t);
        }
        let password = load_password(&self.user)?;
        let creds = Credentials { host: self.host.clone(), user: self.user.clone(), password };
        let t = OwaTransport::connect(creds, OwaConfig::default()).await.map_err(|e| format!("вход не удался: {e}"))?;
        let mut g = self.lock();
        // Параллельный вход невозможен: синхронизация одна, а это единственный вызывающий.
        g.transport = Some(t.clone());
        Ok(t)
    }

    async fn run(self: Arc<Self>, app: AppHandle, days: Option<u32>, cancel: Arc<AtomicBool>) {
        let outcome = self.run_inner(&app, days, cancel).await;
        let mut dropped_transport = false;
        match &outcome {
            Ok(report) => {
                let ms = now_ms();
                if let Err(e) = self.store.set_state(self.account, LAST_OK_KEY, Some(&ms.to_string())) {
                    warn!(error = %e, "не удалось сохранить время синхронизации");
                }
                info!(elapsed_s = report.elapsed.as_secs_f32(), created = report.items_created, updated = report.items_updated, bodies = report.bodies_fetched, attachments = report.attachments_fetched, "синхронизация из окна завершена");
            }
            Err(Outcome::Cancelled) => info!("синхронизация остановлена пользователем"),
            Err(Outcome::Failed { message, drop_transport }) => {
                warn!(error = %message, "синхронизация из окна прервана");
                dropped_transport = *drop_transport;
            }
        }
        if dropped_transport {
            // Сессия мертва (вход отклонён, повторный вход не помог): следующий запуск войдёт заново.
            let old = self.lock().transport.take();
            if let Some(t) = old {
                let _ = tokio::time::timeout(LOGOFF_TIMEOUT, t.logoff()).await;
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
                Ok(report) => {
                    s.last_ok_ms = Some(finished_ms);
                    s.last_error = None;
                    s.last_report = Some(ReportDto::from(report));
                }
                Err(Outcome::Cancelled) => s.last_error = Some("остановлено".to_string()),
                Err(Outcome::Failed { message, .. }) => s.last_error = Some(message.clone()),
            }
        });
        let mut g = self.lock();
        g.task = None;
        g.cancel = None;
    }

    async fn run_inner(self: &Arc<Self>, app: &AppHandle, days: Option<u32>, cancel: Arc<AtomicBool>) -> Result<SyncReport, Outcome> {
        let transport = self.transport().await.map_err(|message| Outcome::Failed { message, drop_transport: false })?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Outcome::Cancelled);
        }
        let opts = SyncOptions {
            folders: FolderSelection::AllMail,
            body_days: match days {
                Some(0) => None,
                Some(d) => Some(d),
                None => SyncOptions::default().body_days,
            },
            ..SyncOptions::default()
        };
        let progress: aiowa_sync::ProgressFn = {
            let app = app.clone();
            let st = self.clone();
            Arc::new(move |e: &SyncEvent| st.on_event(&app, e))
        };
        let engine = Engine::new(Arc::new(transport.clone()), self.store.clone(), self.account, cancel).on_progress(progress);
        match engine.run(&opts).await {
            Ok(r) => Ok(r),
            Err(aiowa_sync::SyncError::Cancelled) => Err(Outcome::Cancelled),
            Err(aiowa_sync::SyncError::Transport(e)) => {
                let drop_transport = e.needs_reauth() || matches!(e, aiowa_transport::TransportError::AuthFailed(_));
                Err(Outcome::Failed { message: e.to_string(), drop_transport })
            }
            Err(e) => Err(Outcome::Failed { message: e.to_string(), drop_transport: false }),
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
        let (cancel, task, transport) = {
            let mut g = self.lock();
            (g.cancel.take(), g.task.take(), g.transport.take())
        };
        if let Some(c) = cancel {
            c.store(true, Ordering::Relaxed);
        }
        tauri::async_runtime::block_on(async move {
            if let Some(t) = task {
                if tokio::time::timeout(EXIT_GRACE, t).await.is_err() {
                    warn!("синхронизация не остановилась за {} с, выхожу из сессии без ожидания", EXIT_GRACE.as_secs());
                }
            }
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
    Failed { message: String, drop_transport: bool },
}

// --- команды Tauri -----------------------------------------------------------

#[tauri::command]
pub fn sync_start(app: AppHandle, state: tauri::State<'_, Arc<SyncState>>, days: Option<u32>) -> SyncStatusDto {
    state.inner().start(app, days)
}

#[tauri::command]
pub fn sync_status(state: tauri::State<'_, Arc<SyncState>>) -> SyncStatusDto {
    state.status()
}

#[tauri::command]
pub fn sync_cancel(app: AppHandle, state: tauri::State<'_, Arc<SyncState>>) -> SyncStatusDto {
    state.cancel(&app)
}
