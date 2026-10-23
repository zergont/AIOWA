<script lang="ts">
  import type { CalendarEvent } from "../api";
  import { allDayBars, eventLabel, eventTone, eventWhen, hourLabel, isWeekend, nowPosition, sameDay, timedSegments, timeRange, weekdayShort } from "../calendar";

  interface Props {
    /** Дни периода (местные полуночи), по колонке на день. */
    days: Date[];
    events: CalendarEvent[];
    selectedPk: number | null;
    /** Текущее время: линия «сейчас» и подсветка сегодняшнего дня. */
    now: Date;
    onselect: (ev: CalendarEvent) => void;
  }
  let { days, events, selectedPk, now, onselect }: Props = $props();

  /** Высота часа в пикселях; та же величина уходит в стили как --hour (линии сетки). */
  const HOUR = 48;
  /** К этому часу сетка прокручивается при открытии. */
  const SCROLL_TO_HOUR = 7;
  const HOURS = Array.from({ length: 23 }, (_, i) => i + 1);
  /**
   * Ниже этой высоты блок показывает одну строку (тему), без времени и места: две строки
   * по 16px (--text-caption) + padding 2+2 + border 1+1 = 38, иначе вторая строка срезалась бы снизу.
   */
  const TWO_LINES_PX = 38;

  const columns = $derived(timedSegments(events, days));
  const allDay = $derived(allDayBars(events, days));
  const nowPos = $derived(nowPosition(now, days));

  let scroller = $state<HTMLDivElement | null>(null);
  let scrolledOnce = false;
  $effect(() => {
    if (!scroller || scrolledOnce) return;
    scrolledOnce = true;
    scroller.scrollTop = SCROLL_TO_HOUR * HOUR - 6;
  });

  function top(min: number): number {
    return (min / 60) * HOUR;
  }
  function height(startMin: number, endMin: number): number {
    return Math.max(22, ((endMin - startMin) / 60) * HOUR - 2);
  }
  function tooltip(ev: CalendarEvent): string {
    return [ev.subject || "(без темы)", eventWhen(ev), ev.location, ev.is_cancelled ? "Отменена" : null].filter(Boolean).join("\n");
  }
</script>

