//! Команды записи календаря: создать, изменить и удалить событие, ответить на приглашение (по
//! событию календаря и по письму-приглашению), сведения о письме о встрече.
//!
//! Порядок одной команды: проверки запроса без сети (конец позже начала, адреса участников,
//! очистка HTML описания) → сервер через транспорт процесса (`SyncState::transport`, без новых
//! входов, если сессия уже есть) под таймаутом → кэш календаря (`upsert_calendar_event`,
//! `store_calendar_details`, `remove_calendar_event`), чтобы окно не ждало перечитывания окна
//! → DTO события из свежего кэша. После `CreateItem`/`UpdateItem` событие перечитывается
//! `GetItem`; если перечитать не удалось (сервер запись уже выполнил), в кэш ложится то, что
//! известно из запроса, а следующее обновление окна сверит его с сервером. Правка шлёт только
//! изменившиеся поля (`changes_for`): сравнение с кэшем по теме, времени (у «весь день» — по
//! календарным датам), «весь день», месту, занятости, напоминанию, участникам (по адресам) и
//! описанию (с очищенным телом из кэша). Участники и описание сравниваются с кэшем только когда
//! окно их видело (`EventRequest::details_known`): редактор, открытый без подробностей, не должен
//! снести участников пустым списком. Границы «весь день» кодируются полуночами UTC календарных
//! дат (`aiowa_core::all_day_bound_utc`); запись идёт в контексте пояса пользователя из настроек
//! OWA (`owa_time_zone`: кэш в `sync_state` на сутки, без него или по истечении — `GetUserConfiguration`), и
//! транспорт шлёт эти даты местными полуночами без зоны, как браузерная OWA, — событие ложится в
//! пояс пользователя. Без пояса запись идёт в контексте UTC.
//!
//! Один замок на все операции записи календаря плюс замок события, общий с чтением подробностей
//! (`calendar.rs`). Удаление и ответ по письму шлют окну `calendar-changed`: сетка календаря могла
//! быть загружена раньше и держит событие в памяти. Письмо о встрече (`meeting_info`) — `GetItem`
//! по письму с кэшем в памяти процесса по `item_pk` и `ChangeKey`; ответ по письму
//! (`meeting_respond`) после успеха шлёт окну `items-changed` и `calendar-changed` и просит
//! синхронизацию папки письма: сервер обычно убирает приглашение из «Входящих» после ответа.
//! Мёртвая сессия сбрасывает транспорт через `SyncState::invalidate_on`. В журнал — только
//! счётчики и коды, без тем, мест и адресов.

use crate::calendar::{CalendarEventDto, CalendarState, ParticipantDto, ITEM_NOT_FOUND};
use crate::ondemand::{describe_transport, is_stale_object, timed_out, KeyedLocks};
use crate::reminders::RemindersState;
use crate::sync::SyncState;
use crate::{err, sanitize_html, AppState, CmdResult, ComposeRecipientDto, InlineMap};
use aiowa_core::{all_day_bound_utc, all_day_date, AccountId, Attendee, CalendarEvent, CalendarEventDetails, CalendarItemChanges, CalendarItemType, FreeBusy, ItemId, MeetingRequestInfo, MeetingResponse, NewCalendarItem, Participant, ResponseType};
use aiowa_store::{CalendarRow, Store};
use aiowa_transport::{MailTransport, TransportError, WriteResult};
use aiowa_transport_owa::OwaTransport;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{info, warn};

/// Таймаут одного запроса к серверу.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Самое долгое напоминание, которое принимает запрос: четыре недели (как в Outlook).
pub(crate) const MAX_REMINDER_MINUTES: i64 = 4 * 7 * 24 * 60;
/// Самое длинное событие: год (защита от опечатки в дате).
const MAX_DURATION_MS: i64 = 366 * 24 * 3600 * 1000;
/// Имя события окну «календарь изменился» (то же шлёт канал уведомлений).
const CALENDAR_CHANGED: &str = "calendar-changed";

/// Состояние команд записи календаря: один замок на все операции, кэш сведений о письмах о
/// встречах и замки по письму (повторный запрос того же письма ждёт первый).
#[derive(Default)]
pub(crate) struct CalendarWriteState {
    lock: tokio::sync::Mutex<()>,
    meetings: Mutex<HashMap<i64, MeetingCacheEntry>>,
    meeting_locks: KeyedLocks<i64>,
}

/// Сведения о письме о встрече на момент `ChangeKey` письма; `linked_tried` — связанное событие
/// уже пытались достать с сервера (повторно не ходим).
struct MeetingCacheEntry {
    change_key: Option<String>,
    info: MeetingRequestInfo,
    linked_tried: bool,
}

impl CalendarWriteState {
    fn meetings(&self) -> std::sync::MutexGuard<'_, HashMap<i64, MeetingCacheEntry>> {
        self.meetings.lock().unwrap_or_else(|p| p.into_inner())
    }
}

// --- DTO ------------------------------------------------------------------------------------

/// Запрос окна на создание или правку события. Моменты — миллисекунды UTC; у «весь день» —
/// местные полуночи первого дня и дня после последнего (на сервер уходят полуночи UTC тех же
/// дат). При правке окно шлёт все поля, а на сервер уходят только изменившиеся.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EventRequest {
    /// Окно видело участников и описание события (редактор открыт с подробностями): пустые
    /// `required`/`optional`/`body_html` тогда значат «убрать». Без этого признака они значат
    /// «не знаю» и на сервер не уходят — иначе правка темы, начатая до загрузки подробностей,
    /// сняла бы участников и описание.
    #[serde(default)]
    details_known: bool,
    #[serde(default)]
    subject: String,
    start_ms: i64,
    end_ms: i64,
    #[serde(default)]
    is_all_day: bool,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    required: Vec<ComposeRecipientDto>,
    #[serde(default)]
    optional: Vec<ComposeRecipientDto>,
    /// За сколько минут напоминать; `None` — без напоминания.
    #[serde(default)]
    reminder_minutes: Option<i64>,
    /// `free`, `tentative`, `busy`, `oof`, `working_elsewhere`; нет — `busy`.
    #[serde(default)]
    free_busy: Option<String>,
    #[serde(default)]
    is_private: bool,
}

/// Ответ `meeting_info`: письмо о встрече с полями встречи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MeetingInfoDto {
    /// `request`, `cancellation`, `response`, `other`.
    kind: &'static str,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    is_all_day: bool,
    location: Option<String>,
    organizer: Option<ParticipantDto>,
    /// Свой ответ: как `my_response` у события.
    my_response: &'static str,
    /// Приглашение устарело: встреча изменена или отменена позже.
    is_out_of_date: bool,
    /// Сервер знает событие в календаре, созданное из этого письма.
    has_calendar_item: bool,
    recurrence_text: Option<String>,
    /// У ответа участника — что он ответил.
    sender_response: Option<&'static str>,
    /// Связанное событие в кэше календаря (для «Удалить из календаря»), если нашлось.
    calendar_pk: Option<i64>,
}

fn meeting_dto(info: &MeetingRequestInfo, calendar_pk: Option<i64>) -> MeetingInfoDto {
    MeetingInfoDto {
        kind: info.kind.key(),
        start_ms: info.start.map(|t| t.as_millisecond()),
        end_ms: info.end.map(|t| t.as_millisecond()),
        is_all_day: info.is_all_day,
        location: info.location.clone(),
        organizer: info.organizer.clone().map(ParticipantDto::from),
        my_response: info.my_response.key(),
        is_out_of_date: info.is_out_of_date,
        has_calendar_item: info.associated_calendar_item.is_some() || calendar_pk.is_some(),
        recurrence_text: info.recurrence_text.clone(),
        sender_response: info.response_type_of_sender.map(ResponseType::key),
        calendar_pk,
    }
}

// --- проверки без сети -----------------------------------------------------------------------

/// Участники запроса в участников события: адрес проверен и обрезан, имя обрезано (пустое и
/// равное адресу — отбрасывается). Повторы по адресу (без учёта регистра) и адреса, уже
/// попавшие в `seen`, пропускаются: один человек не бывает и обязательным, и необязательным.
fn participants(rs: &[ComposeRecipientDto], seen: &mut HashSet<String>) -> CmdResult<Vec<Participant>> {
    let mut out = Vec::with_capacity(rs.len());
    for r in rs {
        let email = r.email.trim();
        if !crate::compose::looks_like_email(email) {
            return Err(format!("Адрес «{email}» не похож на почтовый"));
        }
        if !seen.insert(email.to_lowercase()) {
            continue;
        }
        let name = r.name.as_deref().map(str::trim).filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(email)).map(String::from);
        out.push(Participant { name, email: Some(email.to_string()), routing_type: Some("SMTP".into()) });
    }
    Ok(out)
}

