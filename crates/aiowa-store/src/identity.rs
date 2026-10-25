//! Чей ящик в базе и сводка базы по пути.
//!
//! Один ящик — одна база при любой форме логина (`savvidi`, `savvidi@ngenergo`,
//! `NGENERGO\savvidi`). После входа сервер сообщает адрес ящика и `MailboxGuid`
//! (`sessiondata.ashx`); окно записывает их в `sync_state` учётной записи
//! (`desktop.mailbox_guid`, `desktop.mailbox_email`) и по ним узнаёт базу ящика на диске и не
//! синхронизирует ящик в чужую базу. У баз, заведённых до этого, личности нет: их узнают по
//! логину без домена (`login_key`).
//!
//! Сводка (`read_summary`) читает базу по пути только на чтение, без миграций и без изменения
//! файлов: мастер и вкладка «Данные» смотрят и чужие базы, и базы старых версий.

use crate::{Result, Store, StoreError};
use aiowa_core::AccountId;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// `MailboxGuid` ящика (строчные цифры с дефисами, без скобок).
pub const MAILBOX_GUID_KEY: &str = "desktop.mailbox_guid";
/// Основной адрес ящика в нижнем регистре.
pub const MAILBOX_EMAIL_KEY: &str = "desktop.mailbox_email";
/// Время последней успешной синхронизации из окна, мс UTC (пишет окно, `sync.rs`).
pub const LAST_SYNC_OK_KEY: &str = "desktop.last_sync_ok_ms";

/// Личность ящика, записанная в базе. Поля пусты у баз, заведённых до записи личности.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxIdentity {
    pub mailbox_guid: Option<String>,
    pub email: Option<String>,
}

/// Логин без домена в нижнем регистре: `savvidi`, `savvidi@ngenergo`, `NGENERGO\savvidi` и
/// `Savvidi@NGENERGO.RU` дают `savvidi`. По нему узнаётся база, заведённая под другой формой
/// логина того же ящика.
pub fn login_key(login: &str) -> String {
    let s = login.trim();
    let s = s.rsplit_once('\\').map_or(s, |(_, user)| user);
    let s = s.split_once('@').map_or(s, |(user, _)| user);
    s.trim().to_lowercase()
}

/// Часть адреса до `@` в нижнем регистре (для сравнения с `login_key`).
pub fn email_local_part(email: &str) -> String {
    login_key(email.split_once('@').map_or(email, |(local, _)| local))
}

/// Учётная запись внутри базы: строка `accounts` с личностью и счётчиками.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbAccount {
    pub host: String,
    pub login: String,
    pub identity: MailboxIdentity,
    /// Писем (не удалённых).
    pub items: i64,
    /// Последняя успешная синхронизация из окна, иначе самая поздняя синхронизация папки, мс UTC.
    pub last_sync_ms: Option<i64>,
}

/// Сводка базы по пути: учётные записи, письма, время синхронизации и место на диске.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbSummary {
    pub path: PathBuf,
    pub accounts: Vec<DbAccount>,
    /// Писем во всех учётных записях базы.
    pub items: i64,
    pub last_sync_ms: Option<i64>,
    /// Файл базы и журналы `-wal` и `-shm`.
    pub db_bytes: u64,
    /// Каталог кэша вложений рядом с базой (`attachments`, со вложенными каталогами).
    pub cache_bytes: u64,
}

impl DbSummary {
    pub fn total_bytes(&self) -> u64 {
        self.db_bytes + self.cache_bytes
    }

    /// Личность ящика базы: первая учётная запись с GUID, иначе первая с адресом.
    pub fn identity(&self) -> MailboxIdentity {
        self.accounts
            .iter()
            .find(|a| a.identity.mailbox_guid.is_some())
            .or_else(|| self.accounts.iter().find(|a| a.identity.email.is_some()))
            .map(|a| a.identity.clone())
            .unwrap_or_default()
    }
}

