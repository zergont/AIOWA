/**
 * Чистые функции календаря: дни и недели (неделя с понедельника), диапазоны видов, русские форматы
 * дат и времени, раскладка событий по дням и колонкам. Даты — местное время машины,
 * миллисекунды событий — UTC, как отдаёт бэкенд. Без зависимостей от DOM и Svelte.
 */
import type { CalendarEvent, CalendarEventDetails, EventParticipant, FreeBusy, FreeBusyChoice, ResponseType } from "./api";

export type CalendarViewKind = "day" | "work" | "week" | "month";

export const MINUTES_PER_DAY = 24 * 60;
/** Минимальная длительность блока в раскладке: короткие события занимают место под одну строку текста. */
export const MIN_SLOT_MIN = 30;

// --- дни ---------------------------------------------------------------------------------

export function startOfDay(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

export function addDays(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}

export function startOfMonth(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), 1);
}

export function addMonths(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth() + n, 1);
}

/** Понедельник недели, в которую входит день (местная полночь). */
export function startOfWeek(d: Date): Date {
  return addDays(startOfDay(d), -((d.getDay() + 6) % 7));
}

export function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** Ключ местного дня ГГГГММДД: сравнение и упорядочивание дней без времени. */
export function dayKey(d: Date): number {
  return d.getFullYear() * 10_000 + (d.getMonth() + 1) * 100 + d.getDate();
}

export function isWeekend(d: Date): boolean {
  const w = d.getDay();
  return w === 0 || w === 6;
}

export function minutesOfDay(d: Date): number {
  return d.getHours() * 60 + d.getMinutes();
}

// --- диапазон вида -------------------------------------------------------------------------

export interface ViewRange {
  /** Местная полночь первого дня. */
  start: Date;
  /** Полночь после последнего дня (не включается). */
  end: Date;
  days: Date[];
}

/** День — один день якоря; рабочая неделя — пн–пт недели якоря; неделя — пн–вс; месяц — 6 недель сетки месяца якоря. */
export function viewRange(kind: CalendarViewKind, anchor: Date): ViewRange {
  if (kind === "month") {
    const days = monthGrid(anchor);
    return { start: days[0], end: addDays(days[0], days.length), days };
  }
  const first = kind === "day" ? startOfDay(anchor) : startOfWeek(anchor);
  const count = kind === "day" ? 1 : kind === "work" ? 5 : 7;
  const days = Array.from({ length: count }, (_, i) => addDays(first, i));
  return { start: first, end: addDays(first, count), days };
}

/**
 * Соседний период: для дня — на день, для недель — на неделю (день недели якоря сохраняется),
 * для месяца — на месяц (число сохраняется, в коротком месяце — последнее).
 */
export function shiftAnchor(kind: CalendarViewKind, anchor: Date, dir: -1 | 1): Date {
  if (kind === "month") {
    const m = addMonths(anchor, dir);
    const lastDate = new Date(m.getFullYear(), m.getMonth() + 1, 0).getDate();
    return new Date(m.getFullYear(), m.getMonth(), Math.min(anchor.getDate(), lastDate));
  }
  return addDays(startOfDay(anchor), kind === "day" ? dir : 7 * dir);
}

/** Сетка месяца для мини-календаря: 6 недель по 7 дней с понедельника, в первой неделе 1-е число. */
export function monthGrid(month: Date): Date[] {
  const first = startOfWeek(startOfMonth(month));
  return Array.from({ length: 42 }, (_, i) => addDays(first, i));
}

// --- события «весь день» -------------------------------------------------------------------

/**
 * Местный день по границе события «весь день». Сервер отдаёт границы как настоящие моменты —
 * полуночи в поясе организатора, без нормализации (docs/protocol-owa-json.md § 5g): у события из
 * пояса восточнее местного момент начала приходится на местный вечер предыдущего дня, западнее —
 * на местное утро. Поэтому день показа — местная дата ближайшей полуночи к моменту; это верно,
 * пока пояс организатора отличается от местного меньше чем на 12 часов.
 */
export function allDayDate(ms: number): Date {
  const d = new Date(ms);
  const day = startOfDay(d);
  return d.getTime() - day.getTime() >= 12 * 3600_000 ? addDays(day, 1) : day;
}

