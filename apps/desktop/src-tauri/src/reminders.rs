//! Напоминания о событиях календаря. Планировщик раз в `TICK` смотрит кэш календаря (сети нет;
//! кэш ближайших суток держит свежим окно, см. `App.svelte`) и, когда наступает момент напоминания
//! (`start − reminder_minutes`), шлёт окну событие `reminder` со всеми актуальными напоминаниями
//! и показывает тост — один на событие (повторно только после «отложить»). Событие `reminder`
//! уходит и тогда, когда состав актуальных изменился без новых (событие перенесено, удалено,
//! закрыто): список в нём полный, окно заменяет им свой. Через `OVERDUE_MS` после начала
//! напоминание просрочено и больше не показывается. «Закрыть» и «отложить» хранятся в базе
//! (`sync_state`, ключ `desktop.reminder.<server_id>`) вместе с моментом напоминания, к которому
//! относятся: перенос события или смена напоминания взводят его заново; состояния переживают
//! перезапуск, состояния событий, исчезнувших из кэша, чистятся раз в час. Команды записи
//! календаря, «закрыть» и «отложить» дёргают планировщик (`poke`), чтобы не ждать тика. Тосты —
//! той же настройкой, что у писем (`NotifyState`), но без ограничения частоты: одно событие — один
//! тост, несколько сразу — одна сводка.
//!
//! В журнал попадают только счётчики: ни тем, ни мест.

use crate::notify::NotifyState;
use crate::{err, CmdResult};
use aiowa_core::AccountId;
use aiowa_store::{ReminderMark, ReminderRow, ReminderState, Store};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Период проверки кэша.
pub(crate) const TICK: Duration = Duration::from_secs(30);
/// Через столько после начала события напоминание просрочено.
pub(crate) const OVERDUE_MS: i64 = 30 * 60 * 1000;
/// Насколько назад искать моменты напоминаний: самое долгое напоминание (четыре недели,
/// `calendar_write::MAX_REMINDER_MINUTES`) плюс запас.
const LOOKBACK_MS: i64 = 31 * 24 * 3600 * 1000;
/// Тиков между чистками состояний исчезнувших событий (раз в час).
const PRUNE_EVERY: u32 = 120;
/// «Отложить» принимает от минуты до недели.
const MAX_SNOOZE_MINUTES: i64 = 7 * 24 * 60;
/// Сколько тем перечислять в сводке.
const SUMMARY_SUBJECTS: usize = 3;
const REMINDER_EVENT: &str = "reminder";

fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

// --- DTO ------------------------------------------------------------------------------------

/// Напоминание для окна: событие и сколько минут до его начала (отрицательное — уже началось).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReminderDto {
    calendar_pk: i64,
    subject: Option<String>,
    start_ms: i64,
    end_ms: i64,
    is_all_day: bool,
    location: Option<String>,
    minutes_left: i64,
}

/// Событие окну `reminder`: все напоминания, которые сейчас пора показывать.
#[derive(Debug, Clone, Serialize)]
struct ReminderEvent {
    items: Vec<ReminderDto>,
}

// --- чистая логика (время — параметр) -----------------------------------------------------------

/// Минут до начала: целых вниз (за полминуты до начала — 0, через полминуты после — −1).
pub(crate) fn minutes_left(start_at: i64, now_ms: i64) -> i64 {
    (start_at - now_ms).div_euclid(60_000)
}

fn dto(r: &ReminderRow, now_ms: i64) -> ReminderDto {
    ReminderDto { calendar_pk: r.calendar_pk, subject: r.subject.clone(), start_ms: r.start_at, end_ms: r.end_at, is_all_day: r.is_all_day, location: r.location.clone(), minutes_left: minutes_left(r.start_at, now_ms) }
}

