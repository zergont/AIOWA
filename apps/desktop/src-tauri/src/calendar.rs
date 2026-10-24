//! Команды календаря: события окна `[start_ms, end_ms)` и подробности одного события.
//!
//! Кэш (`calendar_items`, `calendar_ranges` в локальной базе) — первое, что видит окно: без
//! `refresh` команда отвечает только кэшем (что есть) и говорит, когда окно забирали с сервера;
//! о догрузке решает само окно — оно знает, какой период пользователь листает прямо сейчас.
//! С `refresh` — `FindItem`. Сеть — только через транспорт процесса (`SyncState::transport`):
//! входов здесь нет; каждый запрос под таймаутом, ошибка мёртвой сессии сбрасывает транспорт
//! (`SyncState::invalidate_on`). Отказ сети не роняет команду: окно получает кэш и текст ошибки
//! по-русски в поле `error`. Одновременные запросы одного окна или одного события ждут первый
//! под замком по ключу и видят готовый результат, а не идут на сервер ещё раз.
//!
//! Все моменты — миллисекунды UTC; в локальное время переводит окно.

use crate::ondemand::{describe_transport, timed_out, KeyedLocks};
use crate::sync::SyncState;
use crate::{err, sanitize_html, AppState, CmdResult, InlineMap};
use aiowa_core::{AccountId, Attendee, CalendarEvent, CalendarEventDetails, CalendarItemType, FreeBusy, ItemId, Participant, ResponseType};
use aiowa_store::{CalendarRow, Store};
use aiowa_transport::{MailTransport, TransportError};
use jiff::Timestamp;
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tauri::State;
use tracing::{info, warn};

/// Таймаут одного запроса к серверу (`FindItem` окна или `GetItem` события).
const CALENDAR_TIMEOUT: Duration = Duration::from_secs(60);
/// Самое большое окно, которое команда согласна забрать одним запросом (с запасом на
/// шесть недель сетки месяца).
const MAX_WINDOW_MS: i64 = 100 * 24 * 3600 * 1000;

/// Состояние команд календаря: замки по окну `(start_ms, end_ms)` и по событию. Замок события
/// делят чтение подробностей и команды записи (`calendar_write.rs`): правка не переплетается
/// с перечитыванием того же события.
#[derive(Default)]
pub(crate) struct CalendarState {
    windows: KeyedLocks<(i64, i64)>,
    items: KeyedLocks<i64>,
}

impl CalendarState {
    /// Замок одного события по ключу кэша.
    pub(crate) fn item_lock(&self, calendar_pk: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.items.get(calendar_pk)
    }
}

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

// --- DTO ------------------------------------------------------------------------------------

/// Организатор события для окна.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ParticipantDto {
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
}

impl From<Participant> for ParticipantDto {
    fn from(p: Participant) -> Self {
        Self { name: p.name, email: p.email }
    }
}

/// Событие для сетки календаря. Перечисления — ключами интерфейса (`FreeBusy::key` и т. п.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CalendarEventDto {
    calendar_pk: i64,
    subject: Option<String>,
    start_ms: i64,
    end_ms: i64,
    is_all_day: bool,
    location: Option<String>,
    organizer: Option<ParticipantDto>,
    /// `free`, `tentative`, `busy`, `oof`, `working_elsewhere`, `no_data`.
    free_busy: &'static str,
    /// `single`, `occurrence`, `exception`, `recurring_master`.
    calendar_type: &'static str,
    is_meeting: bool,
    is_cancelled: bool,
    /// `unknown`, `organizer`, `tentative`, `accept`, `decline`, `no_response`.
    my_response: &'static str,
    sensitivity: Option<String>,
    has_attachments: bool,
    /// Подробности (тело, участники) уже есть в кэше: панель откроется без сети.
    has_details: bool,
    /// Напоминание за N минут до начала; `None` — без напоминания.
    reminder_minutes: Option<i64>,
}

impl From<CalendarRow> for CalendarEventDto {
    fn from(r: CalendarRow) -> Self {
        let organizer = match (r.organizer_name, r.organizer_email) {
            (None, None) => None,
            (name, email) => Some(ParticipantDto { name, email }),
        };
        Self {
            calendar_pk: r.calendar_pk,
            subject: r.subject,
            start_ms: r.start_at,
            end_ms: r.end_at,
            is_all_day: r.is_all_day,
            location: r.location,
            organizer,
            free_busy: FreeBusy::parse(&r.free_busy).key(),
            calendar_type: CalendarItemType::parse(&r.calendar_type).key(),
            is_meeting: r.is_meeting,
            is_cancelled: r.is_cancelled,
            my_response: ResponseType::parse(&r.my_response).key(),
            sensitivity: r.sensitivity,
            has_attachments: r.has_attachments,
            has_details: r.has_details,
            reminder_minutes: r.reminder_minutes,
        }
    }
}