/** Первый и последний (включительно) дни события «весь день». */
export function allDaySpan(ev: Pick<CalendarEvent, "start_ms" | "end_ms">): { first: Date; last: Date } {
  const first = allDayDate(ev.start_ms);
  const last = addDays(allDayDate(Math.max(ev.end_ms, ev.start_ms)), -1);
  return { first, last: last.getTime() < first.getTime() ? first : last };
}

// --- раскладка -------------------------------------------------------------------------------

export interface Slot {
  /** Минуты от местной полуночи. */
  startMin: number;
  endMin: number;
}

export interface Column {
  /** Номер колонки внутри группы пересекающихся событий и число колонок в ней. */
  col: number;
  cols: number;
}

export interface DaySegment extends Slot {
  event: CalendarEvent;
  /** Индекс дня в диапазоне. */
  day: number;
  /** Событие началось раньше этого дня / кончается позже него. */
  continuesBefore: boolean;
  continuesAfter: boolean;
}

/**
 * Раскладка пересекающихся блоков по колонкам: события сортируются по началу (длинные первыми),
 * группа пересекающихся делит ширину поровну, каждое идёт в первую освободившуюся колонку.
 * Короткие блоки считаются длиной не меньше `MIN_SLOT_MIN`, чтобы подписи не налезали друг на друга.
 */
export function layoutColumns<T extends Slot>(items: T[]): (T & Column)[] {
  const sorted = [...items].sort((a, b) => a.startMin - b.startMin || b.endMin - a.endMin);
  const out: (T & Column)[] = [];
  let cluster: { item: T; col: number }[] = [];
  let colEnds: number[] = [];
  const flush = () => {
    for (const c of cluster) out.push({ ...c.item, col: c.col, cols: colEnds.length });
    cluster = [];
    colEnds = [];
  };
  for (const item of sorted) {
    const s = item.startMin;
    const e = Math.max(item.endMin, s + MIN_SLOT_MIN);
    if (colEnds.length && colEnds.every((end) => end <= s)) flush();
    let col = colEnds.findIndex((end) => end <= s);
    if (col < 0) {
      col = colEnds.length;
      colEnds.push(e);
    } else colEnds[col] = e;
    cluster.push({ item, col });
  }
  flush();
  return out;
}

/**
 * Отрезки событий со временем по дням диапазона: событие через полночь даёт отрезок в каждом
 * задетом дне. Внутри дня отрезки разложены по колонкам.
 */
export function timedSegments(events: CalendarEvent[], days: Date[]): (DaySegment & Column)[][] {
  return days.map((day, i) => {
    const dayStart = day.getTime();
    const dayEnd = addDays(day, 1).getTime();
    const segs: DaySegment[] = [];
    for (const ev of events) {
      if (ev.is_all_day) continue;
      const s = ev.start_ms;
      const e = Math.max(ev.end_ms, s);
      const overlaps = s < dayEnd && e > dayStart;
      const pointInDay = s === e && s >= dayStart && s < dayEnd;
      if (!overlaps && !pointInDay) continue;
      const before = s < dayStart;
      const after = e > dayEnd;
      const startMin = before ? 0 : minutesOfDay(new Date(s));
      const endMin = e >= dayEnd ? MINUTES_PER_DAY : minutesOfDay(new Date(e));
      segs.push({ event: ev, day: i, startMin, endMin: Math.max(endMin, startMin), continuesBefore: before, continuesAfter: after });
    }
    return layoutColumns(segs);
  });
}

export interface AllDayBar {
  event: CalendarEvent;
  /** Индексы первого и последнего дня полосы в диапазоне (включительно). */
  first: number;
  last: number;
  row: number;
  continuesBefore: boolean;
  continuesAfter: boolean;
}