/// Напоминания, которые пора показывать в момент `now_ms`: момент напоминания наступил, событие
/// ещё не просрочено (начало + `OVERDUE_MS` впереди), пользователь его не закрыл и не отложил на
/// потом (состояние считается только для того же момента напоминания: перенесённое событие
/// напоминает о себе заново).
pub(crate) fn due_reminders<'a>(rows: &'a [ReminderRow], states: &HashMap<String, ReminderMark>, now_ms: i64) -> Vec<&'a ReminderRow> {
    rows.iter().filter(|r| r.remind_at <= now_ms && now_ms < r.start_at + OVERDUE_MS && !states.get(&r.server_id).is_some_and(|s| s.hides(r.remind_at, now_ms))).collect()
}

/// Ключ состава актуальных напоминаний: событие и его начало (перенос внутри окна актуальности
/// меняет ключ, а тикающий `minutes_left` — нет).
pub(crate) fn due_key(due: &[&ReminderRow]) -> Vec<(i64, i64)> {
    due.iter().map(|r| (r.calendar_pk, r.start_at)).collect()
}

/// Слать ли окну событие `reminder`: есть новые напоминания или состав актуальных (`key`) отличается
/// от последнего отправленного (`last_sent`, обновляется). Опустевший список тоже отправляется:
/// так окно узнаёт, что показывать больше нечего.
pub(crate) fn needs_emit(fresh: &[i64], key: Vec<(i64, i64)>, last_sent: &mut Vec<(i64, i64)>) -> bool {
    let changed = *last_sent != key;
    if changed {
        *last_sent = key;
    }
    !fresh.is_empty() || changed
}

/// Новые среди актуальных: о них ещё не объявляли (тост). `announced` приводится к актуальным:
/// событие, переставшее быть актуальным (прошло, закрыто, отложено, перенесено), при следующем
/// наступлении объявляется заново.
pub(crate) fn take_fresh(announced: &mut HashSet<i64>, due: &[&ReminderRow]) -> Vec<i64> {
    announced.retain(|pk| due.iter().any(|r| r.calendar_pk == *pk));
    due.iter().map(|r| r.calendar_pk).filter(|pk| announced.insert(*pk)).collect()
}

/// «через 15 мин», «через 2 ч 05 мин», «сейчас», «началось 3 мин назад».
pub(crate) fn countdown(minutes_left: i64) -> String {
    let span = |m: i64| if m >= 60 { format!("{} ч {:02} мин", m / 60, m % 60) } else { format!("{m} мин") };
    match minutes_left {
        0 => "сейчас".to_string(),
        m if m > 0 => format!("через {}", span(m)),
        m => format!("началось {} назад", span(-m)),
    }
}

/// Тема для текста тоста.
fn subject_text(subject: Option<&str>) -> &str {
    subject.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(без темы)")
}

/// Текст тоста одного напоминания: тема, когда (местное время), место.
pub(crate) fn toast_body(r: &ReminderRow, now_ms: i64, tz: &TimeZone) -> String {
    let mut out = subject_text(r.subject.as_deref()).to_string();
    out.push_str(" — ");
    if r.is_all_day {
        match Timestamp::from_millisecond(r.start_at) {
            Ok(t) => out.push_str(&format!("весь день {}", t.to_zoned(tz.clone()).strftime("%d.%m"))),
            Err(_) => out.push_str("весь день"),
        }
    } else {
        match Timestamp::from_millisecond(r.start_at) {
            Ok(t) => out.push_str(&format!("в {} ({})", t.to_zoned(tz.clone()).strftime("%H:%M"), countdown(minutes_left(r.start_at, now_ms)))),
            Err(_) => out.push_str(&countdown(minutes_left(r.start_at, now_ms))),
        }
    }
    if let Some(l) = r.location.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        out.push_str(", ");
        out.push_str(l);
    }
    out
}

/// «N напоминаний» со склонением.
pub(crate) fn summary_text(n: usize) -> String {
    let noun = match (n % 100, n % 10) {
        (11..=14, _) => "напоминаний",
        (_, 1) => "напоминание",
        (_, 2..=4) => "напоминания",
        _ => "напоминаний",
    };
    format!("{n} {noun}")
}

