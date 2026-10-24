<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, errorMessage, type CalendarEvent, type EventParticipant, type EventRequest, type FreeBusyChoice } from "../api";
  import { isValidEmail, mergeRecipients } from "../addresses";
  import { htmlToText } from "../html";
  import { focusBack } from "../popover";
  import { DEFAULT_EVENT_MIN, FREE_BUSY_OPTIONS, REMINDER_OPTIONS, addDays, dateInputValue, parseDateTimeInputs, startOfDay, timeInputValue, type EventDraft } from "../calendar";
  import RecipientsInput from "./RecipientsInput.svelte";
  import Editor from "./Editor.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";
  import ChoicePopover from "./ChoicePopover.svelte";

  /**
   * Редактор события: модально над сеткой. Новое событие (`draft.calendarPk` null) создаётся
   * `calendar_create`, существующее правится `calendar_update` — окно шлёт все поля, бэкенд
   * пишет на сервер только изменившиеся. При участниках приглашения и обновления рассылает сервер:
   * у новой встречи кнопка так и называется, при правке встречи с участниками — вопрос.
   */
  interface Props {
    draft: EventDraft;
    /** Событие сохранено: родитель перечитывает окно и выделяет его. */
    onsaved: (ev: CalendarEvent) => void;
    onclose: () => void;
  }
  let { draft, onsaved, onclose }: Props = $props();

  // Экземпляр мог быть уничтожен, пока шёл запрос: его итог тогда никому не нужен.
  let alive = true;
  onDestroy(() => (alive = false));

  const uid = $props.id();
  const isNew = $derived(draft.calendarPk == null);
  const title = $derived(isNew ? "Новое событие" : "Изменить событие");

  // --- состояние (из заготовки, дальше живёт своей жизнью) ----------------------------------

  let subject = $state(untrack(() => draft.subject));
  let startMs = $state(untrack(() => draft.startMs));
  let endMs = $state(untrack(() => draft.endMs));
  let isAllDay = $state(untrack(() => draft.isAllDay));
  let location = $state(untrack(() => draft.location));
  let body = $state(untrack(() => draft.bodyHtml));
  let required = $state<EventParticipant[]>(untrack(() => mergeRecipients([], draft.required)));
  let optional = $state<EventParticipant[]>(untrack(() => mergeRecipients([], draft.optional)));
  let showOptional = $state(untrack(() => draft.optional.length > 0));
  let reminder = $state<number | null>(untrack(() => draft.reminderMinutes));
  let freeBusy = $state<FreeBusyChoice>(untrack(() => draft.freeBusy));
  let isPrivate = $state(untrack(() => draft.isPrivate));

  let busy = $state(false);
  let error = $state<string | null>(null);
  let confirmCancel = $state(false);
  /** Открыт вопрос «Отправить обновление участникам?». */
  let askUpdates = $state(false);

  let dialogEl = $state<HTMLDivElement | null>(null);
  let saveBtn = $state<HTMLButtonElement | null>(null);
  let cancelBtn = $state<HTMLButtonElement | null>(null);
  let requiredField = $state<ReturnType<typeof RecipientsInput> | null>(null);
  let optionalField = $state<ReturnType<typeof RecipientsInput> | null>(null);

  const hasAttendees = $derived(required.length + optional.length > 0);
  const saveLabel = $derived(isNew && hasAttendees ? "Отправить приглашения" : "Сохранить");

  const keyOf = (rs: EventParticipant[]) => rs.map((r) => r.email.trim().toLowerCase()).join(",");
  /** Слепок полей, которые видят участники (тема, время, место, описание, состав): по нему решается, спрашивать ли о рассылке обновления. */
  function sharedSnapshot(): string {
    return JSON.stringify([subject, startMs, endMs, isAllDay, location, htmlToText(body), keyOf(required), keyOf(optional)]);
  }
  /** Слепок всех полей для «есть ли правки»: текст описания без разметки, адреса без регистра; «частное» при правке не меняется и не считается. */
  function snapshot(): string {
    return JSON.stringify([sharedSnapshot(), reminder, freeBusy, isNew ? isPrivate : null]);
  }
  const initial = untrack(snapshot);
  const sharedInitial = untrack(sharedSnapshot);
  const isDirty = $derived(snapshot() !== initial);
  const sharedDirty = $derived(sharedSnapshot() !== sharedInitial);
  /** Перед сохранением спросить про рассылку: правка встречи с участниками (я организатор), и изменилось что-то видное участникам. */
  const asksUpdates = $derived(!isNew && draft.isOrganizer && (hasAttendees || draft.hadAttendees) && sharedDirty);

  // --- дата и время ----------------------------------------------------------------------------
  // Источник правды — startMs/endMs; поля даты и времени выводятся из них и меняют их по change.
  // У «весь день» endMs — полночь после последнего дня, в поле — сам последний день.

  const startDate = $derived(dateInputValue(new Date(startMs)));
  const startTime = $derived(timeInputValue(new Date(startMs)));
  const endDate = $derived(dateInputValue(isAllDay ? addDays(new Date(endMs), -1) : new Date(endMs)));
  const endTime = $derived(timeInputValue(new Date(endMs)));

  /** Смена начала сдвигает конец, сохраняя длительность (у «весь день» — число дней). */
  function setStart(date: string, time: string) {
    const d = parseDateTimeInputs(date, isAllDay ? null : time);
    if (!d) return;
    if (isAllDay) {
      // Границы — местные полуночи: сдвиг в днях (с округлением) переживает перевод часов, миллисекунды — нет.
      const days = Math.round((endMs - startMs) / 86_400_000);
      startMs = d.getTime();
      endMs = addDays(d, days).getTime();
      return;
    }
    const duration = endMs - startMs;
    startMs = d.getTime();
    endMs = startMs + duration;
  }

  function setEnd(date: string, time: string) {
    if (isAllDay) {
      const d = parseDateTimeInputs(date, null);
      if (d) endMs = addDays(d, 1).getTime();
      return;
    }
    const d = parseDateTimeInputs(date, time);
    if (d) endMs = d.getTime();
  }

  /** Включение «весь день» растягивает событие на его дни; выключение ставит 09:00 первого дня на 30 минут. */
  function setAllDay(on: boolean) {
    if (on === isAllDay) return;
    const first = startOfDay(new Date(startMs));
    if (on) {
      const last = startOfDay(new Date(Math.max(endMs - 1, startMs)));
      startMs = first.getTime();
      endMs = addDays(last, 1).getTime();
    } else {
      startMs = first.getTime() + 9 * 3600_000;
      endMs = startMs + DEFAULT_EVENT_MIN * 60_000;
    }
    isAllDay = on;
  }

  // --- проверки и запрос -------------------------------------------------------------------

  function validate(): string | null {
    requiredField?.commit();
    optionalField?.commit();
    for (const r of [...required, ...optional]) if (!isValidEmail(r.email)) return `Неверный адрес: ${r.email}`;
    // Очищенное поле даты или времени браузер отдаёт пустым, а startMs/endMs хранят прежнее значение:
    // форма показывала бы одно, а сохраняла другое. Проверяются сами поля (у «весь день» полей времени нет).
    for (const el of dialogEl?.querySelectorAll<HTMLInputElement>('.when input:not([type="checkbox"])') ?? []) {
      if (!el.value) return `Заполните поле «${el.getAttribute("aria-label")}».`;
    }
    if (endMs <= startMs) return isAllDay ? "Последний день раньше первого." : "Конец раньше начала или совпадает с ним.";
    return null;
  }

  function clean(rs: EventParticipant[]): EventParticipant[] {
    return rs.map((r) => ({ name: r.name?.trim() || null, email: r.email.trim() }));
  }

  function request(): EventRequest {
    return {
      details_known: draft.detailsKnown,
      subject: subject.trim(),
      start_ms: startMs,
      end_ms: endMs,
      is_all_day: isAllDay,
      location: location.trim() || null,
      body_html: htmlToText(body) ? body : null,
      required: clean(required),
      optional: clean(optional),
      reminder_minutes: reminder,
      free_busy: freeBusy,
      is_private: isPrivate,
    };
  }

  /**
   * Сохранить. Правка без изменений просто закрывает редактор. При правке встречи с участниками
   * (я организатор) `sendUpdates` null означает «ещё не спрашивали»: если изменилось что-то видное
   * участникам, откроется вопрос, и сохранение продолжится с ответом; смена только напоминания или
   * занятости рассылки не требует.
   */
  async function save(sendUpdates: boolean | null = null) {
    if (busy) return;
    const problem = validate();
    if (problem) {
      error = problem;
      return;
    }
    if (!isNew && !isDirty) {
      onclose();
      return;
    }
    if (asksUpdates && sendUpdates == null) {
      askUpdates = true;
      return;
    }
    busy = true;
    error = null;
    try {
      const req = request();
      const ev = draft.calendarPk == null ? await api.calendarCreate(req) : await api.calendarUpdate(draft.calendarPk, req, sendUpdates ?? false);
      if (!alive) return;
      onsaved(ev);
    } catch (e) {
      if (alive) error = errorMessage(e);
    } finally {
      if (alive) busy = false;
    }
  }

  function pickUpdates(id: string) {
    askUpdates = false;
    focusBack(saveBtn);
    void save(id === "send");
  }
  function cancelUpdates() {
    askUpdates = false;
    focusBack(saveBtn);
  }

  function requestCancel() {
    if (busy) return;
    if (isDirty) confirmCancel = true;
    else onclose();
  }
  function confirmCancelNow() {
    confirmCancel = false;
    onclose();
  }
  function keepEditing() {
    confirmCancel = false;
    focusBack(cancelBtn);
  }

  // --- клавиши и фокус --------------------------------------------------------------------------

  const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), select:not(:disabled), [contenteditable="true"], [tabindex]:not([tabindex="-1"])';

  /** Диалог модальный: Tab и Shift+Tab ходят по кругу внутри него. */
  function trapTab(e: KeyboardEvent) {
    if (!dialogEl) return;
    const items = Array.from(dialogEl.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => el.offsetParent !== null);
    if (!items.length) {
      e.preventDefault();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    const inside = active instanceof Node && dialogEl.contains(active);
    if (e.shiftKey && (!inside || active === first)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (!inside || active === last)) {
      e.preventDefault();
      first.focus();
    }
  }

  function onKey(e: KeyboardEvent) {
    // У всплывающих окон внутри свой Esc.
    if (confirmCancel || askUpdates) return;
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      // Список подсказок адресатов закрывает себя сам и останавливает событие.
      e.preventDefault();
      requestCancel();
    } else if (e.key === "Tab") {
      trapTab(e);
    }
  }

  onMount(() => {
    document.getElementById(`${uid}-subject`)?.focus();
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="overlay">
  <div class="popup dialog" role="dialog" aria-modal="true" aria-labelledby="{uid}-title" aria-busy={busy} bind:this={dialogEl}>
    <h2 class="title" id="{uid}-title">{title}</h2>

    {#if draft.isOccurrence}
      <div class="hint caption">Правится только этот экземпляр повторяющейся встречи.</div>
    {/if}
    {#if !isNew && !draft.isOrganizer}
      <div class="hint caption">Встречу ведёт организатор: правки останутся в твоей копии.</div>
    {/if}

    {#if error}
      <div class="error" role="alert">
        <span class="error-text">{error}</span>
        <button class="btn small" onclick={() => (error = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
      </div>
    {/if}

    <div class="fields">
      <div class="row">
        <label class="lbl" for="{uid}-subject">Тема</label>
        <input id="{uid}-subject" class="field" type="text" bind:value={subject} disabled={busy} placeholder="Тема события" autocomplete="off" />
      </div>

      <div class="row">
        <span class="lbl">Начало</span>
        <div class="when">
          <input class="field date" type="date" value={startDate} disabled={busy} aria-label="Дата начала" onchange={(e) => setStart(e.currentTarget.value, startTime)} />
          {#if !isAllDay}
            <input class="field time" type="time" value={startTime} step="300" disabled={busy} aria-label="Время начала" onchange={(e) => setStart(startDate, e.currentTarget.value)} />
          {/if}
          <label class="check">
            <input type="checkbox" checked={isAllDay} disabled={busy} onchange={(e) => setAllDay(e.currentTarget.checked)} />
            Весь день
          </label>
        </div>
      </div>

      <div class="row">
        <span class="lbl">Конец</span>
        <div class="when">
          <input class="field date" type="date" value={endDate} min={startDate} disabled={busy} aria-label={isAllDay ? "Последний день" : "Дата конца"} onchange={(e) => setEnd(e.currentTarget.value, endTime)} />
          {#if !isAllDay}
            <input class="field time" type="time" value={endTime} step="300" disabled={busy} aria-label="Время конца" onchange={(e) => setEnd(endDate, e.currentTarget.value)} />
          {/if}
        </div>
      </div>

      <div class="row">
        <label class="lbl" for="{uid}-location">Место</label>
        <input id="{uid}-location" class="field" type="text" bind:value={location} disabled={busy} placeholder="Переговорная, адрес, ссылка" autocomplete="off" />
      </div>

      <div class="row">
        <label class="lbl" for="{uid}-required">Участники</label>
        <RecipientsInput bind:this={requiredField} id="{uid}-required" bind:recipients={required} disabled={busy} placeholder="Адрес или имя" />
        {#if !showOptional}
          <button class="link-btn" onclick={() => (showOptional = true)} disabled={busy}>Необязательные</button>
        {/if}
      </div>
      {#if showOptional}
        <div class="row">
          <label class="lbl" for="{uid}-optional">Необязательные</label>
          <RecipientsInput bind:this={optionalField} id="{uid}-optional" bind:recipients={optional} disabled={busy} />
        </div>
      {/if}

      <div class="row">
        <label class="lbl" for="{uid}-reminder">Напоминание</label>
        <div class="options">
          <select id="{uid}-reminder" class="field select" bind:value={reminder} disabled={busy}>
            {#each REMINDER_OPTIONS as o (String(o.value))}
              <option value={o.value}>{o.label}</option>
            {/each}
          </select>
          <label class="lbl inline" for="{uid}-freebusy">Занятость</label>
          <select id="{uid}-freebusy" class="field select" bind:value={freeBusy} disabled={busy}>
            {#each FREE_BUSY_OPTIONS as o (o.value)}
              <option value={o.value}>{o.label}</option>
            {/each}
          </select>
          <label class="check" title={isNew ? undefined : "Конфиденциальность задаётся при создании события и правкой не меняется"}>
            <input type="checkbox" bind:checked={isPrivate} disabled={busy || !isNew} />
            Частное
          </label>
        </div>
      </div>
    </div>

    <div class="editor-wrap">
      <Editor bind:value={body} disabled={busy} minHeight={120} label="Описание события" />
    </div>

    <div class="buttons">
      <button class="btn primary" bind:this={saveBtn} onclick={() => save()} disabled={busy} title="Сохранить (Ctrl+Enter)" aria-haspopup={asksUpdates ? "dialog" : undefined} aria-expanded={askUpdates}>
        {#if busy}<span class="spinner" aria-hidden="true"></span>{/if}
        {saveLabel}
      </button>
      <button class="btn" bind:this={cancelBtn} onclick={() => (confirmCancel ? keepEditing() : requestCancel())} disabled={busy} title="Отменить (Esc)" aria-haspopup="dialog" aria-expanded={confirmCancel}>Отменить</button>
      {#if busy}
        <span class="progress caption" aria-live="polite">{isNew ? "Создаю событие…" : "Сохраняю изменения…"}</span>
      {/if}
      <span class="spacer"></span>
      {#if isNew && hasAttendees}
        <span class="caption">Приглашения разошлёт сервер</span>
      {/if}
    </div>

    {#if askUpdates && saveBtn}
      <ChoicePopover anchor={saveBtn} align="start" title="Отправить обновление участникам?" text="Изменения уйдут всем приглашённым письмом." options={[{ id: "send", label: "Отправить обновление", primary: true }, { id: "nosend", label: "Сохранить без рассылки" }]} onpick={pickUpdates} oncancel={cancelUpdates} />
    {/if}
    {#if confirmCancel && cancelBtn}
      <ConfirmPopover anchor={cancelBtn} align="start" title={isNew ? "Отменить создание события?" : "Отменить правку?"} text="Набранное будет потеряно." confirmLabel="Отменить" onconfirm={confirmCancelNow} oncancel={keepEditing} />
    {/if}
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 90;
    background: rgba(0, 0, 0, 0.3);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .dialog {
    position: static;
    width: min(760px, calc(100vw - 48px));
    max-height: calc(100vh - 48px);
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
  }
  .hint {
    margin-top: calc(-1 * var(--space-2));
  }
  .error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    color: var(--danger);
  }
  .error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .row {
    display: grid;
    grid-template-columns: 110px minmax(0, 1fr) auto;
    align-items: start;
    gap: var(--space-2);
  }
  .lbl {
    font: var(--text-caption);
    color: var(--fg-3);
    line-height: var(--control-height);
  }
  .lbl.inline {
    margin-left: var(--space-2);
  }
  .field {
    height: var(--control-height);
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    outline: none;
    user-select: text;
    min-width: 0;
  }
  .field:focus {
    border-bottom: 2px solid var(--accent);
  }
  .field:disabled {
    color: var(--fg-disabled);
  }
  .field::placeholder {
    color: var(--fg-3);
  }
  .field.date {
    width: 160px;
  }
  .field.time {
    width: 110px;
  }
  .field.select {
    font: inherit;
    padding-right: 6px;
  }
  .when,
  .options {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    min-width: 0;
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
    margin-left: var(--space-2);
    white-space: nowrap;
  }
  .link-btn {
    height: var(--control-height);
    padding: 0 8px;
    color: var(--accent);
    border-radius: var(--radius-control);
    white-space: nowrap;
  }
  .link-btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .link-btn:disabled {
    color: var(--fg-disabled);
  }
  .editor-wrap {
    min-width: 0;
  }
  .buttons {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .spacer {
    flex: 1;
  }
  .progress {
    color: var(--fg-2);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
    padding: 0 14px;
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
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn.primary:active:not(:disabled) {
    background: var(--accent-pressed);
  }
  .btn.primary:disabled {
    opacity: 0.6;
    color: var(--fg-on-accent);
  }
  .btn.small {
    height: 24px;
    padding: 0 10px;
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .spinner {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: currentColor;
    animation: spin 0.9s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation-duration: 2.4s;
    }
  }
</style>
