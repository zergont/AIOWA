<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import { api, errorMessage, isDirectoryList, listDate, onContactsChanged, plural, type ComposeRecipient, type DirectoryPerson, type PersonalContact } from "../api";
  import { ICON } from "../actions";
  import { CONTACT_ICON, addressLabel, contactCardOpener, copyText, directoryLine, emailKey, emailLabel, phoneLabel, rememberKinds, telHref } from "../contact";
  import { primePhotos } from "../photos.svelte";
  import Avatar from "./Avatar.svelte";

  /**
   * Раздел «Люди» (docs/obkatka.md п. 8): личные контакты ящика из локальной базы — список с
   * поиском и страница контакта (адреса, телефоны, компания, почтовые адреса, заметки, участники
   * группы) — и вкладка справочника организации с тем же поиском. Контакты приходят синхронизацией
   * вместе с почтой; событие `contacts-changed` перечитывает список. Карточка справочника у
   * человека — та же, что в письме (через контекст App).
   */
  interface Props {
    /** Раздел на экране: спрятанный не обновляется и не ловит клавиши. */
    visible?: boolean;
    /** Новое письмо адресатам (у группы — всем участникам с адресами). */
    oncompose: (to: ComposeRecipient[]) => void;
    /** Письма от этого адреса — поиск в почте. */
    onsearch: (email: string) => void;
  }
  let { visible = true, oncompose, onsearch }: Props = $props();

  /** Карточка справочника — одна на окно (App); берётся при создании компонента. */
  const openCard = contactCardOpener();

  type Tab = "contacts" | "directory";
  let tab = $state<Tab>("contacts");
  let query = $state("");
  let contacts = $state<PersonalContact[]>([]);
  let loaded = $state(false);
  let loading = $state(false);
  let failure = $state<string | null>(null);
  let selectedPk = $state<number | null>(null);
  /** Папки контактов и общее число (строка под поиском). */
  let total = $state(0);
  let folderNames = $state<string[]>([]);
  let synced = $state(false);

  let dirPeople = $state<DirectoryPerson[]>([]);
  let dirLoading = $state(false);
  let dirError = $state<string | null>(null);
  let dirRequest = 0;
  let dirTimer: ReturnType<typeof setTimeout> | undefined;

  let status = $state<string | null>(null);
  let statusTimer: ReturnType<typeof setTimeout> | undefined;
  let searchEl = $state<HTMLInputElement | null>(null);

  const selected = $derived(contacts.find((c) => c.contact_pk === selectedPk) ?? null);
  const people = $derived(contacts.filter((c) => !c.is_group));
  const groups = $derived(contacts.filter((c) => c.is_group));

  let request = 0;
  async function load() {
    const n = ++request;
    loading = true;
    failure = null;
    try {
      const [list, st] = await Promise.all([api.contactsList(query.trim()), api.contactsStatus()]);
      if (n !== request) return;
      contacts = list;
      total = st.total;
      folderNames = st.folders.map((f) => f.name);
      synced = st.folders.some((f) => f.last_synced_ms != null);
      loaded = true;
      if (selectedPk != null && !list.some((c) => c.contact_pk === selectedPk)) selectedPk = null;
      void primePhotos(list.map((c) => c.email));
    } catch (e) {
      if (n === request) failure = errorMessage(e);
    } finally {
      if (n === request) loading = false;
    }
  }

  let queryTimer: ReturnType<typeof setTimeout> | undefined;
  function onQueryInput() {
    clearTimeout(queryTimer);
    clearTimeout(dirTimer);
    if (tab === "contacts") {
      queryTimer = setTimeout(() => void load(), 120);
    } else {
      dirTimer = setTimeout(() => void searchDirectory(), 300);
    }
  }

  async function searchDirectory() {
    const q = query.trim();
    const n = ++dirRequest;
    if (q.length < 2) {
      dirPeople = [];
      dirError = null;
      dirLoading = false;
      return;
    }
    dirLoading = true;
    dirError = null;
    try {
      // Сначала кэш справочника (мгновенно), затем сервер.
      const cached = await api.directorySearch(q, 25, true);
      if (n !== dirRequest) return;
      if (cached.people.length) {
        dirPeople = cached.people;
        rememberKinds(cached.people);
        void primePhotos(cached.people.map((p) => p.email));
      }
      const got = await api.directorySearch(q, 25, false);
      if (n !== dirRequest) return;
      dirPeople = got.people;
      dirError = got.error ?? null;
      rememberKinds(got.people);
      void primePhotos(got.people.map((p) => p.email));
    } catch (e) {
      if (n === dirRequest) dirError = errorMessage(e);
    } finally {
      if (n === dirRequest) dirLoading = false;
    }
  }

  function switchTab(t: Tab) {
    if (tab === t) return;
    tab = t;
    clearTimeout(queryTimer);
    clearTimeout(dirTimer);
    if (t === "contacts") void load();
    else void searchDirectory();
    void tick().then(() => searchEl?.focus());
  }

  function flash(text: string) {
    status = text;
    clearTimeout(statusTimer);
    statusTimer = setTimeout(() => (status = null), 3000);
  }

  async function copy(text: string, what: string) {
    flash((await copyText(text)) ? `${what} скопирован` : `Не удалось скопировать ${what.toLowerCase()}`);
  }

  async function call(e: MouseEvent, phone: string) {
    e.preventDefault();
    try {
      await api.openExternal(telHref(phone));
    } catch {
      flash((await copyText(phone)) ? "Номер скопирован" : "Не удалось открыть номер");
    }
  }

  function writeTo(c: PersonalContact, email?: string) {
    const addr = (email ?? c.email ?? "").trim();
    if (!addr) return;
    oncompose([{ name: c.name !== addr ? c.name : null, email: addr }]);
  }

  /** Письмо группе: состав раскрывается в адреса здесь, на клиенте (участники без адреса пропускаются). */
  function writeToGroup(c: PersonalContact) {
    const seen = new Set<string>();
    const members: ComposeRecipient[] = [];
    for (const m of c.item.members) {
      const email = (m.email ?? "").trim();
      if (!email.includes("@") || seen.has(email.toLowerCase())) continue;
      seen.add(email.toLowerCase());
      members.push({ name: m.name?.trim() || null, email });
    }
    if (!members.length) {
      flash("У группы нет участников с адресами");
      return;
    }
    oncompose(members);
  }

  function openDirectoryCard(p: { name?: string | null; email?: string | null }, anchor: HTMLElement) {
    const email = (p.email ?? "").trim();
    if (!email) return;
    openCard?.({ name: p.name?.trim() || null, email }, anchor);
  }

  function addressText(a: { street?: string | null; postal_code?: string | null; city?: string | null; state?: string | null; country?: string | null }): string {
    return [a.street, [a.postal_code, a.city].filter(Boolean).join(" "), a.state, a.country].filter((s) => s && s.trim()).join(", ");
  }
  function birthdayText(ms: number): string {
    return new Date(ms).toLocaleDateString("ru-RU", { day: "numeric", month: "long", year: "numeric" });
  }
  function orgLine(c: PersonalContact): string {
    return [c.item.job_title, c.item.department, c.item.company].filter((s) => s && s.trim()).join(" · ");
  }

  function onListKey(e: KeyboardEvent) {
    if (!contacts.length) return;
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const i = contacts.findIndex((c) => c.contact_pk === selectedPk);
    const next = e.key === "ArrowDown" ? Math.min(contacts.length - 1, i + 1) : Math.max(0, i - 1);
    selectedPk = contacts[next]?.contact_pk ?? null;
  }

  // Фото участников группы — из кэша справочника (без сети), когда открыта группа.
  $effect(() => {
    const members = selected?.item.members;
    if (members?.length) void primePhotos(members.map((m) => m.email));
  });

  // Первый показ — загрузка; дальше список обновляет событие синхронизации.
  let shown = false;
  $effect(() => {
    if (visible && !shown) {
      shown = true;
      untrack(() => void load());
    }
  });

  onMount(() => {
    let off: (() => void) | undefined;
    void onContactsChanged(() => {
      if (tab === "contacts") void load();
    }).then((u) => (off = u));
    return () => {
      off?.();
      clearTimeout(queryTimer);
      clearTimeout(dirTimer);
      clearTimeout(statusTimer);
    };
  });