/// HTML без видимого текста: только теги, пробелы и `&nbsp;`.
pub(crate) fn html_is_blank(html: &str) -> bool {
    let mut in_tag = false;
    let mut text = String::new();
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ").replace('\u{a0}', " ").trim().is_empty()
}

/// Очищенное описание: `None`, если текста нет.
fn clean_body(html: Option<&str>) -> Option<String> {
    let html = html?;
    let cleaned = sanitize_html(html, &Arc::new(InlineMap::new()));
    (!html_is_blank(&cleaned)).then_some(cleaned)
}

/// Событие для транспорта из запроса окна: проверки (даты, конец позже начала, напоминание,
/// занятость, адреса), обрезка строк, очистка описания той же ammonia, что и у писем. Границы
/// «весь день» — местные полуночи из окна — переводятся в полуночи UTC тех же календарных дат
/// (`all_day_bound_utc`): так даты кодируются моментами, а транспорт шлёт их местными полуночами
/// пояса пользователя.
pub(crate) fn new_item(req: &EventRequest) -> CmdResult<NewCalendarItem> {
    let bad_date = |_| "Дата события вне допустимых значений".to_string();
    let mut start = Timestamp::from_millisecond(req.start_ms).map_err(bad_date)?;
    let mut end = Timestamp::from_millisecond(req.end_ms).map_err(bad_date)?;
    if req.is_all_day {
        let tz = TimeZone::system();
        start = all_day_bound_utc(start, &tz);
        end = all_day_bound_utc(end, &tz);
    }
    if end <= start {
        return Err(if req.is_all_day { "Последний день события раньше первого".into() } else { "Конец события должен быть позже начала".into() });
    }
    if end.as_millisecond() - start.as_millisecond() > MAX_DURATION_MS {
        return Err("Событие длиннее года: проверьте даты".into());
    }
    if let Some(m) = req.reminder_minutes
        && !(0..=MAX_REMINDER_MINUTES).contains(&m)
    {
        return Err(format!("Напоминание — от 0 до {} минут", MAX_REMINDER_MINUTES));
    }
    let free_busy = match req.free_busy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => FreeBusy::Busy,
        Some(s) => match FreeBusy::parse(s) {
            FreeBusy::NoData => return Err("Занятость должна быть одной из: free, tentative, busy, oof, working_elsewhere".into()),
            fb => fb,
        },
    };
    let mut seen = HashSet::new();
    let required = participants(&req.required, &mut seen)?;
    let optional = participants(&req.optional, &mut seen)?;
    Ok(NewCalendarItem {
        subject: req.subject.trim().to_string(),
        start,
        end,
        is_all_day: req.is_all_day,
        location: req.location.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
        body_html: clean_body(req.body_html.as_deref()),
        required,
        optional,
        reminder_minutes: req.reminder_minutes,
        free_busy,
        sensitivity_private: req.is_private,
    })
}

/// Что известно о событии в кэше для сравнения с правкой: строка и, если `GetItem` уже был,
/// подробности — очищенное тело и участники. Без подробностей тело и участники сравнить не с чем:
/// на сервер уходит то, что окно прислало непустым.
pub(crate) struct Known<'a> {
    pub(crate) row: &'a CalendarRow,
    pub(crate) details: Option<(Option<String>, Vec<Attendee>)>,
}

/// Адреса участников в нижнем регистре.
fn emails(ps: &[Participant]) -> HashSet<String> {
    ps.iter().filter_map(|p| p.email.as_deref()).map(|e| e.trim().to_lowercase()).collect()
}

/// Пробелы схлопнуты, у границ тегов убраны: HTML из редактора и из кэша отличаются переносами
/// строк и отступами (для сравнения, не для показа).
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").replace("> ", ">").replace(" <", "<")
}

/// Только изменившиеся поля правки. Время «весь день» сравнивается по календарным датам
/// (`all_day_date`): в кэше границы могут быть полуночами пояса организатора, и правка темы не
/// должна их переписывать. Смена «весь день» уходит вместе с началом и концом (сервер требует
/// их согласованными), а время события на весь день — вместе с `is_all_day: Some(true)`: по
/// нему транспорт шлёт границы местными полуночами дат. Место, снятое в запросе, — `Some(None)`
/// (пустое `DisplayName`); пустое описание при непустом в кэше — пустая строка; участники
/// сравниваются множествами адресов без учёта регистра и порядка. Конфиденциальность правкой не
/// меняется (поля в `CalendarItemChanges` нет; окно при правке флажок блокирует).
pub(crate) fn changes_for(known: &Known<'_>, item: &NewCalendarItem) -> CalendarItemChanges {
    let row = known.row;
    let mut ch = CalendarItemChanges::default();
    if row.subject.as_deref().unwrap_or("").trim() != item.subject {
        ch.subject = Some(item.subject.clone());
    }
    let time_changed = if row.is_all_day && item.is_all_day {
        let tz = TimeZone::system();
        let date = |ms: i64| Timestamp::from_millisecond(ms).ok().map(|t| all_day_date(t, &tz));
        (date(row.start_at), date(row.end_at)) != (Some(all_day_date(item.start, &tz)), Some(all_day_date(item.end, &tz)))
    } else {
        row.start_at != item.start.as_millisecond() || row.end_at != item.end.as_millisecond()
    };
    if time_changed || row.is_all_day != item.is_all_day {
        ch.start = Some(item.start);
        ch.end = Some(item.end);
        if row.is_all_day != item.is_all_day || item.is_all_day {
            ch.is_all_day = Some(item.is_all_day);
        }
    }
    let location_now = row.location.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if location_now != item.location.as_deref() {
        ch.location = Some(item.location.clone());
    }
    if FreeBusy::parse(&row.free_busy) != item.free_busy {
        ch.free_busy = Some(item.free_busy);
    }
    if row.reminder_minutes != item.reminder_minutes {
        ch.reminder_minutes = Some(item.reminder_minutes);
    }
    match &known.details {
        Some((body_now, attendees)) => {
            let body_now = body_now.as_deref().filter(|b| !html_is_blank(b));
            match (body_now, item.body_html.as_deref()) {
                (None, None) => {}
                (Some(a), Some(b)) if squash(a) == squash(b) => {}
                (_, Some(b)) => ch.body_html = Some(b.to_string()),
                (Some(_), None) => ch.body_html = Some(String::new()),
            }
            let (required_now, optional_now): (Vec<&Attendee>, Vec<&Attendee>) = attendees.iter().partition(|a| a.required);
            let set = |list: &[&Attendee]| -> HashSet<String> { list.iter().filter_map(|a| a.participant.email.as_deref()).map(|e| e.trim().to_lowercase()).collect() };
            if emails(&item.required) != set(&required_now) {
                ch.required = Some(item.required.clone());
            }
            if emails(&item.optional) != set(&optional_now) {
                ch.optional = Some(item.optional.clone());
            }
        }
        None => {
            if item.body_html.is_some() {
                ch.body_html = item.body_html.clone();
            }
            if !item.required.is_empty() {
                ch.required = Some(item.required.clone());
            }
            if !item.optional.is_empty() {
                ch.optional = Some(item.optional.clone());
            }
        }
    }
    ch
}

/// Имена изменившихся полей для журнала.
fn changed_fields(ch: &CalendarItemChanges) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ch.subject.is_some() {
        out.push("subject");
    }
    if ch.start.is_some() || ch.end.is_some() {
        out.push("time");
    }
    if ch.is_all_day.is_some() {
        out.push("all_day");
    }
    if ch.location.is_some() {
        out.push("location");
    }
    if ch.body_html.is_some() {
        out.push("body");
    }
    if ch.required.is_some() || ch.optional.is_some() {
        out.push("attendees");
    }
    if ch.reminder_minutes.is_some() {
        out.push("reminder");
    }
    if ch.free_busy.is_some() {
        out.push("free_busy");
    }
    out
}