/// Ответ `calendar_events`.
#[derive(Debug, Serialize)]
pub(crate) struct CalendarRangeDto {
    events: Vec<CalendarEventDto>,
    /// События взяты из кэша, сервер в этот раз не спрашивался (или не ответил — см. `error`).
    from_cache: bool,
    /// Когда окно последний раз забирали с сервера; `None` — кэш окно целиком не покрывает.
    fetched_ms: Option<i64>,
    /// Сервер не ответил: текст для пользователя. События при этом — из кэша.
    error: Option<String>,
}

/// Участник встречи для панели события.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AttendeeDto {
    name: Option<String>,
    email: Option<String>,
    /// Как `my_response` у события.
    response: &'static str,
    required: bool,
}

impl From<Attendee> for AttendeeDto {
    fn from(a: Attendee) -> Self {
        Self { name: a.participant.name, email: a.participant.email, response: a.response.key(), required: a.required }
    }
}

/// Ответ `calendar_event_details`.
#[derive(Debug, Serialize)]
pub(crate) struct CalendarEventDetailsDto {
    event: CalendarEventDto,
    /// Очищенный HTML тела; `None`, если тела нет.
    body_html: Option<String>,
    attendees: Vec<AttendeeDto>,
    reminder_minutes: Option<i64>,
    recurrence_text: Option<String>,
    from_cache: bool,
    /// Обновление с сервера не удалось, показаны подробности из кэша.
    error: Option<String>,
}

// --- решения без сети ------------------------------------------------------------------------

/// Окно должно быть непустым и не длиннее `MAX_WINDOW_MS`.
fn check_window(start_ms: i64, end_ms: i64) -> CmdResult<()> {
    if end_ms <= start_ms {
        return Err("Пустое окно календаря".into());
    }
    if end_ms - start_ms > MAX_WINDOW_MS {
        return Err(format!("Окно календаря больше {} дней", MAX_WINDOW_MS / (24 * 3600 * 1000)));
    }
    Ok(())
}

fn window_bounds(start_ms: i64, end_ms: i64) -> CmdResult<(Timestamp, Timestamp)> {
    let bad = |_| "Окно календаря вне допустимых дат".to_string();
    Ok((Timestamp::from_millisecond(start_ms).map_err(bad)?, Timestamp::from_millisecond(end_ms).map_err(bad)?))
}

/// Кэш окна в DTO. `age_ms` — возраст покрытия (`None` — не покрыто), `error` — почему сервер
/// в этот раз не спрошен или не ответил.
fn cached_range(store: &Store, account: AccountId, start_ms: i64, end_ms: i64, age_ms: Option<i64>, error: Option<String>) -> CmdResult<CalendarRangeDto> {
    let rows = store.calendar_events(account, start_ms, end_ms).map_err(err)?;
    let fetched_ms = age_ms.map(|a| now_ms() - a);
    Ok(CalendarRangeDto { events: rows.into_iter().map(CalendarEventDto::from).collect(), from_cache: true, fetched_ms, error })
}

/// Подробности события из кэша в DTO: тело чистится как у писем, но без подстановки `cid:`
/// (вложения событий не кэшируются, карта пустая — такие картинки теряют `src`).
fn details_dto(store: &Store, row: CalendarRow, from_cache: bool, error: Option<String>) -> CmdResult<CalendarEventDetailsDto> {
    let (body, attendees, reminder_minutes, recurrence_text) = store.calendar_details(row.calendar_pk).map_err(err)?.unwrap_or_default();
    let inline = Arc::new(InlineMap::new());
    let body_html = body.as_deref().map(|h| sanitize_html(h, &inline));
    Ok(CalendarEventDetailsDto { event: row.into(), body_html, attendees: attendees.into_iter().map(AttendeeDto::from).collect(), reminder_minutes, recurrence_text, from_cache, error })
}

// --- сеть -----------------------------------------------------------------------------------

/// Код отказа сервера «элемента нет» (удалён или перенесён другим клиентом).
pub(crate) const ITEM_NOT_FOUND: &str = "ErrorItemNotFound";

/// Текст ошибки транспорта для календаря: удалённое событие словами, остальное — как везде.
fn describe(e: &TransportError) -> String {
    match e {
        TransportError::Server { code, .. } if code == ITEM_NOT_FOUND => "Событие удалено на сервере".into(),
        other => describe_transport(other),
    }
}