/** Полосы «весь день» над сеткой: каждая занимает свои дни, пересекающиеся идут в следующую строку. */
export function allDayBars(events: CalendarEvent[], days: Date[]): { bars: AllDayBar[]; rows: number } {
  if (!days.length) return { bars: [], rows: 0 };
  const keys = days.map(dayKey);
  const firstKey = keys[0];
  const lastKey = keys[keys.length - 1];
  const items: Omit<AllDayBar, "row">[] = [];
  for (const ev of events) {
    if (!ev.is_all_day) continue;
    const span = allDaySpan(ev);
    const a = dayKey(span.first);
    const b = dayKey(span.last);
    if (b < firstKey || a > lastKey) continue;
    const first = Math.max(0, keys.findIndex((k) => k >= a));
    let last = keys.length - 1;
    while (last > first && keys[last] > b) last--;
    items.push({ event: ev, first, last, continuesBefore: a < firstKey, continuesAfter: b > lastKey });
  }
  items.sort((x, y) => x.first - y.first || y.last - y.first - (x.last - x.first) || x.event.start_ms - y.event.start_ms);
  const rowEnds: number[] = [];
  const bars = items.map((it) => {
    let row = rowEnds.findIndex((end) => end < it.first);
    if (row < 0) {
      row = rowEnds.length;
      rowEnds.push(it.last);
    } else rowEnds[row] = it.last;
    return { ...it, row };
  });
  return { bars, rows: rowEnds.length };
}

/** Положение линии «сейчас»: день диапазона и минуты от полуночи; null — сегодня не показан. */
export function nowPosition(now: Date, days: Date[]): { day: number; minutes: number } | null {
  const day = days.findIndex((d) => sameDay(d, now));
  return day < 0 ? null : { day, minutes: minutesOfDay(now) };
}

/** Ключи местных дней, которые занимает событие: у «весь день» — по его дням, у остальных — от начала до конца. */
export function eventDayKeys(ev: Pick<CalendarEvent, "start_ms" | "end_ms" | "is_all_day">): number[] {
  const keys: number[] = [];
  let d: Date;
  let last: Date;
  if (ev.is_all_day) {
    const span = allDaySpan(ev);
    d = span.first;
    last = span.last;
  } else {
    const s = ev.start_ms;
    const e = Math.max(ev.end_ms, s);
    d = startOfDay(new Date(s));
    // Конец ровно в полночь принадлежит предыдущему дню.
    last = startOfDay(new Date(e > s && e === startOfDay(new Date(e)).getTime() ? e - 1 : e));
  }
  for (let i = 0; i < 62 && d.getTime() <= last.getTime(); i++, d = addDays(d, 1)) keys.push(dayKey(d));
  return keys;
}

/**
 * События по дням сетки месяца: «весь день» и многодневные — в каждом задетом дне, первыми;
 * остальные — по началу. Отменённые остаются (их видно зачёркнутыми).
 */
export function eventsByDay(events: CalendarEvent[], days: Date[]): CalendarEvent[][] {
  const index = new Map<number, number>();
  days.forEach((d, i) => index.set(dayKey(d), i));
  const out: CalendarEvent[][] = days.map(() => []);
  const sorted = [...events].sort((a, b) => Number(b.is_all_day) - Number(a.is_all_day) || a.start_ms - b.start_ms || b.end_ms - a.end_ms);
  for (const ev of sorted) {
    for (const k of eventDayKeys(ev)) {
      const i = index.get(k);
      if (i !== undefined) out[i].push(ev);
    }
  }
  return out;
}

// --- редактор события ------------------------------------------------------------------------

/** Длительность нового события по умолчанию. */
export const DEFAULT_EVENT_MIN = 30;

/** Ближайшая граница получаса не раньше момента: начало нового события «сейчас». */
export function nextSlot(d: Date, stepMin = 30): Date {
  const ms = stepMin * 60_000;
  return new Date(Math.ceil(d.getTime() / ms) * ms);
}

/** Значение для input type=date по местной дате: «2026-10-24». */
export function dateInputValue(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

/** Значение для input type=time по местному времени: «09:30». */
export function timeInputValue(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

/** Местный момент из значений полей даты и времени; null — поле пустое или не разобралось. */
export function parseDateTimeInputs(date: string, time: string | null): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date);
  if (!m) return null;
  let h = 0;
  let min = 0;
  if (time != null) {
    const t = /^(\d{2}):(\d{2})/.exec(time);
    if (!t) return null;
    h = Number(t[1]);
    min = Number(t[2]);
  }
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), h, min);
  return Number.isNaN(d.getTime()) ? null : d;
}

/** Варианты напоминания в редакторе: null — без напоминания. */
export const REMINDER_OPTIONS: { value: number | null; label: string }[] = [
  { value: null, label: "Нет" },
  { value: 0, label: "В момент начала" },
  { value: 5, label: "За 5 минут" },
  { value: 15, label: "За 15 минут" },
  { value: 30, label: "За 30 минут" },
  { value: 60, label: "За 1 час" },
  { value: 1440, label: "За 1 день" },
];

