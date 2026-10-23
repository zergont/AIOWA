//! Доменные типы AIOWA. Без ввода-вывода, без зависимости от протокола и базы.
//!
//! Идентификаторы сервера (`FolderId`, `ItemId`, `ConversationId`) непрозрачны:
//! клиент их не разбирает и не сравнивает иначе как на равенство. Локальные
//! суррогатные ключи (`folder_pk`, `item_pk`) живут только в хранилище.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Учётная запись (ящик). В v1 всегда одна, но в схеме с первого дня.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub i64);

/// Идентификатор папки на сервере.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FolderId {
    pub id: String,
    pub change_key: Option<String>,
}

/// Идентификатор элемента (письма, события) на сервере.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId {
    pub id: String,
    pub change_key: Option<String>,
}

/// Идентификатор беседы.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub String);

/// Непрозрачное состояние синхронизации (`SyncState` EWS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState(pub String);

/// Известные папки Exchange. `as_str` даёт значение для протокола.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DistinguishedFolder {
    Inbox,
    SentItems,
    Drafts,
    DeletedItems,
    JunkEmail,
    Outbox,
    Archive,
    Calendar,
    Contacts,
    /// Корень почтовых папок (`msgfolderroot`), от него идёт `SyncFolderHierarchy`.
    MsgFolderRoot,
}

impl DistinguishedFolder {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::SentItems => "sentitems",
            Self::Drafts => "drafts",
            Self::DeletedItems => "deleteditems",
            Self::JunkEmail => "junkemail",
            Self::Outbox => "outbox",
            Self::Archive => "archive",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::MsgFolderRoot => "msgfolderroot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => Self::Inbox,
            "sentitems" => Self::SentItems,
            "drafts" => Self::Drafts,
            "deleteditems" => Self::DeletedItems,
            "junkemail" => Self::JunkEmail,
            "outbox" => Self::Outbox,
            "archive" => Self::Archive,
            "calendar" => Self::Calendar,
            "contacts" => Self::Contacts,
            "msgfolderroot" => Self::MsgFolderRoot,
            _ => return None,
        })
    }
}

impl fmt::Display for DistinguishedFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ссылка на папку: известная или по идентификатору.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FolderRef {
    Distinguished(DistinguishedFolder),
    Id(FolderId),
}

impl From<DistinguishedFolder> for FolderRef {
    fn from(d: DistinguishedFolder) -> Self {
        FolderRef::Distinguished(d)
    }
}

impl From<FolderId> for FolderRef {
    fn from(id: FolderId) -> Self {
        FolderRef::Id(id)
    }
}

/// Папка как её отдаёт сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub parent_id: Option<FolderId>,
    pub display_name: String,
    /// Известное имя, если сервер его сообщил (`inbox`, `sentitems`, ...).
    pub distinguished: Option<String>,
    /// Класс папки, например `IPF.Note`, `IPF.Appointment`.
    pub folder_class: Option<String>,
    pub total_count: i64,
    pub unread_count: i64,
    pub child_folder_count: i64,
}

impl Folder {
    /// Почтовая ли папка (письма, а не календарь, контакты и т. п.).
    pub fn is_mail(&self) -> bool {
        match &self.folder_class {
            Some(c) => c == "IPF.Note" || c.starts_with("IPF.Note."),
            None => true,
        }
    }
}

/// Участник переписки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Participant {
    pub name: Option<String>,
    pub email: Option<String>,
    /// `SMTP`, `EX` (устаревший адрес Exchange) и т. п.
    pub routing_type: Option<String>,
}

/// Тип получателя.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecipientKind {
    To = 1,
    Cc = 2,
    Bcc = 3,
}

/// Важность письма.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

impl Importance {
    pub fn parse(s: &str) -> Self {
        match s {
            "Low" => Self::Low,
            "High" => Self::High,
            _ => Self::Normal,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::High => "High",
        }
    }
}

