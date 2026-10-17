<script lang="ts">
  import { onMount } from "svelte";
  import { api, conversationKeyOf, errorMessage, messagesKey, onSyncProgress, type AccountInfo, type Conversation, type Folder, type Message, type SearchHit, type SyncStatus } from "./lib/api";
  import FolderTree from "./lib/components/FolderTree.svelte";
  import ConversationList from "./lib/components/ConversationList.svelte";
  import ReadingPane from "./lib/components/ReadingPane.svelte";
  import SearchResults from "./lib/components/SearchResults.svelte";
  import StatusBar from "./lib/components/StatusBar.svelte";

  let account = $state<AccountInfo | null>(null);
  let folders = $state<Folder[]>([]);
  let selectedFolder = $state<Folder | null>(null);
  let conversations = $state<Conversation[]>([]);
  let hasMore = $state(false);
  let loadingList = $state(false);
  let selected = $state<Conversation | null>(null);
  let messages = $state<Message[]>([]);
  let loadingMessages = $state(false);
  let query = $state("");
  let searchHits = $state<SearchHit[] | null>(null);
  let searching = $state(false);
  let error = $state<string | null>(null);
  let listWidth = $state(380);
  let syncStatus = $state<SyncStatus | null>(null);

  const PAGE = 100;
  /** Период автосинхронизации при видимом окне. */
  const SYNC_EVERY_MS = 2 * 60_000;

  async function loadFolders() {
    folders = await api.folders();
    const inbox = folders.find((f) => f.distinguished === "inbox") ?? folders.find((f) => f.is_mail) ?? null;
    if (inbox) await openFolder(inbox);
  }

  async function openFolder(f: Folder) {
    selectedFolder = f;
    selected = null;
    messages = [];
    searchHits = null;
    loadingList = true;
    try {
      conversations = await api.conversations(f.folder_pk, undefined, PAGE);
      hasMore = conversations.length === PAGE;
    } finally {
      loadingList = false;
    }
  }

  async function loadMore() {
    if (!selectedFolder || loadingList || !hasMore) return;
    const last = conversations[conversations.length - 1];
    if (!last || last.last_received_ms == null) {
      hasMore = false;
      return;
    }
    loadingList = true;
    try {
      const more = await api.conversations(selectedFolder.folder_pk, { ms: last.last_received_ms, pk: last.conversation_pk }, PAGE);
      conversations = [...conversations, ...more];
      hasMore = more.length === PAGE;
    } finally {
      loadingList = false;
    }
  }

  /** Номер последнего запроса беседы: ответ устаревшего запроса не должен ни подменить письма, ни погасить индикатор. */
  let conversationRequest = 0;

  async function openConversation(c: Conversation) {
    const switching = selected?.conversation_pk !== c.conversation_pk;
    selected = c;
    // При смене беседы старые письма убираем сразу: панель чтения не должна раскрывать письмо прошлой беседы.
    if (switching) messages = [];
    const req = ++conversationRequest;
    loadingMessages = true;
    try {
      const fresh = await api.conversation(c.conversation_pk);
      if (req === conversationRequest) messages = fresh;
    } finally {
      if (req === conversationRequest) loadingMessages = false;
    }
  }

  async function openConversationByPk(pk: number) {
    const existing = conversations.find((c) => c.conversation_pk === pk);
    const switching = selected?.conversation_pk !== pk;
    selected = existing ?? { conversation_pk: pk, topic: null, participants: [], item_count: 0, unread_count: 0, total_count: 0, last_received_ms: null, has_attachments: false };
    if (switching) messages = [];
    const req = ++conversationRequest;
    loadingMessages = true;
    try {
      const fresh = await api.conversation(pk);
      if (req !== conversationRequest) return;
      messages = fresh;
      if (!existing && messages.length) {
        selected = { ...selected, topic: messages[0].subject, total_count: messages.length };
      }
    } finally {
      if (req === conversationRequest) loadingMessages = false;
    }
  }

  // --- догрузка письма с сервера -------------------------------------------

  /** Запросы message_fetch в полёте по item_pk: повторный клик или второй эффект не плодят сетевых вызовов. */
  const fetching = new Map<number, Promise<Message>>();

  /**
   * Догружает тело и инлайновые картинки одного письма и подменяет его в открытой беседе точечно,
   * не трогая остальные карточки. Ошибку пробрасывает вызывающей карточке — она покажет её у себя.
   */
  function fetchMessage(itemPk: number): Promise<void> {
    let p = fetching.get(itemPk);
    if (!p) {
      p = api.messageFetch(itemPk).finally(() => fetching.delete(itemPk));
      fetching.set(itemPk, p);
    }
    return p.then((fresh) => {
      const i = messages.findIndex((m) => m.item_pk === itemPk);
      // Беседу могли переключить, пока шёл запрос: тогда подменять нечего, тело уже в базе.
      if (i >= 0) messages[i] = fresh;
    });
  }

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  function onQueryInput() {
    clearTimeout(searchTimer);
    const q = query.trim();
    if (!q) {
      searchHits = null;
      return;
    }
    searchTimer = setTimeout(async () => {
      searching = true;
      try {
        searchHits = await api.search(q, 100);
      } catch (e) {
        error = errorMessage(e);
      } finally {
        searching = false;
      }
    }, 180);
  }

  function clearSearch() {
    query = "";
    searchHits = null;
  }

  // --- синхронизация ---------------------------------------------------------

  let lastSyncAttempt = 0;

  async function launchSync(folders?: number[]) {
    lastSyncAttempt = Date.now();
    try {
      syncStatus = await api.syncStart(undefined, folders);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Все почтовые папки (кнопка «Обновить», автозапуск, таймер). */
  function startSync() {
    return launchSync();
  }

  /** Только одна папка — кнопка на строке дерева. Пока идёт другая синхронизация, запуск не делается. */
  function startFolderSync(f: Folder) {
    if (syncStatus?.running) {
      error = "Синхронизация уже идёт, дождись её окончания";
      return;
    }
    void launchSync([f.folder_pk]);
  }

  async function cancelSync() {
    try {
      syncStatus = await api.syncCancel();
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Раз в 2 минуты при видимом окне; не запускает повторно, пока идёт предыдущая. */
  function syncTick() {
    if (document.visibilityState !== "visible") return;
    if (syncStatus?.running) return;
    if (Date.now() - lastSyncAttempt < SYNC_EVERY_MS) return;
    void startSync();
  }

  /** После синхронизации: перечитать папки, текущий список и открытую беседу, не сбивая выбор. */
  async function refreshAfterSync() {
    try {
      const fresh = await api.folders();
      folders = fresh;
      if (selectedFolder) {
        const same = fresh.find((f) => f.folder_pk === selectedFolder!.folder_pk);
        if (same) selectedFolder = same;
        if (searchHits === null && !loadingList) {
          const limit = Math.min(500, Math.max(PAGE, conversations.length));
          const rows = await api.conversations(selectedFolder.folder_pk, undefined, limit);
          conversations = rows;
          hasMore = rows.length === limit;
        }
      }
      if (selected && !loadingMessages) {
        // Письма перечитываем только при реальных изменениях (состав, тело, прочитанность, кэш вложений):
        // иначе перезагрузятся фреймы с текстом и собьётся прокрутка. Сначала дешёвый ключ без тел
        // и картинок; полная беседа с base64-картинками читается, лишь если ключ разошёлся.
        const pk = selected.conversation_pk;
        const keys = await api.conversationKey(pk);
        if (conversationKeyOf(keys) !== messagesKey(messages)) {
          const fresh = await api.conversation(pk);
          // Пока читали, беседу могли переключить.
          if (selected?.conversation_pk === pk && !loadingMessages) messages = fresh;
        }
      }
    } catch (e) {
      error = errorMessage(e);
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      document.getElementById("search-input")?.focus();
    }
    if (e.key === "Escape" && document.activeElement?.id === "search-input") {
      clearSearch();
      (document.activeElement as HTMLElement).blur();
    }
  }

  onMount(() => {
    let unlisten: (() => void) | undefined;
    let wasRunning = false;
    const timer = setInterval(syncTick, 15_000);
    document.addEventListener("visibilitychange", syncTick);

    (async () => {
      try {
        unlisten = await onSyncProgress((s) => {
          syncStatus = s;
          if (wasRunning && !s.running) {
            // Пауза в 2 минуты отсчитывается от конца прогона, а не от его начала:
            // после долгой первой синхронизации следующая не должна стартовать сразу.
            lastSyncAttempt = Date.now();
            void refreshAfterSync();
          }
          wasRunning = s.running;
        });
        account = await api.accountInfo();
        await loadFolders();
        syncStatus = await api.syncStatus();
        wasRunning = syncStatus.running;
        // Автозапуск при старте: окно сначала показывает базу, потом тянет новое.
        void startSync();
      } catch (e) {
        error = errorMessage(e);
      }
    })();

    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", syncTick);
      unlisten?.();
    };
  });

  // Перетаскивание границы между списком и чтением.
  let dragging = $state(false);
  function startDrag(e: PointerEvent) {
    dragging = true;
    const startX = e.clientX;
    const startWidth = listWidth;
    const move = (ev: PointerEvent) => {
      listWidth = Math.min(520, Math.max(280, startWidth + ev.clientX - startX));
    };
    const up = () => {
      dragging = false;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="shell" class:dragging style="--list-width: {listWidth}px">
  <header class="topbar">
    <div class="brand">
      <span class="brand-mark" aria-hidden="true"></span>
      <span class="brand-name">AIOWA</span>
    </div>
    <div class="search" class:active={query.length > 0}>
      <svg class="search-icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M10.5 10.5 14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      <input id="search-input" type="search" placeholder="Поиск в почте: тема, текст, от:имя, вложение:" bind:value={query} oninput={onQueryInput} autocomplete="off" spellcheck="false" />
      <kbd class="search-kbd">Ctrl K</kbd>
    </div>
    <div class="account caption">
      {#if account}{account.user}{/if}
    </div>
  </header>

  <nav class="nav" aria-label="Папки">
    <FolderTree {folders} selected={selectedFolder} onselect={openFolder} onsync={startFolderSync} syncing={syncStatus?.running ?? false} />
  </nav>

  <section class="list" aria-label="Список бесед">
    {#if searchHits !== null}
      <SearchResults hits={searchHits} {searching} {query} onopen={openConversationByPk} onclear={clearSearch} selectedPk={selected?.conversation_pk ?? null} />
    {:else}
      <ConversationList folder={selectedFolder} {conversations} {selected} loading={loadingList} {hasMore} onselect={openConversation} onmore={loadMore} />
    {/if}
  </section>

  <div class="splitter" role="separator" aria-orientation="vertical" aria-label="Ширина списка" onpointerdown={startDrag}></div>

  <section class="reader" aria-label="Чтение">
    <ReadingPane conversation={selected} {messages} loading={loadingMessages} onfetch={fetchMessage} />
  </section>

  <footer class="status" aria-label="Состояние синхронизации">
    <StatusBar status={syncStatus} onrefresh={startSync} oncancel={cancelSync} />
  </footer>

  {#if error}
    <div class="error-bar" role="alert">
      {error}
      <button onclick={() => (error = null)} aria-label="Закрыть">×</button>
    </div>
  {/if}
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--nav-width) var(--list-width) 1px minmax(var(--read-min), 1fr);
    grid-template-rows: 48px 1fr 28px;
    grid-template-areas:
      "top top top top"
      "nav list split reader"
      "status status status status";
    height: 100%;
    background: var(--bg-base);
  }
  .shell.dragging {
    cursor: col-resize;
  }
  .shell.dragging :global(iframe) {
    pointer-events: none;
  }

  .topbar {
    grid-area: top;
    display: grid;
    grid-template-columns: var(--nav-width) 1fr auto;
    align-items: center;
    gap: var(--space-4);
    padding: 0 var(--space-4) 0 var(--space-3);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .brand-mark {
    width: 20px;
    height: 20px;
    border-radius: 6px;
    background: linear-gradient(135deg, var(--accent), var(--accent-hover));
  }
  .brand-name {
    font: var(--text-body-strong);
    letter-spacing: 0.02em;
  }

  .search {
    position: relative;
    display: flex;
    align-items: center;
    max-width: 640px;
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
    padding: 0 60px 0 32px;
    user-select: text;
  }
  .search input::placeholder {
    color: var(--fg-3);
  }
  .search input::-webkit-search-cancel-button {
    appearance: none;
  }
  .search-kbd {
    position: absolute;
    right: 8px;
    font: var(--text-caption);
    color: var(--fg-4);
    border: 1px solid var(--stroke-1);
    border-radius: 3px;
    padding: 0 5px;
    line-height: 16px;
  }
  .search.active .search-kbd {
    display: none;
  }

  .nav {
    grid-area: nav;
    overflow: auto;
    padding: var(--space-2) var(--space-2) var(--space-4);
  }
  .list {
    grid-area: list;
    min-width: 0;
    background: var(--bg-layer);
    border: 1px solid var(--stroke-1);
    border-right: 0;
    border-radius: var(--radius-card) 0 0 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .splitter {
    grid-area: split;
    background: var(--stroke-1);
    cursor: col-resize;
    position: relative;
  }
  .splitter::after {
    content: "";
    position: absolute;
    inset: 0 -4px;
  }
  .reader {
    grid-area: reader;
    min-width: 0;
    background: var(--bg-layer);
    border: 1px solid var(--stroke-1);
    border-left: 0;
    border-radius: 0 var(--radius-card) 0 0;
    overflow: hidden;
    margin-right: var(--space-2);
  }

  .status {
    grid-area: status;
    min-width: 0;
  }

  .error-bar {
    position: fixed;
    left: 50%;
    bottom: 36px;
    transform: translateX(-50%);
    background: var(--bg-card);
    color: var(--danger);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
    padding: var(--space-2) var(--space-3);
    display: flex;
    gap: var(--space-3);
    align-items: center;
    max-width: 70vw;
    user-select: text;
  }
  .error-bar button {
    color: var(--fg-2);
    font-size: 18px;
    line-height: 1;
    padding: 0 4px;
  }
</style>