<div class="grid" style="--hour: {HOUR}px; --days: {days.length}">
  <div class="head">
    <div class="gutter"></div>
    {#each days as d, i (i)}
      <div class="day-head" class:today={sameDay(d, now)} class:weekend={isWeekend(d)}>
        <span class="wd">{weekdayShort(d)}</span>
        <span class="num">{d.getDate()}</span>
      </div>
    {/each}
  </div>

  <div class="allday" style="--rows: {Math.max(1, allDay.rows)}">
    <div class="gutter caption">весь день</div>
    <div class="allday-cells">
      {#each days as d, i (i)}
        <div class="allday-cell" class:today={sameDay(d, now)} class:weekend={isWeekend(d)} style="grid-column: {i + 1}"></div>
      {/each}
      {#each allDay.bars as b (b.event.calendar_pk)}
        <button
          class="ev bar {eventTone(b.event)}"
          class:selected={b.event.calendar_pk === selectedPk}
          class:cancelled={b.event.is_cancelled}
          class:declined={b.event.my_response === "decline"}
          class:organizer={b.event.my_response === "organizer"}
          class:before={b.continuesBefore}
          class:after={b.continuesAfter}
          style="grid-column: {b.first + 1} / {b.last + 2}; grid-row: {b.row + 1}"
          onclick={() => onselect(b.event)}
          title={tooltip(b.event)}
          aria-label={eventLabel(b.event)}
          aria-pressed={b.event.calendar_pk === selectedPk}
        >
          <span class="ev-subject">{b.event.subject || "(без темы)"}</span>
          {#if b.event.location}<span class="ev-meta">{b.event.location}</span>{/if}
        </button>
      {/each}
    </div>
  </div>

  <div class="scroller" bind:this={scroller}>
    <div class="body">
      <div class="hours" aria-hidden="true">
        {#each HOURS as h (h)}
          <span class="hour" style="top: {h * HOUR}px">{hourLabel(h)}</span>
        {/each}
      </div>
      <div class="cols">
        {#each days as d, i (i)}
          <div class="col" class:today={sameDay(d, now)} class:weekend={isWeekend(d)}>
            {#each columns[i] as s (s.event.calendar_pk)}
              {@const h = height(s.startMin, s.endMin)}
              <button
                class="ev {eventTone(s.event)}"
                class:selected={s.event.calendar_pk === selectedPk}
                class:cancelled={s.event.is_cancelled}
                class:declined={s.event.my_response === "decline"}
                class:organizer={s.event.my_response === "organizer"}
                class:before={s.continuesBefore}
                class:after={s.continuesAfter}
                style="top: {top(s.startMin)}px; height: {h}px; left: calc({(s.col / s.cols) * 100}% + 2px); width: calc({100 / s.cols}% - 4px)"
                onclick={() => onselect(s.event)}
                title={tooltip(s.event)}
                aria-label={eventLabel(s.event)}
                aria-pressed={s.event.calendar_pk === selectedPk}
              >
                <span class="ev-subject">{s.event.subject || "(без темы)"}</span>
                {#if h >= TWO_LINES_PX}
                  <span class="ev-meta">{timeRange(s.event.start_ms, s.event.end_ms)}{s.event.location ? ` · ${s.event.location}` : ""}</span>
                {/if}
              </button>
            {/each}
            {#if nowPos && nowPos.day === i}
              <div class="now" style="top: {top(nowPos.minutes)}px" aria-hidden="true"></div>
            {/if}
          </div>
        {/each}
      </div>
    </div>
  </div>
</div>

<style>
  .grid {
    height: 100%;
    min-width: 0;
    display: flex;
    flex-direction: column;
    --gutter: 56px;
  }

  /* Шапка и строка «весь день» резервируют место под полосу прокрутки, чтобы колонки совпадали с сеткой ниже. */
  .head,
  .allday {
    flex: none;
    display: grid;
    overflow: hidden;
    scrollbar-gutter: stable;
  }
  .head {
    grid-template-columns: var(--gutter) repeat(var(--days), minmax(0, 1fr));
    height: 44px;
  }
  .day-head {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    border-left: 1px solid var(--stroke-1);
    color: var(--fg-2);
    min-width: 0;
  }
  .wd {
    font: var(--text-caption);
  }
  .num {
    font: var(--text-body-strong);
    min-width: 26px;
    height: 26px;
    line-height: 26px;
    text-align: center;
    border-radius: 50%;
  }
  .day-head.weekend {
    color: var(--fg-3);
  }
  .day-head.today {
    color: var(--fg-1);
  }
  .day-head.today .num {
    background: var(--accent);
    color: var(--fg-on-accent);
  }

  .allday {
    grid-template-columns: var(--gutter) minmax(0, 1fr);
    border-top: 1px solid var(--stroke-1);
    border-bottom: 1px solid var(--stroke-2);
  }
  .allday .gutter {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    padding-right: 8px;
    color: var(--fg-4);
  }
  .allday-cells {
    display: grid;
    grid-template-columns: repeat(var(--days), minmax(0, 1fr));
    grid-template-rows: repeat(var(--rows), 24px);
    row-gap: 2px;
    padding: 3px 0;
    min-width: 0;
  }
  .allday-cell {
    grid-row: 1 / -1;
    border-left: 1px solid var(--stroke-1);
  }
  .allday-cell.weekend {
    background: var(--cal-weekend);
  }
  .allday-cell.today {
    background: var(--cal-today);
  }

  .scroller {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    scrollbar-gutter: stable;
  }
  .body {
    display: grid;
    grid-template-columns: var(--gutter) minmax(0, 1fr);
    height: calc(var(--hour) * 24);
  }
  .hours {
    position: relative;
  }
  .hour {
    position: absolute;
    right: 8px;
    transform: translateY(-50%);
    font: var(--text-caption);
    color: var(--fg-4);
  }
  .cols {
    display: grid;
    grid-template-columns: repeat(var(--days), minmax(0, 1fr));
    min-width: 0;
  }
  .col {
    position: relative;
    min-width: 0;
    border-left: 1px solid var(--stroke-1);
    background-image: repeating-linear-gradient(to bottom, var(--stroke-1) 0, var(--stroke-1) 1px, transparent 1px, transparent var(--hour));
  }
  .col.weekend {
    background-color: var(--cal-weekend);
  }
  .col.today {
    background-color: var(--cal-today);
  }

  /* Блок события: цвет по занятости, свой ответ и отмена — поверх. */
  .ev {
    position: absolute;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    justify-content: flex-start;
    overflow: hidden;
    padding: 2px 6px;
    border: 1px solid transparent;
    border-left-width: 3px;
    border-radius: var(--radius-control);
    text-align: left;
    font: var(--text-caption);
    color: var(--fg-1);
    box-sizing: border-box;
    transition: box-shadow var(--dur-fast) var(--ease-in);
  }
  .ev.bar {
    position: relative;
    margin: 0 2px;
    height: 24px;
    flex-direction: row;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .ev-subject {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
    flex: none;
  }
  .ev-meta {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
    color: var(--fg-2);
  }
  .ev.bar .ev-subject,
  .ev.bar .ev-meta {
    flex: 0 1 auto;
    min-width: 0;
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
  .ev.organizer {
    border-left-width: 4px;
  }
  .ev.organizer .ev-subject {
    color: var(--accent);
  }
  .ev.cancelled {
    opacity: 0.7;
  }
  .ev.cancelled .ev-subject,
  .ev.cancelled .ev-meta {
    text-decoration: line-through;
    color: var(--fg-3);
  }
  .ev.declined {
    opacity: 0.55;
  }
  /* Продолжение с прошлого дня / на следующий: срезанный край с пунктиром. */
  .ev.before {
    border-top-left-radius: 0;
    border-top-right-radius: 0;
    border-top: 1px dashed var(--fg-4);
  }
  .ev.after {
    border-bottom-left-radius: 0;
    border-bottom-right-radius: 0;
    border-bottom: 1px dashed var(--fg-4);
  }
  .ev.bar.before {
    border-radius: 0 var(--radius-control) var(--radius-control) 0;
    border-top: 1px solid transparent;
    border-left: 1px dashed var(--fg-4);
  }
  .ev.bar.after {
    border-radius: var(--radius-control) 0 0 var(--radius-control);
    border-bottom: 1px solid transparent;
    border-right: 1px dashed var(--fg-4);
  }
  .ev:hover {
    box-shadow: var(--shadow-8);
    z-index: 2;
  }
  .ev.selected {
    box-shadow:
      0 0 0 2px var(--accent),
      var(--shadow-2);
    z-index: 2;
  }
  .ev:focus-visible {
    z-index: 3;
  }

  .now {
    position: absolute;
    left: 0;
    right: 0;
    height: 2px;
    background: var(--cal-now);
    z-index: 4;
    pointer-events: none;
  }
  .now::before {
    content: "";
    position: absolute;
    left: -4px;
    top: -3px;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--cal-now);
  }
</style>
