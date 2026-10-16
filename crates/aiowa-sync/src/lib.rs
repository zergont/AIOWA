//! Движок синхронизации: транспорт → хранилище.
//!
//! Порядок: иерархия папок → элементы выбранных папок (метаданные, пачками до 512)
//! → тела за окно `body_days` (пачками `GetItem`). Состояние синхронизации пишется
//! в одной транзакции с изменениями пачки, поэтому прерванный запуск продолжается
//! с места остановки без потерь и без дублей.

use aiowa_core::{AccountId, DistinguishedFolder, FolderId, FolderRef, ItemId, SyncState};
use aiowa_store::{Store, StoreError};
use aiowa_transport::{ItemFetchShape, ItemResult, MailTransport, TransportError};
use jiff::{SignedDuration, Timestamp};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("папка {0} не найдена в иерархии")]
    FolderNotFound(String),
    #[error("папка {0}: сервер {1} раз подряд вернул пустую выдачу без признака конца")]
    StuckFolder(String, u32),
    #[error("отменено пользователем")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, SyncError>;

/// Настройки запуска.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Известные папки для синхронизации элементов.
    pub folders: Vec<DistinguishedFolder>,
    /// Окно загрузки тел в днях; `None` = все.
    pub body_days: Option<u32>,
    /// Загружать ли тела.
    pub fetch_bodies: bool,
    /// Размер пачки `SyncFolderItems` (сервер ограничивает 512).
    pub page_size: u32,
    /// Размер выборки элементов без тел за один шаг.
    pub body_batch: usize,
    /// Ограничение числа пачек `SyncFolderItems` на папку (0 = без ограничения). Для проб.
    pub max_pages: u32,
    /// Контрольная точка WAL каждые N пачек.
    pub checkpoint_every: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            folders: vec![DistinguishedFolder::Inbox, DistinguishedFolder::SentItems],
            body_days: Some(90),
            fetch_bodies: true,
            page_size: 512,
            body_batch: 25,
            max_pages: 0,
            checkpoint_every: 20,
        }
    }
}

/// Итог запуска.
#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    pub folders_in_hierarchy: usize,
    pub folders_synced: usize,
    pub pages: usize,
    pub items_created: usize,
    pub items_updated: usize,
    pub items_deleted: usize,
    pub read_flags: usize,
    pub bodies_fetched: usize,
    pub bodies_failed: usize,
    pub bodies_gone: usize,
    pub elapsed: Duration,
}