/// Состояние флага «к исполнению».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FlagStatus {
    #[default]
    NotFlagged,
    Flagged,
    Complete,
}

impl FlagStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "Flagged" => Self::Flagged,
            "Complete" => Self::Complete,
            _ => Self::NotFlagged,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFlagged => "NotFlagged",
            Self::Flagged => "Flagged",
            Self::Complete => "Complete",
        }
    }
}

/// Метаданные письма: то, что нужно списку и индексу без тела.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub item_id: ItemId,
    pub parent_folder_id: Option<FolderId>,
    pub conversation_id: Option<ConversationId>,
    pub internet_message_id: Option<String>,
    /// `IPM.Note`, `IPM.Schedule.Meeting.Request`, ...
    pub item_class: Option<String>,
    pub subject: Option<String>,
    pub from: Option<Participant>,
    pub sender: Option<Participant>,
    /// Готовая строка получателей для списка (как `DisplayTo` у сервера).
    pub display_to: Option<String>,
    pub received_at: Option<Timestamp>,
    pub sent_at: Option<Timestamp>,
    pub last_modified_at: Option<Timestamp>,
    pub is_read: bool,
    pub is_draft: bool,
    pub has_attachments: bool,
    pub importance: Importance,
    pub flag: FlagStatus,
    pub size: Option<i64>,
    pub preview: Option<String>,
}

/// Тип тела.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BodyType {
    Html,
    Text,
}

impl BodyType {
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("text") { Self::Text } else { Self::Html }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Html => "HTML",
            Self::Text => "Text",
        }
    }
}

/// Тело письма.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    pub body_type: BodyType,
    /// Текст письма без цитируемой истории (`UniqueBody`).
    pub unique_body: Option<String>,
    /// Полное тело, если запрашивалось.
    pub full_body: Option<String>,
    pub is_truncated: bool,
}

/// Метаданные вложения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub is_inline: bool,
    pub content_id: Option<String>,
}

/// Письмо целиком: метаданные, получатели, тело, вложения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageFull {
    pub meta: MessageMeta,
    pub to: Vec<Participant>,
    pub cc: Vec<Participant>,
    pub bcc: Vec<Participant>,
    pub body: Option<MessageBody>,
    pub attachments: Vec<AttachmentMeta>,
}

/// Файл, прикладываемый к исходящему письму (содержимое целиком в памяти: лимиты
/// на размер проверяет вызывающий, транспорт кодирует в base64).
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub name: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for OutgoingAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingAttachment").field("name", &self.name).field("content_type", &self.content_type).field("bytes", &self.bytes.len()).finish()
    }
}

/// Исходящее письмо: получатели, тема, тело HTML, важность. Для ответа и пересылки
/// это только новая часть: цитату оригинала (и вложения у пересылки) добавляет сервер.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub to: Vec<Participant>,
    pub cc: Vec<Participant>,
    pub bcc: Vec<Participant>,
    pub subject: String,
    pub body_html: String,
    pub importance: Importance,
}

/// Вид создаваемого письма. У ответов и пересылки `reference` — письмо-оригинал на сервере.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComposeKind {
    New,
    Reply { reference: ItemId },
    ReplyAll { reference: ItemId },
    Forward { reference: ItemId },
}

impl ComposeKind {
    /// Письмо-оригинал, если вид его предполагает.
    pub fn reference(&self) -> Option<&ItemId> {
        match self {
            Self::New => None,
            Self::Reply { reference } | Self::ReplyAll { reference } | Self::Forward { reference } => Some(reference),
        }
    }
}

/// Подпись пользователя: HTML и признак «добавлять автоматически к новым письмам».
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Signature {
    pub html: String,
    pub auto_add: bool,
}

// ---------------------------------------------------------------------------
// Календарь

/// Занятость по событию (`calendar:LegacyFreeBusyStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FreeBusy {
    Free,
    Tentative,
    #[default]
    Busy,
    Oof,
    WorkingElsewhere,
    NoData,
}

