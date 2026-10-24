<script lang="ts">
  import { onDestroy, onMount, tick, untrack } from "svelte";
  import { api, errorMessage, isDirectoryList, listDate, plural, type ComposeRecipient, type ContactCard, type DirectoryPerson } from "../api";
  import { ICON } from "../actions";
  import { CONTACT_ICON, KIND_LABEL, copyText, directoryLine, emailKey, expandSupported, initials, noteExpandError, rememberKinds, telHref, type ContactRef } from "../contact";
  import { anchorRect, dismissOn, placePopup } from "../popover";
  import { hint, layer } from "../hotkeys";

  /**
   * Карточка контакта у имени в письме или у чипа адресата: сведения справочника организации
   * (кэш, при отсутствии — сервер), переписка по локальной базе и действия «Написать письмо»,
   * «Найти письма», «Копировать адрес». У списка рассылки — его участники, с переходом к карточке
   * каждого и обратно. Esc и щелчок мимо закрывают её, фокус возвращает тот, кто открыл (App).
   */
  interface Props {
    who: ContactRef;
    anchor: HTMLElement;
    onclose: () => void;
    /** Новое письмо этому адресату. */
    oncompose: (to: ComposeRecipient) => void;
    /** Поиск писем от этого адреса. */
    onsearch: (email: string) => void;
    /**
     * Почему «Написать письмо» и «Найти письма» сейчас недоступны (карточка открыта из модального
     * окна — редактора встречи: уход в почту оставил бы его открытым за кадром); null — доступны.
     */
    actionsBlocked?: string | null;
  }
  let { who, anchor, onclose, oncompose, onsearch, actionsBlocked = null }: Props = $props();

  const uid = $props.id();
  let el = $state<HTMLDivElement | null>(null);
  let pos = $state({ left: -9999, top: -9999 });

  /** Кого показываем сейчас: сначала того, кого открыли, дальше — участника списка. */
  let current = $state<ContactRef>(untrack(() => ({ name: who.name, email: who.email })));
  /** Откуда пришли к участнику (кнопка «назад»). */
  let trail = $state<ContactRef[]>([]);

  let card = $state<ContactCard | null>(null);
  let loading = $state(false);
  /** Отказ самой команды (сведения не получены совсем); отказ сервера при кэше — в card.error. */
  let failure = $state<string | null>(null);
  let status = $state<string | null>(null);
  let statusTimer: ReturnType<typeof setTimeout> | undefined;

  let members = $state<DirectoryPerson[] | null>(null);
  let membersLoading = $state(false);
  let membersError = $state<string | null>(null);

  let request = 0;

  const person = $derived(card?.person ?? null);
  const email = $derived((person?.email ?? "").trim() || current.email);
  const name = $derived(person?.name?.trim() || current.name?.trim() || email);
  const kindLabel = $derived(person ? (KIND_LABEL[person.kind] ?? null) : null);
  const isList = $derived(isDirectoryList(person));
  const subtitle = $derived(directoryLine(person));
  const local = $derived(card?.local ?? null);
  const facts = $derived(
    [
      { label: "Компания", value: person?.company?.trim() },
      { label: "Офис", value: person?.office?.trim() },
    ].filter((f): f is { label: string; value: string } => !!f.value),
  );
  const phones = $derived(
    [
      { label: "Рабочий", value: person?.business_phone?.trim() },
      { label: "Мобильный", value: person?.mobile_phone?.trim() },
    ].filter((f): f is { label: string; value: string } => !!f.value),
  );

  async function load(refresh: boolean) {
    const n = ++request;
    loading = true;
    failure = null;
    try {
      const c = await api.contactCard(current.email, refresh);
      if (n !== request) return;
      card = c;
      if (c.person) rememberKinds([c.person]);
    } catch (e) {
      if (n === request) failure = errorMessage(e);
    } finally {
      if (n === request) loading = false;
    }
  }

  async function loadMembers() {
    if (membersLoading) return;
    membersLoading = true;
    membersError = null;
    const forEmail = emailKey(email);
    try {
      const got = await api.directoryExpand(email);
      if (emailKey(email) !== forEmail) return;
      rememberKinds(got);
      members = got.filter((m) => (m.email ?? "").trim() || (m.name ?? "").trim());
      // Кнопка «Показать участников» исчезла вместе с фокусом: он переходит к первому участнику.
      await tick();
      const a = document.activeElement;
      if (el && (!a || a === document.body)) (el.querySelector<HTMLElement>("button.member:not(:disabled)") ?? el).focus();
    } catch (e) {
      noteExpandError(errorMessage(e));
      if (emailKey(email) === forEmail) membersError = errorMessage(e);
    } finally {
      membersLoading = false;
    }
  }

  /** Перейти к карточке участника списка (назад — кнопкой со стрелкой). */
  function openMember(m: DirectoryPerson) {
    const addr = (m.email ?? "").trim();
    if (!addr) return;
    trail = [...trail, { name, email }];
    show({ name: m.name?.trim() || null, email: addr });
  }

  function goBack() {
    const prev = trail[trail.length - 1];
    if (!prev) return;
    trail = trail.slice(0, -1);
    show(prev);
  }

  function show(ref: ContactRef) {
    current = ref;
    card = null;
    members = null;
    membersError = null;
    status = null;
    void load(false);
    el?.focus();
  }

  function flash(text: string) {
    status = text;
    clearTimeout(statusTimer);
    statusTimer = setTimeout(() => (status = null), 3000);
  }

  async function copyAddress() {
    flash((await copyText(email)) ? "Адрес скопирован" : "Не удалось скопировать адрес");
  }

  /**
   * Звонок по ссылке tel: через системное приложение; если оно не открылось (или окно такие ссылки
   * не открывает), номер копируется — его можно вставить в софтфон.
   */
  async function call(e: MouseEvent, phone: string) {
    e.preventDefault();
    try {
      await api.openExternal(telHref(phone));
    } catch {
      flash((await copyText(phone)) ? "Номер скопирован" : "Не удалось открыть номер");
    }
  }

  function compose() {
    oncompose({ name: name !== email ? name : null, email });
  }

  const FOCUSABLE = 'button:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])';

  /**
   * Карточка модальная: Tab и Shift+Tab ходят по кругу по её элементам и дальше не идут — иначе
   * ловушка фокуса окна под ней (редактор встречи) увела бы фокус к себе и закрыла бы карточку.
   */
  function trapTab(e: KeyboardEvent) {
    e.preventDefault();
    e.stopPropagation();
    if (!el) return;
    const items = Array.from(el.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((n) => n.getClientRects().length > 0);
    if (!items.length) return;
    const i = items.indexOf(document.activeElement as HTMLElement);
    const next = i < 0 ? (e.shiftKey ? items.length - 1 : 0) : (i + (e.shiftKey ? -1 : 1) + items.length) % items.length;
    items[next].focus();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
    } else if (e.key === "Tab" && !e.ctrlKey && !e.altKey && !e.metaKey) {
      trapTab(e);
    } else if (e.key === "Backspace" && trail.length && !(e.target instanceof HTMLInputElement)) {
      e.preventDefault();
      goBack();
    }
  }

  /** Последнее письмо: сегодня — «сегодня в 10:15», иначе дата как в списке. */
  function lastText(ms: number): string {
    const d = new Date(ms);
    return d.toDateString() === new Date().toDateString() ? `сегодня в ${listDate(ms)}` : listDate(ms);
  }

  function lettersText(n: number): string {
    return `${n.toLocaleString("ru-RU")} ${plural(n, "письмо", "письма", "писем")}`;
  }

  onMount(() => {
    void load(false);
  });

  // Положение у якоря; карточка растёт по мере загрузки — пересчитывается по размеру.
  $effect(() => {
    const node = el;
    if (!node) return;
    const place = () => (pos = placePopup(anchorRect(anchor), node.offsetWidth, node.offsetHeight, "start"));
    place();
    node.focus();
    const ro = new ResizeObserver(place);
    ro.observe(node);
    const off = dismissOn(node, onclose, anchor);
    return () => {
      ro.disconnect();
      off();
    };
  });

  onDestroy(() => clearTimeout(statusTimer));