/// Сводка нескольких напоминаний: «3 напоминания: Планёрка (09:00), Обед (12:00), …».
pub(crate) fn summary_body(rows: &[&ReminderRow], tz: &TimeZone) -> String {
    let mut out = summary_text(rows.len());
    out.push_str(": ");
    let names: Vec<String> = rows
        .iter()
        .take(SUMMARY_SUBJECTS)
        .map(|r| {
            let when = if r.is_all_day { "весь день".to_string() } else { Timestamp::from_millisecond(r.start_at).map(|t| t.to_zoned(tz.clone()).strftime("%H:%M").to_string()).unwrap_or_default() };
            format!("{} ({when})", subject_text(r.subject.as_deref()))
        })
        .collect();
    out.push_str(&names.join(", "));
    if rows.len() > SUMMARY_SUBJECTS {
        out.push_str(", …");
    }
    out
}

// --- состояние и планировщик ------------------------------------------------------------------

pub(crate) struct RemindersState {
    store: Arc<Store>,
    account: AccountId,
    /// События, о которых тост уже был (ключ кэша): повторно не показываем, пока не отложат.
    announced: Mutex<HashSet<i64>>,
    /// Состав последнего отправленного окну списка (`due_key`): изменение состава — повод для
    /// события `reminder` и без новых напоминаний.
    last_sent: Mutex<Vec<(i64, i64)>>,
    poke: Notify,
    task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    stopping: AtomicBool,
}

impl RemindersState {
    pub(crate) fn new(store: Arc<Store>, account: AccountId) -> Self {
        Self { store, account, announced: Mutex::new(HashSet::new()), last_sent: Mutex::new(Vec::new()), poke: Notify::new(), task: Mutex::new(None), stopping: AtomicBool::new(false) }
    }