/** Занятость, которую можно выбрать в редакторе. */
export const FREE_BUSY_OPTIONS: { value: Exclude<FreeBusy, "no_data">; label: string }[] = [
  { value: "busy", label: "Занят" },
  { value: "free", label: "Свободен" },
  { value: "tentative", label: "Под вопросом" },
  { value: "oof", label: "Нет на месте" },
  { value: "working_elsewhere", label: "Работаю в другом месте" },
];

/** Заготовка редактора: новое событие (`calendarPk` null) или правка существующего. Моменты — местные, как в запросе. */
export interface EventDraft {
  calendarPk: number | null;
  subject: string;
  /** У «весь день» — местная полночь первого дня и полночь после последнего. */
  startMs: number;
  endMs: number;
  isAllDay: boolean;
  location: string;
  bodyHtml: string;
  required: EventParticipant[];
  optional: EventParticipant[];
  reminderMinutes: number | null;
  freeBusy: FreeBusyChoice;
  isPrivate: boolean;
  /** Я организатор (или событие без участников): обновления участникам рассылаю я. */
  isOrganizer: boolean;
  /** На сервере у встречи уже есть участники: при правке спросить, слать ли им обновление. */
  hadAttendees: boolean;
  /** Экземпляр повторяющейся встречи: правится именно он. */
  isOccurrence: boolean;
  /** Участники и описание в заготовке взяты из подробностей (а не пустые от незнания): пустые списки при правке значат «убрать». */
  detailsKnown: boolean;
}

/** Заготовка нового события: со времени `start` на `DEFAULT_EVENT_MIN` минут или на весь день `start`. */
export function newEventDraft(start: Date, allDay = false): EventDraft {
  const s = allDay ? startOfDay(start) : start;
  const e = allDay ? addDays(s, 1) : new Date(s.getTime() + DEFAULT_EVENT_MIN * 60_000);
  return {
    calendarPk: null,
    subject: "",
    startMs: s.getTime(),
    endMs: e.getTime(),
    isAllDay: allDay,
    location: "",
    bodyHtml: "",
    required: [],
    optional: [],
    reminderMinutes: 15,
    freeBusy: "busy",
    isPrivate: false,
    isOrganizer: true,
    hadAttendees: false,
    isOccurrence: false,
    detailsKnown: true,
  };
}

/** Заготовка правки по событию сетки и его подробностям (если уже загружены). */
export function draftFromEvent(ev: CalendarEvent, details: CalendarEventDetails | null): EventDraft {
  let startMs = ev.start_ms;
  let endMs = Math.max(ev.end_ms, ev.start_ms);
  if (ev.is_all_day) {
    const { first, last } = allDaySpan(ev);
    startMs = first.getTime();
    endMs = addDays(last, 1).getTime();
  }
  const attendees = details?.attendees ?? [];
  const people = (required: boolean): EventParticipant[] => attendees.filter((a) => a.required === required && a.email).map((a) => ({ name: a.name, email: a.email as string }));
  const isOrganizer = !ev.is_meeting || ev.my_response === "organizer";
  return {
    calendarPk: ev.calendar_pk,
    subject: ev.subject ?? "",
    startMs,
    endMs,
    isAllDay: ev.is_all_day,
    location: ev.location ?? "",
    bodyHtml: details?.body_html ?? "",
    required: people(true),
    optional: people(false),
    reminderMinutes: details?.reminder_minutes ?? ev.reminder_minutes ?? null,
    freeBusy: ev.free_busy === "no_data" ? "busy" : ev.free_busy,
    isPrivate: (ev.sensitivity ?? "").toLowerCase() === "private",
    isOrganizer,
    hadAttendees: details ? attendees.length > 0 : ev.is_meeting,
    isOccurrence: ev.calendar_type === "occurrence" || ev.calendar_type === "exception",
    detailsKnown: details != null,
  };
}

/** «Приглашение: 24 октября 2026, 09:00–09:30 (30 мин)» — когда встреча по полям письма-приглашения. */
export function meetingWhen(startMs: number | null, endMs: number | null, isAllDay: boolean): string | null {
  if (startMs == null) return null;
  return eventWhen({ start_ms: startMs, end_ms: endMs ?? startMs, is_all_day: isAllDay });
}

