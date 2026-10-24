//! Trait транспорта почты и модель ошибок. Реализации: OWA JSON (`aiowa-transport-owa`),
//! позже EWS SOAP и фейковый сервер для тестов.
//!
//! Уровень абстракции: операции схемы EWS, а не HTTP. Транспорт сам отвечает за
//! сессию, повторный вход и ограничение параллелизма.

use aiowa_core::{FolderChanges, FolderId, FolderRef, ItemChanges, ItemId, MessageFull, Participant, SyncState};
pub use aiowa_core::{
    Attendee, CalendarEvent, CalendarEventDetails, CalendarItemChanges, CalendarItemType, ComposeKind, FreeBusy, MeetingMessageKind, MeetingRequestInfo, MeetingResponse, NewCalendarItem, OutgoingAttachment,
    OutgoingMessage, ResponseType, Signature,
};
use jiff::Timestamp;
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
    /// Ответы шлюза 502/503/504 — временная недоступность, а не отказ по элементу.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Throttled { .. } | Self::Network(_) | Self::SessionLimit)
            || matches!(self, Self::Protocol { status: 502..=504, .. })
            || matches!(self, Self::Server { code, .. } if code == "ErrorServerBusy" || code == "ErrorTimeoutExpired" || code == "ErrorMailboxStoreUnavailable")
    }

    /// Отказ сервера по конкретной операции (`ResponseClass: Error` без признака временности):
    /// именно такие ошибки можно записывать в базу как отказ по письму или вложению. Истёкшая
    /// сессия, отклонённый вход, сеть и исчерпанные повторы сюда не относятся — они про
    /// соединение в целом, и помечать по ним элементы нельзя.
    pub fn is_item_failure(&self) -> bool {
        matches!(self, Self::Server { .. }) && !self.is_retryable()
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

/// Режим удаления письма на сервере.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Перенести в «Удалённые» (`MoveToDeletedItems`): письмо получает новый идентификатор.
    ToDeletedItems,
    /// Удалить навсегда (`HardDelete`).
    Hard,
}

/// Результат операции записи по одному элементу: позиция соответствует запрошенному id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteResult {
    /// Успех. `id` — идентификатор после операции: новый `ChangeKey` после `UpdateItem`,
    /// новый идентификатор в целевой папке после `MoveItem`, для удаления — исходный.
    Ok { id: ItemId },
    /// Сервер отказал по этому элементу (`ResponseClass: Error`).
    Failed { id: ItemId, code: String, message: String },
}

impl WriteResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, WriteResult::Ok { .. })
    }

    /// Идентификатор из результата (новый при успехе, исходный при отказе).
    pub fn id(&self) -> &ItemId {
        match self {
            WriteResult::Ok { id } | WriteResult::Failed { id, .. } => id,
        }
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

/// Вид подписки на уведомления сервера (`SubscribeToNotification`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationKind {
    /// Строки папки (`RowNotification`): письмо добавлено, изменено, удалено.
    FolderRows { folder: FolderId },
    /// Новая почта ящика (`NewMailNotification`), без привязки к папке.
    NewMail,
    /// Дерево папок (`HierarchyNotification`).
    Hierarchy,
    /// Счётчик непросмотренных (`UnseenItemNotification`).
    Unseen,
    /// События календаря (`CalendarItemNotification`) по папке календаря.
    Calendar { folder: FolderId },
}

impl NotificationKind {
    /// Имя типа уведомления, как его принимает сервер.
    pub fn name(&self) -> &'static str {
        match self {
            NotificationKind::FolderRows { .. } => "RowNotification",
            NotificationKind::NewMail => "NewMailNotification",
            NotificationKind::Hierarchy => "HierarchyNotification",
            NotificationKind::Unseen => "UnseenItemNotification",
            NotificationKind::Calendar { .. } => "CalendarItemNotification",
        }
    }
}

/// Запрос подписки: идентификатор выбирает клиент (guid), по нему же приходят события.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRequest {
    pub id: String,
    pub kind: NotificationKind,
}

/// Ответ сервера по одной подписке: позиция соответствует запросу.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionResult {
    pub id: String,
    /// Подписка создана или уже существовала.
    pub created: bool,
    /// Текст отказа сервера (`ErrorInfo`), если подписка не создана.
    pub error: Option<String>,
}