/// Каталог баз по умолчанию: `%LOCALAPPDATA%\AIOWA` (внутри — `<сервер>\<ящик>\mail.db`, как у
/// `default_db_path`).
pub fn data_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("AIOWA")
}

/// Каталоги `<root>\<сервер>\*` (без сортировки).
fn mailbox_dirs(root: &Path) -> Vec<PathBuf> {
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p).map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()).unwrap_or_default()
    };
    dirs(root).iter().flat_map(|host| dirs(host)).collect()
}

/// Базы под каталогом `root`: `<root>\<сервер>\<ящик>\mail.db`, по пути. Каталоги, которые
/// удаляются (`<ящик>.deleting-<время>`), сюда не входят: их перечисляет `scan_leftovers`.
pub fn scan_databases(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = mailbox_dirs(root).into_iter().filter(|d| !is_deleting_dir(d)).map(|d| d.join("mail.db")).filter(|p| p.is_file()).collect();
    out.sort();
    out
}

/// Остатки неполного удаления под `root`: каталоги `<root>\<сервер>\<ящик>.deleting-<время>`
/// (переименованы перед удалением, но удалены не до конца — файл держала другая программа), по
/// пути. Вкладка «Данные» показывает их, чтобы место не терялось незаметно.
pub fn scan_leftovers(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = mailbox_dirs(root).into_iter().filter(|d| is_deleting_dir(d)).collect();
    out.sort();
    out
}

/// Метка каталога базы, который сейчас удаляется (переименован перед удалением).
pub const DELETING_MARK: &str = ".deleting-";

/// Каталог переименован перед удалением: `<имя>.deleting-<цифры>`.
pub fn is_deleting_dir(dir: &Path) -> bool {
    let Some(name) = dir.file_name().map(|n| n.to_string_lossy()) else { return false };
    name.rsplit_once(DELETING_MARK).is_some_and(|(base, stamp)| !base.is_empty() && !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()))
}