/// Событие для кэша из запроса, когда перечитать его с сервера не удалось: что не известно из
/// запроса, берётся из прежней строки кэша (`base`); у нового события — ответ «организатор»,
/// одиночный тип. Следующее обновление окна заменит это сервером.
pub(crate) fn event_from_item(id: ItemId, item: &NewCalendarItem, base: Option<&CalendarRow>) -> CalendarEvent {
    let organizer = base.and_then(|r| match (&r.organizer_name, &r.organizer_email) {
        (None, None) => None,
        (name, email) => Some(Participant { name: name.clone(), email: email.clone(), routing_type: None }),
    });
    let sensitivity = match base {
        Some(r) => r.sensitivity.clone(),
        None => Some(if item.sensitivity_private { "Private" } else { "Normal" }.to_string()),
    };
    CalendarEvent {
        id,
        subject: Some(item.subject.clone()).filter(|s| !s.is_empty()),
        start: item.start,
        end: item.end,
        is_all_day: item.is_all_day,
        location: item.location.clone(),
        organizer,
        free_busy: item.free_busy,
        calendar_type: base.map(|r| CalendarItemType::parse(&r.calendar_type)).unwrap_or_default(),
        is_meeting: item.has_attendees() || base.is_some_and(|r| r.is_meeting),
        is_cancelled: base.is_some_and(|r| r.is_cancelled),
        my_response: base.map(|r| ResponseType::parse(&r.my_response)).unwrap_or(ResponseType::Organizer),
        sensitivity,
        has_attachments: base.is_some_and(|r| r.has_attachments),
        last_modified: None,
        reminder_minutes: item.reminder_minutes,
    }
}

/// Строка кэша обратно в событие с новым своим ответом (когда перечитать событие после ответа
/// не удалось). `None` — даты строки не разбираются (в кэше такого не бывает).
pub(crate) fn event_from_row(row: &CalendarRow, my_response: ResponseType) -> Option<CalendarEvent> {
    let organizer = match (&row.organizer_name, &row.organizer_email) {
        (None, None) => None,
        (name, email) => Some(Participant { name: name.clone(), email: email.clone(), routing_type: None }),
    };
    Some(CalendarEvent {
        id: row.id.clone(),
        subject: row.subject.clone(),
        start: Timestamp::from_millisecond(row.start_at).ok()?,
        end: Timestamp::from_millisecond(row.end_at).ok()?,
        is_all_day: row.is_all_day,
        location: row.location.clone(),
        organizer,
        free_busy: FreeBusy::parse(&row.free_busy),
        calendar_type: CalendarItemType::parse(&row.calendar_type),
        is_meeting: row.is_meeting,
        is_cancelled: row.is_cancelled,
        my_response,
        sensitivity: row.sensitivity.clone(),
        has_attachments: row.has_attachments,
        last_modified: None,
        reminder_minutes: row.reminder_minutes,
    })
}

/// Связанное событие письма о встрече в кэше календаря: по `AssociatedCalendarItemId`, а если по
/// нему не нашлось — единственная встреча в кэше с теми же началом, концом и организатором.
pub(crate) fn linked_event(store: &Store, account: AccountId, info: &MeetingRequestInfo) -> Option<i64> {
    if let Some(id) = &info.associated_calendar_item
        && let Ok(Some(row)) = store.calendar_event_by_server_id(account, &id.id)
    {
        return Some(row.calendar_pk);
    }
    let (Some(start), Some(end)) = (info.start, info.end) else { return None };
    let (s, e) = (start.as_millisecond(), end.as_millisecond());
    let rows = store.calendar_events(account, s, e.max(s + 1)).ok()?;
    let organizer = info.organizer.as_ref().and_then(|p| p.email.as_deref()).map(|m| m.trim().to_lowercase());
    let mut hits = rows.iter().filter(|r| r.is_meeting && r.start_at == s && r.end_at == e && (organizer.is_none() || r.organizer_email.as_deref().map(|m| m.trim().to_lowercase()) == organizer));
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(first.calendar_pk)
}

// --- сеть и кэш -----------------------------------------------------------------------------

/// Отказ сетевого шага: элемента нет на сервере (своя обработка у вызывающего) или текст для
/// пользователя.
#[derive(Debug)]
enum Fail {
    NotFound,
    /// Ключ изменений устарел (`ErrorStaleObject`): элемент изменился на сервере.
    Stale,
    Other(String),
}

impl Fail {
    /// Текст для пользователя; `gone` — как назвать отсутствующий элемент.
    fn text(self, gone: &str) -> String {
        match self {
            Fail::NotFound => gone.to_string(),
            Fail::Stale => "Данные на сервере изменились, повторите попытку".to_string(),
            Fail::Other(s) => s,
        }
    }
}

/// Один запрос к серверу под таймаутом: ошибка транспорта сбрасывает мёртвую сессию и
/// превращается в `Fail`.
async fn call<T>(sync: &SyncState, what: &'static str, fut: impl Future<Output = aiowa_transport::Result<T>>) -> Result<T, Fail> {
    match tokio::time::timeout(WRITE_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(TransportError::Server { code, .. })) if code == ITEM_NOT_FOUND => {
            warn!(step = what, "элемента нет на сервере");
            Err(Fail::NotFound)
        }
        Ok(Err(e)) if is_stale_object(&e) => {
            warn!(step = what, "ключ изменений устарел (ErrorStaleObject)");
            Err(Fail::Stale)
        }
        Ok(Err(e)) => {
            warn!(step = what, error = %e, "запись в календарь отклонена");
            sync.invalidate_on(&e).await;
            Err(Fail::Other(describe_transport(&e)))
        }
        Err(_) => {
            warn!(step = what, "запись в календарь не уложилась в таймаут");
            Err(Fail::Other(timed_out(WRITE_TIMEOUT)))
        }
    }
}

/// Текущий идентификатор письма с сервера (`GetItem` только с идентификатором); полный ключ
/// записывается в базу. Письма нет — `Fail::NotFound`.
async fn fresh_item_id(sync: &SyncState, transport: &impl MailTransport, store: &Store, item_pk: i64, stored: &ItemId) -> Result<ItemId, Fail> {
    let mut got = call(sync, "GetItem", transport.refresh_item_ids(std::slice::from_ref(stored))).await?;
    match got.pop() {
        Some(WriteResult::Ok { id }) => {
            info!(item_pk, stored_ck_len = stored.change_key_len(), fresh_ck_len = id.change_key_len(), "ключ письма перечитан");
            if id.has_full_change_key()
                && let Err(e) = store.update_item_id(item_pk, &id)
            {
                warn!(item_pk, error = %e, "перечитанный ключ не записан в базу");
            }
            Ok(id)
        }
        Some(WriteResult::Failed { code, .. }) if code == ITEM_NOT_FOUND => Err(Fail::NotFound),
        Some(WriteResult::Failed { code, message, .. }) => {
            warn!(item_pk, code, message = %message, "сервер не отдал идентификатор письма");
            Err(Fail::Other(format!("Сервер отказал: {code}")))
        }
        None => Err(Fail::Other("Сервер не вернул идентификатор письма".into())),
    }
}

/// Текущий идентификатор события календаря с сервера (`GetItem` только с идентификатором): ключ
/// строки кэша устаревает, когда событие меняется на сервере (ответ по письму, правка
/// организатора), а в кэш свежий ключ ляжет с событием, перечитанным после записи. События нет
/// — `Fail::NotFound`.
async fn fresh_event_id(sync: &SyncState, transport: &impl MailTransport, calendar_pk: i64, stored: &ItemId) -> Result<ItemId, Fail> {
    let mut got = call(sync, "GetItem", transport.refresh_item_ids(std::slice::from_ref(stored))).await?;
    match got.pop() {
        Some(WriteResult::Ok { id }) => {
            info!(calendar_pk, stored_ck_len = stored.change_key_len(), fresh_ck_len = id.change_key_len(), "ключ события перечитан");
            Ok(id)
        }
        Some(WriteResult::Failed { code, .. }) if code == ITEM_NOT_FOUND => Err(Fail::NotFound),
        Some(WriteResult::Failed { code, message, .. }) => {
            warn!(calendar_pk, code, message = %message, "сервер не отдал идентификатор события");
            Err(Fail::Other(format!("Сервер отказал: {code}")))
        }
        None => Err(Fail::Other("Сервер не вернул идентификатор события".into())),
    }
}

/// Ответ на приглашение со ссылкой (`ReferenceItemId`) на письмо или событие с его ключом
/// изменений: ключ в базе бывает усечённым или устаревшим (п. 19 обкатки), поэтому `fresh`
/// перечитывает его с сервера перед ответом; при `ErrorStaleObject` — ещё одна попытка с заново
/// перечитанным ключом.
async fn respond_with_fresh_key<K, R>(mut fresh: impl FnMut() -> K, mut respond: impl FnMut(ItemId) -> R) -> Result<(), Fail>
where
    K: Future<Output = Result<ItemId, Fail>>,
    R: Future<Output = Result<(), Fail>>,
{
    let id = fresh().await?;
    match respond(id).await {
        Err(Fail::Stale) => {
            warn!("ответ на приглашение: ErrorStaleObject после перечитывания ключа, ещё одна попытка");
            let id = fresh().await?;
            respond(id).await
        }
        other => other,
    }
}

