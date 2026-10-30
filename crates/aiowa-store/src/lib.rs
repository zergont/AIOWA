//! Локальное хранилище AIOWA: один файл SQLite (`mail.db`), WAL, FTS5, кэш вложений рядом.
//!
//! Правила: сервер источник истины для почты, локально уникальны только индексы и
//! состояние синхронизации. Удаления сервера зеркалятся флагом `deleted`.
//! Тела писем хранятся сжатыми (zstd), для поиска хранится текстовая выжимка.
//! Беседы (`conversations`, `conversation_folders`) пересчитываются в той же транзакции,
//! что меняет письма. Файлы вложений лежат в каталоге `attachments` рядом с базой и
//! вытесняются по квоте; извлечённый текст остаётся в базе и после вытеснения.
//! Календарь кэшируется окнами (`calendar`): окно перечитывается с сервера целиком.

pub mod calendar;
pub mod directory;
pub mod drafts;
pub mod identity;
pub mod query;
pub mod schema;
pub mod text;
pub mod ui;
pub mod view_state;

use aiowa_core::{AccountId, Folder, FolderChange, FolderId, ItemChange, ItemId, MessageFull, MessageMeta, RecipientKind, SyncState};
use jiff::Timestamp;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

pub use calendar::{reminder_state_key, CachedTimeZone, CalendarRow, ReminderMark, ReminderRow, ReminderState, OWA_TIME_ZONE_KEY, OWA_TIME_ZONE_TTL_MS};
pub use directory::ContactLocalStats;
pub use drafts::LocalDraft;
pub use identity::{login_key, read_summary, DbAccount, DbSummary, MailboxIdentity};
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
    /// Растёт при каждой записи писем с сервера: по нему кэш подсказок получателей
    /// понимает, что устарел.
    write_generation: AtomicU64,
    recipient_cache: Mutex<Option<RecipientCache>>,
}

/// Подсказка получателя для автодополнения в композере.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientSuggestion {
    pub name: Option<String>,
    pub email: String,
    /// Частота адреса с преимуществом свежих писем: письмо за последние 30 дней даёт 4,
    /// за год — 2, старше — 1.
    pub weight: i64,
}

/// Все известные адреса аккаунта с весами, в порядке убывания веса. Строится одним проходом по
/// `items` и `recipients` (около 0,3 с на 150 тыс. писем), поэтому кэшируется в памяти и
/// перестраивается после записей в базу, но не чаще раза в несколько секунд.
struct RecipientCache {
    account: AccountId,
    built_at: Instant,
    generation: u64,
    rows: Vec<CachedRecipient>,
}

struct CachedRecipient {
    suggestion: RecipientSuggestion,
    /// Адрес в нижнем регистре.
    email_norm: String,
    /// Имя после `norm_lower` (нижний регистр, ё→е).
    name_norm: String,
}

/// Кэш подсказок перестраивается после записи не раньше, чем через столько времени после
/// прошлой сборки (во время синхронизации записи идут каждые несколько секунд).
const RECIPIENT_CACHE_MIN_AGE: Duration = Duration::from_secs(5);
/// И в любом случае не реже раза в полчаса: веса зависят от текущей даты.
const RECIPIENT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 60);

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
    /// Начало текста письма (серверный `Preview`): строка результата показывает его, когда
    /// фрагмента нет (тело ещё не загружено, совпадение только в теме или отправителе).
    pub preview: Option<String>,
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
    /// Файл базы с журналами `-wal` и `-shm` (`identity::db_files_bytes`).
    pub db_bytes: u64,
    pub items: i64,
    pub bodies: i64,
    pub conversations: i64,
    pub attachments: i64,
    pub attachments_fetched: i64,
    pub attachments_with_text: i64,
    /// Каталог кэша вложений на диске целиком (`identity::dir_bytes`), а не сумма размеров по
    /// строкам базы: кэш общий для всех учётных записей базы.
    pub attachment_cache_bytes: u64,
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
/// `is_read` и `is_flagged` — состояние до операции записи, чтобы откатить её при отказе сервера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRef {
    pub item_pk: i64,
    pub folder_pk: i64,
    pub id: ItemId,
    /// 0 тела нет, 1 есть, 2 загрузка не удалась.
    pub body_state: i64,
    pub is_read: bool,
    pub is_flagged: bool,
    /// Беседа письма (для события окну после операции записи).
    pub conversation_pk: Option<i64>,
}

/// Колонки `items` в порядке, который разбирает `item_ref_row`.
const ITEM_REF_COLUMNS: &str = "server_id, change_key, folder_pk, body_state, is_read, flag_status, conversation_pk";

fn item_ref_row(item_pk: i64, r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRef> {
    Ok(ItemRef {
        item_pk,
        folder_pk: r.get(2)?,
        id: ItemId { id: r.get(0)?, change_key: r.get(1)? },
        body_state: r.get(3)?,
        is_read: r.get::<_, i64>(4)? != 0,
        is_flagged: r.get::<_, Option<String>>(5)?.as_deref() == Some(FLAGGED),
        conversation_pk: r.get(6)?,
    })
}

/// Значение `items.flag_status` для поставленного флага (как присылает сервер).
const FLAGGED: &str = "Flagged";
/// Значение `items.flag_status` без флага.
const NOT_FLAGGED: &str = "NotFlagged";

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
    /// Есть ли в беседе (во всех папках) письмо с флагом «к исполнению».
    pub has_flagged: bool,
    /// Начало текста самого свежего письма беседы в этой папке (серверный `Preview`), как в
    /// третьей строке списка OWA; `None` — у письма его нет.
    pub preview: Option<String>,
}

/// Подзапрос «в беседе есть письмо с флагом» для списков бесед (`c` — `conversations`).
const HAS_FLAGGED_SQL: &str = "EXISTS(SELECT 1 FROM items fi WHERE fi.conversation_pk = c.conversation_pk AND fi.deleted = 0 AND fi.flag_status = 'Flagged')";

/// Итог локального удаления папки (`Store::delete_folder_local`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FolderDeleteStats {
    /// Папок в поддереве вместе с самой папкой.
    pub folders: usize,
    /// Писем, помеченных удалёнными (при переносе в «Удалённые» — 0: письма остаются в папке).
    pub items: usize,
    /// Беседы, чьи письма помечены удалёнными (по возрастанию): окно перечитает открытую из них.
    pub conversation_pks: Vec<i64>,
}

/// Итог вытеснения кэша вложений.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvictReport {
    pub files_removed: usize,
    pub bytes_freed: u64,
    pub bytes_left: u64,
}

/// Служебные папки, которые не синхронизируются при выборе «все почтовые»; подпапки тоже.
/// «Черновики» (`drafts`) сюда не входят: они синхронизируются по умолчанию, чтобы черновики (в том
/// числе созданные в OWA) были в списке и открывались в композере.
pub const SERVICE_FOLDERS: &[&str] = &["junkemail", "deleteditems", "outbox", "conversationhistory", "syncissues", "conflicts", "localfailures", "serverfailures"];

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

/// Имя каталога из адреса сервера или логина: всё, кроме букв, цифр, `.`, `-` и `_`, заменяется
/// на `_` (двоеточие порта, скобки IPv6, `\` и `@` логина в имени каталога Windows недопустимы
/// или нежелательны): `mail.x.ru:8443` → `mail.x.ru_8443`. Обычное имя сервера не меняется.
/// Пустая строка и строка из одних точек (`.`, `..`) — тоже `_`: путь не должен уходить выше.
pub fn safe_path_component(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    if out.chars().all(|c| c == '.') { "_".repeat(out.len().max(1)) } else { out }
}

/// Путь к базе учётной записи под каталогом `base`: `<base>\AIOWA\<host>\<user>\mail.db`.
pub fn db_path_under(base: &Path, host: &str, user: &str) -> PathBuf {
    base.join("AIOWA").join(safe_path_component(host)).join(safe_path_component(user)).join("mail.db")
}

