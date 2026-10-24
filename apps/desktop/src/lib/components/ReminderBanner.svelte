<script lang="ts">
  import { onMount } from "svelte";
  import type { ReminderDto } from "../api";
  import { dayLong, eventWhen, reminderCountdown, sameDay, timeRange } from "../calendar";

  /**
   * Всплывающая панель напоминаний (правый нижний угол, над строкой состояния — там нет
   * элементов управления, которые она закрывала бы): тема, через сколько, когда и место.
   * Сворачивается в одну строку «Напоминания (N)»; новое напоминание разворачивает её.
   * Список ведёт родитель; здесь только отсев просроченных (через 30 минут после начала)
   * и обратный отсчёт, который тикает раз в 30 секунд.
   */
  interface Props {
    reminders: ReminderDto[];
    /** Клик по теме: открыть календарь на событии. */
    onopen: (r: ReminderDto) => void;
    ondismiss: (calendarPk: number) => void;
    onsnooze: (calendarPk: number, minutes: number) => void;
    ondismissall: () => void;
  }
  let { reminders, onopen, ondismiss, onsnooze, ondismissall }: Props = $props();

  /** После этого срока с начала напоминание просрочено и не показывается. */
  const OVERDUE_MS = 30 * 60_000;
  const SNOOZE_MIN = 5;

  let now = $state(Date.now());
  onMount(() => {
    const t = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(t);
  });

  const shown = $derived(reminders.filter((r) => now <= r.start_ms + OVERDUE_MS));

  let collapsed = $state(false);
  // Новое напоминание (список вырос) разворачивает свёрнутую панель.
  let lastCount = 0;
  $effect(() => {
    const n = shown.length;
    if (n > lastCount) collapsed = false;
    lastCount = n;
  });

  /** Когда событие: у «весь день» — дни; у остального — время, с датой, если событие не сегодня. */
  function when(r: ReminderDto): string {
    if (r.is_all_day) return eventWhen(r);
    const range = timeRange(r.start_ms, r.end_ms);
    const start = new Date(r.start_ms);
    return sameDay(start, new Date(now)) ? range : `${dayLong(start)}, ${range}`;
  }
</script>

{#if shown.length}
  <section class="banner" class:collapsed aria-label="Напоминания" role="status" aria-live="polite">
    <header class="head">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2.5a3.6 3.6 0 0 0-3.6 3.6v2.4L3 10.6h10l-1.4-2.1V6.1A3.6 3.6 0 0 0 8 2.5Zm-1.5 10a1.5 1.5 0 0 0 3 0" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      <span class="head-title">{shown.length === 1 ? "Напоминание" : `Напоминания (${shown.length})`}</span>
      {#if shown.length > 1 && !collapsed}
        <button class="link" onclick={ondismissall}>Закрыть все</button>
      {/if}
      <button class="toggle" onclick={() => (collapsed = !collapsed)} aria-expanded={!collapsed} aria-label={collapsed ? "Развернуть напоминания" : "Свернуть напоминания"} title={collapsed ? "Развернуть" : "Свернуть"}>
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d={collapsed ? "m3 10 5-5 5 5" : "m3 6 5 5 5-5"} fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
    </header>
    {#if !collapsed}
      <ul class="list">
        {#each shown as r (r.calendar_pk)}
          <li class="item">
            <button class="subject" onclick={() => onopen(r)} title="Открыть в календаре">
              <span class="subject-text">{r.subject || "(без темы)"}</span>
              <span class="countdown">{reminderCountdown(r.start_ms, now)}</span>
            </button>
            <div class="meta caption">
              {when(r)}{r.location ? ` · ${r.location}` : ""}
            </div>
            <div class="buttons">
              <button class="btn" onclick={() => onsnooze(r.calendar_pk, SNOOZE_MIN)}>Отложить {SNOOZE_MIN} мин</button>
              <button class="btn" onclick={() => ondismiss(r.calendar_pk)}>Закрыть</button>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{/if}

<style>
  .banner {
    position: fixed;
    bottom: 36px;
    right: 16px;
    z-index: 80;
    width: 320px;
    max-height: calc(100vh - 100px);
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
    color: var(--fg-1);
  }
  .banner.collapsed {
    width: auto;
  }
  .banner.collapsed .head {
    border-bottom: 0;
  }
  .head {
    flex: none;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
    color: var(--accent);
  }
  .toggle {
    flex: none;
    width: 24px;
    height: 24px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
  }
  .toggle:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .head-title {
    flex: 1;
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  .link {
    font: var(--text-caption);
    color: var(--accent);
    padding: 0 4px;
    border-radius: var(--radius-control);
  }
  .link:hover {
    text-decoration: underline;
  }
  .list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    padding: var(--space-2) var(--space-3) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
  }
  .item:last-child {
    border-bottom: 0;
  }
  .subject {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    width: 100%;
    text-align: left;
    border-radius: var(--radius-control);
    padding: 2px 4px;
    margin: 0 -4px;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .subject:hover {
    background: var(--bg-hover);
  }
  .subject-text {
    flex: 1;
    min-width: 0;
    font: var(--text-body-strong);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .countdown {
    flex: none;
    font: var(--text-caption);
    color: var(--warning);
    white-space: nowrap;
  }
  .meta {
    margin-top: 2px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .buttons {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-2);
  }
  .btn {
    height: 26px;
    padding: 0 10px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    font: var(--text-caption);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
</style>
