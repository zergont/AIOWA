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
