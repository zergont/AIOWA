//! Локальное хранилище AIOWA: один файл SQLite (`mail.db`), WAL, FTS5, кэш вложений рядом.
//!
//! Правила: сервер источник истины для почты, локально уникальны только индексы и
//! состояние синхронизации. Удаления сервера зеркалятся флагом `deleted`.
//! Тела писем хранятся сжатыми (zstd), для поиска хранится текстовая выжимка.
//! Беседы (`conversations`, `conversation_folders`) пересчитываются в той же транзакции,
//! что меняет письма. Файлы вложений лежат в каталоге `attachments` рядом с базой и
//! вытесняются по квоте; извлечённый текст остаётся в базе и после вытеснения.

pub mod query;
pub mod schema;
pub mod text;
pub mod ui;

use aiowa_core::{AccountId, Folder, FolderChange, FolderId, ItemChange, ItemId, MessageFull, MessageMeta, SyncState};
use jiff::Timestamp;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{debug, info, warn};

pub use schema::{FETCH_FAILED, TEXT_FAILED, TEXT_OK, TEXT_PENDING, TEXT_UNSUPPORTED};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("ввод-вывод: {0}")]
    Io(#[from] std::io::Error),
    #[error("сжатие: {0}")]
    Compression(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Хранилище. Потокобезопасно за счёт мьютекса на соединении; операции короткие.
pub struct Store {
    path: PathBuf,
    attachments_dir: PathBuf,
    conn: Mutex<Connection>,
}

/// Статистика применения пачки изменений.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub read_flags: usize,
}

/// Строка списка писем.
#[derive(Debug, Clone)]
pub struct ItemRow {
    pub item_pk: i64,
    pub item_id: ItemId,
    pub folder_pk: i64,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub received_at: Option<Timestamp>,
    pub is_read: bool,
    pub has_attachments: bool,
    pub body_state: i64,
}

/// Результат поиска.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub item_pk: i64,
    pub rank: f64,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub received_at: Option<Timestamp>,
    pub folder_name: Option<String>,
    /// Фрагмент с совпадениями в квадратных скобках.
    pub snippet: String,
    /// Имя файла, если совпадение найдено в тексте вложения, а не в письме.
    pub attachment_name: Option<String>,
    /// Беседа письма; нужна интерфейсу, чтобы открыть результат в области чтения.
    pub conversation_pk: Option<i64>,
}

/// Сводка по папке для `stats`.
#[derive(Debug, Clone)]
pub struct FolderStats {
    pub display_name: String,
    pub distinguished: Option<String>,
    pub server_total: i64,
    pub local_items: i64,
    pub local_bodies: i64,
    pub oldest: Option<Timestamp>,
    pub newest: Option<Timestamp>,
    pub synced: bool,
}

#[derive(Debug, Clone)]
pub struct StoreStats {
    pub db_bytes: u64,
    pub items: i64,
    pub bodies: i64,
    pub conversations: i64,
    pub attachments: i64,
    pub attachments_fetched: i64,
    pub attachments_with_text: i64,
    pub attachment_cache_bytes: i64,
    pub folders: Vec<FolderStats>,
}

/// Папка, выбранная для синхронизации элементов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFolder {
    pub folder_pk: i64,
    pub server_id: String,
    pub name: String,
    pub distinguished: Option<String>,
    pub total_count: i64,
}

/// Вложение, которое надо скачать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTodo {
    pub attachment_pk: i64,
    pub item_pk: i64,
    pub server_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
}

/// Строка вложения для показа.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub attachment_pk: i64,
    pub item_pk: i64,
    pub server_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub is_inline: bool,
    /// `Content-ID` инлайновой картинки, как прислал сервер (без угловых скобок на этом сервере,
    /// но сопоставлять с `src="cid:…"` надо терпимо к скобкам, регистру и URL-кодированию).
    pub content_id: Option<String>,
    pub local_path: Option<PathBuf>,
    pub text_state: i64,
    pub text_len: i64,
    pub fetched_at: Option<Timestamp>,
}

impl AttachmentRow {
    /// Задание на скачивание этого вложения (для `Engine::fetch_one_attachment`).
    pub fn todo(&self) -> AttachmentTodo {
        AttachmentTodo { attachment_pk: self.attachment_pk, item_pk: self.item_pk, server_id: self.server_id.clone(), name: self.name.clone(), content_type: self.content_type.clone(), size: self.size }
    }

    /// MIME-тип вложения — картинка (`image/*`)?
    pub fn is_image(&self) -> bool {
        self.content_type.as_deref().map(|c| c.trim_start().to_ascii_lowercase().starts_with("image/")).unwrap_or(false)
    }
}

/// Ссылка на письмо для запроса к серверу: его папка, идентификатор и состояние тела.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRef {
    pub item_pk: i64,
    pub folder_pk: i64,
    pub id: ItemId,
    /// 0 тела нет, 1 есть, 2 загрузка не удалась.
    pub body_state: i64,
}

/// Колонки `attachments` в порядке, который разбирает `attachment_row`.
const ATTACHMENT_COLUMNS: &str = "attachment_pk, item_pk, server_id, name, content_type, size, is_inline, content_id, local_path, text_state, COALESCE(length(text), 0), fetched_at";

fn attachment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentRow> {
    Ok(AttachmentRow {
        attachment_pk: r.get(0)?,
        item_pk: r.get(1)?,
        server_id: r.get(2)?,
        name: r.get(3)?,
        content_type: r.get(4)?,
        size: r.get(5)?,
        is_inline: r.get::<_, i64>(6)? != 0,
        content_id: r.get(7)?,
        local_path: r.get::<_, Option<String>>(8)?.map(PathBuf::from),
        text_state: r.get(9)?,
        text_len: r.get(10)?,
        fetched_at: ms_ts(r.get(11)?),
    })
}

/// Итог извлечения текста вложения, который записывает синхронизация.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentText {
    Extracted(String),
    Unsupported,
    Failed(String),
}

/// Беседа в папке, как её показывает список OWA.
#[derive(Debug, Clone)]
pub struct ConversationRow {
    pub conversation_pk: i64,
    pub topic: Option<String>,
    /// Отправители писем беседы, свежие первыми, без повторов.
    pub participants: Vec<String>,
    /// Писем беседы в этой папке и непрочитанных среди них.
    pub item_count: i64,
    pub unread_count: i64,
    /// Писем во всей беседе (во всех папках).
    pub total_count: i64,
    pub last_received_at: Option<Timestamp>,
    pub has_attachments: bool,
}

/// Итог вытеснения кэша вложений.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvictReport {
    pub files_removed: usize,
    pub bytes_freed: u64,
    pub bytes_left: u64,
}

/// Служебные папки, которые не синхронизируются при выборе «все почтовые»; подпапки тоже.
pub const SERVICE_FOLDERS: &[&str] = &["junkemail", "deleteditems", "drafts", "outbox", "conversationhistory", "syncissues", "conflicts", "localfailures", "serverfailures"];

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

fn ts_ms(t: Option<Timestamp>) -> Option<i64> {
    t.map(|t| t.as_millisecond())
}

fn ms_ts(v: Option<i64>) -> Option<Timestamp> {
    v.and_then(|ms| Timestamp::from_millisecond(ms).ok())
}

fn compress(s: &str) -> Result<Vec<u8>> {
    zstd::encode_all(s.as_bytes(), 3).map_err(|e| StoreError::Compression(e.to_string()))
}

pub fn decompress(b: &[u8]) -> Result<String> {
    let bytes = zstd::decode_all(b).map_err(|e| StoreError::Compression(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| StoreError::Compression(e.to_string()))
}

/// Ставит на файл метку «получен из интернета» (поток `Zone.Identifier`, зона 3 — как делают
/// браузеры и Outlook): SmartScreen предупредит о запуске исполняемого вложения, Office откроет
/// документ в защищённом просмотре. Только Windows и только NTFS; в остальных случаях — нет операции.
/// Ошибка не фатальна: файл уже записан, вызывающий лишь пишет предупреждение в журнал.
pub fn mark_from_internet(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let stream = format!("{}:Zone.Identifier", path.display());
        std::fs::write(stream, b"[ZoneTransfer]\r\nZoneId=3\r\n")
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(())
    }
}

/// Имя файла для кэша: без символов, запрещённых в Windows, и не длиннее 120 символов.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    let out: String = trimmed.chars().take(120).collect();
    if out.is_empty() { "attachment".to_string() } else { out }
}

