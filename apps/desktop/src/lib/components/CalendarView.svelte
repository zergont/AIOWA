<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, errorMessage, type CalendarEvent } from "../api";
  import { periodTitle, shiftAnchor, startOfDay, updatedLabel, viewRange, type CalendarViewKind } from "../calendar";
  import WeekGrid from "./WeekGrid.svelte";
  import EventDetails from "./EventDetails.svelte";

  interface Props {
    /** Якорный день: для вида «день» — сам день, для недель — любой день недели. Меняется и отсюда (стрелки, «Сегодня»), и снаружи (мини-календарь). */
    anchor: Date;
    kind: CalendarViewKind;
    /** Раздел календаря на экране: спрятанный календарь не ловит клавиши и не обновляется сам. */
    visible?: boolean;
  }
  let { anchor = $bindable(), kind = $bindable(), visible = true }: Props = $props();

  /** Кэш окна старше этого срока дотягивается с сервера фоном; тот же период у автообновления. */
  const STALE_MS = 5 * 60_000;
  /**
   * Пауза между показом кэша и догрузкой с сервера: быстрое листание стрелками не шлёт запрос
   * за каждый промелькнувший период — догружается только тот, на котором остановились.
   */
  const REFRESH_DELAY_MS = 250;
  const KINDS: { kind: CalendarViewKind; label: string; keys: string }[] = [
    { kind: "day", label: "День", keys: "D" },
    { kind: "work", label: "Рабочая неделя", keys: "Shift+W" },
    { kind: "week", label: "Неделя", keys: "W" },
  ];

  let events = $state<CalendarEvent[]>([]);
  /** Запрос к бэкенду в полёте (из кэша отвечает мгновенно, с сервера — до минуты). */
  let loading = $state(false);
  let error = $state<string | null>(null);
  let fetchedMs = $state<number | null>(null);
  let selectedPk = $state<number | null>(null);
  let now = $state(new Date());

  const range = $derived(viewRange(kind, anchor));
  const title = $derived(periodTitle(kind, range));
  const selectedEvent = $derived(selectedPk == null ? null : (events.find((e) => e.calendar_pk === selectedPk) ?? null));
  const statusText = $derived.by(() => {
    if (loading) return "Обновляю…";
    if (fetchedMs != null) return `обновлено ${updatedLabel(fetchedMs, now)}`;
    return "";
  });

  /** Номер последнего запроса: ответ устаревшего (другой период) не подменяет события. */
  let request = 0;
  /** Когда последний раз ходили к бэкенду: автообновление отсчитывает от него и от fetched_ms. */
  let lastAttempt = 0;

  async function load(startMs: number, endMs: number, refresh: boolean) {
    const req = ++request;
    loading = true;
    lastAttempt = Date.now();
    try {
      const r = await api.calendarEvents(startMs, endMs, refresh);
      if (req !== request) return;
      events = r.events;
      fetchedMs = r.fetched_ms;
      error = r.error;
      // Без refresh бэкенд всегда отвечает кэшем (что есть): если он старый или окно ещё не забирали
      // (fetched_ms == null), дотянуть с сервера фоном — с паузой, поверх уже показанных событий.
      // Если бэкенд только что отказал (error), повторять сразу не нужно — это сделает автообновление.
      if (!refresh && r.from_cache && !r.error && (r.fetched_ms == null || Date.now() - r.fetched_ms > STALE_MS)) {
        setTimeout(() => {
          if (req === request) void load(startMs, endMs, true);
        }, REFRESH_DELAY_MS);
      }
    } catch (e) {
      if (req === request) error = errorMessage(e);
    } finally {
      if (req === request) loading = false;
    }
  }

  // Смена периода: сначала кэш (мгновенно), затем при необходимости сервер.
  $effect(() => {
    const startMs = range.start.getTime();
    const endMs = range.end.getTime();
    untrack(() => void load(startMs, endMs, false));
  });

  function refresh() {
    void load(range.start.getTime(), range.end.getTime(), true);
  }

  /** Автообновление раз в 5 минут при видимом окне и показанном календаре; не запускается поверх идущего запроса. */
  function tick() {
    if (!visible || document.visibilityState !== "visible" || loading) return;
    const since = Math.max(lastAttempt, fetchedMs ?? 0);
    if (Date.now() - since < STALE_MS) return;
    refresh();
  }

  onMount(() => {
    const clock = setInterval(() => (now = new Date()), 30_000);
    const poll = setInterval(tick, 30_000);
    document.addEventListener("visibilitychange", tick);
    return () => {
      clearInterval(clock);
      clearInterval(poll);
      document.removeEventListener("visibilitychange", tick);
    };
  });

  // Календарь снова на экране после почты: линия «сейчас» на месте и, если кэш успел устареть, обновление.
  $effect(() => {
    if (!visible) return;
    untrack(() => {
      now = new Date();
      tick();
    });
  });

  function goToday() {
    anchor = startOfDay(new Date());
  }
  function shift(dir: -1 | 1) {
    anchor = shiftAnchor(kind, anchor, dir);
  }

  /** Фокус в поле ввода или во фрейме: клавиши-буквы и стрелки там не наши. */
  function inEditable(): boolean {
    const el = document.activeElement;
    return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLIFrameElement || (el instanceof HTMLElement && el.isContentEditable);
  }

  function onKeydown(e: KeyboardEvent) {
    if (!visible || e.ctrlKey || e.metaKey || e.altKey || inEditable()) return;
    if (e.key === "ArrowLeft") shift(-1);
    else if (e.key === "ArrowRight") shift(1);
    else if (e.key === "Escape") {
      if (selectedPk == null) return;
      selectedPk = null;
    }
    // По коду клавиши, а не по символу: в русской раскладке e.key даёт кириллицу.
    else if (e.code === "KeyT") goToday();
    else if (e.code === "KeyD") kind = "day";
    else if (e.code === "KeyW") kind = e.shiftKey ? "work" : "week";
    else return;
    e.preventDefault();
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="cal">
  <div class="toolbar" role="toolbar" aria-label="Календарь">
    <button class="btn" onclick={goToday} title="Сегодня (T)">Сегодня</button>
    <div class="arrows">
      <button class="icon-btn" onclick={() => shift(-1)} aria-label="Предыдущий период" title="Назад (←)">
        <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M7.5 2.5 4 6l3.5 3.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="icon-btn" onclick={() => shift(1)} aria-label="Следующий период" title="Вперёд (→)">
        <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M4.5 2.5 8 6l-3.5 3.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
    </div>
    <h2 class="title" aria-live="polite">{title}</h2>
    <div class="segmented" role="group" aria-label="Вид">
      {#each KINDS as k (k.kind)}
        <button class:on={kind === k.kind} aria-pressed={kind === k.kind} onclick={() => (kind = k.kind)} title="{k.label} ({k.keys})">{k.label}</button>
      {/each}
    </div>
    <span class="status caption" aria-live="polite">{statusText}</span>
    <button class="btn" onclick={refresh} disabled={loading} title="Перечитать события с сервера">
      <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5v3h-3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Обновить
    </button>
  </div>

  {#if error}
    <div class="error-row" role="alert">
      <span class="error-text">{error}</span>
      <button class="btn small" onclick={() => (error = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
    </div>
  {/if}

  <div class="main">
    <div class="grid-wrap">
      <WeekGrid days={range.days} {events} {selectedPk} {now} onselect={(ev) => (selectedPk = ev.calendar_pk)} />
    </div>
    {#if selectedEvent}
      <aside class="details" aria-label="Событие">
        <EventDetails event={selectedEvent} onclose={() => (selectedPk = null)} />
      </aside>
    {/if}
  </div>
</div>

<style>
  .cal {
    height: 100%;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .toolbar {
    flex: none;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--stroke-1);
    min-width: 0;
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .btn:disabled {
    color: var(--fg-disabled);
  }
  .btn.small {
    height: 24px;
    padding: 0 10px;
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .arrows {
    display: inline-flex;
    gap: 2px;
  }
  .icon-btn {
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-2);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .icon-btn:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .title {
    flex: 1;
    min-width: 0;
    margin: 0;
    font: var(--text-subtitle);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .status {
    white-space: nowrap;
  }
  .error-row {
    flex: none;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-4);
    font: var(--text-caption);
    color: var(--danger);
    border-bottom: 1px solid var(--stroke-1);
    min-width: 0;
  }
  .error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .main {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .grid-wrap {
    flex: 1;
    min-width: 0;
  }
  .details {
    flex: none;
    width: 340px;
    min-width: 0;
    border-left: 1px solid var(--stroke-1);
    background: var(--bg-layer-alt);
  }
</style>
