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

/// Тип получателя. Числа — значения колонки `recipients.kind` в хранилище.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecipientKind {
    To = 1,
    Cc = 2,
    Bcc = 3,
    /// Адрес из заголовка `Reply-To` (`message:ReplyTo`): куда слать ответ вместо `From`.
    ReplyTo = 4,
}

impl RecipientKind {
    pub const fn as_i64(self) -> i64 {
        self as i64
    }
}

/// Символы, запрещённые в имени папки (как в Outlook и OWA).
pub const FOLDER_NAME_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Проверяет имя папки для создания или переименования: 1–255 символов после обрезки пробелов,
/// без управляющих символов и без `\ / : * ? " < > |`. Возвращает обрезанное имя или текст ошибки
/// по-русски для показа пользователю.
pub fn validate_folder_name(name: &str) -> std::result::Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Имя папки не может быть пустым".to_string());
    }
    if trimmed.chars().count() > 255 {
        return Err("Имя папки длиннее 255 символов".to_string());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Имя папки содержит управляющие символы".to_string());
    }
    if let Some(c) = trimmed.chars().find(|c| FOLDER_NAME_FORBIDDEN.contains(c)) {
        return Err(format!("Имя папки не может содержать символ «{c}» (запрещены \\ / : * ? \" < > |)"));
    }
    Ok(trimmed.to_string())
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
    /// Адреса `Reply-To` (`message:ReplyTo`): пусто, если заголовка нет. Ответ идёт сюда,
    /// а не на `From`, если список непуст и отличается от отправителя.
    #[serde(default)]
    pub reply_to: Vec<Participant>,
    pub body: Option<MessageBody>,
    pub attachments: Vec<AttachmentMeta>,
}

impl MessageFull {
    /// Адреса `Reply-To`, отличающиеся от отправителя (`From`, без учёта регистра адреса):
    /// именно их композер ставит в «Кому» у ответа вместо отправителя. Пусто — отвечать `From`.
    pub fn effective_reply_to(&self) -> Vec<Participant> {
        effective_reply_to(self.meta.from.as_ref(), &self.reply_to)
    }
}

/// Адреса `Reply-To`, которые заменяют отправителя в ответе: список непуст и не сводится к самому
/// `from` (сравнение адресов без учёта регистра). Если `Reply-To` — тот же адрес, что `From`,
/// заменять нечего, и возвращается пусто.
pub fn effective_reply_to(from: Option<&Participant>, reply_to: &[Participant]) -> Vec<Participant> {
    let from_email = from.and_then(|p| p.email.as_deref()).map(str::to_ascii_lowercase);
    let differs = reply_to.iter().any(|p| p.email.as_deref().map(str::to_ascii_lowercase) != from_email);
    if differs { reply_to.iter().filter(|p| p.email.is_some() || p.name.is_some()).cloned().collect() } else { Vec::new() }
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
    /// Минут до начала, если напоминание включено (`item:ReminderIsSet` и
    /// `item:ReminderMinutesBeforeStart` запрашиваются и у `FindItem`).
    pub reminder_minutes: Option<i64>,
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

// ---------------------------------------------------------------------------
// Календарь: запись

/// Новое событие для `CreateItem` (`CalendarItem` в папке `calendar`). Моменты в UTC; у события
/// на весь день `start`/`end` — полуночи UTC календарных дат (`all_day_bound_utc`): это кодировка
/// самих дат, не зависящая от пояса машины. Транспорт с поясом пользователя шлёт их как местные
/// полуночи этих дат без зоны (`2026-10-26T00:00:00`) в контексте его пояса; без пояса — как
/// полуночи UTC (тогда сервер хранит событие в поясе UTC). При непустых `required`/`optional`
/// сервер рассылает приглашения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCalendarItem {
    pub subject: String,
    pub start: Timestamp,
    pub end: Timestamp,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub body_html: Option<String>,
    pub required: Vec<Participant>,
    pub optional: Vec<Participant>,
    /// Минут до начала; `None` — без напоминания.
    pub reminder_minutes: Option<i64>,
    pub free_busy: FreeBusy,
    /// Конфиденциальность `Private` вместо `Normal`.
    pub sensitivity_private: bool,
}

impl NewCalendarItem {
    /// Есть ли участники: тогда это встреча, и сервер рассылает приглашения.
    pub fn has_attendees(&self) -> bool {
        !self.required.is_empty() || !self.optional.is_empty()
    }
}

