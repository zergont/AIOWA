//! Движок синхронизации: транспорт → хранилище.
//!
//! Порядок: иерархия папок → элементы выбранных папок (метаданные, пачками до 512)
//! → тела за окно `body_days` (пачками `GetItem`) → вложения тех же писем (по одному,
//! с извлечением текста) → вытеснение кэша вложений по квоте. Состояние синхронизации
//! пишется в одной транзакции с изменениями пачки, поэтому прерванный запуск продолжается
//! с места остановки без потерь и без дублей.

use aiowa_core::{AccountId, FolderId, FolderRef, ItemId, SyncState};
use aiowa_extract::Extraction;
use aiowa_store::{AttachmentText, AttachmentTodo, Store, StoreError, SyncFolder, FETCH_FAILED};
use aiowa_transport::{ItemFetchShape, ItemResult, MailTransport, TransportError};
use jiff::{SignedDuration, Timestamp};
use std::future::Future;
use std::path::PathBuf;
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

/// Какие папки синхронизировать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderSelection {
    /// Все почтовые папки (`IPF.Note*`) кроме служебных и их подпапок: входящие,
    /// отправленные, затем остальные по убыванию числа писем.
    AllMail,
    /// Явный список: известные имена (`inbox`, `sentitems`) или отображаемые имена.
    Named(Vec<String>),
}

/// Настройки запуска.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub folders: FolderSelection,
    /// Окно загрузки тел и вложений в днях; `None` = все.
    pub body_days: Option<u32>,
    /// Загружать ли тела.
    pub fetch_bodies: bool,
    /// Загружать ли вложения (только у писем в окне, не инлайновые, до лимита размера).
    pub fetch_attachments: bool,
    /// Лимит размера одного вложения, байт.
    pub max_attachment_bytes: u64,
    /// Квота кэша файлов вложений на диске, байт; лишнее вытесняется по давности обращения.
    pub attachment_cache_bytes: u64,
    /// Предел извлечённого текста одного вложения, символов.
    pub attachment_text_chars: usize,
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
            folders: FolderSelection::AllMail,
            body_days: Some(90),
            fetch_bodies: true,
            fetch_attachments: true,
            max_attachment_bytes: 25 * 1024 * 1024,
            attachment_cache_bytes: 2 * 1024 * 1024 * 1024,
            attachment_text_chars: aiowa_extract::DEFAULT_MAX_CHARS,
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
    pub attachments_fetched: usize,
    pub attachments_failed: usize,
    pub attachments_bytes: u64,
    pub attachments_with_text: usize,
    pub cache_files_evicted: usize,
    pub cache_bytes_evicted: u64,
    pub elapsed: Duration,
}

/// Итог скачивания одного вложения.
#[derive(Debug, Clone)]
pub struct FetchedAttachment {
    pub path: PathBuf,
    pub bytes: usize,
    pub text_chars: usize,
    pub text_state: i64,
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

    /// Папки по выбору пользователя, в порядке синхронизации.
    pub fn plan_folders(&self, selection: &FolderSelection) -> Result<Vec<SyncFolder>> {
        match selection {
            FolderSelection::AllMail => Ok(self.store.folders_for_sync(self.account)?),
            FolderSelection::Named(names) => {
                let mut out: Vec<SyncFolder> = Vec::new();
                for name in names {
                    let f = self.store.resolve_folder(self.account, name)?.ok_or_else(|| SyncError::FolderNotFound(name.clone()))?;
                    if !out.iter().any(|x| x.folder_pk == f.folder_pk) {
                        out.push(f);
                    }
                }
                Ok(out)
            }
        }
    }