impl FreeBusy {
    /// Разбирает и имя EWS (`Busy`, `OOF`), и ключ интерфейса (`busy`, `oof`); неизвестное — `NoData`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "free" => Self::Free,
            "tentative" => Self::Tentative,
            "busy" => Self::Busy,
            "oof" => Self::Oof,
            "workingelsewhere" | "working_elsewhere" => Self::WorkingElsewhere,
            _ => Self::NoData,
        }
    }
    /// Имя значения в схеме EWS (так оно хранится в базе).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Tentative => "Tentative",
            Self::Busy => "Busy",
            Self::Oof => "OOF",
            Self::WorkingElsewhere => "WorkingElsewhere",
            Self::NoData => "NoData",
        }
    }
    /// Ключ для интерфейса: `free`, `tentative`, `busy`, `oof`, `working_elsewhere`, `no_data`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Tentative => "tentative",
            Self::Busy => "busy",
            Self::Oof => "oof",
            Self::WorkingElsewhere => "working_elsewhere",
            Self::NoData => "no_data",
        }
    }
}

/// Вид элемента календаря (`calendar:CalendarItemType`): одиночное событие, экземпляр или
/// исключение повторяющейся встречи, сама повторяющаяся встреча.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CalendarItemType {
    #[default]
    Single,
    Occurrence,
    Exception,
    RecurringMaster,
}

impl CalendarItemType {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "occurrence" => Self::Occurrence,
            "exception" => Self::Exception,
            "recurringmaster" | "recurring_master" => Self::RecurringMaster,
            _ => Self::Single,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "Single",
            Self::Occurrence => "Occurrence",
            Self::Exception => "Exception",
            Self::RecurringMaster => "RecurringMaster",
        }
    }
    /// Ключ для интерфейса: `single`, `occurrence`, `exception`, `recurring_master`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Occurrence => "occurrence",
            Self::Exception => "exception",
            Self::RecurringMaster => "recurring_master",
        }
    }
    /// Экземпляр, исключение или сама повторяющаяся встреча.
    pub const fn is_recurring(self) -> bool {
        !matches!(self, Self::Single)
    }
}

/// Ответ на приглашение (`calendar:MyResponseType`, `ResponseType` у участника).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ResponseType {
    #[default]
    Unknown,
    Organizer,
    Tentative,
    Accept,
    Decline,
    NoResponseReceived,
}

impl ResponseType {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "organizer" => Self::Organizer,
            "tentative" => Self::Tentative,
            "accept" => Self::Accept,
            "decline" => Self::Decline,
            "noresponsereceived" | "no_response" => Self::NoResponseReceived,
            _ => Self::Unknown,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Organizer => "Organizer",
            Self::Tentative => "Tentative",
            Self::Accept => "Accept",
            Self::Decline => "Decline",
            Self::NoResponseReceived => "NoResponseReceived",
        }
    }
    /// Ключ для интерфейса: `unknown`, `organizer`, `tentative`, `accept`, `decline`, `no_response`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Organizer => "organizer",
            Self::Tentative => "tentative",
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::NoResponseReceived => "no_response",
        }
    }
}

/// Событие календаря, как его отдаёт `FindItem` с `CalendarView`: повторяющиеся встречи уже
/// развёрнуты в экземпляры (`Occurrence`/`Exception`), у каждого свой `ItemId`.
/// `start`/`end` — моменты в UTC; у событий на весь день это границы суток в часовом поясе
/// контекста запроса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: ItemId,
    pub subject: Option<String>,
    pub start: Timestamp,
    pub end: Timestamp,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub organizer: Option<Participant>,
    pub free_busy: FreeBusy,
    pub calendar_type: CalendarItemType,
    pub is_meeting: bool,
    pub is_cancelled: bool,
    pub my_response: ResponseType,
    /// `Normal`, `Personal`, `Private`, `Confidential`.
    pub sensitivity: Option<String>,
    pub has_attachments: bool,
    pub last_modified: Option<Timestamp>,
}