pub struct Engine {
    transport: Arc<dyn MailTransport>,
    store: Arc<Store>,
    account: AccountId,
    cancel: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(transport: Arc<dyn MailTransport>, store: Arc<Store>, account: AccountId, cancel: Arc<AtomicBool>) -> Self {
        Self { transport, store, account, cancel }
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(SyncError::Cancelled) } else { Ok(()) }
    }

    pub async fn run(&self, opts: &SyncOptions) -> Result<SyncReport> {
        let started = std::time::Instant::now();
        let mut report = SyncReport::default();

        report.folders_in_hierarchy = self.sync_hierarchy().await?;

        for d in &opts.folders {
            self.check_cancel()?;
            let Some((folder_pk, server_id)) = self.store.folder_by_distinguished(self.account, d.as_str())? else {
                warn!(folder = %d, "папки нет в иерархии, пропуск");
                continue;
            };
            self.sync_items(folder_pk, &server_id, d.as_str(), opts, &mut report).await?;
            report.folders_synced += 1;
        }

        if opts.fetch_bodies {
            let since = opts.body_days.map(|days| Timestamp::now() - SignedDuration::from_hours(24 * i64::from(days)));
            for d in &opts.folders {
                self.check_cancel()?;
                let Some((folder_pk, _)) = self.store.folder_by_distinguished(self.account, d.as_str())? else { continue };
                self.fetch_bodies(folder_pk, d.as_str(), since, opts, &mut report).await?;
            }
        }

        self.store.checkpoint()?;
        report.elapsed = started.elapsed();
        Ok(report)
    }

    /// Иерархия папок с сохранённым состоянием; при недействительном состоянии — заново.
    async fn sync_hierarchy(&self) -> Result<usize> {
        let mut state = self.store.get_state(self.account, "hierarchy")?.map(SyncState);
        let mut reset_once = false;
        let mut total = 0usize;
        let mut rounds = 0u32;
        loop {
            self.check_cancel()?;
            let transport = self.transport.clone();
            let st = state.clone();
            let result = with_retry("SyncFolderHierarchy", || {
                let t = transport.clone();
                let st = st.clone();
                async move { t.sync_folder_hierarchy(st.as_ref()).await }
            })
            .await;
            let changes = match result {
                Ok(c) => c,
                Err(SyncError::Transport(e)) if e.invalidates_sync_state() && !reset_once => {
                    warn!("состояние иерархии недействительно, полная выгрузка");
                    reset_once = true;
                    state = None;
                    continue;
                }
                Err(e) => return Err(e),
            };
            total += self.store.apply_hierarchy_batch(self.account, &changes.changes, &changes.state)?;
            rounds += 1;
            let done = changes.includes_last || changes.changes.is_empty() || rounds >= 50;
            state = Some(changes.state);
            if done {
                break;
            }
        }
        info!(folders = total, "иерархия папок синхронизирована");
        Ok(total)
    }

    async fn sync_items(&self, folder_pk: i64, server_id: &str, name: &str, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let folder_ref = FolderRef::Id(FolderId { id: server_id.to_string(), change_key: None });
        let mut state = self.store.folder_sync_state(folder_pk)?;
        let mut reset_once = false;
        let mut pages = 0u32;
        let mut empty_streak = 0u32;
        let mut same_state_streak = 0u32;
        let started = std::time::Instant::now();
        loop {
            self.check_cancel()?;
            if opts.max_pages > 0 && pages >= opts.max_pages {
                info!(folder = name, pages, "достигнут предел пачек, папка синхронизирована частично");
                break;
            }
            let transport = self.transport.clone();
            let fr = folder_ref.clone();
            let st = state.clone();
            let page = opts.page_size;
            let result = with_retry("SyncFolderItems", || {
                let t = transport.clone();
                let fr = fr.clone();
                let st = st.clone();
                async move { t.sync_folder_items(&fr, st.as_ref(), page, true).await }
            })
            .await;
            let changes = match result {
                Ok(c) => c,
                Err(SyncError::Transport(e)) if e.invalidates_sync_state() && !reset_once => {
                    warn!(folder = name, "состояние папки недействительно, синхронизация заново");
                    reset_once = true;
                    state = None;
                    self.store.set_folder_sync_state(folder_pk, None, false)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            pages += 1;
            report.pages += 1;
            // Предохранители от зацикливания: пустые выдачи без признака конца и неизменное состояние.
            if changes.changes.is_empty() {
                empty_streak += 1;
            } else {
                empty_streak = 0;
            }
            if state.as_ref() == Some(&changes.state) {
                same_state_streak += 1;
            } else {
                same_state_streak = 0;
            }
            let stats = self.store.apply_item_batch(self.account, folder_pk, &changes.changes, &changes.state, changes.includes_last)?;
            report.items_created += stats.created;
            report.items_updated += stats.updated;
            report.items_deleted += stats.deleted;
            report.read_flags += stats.read_flags;
            state = Some(changes.state);
            info!(folder = name, page = pages, changes = changes.changes.len(), created = stats.created, updated = stats.updated, deleted = stats.deleted, read_flags = stats.read_flags, elapsed_s = started.elapsed().as_secs(), "пачка применена");
            if pages as usize % opts.checkpoint_every == 0 {
                self.store.checkpoint()?;
            }
            if changes.includes_last {
                break;
            }
            if empty_streak >= 3 || same_state_streak >= 3 {
                if empty_streak >= 3 {
                    // Пустая выдача трижды подряд: сервер больше ничего не отдаёт, считаем папку завершённой.
                    self.store.set_folder_sync_state(folder_pk, state.as_ref(), true)?;
                    break;
                }
                return Err(SyncError::StuckFolder(name.to_string(), same_state_streak));
            }
        }
        Ok(())
    }

    async fn fetch_bodies(&self, folder_pk: i64, name: &str, since: Option<Timestamp>, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let pending = self.store.count_items_without_bodies(self.account, folder_pk, since)?;
        if pending == 0 {
            return Ok(());
        }
        info!(folder = name, pending, "загрузка тел");
        let mut done = 0i64;
        let mut batches = 0usize;
        loop {
            self.check_cancel()?;
            let batch = self.store.items_without_bodies(self.account, folder_pk, since, opts.body_batch)?;
            if batch.is_empty() {
                break;
            }
            let ids: Vec<ItemId> = batch.iter().map(|(_, id)| id.clone()).collect();
            let transport = self.transport.clone();
            let ids_for_call = ids.clone();
            let result = with_retry("GetItem", || {
                let t = transport.clone();
                let ids = ids_for_call.clone();
                async move { t.get_items(&ids, ItemFetchShape::WithUniqueBody).await }
            })
            .await;
            let results = match result {
                Ok(r) => r,
                Err(SyncError::Transport(e)) if !e.is_retryable() && !e.needs_reauth() => {
                    warn!(folder = name, error = %e, count = ids.len(), "пачка тел отвергнута целиком, помечена как ошибка");
                    self.store.mark_body_failed(self.account, &ids)?;
                    report.bodies_failed += ids.len();
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mut messages = Vec::with_capacity(results.len());
            let mut gone = Vec::new();
            let mut failed = Vec::new();
            for r in results {
                match r {
                    ItemResult::Ok(m) => messages.push(m),
                    ItemResult::Failed { id, code, message } => {
                        if code == "ErrorItemNotFound" {
                            gone.push(id);
                        } else {
                            warn!(folder = name, code, message = %message, "тело не загружено");
                            failed.push(id);
                        }
                    }
                }
            }
            let stored = self.store.store_full_messages(self.account, folder_pk, &messages)?;
            if !gone.is_empty() {
                report.bodies_gone += self.store.mark_items_deleted(self.account, &gone)?;
            }
            if !failed.is_empty() {
                self.store.mark_body_failed(self.account, &failed)?;
                report.bodies_failed += failed.len();
            }
            report.bodies_fetched += stored;
            done += stored as i64;
            batches += 1;
            if batches % 10 == 0 {
                info!(folder = name, done, pending, "тела: прогресс");
            }
            if batches % opts.checkpoint_every == 0 {
                self.store.checkpoint()?;
            }
        }
        info!(folder = name, done, "тела загружены");
        Ok(())
    }
}

/// Повторы для временных ошибок: троттлинг, сеть, лимит сессий. Истечение сессии
/// транспорт обрабатывает сам (один повторный вход), сюда оно приходит уже как окончательное.
async fn with_retry<T, F, Fut>(op: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, TransportError>>,
{
    let mut attempt = 0u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < 6 => {
                attempt += 1;
                let delay = match &e {
                    TransportError::Throttled { retry_after: Some(d) } => *d,
                    TransportError::SessionLimit => Duration::from_secs(60),
                    _ => Duration::from_secs(2u64.pow(attempt).min(60)),
                };
                warn!(op, attempt, delay_s = delay.as_secs_f32(), error = %e, "временная ошибка, повтор");
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}
