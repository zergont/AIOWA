//! Кэш календаря: экземпляры событий по окнам (`calendar_items`) и окна, которые уже забирали
//! с сервера (`calendar_ranges`). Сервер — источник истины: окно перечитывается целиком и
//! заменяет кэшированные экземпляры, пересекающие его; подробности (`GetItem`) живут, пока у
//! события не сменился `ChangeKey`. Все моменты — миллисекунды UTC.

use crate::{compress, decompress, now_ms, ts_ms, Result, Store, StoreError};
use aiowa_core::{AccountId, Attendee, CalendarEvent, CalendarEventDetails, ItemId};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::HashSet;
use tracing::debug;

/// Событие с напоминанием для планировщика: `remind_at` — момент напоминания
/// (`start_at − reminder_minutes`), миллисекунды UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderRow {
    pub calendar_pk: i64,
    /// `ItemId.id` события без `ChangeKey`: ключ состояния напоминания (`reminder_state`).
    pub server_id: String,
    pub subject: Option<String>,
    pub start_at: i64,
    pub end_at: i64,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub reminder_minutes: i64,
    pub remind_at: i64,
}

/// Состояние напоминания, выставленное пользователем: закрыто насовсем или отложено до момента
/// (миллисекунды UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderState {
    Dismissed,
    SnoozedUntil(i64),
}

impl ReminderState {
    /// Напоминание сейчас скрыто: закрыто или отложено до момента позже `now_ms`.
    pub fn hides_at(self, now_ms: i64) -> bool {
        match self {
            Self::Dismissed => true,
            Self::SnoozedUntil(until) => now_ms < until,
        }
    }
}

/// Состояние напоминания вместе с моментом напоминания (`remind_at` = `start_at − reminder_minutes`),
/// к которому оно относится: после переноса события или смены напоминания момент другой, и
/// прежнее «закрыто»/«отложено» не действует — напоминание взводится заново, как в Outlook.
/// Хранится в `sync_state` под ключом `desktop.reminder.<server_id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderMark {
    pub state: ReminderState,
    pub remind_at: i64,
}

impl ReminderMark {
    /// Значение в базе: `dismissed:<момент>` или `snoozed_until:<мс>:<момент>`.
    pub fn encode(self) -> String {
        match self.state {
            ReminderState::Dismissed => format!("dismissed:{}", self.remind_at),
            ReminderState::SnoozedUntil(ms) => format!("snoozed_until:{ms}:{}", self.remind_at),
        }
    }

    /// Разбор значения из базы; неизвестное или без момента напоминания — `None` (состояния нет).
    pub fn decode(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(':').map(str::trim);
        let kind = parts.next()?;
        let num = |p: Option<&str>| p?.parse::<i64>().ok();
        let mark = if kind.eq_ignore_ascii_case("dismissed") {
            Self { state: ReminderState::Dismissed, remind_at: num(parts.next())? }
        } else if kind.eq_ignore_ascii_case("snoozed_until") {
            Self { state: ReminderState::SnoozedUntil(num(parts.next())?), remind_at: num(parts.next())? }
        } else {
            return None;
        };
        parts.next().is_none().then_some(mark)
    }

    /// Напоминание с моментом `remind_at` сейчас скрыто: состояние относится к этому же моменту
    /// и ещё действует (закрыто или отложено до момента позже `now_ms`).
    pub fn hides(self, remind_at: i64, now_ms: i64) -> bool {
        self.remind_at == remind_at && self.state.hides_at(now_ms)
    }
}

/// Ключ состояния напоминания в `sync_state`.
pub fn reminder_state_key(server_id: &str) -> String {
    format!("desktop.reminder.{server_id}")
}

/// Ключ `sync_state` с часовым поясом пользователя из настроек OWA (Windows-идентификатор,
/// например `Russian Standard Time`): его читают окно и CLI перед записью в календарь
/// (`Store::owa_time_zone`), а с сервера (`MailTransport::user_time_zone`) берут и кладут сюда
/// (`Store::set_owa_time_zone`), когда записи нет или она старше `OWA_TIME_ZONE_TTL_MS`. Пустая
/// строка — маркер «в OWA пояс не задан»: он годен тот же срок и избавляет от запроса к серверу
/// при каждом обращении к календарю и каждой записи.
pub const OWA_TIME_ZONE_KEY: &str = "desktop.owa_timezone";

