//! Локальное хранилище AIOWA: один файл SQLite (`mail.db`), WAL, FTS5.
//!
//! Правила: сервер источник истины для почты, локально уникальны только индексы и
//! состояние синхронизации. Удаления сервера зеркалятся флагом `deleted`.
//! Тела писем хранятся сжатыми (zstd), для поиска хранится текстовая выжимка.

pub mod schema;
pub mod text;

use aiowa_core::{AccountId, Folder, FolderChange, FolderId, ItemChange, ItemId, MessageFull, MessageMeta, SyncState};
use jiff::Timestamp;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{debug, info};

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
    pub snippet: String,
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
    pub folders: Vec<FolderStats>,
}

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

impl Store {
    /// Открывает (создавая при необходимости) базу и применяет миграции.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -65536;",
        )?;
        schema::migrate(&conn)?;
        info!(path = %path.display(), "хранилище открыто");
        Ok(Self { path, conn: Mutex::new(conn) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| StoreError::Other("мьютекс соединения отравлен".into()))?;
        f(&conn)
    }

    fn with_tx<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| StoreError::Other("мьютекс соединения отравлен".into()))?;
        let tx = conn.transaction()?;
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
            let now = now_ms();
            for f in folders {
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
            for ch in changes {
                match ch {
                    FolderChange::Create(f) | FolderChange::Update(f) => {
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
                        upserts += 1;
                    }
                    FolderChange::Delete(id) => {
                        tx.execute("UPDATE folders SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id, now])?;
                    }
                }
            }
            drop(st);
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
            let mut n = 0;
            for id in ids {
                if let Some(pk) = item_pk_of(tx, account, &id.id)? {
                    fts_delete(tx, pk)?;
                }
                n += tx.execute("UPDATE items SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2", params![account.0, id.id, now])?;
            }
            Ok(n)
        })
    }
}

fn apply_changes_in_tx(tx: &Transaction, account: AccountId, folder_pk: i64, changes: &[ItemChange]) -> Result<ApplyStats> {
    {
        {
            let mut stats = ApplyStats::default();
            let now = now_ms();
            for ch in changes {
                match ch {
                    ItemChange::Create(m) | ItemChange::Update(m) => {
                        // Метаданные индексируются сразу: тема и отправитель ищутся по всему ящику,
                        // тело добавится в индекс при загрузке. Старую версию снимаем до изменения источника.
                        if let Some(pk) = item_pk_of(tx, account, &m.item_id.id)? {
                            fts_delete(tx, pk)?;
                        }
                        let existed = upsert_meta(tx, account, folder_pk, m, now)?;
                        if let Some(pk) = item_pk_of(tx, account, &m.item_id.id)? {
                            fts_insert(tx, pk)?;
                        }
                        if existed {
                            stats.updated += 1;
                        } else {
                            stats.created += 1;
                        }
                    }
                    ItemChange::Delete(id) => {
                        if let Some(pk) = item_pk_of(tx, account, &id.id)? {
                            fts_delete(tx, pk)?;
                        }
                        stats.deleted += tx.execute(
                            "UPDATE items SET deleted = 1, updated_at = ?3 WHERE account_id = ?1 AND server_id = ?2",
                            params![account.0, id.id, now],
                        )?;
                    }
                    ItemChange::ReadFlag { item_id, is_read } => {
                        stats.read_flags += tx.execute(
                            "UPDATE items SET is_read = ?3, updated_at = ?4 WHERE account_id = ?1 AND server_id = ?2",
                            params![account.0, item_id.id, *is_read as i64, now],
                        )?;
                    }
                }
            }
            Ok(stats)
        }
    }
}

impl Store {