</script>

<div class="people">
  <div class="side">
    <div class="segmented tabs" role="tablist" aria-label="Люди">
      <button role="tab" aria-selected={tab === "contacts"} class:on={tab === "contacts"} onclick={() => switchTab("contacts")}>Мои контакты</button>
      <button role="tab" aria-selected={tab === "directory"} class:on={tab === "directory"} onclick={() => switchTab("directory")}>Справочник</button>
    </div>
    <div class="search" class:active={query.length > 0}>
      <svg class="search-icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M10.5 10.5 14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      <input type="search" placeholder={tab === "contacts" ? "Имя, адрес, компания, телефон" : "Имя или адрес в справочнике"} bind:value={query} bind:this={searchEl} oninput={onQueryInput} onkeydown={onListKey} autocomplete="off" spellcheck="false" aria-label={tab === "contacts" ? "Поиск по моим контактам" : "Поиск по справочнику"} />
    </div>

    {#if tab === "contacts"}
      <div class="summary caption" role="status">
        {#if failure}
          <span class="error">{failure}</span>
        {:else if !loaded}
          Загружаю контакты…
        {:else if !folderNames.length}
          Папок контактов в ящике не найдено
        {:else if !synced}
          Контакты ещё не загружены с сервера: ждут синхронизации
        {:else if query.trim()}
          Найдено: {contacts.length.toLocaleString("ru-RU")} из {total.toLocaleString("ru-RU")}
        {:else}
          {total.toLocaleString("ru-RU")} {plural(total, "запись", "записи", "записей")}{folderNames.length > 1 ? ` в ${folderNames.length} папках` : ""}
        {/if}
      </div>
      <div class="list" role="listbox" aria-label="Мои контакты" tabindex="-1">
        {#if loaded && !contacts.length && !failure}
          <div class="empty caption">{query.trim() ? "Ничего не найдено" : synced ? "Личных контактов нет" : ""}</div>
        {/if}
        {#if people.length}
          {#if groups.length}<div class="sect caption">Контакты</div>{/if}
          {#each people as c (c.contact_pk)}
            <button type="button" class="row" class:on={c.contact_pk === selectedPk} role="option" aria-selected={c.contact_pk === selectedPk} onclick={() => (selectedPk = c.contact_pk)}>
              <Avatar name={c.name} email={c.email} size={32} />
              <span class="who">
                <span class="name">{c.name}</span>
                {#if c.email || orgLine(c)}<span class="line caption">{c.email || orgLine(c)}</span>{/if}
              </span>
            </button>
          {/each}
        {/if}
        {#if groups.length}
          <div class="sect caption">Группы</div>
          {#each groups as c (c.contact_pk)}
            <button type="button" class="row" class:on={c.contact_pk === selectedPk} role="option" aria-selected={c.contact_pk === selectedPk} onclick={() => (selectedPk = c.contact_pk)}>
              <span class="group-avatar" aria-hidden="true">
                <svg width="16" height="16" viewBox="0 0 16 16"><path d={CONTACT_ICON.people} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
              </span>
              <span class="who">
                <span class="name">{c.name}</span>
                <span class="line caption">{c.item.members.length.toLocaleString("ru-RU")} {plural(c.item.members.length, "участник", "участника", "участников")}</span>
              </span>
            </button>
          {/each}
        {/if}
      </div>
    {:else}
      <div class="summary caption" role="status">
        {#if dirError}
          <span class="error">{dirError}</span>
        {:else if query.trim().length < 2}
          Введите хотя бы две буквы имени или адреса
        {:else if dirLoading && !dirPeople.length}
          Ищу в справочнике…
        {:else}
          Найдено: {dirPeople.length.toLocaleString("ru-RU")}{#if dirLoading} · уточняю на сервере…{/if}
        {/if}
      </div>
      <div class="list" aria-label="Справочник организации">
        {#if !dirLoading && !dirPeople.length && query.trim().length >= 2 && !dirError}
          <div class="empty caption">Ничего не найдено</div>
        {/if}
        {#each dirPeople as p, i (`${emailKey(p.email)}:${i}`)}
          {@const line = directoryLine(p)}
          <button type="button" class="row" onclick={(e) => openDirectoryCard(p, e.currentTarget)} disabled={!(p.email ?? "").trim()} title={p.email ?? ""} aria-haspopup="dialog">
            {#if isDirectoryList(p)}
              <span class="group-avatar" aria-hidden="true">
                <svg width="16" height="16" viewBox="0 0 16 16"><path d={CONTACT_ICON.people} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
              </span>
            {:else}
              <Avatar name={p.name} email={p.email} size={32} />
            {/if}
            <span class="who">
              <span class="name">{p.name?.trim() || p.email}</span>
              <span class="line caption">{line || p.email || ""}</span>
            </span>
          </button>
        {/each}
      </div>
    {/if}
  </div>

  <div class="detail">
    {#if tab === "contacts" && selected}
      {@const c = selected}
      <div class="head">
        {#if c.is_group}
          <span class="group-avatar big" aria-hidden="true">
            <svg width="28" height="28" viewBox="0 0 16 16"><path d={CONTACT_ICON.people} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </span>
        {:else}
          <Avatar name={c.name} email={c.email} size={64} />
        {/if}
        <div class="who">
          <h2 class="title">{c.name}</h2>
          {#if orgLine(c)}<div class="sub">{orgLine(c)}</div>{/if}
          <span class="badge">{c.is_group ? "Группа контактов" : "Личный контакт"}</span>
        </div>
        <div class="head-actions">
          {#if c.is_group}
            <button class="btn primary" onclick={() => writeToGroup(c)} disabled={!c.item.members.length}>
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.compose} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
              Написать группе
            </button>
          {:else}
            <button class="btn primary" onclick={() => writeTo(c)} disabled={!c.email}>
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.compose} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
              Написать письмо
            </button>
            <button class="btn" onclick={() => c.email && onsearch(c.email)} disabled={!c.email} title="Письма от этого адреса в почте на компьютере">
              <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.search} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
              Найти письма
            </button>
            <button class="btn" onclick={(e) => c.email && openDirectoryCard({ name: c.name, email: c.email }, e.currentTarget)} disabled={!c.email} title="Запись справочника организации по этому адресу" aria-haspopup="dialog">В справочнике</button>
          {/if}
        </div>
      </div>

      <dl class="facts">
        {#each c.item.emails as e, i (`${e.key ?? ""}:${i}`)}
          <div class="fact">
            <dt>{emailLabel(e.key)}</dt>
            <dd class="addr">
              <button class="link-btn" onclick={() => writeTo(c, e.email)} title="Написать на этот адрес">{e.email}</button>
              {#if e.name && e.name !== c.name}<span class="caption"> — {e.name}</span>{/if}
              <button class="icon-btn small" onclick={() => void copy(e.email, "Адрес")} aria-label="Копировать адрес" title="Копировать адрес">
                <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.copy} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
              </button>
            </dd>
          </div>
        {/each}
        {#each c.item.phones as p, i (`${p.key ?? ""}:${i}`)}
          <div class="fact">
            <dt>{phoneLabel(p.key)}</dt>
            <dd><a class="link" href={telHref(p.number)} onclick={(e) => void call(e, p.number)} title="Позвонить">{p.number}</a></dd>
          </div>
        {/each}
        {#each c.item.im_addresses as im, i (`${im}:${i}`)}
          <div class="fact">
            <dt>Мессенджер</dt>
            <dd>{im}</dd>
          </div>
        {/each}
        {#if c.item.company}
          <div class="fact"><dt>Компания</dt><dd>{c.item.company}</dd></div>
        {/if}
        {#if c.item.job_title}
          <div class="fact"><dt>Должность</dt><dd>{c.item.job_title}</dd></div>
        {/if}
        {#if c.item.department}
          <div class="fact"><dt>Отдел</dt><dd>{c.item.department}</dd></div>
        {/if}
        {#if c.item.office}
          <div class="fact"><dt>Офис</dt><dd>{c.item.office}</dd></div>
        {/if}
        {#each c.item.addresses as a, i (`${a.key ?? ""}:${i}`)}
          <div class="fact">
            <dt>{addressLabel(a.key)}</dt>
            <dd>{addressText(a)}</dd>
          </div>
        {/each}
        {#if c.birthday_ms != null}
          <div class="fact"><dt>День рождения</dt><dd>{birthdayText(c.birthday_ms)}</dd></div>
        {/if}
        {#if c.item.nickname}
          <div class="fact"><dt>Прозвище</dt><dd>{c.item.nickname}</dd></div>
        {/if}
      </dl>

      {#if c.is_group}
        <div class="section">
          <div class="sect-head caption">Участники: {c.item.members.length.toLocaleString("ru-RU")}</div>
          {#if c.item.members.length}
            <ul class="members">
              {#each c.item.members as m, i (`${emailKey(m.email)}:${i}`)}
                <li>
                  <button class="member" onclick={(e) => openDirectoryCard(m, e.currentTarget)} disabled={!(m.email ?? "").trim()} title={m.email ?? ""} aria-haspopup="dialog">
                    <Avatar name={m.name} email={m.email} size={24} />
                    <span class="member-name">{m.name?.trim() || m.email}</span>
                    {#if m.name?.trim() && m.email}<span class="caption">{m.email}</span>{/if}
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/if}

      {#if c.item.notes}
        <div class="section">
          <div class="sect-head caption">Заметки</div>
          <div class="notes">{c.item.notes}</div>
        </div>
      {/if}

      {#if c.modified_ms != null}
        <div class="foot caption">Изменён {listDate(c.modified_ms)}{#if c.item.has_photo} · в OWA есть фото{/if}</div>
      {/if}
      {#if status}<div class="status caption" role="status">{status}</div>{/if}
    {:else if tab === "contacts"}
      <div class="placeholder caption">
        {#if loaded && !total && synced}
          В ящике нет личных контактов. Контакт, созданный в OWA или Outlook, появится здесь после синхронизации.
        {:else}
          Выберите контакт слева
        {/if}
      </div>
    {:else}
      <div class="placeholder caption">Справочник организации: найденные люди открываются карточкой с переходом к письму и переписке</div>
    {/if}
  </div>
</div>

<style>
  .people {
    display: grid;
    grid-template-columns: minmax(280px, 380px) 1fr;
    height: 100%;
    min-height: 0;
  }
  .side {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-right: 1px solid var(--stroke-1);
    padding: var(--space-3) var(--space-3) 0;
    gap: var(--space-2);
  }
  .tabs {
    align-self: start;
  }
  .search {
    position: relative;
    display: flex;
    align-items: center;
    height: var(--control-height);
    background: var(--bg-layer-alt);
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    transition: border-color var(--dur-fast) var(--ease-in);
  }
  .search:focus-within {
    border-bottom: 2px solid var(--accent);
  }
  .search-icon {
    position: absolute;
    left: 10px;
    color: var(--fg-3);
    pointer-events: none;
  }
  .search input {
    flex: 1;
    height: 100%;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 var(--space-3) 0 32px;
    user-select: text;
  }
  .search input::placeholder {
    color: var(--fg-3);
  }
  .search input::-webkit-search-cancel-button {
    appearance: none;
  }
  .summary {
    color: var(--fg-3);
    padding: 0 var(--space-1);
    min-height: 16px;
  }
  .error {
    color: var(--danger, var(--fg-2));
  }
  .list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    margin: 0 calc(-1 * var(--space-3));
    padding: 0 var(--space-2) var(--space-3);
    outline: none;
  }
  .sect {
    padding: var(--space-2) var(--space-2) 2px;
    color: var(--fg-3);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-size: 11px;
  }
  .empty,
  .placeholder {
    color: var(--fg-3);
    padding: var(--space-4) var(--space-3);
    text-align: center;
  }
  .placeholder {
    max-width: 420px;
    margin: 20vh auto 0;
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    min-width: 0;
    padding: 4px var(--space-2);
    border-radius: var(--radius-control);
    text-align: left;
    color: var(--fg-1);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.on {
    background: var(--bg-selected);
  }
  .row:disabled {
    opacity: 0.6;
  }
  .who {
    min-width: 0;
    display: flex;
    flex-direction: column;
    line-height: 16px;
  }
  .name {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .line {
    color: var(--fg-3);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .group-avatar {
    flex: none;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: var(--accent-soft);
    color: var(--accent);
  }
  .group-avatar.big {
    width: 64px;
    height: 64px;
  }

  .detail {
    min-width: 0;
    overflow-y: auto;
    padding: var(--space-4) var(--space-5);
    user-select: text;
  }
  .head {
    display: flex;
    align-items: flex-start;
    gap: var(--space-4);
    flex-wrap: wrap;
  }
  .head .who {
    flex: 1;
    min-width: 200px;
  }
  .title {
    margin: 0;
    font: var(--text-body-strong);
    font-size: 20px;
    line-height: 26px;
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
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
    align-items: center;
  }
  .facts {
    margin: var(--space-4) 0 0;
    display: grid;
    grid-template-columns: max-content 1fr;
    column-gap: var(--space-4);
    row-gap: var(--space-2);
    max-width: 720px;
  }
  .fact {
    display: contents;
  }
  dt {
    color: var(--fg-3);
    white-space: nowrap;
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
    flex-wrap: wrap;
  }
  .link-btn {
    color: var(--accent);
    text-align: left;
    overflow-wrap: anywhere;
  }
  .link-btn:hover,
  .link:hover {
    text-decoration: underline;
  }
  .link {
    color: var(--accent);
  }
  .icon-btn {
    width: 24px;
    height: 24px;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
  }
  .icon-btn:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .section {
    margin-top: var(--space-4);
    max-width: 720px;
  }
  .sect-head {
    color: var(--fg-3);
    margin-bottom: var(--space-1);
  }
  .members {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .member {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: 2px var(--space-2);
    margin-left: calc(-1 * var(--space-2));
    border-radius: var(--radius-control);
    text-align: left;
    color: var(--fg-1);
  }
  .member:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .member-name {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .notes {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    padding: var(--space-2) var(--space-3);
    background: var(--bg-layer-alt);
    border-radius: var(--radius-control);
  }
  .foot,
  .status {
    margin-top: var(--space-3);
    color: var(--fg-3);
  }
</style>