/// Один запрос к серверу под таймаутом: ошибка транспорта сбрасывает мёртвую сессию и
/// превращается в текст для пользователя.
async fn step<T>(sync: &SyncState, what: &'static str, fut: impl Future<Output = aiowa_transport::Result<T>>) -> CmdResult<T> {
    match tokio::time::timeout(CALENDAR_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            warn!(step = what, error = %e, "запрос календаря отклонён");
            sync.invalidate_on(&e).await;
            Err(describe(&e))
        }
        Err(_) => {
            warn!(step = what, "запрос календаря не уложился в таймаут");
            Err(timed_out(CALENDAR_TIMEOUT))
        }
    }
}

/// События окна с сервера. Ошибка — текст для пользователя.
async fn fetch_window(sync: &SyncState, start_ms: i64, end_ms: i64) -> CmdResult<Vec<CalendarEvent>> {
    let (start, end) = window_bounds(start_ms, end_ms)?;
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    step(sync, "FindItem", transport.find_calendar_view(start, end)).await
}

/// Событие целиком с сервера.
async fn fetch_details(sync: &SyncState, id: &ItemId) -> CmdResult<CalendarEventDetails> {
    let transport = sync.transport().await.map_err(|e| e.to_string())?;
    step(sync, "GetItem", transport.get_calendar_item(id)).await
}

// --- команды Tauri ---------------------------------------------------------------------------

/// События окна `[start_ms, end_ms)`: без `refresh` — кэш, что есть (`fetched_ms` — когда окно
/// забирали с сервера, `None` — кэш окно не покрывает), сервер не спрашивается; с `refresh` —
/// `FindItem` с сервера, запись в кэш и свежий кэш в ответ. Отказ сервера или входа — кэш
/// (что есть) плюс `error`; ошибка локальной базы — `Err`.
#[tauri::command]
pub(crate) async fn calendar_events(start_ms: i64, end_ms: i64, refresh: bool, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cal: State<'_, CalendarState>) -> CmdResult<CalendarRangeDto> {
    check_window(start_ms, end_ms)?;
    let lock = cal.windows.get((start_ms, end_ms));
    let _busy = lock.lock().await;
    let store = state.store.clone();
    let account = state.account;
    // Пока ждали замок, окно мог забрать первый вызывающий: кэш в ответе — уже после него.
    let age = store.calendar_range_age(account, start_ms, end_ms).map_err(err)?;
    if !refresh {
        return cached_range(&store, account, start_ms, end_ms, age, None);
    }
    let events = match fetch_window(&sync, start_ms, end_ms).await {
        Ok(events) => events,
        Err(e) => return cached_range(&store, account, start_ms, end_ms, age, Some(e)),
    };
    let fetched = events.len();
    let st = store.clone();
    tauri::async_runtime::spawn_blocking(move || st.replace_calendar_range(account, start_ms, end_ms, &events)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))?;
    info!(start_ms, end_ms, events = fetched, refresh, "окно календаря загружено с сервера");
    // Пояс пользователя в кэш (или обновить устаревший), чтобы запись его не ждала.
    crate::calendar_write::prefetch_time_zone(sync.inner().clone(), store.clone(), account);
    let rows = store.calendar_events(account, start_ms, end_ms).map_err(err)?;
    Ok(CalendarRangeDto { events: rows.into_iter().map(CalendarEventDto::from).collect(), from_cache: false, fetched_ms: Some(now_ms()), error: None })
}