/** «через 15 мин», «сейчас», «началось 5 мин назад» — для напоминания. */
export function reminderCountdown(startMs: number, now: number): string {
  const diffMin = Math.round((startMs - now) / 60_000);
  if (diffMin > 0) return `через ${formatDuration(diffMin * 60_000)}`;
  if (diffMin === 0) return "сейчас";
  return `началось ${formatDuration(-diffMin * 60_000)} назад`;
}

// --- форматы ru-RU ----------------------------------------------------------------------------

const fmtWeekdayShort = new Intl.DateTimeFormat("ru-RU", { weekday: "short" });
const fmtDayMonth = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "long" });
const fmtDayMonthShort = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short" });
const fmtDayLong = new Intl.DateTimeFormat("ru-RU", { weekday: "long", day: "numeric", month: "long", year: "numeric" });
const fmtMonthYear = new Intl.DateTimeFormat("ru-RU", { month: "long", year: "numeric" });
const fmtTime = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });

const noYearSuffix = (s: string) => s.replace(/\s*г\.$/, "");
const capitalize = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** «09:30». */
export function formatTime(ms: number): string {
  return fmtTime.format(new Date(ms));
}

/** Подпись часа на шкале: «09:00». */
export function hourLabel(h: number): string {
  return `${String(h).padStart(2, "0")}:00`;
}

/** «пн». */
export function weekdayShort(d: Date): string {
  return fmtWeekdayShort.format(d);
}

/** «Понедельник, 20 октября 2026». */
export function dayLong(d: Date): string {
  return capitalize(noYearSuffix(fmtDayLong.format(d)));
}

/** «Октябрь 2026». */
export function monthTitle(d: Date): string {
  return capitalize(noYearSuffix(fmtMonthYear.format(d)));
}

/** Заголовок отрезка дней (включительно): «20–26 октября 2026», «28 сентября – 4 октября 2026», «29 декабря 2025 – 4 января 2026». */
export function dateRangeTitle(a: Date, b: Date): string {
  if (sameDay(a, b)) return `${fmtDayMonth.format(a)} ${a.getFullYear()}`;
  if (a.getFullYear() === b.getFullYear()) {
    if (a.getMonth() === b.getMonth()) return `${a.getDate()}–${fmtDayMonth.format(b)} ${b.getFullYear()}`;
    return `${fmtDayMonth.format(a)} – ${fmtDayMonth.format(b)} ${b.getFullYear()}`;
  }
  return `${fmtDayMonth.format(a)} ${a.getFullYear()} – ${fmtDayMonth.format(b)} ${b.getFullYear()}`;
}

/** Заголовок периода в панели календаря; у месяца — месяц якоря (`anchor`), а не первый день сетки. */
export function periodTitle(kind: CalendarViewKind, range: ViewRange, anchor?: Date): string {
  if (kind === "day") return dayLong(range.days[0]);
  if (kind === "month") return monthTitle(anchor ?? range.days[Math.floor(range.days.length / 2)]);
  return dateRangeTitle(range.days[0], range.days[range.days.length - 1]);
}

export function plural(n: number, one: string, few: string, many: string): string {
  const n10 = n % 10;
  const n100 = n % 100;
  if (n10 === 1 && n100 !== 11) return one;
  if (n10 >= 2 && n10 <= 4 && (n100 < 12 || n100 > 14)) return few;
  return many;
}

/** «30 мин», «1 ч 30 мин», «2 дня», «1 день 3 ч». */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 60_000));
  const d = Math.floor(total / MINUTES_PER_DAY);
  const h = Math.floor((total % MINUTES_PER_DAY) / 60);
  const m = total % 60;
  const parts: string[] = [];
  if (d) parts.push(`${d} ${plural(d, "день", "дня", "дней")}`);
  if (h) parts.push(`${h} ч`);
  if (m || !parts.length) parts.push(`${m} мин`);
  return parts.join(" ");
}

/** «09:00–10:30»; конец в полночь следующего дня показывается как 24:00. */
export function timeRange(startMs: number, endMs: number): string {
  const s = new Date(startMs);
  const e = new Date(Math.max(endMs, startMs));
  const endsAtMidnight = e.getTime() === addDays(startOfDay(s), 1).getTime();
  return `${fmtTime.format(s)}–${endsAtMidnight ? "24:00" : fmtTime.format(e)}`;
}