    fn announced(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.announced.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn last_sent(&self) -> std::sync::MutexGuard<'_, Vec<(i64, i64)>> {
        self.last_sent.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Запускает задачу планировщика (один раз, при старте приложения).
    pub(crate) fn start(self: &Arc<Self>, app: AppHandle) {
        let this = self.clone();
        let handle = tauri::async_runtime::spawn(async move { this.run(app).await });
        *self.task.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        info!("напоминания: планировщик запущен");
    }

    /// Проверить кэш сейчас, не дожидаясь тика (после записи в календарь, «закрыть», «отложить»).
    pub(crate) fn poke(&self) {
        self.poke.notify_one();
    }

    /// При выходе: остановить задачу. Она только читает базу и шлёт события, ждать её не нужно.
    pub(crate) fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.poke.notify_one();
        if let Some(t) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            t.abort();
        }
    }

    async fn run(self: Arc<Self>, app: AppHandle) {
        let mut ticks: u32 = 0;
        while !self.stopping.load(Ordering::SeqCst) {
            self.tick(&app, ticks);
            ticks = ticks.wrapping_add(1);
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = self.poke.notified() => {}
            }
        }
    }

    /// Строки с напоминанием, момент которых уже наступил (до `LOOKBACK_MS` назад), и состояния
    /// напоминаний пользователя.
    fn load(&self, now_ms: i64) -> CmdResult<(Vec<ReminderRow>, HashMap<String, ReminderMark>)> {
        let rows = self.store.calendar_events_with_reminders(self.account, now_ms - LOOKBACK_MS, now_ms + 1).map_err(err)?;
        let states = self.store.reminder_states(self.account).map_err(err)?.into_iter().collect();
        Ok((rows, states))
    }

    /// Напоминания, которые пора показывать сейчас (без побочных эффектов).
    fn pending(&self) -> CmdResult<Vec<ReminderDto>> {
        let now = now_ms();
        let (rows, states) = self.load(now)?;
        Ok(due_reminders(&rows, &states, now).into_iter().map(|r| dto(r, now)).collect())
    }

    /// Один тик: новые актуальные напоминания — событие окну (со всем актуальным списком) и тост;
    /// изменившийся без новых состав (перенос, удаление, «закрыть») — только событие.
    fn tick(&self, app: &AppHandle, ticks: u32) {
        if ticks.is_multiple_of(PRUNE_EVERY) {
            match self.store.prune_reminder_states(self.account) {
                Ok(n) if n > 0 => debug!(removed = n, "напоминания: состояния исчезнувших событий забыты"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "напоминания: чистка состояний не удалась"),
            }
        }
        let now = now_ms();
        let (rows, states) = match self.load(now) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "напоминания: кэш не прочитан");
                return;
            }
        };
        let due = due_reminders(&rows, &states, now);
        let fresh = take_fresh(&mut self.announced(), &due);
        if !needs_emit(&fresh, due_key(&due), &mut self.last_sent()) {
            return;
        }
        let items: Vec<ReminderDto> = due.iter().map(|r| dto(r, now)).collect();
        if fresh.is_empty() {
            debug!(due = items.len(), "напоминания: состав изменился");
        } else {
            info!(due = items.len(), fresh = fresh.len(), "напоминания: пора показать");
        }
        if let Err(e) = app.emit(REMINDER_EVENT, ReminderEvent { items }) {
            warn!(error = %e, "не удалось отправить событие reminder");
        }
        if !fresh.is_empty() {
            let fresh_rows: Vec<&ReminderRow> = due.iter().copied().filter(|r| fresh.contains(&r.calendar_pk)).collect();
            show_toast(app, &fresh_rows, now);
        }
    }

    /// Состояние напоминания события `calendar_pk` в базу — привязанным к текущему моменту
    /// напоминания строки. События нет в кэше или напоминание снято — записывать нечего.
    fn mark(&self, calendar_pk: i64, state: ReminderState) -> CmdResult<()> {
        let Some(row) = self.store.calendar_event(calendar_pk).map_err(err)? else { return Ok(()) };
        let Some(minutes) = row.reminder_minutes else { return Ok(()) };
        let mark = ReminderMark { state, remind_at: row.start_at - minutes * 60_000 };
        self.store.set_reminder_state(self.account, &row.id.id, Some(mark)).map_err(|e| format!("Ошибка локальной базы: {e}"))?;
        self.announced().remove(&calendar_pk);
        Ok(())
    }

    /// Закрыть напоминание насовсем (до переноса события или смены напоминания).
    fn dismiss(&self, calendar_pk: i64) -> CmdResult<()> {
        self.mark(calendar_pk, ReminderState::Dismissed)?;
        debug!(calendar_pk, "напоминание закрыто");
        self.poke();
        Ok(())
    }

    /// Отложить напоминание на `minutes`: покажется (и тост будет) снова, когда срок выйдет.
    fn snooze(&self, calendar_pk: i64, minutes: i64) -> CmdResult<()> {
        if !(1..=MAX_SNOOZE_MINUTES).contains(&minutes) {
            return Err(format!("Отложить можно от 1 минуты до {} дней", MAX_SNOOZE_MINUTES / (24 * 60)));
        }
        self.mark(calendar_pk, ReminderState::SnoozedUntil(now_ms() + minutes * 60_000))?;
        debug!(calendar_pk, minutes, "напоминание отложено");
        self.poke();
        Ok(())
    }
}

/// Тост о новых напоминаниях, если тосты включены: одно — с темой и временем, несколько — сводка.
fn show_toast(app: &AppHandle, fresh: &[&ReminderRow], now_ms: i64) {
    let Some(notify) = app.try_state::<Arc<NotifyState>>() else { return };
    if !notify.toasts_enabled() || fresh.is_empty() {
        return;
    }
    let tz = TimeZone::system();
    let (title, body) = match fresh {
        [one] => ("Напоминание".to_string(), toast_body(one, now_ms, &tz)),
        many => ("Напоминания".to_string(), summary_body(many, &tz)),
    };
    debug!(count = fresh.len(), "напоминания: тост");
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        warn!(error = %e, "тост напоминания не показан");
    }
}

// --- команды Tauri -----------------------------------------------------------

/// Напоминания, которые пора показывать сейчас (при старте окна).
#[tauri::command]
pub(crate) fn reminders_pending(rs: State<'_, Arc<RemindersState>>) -> CmdResult<Vec<ReminderDto>> {
    rs.pending()
}

