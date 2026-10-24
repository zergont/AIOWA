<script lang="ts">
  import type { CalendarEvent } from "../api";
  import { dayLong, eventLabel, eventTone, eventWhen, eventsByDay, formatTime, isWeekend, sameDay } from "../calendar";

  /**
   * Сетка месяца: 6 недель по 7 дней, в ячейке до `MAX_PER_DAY` событий и «ещё N». Клик по числу
   * или по «ещё» ведёт в вид «День», двойной клик по пустому месту ячейки — новое событие в 09:00.
   */
  interface Props {
    /** 42 дня сетки (местные полуночи) с понедельника первой недели месяца. */
    days: Date[];
    /** Месяц якоря: дни соседних месяцев приглушены. */
    month: Date;
    events: CalendarEvent[];
    selectedPk: number | null;
    now: Date;
    onselect: (ev: CalendarEvent) => void;
    /** Открыть день (число дня, «ещё N»). */
    onpickday: (d: Date) => void;
    /** Новое событие с этого момента; `allDay` — на весь день. */
    oncreate: (startMs: number, allDay: boolean) => void;
  }
  let { days, month, events, selectedPk, now, onselect, onpickday, oncreate }: Props = $props();

  const WEEKDAYS = ["пн", "вт", "ср", "чт", "пт", "сб", "вс"];
  const MAX_PER_DAY = 3;
  /** Час начала события по двойному клику в ячейке дня. */
  const CREATE_HOUR = 9;

  const cells = $derived(eventsByDay(events, days));

  function tooltip(ev: CalendarEvent): string {
    return [ev.subject || "(без темы)", eventWhen(ev), ev.location, ev.is_cancelled ? "Отменена" : null].filter(Boolean).join("\n");
  }

  /** Двойной клик по пустому месту ячейки (не по событию и не по числу). */
  function onCellDblClick(e: MouseEvent, d: Date) {
    if (e.target instanceof Element && e.target.closest("button")) return;
    e.preventDefault();
    oncreate(new Date(d.getFullYear(), d.getMonth(), d.getDate(), CREATE_HOUR).getTime(), false);
  }
</script>

<div class="month">
  <div class="names" aria-hidden="true">
    {#each WEEKDAYS as w, i (w)}
      <span class="name" class:weekend={i >= 5}>{w}</span>
    {/each}
  </div>
  <div class="cells">
    {#each days as d, i (d.getTime())}
      {@const list = cells[i]}
      {@const rest = list.length - MAX_PER_DAY}
      <div class="cell" class:other={d.getMonth() !== month.getMonth()} class:today={sameDay(d, now)} class:weekend={isWeekend(d)} role="presentation" ondblclick={(e) => onCellDblClick(e, d)}>
        <button class="num" onclick={() => onpickday(d)} aria-label={dayLong(d)} title="Открыть день">{d.getDate()}</button>
        <div class="evs">
          {#each list.slice(0, MAX_PER_DAY) as ev (ev.calendar_pk)}
            <button
              class="ev {eventTone(ev)}"
              class:allday={ev.is_all_day}
              class:selected={ev.calendar_pk === selectedPk}
              class:cancelled={ev.is_cancelled}
              class:declined={ev.my_response === "decline"}
              class:organizer={ev.my_response === "organizer"}
              onclick={() => onselect(ev)}
              title={tooltip(ev)}
              aria-label={eventLabel(ev)}
              aria-pressed={ev.calendar_pk === selectedPk}
            >
              {#if !ev.is_all_day}<span class="ev-time">{formatTime(ev.start_ms)}</span>{/if}
              <span class="ev-subject">{ev.subject || "(без темы)"}</span>
            </button>
          {/each}
          {#if rest > 0}
            <button class="more" onclick={() => onpickday(d)} title="Открыть день">ещё {rest}</button>
          {/if}
        </div>
      </div>
    {/each}
  </div>
</div>

<style>
  .month {
    height: 100%;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .names {
    flex: none;
    display: grid;
    grid-template-columns: repeat(7, minmax(0, 1fr));
    height: 28px;
    border-bottom: 1px solid var(--stroke-1);
  }
  .name {
    display: flex;
    align-items: center;
    justify-content: center;
    font: var(--text-caption);
    color: var(--fg-3);
  }
  .name.weekend {
    color: var(--fg-4);
  }
  .cells {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: repeat(7, minmax(0, 1fr));
    grid-template-rows: repeat(6, minmax(0, 1fr));
  }
  .cell {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    padding: 2px 3px 3px;
    border-right: 1px solid var(--stroke-1);
    border-bottom: 1px solid var(--stroke-1);
  }
  .cell:nth-child(7n) {
    border-right: 0;
  }
  .cell.weekend {
    background: var(--cal-weekend);
  }
  .cell.today {
    background: var(--cal-today);
  }
  .num {
    align-self: flex-end;
    min-width: 24px;
    height: 24px;
    padding: 0 6px;
    font: var(--text-body-strong);
    color: var(--fg-2);
    border-radius: 12px;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .num:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .cell.other .num {
    color: var(--fg-4);
    font-weight: 400;
  }
  .cell.today .num {
    background: var(--accent);
    color: var(--fg-on-accent);
  }
  .cell.today .num:hover {
    background: var(--accent-hover);
  }
  .evs {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow: hidden;
  }
  .ev {
    flex: none;
    display: flex;
    align-items: center;
    gap: 4px;
    height: 20px;
    padding: 0 6px 0 5px;
    border: 1px solid transparent;
    border-left-width: 3px;
    border-radius: var(--radius-control);
    font: var(--text-caption);
    color: var(--fg-1);
    text-align: left;
    min-width: 0;
    transition: box-shadow var(--dur-fast) var(--ease-in);
  }
  .ev-time {
    flex: none;
    color: var(--fg-3);
  }
  .ev-subject {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 600;
  }
  .ev.busy {
    background: var(--accent-soft);
    border-left-color: var(--accent);
  }
  .ev.tentative {
    background:
      repeating-linear-gradient(135deg, var(--accent-soft) 0, var(--accent-soft) 3px, transparent 3px, transparent 7px),
      var(--bg-layer-alt);
    border-color: var(--stroke-1);
    border-left-color: var(--accent);
    opacity: 0.85;
  }
  .ev.free {
    background: var(--bg-layer-alt);
    border-color: var(--stroke-2);
    border-left-color: var(--accent);
  }
  .ev.oof {
    background: var(--cal-oof-soft);
    border-left-color: var(--cal-oof);
  }
  .ev.elsewhere {
    background: var(--bg-layer-alt);
    border-color: var(--stroke-2);
    border-left-color: var(--accent);
    border-left-style: dashed;
  }
  .ev.nodata {
    background: var(--bg-hover);
    border-left-color: var(--fg-4);
  }
  .ev.organizer .ev-subject {
    color: var(--accent);
  }
  .ev.cancelled {
    opacity: 0.7;
  }
  .ev.cancelled .ev-subject {
    text-decoration: line-through;
    color: var(--fg-3);
  }
  .ev.declined {
    opacity: 0.55;
  }
  .ev:hover {
    box-shadow: var(--shadow-8);
    z-index: 1;
  }
  .ev.selected {
    box-shadow:
      0 0 0 2px var(--accent),
      var(--shadow-2);
    z-index: 1;
  }
  .more {
    flex: none;
    align-self: flex-start;
    height: 18px;
    padding: 0 6px;
    font: var(--text-caption);
    color: var(--accent);
    border-radius: var(--radius-control);
  }
  .more:hover {
    background: var(--bg-hover);
  }
</style>