/// Срок годности кэша пояса пользователя: сутки. Смена пояса в настройках OWA (переезд,
/// командировка) подхватывается не позже чем через сутки, без ручной чистки `sync_state`.
pub const OWA_TIME_ZONE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Запись кэша пояса пользователя (`OWA_TIME_ZONE_KEY`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTimeZone {
    /// Пояс; `None` — в настройках OWA пояс не задан (в базе — пустая строка).
    pub zone: Option<String>,
    /// Запись старше `OWA_TIME_ZONE_TTL_MS`: пояс стоит перечитать с сервера, а при отказе сервера
    /// годится и эта.
    pub stale: bool,
}

impl Store {
    /// Кэш пояса пользователя с признаком устаревания; `None` — записи нет.
    pub fn owa_time_zone(&self, account: AccountId) -> Result<Option<CachedTimeZone>> {
        self.owa_time_zone_at(account, now_ms())
    }

    fn owa_time_zone_at(&self, account: AccountId, now: i64) -> Result<Option<CachedTimeZone>> {
        self.with_conn(|c| {
            let row: Option<(Option<String>, i64)> = c.query_row("SELECT value, updated_at FROM sync_state WHERE account_id = ?1 AND key = ?2", params![account.0, OWA_TIME_ZONE_KEY], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            Ok(row.map(|(value, updated_at)| CachedTimeZone { zone: value.filter(|z| !z.trim().is_empty()), stale: now - updated_at >= OWA_TIME_ZONE_TTL_MS }))
        })
    }

    /// Пояс пользователя в кэш с моментом записи «сейчас»; `None` — маркер «в OWA пояс не задан».
    pub fn set_owa_time_zone(&self, account: AccountId, zone: Option<&str>) -> Result<()> {
        self.set_state(account, OWA_TIME_ZONE_KEY, Some(zone.map(str::trim).unwrap_or("")))
    }
}

/// Строка кэша календаря для сетки и панели события. `free_busy`, `calendar_type`, `my_response`
/// хранятся именами значений EWS (`Busy`, `Occurrence`, `Accept`); их разбирают
/// `FreeBusy::parse`, `CalendarItemType::parse`, `ResponseType::parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarRow {
    pub calendar_pk: i64,
    pub id: ItemId,
    pub subject: Option<String>,
    pub start_at: i64,
    pub end_at: i64,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub free_busy: String,
    pub calendar_type: String,
    pub is_meeting: bool,
    pub is_cancelled: bool,
    pub my_response: String,
    pub sensitivity: Option<String>,
    pub has_attachments: bool,
    /// Тело и участники уже загружены (`GetItem`).
    pub has_details: bool,
    /// Минут до начала, если напоминание включено (из `FindItem`, обновляется `GetItem`).
    pub reminder_minutes: Option<i64>,
}

/// Колонки `calendar_items` в порядке, который разбирает `calendar_row`.
const CALENDAR_COLUMNS: &str = "calendar_pk, server_id, change_key, subject, start_at, end_at, is_all_day, location, organizer_name, organizer_email,
        free_busy, calendar_type, is_meeting, is_cancelled, my_response, sensitivity, has_attachments, details_at IS NOT NULL, reminder_minutes";

fn calendar_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarRow> {
    Ok(CalendarRow {
        calendar_pk: r.get(0)?,
        id: ItemId { id: r.get(1)?, change_key: r.get(2)? },
        subject: r.get(3)?,
        start_at: r.get(4)?,
        end_at: r.get(5)?,
        is_all_day: r.get::<_, i64>(6)? != 0,
        location: r.get(7)?,
        organizer_name: r.get(8)?,
        organizer_email: r.get(9)?,
        free_busy: r.get(10)?,
        calendar_type: r.get(11)?,
        is_meeting: r.get::<_, i64>(12)? != 0,
        is_cancelled: r.get::<_, i64>(13)? != 0,
        my_response: r.get(14)?,
        sensitivity: r.get(15)?,
        has_attachments: r.get::<_, i64>(16)? != 0,
        has_details: r.get::<_, i64>(17)? != 0,
        reminder_minutes: r.get(18)?,
    })
}

/// Условие «экземпляр пересекает окно `[?2, ?3)`»: начинается в окне, либо начался раньше и ещё
/// не кончился. Событие нулевой длины на границе начала окна попадает в него; событие, которое
/// кончается ровно в начале окна, — нет.
const INTERSECTS_SQL: &str = "start_at < ?3 AND (start_at >= ?2 OR end_at > ?2)";

/// Вставка или обновление экземпляра по `(account_id, server_id)`. Напоминание приходит и с
/// `FindItem`, поэтому пишется всегда; подробности (`body_html`, `attendees_json`,
/// `recurrence_text`, `details_at`) сохраняются, пока `ChangeKey` тот же (или сервер его не
/// прислал); при новом ключе сбрасываются.
const UPSERT_SQL: &str = "INSERT INTO calendar_items(account_id, server_id, change_key, subject, start_at, end_at, is_all_day, location, organizer_name, organizer_email,
        free_busy, calendar_type, is_meeting, is_cancelled, my_response, sensitivity, has_attachments, last_modified_at, fetched_at, reminder_minutes)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
     ON CONFLICT(account_id, server_id) DO UPDATE SET
       subject = excluded.subject, start_at = excluded.start_at, end_at = excluded.end_at, is_all_day = excluded.is_all_day,
       location = excluded.location, organizer_name = excluded.organizer_name, organizer_email = excluded.organizer_email,
       free_busy = excluded.free_busy, calendar_type = excluded.calendar_type, is_meeting = excluded.is_meeting,
       is_cancelled = excluded.is_cancelled, my_response = excluded.my_response, sensitivity = excluded.sensitivity,
       has_attachments = excluded.has_attachments, last_modified_at = COALESCE(excluded.last_modified_at, calendar_items.last_modified_at),
       fetched_at = excluded.fetched_at, reminder_minutes = excluded.reminder_minutes,
       body_html        = CASE WHEN excluded.change_key IS NULL OR excluded.change_key = calendar_items.change_key THEN calendar_items.body_html ELSE NULL END,
       attendees_json   = CASE WHEN excluded.change_key IS NULL OR excluded.change_key = calendar_items.change_key THEN calendar_items.attendees_json ELSE NULL END,
       recurrence_text  = CASE WHEN excluded.change_key IS NULL OR excluded.change_key = calendar_items.change_key THEN calendar_items.recurrence_text ELSE NULL END,
       details_at       = CASE WHEN excluded.change_key IS NULL OR excluded.change_key = calendar_items.change_key THEN calendar_items.details_at ELSE NULL END,
       change_key = COALESCE(excluded.change_key, calendar_items.change_key)";

fn upsert_event(tx: &Transaction, account: AccountId, ev: &CalendarEvent, now: i64) -> Result<()> {
    tx.prepare_cached(UPSERT_SQL)?.execute(params![
        account.0,
        ev.id.id,
        ev.id.change_key,
        ev.subject,
        ev.start.as_millisecond(),
        ev.end.as_millisecond(),
        ev.is_all_day as i64,
        ev.location,
        ev.organizer.as_ref().and_then(|p| p.name.as_deref()),
        ev.organizer.as_ref().and_then(|p| p.email.as_deref()),
        ev.free_busy.as_str(),
        ev.calendar_type.as_str(),
        ev.is_meeting as i64,
        ev.is_cancelled as i64,
        ev.my_response.as_str(),
        ev.sensitivity,
        ev.has_attachments as i64,
        ts_ms(ev.last_modified),
        now,
        ev.reminder_minutes
    ])?;
    Ok(())
}

/// Условие строк с напоминанием: оно включено, событие не отменено и не отклонено.
const REMINDER_SQL: &str = "reminder_minutes IS NOT NULL AND is_cancelled = 0 AND my_response != 'Decline'";

