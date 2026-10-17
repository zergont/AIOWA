//! Trait транспорта почты и модель ошибок. Реализации: OWA JSON (`aiowa-transport-owa`),
//! позже EWS SOAP и фейковый сервер для тестов.
//!
//! Уровень абстракции: операции схемы EWS, а не HTTP. Транспорт сам отвечает за
//! сессию, повторный вход и ограничение параллелизма.

use aiowa_core::{FolderChanges, FolderId, FolderRef, ItemChanges, ItemId, MessageFull, SyncState};
use secrecy::SecretString;
use std::time::Duration;

pub use async_trait::async_trait;

/// Ошибка транспорта. Предикаты подсказывают движку синхронизации, что делать.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Сессия истекла: сервер ответил 440/401 или редиректом на страницу входа.
    #[error("сессия истекла")]
    SessionExpired,
    /// Отказ CSRF-защиты: нет или устарела канарейка (редирект на errorfe.aspx).
    #[error("отказ CSRF-защиты")]
    CsrfRejected,
    /// Исчерпан лимит одновременных сессий на ящик.
    #[error("исчерпан лимит сессий на ящик, подождите")]
    SessionLimit,
    /// Вход отклонён сервером (неверные учётные данные или формат логина).
    #[error("вход отклонён: {0}")]
    AuthFailed(String),
    /// Сервер просит подождать (ErrorServerBusy, BackOffMilliseconds).
    #[error("сервер просит подождать {retry_after:?}")]
    Throttled { retry_after: Option<Duration> },
    /// Сетевая ошибка: соединение, TLS, таймаут.
    #[error("сеть: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// Ответ не разобран или HTTP-статус неожиданный. `raw` усечён.
    #[error("протокол {op}: HTTP {status}: {raw}")]
    Protocol { op: String, status: u16, raw: String },
    /// Сервер вернул ошибку уровня операции (`ResponseClass: Error`).
    #[error("сервер {code}: {message}")]
    Server { code: String, message: String },
    /// Операция отменена клиентом.
    #[error("отменено")]
    Cancelled,
    /// Ответ больше допустимого размера (например, вложение сверх лимита).
    #[error("ответ больше лимита {limit} байт")]
    TooLarge { limit: u64 },
}

impl TransportError {
    /// Стоит ли повторить ту же операцию позже без вмешательства человека.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Throttled { .. } | Self::Network(_) | Self::SessionLimit)
            || matches!(self, Self::Server { code, .. } if code == "ErrorServerBusy" || code == "ErrorTimeoutExpired" || code == "ErrorMailboxStoreUnavailable")
    }

    /// Нужен ли повторный вход перед следующей попыткой.
    pub fn needs_reauth(&self) -> bool {
        matches!(self, Self::SessionExpired | Self::CsrfRejected)
    }

    /// Состояние синхронизации недействительно: папку надо синхронизировать заново.
    pub fn invalidates_sync_state(&self) -> bool {
        matches!(self, Self::Server { code, .. } if code == "ErrorInvalidSyncStateData" || code == "ErrorSyncFolderNotFound")
    }
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Учётные данные для входа. Пароль не печатается и не клонируется без нужды.
#[derive(Clone)]
pub struct Credentials {
    pub host: String,
    pub user: String,
    pub password: SecretString,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("host", &self.host).field("user", &self.user).field("password", &"***").finish()
    }
}

/// Какие данные запрашивать у элементов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemFetchShape {
    /// Только метаданные списка (без тела и получателей).
    Meta,
    /// Метаданные, получатели, вложения и `UniqueBody`.
    WithUniqueBody,
    /// То же плюс полное тело с цитатами.
    WithFullBody,
}

/// Результат запроса одного элемента в `get_items`: позиция соответствует запрошенному id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemResult {
    Ok(MessageFull),
    /// Сервер отказал по этому элементу (например, `ErrorItemNotFound`: удалён между вызовами).
    Failed { id: ItemId, code: String, message: String },
}

impl ItemResult {
    pub fn is_not_found(&self) -> bool {
        matches!(self, ItemResult::Failed { code, .. } if code == "ErrorItemNotFound")
    }
}

/// Содержимое вложения, скачанное с сервера.
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentContent {
    pub bytes: Vec<u8>,
    /// MIME-тип из ответа сервера, если был.
    pub content_type: Option<String>,
    /// Имя файла из ответа сервера (`Content-Disposition`), если было.
    pub file_name: Option<String>,
}

impl std::fmt::Debug for AttachmentContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentContent").field("bytes", &self.bytes.len()).field("content_type", &self.content_type).field("file_name", &self.file_name).finish()
    }
}

/// Счётчики папки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCounts {
    pub id: FolderId,
    pub total: i64,
    pub unread: i64,
}

/// Сведения о сервере после входа.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerInfo {
    pub host: String,
    /// Сборка сервера, например `15.2.1748.39`.
    pub build: Option<String>,
}

/// Транспорт почты. Все методы потокобезопасны и могут вызываться параллельно;
/// реализация сама ограничивает число одновременных запросов.
#[async_trait]
pub trait MailTransport: Send + Sync {
    /// Иерархия почтовых папок. `state` = `None` — полная выгрузка.
    async fn sync_folder_hierarchy(&self, state: Option<&SyncState>) -> Result<FolderChanges>;

    /// Изменения элементов папки пачкой до `max_changes` (сервер ограничивает 512).
    /// При `with_meta` создания и обновления приходят с метаданными списка.
    async fn sync_folder_items(&self, folder: &FolderRef, state: Option<&SyncState>, max_changes: u32, with_meta: bool) -> Result<ItemChanges>;

    /// Элементы по идентификаторам. Реализация сама режет на допустимые пачки.
    /// Результат той же длины и в том же порядке, что `ids`; отказ по одному элементу
    /// приходит как `ItemResult::Failed`, `Err` означает отказ всего вызова.
    async fn get_items(&self, ids: &[ItemId], shape: ItemFetchShape) -> Result<Vec<ItemResult>>;

    /// Счётчики папок.
    async fn get_folder_counts(&self, folders: &[FolderRef]) -> Result<Vec<FolderCounts>>;

    /// Содержимое файлового вложения по `AttachmentId`. Ответ больше `max_bytes`
    /// обрывается с `TooLarge`; отсутствующее вложение приходит как `Server`/`Protocol`.
    async fn get_attachment(&self, attachment_id: &str, max_bytes: u64) -> Result<AttachmentContent>;

    /// Завершить серверную сессию. После вызова транспорт непригоден.
    async fn logoff(&self) -> Result<()>;

    fn server_info(&self) -> ServerInfo;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates() {
        assert!(TransportError::SessionExpired.needs_reauth());
        assert!(TransportError::CsrfRejected.needs_reauth());
        assert!(!TransportError::SessionExpired.is_retryable());
        assert!(TransportError::Throttled { retry_after: None }.is_retryable());
        assert!(TransportError::Server { code: "ErrorInvalidSyncStateData".into(), message: String::new() }.invalidates_sync_state());
        assert!(!TransportError::Cancelled.is_retryable());
        assert!(!TransportError::TooLarge { limit: 1 }.is_retryable());
    }

    #[test]
    fn credentials_hide_password() {
        let c = Credentials { host: "h".into(), user: "u".into(), password: SecretString::from("p".to_string()) };
        assert!(!format!("{c:?}").contains('p') || format!("{c:?}").contains("***"));
    }
}
