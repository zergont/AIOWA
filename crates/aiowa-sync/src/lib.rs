//! Движок синхронизации: транспорт → хранилище.
//!
//! Порядок: иерархия папок → свежие письма папок, которые ещё ни разу не синхронизировались
//! (последняя страница по дате, `FindItem`) → элементы выбранных папок (метаданные, пачками до
//! 512) → тела за окно `body_days` (пачками `GetItem`) → вложения тех же писем (по одному,
//! с извлечением текста) → вытеснение кэша вложений по квоте. Состояние синхронизации
//! пишется в одной транзакции с изменениями пачки, поэтому прерванный запуск продолжается
//! с места остановки без потерь и без дублей. О каждой записи в базу (дерево, свежая страница,
//! пачка с изменениями) движок сообщает событием: окно показывает данные по ходу прогона.

use aiowa_core::{AccountId, FolderChange, FolderId, FolderRef, ItemChange, ItemId, SyncState};
use aiowa_extract::Extraction;
use aiowa_store::{AttachmentText, AttachmentTodo, Store, StoreError, SyncFolder, FETCH_FAILED};
use aiowa_transport::{ItemFetchShape, ItemResult, MailTransport, TransportError};
use jiff::{SignedDuration, Timestamp};
use std::collections::HashSet;
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
    /// Явный список по первичным ключам папок в локальной базе (кнопка на папке в окне).
    Ids(Vec<i64>),
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
    /// Свежие письма первыми: у папки без состояния синхронизации до полной выгрузки берётся
    /// последняя страница из стольких писем по дате получения (0 = не брать).
    pub recent_first: u32,
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
            recent_first: 100,
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
    /// Писем свежих страниц (`FindItem`) у папок, синхронизируемых впервые.
    pub recent_items: usize,
    /// Писем свежих страниц, которых полная выгрузка не прислала (на сервере их уже нет):
    /// помечены удалёнными, входят и в `items_deleted`.
    pub recent_vanished: usize,
    pub elapsed: Duration,
}

/// Итог загрузки тела одного письма по требованию (`Engine::fetch_body`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyFetchOutcome {
    /// Тело сохранено, `body_state = 1`.
    Stored,
    /// Письма на сервере больше нет (`ErrorItemNotFound`); локально помечено удалённым.
    Gone,
    /// Сервер отказал по этому письму; локально `body_state = 2`.
    Failed { code: String, message: String },
}

/// Итог скачивания одного вложения.
#[derive(Debug, Clone)]
pub struct FetchedAttachment {
    pub path: PathBuf,
    pub bytes: usize,
    pub text_chars: usize,
    pub text_state: i64,
}

/// Этап синхронизации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStage {
    /// Иерархия папок.
    Hierarchy,
    /// Свежие письма папки, синхронизируемой впервые (`FindItem`, последняя страница по дате).
    Recent,
    /// Метаданные писем папки (`SyncFolderItems`).
    Items,
    /// Тела писем за окно.
    Bodies,
    /// Вложения писем за окно.
    Attachments,
}

/// Событие хода синхронизации. Отправляется движком через колбэк `Engine::on_progress`;
/// подходит для строки состояния: папка, сколько сделано, сколько ожидается.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// Начался этап; `folder` = `None` для иерархии.
    Stage { stage: SyncStage, folder: Option<String> },
    /// Продвижение внутри этапа: `done` элементов обработано; `total` известно не всегда
    /// (при инкрементальной выгрузке сервер не сообщает, сколько изменений впереди).
    Progress { stage: SyncStage, folder: Option<String>, done: u64, total: Option<u64> },
    /// Ошибка, не прервавшая синхронизацию (отвергнутая пачка тел, нескачанное вложение).
    Warning { stage: SyncStage, folder: Option<String>, message: String },
    /// Дерево папок записано в базу: `upserts` папок создано или изменено (имена, счётчики),
    /// `deletes` помечено удалёнными. Окну пора перечитать дерево. Отправляется при любых
    /// изменениях дерева, в том числе только удалениях; без изменений — нет.
    FoldersApplied { upserts: usize, deletes: usize },
    /// Письма папки записаны в базу: свежая страница (`recent`) или пачка `SyncFolderItems`, в
    /// которой что-то изменилось (пустые пачки события не дают). `changed` — создано, обновлено,
    /// удалено и пометок прочтения вместе. Окну пора перечитать список этой папки.
    ItemsApplied { folder_pk: i64, folder: String, changed: usize, recent: bool },
}

/// Приёмник событий хода. Вызывается из асинхронного контекста, должен быть быстрым.
pub type ProgressFn = Arc<dyn Fn(&SyncEvent) + Send + Sync>;