fn reminder_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReminderRow> {
    let start_at: i64 = r.get(3)?;
    let reminder_minutes: i64 = r.get(7)?;
    Ok(ReminderRow {
        calendar_pk: r.get(0)?,
        server_id: r.get(1)?,
        subject: r.get(2)?,
        start_at,
        end_at: r.get(4)?,
        is_all_day: r.get::<_, i64>(5)? != 0,
        location: r.get(6)?,
        reminder_minutes,
        remind_at: start_at - reminder_minutes * 60_000,
    })
}

/// Вычитает `[rs, re)` из каждого интервала `uncovered`.
fn subtract_range(uncovered: &mut Vec<(i64, i64)>, rs: i64, re: i64) {
    let mut next = Vec::with_capacity(uncovered.len() + 1);
    for &(s, e) in uncovered.iter() {
        if re <= s || rs >= e {
            next.push((s, e));
            continue;
        }
        if rs > s {
            next.push((s, rs));
        }
        if re < e {
            next.push((re, e));
        }
    }
    *uncovered = next;
}

impl Store {
    /// Заменяет кэш окна `[start_ms, end_ms)` набором с сервера одной транзакцией: кэшированные
    /// экземпляры, пересекающие окно, которых нет в наборе, удаляются; остальные вставляются или
    /// обновляются (подробности сохраняются при том же `ChangeKey`); окно записывается в
    /// `calendar_ranges` (окна, целиком лежащие в новом, забываются). Возвращает число
    /// записанных событий.
    pub fn replace_calendar_range(&self, account: AccountId, start_ms: i64, end_ms: i64, events: &[CalendarEvent]) -> Result<usize> {
        self.with_tx(|tx| {
            let now = now_ms();
            let keep: HashSet<&str> = events.iter().map(|e| e.id.id.as_str()).collect();
            let stale: Vec<i64> = {
                let mut st = tx.prepare_cached(&format!("SELECT calendar_pk, server_id FROM calendar_items WHERE account_id = ?1 AND {INTERSECTS_SQL}"))?;
                st.query_map(params![account.0, start_ms, end_ms], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                    .filter_map(|r| r.ok())
                    .filter(|(_, sid)| !keep.contains(sid.as_str()))
                    .map(|(pk, _)| pk)
                    .collect()
            };
            {
                let mut del = tx.prepare_cached("DELETE FROM calendar_items WHERE calendar_pk = ?1")?;
                for pk in &stale {
                    del.execute(params![pk])?;
                }
            }
            for ev in events {
                upsert_event(tx, account, ev, now)?;
            }
            tx.prepare_cached("DELETE FROM calendar_ranges WHERE account_id = ?1 AND start_at >= ?2 AND end_at <= ?3")?.execute(params![account.0, start_ms, end_ms])?;
            tx.prepare_cached("INSERT INTO calendar_ranges(account_id, start_at, end_at, fetched_at) VALUES (?1, ?2, ?3, ?4)")?.execute(params![account.0, start_ms, end_ms, now])?;
            debug!(start_ms, end_ms, events = events.len(), removed = stale.len(), "кэш календаря: окно заменено");
            Ok(events.len())
        })
    }

    /// Экземпляры, пересекающие окно `[start_ms, end_ms)`, по началу (затем по концу и ключу).
    pub fn calendar_events(&self, account: AccountId, start_ms: i64, end_ms: i64) -> Result<Vec<CalendarRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {CALENDAR_COLUMNS} FROM calendar_items WHERE account_id = ?1 AND {INTERSECTS_SQL} ORDER BY start_at, end_at, calendar_pk"))?;
            let rows = st.query_map(params![account.0, start_ms, end_ms], calendar_row)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Миллисекунд с последней загрузки окна, покрывающего `[start_ms, end_ms)` (в том числе
    /// объединением нескольких загруженных окон): возраст самой старой части покрытия.
    /// `None` — окно не покрыто целиком.
    pub fn calendar_range_age(&self, account: AccountId, start_ms: i64, end_ms: i64) -> Result<Option<i64>> {
        if end_ms <= start_ms {
            return Ok(Some(0));
        }
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT start_at, end_at, fetched_at FROM calendar_ranges WHERE account_id = ?1 AND start_at < ?3 AND end_at > ?2 ORDER BY fetched_at DESC")?;
            let ranges: Vec<(i64, i64, i64)> = st.query_map(params![account.0, start_ms, end_ms], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
            let mut uncovered = vec![(start_ms, end_ms)];
            for (rs, re, fetched) in ranges {
                subtract_range(&mut uncovered, rs, re);
                if uncovered.is_empty() {
                    return Ok(Some((now_ms() - fetched).max(0)));
                }
            }
            Ok(None)
        })
    }

    /// Вставляет или обновляет одно событие (после `CreateItem`/`UpdateItem`, чтобы кэш не ждал
    /// перечитывания окна); подробности сохраняются при том же `ChangeKey`. Возвращает `calendar_pk`.
    pub fn upsert_calendar_event(&self, account: AccountId, event: &CalendarEvent) -> Result<i64> {
        self.with_tx(|tx| {
            upsert_event(tx, account, event, now_ms())?;
            let pk: i64 = tx.prepare_cached("SELECT calendar_pk FROM calendar_items WHERE account_id = ?1 AND server_id = ?2")?.query_row(params![account.0, event.id.id], |r| r.get(0))?;
            Ok(pk)
        })
    }

    /// Минуты напоминания события из кэша (`None` — напоминания нет или события нет).
    pub fn calendar_reminder_minutes(&self, calendar_pk: i64) -> Result<Option<i64>> {
        self.with_conn(|c| Ok(c.prepare_cached("SELECT reminder_minutes FROM calendar_items WHERE calendar_pk = ?1")?.query_row(params![calendar_pk], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten()))
    }

    /// Удаляет событие из кэша (после `DeleteItem`); `false` — его и не было.
    pub fn remove_calendar_event(&self, calendar_pk: i64) -> Result<bool> {
        self.with_conn(|c| Ok(c.prepare_cached("DELETE FROM calendar_items WHERE calendar_pk = ?1")?.execute(params![calendar_pk])? > 0))
    }

    /// События с включённым напоминанием (не отменённые, не отклонённые), у которых момент
    /// напоминания `start_at − reminder_minutes` лежит в окне `[from_ms, to_ms)`; по моменту
    /// напоминания. Закрытые и отложенные напоминания здесь не отсеиваются — см. `reminder_states`.
    pub fn calendar_events_with_reminders(&self, account: AccountId, from_ms: i64, to_ms: i64) -> Result<Vec<ReminderRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!(
                "SELECT calendar_pk, server_id, subject, start_at, end_at, is_all_day, location, reminder_minutes
                 FROM calendar_items WHERE account_id = ?1 AND {REMINDER_SQL}
                   AND start_at - reminder_minutes * 60000 >= ?2 AND start_at - reminder_minutes * 60000 < ?3
                 ORDER BY start_at - reminder_minutes * 60000, start_at, calendar_pk"
            ))?;
            let rows = st.query_map(params![account.0, from_ms, to_ms], reminder_row)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Состояние напоминания события (`desktop.reminder.<server_id>` в `sync_state`) с моментом
    /// напоминания, к которому оно относится.
    pub fn reminder_state(&self, account: AccountId, server_id: &str) -> Result<Option<ReminderMark>> {
        Ok(self.get_state(account, &reminder_state_key(server_id))?.as_deref().and_then(ReminderMark::decode))
    }

    /// Записывает состояние напоминания; `None` — снимает (напоминание снова показывается).
    pub fn set_reminder_state(&self, account: AccountId, server_id: &str, state: Option<ReminderMark>) -> Result<()> {
        let key = reminder_state_key(server_id);
        match state {
            Some(s) => self.set_state(account, &key, Some(&s.encode())),
            None => self.with_conn(|c| {
                c.execute("DELETE FROM sync_state WHERE account_id = ?1 AND key = ?2", params![account.0, key])?;
                Ok(())
            }),
        }
    }

    /// Все состояния напоминаний аккаунта: `(server_id, состояние с моментом)`; нечитаемые
    /// значения пропускаются.
    pub fn reminder_states(&self, account: AccountId) -> Result<Vec<(String, ReminderMark)>> {
        let prefix = reminder_state_key("");
        self.with_conn(|c| {
            let mut st = c.prepare_cached("SELECT key, value FROM sync_state WHERE account_id = ?1 AND key LIKE ?2 ESCAPE '\\'")?;
            let pattern = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            let rows = st.query_map(params![account.0, pattern], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (key, value) = row?;
                if let (Some(id), Some(state)) = (key.strip_prefix(&prefix), value.as_deref().and_then(ReminderMark::decode)) {
                    out.push((id.to_string(), state));
                }
            }
            Ok(out)
        })
    }

    /// Забывает состояния напоминаний событий, которых больше нет в кэше (чтобы `sync_state`
    /// не рос); возвращает число удалённых.
    pub fn prune_reminder_states(&self, account: AccountId) -> Result<usize> {
        let prefix = reminder_state_key("");
        self.with_conn(|c| {
            let pattern = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            let n = c.execute(
                "DELETE FROM sync_state WHERE account_id = ?1 AND key LIKE ?2 ESCAPE '\\'
                   AND NOT EXISTS (SELECT 1 FROM calendar_items ci WHERE ci.account_id = ?1 AND ?3 || ci.server_id = sync_state.key)",
                params![account.0, pattern, prefix],
            )?;
            Ok(n)
        })
    }

    /// Один экземпляр по ключу.
    pub fn calendar_event(&self, calendar_pk: i64) -> Result<Option<CalendarRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {CALENDAR_COLUMNS} FROM calendar_items WHERE calendar_pk = ?1"))?;
            Ok(st.query_row(params![calendar_pk], calendar_row).optional()?)
        })
    }

    /// Один экземпляр по идентификатору на сервере (`ItemId.id` без `ChangeKey`): так письмо о
    /// встрече находит связанное событие (`AssociatedCalendarItemId`) в кэше.
    pub fn calendar_event_by_server_id(&self, account: AccountId, server_id: &str) -> Result<Option<CalendarRow>> {
        self.with_conn(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {CALENDAR_COLUMNS} FROM calendar_items WHERE account_id = ?1 AND server_id = ?2"))?;
            Ok(st.query_row(params![account.0, server_id], calendar_row).optional()?)
        })
    }

    /// Записывает подробности события (`GetItem`): тело (сжатое), участники, напоминание, текст
    /// повторения и свежие поля самого события вместе с его `ChangeKey`.
    pub fn store_calendar_details(&self, calendar_pk: i64, details: &CalendarEventDetails) -> Result<()> {
        let ev = &details.event;
        let body_html = details.body.as_ref().and_then(|b| b.full_body.as_deref().or(b.unique_body.as_deref())).map(compress).transpose()?;
        let attendees_json = serde_json::to_string(&details.attendees).map_err(|e| StoreError::Other(format!("участники в JSON: {e}")))?;
        self.with_conn(|c| {
            let now = now_ms();
            let n = c
                .prepare_cached(
                    "UPDATE calendar_items SET change_key = COALESCE(?2, change_key), subject = ?3, start_at = ?4, end_at = ?5, is_all_day = ?6, location = ?7,
                        organizer_name = ?8, organizer_email = ?9, free_busy = ?10, calendar_type = ?11, is_meeting = ?12, is_cancelled = ?13, my_response = ?14,
                        sensitivity = ?15, has_attachments = ?16, last_modified_at = COALESCE(?17, last_modified_at),
                        body_html = ?18, attendees_json = ?19, reminder_minutes = ?20, recurrence_text = ?21, details_at = ?22
                     WHERE calendar_pk = ?1",
                )?
                .execute(params![
                    calendar_pk,
                    ev.id.change_key,
                    ev.subject,
                    ev.start.as_millisecond(),
                    ev.end.as_millisecond(),
                    ev.is_all_day as i64,
                    ev.location,
                    ev.organizer.as_ref().and_then(|p| p.name.as_deref()),
                    ev.organizer.as_ref().and_then(|p| p.email.as_deref()),
                    ev.free_busy.as_str(),
                    ev.calendar_type.as_str(),
                    ev.is_meeting as i64,
                    ev.is_cancelled as i64,
                    ev.my_response.as_str(),
                    ev.sensitivity,
                    ev.has_attachments as i64,
                    ts_ms(ev.last_modified),
                    body_html,
                    attendees_json,
                    details.reminder_minutes,
                    details.recurrence_text,
                    now
                ])?;
            if n == 0 {
                return Err(StoreError::Other(format!("событие #{calendar_pk} не найдено в кэше календаря")));
            }
            Ok(())
        })
    }

    /// Подробности события из кэша: (тело HTML, участники, минуты напоминания, текст повторения).
    /// `None` — события нет или подробности ещё не загружались.
    #[allow(clippy::type_complexity)]
    pub fn calendar_details(&self, calendar_pk: i64) -> Result<Option<(Option<String>, Vec<Attendee>, Option<i64>, Option<String>)>> {
        self.with_conn(|c| {
            let row: Option<(Option<Vec<u8>>, Option<String>, Option<i64>, Option<String>, Option<i64>)> = c
                .prepare_cached("SELECT body_html, attendees_json, reminder_minutes, recurrence_text, details_at FROM calendar_items WHERE calendar_pk = ?1")?
                .query_row(params![calendar_pk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional()?;
            let Some((body, attendees, reminder, recurrence, details_at)) = row else { return Ok(None) };
            if details_at.is_none() {
                return Ok(None);
            }
            let body = body.as_deref().map(decompress).transpose()?;
            let attendees: Vec<Attendee> = attendees.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
            Ok(Some((body, attendees, reminder, recurrence)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiowa_core::{BodyType, CalendarItemType, FreeBusy, MessageBody, Participant, ResponseType};
    use jiff::Timestamp;

    fn tmp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("aiowa-store-cal-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(dir.join("mail.db")).unwrap()
    }

    fn ms(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_millisecond()
    }

    fn event(id: &str, ck: &str, start: &str, end: &str) -> CalendarEvent {
        CalendarEvent {
            id: ItemId { id: id.into(), change_key: Some(ck.into()) },
            subject: Some(format!("Событие {id}")),
            start: start.parse().unwrap(),
            end: end.parse().unwrap(),
            is_all_day: false,
            location: Some("Зал".into()),
            organizer: Some(Participant { name: Some("Иван".into()), email: Some("i@x.ru".into()), routing_type: None }),
            free_busy: FreeBusy::Busy,
            calendar_type: CalendarItemType::Single,
            is_meeting: true,
            is_cancelled: false,
            my_response: ResponseType::Accept,
            sensitivity: Some("Normal".into()),
            has_attachments: false,
            last_modified: None,
            reminder_minutes: Some(15),
        }
    }

    fn details(ev: CalendarEvent, body: &str) -> CalendarEventDetails {
        CalendarEventDetails {
            event: ev,
            body: Some(MessageBody { body_type: BodyType::Html, unique_body: None, full_body: Some(body.into()), is_truncated: false }),
            attendees: vec![
                Attendee { participant: Participant { name: Some("Пётр".into()), email: Some("p@x.ru".into()), routing_type: Some("SMTP".into()) }, response: ResponseType::Tentative, required: true },
                Attendee { participant: Participant { name: None, email: Some("o@x.ru".into()), routing_type: None }, response: ResponseType::NoResponseReceived, required: false },
            ],
            reminder_minutes: Some(15),
            recurrence_text: Some("Каждую неделю: пн".into()),
        }
    }

    #[test]
    fn replace_range_keeps_details_on_same_change_key() {
        let s = tmp_store("replace");
        let acc = s.account("h", "u").unwrap();
        let (mon, tue, wed) = (ms("2026-10-19T00:00:00Z"), ms("2026-10-20T00:00:00Z"), ms("2026-10-21T00:00:00Z"));
        let a = event("A", "a1", "2026-10-19T07:00:00Z", "2026-10-19T08:00:00Z");
        let b = event("B", "b1", "2026-10-19T09:00:00Z", "2026-10-19T10:00:00Z");
        let c = event("C", "c1", "2026-10-19T11:00:00Z", "2026-10-19T12:00:00Z");
        let outside = event("X", "x1", "2026-10-20T07:00:00Z", "2026-10-20T08:00:00Z");
        assert_eq!(s.replace_calendar_range(acc, mon, tue, &[a.clone(), b.clone(), c.clone()]).unwrap(), 3);
        assert_eq!(s.replace_calendar_range(acc, tue, wed, std::slice::from_ref(&outside)).unwrap(), 1);
        let rows = s.calendar_events(acc, mon, tue).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.id.as_str()).collect::<Vec<_>>(), vec!["A", "B", "C"]);
        assert_eq!((rows[0].free_busy.as_str(), rows[0].calendar_type.as_str(), rows[0].my_response.as_str()), ("Busy", "Single", "Accept"));
        assert_eq!((rows[0].organizer_email.as_deref(), rows[0].location.as_deref(), rows[0].is_meeting, rows[0].has_details), (Some("i@x.ru"), Some("Зал"), true, false));
        let a_pk = rows[0].calendar_pk;
        let b_pk = rows[1].calendar_pk;

        // Подробности у A и B; событие по ключу видит их.
        s.store_calendar_details(a_pk, &details(a.clone(), "<p>Повестка A</p>")).unwrap();
        s.store_calendar_details(b_pk, &details(b.clone(), "<p>Повестка B</p>")).unwrap();
        assert!(s.calendar_event(a_pk).unwrap().unwrap().has_details);
        let (body, attendees, reminder, recurrence) = s.calendar_details(a_pk).unwrap().unwrap();
        assert_eq!(body.as_deref(), Some("<p>Повестка A</p>"));
        assert_eq!(attendees.len(), 2);
        assert_eq!((attendees[0].participant.email.as_deref(), attendees[0].response, attendees[0].required), (Some("p@x.ru"), ResponseType::Tentative, true));
        assert_eq!((attendees[1].response, attendees[1].required), (ResponseType::NoResponseReceived, false));
        assert_eq!((reminder, recurrence.as_deref()), (Some(15), Some("Каждую неделю: пн")));
        assert!(s.calendar_details(rows[2].calendar_pk).unwrap().is_none(), "у C подробностей нет");
        assert!(s.calendar_details(999_999).unwrap().is_none());
        assert!(s.store_calendar_details(999_999, &details(a.clone(), "x")).is_err());

        // Новая выдача окна: C исчезло, A с тем же ключом (подробности живут, тема обновилась),
        // B с новым ключом (подробности сброшены), D новое. Соседнее окно не тронуто.
        let mut a2 = a.clone();
        a2.subject = Some("Событие A (перенос)".into());
        let mut b2 = b.clone();
        b2.id.change_key = Some("b2".into());
        let d = event("D", "d1", "2026-10-19T13:00:00Z", "2026-10-19T14:00:00Z");
        assert_eq!(s.replace_calendar_range(acc, mon, tue, &[a2, b2, d]).unwrap(), 3);
        let rows = s.calendar_events(acc, mon, tue).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.id.as_str()).collect::<Vec<_>>(), vec!["A", "B", "D"]);
        assert_eq!(rows[0].calendar_pk, a_pk, "строка та же, ключ не меняется");
        assert_eq!(rows[0].subject.as_deref(), Some("Событие A (перенос)"));
        assert!(rows[0].has_details && !rows[1].has_details && !rows[2].has_details);
        assert_eq!(rows[1].id.change_key.as_deref(), Some("b2"));
        assert!(s.calendar_details(b_pk).unwrap().is_none(), "подробности B сброшены вместе с ключом");
        assert_eq!(s.calendar_details(a_pk).unwrap().unwrap().0.as_deref(), Some("<p>Повестка A</p>"));
        assert_eq!(s.calendar_events(acc, tue, wed).unwrap().len(), 1, "соседнее окно не тронуто");
        // Напоминание из FindItem пишется всегда, и при новом ключе тоже (B получило 15 из выдачи).
        let reminders = s.calendar_events_with_reminders(acc, mon, tue).unwrap();
        assert_eq!(reminders.iter().map(|r| r.calendar_pk).collect::<Vec<_>>(), vec![a_pk, b_pk, rows[2].calendar_pk], "D тоже с напоминанием");
        assert_eq!((reminders[0].reminder_minutes, reminders[0].remind_at), (15, ms("2026-10-19T06:45:00Z")));

        // Подробности с новым ключом от GetItem обновляют и ключ, и поля события.
        let mut fresh = s.calendar_event(b_pk).unwrap().unwrap();
        let mut b3 = b.clone();
        b3.id.change_key = Some("b3".into());
        b3.is_cancelled = true;
        s.store_calendar_details(b_pk, &details(b3, "<p>B</p>")).unwrap();
        fresh = s.calendar_event(fresh.calendar_pk).unwrap().unwrap();
        assert_eq!((fresh.id.change_key.as_deref(), fresh.is_cancelled, fresh.has_details), (Some("b3"), true, true));
        // Пустое окно с сервера очищает кэш окна.
        assert_eq!(s.replace_calendar_range(acc, mon, wed, &[]).unwrap(), 0);
        assert!(s.calendar_events(acc, mon, wed).unwrap().is_empty());
    }

    #[test]
    fn events_by_intersection() {
        let s = tmp_store("intersect");
        let acc = s.account("h", "u").unwrap();
        let (day_start, day_end) = (ms("2026-10-20T00:00:00Z"), ms("2026-10-21T00:00:00Z"));
        let mut all_day = event("ALL", "k", "2026-10-20T00:00:00Z", "2026-10-21T00:00:00Z");
        all_day.is_all_day = true;
        let events = vec![
            all_day,
            event("NIGHT", "k", "2026-10-19T23:00:00Z", "2026-10-20T01:00:00Z"),
            event("ENDS_AT_START", "k", "2026-10-19T22:00:00Z", "2026-10-20T00:00:00Z"),
            event("ZERO_AT_START", "k", "2026-10-20T00:00:00Z", "2026-10-20T00:00:00Z"),
            event("STARTS_AT_END", "k", "2026-10-21T00:00:00Z", "2026-10-21T01:00:00Z"),
            event("LATE", "k", "2026-10-20T23:30:00Z", "2026-10-21T00:30:00Z"),
            event("MULTI", "k", "2026-10-18T00:00:00Z", "2026-10-23T00:00:00Z"),
        ];
        s.replace_calendar_range(acc, ms("2026-10-18T00:00:00Z"), ms("2026-10-24T00:00:00Z"), &events).unwrap();
        let ids = |a: i64, b: i64| s.calendar_events(acc, a, b).unwrap().into_iter().map(|r| r.id.id).collect::<Vec<_>>();
        assert_eq!(ids(day_start, day_end), vec!["MULTI", "NIGHT", "ZERO_AT_START", "ALL", "LATE"], "по началу, затем по концу; конец ровно в начале окна и начало ровно в конце — вне окна");
        assert_eq!(ids(ms("2026-10-21T00:00:00Z"), ms("2026-10-22T00:00:00Z")), vec!["MULTI", "LATE", "STARTS_AT_END"], "событие через полночь видно в обоих днях");
        assert_eq!(ids(ms("2026-10-20T12:00:00Z"), ms("2026-10-20T13:00:00Z")), vec!["MULTI", "ALL"]);
        assert!(s.calendar_events(acc, ms("2026-10-25T00:00:00Z"), ms("2026-10-26T00:00:00Z")).unwrap().is_empty());
        assert!(s.calendar_events(acc, day_start, day_end).unwrap().iter().find(|r| r.id.id == "ALL").unwrap().is_all_day);
    }

    #[test]
    fn upsert_remove_and_reminders() {
        let s = tmp_store("reminders");
        let acc = s.account("h", "u").unwrap();
        let (mon, tue) = (ms("2026-10-19T00:00:00Z"), ms("2026-10-20T00:00:00Z"));
        // Одиночная вставка после CreateItem: строка видна в окне, напоминание записано.
        let a = event("A", "a1", "2026-10-19T07:00:00Z", "2026-10-19T08:00:00Z");
        let a_pk = s.upsert_calendar_event(acc, &a).unwrap();
        assert_eq!(s.calendar_events(acc, mon, tue).unwrap().len(), 1);
        assert!(s.calendar_range_age(acc, mon, tue).unwrap().is_none(), "одиночная вставка окно не покрывает");
        let rem = s.calendar_events_with_reminders(acc, mon, tue).unwrap();
        assert_eq!((rem.len(), rem[0].calendar_pk, rem[0].server_id.as_str(), rem[0].remind_at), (1, a_pk, "A", ms("2026-10-19T06:45:00Z")));
        assert_eq!((rem[0].subject.as_deref(), rem[0].location.as_deref(), rem[0].is_all_day, rem[0].end_at), (Some("Событие A"), Some("Зал"), false, ms("2026-10-19T08:00:00Z")));
        // Повторная вставка того же события с новым ключом — тот же pk, подробности сброшены, поля обновлены.
        s.store_calendar_details(a_pk, &details(a.clone(), "<p>A</p>")).unwrap();
        let mut a2 = a.clone();
        a2.id.change_key = Some("a2".into());
        a2.start = "2026-10-19T09:00:00Z".parse().unwrap();
        a2.end = "2026-10-19T09:45:00Z".parse().unwrap();
        a2.reminder_minutes = Some(30);
        assert_eq!(s.upsert_calendar_event(acc, &a2).unwrap(), a_pk);
        let row = s.calendar_event(a_pk).unwrap().unwrap();
        assert_eq!((row.start_at, row.has_details, row.id.change_key.as_deref()), (ms("2026-10-19T09:00:00Z"), false, Some("a2")));
        assert_eq!(s.calendar_reminder_minutes(a_pk).unwrap(), Some(30));
        assert_eq!(row.reminder_minutes, Some(30), "напоминание есть и в строке кэша");
        assert_eq!(s.calendar_reminder_minutes(999_999).unwrap(), None);
        assert_eq!(s.calendar_event_by_server_id(acc, "A").unwrap().map(|r| r.calendar_pk), Some(a_pk));
        assert!(s.calendar_event_by_server_id(acc, "NOPE").unwrap().is_none());
        assert_eq!(s.calendar_events_with_reminders(acc, mon, tue).unwrap()[0].remind_at, ms("2026-10-19T08:30:00Z"));
        // Окно по моменту напоминания: [08:30, 08:31) находит, [08:31, …) — нет; без напоминания,
        // отменённое и отклонённое не попадают.
        assert_eq!(s.calendar_events_with_reminders(acc, ms("2026-10-19T08:30:00Z"), ms("2026-10-19T08:31:00Z")).unwrap().len(), 1);
        assert!(s.calendar_events_with_reminders(acc, ms("2026-10-19T08:31:00Z"), tue).unwrap().is_empty());
        let mut none = event("N", "n1", "2026-10-19T10:00:00Z", "2026-10-19T11:00:00Z");
        none.reminder_minutes = None;
        let mut cancelled = event("C", "c1", "2026-10-19T10:00:00Z", "2026-10-19T11:00:00Z");
        cancelled.is_cancelled = true;
        let mut declined = event("D", "d1", "2026-10-19T10:00:00Z", "2026-10-19T11:00:00Z");
        declined.my_response = ResponseType::Decline;
        let mut zero = event("Z", "z1", "2026-10-19T10:00:00Z", "2026-10-19T11:00:00Z");
        zero.reminder_minutes = Some(0);
        for ev in [&none, &cancelled, &declined, &zero] {
            s.upsert_calendar_event(acc, ev).unwrap();
        }
        let rem = s.calendar_events_with_reminders(acc, mon, tue).unwrap();
        assert_eq!(rem.iter().map(|r| r.server_id.as_str()).collect::<Vec<_>>(), vec!["A", "Z"]);
        assert_eq!(rem[1].remind_at, rem[1].start_at, "напоминание «в момент начала»");
        // Состояния напоминаний в KV (с моментом напоминания): закрыто, отложено, снято; обход и чистка.
        let remind_a = ms("2026-10-19T08:30:00Z");
        let dismissed_a = ReminderMark { state: ReminderState::Dismissed, remind_at: remind_a };
        let snoozed_z = ReminderMark { state: ReminderState::SnoozedUntil(ms("2026-10-19T10:05:00Z")), remind_at: ms("2026-10-19T10:00:00Z") };
        assert_eq!(s.reminder_state(acc, "A").unwrap(), None);
        s.set_reminder_state(acc, "A", Some(dismissed_a)).unwrap();
        s.set_reminder_state(acc, "Z", Some(snoozed_z)).unwrap();
        s.set_reminder_state(acc, "GONE", Some(dismissed_a)).unwrap();
        assert_eq!(s.reminder_state(acc, "A").unwrap(), Some(dismissed_a));
        assert_eq!(s.reminder_state(acc, "Z").unwrap(), Some(snoozed_z));
        assert_eq!(s.get_state(acc, "desktop.reminder.Z").unwrap(), Some(format!("snoozed_until:{}:{}", ms("2026-10-19T10:05:00Z"), ms("2026-10-19T10:00:00Z"))));
        assert_eq!(s.get_state(acc, "desktop.reminder.A").unwrap(), Some(format!("dismissed:{remind_a}")));
        let mut states = s.reminder_states(acc).unwrap();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(states.len(), 3);
        assert_eq!(states[0], ("A".to_string(), dismissed_a));
        assert_eq!(s.prune_reminder_states(acc).unwrap(), 1, "состояние события, которого нет в кэше, забыто");
        assert_eq!(s.reminder_states(acc).unwrap().len(), 2);
        s.set_reminder_state(acc, "A", None).unwrap();
        assert_eq!(s.reminder_state(acc, "A").unwrap(), None);
        // Значение старого вида (без момента) не читается: состояния нет.
        s.set_state(acc, "desktop.reminder.A", Some("dismissed")).unwrap();
        assert_eq!(s.reminder_state(acc, "A").unwrap(), None);
        assert_eq!(s.reminder_states(acc).unwrap().len(), 1);
        assert!(ReminderState::Dismissed.hides_at(0));
        assert!(ReminderState::SnoozedUntil(100).hides_at(99) && !ReminderState::SnoozedUntil(100).hides_at(100));
        assert_eq!(ReminderMark::decode("snoozed_until:5:7"), Some(ReminderMark { state: ReminderState::SnoozedUntil(5), remind_at: 7 }));
        assert_eq!(ReminderMark::decode(" dismissed : 9 "), Some(ReminderMark { state: ReminderState::Dismissed, remind_at: 9 }));
        assert_eq!(ReminderMark::decode("dismissed"), None);
        assert_eq!(ReminderMark::decode("snoozed_until:5"), None);
        assert_eq!(ReminderMark::decode("dismissed:9:1"), None);
        assert_eq!(ReminderMark::decode("что-то"), None);
        // Состояние действует только для своего момента: после переноса события оно не скрывает напоминание.
        assert!(dismissed_a.hides(remind_a, 0));
        assert!(!dismissed_a.hides(remind_a + 1, 0));
        assert!(snoozed_z.hides(ms("2026-10-19T10:00:00Z"), ms("2026-10-19T10:04:00Z")));
        assert!(!snoozed_z.hides(ms("2026-10-19T10:00:00Z"), ms("2026-10-19T10:05:00Z")));
        // Удаление из кэша после DeleteItem.
        assert!(s.remove_calendar_event(a_pk).unwrap());
        assert!(!s.remove_calendar_event(a_pk).unwrap());
        assert!(s.calendar_event(a_pk).unwrap().is_none());
        assert_eq!(s.calendar_events(acc, mon, tue).unwrap().len(), 4);
    }

    #[test]
    fn range_age_coverage() {
        let s = tmp_store("ranges");
        let acc = s.account("h", "u").unwrap();
        let d = |n: i64| ms("2026-10-19T00:00:00Z") + n * 24 * 3600 * 1000;
        assert!(s.calendar_range_age(acc, d(0), d(7)).unwrap().is_none(), "ничего не загружали");
        s.replace_calendar_range(acc, d(0), d(7), &[]).unwrap();
        let age = s.calendar_range_age(acc, d(0), d(7)).unwrap().expect("неделя загружена");
        assert!((0..60_000).contains(&age), "{age}");
        assert!(s.calendar_range_age(acc, d(2), d(3)).unwrap().is_some(), "день внутри недели покрыт");
        assert!(s.calendar_range_age(acc, d(5), d(9)).unwrap().is_none(), "окно выходит за неделю");
        // Соседнее окно: объединение покрывает двухнедельный запрос, возраст — по старшему окну.
        s.with_conn(|c| {
            c.execute("UPDATE calendar_ranges SET fetched_at = fetched_at - 600000", [])?;
            Ok(())
        })
        .unwrap();
        s.replace_calendar_range(acc, d(7), d(14), &[]).unwrap();
        let two_weeks = s.calendar_range_age(acc, d(0), d(14)).unwrap().expect("объединение двух окон");
        assert!(two_weeks >= 600_000, "возраст самой старой части: {two_weeks}");
        let second = s.calendar_range_age(acc, d(7), d(14)).unwrap().unwrap();
        assert!(second < 600_000, "{second}");
        assert!(s.calendar_range_age(acc, d(3), d(10)).unwrap().is_some(), "окно через стык двух загруженных");
        // Разрыв между окнами — не покрыто.
        s.replace_calendar_range(acc, d(21), d(28), &[]).unwrap();
        assert!(s.calendar_range_age(acc, d(10), d(25)).unwrap().is_none());
        // Повторная загрузка той же недели поглощает старую запись окна и молодит возраст.
        s.replace_calendar_range(acc, d(0), d(7), &[]).unwrap();
        assert!(s.calendar_range_age(acc, d(0), d(14)).unwrap().unwrap() < 600_000);
        let n: i64 = s.debug_query("SELECT COUNT(*) FROM calendar_ranges").unwrap()[1].parse().unwrap();
        assert_eq!(n, 3, "старое окно той же недели забыто");
        // Загрузка месяца поглощает недели внутри него.
        s.replace_calendar_range(acc, d(-5), d(30), &[]).unwrap();
        let n: i64 = s.debug_query("SELECT COUNT(*) FROM calendar_ranges").unwrap()[1].parse().unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.calendar_range_age(acc, d(3), d(3)).unwrap(), Some(0), "пустое окно покрыто всегда");
        let mut u = vec![(0, 10)];
        subtract_range(&mut u, 3, 5);
        assert_eq!(u, vec![(0, 3), (5, 10)]);
        subtract_range(&mut u, -1, 4);
        assert_eq!(u, vec![(5, 10)]);
        subtract_range(&mut u, 5, 10);
        assert!(u.is_empty());
    }

    #[test]
    fn owa_time_zone_cache_expires_and_keeps_unset_marker() {
        let s = tmp_store("tz");
        let acc = s.account("h", "u").unwrap();
        assert_eq!(s.owa_time_zone(acc).unwrap(), None, "записи нет");
        s.set_owa_time_zone(acc, Some(" Russian Standard Time ")).unwrap();
        let now = now_ms();
        assert_eq!(s.owa_time_zone_at(acc, now).unwrap(), Some(CachedTimeZone { zone: Some("Russian Standard Time".into()), stale: false }));
        // Запас в секунду: между записью и `now` часы успевают тикнуть на миллисекунды.
        assert!(!s.owa_time_zone_at(acc, now + OWA_TIME_ZONE_TTL_MS - 1000).unwrap().unwrap().stale, "почти сутки — ещё годен");
        assert!(s.owa_time_zone_at(acc, now + OWA_TIME_ZONE_TTL_MS + 1000).unwrap().unwrap().stale, "старше суток — перечитать");
        // «В OWA пояс не задан» — тоже запись, годная сутки: сервер не спрашивается на каждом окне.
        s.set_owa_time_zone(acc, None).unwrap();
        assert_eq!(s.get_state(acc, OWA_TIME_ZONE_KEY).unwrap(), Some(String::new()));
        assert_eq!(s.owa_time_zone_at(acc, now_ms()).unwrap(), Some(CachedTimeZone { zone: None, stale: false }));
        assert!(s.owa_time_zone_at(acc, now_ms() + OWA_TIME_ZONE_TTL_MS + 1000).unwrap().unwrap().stale);
    }
}