/// Закрыть напоминание насовсем.
#[tauri::command]
pub(crate) fn reminder_dismiss(calendar_pk: i64, rs: State<'_, Arc<RemindersState>>) -> CmdResult<()> {
    rs.dismiss(calendar_pk)
}

/// Отложить напоминание на `minutes` минут.
#[tauri::command]
pub(crate) fn reminder_snooze(calendar_pk: i64, minutes: i64, rs: State<'_, Arc<RemindersState>>) -> CmdResult<()> {
    rs.snooze(calendar_pk, minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn row(pk: i64, start_at: i64, reminder_minutes: i64) -> ReminderRow {
        ReminderRow {
            calendar_pk: pk,
            server_id: format!("S{pk}"),
            subject: Some(format!("Событие {pk}")),
            start_at,
            end_at: start_at + 30 * MIN,
            is_all_day: false,
            location: Some("Зал".into()),
            reminder_minutes,
            remind_at: start_at - reminder_minutes * MIN,
        }
    }

    #[test]
    fn due_by_moment_overdue_and_user_state() {
        let t0 = 1_800_000_000_000;
        let rows = vec![row(1, t0 + 15 * MIN, 15), row(2, t0 + 20 * MIN, 15), row(3, t0 - 10 * MIN, 5), row(4, t0 - 31 * MIN, 5), row(5, t0 + 60 * MIN, 0)];
        let none = HashMap::new();
        let pks = |now: i64, states: &HashMap<String, ReminderMark>| due_reminders(&rows, states, now).iter().map(|r| r.calendar_pk).collect::<Vec<_>>();
        assert_eq!(pks(t0, &none), vec![1, 3], "1 — момент наступил ровно сейчас; 2 — через 5 мин; 3 — началось 10 мин назад; 4 — просрочено; 5 — «в момент начала» через час");
        assert_eq!(pks(t0 - 1, &none), vec![3]);
        assert_eq!(pks(t0 + 5 * MIN, &none), vec![1, 2, 3]);
        assert_eq!(pks(t0 + 20 * MIN, &none), vec![1, 2], "3 просрочено ровно через 30 мин после начала");
        assert_eq!(pks(t0 + 60 * MIN, &none), vec![5], "напоминание «0 минут» — в момент начала");
        let mut states = HashMap::new();
        states.insert("S1".to_string(), ReminderMark { state: ReminderState::Dismissed, remind_at: rows[0].remind_at });
        states.insert("S3".to_string(), ReminderMark { state: ReminderState::SnoozedUntil(t0 + 5 * MIN), remind_at: rows[2].remind_at });
        assert_eq!(pks(t0, &states), Vec::<i64>::new(), "закрытое и отложенное скрыты");
        assert_eq!(pks(t0 + 5 * MIN, &states), vec![2, 3], "срок отсрочки вышел — снова показывается");
        // Событие 1 перенесено (или сменилось напоминание): момент напоминания другой, «закрыто» не действует.
        let moved = vec![row(1, t0 + 25 * MIN, 15)];
        assert_eq!(due_reminders(&moved, &states, t0 + 10 * MIN).iter().map(|r| r.calendar_pk).collect::<Vec<_>>(), vec![1], "после переноса напоминание взводится заново");
        assert!(due_reminders(&[], &none, t0).is_empty());
    }

    #[test]
    fn emit_on_fresh_or_changed_set() {
        let t0 = 1_800_000_000_000;
        let a = row(1, t0, 15);
        let b = row(2, t0 + 5 * MIN, 15);
        let mut last = Vec::new();
        assert!(needs_emit(&[1], due_key(&[&a]), &mut last), "новое напоминание — слать");
        assert_eq!(last, vec![(1, t0)]);
        assert!(!needs_emit(&[], due_key(&[&a]), &mut last), "ничего не изменилось — молчание");
        assert!(needs_emit(&[], due_key(&[&a, &b]), &mut last), "состав вырос без новых (после отсрочки) — слать");
        let moved = row(1, t0 + 2 * MIN, 15);
        assert!(needs_emit(&[], due_key(&[&moved, &b]), &mut last), "перенос при том же pk — слать");
        assert!(needs_emit(&[], due_key(&[]), &mut last), "ушло из актуальных — слать пустой список");
        assert!(last.is_empty());
        assert!(!needs_emit(&[], due_key(&[]), &mut last));
    }

    #[test]
    fn fresh_are_announced_once_until_they_leave_and_return() {
        let t0 = 1_800_000_000_000;
        let a = row(1, t0, 15);
        let b = row(2, t0, 15);
        let mut announced = HashSet::new();
        assert_eq!(take_fresh(&mut announced, &[&a]), vec![1], "первое появление — тост");
        assert_eq!(take_fresh(&mut announced, &[&a]), Vec::<i64>::new(), "повтор — без тоста");
        assert_eq!(take_fresh(&mut announced, &[&a, &b]), vec![2], "только новое");
        assert_eq!(take_fresh(&mut announced, &[&b]), Vec::<i64>::new());
        assert_eq!(announced.iter().copied().collect::<Vec<_>>(), vec![2], "ушедшее из актуальных забыто");
        assert_eq!(take_fresh(&mut announced, &[&a, &b]), vec![1], "вернувшееся (после отсрочки) объявляется снова");
        assert!(take_fresh(&mut announced, &[]).is_empty());
        assert!(announced.is_empty());
    }

    #[test]
    fn minutes_and_texts() {
        let t0 = 1_800_000_000_000;
        assert_eq!(minutes_left(t0 + 15 * MIN, t0), 15);
        assert_eq!(minutes_left(t0 + 30_000, t0), 0, "за полминуты — 0");
        assert_eq!(minutes_left(t0 - 30_000, t0), -1, "через полминуты после начала — −1");
        assert_eq!(minutes_left(t0 - 3 * MIN, t0), -3);
        assert_eq!(countdown(0), "сейчас");
        assert_eq!(countdown(15), "через 15 мин");
        assert_eq!(countdown(125), "через 2 ч 05 мин");
        assert_eq!(countdown(-3), "началось 3 мин назад");
        assert_eq!(countdown(-90), "началось 1 ч 30 мин назад");
        assert_eq!(summary_text(1), "1 напоминание");
        assert_eq!(summary_text(3), "3 напоминания");
        assert_eq!(summary_text(5), "5 напоминаний");
        assert_eq!(summary_text(11), "11 напоминаний");
        assert_eq!(summary_text(22), "22 напоминания");
        let tz = TimeZone::fixed(jiff::tz::Offset::constant(3));
        // 2027-01-15T06:00:00Z = 09:00 по UTC+3.
        let start = "2027-01-15T06:00:00Z".parse::<Timestamp>().unwrap().as_millisecond();
        let r = row(1, start, 15);
        assert_eq!(toast_body(&r, start - 15 * MIN, &tz), "Событие 1 — в 09:00 (через 15 мин), Зал");
        let mut all_day = row(2, start - 6 * 60 * MIN, 1440);
        all_day.is_all_day = true;
        all_day.location = Some("  ".into());
        all_day.subject = None;
        assert_eq!(toast_body(&all_day, start, &tz), "(без темы) — весь день 15.01");
        let rows = [&r, &all_day, &r, &r];
        let s = summary_body(&rows, &tz);
        assert!(s.starts_with("4 напоминания: Событие 1 (09:00), (без темы) (весь день), Событие 1 (09:00), …"), "{s}");
        assert_eq!(summary_body(&rows[..2], &tz), "2 напоминания: Событие 1 (09:00), (без темы) (весь день)");
        let d = dto(&r, start - 15 * MIN);
        assert_eq!((d.calendar_pk, d.minutes_left, d.start_ms, d.end_ms, d.is_all_day), (1, 15, start, start + 30 * MIN, false));
        assert_eq!((d.subject.as_deref(), d.location.as_deref()), (Some("Событие 1"), Some("Зал")));
    }
}