fn db_err<E: std::fmt::Display>(e: E) -> String {
    format!("Ошибка локальной базы: {e}")
}

/// Запись в базу в блокирующем потоке (тело события сжимается).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> CmdResult<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?
}

fn row_dto(store: &Store, calendar_pk: i64) -> CmdResult<CalendarEventDto> {
    store.calendar_event(calendar_pk).map_err(err)?.map(CalendarEventDto::from).ok_or_else(|| "Событие исчезло из кэша календаря".to_string())
}

/// Событие с подробностями (`GetItem`) в кэш; DTO свежей строки.
fn store_fresh(store: &Store, account: AccountId, details: &CalendarEventDetails) -> CmdResult<CalendarEventDto> {
    let pk = store.upsert_calendar_event(account, &details.event).map_err(db_err)?;
    store.store_calendar_details(pk, details).map_err(db_err)?;
    row_dto(store, pk)
}

/// Событие без подробностей в кэш (когда сервер его не отдал); DTO свежей строки.
fn store_event(store: &Store, account: AccountId, event: &CalendarEvent) -> CmdResult<CalendarEventDto> {
    let pk = store.upsert_calendar_event(account, event).map_err(db_err)?;
    row_dto(store, pk)
}

/// После записи на сервере: перечитать событие `GetItem` и положить в кэш; если сервер его не
/// отдал — положить `fallback` (что известно из запроса) и предупредить в журнале.
async fn reread(sync: &SyncState, transport: &aiowa_transport_owa::OwaTransport, store: Arc<Store>, account: AccountId, id: &ItemId, fallback: CalendarEvent) -> CmdResult<CalendarEventDto> {
    match call(sync, "GetItem", transport.get_calendar_item(id)).await {
        Ok(details) => blocking(move || store_fresh(&store, account, &details)).await,
        Err(f) => {
            warn!(error = %f.text("ErrorItemNotFound"), "запись выполнена, но перечитать событие не удалось: в кэш ложится запрос");
            blocking(move || store_event(&store, account, &fallback)).await
        }
    }
}

/// Пояс пользователя для записи в календарь: из кэша (`Store::owa_time_zone`, ключ
/// `OWA_TIME_ZONE_KEY` в `sync_state`), а когда записи нет или она старше суток
/// (`OWA_TIME_ZONE_TTL_MS`) — из настроек OWA (`GetUserConfiguration OWA.UserOptions`) с записью
/// в кэш: так смена пояса в настройках OWA подхватывается без чистки базы. «В OWA пояс не задан»
/// кэшируется на тот же срок, чтобы не спрашивать сервер при каждом обращении к календарю и
/// каждой записи. Если сервер не ответил, остаётся устаревший пояс из кэша, когда он есть. Без
/// пояса запись идёт в контексте UTC, и событие на весь день легло бы в пояс UTC, поэтому отказ
/// попадает в журнал предупреждением; саму запись он не останавливает. Ошибка чтения кэша — тоже
/// только предупреждение.
pub(crate) async fn owa_time_zone(sync: &SyncState, transport: &OwaTransport, store: &Store, account: AccountId) -> Option<String> {
    let cached = match store.owa_time_zone(account) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "кэш пояса пользователя не прочитан");
            None
        }
    };
    if let Some(c) = &cached
        && !c.stale
    {
        return c.zone.clone();
    }
    match call(sync, "GetUserConfiguration", transport.user_time_zone()).await {
        Ok(zone) => {
            if let Err(e) = store.set_owa_time_zone(account, zone.as_deref()) {
                warn!(error = %e, "пояс пользователя не записан в кэш");
            }
            match &zone {
                Some(tz) => info!(zone = %tz, refreshed = cached.is_some(), "пояс пользователя взят из настроек OWA"),
                None => warn!("в настройках OWA пояс не задан: запись в календарь идёт в контексте UTC"),
            }
            zone
        }
        Err(f) => {
            let kept = cached.and_then(|c| c.zone);
            match &kept {
                Some(tz) => warn!(error = %f.text("объекта OWA.UserOptions нет"), zone = %tz, "пояс пользователя не обновлён: остаётся прежний из кэша"),
                None => warn!(error = %f.text("объекта OWA.UserOptions нет"), "пояс пользователя не получен: запись в календарь идёт в контексте UTC"),
            }
            kept
        }
    }
}

/// Пояс пользователя в кэш заранее (обращение к календарю), чтобы запись его не ждала: в фоне,
/// если записи в кэше нет или она старше суток и транспорт процесса уже есть.
pub(crate) fn prefetch_time_zone(sync: Arc<SyncState>, store: Arc<Store>, account: AccountId) {
    if store.owa_time_zone(account).ok().flatten().is_some_and(|c| !c.stale) {
        return;
    }
    let Some(transport) = sync.current_transport() else { return };
    tauri::async_runtime::spawn(async move {
        owa_time_zone(&sync, &transport, &store, account).await;
    });
}

fn emit_calendar_changed(app: &AppHandle) {
    if let Err(e) = app.emit(CALENDAR_CHANGED, ()) {
        warn!(error = %e, "не удалось отправить событие calendar-changed");
    }
}

fn poke_reminders(app: &AppHandle) {
    if let Some(r) = app.try_state::<Arc<RemindersState>>() {
        r.poke();
    }
}

fn parse_response(s: &str) -> CmdResult<MeetingResponse> {
    MeetingResponse::parse(s).ok_or_else(|| "Ответ должен быть одним из: accept, tentative, decline".to_string())
}

// --- команды Tauri ---------------------------------------------------------------------------