/// Событие канала уведомлений. Содержимое писем здесь минимально (отправитель, тема, начало
/// превью у новой почты) и нужно только для тоста; в журнал пишутся только типы и счётчики.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// Изменились строки папки: нужна синхронизация папки. `event_type` — `RowAdded`,
    /// `RowModified`, `RowDeleted`, `QueryResultChanged`, `Reload` или как прислал сервер.
    FolderRows { subscription: String, folder: Option<FolderId>, event_type: String },
    /// Новое письмо в ящике.
    NewMail { subscription: String, item_id: Option<ItemId>, folder: Option<FolderId>, from: Option<Participant>, subject: Option<String>, preview: Option<String>, is_read: Option<bool> },
    /// Изменилось дерево папок. `folder` и `event_type` — как прислал сервер (у браузерной OWA
    /// `Modified` по папке приходит и при смене её счётчиков, то есть вместе с каждым письмом);
    /// без папки или при создании, удалении, переносе папки вызывающий обновляет всё дерево.
    Hierarchy { subscription: String, folder: Option<FolderId>, event_type: String },
    /// Изменился счётчик непросмотренных.
    Unseen { subscription: String, count: Option<i64> },
    /// Изменился календарь.
    Calendar { subscription: String, folder: Option<FolderId> },
    /// Сервер подтверждает, что поток жив.
    KeepAlive,
    /// Подписки потеряны (новая сессия или просьба сервера): нужно подписаться заново.
    Reload,
    /// Блок неизвестного вида; `raw_kind` — только имя типа или ключи, без содержимого.
    Unknown { raw_kind: String },
}

impl NotificationEvent {
    /// Короткое имя варианта для журнала и счётчиков.
    pub fn kind_name(&self) -> &'static str {
        match self {
            NotificationEvent::FolderRows { .. } => "FolderRows",
            NotificationEvent::NewMail { .. } => "NewMail",
            NotificationEvent::Hierarchy { .. } => "Hierarchy",
            NotificationEvent::Unseen { .. } => "Unseen",
            NotificationEvent::Calendar { .. } => "Calendar",
            NotificationEvent::KeepAlive => "KeepAlive",
            NotificationEvent::Reload => "Reload",
            NotificationEvent::Unknown { .. } => "Unknown",
        }
    }
}

/// Итог одного цикла потока уведомлений.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEnd {
    /// Разобранных блоков (включая keep-alive).
    pub events: usize,
    pub elapsed: Duration,
    /// Поток закрыл сервер (обычный конец цикла), а не обрыв соединения.
    pub finished_by_server: bool,
}

