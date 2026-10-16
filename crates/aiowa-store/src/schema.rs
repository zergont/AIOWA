//! Схема базы и миграции через `PRAGMA user_version`.
//! Правило: миграции только добавляют; переименования и удаления колонок через новую таблицу.

use rusqlite::Connection;
use tracing::info;

pub const CURRENT_VERSION: i64 = 2;

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

/// Удаляет строку из индекса по текущим значениям источника. Вызывать ДО изменения
/// items/bodies/recipients, иначе старые токены останутся в индексе навсегда.
pub const FTS_DELETE_SQL: &str = "INSERT INTO items_fts(items_fts, rowid, subject, from_name, from_email, recipients, unique_text)
    SELECT 'delete', item_pk, subject, from_name, from_email, recipients, unique_text FROM items_fts_src WHERE item_pk = ?1";

/// Добавляет строку в индекс по текущим значениям источника.
pub const FTS_INSERT_SQL: &str = "INSERT INTO items_fts(rowid, subject, from_name, from_email, recipients, unique_text)
    SELECT item_pk, subject, from_name, from_email, recipients, unique_text FROM items_fts_src WHERE item_pk = ?1";

/// Версия 2: индекс с внешним содержимым вместо contentless и флаг `fts_indexed`.
/// Идемпотентна: базы, созданные ранними сборками версии 1 без колонки и представления,
/// приводятся к тому же виду, что и новые; индекс пересобирается из таблиц.
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

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n == column))
}

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
        // Удалённые строки в индексе не нужны: удаляем их из представления на время пересборки
        // нельзя, поэтому пересобираем всё и затем снимаем надгробия поштучно.
        conn.execute_batch(V2_FTS)?;
        let mut st = conn.prepare("SELECT item_pk FROM items WHERE deleted = 1")?;
        let tombstones = st.query_map([], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(st);
        for pk in tombstones {
            conn.execute("DELETE FROM items_fts WHERE rowid = ?1", [pk])?;
        }
        conn.pragma_update(None, "user_version", 2)?;
        info!("схема базы обновлена до версии 2: индекс пересобран");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_in_memory_twice() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, CURRENT_VERSION);
        // FTS5 доступна в bundled-сборке; внешнее содержимое читается из представления.
        conn.execute("INSERT INTO accounts(account_id, host, login, created_at) VALUES (1, 'h', 'u', 0)", []).unwrap();
        conn.execute("INSERT INTO folders(folder_pk, account_id, server_id, display_name, updated_at) VALUES (1, 1, 'F', 'Папка', 0)", []).unwrap();
        conn.execute("INSERT INTO items(item_pk, account_id, server_id, folder_pk, subject, updated_at) VALUES (7, 1, 'I', 1, 'тема про письмо', 0)", []).unwrap();
        conn.execute(FTS_INSERT_SQL, [7i64]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let snip: String = conn.query_row("SELECT snippet(items_fts, 0, '[', ']', '…', 8) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert!(snip.contains("[письмо]"), "{snip}");
        conn.execute(FTS_DELETE_SQL, [7i64]).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'письм*'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