/// Путь журнала SQLite рядом с базой: `mail.db` + `-wal` → `mail.db-wal`.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Размер базы на диске: файл и журналы `-wal` и `-shm` (нет файла — 0). Один подсчёт для
/// строки состояния, настроек и вкладки «Данные» (`Store::disk_size_bytes` зовёт его же).
pub fn db_files_bytes(db: &Path) -> u64 {
    ["", "-wal", "-shm"].iter().filter_map(|suffix| std::fs::metadata(sidecar(db, suffix)).ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

/// Каталог кэша вложений базы: `attachments` рядом с файлом базы.
pub fn attachments_dir_for(db: &Path) -> PathBuf {
    db.parent().map(|d| d.join("attachments")).unwrap_or_else(|| PathBuf::from("attachments"))
}

/// Размер каталога со всеми вложенными (нет каталога — 0). Ссылки не разыменовываются. Кэш
/// вложений считается только так (`Store::attachment_cache_bytes` зовёт его же).
pub fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Соединение только на чтение. Если журнала `-wal` нет, база закрыта чисто и всё лежит в
/// основном файле: она открывается неизменяемой (`immutable=1`), и SQLite не создаёт рядом
/// `-shm`. Иначе — обычное чтение рядом с пишущим процессом (журнал и `-shm` уже есть).
fn open_read_only(path: &Path) -> Result<Connection> {
    if !path.is_file() {
        return Err(StoreError::Other(format!("нет файла базы {}", path.display())));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let immutable = !sidecar(path, "-wal").exists() && !path.as_os_str().to_string_lossy().starts_with(r"\\");
    let uri = file_uri(path, immutable);
    let conn = Connection::open_with_flags(&uri, flags)?;
    conn.execute_batch("PRAGMA query_only = 1; PRAGMA busy_timeout = 2000;")?;
    Ok(conn)
}

/// URI SQLite для пути: `file:///D:/x/mail.db?mode=ro[&immutable=1]`. Разделители — `/`;
/// `%`, `?` и `#` кодируются, остальное (в том числе кириллица) — как есть.
fn file_uri(path: &Path, immutable: bool) -> String {
    let mut p = String::new();
    for c in path.to_string_lossy().chars() {
        match c {
            '\\' => p.push('/'),
            '%' => p.push_str("%25"),
            '?' => p.push_str("%3F"),
            '#' => p.push_str("%23"),
            c => p.push(c),
        }
    }
    let prefix = if p.starts_with('/') { "file://" } else { "file:///" };
    format!("{prefix}{p}?mode=ro{}", if immutable { "&immutable=1" } else { "" })
}

fn state_value(c: &Connection, account_id: i64, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM sync_state WHERE account_id = ?1 AND key = ?2", params![account_id, key], |r| r.get::<_, Option<String>>(0)).optional().ok().flatten().flatten().filter(|v| !v.trim().is_empty())
}

/// Сводка базы по пути: только чтение, без миграций; таблиц старой схемы, которых нет, просто
/// не видно в цифрах. Файла нет или это не база — ошибка.
pub fn read_summary(path: &Path) -> Result<DbSummary> {
    let started = std::time::Instant::now();
    let conn = open_read_only(path)?;
    let rows: Vec<(i64, String, String)> = {
        let mut st = conn.prepare("SELECT account_id, host, login FROM accounts ORDER BY account_id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut accounts = Vec::with_capacity(rows.len());
    for (id, host, login) in rows {
        let identity = MailboxIdentity { mailbox_guid: state_value(&conn, id, MAILBOX_GUID_KEY), email: state_value(&conn, id, MAILBOX_EMAIL_KEY) };
        let items = conn.query_row("SELECT COUNT(*) FROM items WHERE account_id = ?1 AND deleted = 0", params![id], |r| r.get::<_, i64>(0)).unwrap_or(0);
        let last_ok = state_value(&conn, id, LAST_SYNC_OK_KEY).and_then(|v| v.trim().parse::<i64>().ok());
        let last_folder = conn.query_row("SELECT MAX(last_synced_at) FROM folders WHERE account_id = ?1", params![id], |r| r.get::<_, Option<i64>>(0)).ok().flatten();
        accounts.push(DbAccount { host, login, identity, items, last_sync_ms: last_ok.or(last_folder) });
    }
    drop(conn);
    let summary = DbSummary {
        path: path.to_path_buf(),
        items: accounts.iter().map(|a| a.items).sum(),
        last_sync_ms: accounts.iter().filter_map(|a| a.last_sync_ms).max(),
        accounts,
        db_bytes: db_files_bytes(path),
        cache_bytes: dir_bytes(&attachments_dir_for(path)),
    };
    debug!(accounts = summary.accounts.len(), items = summary.items, ms = started.elapsed().as_millis(), "сводка базы прочитана");
    Ok(summary)
}

impl Store {
    /// Личность ящика учётной записи (что записано; пустые значения — `None`).
    pub fn mailbox_identity(&self, account: AccountId) -> Result<MailboxIdentity> {
        let get = |key: &str| -> Result<Option<String>> { Ok(self.get_state(account, key)?.filter(|v| !v.trim().is_empty())) };
        Ok(MailboxIdentity { mailbox_guid: get(MAILBOX_GUID_KEY)?, email: get(MAILBOX_EMAIL_KEY)? })
    }

    /// Записать известные поля личности; `None` прежнее значение не стирает.
    pub fn set_mailbox_identity(&self, account: AccountId, identity: &MailboxIdentity) -> Result<()> {
        if let Some(guid) = &identity.mailbox_guid {
            self.set_state(account, MAILBOX_GUID_KEY, Some(guid))?;
        }
        if let Some(email) = &identity.email {
            self.set_state(account, MAILBOX_EMAIL_KEY, Some(&email.to_lowercase()))?;
        }
        Ok(())
    }

    /// Учётная запись базы для входа с этим логином. Логин в базе мог быть записан в другой форме
    /// (`savvidi@ngenergo` вместо `savvidi`), и тогда простой `account` завёл бы рядом пустую
    /// учётную запись. Порядок:
    /// 1. записи этого сервера с тем же логином без домена (`login_key`) или с записанным адресом
    ///    ящика, у которого часть до `@` совпадает с ним; из нескольких — та, где больше папок
    ///    (пустая запись, заведённая раньше по ошибке, проигрывает заполненной);
    /// 2. единственная запись этого сервера в базе (базу выбрали в мастере по GUID ящика);
    /// 3. новая запись (`account`).
    pub fn resolve_account(&self, host: &str, login: &str) -> Result<AccountId> {
        let key = login_key(login);
        let found = self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT a.account_id, a.login,
                        (SELECT value FROM sync_state s WHERE s.account_id = a.account_id AND s.key = ?2),
                        (SELECT COUNT(*) FROM folders f WHERE f.account_id = a.account_id),
                        a.login = ?3
                   FROM accounts a WHERE lower(a.host) = lower(?1)",
            )?;
            let rows: Vec<(i64, String, Option<String>, i64, bool)> = st.query_map(params![host, MAILBOX_EMAIL_KEY, login], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })?;
        let mut matching: Vec<&(i64, String, Option<String>, i64, bool)> = found.iter().filter(|(_, l, email, _, _)| login_key(l) == key || email.as_deref().is_some_and(|e| email_local_part(e) == key)).collect();
        // Больше папок — впереди; при равенстве — точное совпадение логина.
        matching.sort_by(|a, b| b.3.cmp(&a.3).then(b.4.cmp(&a.4)));
        if let Some((id, l, ..)) = matching.first() {
            if l != login {
                info!(account = id, "учётная запись в базе найдена по логину без домена");
            }
            return Ok(AccountId(*id));
        }
        if let [(id, ..)] = found.as_slice() {
            info!(account = id, "в базе одна учётная запись этого сервера: вход с другим логином идёт в неё");
            return Ok(AccountId(*id));
        }
        self.account(host, login)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aiowa-store-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Папка и `n` писем учётной записи (одно из них — удалённое).
    fn seed(store: &Store, account: AccountId, n: i64, last_synced_at: i64) {
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO folders(account_id, server_id, display_name, last_synced_at, updated_at) VALUES (?1, ?2, 'Входящие', ?3, 0)",
                    params![account.0, format!("F-{}", account.0), last_synced_at],
                )?;
                let folder_pk = c.last_insert_rowid();
                for i in 0..=n {
                    c.execute("INSERT INTO items(account_id, server_id, folder_pk, deleted, updated_at) VALUES (?1, ?2, ?3, ?4, 0)", params![account.0, format!("I-{}-{i}", account.0), folder_pk, i64::from(i == n)])?;
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn login_forms_of_one_mailbox_share_a_key() {
        for login in ["savvidi", "savvidi@ngenergo", "NGENERGO\\savvidi", " Savvidi@NGENERGO.RU ", "ngenergo\\SAVVIDI"] {
            assert_eq!(login_key(login), "savvidi", "{login}");
        }
        assert_ne!(login_key("savvidi2@ngenergo"), "savvidi");
        assert_eq!(email_local_part("S.Avvidi@ngenergo.ru"), "s.avvidi");
        assert_eq!(login_key(""), "");
    }

    #[test]
    fn identity_and_summary_are_read_without_changes() {
        let dir = tmp_dir("summary");
        let path = dir.join("mail.db");
        let store = Store::open(&path).unwrap();
        let acc = store.account("mail.x.ru", "ivanov@x").unwrap();
        assert_eq!(store.mailbox_identity(acc).unwrap(), MailboxIdentity::default(), "у новой базы личности нет");
        seed(&store, acc, 3, 1_000);
        store.set_mailbox_identity(acc, &MailboxIdentity { mailbox_guid: Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into()), email: Some("Ivanov@X.ru".into()) }).unwrap();
        // `None` не стирает записанное.
        store.set_mailbox_identity(acc, &MailboxIdentity { mailbox_guid: None, email: None }).unwrap();
        let id = store.mailbox_identity(acc).unwrap();
        assert_eq!(id.mailbox_guid.as_deref(), Some("0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9"));
        assert_eq!(id.email.as_deref(), Some("ivanov@x.ru"), "адрес в нижнем регистре");

        // Сводка рядом с открытой базой (журнал есть).
        let s = read_summary(&path).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!((s.accounts[0].host.as_str(), s.accounts[0].login.as_str()), ("mail.x.ru", "ivanov@x"));
        assert_eq!(s.items, 3, "удалённое письмо не считается");
        assert_eq!(s.last_sync_ms, Some(1_000), "без отметки окна — по папкам");
        assert_eq!(s.identity(), id);
        assert!(s.db_bytes > 0);

        // Отметка окна важнее времени папок.
        store.set_state(acc, LAST_SYNC_OK_KEY, Some("5000")).unwrap();
        assert_eq!(read_summary(&path).unwrap().last_sync_ms, Some(5_000));

        // Кэш вложений считается вместе с вложенными каталогами.
        let att = attachments_dir_for(&path);
        std::fs::create_dir_all(att.join("sub")).unwrap();
        std::fs::write(att.join("1-a.txt"), b"12345").unwrap();
        std::fs::write(att.join("sub").join("2-b.txt"), b"123").unwrap();
        assert_eq!(read_summary(&path).unwrap().cache_bytes, 8);
        // Размеры у открытой базы (строка состояния, настройки) — тем же подсчётом, что у сводки.
        assert_eq!(store.attachment_cache_bytes(), 8, "вложенный каталог кэша тоже считается");
        assert_eq!(store.disk_size_bytes(), db_files_bytes(&path));

        // Закрытая база: журналов нет, чтение их не создаёт и файл не меняет.
        drop(store);
        assert!(!sidecar(&path, "-wal").exists(), "SQLite убирает журнал при закрытии");
        let before = std::fs::read(&path).unwrap();
        let s = read_summary(&path).unwrap();
        assert_eq!(s.items, 3);
        assert_eq!(s.identity().email.as_deref(), Some("ivanov@x.ru"));
        assert!(!sidecar(&path, "-wal").exists() && !sidecar(&path, "-shm").exists(), "чтение не создаёт журналов");
        assert_eq!(std::fs::read(&path).unwrap(), before, "файл базы не изменился");

        assert!(read_summary(&dir.join("нет.db")).is_err());
        std::fs::write(dir.join("мусор.db"), b"not a database at all, just text").unwrap();
        assert!(read_summary(&dir.join("мусор.db")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_of_old_schema_without_tables() {
        let dir = tmp_dir("old");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE accounts(account_id INTEGER PRIMARY KEY, host TEXT, login TEXT, created_at INTEGER); INSERT INTO accounts VALUES (1, 'h', 'u', 0);").unwrap();
        drop(c);
        let s = read_summary(&path).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!((s.items, s.last_sync_ms), (0, None));
        assert_eq!(s.identity(), MailboxIdentity::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_is_resolved_across_login_forms() {
        let dir = tmp_dir("resolve");
        let store = Store::open(dir.join("mail.db")).unwrap();
        let old = store.account("mail.x.ru", "savvidi@ngenergo").unwrap();
        seed(&store, old, 2, 0);
        // Другая форма логина — та же запись, новой не заводится.
        assert_eq!(store.resolve_account("mail.x.ru", "savvidi").unwrap(), old);
        assert_eq!(store.resolve_account("MAIL.X.RU", "NGENERGO\\Savvidi").unwrap(), old);
        assert_eq!(store.resolve_account("mail.x.ru", "savvidi@ngenergo").unwrap(), old);
        // Пустая запись с тем же логином без домена (заведена раньше по ошибке) проигрывает заполненной.
        let empty = store.account("mail.x.ru", "savvidi").unwrap();
        assert_ne!(empty, old);
        assert_eq!(store.resolve_account("mail.x.ru", "savvidi").unwrap(), old);
        // Совсем другой логин при нескольких записях — новая запись.
        let other = store.resolve_account("mail.x.ru", "petrov").unwrap();
        assert!(other != old && other != empty);
        let _ = std::fs::remove_dir_all(&dir);

        // Единственная запись сервера: вход с непохожим логином (база выбрана по GUID) идёт в неё.
        let dir = tmp_dir("resolve-single");
        let store = Store::open(dir.join("mail.db")).unwrap();
        let only = store.account("mail.x.ru", "CORP\\i.ivanov").unwrap();
        assert_eq!(store.resolve_account("mail.x.ru", "ivanov@corp.ru").unwrap(), only);
        // По записанному адресу ящика: часть до @ совпадает с логином.
        let second = store.account("mail.x.ru", "CORP\\p.petrov").unwrap();
        store.set_mailbox_identity(second, &MailboxIdentity { mailbox_guid: None, email: Some("petr@x.ru".into()) }).unwrap();
        assert_eq!(store.resolve_account("mail.x.ru", "petr@corp.ru").unwrap(), second);
        // Другой сервер — своя запись.
        let elsewhere = store.resolve_account("mail.y.ru", "ivanov@corp.ru").unwrap();
        assert!(elsewhere != only && elsewhere != second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn databases_are_found_two_levels_down() {
        let root = tmp_dir("scan");
        for (dir, db) in [("mail.x.ru/ivanov_x.ru", true), ("mail.x.ru/empty", false), ("mail.y.ru/petrov", true), ("mail.y.ru/old.deleting-1", true)] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            if db {
                std::fs::write(d.join("mail.db"), b"").unwrap();
            }
        }
        std::fs::write(root.join("mail.x.ru").join("mail.db"), b"").unwrap();
        // Остаток удаления без файла базы (удалились только журналы) и ящик, в имени которого
        // метка есть, но без отметки времени, — не остаток, а обычный ящик.
        std::fs::create_dir_all(root.join("mail.x.ru").join("ivanov_x.ru.deleting-1761390000000").join("attachments")).unwrap();
        std::fs::create_dir_all(root.join("mail.x.ru").join("a.deleting-b")).unwrap();
        std::fs::write(root.join("mail.x.ru").join("a.deleting-b").join("mail.db"), b"").unwrap();
        let found = scan_databases(&root);
        assert_eq!(found, vec![root.join("mail.x.ru").join("a.deleting-b").join("mail.db"), root.join("mail.x.ru").join("ivanov_x.ru").join("mail.db"), root.join("mail.y.ru").join("petrov").join("mail.db")]);
        assert!(scan_databases(&root.join("нет")).is_empty());
        assert_eq!(scan_leftovers(&root), vec![root.join("mail.x.ru").join("ivanov_x.ru.deleting-1761390000000"), root.join("mail.y.ru").join("old.deleting-1")], "остатки удаления видны отдельно");
        assert!(!is_deleting_dir(Path::new(".deleting-1")) && !is_deleting_dir(Path::new("x.deleting-")) && !is_deleting_dir(Path::new("x.deleting-1a")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uri_escapes_special_characters() {
        assert_eq!(file_uri(Path::new(r"D:\Данные\a#b?c%d\mail.db"), true), "file:///D:/Данные/a%23b%3Fc%25d/mail.db?mode=ro&immutable=1");
        assert_eq!(file_uri(Path::new("/tmp/mail.db"), false), "file:///tmp/mail.db?mode=ro");
    }
}