/// Участник встречи с его ответом на приглашение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub participant: Participant,
    pub response: ResponseType,
    /// Обязательный (`RequiredAttendees`) или необязательный (`OptionalAttendees`).
    pub required: bool,
}

/// Событие целиком (`GetItem`): тело, участники, напоминание, описание повторения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEventDetails {
    pub event: CalendarEvent,
    pub body: Option<MessageBody>,
    pub attendees: Vec<Attendee>,
    /// Минут до начала, если напоминание включено.
    pub reminder_minutes: Option<i64>,
    /// Человекочитаемое описание повторения (собирается клиентом из структуры `Recurrence`).
    pub recurrence_text: Option<String>,
}

/// Изменение элемента, как его отдаёт `SyncFolderItems`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemChange {
    /// Новый элемент; метаданные присутствуют, если запрашивались.
    Create(MessageMeta),
    /// Изменённый элемент.
    Update(MessageMeta),
    /// Удалён или перемещён из папки.
    Delete(ItemId),
    /// Изменился только признак прочитанности.
    ReadFlag { item_id: ItemId, is_read: bool },
}

/// Результат одного вызова `SyncFolderItems`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemChanges {
    pub state: SyncState,
    pub changes: Vec<ItemChange>,
    /// Сервер сообщил, что изменений больше нет (или выдача короче запрошенного).
    pub includes_last: bool,
}

/// Изменение папки из `SyncFolderHierarchy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FolderChange {
    Create(Folder),
    Update(Folder),
    Delete(FolderId),
}

/// Результат `SyncFolderHierarchy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderChanges {
    pub state: SyncState,
    pub changes: Vec<FolderChange>,
    pub includes_last: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguished_roundtrip() {
        for d in [DistinguishedFolder::Inbox, DistinguishedFolder::SentItems, DistinguishedFolder::MsgFolderRoot] {
            assert_eq!(DistinguishedFolder::parse(d.as_str()), Some(d));
        }
        assert_eq!(DistinguishedFolder::parse("nope"), None);
    }

    #[test]
    fn calendar_enums_roundtrip() {
        for fb in [FreeBusy::Free, FreeBusy::Tentative, FreeBusy::Busy, FreeBusy::Oof, FreeBusy::WorkingElsewhere, FreeBusy::NoData] {
            assert_eq!(FreeBusy::parse(fb.as_str()), fb);
            assert_eq!(FreeBusy::parse(fb.key()), fb);
        }
        assert_eq!(FreeBusy::parse("Что-то"), FreeBusy::NoData);
        for t in [CalendarItemType::Single, CalendarItemType::Occurrence, CalendarItemType::Exception, CalendarItemType::RecurringMaster] {
            assert_eq!(CalendarItemType::parse(t.as_str()), t);
            assert_eq!(CalendarItemType::parse(t.key()), t);
        }
        assert!(!CalendarItemType::Single.is_recurring() && CalendarItemType::Occurrence.is_recurring());
        for r in [ResponseType::Unknown, ResponseType::Organizer, ResponseType::Tentative, ResponseType::Accept, ResponseType::Decline, ResponseType::NoResponseReceived] {
            assert_eq!(ResponseType::parse(r.as_str()), r);
            assert_eq!(ResponseType::parse(r.key()), r);
        }
        assert_eq!(ResponseType::parse(""), ResponseType::Unknown);
    }

    #[test]
    fn mail_folder_classes() {
        let mk = |class: Option<&str>| Folder {
            id: FolderId { id: "x".into(), change_key: None },
            parent_id: None,
            display_name: "f".into(),
            distinguished: None,
            folder_class: class.map(String::from),
            total_count: 0,
            unread_count: 0,
            child_folder_count: 0,
        };
        assert!(mk(Some("IPF.Note")).is_mail());
        assert!(mk(Some("IPF.Note.OutlookHomepage")).is_mail());
        assert!(!mk(Some("IPF.Appointment")).is_mail());
        assert!(mk(None).is_mail());
    }
}
