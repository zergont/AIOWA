<script lang="ts">
  import { untrack } from "svelte";
  import { api, errorMessage, personLabel, type Attendee, type CalendarEvent, type CalendarEventDetails } from "../api";
  import { FREE_BUSY_LABEL, MY_RESPONSE_LABEL, RESPONSE_LABEL, eventWhen, reminderLabel, sensitivityLabel } from "../calendar";
  import BodyFrame from "./BodyFrame.svelte";

  interface Props {
    /** Событие из сетки; подробности панель грузит сама. */
    event: CalendarEvent;
    onclose: () => void;
  }
  let { event, onclose }: Props = $props();

  let details = $state<CalendarEventDetails | null>(null);
  let loading = $state(false);
  /** Идёт перечитывание с сервера по кнопке (в отличие от тихого чтения кэша). */
  let refreshing = $state(false);
  let error = $state<string | null>(null);
  /** Номер последнего запроса: ответ устаревшего не подменяет панель другого события. */
  let request = 0;

  /** Шапка — из подробностей, если они загружены для этого же события, иначе из сетки. */
  const ev = $derived(details && details.event.calendar_pk === event.calendar_pk ? details.event : event);
  const required = $derived((details?.attendees ?? []).filter((a) => a.required));
  const optional = $derived((details?.attendees ?? []).filter((a) => !a.required));
  const myResponse = $derived(ev.is_meeting ? MY_RESPONSE_LABEL[ev.my_response] : null);
  const reminder = $derived(reminderLabel(details?.reminder_minutes ?? null));
  const recurring = $derived(ev.calendar_type === "occurrence" || ev.calendar_type === "exception" || ev.calendar_type === "recurring_master");

  async function load(pk: number, refresh: boolean) {
    const req = ++request;
    loading = true;
    refreshing = refresh;
    error = null;
    try {
      const d = await api.calendarEventDetails(pk, refresh);
      if (req === request) {
        details = d;
        // Сервер не ответил, показаны подробности из кэша: скажем об этом, не пряча их.
        error = d.error ?? null;
      }
    } catch (e) {
      if (req === request) error = errorMessage(e);
    } finally {
      if (req === request) {
        loading = false;
        refreshing = false;
      }
    }
  }

  // Другое событие — подробности с нуля. Тот же pk в новом объекте (сетка перечитана после обновления)
  // — тихое перечитывание из кэша, старые подробности остаются на экране, пока не придут свежие.
  $effect(() => {
    const pk = event.calendar_pk;
    untrack(() => {
      if (details?.event.calendar_pk !== pk) details = null;
      void load(pk, false);
    });
  });

  function refresh() {
    void load(event.calendar_pk, true);
  }

  function responseClass(a: Attendee): string {
    switch (a.response) {
      case "accept":
      case "organizer":
        return "ok";
      case "tentative":
        return "maybe";
      case "decline":
        return "no";
      default:
        return "none";
    }
  }
</script>

