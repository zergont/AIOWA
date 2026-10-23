<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { addMonths, dayKey, dayLong, isWeekend, monthGrid, monthTitle, sameDay, startOfDay, startOfMonth, type ViewRange } from "../calendar";

  interface Props {
    /** Якорный день сетки календаря: подсвечен, клик по дню меняет его. */
    selected: Date;
    /** Период, показанный в сетке: его дни подсвечены. */
    range?: ViewRange | null;
    onselect: (d: Date) => void;
  }
  let { selected, range = null, onselect }: Props = $props();

  const WEEKDAYS = ["пн", "вт", "ср", "чт", "пт", "сб", "вс"];

  // Показанный месяц листается отдельно от выбора; смена выбранного дня (стрелки в сетке,
  // «Сегодня») возвращает к его месяцу. Нужно именно начальное значение: дальше его ведёт эффект.
  let month = $state(startOfMonth(untrack(() => selected)));
  $effect(() => {
    const m = startOfMonth(selected);
    untrack(() => {
      if (m.getTime() !== month.getTime()) month = m;
    });
  });

  // Сегодняшний день обновляется сам: окно может висеть открытым через полночь.
  let today = $state(startOfDay(new Date()));
  onMount(() => {
    const t = setInterval(() => {
      const d = startOfDay(new Date());
      if (!sameDay(d, today)) today = d;
    }, 60_000);
    return () => clearInterval(t);
  });

  const cells = $derived(monthGrid(month));
  const rangeKeys = $derived.by(() => {
    if (!range || !range.days.length) return null;
    return { from: dayKey(range.days[0]), to: dayKey(range.days[range.days.length - 1]) };
  });

  function inRange(d: Date): boolean {
    if (!rangeKeys) return false;
    const k = dayKey(d);
    return k >= rangeKeys.from && k <= rangeKeys.to;
  }
</script>

<div class="mini">
  <div class="head">
    <button class="nav" onclick={() => (month = addMonths(month, -1))} aria-label="Предыдущий месяц" title="Предыдущий месяц">
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M7.5 2.5 4 6l3.5 3.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
    <button class="title" onclick={() => (month = startOfMonth(today))} title="К текущему месяцу">{monthTitle(month)}</button>
    <button class="nav" onclick={() => (month = addMonths(month, 1))} aria-label="Следующий месяц" title="Следующий месяц">
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2.5 8 6l-3.5 3.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
  </div>
  <div class="names" aria-hidden="true">
    {#each WEEKDAYS as w, i (w)}
      <span class="name" class:weekend={i >= 5}>{w}</span>
    {/each}
  </div>
  <div class="days" role="group" aria-label="Дни месяца">
    {#each cells as d (d.getTime())}
      {@const isSelected = sameDay(d, selected)}
      <button class="day" class:other={d.getMonth() !== month.getMonth()} class:today={sameDay(d, today)} class:selected={isSelected} class:inrange={inRange(d)} class:weekend={isWeekend(d)} onclick={() => onselect(d)} aria-label={dayLong(d)} aria-pressed={isSelected}>
        {d.getDate()}
      </button>
    {/each}
  </div>
</div>

<style>
  .mini {
    padding: var(--space-1) var(--space-1) var(--space-2);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 2px;
    height: 32px;
    margin-bottom: var(--space-1);
  }
  .nav {
    width: 28px;
    height: 28px;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .nav:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .title {
    flex: 1;
    min-width: 0;
    height: 28px;
    font: var(--text-body-strong);
    color: var(--fg-1);
    border-radius: var(--radius-control);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .title:hover {
    background: var(--bg-hover);
  }
  .names,
  .days {
    display: grid;
    grid-template-columns: repeat(7, minmax(0, 1fr));
    gap: 2px;
  }
  .name {
    font: var(--text-caption);
    color: var(--fg-3);
    text-align: center;
    line-height: 20px;
  }
  .name.weekend {
    color: var(--fg-4);
  }
  .day {
    position: relative;
    height: 28px;
    font: var(--text-caption);
    color: var(--fg-2);
    border-radius: var(--radius-control);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .day:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .day.weekend {
    color: var(--fg-3);
  }
  .day.other {
    color: var(--fg-4);
  }
  .day.inrange {
    background: var(--bg-selected);
    border-radius: 0;
  }
  .day.inrange:hover {
    background: var(--bg-pressed);
  }
  .day.today {
    font-weight: 600;
    color: var(--accent);
  }
  .day.selected {
    background: var(--accent);
    color: var(--fg-on-accent);
    border-radius: var(--radius-control);
  }
  .day.selected:hover {
    background: var(--accent-hover);
  }
</style>