/// Путь к базе по умолчанию: `%LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db`. Один для CLI и окна,
/// чтобы оба открывали одну и ту же базу.
pub fn default_db_path(host: &str, user: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    db_path_under(&base, host, user)
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
        // Каталог кэша — тот же, что считают вкладка «Данные» и сводка чужих баз.
        let attachments_dir = identity::attachments_dir_for(&path);
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
        Ok(Self { path, attachments_dir, conn: Mutex::new(conn), write_generation: AtomicU64::new(0), recipient_cache: Mutex::new(None) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Каталог кэша вложений рядом с базой.
    pub fn attachments_dir(&self) -> &Path {
        &self.attachments_dir
    }

    /// Место на диске под кэш вложений, байт: каталог кэша со вложенными (нет каталога — 0).
    /// Тот же подсчёт, что у вкладки «Данные» (`identity::dir_bytes`): цифры в окне не расходятся.
    pub fn attachment_cache_bytes(&self) -> u64 {
        identity::dir_bytes(&self.attachments_dir)
    }

    /// Размер базы на диске, байт: файл базы и журналы `-wal` и `-shm` рядом с ним
    /// (`identity::db_files_bytes`, как у вкладки «Данные»).
    pub fn disk_size_bytes(&self) -> u64 {
        identity::db_files_bytes(&self.path)
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
    /// (нежелательная, удалённые, исходящие, журнал бесед, ошибки синхронизации) и их подпапок;
    /// «Черновики» синхронизируются как обычная папка (`SERVICE_FOLDERS`). Служебная папка, которую
    /// пользователь уже синхронизировал отдельно (есть состояние синхронизации), остаётся в наборе:
    /// раз попросили, дальше обновляем сами.
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

    /// Идентификатор не удалённой папки на сервере (с `ChangeKey`, если известен).
    pub fn folder_id_by_pk(&self, account: AccountId, folder_pk: i64) -> Result<Option<FolderId>> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT server_id, change_key FROM folders WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0", params![account.0, folder_pk], |r| Ok(FolderId { id: r.get(0)?, change_key: r.get(1)? }))
                .optional()?)
        })
    }

    /// Идентификатор корня почтовых папок (`msgfolderroot`) по локальной базе: сам корень
    /// `SyncFolderHierarchy` не присылает, но он — родитель «Входящих» и прочих папок верхнего
    /// уровня. `None`, пока иерархия не синхронизирована.
    pub fn msgfolderroot_server_id(&self, account: AccountId) -> Result<Option<String>> {
        self.with_conn(|c| {
            let by_inbox: Option<String> = c
                .query_row("SELECT parent_server_id FROM folders WHERE account_id = ?1 AND distinguished = 'inbox' AND deleted = 0", params![account.0], |r| r.get(0))
                .optional()?
                .flatten();
            if by_inbox.is_some() {
                return Ok(by_inbox);
            }
            // Нет «Входящих»: самый частый родитель, которого нет среди известных папок.
            Ok(c.query_row(
                "SELECT f.parent_server_id FROM folders f
                 WHERE f.account_id = ?1 AND f.deleted = 0 AND f.parent_server_id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM folders p WHERE p.account_id = f.account_id AND p.server_id = f.parent_server_id)
                 GROUP BY f.parent_server_id ORDER BY COUNT(*) DESC LIMIT 1",
                params![account.0],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
    }

    // --- локальные операции с папками ------------------------------------------
    //
    // Зеркало серверных CreateFolder/UpdateFolder/DeleteFolder: окно меняет дерево сразу после
    // ответа сервера, не дожидаясь синхронизации иерархии (она потом сверит имена, счётчики и
    // родителей). Агрегаты бесед по папкам пересчитываются только при удалении писем.

    /// Вставляет созданную на сервере папку: `folder.id` — из ответа `CreateFolder`, `parent_id` —
    /// родитель (для корня — `msgfolderroot_server_id`), класс `IPF.Note`, счётчики нули. Если
    /// синхронизация иерархии уже успела создать строку, она обновляется. Возвращает `folder_pk`.
    pub fn insert_folder(&self, account: AccountId, folder: &Folder) -> Result<i64> {
        self.with_tx(|tx| {
            upsert_folder(tx, account, folder, now_ms())?;
            Ok(tx.query_row("SELECT folder_pk FROM folders WHERE account_id = ?1 AND server_id = ?2", params![account.0, folder.id.id], |r| r.get(0))?)
        })
    }

    /// Переименовывает папку локально; `new_id` — идентификатор из ответа `UpdateFolder` (новый
    /// `ChangeKey`), если сервер его вернул.
    pub fn rename_folder_local(&self, folder_pk: i64, display_name: &str, new_id: Option<&FolderId>) -> Result<()> {
        self.with_conn(|c| {
            let now = now_ms();
            let n = match new_id {
                Some(id) => c.execute("UPDATE folders SET display_name = ?2, server_id = ?3, change_key = COALESCE(?4, change_key), updated_at = ?5 WHERE folder_pk = ?1", params![folder_pk, display_name, id.id, id.change_key, now])?,
                None => c.execute("UPDATE folders SET display_name = ?2, updated_at = ?3 WHERE folder_pk = ?1", params![folder_pk, display_name, now])?,
            };
            if n == 0 {
                return Err(StoreError::Other(format!("папка #{folder_pk} не найдена")));
            }
            Ok(())
        })
    }

    /// Удаляет папку локально вместе с подпапками. `to_deleted = Some(pk «Удалённых»)` — сервер
    /// перенёс папку в «Удалённые» (`DeleteFolder MoveToDeletedItems`): папка с подпапками и
    /// письмами остаётся, меняется только её родитель. `None` — удалена навсегда (`HardDelete`):
    /// папки поддерева получают `deleted = 1`, их письма тоже (снимаются с индекса, агрегаты бесед
    /// пересчитываются). Служебную папку (`distinguished`) удалить нельзя.
    pub fn delete_folder_local(&self, folder_pk: i64, to_deleted: Option<i64>) -> Result<FolderDeleteStats> {
        self.write_generation.fetch_add(1, Ordering::Relaxed);
        self.with_tx(|tx| {
            let (account_id, distinguished): (i64, Option<String>) = tx
                .query_row("SELECT account_id, distinguished FROM folders WHERE folder_pk = ?1 AND deleted = 0", params![folder_pk], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| StoreError::Other(format!("папка #{folder_pk} не найдена")))?;
            if let Some(d) = distinguished {
                return Err(StoreError::Other(format!("служебную папку ({d}) удалить нельзя")));
            }
            let subtree = folder_subtree(tx, account_id, folder_pk)?;
            let now = now_ms();
            match to_deleted {
                Some(trash_pk) => {
                    if subtree.contains(&trash_pk) {
                        return Err(StoreError::Other("папку нельзя перенести в её же подпапку".into()));
                    }
                    let trash_id: String = tx
                        .query_row("SELECT server_id FROM folders WHERE account_id = ?1 AND folder_pk = ?2 AND deleted = 0", params![account_id, trash_pk], |r| r.get(0))
                        .optional()?
                        .ok_or_else(|| StoreError::Other(format!("папка «Удалённые» #{trash_pk} не найдена")))?;
                    tx.execute("UPDATE folders SET parent_server_id = ?2, updated_at = ?3 WHERE folder_pk = ?1", params![folder_pk, trash_id, now])?;
                    Ok(FolderDeleteStats { folders: subtree.len(), items: 0, conversation_pks: Vec::new() })
                }
                None => {
                    let mut touched = Touched::default();
                    let mut items = 0usize;
                    for &pk in &subtree {
                        let rows: Vec<(i64, Option<i64>)> = tx
                            .prepare_cached("SELECT item_pk, conversation_pk FROM items WHERE folder_pk = ?1 AND deleted = 0")?
                            .query_map(params![pk], |r| Ok((r.get(0)?, r.get(1)?)))?
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        for (item_pk, conv) in rows {
                            fts_delete(tx, item_pk)?;
                            touched.add(conv, pk);
                            tx.prepare_cached("UPDATE items SET deleted = 1, updated_at = ?2 WHERE item_pk = ?1")?.execute(params![item_pk, now])?;
                            items += 1;
                        }
                        tx.prepare_cached("UPDATE folders SET deleted = 1, total_count = 0, unread_count = 0, updated_at = ?2 WHERE folder_pk = ?1")?.execute(params![pk, now])?;
                    }
                    let conversation_pks = touched.recompute(tx)?;
                    Ok(FolderDeleteStats { folders: subtree.len(), items, conversation_pks })
                }
            }
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
        self.write_generation.fetch_add(1, Ordering::Relaxed);
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
        self.write_generation.fetch_add(1, Ordering::Relaxed);
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
        self.write_generation.fetch_add(1, Ordering::Relaxed);
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

                // Reply-To лежит в той же таблице с kind = 4 (`RecipientKind::ReplyTo`): отдельной
                // колонки и миграции не нужно, представление индекса берёт только kind 1 и 2.
                tx.execute("DELETE FROM recipients WHERE item_pk = ?1", params![item_pk])?;
                let mut rec = tx.prepare_cached("INSERT INTO recipients(item_pk, kind, name, email, position) VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for (kind, list) in [(RecipientKind::To, &m.to), (RecipientKind::Cc, &m.cc), (RecipientKind::Bcc, &m.bcc), (RecipientKind::ReplyTo, &m.reply_to)] {
                    for (i, p) in list.iter().enumerate() {
                        rec.execute(params![item_pk, kind.as_i64(), p.name, p.email, i as i64])?;
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
            debug!(attachment_pk = todo.attachment_pk, error = %e, "текст вложения не извлечён");
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

    /// Вложение по ключу, только если письмо принадлежит учётной записи `account`: для протокола
    /// предпросмотра, который не должен отдавать чужие и несуществующие ключи.
    pub fn attachment_of_account(&self, account: AccountId, attachment_pk: i64) -> Result<Option<AttachmentRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachments WHERE attachment_pk = ?1 AND item_pk IN (SELECT item_pk FROM items WHERE account_id = ?2)"))?;
            Ok(st.query_row(params![attachment_pk, account.0], attachment_row).optional()?)
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

    /// Ключ письма по серверному идентификатору (и удалённого тоже) вместе с признаком `deleted`:
    /// для сверки после записи на сервер (черновик после `CreateItem`/`DeleteItem`).
    pub fn item_pk_by_server_id(&self, account: AccountId, server_id: &str) -> Result<Option<(i64, bool)>> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT item_pk, deleted FROM items WHERE account_id = ?1 AND server_id = ?2", params![account.0, server_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)))
                .optional()?)
        })
    }

    /// Ссылка на не удалённое письмо для запроса к серверу.
    pub fn item_ref(&self, item_pk: i64) -> Result<Option<ItemRef>> {
        self.with_conn(|c| Ok(c.query_row(&format!("SELECT {ITEM_REF_COLUMNS} FROM items WHERE item_pk = ?1 AND deleted = 0"), params![item_pk], |r| item_ref_row(item_pk, r)).optional()?))
    }

    /// Ссылки на не удалённые письма пакетно, в порядке `item_pks`; неизвестные и удалённые
    /// пропускаются (вызывающий сверяет длину, если это важно).
    pub fn item_refs(&self, item_pks: &[i64]) -> Result<Vec<ItemRef>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ITEM_REF_COLUMNS} FROM items WHERE item_pk = ?1 AND deleted = 0"))?;
            let mut out = Vec::with_capacity(item_pks.len());
            for &pk in item_pks {
                if let Some(r) = st.query_row(params![pk], |r| item_ref_row(pk, r)).optional()? {
                    out.push(r);
                }
            }
            Ok(out)
        })
    }

    /// Письма беседы (не удалённые), при `folder_pk` — только лежащие в этой папке;
    /// порядок как в области чтения: новые первыми.
    pub fn conversation_item_pks(&self, account: AccountId, conversation_pk: i64, folder_pk: Option<i64>) -> Result<Vec<i64>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT item_pk FROM items WHERE conversation_pk = ?1 AND account_id = ?2 AND deleted = 0 AND (?3 IS NULL OR folder_pk = ?3)
                 ORDER BY COALESCE(received_at, sent_at) DESC, item_pk DESC",
            )?;
            Ok(st.query_map(params![conversation_pk, account.0, folder_pk], |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    // --- локальное применение операций записи -------------------------------
    //
    // Окно меняет базу сразу (оптимистично), затем идёт на сервер и при отказе откатывает
    // обратным вызовом той же функции. Агрегаты бесед и папок пересчитываются здесь же,
    // в той же транзакции, как в `apply_item_batch`; индекс трогает только удаление.

    /// Помечает письма прочитанными или непрочитанными; уже находящиеся в нужном состоянии не
    /// меняются. Возвращает беседы затронутых писем (без повторов).
    pub fn set_read_local(&self, item_pks: &[i64], read: bool) -> Result<Vec<i64>> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut touched = Touched::default();
            let mut convs: BTreeSet<i64> = BTreeSet::new();
            let mut folder_unread: HashMap<i64, i64> = HashMap::new();
            for &pk in item_pks {
                let Some(st) = item_pos(tx, pk)? else { continue };
                if st.deleted {
                    continue;
                }
                if st.is_read != read {
                    tx.prepare_cached("UPDATE items SET is_read = ?2, updated_at = ?3 WHERE item_pk = ?1")?.execute(params![pk, read as i64, now])?;
                    *folder_unread.entry(st.folder_pk).or_default() += if read { -1 } else { 1 };
                    touched.add(st.conversation_pk, st.folder_pk);
                }
                convs.extend(st.conversation_pk);
            }
            touched.recompute(tx)?;
            for (f, d) in folder_unread {
                adjust_folder_counts(tx, f, 0, d, now)?;
            }
            Ok(convs.into_iter().collect())
        })
    }

    /// Ставит или снимает флаг «к исполнению». Флаг в агрегаты и индекс не входит.
    /// Возвращает беседы затронутых писем.
    pub fn set_flag_local(&self, item_pks: &[i64], flagged: bool) -> Result<Vec<i64>> {
        self.with_tx(|tx| {
            let now = now_ms();
            let status = if flagged { FLAGGED } else { NOT_FLAGGED };
            let mut convs: BTreeSet<i64> = BTreeSet::new();
            for &pk in item_pks {
                let Some(st) = item_pos(tx, pk)? else { continue };
                if st.deleted {
                    continue;
                }
                tx.prepare_cached("UPDATE items SET flag_status = ?2, updated_at = ?3 WHERE item_pk = ?1")?.execute(params![pk, status, now])?;
                convs.extend(st.conversation_pk);
            }
            Ok(convs.into_iter().collect())
        })
    }

    /// Переносит письмо в папку `folder_pk` и, если сервер уже выдал новый идентификатор,
    /// записывает его. Пересчитывает агрегаты беседы в обеих папках и счётчики папок.
    /// Если строка с новым идентификатором уже есть (параллельная синхронизация успела создать
    /// письмо в целевой папке), текущая строка помечается удалённой, а та получает папку.
    pub fn move_local(&self, item_pk: i64, folder_pk: i64, new_id: Option<&ItemId>) -> Result<()> {
        self.with_tx(|tx| {
            let Some(st) = item_pos(tx, item_pk)? else { return Err(StoreError::Other(format!("письмо #{item_pk} не найдено"))) };
            if st.deleted {
                return Err(StoreError::Other(format!("письмо #{item_pk} удалено")));
            }
            let now = now_ms();
            let mut touched = Touched::default();
            touched.add(st.conversation_pk, st.folder_pk);
            touched.add(st.conversation_pk, folder_pk);
            let twin: Option<i64> = match new_id {
                Some(id) if id.id != st.server_id => tx
                    .prepare_cached("SELECT item_pk FROM items WHERE account_id = ?1 AND server_id = ?2 AND item_pk <> ?3")?
                    .query_row(params![st.account_id, id.id, item_pk], |r| r.get(0))
                    .optional()?,
                _ => None,
            };
            match (new_id, twin) {
                (Some(id), Some(twin_pk)) => {
                    fts_delete(tx, item_pk)?;
                    tx.prepare_cached("UPDATE items SET deleted = 1, updated_at = ?2 WHERE item_pk = ?1")?.execute(params![item_pk, now])?;
                    let twin_pos = item_pos(tx, twin_pk)?;
                    tx.prepare_cached("UPDATE items SET folder_pk = ?2, change_key = ?3, deleted = 0, updated_at = ?4 WHERE item_pk = ?1")?.execute(params![twin_pk, folder_pk, id.change_key, now])?;
                    if let Some(tp) = twin_pos {
                        touched.add(tp.conversation_pk, tp.folder_pk);
                        if tp.deleted {
                            fts_insert(tx, twin_pk)?;
                        }
                    }
                }
                (Some(id), None) => {
                    tx.prepare_cached("UPDATE items SET folder_pk = ?2, server_id = ?3, change_key = ?4, updated_at = ?5 WHERE item_pk = ?1")?.execute(params![item_pk, folder_pk, id.id, id.change_key, now])?;
                }
                (None, _) => {
                    tx.prepare_cached("UPDATE items SET folder_pk = ?2, updated_at = ?3 WHERE item_pk = ?1")?.execute(params![item_pk, folder_pk, now])?;
                }
            }
            touched.recompute(tx)?;
            if st.folder_pk != folder_pk {
                let unread = if st.is_read { 0 } else { 1 };
                adjust_folder_counts(tx, st.folder_pk, -1, -unread, now)?;
                adjust_folder_counts(tx, folder_pk, 1, unread, now)?;
            }
            Ok(())
        })
    }

    /// Помечает письма удалёнными (жёсткое удаление или письмо уже лежало в «Удалённых»):
    /// снимает с индекса, пересчитывает агрегаты и счётчики папок. Возвращает беседы.
    pub fn mark_deleted_local(&self, item_pks: &[i64]) -> Result<Vec<i64>> {
        self.set_deleted_local(item_pks, true)
    }

    /// Обратная операция к `mark_deleted_local` для отката при отказе сервера: возвращает письма
    /// в список и индекс. Возвращает беседы.
    pub fn restore_deleted_local(&self, item_pks: &[i64]) -> Result<Vec<i64>> {
        self.set_deleted_local(item_pks, false)
    }

    fn set_deleted_local(&self, item_pks: &[i64], deleted: bool) -> Result<Vec<i64>> {
        self.with_tx(|tx| {
            let now = now_ms();
            let mut touched = Touched::default();
            let mut convs: BTreeSet<i64> = BTreeSet::new();
            let mut folder_delta: HashMap<i64, (i64, i64)> = HashMap::new();
            for &pk in item_pks {
                let Some(st) = item_pos(tx, pk)? else { continue };
                if st.deleted == deleted {
                    convs.extend(st.conversation_pk);
                    continue;
                }
                if deleted {
                    fts_delete(tx, pk)?;
                }
                tx.prepare_cached("UPDATE items SET deleted = ?2, updated_at = ?3 WHERE item_pk = ?1")?.execute(params![pk, deleted as i64, now])?;
                if !deleted {
                    fts_insert(tx, pk)?;
                }
                let sign = if deleted { -1 } else { 1 };
                let d = folder_delta.entry(st.folder_pk).or_default();
                d.0 += sign;
                d.1 += if st.is_read { 0 } else { sign };
                touched.add(st.conversation_pk, st.folder_pk);
                convs.extend(st.conversation_pk);
            }
            touched.recompute(tx)?;
            for (f, (dt, du)) in folder_delta {
                adjust_folder_counts(tx, f, dt, du, now)?;
            }
            Ok(convs.into_iter().collect())
        })
    }

    /// Записывает идентификатор письма после операции на сервере (новый `ChangeKey` после
    /// `GetItem`/`UpdateItem`). Удалённые строки тоже обновляются: это нужно откату.
    ///
    /// Полный ключ усечённым не заменяется: ответ `UpdateItem` отдаёт 8-символьный ключ без
    /// номера версии, и с ним ответ на письмо падает с `ErrorStaleObject` (docs/obkatka.md п. 19).
    /// При том же `server_id` ключ короче сохранённого (или отсутствующий) оставляет прежний.
    pub fn update_item_id(&self, item_pk: i64, id: &ItemId) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE items SET
                   change_key = CASE
                     WHEN ?3 IS NULL THEN change_key
                     WHEN server_id = ?2 AND change_key IS NOT NULL AND length(?3) < length(change_key) THEN change_key
                     ELSE ?3 END,
                   server_id = ?2,
                   updated_at = ?4
                 WHERE item_pk = ?1",
                params![item_pk, id.id, id.change_key, now_ms()],
            )?;
            if n == 0 {
                return Err(StoreError::Other(format!("письмо #{item_pk} не найдено")));
            }
            Ok(())
        })
    }

    /// Письма с усечённым ключом изменений (короче `min_len` символов): их ключи нужно
    /// перечитать с сервера, иначе ответ и пересылка на них отказывают (`ErrorStaleObject`).
    pub fn items_with_short_change_keys(&self, account: AccountId, min_len: usize, limit: usize) -> Result<Vec<(i64, ItemId)>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT item_pk, server_id, change_key FROM items WHERE account_id = ?1 AND deleted = 0 AND change_key IS NOT NULL AND length(change_key) < ?2 ORDER BY item_pk LIMIT ?3")?;
            let rows = st.query_map(params![account.0, min_len as i64, limit as i64], |r| Ok((r.get::<_, i64>(0)?, ItemId { id: r.get(1)?, change_key: r.get(2)? })))?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
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
                    warn!(attachment_pk = *pk, error = %e, "файл кэша не удалён");
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
                        snippet(items_fts, 4, ?1, ?2, '…', 14), snippet(items_fts, 5, ?1, ?2, '…', 14), i.conversation_pk, i.preview
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
                        COALESCE(i.preview, ''), '', i.conversation_pk, i.preview
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
                    r.get::<_, Option<String>>(10)?,
                ))
            })?;
            let raw = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            let mut hits = Vec::with_capacity(raw.len());
            for (item_pk, rank, subject, from_name, from_email, received_at, folder_name, body_snip, att_snip, conversation_pk, preview) in raw {
                let body_hit = body_snip.contains(MARK_OPEN);
                let att_hit = att_snip.contains(MARK_OPEN);
                let (snippet, attachment_name) = if att_hit && !body_hit {
                    let words = marked_words(&att_snip, MARK_OPEN, MARK_CLOSE);
                    (att_snip, attachment_name_for(c, item_pk, &words)?)
                } else {
                    (body_snip, None)
                };
                let snippet = snippet.replace(MARK_OPEN, "[").replace(MARK_CLOSE, "]");
                let preview = preview.filter(|p| !p.trim().is_empty());
                hits.push(SearchHit { item_pk, rank, subject, from_name, from_email, received_at: ms_ts(received_at), folder_name, snippet, attachment_name, conversation_pk, preview });
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
            let mut st = c.prepare_cached(&format!(
                "SELECT c.conversation_pk, c.topic, cf.item_count, cf.unread_count, c.item_count, cf.last_received_at, c.has_attachments, {HAS_FLAGGED_SQL}
                 FROM conversation_folders cf JOIN conversations c ON c.conversation_pk = cf.conversation_pk
                 WHERE cf.folder_pk = ?1 AND c.account_id = ?2
                 ORDER BY cf.last_received_at DESC, c.conversation_pk DESC LIMIT ?3"
            ))?;
            let mut rows: Vec<ConversationRow> = st
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
                        has_flagged: r.get::<_, i64>(7)? != 0,
                        preview: None,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ui::fill_previews(c, folder_pk, &mut rows)?;
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

    /// Счётчики учётной записи. Размеры на диске — как у вкладки «Данные» и строки состояния:
    /// база с `-wal` и `-shm` (при любом имени файла базы) и каталог кэша вложений целиком.
    pub fn stats(&self, account: AccountId) -> Result<StoreStats> {
        let db_bytes = self.disk_size_bytes();
        let attachment_cache_bytes = self.attachment_cache_bytes();
        self.with_conn(|c| {
            let items: i64 = c.query_row("SELECT COUNT(*) FROM items WHERE account_id = ?1 AND deleted = 0", params![account.0], |r| r.get(0))?;
            let bodies: i64 = c.query_row("SELECT COUNT(*) FROM bodies b JOIN items i ON i.item_pk = b.item_pk WHERE i.account_id = ?1 AND i.deleted = 0", params![account.0], |r| r.get(0))?;
            let conversations: i64 = c.query_row("SELECT COUNT(*) FROM conversations WHERE account_id = ?1 AND item_count > 0", params![account.0], |r| r.get(0))?;
            let (attachments, attachments_fetched, attachments_with_text): (i64, i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(a.fetched_at IS NOT NULL), 0), COALESCE(SUM(a.text_state = 1), 0)
                 FROM attachments a JOIN items i ON i.item_pk = a.item_pk WHERE i.account_id = ?1 AND i.deleted = 0 AND a.is_inline = 0",
                params![account.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
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

    // --- адреса для композера --------------------------------------------------

    /// Подсказки получателей по префиксу адреса или слова имени (без учёта регистра и ё),
    /// по убыванию веса. Пустой префикс — самые частые адреса. Источник: получатели
    /// загруженных писем и отправители всех писем аккаунта; адреса без `@` (устаревшие EX)
    /// не предлагаются, один адрес в разном регистре — одна подсказка.
    pub fn suggest_recipients(&self, account: AccountId, prefix: &str, limit: usize) -> Result<Vec<RecipientSuggestion>> {
        let needle = text::norm_lower(prefix.trim());
        let mut cache = self.recipient_cache.lock().map_err(|_| StoreError::Other("мьютекс кэша адресов отравлен".into()))?;
        let generation = self.write_generation.load(Ordering::Relaxed);
        let stale = match &*cache {
            None => true,
            Some(c) => c.account != account || c.built_at.elapsed() > RECIPIENT_CACHE_MAX_AGE || (c.generation != generation && c.built_at.elapsed() > RECIPIENT_CACHE_MIN_AGE),
        };
        if stale {
            *cache = Some(self.build_recipient_cache(account, generation)?);
        }
        let rows = &cache.as_ref().expect("кэш только что построен").rows;
        Ok(rows.iter().filter(|r| needle.is_empty() || word_prefix_match(&r.email_norm, &needle) || word_prefix_match(&r.name_norm, &needle)).take(limit).map(|r| r.suggestion.clone()).collect())
    }

    /// Сбрасывает кэш подсказок получателей: следующий вызов `suggest_recipients` перестроит его.
    pub fn invalidate_recipient_cache(&self) {
        if let Ok(mut c) = self.recipient_cache.lock() {
            *c = None;
        }
    }

    fn build_recipient_cache(&self, account: AccountId, generation: u64) -> Result<RecipientCache> {
        let started = Instant::now();
        let now = now_ms();
        let day = 24 * 3600 * 1000i64;
        // `+account_id` отключает индекс по account_id: с ним планировщик читает строки
        // вразброс (1,8 с на 150 тыс. писем), последовательный проход по таблице втрое быстрее.
        // Имя берётся от самого свежего письма, где оно есть: MAX по строке «дата|имя».
        let rows = self.with_conn(|c| {
            let mut st = c.prepare(
                "SELECT lower(email) AS e,
                        MAX(CASE WHEN name IS NOT NULL AND name != '' THEN printf('%020d', COALESCE(at, 0)) || '|' || name END) AS named,
                        MAX(COALESCE(at, 0)) AS last_at,
                        SUM(CASE WHEN at >= ?2 THEN 4 WHEN at >= ?3 THEN 2 ELSE 1 END) AS weight
                   FROM (SELECT r.email AS email, r.name AS name, COALESCE(i.received_at, i.sent_at) AS at
                           FROM recipients r JOIN items i ON i.item_pk = r.item_pk
                          WHERE +i.account_id = ?1 AND i.deleted = 0 AND r.email LIKE '%@%'
                         UNION ALL
                         SELECT from_email, from_name, COALESCE(received_at, sent_at)
                           FROM items WHERE +account_id = ?1 AND deleted = 0 AND from_email LIKE '%@%')
                  GROUP BY e ORDER BY weight DESC, last_at DESC",
            )?;
            let rows = st
                .query_map(params![account.0, now - 30 * day, now - 365 * day], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(3)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let rows: Vec<CachedRecipient> = rows
            .into_iter()
            .map(|(email, named, weight)| {
                let name = named.and_then(|n| n.split_once('|').map(|(_, name)| name.to_string())).filter(|n| !n.is_empty());
                let name_norm = name.as_deref().map(text::norm_lower).unwrap_or_default();
                CachedRecipient { email_norm: email.clone(), name_norm, suggestion: RecipientSuggestion { name, email, weight } }
            })
            .collect();
        debug!(addresses = rows.len(), ms = started.elapsed().as_millis(), "кэш подсказок получателей построен");
        Ok(RecipientCache { account, built_at: Instant::now(), generation, rows })
    }

    /// Собственные адреса пользователя: самые частые адреса отправителя в «Отправленных»
    /// (до трёх, в нижнем регистре, без повторов). Пусто, если папка или письма не загружены.
    pub fn own_addresses(&self, account: AccountId) -> Result<Vec<String>> {
        let Some((sent_pk, _)) = self.folder_by_distinguished(account, "sentitems")? else { return Ok(vec![]) };
        self.with_conn(|c| {
            let mut st = c.prepare_cached(
                "SELECT lower(from_email) AS e, COUNT(*) AS n FROM items
                  WHERE folder_pk = ?1 AND deleted = 0 AND from_email LIKE '%@%'
                  GROUP BY e ORDER BY n DESC, e LIMIT 3",
            )?;
            Ok(st.query_map(params![sent_pk], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?)
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

/// Начинается ли строка или любое её слово с `needle` (границы слов — не буквы и не цифры:
/// пробел, точка, `@`, дефис). Обе строки уже нормализованы.
fn word_prefix_match(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.match_indices(needle).any(|(i, _)| i == 0 || hay[..i].chars().next_back().map(|c| !c.is_alphanumeric()).unwrap_or(true))
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

/// Поддерево папки (сама папка первой, затем не удалённые подпапки в ширину) по цепочке
/// `parent_server_id`; защита от цикла — через множество увиденных и предел глубины.
fn folder_subtree(tx: &Transaction, account_id: i64, folder_pk: i64) -> Result<Vec<i64>> {
    let mut out = vec![folder_pk];
    let mut queue: Vec<(i64, usize)> = vec![(folder_pk, 0)];
    let mut st = tx.prepare_cached("SELECT c.folder_pk FROM folders c JOIN folders p ON p.account_id = c.account_id AND p.server_id = c.parent_server_id WHERE p.folder_pk = ?1 AND c.account_id = ?2 AND c.deleted = 0")?;
    while let Some((pk, depth)) = queue.pop() {
        if depth > 64 {
            break;
        }
        let children = st.query_map(params![pk, account_id], |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        for c in children {
            if !out.contains(&c) {
                out.push(c);
                queue.push((c, depth + 1));
            }
        }
    }
    Ok(out)
}

/// Текущее положение письма: ключ, беседа, папка, прочитанность и признак удаления.
struct ItemState {
    item_pk: i64,
    conversation_pk: Option<i64>,
    folder_pk: i64,
    is_read: bool,
    deleted: bool,
}

fn item_state(tx: &Transaction, account: AccountId, server_id: &str) -> Result<Option<ItemState>> {
    let mut st = tx.prepare_cached("SELECT item_pk, conversation_pk, folder_pk, is_read, deleted FROM items WHERE account_id = ?1 AND server_id = ?2")?;
    Ok(st
        .query_row(params![account.0, server_id], |r| {
            Ok(ItemState { item_pk: r.get(0)?, conversation_pk: r.get(1)?, folder_pk: r.get(2)?, is_read: r.get::<_, i64>(3)? != 0, deleted: r.get::<_, i64>(4)? != 0 })
        })
        .optional()?)
}

/// Положение и состояние письма по первичному ключу (для локальных операций записи).
struct ItemPos {
    account_id: i64,
    server_id: String,
    conversation_pk: Option<i64>,
    folder_pk: i64,
    is_read: bool,
    deleted: bool,
}

fn item_pos(tx: &Transaction, item_pk: i64) -> Result<Option<ItemPos>> {
    let mut st = tx.prepare_cached("SELECT account_id, server_id, conversation_pk, folder_pk, is_read, deleted FROM items WHERE item_pk = ?1")?;
    Ok(st
        .query_row(params![item_pk], |r| {
            Ok(ItemPos { account_id: r.get(0)?, server_id: r.get(1)?, conversation_pk: r.get(2)?, folder_pk: r.get(3)?, is_read: r.get::<_, i64>(4)? != 0, deleted: r.get::<_, i64>(5)? != 0 })
        })
        .optional()?)
}

/// Сдвигает локальные счётчики папки (`total_count`, `unread_count`) между синхронизациями
/// иерархии; ниже нуля не опускает. Следующая синхронизация перепишет их серверными.
fn adjust_folder_counts(tx: &Transaction, folder_pk: i64, d_total: i64, d_unread: i64, now: i64) -> Result<()> {
    if d_total == 0 && d_unread == 0 {
        return Ok(());
    }
    tx.prepare_cached("UPDATE folders SET total_count = MAX(0, total_count + ?2), unread_count = MAX(0, unread_count + ?3), updated_at = ?4 WHERE folder_pk = ?1")?
        .execute(params![folder_pk, d_total, d_unread, now])?;
    Ok(())
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

    /// Пересчитывает агрегаты затронутых пар и бесед; возвращает беседы (по возрастанию).
    fn recompute(&self, tx: &Transaction) -> Result<Vec<i64>> {
        let mut convs: BTreeSet<i64> = BTreeSet::new();
        for (c, f) in &self.pairs {
            recompute_conversation_folder(tx, *c, *f)?;
            convs.insert(*c);
        }
        for c in &convs {
            recompute_conversation(tx, *c)?;
        }
        Ok(convs.into_iter().collect())
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
                    // Сервер видит письмо в другой папке, чем локальная строка (перенос локально
                    // применён, а сервер ещё ждёт окно отмены, и `Update` вернул письмо назад):
                    // счётчики папок сдвигаются вместе со строкой, иначе `move_local` при
                    // подтверждении или откате посчитает дельту от фактической папки второй раз.
                    if !p.deleted && p.folder_pk != folder_pk {
                        adjust_folder_counts(tx, p.folder_pk, -1, -(!p.is_read as i64), now)?;
                        adjust_folder_counts(tx, folder_pk, 1, !m.is_read as i64, now)?;
                    }
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
                if n > 0
                    && let Some(st) = item_state(tx, account, &item_id.id)?
                {
                    touched.add(st.conversation_pk, st.folder_pk);
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
    if !existed
        && let (Some(imid), Some(received)) = (&m.internet_message_id, m.received_at)
    {
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
            reply_to: vec![],
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: Some(body.into()), full_body: None, is_truncated: false }),
            attachments,
        }
    }

    #[test]
    fn reply_to_is_stored_as_kind_4() {
        let s = tmp_store("replyto");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox"))]).unwrap();
        let (fpk, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let mut m = full(meta("A", "Рассылка"), "<p>Новости</p>", vec![]);
        m.reply_to = vec![Participant { name: Some("Шлёпанцев Владелец".into()), email: Some("owner@list.ru".into()), routing_type: Some("SMTP".into()) }];
        s.store_full_messages(acc, fpk, std::slice::from_ref(&m)).unwrap();
        let pk: i64 = s.debug_query("SELECT item_pk FROM items WHERE server_id = 'A'").unwrap()[1].parse().unwrap();
        let rec = s.recipients(pk).unwrap();
        assert_eq!(rec.iter().map(|r| (r.kind, r.email.clone().unwrap())).collect::<Vec<_>>(), vec![(1, "me@x.ru".to_string()), (4, "owner@list.ru".to_string())]);
        let head = s.message_head(pk).unwrap().unwrap();
        assert_eq!(head.reply_to, vec![Participant { name: Some("Шлёпанцев Владелец".into()), email: Some("owner@list.ru".into()), routing_type: None }]);
        assert_eq!(aiowa_core::effective_reply_to(Some(&meta("A", "x").from.unwrap()), &head.reply_to).len(), 1, "адрес отличается от отправителя — ответ идёт на него");
        let conv = s.item_ref(pk).unwrap().unwrap().conversation_pk.unwrap();
        assert_eq!(s.conversation_messages(acc, conv).unwrap()[0].reply_to.len(), 1);
        // Reply-To не попадает в индекс получателей (представление берёт kind 1 и 2) и в подсказки не мешает «Кому».
        assert!(s.search(acc, "кому:шлепанцев", 10).unwrap().is_empty(), "Reply-To не индексируется как получатель");
        assert_eq!(s.search(acc, "кому:я", 10).unwrap().len(), 1);
        // Повторная запись без Reply-To (заголовок исчез) убирает строки kind 4.
        m.reply_to.clear();
        s.store_full_messages(acc, fpk, &[m]).unwrap();
        assert!(s.message_head(pk).unwrap().unwrap().reply_to.is_empty());
        assert_eq!(s.recipients(pk).unwrap().len(), 1);
    }

    #[test]
    fn folder_local_operations() {
        let s = tmp_store("folderops");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(
            acc,
            &[
                folder_with("Входящие", Some("inbox"), Some("IPF.Note"), Some("ROOT"), 100),
                folder_with("Удаленные", Some("deleteditems"), Some("IPF.Note"), Some("ROOT"), 5),
                folder_with("Проекты", None, Some("IPF.Note"), Some("ROOT"), 3),
                folder_with("Старые", None, Some("IPF.Note"), Some("Проекты"), 2),
                folder_with("Архив 2020", None, Some("IPF.Note"), Some("Старые"), 1),
            ],
        )
        .unwrap();
        assert_eq!(s.msgfolderroot_server_id(acc).unwrap().as_deref(), Some("F-ROOT"), "корень — родитель «Входящих»");
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (trash, _) = s.folder_by_distinguished(acc, "deleteditems").unwrap().unwrap();
        let projects = s.resolve_folder(acc, "Проекты").unwrap().unwrap().folder_pk;
        let old = s.resolve_folder(acc, "Старые").unwrap().unwrap().folder_pk;
        let archive = s.resolve_folder(acc, "Архив 2020").unwrap().unwrap().folder_pk;

        // insert_folder: новая папка в корне, повтор (синхронизация успела первой) не дублирует.
        let created = Folder {
            id: FolderId { id: "F-NEW".into(), change_key: Some("n1".into()) },
            parent_id: Some(FolderId { id: "F-ROOT".into(), change_key: None }),
            display_name: "AIOWA тест".into(),
            distinguished: None,
            folder_class: Some("IPF.Note".into()),
            total_count: 0,
            unread_count: 0,
            child_folder_count: 0,
        };
        let new_pk = s.insert_folder(acc, &created).unwrap();
        assert_eq!(s.insert_folder(acc, &created).unwrap(), new_pk);
        assert_eq!(s.folder_id_by_pk(acc, new_pk).unwrap(), Some(created.id.clone()));
        assert_eq!(s.list_folders(acc).unwrap().iter().filter(|(_, f)| f.display_name == "AIOWA тест").count(), 1);

        // rename_folder_local: имя и ChangeKey из ответа сервера; неизвестная папка — ошибка.
        s.rename_folder_local(new_pk, "AIOWA тест 2", Some(&FolderId { id: "F-NEW".into(), change_key: Some("n2".into()) })).unwrap();
        let f = s.folder_by_pk(acc, new_pk).unwrap().unwrap();
        assert_eq!(f.name, "AIOWA тест 2");
        assert_eq!(s.folder_id_by_pk(acc, new_pk).unwrap().unwrap().change_key.as_deref(), Some("n2"));
        s.rename_folder_local(new_pk, "AIOWA тест 3", None).unwrap();
        assert_eq!(s.folder_id_by_pk(acc, new_pk).unwrap().unwrap().change_key.as_deref(), Some("n2"), "без идентификатора ключ не трогается");
        assert!(s.rename_folder_local(9999, "x", None).is_err());

        // Письма в поддереве «Проекты»: по одному в каждой папке, одна беседа.
        s.apply_item_changes(acc, projects, &[ItemChange::Create(meta_at("P", "Проект", "2026-10-01T10:00:00Z", Some("c1")))]).unwrap();
        s.apply_item_changes(acc, old, &[ItemChange::Create(meta_at("O", "RE: Проект", "2026-10-02T10:00:00Z", Some("c1")))]).unwrap();
        s.apply_item_changes(acc, archive, &[ItemChange::Create(meta_at("Z", "Другое", "2026-10-03T10:00:00Z", None))]).unwrap();
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(meta_at("I", "RE: Проект", "2026-10-04T10:00:00Z", Some("c1")))]).unwrap();
        assert_eq!(s.search(acc, "тема:проект", 10).unwrap().len(), 3);

        // Служебную папку удалить нельзя; перенос в собственную подпапку — тоже.
        assert!(s.delete_folder_local(inbox, None).is_err());
        assert!(s.delete_folder_local(projects, Some(old)).is_err());

        // В «Удалённые»: поддерево целиком меняет родителя, письма на месте.
        let st = s.delete_folder_local(projects, Some(trash)).unwrap();
        assert_eq!(st, FolderDeleteStats { folders: 3, items: 0, conversation_pks: Vec::new() });
        let folders = s.list_folders(acc).unwrap();
        let parent_of = |name: &str| folders.iter().find(|(_, f)| f.display_name == name).map(|(_, f)| f.parent_id.as_ref().unwrap().id.clone()).unwrap();
        assert_eq!(parent_of("Проекты"), "F-Удаленные");
        assert_eq!(parent_of("Старые"), "F-Проекты", "подпапка остаётся под своей папкой");
        assert_eq!(s.list_items(acc, old, 10).unwrap().len(), 1);
        assert_eq!(s.search(acc, "тема:проект", 10).unwrap().len(), 3);

        // Навсегда: три папки и три письма помечены удалёнными, беседа в «Входящих» похудела, индекс чист.
        let st = s.delete_folder_local(projects, None).unwrap();
        assert_eq!((st.folders, st.items), (3, 3));
        let project_conv = s.list_conversations(acc, inbox, 10).unwrap().into_iter().find(|c| c.topic.as_deref() == Some("Проект")).unwrap().conversation_pk;
        assert_eq!(st.conversation_pks.len(), 2, "беседы «Проект» и «Другое»");
        assert!(st.conversation_pks.contains(&project_conv), "беседа с письмом во «Входящих» названа: окно перечитает её");
        assert!(s.folder_by_pk(acc, projects).unwrap().is_none());
        assert!(s.folder_by_pk(acc, archive).unwrap().is_none());
        assert!(s.list_folders(acc).unwrap().iter().all(|(_, f)| f.display_name != "Старые"));
        assert!(s.list_items(acc, old, 10).unwrap().is_empty());
        assert_eq!(s.search(acc, "тема:проект", 10).unwrap().len(), 1, "осталось письмо во «Входящих»");
        assert!(s.search(acc, "тема:другое", 10).unwrap().is_empty());
        let conv = s.list_conversations(acc, inbox, 10).unwrap().into_iter().find(|c| c.topic.as_deref() == Some("Проект")).unwrap();
        assert_eq!((conv.item_count, conv.total_count), (1, 1), "агрегаты беседы пересчитаны без удалённых писем");
        assert!(s.list_conversations(acc, old, 10).unwrap().is_empty());
        assert!(s.delete_folder_local(projects, None).is_err(), "удалённая папка не находится");
        assert_eq!(s.stats(acc).unwrap().items, 1);
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
    fn recipient_suggestions_and_own_addresses() {
        let s = tmp_store("recipients");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Отправленные", Some("sentitems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (sent, _) = s.folder_by_distinguished(acc, "sentitems").unwrap().unwrap();
        let from = |name: Option<&str>, email: &str| Some(Participant { name: name.map(String::from), email: Some(email.into()), routing_type: None });
        let recent = Timestamp::now() - jiff::SignedDuration::from_hours(24);
        let old = Timestamp::now() - jiff::SignedDuration::from_hours(24 * 800);
        // Входящие: три письма от Петрова (два свежих, одно старое; адрес в разном регистре),
        // одно старое от Сидоровой и одно от устаревшего адреса EX (без @).
        let mut items = Vec::new();
        for (id, name, email, at) in [
            ("p1", Some("Пётр Петров"), "Petrov@x.ru", recent),
            ("p2", Some("Пётр Петров"), "petrov@x.ru", recent),
            ("p3", Some("Петров П."), "PETROV@x.ru", old),
            ("s1", Some("Анна Сидорова"), "sidorova@y.ru", old),
            ("ex", Some("Старый"), "/o=Org/ou=Exchange/cn=Recipients/cn=old", recent),
        ] {
            let mut m = meta_at(id, "Тема", "2026-10-16T10:00:00Z", Some(id));
            m.from = from(name, email);
            m.received_at = Some(at);
            items.push(ItemChange::Create(m));
        }
        s.apply_item_changes(acc, inbox, &items).unwrap();
        // Отправленные: свои письма — от me@x.ru (3), от Me@X.ru (в другом регистре) и одно от alias@x.ru.
        let mut sent_items = Vec::new();
        for (id, email) in [("o1", "me@x.ru"), ("o2", "me@x.ru"), ("o3", "Me@X.ru"), ("o4", "alias@x.ru")] {
            let mut m = meta_at(id, "Исходящее", "2026-10-16T10:00:00Z", Some(id));
            m.from = from(Some("Я"), email);
            m.received_at = Some(recent);
            sent_items.push(ItemChange::Create(m));
        }
        s.apply_item_changes(acc, sent, &sent_items).unwrap();
        // Получатели исходящего письма: Иванов (Кому) и Сидорова (Копия) — она же отправитель старого входящего.
        let mut o1 = meta_at("o1", "Исходящее", "2026-10-16T10:00:00Z", Some("o1"));
        o1.from = from(Some("Я"), "me@x.ru");
        o1.received_at = Some(recent);
        let mut f = full(o1, "<p>текст</p>", vec![]);
        f.to = vec![Participant { name: Some("Иван Иванов".into()), email: Some("ivanov@z.ru".into()), routing_type: None }];
        f.cc = vec![Participant { name: Some("Анна Сидорова".into()), email: Some("Sidorova@y.ru".into()), routing_type: None }];
        s.store_full_messages(acc, sent, &[f]).unwrap();

        assert_eq!(s.own_addresses(acc).unwrap(), vec!["me@x.ru".to_string(), "alias@x.ru".to_string()], "самый частый первым, регистр схлопнут, без дублей");

        // Префикс адреса: регистр не важен, три письма Петрова — одна подсказка с суммарным весом.
        let petrov = s.suggest_recipients(acc, "PET", 10).unwrap();
        assert_eq!(petrov.len(), 1, "{petrov:?}");
        assert_eq!(petrov[0].email, "petrov@x.ru");
        assert_eq!(petrov[0].weight, 4 + 4 + 1, "два свежих письма по 4, старое 1");
        assert_eq!(petrov[0].name.as_deref(), Some("Пётр Петров"), "имя от самого свежего письма");
        // Префикс слова имени, без учёта регистра и ё.
        let by_name = s.suggest_recipients(acc, "петр", 10).unwrap();
        assert_eq!(by_name.len(), 1, "{by_name:?}");
        assert_eq!(by_name[0].email, "petrov@x.ru");
        assert_eq!(s.suggest_recipients(acc, "Сидор", 10).unwrap().len(), 1, "по фамилии (второе слово имени)");
        let sidorova = &s.suggest_recipients(acc, "sidorova", 10).unwrap()[0];
        assert_eq!(sidorova.weight, 1 + 4, "старое входящее 1 плюс свежая копия исходящего 4");
        assert!(s.suggest_recipients(acc, "иван", 10).unwrap().iter().any(|r| r.email == "ivanov@z.ru"), "получатель из таблицы recipients");
        assert!(s.suggest_recipients(acc, "old", 10).unwrap().is_empty(), "адрес EX без @ не предлагается");
        assert!(s.suggest_recipients(acc, "etrov", 10).unwrap().is_empty(), "середина слова без границы не совпадает");
        assert_eq!(s.suggest_recipients(acc, "x.ru", 10).unwrap().len(), 3, "домен после @ — граница слова: petrov, me, alias");
        // Пустой префикс: все адреса по убыванию веса; лимит соблюдается.
        let all = s.suggest_recipients(acc, "", 100).unwrap();
        assert_eq!(all.len(), 5, "{all:?}");
        assert!(all.windows(2).all(|w| w[0].weight >= w[1].weight));
        assert_eq!(all[0].email, "me@x.ru");
        assert_eq!(s.suggest_recipients(acc, "", 2).unwrap().len(), 2);
        // Новый адрес появляется после сброса кэша.
        let mut n = meta_at("n1", "Новое", "2026-10-16T10:00:00Z", Some("n1"));
        n.from = from(Some("Новый Контакт"), "new@q.ru");
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(n)]).unwrap();
        s.invalidate_recipient_cache();
        assert_eq!(s.suggest_recipients(acc, "new", 10).unwrap().len(), 1);
    }

    #[test]
    fn word_prefix_boundaries() {
        assert!(word_prefix_match("иван петров", "петр"));
        assert!(word_prefix_match("иван петров", "иван п"));
        assert!(!word_prefix_match("иван петров", "ван"));
        assert!(word_prefix_match("ivan.petrov@x.ru", "petrov"));
        assert!(word_prefix_match("ivan.petrov@x.ru", "x.ru"));
        assert!(!word_prefix_match("sidorova@y.ru", "ova"));
        assert!(word_prefix_match("anything", ""));
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
        s.store_full_messages(acc, fpk, std::slice::from_ref(&msg)).unwrap();

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
        assert_eq!(s.attachment_cache_bytes(), b"%PDF fake".len() as u64, "кэш — это файлы каталога вложений");
        assert!(s.disk_size_bytes() >= std::fs::metadata(s.path()).unwrap().len());
        assert_eq!(s.stats(acc).unwrap().attachment_cache_bytes, b"%PDF fake".len() as u64, "`stats` — тот же подсчёт");
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
    fn stats_sizes_match_data_tab_for_any_file_name() {
        // Не `mail.db`: журнал — `dev.sqlite-wal`, а не `dev.db-wal`.
        let dir = std::env::temp_dir().join(format!("aiowa-store-test-stats-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open(dir.join("dev.sqlite")).unwrap();
        let acc = s.account("h", "u").unwrap();
        assert_eq!(s.attachments_dir(), identity::attachments_dir_for(s.path()));
        std::fs::create_dir_all(s.attachments_dir().join("sub")).unwrap();
        std::fs::write(s.attachments_dir().join("sub").join("x.bin"), [0u8; 11]).unwrap();
        let mut wal = s.path().as_os_str().to_owned();
        wal.push("-wal");
        assert!(Path::new(&wal).is_file(), "журнал открытой базы рядом с ней");
        let st = s.stats(acc).unwrap();
        assert_eq!(st.db_bytes, identity::db_files_bytes(s.path()));
        assert!(st.db_bytes > std::fs::metadata(s.path()).unwrap().len(), "журналы посчитаны");
        assert_eq!(st.attachment_cache_bytes, 11, "каталог кэша на диске со вложенными, хотя строк вложений в базе нет");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
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
        // По учётной записи: свой ключ виден, чужой и несуществующий — нет.
        assert_eq!(s.attachment_of_account(acc, pdf.attachment_pk).unwrap().map(|r| r.server_id), Some("f1".to_string()));
        let other = s.account("h", "другой").unwrap();
        assert!(s.attachment_of_account(other, pdf.attachment_pk).unwrap().is_none());
        assert!(s.attachment_of_account(acc, 999_999).unwrap().is_none());
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
        assert_eq!(keys, vec![crate::ui::MessageKey { item_pk: a_pk, body_state: 1, is_read: false, cached: 0, is_flagged: false }], "{keys:?}");
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
    fn local_write_operations() {
        let s = tmp_store("write");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Отправленные", Some("sentitems")), folder("Архив", None), folder("Удаленные", Some("deleteditems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (sent, _) = s.folder_by_distinguished(acc, "sentitems").unwrap().unwrap();
        let archive = s.resolve_folder(acc, "архив").unwrap().unwrap().folder_pk;
        s.apply_item_changes(
            acc,
            inbox,
            &[
                ItemChange::Create(meta_at("A", "Договор", "2026-10-01T10:00:00Z", Some("c1"))),
                ItemChange::Create(meta_at("B", "RE: Договор", "2026-10-02T10:00:00Z", Some("c1"))),
                ItemChange::Create(meta_at("X", "Другое", "2026-09-01T10:00:00Z", None)),
            ],
        )
        .unwrap();
        s.apply_item_changes(acc, sent, &[ItemChange::Create(meta_at("S", "RE: Договор", "2026-10-03T10:00:00Z", Some("c1")))]).unwrap();
        let pk = |id: &str| -> i64 { s.debug_query(&format!("SELECT item_pk FROM items WHERE server_id = '{id}'")).unwrap()[1].parse().unwrap() };
        let (a, b, x, sp) = (pk("A"), pk("B"), pk("X"), pk("S"));
        let folder_counts = |fpk: i64| -> (i64, i64) {
            let line = &s.debug_query(&format!("SELECT total_count || ',' || unread_count FROM folders WHERE folder_pk = {fpk}")).unwrap()[1];
            let (t, u) = line.split_once(',').unwrap();
            (t.parse().unwrap(), u.parse().unwrap())
        };
        let conv_in = |fpk: i64| -> ConversationRow { s.list_conversations(acc, fpk, 10).unwrap().into_iter().find(|c| c.topic.as_deref() == Some("Договор")).expect("беседа «Договор»") };
        let c1 = conv_in(inbox).conversation_pk;
        assert_eq!(folder_counts(inbox), (10, 1), "счётчики папки из иерархии до операций");

        // item_refs: пакетно, в порядке запроса, неизвестные пропущены, состояние до операции.
        let refs = s.item_refs(&[b, a, 9999]).unwrap();
        assert_eq!(refs.iter().map(|r| r.item_pk).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!((refs[0].folder_pk, refs[0].id.id.as_str(), refs[0].is_read, refs[0].is_flagged), (inbox, "B", false, false));

        // conversation_item_pks: вся беседа и только папка, новые первыми.
        assert_eq!(s.conversation_item_pks(acc, c1, None).unwrap(), vec![sp, b, a]);
        assert_eq!(s.conversation_item_pks(acc, c1, Some(inbox)).unwrap(), vec![b, a]);
        assert_eq!(s.conversation_item_pks(acc, c1, Some(archive)).unwrap(), Vec::<i64>::new());

        // set_read_local: is_read, агрегаты беседы (в папке и всей), непрочитанные папки.
        assert_eq!(s.set_read_local(&[a, x], true).unwrap().len(), 2, "две беседы затронуты");
        assert!(s.item_ref(a).unwrap().unwrap().is_read);
        let c = conv_in(inbox);
        assert_eq!((c.item_count, c.unread_count, c.total_count), (2, 1, 3));
        assert_eq!(s.debug_query(&format!("SELECT unread_count FROM conversations WHERE conversation_pk = {c1}")).unwrap()[1], "2", "непрочитанных во всей беседе: B и S");
        assert_eq!(folder_counts(inbox), (10, 0), "два письма прочитаны, ниже нуля не уходит");
        assert_eq!(s.set_read_local(&[a], true).unwrap(), vec![c1], "повтор не меняет состояние, но беседу возвращает");
        s.set_read_local(&[a], false).unwrap();
        assert_eq!(folder_counts(inbox), (10, 1));
        assert_eq!(conv_in(inbox).unread_count, 2);
        s.set_read_local(&[a], true).unwrap();

        // set_flag_local: флаг на письме, в ключах беседы и признак беседы в списках.
        assert_eq!(s.set_flag_local(&[b], true).unwrap(), vec![c1]);
        assert!(s.item_ref(b).unwrap().unwrap().is_flagged);
        assert!(s.message_head(b).unwrap().unwrap().is_flagged);
        assert!(s.conversation_keys(acc, c1).unwrap().iter().any(|k| k.item_pk == b && k.is_flagged));
        assert!(conv_in(inbox).has_flagged);
        assert!(s.conversations_page(acc, sent, None, 10).unwrap()[0].has_flagged, "признак общий для беседы во всех папках");
        assert!(!s.list_conversations(acc, inbox, 10).unwrap().iter().find(|c| c.topic.as_deref() == Some("Другое")).unwrap().has_flagged);
        s.set_flag_local(&[b], false).unwrap();
        assert!(!conv_in(inbox).has_flagged);
        assert!(!s.message_head(b).unwrap().unwrap().is_flagged);

        // move_local с новым идентификатором: папка, id, агрегаты обеих папок, счётчики папок.
        s.move_local(a, archive, Some(&ItemId { id: "A2".into(), change_key: Some("k2".into()) })).unwrap();
        let r = s.item_ref(a).unwrap().unwrap();
        assert_eq!((r.folder_pk, r.id.id.as_str(), r.id.change_key.as_deref()), (archive, "A2", Some("k2")));
        let c = conv_in(inbox);
        assert_eq!((c.item_count, c.unread_count, c.total_count), (1, 1, 3), "осталось непрочитанное B");
        let c = conv_in(archive);
        assert_eq!((c.item_count, c.unread_count, c.total_count), (1, 0, 3));
        assert_eq!(folder_counts(inbox), (9, 0), "перенесено прочитанное: непрочитанных не убавилось (ранее ушло в ноль)");
        assert_eq!(folder_counts(archive), (11, 1));
        assert_eq!(s.conversation_item_pks(acc, c1, Some(archive)).unwrap(), vec![a]);
        // Откат без идентификатора: папка возвращается, id остаётся новым.
        s.move_local(a, inbox, None).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().folder_pk, inbox);
        assert_eq!(conv_in(inbox).item_count, 2);
        assert!(s.list_conversations(acc, archive, 10).unwrap().is_empty());
        assert_eq!(folder_counts(archive), (10, 1));

        // update_item_id: новый ChangeKey после GetItem/UpdateItem.
        s.update_item_id(a, &ItemId { id: "A2".into(), change_key: Some("k3".into()) }).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().id.change_key.as_deref(), Some("k3"));
        assert!(s.update_item_id(9999, &ItemId { id: "n".into(), change_key: None }).is_err());
        // Полный ключ усечённым не заменяется (ответ UpdateItem — 8 символов, п. 19 обкатки),
        // без ключа — остаётся прежний; с новым server_id ключ берётся как есть.
        let full = "CQAAABYAAAD7bL0OVIq7Rr0Dd0Rdq5a5AAAbuyX5".to_string();
        s.update_item_id(a, &ItemId { id: "A2".into(), change_key: Some(full.clone()) }).unwrap();
        s.update_item_id(a, &ItemId { id: "A2".into(), change_key: Some("CQAAAA==".into()) }).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().id.change_key.as_deref(), Some(full.as_str()), "усечённый ключ не затирает полный");
        s.update_item_id(a, &ItemId { id: "A2".into(), change_key: None }).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().id.change_key.as_deref(), Some(full.as_str()), "без ключа прежний остаётся");
        assert!(s.items_with_short_change_keys(acc, ItemId::MIN_FULL_CHANGE_KEY, 10).unwrap().iter().all(|(pk, _)| *pk != a));
        s.update_item_id(a, &ItemId { id: "A3".into(), change_key: Some("CQAAAA==".into()) }).unwrap();
        let r = s.item_ref(a).unwrap().unwrap();
        assert_eq!((r.id.id.as_str(), r.id.change_key.as_deref()), ("A3", Some("CQAAAA==")), "новый идентификатор после переноса берётся целиком");
        let short = s.items_with_short_change_keys(acc, ItemId::MIN_FULL_CHANGE_KEY, 10).unwrap();
        assert!(short.iter().any(|(pk, id)| *pk == a && id.id == "A3"), "усечённый ключ попадает в выборку на починку: {short:?}");

        // mark_deleted_local и восстановление: агрегаты, индекс, счётчики папки.
        assert_eq!(s.search(acc, "тема:договор", 10).unwrap().len(), 3);
        assert_eq!(s.mark_deleted_local(&[b]).unwrap(), vec![c1]);
        assert!(s.item_ref(b).unwrap().is_none());
        let c = conv_in(inbox);
        assert_eq!((c.item_count, c.unread_count, c.total_count), (1, 0, 2));
        assert_eq!(s.search(acc, "тема:договор", 10).unwrap().len(), 2, "удалённое письмо снято с индекса");
        assert_eq!(folder_counts(inbox), (9, 0));
        assert_eq!(s.restore_deleted_local(&[b]).unwrap(), vec![c1]);
        let c = conv_in(inbox);
        assert_eq!((c.item_count, c.unread_count, c.total_count), (2, 1, 3));
        assert_eq!(s.search(acc, "тема:договор", 10).unwrap().len(), 3, "восстановленное письмо снова в индексе");
        assert_eq!(folder_counts(inbox), (10, 1));
        assert!(!s.item_refs(&[b]).unwrap()[0].is_read);

        // move_local, когда синхронизация уже создала письмо с новым id в целевой папке:
        // текущая строка гасится, двойник получает папку, беседа в источнике редеет.
        s.apply_item_changes(acc, archive, &[ItemChange::Create(meta_at("B2", "RE: Договор", "2026-10-02T10:00:00Z", Some("c1")))]).unwrap();
        let b2 = pk("B2");
        s.move_local(b, archive, Some(&ItemId { id: "B2".into(), change_key: Some("z".into()) })).unwrap();
        assert!(s.item_ref(b).unwrap().is_none(), "старая строка помечена удалённой");
        let r = s.item_ref(b2).unwrap().unwrap();
        assert_eq!((r.folder_pk, r.id.change_key.as_deref()), (archive, Some("z")));
        assert_eq!(conv_in(inbox).item_count, 1);
        assert_eq!(conv_in(archive).item_count, 1);
        assert_eq!(s.stats(acc).unwrap().items, 4, "A, X, S и B2");
    }

    /// Окно отмены: письмо перенесено локально (`move_local` без идентификатора), а синхронизация
    /// исходной папки принесла по нему `Update` — строка возвращается в исходную папку вместе со
    /// счётчиками, и подтверждение (или откат) переноса даёт верную дельту. `ReadFlag` строку не возвращает.
    #[test]
    fn sync_update_during_undo_window_keeps_folder_counts() {
        let s = tmp_store("staged-update");
        let acc = s.account("h", "u").unwrap();
        s.upsert_folders(acc, &[folder("Входящие", Some("inbox")), folder("Удаленные", Some("deleteditems"))]).unwrap();
        let (inbox, _) = s.folder_by_distinguished(acc, "inbox").unwrap().unwrap();
        let (trash, _) = s.folder_by_distinguished(acc, "deleteditems").unwrap().unwrap();
        let counts = |fpk: i64| -> (i64, i64) {
            let line = &s.debug_query(&format!("SELECT total_count || ',' || unread_count FROM folders WHERE folder_pk = {fpk}")).unwrap()[1];
            let (t, u) = line.split_once(',').unwrap();
            (t.parse().unwrap(), u.parse().unwrap())
        };
        let a_meta = meta_at("A", "Договор", "2026-10-01T10:00:00Z", Some("c1"));
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(a_meta.clone())]).unwrap();
        let a: i64 = s.debug_query("SELECT item_pk FROM items WHERE server_id = 'A'").unwrap()[1].parse().unwrap();
        assert_eq!((counts(inbox), counts(trash)), ((10, 1), (10, 1)), "счётчики из иерархии; синхронизация их не трогает");

        // Удаление поставлено в окно отмены: локально письмо в «Удалённых».
        s.move_local(a, trash, None).unwrap();
        assert_eq!((counts(inbox), counts(trash)), ((9, 0), (11, 2)));
        assert!(s.list_conversations(acc, inbox, 10).unwrap().is_empty());

        // Другой клиент изменил письмо: Update по «Входящим» возвращает строку туда же вместе со счётчиками.
        s.apply_item_changes(acc, inbox, &[ItemChange::Update(a_meta.clone())]).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().folder_pk, inbox, "сервер видит письмо во «Входящих»");
        assert_eq!((counts(inbox), counts(trash)), ((10, 1), (10, 1)), "счётчики следуют за строкой");
        assert_eq!(s.list_conversations(acc, inbox, 10).unwrap().len(), 1);

        // Срок вышел: подтверждение переноса с новым идентификатором — дельта ровно одна.
        s.move_local(a, trash, Some(&ItemId { id: "A2".into(), change_key: Some("k2".into()) })).unwrap();
        assert_eq!(s.item_ref(a).unwrap().unwrap().folder_pk, trash);
        assert_eq!((counts(inbox), counts(trash)), ((9, 0), (11, 2)), "как после одного переноса, а не двух");
        assert!(s.list_conversations(acc, inbox, 10).unwrap().is_empty());

        // Откат после такого же Update: строка и счётчики как до действия (письмо прочитанное,
        // чтобы счётчик непрочитанных не упирался в ноль).
        let mut b_meta = meta_at("B", "Счёт", "2026-10-02T10:00:00Z", Some("c2"));
        b_meta.is_read = true;
        s.apply_item_changes(acc, inbox, &[ItemChange::Create(b_meta.clone())]).unwrap();
        let b: i64 = s.debug_query("SELECT item_pk FROM items WHERE server_id = 'B'").unwrap()[1].parse().unwrap();
        let before = (counts(inbox), counts(trash));
        s.move_local(b, trash, None).unwrap();
        s.apply_item_changes(acc, inbox, &[ItemChange::Update(b_meta)]).unwrap();
        s.move_local(b, inbox, None).unwrap();
        assert_eq!(s.item_ref(b).unwrap().unwrap().folder_pk, inbox);
        assert_eq!((counts(inbox), counts(trash)), before, "после отката дрейфа нет");

        // ReadFlagChange строку не возвращает и счётчики папок не трогает.
        s.move_local(b, trash, None).unwrap();
        let moved = (counts(inbox), counts(trash));
        s.apply_item_changes(acc, inbox, &[ItemChange::ReadFlag { item_id: ItemId { id: "B".into(), change_key: None }, is_read: false }]).unwrap();
        let r = s.item_ref(b).unwrap().unwrap();
        assert_eq!((r.folder_pk, r.is_read), (trash, false));
        assert_eq!((counts(inbox), counts(trash)), moved);

        // Update по удалённой строке счётчики не сдвигает (её в исходной папке не считали).
        s.mark_deleted_local(&[b]).unwrap();
        let after_delete = (counts(inbox), counts(trash));
        s.apply_item_changes(acc, inbox, &[ItemChange::Update(meta_at("B", "Счёт", "2026-10-02T10:00:00Z", Some("c2")))]).unwrap();
        assert_eq!(s.item_ref(b).unwrap().unwrap().folder_pk, inbox);
        assert_eq!((counts(inbox), counts(trash)), after_delete);
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
                folder_with("Черновики", Some("drafts"), Some("IPF.Note"), None, 12),
                folder_with("Исходящие", Some("outbox"), Some("IPF.Note"), None, 1),
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
        assert_eq!(names, vec!["Входящие", "Отправленные", "Проекты", "Черновики", "Мелочь", "RSS"], "черновики синхронизируются по умолчанию, исходящие и удалённые — нет");
        assert!(!SERVICE_FOLDERS.contains(&"drafts") && SERVICE_FOLDERS.contains(&"outbox"));
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

    #[test]
    fn db_path_components_are_valid_directory_names() {
        // Обычные имена не меняются: прежние базы остаются на своих местах.
        assert_eq!(safe_path_component("mail.x.ru"), "mail.x.ru");
        assert_eq!(safe_path_component("xn--80a1acny.xn--p1ai"), "xn--80a1acny.xn--p1ai");
        assert_eq!(safe_path_component("ivanov@x.ru"), "ivanov_x.ru");
        assert_eq!(safe_path_component("CORP\\ivanov"), "CORP_ivanov");
        // Порт и IPv6 — без двоеточий и скобок.
        assert_eq!(safe_path_component("mail.x.ru:8443"), "mail.x.ru_8443");
        assert_eq!(safe_path_component("[::1]"), "___1_");
        assert_eq!(safe_path_component("[::1]:8443"), "___1__8443");
        // Путь не уходит выше каталога AIOWA.
        assert_eq!(safe_path_component(".."), "__");
        assert_eq!(safe_path_component("."), "_");
        assert_eq!(safe_path_component(""), "_");

        let base = std::env::temp_dir().join(format!("aiowa-store-dbpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for (host, user) in [("mail.x.ru:8443", "ivanov@x.ru"), ("[::1]", "CORP\\ivanov"), ("10.0.0.5:444", "..")] {
            let path = db_path_under(&base, host, user);
            assert!(path.starts_with(base.join("AIOWA")), "{}", path.display());
            let host_dir = path.parent().and_then(Path::parent).and_then(Path::file_name).unwrap().to_string_lossy().into_owned();
            assert!(!host_dir.contains([':', '[', ']']), "{host_dir}");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
        assert_eq!(db_path_under(&base, "mail.x.ru", "ivanov@x.ru"), base.join("AIOWA").join("mail.x.ru").join("ivanov_x.ru").join("mail.db"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