pub struct Engine {
    transport: Arc<dyn MailTransport>,
    store: Arc<Store>,
    account: AccountId,
    cancel: Arc<AtomicBool>,
    progress: Option<ProgressFn>,
}

impl Engine {
    pub fn new(transport: Arc<dyn MailTransport>, store: Arc<Store>, account: AccountId, cancel: Arc<AtomicBool>) -> Self {
        Self { transport, store, account, cancel, progress: None }
    }

    /// Подписка на события хода. Один приёмник; повторный вызов заменяет предыдущий.
    pub fn on_progress(mut self, f: ProgressFn) -> Self {
        self.progress = Some(f);
        self
    }

    fn emit(&self, event: SyncEvent) {
        if let Some(p) = &self.progress {
            p(&event);
        }
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
            FolderSelection::Ids(pks) => {
                let mut out: Vec<SyncFolder> = Vec::new();
                for pk in pks {
                    let f = self.store.folder_by_pk(self.account, *pk)?.ok_or_else(|| SyncError::FolderNotFound(format!("#{pk}")))?;
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
        let mut report = SyncReport { folders_in_hierarchy: self.sync_hierarchy().await?, ..Default::default() };
        let plan = self.plan_folders(&opts.folders)?;
        info!(folders = plan.len(), names = %plan.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "), "папки к синхронизации");

        // Свежие письма первыми: последние страницы всех впервые синхронизируемых папок (в порядке
        // плана, «Входящие» первыми) — до полных выгрузок, которые на большом ящике идут десятки минут.
        if opts.recent_first > 0 {
            for f in &plan {
                self.check_cancel()?;
                self.fetch_recent(f, opts.recent_first, &mut report).await?;
            }
        }

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
    /// Открыт отдельно от `run`: окно зовёт его после операций с папками (создание,
    /// переименование, удаление), когда письма перечитывать не нужно. Возвращает число upsert-ов.
    pub async fn sync_hierarchy(&self) -> Result<usize> {
        self.emit(SyncEvent::Stage { stage: SyncStage::Hierarchy, folder: None });
        let mut state = self.store.get_state(self.account, "hierarchy")?.map(SyncState);
        let mut reset_once = false;
        let mut total = 0usize;
        let mut deletes = 0usize;
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
            deletes += changes.changes.iter().filter(|c| matches!(c, FolderChange::Delete(_))).count();
            rounds += 1;
            let done = changes.includes_last || changes.changes.is_empty() || rounds >= 50;
            state = Some(changes.state);
            if done {
                break;
            }
        }
        info!(folders = total, deleted = deletes, "иерархия папок синхронизирована");
        self.emit(SyncEvent::Progress { stage: SyncStage::Hierarchy, folder: None, done: total as u64, total: Some(total as u64) });
        // Удаление папки на сервере (без upsert-ов) тоже меняет дерево в окне.
        if total > 0 || deletes > 0 {
            self.emit(SyncEvent::FoldersApplied { upserts: total, deletes });
        }
        Ok(total)
    }

    /// Последняя страница писем папки, которую ещё ни разу не синхронизировали (нет состояния
    /// `SyncFolderItems`): `FindItem` по дате получения по убыванию, письма — в базу как обычные
    /// заголовки, без состояния папки. Полная выгрузка потом присылает те же письма `Create`-ами
    /// с теми же идентификаторами, и они ложатся поверх (upsert по идентификатору: без дублей,
    /// тело, вложения и место в индексе сохраняются, флаги — как на сервере в момент выгрузки).
    ///
    /// Идентификаторы страницы запоминаются в `sync_state` (`recent_pending.<папка>`): письмо,
    /// которого полная выгрузка так и не прислала, к её концу сверяется с сервером и, если его там
    /// уже нет (удалено или перенесено, пока шла выгрузка), помечается удалённым (`drop_vanished`).
    /// Прерванный запуск продолжает с тем же списком. Отказ сервера синхронизацию не останавливает:
    /// папка просто выгружается обычным порядком.
    async fn fetch_recent(&self, folder: &SyncFolder, max: u32, report: &mut SyncReport) -> Result<()> {
        let folder_pk = folder.folder_pk;
        let name = folder.name.as_str();
        if folder.total_count == 0 || self.store.folder_sync_state(folder_pk)?.is_some() || !self.load_pending(folder_pk)?.is_empty() {
            return Ok(());
        }
        self.emit(SyncEvent::Stage { stage: SyncStage::Recent, folder: Some(name.to_string()) });
        let folder_ref = FolderRef::Id(FolderId { id: folder.server_id.clone(), change_key: None });
        let started = std::time::Instant::now();
        let metas = match self.transport.find_recent_items(&folder_ref, max).await {
            Ok(m) => m,
            Err(e) => {
                warn!(folder = name, error = %e, "свежие письма не получены, папка выгружается обычным порядком");
                return Ok(());
            }
        };
        if metas.is_empty() {
            return Ok(());
        }
        // Сначала список, потом письма: запуск, прерванный между ними, оставит лишь лишние
        // идентификаторы (их пометка удалёнными ничего не найдёт), а не письма без учёта.
        let pending: HashSet<String> = metas.iter().map(|m| m.item_id.id.clone()).collect();
        self.save_pending(folder_pk, &pending)?;
        let changes: Vec<ItemChange> = metas.into_iter().map(ItemChange::Create).collect();
        let stats = self.store.apply_item_changes(self.account, folder_pk, &changes)?;
        report.recent_items += changes.len();
        report.items_created += stats.created;
        report.items_updated += stats.updated;
        let n = changes.len() as u64;
        self.emit(SyncEvent::Progress { stage: SyncStage::Recent, folder: Some(name.to_string()), done: n, total: Some(n) });
        self.emit(SyncEvent::ItemsApplied { folder_pk, folder: name.to_string(), changed: changes.len(), recent: true });
        info!(folder = name, items = changes.len(), created = stats.created, updated = stats.updated, ms = started.elapsed().as_millis() as u64, "свежие письма папки записаны");
        Ok(())
    }

    fn pending_key(folder_pk: i64) -> String {
        format!("recent_pending.{folder_pk}")
    }

    /// Идентификаторы свежей страницы папки, которых полная выгрузка ещё не прислала.
    fn load_pending(&self, folder_pk: i64) -> Result<HashSet<String>> {
        Ok(self.store.get_state(self.account, &Self::pending_key(folder_pk))?.map(|v| v.lines().filter(|l| !l.is_empty()).map(String::from).collect()).unwrap_or_default())
    }

    /// Пустой список хранится пустой строкой (`get_state` не читает NULL).
    fn save_pending(&self, folder_pk: i64, ids: &HashSet<String>) -> Result<()> {
        let value = ids.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
        Ok(self.store.set_state(self.account, &Self::pending_key(folder_pk), Some(&value))?)
    }

    /// Конец выгрузки папки: снять с учёта свежую страницу (`drop_vanished`) и сообщить окну,
    /// если что-то пропало.
    async fn finish_recent(&self, folder: &SyncFolder, pending: &mut HashSet<String>, report: &mut SyncReport) -> Result<()> {
        let n = self.drop_vanished(folder, pending).await?;
        if n > 0 {
            report.recent_vanished += n;
            report.items_deleted += n;
            self.emit(SyncEvent::ItemsApplied { folder_pk: folder.folder_pk, folder: folder.name.clone(), changed: n, recent: false });
        }
        Ok(())
    }

    /// Полная выгрузка папки дошла до конца: письма свежей страницы, которых она так и не
    /// прислала, скорее всего удалены или перенесены на сервере за время выгрузки (у перенесённого
    /// письма новый идентификатор, старый сервер больше не знает). Проверяются только письма, которые
    /// всё ещё лежат в этой папке (перенесённое из окна живёт в другой), и удалёнными помечаются только
    /// те, по которым сервер ответил `ErrorItemNotFound`: отсутствие в выгрузке само по себе не довод
    /// (список пишется после пачки, и прерванный между ними запуск оставил бы в нём уже присланные
    /// письма). Сервер не ответил — список остаётся до следующей синхронизации. Возвращает, сколько помечено.
    async fn drop_vanished(&self, folder: &SyncFolder, pending: &mut HashSet<String>) -> Result<usize> {
        if pending.is_empty() {
            return Ok(0);
        }
        let mut candidates: Vec<ItemId> = Vec::new();
        for id in pending.iter() {
            if let Some((pk, false)) = self.store.item_pk_by_server_id(self.account, id)?
                && self.store.item_ref(pk)?.is_some_and(|r| r.folder_pk == folder.folder_pk)
            {
                candidates.push(ItemId { id: id.clone(), change_key: None });
            }
        }
        let mut vanished: Vec<ItemId> = Vec::new();
        if !candidates.is_empty() {
            let transport = self.transport.clone();
            let ids = candidates.clone();
            let checked = with_retry("GetItem", || {
                let t = transport.clone();
                let ids = ids.clone();
                async move { t.get_items(&ids, ItemFetchShape::Meta).await }
            })
            .await;
            match checked {
                Ok(results) => vanished.extend(results.into_iter().filter_map(|r| match r {
                    ItemResult::Failed { id, code, .. } if code == "ErrorItemNotFound" => Some(id),
                    _ => None,
                })),
                Err(SyncError::Transport(e)) => {
                    warn!(folder = folder.name.as_str(), candidates = candidates.len(), error = %e, "письма свежей страницы не проверены, повтор при следующей синхронизации");
                    return Ok(0);
                }
                Err(e) => return Err(e),
            }
        }
        let n = if vanished.is_empty() { 0 } else { self.store.mark_items_deleted(self.account, &vanished)? };
        pending.clear();
        self.save_pending(folder.folder_pk, pending)?;
        info!(folder = folder.name.as_str(), checked = candidates.len(), vanished = n, "свежая страница сверена с полной выгрузкой");
        Ok(n)
    }

    async fn sync_items(&self, folder: &SyncFolder, opts: &SyncOptions, report: &mut SyncReport) -> Result<()> {
        let folder_pk = folder.folder_pk;
        let name = folder.name.as_str();
        let folder_ref = FolderRef::Id(FolderId { id: folder.server_id.clone(), change_key: None });
        let mut state = self.store.folder_sync_state(folder_pk)?;
        // При полной выгрузке ожидаемый объём известен из счётчика папки; при инкрементальной — нет.
        let expected = if state.is_none() { Some(folder.total_count.max(0) as u64) } else { None };
        let mut processed = 0u64;
        // Свежая страница, которую полная выгрузка ещё не подтвердила (`fetch_recent`).
        let mut pending = self.load_pending(folder_pk)?;
        self.emit(SyncEvent::Stage { stage: SyncStage::Items, folder: Some(name.to_string()) });
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
            processed += changes.changes.len() as u64;
            self.emit(SyncEvent::Progress { stage: SyncStage::Items, folder: Some(name.to_string()), done: processed, total: expected });
            info!(folder = name, page = pages, changes = changes.changes.len(), created = stats.created, updated = stats.updated, deleted = stats.deleted, read_flags = stats.read_flags, elapsed_s = started.elapsed().as_secs(), "пачка применена");
            let changed = stats.created + stats.updated + stats.deleted + stats.read_flags;
            if changed > 0 {
                self.emit(SyncEvent::ItemsApplied { folder_pk, folder: name.to_string(), changed, recent: false });
            }
            if !pending.is_empty() {
                let before = pending.len();
                for ch in &changes.changes {
                    let id = match ch {
                        ItemChange::Create(m) | ItemChange::Update(m) => &m.item_id.id,
                        ItemChange::Delete(id) | ItemChange::ReadFlag { item_id: id, .. } => &id.id,
                    };
                    pending.remove(id);
                }
                if pending.len() != before {
                    self.save_pending(folder_pk, &pending)?;
                }
            }
            if (pages as usize).is_multiple_of(opts.checkpoint_every) {
                self.store.checkpoint()?;
            }
            if changes.includes_last {
                self.finish_recent(folder, &mut pending, report).await?;
                break;
            }
            if empty_streak >= 3 || same_state_streak >= 3 {
                if empty_streak >= 3 {
                    // Пустая выдача трижды подряд: сервер больше ничего не отдаёт, считаем папку завершённой.
                    self.store.set_folder_sync_state(folder_pk, state.as_ref(), true)?;
                    self.finish_recent(folder, &mut pending, report).await?;
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
        self.emit(SyncEvent::Stage { stage: SyncStage::Bodies, folder: Some(name.to_string()) });
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
            // Помечаем пачку отказом только по ответу сервера об операции; отклонённый вход,
            // истёкшая сессия, сеть и неожиданные HTTP-ответы останавливают запуск, иначе
            // временная беда закрепилась бы в базе как `body_state = 2` навсегда.
            let results = match result {
                Ok(r) => r,
                Err(SyncError::Transport(e)) if e.is_item_failure() => {
                    warn!(folder = name, error = %e, count = ids.len(), "пачка тел отвергнута целиком, помечена как ошибка");
                    self.emit(SyncEvent::Warning { stage: SyncStage::Bodies, folder: Some(name.to_string()), message: format!("пачка тел ({}) отвергнута: {e}", ids.len()) });
                    self.store.mark_body_failed(self.account, &ids)?;
                    report.bodies_failed += ids.len();
                    done += ids.len() as i64;
                    self.emit(SyncEvent::Progress { stage: SyncStage::Bodies, folder: Some(name.to_string()), done: done as u64, total: Some(pending as u64) });
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
            done += stored as i64 + gone.len() as i64 + failed.len() as i64;
            self.emit(SyncEvent::Progress { stage: SyncStage::Bodies, folder: Some(name.to_string()), done: done as u64, total: Some(pending as u64) });
            batches += 1;
            if batches.is_multiple_of(10) {
                info!(folder = name, done, pending, "тела: прогресс");
            }
            if batches.is_multiple_of(opts.checkpoint_every) {
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
        self.emit(SyncEvent::Stage { stage: SyncStage::Attachments, folder: Some(name.to_string()) });
        let mut done = 0i64;
        let mut handled = 0u64;
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
                    // По вложению поэлементны также слишком большой ответ и неожиданный ответ на запрос
                    // файла (например, не файловое вложение); сессия, вход и сеть — нет.
                    Err(SyncError::Transport(e)) if e.is_item_failure() || matches!(e, TransportError::TooLarge { .. }) || (matches!(e, TransportError::Protocol { .. }) && !e.is_retryable()) => {
                        // Имя файла — в предупреждении для окна, в журнал только ключ (политика журналов).
                        warn!(folder = name, attachment_pk = todo.attachment_pk, error = %e, "вложение не скачано");
                        self.emit(SyncEvent::Warning { stage: SyncStage::Attachments, folder: Some(name.to_string()), message: format!("вложение «{}» не скачано: {e}", todo.name.as_deref().unwrap_or("?")) });
                        self.store.mark_attachment_failed(todo.attachment_pk, FETCH_FAILED)?;
                        report.attachments_failed += 1;
                    }
                    Err(e) => return Err(e),
                }
                handled += 1;
                self.emit(SyncEvent::Progress { stage: SyncStage::Attachments, folder: Some(name.to_string()), done: handled, total: Some(pending as u64) });
            }
            if done % 40 == 0 {
                info!(folder = name, done, pending, "вложения: прогресс");
            }
        }
        info!(folder = name, done, "вложения загружены");
        Ok(())
    }

    /// Тело одного письма по требованию (открытие в окне): зеркало `fetch_bodies` для одного
    /// идентификатора. Отказ сервера по письму записывается в базу и возвращается как итог;
    /// временные ошибки повторяются, а истёкшая сессия, отклонённый вход, сеть и исчерпанные
    /// повторы приходят как `Err` без записи в базу — вызывающий решает, сбрасывать ли транспорт.
    pub async fn fetch_body(&self, id: &ItemId, folder_pk: i64) -> Result<BodyFetchOutcome> {
        let transport = self.transport.clone();
        let ids = vec![id.clone()];
        let ids_for_call = ids.clone();
        let result = with_retry("GetItem", || {
            let t = transport.clone();
            let ids = ids_for_call.clone();
            async move { t.get_items(&ids, ItemFetchShape::WithUniqueBody).await }
        })
        .await;
        let results = match result {
            Ok(r) => r,
            Err(SyncError::Transport(e)) if e.is_item_failure() => {
                warn!(item = %id.id, error = %e, "тело по требованию: вызов отвергнут, помечено как ошибка");
                self.store.mark_body_failed(self.account, &ids)?;
                let (code, message) = match e {
                    TransportError::Server { code, message } => (code, message),
                    other => ("TransportError".to_string(), other.to_string()),
                };
                return Ok(BodyFetchOutcome::Failed { code, message });
            }
            Err(e) => return Err(e),
        };
        let Some(first) = results.into_iter().next() else {
            self.store.mark_body_failed(self.account, &ids)?;
            return Ok(BodyFetchOutcome::Failed { code: "EmptyResponse".into(), message: "сервер вернул пустой ответ".into() });
        };
        match first {
            ItemResult::Ok(m) => {
                self.store.store_full_messages(self.account, folder_pk, &[m])?;
                Ok(BodyFetchOutcome::Stored)
            }
            ItemResult::Failed { id, code, message } => {
                if code == "ErrorItemNotFound" {
                    self.store.mark_items_deleted(self.account, &[id])?;
                    Ok(BodyFetchOutcome::Gone)
                } else {
                    warn!(item = %id.id, code, message = %message, "тело по требованию не загружено");
                    self.store.mark_body_failed(self.account, &[id])?;
                    Ok(BodyFetchOutcome::Failed { code, message })
                }
            }
        }
    }

    /// Скачивает одно вложение, извлекает текст (в блокирующем потоке) и кладёт в кэш.
    /// Ошибка транспорта возвращается как есть: решение о пометке принимает вызывающий.
    /// Для картинок (`image/*`) извлечение сразу даёт `Unsupported`, то есть почти бесплатно.
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