/// Подробности события: из кэша, если они уже загружены и обновление не запрошено; иначе
/// `GetItem`, запись в кэш и свежие данные в ответ. Если сервер не ответил, а подробности в кэше
/// есть — они и `error`; если нет — `Err` с тем же текстом.
#[tauri::command]
pub(crate) async fn calendar_event_details(calendar_pk: i64, refresh: bool, state: State<'_, AppState>, sync: State<'_, Arc<SyncState>>, cal: State<'_, CalendarState>) -> CmdResult<CalendarEventDetailsDto> {
    let lock = cal.items.get(calendar_pk);
    let _busy = lock.lock().await;
    let store = state.store.clone();
    let row = store.calendar_event(calendar_pk).map_err(err)?.ok_or_else(|| "Событие не найдено в кэше календаря".to_string())?;
    let (row, from_cache, error) = if !refresh && row.has_details {
        (row, true, None)
    } else {
        match fetch_details(&sync, &row.id).await {
            Ok(details) => {
                let st = store.clone();
                tauri::async_runtime::spawn_blocking(move || st.store_calendar_details(calendar_pk, &details)).await.map_err(err)?.map_err(|e| format!("Ошибка локальной базы: {e}"))?;
                info!(calendar_pk, refresh, "подробности события загружены с сервера");
                let fresh = store.calendar_event(calendar_pk).map_err(err)?.ok_or_else(|| "Событие исчезло из кэша календаря".to_string())?;
                (fresh, false, None)
            }
            Err(e) if row.has_details => (row, true, Some(e)),
            Err(e) => return Err(e),
        }
    };
    // Чтение и очистка тела — в блокирующем потоке, как у писем.
    tauri::async_runtime::spawn_blocking(move || details_dto(&store, row, from_cache, error)).await.map_err(err)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(free_busy: &str, calendar_type: &str, my_response: &str) -> CalendarRow {
        CalendarRow {
            calendar_pk: 7,
            id: ItemId { id: "AAMk".into(), change_key: Some("CQAA".into()) },
            subject: Some("Планёрка".into()),
            start_at: 1_000,
            end_at: 2_000,
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            free_busy: free_busy.into(),
            calendar_type: calendar_type.into(),
            is_meeting: true,
            is_cancelled: false,
            my_response: my_response.into(),
            sensitivity: Some("Normal".into()),
            has_attachments: false,
            has_details: false,
            reminder_minutes: Some(15),
        }
    }

    #[test]
    fn enum_names_from_store_become_ui_keys() {
        let cases = [
            (("Free", "Single", "Unknown"), ("free", "single", "unknown")),
            (("Tentative", "Occurrence", "Organizer"), ("tentative", "occurrence", "organizer")),
            (("Busy", "Exception", "Tentative"), ("busy", "exception", "tentative")),
            (("OOF", "RecurringMaster", "Accept"), ("oof", "recurring_master", "accept")),
            (("WorkingElsewhere", "Single", "Decline"), ("working_elsewhere", "single", "decline")),
            (("NoData", "Single", "NoResponseReceived"), ("no_data", "single", "no_response")),
            // Уже ключи интерфейса (на случай старой записи) и мусор.
            (("working_elsewhere", "recurring_master", "no_response"), ("working_elsewhere", "recurring_master", "no_response")),
            (("", "???", "garbage"), ("no_data", "single", "unknown")),
        ];
        for ((fb, ct, mr), want) in cases {
            let dto = CalendarEventDto::from(row(fb, ct, mr));
            assert_eq!((dto.free_busy, dto.calendar_type, dto.my_response), want, "{fb}/{ct}/{mr}");
        }
        let dto = CalendarEventDto::from(row("Busy", "Single", "Accept"));
        assert_eq!((dto.calendar_pk, dto.start_ms, dto.end_ms, dto.is_meeting, dto.reminder_minutes), (7, 1_000, 2_000, true, Some(15)));
        assert!(dto.organizer.is_none(), "без имени и адреса организатора нет");
        let mut r = row("Busy", "Single", "Accept");
        r.organizer_email = Some("o@x.ru".into());
        let dto = CalendarEventDto::from(r);
        assert_eq!(dto.organizer, Some(ParticipantDto { name: None, email: Some("o@x.ru".into()) }));
    }

    #[test]
    fn attendee_dto_carries_response_key() {
        let a = Attendee { participant: Participant { name: Some("Пётр".into()), email: Some("p@x.ru".into()), routing_type: Some("SMTP".into()) }, response: ResponseType::NoResponseReceived, required: false };
        let dto = AttendeeDto::from(a);
        assert_eq!(dto, AttendeeDto { name: Some("Пётр".into()), email: Some("p@x.ru".into()), response: "no_response", required: false });
    }

    #[test]
    fn window_is_checked_before_any_work() {
        let day = 24 * 3600 * 1000;
        assert!(check_window(0, day).is_ok());
        assert!(check_window(0, 7 * day).is_ok());
        assert!(check_window(0, 0).is_err(), "пустое окно");
        assert!(check_window(day, 0).is_err(), "перевёрнутое окно");
        assert!(check_window(0, MAX_WINDOW_MS).is_ok());
        assert!(check_window(0, MAX_WINDOW_MS + 1).is_err(), "слишком большое окно");
        assert!(window_bounds(0, day).is_ok());
        assert!(window_bounds(i64::MIN, i64::MAX).is_err(), "за пределами дат jiff");
    }

    #[test]
    fn item_not_found_has_its_own_text() {
        assert_eq!(describe(&TransportError::Server { code: ITEM_NOT_FOUND.into(), message: "secret".into() }), "Событие удалено на сервере");
        assert_eq!(describe(&TransportError::SessionExpired), describe_transport(&TransportError::SessionExpired));
    }
}