    /// Элементы папки без тела, не старше `since`, самые свежие первыми.
    pub fn items_without_bodies(&self, account: AccountId, folder_pk: i64, since: Option<Timestamp>, limit: usize) -> Result<Vec<(i64, ItemId)>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT item_pk, server_id, change_key FROM items
                 WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0 AND body_state = 0
                   AND (?3 IS NULL OR received_at >= ?3)
                 ORDER BY received_at DESC LIMIT ?4",
            )?;
            let rows = st.query_map(params![account.0, folder_pk, ts_ms(since), limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, ItemId { id: r.get(1)?, change_key: r.get(2)? }))
            })?;
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

    /// Сохраняет письма целиком: метаданные, получатели, тело, вложения, индекс.
    pub fn store_full_messages(&self, account: AccountId, folder_pk: i64, messages: &[MessageFull]) -> Result<usize> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut stored = 0;
            for m in messages {
                // Снять старую версию с индекса до любых изменений текста.
                if let Some(pk) = item_pk_of(tx, account, &m.meta.item_id.id)? {
                    fts_delete(tx, pk)?;
                }
                upsert_meta(tx, account, folder_pk, &m.meta, now)?;
                let item_pk: i64 = tx.query_row("SELECT item_pk FROM items WHERE account_id = ?1 AND server_id = ?2", params![account.0, m.meta.item_id.id], |r| r.get(0))?;

                tx.execute("DELETE FROM recipients WHERE item_pk = ?1", params![item_pk])?;
                let mut rec = tx.prepare_cached("INSERT INTO recipients(item_pk, kind, name, email, position) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for (kind, list) in [(1i64, &m.to), (2, &m.cc), (3, &m.bcc)] {
                    for (i, p) in list.iter().enumerate() {
                        rec.execute(params![item_pk, kind, p.name, p.email, i as i64])?;
                    }
                }

                tx.execute("DELETE FROM attachments WHERE item_pk = ?1", params![item_pk])?;
                let mut att = tx.prepare_cached("INSERT INTO attachments(item_pk, server_id, name, content_type, size, is_inline, content_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
                for a in &m.attachments {
                    att.execute(params![item_pk, a.id, a.name, a.content_type, a.size, a.is_inline as i64, a.content_id])?;
                }

                let (body_type, unique_blob, full_blob, unique_text, truncated) = match &m.body {
                    Some(b) => {
                        let text_source = b.unique_body.as_deref().or(b.full_body.as_deref()).unwrap_or("");
                        let unique_text = match b.body_type {
                            aiowa_core::BodyType::Html => text::html_to_text(text_source),
                            aiowa_core::BodyType::Text => text::normalize_ws(text_source),
                        };
                        (
                            b.body_type.as_str(),
                            b.unique_body.as_deref().map(compress).transpose()?,
                            b.full_body.as_deref().map(compress).transpose()?,
                            unique_text,
                            b.is_truncated,
                        )
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
            Ok(stored)
        })
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

    // --- поиск --------------------------------------------------------------

    /// Полнотекстовый поиск: каждое слово запроса ищется как префикс, все слова обязательны.
    pub fn search(&self, account: AccountId, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let fts_query = text::fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT i.item_pk, bm25(items_fts, 8.0, 4.0, 2.0, 1.0, 1.0) AS rank,
                        i.subject, i.from_name, i.from_email, i.received_at, f.display_name,
                        snippet(items_fts, 4, '[', ']', '…', 14)
                 FROM items_fts
                 JOIN items i ON i.item_pk = items_fts.rowid
                 LEFT JOIN folders f ON f.folder_pk = i.folder_pk
                 WHERE items_fts MATCH ?1 AND i.account_id = ?2 AND i.deleted = 0
                 ORDER BY rank LIMIT ?3",
            )?;
            let rows = st.query_map(params![fts_query, account.0, limit as i64], |r| {
                Ok(SearchHit {
                    item_pk: r.get(0)?,
                    rank: r.get(1)?,
                    subject: r.get(2)?,
                    from_name: r.get(3)?,
                    from_email: r.get(4)?,
                    received_at: ms_ts(r.get(5)?),
                    folder_name: r.get(6)?,
                    snippet: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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

    // --- статистика ---------------------------------------------------------

    pub fn stats(&self, account: AccountId) -> Result<StoreStats> {
        let db_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(self.path.with_extension("db-wal")).map(|m| m.len()).unwrap_or(0);
        self.with_conn(|c| {
            let items: i64 = c.query_row("SELECT COUNT(*) FROM items WHERE account_id = ?1 AND deleted = 0", params![account.0], |r| r.get(0))?;
            let bodies: i64 = c.query_row("SELECT COUNT(*) FROM bodies b JOIN items i ON i.item_pk = b.item_pk WHERE i.account_id = ?1 AND i.deleted = 0", params![account.0], |r| r.get(0))?;
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
            Ok(StoreStats { db_bytes, items, bodies, folders })
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

fn item_pk_of(tx: &Transaction, account: AccountId, server_id: &str) -> Result<Option<i64>> {
    Ok(tx.query_row("SELECT item_pk FROM items WHERE account_id = ?1 AND server_id = ?2", params![account.0, server_id], |r| r.get(0)).optional()?)
}

fn is_fts_indexed(tx: &Transaction, item_pk: i64) -> Result<bool> {
    Ok(tx.query_row("SELECT fts_indexed FROM items WHERE item_pk = ?1", params![item_pk], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) != 0)
}

/// Снимает строку с индекса (если была проиндексирована) по текущим значениям источника.
fn fts_delete(tx: &Transaction, item_pk: i64) -> Result<()> {
    if is_fts_indexed(tx, item_pk)? {
        tx.execute(schema::FTS_DELETE_SQL, params![item_pk])?;
        tx.execute("UPDATE items SET fts_indexed = 0 WHERE item_pk = ?1", params![item_pk])?;
    }
    Ok(())
}

fn fts_insert(tx: &Transaction, item_pk: i64) -> Result<()> {
    tx.execute(schema::FTS_INSERT_SQL, params![item_pk])?;
    tx.execute("UPDATE items SET fts_indexed = 1 WHERE item_pk = ?1", params![item_pk])?;
    Ok(())
}

/// Вставляет или обновляет метаданные; возвращает `true`, если запись уже была.
///
/// Перемещение письма меняет его `ItemId`: источник присылает `Delete`, приёмник `Create`
/// с новым id и тем же `InternetMessageId`. Если новый id неизвестен, а удалённая строка с тем
/// же `InternetMessageId` и временем получения есть, строка «усыновляется»: получает новый id
/// и папку, сохраняя тело, получателей и место в индексе.
fn upsert_meta(tx: &Transaction, account: AccountId, folder_pk: i64, m: &MessageMeta, now: i64) -> Result<bool> {
    let mut existed: bool = tx
        .query_row("SELECT 1 FROM items WHERE account_id = ?1 AND server_id = ?2", params![account.0, m.item_id.id], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if !existed {
        if let (Some(imid), Some(received)) = (&m.internet_message_id, m.received_at) {
            let orphan: Option<i64> = tx
                .query_row(
                    "SELECT item_pk FROM items WHERE account_id = ?1 AND internet_message_id = ?2 AND received_at = ?3 AND deleted = 1 LIMIT 1",
                    params![account.0, imid, received.as_millisecond()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(pk) = orphan {
                tx.execute("UPDATE items SET server_id = ?2, change_key = ?3, folder_pk = ?4, deleted = 0, updated_at = ?5 WHERE item_pk = ?1", params![pk, m.item_id.id, m.item_id.change_key, folder_pk, now])?;
                existed = true;
            }
        }
    }
    tx.execute(
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
        params![
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
        ],
    )?;
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{BodyType, DistinguishedFolder, Importance, MessageBody, Participant};

    fn tmp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn folder(name: &str, dist: Option<&str>) -> Folder {
        Folder {
            id: FolderId { id: format!("F-{name}"), change_key: Some("ck".into()) },
            parent_id: None,
            display_name: name.to_string(),
            distinguished: dist.map(String::from),
            folder_class: Some("IPF.Note".into()),
            total_count: 10,
            unread_count: 1,
            child_folder_count: 0,
        }
    }

    fn meta(id: &str, subject: &str) -> MessageMeta {
        MessageMeta {
            item_id: ItemId { id: id.to_string(), change_key: Some("c".into()) },
            parent_folder_id: None,
            conversation_id: None,
            internet_message_id: Some(format!("<{id}@x>")),
            item_class: Some("IPM.Note".into()),
            subject: Some(subject.to_string()),
            from: Some(Participant { name: Some("Василий Петров".into()), email: Some("vp@x.ru".into()), routing_type: None }),
            sender: None,
            display_to: None,
            received_at: Some("2026-10-16T10:00:00Z".parse().unwrap()),
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

    #[test]
    fn roundtrip_and_search() {
        let s = tmp_store();
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (fpk, _) = s.folder_by_distinguished(acc, DistinguishedFolder::Inbox.as_str()).unwrap().unwrap();

        let stats = s.apply_item_changes(acc, fpk, &[ItemChange::Create(meta("A", "Запчасти на Колыму")), ItemChange::Create(meta("B", "Отчёт"))]).unwrap();
        assert_eq!(stats.created, 2);
        assert_eq!(s.count_items_without_bodies(acc, fpk, None).unwrap(), 2);

        let full = MessageFull {
            meta: meta("A", "Запчасти на Колыму"),
            to: vec![Participant { name: Some("Я".into()), email: Some("me@x.ru".into()), routing_type: None }],
            cc: vec![],
            bcc: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some("<p>Отправили <b>запчасти</b> вчера.</p>".into()), full_body: None, is_truncated: false }),
            attachments: vec![],
        };
        assert_eq!(s.store_full_messages(acc, fpk, &[full]).unwrap(), 1);
        assert_eq!(s.count_items_without_bodies(acc, fpk, None).unwrap(), 1);

        let hits = s.search(acc, "запчаст колым", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].snippet.contains("запчасти"));
        // Отправитель индексируется по метаданным у обоих писем, тело есть только у одного.
        assert_eq!(s.search(acc, "петров", 10).unwrap().len(), 2);
        assert_eq!(s.search(acc, "отчёт", 10).unwrap().len(), 1, "тема без тела тоже ищется");
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
    }
}