/// Календарная дата границы события «весь день» в поясе `tz`: дата ближайшей к моменту полуночи.
/// Момент может быть полуночью другого пояса (чужие события в кэше, см. § 5g документации:
/// у пояса восточнее местного граница приходится на местный вечер предыдущего дня), поэтому
/// берётся не дата момента, а дата ближайшей полуночи; верно, пока пояса расходятся меньше чем на
/// 12 часов.
pub fn all_day_date(t: Timestamp, tz: &jiff::tz::TimeZone) -> jiff::civil::Date {
    let z = t.to_zoned(tz.clone());
    let d = z.date();
    if z.hour() >= 12 { d.tomorrow().unwrap_or(d) } else { d }
}

/// Граница события «весь день» для транспорта: полночь UTC календарной даты (`all_day_date`)
/// момента `t` в поясе `tz`. Так дата кодируется моментом независимо от пояса машины; транспорт
/// превращает её обратно в дату (`all_day_civil_date`) и шлёт серверу местную полночь пояса
/// пользователя. Местная полночь как момент UTC (`…T21:00:00Z`) в контексте UTC сдвинула бы даты.
pub fn all_day_bound_utc(t: Timestamp, tz: &jiff::tz::TimeZone) -> Timestamp {
    all_day_date(t, tz).to_zoned(jiff::tz::TimeZone::UTC).map(|z| z.timestamp()).unwrap_or(t)
}

/// Календарная дата границы «весь день» из момента, как его хранит `NewCalendarItem` или
/// `CalendarItemChanges`: дата ближайшей полуночи UTC. Точна для полуночей UTC
/// (`all_day_bound_utc`) и терпима к местным полуночам любого пояса в пределах ±12 часов.
pub fn all_day_civil_date(t: Timestamp) -> jiff::civil::Date {
    all_day_date(t, &jiff::tz::TimeZone::UTC)
}

/// Изменения события для `UpdateItem`: `None` — поле не трогать. У `location` и `reminder_minutes`
/// внутреннее `None` снимает значение (место — пустое `DisplayName`, напоминание — `ReminderIsSet:
/// false`); пустой список участников убирает всех. У события на весь день вместе с `start`/`end`
/// должен идти `is_all_day: Some(true)`: по нему транспорт шлёт границы местными полуночами дат
/// (`all_day_civil_date`), а не моментами UTC.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CalendarItemChanges {
    pub subject: Option<String>,
    pub start: Option<Timestamp>,
    pub end: Option<Timestamp>,
    pub is_all_day: Option<bool>,
    pub location: Option<Option<String>>,
    pub body_html: Option<String>,
    pub required: Option<Vec<Participant>>,
    pub optional: Option<Vec<Participant>>,
    pub reminder_minutes: Option<Option<i64>>,
    pub free_busy: Option<FreeBusy>,
}

impl CalendarItemChanges {
    /// Ни одно поле не меняется: запрос к серверу не нужен.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Ответ на приглашение (`AcceptItem`, `TentativelyAcceptItem`, `DeclineItem`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MeetingResponse {
    Accept,
    Tentative,
    Decline,
}

impl MeetingResponse {
    /// Разбирает ключ интерфейса (`accept`, `tentative`, `decline`) без учёта регистра.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "accept" => Some(Self::Accept),
            "tentative" => Some(Self::Tentative),
            "decline" => Some(Self::Decline),
            _ => None,
        }
    }
    pub const fn key(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Tentative => "tentative",
            Self::Decline => "decline",
        }
    }
    /// Каким станет свой ответ (`MyResponseType`) после отправки.
    pub const fn response_type(self) -> ResponseType {
        match self {
            Self::Accept => ResponseType::Accept,
            Self::Tentative => ResponseType::Tentative,
            Self::Decline => ResponseType::Decline,
        }
    }
}

/// Вид письма о встрече по `ItemClass`: приглашение (`IPM.Schedule.Meeting.Request`), отмена
/// (`…Canceled`), ответ участника (`…Resp.Pos`/`Neg`/`Tent`) или другое (`…Notification.Forward`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum MeetingMessageKind {
    Request,
    Cancellation,
    Response,
    #[default]
    Other,
}

impl MeetingMessageKind {
    /// Префикс классов писем о встречах.
    pub const CLASS_PREFIX: &'static str = "IPM.Schedule.Meeting.";

    /// Вид по классу письма (без учёта регистра); не письмо о встрече — `Other`.
    pub fn from_item_class(class: &str) -> Self {
        let lower = class.trim().to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix(&Self::CLASS_PREFIX.to_ascii_lowercase()) else { return Self::Other };
        if rest.starts_with("request") {
            Self::Request
        } else if rest.starts_with("canceled") || rest.starts_with("cancelled") {
            Self::Cancellation
        } else if rest.starts_with("resp.") {
            Self::Response
        } else {
            Self::Other
        }
    }

