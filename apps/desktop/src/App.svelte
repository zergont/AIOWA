<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import { api, conversationKeyOf, conversationsText, errorMessage, itemKindOf, messageNeedsFetch, messagesKey, newMailSender, onCalendarChanged, onCloseRequested, onFoldersChanged, onItemsChanged, onNewMail, onNotifyStatus, onReminder, onSyncProgress, onWriteFailed, type AccountInfo, type ComposeContext, type ComposeKind, type ComposeRecipient, type Conversation, type Folder, type ItemsChanged, type Message, type NewMail, type NotifyStatus, type ReminderDto, type SearchHit, type StageAction, type StagedDto, type SyncStatus, type WriteReport } from "./lib/api";
  import { onSyncLive, type SyncLive } from "./lib/api";
  import { ICON, failureText, findArchiveFolder, type DragPayload, type ItemAction } from "./lib/actions";
  import { focusBack, type Anchor } from "./lib/popover";
  import { hint, keysOf, matchHotkey, preventBrowserKeys } from "./lib/hotkeys";
  import { printMessage } from "./lib/print";
  import { mergeRecipients, sameEmail } from "./lib/addresses";
  import { directoryLine, loadDirectoryCaps, provideContactCard, type ContactRef } from "./lib/contact";
  import type { DirectoryPerson, SuggestPerson } from "./lib/api";
  import ContactCard from "./lib/components/ContactCard.svelte";
  import ChoicePopover from "./lib/components/ChoicePopover.svelte";
  import FolderTree from "./lib/components/FolderTree.svelte";
  import ConversationList, { mailActionsApply } from "./lib/components/ConversationList.svelte";
  import ReadingPane from "./lib/components/ReadingPane.svelte";
  import SearchResults from "./lib/components/SearchResults.svelte";
  import SearchSuggest from "./lib/components/SearchSuggest.svelte";
  import StatusBar from "./lib/components/StatusBar.svelte";
  import Logo from "./lib/components/Logo.svelte";
  import Composer, { type CloseReason } from "./lib/components/Composer.svelte";
  import type { LocalDraft } from "./lib/api";
  import ConfirmPopover from "./lib/components/ConfirmPopover.svelte";
  import CalendarView from "./lib/components/CalendarView.svelte";
  import MiniMonth from "./lib/components/MiniMonth.svelte";
  import PeopleView from "./lib/components/PeopleView.svelte";
  import ReminderBanner from "./lib/components/ReminderBanner.svelte";
  import Settings from "./lib/components/Settings.svelte";
  import { addDays, sameDay, startOfDay, viewRange, type CalendarViewKind } from "./lib/calendar";
  import { loadSettings, prefs, syncEveryMs as syncPeriodMs } from "./lib/settings.svelte";
  import { setUpdateGuard } from "./lib/updates.svelte";

  interface Props {
    /**
     * Запустить синхронизацию при старте окна. false — первый старт после мастера со снятой
     * галочкой «Начать синхронизацию сразу»: окно ждёт, пока пользователь запустит её сам.
     */
    autoSync?: boolean;
  }
  let { autoSync = true }: Props = $props();

  let account = $state<AccountInfo | null>(null);
  let folders = $state<Folder[]>([]);
  let selectedFolder = $state<Folder | null>(null);
  let conversations = $state<Conversation[]>([]);
  let hasMore = $state(false);
  let loadingList = $state(false);
  let selected = $state<Conversation | null>(null);
  /**
   * Множественный выбор в списке: все выбранные беседы (включая `selected` — строку-курсор).
   * При одной беседе область чтения показывает её письма, при нескольких — «Выбрано N бесед».
   */
  let selectedPks = $state<Set<number>>(new Set());
  const multiCount = $derived(selectedPks.size > 1 ? selectedPks.size : 0);
  let messages = $state<Message[]>([]);
  let loadingMessages = $state(false);
  let query = $state("");
  let searchHits = $state<SearchHit[] | null>(null);
  let searching = $state(false);
  let error = $state<string | null>(null);
  let listWidth = $state(380);
  let syncStatus = $state<SyncStatus | null>(null);
  /** Канал мгновенных уведомлений; null — бэкенд о нём ещё не сообщал (или команды нет). */
  let notify = $state<NotifyStatus | null>(null);

  // --- разделы: почта и календарь --------------------------------------------------------
  // Панели обоих разделов при переключении не размонтируются, а прячутся: открытая беседа, композер
  // с набранным текстом и прокрутка списка — как и выбранное событие с прокруткой сетки — остаются
  // как были. Календарь создаётся при первом показе (на старте приложения запроса календаря нет);
  // якорный день и вид живут здесь, потому что их меняет и мини-календарь в колонке навигации.

  type Section = "mail" | "calendar" | "people";
  let section = $state<Section>("mail");
  /** Календарь уже показывали: дальше он только прячется. */
  let calendarMounted = $state(false);
  /** Раздел «Люди» уже показывали (docs/obkatka.md п. 8): создаётся при первом показе, дальше прячется. */
  let peopleMounted = $state(false);
  $effect(() => {
    if (section === "calendar") calendarMounted = true;
    if (section === "people") peopleMounted = true;
  });
  /** Якорный день сетки календаря: меняют мини-календарь, стрелки и «Сегодня». */
  let calendarAnchor = $state(startOfDay(new Date()));
  let calendarKind = $state<CalendarViewKind>("week");
  const calendarRange = $derived(viewRange(calendarKind, calendarAnchor));
  /** Выделенное событие в сетке: ставится и из напоминания (клик по теме). */
  let calendarSelectedPk = $state<number | null>(null);
  /** Дни с событиями по загруженным окнам календаря — точки в мини-календаре. */
  let calendarEventDays = $state(new Set<number>());
  /** Сетка календаря: ей App передаёт клавиши раздела (←, →, T, D, W, M, N, Esc). */
  let calendarView = $state<ReturnType<typeof CalendarView> | null>(null);

  // --- напоминания о событиях -------------------------------------------------------------
  // Планировщик бэкенда шлёт событие `reminder` с полным актуальным списком (и когда он опустел);
  // на старте окна список запрашивается. Панель сама прячет просроченные (30 минут после начала),
  // закрытие и откладывание — команды, после которых бэкенд присылает список заново.

  let reminders = $state<ReminderDto[]>([]);

  /** Список из бэкенда целиком заменяет свой: перенесённые, удалённые и закрытые в нём уже отсутствуют. */
  function setReminders(items: ReminderDto[]) {
    reminders = [...items].sort((a, b) => a.start_ms - b.start_ms);
  }

  // Планировщик читает только кэш календаря, а CalendarView обновляет его лишь пока раздел на
  // экране. Чтобы напоминания приходили и тому, кто весь день в почте (встреча поставлена коллегой,
  // приглашение принято из письма), окно [сегодня, +2 суток) перечитывается отсюда раз в 5 минут
  // и по calendar-changed; первый раз — с задержкой, когда синхронизация уже вошла на сервер.
  const NEAR_WINDOW_DAYS = 2;
  const NEAR_REFRESH_MS = 5 * 60_000;
  const NEAR_FIRST_DELAY_MS = 30_000;
  let nearRefreshing = false;

  async function refreshNearWindow() {
    if (nearRefreshing) return;
    nearRefreshing = true;
    try {
      const start = startOfDay(new Date());
      await api.calendarEvents(start.getTime(), addDays(start, NEAR_WINDOW_DAYS).getTime(), true);
    } catch (e) {
      console.warn("calendar near window:", errorMessage(e));
    } finally {
      nearRefreshing = false;
    }
  }

  function dropReminder(calendarPk: number) {
    reminders = reminders.filter((r) => r.calendar_pk !== calendarPk);
  }

  async function dismissReminder(calendarPk: number) {
    dropReminder(calendarPk);
    try {
      await api.reminderDismiss(calendarPk);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  async function snoozeReminder(calendarPk: number, minutes: number) {
    dropReminder(calendarPk);
    try {
      await api.reminderSnooze(calendarPk, minutes);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  async function dismissAllReminders() {
    const pks = reminders.map((r) => r.calendar_pk);
    reminders = [];
    for (const pk of pks) {
      try {
        await api.reminderDismiss(pk);
      } catch (e) {
        error = errorMessage(e);
      }
    }
  }

  /** Клик по теме напоминания: календарь на дне события с открытой карточкой; напоминание закрывается. */
  function openReminder(r: ReminderDto) {
    calendarAnchor = startOfDay(new Date(r.start_ms));
    calendarKind = "day";
    calendarSelectedPk = r.calendar_pk;
    section = "calendar";
    void dismissReminder(r.calendar_pk);
  }
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
  const draftsFolderPk = $derived(folders.find((f) => f.distinguished === "drafts")?.folder_pk ?? null);
  /** Папка архива: служебная archive или «Архив» верхнего уровня; null — предложим создать при первом «В архив». */
  const archiveFolder = $derived(findArchiveFolder(folders));

  // --- композер: новое письмо, ответ, ответ всем, пересылка ------------------------------

  /** Открытая заготовка; новая заготовка — новый экземпляр композера ({#key}). */
  let compose = $state<ComposeContext | null>(null);
  /** Локальные черновики с прошлого запуска или закрытые без связи: полоса внизу предлагает открыть. */
  let unsavedDrafts = $state<LocalDraft[]>([]);
  /** Беседа, над карточками которой стоит композер ответа; у нового письма null. */
  let composeConversationPk = $state<number | null>(null);
  let composerDirty = $state(false);
  /** Композер отправляет или сохраняет черновик: заменять его и закрывать окно сейчас нельзя. */
  let composerBusy = $state<"send" | "draft" | null>(null);
  /** Идёт загрузка заготовки или черновика (черновик может читаться с сервера до минуты): строка состояния это показывает. */
  let composeLoading = $state<"draft" | "compose" | null>(null);
  /** Открытый композер: у него родитель спрашивает, можно ли его закрыть (askClose). */
  let composerRef = $state<ReturnType<typeof Composer> | null>(null);
  /** Письмо отправлено или черновик сохранён во время идущей синхронизации: «Отправленные» и «Черновики» дотянуть сразу после её конца. */
  let pendingComposeSync: number[] = [];
  /** Крестик или Alt+F4 при несохранённом письме: бэкенд не закрыл окно и ждёт подтверждения — якорь попапа; null — не запрошено. */
  let closeRequest = $state<Anchor | null>(null);
  /** Самое верхнее развёрнутое письмо в области чтения (для Ctrl+R и других клавиш). */
  let topExpandedPk = $state<number | null>(null);
  let readerEl = $state<HTMLElement | null>(null);
  /** Короткое уведомление в строке состояния («Отправлено»). */
  let notice = $state<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  const composerAlone = $derived(compose !== null && (compose.kind === "new" || composeConversationPk === null || composeConversationPk !== selected?.conversation_pk || multiCount > 0));

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
   * Открыть композер. Один за раз: если текущий с правками — спросить, прежде чем заменить, тем же
   * выбором, что у «Отменить» в композере (сохранить черновик, закрыть без сохранения, удалить,
   * вернуться). `conversationPk` — беседа исходного письма (композер ответа стоит над её карточками).
   * `to` — адресаты нового письма заранее («Написать письмо» из карточки контакта).
   */
  function openCompose(kind: ComposeKind, itemPk: number | null, conversationPk: number | null, draft = false, to: ComposeRecipient[] = []) {
    // Композер живёт в области чтения почты: из календаря сначала возвращаемся туда.
    section = "mail";
    if (compose && composerBusy) {
      error = busyText();
      return;
    }
    if (compose && composerDirty) {
      // Вопрос — после обновления DOM: из календаря область чтения ещё спрятана.
      void tick().then(async () => {
        const reason = composerRef ? await composerRef.askClose() : "cancel";
        // null — пользователь вернулся к письму; иначе композер уже закрыт (closeCompose).
        if (reason != null) void loadCompose(kind, itemPk, conversationPk, draft, to);
      });
      return;
    }
    void loadCompose(kind, itemPk, conversationPk, draft, to);
  }

  /** Открыть существующий черновик в композере (кнопка на карточке, двойной клик и Enter в «Черновиках»). */
  function openDraft(itemPk: number, conversationPk: number | null) {
    // Этот черновик уже открыт: заменять нечего.
    if (compose?.draft?.item_pk === itemPk) return;
    openCompose("new", itemPk, conversationPk, true);
  }

  /** Черновик беседы из списка «Черновиков»: самое свежее письмо беседы в этой папке. */
  async function openDraftFromConversation(c: Conversation) {
    if (!selectedFolder) return;
    try {
      const pks = await api.conversationItemPks(c.conversation_pk, selectedFolder.folder_pk);
      if (!pks.length) {
        error = "В этой папке нет черновиков беседы";
        return;
      }
      openDraft(pks[0], c.conversation_pk);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Плашка «Вы ответили / переслали»: та же беседа в «Отправленных», где лежит ответ или пересылка. */
  async function findSentOfSelected() {
    const pk = selected?.conversation_pk;
    const sent = folders.find((f) => f.folder_pk === sentFolderPk);
    if (pk == null || !sent) {
      error = "Папка «Отправленные» ещё не синхронизирована";
      return;
    }
    try {
      if (selectedFolder?.folder_pk !== sent.folder_pk) await openFolder(sent);
      await openConversationByPk(pk);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Пока идёт отправка, композер не заменяется: иначе её итог пришёл бы уже в другой композер. */
  function busyText(): string {
    return composerBusy === "send" ? "Подожди: письмо отправляется" : "Подожди: черновик сохраняется";
  }

  // --- локальные черновики (письма, пережившие падение, закрытие окна или отсутствие связи) ---

  async function refreshLocalDrafts() {
    try {
      const list = await api.composeLocalList();
      // Открытый сейчас композер хранит свой черновик сам: в полосе он не нужен.
      unsavedDrafts = list.filter((d) => d.draft_id !== compose?.restore?.draft_id);
    } catch (e) {
      console.warn("compose_local_list:", errorMessage(e));
    }
  }

  /** Открыть локальный черновик в композере: заготовка по виду письма (оригинал удалён — как новое письмо), поверх — сохранённый текст. */
  async function restoreDraft(d: LocalDraft) {
    if (composeLoading || composerBusy) return;
    section = "mail";
    if (compose && composerDirty) {
      await tick();
      const reason = composerRef ? await composerRef.askClose() : "cancel";
      if (reason == null) return;
    }
    composeLoading = "compose";
    try {
      let ctx: ComposeContext;
      try {
        ctx = await api.composeContext(d.kind, d.reference_item_pk);
      } catch (e) {
        console.warn("compose_context для локального черновика:", errorMessage(e));
        ctx = await api.composeContext("new", null);
        // Ответ или пересылка уже на сервере: вид и оригинал остаются, чтобы сохранение не заменило
        // тело с цитатой одним новым текстом, а сказало, что цитату не собрать.
        if (d.draft_server_id && d.kind !== "new") ctx = { ...ctx, kind: d.kind, reference_item_pk: d.reference_item_pk };
      }
      compose = { ...ctx, to: d.to, cc: d.cc, bcc: d.bcc, subject: d.subject, body_html: d.body_html, restore: d };
      composeConversationPk = null;
      composerDirty = true;
      unsavedDrafts = unsavedDrafts.filter((x) => x.draft_id !== d.draft_id);
    } catch (e) {
      error = errorMessage(e);
    } finally {
      composeLoading = null;
    }
  }

  async function dropLocalDraft(d: LocalDraft) {
    try {
      await api.composeLocalDrop(d.draft_id);
      unsavedDrafts = unsavedDrafts.filter((x) => x.draft_id !== d.draft_id);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  async function loadCompose(kind: ComposeKind, itemPk: number | null, conversationPk: number | null, draft = false, to: ComposeRecipient[] = []) {
    if (composeLoading || composerBusy) return;
    composeLoading = draft ? "draft" : "compose";
    try {
      const ctx = draft && itemPk != null ? await api.composeOpenDraft(itemPk) : await api.composeContext(kind, itemPk);
      compose = to.length ? { ...ctx, to: mergeRecipients(ctx.to, to) } : ctx;
      composeConversationPk = kind === "new" ? null : conversationPk;
      composerDirty = false;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      composeLoading = null;
    }
  }

  /** Ответ с карточки письма (кнопки и клавиши): беседа — открытая. */
  function composeFromMessage(itemPk: number, kind: ComposeKind) {
    openCompose(kind, itemPk, selected?.conversation_pk ?? null);
  }

  /**
   * Верхнее развёрнутое письмо для Ctrl+R и Ctrl+Shift+F; null — его нет или это не письмо: на контакт,
   * встречу, задачу и заметку не отвечают и их не пересылают (п. 14 обкатки).
   */
  function replyablePk(): number | null {
    const m = topExpandedPk == null ? undefined : messages.find((x) => x.item_pk === topExpandedPk);
    return m && itemKindOf(m.item_class, false) === "mail" ? m.item_pk : null;
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

  function closeCompose(reason: CloseReason) {
    compose = null;
    composeConversationPk = null;
    composerDirty = false;
    composerBusy = null;
    if (reason === "sent") showNotice("Отправлено");
    else if (reason === "draft") showNotice("Черновик сохранён в «Черновиках» на сервере");
    else if (reason === "discard") showNotice("Черновик удалён");
    else if (reason === "local") {
      showNotice("Нет связи: письмо сохранено в приложении, открыть его можно из полосы внизу или при следующем запуске");
      void refreshLocalDrafts();
    }
    // Отправленное появляется в «Отправленных», а черновик приходит в «Черновики» или уходит из них
    // после синхронизации этих папок.
    if (reason === "sent") syncComposeFolders([sentFolderPk, draftsFolderPk]);
    // Удалённый черновик уходит в «Удалённые»: там он должен появиться.
    else if (reason === "discard") syncComposeFolders([draftsFolderPk, deletedFolderPk]);
    else if (reason !== "cancel") syncComposeFolders([draftsFolderPk]);
  }

  /** Синхронизировать папки после композера. Идущую синхронизацию бэкенд не прерывает и запрос не
   * ставит в очередь — тогда папки дотянем после её конца. */
  function syncComposeFolders(pks: (number | null)[]) {
    const wanted = pks.filter((p): p is number => p != null);
    if (!wanted.length) return;
    if (syncStatus?.running) pendingComposeSync = [...new Set([...pendingComposeSync, ...wanted])];
    else void launchSync(wanted);
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
    // Письмо должно быть на экране: из календаря сначала показать почту; вопрос — после
    // обновления DOM (спрятанная область чтения даёт нулевой прямоугольник).
    section = "mail";
    // Идёт отправка или сохранение: спросить, бросать ли их. Иначе — тот же выбор, что у
    // «Отменить» в композере; окно закрывается, когда композер сохранил или удалил черновик.
    if (composerBusy || !composerRef) {
      void tick().then(() => {
        closeRequest = composerAnchor();
      });
      return;
    }
    const ref = composerRef;
    void tick().then(async () => {
      const reason = await ref.askClose();
      if (reason != null) void confirmClose();
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

  /**
   * Перед установкой обновления (приложение закроется само): несохранённое письмо — тот же выбор,
   * что при закрытии окна (askClose композера); идёт отправка или сохранение — подождать.
   * false — не устанавливать.
   */
  async function beforeUpdateInstall(): Promise<boolean> {
    if (!compose) return true;
    if (composerBusy) {
      error = busyText();
      return false;
    }
    if (!composerDirty) return true;
    section = "mail";
    await tick();
    const ref = composerRef;
    if (!ref) {
      error = "Сохраните или закройте письмо в композере и повторите установку";
      return false;
    }
    return (await ref.askClose()) != null;
  }
  setUpdateGuard(beforeUpdateInstall);

  const PAGE = 100;
  /**
   * Период автосинхронизации при видимом окне — из настроек (по умолчанию 2 минуты). При открытом
   * канале уведомлений изменения приходят от сервера сразу, таймер остаётся страховкой: не чаще
   * раза в 10 минут.
   */
  const syncEveryMs = $derived(syncPeriodMs(notify?.connected ?? false, prefs.syncIntervalMin));
  /** Первый старт без «Начать синхронизацию сразу»: таймер молчит, пока синхронизацию не запустят вручную. */
  let syncHeld = untrack(() => !autoSync);

  async function loadFolders() {
    folders = await api.folders();
    const inbox = folders.find((f) => f.distinguished === "inbox") ?? folders.find((f) => f.is_mail) ?? null;
    if (inbox) await openFolder(inbox);
  }

  async function openFolder(f: Folder) {
    selectedFolder = f;
    selected = null;
    selectedPks = new Set();
    messages = [];
    cancelSearch();
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
    // У строк списка курсор всегда число (у беседы без даты — 0); null бывает только у заглушки из поиска.
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
    const switching = selected?.conversation_pk !== c.conversation_pk || selectedPks.size !== 1;
    selected = c;
    selectedPks = new Set([c.conversation_pk]);
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
    const switching = selected?.conversation_pk !== pk || selectedPks.size !== 1;
    selected = existing ?? { conversation_pk: pk, topic: null, participants: [], item_count: 0, unread_count: 0, total_count: 0, last_received_ms: null, has_attachments: false, has_flagged: false };
    selectedPks = new Set([pk]);
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

  /**
   * Множественный выбор из списка (Ctrl/Shift+клик, Ctrl+A, Shift+стрелки): письма не грузим,
   * область чтения показывает «Выбрано N бесед»; ответ запроса, который ещё в полёте, отбрасывается.
   */
  function onMultiSelection(pks: number[], focus: Conversation) {
    selectedPks = new Set(pks);
    selected = focus;
    messages = [];
    conversationRequest++;
    loadingMessages = false;
  }

  /** После ответа на приглашение с карточки: перечитать открытую беседу (класс и состояние письма меняются на сервере). */
  async function rereadConversation() {
    if (!selected || loadingMessages) return;
    const pk = selected.conversation_pk;
    try {
      const fresh = await api.conversation(pk);
      if (selected?.conversation_pk === pk && !loadingMessages) messages = fresh;
    } catch (e) {
      error = errorMessage(e);
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

  /** Ctrl+P: напечатать письмо (тело догружается, если его ещё нет). */
  async function printMessageByPk(itemPk: number) {
    try {
      let m = messages.find((x) => x.item_pk === itemPk);
      if (m && messageNeedsFetch(m)) {
        await fetchMessage(itemPk);
        m = messages.find((x) => x.item_pk === itemPk);
      }
      if (m) await printMessage(m);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  // --- запись: прочитано, флаг, перенос, архив, удаление -----------------------------------
  // Прочитано и флаг уходят на сервер сразу (items_*). Удаление в «Удалённые», перенос и архив идут
  // через items_stage: локально применяются сразу, сервер ждёт 6 с, пока действует «Отменить».

  /** Папка назначения действия: для архива — папка архива (вызывающий уже убедился, что она есть). */
  function targetFolderPk(a: ItemAction): number | null {
    if (a.kind === "move") return a.folderPk;
    if (a.kind === "archive") return archiveFolder?.folder_pk ?? null;
    if (a.kind === "delete" && !a.permanent) return deletedFolderPk;
    return null;
  }

  function callWrite(itemPks: number[], a: ItemAction): Promise<WriteReport> {
    switch (a.kind) {
      case "read":
        return api.itemsMarkRead(itemPks, a.read);
      case "flag":
        return api.itemsSetFlag(itemPks, a.flagged);
      case "move":
        return api.itemsMove(itemPks, a.folderPk);
      case "archive": {
        const pk = archiveFolder?.folder_pk;
        return pk == null ? Promise.reject("Папки «Архив» нет") : api.itemsMove(itemPks, pk);
      }
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
    const target = targetFolderPk(a);
    for (const m of messages) {
      if (!set.has(m.item_pk)) continue;
      if (a.kind === "read") m.is_read = a.read;
      else if (a.kind === "flag") m.is_flagged = a.flagged;
      else if (target != null) {
        m.folder_pk = target;
        m.folder_name = nameOf(target) ?? m.folder_name;
      }
    }
  }

  /** Оптимистичная правка строк списка для действия над беседами в текущей папке. */
  function applyToRows(cs: Conversation[], a: ItemAction) {
    if (a.kind === "read" || a.kind === "flag") {
      const set = new Set(cs.map((c) => c.conversation_pk));
      for (const row of conversations) {
        if (!set.has(row.conversation_pk)) continue;
        if (a.kind === "read") row.unread_count = a.read ? 0 : row.item_count;
        else row.has_flagged = a.flagged;
      }
    } else dropRows(new Set(cs.map((c) => c.conversation_pk)));
  }

  /**
   * Беседы ушли из текущей папки: убрать строки и снять их с выбора; если среди них была открытая —
   * перейти к первой оставшейся ниже неё (иначе к последней) или снять выбор.
   */
  function dropRows(pks: Set<number>) {
    const old = conversations;
    const rows = old.filter((c) => !pks.has(c.conversation_pk));
    if (rows.length === old.length) return;
    conversations = rows;
    if (selectedPks.size) selectedPks = new Set([...selectedPks].filter((pk) => !pks.has(pk)));
    if (!selected || !pks.has(selected.conversation_pk)) return;
    const idx = old.findIndex((c) => c.conversation_pk === selected!.conversation_pk);
    const next = old.slice(idx + 1).find((c) => !pks.has(c.conversation_pk)) ?? rows[rows.length - 1];
    if (next) void openConversation(next).catch((e) => (error = errorMessage(e)));
    else clearSelection();
  }

  function clearSelection() {
    selected = null;
    selectedPks = new Set();
    messages = [];
  }

  function selectNeighbour(rows: Conversation[], idx: number) {
    const next = rows[Math.min(idx, rows.length - 1)];
    if (next) void openConversation(next).catch((e) => (error = errorMessage(e)));
    else clearSelection();
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
      report = await track(callWrite(itemPks, a));
    } catch (e) {
      await refreshAfterWrite({ conversation_pks: selected ? [selected.conversation_pk] : [], folder_pks: [] });
      throw errorMessage(e);
    }
    const failed = failureText(report);
    if (failed) throw failed;
  }

  // --- окно отмены: удаление, перенос, архив --------------------------------------------------------
  // Бэкенд применяет действие локально и держит откат в памяти до undo_until_ms, потом сам отправляет
  // на сервер. Окно показывает строку «… · Отменить» с убывающей полоской; Ctrl+Z или кнопка — items_undo;
  // новое действие с окном отмены сначала коммитит предыдущее (items_commit). Удаление навсегда и
  // пометки идут без окна отмены.

  /** Что вернуть в выбор после отката: папка и беседы на момент действия. */
  interface Restore {
    folderPk: number | null;
    pks: number[];
    focusPk: number | null;
  }
  interface Staged {
    id: string;
    text: string;
    sinceMs: number;
    untilMs: number;
    restore: Restore;
  }
  let staged = $state<Staged | null>(null);
  let stageTimer: ReturnType<typeof setTimeout> | undefined;
  const UNDO_DEFAULT_MS = 6000;
  /** Строка убирается чуть раньше срока: нажатие в последний миг не должно попадать в уже истёкшее окно. */
  const UNDO_MARGIN_MS = 250;

  /** Действие с окном отмены; null — выполняется сразу (пометки, удаление навсегда). */
  function stageActionOf(a: ItemAction): StageAction | null {
    if (a.kind === "delete") return a.permanent ? null : "delete";
    if (a.kind === "move") return "move";
    if (a.kind === "archive") return "archive";
    return null;
  }

  /** Срок отмены — миллисекунды эпохи; странное значение от бэкенда (0, относительное) заменяется на 6 с. */
  function undoDeadline(untilMs: number, now: number): number {
    const d = untilMs - now;
    return Number.isFinite(d) && d > 300 && d < 120_000 ? untilMs : now + UNDO_DEFAULT_MS;
  }

  function stageText(a: ItemAction, what: string): string {
    if (a.kind === "delete") return `Удалено: ${what}`;
    const pk = targetFolderPk(a);
    const name = folders.find((f) => f.folder_pk === pk)?.name;
    return name ? `Перенесено в «${name}»: ${what}` : `Перенесено: ${what}`;
  }

  function clearStaged() {
    staged = null;
    clearTimeout(stageTimer);
  }

  /** Предыдущее действие уходит на сервер сейчас: окно отмены у него закрывается. */
  function commitStaged() {
    const s = staged;
    if (!s) return;
    clearStaged();
    api
      .itemsCommit(s.id)
      .then((r) => {
        const failed = r ? failureText(r) : null;
        if (failed) error = failed;
      })
      .catch((e) => (error = errorMessage(e)));
  }

  /**
   * Действие с окном отмены: окно и локальная база меняются сразу, сервер — через 6 с.
   * `what` — для строки состояния («3 беседы», «письмо»). Отказ — исключение с текстом.
   */
  async function stageWrite(itemPks: number[], a: ItemAction, action: StageAction, what: string, restore: Restore): Promise<void> {
    if (!itemPks.length) return;
    commitStaged();
    applyToMessages(itemPks, a);
    let r: StagedDto;
    try {
      r = await track(api.itemsStage(action, itemPks, action === "delete" ? null : targetFolderPk(a), false));
    } catch (e) {
      await refreshAfterWrite({ conversation_pks: restore.pks, folder_pks: [] });
      throw errorMessage(e);
    }
    if (r.done > 0) {
      const now = Date.now();
      const until = undoDeadline(r.undo_until_ms, now);
      clearTimeout(stageTimer);
      staged = { id: r.stage_id, text: stageText(a, what), sinceMs: now, untilMs: until, restore };
      stageTimer = setTimeout(() => {
        if (staged?.id === r.stage_id) staged = null;
      }, Math.max(0, until - now - UNDO_MARGIN_MS));
    }
    // Локальная база уже изменена: папки и список перечитываем сразу, не дожидаясь items-changed после отправки.
    void refreshAfterWrite({ conversation_pks: [], folder_pks: [] });
    const failed = failureText(r);
    if (failed) throw failed;
  }

  /** Кнопка «Отменить» или Ctrl+Z: откат на бэкенде, окно перечитывает базу и возвращает выбор. */
  async function undoStaged() {
    const s = staged;
    if (!s) return;
    clearStaged();
    try {
      const report = await track(api.itemsUndo(s.id));
      const failed = failureText(report);
      if (failed) error = failed;
      await refreshAfterWrite({ conversation_pks: s.restore.pks, folder_pks: [] });
      restoreSelection(s.restore);
      showNotice("Отменено");
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** После отката: вернуть выбор беседам, которые снова в списке (одна — открыть, несколько — выделить). */
  function restoreSelection(r: Restore) {
    if (!selectedFolder || selectedFolder.folder_pk !== r.folderPk) return;
    const back = conversations.filter((c) => r.pks.includes(c.conversation_pk));
    if (!back.length) return;
    if (back.length === 1) {
      void openConversation(back[0]).catch((e) => (error = errorMessage(e)));
      return;
    }
    onMultiSelection(
      back.map((c) => c.conversation_pk),
      back.find((c) => c.conversation_pk === r.focusPk) ?? back[0],
    );
  }

  // --- архив ---------------------------------------------------------------------------------------
  // «В архив» — перенос в папку архива. Если её нет, окно спрашивает, создать ли папку «Архив» в корне;
  // после создания действие повторяется.

  let archivePrompt = $state<{ anchor: Anchor; resume: () => Promise<void> } | null>(null);

  function askArchive(anchor: Anchor, resume: () => Promise<void>) {
    archivePrompt = { anchor, resume };
  }

  async function createArchiveAndResume() {
    const p = archivePrompt;
    archivePrompt = null;
    if (!p) return;
    try {
      await api.folderCreate(null, "Архив");
      await reloadFolders();
      if (!archiveFolder) throw "Папка «Архив» не нашлась после создания: обнови папки и повтори";
      await p.resume();
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Действие над одним письмом с карточки; `anchor` — нажатая кнопка (якорь вопроса о папке архива). */
  async function onMessageAction(itemPk: number, a: ItemAction, anchor?: HTMLElement): Promise<void> {
    if (a.kind === "archive" && !archiveFolder) {
      askArchive(anchor ?? composerAnchor(), () => onMessageAction(itemPk, a, anchor));
      return;
    }
    const action = stageActionOf(a);
    if (!action) return runWrite([itemPk], a);
    const restore: Restore = { folderPk: selectedFolder?.folder_pk ?? null, pks: selected ? [selected.conversation_pk] : [], focusPk: selected?.conversation_pk ?? null };
    return stageWrite([itemPk], a, action, "письмо", restore);
  }

  /** Больше бесед за раз не обрабатываем: по каждой — запрос писем, затем одна команда записи. */
  const BULK_LIMIT = 200;

  /**
   * Действие над всеми письмами выбранных бесед в текущей папке (меню списка, панель «Выбрано N»,
   * клавиши Delete и E, бросок на папку). Исключение — «Снять флаг»: признак has_flagged в строке
   * считается по всей беседе, поэтому и снимается флаг со всех её писем, где бы они ни лежали.
   */
  async function onListAction(targets: Conversation[], a: ItemAction, anchor?: Anchor): Promise<void> {
    if (!selectedFolder || !targets.length) return;
    if (targets.length > BULK_LIMIT) throw `Выбрано слишком много бесед (${targets.length}): за раз можно обработать не больше ${BULK_LIMIT}`;
    if (a.kind === "archive" && !archiveFolder) {
      askArchive(anchor ?? composerAnchor(), () => onListAction(targets, a, anchor));
      return;
    }
    const folderPk = a.kind === "flag" && !a.flagged ? null : selectedFolder.folder_pk;
    const lists = await Promise.all(targets.map((c) => api.conversationItemPks(c.conversation_pk, folderPk)));
    const itemPks = [...new Set(lists.flat())];
    if (!itemPks.length) return;
    const restore: Restore = { folderPk: selectedFolder.folder_pk, pks: targets.map((c) => c.conversation_pk), focusPk: selected?.conversation_pk ?? null };
    applyToRows(targets, a);
    const action = stageActionOf(a);
    if (action) await stageWrite(itemPks, a, action, conversationsText(targets.length), restore);
    else await runWrite(itemPks, a);
  }

  /** Беседы брошены на папку дерева: перенос с окном отмены. */
  function dropConversations(target: Folder, payload: DragPayload) {
    if (!selectedFolder || payload.folder_pk !== selectedFolder.folder_pk || target.folder_pk === selectedFolder.folder_pk) return;
    const set = new Set(payload.conversation_pks);
    const targets = conversations.filter((c) => set.has(c.conversation_pk));
    if (!targets.length) return;
    void onListAction(targets, { kind: "move", folderPk: target.folder_pk }).catch((e) => (error = errorMessage(e)));
  }

  // --- папки: создать, переименовать, удалить ---------------------------------------------------
  // Команды меняют локальную таблицу папок и запускают синхронизацию иерархии; событие folders-changed
  // перечитывает дерево. Ошибки (строкой по-русски) дерево показывает у строки.

  async function createFolder(parentPk: number | null, name: string) {
    await api.folderCreate(parentPk, name);
    await reloadFolders();
  }

  async function renameFolder(f: Folder, name: string) {
    await api.folderRename(f.folder_pk, name);
    await reloadFolders();
  }

  async function deleteFolder(f: Folder, permanent: boolean) {
    await api.folderDelete(f.folder_pk, permanent);
    await reloadFolders();
    showNotice(permanent ? `Папка «${f.name}» удалена` : `Папка «${f.name}» перемещена в «Удалённые»`);
  }

  /**
   * Перечитать папки, не сбивая выбранную (объект заменяется свежим со счётчиками). Если выбранная
   * папка исчезла (удалена навсегда, пропала с сервера) — открыть «Входящие».
   */
  /** Когда дерево перечитывалось последний раз (мс): живое обновление не повторяет только что сделанное. */
  let foldersReloadedAt = 0;

  async function reloadFolders() {
    foldersReloadedAt = Date.now();
    const fresh = await api.folders();
    folders = fresh;
    if (selectedFolder) {
      const same = fresh.find((f) => f.folder_pk === selectedFolder!.folder_pk);
      if (same) selectedFolder = same;
      else {
        const inbox = fresh.find((f) => f.distinguished === "inbox") ?? fresh.find((f) => f.is_mail) ?? null;
        if (inbox) await openFolder(inbox);
        else {
          selectedFolder = null;
          conversations = [];
          clearSelection();
        }
      }
    } else if (searchHits === null) {
      // Первый запуск: папки появились по ходу синхронизации — «Входящие» открываются сразу, не по концу прогона.
      const inbox = fresh.find((f) => f.distinguished === "inbox") ?? fresh.find((f) => f.is_mail) ?? null;
      if (inbox) await openFolder(inbox);
    }
  }

  /**
   * Множественный выбор после перечитывания списка: беседы, которых больше нет, уходят из выбора;
   * осталась одна — она открывается, не осталось ни одной — выбор снимается (иначе панель
   * «Выбрано N» показывала бы пустой набор). Живое обновление перед этим дочитывает список до
   * выбранных бесед, сдвинутых новой почтой за конец страницы (`liveReachSelection`).
   */
  function pruneSelection(rows: Conversation[]) {
    if (selectedPks.size <= 1) return;
    const keep = rows.filter((c) => selectedPks.has(c.conversation_pk));
    if (keep.length === selectedPks.size) return;
    if (keep.length <= 1) {
      if (keep[0]) void openConversation(keep[0]).catch((e) => (error = errorMessage(e)));
      else clearSelection();
      return;
    }
    selectedPks = new Set(keep.map((c) => c.conversation_pk));
    if (selected && !selectedPks.has(selected.conversation_pk)) selected = keep[0];
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
        if (selectedPks.size > 1) pruneSelection(rows);
        else {
          const same = rows.find((c) => c.conversation_pk === selected!.conversation_pk);
          if (same) selected = same;
          else if (before >= 0) selectNeighbour(rows, before);
        }
      }
    } catch (e) {
      error = errorMessage(e);
    }
  }

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  /** Номер текущего поиска: ответ устаревшего запроса (поиск очищен, открыта папка, набран новый
   *  текст) отбрасывается, иначе он вернул бы список результатов поверх уже показанного. */
  let searchSeq = 0;

  /** Отменить отложенный и идущий поиск: их ответы больше ничего не покажут. */
  function cancelSearch() {
    clearTimeout(searchTimer);
    searchSeq++;
    searching = false;
  }

  function onQueryInput() {
    cancelSearch();
    const q = query.trim();
    suggestOpen = true;
    updateSuggestions(q);
    if (!q) {
      searchHits = null;
      return;
    }
    const seq = searchSeq;
    searchTimer = setTimeout(async () => {
      searching = true;
      try {
        const hits = await api.search(q, 100);
        if (seq === searchSeq) searchHits = hits;
      } catch (e) {
        if (seq === searchSeq) error = errorMessage(e);
      } finally {
        if (seq === searchSeq) searching = false;
      }
    }, 180);
  }

  function clearSearch() {
    cancelSearch();
    query = "";
    searchHits = null;
    suggestOpen = false;
    resetSuggestions();
  }

  /** Поиск сразу, без паузы ввода: Enter, выбор подсказки, «Искать во всей почте». */
  function runSearchNow() {
    cancelSearch();
    const q = query.trim();
    if (!q) {
      searchHits = null;
      return;
    }
    const seq = searchSeq;
    searching = true;
    api
      .search(q, 100)
      .then((hits) => {
        if (seq === searchSeq) searchHits = hits;
      })
      .catch((e) => {
        if (seq === searchSeq) error = errorMessage(e);
      })
      .finally(() => {
        if (seq === searchSeq) searching = false;
      });
  }

  // --- подсказки поиска (п. 9 обкатки) ----------------------------------------------------
  // Под строкой поиска: раздел «От» — частые корреспонденты из базы (сразу, без сети), затем кэш
  // справочника и FindPeople с паузой; недавние запросы (localStorage); «Искать … во всей почте».
  // Выбор человека ставит «от:адрес» — точное условие по адресу, не поиск по словам.
  // Недавние запросы — личные данные ящика (адреса, имена), а профиль WebView2 у всех учётных записей
  // один: ключ свой у каждого ящика (сервер и имя), ключи других ящиков и общий ключ прежних версий
  // при старте убираются, «Забыть учётную запись» стирает и свой.

  /** Ключ недавних запросов без суффикса ящика — так их хранили прежние версии (общий на все ящики). */
  const RECENT_KEY = "aiowa.search.recent";
  const RECENT_MAX = 8;
  let suggestOpen = $state(false);
  let localPeople = $state<SuggestPerson[]>([]);
  let dirPeople = $state<SuggestPerson[]>([]);
  let suggestLoading = $state(false);
  let recentQueries = $state<string[]>([]);
  /** Ключ недавних запросов открытого ящика; null — ящик ещё не известен или забыт: запросы только в памяти. */
  let recentKey: string | null = null;
  let suggestSeq = 0;
  let suggestTimer: ReturnType<typeof setTimeout> | undefined;
  const suggestPeople = $derived(mergePeople(localPeople, dirPeople));
  const suggestVisible = $derived(suggestOpen && section === "mail" && (suggestPeople.length > 0 || suggestLoading || query.trim() !== "" || recentQueries.length > 0));

  /** Недавние запросы ящика `a` (после account_info); чужие и общий ключ прежних версий убираются. */
  function loadRecentQueries(a: AccountInfo) {
    recentKey = `${RECENT_KEY}:${a.host.trim().toLowerCase()}|${a.user.trim().toLowerCase()}`;
    dropRecentQueries(recentKey);
    try {
      const v = JSON.parse(localStorage.getItem(recentKey) ?? "[]");
      recentQueries = Array.isArray(v) ? v.filter((x): x is string => typeof x === "string").slice(0, RECENT_MAX) : [];
    } catch {
      recentQueries = [];
    }
  }
  /** Убрать из хранилища окна недавние запросы всех ящиков, кроме ключа `keep`. */
  function dropRecentQueries(keep: string | null) {
    try {
      const keys: string[] = [];
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k !== null && k !== keep && (k === RECENT_KEY || k.startsWith(`${RECENT_KEY}:`))) keys.push(k);
      }
      for (const k of keys) localStorage.removeItem(k);
    } catch {
      // Хранилище окна недоступно: в нём и нечего убирать.
    }
  }
  /** «Забыть учётную запись»: недавние запросы ящика стираются вместе с ней. */
  function forgetRecentQueries() {
    recentQueries = [];
    recentKey = null;
    dropRecentQueries(null);
  }
  function rememberQuery(q: string) {
    const t = q.trim();
    if (!t) return;
    recentQueries = [t, ...recentQueries.filter((x) => x !== t)].slice(0, RECENT_MAX);
    if (!recentKey) return;
    try {
      localStorage.setItem(recentKey, JSON.stringify(recentQueries));
    } catch {
      // Хранилище окна недоступно: подсказки живут до перезапуска.
    }
  }
  function mergePeople(base: SuggestPerson[], more: SuggestPerson[]): SuggestPerson[] {
    const seen = new Set(base.map((p) => p.email.toLowerCase()));
    const out = [...base];
    for (const p of more) {
      const k = p.email.toLowerCase();
      if (!seen.has(k)) {
        seen.add(k);
        out.push(p);
      }
    }
    return out.slice(0, 8);
  }
  function toSuggest(p: DirectoryPerson): SuggestPerson | null {
    const email = (p.email ?? "").trim();
    if (!email) return null;
    return { name: p.name?.trim() || null, email, source: "directory", title: directoryLine(p) || null };
  }
  /** Подсказки людей по тексту: без операторов и от двух символов. */
  function updateSuggestions(q: string) {
    clearTimeout(suggestTimer);
    const seq = ++suggestSeq;
    if (q.length < 2 || /(^|\s)[^\s:]+:/.test(q)) {
      localPeople = [];
      dirPeople = [];
      suggestLoading = false;
      return;
    }
    suggestLoading = true;
    api
      .recipientsSuggest(q, 5)
      .then((rows) => {
        if (seq === suggestSeq) localPeople = rows.map((r) => ({ name: r.name, email: r.email, source: "local" as const }));
      })
      .catch(() => {});
    api
      .directorySearch(q, 5, true)
      .then((d) => {
        if (seq === suggestSeq) dirPeople = d.people.map(toSuggest).filter((p): p is SuggestPerson => p != null);
      })
      .catch(() => {});
    suggestTimer = setTimeout(() => {
      api
        .directorySearch(q, 5, false)
        .then((d) => {
          if (seq === suggestSeq) dirPeople = mergePeople(dirPeople, d.people.map(toSuggest).filter((p): p is SuggestPerson => p != null));
        })
        .catch(() => {})
        .finally(() => {
          if (seq === suggestSeq) suggestLoading = false;
        });
    }, 350);
  }
  function pickFrom(p: SuggestPerson) {
    query = `от:${p.email}`;
    finishPick();
  }
  function pickTo(p: SuggestPerson) {
    query = `кому:${p.email}`;
    finishPick();
  }
  function pickRecent(q: string) {
    query = q;
    finishPick();
  }
  function searchAll() {
    finishPick();
  }
  /**
   * Убрать подсказки людей и погасить запросы, что ещё идут (FindPeople с паузой): их ответы после
   * этого не применяются — иначе после Esc в пустом поле всплыли бы люди по стёртому тексту.
   */
  function resetSuggestions() {
    clearTimeout(suggestTimer);
    suggestSeq++;
    localPeople = [];
    dirPeople = [];
    suggestLoading = false;
  }
  function finishPick() {
    rememberQuery(query);
    suggestOpen = false;
    resetSuggestions();
    runSearchNow();
  }
  /**
   * Карточка из подсказок: панель закрывается вместе с кнопкой, поэтому якорь — поле поиска (живой
   * элемент): карточка встаёт под ним, а после закрытия фокус возвращается в поле.
   */
  function suggestCard(p: SuggestPerson, anchor: HTMLElement) {
    suggestOpen = false;
    openContactCard({ name: p.name, email: p.email }, document.getElementById("search-input") ?? anchor);
  }
  /** Фокус в поле поиска: подсказки; карточка, открытая из них (её якорь — само поле), закрывается. */
  function onSearchFocus(e: FocusEvent) {
    suggestOpen = true;
    if (contact && contact.anchor === e.currentTarget) closeContact(false);
  }
  function onSearchKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      searchAll();
    }
  }

  // --- карточка контакта ------------------------------------------------------------------
  // Одна на окно: её открывают имена в карточке письма и чипы адресатов (композер, приглашение)
  // через контекст. Повторное открытие с того же якоря закрывает её; фокус возвращается якорю.

  let contact = $state<{ who: ContactRef; anchor: HTMLElement; blocked: string | null } | null>(null);

  provideContactCard(openContactCard);
  function openContactCard(who: ContactRef, anchor: HTMLElement) {
    if (contact && contact.anchor === anchor && sameEmail(contact.who.email, who.email)) {
      closeContact();
      return;
    }
    // Чип в модальном окне (редактор встречи): «Написать письмо» и «Найти письма» ушли бы в почту,
    // а окно осталось бы открытым за кадром со своими клавишами (Ctrl+Enter — сохранить и разослать).
    const blocked = anchor.closest('[aria-modal="true"]') ? "Сначала закройте окно, из которого открыта карточка" : null;
    contact = { who, anchor, blocked };
  }

  function closeContact(returnFocus = true) {
    const anchor = contact?.anchor;
    contact = null;
    if (returnFocus) focusBack(anchor);
  }

  /** «Написать письмо» из карточки: новое письмо этому адресату (фокус заберёт композер). */
  function composeToContact(to: ComposeRecipient) {
    if (contact?.blocked) return;
    closeContact(false);
    openCompose("new", null, null, false, [to]);
  }

  /** «Найти письма» из карточки: поиск в почте по отправителю. */
  /** Из раздела «Люди»: новое письмо адресатам (всей группе) — карточка контакта при этом не открыта. */
  function composeToPerson(to: ComposeRecipient[]) {
    openCompose("new", null, null, false, to);
  }

  /** Из раздела «Люди»: письма от адреса — поиск в почте. */
  function searchPerson(email: string) {
    section = "mail";
    query = `от:${email}`;
    finishPick();
  }

  function searchContact(email: string) {
    if (contact?.blocked) return;
    closeContact();
    section = "mail";
    query = `от:${email}`;
    finishPick();
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

  /** Все почтовые папки (кнопка «Обновить», «Синхронизировать сейчас», автозапуск, таймер). */
  function startSync() {
    syncHeld = false;
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

  /** Раз в интервал из настроек (при открытом канале уведомлений — не чаще 10 минут) при видимом окне; не запускает повторно, пока идёт предыдущая. */
  function syncTick() {
    if (syncHeld) return;
    if (document.visibilityState !== "visible") return;
    if (syncStatus?.running) return;
    if (Date.now() - lastSyncAttempt < syncEveryMs) return;
    void startSync();
  }

  // --- канал уведомлений: новое письмо, тосты ---------------------------------------------
  // Бэкенд по событиям сервера сам синхронизирует затронутые папки; окно показывает строку
  // «Новое письмо: <отправитель>» на 5 с и по клику ведёт к письму во «Входящих».

  /** Последнее уведомление о новом письме, пока строка на экране. */
  let newMail = $state<NewMail | null>(null);
  let newMailTimer: ReturnType<typeof setTimeout> | undefined;

  function showNewMail(m: NewMail) {
    newMail = m;
    clearTimeout(newMailTimer);
    newMailTimer = setTimeout(() => (newMail = null), 5000);
  }

  /** Тема без префиксов ответа и пересылки, в нижнем регистре: так сравниваются тема письма и тема беседы. */
  function topicKey(s: string | null): string {
    return (s ?? "")
      .replace(/^(\s*(re|fw|fwd|aw|wg|ответ|пересл)\s*:\s*)+/i, "")
      .trim()
      .toLowerCase();
  }

  /**
   * Беседа нового письма среди верхних строк списка папки: по item_pk, если бэкенд его прислал
   * (письма беседы в папке — локальный запрос), иначе по теме. null — письма в базе ещё нет
   * (синхронизация папки по уведомлению только идёт) или оно не в этих строках.
   */
  async function findNewMailConversation(m: NewMail, rows: Conversation[], folderPk: number): Promise<Conversation | null> {
    const itemPk = m.item_pk ?? null;
    if (itemPk != null) {
      for (const c of rows) {
        const pks = await api.conversationItemPks(c.conversation_pk, folderPk);
        if (pks.includes(itemPk)) return c;
      }
      return null;
    }
    const key = topicKey(m.subject);
    if (!key) return null;
    return rows.find((c) => topicKey(c.topic) === key) ?? null;
  }

  /**
   * Письмо из строки «Новое письмо», к которому нужно перейти после конца синхронизации: уведомление
   * приходит раньше, чем бэкенд дотянет папку (пауза 1,5 с и сам прогон), так что клик в первые секунды
   * почти всегда застаёт письмо ещё не в базе. `until` — срок, после которого ждать уже нечего.
   */
  let pendingNewMail: { mail: NewMail; folderPk: number; until: number } | null = null;
  const NEW_MAIL_WAIT_MS = 60_000;

  /** Клик по строке «Новое письмо»: во «Входящие» (или папку из события) и, когда письмо в базе, к его беседе. */
  async function openNewMail() {
    const m = newMail;
    if (!m) return;
    clearTimeout(newMailTimer);
    newMail = null;
    section = "mail";
    const target = folders.find((f) => f.folder_pk === m.folder_pk && f.is_mail) ?? folders.find((f) => f.distinguished === "inbox") ?? null;
    if (!target) return;
    try {
      clearSearch();
      await openFolder(target);
      const found = syncStatus?.running ? false : await locateNewMail(m, target.folder_pk);
      pendingNewMail = found ? null : { mail: m, folderPk: target.folder_pk, until: Date.now() + NEW_MAIL_WAIT_MS };
    } catch (e) {
      error = errorMessage(e);
    }
  }

  /** Найти беседу нового письма в показанном списке и открыть её; false — письма в списке ещё нет. */
  async function locateNewMail(m: NewMail, folderPk: number): Promise<boolean> {
    const c = await findNewMailConversation(m, conversations.slice(0, 20), folderPk);
    if (!c) return false;
    // Пока искали, пользователь мог уйти в другую папку или открыть беседу сам: тогда не вмешиваемся.
    if (selectedFolder?.folder_pk === folderPk && selected === null) await openConversation(c);
    return true;
  }

  /** После конца синхронизации (список уже перечитан): довести отложенный переход к новому письму. */
  async function finishPendingNewMail() {
    const p = pendingNewMail;
    if (!p) return;
    if (Date.now() > p.until || selectedFolder?.folder_pk !== p.folderPk || selected !== null) {
      pendingNewMail = null;
      return;
    }
    try {
      if (await locateNewMail(p.mail, p.folderPk)) pendingNewMail = null;
    } catch (e) {
      pendingNewMail = null;
      error = errorMessage(e);
    }
  }

  /** Переключатель тостов: окно меняется сразу, итог придёт событием notify-status; при отказе — перечитать состояние. */
  async function setToasts(enabled: boolean) {
    const prev = prefs.toasts;
    if (notify) notify = { ...notify, toasts_enabled: enabled };
    prefs.toasts = enabled;
    try {
      await api.notifySetToasts(enabled);
    } catch (e) {
      error = errorMessage(e);
      prefs.toasts = prev;
      try {
        notify = await api.notifyStatus();
      } catch {
        /* команды ещё нет — состояние остаётся как было */
      }
    }
  }

  /** Тосты для переключателя в строке состояния: канал знает точно, до его ответа — настройки. */
  const toastsOn = $derived(notify?.toasts_enabled ?? prefs.toasts);

  // --- окно настроек ----------------------------------------------------------------------

  let settingsOpen = $state(false);
  let settingsBtn = $state<HTMLButtonElement | null>(null);

  /**
   * Куда вернуть фокус после настроек: туда, где он был до открытия (Ctrl+, из редактора письма —
   * обратно в текст, с прежним положением курсора). Открыли кнопкой — там окажется сама шестерёнка.
   */
  let settingsReturn: { el: HTMLElement; range: Range | null } | null = null;

  function openSettings() {
    const a = document.activeElement;
    if (a instanceof HTMLElement && a !== document.body) {
      const sel = window.getSelection();
      const r = a.isContentEditable && sel && sel.rangeCount > 0 ? sel.getRangeAt(0) : null;
      settingsReturn = { el: a, range: r && a.contains(r.commonAncestorContainer) ? r.cloneRange() : null };
    } else settingsReturn = null;
    settingsOpen = true;
  }

  function closeSettings() {
    settingsOpen = false;
    const back = settingsReturn;
    settingsReturn = null;
    void tick().then(() => {
      // Прежний элемент исчез или скрыт — на шестерёнку.
      if (!back || !back.el.isConnected || back.el.getClientRects().length === 0) return focusBack(settingsBtn);
      focusBack(back.el);
      if (back.range && document.activeElement === back.el) {
        const sel = window.getSelection();
        sel?.removeAllRanges();
        sel?.addRange(back.range);
      }
    });
  }

  /** После синхронизации: перечитать папки, текущий список и открытую беседу, не сбивая выбор. */
  async function refreshAfterSync() {
    try {
      await reloadFolders();
      const rows = await reloadList();
      if (rows) pruneSelection(rows);
      if (selected && selectedPks.size <= 1 && !loadingMessages) {
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

  // --- живое окно во время синхронизации -----------------------------------------------------
  // Бэкенд по ходу прогона шлёт sync-live (не чаще раза в 2,5 с): дерево записано, в каких папках
  // записаны письма. Окно мягко перечитывает дерево, открытый список и открытую беседу: выбор,
  // раскрытые письма, прокрутка (её держит список) и множественный выбор остаются, пустые
  // состояния уходят сами. Пока идёт команда записи, загрузка списка или открыто окно отмены,
  // обновление ждёт; при открытом композере беседа под ним не перечитывается. Полное
  // перечитывание — по концу прогона.

  /**
   * Задержка живого обновления: за неё сливаются близкие события, а прогон «только дерево» (после
   * команды с папкой) успевает закончиться и прислать свой folders-changed — тогда дерево по
   * sync-live второй раз не перечитывается.
   */
  const LIVE_DELAY_MS = 150;
  /** Повтор, пока пользователь занят. */
  const LIVE_RETRY_MS = 1000;
  /** Что перечитать при ближайшем живом обновлении. */
  let liveWant: { folders: boolean; pks: Set<number> } = { folders: false, pks: new Set() };
  let liveTimer: ReturnType<typeof setTimeout> | undefined;
  let liveRunning = false;
  /** Команды записи в полёте: пока они не ответили, окно держит оптимистичную правку, и база её ещё не видит. */
  let writesInFlight = 0;

  function track<T>(p: Promise<T>): Promise<T> {
    writesInFlight++;
    return p.finally(() => writesInFlight--);
  }

  function onLive(ev: SyncLive) {
    if (ev.folders) liveWant.folders = true;
    for (const pk of ev.folder_pks) liveWant.pks.add(pk);
    scheduleLive(LIVE_DELAY_MS);
  }

  function scheduleLive(ms: number) {
    if (liveTimer !== undefined) return;
    liveTimer = setTimeout(() => {
      liveTimer = undefined;
      void liveRefresh();
    }, ms);
  }

  /**
   * Пользователь что-то делает: команда записи, загрузка списка, отправка или открытие черновика.
   * Открытое окно отмены — тоже: удаление, перенос или архив ещё не ушли на сервер, и прогон, прочитавший
   * папку до них, вернул бы перенесённую беседу в список до подтверждения переноса.
   */
  function liveBusy(): boolean {
    return liveRunning || writesInFlight > 0 || loadingList || staged !== null || composerBusy !== null || composeLoading !== null;
  }

  /** Сколько страниц живое обновление дочитывает за раз, разыскивая выбранные беседы за концом списка. */
  const LIVE_SELECTION_PAGES = 5;

  /** Беседа a стоит в списке ниже b: свежие первыми, при равном времени выше больший ключ (порядок базы). */
  function sortsBelow(a: Conversation, b: Conversation): boolean {
    const am = a.last_received_ms ?? -Infinity;
    const bm = b.last_received_ms ?? -Infinity;
    return am !== bm ? am < bm : a.conversation_pk < b.conversation_pk;
  }

  /**
   * Живое обновление перечитывает список тем же объёмом, что показан, а новая почта сверху сдвигает
   * строки вниз: выбранные беседы у конца страницы в перечитанное не попадают, хотя из папки не ушли,
   * и pruneSelection сняла бы их с выбора. Пока множественный выбор держит такие беседы (по прежнему
   * месту — ниже последней перечитанной строки), список дочитывается страницами, как прокруткой, — до
   * них, но не больше LIVE_SELECTION_PAGES страниц. Пропавшие выше конца страницы действительно ушли
   * из папки: их снимет pruneSelection. `old` — список до перечитывания. false — список за это время
   * сменился или его грузит кто-то ещё (другая папка, догрузка прокруткой): чистить выбор сейчас нельзя.
   */
  async function liveReachSelection(folderPk: number, old: Conversation[]): Promise<boolean> {
    if (selectedPks.size <= 1 || !hasMore) return true;
    const shown = conversations;
    const have = new Set(shown.map((c) => c.conversation_pk));
    let lost = old.filter((c) => selectedPks.has(c.conversation_pk) && !have.has(c.conversation_pk));
    if (!lost.length) return true;
    if (loadingList) return false;
    // Догрузка прокруткой (loadMore) ждёт, пока список дочитывается здесь: иначе строки задвоились бы.
    loadingList = true;
    const more: Conversation[] = [];
    let last: Conversation | undefined = shown[shown.length - 1];
    let fetched = false;
    let full = true;
    try {
      for (let page = 0; page < LIVE_SELECTION_PAGES; page++) {
        const tail = last;
        if (!tail || tail.last_received_ms == null) {
          full = false;
          break;
        }
        lost = lost.filter((c) => !have.has(c.conversation_pk) && sortsBelow(c, tail));
        if (!lost.length) break;
        const next = await api.conversations(folderPk, { ms: tail.last_received_ms, pk: tail.conversation_pk }, PAGE);
        if (conversations !== shown || selectedFolder?.folder_pk !== folderPk) return false;
        fetched = true;
        for (const c of next) {
          if (have.has(c.conversation_pk)) continue;
          have.add(c.conversation_pk);
          more.push(c);
        }
        last = next[next.length - 1];
        full = next.length === PAGE;
        if (!full) break;
      }
    } finally {
      loadingList = false;
    }
    if (fetched) {
      if (more.length) conversations = [...shown, ...more];
      hasMore = full;
    }
    return true;
  }

  async function liveRefresh() {
    if (!liveWant.folders && liveWant.pks.size === 0) return;
    if (liveBusy()) {
      scheduleLive(LIVE_RETRY_MS);
      return;
    }
    const want = liveWant;
    liveWant = { folders: false, pks: new Set() };
    liveRunning = true;
    try {
      // Во время полного прогона дерево перечитывается здесь: folders-changed он не шлёт. Прогон
      // «только дерево» шлёт его по концу, сразу за этим событием, — тогда дерево уже перечитано.
      if (want.folders && Date.now() - foldersReloadedAt > LIVE_RETRY_MS) await reloadFolders();
      const folderPk = selectedFolder?.folder_pk;
      if (folderPk == null || !want.pks.has(folderPk)) return;
      const old = conversations;
      const rows = await reloadList();
      if (!rows) return;
      // Множественный выбор чистится по списку, дочитанному до выбранных бесед, а не по одной странице.
      // Список сменился по ходу — повтор чуть позже (другая папка его не пройдёт).
      if (!(await liveReachSelection(folderPk, old))) {
        liveWant.pks.add(folderPk);
        scheduleLive(LIVE_RETRY_MS);
        return;
      }
      pruneSelection(conversations);
      if (!selected || selectedPks.size > 1) return;
      const pk = selected.conversation_pk;
      const same = rows.find((c) => c.conversation_pk === pk);
      if (!same) return;
      selected = same;
      // Беседа под композером не трогается; иначе — только при реальных изменениях (дешёвый ключ, как по концу прогона).
      if (compose || loadingMessages) return;
      const keys = await api.conversationKey(pk);
      const withFlag = keys.every((k) => k.is_flagged !== undefined);
      if (conversationKeyOf(keys, withFlag) !== messagesKey(messages, withFlag)) {
        const fresh = await api.conversation(pk);
        if (selected?.conversation_pk === pk && !loadingMessages && !compose) messages = fresh;
      }
    } catch (e) {
      // Живое обновление — не действие пользователя: ошибку не показываем, конец прогона перечитает всё.
      console.warn("sync-live:", errorMessage(e));
    } finally {
      liveRunning = false;
    }
  }

  /**
   * Глобальные клавиши — по таблице lib/hotkeys.ts. matchHotkey не вернёт клавишу, пока на экране
   * слой (модальное окно, вопрос, выбор папки, меню): действия не должны трогать письмо или событие
   * под ним, свои клавиши (Esc, стрелки, Tab) слой ловит сам; перезагрузку (F5, Ctrl+R) и печать
   * окна целиком таблица гасит и тогда. Не вернёт и в поле ввода или редакторе, если клавиша там не
   * действует, и в чужом разделе. Считается только видимый слой: редактор встречи в скрытом разделе
   * календаря (display: none) клавиши почты не держит.
   */
  function onKeydown(e: KeyboardEvent) {
    const id = matchHotkey(e, section);
    switch (id) {
      case null:
        return;
      case "settings":
        openSettings();
        return;
      case "section-mail":
        section = "mail";
        return;
      case "section-calendar":
        showCalendar();
        return;
      case "section-people":
        section = "people";
        return;
      case "search":
        // Поле поиска спрятано в календаре: сначала показать почту, фокус — после обновления DOM.
        section = "mail";
        void tick().then(() => document.getElementById("search-input")?.focus());
        return;
      case "new-mail":
        openCompose("new", null, null);
        return;
      case "undo":
        // Откат последнего удаления, переноса или архива, пока окно отмены открыто; иначе клавиша ничья.
        if (staged) {
          e.preventDefault();
          void undoStaged();
        }
        return;
      case "print":
        // Печатаем развёрнутое письмо; печать окна целиком (со списками и панелями) не нужна нигде.
        if (topExpandedPk != null) void printMessageByPk(topExpandedPk);
        return;
      case "reply":
      case "reply-all": {
        const pk = replyablePk();
        if (pk != null) composeFromMessage(pk, id === "reply" ? "reply" : "reply_all");
        return;
      }
      case "forward": {
        const pk = replyablePk();
        if (pk != null) composeFromMessage(pk, "forward");
        return;
      }
      case "archive": {
        // «В архив» для выбранных бесед; в списке клавишу обрабатывает он сам (и гасит событие).
        // Контакты, встречи и задачи в почтовые папки не переносятся (п. 14 обкатки).
        if (e.defaultPrevented || searchHits !== null) return;
        const targets = (multiCount ? conversations.filter((c) => selectedPks.has(c.conversation_pk)) : selected ? [selected] : []).filter(mailActionsApply);
        if (targets.length && archiveFolder?.folder_pk !== selectedFolder?.folder_pk) {
          e.preventDefault();
          void onListAction(targets, { kind: "archive" }).catch((err) => (error = errorMessage(err)));
        }
        return;
      }
      case "search-clear":
        if (document.activeElement?.id === "search-input") {
          clearSearch();
          (document.activeElement as HTMLElement).blur();
        }
        return;
      case "reload":
      case "print-window":
        // Только гасятся: это сделала сама таблица.
        return;
      default:
        // Клавиши календаря: раздел на экране, слоёв поверх нет.
        if (calendarView?.hotkey(id)) e.preventDefault();
    }
  }

  onMount(() => {
    let unlisten: (() => void) | undefined;
    let unlistenItems: (() => void) | undefined;
    let unlistenClose: (() => void) | undefined;
    let unlistenNotify: (() => void) | undefined;
    let unlistenNewMail: (() => void) | undefined;
    let unlistenReminder: (() => void) | undefined;
    let unlistenCalendar: (() => void) | undefined;
    let unlistenFolders: (() => void) | undefined;
    let unlistenWriteFailed: (() => void) | undefined;
    let unlistenLive: (() => void) | undefined;
    let wasRunning = false;
    // Настройки окна (тема, задержка автопометки, интервал таймера, тосты) — первым делом:
    // тема прошлого запуска уже стоит из localStorage, здесь она сверяется с базой.
    void loadSettings();
    const timer = setInterval(syncTick, 15_000);
    const nearTimer = setInterval(() => void refreshNearWindow(), NEAR_REFRESH_MS);
    const nearFirst = setTimeout(() => void refreshNearWindow(), NEAR_FIRST_DELAY_MS);
    document.addEventListener("visibilitychange", syncTick);

    (async () => {
      try {
        unlistenNotify = await onNotifyStatus((s) => (notify = s));
        unlistenNewMail = await onNewMail(showNewMail);
        unlistenReminder = await onReminder((ev) => setReminders(ev.items));
        unlistenCalendar = await onCalendarChanged(() => void refreshNearWindow());
        // Напоминания, которые пора показать, пока окна не было; команды может не быть в старом бэкенде.
        api.remindersPending()
          .then(setReminders)
          .catch((e) => console.warn("reminders_pending:", errorMessage(e)));
        // Канал запускается после первой удачной синхронизации; до неё команда отвечает «не подключён».
        // Если бэкенд собран без неё, окно остаётся в режиме «по таймеру».
        api.notifyStatus()
          .then((s) => (notify = s))
          .catch((e) => console.warn("notify_status:", errorMessage(e)));
        unlisten = await onSyncProgress((s) => {
          syncStatus = s;
          if (wasRunning && !s.running) {
            // Пауза таймера (интервал из настроек) отсчитывается от конца прогона, а не от его начала:
            // после долгой первой синхронизации следующая не должна стартовать сразу.
            lastSyncAttempt = Date.now();
            void refreshAfterSync().then(finishPendingNewMail);
            if (pendingComposeSync.length) {
              const wanted = pendingComposeSync;
              pendingComposeSync = [];
              void launchSync(wanted);
            }
          }
          wasRunning = s.running;
        });
        unlistenItems = await onItemsChanged((ev) => void refreshAfterWrite(ev));
        unlistenFolders = await onFoldersChanged(() => void reloadFolders().catch((e) => (error = errorMessage(e))));
        unlistenLive = await onSyncLive(onLive);
        // Отказ сервера по действию, окно отмены которого уже закрыто: локально оно откачено,
        // список перечитает items-changed, пользователю нужен только текст.
        unlistenWriteFailed = await onWriteFailed((ev) => (error = ev.message));
        unlistenClose = await onCloseRequested(onCloseRequest);
        account = await api.accountInfo();
        loadRecentQueries(account);
        void loadDirectoryCaps();
        await loadFolders();
        void refreshLocalDrafts();
        syncStatus = await api.syncStatus();
        wasRunning = syncStatus.running;
        // Автозапуск при старте: окно сначала показывает базу, потом тянет новое. Первый старт
        // после мастера со снятой галочкой «Начать синхронизацию сразу» — ждём кнопки «Обновить».
        if (!syncHeld) void startSync();
        else showNotice("Синхронизация не запущена: нажмите «Обновить», когда будете готовы");
      } catch (e) {
        error = errorMessage(e);
      }
    })();

    return () => {
      clearInterval(timer);
      clearInterval(nearTimer);
      clearTimeout(nearFirst);
      clearTimeout(noticeTimer);
      clearTimeout(newMailTimer);
      clearTimeout(stageTimer);
      clearTimeout(liveTimer);
      document.removeEventListener("visibilitychange", syncTick);
      unlisten?.();
      unlistenLive?.();
      unlistenItems?.();
      unlistenFolders?.();
      unlistenWriteFailed?.();
      unlistenClose?.();
      unlistenNotify?.();
      unlistenNewMail?.();
      unlistenReminder?.();
      unlistenCalendar?.();
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

<!-- F5, Ctrl+R, Ctrl+P гасятся в фазе захвата: компонент, остановивший всплытие, их не пропустит. -->
<svelte:window onkeydown={onKeydown} onkeydowncapture={preventBrowserKeys} />

{#snippet composerSnippet()}
  {#key compose}
    {#if compose}
      <Composer bind:this={composerRef} ctx={compose} bind:dirty={composerDirty} bind:busy={composerBusy} onclose={closeCompose} />
    {/if}
  {/key}
{/snippet}

<div class="shell" class:dragging style="--list-width: {listWidth}px">
  <header class="topbar">
    <div class="brand">
      <Logo size={20} />
      <span class="brand-name">AIOWA</span>
    </div>
    <div class="segmented sections" role="tablist" aria-label="Разделы">
      <button role="tab" aria-selected={section === "mail"} class:on={section === "mail"} onclick={() => (section = "mail")} title={hint("Почта", "section-mail")}>Почта</button>
      <button role="tab" aria-selected={section === "calendar"} class:on={section === "calendar"} onclick={showCalendar} title={hint("Календарь", "section-calendar")}>Календарь</button>
      <button role="tab" aria-selected={section === "people"} class:on={section === "people"} onclick={() => (section = "people")} title={hint("Люди", "section-people")}>Люди</button>
    </div>
    <button class="new-mail" onclick={() => openCompose("new", null, null)} title={hint("Новое письмо", "new-mail")}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.compose} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Новое письмо
    </button>
    <div class="search" class:active={query.length > 0} class:off={section !== "mail"}>
      <svg class="search-icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M10.5 10.5 14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      <input id="search-input" type="search" placeholder="Поиск в почте: тема, текст, от:имя, вложение:" bind:value={query} oninput={onQueryInput} onkeydown={onSearchKey} onfocus={onSearchFocus} onblur={() => (suggestOpen = false)} autocomplete="off" spellcheck="false" />
      <kbd class="search-kbd">{keysOf("search").replace("+", " ")}</kbd>
      {#if suggestVisible}
        <SearchSuggest {query} people={suggestPeople} recent={recentQueries} loading={suggestLoading} onfrom={pickFrom} onto={pickTo} oncard={suggestCard} onrecent={pickRecent} onall={searchAll} />
      {/if}
    </div>
    <div class="top-right">
      <div class="account caption">
        {#if account}{account.user}{/if}
      </div>
      <button class="settings-btn" bind:this={settingsBtn} onclick={openSettings} aria-label="Настройки" aria-haspopup="dialog" aria-expanded={settingsOpen} title={hint("Настройки", "settings")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.settings} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
      </button>
    </div>
  </header>

  <nav class="nav" aria-label={section === "mail" ? "Папки" : section === "calendar" ? "Мини-календарь" : "Люди"}>
    <div class="folders" class:off={section !== "mail"}>
      <FolderTree {folders} selected={selectedFolder} onselect={openFolder} onsync={startFolderSync} syncing={syncStatus?.running ?? false} oncreate={createFolder} onrename={renameFolder} ondelete={deleteFolder} ondrop={dropConversations} />
    </div>
    {#if section === "calendar"}
      <MiniMonth selected={calendarAnchor} range={calendarKind === "month" ? null : calendarRange} eventDays={calendarEventDays} onselect={pickCalendarDay} />
    {/if}
  </nav>

  <section class="list" class:off={section !== "mail"} aria-label="Список бесед">
    {#if searchHits !== null}
      <SearchResults hits={searchHits} {searching} {query} onopen={openConversationByPk} onclear={clearSearch} selectedPk={selected?.conversation_pk ?? null} />
    {:else}
      <ConversationList folder={selectedFolder} {conversations} {selected} {selectedPks} loading={loadingList} {hasMore} onselect={openConversation} onselection={onMultiSelection} onmore={loadMore} {folders} archiveFolderPk={archiveFolder?.folder_pk ?? null} onaction={onListAction} oncompose={composeFromConversation} onopendraft={openDraftFromConversation} />
    {/if}
  </section>

  <div class="splitter" class:off={section !== "mail"} role="separator" aria-orientation="vertical" aria-label="Ширина списка" onpointerdown={startDrag}></div>

  <section class="reader" class:off={section !== "mail"} aria-label="Чтение" bind:this={readerEl}>
    <ReadingPane conversation={multiCount ? null : selected} messages={multiCount ? [] : messages} loading={loadingMessages} onfetch={fetchMessage} {folders} {deletedFolderPk} archiveFolderPk={archiveFolder?.folder_pk ?? null} {draftsFolderPk} {multiCount} onaction={onMessageAction} oncompose={composeFromMessage} onopendraft={(itemPk) => openDraft(itemPk, selected?.conversation_pk ?? null)} onfindsent={findSentOfSelected} draftOpening={composeLoading === "draft"} composer={compose ? composerSnippet : undefined} {composerAlone} bind:topExpandedPk onmeeting={() => void rereadConversation()} />
  </section>

  {#if calendarMounted}
    <section class="calendar" class:off={section !== "calendar"} aria-label="Календарь">
      <CalendarView bind:this={calendarView} bind:anchor={calendarAnchor} bind:kind={calendarKind} visible={section === "calendar"} bind:selectedPk={calendarSelectedPk} bind:eventDays={calendarEventDays} />
    </section>
  {/if}

  {#if peopleMounted}
    <section class="calendar people-section" class:off={section !== "people"} aria-label="Люди">
      <PeopleView visible={section === "people"} oncompose={composeToPerson} onsearch={searchPerson} />
    </section>
  {/if}

  <footer class="status" aria-label="Состояние синхронизации">
    <StatusBar status={syncStatus} notice={composeLoading === "draft" ? "Открываю черновик…" : notice} {notify} toastsEnabled={toastsOn} syncEveryMin={Math.round(syncEveryMs / 60_000)} newMail={newMail ? newMailSender(newMail) : null} onrefresh={startSync} oncancel={cancelSync} ontoasts={setToasts} onnewmail={openNewMail} undo={staged} onundo={() => void undoStaged()} />
  </footer>

  {#if settingsOpen}
    <Settings syncing={syncStatus?.running ?? false} onsync={() => void startSync()} unsavedDraft={closeGuard} onforget={forgetRecentQueries} onclose={closeSettings} />
  {/if}

  {#if archivePrompt}
    <ChoicePopover anchor={archivePrompt.anchor} align="start" title="Папки «Архив» нет. Создать?" text="Папка появится в корне ящика, и письма переедут в неё." options={[{ id: "create", label: "Создать папку «Архив»", primary: true }]} onpick={() => void createArchiveAndResume()} oncancel={() => (archivePrompt = null)} />
  {/if}

  <ReminderBanner {reminders} onopen={openReminder} ondismiss={(pk) => void dismissReminder(pk)} onsnooze={(pk, min) => void snoozeReminder(pk, min)} ondismissall={() => void dismissAllReminders()} />

  {#if contact}
    {#key contact}
      <ContactCard who={contact.who} anchor={contact.anchor} actionsBlocked={contact.blocked} onclose={() => closeContact()} oncompose={composeToContact} onsearch={searchContact} />
    {/key}
  {/if}

  {#if closeRequest}
    <ConfirmPopover anchor={closeRequest} align="start" title="Закрыть окно?" text={composerBusy ? "Отправка или сохранение ещё идёт: письмо может не уйти, черновик — не сохраниться." : "Набранное письмо останется в приложении: его предложат открыть при следующем запуске."} confirmLabel="Закрыть" onconfirm={confirmClose} oncancel={() => (closeRequest = null)} />
  {/if}

  {#if unsavedDrafts.length && !compose}
    <div class="restore-bar" role="status">
      <span class="restore-text">Несохранённое письмо{unsavedDrafts.length > 1 ? ` (${unsavedDrafts.length})` : ""}: «{unsavedDrafts[0].subject || "без темы"}»</span>
      <button class="restore-btn" onclick={() => void restoreDraft(unsavedDrafts[0])}>Открыть</button>
      <button class="restore-btn" onclick={() => void dropLocalDraft(unsavedDrafts[0])}>Удалить</button>
    </div>
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
    padding: 0 var(--space-3) 0 var(--space-3);
  }
  .sections {
    justify-self: start;
  }
  /* Учётная запись и шестерёнка — всегда в последней колонке (поиск в календаре спрятан, и без
     явной колонки блок съехал бы влево). */
  .top-right {
    grid-column: 5;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .account {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .settings-btn {
    flex: none;
    width: var(--control-height);
    height: var(--control-height);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-2);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .settings-btn:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .settings-btn:active {
    background: var(--bg-pressed);
  }
  .settings-btn[aria-expanded="true"] {
    background: var(--bg-selected);
    color: var(--fg-1);
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

  .restore-bar {
    position: fixed;
    left: 50%;
    bottom: 36px;
    transform: translateX(-50%);
    background: var(--bg-card);
    color: var(--fg-1);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
    padding: var(--space-2) var(--space-3);
    display: flex;
    gap: var(--space-3);
    align-items: center;
    max-width: 70vw;
    z-index: 5;
  }
  .restore-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .restore-btn {
    color: var(--accent);
    white-space: nowrap;
  }
  .restore-btn:hover {
    text-decoration: underline;
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
