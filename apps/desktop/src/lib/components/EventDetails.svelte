<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api, errorMessage, personLabel, type Attendee, type CalendarEvent, type CalendarEventDetails, type MeetingResponse } from "../api";
  import { FREE_BUSY_LABEL, MY_RESPONSE_LABEL, RESPONSE_LABEL, eventWhen, reminderLabel, sensitivityLabel } from "../calendar";
  import { focusBack } from "../popover";
  import { hint } from "../hotkeys";
  import BodyFrame from "./BodyFrame.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";
  import ChoicePopover from "./ChoicePopover.svelte";

  interface Props {
    /** Событие из сетки; подробности панель грузит сама. */
    event: CalendarEvent;
    onclose: () => void;
    /** «Изменить»: открыть редактор с событием и его подробностями (кнопка доступна только с ними). */
    onedit: (ev: CalendarEvent, details: CalendarEventDetails | null) => void;
    /** Событие удалено или на приглашение дан ответ: родитель перечитывает окно календаря. */
    onchanged: (what: "deleted" | "responded") => void;
  }
  let { event, onclose, onedit, onchanged }: Props = $props();

  let alive = true;
  onDestroy(() => (alive = false));

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
  const reminder = $derived(reminderLabel(details?.reminder_minutes ?? ev.reminder_minutes ?? null));
  const recurring = $derived(ev.calendar_type === "occurrence" || ev.calendar_type === "exception" || ev.calendar_type === "recurring_master");
  /** Подробности этого события загружены: только тогда редактор открывается — иначе он не знал бы участников и описания. */
  const detailsReady = $derived(details != null && details.event.calendar_pk === event.calendar_pk);

  // --- действия: изменить, удалить (отменить встречу), ответить на приглашение -----------------
  // Команды идут отсюда (панель показывает занятость и ошибку у себя), а родитель по onchanged
  // перечитывает окно календаря: после удаления событие уйдёт из сетки, после ответа сменится значок.

  /** Я организатор встречи: удаление — это отмена, и участников можно уведомить. */
  const isOrganizerMeeting = $derived(ev.is_meeting && ev.my_response === "organizer");
  /** Приглашённый на живую встречу: можно принять, ответить «под вопросом» или отклонить. */
  const canRespond = $derived(ev.is_meeting && ev.my_response !== "organizer" && !ev.is_cancelled);

  const RESPONSES: { id: MeetingResponse; label: string; question: string }[] = [
    { id: "accept", label: "Принять", question: "Принять приглашение?" },
    { id: "tentative", label: "Под вопросом", question: "Ответить «под вопросом»?" },
    { id: "decline", label: "Отклонить", question: "Отклонить приглашение?" },
  ];

  let busy = $state(false);
  let actionError = $state<string | null>(null);
  let deleteBtn = $state<HTMLButtonElement | null>(null);
  /** Открыт вопрос об удалении: у организатора — с выбором про уведомление участников. */
  let confirmDelete = $state(false);
  /** Открыт вопрос «отправить ответ?» для выбранного ответа; якорь — нажатая кнопка. */
  let pendingResponse = $state<{ response: MeetingResponse; anchor: HTMLButtonElement } | null>(null);

  function requestDelete() {
    if (busy) return;
    confirmDelete = true;
  }
  function closeDelete() {
    confirmDelete = false;
    focusBack(deleteBtn);
  }

  async function deleteNow(sendCancellations: boolean) {
    closeDelete();
    if (busy) return;
    busy = true;
    actionError = null;
    try {
      await api.calendarDelete(event.calendar_pk, sendCancellations);
      if (!alive) return;
      onchanged("deleted");
    } catch (e) {
      if (alive) actionError = errorMessage(e);
    } finally {
      if (alive) busy = false;
    }
  }

  function askRespond(e: MouseEvent, response: MeetingResponse) {
    if (busy) return;
    if (pendingResponse?.response === response) {
      closeRespond();
      return;
    }
    pendingResponse = { response, anchor: e.currentTarget as HTMLButtonElement };
  }
  function closeRespond() {
    const anchor = pendingResponse?.anchor;
    pendingResponse = null;
    focusBack(anchor);
  }

  async function respondNow(sendResponse: boolean) {
    const p = pendingResponse;
    closeRespond();
    if (!p || busy) return;
    busy = true;
    actionError = null;
    try {
      await api.calendarRespond(event.calendar_pk, p.response, sendResponse);
      if (!alive) return;
      // После отказа сервер убирает встречу из календаря: перечитывать её нечего, сетку обновит родитель.
      if (p.response !== "decline") void load(event.calendar_pk, true);
      onchanged("responded");
    } catch (e) {
      if (alive) actionError = errorMessage(e);
    } finally {
      if (alive) busy = false;
    }
  }

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
      <button class="icon-btn" onclick={onclose} aria-label="Закрыть" title={hint("Закрыть", "cal-deselect")}>
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
    <div class="actions" role="toolbar" aria-label="Действия с событием" aria-busy={busy}>
      {#if !ev.is_cancelled}
        <button class="btn" onclick={() => detailsReady && onedit(ev, details)} disabled={busy || !detailsReady} title={detailsReady ? undefined : "Сначала загружаются подробности события"}>Изменить</button>
      {/if}
      <button class="btn danger" bind:this={deleteBtn} onclick={() => (confirmDelete ? closeDelete() : requestDelete())} disabled={busy} aria-haspopup="dialog" aria-expanded={confirmDelete}>
        {isOrganizerMeeting && !ev.is_cancelled ? "Отменить встречу" : "Удалить"}
      </button>
      {#if canRespond}
        <span class="act-sep" aria-hidden="true"></span>
        {#each RESPONSES as r (r.id)}
          <button class="btn resp {r.id}" class:on={ev.my_response === r.id} onclick={(e) => askRespond(e, r.id)} disabled={busy} aria-pressed={ev.my_response === r.id} aria-haspopup="dialog" aria-expanded={pendingResponse?.response === r.id} title={ev.my_response === r.id ? `Текущий ответ: ${r.label.toLowerCase()}` : r.question}>{r.label}</button>
        {/each}
      {/if}
    </div>
    {#if actionError}
      <div class="action-error" role="alert">
        <span class="note-text">{actionError}</span>
        <button class="btn" onclick={() => (actionError = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
      </div>
    {/if}
    {#if confirmDelete && deleteBtn}
      {#if isOrganizerMeeting && !ev.is_cancelled}
        <ChoicePopover anchor={deleteBtn} align="start" title="Отменить встречу?" text="Встреча уйдёт в «Удалённые». С отменой участники получат письмо и она исчезнет из их календарей." options={[{ id: "send", label: "Отправить отмену участникам", danger: true }, { id: "nosend", label: "Удалить без уведомления" }]} onpick={(id) => deleteNow(id === "send")} oncancel={closeDelete} />
      {:else}
        <ConfirmPopover anchor={deleteBtn} align="start" title="Удалить событие?" text="Событие уйдёт в «Удалённые» на сервере." confirmLabel="Удалить" onconfirm={() => deleteNow(false)} oncancel={closeDelete} />
      {/if}
    {/if}
    {#if pendingResponse}
      <ChoicePopover anchor={pendingResponse.anchor} align="start" title={RESPONSES.find((r) => r.id === pendingResponse?.response)?.question ?? ""} text="Организатор получит письмо с ответом, если его отправить." options={[{ id: "send", label: "Отправить ответ", primary: true }, { id: "nosend", label: "Не отправлять" }]} onpick={(id) => respondNow(id === "send")} oncancel={closeRespond} />
    {/if}
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
  .actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
    margin-top: var(--space-2);
  }
  .actions .btn {
    height: 28px;
    font: var(--text-body);
    color: var(--fg-1);
  }
  .actions .btn.danger:hover:not(:disabled) {
    color: var(--danger);
  }
  .actions .btn:disabled {
    color: var(--fg-disabled);
  }
  .act-sep {
    width: 1px;
    height: 16px;
    margin: 0 4px;
    background: var(--stroke-2);
  }
  .btn.resp.on {
    background: var(--accent-soft);
    border-color: var(--accent);
    color: var(--accent);
  }
  .btn.resp.decline.on {
    background: transparent;
    border-color: var(--danger);
    color: var(--danger);
  }
  .btn.resp.tentative.on {
    background: transparent;
    border-color: var(--warning);
    color: var(--warning);
  }
  .action-error {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-2);
    color: var(--danger);
    font: var(--text-caption);
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