</script>

<div class="popup contact" role="dialog" aria-modal="true" aria-labelledby="{uid}-name" aria-describedby={subtitle ? `${uid}-sub` : undefined} aria-busy={loading} tabindex="-1" bind:this={el} {@attach layer} style="left: {pos.left}px; top: {pos.top}px" onkeydown={onKey}>
  <div class="head">
    {#if trail.length}
      <button class="icon-btn" onclick={goBack} aria-label="Назад к «{trail[trail.length - 1].name || trail[trail.length - 1].email}»" title="Назад (Backspace)">
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.back} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
    {/if}
    <div class="avatar" class:list={isList} aria-hidden="true">
      {#if isList}
        <svg width="20" height="20" viewBox="0 0 16 16"><path d={CONTACT_ICON.people} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      {:else}
        {initials(name, email)}
      {/if}
    </div>
    <div class="who">
      <h2 class="name" id="{uid}-name">{name}</h2>
      {#if subtitle}<div class="sub" id="{uid}-sub">{subtitle}</div>{/if}
      {#if kindLabel}<span class="badge">{kindLabel}</span>{/if}
    </div>
    <div class="head-actions">
      <button class="icon-btn" onclick={() => void load(true)} disabled={loading} aria-label="Обновить из справочника" title="Обновить из справочника">
        {#if loading}
          <span class="spinner" aria-hidden="true"></span>
        {:else}
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.refresh} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {/if}
      </button>
      <button class="icon-btn" onclick={onclose} aria-label="Закрыть" title={hint("Закрыть", "contact-close")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.close} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
      </button>
    </div>
  </div>

  <dl class="facts">
    <div class="fact">
      <dt>Почта</dt>
      <dd class="addr">
        <span class="addr-text">{email}</span>
        <button class="icon-btn small" onclick={copyAddress} aria-label="Копировать адрес" title="Копировать адрес">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.copy} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
      </dd>
    </div>
    {#each phones as p (p.label)}
      <div class="fact">
        <dt>{p.label}</dt>
        <dd><a class="link" href={telHref(p.value)} onclick={(e) => void call(e, p.value)} title="Позвонить">{p.value}</a></dd>
      </div>
    {/each}
    {#each facts as f (f.label)}
      <div class="fact">
        <dt>{f.label}</dt>
        <dd>{f.value}</dd>
      </div>
    {/each}
  </dl>

  {#if loading && !card}
    <div class="msg caption" role="status"><span class="spinner" aria-hidden="true"></span> Загружаю сведения из справочника…</div>
  {:else if failure}
    <div class="msg caption error" role="alert">{failure}</div>
  {:else if card}
    {#if card.error}
      <div class="msg caption warn" role="status">{card.person && card.from_cache ? `Показано из кэша: ${card.error}` : card.error}</div>
    {:else if !card.person}
      <div class="msg caption">В справочнике организации не найден</div>
    {/if}
  {/if}

  {#if local}
    <div class="section caption">
      {#if local.messages_from || local.messages_to}
        Переписка: от него — {lettersText(local.messages_from)}, ему — {lettersText(local.messages_to)}{#if local.last_ms != null}; последнее — {lastText(local.last_ms)}{/if}
      {:else}
        В почте на этом компьютере писем с этим адресом нет
      {/if}
    </div>
  {/if}

  {#if isList && (members || expandSupported())}
    <div class="section members">
      {#if members}
        <div class="members-head caption">Участники: {members.length.toLocaleString("ru-RU")}</div>
        {#if members.length}
          <ul class="member-list">
            {#each members as m, i (`${emailKey(m.email)}:${i}`)}
              {@const line = directoryLine(m)}
              <li>
                <button class="member" onclick={() => openMember(m)} disabled={!(m.email ?? "").trim()} title={m.email ?? ""}>
                  <span class="member-name">{m.name?.trim() || m.email}</span>
                  {#if line}<span class="member-line caption">{line}</span>{/if}
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      {:else}
        <button class="btn small" onclick={loadMembers} disabled={membersLoading}>
          {#if membersLoading}<span class="spinner" aria-hidden="true"></span>{/if}
          Показать участников
        </button>
      {/if}
      {#if membersError}<div class="msg caption error" role="alert">{membersError}</div>{/if}
    </div>
  {/if}

  <div class="actions">
    <button class="btn primary" onclick={compose} disabled={actionsBlocked !== null} title={actionsBlocked ?? undefined} aria-describedby={actionsBlocked ? `${uid}-blocked` : undefined}>
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.compose} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Написать письмо
    </button>
    <button class="btn" onclick={() => onsearch(email)} disabled={actionsBlocked !== null} title={actionsBlocked ?? "Письма от этого адреса в почте на компьютере"} aria-describedby={actionsBlocked ? `${uid}-blocked` : undefined}>
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.search} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Найти письма
    </button>
    <button class="btn" onclick={copyAddress}>Копировать адрес</button>
  </div>
  {#if actionsBlocked}<div class="status caption" id="{uid}-blocked">{actionsBlocked}</div>{/if}
  {#if status}<div class="status caption" role="status">{status}</div>{/if}
</div>

<style>
  .contact {
    width: 360px;
    max-width: calc(100vw - 16px);
    max-height: calc(100vh - 16px);
    overflow-y: auto;
    padding: var(--space-3) var(--space-4) var(--space-3);
    outline: none;
    user-select: text;
  }
  .head {
    display: flex;
    align-items: flex-start;
    gap: var(--space-3);
  }
  .avatar {
    flex: none;
    width: 48px;
    height: 48px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--accent-soft);
    color: var(--accent);
    font: var(--text-body-large);
    font-weight: 600;
  }
  .who {
    flex: 1;
    min-width: 0;
  }
  .name {
    margin: 0;
    font: var(--text-body-strong);
    font-size: 16px;
    line-height: 22px;
    overflow-wrap: anywhere;
  }
  .sub {
    margin-top: 2px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .badge {
    display: inline-block;
    margin-top: var(--space-1);
    font: var(--text-caption);
    line-height: 16px;
    padding: 0 6px;
    border: 1px solid var(--stroke-2);
    border-radius: 9px;
    color: var(--fg-3);
  }
  .head-actions {
    flex: none;
    display: flex;
    gap: 2px;
    margin: -4px -8px 0 0;
  }
  .icon-btn {
    width: 28px;
    height: 28px;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .icon-btn.small {
    width: 24px;
    height: 24px;
  }
  .icon-btn:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .icon-btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .facts {
    margin: var(--space-3) 0 0;
    display: grid;
    gap: 2px;
  }
  .fact {
    display: grid;
    grid-template-columns: 84px minmax(0, 1fr);
    gap: var(--space-2);
    align-items: center;
    min-height: 24px;
  }
  dt {
    font: var(--text-caption);
    color: var(--fg-3);
  }
  dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .addr {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }
  .addr-text {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .link {
    color: var(--accent);
    text-decoration: none;
    border-radius: 2px;
  }
  .link:hover {
    text-decoration: underline;
  }
  .msg {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-2);
    overflow-wrap: anywhere;
  }
  .msg.error {
    color: var(--danger);
  }
  .msg.warn {
    color: var(--warning);
  }
  .section {
    margin-top: var(--space-3);
    padding-top: var(--space-2);
    border-top: 1px solid var(--stroke-1);
  }
  .members-head {
    margin-bottom: var(--space-1);
  }
  .member-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 220px;
    overflow-y: auto;
  }
  .member {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    width: 100%;
    padding: 4px var(--space-2);
    border-radius: var(--radius-control);
    text-align: left;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .member:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .member:disabled {
    color: var(--fg-3);
  }
  .member-name,
  .member-line {
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-top: var(--space-3);
    padding-top: var(--space-3);
    border-top: 1px solid var(--stroke-1);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 10px;
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
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover {
    background: var(--accent-hover);
  }
  .btn.primary:active {
    background: var(--accent-pressed);
  }
  .status {
    margin-top: var(--space-2);
  }
  .spinner {
    flex: none;
    display: inline-block;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: var(--accent);
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