{#snippet attendeeList(list: Attendee[])}
  <ul class="people">
    {#each list as a}
      <li class="person">
        <span class="dot {responseClass(a)}" aria-hidden="true"></span>
        <span class="person-name">{personLabel(a.name, a.email) || "(без имени)"}</span>
        {#if a.name && a.email}<span class="person-email caption">{a.email}</span>{/if}
        <span class="person-resp caption">{RESPONSE_LABEL[a.response]}</span>
      </li>
    {/each}
  </ul>
{/snippet}

<div class="panel">
  <header class="head">
    <div class="head-row">
      <h2 class="subject" class:cancelled={ev.is_cancelled}>{ev.subject || "(без темы)"}</h2>
      <button class="icon-btn" onclick={refresh} disabled={loading} aria-label="Обновить" title="Перечитать событие с сервера" aria-busy={loading}>
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5v3h-3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="icon-btn" onclick={onclose} aria-label="Закрыть" title="Закрыть (Esc)">
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="m4 4 8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      </button>
    </div>
    <div class="badges">
      {#if ev.is_cancelled}<span class="badge danger">отменена</span>{/if}
      {#if FREE_BUSY_LABEL[ev.free_busy]}<span class="badge tone-{ev.free_busy}">{FREE_BUSY_LABEL[ev.free_busy]}</span>{/if}
      {#if recurring}<span class="badge">повторяющаяся</span>{/if}
      {#if sensitivityLabel(ev.sensitivity)}<span class="badge">{sensitivityLabel(ev.sensitivity)}</span>{/if}
      {#if ev.has_attachments}<span class="badge">с вложениями</span>{/if}
    </div>
  </header>

  <div class="content">
    <dl class="facts">
      <div class="fact">
        <dt>Когда</dt>
        <dd>{eventWhen(ev)}</dd>
      </div>
      {#if ev.location}
        <div class="fact">
          <dt>Где</dt>
          <dd>{ev.location}</dd>
        </div>
      {/if}
      {#if ev.organizer}
        <div class="fact">
          <dt>Организатор</dt>
          <dd>
            {personLabel(ev.organizer.name, ev.organizer.email) || "—"}
            {#if ev.organizer.name && ev.organizer.email}<span class="caption">{ev.organizer.email}</span>{/if}
          </dd>
        </div>
      {/if}
      {#if myResponse}
        <div class="fact">
          <dt>Мой ответ</dt>
          <dd>{myResponse}</dd>
        </div>
      {/if}
      {#if reminder}
        <div class="fact">
          <dt>Напоминание</dt>
          <dd>{reminder}</dd>
        </div>
      {/if}
      {#if details?.recurrence_text}
        <div class="fact">
          <dt>Повторение</dt>
          <dd>{details.recurrence_text}</dd>
        </div>
      {/if}
    </dl>

    {#if loading && (!details || refreshing)}
      <div class="note" aria-live="polite">
        <span class="spinner" aria-hidden="true"></span>
        <span class="caption">{refreshing ? "Перечитываю с сервера…" : "Загружаю подробности…"}</span>
      </div>
    {:else if error}
      <div class="note failed" role="alert">
        <span class="note-text">{error}</span>
        <button class="btn" onclick={() => load(event.calendar_pk, true)}>Повторить</button>
      </div>
    {/if}

    {#if details}
      {#if details.attendees.length}
        <section class="section" aria-label="Участники">
          <h3 class="section-title">Участники <span class="caption">{details.attendees.length}</span></h3>
          {#if required.length}
            {@render attendeeList(required)}
          {/if}
          {#if optional.length}
            <h4 class="sub caption">Необязательные</h4>
            {@render attendeeList(optional)}
          {/if}
        </section>
      {/if}
      <section class="section body" aria-label="Описание">
        {#if details.body_html}
          <BodyFrame html={details.body_html} title="Описание события" />
        {:else if !loading}
          <p class="caption empty">Описания нет.</p>
        {/if}
      </section>
    {/if}
  </div>
</div>

<style>
  .panel {
    height: 100%;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .head {
    flex: none;
    padding: var(--space-3) var(--space-3) var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--stroke-1);
  }
  .head-row {
    display: flex;
    align-items: flex-start;
    gap: var(--space-1);
  }
  .subject {
    flex: 1;
    min-width: 0;
    margin: 0;
    font: var(--text-body-strong);
    font-size: 16px;
    line-height: 24px;
    overflow-wrap: anywhere;
    user-select: text;
  }
  .subject.cancelled {
    text-decoration: line-through;
    color: var(--fg-3);
  }
  .icon-btn {
    flex: none;
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .icon-btn:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .icon-btn:disabled {
    color: var(--fg-disabled);
  }
  .badges {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: var(--space-1);
  }
  .badges:empty {
    display: none;
  }
  .badge {
    font: var(--text-caption);
    color: var(--fg-3);
    border: 1px solid var(--stroke-2);
    border-radius: 3px;
    padding: 0 6px;
    line-height: 18px;
  }
  .badge.danger {
    color: var(--danger);
    border-color: var(--danger);
  }
  .badge.tone-busy {
    color: var(--accent);
    border-color: var(--accent);
  }
  .badge.tone-tentative {
    color: var(--warning);
    border-color: var(--warning);
  }
  .badge.tone-oof {
    color: var(--cal-oof);
    border-color: var(--cal-oof);
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--space-3) var(--space-4) var(--space-5);
    user-select: text;
  }
  .facts {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .fact {
    display: grid;
    grid-template-columns: 96px minmax(0, 1fr);
    gap: var(--space-2);
    align-items: baseline;
  }
  .fact dt {
    font: var(--text-caption);
    color: var(--fg-3);
  }
  .fact dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .fact dd .caption {
    display: block;
  }

  .note {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
  .note.failed {
    color: var(--danger);
  }
  .note-text {
    flex: 1;
    min-width: 0;
  }
  .btn {
    flex: none;
    height: 24px;
    padding: 0 10px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-2);
    font: var(--text-caption);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
  .spinner {
    width: 12px;
    height: 12px;
    flex: none;
    border: 2px solid var(--stroke-2);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .section {
    margin-top: var(--space-4);
  }
  .section-title {
    margin: 0 0 var(--space-2);
    font: var(--text-body-strong);
  }
  .section-title .caption {
    font-weight: 400;
    margin-left: 4px;
  }
  .sub {
    margin: var(--space-2) 0 var(--space-1);
    font-weight: 600;
  }
  .people {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .person {
    display: grid;
    grid-template-columns: 10px minmax(0, 1fr) auto;
    column-gap: var(--space-2);
    align-items: baseline;
    line-height: 20px;
  }
  .person-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .person-email {
    grid-column: 2;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .person-resp {
    grid-column: 3;
    grid-row: 1;
    white-space: nowrap;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    align-self: center;
    background: var(--fg-4);
  }
  .dot.ok {
    background: var(--success);
  }
  .dot.maybe {
    background: var(--warning);
  }
  .dot.no {
    background: var(--danger);
  }
  .dot.none {
    background: transparent;
    border: 1px solid var(--fg-4);
  }
  .body {
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    overflow: hidden;
    background: var(--bg-card);
  }
  .empty {
    margin: 0;
    padding: var(--space-3) var(--space-4);
  }
</style>
