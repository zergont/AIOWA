//! Схема базы и миграции через `PRAGMA user_version`.
//! Правило: миграции только добавляют; переименования и удаления колонок через новую таблицу.
//!
//! Версия 3 (итерация 2): кэш и текст вложений, беседы с агрегатами по папкам,
//! колонка `attachment_text` в индексе и нормализация ё→е функцией `fts_norm`.
//! Версия 4 (этап 4): кэш событий календаря и окна, которые уже забирали с сервера.
//! Версия 5: кэш справочника организации и индексы адресов без учёта регистра (карточка контакта).

use crate::text;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use tracing::info;

pub const CURRENT_VERSION: i64 = 8;

/// Состояния текста вложения (`attachments.text_state`).
pub const TEXT_PENDING: i64 = 0;
pub const TEXT_OK: i64 = 1;
pub const TEXT_UNSUPPORTED: i64 = 2;
pub const TEXT_FAILED: i64 = 3;
pub const FETCH_FAILED: i64 = 4;

const V1: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
  account_id   INTEGER PRIMARY KEY,
  host         TEXT NOT NULL,
  login        TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  UNIQUE(host, login)
);

CREATE TABLE IF NOT EXISTS folders (
  folder_pk        INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id        TEXT NOT NULL,
  change_key       TEXT,
  parent_server_id TEXT,
  distinguished    TEXT,
  display_name     TEXT NOT NULL,
  folder_class     TEXT,
  total_count      INTEGER NOT NULL DEFAULT 0,
  unread_count     INTEGER NOT NULL DEFAULT 0,
  child_count      INTEGER NOT NULL DEFAULT 0,
  sync_state       TEXT,
  sync_complete    INTEGER NOT NULL DEFAULT 0,
  sync_priority    INTEGER NOT NULL DEFAULT 100,
  last_synced_at   INTEGER,
  deleted          INTEGER NOT NULL DEFAULT 0,
  updated_at       INTEGER NOT NULL,
  UNIQUE(account_id, server_id)
);
CREATE INDEX IF NOT EXISTS folders_parent ON folders(account_id, parent_server_id);

CREATE TABLE IF NOT EXISTS items (
  item_pk             INTEGER PRIMARY KEY,
  account_id          INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id           TEXT NOT NULL,
  change_key          TEXT,
  folder_pk           INTEGER NOT NULL REFERENCES folders(folder_pk),
  conversation_id     TEXT,
  internet_message_id TEXT,
  item_class          TEXT,
  subject             TEXT,
  from_name           TEXT,
  from_email          TEXT,
  sender_email        TEXT,
  display_to          TEXT,
  received_at         INTEGER,
  sent_at             INTEGER,
  last_modified_at    INTEGER,
  is_read             INTEGER NOT NULL DEFAULT 0,
  is_draft            INTEGER NOT NULL DEFAULT 0,
  has_attachments     INTEGER NOT NULL DEFAULT 0,
  importance          TEXT,
  flag_status         TEXT,
  size                INTEGER,
  preview             TEXT,
  body_state          INTEGER NOT NULL DEFAULT 0,
  fts_indexed         INTEGER NOT NULL DEFAULT 0,
  deleted             INTEGER NOT NULL DEFAULT 0,
  updated_at          INTEGER NOT NULL,
  UNIQUE(account_id, server_id)
);
CREATE INDEX IF NOT EXISTS items_folder_received ON items(folder_pk, received_at DESC);
CREATE INDEX IF NOT EXISTS items_conversation ON items(account_id, conversation_id);
CREATE INDEX IF NOT EXISTS items_internet_id ON items(account_id, internet_message_id);
CREATE INDEX IF NOT EXISTS items_pending_bodies ON items(folder_pk, received_at DESC) WHERE body_state = 0 AND deleted = 0;