    /// Письмо о встрече вообще (любого вида, включая `Other` с префиксом класса).
    pub fn is_meeting_class(class: &str) -> bool {
        class.trim().to_ascii_lowercase().starts_with(&Self::CLASS_PREFIX.to_ascii_lowercase())
    }

    /// Ключ для интерфейса: `request`, `cancellation`, `response`, `other`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Cancellation => "cancellation",
            Self::Response => "response",
            Self::Other => "other",
        }
    }
}

/// Ответ участника по классу его письма: `…Resp.Pos` — принял, `…Resp.Neg` — отклонил,
/// `…Resp.Tent` — под вопросом.
pub fn response_from_item_class(class: &str) -> Option<ResponseType> {
    let lower = class.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("ipm.schedule.meeting.resp.")?;
    if rest.starts_with("pos") {
        Some(ResponseType::Accept)
    } else if rest.starts_with("neg") {
        Some(ResponseType::Decline)
    } else if rest.starts_with("tent") {
        Some(ResponseType::Tentative)
    } else {
        None
    }
}

/// Письмо о встрече (`GetItem` по письму-приглашению, отмене или ответу): время и место встречи,
/// организатор, свой ответ, признак устаревшего приглашения, связанное событие календаря.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingRequestInfo {
    pub kind: MeetingMessageKind,
    pub start: Option<Timestamp>,
    pub end: Option<Timestamp>,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub organizer: Option<Participant>,
    /// Свой ответ на приглашение (у ответов участников — `Unknown`).
    pub my_response: ResponseType,
    /// Приглашение устарело: встреча изменена или отменена позже.
    pub is_out_of_date: bool,
    /// Событие в календаре, созданное из этого письма (если сервер его сообщил).
    pub associated_calendar_item: Option<ItemId>,
    pub recurrence_text: Option<String>,
    /// Ответ отправителя у письма-ответа (`Response`): принял, отклонил, под вопросом.
    pub response_type_of_sender: Option<ResponseType>,
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
    fn meeting_message_kinds_and_responses() {
        assert_eq!(MeetingMessageKind::from_item_class("IPM.Schedule.Meeting.Request"), MeetingMessageKind::Request);
        assert_eq!(MeetingMessageKind::from_item_class("ipm.schedule.meeting.canceled"), MeetingMessageKind::Cancellation);
        assert_eq!(MeetingMessageKind::from_item_class("IPM.Schedule.Meeting.Resp.Pos"), MeetingMessageKind::Response);
        assert_eq!(MeetingMessageKind::from_item_class("IPM.Schedule.Meeting.Notification.Forward"), MeetingMessageKind::Other);
        assert_eq!(MeetingMessageKind::from_item_class("IPM.Note"), MeetingMessageKind::Other);
        assert!(MeetingMessageKind::is_meeting_class("IPM.Schedule.Meeting.Notification.Forward") && !MeetingMessageKind::is_meeting_class("IPM.Note"));
        assert_eq!(MeetingMessageKind::Cancellation.key(), "cancellation");
        assert_eq!(response_from_item_class("IPM.Schedule.Meeting.Resp.Pos"), Some(ResponseType::Accept));
        assert_eq!(response_from_item_class("IPM.Schedule.Meeting.Resp.Neg"), Some(ResponseType::Decline));
        assert_eq!(response_from_item_class("IPM.Schedule.Meeting.Resp.Tent"), Some(ResponseType::Tentative));
        assert_eq!(response_from_item_class("IPM.Schedule.Meeting.Request"), None);
        for r in [MeetingResponse::Accept, MeetingResponse::Tentative, MeetingResponse::Decline] {
            assert_eq!(MeetingResponse::parse(r.key()), Some(r));
            assert_eq!(MeetingResponse::parse(&r.key().to_ascii_uppercase()), Some(r));
        }
        assert_eq!(MeetingResponse::parse("maybe"), None);
        assert_eq!(MeetingResponse::Tentative.response_type(), ResponseType::Tentative);
        assert!(CalendarItemChanges::default().is_empty());
        assert!(!CalendarItemChanges { location: Some(None), ..Default::default() }.is_empty());
        let item = NewCalendarItem {
            subject: "x".into(),
            start: Timestamp::UNIX_EPOCH,
            end: Timestamp::UNIX_EPOCH,
            is_all_day: false,
            location: None,
            body_html: None,
            required: vec![],
            optional: vec![Participant { name: None, email: Some("o@x.ru".into()), routing_type: None }],
            reminder_minutes: None,
            free_busy: FreeBusy::Busy,
            sensitivity_private: false,
        };
        assert!(item.has_attendees());
        assert!(!NewCalendarItem { optional: vec![], ..item }.has_attendees());
    }

    #[test]
    fn all_day_bounds_follow_nearest_local_midnight() {
        let msk = jiff::tz::TimeZone::fixed(jiff::tz::Offset::constant(3));
        let ts = |s: &str| s.parse::<Timestamp>().unwrap();
        // Местная полночь 26.10 по МСК — 25.10 21:00Z: дата 26.10, на сервер — 26.10 00:00Z.
        assert_eq!(all_day_date(ts("2026-10-25T21:00:00Z"), &msk), jiff::civil::date(2026, 10, 26));
        assert_eq!(all_day_bound_utc(ts("2026-10-25T21:00:00Z"), &msk), ts("2026-10-26T00:00:00Z"));
        // Полночь организатора из UTC+6 (25.10 18:00Z) по МСК — вечер 25.10: ближайшая полночь — 26.10.
        assert_eq!(all_day_date(ts("2026-10-25T18:00:00Z"), &msk), jiff::civil::date(2026, 10, 26));
        // Полночь организатора из UTC−5 (26.10 05:00Z) по МСК — утро 26.10: то же 26.10.
        assert_eq!(all_day_date(ts("2026-10-26T05:00:00Z"), &msk), jiff::civil::date(2026, 10, 26));
        // Полночь UTC уже в нужном виде: не меняется.
        assert_eq!(all_day_bound_utc(ts("2026-10-26T00:00:00Z"), &msk), ts("2026-10-26T00:00:00Z"));
        assert_eq!(all_day_bound_utc(ts("2026-10-26T00:00:00Z"), &jiff::tz::TimeZone::UTC), ts("2026-10-26T00:00:00Z"));
        // Обратно в дату: полночь UTC — точно, местные полуночи МСК и UTC−5 — по ближайшей полуночи.
        assert_eq!(all_day_civil_date(ts("2026-10-26T00:00:00Z")), jiff::civil::date(2026, 10, 26));
        assert_eq!(all_day_civil_date(ts("2026-10-25T21:00:00Z")), jiff::civil::date(2026, 10, 26));
        assert_eq!(all_day_civil_date(ts("2026-10-26T05:00:00Z")), jiff::civil::date(2026, 10, 26));
    }

    #[test]
    fn folder_name_validation() {
        assert_eq!(validate_folder_name("  AIOWA тест  ").unwrap(), "AIOWA тест");
        assert!(validate_folder_name("").is_err());
        assert!(validate_folder_name("   ").is_err());
        assert!(validate_folder_name(&"я".repeat(256)).is_err());
        assert_eq!(validate_folder_name(&"я".repeat(255)).unwrap().chars().count(), 255);
        assert!(validate_folder_name("а\tб").is_err());
        for bad in ["a/b", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b"] {
            let e = validate_folder_name(bad).unwrap_err();
            assert!(e.contains("не может содержать"), "{bad}: {e}");
        }
        assert_eq!(RecipientKind::ReplyTo.as_i64(), 4);
        assert_eq!(RecipientKind::To.as_i64(), 1);
    }

    #[test]
    fn reply_to_replaces_sender_only_when_different() {
        let p = |e: &str| Participant { name: None, email: Some(e.into()), routing_type: None };
        let from = p("list@x.ru");
        assert!(effective_reply_to(Some(&from), &[]).is_empty(), "нет Reply-To — отвечать отправителю");
        assert!(effective_reply_to(Some(&from), &[p("LIST@x.ru")]).is_empty(), "тот же адрес в другом регистре — заменять нечего");
        assert_eq!(effective_reply_to(Some(&from), &[p("owner@x.ru")]), vec![p("owner@x.ru")]);
        assert_eq!(effective_reply_to(Some(&from), &[p("list@x.ru"), p("owner@x.ru")]).len(), 2, "хотя бы один другой адрес — весь список");
        assert_eq!(effective_reply_to(None, &[p("owner@x.ru")]), vec![p("owner@x.ru")]);
        let empty = Participant::default();
        assert!(effective_reply_to(Some(&from), std::slice::from_ref(&empty)).is_empty(), "участник без имени и адреса отбрасывается");
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