/// Создать событие: проверки → пояс пользователя (`owa_time_zone`) → `CreateItem` (при
/// участниках сервер рассылает приглашения) → `GetItem` → кэш → DTO.
#[tauri::command]
pub(crate) async fn calendar_create(req: EventRequest, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<CalendarEventDto> {
    let item = new_item(&req)?;
    let _busy = cw.lock.lock().await;
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let zone = owa_time_zone(&sync, &transport, &state.store, state.account).await;
    let id = call(&sync, "CreateItem", transport.create_calendar_item(&item, zone.as_deref())).await.map_err(|f| f.text("Папка календаря не найдена на сервере"))?;
    info!(attendees = item.required.len() + item.optional.len(), all_day = item.is_all_day, reminder = ?item.reminder_minutes, zone = zone.is_some(), "событие календаря создано");
    let fallback = event_from_item(id.clone(), &item, None);
    let dto = reread(&sync, &transport, state.store.clone(), state.account, &id, fallback).await?;
    poke_reminders(&app);
    Ok(dto)
}

/// Изменить событие: только изменившиеся поля (`UpdateItem` по идентификатору экземпляра) →
/// `GetItem` → кэш → DTO. Без изменений сервер не спрашивается. `send_updates` — разослать
/// участникам обновление (окно спрашивает об этом организатора). Участники и описание
/// сравниваются с кэшем, только если окно их видело (`details_known`): иначе пустые списки и
/// пустое описание в запросе не значат «убрать».
#[tauri::command]
pub(crate) async fn calendar_update(calendar_pk: i64, req: EventRequest, send_updates: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<CalendarEventDto> {
    let item = new_item(&req)?;
    let _busy = cw.lock.lock().await;
    let item_lock = app.state::<CalendarState>().item_lock(calendar_pk);
    let _item = item_lock.lock().await;
    let store = state.store.clone();
    let account = state.account;
    let row = store.calendar_event(calendar_pk).map_err(err)?.ok_or_else(|| "Событие не найдено в кэше календаря".to_string())?;
    let details = if row.has_details && req.details_known { store.calendar_details(calendar_pk).map_err(err)? } else { None };
    let known = Known { row: &row, details: details.map(|(body, attendees, _, _)| (body.map(|h| sanitize_html(&h, &Arc::new(InlineMap::new()))), attendees)) };
    let changes = changes_for(&known, &item);
    if changes.is_empty() {
        info!(calendar_pk, "правка события: изменений нет, сервер не спрашивается");
        return Ok(CalendarEventDto::from(row));
    }
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    let zone = owa_time_zone(&sync, &transport, &store, account).await;
    let new_id = call(&sync, "UpdateItem", transport.update_calendar_item(&row.id, &changes, send_updates, zone.as_deref())).await.map_err(|f| f.text("Событие удалено на сервере"))?;
    info!(calendar_pk, fields = ?changed_fields(&changes), send_updates, zone = zone.is_some(), calendar_type = %row.calendar_type, "событие календаря изменено");
    let fallback = event_from_item(new_id.clone(), &item, Some(&row));
    let dto = reread(&sync, &transport, store, account, &new_id, fallback).await?;
    poke_reminders(&app);
    Ok(dto)
}

/// Удалить событие в «Удалённые» (`send_cancellations` — разослать участникам отмену) и убрать
/// из кэша; окну — `calendar-changed` (удаление зовут и из письма-отмены, когда сетка календаря
/// не на экране). Если на сервере события уже нет — только из кэша. У чужой встречи рассылка
/// не делается: для приглашённого `SendToAllAndSaveCopy` — это не отмена, а отказ организатору.
#[tauri::command]
pub(crate) async fn calendar_delete(calendar_pk: i64, send_cancellations: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<()> {
    let _busy = cw.lock.lock().await;
    let item_lock = app.state::<CalendarState>().item_lock(calendar_pk);
    let _item = item_lock.lock().await;
    let store = &state.store;
    let row = store.calendar_event(calendar_pk).map_err(err)?.ok_or_else(|| "Событие не найдено в кэше календаря".to_string())?;
    let send_cancellations = send_cancellations && !(row.is_meeting && ResponseType::parse(&row.my_response) != ResponseType::Organizer);
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    match call(&sync, "DeleteItem", transport.delete_calendar_item(&row.id, send_cancellations)).await {
        Ok(()) => info!(calendar_pk, send_cancellations, is_meeting = row.is_meeting, "событие календаря удалено"),
        Err(Fail::NotFound) => info!(calendar_pk, "события на сервере уже нет, убираю из кэша"),
        Err(f) => return Err(f.text("Событие удалено на сервере")),
    }
    store.remove_calendar_event(calendar_pk).map_err(db_err)?;
    if let Err(e) = store.set_reminder_state(state.account, &row.id.id, None) {
        warn!(calendar_pk, error = %e, "состояние напоминания удалённого события не снято");
    }
    emit_calendar_changed(&app);
    poke_reminders(&app);
    Ok(())
}

/// Ответить на приглашение по событию календаря (`accept`, `tentative`, `decline`), с ответом
/// организатору или без. После — событие перечитывается: отказ убирает его из календаря на
/// сервере (и из кэша), остальное обновляет свой ответ.
#[tauri::command]
pub(crate) async fn calendar_respond(calendar_pk: i64, response: String, send_response: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<()> {
    let response = parse_response(&response)?;
    let _busy = cw.lock.lock().await;
    let item_lock = app.state::<CalendarState>().item_lock(calendar_pk);
    let _item = item_lock.lock().await;
    let store = state.store.clone();
    let account = state.account;
    let row = store.calendar_event(calendar_pk).map_err(err)?.ok_or_else(|| "Событие не найдено в кэше календаря".to_string())?;
    if !row.is_meeting {
        return Err("Это не встреча: отвечать некому".into());
    }
    if ResponseType::parse(&row.my_response) == ResponseType::Organizer {
        return Err("Вы организатор этой встречи: на своё приглашение не отвечают".into());
    }
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    // Ключ события в кэше устаревает, когда событие меняется на сервере, а кэш окна ещё не
    // обновлён: он перечитывается, как у ответа по письму.
    respond_with_fresh_key(
        || fresh_event_id(&sync, &transport, calendar_pk, &row.id),
        |id| {
            let (sync, transport) = (&sync, &transport);
            async move { call(sync, "CreateItem", transport.respond_to_meeting(&id, response, send_response, None)).await }
        },
    )
    .await
    .map_err(|f| f.text("Событие удалено на сервере"))?;
    info!(calendar_pk, response = response.key(), send_response, "ответ на приглашение отправлен по событию");
    match call(&sync, "GetItem", transport.get_calendar_item(&row.id)).await {
        Ok(details) => {
            let st = store.clone();
            blocking(move || store_fresh(&st, account, &details)).await?;
        }
        Err(Fail::NotFound) => {
            store.remove_calendar_event(calendar_pk).map_err(db_err)?;
        }
        Err(f) => {
            let e = f.text("Событие удалено на сервере");
            warn!(calendar_pk, error = %e, "ответ принят, но перечитать событие не удалось: кэш правится по ответу");
            if response == MeetingResponse::Decline {
                store.remove_calendar_event(calendar_pk).map_err(db_err)?;
            } else if let Some(ev) = event_from_row(&row, response.response_type()) {
                store.upsert_calendar_event(account, &ev).map_err(db_err)?;
            }
        }
    }
    poke_reminders(&app);
    Ok(())
}

/// Сведения о письме о встрече: `GetItem` по письму с кэшем в памяти процесса по `item_pk` и
/// `ChangeKey` письма. Связанное событие ищется в кэше календаря; у отмены, если его там нет,
/// — один раз достаётся с сервера (`GetItem`) и кладётся в кэш, чтобы окно могло удалить его.
#[tauri::command]
pub(crate) async fn meeting_info(item_pk: i64, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<MeetingInfoDto> {
    let lock = cw.meeting_locks.get(item_pk);
    let _busy = lock.lock().await;
    let store = state.store.clone();
    let account = state.account;
    let item = store.item_ref(item_pk).map_err(err)?.ok_or_else(|| "Письмо удалено".to_string())?;
    let cached = cw.meetings().get(&item_pk).filter(|e| e.change_key == item.id.change_key).map(|e| (e.info.clone(), e.linked_tried));
    let (info, linked_tried) = match cached {
        Some(c) => c,
        None => {
            let transport = sync.transport().await.map_err(|e| e.to_string())?;
            let info = call(&sync, "GetItem", transport.get_meeting_request(&item.id)).await.map_err(|f| f.text("Письмо удалено на сервере"))?;
            info!(item_pk, kind = info.kind.key(), my_response = info.my_response.as_str(), out_of_date = info.is_out_of_date, associated = info.associated_calendar_item.is_some(), "сведения о письме о встрече загружены");
            cw.meetings().insert(item_pk, MeetingCacheEntry { change_key: item.id.change_key.clone(), info: info.clone(), linked_tried: false });
            (info, false)
        }
    };
    let mut calendar_pk = linked_event(&store, account, &info);
    if calendar_pk.is_none()
        && !linked_tried
        && info.kind == aiowa_core::MeetingMessageKind::Cancellation
        && let Some(assoc) = info.associated_calendar_item.clone()
    {
        if let Some(e) = cw.meetings().get_mut(&item_pk) {
            e.linked_tried = true;
        }
        let transport = sync.transport().await.map_err(|e| e.to_string())?;
        match call(&sync, "GetItem", transport.get_calendar_item(&assoc)).await {
            Ok(details) => {
                let st = store.clone();
                calendar_pk = Some(blocking(move || {
                    let pk = st.upsert_calendar_event(account, &details.event).map_err(db_err)?;
                    st.store_calendar_details(pk, &details).map_err(db_err)?;
                    Ok(pk)
                })
                .await?);
                info!(item_pk, "связанное событие отменённой встречи положено в кэш");
            }
            Err(Fail::NotFound) => info!(item_pk, "связанного события отменённой встречи на сервере уже нет"),
            Err(f) => warn!(item_pk, error = %f.text("нет на сервере"), "связанное событие отменённой встречи не загружено"),
        }
    }
    Ok(meeting_dto(&info, calendar_pk))
}

/// Ответить на приглашение по письму. После успеха: кэш сведений о письме сброшен, связанное
/// событие в кэше календаря поправлено (отказ убирает его; если принятой встречи в кэше нет —
/// она достаётся `GetItem` по `AssociatedCalendarItemId`, чтобы напоминание пришло и без захода
/// в календарь), окну — `items-changed` (беседа и папка письма) и `calendar-changed`, папке
/// письма — синхронизация: сервер обычно убирает приглашение из «Входящих» после ответа.
#[tauri::command]
pub(crate) async fn meeting_respond(item_pk: i64, response: String, send_response: bool, app: AppHandle, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cw: State<'_, CalendarWriteState>) -> CmdResult<()> {
    let response = parse_response(&response)?;
    let _busy = cw.lock.lock().await;
    let store = state.store.clone();
    let account = state.account;
    let item = store.item_ref(item_pk).map_err(err)?.ok_or_else(|| "Письмо удалено".to_string())?;
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    // Ответ ссылается на письмо-приглашение (`ReferenceItemId`) с его ключом изменений.
    respond_with_fresh_key(
        || fresh_item_id(&sync, &transport, &store, item_pk, &item.id),
        |id| {
            let (sync, transport) = (&sync, &transport);
            async move { call(sync, "CreateItem", transport.respond_to_meeting(&id, response, send_response, None)).await }
        },
    )
    .await
    .map_err(|f| f.text("Письмо удалено на сервере"))?;
    info!(item_pk, response = response.key(), send_response, "ответ на приглашение отправлен по письму");
    // Сведения о письме устарели (свой ответ и ChangeKey письма сменились); связанное событие —
    // по ним, пока они ещё есть.
    let entry = cw.meetings().remove(&item_pk);
    let linked = entry.as_ref().and_then(|e| linked_event(&store, account, &e.info));
    match linked {
        Some(pk) => {
            let adjusted = match store.calendar_event(pk) {
                Ok(Some(row)) if response == MeetingResponse::Decline => store.remove_calendar_event(row.calendar_pk).map(|_| ()),
                Ok(Some(row)) => match event_from_row(&row, response.response_type()) {
                    Some(ev) => store.upsert_calendar_event(account, &ev).map(|_| ()),
                    None => Ok(()),
                },
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = adjusted {
                warn!(item_pk, calendar_pk = pk, error = %e, "связанное событие в кэше не поправлено");
            }
        }
        None if response != MeetingResponse::Decline => {
            if let Some(assoc) = entry.and_then(|e| e.info.associated_calendar_item) {
                match call(&sync, "GetItem", transport.get_calendar_item(&assoc)).await {
                    Ok(details) => {
                        let st = store.clone();
                        match blocking(move || store_fresh(&st, account, &details)).await {
                            Ok(_) => info!(item_pk, "событие принятого приглашения положено в кэш календаря"),
                            Err(e) => warn!(item_pk, error = %e, "событие принятого приглашения не положено в кэш"),
                        }
                    }
                    Err(f) => warn!(item_pk, error = %f.text(ITEM_NOT_FOUND), "событие принятого приглашения не загружено"),
                }
            }
        }
        None => {}
    }
    crate::write::emit_items_changed(&app, item.conversation_pk.into_iter().collect(), vec![item.folder_pk]);
    emit_calendar_changed(&app);
    sync.inner().request(app.clone(), Some(vec![item.folder_pk]));
    poke_reminders(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::MeetingMessageKind;

    fn rcpt(name: Option<&str>, email: &str) -> ComposeRecipientDto {
        ComposeRecipientDto { name: name.map(String::from), email: email.to_string() }
    }

    fn req() -> EventRequest {
        EventRequest {
            details_known: true,
            subject: "  Планёрка  ".into(),
            start_ms: 1_700_000_000_000,
            end_ms: 1_700_000_000_000 + 30 * 60_000,
            is_all_day: false,
            location: Some("  Зал 3 ".into()),
            body_html: Some(r#"<div onclick="x()">Повестка<script>alert(1)</script></div>"#.into()),
            required: vec![rcpt(Some(" Анна "), " a@y.ru "), rcpt(None, "B@Y.RU")],
            optional: vec![rcpt(Some("b@y.ru"), "b@y.ru"), rcpt(Some("Вера"), "v@z.ru")],
            reminder_minutes: Some(15),
            free_busy: Some("tentative".into()),
            is_private: true,
        }
    }

    fn row() -> CalendarRow {
        CalendarRow {
            calendar_pk: 7,
            id: ItemId { id: "AAMk".into(), change_key: Some("CQAA".into()) },
            subject: Some("Планёрка".into()),
            start_at: 1_700_000_000_000,
            end_at: 1_700_000_000_000 + 30 * 60_000,
            is_all_day: false,
            location: Some("Зал 3".into()),
            organizer_name: Some("Я".into()),
            organizer_email: Some("me@x.ru".into()),
            free_busy: "Tentative".into(),
            calendar_type: "Single".into(),
            is_meeting: true,
            is_cancelled: false,
            my_response: "Organizer".into(),
            sensitivity: Some("Private".into()),
            has_attachments: false,
            has_details: true,
            reminder_minutes: Some(15),
        }
    }

    fn attendee(email: &str, required: bool) -> Attendee {
        Attendee { participant: Participant { name: None, email: Some(email.into()), routing_type: Some("SMTP".into()) }, response: ResponseType::NoResponseReceived, required }
    }

    fn known_full(row: &CalendarRow) -> Known<'_> {
        Known { row, details: Some((Some("<div>Повестка</div>".into()), vec![attendee("A@y.ru", true), attendee("b@y.ru", true), attendee("v@z.ru", false)])) }
    }

    #[test]
    fn request_is_checked_and_cleaned() {
        let item = new_item(&req()).unwrap();
        assert_eq!(item.subject, "Планёрка");
        assert_eq!(item.location.as_deref(), Some("Зал 3"));
        assert_eq!((item.start.as_millisecond(), item.end.as_millisecond(), item.is_all_day), (1_700_000_000_000, 1_700_000_000_000 + 30 * 60_000, false));
        assert_eq!((item.reminder_minutes, item.free_busy, item.sensitivity_private), (Some(15), FreeBusy::Tentative, true));
        let body = item.body_html.as_deref().unwrap();
        assert!(body.contains("Повестка") && !body.contains("script") && !body.contains("onclick"), "{body}");
        assert_eq!(item.required.iter().map(|p| p.email.as_deref().unwrap()).collect::<Vec<_>>(), vec!["a@y.ru", "B@Y.RU"], "адреса обрезаны, регистр сохранён");
        assert_eq!(item.required[0].name.as_deref(), Some("Анна"));
        assert_eq!(item.required[0].routing_type.as_deref(), Some("SMTP"));
        assert_eq!(item.optional.iter().map(|p| p.email.as_deref().unwrap()).collect::<Vec<_>>(), vec!["v@z.ru"], "повтор обязательного среди необязательных отброшен");
        assert!(item.has_attendees());

        let r = EventRequest { end_ms: req().start_ms, ..req() };
        assert_eq!(new_item(&r).unwrap_err(), "Конец события должен быть позже начала");
        let r = EventRequest { end_ms: req().start_ms - 1, is_all_day: true, ..req() };
        assert_eq!(new_item(&r).unwrap_err(), "Последний день события раньше первого");
        let r = EventRequest { end_ms: req().start_ms + MAX_DURATION_MS + 1, ..req() };
        assert!(new_item(&r).unwrap_err().contains("длиннее года"));
        let r = EventRequest { reminder_minutes: Some(-1), ..req() };
        assert!(new_item(&r).unwrap_err().starts_with("Напоминание"));
        let r = EventRequest { reminder_minutes: Some(MAX_REMINDER_MINUTES + 1), ..req() };
        assert!(new_item(&r).is_err());
        let r = EventRequest { reminder_minutes: Some(0), ..req() };
        assert_eq!(new_item(&r).unwrap().reminder_minutes, Some(0), "«в момент начала» допустимо");
        let r = EventRequest { free_busy: Some("maybe".into()), ..req() };
        assert!(new_item(&r).unwrap_err().starts_with("Занятость"));
        let r = EventRequest { free_busy: None, ..req() };
        assert_eq!(new_item(&r).unwrap().free_busy, FreeBusy::Busy, "без занятости — «занят»");
        let r = EventRequest { free_busy: Some("WorkingElsewhere".into()), ..req() };
        assert_eq!(new_item(&r).unwrap().free_busy, FreeBusy::WorkingElsewhere);
        let r = EventRequest { required: vec![rcpt(None, "кто-то")], ..req() };
        assert!(new_item(&r).unwrap_err().contains("не похож на почтовый"));
        let r = EventRequest { start_ms: i64::MIN, ..req() };
        assert!(new_item(&r).unwrap_err().contains("вне допустимых"));
        let r = EventRequest { body_html: Some("<p>&nbsp; </p><br>".into()), location: Some("   ".into()), required: vec![], optional: vec![], ..req() };
        let item = new_item(&r).unwrap();
        assert!(!item.has_attendees());
        assert_eq!((item.body_html, item.location), (None, None), "пустые описание и место не отправляются");
    }

    #[test]
    fn blank_html_detection() {
        assert!(html_is_blank(""));
        assert!(html_is_blank("<p></p><div><br></div>"));
        assert!(html_is_blank("<p>&nbsp;\u{a0} </p>"));
        assert!(!html_is_blank("<p>текст</p>"));
        assert!(!html_is_blank("а < б"));
    }

    #[test]
    fn unchanged_request_yields_no_changes() {
        let row = row();
        let item = new_item(&req()).unwrap();
        let ch = changes_for(&known_full(&row), &item);
        assert!(ch.is_empty(), "{ch:?}");
        // Тот же набор участников в другом порядке и регистре — не изменение.
        let shuffled = EventRequest { required: vec![rcpt(None, "b@y.ru"), rcpt(None, "A@Y.RU")], ..req() };
        assert!(changes_for(&known_full(&row), &new_item(&shuffled).unwrap()).is_empty());
        // Описание с другими переносами строк — не изменение.
        let spaced = EventRequest { body_html: Some("<div>\n  Повестка\n</div>".into()), ..req() };
        assert!(changes_for(&known_full(&row), &new_item(&spaced).unwrap()).is_empty());
    }

    #[test]
    fn only_changed_fields_are_sent() {
        let row = row();
        let known = known_full(&row);
        let item = new_item(&EventRequest { subject: "Планёрка 2".into(), ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!(ch, CalendarItemChanges { subject: Some("Планёрка 2".into()), ..Default::default() });
        assert_eq!(changed_fields(&ch), vec!["subject"]);

        // Снятое место — DeleteItemField; напоминание снято; занятость другая.
        let item = new_item(&EventRequest { location: None, reminder_minutes: None, free_busy: Some("oof".into()), ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!(ch.location, Some(None));
        assert_eq!(ch.reminder_minutes, Some(None));
        assert_eq!(ch.free_busy, Some(FreeBusy::Oof));
        assert!(ch.subject.is_none() && ch.start.is_none() && ch.body_html.is_none() && ch.required.is_none());
        assert_eq!(changed_fields(&ch), vec!["location", "reminder", "free_busy"]);

        // Перенос — только время.
        let item = new_item(&EventRequest { start_ms: req().start_ms + 3_600_000, end_ms: req().end_ms + 3_600_000, ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!((ch.start, ch.end, ch.is_all_day), (Some(item.start), Some(item.end), None));
        assert_eq!(changed_fields(&ch), vec!["time"]);

        // «Весь день» уходит вместе с началом и концом (у «весь день» на сутки позже — конец уже другой).
        let item = new_item(&EventRequest { is_all_day: true, end_ms: req().start_ms + 24 * 3_600_000, ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!((ch.is_all_day, ch.start.is_some(), ch.end.is_some()), (Some(true), true, true));

        // Участники: новый обязательный — только список обязательных; убранный необязательный — только необязательных (пустой список).
        let item = new_item(&EventRequest { required: vec![rcpt(None, "a@y.ru"), rcpt(None, "b@y.ru"), rcpt(None, "n@y.ru")], ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!(ch.required.as_ref().map(Vec::len), Some(3));
        assert!(ch.optional.is_none());
        let item = new_item(&EventRequest { optional: vec![], ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!(ch.optional, Some(vec![]), "пустой список убирает всех необязательных");
        assert!(ch.required.is_none());

        // Описание: другое — уходит очищенным; снятое при непустом в кэше — пустая строка.
        let item = new_item(&EventRequest { body_html: Some("<p>Новая повестка</p>".into()), ..req() }).unwrap();
        assert_eq!(changes_for(&known, &item).body_html.as_deref(), Some("<p>Новая повестка</p>"));
        let item = new_item(&EventRequest { body_html: None, ..req() }).unwrap();
        assert_eq!(changes_for(&known, &item).body_html.as_deref(), Some(""));
        // Пустое описание в кэше и пустое в запросе — ничего.
        let blank = Known { row: &row, details: Some((Some("<p>&nbsp;</p>".into()), vec![])) };
        let item = new_item(&EventRequest { body_html: None, required: vec![], optional: vec![], ..req() }).unwrap();
        assert!(changes_for(&blank, &item).body_html.is_none());
        // Конфиденциальность правкой не передаётся: поля нет.
        let item = new_item(&EventRequest { is_private: false, ..req() }).unwrap();
        assert!(changes_for(&known, &item).is_empty());
    }

    #[test]
    fn without_cached_details_only_non_empty_body_and_attendees_go() {
        let mut row = row();
        row.has_details = false;
        let known = Known { row: &row, details: None };
        let item = new_item(&req()).unwrap();
        let ch = changes_for(&known, &item);
        assert!(ch.body_html.is_some() && ch.required.is_some() && ch.optional.is_some(), "{ch:?}");
        assert!(ch.subject.is_none() && ch.start.is_none() && ch.location.is_none());
        let item = new_item(&EventRequest { body_html: None, required: vec![], optional: vec![], ..req() }).unwrap();
        assert!(changes_for(&known, &item).is_empty(), "неизвестное не трогаем");
        // Подробности в кэше есть, но окно их не видело (`details_known = false`): `calendar_update`
        // даёт `details: None`, и редактор, открытый до загрузки подробностей, участников и описание
        // не снимает — уходит только тема.
        let row = self::row();
        assert!(row.has_details);
        let known = Known { row: &row, details: None };
        let item = new_item(&EventRequest { details_known: false, subject: "Планёрка 2".into(), body_html: None, required: vec![], optional: vec![], ..req() }).unwrap();
        let ch = changes_for(&known, &item);
        assert_eq!(ch, CalendarItemChanges { subject: Some("Планёрка 2".into()), ..Default::default() });
    }

    #[test]
    fn all_day_bounds_are_utc_midnights_compared_by_date() {
        let tz = TimeZone::system();
        let local_midnight = |d: jiff::civil::Date| d.to_zoned(tz.clone()).unwrap().timestamp().as_millisecond();
        let utc_midnight = |d: jiff::civil::Date| d.to_zoned(TimeZone::UTC).unwrap().timestamp();
        let (d1, d2, d3) = (jiff::civil::date(2026, 10, 26), jiff::civil::date(2026, 10, 27), jiff::civil::date(2026, 10, 28));
        // Окно шлёт местные полуночи; на сервер уходят полуночи UTC тех же дат.
        let all_day = EventRequest { is_all_day: true, start_ms: local_midnight(d1), end_ms: local_midnight(d2), ..req() };
        let item = new_item(&all_day).unwrap();
        assert_eq!((item.start, item.end, item.is_all_day), (utc_midnight(d1), utc_midnight(d2), true));
        // Последний день раньше первого — ошибка и после перевода дат.
        let bad = EventRequest { end_ms: local_midnight(d1), ..all_day.clone() };
        assert_eq!(new_item(&bad).unwrap_err(), "Последний день события раньше первого");
        // В кэше границы — полуночи UTC (своё событие) или местные полуночи: правка темы время не трогает.
        let mut row = row();
        row.is_all_day = true;
        for (s, e) in [(utc_midnight(d1).as_millisecond(), utc_midnight(d2).as_millisecond()), (local_midnight(d1), local_midnight(d2))] {
            row.start_at = s;
            row.end_at = e;
            let ch = changes_for(&known_full(&row), &new_item(&EventRequest { subject: "Планёрка 2".into(), ..all_day.clone() }).unwrap());
            assert_eq!(ch, CalendarItemChanges { subject: Some("Планёрка 2".into()), ..Default::default() }, "{ch:?}");
        }
        // Другой последний день — время уходит, и с ним is_all_day: Some(true) (транспорт по нему шлёт местные полуночи дат).
        let ch = changes_for(&known_full(&row), &new_item(&EventRequest { end_ms: local_midnight(d3), ..all_day.clone() }).unwrap());
        assert_eq!((ch.start, ch.end, ch.is_all_day), (Some(utc_midnight(d1)), Some(utc_midnight(d3)), Some(true)));
        assert_eq!(changed_fields(&ch), vec!["time", "all_day"]);
        // Включение «весь день» у обычного события: время и признак.
        let mut timed = self::row();
        timed.start_at = local_midnight(d1) + 9 * 3_600_000;
        timed.end_at = local_midnight(d1) + 10 * 3_600_000;
        let ch = changes_for(&known_full(&timed), &new_item(&all_day).unwrap());
        assert_eq!((ch.start, ch.end, ch.is_all_day), (Some(utc_midnight(d1)), Some(utc_midnight(d2)), Some(true)));
        // Снятие «весь день»: время сравнивается по миллисекундам и уходит вместе с признаком.
        let ch = changes_for(&known_full(&row), &new_item(&EventRequest { is_all_day: false, start_ms: local_midnight(d1) + 9 * 3_600_000, end_ms: local_midnight(d1) + 10 * 3_600_000, ..all_day }).unwrap());
        assert_eq!((ch.is_all_day, ch.start.is_some(), ch.end.is_some()), (Some(false), true, true));
    }

    #[test]
    fn fallback_events_for_cache() {
        let item = new_item(&req()).unwrap();
        let id = ItemId { id: "NEW".into(), change_key: Some("k".into()) };
        let ev = event_from_item(id.clone(), &item, None);
        assert_eq!((ev.id, ev.subject.as_deref(), ev.is_meeting, ev.my_response, ev.calendar_type), (id, Some("Планёрка"), true, ResponseType::Organizer, CalendarItemType::Single));
        assert_eq!((ev.sensitivity.as_deref(), ev.reminder_minutes, ev.free_busy, ev.organizer), (Some("Private"), Some(15), FreeBusy::Tentative, None));
        // При правке неизвестное берётся из строки: организатор, тип, вложения, конфиденциальность.
        let mut base = row();
        base.calendar_type = "Occurrence".into();
        base.has_attachments = true;
        base.sensitivity = Some("Normal".into());
        let plain = new_item(&EventRequest { subject: "".into(), required: vec![], optional: vec![], is_private: true, ..req() }).unwrap();
        let ev = event_from_item(ItemId { id: "AAMk".into(), change_key: Some("k2".into()) }, &plain, Some(&base));
        assert_eq!((ev.subject, ev.is_meeting, ev.calendar_type, ev.has_attachments), (None, true, CalendarItemType::Occurrence, true));
        assert_eq!((ev.sensitivity.as_deref(), ev.organizer.as_ref().and_then(|o| o.email.as_deref())), (Some("Normal"), Some("me@x.ru")));
        // Строка обратно в событие с новым ответом.
        let ev = event_from_row(&base, ResponseType::Tentative).unwrap();
        assert_eq!((ev.my_response, ev.start.as_millisecond(), ev.reminder_minutes, ev.free_busy), (ResponseType::Tentative, base.start_at, Some(15), FreeBusy::Tentative));
    }

    fn info(start: &str, end: &str, organizer: Option<&str>, associated: Option<&str>) -> MeetingRequestInfo {
        MeetingRequestInfo {
            kind: MeetingMessageKind::Request,
            start: Some(start.parse().unwrap()),
            end: Some(end.parse().unwrap()),
            is_all_day: false,
            location: None,
            organizer: organizer.map(|e| Participant { name: None, email: Some(e.into()), routing_type: None }),
            my_response: ResponseType::NoResponseReceived,
            is_out_of_date: false,
            associated_calendar_item: associated.map(|id| ItemId { id: id.into(), change_key: None }),
            recurrence_text: None,
            response_type_of_sender: None,
        }
    }

    #[test]
    fn meeting_dto_uses_ui_keys() {
        let mut i = info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", Some("o@x.ru"), Some("CAL"));
        i.kind = MeetingMessageKind::Response;
        i.response_type_of_sender = Some(ResponseType::Accept);
        let dto = meeting_dto(&i, None);
        assert_eq!((dto.kind, dto.my_response, dto.sender_response, dto.has_calendar_item, dto.calendar_pk), ("response", "no_response", Some("accept"), true, None));
        let start_ms = "2026-10-20T07:00:00Z".parse::<Timestamp>().unwrap().as_millisecond();
        assert_eq!((dto.start_ms, dto.organizer.as_ref().and_then(|o| o.email.as_deref())), (Some(start_ms), Some("o@x.ru")));
        let dto = meeting_dto(&info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", None, None), Some(5));
        assert_eq!((dto.has_calendar_item, dto.calendar_pk, dto.sender_response), (true, Some(5), None));
        assert!(!meeting_dto(&info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", None, None), None).has_calendar_item);
    }

    #[test]
    fn linked_event_by_id_then_by_time_and_organizer() {
        let dir = std::env::temp_dir().join(format!("aiowa-desktop-calwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open(dir.join("mail.db")).unwrap();
        let acc = s.account("h", "u").unwrap();
        let ev = |id: &str, start: &str, end: &str, organizer: &str, meeting: bool| CalendarEvent {
            id: ItemId { id: id.into(), change_key: Some("k".into()) },
            subject: Some("Встреча".into()),
            start: start.parse().unwrap(),
            end: end.parse().unwrap(),
            is_all_day: false,
            location: None,
            organizer: Some(Participant { name: None, email: Some(organizer.into()), routing_type: None }),
            free_busy: FreeBusy::Busy,
            calendar_type: CalendarItemType::Single,
            is_meeting: meeting,
            is_cancelled: false,
            my_response: ResponseType::NoResponseReceived,
            sensitivity: None,
            has_attachments: false,
            last_modified: None,
            reminder_minutes: None,
        };
        let a = s.upsert_calendar_event(acc, &ev("A", "2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", "O@x.ru", true)).unwrap();
        let b = s.upsert_calendar_event(acc, &ev("B", "2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", "p@x.ru", true)).unwrap();
        let _c = s.upsert_calendar_event(acc, &ev("C", "2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", "q@x.ru", false)).unwrap();
        // По идентификатору связанного события.
        assert_eq!(linked_event(&s, acc, &info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", None, Some("B"))), Some(b));
        // Идентификатора в кэше нет — по времени и организатору (регистр адреса не важен).
        assert_eq!(linked_event(&s, acc, &info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", Some("o@X.ru"), Some("GONE"))), Some(a));
        // Без организатора две встречи на то же время — неоднозначно.
        assert_eq!(linked_event(&s, acc, &info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", None, None)), None);
        // Не встреча (C) не считается; другое время — нет.
        assert_eq!(linked_event(&s, acc, &info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", Some("q@x.ru"), None)), None);
        assert_eq!(linked_event(&s, acc, &info("2026-10-20T08:00:00Z", "2026-10-20T08:30:00Z", Some("o@x.ru"), None)), None);
        let mut no_time = info("2026-10-20T07:00:00Z", "2026-10-20T07:30:00Z", Some("o@x.ru"), None);
        no_time.start = None;
        assert_eq!(linked_event(&s, acc, &no_time), None);
    }

    #[test]
    fn response_parsing_and_failure_texts() {
        assert_eq!(parse_response(" Accept ").unwrap(), MeetingResponse::Accept);
        assert!(parse_response("maybe").is_err());
        assert_eq!(Fail::NotFound.text("Письмо удалено на сервере"), "Письмо удалено на сервере");
        assert_eq!(Fail::Other("Нет связи с сервером".into()).text("x"), "Нет связи с сервером");
    }

    /// Ответ на приглашение: ключ перечитывается до ответа и ответ идёт с ним; при
    /// `ErrorStaleObject` — ключ перечитывается ещё раз и ответ повторяется, но только однажды;
    /// другие отказы не повторяются, отказ перечитывания до ответа не доходит.
    #[test]
    fn meeting_response_uses_fresh_key_and_retries_stale_once() {
        let key = |n: usize| ItemId { id: "AAMk".into(), change_key: Some(format!("CQAAABYAAAA{n}")) };
        let run = |answers: Vec<Result<(), Fail>>, fresh_ok: bool| {
            let (mut refreshed, mut sent, mut answers) = (0usize, Vec::<ItemId>::new(), answers.into_iter());
            let r = tauri::async_runtime::block_on(respond_with_fresh_key(
                || {
                    refreshed += 1;
                    std::future::ready(if fresh_ok { Ok(key(refreshed)) } else { Err(Fail::NotFound) })
                },
                |id| {
                    sent.push(id);
                    std::future::ready(answers.next().expect("лишний ответ"))
                },
            ));
            (r, refreshed, sent)
        };
        let (r, refreshed, sent) = run(vec![Ok(())], true);
        assert!(r.is_ok() && refreshed == 1 && sent == vec![key(1)], "ответ со свежим ключом, без повтора");
        let (r, refreshed, sent) = run(vec![Err(Fail::Stale), Ok(())], true);
        assert!(r.is_ok() && refreshed == 2 && sent == vec![key(1), key(2)], "после ErrorStaleObject — заново перечитанный ключ");
        let (r, refreshed, sent) = run(vec![Err(Fail::Stale), Err(Fail::Stale)], true);
        assert!(matches!(r, Err(Fail::Stale)) && refreshed == 2 && sent.len() == 2, "повтор один");
        let (r, refreshed, sent) = run(vec![Err(Fail::Other("Нет связи с сервером".into()))], true);
        assert!(matches!(r, Err(Fail::Other(_))) && refreshed == 1 && sent.len() == 1, "сетевой отказ не повторяется");
        let (r, refreshed, sent) = run(vec![], false);
        assert!(matches!(r, Err(Fail::NotFound)) && refreshed == 1 && sent.is_empty(), "без ключа ответа нет");
    }
}