impl Store {
    /// Открывает (создавая при необходимости) базу и применяет миграции.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let attachments_dir = path.parent().map(|d| d.join("attachments")).unwrap_or_else(|| PathBuf::from("attachments"));
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -65536;",
        )?;
        schema::register_functions(&conn)?;
        schema::migrate(&conn)?;
        info!(path = %path.display(), "хранилище открыто");
        Ok(Self { path, attachments_dir, conn: Mutex::new(conn) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Каталог кэша вложений рядом с базой.
    pub fn attachments_dir(&self) -> &Path {
        &self.attachments_dir
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| StoreError::Other("мьютекс соединения отравлен".into()))?;
        f(&conn)
    }

    fn with_tx<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| StoreError::Other("мьютекс соединения отравлен".into()))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    // --- аккаунты -----------------------------------------------------------

    pub fn account(&self, host: &str, login: &str) -> Result<AccountId> {
        self.with_conn(|c| {
            if let Some(id) = c.query_row("SELECT account_id FROM accounts WHERE host = ?1 AND login = ?2", params![host, login], |r| r.get::<_, i64>(0)).optional()? {
                return Ok(AccountId(id));
            }
            c.execute("INSERT INTO accounts(host, login, created_at) VALUES (?1, ?2, ?3)", params![host, login, now_ms()])?;
            Ok(AccountId(c.last_insert_rowid()))
        })
    }

    // --- состояние ----------------------------------------------------------

    pub fn get_state(&self, account: AccountId, key: &str) -> Result<Option<String>> {
        self.with_conn(|c| Ok(c.query_row("SELECT value FROM sync_state WHERE account_id = ?1 AND key = ?2", params![account.0, key], |r| r.get(0)).optional()?))
    }

    pub fn set_state(&self, account: AccountId, key: &str, value: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO sync_state(account_id, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![account.0, key, value, now_ms()],
            )?;
            Ok(())
        })
    }

    // --- папки --------------------------------------------------------------

    pub fn upsert_folders(&self, account: AccountId, folders: &[Folder]) -> Result<()> {
        self.with_tx(|tx| {
            let now = now_ms();
            for f in folders {
                upsert_folder(tx, account, f, now)?;
            }
            Ok(())
        })
    }

    pub fn mark_folder_deleted(&self, account: AccountId, id: &FolderId) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE folders SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id, now_ms()])?;
            Ok(())
        })
    }

    pub fn folder_pk(&self, account: AccountId, server_id: &str) -> Result<Option<i64>> {
        self.with_conn(|c| Ok(c.query_row("SELECT folder_pk FROM folders WHERE account_id = ?1 AND server_id = ?2", params![account.0, server_id], |r| r.get(0)).optional()?))
    }

    /// Папка по известному имени (`inbox`, `sentitems`, ...): (folder_pk, server_id).
    pub fn folder_by_distinguished(&self, account: AccountId, name: &str) -> Result<Option<(i64, String)>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT folder_pk, server_id FROM folders WHERE account_id = ?1 AND distinguished = ?2 AND deleted = 0",
                params![account.0, name],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
        })
    }

    /// Папка по известному имени или по отображаемому имени без учёта регистра и ё.
    pub fn resolve_folder(&self, account: AccountId, name: &str) -> Result<Option<SyncFolder>> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT folder_pk, server_id, display_name, distinguished, total_count FROM folders
                 WHERE account_id = ?1 AND deleted = 0 AND (distinguished = ?2 OR norm_lower(display_name) = norm_lower(?2))
                 ORDER BY (distinguished = ?2) DESC, total_count DESC LIMIT 1",
            )?;
            Ok(st.query_row(params![account.0, name], |r| Ok(SyncFolder { folder_pk: r.get(0)?, server_id: r.get(1)?, name: r.get(2)?, distinguished: r.get(3)?, total_count: r.get(4)? })).optional()?)
        })
    }

    /// Папка по первичному ключу (для синхронизации одной папки из окна).
    pub fn folder_by_pk(&self, account: AccountId, folder_pk: i64) -> Result<Option<SyncFolder>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT folder_pk, server_id, display_name, distinguished, total_count FROM folders
                 WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0",
            )?;
            Ok(st.query_row(params![account.0, folder_pk], |r| Ok(SyncFolder { folder_pk: r.get(0)?, server_id: r.get(1)?, name: r.get(2)?, distinguished: r.get(3)?, total_count: r.get(4)? })).optional()?)
        })
    }

    /// Все почтовые папки для синхронизации: класс `IPF.Note*`, не удалённые, кроме служебных
    /// (нежелательная, удалённые, черновики, исходящие, журнал бесед, ошибки синхронизации) и их
    /// подпапок. Служебная папка, которую пользователь уже синхронизировал отдельно (есть состояние
    /// синхронизации), остаётся в наборе: раз попросили, дальше обновляем сами.
    /// Порядок: входящие, отправленные, затем по убыванию числа писем.
    pub fn folders_for_sync(&self, account: AccountId) -> Result<Vec<SyncFolder>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT folder_pk, server_id, display_name, distinguished, total_count, parent_server_id, folder_class,
                        sync_state IS NOT NULL
                 FROM folders WHERE account_id = ?1 AND deleted = 0",
            )?;
            struct Row {
                f: SyncFolder,
                parent: Option<String>,
                class: Option<String>,
                synced: bool,
            }
            let rows: Vec<Row> = st
                .query_map(params![account.0], |r| {
                    Ok(Row {
                        f: SyncFolder { folder_pk: r.get(0)?, server_id: r.get(1)?, name: r.get(2)?, distinguished: r.get(3)?, total_count: r.get(4)? },
                        parent: r.get(5)?,
                        class: r.get(6)?,
                        synced: r.get::<_, i64>(7)? != 0,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let by_id: HashMap<&str, &Row> = rows.iter().map(|r| (r.f.server_id.as_str(), r)).collect();
            let is_service = |r: &Row| r.f.distinguished.as_deref().map(|d| SERVICE_FOLDERS.contains(&d)).unwrap_or(false);
            let mut out: Vec<SyncFolder> = Vec::new();
            for r in &rows {
                let class_ok = r.class.as_deref().map(|c| c == "IPF.Note" || c.starts_with("IPF.Note.")).unwrap_or(false);
                if !class_ok {
                    continue;
                }
                if r.synced {
                    out.push(r.f.clone());
                    continue;
                }
                if is_service(r) {
                    continue;
                }
                // Подпапки служебных папок: идём по цепочке родителей (с защитой от цикла).
                let mut excluded = false;
                let mut cur = r.parent.as_deref();
                let mut hops = 0;
                while let Some(pid) = cur {
                    hops += 1;
                    if hops > 64 {
                        break;
                    }
                    match by_id.get(pid) {
                        Some(p) => {
                            if is_service(p) {
                                excluded = true;
                                break;
                            }
                            cur = p.parent.as_deref();
                        }
                        None => break,
                    }
                }
                if !excluded {
                    out.push(r.f.clone());
                }
            }
            let priority = |f: &SyncFolder| match f.distinguished.as_deref() {
                Some("inbox") => 0,
                Some("sentitems") => 1,
                _ => 2,
            };
            out.sort_by(|a, b| priority(a).cmp(&priority(b)).then(b.total_count.cmp(&a.total_count)).then(a.name.cmp(&b.name)));
            Ok(out)
        })
    }

    pub fn list_folders(&self, account: AccountId) -> Result<Vec<(i64, Folder)>> {
        self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT folder_pk, server_id, change_key, parent_server_id, distinguished, display_name, folder_class, total_count, unread_count, child_count
                 FROM folders WHERE account_id = ?1 AND deleted = 0 ORDER BY display_name",
            )?;
            let rows = st.query_map(params![account.0], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Folder {
                        id: FolderId { id: r.get(1)?, change_key: r.get(2)? },
                        parent_id: r.get::<_, Option<String>>(3)?.map(|id| FolderId { id, change_key: None }),
                        distinguished: r.get(4)?,
                        display_name: r.get(5)?,
                        folder_class: r.get(6)?,
                        total_count: r.get(7)?,
                        unread_count: r.get(8)?,
                        child_folder_count: r.get(9)?,
                    },
                ))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn folder_sync_state(&self, folder_pk: i64) -> Result<Option<SyncState>> {
        self.with_conn(|c| Ok(c.query_row("SELECT sync_state FROM folders WHERE folder_pk = ?1", params![folder_pk], |r| r.get::<_, Option<String>>(0)).optional()?.flatten().map(SyncState)))
    }

    pub fn set_folder_sync_state(&self, folder_pk: i64, state: Option<&SyncState>, complete: bool) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE folders SET sync_state = ?2, sync_complete = ?3, last_synced_at = ?4 WHERE folder_pk = ?1",
                params![folder_pk, state.map(|s| s.0.as_str()), complete as i64, now_ms()],
            )?;
            Ok(())
        })
    }

    // --- элементы -----------------------------------------------------------

    /// Применяет пачку изменений `SyncFolderItems` и новое состояние папки одной транзакцией:
    /// состояние никогда не уходит вперёд без изменений и не отстаёт от них.
    pub fn apply_item_batch(&self, account: AccountId, folder_pk: i64, changes: &[ItemChange], state: &SyncState, complete: bool) -> Result<ApplyStats> {
        self.with_tx(|tx| {
            let stats = apply_changes_in_tx(tx, account, folder_pk, changes)?;
            tx.execute(
                "UPDATE folders SET sync_state = ?2, sync_complete = ?3, last_synced_at = ?4 WHERE folder_pk = ?1",
                params![folder_pk, state.0, complete as i64, now_ms()],
            )?;
            Ok(stats)
        })
    }

    /// Применяет пачку изменений иерархии и её состояние одной транзакцией. Возвращает число upsert-ов.
    pub fn apply_hierarchy_batch(&self, account: AccountId, changes: &[FolderChange], state: &SyncState) -> Result<usize> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut upserts = 0usize;
            for ch in changes {
                match ch {
                    FolderChange::Create(f) | FolderChange::Update(f) => {
                        upsert_folder(tx, account, f, now)?;
                        upserts += 1;
                    }
                    FolderChange::Delete(id) => {
                        tx.execute("UPDATE folders SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id, now])?;
                    }
                }
            }
            tx.execute(
                "INSERT INTO sync_state(account_id, key, value, updated_at) VALUES (?1, 'hierarchy', ?2, ?3)
                 ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![account.0, state.0, now],
            )?;
            Ok(upserts)
        })
    }

    /// Применяет пачку изменений `SyncFolderItems` одной транзакцией (без состояния).
    pub fn apply_item_changes(&self, account: AccountId, folder_pk: i64, changes: &[ItemChange]) -> Result<ApplyStats> {
        self.with_tx(|tx| apply_changes_in_tx(tx, account, folder_pk, changes))
    }

    /// Помечает элементы удалёнными (например, `ErrorItemNotFound` при загрузке тела).
    pub fn mark_items_deleted(&self, account: AccountId, ids: &[ItemId]) -> Result<usize> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut touched = Touched::default();
            let mut n = 0;
            for id in ids {
                if let Some(st) = item_state(tx, account, &id.id)? {
                    fts_delete(tx, st.item_pk)?;
                    touched.add(st.conversation_pk, st.folder_pk);
                }
                n += tx.execute("UPDATE items SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2 AND deleted = 0", params![account.0, id.id, now])?;
            }
            touched.recompute(tx)?;
            Ok(n)
        })
    }

    /// Элементы папки без тела, не старше `since`, самые свежие первыми.
    pub fn items_without_bodies(&self, account: AccountId, folder_pk: i64, since: Option<Timestamp>, limit: usize) -> Result<Vec<(i64, ItemId)>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT item_pk, server_id, change_key FROM items
                 WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0 AND body_state = 0
                   AND (?3 IS NULL OR received_at >= ?3)
                 ORDER BY received_at DESC LIMIT ?4",
            )?;
            let rows = st.query_map(params![account.0, folder_pk, ts_ms(since), limit as i64], |r| Ok((r.get::<_, i64>(0)?, ItemId { id: r.get(1)?, change_key: r.get(2)? })))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn count_items_without_bodies(&self, account: AccountId, folder_pk: i64, since: Option<Timestamp>) -> Result<i64> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM items WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0 AND body_state = 0 AND (?3 IS NULL OR received_at >= ?3)",
                params![account.0, folder_pk, ts_ms(since)],
                |r| r.get(0),
            )?)
        })
    }

    /// Сохраняет письма целиком: метаданные, получатели, тело, вложения, индекс, беседы.
    pub fn store_full_messages(&self, account: AccountId, folder_pk: i64, messages: &[MessageFull]) -> Result<usize> {
        let (stored, stale_files) = self.with_tx(|tx| {
            let now = now_ms();
            let mut stored = 0;
            let mut touched = Touched::default();
            let mut stale_files: Vec<PathBuf> = Vec::new();
            for m in messages {
                // Снять старую версию с индекса до любых изменений текста.
                let prev = item_state(tx, account, &m.meta.item_id.id)?;
                if let Some(p) = &prev {
                    fts_delete(tx, p.item_pk)?;
                    touched.add(p.conversation_pk, p.folder_pk);
                }
                let up = upsert_meta(tx, account, folder_pk, &m.meta, now)?;
                if let Some((c, f)) = up.adopted_from {
                    touched.add(c, f);
                }
                let item_pk = up.item_pk;
                let conv_pk = link_conversation(tx, account, item_pk, &m.meta)?;
                touched.add(Some(conv_pk), folder_pk);

                tx.execute("DELETE FROM recipients WHERE item_pk = ?1", params![item_pk])?;
                let mut rec = tx.prepare_cached("INSERT INTO recipients(item_pk, kind, name, email, position) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for (kind, list) in [(1i64, &m.to), (2, &m.cc), (3, &m.bcc)] {
                    for (i, p) in list.iter().enumerate() {
                        rec.execute(params![item_pk, kind, p.name, p.email, i as i64])?;
                    }
                }

                // Вложения: upsert по (item_pk, server_id), чтобы не терять скачанное; исчезнувшие удаляем.
                let mut att = tx.prepare_cached(
                    "INSERT INTO attachments(item_pk, server_id, name, content_type, size, is_inline, content_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(item_pk, server_id) DO UPDATE SET name = excluded.name, content_type = COALESCE(excluded.content_type, attachments.content_type),
                       size = COALESCE(excluded.size, attachments.size), is_inline = excluded.is_inline, content_id = excluded.content_id",
                )?;
                for a in &m.attachments {
                    att.execute(params![item_pk, a.id, a.name, a.content_type, a.size, a.is_inline as i64, a.content_id])?;
                }
                {
                    let mut st = tx.prepare_cached("SELECT attachment_pk, server_id, local_path FROM attachments WHERE item_pk = ?1")?;
                    let rows: Vec<(i64, String, Option<String>)> = st.query_map(params![item_pk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
                    for (pk, sid, path) in rows {
                        if !m.attachments.iter().any(|a| a.id == sid) {
                            tx.execute("DELETE FROM attachments WHERE attachment_pk = ?1", params![pk])?;
                            if let Some(p) = path {
                                stale_files.push(PathBuf::from(p));
                            }
                        }
                    }
                }

                let (body_type, unique_blob, full_blob, unique_text, truncated) = match &m.body {
                    Some(b) => {
                        let text_source = b.unique_body.as_deref().or(b.full_body.as_deref()).unwrap_or("");
                        let unique_text = match b.body_type {
                            aiowa_core::BodyType::Html => text::html_to_text(text_source),
                            aiowa_core::BodyType::Text => text::normalize_ws(text_source),
                        };
                        (b.body_type.as_str(), b.unique_body.as_deref().map(compress).transpose()?, b.full_body.as_deref().map(compress).transpose()?, unique_text, b.is_truncated)
                    }
                    None => ("HTML", None, None, String::new(), false),
                };
                tx.execute(
                    "INSERT INTO bodies(item_pk, body_type, unique_body, full_body, unique_text, is_truncated, fetched_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(item_pk) DO UPDATE SET body_type = excluded.body_type, unique_body = excluded.unique_body,
                       full_body = COALESCE(excluded.full_body, bodies.full_body), unique_text = excluded.unique_text,
                       is_truncated = excluded.is_truncated, fetched_at = excluded.fetched_at",
                    params![item_pk, body_type, unique_blob, full_blob, unique_text, truncated as i64, now],
                )?;
                tx.execute("UPDATE items SET body_state = 1, updated_at = ?2 WHERE item_pk = ?1", params![item_pk, now])?;

                fts_insert(tx, item_pk)?;
                stored += 1;
            }
            touched.recompute(tx)?;
            Ok((stored, stale_files))
        })?;
        for p in stale_files {
            let _ = std::fs::remove_file(p);
        }
        Ok(stored)
    }

    pub fn mark_body_failed(&self, account: AccountId, ids: &[ItemId]) -> Result<()> {
        self.with_tx(|tx| {
            let mut st = tx.prepare_cached("UPDATE items SET body_state = 2, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2")?;
            let now = now_ms();
            for id in ids {
                st.execute(params![account.0, id.id, now])?;
            }
            Ok(())
        })
    }

    /// Текст тела для показа или индексации.
    pub fn body_text(&self, item_pk: i64) -> Result<Option<String>> {
        self.with_conn(|c| Ok(c.query_row("SELECT unique_text FROM bodies WHERE item_pk = ?1", params![item_pk], |r| r.get(0)).optional()?))
    }

    // --- вложения -----------------------------------------------------------

    /// Вложения писем папки, которые надо скачать: не инлайновые, ещё не скачанные, в окне
    /// по дате получения и не больше `max_size` байт. Самые свежие письма первыми.
    pub fn attachments_to_fetch(&self, account: AccountId, folder_pk: i64, since: Option<Timestamp>, max_size: i64, limit: usize) -> Result<Vec<AttachmentTodo>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT a.attachment_pk, a.item_pk, a.server_id, a.name, a.content_type, a.size
                 FROM attachments a JOIN items i ON i.item_pk = a.item_pk
                 WHERE i.account_id = ?1 AND i.folder_pk = ?2 AND i.deleted = 0
                   AND a.is_inline = 0 AND a.fetched_at IS NULL AND a.text_state = 0
                   AND (?3 IS NULL OR i.received_at >= ?3) AND (a.size IS NULL OR a.size <= ?4)
                 ORDER BY i.received_at DESC, a.attachment_pk LIMIT ?5",
            )?;
            let rows = st.query_map(params![account.0, folder_pk, ts_ms(since), max_size, limit as i64], |r| {
                Ok(AttachmentTodo { attachment_pk: r.get(0)?, item_pk: r.get(1)?, server_id: r.get(2)?, name: r.get(3)?, content_type: r.get(4)?, size: r.get(5)? })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn count_attachments_to_fetch(&self, account: AccountId, folder_pk: i64, since: Option<Timestamp>, max_size: i64) -> Result<i64> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM attachments a JOIN items i ON i.item_pk = a.item_pk
                 WHERE i.account_id = ?1 AND i.folder_pk = ?2 AND i.deleted = 0
                   AND a.is_inline = 0 AND a.fetched_at IS NULL AND a.text_state = 0
                   AND (?3 IS NULL OR i.received_at >= ?3) AND (a.size IS NULL OR a.size <= ?4)",
                params![account.0, folder_pk, ts_ms(since), max_size],
                |r| r.get(0),
            )?)
        })
    }

    /// Кладёт скачанный файл в кэш и записывает текст; индекс письма пересобирается в той же
    /// транзакции. Возвращает путь к файлу.
    pub fn save_attachment(&self, todo: &AttachmentTodo, bytes: &[u8], content_type: Option<&str>, text: AttachmentText) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.attachments_dir)?;
        let name = safe_file_name(todo.name.as_deref().unwrap_or("attachment"));
        let path = self.attachments_dir.join(format!("{}-{}", todo.attachment_pk, name));
        std::fs::write(&path, bytes)?;
        // Файл пришёл из письма: метка зоны «Интернет», чтобы система предупреждала при открытии.
        if let Err(e) = mark_from_internet(&path) {
            warn!(attachment_pk = todo.attachment_pk, error = %e, "не удалось поставить метку Zone.Identifier на файл кэша");
        }
        let sha: Vec<u8> = Sha256::digest(bytes).to_vec();
        let (text_value, text_state, err) = match text {
            AttachmentText::Extracted(t) => (Some(t), TEXT_OK, None),
            AttachmentText::Unsupported => (None, TEXT_UNSUPPORTED, None),
            AttachmentText::Failed(e) => (None, TEXT_FAILED, Some(e)),
        };
        if let Some(e) = err {
            debug!(attachment_pk = todo.attachment_pk, name = %name, error = %e, "текст вложения не извлечён");
        }
        let path_str = path.to_string_lossy().into_owned();
        self.with_tx(|tx| {
            fts_delete(tx, todo.item_pk)?;
            let now = now_ms();
            tx.execute(
                "UPDATE attachments SET local_path = ?2, sha256 = ?3, text = ?4, text_state = ?5, fetched_at = ?6, last_access_at = ?6,
                   size = ?7, content_type = COALESCE(?8, content_type)
                 WHERE attachment_pk = ?1",
                params![todo.attachment_pk, path_str, sha, text_value, text_state, now, bytes.len() as i64, content_type],
            )?;
            fts_insert(tx, todo.item_pk)?;
            Ok(())
        })?;
        Ok(path)
    }

    /// Отмечает вложение как не скачанное (`FETCH_FAILED`) или с ошибкой извлечения.
    pub fn mark_attachment_failed(&self, attachment_pk: i64, state: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE attachments SET text_state = ?2 WHERE attachment_pk = ?1", params![attachment_pk, state])?;
            Ok(())
        })
    }

    pub fn attachments_of_item(&self, item_pk: i64) -> Result<Vec<AttachmentRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachments WHERE item_pk = ?1 ORDER BY attachment_pk"))?;
            let rows = st.query_map(params![item_pk], attachment_row)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Одно вложение по ключу.
    pub fn attachment(&self, attachment_pk: i64) -> Result<Option<AttachmentRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachments WHERE attachment_pk = ?1"))?;
            Ok(st.query_row(params![attachment_pk], attachment_row).optional()?)
        })
    }

    /// Отмечает обращение к файлу кэша (для вытеснения по давности).
    pub fn touch_attachment(&self, attachment_pk: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE attachments SET last_access_at = ?2 WHERE attachment_pk = ?1", params![attachment_pk, now_ms()])?;
            Ok(())
        })
    }

    /// Забывает путь к файлу кэша (файл исчез с диска помимо вытеснения); текст и хэш остаются.
    pub fn forget_attachment_file(&self, attachment_pk: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE attachments SET local_path = NULL WHERE attachment_pk = ?1", params![attachment_pk])?;
            Ok(())
        })
    }

    /// Ссылка на не удалённое письмо для запроса к серверу.
    pub fn item_ref(&self, item_pk: i64) -> Result<Option<ItemRef>> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT server_id, change_key, folder_pk, body_state FROM items WHERE item_pk = ?1 AND deleted = 0", params![item_pk], |r| {
                Ok(ItemRef { item_pk, folder_pk: r.get(2)?, id: ItemId { id: r.get(0)?, change_key: r.get(1)? }, body_state: r.get(3)? })
            })
            .optional()?)
        })
    }

    /// Возвращает вложение в состояние «не скачано» (для повторной попытки).
    pub fn reset_attachment(&self, attachment_pk: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE attachments SET text_state = 0, fetched_at = NULL WHERE attachment_pk = ?1", params![attachment_pk])?;
            Ok(())
        })
    }

    /// Извлечённый текст вложения.
    pub fn attachment_text(&self, attachment_pk: i64) -> Result<Option<String>> {
        self.with_conn(|c| Ok(c.query_row("SELECT text FROM attachments WHERE attachment_pk = ?1", params![attachment_pk], |r| r.get(0)).optional()?.flatten()))
    }

    /// Вытесняет из кэша самые давно не использованные файлы, пока суммарный размер не
    /// уложится в квоту. Текст и хэш остаются в базе.
    pub fn evict_attachment_cache(&self, quota_bytes: u64) -> Result<EvictReport> {
        let rows: Vec<(i64, String, i64)> = self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT attachment_pk, local_path, COALESCE(size, 0) FROM attachments WHERE local_path IS NOT NULL ORDER BY last_access_at ASC, attachment_pk ASC")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let sizes: Vec<u64> = rows.iter().map(|(_, p, s)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(*s as u64)).collect();
        let mut total: u64 = sizes.iter().sum();
        let mut report = EvictReport { bytes_left: total, ..Default::default() };
        if total <= quota_bytes {
            return Ok(report);
        }
        let mut removed: Vec<i64> = Vec::new();
        for ((pk, path, _), size) in rows.iter().zip(sizes.iter()) {
            if total <= quota_bytes {
                break;
            }
            let gone = match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(_) if !Path::new(path).exists() => true,
                Err(e) => {
                    warn!(path, error = %e, "файл кэша не удалён");
                    false
                }
            };
            if gone {
                total = total.saturating_sub(*size);
                report.bytes_freed += size;
                report.files_removed += 1;
                removed.push(*pk);
            }
        }
        report.bytes_left = total;
        if !removed.is_empty() {
            self.with_tx(|tx| {
                let mut st = tx.prepare_cached("UPDATE attachments SET local_path = NULL WHERE attachment_pk = ?1")?;
                for pk in &removed {
                    st.execute(params![pk])?;
                }
                Ok(())
            })?;
        }
        Ok(report)
    }

    // --- поиск --------------------------------------------------------------

    /// Поиск как в OWA: операторы (`от:`, `кому:`, `тема:`, `вложение:`, `папка:`, `после:`, `до:`)
    /// и префиксный поиск по словам; все условия обязательны. Если текстовых условий нет,
    /// работает как фильтр по письмам, свежие первыми.
    pub fn search(&self, account: AccountId, user_query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let q = query::parse(user_query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let fts = q.fts_match();
        const MARK_OPEN: &str = "\u{1}";
        const MARK_CLOSE: &str = "\u{2}";

        let mut sql = String::new();
        let mut args: Vec<SqlValue> = Vec::new();
        if fts.is_some() {
            sql.push_str(
                "SELECT i.item_pk, bm25(items_fts, 8.0, 4.0, 2.0, 1.0, 1.0, 0.5) AS rank,
                        i.subject, i.from_name, i.from_email, i.received_at, f.display_name,
                        snippet(items_fts, 4, ?1, ?2, '…', 14), snippet(items_fts, 5, ?1, ?2, '…', 14), i.conversation_pk
                 FROM items_fts JOIN items i ON i.item_pk = items_fts.rowid
                 LEFT JOIN folders f ON f.folder_pk = i.folder_pk
                 WHERE items_fts MATCH ?3 AND i.account_id = ?4 AND i.deleted = 0",
            );
            args.push(SqlValue::Text(MARK_OPEN.into()));
            args.push(SqlValue::Text(MARK_CLOSE.into()));
            args.push(SqlValue::Text(fts.clone().unwrap_or_default()));
            args.push(SqlValue::Integer(account.0));
        } else {
            sql.push_str(
                "SELECT i.item_pk, 0.0 AS rank, i.subject, i.from_name, i.from_email, i.received_at, f.display_name,
                        COALESCE(i.preview, ''), '', i.conversation_pk
                 FROM items i LEFT JOIN folders f ON f.folder_pk = i.folder_pk
                 WHERE i.account_id = ?1 AND i.deleted = 0",
            );
            args.push(SqlValue::Integer(account.0));
        }
        if q.has_attachment {
            sql.push_str(" AND i.has_attachments = 1");
        }
        if let Some(a) = q.after_ms {
            args.push(SqlValue::Integer(a));
            sql.push_str(&format!(" AND COALESCE(i.received_at, i.sent_at) >= ?{}", args.len()));
        }
        if let Some(b) = q.before_ms {
            args.push(SqlValue::Integer(b));
            sql.push_str(&format!(" AND COALESCE(i.received_at, i.sent_at) < ?{}", args.len()));
        }
        if let Some(folder) = &q.folder {
            args.push(SqlValue::Text(folder.clone()));
            let n = args.len();
            sql.push_str(&format!(" AND (f.distinguished = ?{n} OR norm_lower(f.display_name) = norm_lower(?{n}) OR norm_lower(f.display_name) LIKE norm_lower(?{n}) || '%')"));
        }
        args.push(SqlValue::Integer(limit as i64));
        // Как в OWA: результаты от новых к старым; релевантность (bm25) только разводит письма
        // с одинаковой датой и остаётся в `SearchHit::rank`.
        sql.push_str(&format!(" ORDER BY COALESCE(i.received_at, i.sent_at) DESC, rank LIMIT ?{}", args.len()));

        self.with_conn(|c| {
            let mut st = c.prepare_cached(&sql)?;
            let rows = st.query_map(params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            })?;
            let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            let mut hits = Vec::with_capacity(raw.len());
            for (item_pk, rank, subject, from_name, from_email, received_at, folder_name, body_snip, att_snip, conversation_pk) in raw {
                let body_hit = body_snip.contains(MARK_OPEN);
                let att_hit = att_snip.contains(MARK_OPEN);
                let (snippet, attachment_name) = if att_hit && !body_hit {
                    let words = marked_words(&att_snip, MARK_OPEN, MARK_CLOSE);
                    (att_snip, attachment_name_for(c, item_pk, &words)?)
                } else {
                    (body_snip, None)
                };
                let snippet = snippet.replace(MARK_OPEN, "[").replace(MARK_CLOSE, "]");
                hits.push(SearchHit { item_pk, rank, subject, from_name, from_email, received_at: ms_ts(received_at), folder_name, snippet, attachment_name, conversation_pk });
            }
            Ok(hits)
        })
    }

    pub fn list_items(&self, account: AccountId, folder_pk: i64, limit: usize) -> Result<Vec<ItemRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT item_pk, server_id, change_key, folder_pk, subject, from_name, from_email, received_at, is_read, has_attachments, body_state
                 FROM items WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0 ORDER BY received_at DESC LIMIT ?3",
            )?;
            let rows = st.query_map(params![account.0, folder_pk, limit as i64], |r| {
                Ok(ItemRow {
                    item_pk: r.get(0)?,
                    item_id: ItemId { id: r.get(1)?, change_key: r.get(2)? },
                    folder_pk: r.get(3)?,
                    subject: r.get(4)?,
                    from_name: r.get(5)?,
                    from_email: r.get(6)?,
                    received_at: ms_ts(r.get(7)?),
                    is_read: r.get::<_, i64>(8)? != 0,
                    has_attachments: r.get::<_, i64>(9)? != 0,
                    body_state: r.get(10)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    // --- беседы -------------------------------------------------------------

    /// Беседы папки, как в списке OWA: свежие первыми, с участниками и счётчиками.
    pub fn list_conversations(&self, account: AccountId, folder_pk: i64, limit: usize) -> Result<Vec<ConversationRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT c.conversation_pk, c.topic, cf.item_count, cf.unread_count, c.item_count, cf.last_received_at, c.has_attachments
                 FROM conversation_folders cf JOIN conversations c ON c.conversation_pk = cf.conversation_pk
                 WHERE cf.folder_pk = ?1 AND c.account_id = ?2
                 ORDER BY cf.last_received_at DESC, c.conversation_pk DESC LIMIT ?3",
            )?;
            let rows: Vec<ConversationRow> = st
                .query_map(params![folder_pk, account.0, limit as i64], |r| {
                    Ok(ConversationRow {
                        conversation_pk: r.get(0)?,
                        topic: r.get(1)?,
                        participants: Vec::new(),
                        item_count: r.get(2)?,
                        unread_count: r.get(3)?,
                        total_count: r.get(4)?,
                        last_received_at: ms_ts(r.get::<_, Option<i64>>(5)?),
                        has_attachments: r.get::<_, i64>(6)? != 0,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut people = c.prepare_cached(
                "SELECT COALESCE(from_name, from_email) FROM items WHERE conversation_pk = ?1 AND deleted = 0
                 ORDER BY COALESCE(received_at, sent_at) DESC LIMIT 50",
            )?;
            let mut out = Vec::with_capacity(rows.len());
            for mut row in rows {
                let names = people.query_map(params![row.conversation_pk], |r| r.get::<_, Option<String>>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
                let mut seen: Vec<String> = Vec::new();
                for n in names.into_iter().flatten() {
                    if !n.is_empty() && !seen.contains(&n) {
                        seen.push(n);
                    }
                }
                row.participants = seen;
                out.push(row);
            }
            Ok(out)
        })
    }

    // --- статистика ---------------------------------------------------------

    pub fn stats(&self, account: AccountId) -> Result<StoreStats> {
        let db_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(self.path.with_extension("db-wal")).map(|m| m.len()).unwrap_or(0);
        self.with_conn(|c| {
            let items: i64 = c.query_row("SELECT COUNT(*) FROM items WHERE account_id = ?1 AND deleted = 0", params![account.0], |r| r.get(0))?;
            let bodies: i64 = c.query_row("SELECT COUNT(*) FROM bodies b JOIN items i ON i.item_pk = b.item_pk WHERE i.account_id = ?1 AND i.deleted = 0", params![account.0], |r| r.get(0))?;
            let conversations: i64 = c.query_row("SELECT COUNT(*) FROM conversations WHERE account_id = ?1 AND item_count > 0", params![account.0], |r| r.get(0))?;
            let (attachments, attachments_fetched, attachments_with_text, attachment_cache_bytes): (i64, i64, i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(a.fetched_at IS NOT NULL), 0), COALESCE(SUM(a.text_state = 1), 0), COALESCE(SUM(CASE WHEN a.local_path IS NOT NULL THEN a.size ELSE 0 END), 0)
                 FROM attachments a JOIN items i ON i.item_pk = a.item_pk WHERE i.account_id = ?1 AND i.deleted = 0 AND a.is_inline = 0",
                params![account.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let mut st = c.prepare(
                "SELECT f.display_name, f.distinguished, f.total_count, f.sync_complete,
                        (SELECT COUNT(*) FROM items i WHERE i.folder_pk = f.folder_pk AND i.deleted = 0),
                        (SELECT COUNT(*) FROM items i WHERE i.folder_pk = f.folder_pk AND i.deleted = 0 AND i.body_state = 1),
                        (SELECT MIN(received_at) FROM items i WHERE i.folder_pk = f.folder_pk AND i.deleted = 0),
                        (SELECT MAX(received_at) FROM items i WHERE i.folder_pk = f.folder_pk AND i.deleted = 0)
                 FROM folders f WHERE f.account_id = ?1 AND f.deleted = 0 AND (f.sync_state IS NOT NULL OR f.total_count > 0)
                 ORDER BY f.total_count DESC",
            )?;
            let folders = st
                .query_map(params![account.0], |r| {
                    Ok(FolderStats {
                        display_name: r.get(0)?,
                        distinguished: r.get(1)?,
                        server_total: r.get(2)?,
                        synced: r.get::<_, i64>(3)? != 0,
                        local_items: r.get(4)?,
                        local_bodies: r.get(5)?,
                        oldest: ms_ts(r.get(6)?),
                        newest: ms_ts(r.get(7)?),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(StoreStats { db_bytes, items, bodies, conversations, attachments, attachments_fetched, attachments_with_text, attachment_cache_bytes, folders })
        })
    }

    /// Отладка: выполняет запрос только на чтение и возвращает строки как текст (значения через табуляцию).
    pub fn debug_query(&self, sql: &str) -> Result<Vec<String>> {
        let head = sql.trim_start().to_ascii_lowercase();
        if !(head.starts_with("select") || head.starts_with("pragma") || head.starts_with("explain") || head.starts_with("with")) {
            return Err(StoreError::Other("разрешены только SELECT, WITH, PRAGMA и EXPLAIN".into()));
        }
        self.with_conn(|c| {
            let mut st = c.prepare(sql)?;
            let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
            let mut out = vec![cols.join("\t")];
            let mut rows = st.query([])?;
            while let Some(row) = rows.next()? {
                let mut cells = Vec::with_capacity(cols.len());
                for i in 0..cols.len() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    cells.push(match v {
                        rusqlite::types::Value::Null => "NULL".to_string(),
                        rusqlite::types::Value::Integer(n) => n.to_string(),
                        rusqlite::types::Value::Real(f) => f.to_string(),
                        rusqlite::types::Value::Text(s) => s,
                        rusqlite::types::Value::Blob(b) => format!("<blob {} байт>", b.len()),
                    });
                }
                out.push(cells.join("\t"));
            }
            Ok(out)
        })
    }

    /// Контрольная точка WAL, чтобы файл журнала не разрастался при массовой загрузке.
    pub fn checkpoint(&self) -> Result<()> {
        self.with_conn(|c| {
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            debug!("wal checkpoint");
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Внутренние операции в транзакции

fn upsert_folder(tx: &Transaction, account: AccountId, f: &Folder, now: i64) -> Result<()> {
    let mut st = tx.prepare_cached(
        "INSERT INTO folders(account_id, server_id, change_key, parent_server_id, distinguished, display_name, folder_class, total_count, unread_count, child_count, deleted, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11)
         ON CONFLICT(account_id, server_id) DO UPDATE SET
           change_key = excluded.change_key, parent_server_id = excluded.parent_server_id,
           distinguished = COALESCE(excluded.distinguished, folders.distinguished),
           display_name = excluded.display_name, folder_class = COALESCE(excluded.folder_class, folders.folder_class),
           total_count = excluded.total_count, unread_count = excluded.unread_count, child_count = excluded.child_count,
           deleted = 0, updated_at = excluded.updated_at",
    )?;
    st.execute(params![
        account.0,
        f.id.id,
        f.id.change_key,
        f.parent_id.as_ref().map(|p| p.id.as_str()),
        f.distinguished,
        f.display_name,
        f.folder_class,
        f.total_count,
        f.unread_count,
        f.child_folder_count,
        now
    ])?;
    Ok(())
}

/// Текущее положение письма: ключ, беседа, папка.
struct ItemState {
    item_pk: i64,
    conversation_pk: Option<i64>,
    folder_pk: i64,
}

fn item_state(tx: &Transaction, account: AccountId, server_id: &str) -> Result<Option<ItemState>> {
    let mut st = tx.prepare_cached("SELECT item_pk, conversation_pk, folder_pk FROM items WHERE account_id = ?1 AND server_id = ?2")?;
    Ok(st.query_row(params![account.0, server_id], |r| Ok(ItemState { item_pk: r.get(0)?, conversation_pk: r.get(1)?, folder_pk: r.get(2)? })).optional()?)
}

/// Пары (беседа, папка), чьи агрегаты надо пересчитать в конце транзакции.
#[derive(Default)]
struct Touched {
    pairs: BTreeSet<(i64, i64)>,
}

impl Touched {
    fn add(&mut self, conversation_pk: Option<i64>, folder_pk: i64) {
        if let Some(c) = conversation_pk {
            self.pairs.insert((c, folder_pk));
        }
    }

    fn recompute(&self, tx: &Transaction) -> Result<()> {
        let mut convs: BTreeSet<i64> = BTreeSet::new();
        for (c, f) in &self.pairs {
            recompute_conversation_folder(tx, *c, *f)?;
            convs.insert(*c);
        }
        for c in convs {
            recompute_conversation(tx, c)?;
        }
        Ok(())
    }
}

fn recompute_conversation_folder(tx: &Transaction, conversation_pk: i64, folder_pk: i64) -> Result<()> {
    let mut st = tx.prepare_cached(
        "SELECT COUNT(*), COALESCE(SUM(is_read = 0), 0), MAX(COALESCE(received_at, sent_at)) FROM items
         WHERE conversation_pk = ?1 AND folder_pk = ?2 AND deleted = 0",
    )?;
    let (n, unread, last): (i64, i64, Option<i64>) = st.query_row(params![conversation_pk, folder_pk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if n == 0 {
        tx.prepare_cached("DELETE FROM conversation_folders WHERE conversation_pk = ?1 AND folder_pk = ?2")?.execute(params![conversation_pk, folder_pk])?;
    } else {
        tx.prepare_cached(
            "INSERT INTO conversation_folders(conversation_pk, folder_pk, last_received_at, item_count, unread_count) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(conversation_pk, folder_pk) DO UPDATE SET last_received_at = excluded.last_received_at, item_count = excluded.item_count, unread_count = excluded.unread_count",
        )?
        .execute(params![conversation_pk, folder_pk, last.unwrap_or(0), n, unread])?;
    }
    Ok(())
}

fn recompute_conversation(tx: &Transaction, conversation_pk: i64) -> Result<()> {
    let mut st = tx.prepare_cached(
        "SELECT COUNT(*), COALESCE(SUM(is_read = 0), 0), MAX(COALESCE(received_at, sent_at)), COALESCE(MAX(has_attachments), 0)
         FROM items WHERE conversation_pk = ?1 AND deleted = 0",
    )?;
    let (n, unread, last, has_att): (i64, i64, Option<i64>, i64) = st.query_row(params![conversation_pk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let topic: Option<String> = tx
        .prepare_cached("SELECT subject FROM items WHERE conversation_pk = ?1 AND deleted = 0 ORDER BY COALESCE(received_at, sent_at) ASC, item_pk ASC LIMIT 1")?
        .query_row(params![conversation_pk], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten()
        .map(|s| text::topic_of(&s));
    tx.prepare_cached("UPDATE conversations SET item_count = ?2, unread_count = ?3, last_received_at = ?4, has_attachments = ?5, topic = COALESCE(?6, topic) WHERE conversation_pk = ?1")?
        .execute(params![conversation_pk, n, unread, last, has_att, topic])?;
    Ok(())
}

/// Ключ беседы письма: `ConversationId` сервера, а без него письмо образует свою беседу.
fn conversation_key(m: &MessageMeta) -> String {
    m.conversation_id.as_ref().map(|c| c.0.clone()).unwrap_or_else(|| format!("item:{}", m.item_id.id))
}

/// Находит или создаёт беседу и привязывает к ней письмо. Возвращает `conversation_pk`.
fn link_conversation(tx: &Transaction, account: AccountId, item_pk: i64, m: &MessageMeta) -> Result<i64> {
    let key = conversation_key(m);
    tx.prepare_cached("INSERT OR IGNORE INTO conversations(account_id, server_id) VALUES (?1, ?2)")?.execute(params![account.0, key])?;
    let conv_pk: i64 = tx.prepare_cached("SELECT conversation_pk FROM conversations WHERE account_id = ?1 AND server_id = ?2")?.query_row(params![account.0, key], |r| r.get(0))?;
    tx.prepare_cached("UPDATE items SET conversation_pk = ?2 WHERE item_pk = ?1 AND (conversation_pk IS NULL OR conversation_pk <> ?2)")?.execute(params![item_pk, conv_pk])?;
    Ok(conv_pk)
}

fn apply_changes_in_tx(tx: &Transaction, account: AccountId, folder_pk: i64, changes: &[ItemChange]) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let mut touched = Touched::default();
    let now = now_ms();
    for ch in changes {
        match ch {
            ItemChange::Create(m) | ItemChange::Update(m) => {
                // Метаданные индексируются сразу: тема и отправитель ищутся по всему ящику,
                // тело добавится в индекс при загрузке. Старую версию снимаем до изменения источника.
                let prev = item_state(tx, account, &m.item_id.id)?;
                if let Some(p) = &prev {
                    fts_delete(tx, p.item_pk)?;
                    touched.add(p.conversation_pk, p.folder_pk);
                }
                let up = upsert_meta(tx, account, folder_pk, m, now)?;
                if let Some((c, f)) = up.adopted_from {
                    touched.add(c, f);
                }
                let conv_pk = link_conversation(tx, account, up.item_pk, m)?;
                touched.add(Some(conv_pk), folder_pk);
                fts_insert(tx, up.item_pk)?;
                if up.existed {
                    stats.updated += 1;
                } else {
                    stats.created += 1;
                }
            }
            ItemChange::Delete(id) => {
                if let Some(st) = item_state(tx, account, &id.id)? {
                    fts_delete(tx, st.item_pk)?;
                    touched.add(st.conversation_pk, st.folder_pk);
                }
                stats.deleted += tx.execute("UPDATE items SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id, now])?;
            }
            ItemChange::ReadFlag { item_id, is_read } => {
                let n = tx.execute("UPDATE items SET is_read = ?3, updated_at = ?4 WHERE account_id = ?1 AND server_id = ?2", params![account.0, item_id.id, *is_read as i64, now])?;
                stats.read_flags += n;
                if n > 0 {
                    if let Some(st) = item_state(tx, account, &item_id.id)? {
                        touched.add(st.conversation_pk, st.folder_pk);
                    }
                }
            }
        }
    }
    touched.recompute(tx)?;
    Ok(stats)
}

fn is_fts_indexed(tx: &Transaction, item_pk: i64) -> Result<bool> {
    Ok(tx.prepare_cached("SELECT fts_indexed FROM items WHERE item_pk = ?1")?.query_row(params![item_pk], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) != 0)
}

/// Снимает строку с индекса (если была проиндексирована) по текущим значениям источника.
fn fts_delete(tx: &Transaction, item_pk: i64) -> Result<()> {
    if is_fts_indexed(tx, item_pk)? {
        tx.prepare_cached(schema::FTS_DELETE_SQL)?.execute(params![item_pk])?;
        tx.prepare_cached("UPDATE items SET fts_indexed = 0 WHERE item_pk = ?1")?.execute(params![item_pk])?;
    }
    Ok(())
}

fn fts_insert(tx: &Transaction, item_pk: i64) -> Result<()> {
    tx.prepare_cached(schema::FTS_INSERT_SQL)?.execute(params![item_pk])?;
    tx.prepare_cached("UPDATE items SET fts_indexed = 1 WHERE item_pk = ?1")?.execute(params![item_pk])?;
    Ok(())
}

struct Upserted {
    item_pk: i64,
    /// Запись уже была (по id или усыновлена).
    existed: bool,
    /// Откуда усыновлена удалённая строка: (беседа, папка), чтобы пересчитать её агрегаты.
    adopted_from: Option<(Option<i64>, i64)>,
}

/// Вставляет или обновляет метаданные.
///
/// Перемещение письма меняет его `ItemId`: источник присылает `Delete`, приёмник `Create`
/// с новым id и тем же `InternetMessageId`. Если новый id неизвестен, а удалённая строка с тем
/// же `InternetMessageId` и временем получения есть, строка «усыновляется»: получает новый id
/// и папку, сохраняя тело, получателей, вложения и место в индексе.
fn upsert_meta(tx: &Transaction, account: AccountId, folder_pk: i64, m: &MessageMeta, now: i64) -> Result<Upserted> {
    let mut existed = item_state(tx, account, &m.item_id.id)?.is_some();
    let mut adopted_from = None;
    if !existed {
        if let (Some(imid), Some(received)) = (&m.internet_message_id, m.received_at) {
            let orphan: Option<(i64, Option<i64>, i64)> = tx
                .prepare_cached("SELECT item_pk, conversation_pk, folder_pk FROM items WHERE account_id = ?1 AND internet_message_id = ?2 AND received_at = ?3 AND deleted = 1 LIMIT 1")?
                .query_row(params![account.0, imid, received.as_millisecond()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            if let Some((pk, conv, old_folder)) = orphan {
                tx.execute("UPDATE items SET server_id = ?2, change_key = ?3, folder_pk = ?4, deleted = 0, updated_at = ?5 WHERE item_pk = ?1", params![pk, m.item_id.id, m.item_id.change_key, folder_pk, now])?;
                existed = true;
                adopted_from = Some((conv, old_folder));
            }
        }
    }
    tx.prepare_cached(
        "INSERT INTO items(account_id, server_id, change_key, folder_pk, conversation_id, internet_message_id, item_class, subject,
                           from_name, from_email, sender_email, display_to, received_at, sent_at, last_modified_at,
                           is_read, is_draft, has_attachments, importance, flag_status, size, preview, body_state, deleted, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, 0, 0, ?23)
         ON CONFLICT(account_id, server_id) DO UPDATE SET
           change_key = excluded.change_key, folder_pk = excluded.folder_pk,
           conversation_id = COALESCE(excluded.conversation_id, items.conversation_id),
           internet_message_id = COALESCE(excluded.internet_message_id, items.internet_message_id),
           item_class = COALESCE(excluded.item_class, items.item_class),
           subject = COALESCE(excluded.subject, items.subject),
           from_name = COALESCE(excluded.from_name, items.from_name), from_email = COALESCE(excluded.from_email, items.from_email),
           sender_email = COALESCE(excluded.sender_email, items.sender_email), display_to = COALESCE(excluded.display_to, items.display_to),
           received_at = COALESCE(excluded.received_at, items.received_at), sent_at = COALESCE(excluded.sent_at, items.sent_at),
           last_modified_at = COALESCE(excluded.last_modified_at, items.last_modified_at),
           is_read = excluded.is_read, is_draft = excluded.is_draft, has_attachments = excluded.has_attachments,
           importance = excluded.importance, flag_status = excluded.flag_status, size = COALESCE(excluded.size, items.size),
           preview = COALESCE(excluded.preview, items.preview), deleted = 0, updated_at = excluded.updated_at",
    )?
    .execute(params![
        account.0,
        m.item_id.id,
        m.item_id.change_key,
        folder_pk,
        m.conversation_id.as_ref().map(|c| c.0.as_str()),
        m.internet_message_id,
        m.item_class,
        m.subject,
        m.from.as_ref().and_then(|p| p.name.as_deref()),
        m.from.as_ref().and_then(|p| p.email.as_deref()),
        m.sender.as_ref().and_then(|p| p.email.as_deref()),
        m.display_to,
        ts_ms(m.received_at),
        ts_ms(m.sent_at),
        ts_ms(m.last_modified_at),
        m.is_read as i64,
        m.is_draft as i64,
        m.has_attachments as i64,
        m.importance.as_str(),
        m.flag.as_str(),
        m.size,
        m.preview,
        now
    ])?;
    let item_pk: i64 = tx.prepare_cached("SELECT item_pk FROM items WHERE account_id = ?1 AND server_id = ?2")?.query_row(params![account.0, m.item_id.id], |r| r.get(0))?;
    Ok(Upserted { item_pk, existed, adopted_from })
}

/// Слова между маркерами подсветки в сниппете.
fn marked_words(snippet: &str, open: &str, close: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = snippet;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(close) else { break };
        let w = after[..end].trim();
        if !w.is_empty() {
            out.push(text::norm_lower(w));
        }
        rest = &after[end + close.len()..];
    }
    out
}

/// Имя вложения письма, в чьём тексте или имени встречаются подсвеченные слова.
fn attachment_name_for(c: &Connection, item_pk: i64, words: &[String]) -> Result<Option<String>> {
    let mut st = c.prepare_cached("SELECT name, text FROM attachments WHERE item_pk = ?1 AND is_inline = 0 ORDER BY attachment_pk")?;
    let rows: Vec<(Option<String>, Option<String>)> = st.query_map(params![item_pk], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
    let mut fallback: Option<String> = None;
    for (name, text_opt) in rows {
        let name_l = text::norm_lower(name.as_deref().unwrap_or(""));
        let text_l = text_opt.as_deref().map(text::norm_lower).unwrap_or_default();
        if words.iter().any(|w| text_l.contains(w.as_str()) || name_l.contains(w.as_str())) {
            return Ok(name);
        }
        if fallback.is_none() && text_opt.is_some() {
            fallback = name;
        }
    }
    Ok(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{AttachmentMeta, BodyType, ConversationId, DistinguishedFolder, Importance, MessageBody, Participant};

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-store-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn folder(name: &str, dist: Option<&str>) -> Folder {
        folder_with(name, dist, Some("IPF.Note"), None, 10)
    }

    fn folder_with(name: &str, dist: Option<&str>, class: Option<&str>, parent: Option<&str>, total: i64) -> Folder {
        Folder {
            id: FolderId { id: format!("F-{name}"), change_key: Some("ck".into()) },
            parent_id: parent.map(|p| FolderId { id: format!("F-{p}"), change_key: None }),
            display_name: name.to_string(),
            distinguished: dist.map(String::from),
            folder_class: class.map(String::from),
            total_count: total,
            unread_count: 1,
            child_folder_count: 0,
        }
    }

    fn meta(id: &str, subject: &str) -> MessageMeta {
        meta_at(id, subject, "2026-10-16T10:00:00Z", Some("conv-1"))
    }

    fn meta_at(id: &str, subject: &str, received: &str, conv: Option<&str>) -> MessageMeta {
        MessageMeta {
            item_id: ItemId { id: id.to_string(), change_key: Some("c".into()) },
            parent_folder_id: None,
            conversation_id: conv.map(|c| ConversationId(c.to_string())),
            internet_message_id: Some(format!("<{id}@x>")),
            item_class: Some("IPM.Note".into()),
            subject: Some(subject.to_string()),
            from: Some(Participant { name: Some("Василий Петров".into()), email: Some("vp@x.ru".into()), routing_type: None }),
            sender: None,
            display_to: None,
            received_at: Some(received.parse().unwrap()),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            has_attachments: false,
            importance: Importance::Normal,
            flag: Default::default(),
            size: Some(100),
            preview: None,
        }
    }

    fn full(m: MessageMeta, body: &str, attachments: Vec<AttachmentMeta>) -> MessageFull {
        let mut m = m;
        m.has_attachments = !attachments.is_empty();
        MessageFull {
            meta: m,
            to: vec![Participant { name: Some("Я".into()), email: Some("me@x.ru".into()), routing_type: None }],
            cc: vec![],
            bcc: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some(body.into()), full_body: None, is_truncated: false }),
            attachments,
        }
    }

    #[test]
    fn roundtrip_and_search() {
        let s = tmp_store("basic");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (fpk, _) = s.folder_by_distinguished(acc, DistinguishedFolder::Inbox.as_str()).unwrap().unwrap();

        let stats = s.apply_item_changes(acc, fpk, &[ItemChange::Create(meta("A", "Запчасти на Колыму")), ItemChange::Create(meta("B", "Отчёт"))]).unwrap();
        assert_eq!(stats.created, 2);
        assert_eq!(s.count_items_without_bodies(acc, fpk, None).unwrap(), 2);

        assert_eq!(s.store_full_messages(acc, fpk, &[full(meta("A", "Запчасти на Колыму"), "<p>Отправили <b>запчасти</b> вчера.</p>", vec![])]).unwrap(), 1);
        assert_eq!(s.count_items_without_bodies(acc, fpk, None).unwrap(), 1);

        let hits = s.search(acc, "запчаст колым", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].snippet.contains("[запчасти]"), "{}", hits[0].snippet);
        // Отправитель индексируется по метаданным у обоих писем, тело есть только у одного.
        assert_eq!(s.search(acc, "петров", 10).unwrap().len(), 2);
        assert_eq!(s.search(acc, "отчёт", 10).unwrap().len(), 1, "тема без тела тоже ищется");
        assert_eq!(s.search(acc, "отчет", 10).unwrap().len(), 1, "ё и е равнозначны");
        assert!(s.search(acc, "нетакого", 10).unwrap().is_empty());

        let st = s.apply_item_changes(acc, fpk, &[ItemChange::ReadFlag { item_id: ItemId { id: "A".into(), change_key: None }, is_read: true }, ItemChange::Delete(ItemId { id: "B".into(), change_key: None })]).unwrap();
        assert_eq!((st.read_flags, st.deleted), (1, 1));
        assert_eq!(s.search(acc, "петров", 10).unwrap().len(), 1, "удалённое письмо снято с индекса");
        let rows = s.list_items(acc, fpk, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_read);

        let stats = s.stats(acc).unwrap();
        assert_eq!(stats.items, 1);
        assert_eq!(stats.bodies, 1);
        assert_eq!(stats.conversations, 1);
    }

    #[test]
    fn search_operators() {
        let s = tmp_store("ops");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Отправленные", Some("sentitems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (sent, _) = s.folder_by_distinguished(acc, "sentitems").unwrap().unwrap();
        let mut a = meta_at("A", "Счёт за октябрь", "2026-10-10T10:00:00Z", Some("c1"));
        a.has_attachments = true;
        let mut b = meta_at("B", "Счёт за сентябрь", "2026-09-10T10:00:00Z", Some("c2"));
        b.from = Some(Participant { name: Some("Иван Сидоров".into()), email: Some("is@y.ru".into()), routing_type: None });
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(a), ItemChange::Create(b)]).unwrap();
        s.apply_item_changes(acc, sent, &[ItemChange::Create(meta_at("C", "RE: Счёт за октябрь", "2026-10-11T10:00:00Z", Some("c1")))]).unwrap();
        s.store_full_messages(acc, sent, &[full(meta_at("C", "RE: Счёт за октябрь", "2026-10-11T10:00:00Z", Some("c1")), "<p>Оплатим</p>", vec![])]).unwrap();

        let ids = |hits: Vec<SearchHit>| hits.into_iter().map(|h| h.item_pk).collect::<Vec<_>>();
        assert_eq!(s.search(acc, "счет", 10).unwrap().len(), 3);
        assert_eq!(s.search(acc, "счет от:петров", 10).unwrap().len(), 2);
        assert_eq!(s.search(acc, "счет from:сидоров", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет кому:me@x.ru", 10).unwrap().len(), 1, "получатели известны только у письма с телом");
        assert_eq!(s.search(acc, "тема:сентябрь", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет вложение:", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет has:attachment", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет папка:отправленные", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет in:inbox", 10).unwrap().len(), 2);
        assert_eq!(s.search(acc, "счет после:2026-10-01", 10).unwrap().len(), 2);
        assert_eq!(s.search(acc, "счет до:30.09.2026", 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "счет после:2026-10-01 до:2026-10-10", 10).unwrap().len(), 1);
        // Только фильтры, без текста: список по дате, свежие первыми.
        let only = s.search(acc, "после:2026-10-01", 10).unwrap();
        assert_eq!(ids(only).len(), 2);
        assert!(s.search(acc, "", 10).unwrap().is_empty());
    }

    #[test]
    fn attachments_cache_and_text_search() {
        let s = tmp_store("att");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (fpk, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let att = |id: &str, name: &str, size: i64, inline: bool| AttachmentMeta { id: id.into(), name: Some(name.into()), content_type: Some("application/pdf".into()), size: Some(size), is_inline: inline, content_id: None };
        let msg = full(meta("A", "Письмо с файлами"), "<p>см. вложение</p>", vec![att("att1", "Спецификация №156.pdf", 1000, false), att("att2", "logo.png", 50, true), att("att3", "huge.pdf", 50_000_000, false)]);
        s.store_full_messages(acc, fpk, &[msg.clone()]).unwrap();

        // Имя файла ищется сразу по метаданным вложения.
        let hits = s.search(acc, "спецификац", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].attachment_name.as_deref(), Some("Спецификация №156.pdf"));

        // В очередь на скачивание попадают только не инлайновые и не больше лимита.
        let todo = s.attachments_to_fetch(acc, fpk, None, 25 * 1024 * 1024, 10).unwrap();
        assert_eq!(todo.iter().map(|t| t.server_id.as_str()).collect::<Vec<_>>(), vec!["att1"]);
        assert_eq!(s.count_attachments_to_fetch(acc, fpk, None, 25 * 1024 * 1024).unwrap(), 1);
        assert_eq!(s.count_attachments_to_fetch(acc, fpk, None, i64::MAX).unwrap(), 2);

        let path = s.save_attachment(&todo[0], b"%PDF fake", Some("application/pdf"), AttachmentText::Extracted("Поставка шлёпанцев в количестве 40 пар".into())).unwrap();
        assert!(path.exists());
        assert!(path.starts_with(s.attachments_dir()));
        assert!(s.attachments_to_fetch(acc, fpk, None, i64::MAX, 10).unwrap().iter().all(|t| t.server_id != "att1"));

        let hits = s.search(acc, "шлепанц", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].attachment_name.as_deref(), Some("Спецификация №156.pdf"));
        assert!(hits[0].snippet.contains("[шлепанцев]"), "{}", hits[0].snippet);
        // Совпадение в теле письма имеет приоритет над вложением.
        let hits = s.search(acc, "вложение", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].attachment_name.is_none(), "{hits:?}");

        // Повторная запись письма (например, после Update) не теряет скачанное вложение.
        s.store_full_messages(acc, fpk, &[msg]).unwrap();
        let rows = s.attachments_of_item(hits[0].item_pk).unwrap();
        let a1 = rows.iter().find(|r| r.server_id == "att1").unwrap();
        assert!(a1.local_path.is_some() && a1.text_state == TEXT_OK && a1.fetched_at.is_some());
        assert_eq!(s.search(acc, "шлепанц", 10).unwrap().len(), 1);

        // Вытеснение по квоте: файл удалён, текст остался.
        let r = s.evict_attachment_cache(0).unwrap();
        assert_eq!(r.files_removed, 1);
        assert!(!path.exists());
        assert!(s.attachments_of_item(a1.item_pk).unwrap().iter().find(|r| r.server_id == "att1").unwrap().local_path.is_none());
        assert_eq!(s.search(acc, "шлепанц", 10).unwrap().len(), 1);
        let st = s.stats(acc).unwrap();
        assert_eq!((st.attachments, st.attachments_fetched, st.attachments_with_text), (2, 1, 1));

        // Исчезнувшее с сервера вложение удаляется вместе со строкой.
        let msg2 = full(meta("A", "Письмо с файлами"), "<p>см. вложение</p>", vec![att("att3", "huge.pdf", 50_000_000, false)]);
        s.store_full_messages(acc, fpk, &[msg2]).unwrap();
        assert_eq!(s.attachments_of_item(a1.item_pk).unwrap().len(), 1);
        assert!(s.search(acc, "шлепанц", 10).unwrap().is_empty());
    }

    #[test]
    fn on_demand_lookups() {
        let s = tmp_store("ondemand");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (fpk, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        s.apply_item_changes(acc, fpk, &[ItemChange::Create(meta("A", "С картинкой")), ItemChange::Create(meta("B", "Без тела"))]).unwrap();

        // Ссылка на письмо: папка, идентификатор, состояние тела; удалённое письмо не видно.
        let rows = s.list_items(acc, fpk, 10).unwrap();
        let a_pk = rows.iter().find(|r| r.item_id.id == "A").unwrap().item_pk;
        let b_pk = rows.iter().find(|r| r.item_id.id == "B").unwrap().item_pk;
        let r = s.item_ref(a_pk).unwrap().unwrap();
        assert_eq!((r.item_pk, r.folder_pk, r.id.id.as_str(), r.id.change_key.as_deref(), r.body_state), (a_pk, fpk, "A", Some("c"), 0));
        assert!(s.item_ref(999_999).unwrap().is_none());
        s.mark_items_deleted(acc, &[ItemId { id: "B".into(), change_key: None }]).unwrap();
        assert!(s.item_ref(b_pk).unwrap().is_none(), "удалённое письмо не возвращается");
        s.mark_body_failed(acc, &[ItemId { id: "A".into(), change_key: None }]).unwrap();
        assert_eq!(s.item_ref(a_pk).unwrap().unwrap().body_state, 2);

        // Вложения с Content-ID; одно по ключу; задание на скачивание.
        let inline = AttachmentMeta { id: "img1".into(), name: Some("image001.png".into()), content_type: Some("image/png".into()), size: Some(10), is_inline: true, content_id: Some("image001.png@01DD5D86.89427E20".into()) };
        let file = AttachmentMeta { id: "f1".into(), name: Some("отчёт.pdf".into()), content_type: Some("application/pdf".into()), size: Some(100), is_inline: false, content_id: None };
        s.store_full_messages(acc, fpk, &[full(meta("A", "С картинкой"), "<p>см. <img src=\"cid:image001.png@01DD5D86.89427E20\"></p>", vec![inline, file])]).unwrap();
        assert_eq!(s.item_ref(a_pk).unwrap().unwrap().body_state, 1);
        let atts = s.attachments_of_item(a_pk).unwrap();
        let img = atts.iter().find(|a| a.server_id == "img1").unwrap();
        assert_eq!(img.content_id.as_deref(), Some("image001.png@01DD5D86.89427E20"));
        assert!(img.is_inline && img.is_image());
        let pdf = atts.iter().find(|a| a.server_id == "f1").unwrap();
        assert!(pdf.content_id.is_none() && !pdf.is_image());
        let one = s.attachment(pdf.attachment_pk).unwrap().unwrap();
        assert_eq!((one.attachment_pk, one.item_pk, one.server_id.as_str(), one.name.as_deref()), (pdf.attachment_pk, a_pk, "f1", Some("отчёт.pdf")));
        assert!(s.attachment(999_999).unwrap().is_none());
        let todo = one.todo();
        assert_eq!(todo, AttachmentTodo { attachment_pk: pdf.attachment_pk, item_pk: a_pk, server_id: "f1".into(), name: Some("отчёт.pdf".into()), content_type: Some("application/pdf".into()), size: Some(100) });

        // Скачали картинку: путь есть; обращение сдвигает last_access_at; забыть файл — путь пропал, текст и состояние остались.
        let path = s.save_attachment(&img.todo(), b"\x89PNG", Some("image/png"), AttachmentText::Unsupported).unwrap();
        let before: i64 = s.debug_query(&format!("SELECT last_access_at FROM attachments WHERE attachment_pk = {}", img.attachment_pk)).unwrap()[1].parse().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.touch_attachment(img.attachment_pk).unwrap();
        let after: i64 = s.debug_query(&format!("SELECT last_access_at FROM attachments WHERE attachment_pk = {}", img.attachment_pk)).unwrap()[1].parse().unwrap();
        assert!(after > before, "{after} > {before}");
        let cached = s.attachment(img.attachment_pk).unwrap().unwrap();
        assert_eq!(cached.local_path.as_deref(), Some(path.as_path()));
        assert_eq!(cached.text_state, TEXT_UNSUPPORTED);
        s.forget_attachment_file(img.attachment_pk).unwrap();
        let forgotten = s.attachment(img.attachment_pk).unwrap().unwrap();
        assert!(forgotten.local_path.is_none() && forgotten.fetched_at.is_some() && forgotten.text_state == TEXT_UNSUPPORTED);

        // Отладочная функция распаковки тела.
        let lines = s.debug_query(&format!("SELECT instr(zstd_text(unique_body), 'cid:') > 0 FROM bodies WHERE item_pk = {a_pk}")).unwrap();
        assert_eq!(lines[1], "1");

        // Ключи беседы: только живые письма, порядок как у conversation_messages, cached по базе.
        let conv_pk = s.list_conversations(acc, fpk, 10).unwrap()[0].conversation_pk;
        let keys = s.conversation_keys(acc, conv_pk).unwrap();
        assert_eq!(keys, vec![crate::ui::MessageKey { item_pk: a_pk, body_state: 1, is_read: false, cached: 0 }], "{keys:?}");
        s.save_attachment(&img.todo(), b"\x89PNG", Some("image/png"), AttachmentText::Unsupported).unwrap();
        assert_eq!(s.conversation_keys(acc, conv_pk).unwrap()[0].cached, 1);
        let heads = s.conversation_messages(acc, conv_pk).unwrap();
        assert_eq!(heads.iter().map(|h| h.item_pk).collect::<Vec<_>>(), vec![a_pk]);
    }

    #[test]
    fn attachment_file_gets_internet_zone_mark() {
        let dir = std::env::temp_dir().join(format!("aiowa-store-test-motw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("файл.txt");
        std::fs::write(&path, b"x").unwrap();
        mark_from_internet(&path).unwrap();
        // Содержимое файла не меняется; на Windows/NTFS появляется поток Zone.Identifier.
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        #[cfg(windows)]
        assert!(std::fs::read_to_string(format!("{}:Zone.Identifier", path.display())).unwrap().contains("ZoneId=3"));
    }

    #[test]
    fn conversations_follow_items() {
        let s = tmp_store("conv");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Отправленные", Some("sentitems")), folder("Архив", None)]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (sent, _) = s.folder_by_distinguished(acc, "sentitems").unwrap().unwrap();
        let archive = s.resolve_folder(acc, "архив").unwrap().unwrap().folder_pk;

        let mut b = meta_at("B", "RE: Договор", "2026-10-02T10:00:00Z", Some("c1"));
        b.from = Some(Participant { name: Some("Анна".into()), email: Some("a@x.ru".into()), routing_type: None });
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(meta_at("A", "Договор", "2026-10-01T10:00:00Z", Some("c1"))), ItemChange::Create(b), ItemChange::Create(meta_at("X", "Другое", "2026-09-01T10:00:00Z", None))]).unwrap();
        s.apply_item_changes(acc, sent, &[ItemChange::Create(meta_at("S", "RE: Договор", "2026-10-03T10:00:00Z", Some("c1")))]).unwrap();

        let list = s.list_conversations(acc, inbox, 10).unwrap();
        assert_eq!(list.len(), 2);
        let c = &list[0];
        assert_eq!(c.topic.as_deref(), Some("Договор"));
        assert_eq!((c.item_count, c.unread_count, c.total_count), (2, 2, 3));
        assert_eq!(c.participants, vec!["Василий Петров", "Анна"]);
        assert_eq!(c.last_received_at, Some("2026-10-02T10:00:00Z".parse().unwrap()));
        assert_eq!(list[1].topic.as_deref(), Some("Другое"));
        assert_eq!(s.list_conversations(acc, sent, 10).unwrap().len(), 1);

        // Прочитали одно письмо и удалили второе: агрегаты пересчитаны.
        s.apply_item_changes(acc, inbox, &[ItemChange::ReadFlag { item_id: ItemId { id: "A".into(), change_key: None }, is_read: true }, ItemChange::Delete(ItemId { id: "B".into(), change_key: None })]).unwrap();
        let c = &s.list_conversations(acc, inbox, 10).unwrap()[0];
        assert_eq!((c.item_count, c.unread_count, c.total_count), (1, 0, 2));
        assert_eq!(c.last_received_at, Some("2026-10-01T10:00:00Z".parse().unwrap()));

        // Перемещение: Delete в источнике, Create с новым id в приёмнике усыновляет строку,
        // беседа уходит из входящих в архив.
        s.apply_item_changes(acc, inbox, &[ItemChange::Delete(ItemId { id: "A".into(), change_key: None })]).unwrap();
        assert!(s.list_conversations(acc, inbox, 10).unwrap().iter().all(|c| c.topic.as_deref() != Some("Договор")));
        let mut moved = meta_at("A2", "Договор", "2026-10-01T10:00:00Z", Some("c1"));
        moved.internet_message_id = Some("<A@x>".into());
        s.apply_item_changes(acc, archive, &[ItemChange::Create(moved)]).unwrap();
        let arch = s.list_conversations(acc, archive, 10).unwrap();
        assert_eq!(arch.len(), 1);
        assert_eq!((arch[0].item_count, arch[0].total_count), (1, 2));
        assert_eq!(s.stats(acc).unwrap().items, 3);
    }

    #[test]
    fn folders_for_sync_rule() {
        let s = tmp_store("folders");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(
            acc,
            &[
                folder_with("Входящие", Some("inbox"), Some("IPF.Note"), None, 100),
                folder_with("Отправленные", Some("sentitems"), Some("IPF.Note"), None, 5000),
                folder_with("Проекты", None, Some("IPF.Note"), None, 300),
                folder_with("Мелочь", None, Some("IPF.Note"), None, 10),
                folder_with("Удаленные", Some("deleteditems"), Some("IPF.Note"), None, 900),
                folder_with("Старое", None, Some("IPF.Note"), Some("Удаленные"), 50),
                folder_with("Совсем старое", None, Some("IPF.Note"), Some("Старое"), 5),
                folder_with("Ошибки синхронизации", Some("syncissues"), Some("IPF.Note"), None, 0),
                folder_with("Конфликты", Some("conflicts"), Some("IPF.Note"), Some("Ошибки синхронизации"), 7),
                folder_with("Журнал бесед", Some("conversationhistory"), Some("IPF.Note.Microsoft.Conversation"), None, 70),
                folder_with("Календарь", Some("calendar"), Some("IPF.Appointment"), None, 800),
                folder_with("Без класса", None, None, None, 3),
                folder_with("RSS", None, Some("IPF.Note.OutlookHomepage"), None, 0),
            ],
        )
        .unwrap();
        let names: Vec<String> = s.folders_for_sync(acc).unwrap().into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["Входящие", "Отправленные", "Проекты", "Мелочь", "RSS"]);
        assert_eq!(s.resolve_folder(acc, "SENTITEMS").unwrap().map(|f| f.name), None, "известное имя чувствительно к регистру");
        assert_eq!(s.resolve_folder(acc, "sentitems").unwrap().map(|f| f.name), Some("Отправленные".into()));
        assert_eq!(s.resolve_folder(acc, "проекты").unwrap().map(|f| f.name), Some("Проекты".into()));
        assert!(s.resolve_folder(acc, "нет такой").unwrap().is_none());
    }

    #[test]
    fn safe_names() {
        assert_eq!(safe_file_name("Счёт №25 от 15.10.2026.pdf"), "Счёт №25 от 15.10.2026.pdf");
        assert_eq!(safe_file_name("a/b\\c:d*e?.pdf"), "a_b_c_d_e_.pdf");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name("x".repeat(200).as_str()).chars().count(), 120);
    }
}
