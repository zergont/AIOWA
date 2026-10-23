<script lang="ts">
  import { onMount, tick } from "svelte";
  import { api, conversationKeyOf, errorMessage, messagesKey, onCloseRequested, onItemsChanged, onSyncProgress, type AccountInfo, type ComposeContext, type ComposeKind, type Conversation, type Folder, type ItemsChanged, type Message, type SearchHit, type SyncStatus, type WriteReport } from "./lib/api";
  import { ICON, failureText, type ItemAction } from "./lib/actions";
  import { focusBack, type Anchor } from "./lib/popover";
  import FolderTree from "./lib/components/FolderTree.svelte";
  import ConversationList from "./lib/components/ConversationList.svelte";
  import ReadingPane from "./lib/components/ReadingPane.svelte";
  import SearchResults from "./lib/components/SearchResults.svelte";
  import StatusBar from "./lib/components/StatusBar.svelte";
  import Composer from "./lib/components/Composer.svelte";
  import ConfirmPopover from "./lib/components/ConfirmPopover.svelte";
  import CalendarView from "./lib/components/CalendarView.svelte";
  import MiniMonth from "./lib/components/MiniMonth.svelte";
  import { sameDay, startOfDay, viewRange, type CalendarViewKind } from "./lib/calendar";

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

  // --- разделы: почта и календарь --------------------------------------------------------
  // Панели обоих разделов при переключении не размонтируются, а прячутся: открытая беседа, композер
  // с набранным текстом и прокрутка списка — как и выбранное событие с прокруткой сетки — остаются
  // как были. Календарь создаётся при первом показе (на старте приложения запроса календаря нет);
  // якорный день и вид живут здесь, потому что их меняет и мини-календарь в колонке навигации.

  type Section = "mail" | "calendar";
  let section = $state<Section>("mail");
  /** Календарь уже показывали: дальше он только прячется. */
  let calendarMounted = $state(false);
  $effect(() => {
    if (section === "calendar") calendarMounted = true;
  });
  /** Якорный день сетки календаря: меняют мини-календарь, стрелки и «Сегодня». */
  let calendarAnchor = $state(startOfDay(new Date()));
  let calendarKind = $state<CalendarViewKind>("week");
  const calendarRange = $derived(viewRange(calendarKind, calendarAnchor));
  /**
   * Какое «сегодня» было при последней смене якоря. Якорь, равный ему, значил «сегодня»: окно может
   * висеть открытым сутками, и календарь, который ещё не листали, должен открыться на сегодняшнем
   * дне, а не на дне запуска. Пользователь, ушедший на другой день, своё место не теряет.
   */
  let anchorSetOn = startOfDay(new Date());
  $effect(() => {
    calendarAnchor; // зависимость: любая смена якоря
    anchorSetOn = startOfDay(new Date());
  });

  function showCalendar() {
    const today = startOfDay(new Date());
    if (sameDay(calendarAnchor, anchorSetOn) && !sameDay(anchorSetOn, today)) calendarAnchor = today;
    section = "calendar";
  }

  function pickCalendarDay(d: Date) {
    if (d.getTime() !== calendarAnchor.getTime()) calendarAnchor = d;
  }

  const deletedFolderPk = $derived(folders.find((f) => f.distinguished === "deleteditems")?.folder_pk ?? null);
  const sentFolderPk = $derived(folders.find((f) => f.distinguished === "sentitems")?.folder_pk ?? null);

  // --- композер: новое письмо, ответ, ответ всем, пересылка ------------------------------

  /** Открытая заготовка; новая заготовка — новый экземпляр композера ({#key}). */
  let compose = $state<ComposeContext | null>(null);
  /** Беседа, над карточками которой стоит композер ответа; у нового письма null. */
  let composeConversationPk = $state<number | null>(null);
  let composerDirty = $state(false);
  /** Композер отправляет или сохраняет черновик: заменять его и закрывать окно сейчас нельзя. */
  let composerBusy = $state<"send" | "draft" | null>(null);
  let composeLoading = false;
  /** Запрошенное письмо, пока пользователь решает, бросить ли текущее. */
  let pendingCompose = $state<{ kind: ComposeKind; itemPk: number | null; conversationPk: number | null; anchor: Anchor } | null>(null);
  /** Куда вернуть фокус, когда подтверждение замены закрыто без замены. */
  let composeReturnTo: HTMLElement | null = null;
  /** Письмо отправлено во время идущей синхронизации: «Отправленные» дотянуть сразу после её конца. */
  let pendingSentSync = false;
  /** Крестик или Alt+F4 при несохранённом письме: бэкенд не закрыл окно и ждёт подтверждения — якорь попапа; null — не запрошено. */
  let closeRequest = $state<Anchor | null>(null);
  /** Самое верхнее развёрнутое письмо в области чтения (для Ctrl+R и других клавиш). */
  let topExpandedPk = $state<number | null>(null);
  let readerEl = $state<HTMLElement | null>(null);
  /** Короткое уведомление в строке состояния («Отправлено»). */
  let notice = $state<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  const composerAlone = $derived(compose !== null && (compose.kind === "new" || composeConversationPk === null || composeConversationPk !== selected?.conversation_pk));

  function showNotice(text: string) {
    notice = text;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), 5000);
  }

  /** Якорь подтверждения «отменить текущее письмо?»: верх области чтения, где стоит композер. */
  function composerAnchor(): Anchor {
    const r = readerEl?.getBoundingClientRect();
    return { x: (r?.left ?? 0) + 24, y: (r?.top ?? 0) + 56 };
  }

  /**
   * Открыть композер. Один за раз: если текущий с правками — спросить, прежде чем заменить.
   * `conversationPk` — беседа исходного письма (композер ответа стоит над её карточками).
   */
  function openCompose(kind: ComposeKind, itemPk: number | null, conversationPk: number | null) {
    // Композер живёт в области чтения почты: из календаря сначала возвращаемся туда.
    section = "mail";
    if (compose && composerBusy) {
      error = busyText();
      return;
    }
    if (compose && composerDirty) {
      composeReturnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      // Якорь — после обновления DOM: из календаря область чтения ещё спрятана, её прямоугольник нулевой.
      void tick().then(() => {
        pendingCompose = { kind, itemPk, conversationPk, anchor: composerAnchor() };
      });
      return;
    }
    void loadCompose(kind, itemPk, conversationPk);
  }

  /** Пока идёт отправка, композер не заменяется: иначе её итог пришёл бы уже в другой композер. */
  function busyText(): string {
    return composerBusy === "send" ? "Подожди: письмо отправляется" : "Подожди: черновик сохраняется";
  }

  async function loadCompose(kind: ComposeKind, itemPk: number | null, conversationPk: number | null) {
    if (composeLoading || composerBusy) return;
    composeLoading = true;
    try {
      const ctx = await api.composeContext(kind, itemPk);
      compose = ctx;
      composeConversationPk = kind === "new" ? null : conversationPk;
      composerDirty = false;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      composeLoading = false;
    }
  }

  function confirmReplace() {
    const p = pendingCompose;
    cancelReplace();
    if (!p) return;
    // Отправку могли начать, пока подтверждение было открыто.
    if (compose && composerBusy) {
      error = busyText();
      return;
    }
    void loadCompose(p.kind, p.itemPk, p.conversationPk);
  }

  function cancelReplace() {
    pendingCompose = null;
    focusBack(composeReturnTo);
    composeReturnTo = null;
  }

  /** Ответ с карточки письма (кнопки и клавиши): беседа — открытая. */
  function composeFromMessage(itemPk: number, kind: ComposeKind) {
    openCompose(kind, itemPk, selected?.conversation_pk ?? null);
  }

  /** Ответ из меню списка: самое свежее письмо беседы в текущей папке. */
  async function composeFromConversation(c: Conversation, kind: ComposeKind) {
    if (!selectedFolder) return;
    try {
      const pks = await api.conversationItemPks(c.conversation_pk, selectedFolder.folder_pk);
      if (!pks.length) {
        error = "В этой папке нет писем беседы";
        return;
      }
      openCompose(kind, pks[0], c.conversation_pk);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  function closeCompose(reason: "sent" | "draft" | "cancel") {
    compose = null;
    composeConversationPk = null;
    composerDirty = false;
    composerBusy = null;
    if (reason === "sent") {
      showNotice("Отправлено");
      // Отправленное появится в «Отправленных» после синхронизации этой папки. Идущую синхронизацию
      // бэкенд не прерывает и запрос не ставит в очередь — тогда папку дотянем после её конца.
      if (sentFolderPk != null) {
        if (syncStatus?.running) pendingSentSync = true;
        else void launchSync([sentFolderPk]);
      }
    } else if (reason === "draft") {
      showNotice("Черновик сохранён в «Черновиках» на сервере");
    }
  }

  // --- закрытие окна при несохранённом письме ---------------------------------------------
  // Внутри приложения замена и отмена письма с правками требуют подтверждения; крестик и Alt+F4
  // не должны быть лазейкой. Флаг держит бэкенд: при нём CloseRequested превращается в событие.

  const closeGuard = $derived(compose !== null && (composerDirty || composerBusy !== null));
  $effect(() => {
    const guarded = closeGuard;
    api.windowCloseGuard(guarded).catch((e) => console.warn("window_close_guard:", errorMessage(e)));
  });

  function onCloseRequest() {
    // Флаг мог ещё не сняться (письмо только что закрыто): тогда нечего спрашивать.
    if (!closeGuard) {
      void confirmClose();
      return;
    }
    // Письмо, которое будет потеряно, должно быть на экране: из календаря сначала показать почту;
    // якорь попапа — после обновления DOM (спрятанная область чтения даёт нулевой прямоугольник).
    section = "mail";
    void tick().then(() => {
      closeRequest = composerAnchor();
    });
  }

  async function confirmClose() {
    closeRequest = null;
    try {
      await api.windowCloseConfirmed();
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Фокус в поле ввода или редакторе: клавиши-буквы с Ctrl там не наши. */
  function inEditable(): boolean {
    const el = document.activeElement;
    return el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLIFrameElement || (el instanceof HTMLElement && el.isContentEditable);
  }

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
    selected = existing ?? { conversation_pk: pk, topic: null, participants: [], item_count: 0, unread_count: 0, total_count: 0, last_received_ms: null, has_attachments: false, has_flagged: false };
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

  // --- запись: прочитано, флаг, перенос, удаление ----------------------------------

  function callWrite(itemPks: number[], a: ItemAction): Promise<WriteReport> {
    switch (a.kind) {
      case "read":
        return api.itemsMarkRead(itemPks, a.read);
      case "flag":
        return api.itemsSetFlag(itemPks, a.flagged);
      case "move":
        return api.itemsMove(itemPks, a.folderPk);
      case "delete":
        return api.itemsDelete(itemPks, a.permanent);
    }
  }

  /**
   * Оптимистичная правка открытой беседы: окно меняется сразу, не дожидаясь сервера.
   * Событие items-changed потом сверит всё с базой (и откатит отказавшее).
   */
  function applyToMessages(itemPks: number[], a: ItemAction) {
    const set = new Set(itemPks);
    const nameOf = (pk: number) => folders.find((f) => f.folder_pk === pk)?.name ?? null;
    if (a.kind === "delete" && a.permanent) {
      messages = messages.filter((m) => !set.has(m.item_pk));
      return;
    }
    for (const m of messages) {
      if (!set.has(m.item_pk)) continue;
      if (a.kind === "read") m.is_read = a.read;
      else if (a.kind === "flag") m.is_flagged = a.flagged;
      else if (a.kind === "move") {
        m.folder_pk = a.folderPk;
        m.folder_name = nameOf(a.folderPk) ?? m.folder_name;
      } else if (deletedFolderPk != null) {
        m.folder_pk = deletedFolderPk;
        m.folder_name = nameOf(deletedFolderPk) ?? m.folder_name;
      }
    }
  }

  /** Оптимистичная правка строки списка для действия над всей беседой в текущей папке. */
  function applyToRow(c: Conversation, a: ItemAction) {
    const row = conversations.find((x) => x.conversation_pk === c.conversation_pk);
    if (!row) return;
    if (a.kind === "read") row.unread_count = a.read ? 0 : row.item_count;
    else if (a.kind === "flag") row.has_flagged = a.flagged;
    else dropRow(c.conversation_pk);
  }

  /** Беседа ушла из текущей папки: убрать строку; если она была открыта — перейти к соседней или снять выбор. */
  function dropRow(pk: number) {
    const idx = conversations.findIndex((c) => c.conversation_pk === pk);
    if (idx < 0) return;
    const rows = conversations.filter((c) => c.conversation_pk !== pk);
    conversations = rows;
    if (selected?.conversation_pk === pk) selectNeighbour(rows, idx);
  }

  function selectNeighbour(rows: Conversation[], idx: number) {
    const next = rows[Math.min(idx, rows.length - 1)];
    if (next) void openConversation(next).catch((e) => (error = errorMessage(e)));
    else {
      selected = null;
      messages = [];
    }
  }

  /**
   * Выполнить действие над письмами: сразу поправить окно, затем команда (локально → сервер).
   * Отказ — исключение с текстом для пользователя; окно вернётся к серверному состоянию по items-changed,
   * а при отказе всего вызова — немедленным перечитыванием.
   */
  async function runWrite(itemPks: number[], a: ItemAction): Promise<void> {
    if (!itemPks.length) return;
    applyToMessages(itemPks, a);
    let report: WriteReport;
    try {
      report = await callWrite(itemPks, a);
    } catch (e) {
      await refreshAfterWrite({ conversation_pks: selected ? [selected.conversation_pk] : [], folder_pks: [] });
      throw errorMessage(e);
    }
    const failed = failureText(report);
    if (failed) throw failed;
  }

  /** Действие над одним письмом с карточки. */
  function onMessageAction(itemPk: number, a: ItemAction): Promise<void> {
    return runWrite([itemPk], a);
  }

  /**
   * Действие над всеми письмами беседы в текущей папке (меню списка, клавиша Delete).
   * Исключение — «Снять флаг»: признак has_flagged в строке считается по всей беседе, поэтому и
   * снимается флаг со всех её писем, где бы они ни лежали; иначе флаг на письме из другой папки
   * оставался бы, а пункт меню ничего не делал.
   */
  async function onConversationAction(c: Conversation, a: ItemAction): Promise<void> {
    if (!selectedFolder) return;
    const folderPk = a.kind === "flag" && !a.flagged ? null : selectedFolder.folder_pk;
    const itemPks = await api.conversationItemPks(c.conversation_pk, folderPk);
    if (!itemPks.length) return;
    applyToRow(c, a);
    await runWrite(itemPks, a);
  }

  /** Перечитать папки, не сбивая выбранную (объект заменяется свежим со счётчиками). */
  async function reloadFolders() {
    const fresh = await api.folders();
    folders = fresh;
    if (selectedFolder) {
      const same = fresh.find((f) => f.folder_pk === selectedFolder!.folder_pk);
      if (same) selectedFolder = same;
    }
  }

  /** Перечитать список текущей папки тем же объёмом, что показан (прокрутка остаётся). null — сейчас нельзя. */
  async function reloadList(): Promise<Conversation[] | null> {
    if (!selectedFolder || searchHits !== null || loadingList) return null;
    const limit = Math.min(500, Math.max(PAGE, conversations.length));
    const rows = await api.conversations(selectedFolder.folder_pk, undefined, limit);
    conversations = rows;
    hasMore = rows.length === limit;
    return rows;
  }

  /**
   * После команды записи (событие items-changed): папки и список текущей папки перечитываются,
   * открытая беседа — если затронута. Если беседы в папке больше нет, выбор переходит к соседней строке.
   */
  async function refreshAfterWrite(ev: ItemsChanged) {
    try {
      await reloadFolders();
      if (selected && ev.conversation_pks.includes(selected.conversation_pk) && !loadingMessages) {
        const pk = selected.conversation_pk;
        const fresh = await api.conversation(pk);
        if (selected?.conversation_pk === pk && !loadingMessages) messages = fresh;
      }
      const before = selected ? conversations.findIndex((c) => c.conversation_pk === selected!.conversation_pk) : -1;
      const rows = await reloadList();
      if (rows && selected) {
        const same = rows.find((c) => c.conversation_pk === selected!.conversation_pk);
        if (same) selected = same;
        else if (before >= 0) selectNeighbour(rows, before);
      }
    } catch (e) {
      error = errorMessage(e);
    }
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
      await reloadFolders();
      await reloadList();
      if (selected && !loadingMessages) {
        // Письма перечитываем только при реальных изменениях (состав, тело, прочитанность, кэш вложений):
        // иначе перезагрузятся фреймы с текстом и собьётся прокрутка. Сначала дешёвый ключ без тел
        // и картинок; полная беседа с base64-картинками читается, лишь если ключ разошёлся.
        const pk = selected.conversation_pk;
        const keys = await api.conversationKey(pk);
        const withFlag = keys.every((k) => k.is_flagged !== undefined);
        if (conversationKeyOf(keys, withFlag) !== messagesKey(messages, withFlag)) {
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
    const ctrl = e.ctrlKey || e.metaKey;
    // По коду клавиши, а не по символу: в русской раскладке e.key даёт кириллицу.
    const is = (code: string, key: string) => e.code === code || e.key.toLowerCase() === key;
    if (ctrl && e.shiftKey && !e.altKey && (e.code === "Digit1" || e.code === "Digit2")) {
      e.preventDefault();
      if (e.code === "Digit1") section = "mail";
      else showCalendar();
      return;
    }
    if (ctrl && !e.shiftKey && !e.altKey && is("KeyK", "k")) {
      e.preventDefault();
      // Поле поиска спрятано в календаре: сначала показать почту, фокус — после обновления DOM.
      section = "mail";
      void tick().then(() => document.getElementById("search-input")?.focus());
      return;
    }
    if (ctrl && !e.shiftKey && !e.altKey && is("KeyN", "n")) {
      e.preventDefault();
      openCompose("new", null, null);
      return;
    }
    // Перезагрузку окна (Ctrl+R, F5) гасим везде, даже в редакторе: она бы потеряла набранное письмо.
    if (e.key === "F5" || (ctrl && !e.altKey && is("KeyR", "r"))) {
      e.preventDefault();
      if (e.key !== "F5" && section === "mail" && !inEditable() && topExpandedPk != null) composeFromMessage(topExpandedPk, e.shiftKey ? "reply_all" : "reply");
      return;
    }
    if (ctrl && e.shiftKey && !e.altKey && is("KeyF", "f") && !inEditable()) {
      e.preventDefault();
      if (section === "mail" && topExpandedPk != null) composeFromMessage(topExpandedPk, "forward");
      return;
    }
    if (e.key === "Escape" && document.activeElement?.id === "search-input") {
      clearSearch();
      (document.activeElement as HTMLElement).blur();
    }
  }

  onMount(() => {
    let unlisten: (() => void) | undefined;
    let unlistenItems: (() => void) | undefined;
    let unlistenClose: (() => void) | undefined;
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
            if (pendingSentSync && sentFolderPk != null) {
              pendingSentSync = false;
              void launchSync([sentFolderPk]);
            }
          }
          wasRunning = s.running;
        });
        unlistenItems = await onItemsChanged((ev) => void refreshAfterWrite(ev));
        unlistenClose = await onCloseRequested(onCloseRequest);
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
      clearTimeout(noticeTimer);
      document.removeEventListener("visibilitychange", syncTick);
      unlisten?.();
      unlistenItems?.();
      unlistenClose?.();
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

{#snippet composerSnippet()}
  {#key compose}
    {#if compose}
      <Composer ctx={compose} bind:dirty={composerDirty} bind:busy={composerBusy} onclose={closeCompose} />
    {/if}
  {/key}
{/snippet}

<div class="shell" class:dragging style="--list-width: {listWidth}px">
  <header class="topbar">
    <div class="brand">
      <span class="brand-mark" aria-hidden="true"></span>
      <span class="brand-name">AIOWA</span>
    </div>
    <div class="segmented sections" role="tablist" aria-label="Разделы">
      <button role="tab" aria-selected={section === "mail"} class:on={section === "mail"} onclick={() => (section = "mail")} title="Почта (Ctrl+Shift+1)">Почта</button>
      <button role="tab" aria-selected={section === "calendar"} class:on={section === "calendar"} onclick={showCalendar} title="Календарь (Ctrl+Shift+2)">Календарь</button>
    </div>
    <button class="new-mail" onclick={() => openCompose("new", null, null)} title="Новое письмо (Ctrl+N)">
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.compose} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Новое письмо
    </button>
    <div class="search" class:active={query.length > 0} class:off={section !== "mail"}>
      <svg class="search-icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M10.5 10.5 14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      <input id="search-input" type="search" placeholder="Поиск в почте: тема, текст, от:имя, вложение:" bind:value={query} oninput={onQueryInput} autocomplete="off" spellcheck="false" />
      <kbd class="search-kbd">Ctrl K</kbd>
    </div>
    <div class="account caption">
      {#if account}{account.user}{/if}
    </div>
  </header>

  <nav class="nav" aria-label={section === "mail" ? "Папки" : "Мини-календарь"}>
    <div class="folders" class:off={section !== "mail"}>
      <FolderTree {folders} selected={selectedFolder} onselect={openFolder} onsync={startFolderSync} syncing={syncStatus?.running ?? false} />
    </div>
    {#if section === "calendar"}
      <MiniMonth selected={calendarAnchor} range={calendarRange} onselect={pickCalendarDay} />
    {/if}
  </nav>

  <section class="list" class:off={section !== "mail"} aria-label="Список бесед">
    {#if searchHits !== null}
      <SearchResults hits={searchHits} {searching} {query} onopen={openConversationByPk} onclear={clearSearch} selectedPk={selected?.conversation_pk ?? null} />
    {:else}
      <ConversationList folder={selectedFolder} {conversations} {selected} loading={loadingList} {hasMore} onselect={openConversation} onmore={loadMore} {folders} onaction={onConversationAction} oncompose={composeFromConversation} />
    {/if}
  </section>

  <div class="splitter" class:off={section !== "mail"} role="separator" aria-orientation="vertical" aria-label="Ширина списка" onpointerdown={startDrag}></div>

  <section class="reader" class:off={section !== "mail"} aria-label="Чтение" bind:this={readerEl}>
    <ReadingPane conversation={selected} {messages} loading={loadingMessages} onfetch={fetchMessage} {folders} {deletedFolderPk} onaction={onMessageAction} oncompose={composeFromMessage} composer={compose ? composerSnippet : undefined} {composerAlone} bind:topExpandedPk />
  </section>

  {#if calendarMounted}
    <section class="calendar" class:off={section !== "calendar"} aria-label="Календарь">
      <CalendarView bind:anchor={calendarAnchor} bind:kind={calendarKind} visible={section === "calendar"} />
    </section>
  {/if}

  <footer class="status" aria-label="Состояние синхронизации">
    <StatusBar status={syncStatus} {notice} onrefresh={startSync} oncancel={cancelSync} />
  </footer>

  {#if pendingCompose}
    <ConfirmPopover anchor={pendingCompose.anchor} align="start" title="Отменить текущее письмо?" text="Набранный текст будет потерян, откроется новое." confirmLabel="Отменить письмо" onconfirm={confirmReplace} oncancel={cancelReplace} />
  {/if}

  {#if closeRequest}
    <ConfirmPopover anchor={closeRequest} align="start" title="Закрыть окно?" text={composerBusy ? "Отправка ещё идёт: письмо может не уйти." : "Набранное письмо будет потеряно."} confirmLabel="Закрыть" onconfirm={confirmClose} oncancel={() => (closeRequest = null)} />
  {/if}

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
    grid-template-columns: calc(var(--nav-width) - var(--space-4)) auto auto 1fr auto;
    align-items: center;
    gap: var(--space-4);
    padding: 0 var(--space-4) 0 var(--space-3);
  }
  .sections {
    justify-self: start;
  }
  .new-mail {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
    padding: 0 12px;
    border: 1px solid var(--accent);
    border-radius: var(--radius-control);
    background: var(--accent);
    color: var(--fg-on-accent);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .new-mail:hover {
    background: var(--accent-hover);
  }
  .new-mail:active {
    background: var(--accent-pressed);
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

  /* Календарь занимает место списка и области чтения; панели другого раздела спрятаны, не размонтированы. */
  .calendar {
    grid-row: 2;
    grid-column: list-start / reader-end;
    min-width: 0;
    background: var(--bg-layer);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card) var(--radius-card) 0 0;
    overflow: hidden;
    margin-right: var(--space-2);
  }
  .folders.off,
  .list.off,
  .splitter.off,
  .reader.off,
  .calendar.off,
  .search.off {
    display: none;
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