/// Новый случайный идентификатор в форме guid (строчные шестнадцатеричные цифры с дефисами):
/// для `cid` канала уведомлений и `SubscriptionId`.
pub fn new_guid() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
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

    /// Пометить письма прочитанными или непрочитанными (`UpdateItem` поля `message:IsRead`).
    /// Результат той же длины и порядка, что `ids`; отказ по одному элементу — `Failed`,
    /// `Err` — отказ всего вызова (сессия, сеть). В `Ok` — идентификатор с новым `ChangeKey`.
    async fn set_read(&self, ids: &[ItemId], read: bool) -> Result<Vec<WriteResult>>;

    /// Поставить или снять флаг «к исполнению» (`UpdateItem` поля `item:Flag`).
    async fn set_flag(&self, ids: &[ItemId], flagged: bool) -> Result<Vec<WriteResult>>;

    /// Перенести письма в папку (`MoveItem`); в `Ok` новый идентификатор в целевой папке.
    async fn move_items(&self, ids: &[ItemId], to_folder: &FolderRef) -> Result<Vec<WriteResult>>;

    /// Удалить письма (`DeleteItem`): в «Удалённые» или навсегда. В `Ok` исходный идентификатор
    /// (для `ToDeletedItems` — новый, если сервер его сообщил).
    async fn delete_items(&self, ids: &[ItemId], mode: DeleteMode) -> Result<Vec<WriteResult>>;

    /// Создать почтовую папку (`CreateFolder`, класс `IPF.Note`) внутри `parent` (известная папка,
    /// `msgfolderroot` для корня ящика, или папка по идентификатору). Возвращает идентификатор
    /// созданной папки (`Folders[0].FolderId`). Отказ сервера (например, имя занято:
    /// `ErrorFolderExists`) приходит как `Server`.
    async fn create_folder(&self, parent: &FolderRef, display_name: &str) -> Result<FolderId>;

    /// Переименовать папку (`UpdateFolder` поля `folder:DisplayName`). Возвращает идентификатор с
    /// новым `ChangeKey` (или исходный, если сервер его не вернул).
    async fn rename_folder(&self, id: &FolderId, display_name: &str) -> Result<FolderId>;

    /// Перенести папку вместе с содержимым в другую папку (`MoveFolder`). Возвращает идентификатор
    /// папки после переноса (`Folders[0].FolderId`, у папок он обычно не меняется).
    async fn move_folder(&self, id: &FolderId, parent: &FolderRef) -> Result<FolderId>;

    /// Удалить папку с содержимым (`DeleteFolder`): `permanent` — навсегда (`HardDelete`), иначе
    /// в «Удалённые» (`MoveToDeletedItems`: папка становится подпапкой «Удалённых»).
    async fn delete_folder(&self, id: &FolderId, permanent: bool) -> Result<()>;

    /// Создать черновик в «Черновиках» (`CreateItem` с `MessageDisposition: SaveOnly`).
    /// Для `Reply`/`ReplyAll`/`Forward` сервер сам добавляет цитируемый оригинал (у пересылки —
    /// и его вложения); получатели и тема передаются явно. Возвращает идентификатор черновика.
    async fn create_draft(&self, kind: &ComposeKind, msg: &OutgoingMessage) -> Result<ItemId>;

    /// Приложить файлы к черновику (`CreateAttachment`, по одному запросу на файл). Возвращает
    /// идентификатор черновика с актуальным `ChangeKey` (`RootItemChangeKey` последнего ответа):
    /// именно его нужно передавать в `send_draft`.
    async fn add_attachments(&self, item: &ItemId, files: &[OutgoingAttachment]) -> Result<ItemId>;

    /// Отправить черновик (`SendItem` с копией в «Отправленных»).
    async fn send_draft(&self, item: &ItemId) -> Result<()>;

    /// Перезаписать тему, тело и получателей черновика (`UpdateItem` с `SaveOnly`). Возвращает
    /// идентификатор с новым `ChangeKey`.
    async fn update_draft(&self, item: &ItemId, msg: &OutgoingMessage) -> Result<ItemId>;

    /// Подпись пользователя из настроек OWA (`GetUserConfiguration OWA.UserOptions`): `None`,
    /// если подпись не задана.
    async fn user_signature(&self) -> Result<Option<Signature>>;

    /// Часовой пояс пользователя из настроек OWA (ключ `timezone` словаря `OWA.UserOptions`):
    /// идентификатор Windows, например `Russian Standard Time`; `None`, если не задан. Нужен
    /// записи в календарь (`create_calendar_item`/`update_calendar_item`), чтобы событие легло в
    /// пояс пользователя, как в браузерной OWA.
    async fn user_time_zone(&self) -> Result<Option<String>>;

    /// События календаря в окне `[start, end)` (`FindItem` с `CalendarView` по папке `calendar`):
    /// повторяющиеся встречи приходят развёрнутыми в экземпляры. Моменты в UTC. Если сервер
    /// режет выдачу, реализация догружает остаток сама; порядок — по началу.
    async fn find_calendar_view(&self, start: Timestamp, end: Timestamp) -> Result<Vec<CalendarEvent>>;

    /// Событие целиком (`GetItem`): тело HTML, участники с ответами, напоминание, повторение.
    /// Отказ сервера по элементу (например, `ErrorItemNotFound`) приходит как `Server`.
    async fn get_calendar_item(&self, id: &ItemId) -> Result<CalendarEventDetails>;

    /// Создать событие в папке `calendar` (`CreateItem` с `CalendarItem`): при участниках сервер
    /// рассылает приглашения (`SendMeetingInvitations: SendToAllAndSaveCopy`), без них —
    /// `SendToNone`. `time_zone` — пояс пользователя (Windows-идентификатор из `user_time_zone`):
    /// запрос уходит в контексте этого пояса, и границы «весь день» — местными полуночами дат;
    /// без пояса контекст — UTC, и событие ляжет в пояс UTC. Возвращает идентификатор созданного
    /// события (с `ChangeKey`).
    async fn create_calendar_item(&self, item: &NewCalendarItem, time_zone: Option<&str>) -> Result<ItemId>;

    /// Изменить только заполненные поля события (`UpdateItem` с `SetItemField`/`DeleteItemField`);
    /// для экземпляра повторяющейся встречи правится именно экземпляр (его `ItemId`).
    /// `send_updates` — разослать участникам обновление; `time_zone` — как у
    /// `create_calendar_item` (границы «весь день» идут местными полуночами, когда в `changes`
    /// `is_all_day: Some(true)`). Возвращает идентификатор с новым `ChangeKey`; пустые `changes` —
    /// без запроса, прежний идентификатор.
    async fn update_calendar_item(&self, id: &ItemId, changes: &CalendarItemChanges, send_updates: bool, time_zone: Option<&str>) -> Result<ItemId>;

    /// Удалить событие в «Удалённые» (`DeleteItem` с `MoveToDeletedItems`); `send_cancellations` —
    /// разослать участникам отмену (`SendMeetingCancellations`).
    async fn delete_calendar_item(&self, id: &ItemId, send_cancellations: bool) -> Result<()>;

    /// Ответить на приглашение (`CreateItem` с `AcceptItem`/`TentativelyAcceptItem`/`DeclineItem`
    /// по `ReferenceItemId`): и для письма-приглашения, и для события календаря. `send_response`
    /// — отправить ответ организатору (`SendAndSaveCopy`), иначе `SaveOnly`; `body_html` — текст
    /// ответа.
    async fn respond_to_meeting(&self, reference: &ItemId, response: MeetingResponse, send_response: bool, body_html: Option<&str>) -> Result<()>;

    /// Письмо о встрече (`GetItem` по письму-приглашению, отмене или ответу) с полями встречи:
    /// вид по `ItemClass`, время и место, организатор, свой ответ, устарело ли, связанное событие.
    async fn get_meeting_request(&self, id: &ItemId) -> Result<MeetingRequestInfo>;

    /// Подписаться на уведомления (`SubscribeToNotification`). Результат той же длины и порядка,
    /// что `subs`; отказ по одной подписке — `created: false` с текстом, `Err` — отказ вызова.
    /// Подписки живут в рамках серверной сессии: после повторного входа их нужно создать заново.
    async fn subscribe_notifications(&self, subs: &[SubscriptionRequest]) -> Result<Vec<SubscriptionResult>>;

    /// Один цикл длинного опроса (`PendingNotificationRequest`): читает поток до закрытия
    /// сервером (около 5–6 минут) или до ошибки, на каждый разобранный блок зовёт `sink`.
    /// Истёкшая сессия восстанавливается одним повторным входом, как у остальных вызовов;
    /// подписки при этом теряются, и в `sink` приходит `Reload`. Тишина дольше двух с половиной
    /// минут (сервер шлёт keep-alive каждые 40 с) — `Network`: так мёртвое соединение после сна
    /// или смены сети замечается раньше, чем истёк бы таймер окна. Поток не занимает
    /// разрешение семафора: синхронизация и команды окна идут
    /// параллельно с открытым потоком.
    async fn notification_stream(&self, cid: &str, sink: &(dyn Fn(NotificationEvent) + Send + Sync)) -> Result<StreamEnd>;

    /// Завершить цикл опроса (`FinishNotificationRequest`): вызывается после конца потока и
    /// перед остановкой канала.
    async fn finish_notification_stream(&self, cid: &str) -> Result<()>;

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
        assert!(TransportError::Protocol { op: "GetItem".into(), status: 503, raw: String::new() }.is_retryable());
        assert!(!TransportError::Protocol { op: "GetItem".into(), status: 500, raw: String::new() }.is_retryable());
    }

    #[test]
    fn item_failure_is_only_final_server_error() {
        assert!(TransportError::Server { code: "ErrorInternalServerError".into(), message: String::new() }.is_item_failure());
        assert!(!TransportError::Server { code: "ErrorServerBusy".into(), message: String::new() }.is_item_failure(), "временный код сервера — не отказ по элементу");
        assert!(!TransportError::AuthFailed("x".into()).is_item_failure());
        assert!(!TransportError::SessionExpired.is_item_failure());
        assert!(!TransportError::Protocol { op: "GetItem".into(), status: 500, raw: String::new() }.is_item_failure());
        assert!(!TransportError::Cancelled.is_item_failure());
    }

    #[test]
    fn guid_shape() {
        let g = new_guid();
        assert_eq!(g.len(), 36, "{g}");
        assert_eq!(g.matches('-').count(), 4);
        assert!(g.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(g, new_guid());
        assert_eq!(NotificationKind::NewMail.name(), "NewMailNotification");
        assert_eq!(NotificationEvent::KeepAlive.kind_name(), "KeepAlive");
    }

    #[test]
    fn credentials_hide_password() {
        let c = Credentials { host: "h".into(), user: "u".into(), password: SecretString::from("p".to_string()) };
        assert!(!format!("{c:?}").contains('p') || format!("{c:?}").contains("***"));
    }
}