/** Когда событие, для панели события: день, время и длительность; у «весь день» — дни. */
export function eventWhen(ev: Pick<CalendarEvent, "start_ms" | "end_ms" | "is_all_day">): string {
  if (ev.is_all_day) {
    const { first, last } = allDaySpan(ev);
    return `Весь день, ${dateRangeTitle(first, last)}`;
  }
  const s = new Date(ev.start_ms);
  const e = new Date(Math.max(ev.end_ms, ev.start_ms));
  const duration = formatDuration(e.getTime() - s.getTime());
  const endsAtMidnight = e.getTime() === addDays(startOfDay(s), 1).getTime();
  if (sameDay(s, e) || endsAtMidnight) return `${dayLong(s)}, ${timeRange(ev.start_ms, ev.end_ms)} (${duration})`;
  return `${fmtDayMonth.format(s)} ${s.getFullYear()}, ${fmtTime.format(s)} – ${fmtDayMonth.format(e)} ${e.getFullYear()}, ${fmtTime.format(e)} (${duration})`;
}

/** Время обновления: «в 14:32» сегодня, иначе «16 окт, 14:32». */
export function updatedLabel(ms: number, now: Date = new Date()): string {
  const d = new Date(ms);
  if (sameDay(d, now)) return `в ${fmtTime.format(d)}`;
  return `${fmtDayMonthShort.format(d).replace(".", "")}, ${fmtTime.format(d)}`;
}

// --- подписи состояний -------------------------------------------------------------------------

/** Ответ участника в списке участников. */
export const RESPONSE_LABEL: Record<ResponseType, string> = {
  unknown: "неизвестно",
  organizer: "организатор",
  tentative: "под вопросом",
  accept: "принял",
  decline: "отклонил",
  no_response: "без ответа",
};

/** Свой ответ на приглашение; null — показывать нечего. */
export const MY_RESPONSE_LABEL: Record<ResponseType, string | null> = {
  unknown: null,
  organizer: "Вы организатор",
  tentative: "Под вопросом",
  accept: "Принято",
  decline: "Отклонено",
  no_response: "Без ответа",
};

export const FREE_BUSY_LABEL: Record<FreeBusy, string | null> = {
  free: "Свободен",
  tentative: "Под вопросом",
  busy: "Занят",
  oof: "Нет на месте",
  working_elsewhere: "Работаю в другом месте",
  no_data: null,
};

export function sensitivityLabel(s: string | null): string | null {
  switch ((s ?? "").toLowerCase()) {
    case "private":
    case "personal":
      return "личное";
    case "confidential":
      return "конфиденциально";
    default:
      return null;
  }
}

/** «за 15 мин», «за 1 ч», «в момент начала»; null — без напоминания. */
export function reminderLabel(minutes: number | null): string | null {
  if (minutes == null || minutes < 0) return null;
  if (minutes === 0) return "в момент начала";
  return `за ${formatDuration(minutes * 60_000)}`;
}

export type EventTone = "busy" | "tentative" | "free" | "oof" | "elsewhere" | "nodata";

/** Оформление блока: по занятости; свой ответ «под вопросом» рисуется как tentative. */
export function eventTone(ev: Pick<CalendarEvent, "free_busy" | "my_response">): EventTone {
  if (ev.my_response === "tentative") return "tentative";
  switch (ev.free_busy) {
    case "free":
      return "free";
    case "tentative":
      return "tentative";
    case "oof":
      return "oof";
    case "working_elsewhere":
      return "elsewhere";
    case "no_data":
      return "nodata";
    default:
      return "busy";
  }
}

/** Текст блока для читалок и подсказки: тема, время, место, состояние. */
export function eventLabel(ev: CalendarEvent): string {
  const parts = [ev.subject || "(без темы)", ev.is_all_day ? "весь день" : timeRange(ev.start_ms, ev.end_ms)];
  if (ev.location) parts.push(ev.location);
  if (ev.is_cancelled) parts.push("отменена");
  else if (ev.my_response === "tentative" || ev.free_busy === "tentative") parts.push("под вопросом");
  return parts.join(", ");
}