    pub async fn run(&self, opts: &SyncOptions) -> Result<SyncReport> {
        let started = std::time::Instant::now();
        let mut report = SyncReport::default();

        report.folders_in_hierarchy = self.sync_hierarchy().await?;
        let plan = self.plan_folders(&opts.folders)?;
        info!(folders = plan.len(), names = %plan.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "), "папки к синхронизации");

        for f in &plan {
            self.check_cancel()?;
            self.sync_items(f, opts, &mut report).await?;
            report.folders_synced += 1;
        }

        let since = opts.body_days.map(|days| Timestamp::now() - SignedDuration::from_hours(24 * i64::from(days)));
        if opts.fetch_bodies {
            for f in &plan {
                self.check_cancel()?;
                self.fetch_bodies(f, since, opts, &mut report).await?;
            }
        }
        if opts.fetch_attachments {
            for f in &plan {
                self.check_cancel()?;
                self.fetch_attachments(f, since, opts, &mut report).await?;
            }
            let evicted = self.store.evict_attachment_cache(opts.attachment_cache_bytes)?;
            report.cache_files_evicted = evicted.files_removed;
            report.cache_bytes_evicted = evicted.bytes_freed;
            if evicted.files_removed > 0 {
                info!(files = evicted.files_removed, mb = evicted.bytes_freed / 1_048_576, left_mb = evicted.bytes_left / 1_048_576, "кэш вложений ужат до квоты");
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

    async fn sync_items(&self, folder: &SyncFolder, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let folder_pk = folder.folder_pk;
        let name = folder.name.as_str();
        let folder_ref = FolderRef::Id(FolderId { id: folder.server_id.clone(), change_key: None });
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

    async fn fetch_bodies(&self, folder: &SyncFolder, since: Option<Timestamp>, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let folder_pk = folder.folder_pk;
        let name = folder.name.as_str();
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

    /// Вложения писем папки в окне: по одному, с учётом результата по каждому; неустранимый
    /// отказ по вложению помечается и не останавливает остальные.
    async fn fetch_attachments(&self, folder: &SyncFolder, since: Option<Timestamp>, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let folder_pk = folder.folder_pk;
        let name = folder.name.as_str();
        let max = i64::try_from(opts.max_attachment_bytes).unwrap_or(i64::MAX);
        let pending = self.store.count_attachments_to_fetch(self.account, folder_pk, since, max)?;
        if pending == 0 {
            return Ok(());
        }
        info!(folder = name, pending, "загрузка вложений");
        let mut done = 0i64;
        loop {
            self.check_cancel()?;
            let batch = self.store.attachments_to_fetch(self.account, folder_pk, since, max, 8)?;
            if batch.is_empty() {
                break;
            }
            for todo in &batch {
                self.check_cancel()?;
                match self.fetch_one_attachment(todo, opts.max_attachment_bytes, opts.attachment_text_chars).await {
                    Ok(f) => {
                        report.attachments_fetched += 1;
                        report.attachments_bytes += f.bytes as u64;
                        if f.text_state == aiowa_store::TEXT_OK {
                            report.attachments_with_text += 1;
                        }
                        done += 1;
                    }
                    Err(SyncError::Transport(e)) if !e.is_retryable() && !e.needs_reauth() => {
                        warn!(folder = name, attachment = todo.name.as_deref().unwrap_or("?"), error = %e, "вложение не скачано");
                        self.store.mark_attachment_failed(todo.attachment_pk, FETCH_FAILED)?;
                        report.attachments_failed += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
            if done % 40 == 0 {
                info!(folder = name, done, pending, "вложения: прогресс");
            }
        }
        info!(folder = name, done, "вложения загружены");
        Ok(())
    }

    /// Скачивает одно вложение, извлекает текст (в блокирующем потоке) и кладёт в кэш.
    /// Ошибка транспорта возвращается как есть: решение о пометке принимает вызывающий.
    pub async fn fetch_one_attachment(&self, todo: &AttachmentTodo, max_bytes: u64, max_chars: usize) -> Result<FetchedAttachment> {
        let transport = self.transport.clone();
        let id = todo.server_id.clone();
        let content = with_retry("GetFileAttachment", || {
            let t = transport.clone();
            let id = id.clone();
            async move { t.get_attachment(&id, max_bytes).await }
        })
        .await?;
        let content_type = content.content_type.clone().or_else(|| todo.content_type.clone());
        let name = todo.name.clone().or(content.file_name.clone());
        let bytes = content.bytes;
        let (bytes, extraction) = tokio::task::spawn_blocking(move || {
            let e = aiowa_extract::extract_text(name.as_deref(), content_type.as_deref(), &bytes, max_chars);
            (bytes, e)
        })
        .await
        .map_err(|e| SyncError::Store(StoreError::Other(format!("извлечение текста прервано: {e}"))))?;
        let (text, text_state, text_chars) = match extraction {
            Extraction::Text(t) => {
                let n = t.chars().count();
                (AttachmentText::Extracted(t), aiowa_store::TEXT_OK, n)
            }
            Extraction::Unsupported => (AttachmentText::Unsupported, aiowa_store::TEXT_UNSUPPORTED, 0),
            Extraction::Failed(e) => (AttachmentText::Failed(e), aiowa_store::TEXT_FAILED, 0),
        };
        let content_type = content.content_type.clone().or_else(|| todo.content_type.clone());
        let path = self.store.save_attachment(todo, &bytes, content_type.as_deref(), text)?;
        Ok(FetchedAttachment { path, bytes: bytes.len(), text_chars, text_state })
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