CREATE TABLE IF NOT EXISTS recipients (
  item_pk    INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  kind       INTEGER NOT NULL,
  name       TEXT,
  email      TEXT,
  position   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS recipients_item ON recipients(item_pk);
CREATE INDEX IF NOT EXISTS recipients_email ON recipients(email);

CREATE TABLE IF NOT EXISTS bodies (
  item_pk       INTEGER PRIMARY KEY REFERENCES items(item_pk) ON DELETE CASCADE,
  body_type     TEXT NOT NULL,
  unique_body   BLOB,
  full_body     BLOB,
  unique_text   TEXT,
  is_truncated  INTEGER NOT NULL DEFAULT 0,
  fetched_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attachments (
  attachment_pk INTEGER PRIMARY KEY,
  item_pk       INTEGER NOT NULL REFERENCES items(item_pk) ON DELETE CASCADE,
  server_id     TEXT NOT NULL,
  name          TEXT,
  content_type  TEXT,
  size          INTEGER,
  is_inline     INTEGER NOT NULL DEFAULT 0,
  content_id    TEXT
);
CREATE INDEX IF NOT EXISTS attachments_item ON attachments(item_pk);

CREATE TABLE IF NOT EXISTS sync_state (
  account_id  INTEGER NOT NULL REFERENCES accounts(account_id),
  key         TEXT NOT NULL,
  value       TEXT,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY(account_id, key)
);

-- Источник текста для индекса: представление над items, recipients и bodies.
-- FTS5 с внешним содержимым читает текст отсюда при построении сниппетов,
-- поэтому тексты в базе не дублируются. rowid индекса = item_pk.
CREATE VIEW IF NOT EXISTS items_fts_src AS
  SELECT i.item_pk,
         COALESCE(i.subject, '')    AS subject,
         COALESCE(i.from_name, '')  AS from_name,
         COALESCE(i.from_email, '') AS from_email,
         COALESCE((SELECT group_concat(COALESCE(r.name, r.email), ', ')
                     FROM recipients r WHERE r.item_pk = i.item_pk AND r.kind IN (1, 2)), '') AS recipients,
         COALESCE(b.unique_text, '') AS unique_text
    FROM items i LEFT JOIN bodies b ON b.item_pk = i.item_pk;

CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
  subject, from_name, from_email, recipients, unique_text,
  content='items_fts_src', content_rowid='item_pk',
  tokenize = 'unicode61 remove_diacritics 2'
);
"#;

/// Версия 2: индекс с внешним содержимым вместо contentless и флаг `fts_indexed`.
const V2_FTS: &str = r#"
DROP TABLE IF EXISTS items_fts;
CREATE VIRTUAL TABLE items_fts USING fts5(
  subject, from_name, from_email, recipients, unique_text,
  content='items_fts_src', content_rowid='item_pk',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO items_fts(items_fts) VALUES('rebuild');
UPDATE items SET fts_indexed = 1 WHERE deleted = 0;
UPDATE items SET fts_indexed = 0 WHERE deleted = 1;
"#;

/// Версия 3, таблицы: кэш и текст вложений, беседы и их агрегаты по папкам.
const V3_TABLES: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS attachments_item_server ON attachments(item_pk, server_id);
CREATE INDEX IF NOT EXISTS attachments_cached ON attachments(last_access_at) WHERE local_path IS NOT NULL;
CREATE INDEX IF NOT EXISTS attachments_pending ON attachments(item_pk) WHERE fetched_at IS NULL AND text_state = 0 AND is_inline = 0;

CREATE TABLE IF NOT EXISTS conversations (
  conversation_pk  INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id        TEXT NOT NULL,
  topic            TEXT,
  last_received_at INTEGER,
  item_count       INTEGER NOT NULL DEFAULT 0,
  unread_count     INTEGER NOT NULL DEFAULT 0,
  has_attachments  INTEGER NOT NULL DEFAULT 0,
  flag_status      INTEGER NOT NULL DEFAULT 0,
  sync_state       TEXT,
  UNIQUE(account_id, server_id)
);

-- Беседа видна в папке, если в ней есть хоть одно её письмо; агрегаты пересчитываются
-- в той же транзакции, что меняет письма.
CREATE TABLE IF NOT EXISTS conversation_folders (
  conversation_pk  INTEGER NOT NULL REFERENCES conversations(conversation_pk) ON DELETE CASCADE,
  folder_pk        INTEGER NOT NULL REFERENCES folders(folder_pk) ON DELETE CASCADE,
  last_received_at INTEGER NOT NULL,
  item_count       INTEGER NOT NULL,
  unread_count     INTEGER NOT NULL,
  PRIMARY KEY (conversation_pk, folder_pk)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS conversation_folders_list ON conversation_folders(folder_pk, last_received_at DESC);
"#;

/// Версия 3, заполнение бесед по уже загруженным письмам.
const V3_BACKFILL: &str = r#"
INSERT OR IGNORE INTO conversations(account_id, server_id)
  SELECT DISTINCT account_id, COALESCE(conversation_id, 'item:' || server_id) FROM items;
UPDATE items SET conversation_pk = (
  SELECT c.conversation_pk FROM conversations c
   WHERE c.account_id = items.account_id AND c.server_id = COALESCE(items.conversation_id, 'item:' || items.server_id));
INSERT OR REPLACE INTO conversation_folders(conversation_pk, folder_pk, last_received_at, item_count, unread_count)
  SELECT conversation_pk, folder_pk, COALESCE(MAX(COALESCE(received_at, sent_at)), 0), COUNT(*), COALESCE(SUM(is_read = 0), 0)
    FROM items WHERE deleted = 0 AND conversation_pk IS NOT NULL GROUP BY conversation_pk, folder_pk;
WITH agg AS (
  SELECT conversation_pk, COUNT(*) AS n, COALESCE(SUM(is_read = 0), 0) AS u,
         MAX(COALESCE(received_at, sent_at)) AS last, MAX(has_attachments) AS ha
    FROM items WHERE deleted = 0 AND conversation_pk IS NOT NULL GROUP BY conversation_pk)
UPDATE conversations SET item_count = agg.n, unread_count = agg.u, last_received_at = agg.last, has_attachments = agg.ha
  FROM agg WHERE agg.conversation_pk = conversations.conversation_pk;
UPDATE conversations SET topic = (
  SELECT aiowa_topic(i.subject) FROM items i
   WHERE i.conversation_pk = conversations.conversation_pk AND i.deleted = 0
   ORDER BY COALESCE(i.received_at, i.sent_at) LIMIT 1);
"#;

/// Представление-источник индекса (версия 3): нормализация ё→е и текст вложений.
/// `fts_norm`, `norm_lower` и `aiowa_topic` — скалярные функции из Rust, регистрируются
/// на каждом соединении до первого обращения к представлению.
const V3_VIEW: &str = r#"
DROP VIEW IF EXISTS items_fts_src;
CREATE VIEW items_fts_src AS
  SELECT i.item_pk,
         fts_norm(COALESCE(i.subject, ''))    AS subject,
         fts_norm(COALESCE(i.from_name, ''))  AS from_name,
         COALESCE(i.from_email, '')           AS from_email,
         fts_norm(COALESCE((SELECT group_concat(COALESCE(r.name, '') || ' ' || COALESCE(r.email, ''), ', ')
                     FROM recipients r WHERE r.item_pk = i.item_pk AND r.kind IN (1, 2)), '')) AS recipients,
         fts_norm(COALESCE(b.unique_text, '')) AS unique_text,
         fts_norm(COALESCE((SELECT group_concat(COALESCE(a.name, '') || char(10) || COALESCE(a.text, ''), char(10))
                     FROM attachments a WHERE a.item_pk = i.item_pk AND a.is_inline = 0), '')) AS attachment_text
    FROM items i LEFT JOIN bodies b ON b.item_pk = i.item_pk;
"#;

const V3_FTS: &str = r#"
DROP TABLE IF EXISTS items_fts;
CREATE VIRTUAL TABLE items_fts USING fts5(
  subject, from_name, from_email, recipients, unique_text, attachment_text,
  content='items_fts_src', content_rowid='item_pk',
  tokenize = 'unicode61 remove_diacritics 2'
);
INSERT INTO items_fts(items_fts) VALUES('rebuild');
UPDATE items SET fts_indexed = 1 WHERE deleted = 0;
UPDATE items SET fts_indexed = 0 WHERE deleted = 1;
"#;

/// Версия 4: кэш календаря. `calendar_items` — экземпляры событий, как их отдаёт `FindItem` с
/// `CalendarView` (повторяющиеся встречи уже развёрнуты, у каждого экземпляра свой `server_id`);
/// моменты в миллисекундах UTC; тело и участники появляются после `GetItem` (`details_at`).
/// `calendar_ranges` — окна, которые уже забирали с сервера, со временем загрузки.
const V4_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS calendar_items (
  calendar_pk      INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL REFERENCES accounts(account_id),
  server_id        TEXT NOT NULL,
  change_key       TEXT,
  subject          TEXT,
  start_at         INTEGER NOT NULL,
  end_at           INTEGER NOT NULL,
  is_all_day       INTEGER NOT NULL DEFAULT 0,
  location         TEXT,
  organizer_name   TEXT,
  organizer_email  TEXT,
  free_busy        TEXT NOT NULL DEFAULT 'Busy',
  calendar_type    TEXT NOT NULL DEFAULT 'Single',
  is_meeting       INTEGER NOT NULL DEFAULT 0,
  is_cancelled     INTEGER NOT NULL DEFAULT 0,
  my_response      TEXT NOT NULL DEFAULT 'Unknown',
  sensitivity      TEXT,
  has_attachments  INTEGER NOT NULL DEFAULT 0,
  last_modified_at INTEGER,
  body_html        BLOB,
  attendees_json   TEXT,
  reminder_minutes INTEGER,
  recurrence_text  TEXT,
  fetched_at       INTEGER NOT NULL,
  details_at       INTEGER,
  UNIQUE(account_id, server_id)
);
CREATE INDEX IF NOT EXISTS calendar_items_range ON calendar_items(account_id, start_at, end_at);

CREATE TABLE IF NOT EXISTS calendar_ranges (
  account_id  INTEGER NOT NULL REFERENCES accounts(account_id),
  start_at    INTEGER NOT NULL,
  end_at      INTEGER NOT NULL,
  fetched_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS calendar_ranges_account ON calendar_ranges(account_id, start_at);
"#;

/// Версия 5: кэш справочника организации. `directory_people` — записи `FindPeople`/`ResolveNames`/
/// `ExpandDL` по SMTP-адресу в нижнем регистре; имя и вид (`mailbox`, `list`, `contact`, `room`,
/// `other`) — колонками, остальные поля (должность, отдел, компания, офис, телефоны, псевдоним) —
/// объектом JSON. Индексы по адресу без учёта регистра нужны счётчикам переписки карточки
/// контакта (`Store::contact_local_stats`): без них — полный проход по `items`.
const V5_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS directory_people (
  account_id   INTEGER NOT NULL REFERENCES accounts(account_id),
  email        TEXT NOT NULL,
  display_name TEXT,
  kind         TEXT NOT NULL DEFAULT 'other',
  json         TEXT NOT NULL DEFAULT '{}',
  fetched_at   INTEGER NOT NULL,
  PRIMARY KEY (account_id, email)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS items_from_lower ON items(account_id, lower(from_email));
CREATE INDEX IF NOT EXISTS recipients_email_lower ON recipients(lower(email));
"#;

pub const FTS_COLUMNS: &str = "subject, from_name, from_email, recipients, unique_text, attachment_text";

/// Удаляет строку из индекса по текущим значениям источника. Вызывать ДО изменения
/// items/bodies/recipients/attachments, иначе старые токены останутся в индексе навсегда.
pub const FTS_DELETE_SQL: &str = "INSERT INTO items_fts(items_fts, rowid, subject, from_name, from_email, recipients, unique_text, attachment_text)
    SELECT 'delete', item_pk, subject, from_name, from_email, recipients, unique_text, attachment_text FROM items_fts_src WHERE item_pk = ?1";

/// Добавляет строку в индекс по текущим значениям источника.
pub const FTS_INSERT_SQL: &str = "INSERT INTO items_fts(rowid, subject, from_name, from_email, recipients, unique_text, attachment_text)
    SELECT item_pk, subject, from_name, from_email, recipients, unique_text, attachment_text FROM items_fts_src WHERE item_pk = ?1";

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n == column))
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
    if !has_column(conn, table, column)? {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

fn scalar_text(ctx: &rusqlite::functions::Context<'_>, f: fn(&str) -> String) -> rusqlite::Result<Option<String>> {
    Ok(match ctx.get_raw(0) {
        ValueRef::Null => None,
        ValueRef::Text(b) => Some(f(std::str::from_utf8(b).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?)),
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(r) => Some(r.to_string()),
        ValueRef::Blob(_) => None,
    })
}

/// Регистрирует скалярные функции, которые использует схема (представление индекса,
/// сравнения без учёта регистра и ё). Вызывать на каждом соединении до миграций.
/// `zstd_text(blob)` распаковывает сжатое тело письма — для отладочных запросов `aiowa sql`.
pub fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS;
    conn.create_scalar_function("fts_norm", 1, flags, |ctx| scalar_text(ctx, text::fts_norm))?;
    conn.create_scalar_function("norm_lower", 1, flags, |ctx| scalar_text(ctx, text::norm_lower))?;
    conn.create_scalar_function("aiowa_topic", 1, flags, |ctx| scalar_text(ctx, text::topic_of))?;
    conn.create_scalar_function("zstd_text", 1, flags, |ctx| -> rusqlite::Result<Option<String>> {
        match ctx.get_raw(0) {
            ValueRef::Blob(b) => Ok(Some(crate::decompress(b).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?)),
            _ => Ok(None),
        }
    })?;
    Ok(())
}

/// Версия 6: локальные черновики композера (docs/obkatka.md п. 22).
const V6_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS local_drafts (
  draft_id          TEXT PRIMARY KEY,
  account_id        INTEGER NOT NULL REFERENCES accounts(account_id),
  kind              TEXT NOT NULL,
  reference_item_pk INTEGER,
  draft_server_id   TEXT,
  subject           TEXT NOT NULL DEFAULT '',
  to_json           TEXT NOT NULL DEFAULT '[]',
  cc_json           TEXT NOT NULL DEFAULT '[]',
  bcc_json          TEXT NOT NULL DEFAULT '[]',
  body_html         TEXT NOT NULL DEFAULT '',
  attachments_json  TEXT NOT NULL DEFAULT '[]',
  removed_json      TEXT NOT NULL DEFAULT '[]',
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS local_drafts_account ON local_drafts(account_id, updated_at DESC);
"#;

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= CURRENT_VERSION {
        return Ok(());
    }
    if version < 1 {
        conn.execute_batch(V1)?;
        conn.pragma_update(None, "user_version", 1)?;
        info!("схема базы создана (версия 1)");
    }
    if version < 2 {
        if !has_column(conn, "items", "fts_indexed")? {
            conn.execute_batch("ALTER TABLE items ADD COLUMN fts_indexed INTEGER NOT NULL DEFAULT 0;")?;
        }
        // Представление могло отсутствовать в ранних базах версии 1.
        let view_sql = V1.split("CREATE VIEW IF NOT EXISTS items_fts_src").nth(1).map(|rest| format!("CREATE VIEW IF NOT EXISTS items_fts_src{}", rest.split(';').next().unwrap_or(""))).unwrap_or_default();
        if !view_sql.is_empty() {
            conn.execute_batch(&format!("{view_sql};"))?;
        }
        conn.execute_batch(V2_FTS)?;
        delete_tombstones_from_fts(conn)?;
        conn.pragma_update(None, "user_version", 2)?;
        info!("схема базы обновлена до версии 2: индекс пересобран");
    }
    if version < 3 {
        let started = std::time::Instant::now();
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            add_column_if_missing(conn, "attachments", "local_path", "TEXT")?;
            add_column_if_missing(conn, "attachments", "sha256", "BLOB")?;
            add_column_if_missing(conn, "attachments", "text", "TEXT")?;
            add_column_if_missing(conn, "attachments", "text_state", "INTEGER NOT NULL DEFAULT 0")?;
            add_column_if_missing(conn, "attachments", "fetched_at", "INTEGER")?;
            add_column_if_missing(conn, "attachments", "last_access_at", "INTEGER")?;
            // Дубли (item_pk, server_id) в старых базах невозможны: вложения писались заменой целиком.
            conn.execute_batch(V3_TABLES)?;
            add_column_if_missing(conn, "items", "conversation_pk", "INTEGER REFERENCES conversations(conversation_pk)")?;
            conn.execute_batch("CREATE INDEX IF NOT EXISTS items_conv_folder ON items(conversation_pk, folder_pk);")?;
            conn.execute_batch(V3_BACKFILL)?;
            conn.execute_batch(V3_VIEW)?;
            conn.execute_batch(V3_FTS)?;
            delete_tombstones_from_fts(conn)?;
            conn.pragma_update(None, "user_version", 3)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!(elapsed_s = started.elapsed().as_secs_f32(), "схема базы обновлена до версии 3: вложения, беседы, индекс пересобран");
    }
    if version < 4 {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch(V4_TABLES)?;
            conn.pragma_update(None, "user_version", 4)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!("схема базы обновлена до версии 4: кэш календаря");
    }
    if version < 5 {
        let started = std::time::Instant::now();
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch(V5_TABLES)?;
            conn.pragma_update(None, "user_version", 5)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!(elapsed_s = started.elapsed().as_secs_f32(), "схема базы обновлена до версии 5: кэш справочника");
    }
    if version < 6 {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch(V6_TABLES)?;
            conn.pragma_update(None, "user_version", 6)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!("схема базы обновлена до версии 6: локальные черновики");
    }
    if version < 7 {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            // Последнее действие с письмом (ответ, пересылка): код PR_LAST_VERB_EXECUTED, когда,
            // и запрашивалось ли оно у сервера (для дозапроса по уже скачанным письмам).
            add_column_if_missing(conn, "items", "last_verb", "INTEGER")?;
            add_column_if_missing(conn, "items", "last_verb_at", "INTEGER")?;
            add_column_if_missing(conn, "items", "verb_checked", "INTEGER NOT NULL DEFAULT 0")?;
            conn.pragma_update(None, "user_version", 7)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!("схема базы обновлена до версии 7: последнее действие с письмом");
    }
    if version < 8 {
        // Не почтовые элементы (контакты, встречи, задачи, заметки в почтовых папках) записывались
        // непрочитанными: у них нет IsRead. Снимаем признак и пересчитываем счётчики бесед.
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch(V8_NON_MAIL_READ)?;
            conn.pragma_update(None, "user_version", 8)?;
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(e);
            }
        }
        info!("схема базы обновлена до версии 8: не почтовые элементы не считаются непрочитанными");
    }
    Ok(())
}

/// Версия 8: снять «непрочитано» с контактов, встреч, задач и заметок, пересчитать беседы.
const V8_NON_MAIL_READ: &str = r#"
UPDATE items SET is_read = 1 WHERE is_read = 0 AND (item_class LIKE 'IPM.Contact%' OR item_class LIKE 'IPM.DistList%'
  OR item_class LIKE 'IPM.Appointment%' OR item_class LIKE 'IPM.Task%' OR item_class LIKE 'IPM.StickyNote%' OR item_class LIKE 'IPM.Activity%');
UPDATE conversation_folders SET unread_count = (SELECT COUNT(*) FROM items i WHERE i.conversation_pk = conversation_folders.conversation_pk
  AND i.folder_pk = conversation_folders.folder_pk AND i.deleted = 0 AND i.is_read = 0)
 WHERE conversation_pk IN (SELECT conversation_pk FROM items WHERE conversation_pk IS NOT NULL AND (item_class LIKE 'IPM.Contact%' OR item_class LIKE 'IPM.DistList%'
  OR item_class LIKE 'IPM.Appointment%' OR item_class LIKE 'IPM.Task%' OR item_class LIKE 'IPM.StickyNote%' OR item_class LIKE 'IPM.Activity%'));
UPDATE conversations SET unread_count = (SELECT COUNT(*) FROM items i WHERE i.conversation_pk = conversations.conversation_pk AND i.deleted = 0 AND i.is_read = 0)
 WHERE conversation_pk IN (SELECT conversation_pk FROM items WHERE conversation_pk IS NOT NULL AND (item_class LIKE 'IPM.Contact%' OR item_class LIKE 'IPM.DistList%'
  OR item_class LIKE 'IPM.Appointment%' OR item_class LIKE 'IPM.Task%' OR item_class LIKE 'IPM.StickyNote%' OR item_class LIKE 'IPM.Activity%'));
"#;

/// После `rebuild` в индексе есть и удалённые письма (представление их не отличает):
/// снимаем их поштучно.
fn delete_tombstones_from_fts(conn: &Connection) -> rusqlite::Result<()> {
    let mut st = conn.prepare("SELECT item_pk FROM items WHERE deleted = 1")?;
    let tombstones = st.query_map([], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(st);
    for pk in tombstones {
        conn.execute("DELETE FROM items_fts WHERE rowid = ?1", [pk])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_in_memory_twice() {
        let conn = Connection::open_in_memory().unwrap();
        register_functions(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, CURRENT_VERSION);
        // FTS5 доступна в bundled-сборке; внешнее содержимое читается из представления.
        conn.execute("INSERT INTO accounts(account_id, host, login, created_at) VALUES (1, 'h', 'u', 0)", []).unwrap();
        conn.execute("INSERT INTO folders(folder_pk, account_id, server_id, display_name, updated_at) VALUES (1, 1, 'F', 'Папка', 0)", []).unwrap();
        conn.execute("INSERT INTO items(item_pk, account_id, server_id, folder_pk, subject, updated_at) VALUES (7, 1, 'I', 1, 'тема про письмо и счёт', 0)", []).unwrap();
        conn.execute("INSERT INTO attachments(item_pk, server_id, name, text, text_state) VALUES (7, 'A', 'отчёт.pdf', 'в файле написано про шлёпанцы', 1)", []).unwrap();
        conn.execute(FTS_INSERT_SQL, [7i64]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        // ё→е: запрос без ё находит текст с ё, и наоборот через нормализацию запроса.
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'счет'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        // Текст вложения в своей колонке; фильтр по колонке со скобками работает.
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'attachment_text: (\"шлепанц\"* \"отчет\"*)'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH '{from_name from_email}: (\"шлепанц\"*)'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        let snip: String = conn.query_row("SELECT snippet(items_fts, 0, '[', ']', '…', 8) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert!(snip.contains("[письмо]"), "{snip}");
        conn.execute(FTS_DELETE_SQL, [7i64]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn migrates_v2_database_with_conversations() {
        // База версии 2 с письмами и вложением: после миграции беседы и агрегаты заполнены.
        let conn = Connection::open_in_memory().unwrap();
        register_functions(&conn).unwrap();
        conn.execute_batch(V1).unwrap();
        conn.execute_batch(V2_FTS).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute("INSERT INTO accounts(account_id, host, login, created_at) VALUES (1, 'h', 'u', 0)", []).unwrap();
        conn.execute("INSERT INTO folders(folder_pk, account_id, server_id, display_name, updated_at) VALUES (1, 1, 'F', 'Входящие', 0)", []).unwrap();
        conn.execute_batch(
            "INSERT INTO items(item_pk, account_id, server_id, folder_pk, conversation_id, subject, received_at, is_read, has_attachments, updated_at)
               VALUES (1, 1, 'I1', 1, 'C1', 'RE: Договор', 200, 0, 1, 0), (2, 1, 'I2', 1, 'C1', 'Договор', 100, 1, 0, 0), (3, 1, 'I3', 1, NULL, 'Одиночное', 50, 1, 0, 0);
             INSERT INTO attachments(item_pk, server_id, name, is_inline) VALUES (1, 'A1', 'скан.pdf', 0);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let (topic, n, u, last, ha): (String, i64, i64, i64, i64) = conn
            .query_row("SELECT topic, item_count, unread_count, last_received_at, has_attachments FROM conversations WHERE server_id = 'C1'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap();
        assert_eq!((topic.as_str(), n, u, last, ha), ("Договор", 2, 1, 200, 1));
        let cf: i64 = conn.query_row("SELECT COUNT(*) FROM conversation_folders WHERE folder_pk = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(cf, 2, "две беседы в папке, одиночное письмо получило свою");
        let linked: i64 = conn.query_row("SELECT COUNT(*) FROM items WHERE conversation_pk IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(linked, 3);
        let ts: i64 = conn.query_row("SELECT text_state FROM attachments WHERE server_id = 'A1'", [], |r| r.get(0)).unwrap();
        assert_eq!(ts, TEXT_PENDING);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'attachment_text: скан'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "имя вложения проиндексировано при пересборке");
        // Версия 4 применилась следом: таблицы календаря есть и пусты.
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, CURRENT_VERSION);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM calendar_items", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM calendar_ranges", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        // И версия 5: кэш справочника пуст.
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM directory_people", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn migrates_v4_database_to_directory_cache_idempotently() {
        // База версии 4 (таблицы v5 уже частично есть, как после прерванной миграции): повторное
        // применение не падает, версия растёт, индексы адресов без учёта регистра используются.
        let conn = Connection::open_in_memory().unwrap();
        register_functions(&conn).unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, CURRENT_VERSION);
        // Версия 5 → 6 (локальные черновики) тоже повторяема.
        conn.pragma_update(None, "user_version", 5).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, CURRENT_VERSION);
        assert_eq!(conn.query_row("SELECT count(*) FROM local_drafts", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert!(has_column(&conn, "items", "last_verb").unwrap() && has_column(&conn, "items", "verb_checked").unwrap());
        let plan: String = conn
            .query_row("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM items WHERE account_id = 1 AND lower(from_email) = 'a@x.ru'", [], |r| r.get::<_, String>(3))
            .unwrap();
        assert!(plan.contains("items_from_lower"), "{plan}");
        let plan: String = conn.query_row("EXPLAIN QUERY PLAN SELECT item_pk FROM recipients WHERE lower(email) = 'a@x.ru'", [], |r| r.get::<_, String>(3)).unwrap();
        assert!(plan.contains("recipients_email_lower"), "{plan}");
    }
}
