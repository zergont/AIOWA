<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api, errorMessage, formatSize, fullDate, isInlinePicture, isPreviewable, listDate, meetingKindOf, messageNeedsFetch, personLabel, senderResponseOf, type Attachment, type ComposeKind, type Folder, type MeetingInfo, type MeetingResponse, type Message, type ResponseType } from "../api";
  import { COMPOSE_ACTIONS, ICON, type ItemAction } from "../actions";
  import { meetingWhen } from "../calendar";
  import { contactCardOpener } from "../contact";
  import { focusBack } from "../popover";
  import { hint, keysOf } from "../hotkeys";
  import { printMessage } from "../print";
  import { prefs } from "../settings.svelte";
  import { darkMailByDefault, MOON_ICON, SUN_ICON, windowIsDark } from "../darkmail.svelte";
  import FolderPicker from "./FolderPicker.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";
  import ChoicePopover from "./ChoicePopover.svelte";
  import BodyFrame from "./BodyFrame.svelte";
  import AttachmentPreview from "./AttachmentPreview.svelte";

  interface Props {
    message: Message;
    expanded: boolean;
    ontoggle: () => void;
    /** Догрузить письмо с сервера; промис отклоняется текстом ошибки для пользователя. */
    onfetch: (itemPk: number) => Promise<void>;
    /** Папки для «Переместить…». */
    folders: Folder[];
    /** Папка «Удалённые»: письмо в ней удаляется только навсегда. */
    deletedFolderPk: number | null;
    /** Папка архива: письму в ней «В архив» недоступно; null — папки нет (родитель предложит создать). */
    archiveFolderPk?: number | null;
    /** Папка «Черновики»: письмо в ней открывается композером, а не отвечается. */
    draftsFolderPk?: number | null;
    /** Действие над этим письмом; `anchor` — нажатая кнопка (для вопросов вроде «создать папку архива?»); промис отклоняется текстом ошибки. */
    onaction: (action: ItemAction, anchor?: HTMLElement) => Promise<void>;
    /** Открыть композер: ответ, ответ всем или пересылка этого письма. */
    oncompose: (kind: ComposeKind) => void;
    /** Открыть этот черновик в композере. */
    onopendraft?: () => void;
    /** Черновик открывается (может читаться с сервера): кнопка занята. */
    draftOpening?: boolean;
    /** Дан ответ на приглашение или отменённая встреча убрана из календаря: родитель перечитывает беседу. */
    onmeeting?: () => void;
  }
  let { message, expanded, ontoggle, onfetch, folders, deletedFolderPk, archiveFolderPk = null, draftsFolderPk = null, onaction, oncompose, onopendraft, draftOpening = false, onmeeting }: Props = $props();

  /** Черновик: по признаку письма или по папке — у него вместо панели действий «Открыть черновик». */
  const isDraft = $derived(message.is_draft || (draftsFolderPk != null && message.folder_pk === draftsFolderPk));

  const to = $derived(message.recipients.filter((r) => r.kind === 1 && personLabel(r.name, r.email)));
  const cc = $derived(message.recipients.filter((r) => r.kind === 2 && personLabel(r.name, r.email)));
  /** Адреса Reply-To, отличные от отправителя: ответ уйдёт им, карточка это показывает. */
  const replyTo = $derived.by(() => {
    const from = (message.from_email ?? "").trim().toLowerCase();
    return (message.reply_to ?? []).filter((p) => ((p.email ?? "").trim().toLowerCase() !== from || !p.email) && personLabel(p.name, p.email));
  });

  // --- карточка контакта ------------------------------------------------------
  // Имя отправителя и получатели развёрнутого письма открывают карточку контакта (её держит App).
  // У свёрнутого письма заголовок целиком — кнопка «развернуть», имена в нём не кликаются.

  const openContact = contactCardOpener();

  function showContact(e: MouseEvent, name: string | null, email: string | null) {
    // Щелчок по имени не должен сворачивать письмо.
    e.stopPropagation();
    const addr = (email ?? "").trim();
    if (!openContact || !addr) return;
    openContact({ name: name?.trim() || null, email: addr }, e.currentTarget as HTMLElement);
  }
  const inArchive = $derived(archiveFolderPk != null && message.folder_pk === archiveFolderPk);
  // В списке файлов всё, кроме инлайновых картинок: те уже стоят в теле письма.
  const files = $derived(message.attachments.filter((a) => !isInlinePicture(a)));
  const hasBody = $derived(message.body_html != null);
  const inDeleted = $derived(deletedFolderPk != null && message.folder_pk === deletedFolderPk);

  // --- догрузка тела и инлайновых картинок ----------------------------------

  let fetching = $state(false);
  let fetchError = $state<string | null>(null);
  // Автоматический запрос делается один раз на карточку; дальше только по кнопке, чтобы не зациклиться
  // на письме, которое сервер так и не отдаёт.
  let autoFetched = false;
  const needsFetch = $derived(messageNeedsFetch(message));

  async function fetchNow() {
    if (fetching) return;
    fetching = true;
    fetchError = null;
    try {
      await onfetch(message.item_pk);
    } catch (e) {
      fetchError = errorMessage(e);
    } finally {
      fetching = false;
    }
  }

  $effect(() => {
    if (!expanded || !needsFetch) return;
    untrack(() => {
      if (autoFetched) return;
      autoFetched = true;
      void fetchNow();
    });
  });

  // --- действия: прочитано, флаг, перенос, удаление -----------------------------

  let busy = $state(false);
  let actionError = $state<string | null>(null);
  let picker = $state(false);
  let confirmDelete = $state(false);
  let moveBtn = $state<HTMLButtonElement | null>(null);
  let archiveBtn = $state<HTMLButtonElement | null>(null);
  let deleteBtn = $state<HTMLButtonElement | null>(null);

  async function run(action: ItemAction, anchor?: HTMLElement | null) {
    if (busy) return;
    // Ручная пометка отменяет автопометку: иначе таймер снова сделал бы письмо прочитанным.
    if (action.kind === "read") autoMarked = true;
    busy = true;
    actionError = null;
    try {
      await onaction(action, anchor ?? undefined);
    } catch (e) {
      actionError = errorMessage(e);
    } finally {
      busy = false;
    }
  }

  function requestDelete() {
    if (inDeleted) confirmDelete = true;
    else void run({ kind: "delete", permanent: false });
  }
  function confirmDeleteNow() {
    confirmDelete = false;
    focusBack(deleteBtn);
    void run({ kind: "delete", permanent: true });
  }
  function cancelDelete() {
    confirmDelete = false;
    focusBack(deleteBtn);
  }

  function moveTo(f: Folder) {
    closePicker();
    void run({ kind: "move", folderPk: f.folder_pk });
  }
  function closePicker() {
    picker = false;
    focusBack(moveBtn);
  }

  // --- автопометка прочитанным --------------------------------------------------
  // Непрочитанное письмо, развёрнутое и видимое заданное в настройках время (по умолчанию 1,5 с),
  // помечается прочитанным (как в OWA); задержка 0 — автопометка выключена.
  // Делается один раз на карточку: если пользователь вернул «непрочитано» (кнопкой карточки, меню
  // списка или из другого клиента), таймер не повторяется. Идёт «тихо»: отказ (нет связи, нет
  // пароля) только в консоль — это не действие пользователя, а строка состояния уже показывает
  // ту же проблему; локальная пометка на бэкенде откатывается, точка непрочитанного остаётся.

  let cardEl = $state<HTMLElement | null>(null);
  let onScreen = $state(false);
  let autoMarked = false;
  // Нужно именно начальное значение: дальше его ведёт эффект автопометки.
  let lastRead = untrack(() => message.is_read);

  async function autoRead() {
    autoMarked = true;
    busy = true;
    try {
      await onaction({ kind: "read", read: true });
    } catch (e) {
      console.warn("автопометка прочитанным не удалась:", errorMessage(e));
    } finally {
      busy = false;
    }
  }

  $effect(() => {
    const el = cardEl;
    if (!el) return;
    const io = new IntersectionObserver(([en]) => {
      onScreen = en?.isIntersecting ?? false;
    });
    io.observe(el);
    return () => io.disconnect();
  });

  $effect(() => {
    const read = message.is_read;
    // Переход «прочитано → непрочитано» при живой карточке — ручная пометка извне (меню списка,
    // другой клиент): автопометку не повторяем, иначе через пару секунд она отменила бы действие.
    if (lastRead && !read) autoMarked = true;
    lastRead = read;
    // Задержку читаем до выходов: смена настройки перезапускает отсчёт у открытых карточек.
    const delay = prefs.autoReadMs;
    if (!delay || !expanded || !onScreen || read || autoMarked) return;
    const t = setTimeout(() => {
      if (document.visibilityState !== "visible" || autoMarked || busy) return;
      void autoRead();
    }, delay);
    return () => clearTimeout(t);
  });

  // --- вложения ---------------------------------------------------------------

  interface FileStatus {
    busy: "open" | "save" | null;
    error: string | null;
    saved: string | null;
  }
  const IDLE: FileStatus = { busy: null, error: null, saved: null };
  let fileStatus = $state<Record<number, FileStatus>>({});
  // Вложения, скачанные из этой карточки: DTO письма не перечитываем, помечаем локально.
  let cachedHere = $state<Set<number>>(new Set());
  const savedTimers = new Map<number, ReturnType<typeof setTimeout>>();

  function status(pk: number): FileStatus {
    return fileStatus[pk] ?? IDLE;
  }
  function setStatus(pk: number, patch: Partial<FileStatus>) {
    fileStatus[pk] = { ...status(pk), ...patch };
  }
  function markCached(pk: number) {
    if (!cachedHere.has(pk)) cachedHere = new Set([...cachedHere, pk]);
  }

  async function openAttachment(a: Attachment) {
    const pk = a.attachment_pk;
    if (status(pk).busy) return;
    setStatus(pk, { busy: "open", error: null, saved: null });
    try {
      await api.attachmentOpen(pk);
      markCached(pk);
    } catch (e) {
      setStatus(pk, { error: errorMessage(e) });
    } finally {
      setStatus(pk, { busy: null });
    }
  }

  async function saveAttachment(a: Attachment) {
    const pk = a.attachment_pk;
    if (status(pk).busy) return;
    setStatus(pk, { busy: "save", error: null, saved: null });
    try {
      const path = await api.attachmentSaveAs(pk);
      if (path != null) {
        markCached(pk);
        setStatus(pk, { saved: path });
        clearTimeout(savedTimers.get(pk));
        savedTimers.set(
          pk,
          setTimeout(() => setStatus(pk, { saved: null }), 8000),
        );
      }
    } catch (e) {
      setStatus(pk, { error: errorMessage(e) });
    } finally {
      setStatus(pk, { busy: null });
    }
  }

  onDestroy(() => {
    for (const t of savedTimers.values()) clearTimeout(t);
  });

  // --- предпросмотр вложений ----------------------------------------------------
  // Картинки, PDF и текст открываются поверх области чтения; остальные типы — системным приложением.
  // Листаются все файлы письма: для непредпросматриваемого окно предложит «Открыть».

  let previewOpen = $state(false);
  let previewIndex = $state(0);
  let previewReturnTo: HTMLElement | null = null;

  function clickAttachment(e: MouseEvent, a: Attachment) {
    if (!isPreviewable(a)) {
      void openAttachment(a);
      return;
    }
    const i = files.findIndex((f) => f.attachment_pk === a.attachment_pk);
    if (i < 0) return;
    previewReturnTo = e.currentTarget instanceof HTMLElement ? e.currentTarget : null;
    previewIndex = i;
    previewOpen = true;
  }
  function closePreview() {
    previewOpen = false;
    focusBack(previewReturnTo);
    previewReturnTo = null;
  }
  $effect(() => {
    // Письмо перечитано и вложений стало меньше: индекс не должен указывать в пустоту.
    if (previewOpen && previewIndex >= files.length) {
      if (files.length) previewIndex = files.length - 1;
      else previewOpen = false;
    }
  });

  // --- тёмные тона -------------------------------------------------------------
  // В тёмной теме окна письмо по настройке «Вид» перекрашено в тёмные тона или на светлом листе;
  // кнопка с солнцем или луной меняет это для этого письма, пока открыта беседа (карточка живёт
  // до смены беседы). В светлой теме кнопки нет. Печать всегда в исходных цветах на белом.

  /** Выбор кнопкой для этого письма; null — как в настройках. */
  let darkChoice = $state<boolean | null>(null);
  const darkCapable = $derived(Array.isArray(message.dark_colors));
  const darkToggleShown = $derived(windowIsDark() && hasBody && darkCapable);
  const mailDark = $derived(windowIsDark() && darkCapable && (darkChoice ?? darkMailByDefault()));

  // --- печать -----------------------------------------------------------------
  // Печатается шапка и то же очищенное тело, что в карточке; письмо без тела сначала догружается.

  let printing = $state(false);

  async function print() {
    if (printing) return;
    printing = true;
    actionError = null;
    try {
      if (needsFetch) await onfetch(message.item_pk);
      await printMessage(message);
    } catch (e) {
      actionError = errorMessage(e);
    } finally {
      printing = false;
    }
  }

  // --- встреча: приглашение, отмена, ответ участника ----------------------------------------
  // По классу письма карточка знает, что это письмо о встрече, а поля встречи (время, место,
  // организатор, мой ответ) берёт у бэкенда (`meeting_info`, GetItem по письму) при первом
  // развёртывании. Ответ уходит по письму (`meeting_respond`); после — сведения перечитываются.

  const meetingKind = $derived(meetingKindOf(message.item_class));
  let info = $state<MeetingInfo | null>(null);
  let infoLoading = $state(false);
  let infoError = $state<string | null>(null);
  /** Сведения уже запрашивались для этого письма: автозапрос один, дальше — по кнопке. */
  let infoTried = false;
  let meetingBusy = $state(false);
  let meetingError = $state<string | null>(null);
  /** Отменённая встреча убрана из календаря отсюда. */
  let removedFromCalendar = $state(false);
  /** Открыт вопрос «отправить ответ?» для выбранного ответа; якорь — нажатая кнопка. */
  let pendingResponse = $state<{ response: MeetingResponse; anchor: HTMLButtonElement } | null>(null);

  const RESPONSES: { id: MeetingResponse; label: string; question: string }[] = [
    { id: "accept", label: "Принять", question: "Принять приглашение?" },
    { id: "tentative", label: "Под вопросом", question: "Ответить «под вопросом»?" },
    { id: "decline", label: "Отклонить", question: "Отклонить приглашение?" },
  ];

  const MY_RESPONSE: Record<ResponseType, string> = {
    unknown: "Без ответа",
    organizer: "Вы организатор",
    tentative: "Вы ответили «под вопросом»",
    accept: "Вы приняли",
    decline: "Вы отклонили",
    no_response: "Без ответа",
  };

  /** Заголовок баннера; у ответа участника — кто и что ответил. */
  const meetingTitle = $derived.by(() => {
    switch (meetingKind) {
      case "request":
        return "Приглашение на встречу";
      case "cancellation":
        return "Встреча отменена";
      case "response": {
        const who = personLabel(message.from_name, message.from_email) || "Участник";
        const resp = info?.sender_response ?? senderResponseOf(message.item_class);
        const verb = resp === "accept" ? "принял(а) приглашение" : resp === "decline" ? "отклонил(а) приглашение" : resp === "tentative" ? "ответил(а) «под вопросом»" : "ответил(а) на приглашение";
        return `${who} ${verb}`;
      }
      default:
        return "Сообщение о встрече";
    }
  });
  const meetingTime = $derived(info ? meetingWhen(info.start_ms, info.end_ms, info.is_all_day) : null);
  const myResponseClass = $derived(info?.my_response === "accept" || info?.my_response === "organizer" ? "ok" : info?.my_response === "tentative" ? "maybe" : info?.my_response === "decline" ? "no" : "none");

  async function loadInfo() {
    if (infoLoading) return;
    infoLoading = true;
    infoError = null;
    try {
      info = await api.meetingInfo(message.item_pk);
    } catch (e) {
      infoError = errorMessage(e);
    } finally {
      infoLoading = false;
    }
  }

  $effect(() => {
    if (!expanded || !meetingKind) return;
    untrack(() => {
      if (infoTried) return;
      infoTried = true;
      void loadInfo();
    });
  });

  /** Приглашение устарело (организатор прислал новое): ответ на него сервер отвергнет. */
  const outOfDate = $derived(info?.is_out_of_date === true);

  function askRespond(e: MouseEvent, response: MeetingResponse) {
    if (meetingBusy || outOfDate) return;
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
    if (!p || meetingBusy) return;
    meetingBusy = true;
    meetingError = null;
    try {
      await api.meetingRespond(message.item_pk, p.response, sendResponse);
      // Окно отвечает сразу, свежие сведения с сервера придут следом.
      if (info) info = { ...info, my_response: p.response };
      onmeeting?.();
      void loadInfo();
    } catch (e) {
      meetingError = errorMessage(e);
    } finally {
      meetingBusy = false;
    }
  }

  /** «Удалить из календаря» у отмены: связанное событие в «Удалённые». */
  async function removeFromCalendar() {
    const pk = info?.calendar_pk;
    if (pk == null || meetingBusy) return;
    meetingBusy = true;
    meetingError = null;
    try {
      await api.calendarDelete(pk, false);
      removedFromCalendar = true;
      if (info) info = { ...info, calendar_pk: null, has_calendar_item: false };
      onmeeting?.();
    } catch (e) {
      meetingError = errorMessage(e);
    } finally {
      meetingBusy = false;
    }
  }

  /** Получатели строкой: подсказка у строки «Кому», которая обрезается многоточием. */
  function peopleText(list: { name: string | null; email: string | null }[]): string {
    return list.map((p) => personLabel(p.name, p.email)).join(", ");
  }

  function initials(name: string | null, email: string | null): string {
    const s = (name || email || "?").trim();
    const parts = s.split(/\s+/).filter(Boolean);
    return parts.length >= 2 ? (parts[0][0] + parts[1][0]).toUpperCase() : s.slice(0, 2).toUpperCase();
  }
</script>

<!-- Люди через запятую; с адресом — кнопка карточки контакта. В одну строку: иначе лишние пробелы перед запятыми. -->
{#snippet people(list: { name: string | null; email: string | null }[])}
  {#each list as p, i (i)}{#if i}{", "}{/if}{#if openContact && p.email}<button type="button" class="person" onclick={(e) => showContact(e, p.name, p.email)} aria-haspopup="dialog" title={p.name ? `${p.email} — карточка контакта` : "Карточка контакта"}>{personLabel(p.name, p.email)}</button>{:else}{personLabel(p.name, p.email)}{/if}{/each}
{/snippet}

<!-- Переключатель тёмных тонов письма: в панели действий письма и черновика, только в тёмной теме. -->
{#snippet darkToggle()}
  {#if darkToggleShown}
    <button class="act" onclick={() => (darkChoice = !mailDark)} aria-label={mailDark ? "Показать как задумано" : "В тёмных тонах"} title={mailDark ? "Показать как задумано: исходные цвета на светлом листе" : "В тёмных тонах"}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={mailDark ? SUN_ICON : MOON_ICON} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
  {/if}
{/snippet}

<article class="card" class:expanded class:unread={!message.is_read} class:flagged={message.is_flagged} bind:this={cardEl}>
  <div class="head">
    <!-- Клавиши имён (Enter, пробел) не должны сворачивать письмо: реагируем только на сам заголовок. -->
    <div class="head-main" onclick={ontoggle} role="button" tabindex="0" onkeydown={(e) => e.target === e.currentTarget && (e.key === "Enter" || e.key === " ") && ontoggle()} aria-expanded={expanded}>
      <div class="avatar" aria-hidden="true">{initials(message.from_name, message.from_email)}</div>
      <div class="who">
        <div class="from">
          {#if expanded && openContact && message.from_email}
            <button type="button" class="name person" onclick={(e) => showContact(e, message.from_name, message.from_email)} aria-haspopup="dialog" title="Карточка контакта">{personLabel(message.from_name, message.from_email)}</button>
          {:else}
            <span class="name">{personLabel(message.from_name, message.from_email) || "(без отправителя)"}</span>
          {/if}
          {#if message.from_name && message.from_email}<span class="email caption">{message.from_email}</span>{/if}
          {#if message.importance === "High"}<span class="badge high">важное</span>{/if}
          {#if message.is_draft}<span class="badge">черновик</span>{/if}
        </div>
        {#if expanded}
          {#if replyTo.length}<div class="line caption reply-to" title="Ответ уйдёт по адресу из заголовка Reply-To, а не отправителю">Ответ на: {@render people(replyTo)}</div>{/if}
          <div class="line caption" title={to.length ? peopleText(to) : undefined}>Кому: {#if to.length}{@render people(to)}{:else}{message.display_to || "—"}{/if}</div>
          {#if cc.length}<div class="line caption" title={peopleText(cc)}>Копия: {@render people(cc)}</div>{/if}
        {:else}
          <div class="line caption preview">{message.preview || ""}</div>
        {/if}
      </div>
      <div class="when">
        <span class="date-row">
          {#if message.is_flagged}
            <svg class="flag-mark" width="12" height="12" viewBox="0 0 16 16" role="img" aria-label="С флагом"><path d={ICON.flag} fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
          {/if}
          {#if !message.is_read}<span class="unread-dot" role="img" aria-label="Непрочитанное"></span>{/if}
          <span class="date" title={fullDate(message.received_ms ?? message.sent_ms)}>{expanded ? fullDate(message.received_ms ?? message.sent_ms) : listDate(message.received_ms ?? message.sent_ms)}</span>
        </span>
        {#if message.folder_name}<span class="caption folder">{message.folder_name}</span>{/if}
      </div>
    </div>

    {#if expanded && isDraft}
      <div class="actions" role="toolbar" aria-label="Действия с черновиком">
        <button class="act open-draft" onclick={() => onopendraft?.()} disabled={!onopendraft || draftOpening} aria-busy={draftOpening} title={`Открыть черновик в композере (${keysOf("list-open")} в списке)`}>
          {#if draftOpening}
            <span class="spinner" aria-hidden="true"></span>
            Открываю…
          {:else}
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.draft} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            Открыть черновик
          {/if}
        </button>
        {@render darkToggle()}
      </div>
    {:else if expanded}
      <div class="actions" role="toolbar" aria-label="Действия с письмом" aria-busy={busy}>
        {#each COMPOSE_ACTIONS as a (a.kind)}
          <button class="act" onclick={() => oncompose(a.kind)} aria-label={a.label} title="{a.label} ({a.keys})">
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={a.icon} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
        {/each}
        <button class="act" onclick={print} disabled={printing} aria-label="Печать" title={hint("Печать", "print")} aria-busy={printing}>
          {#if printing}
            <span class="spinner" aria-hidden="true"></span>
          {:else}
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.print} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
          {/if}
        </button>
        {@render darkToggle()}
        <span class="act-sep" aria-hidden="true"></span>
        <button class="act" onclick={() => run({ kind: "read", read: !message.is_read })} disabled={busy} aria-label={message.is_read ? "Пометить непрочитанным" : "Пометить прочитанным"} title={message.is_read ? "Пометить непрочитанным" : "Пометить прочитанным"}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={message.is_read ? ICON.unread : ICON.read} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="act" class:on={message.is_flagged} onclick={() => run({ kind: "flag", flagged: !message.is_flagged })} disabled={busy} aria-label={message.is_flagged ? "Снять флаг" : "Поставить флаг"} title={message.is_flagged ? "Снять флаг" : "Поставить флаг"} aria-pressed={message.is_flagged}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.flag} fill={message.is_flagged ? "currentColor" : "none"} stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="act" bind:this={moveBtn} onclick={() => (picker = !picker)} disabled={busy} aria-label="Переместить…" title="Переместить…" aria-haspopup="dialog" aria-expanded={picker}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.move} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="act" bind:this={archiveBtn} onclick={() => run({ kind: "archive" }, archiveBtn)} disabled={busy || inArchive} aria-label="В архив" title={inArchive ? "Письмо уже в архиве" : "В архив"}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.archive} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="act danger" bind:this={deleteBtn} onclick={() => (confirmDelete ? cancelDelete() : requestDelete())} disabled={busy} aria-label={inDeleted ? "Удалить навсегда…" : "Удалить"} title={inDeleted ? "Удалить навсегда…" : "Удалить (в «Удалённые»)"} aria-haspopup={inDeleted ? "dialog" : undefined}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.delete} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        {#if picker && moveBtn}
          <FolderPicker {folders} currentFolderPk={message.folder_pk} anchor={moveBtn} onpick={moveTo} onclose={closePicker} />
        {/if}
        {#if confirmDelete && deleteBtn}
          <ConfirmPopover anchor={deleteBtn} title="Удалить письмо навсегда?" text="Письмо исчезнет с сервера, вернуть его будет нельзя." confirmLabel="Удалить навсегда" onconfirm={confirmDeleteNow} oncancel={cancelDelete} />
        {/if}
      </div>
    {/if}
  </div>

  {#if actionError}
    <div class="note status failed action-error" role="alert">
      <span class="status-text">{actionError}</span>
      <button class="btn" onclick={() => (actionError = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
    </div>
  {/if}

  {#if expanded && meetingKind}
    <section class="meeting kind-{meetingKind}" aria-label="Встреча" aria-busy={meetingBusy || infoLoading}>
      <div class="meeting-head">
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 4h11v9.5h-11V4Zm0 3h11M5 2.5V5m6-2.5V5" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        <span class="meeting-title">{meetingTitle}</span>
        {#if info?.is_out_of_date}<span class="badge">устарело</span>{/if}
        {#if info && meetingKind === "request"}
          <span class="meeting-status {myResponseClass}">
            <span class="dot" aria-hidden="true"></span>
            {MY_RESPONSE[info.my_response]}
          </span>
        {/if}
      </div>
      {#if info}
        <div class="meeting-facts caption">
          {#if meetingTime}<div>{meetingTime}</div>{/if}
          {#if info.location}<div>Где: {info.location}</div>{/if}
          {#if info.organizer}<div>Организатор: {personLabel(info.organizer.name, info.organizer.email) || "—"}</div>{/if}
          {#if info.recurrence_text}<div>{info.recurrence_text}</div>{/if}
        </div>
      {:else if infoLoading}
        <div class="meeting-facts caption"><span class="spinner" aria-hidden="true"></span> Загружаю сведения о встрече…</div>
      {:else if infoError}
        <div class="meeting-msg failed" role="alert">
          <span class="meeting-msg-text">{infoError}</span>
          <button class="btn" onclick={loadInfo}>Повторить</button>
        </div>
      {/if}
      {#if meetingKind === "request" && info?.my_response !== "organizer"}
        <div class="meeting-actions" role="toolbar" aria-label="Ответ на приглашение">
          {#each RESPONSES as r (r.id)}
            <button class="mbtn {r.id}" class:on={info?.my_response === r.id} onclick={(e) => askRespond(e, r.id)} disabled={meetingBusy || infoLoading || outOfDate} aria-pressed={info?.my_response === r.id} aria-haspopup="dialog" aria-expanded={pendingResponse?.response === r.id} title={outOfDate ? "Приглашение устарело" : info?.my_response === r.id ? `Текущий ответ: ${r.label.toLowerCase()}` : r.question}>{r.label}</button>
          {/each}
          {#if meetingBusy}<span class="spinner" aria-hidden="true"></span>{/if}
        </div>
        {#if outOfDate}
          <div class="meeting-facts caption">Приглашение устарело: организатор прислал обновление — ответьте из последнего приглашения или из календаря.</div>
        {/if}
      {:else if meetingKind === "cancellation"}
        <div class="meeting-actions">
          {#if removedFromCalendar}
            <span class="caption">Удалено из календаря</span>
          {:else if info?.calendar_pk != null}
            <button class="mbtn" onclick={removeFromCalendar} disabled={meetingBusy}>Удалить из календаря</button>
            {#if meetingBusy}<span class="spinner" aria-hidden="true"></span>{/if}
          {/if}
        </div>
      {/if}
      {#if meetingError}
        <div class="meeting-msg failed" role="alert">
          <span class="meeting-msg-text">{meetingError}</span>
          <button class="btn" onclick={() => (meetingError = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
        </div>
      {/if}
      {#if pendingResponse}
        <ChoicePopover anchor={pendingResponse.anchor} align="start" title={RESPONSES.find((r) => r.id === pendingResponse?.response)?.question ?? ""} text="Организатор получит письмо с ответом, если его отправить." options={[{ id: "send", label: "Отправить ответ", primary: true }, { id: "nosend", label: "Не отправлять" }]} onpick={(id) => respondNow(id === "send")} oncancel={closeRespond} />
      {/if}
    </section>
  {/if}

  {#if expanded}
    {#if files.length}
      <div class="files" aria-label="Вложения">
        {#each files as a (a.attachment_pk)}
          {@const st = status(a.attachment_pk)}
          {@const cached = a.cached || cachedHere.has(a.attachment_pk)}
          {@const name = a.name ?? "вложение"}
          {@const previewable = isPreviewable(a)}
          <div class="file" class:cached class:busy={st.busy != null} class:failed={st.error != null} title={[a.content_type ?? "", previewable ? "Клик — предпросмотр" : "Клик — открыть в приложении", cached ? "Файл в кэше" : "Файл будет скачан при открытии"].filter(Boolean).join("\n")}>
            <button class="file-open" onclick={(e) => clickAttachment(e, a)} disabled={st.busy != null} aria-label={previewable ? `Предпросмотр вложения ${name}` : `Открыть вложение ${name}`} aria-haspopup={previewable ? "dialog" : undefined} aria-busy={st.busy === "open"}>
              {#if st.busy === "open"}
                <span class="spinner" aria-hidden="true"></span>
              {:else}
                <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 2.5h7l3 3v8H3v-11Zm7 0v3h3" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
              {/if}
              <span class="file-name">{name}</span>
              <span class="caption file-size">{formatSize(a.size)}</span>
            </button>
            <button class="file-save" onclick={() => saveAttachment(a)} disabled={st.busy != null} aria-label="Сохранить как… {name}" title="Сохранить как…" aria-busy={st.busy === "save"}>
              {#if st.busy === "save"}
                <span class="spinner" aria-hidden="true"></span>
              {:else}
                <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2.5v8m0 0 3-3m-3 3-3-3M3 12.5h10" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
              {/if}
            </button>
          </div>
        {/each}
      </div>
      {#each files as a (a.attachment_pk)}
        {@const st = status(a.attachment_pk)}
        {#if st.error}
          <div class="file-msg failed" role="alert">
            <span class="file-msg-text"><span class="file-msg-name">{a.name ?? "вложение"}:</span> {st.error}</span>
            <button class="btn" onclick={() => setStatus(a.attachment_pk, { error: null })} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
          </div>
        {:else if st.saved}
          <div class="file-msg ok" role="status">
            <span class="file-msg-text">Сохранено: <span class="path">{st.saved}</span></span>
          </div>
        {/if}
      {/each}
    {/if}

    <div class="body">
      {#if fetching}
        <div class="note status" aria-live="polite">
          <span class="spinner" aria-hidden="true"></span>
          <span class="status-text">{hasBody ? "Загружаю изображения письма…" : "Загружаю письмо с сервера…"}</span>
        </div>
      {:else if fetchError}
        <div class="note status failed" role="alert">
          <span class="status-text">{fetchError}</span>
          <button class="btn" onclick={fetchNow}>Повторить</button>
        </div>
      {/if}
      {#if hasBody}
        <BodyFrame html={message.body_html ?? ""} dark={mailDark} darkColors={message.dark_colors} darkTag={message.dark_tag} onprint={() => void print()} />
        {#if message.is_truncated}<div class="caption note">Письмо обрезано при загрузке.</div>{/if}
      {:else if message.body_state === 2}
        <div class="note status">
          <span class="status-text caption">Сервер не отдал текст письма.</span>
          {#if !fetching && !fetchError}<button class="btn" onclick={fetchNow}>Запросить ещё раз</button>{/if}
        </div>
      {:else if message.body_state === 1}
        <div class="caption note">Письмо без текста.</div>
      {:else if !fetching && !fetchError}
        <div class="note status">
          <span class="status-text caption">Текст письма ещё не загружен.</span>
          <button class="btn" onclick={fetchNow}>Загрузить</button>
        </div>
      {/if}
      {#if !hasBody && message.preview}<p class="preview-text">{message.preview}</p>{/if}
    </div>
  {/if}
</article>

{#if previewOpen && files.length}
  <AttachmentPreview
    {files}
    bind:index={previewIndex}
    status={status(files[previewIndex]?.attachment_pk ?? -1)}
    onopen={(a) => void openAttachment(a)}
    onsave={(a) => void saveAttachment(a)}
    onclearerror={(a) => setStatus(a.attachment_pk, { error: null })}
    onclose={closePreview}
  />
{/if}

<style>
  .card {
    /* Карточки лежат в прокручиваемой flex-колонке: без flex: none они сжимаются в полоски. */
    flex: none;
    background: var(--bg-card);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-2);
    overflow: hidden;
  }
  /* Непрочитанное: полоска у левого края, без сдвига содержимого. */
  .card.unread {
    box-shadow:
      inset 3px 0 0 var(--unread),
      var(--shadow-2);
  }
  .head {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-3) var(--space-3) var(--space-4);
  }
  .head-main {
    flex: 1;
    min-width: 0;
    display: grid;
    grid-template-columns: 36px 1fr auto;
    gap: var(--space-3);
    align-items: start;
    cursor: default;
    border-radius: var(--radius-control);
  }
  .head-main:focus-visible {
    outline-offset: 2px;
  }
  .card:not(.expanded) .head:hover {
    background: var(--bg-hover);
  }
  .avatar {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    background: var(--accent-soft);
    color: var(--accent);
    font: var(--text-body-strong);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .who {
    min-width: 0;
  }
  .from {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
    flex-wrap: wrap;
  }
  .name {
    font: var(--text-body-strong);
    user-select: text;
  }
  .email {
    user-select: text;
  }
  /* Имя, открывающее карточку контакта: выглядит как текст, по наведению — как ссылка. */
  .person {
    font: inherit;
    color: inherit;
    padding: 0;
    border-radius: 2px;
    max-width: 100%;
    vertical-align: baseline;
  }
  .name.person {
    font: var(--text-body-strong);
  }
  .person:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .person:focus-visible {
    outline-offset: 0;
  }
  .badge {
    font: var(--text-caption);
    border: 1px solid var(--stroke-2);
    border-radius: 9px;
    padding: 0 6px;
    line-height: 16px;
    color: var(--fg-3);
  }
  .badge.high {
    color: var(--danger);
    border-color: var(--danger);
  }
  .line {
    margin-top: 2px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  /* Имя в фокусе с клавиатуры: строка раскрывается целиком — край строки не срезает рамку фокуса,
     и строка не остаётся прокрученной вбок к имени за многоточием (без подписи «Кому:»). */
  .line:has(.person:focus-visible) {
    overflow: visible;
    white-space: normal;
  }
  .reply-to {
    color: var(--fg-2);
  }
  .when {
    text-align: right;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
    white-space: nowrap;
  }
  .date-row {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 16px;
  }
  .date {
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .flag-mark {
    color: var(--danger);
    flex: none;
  }
  .unread-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--unread);
    flex: none;
  }

  /* --- панель действий --- */
  .actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 2px;
    margin-top: -2px;
  }
  .act {
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .act:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .act:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .act:disabled {
    color: var(--fg-disabled);
  }
  .act.on {
    color: var(--danger);
  }
  .act.danger:hover:not(:disabled) {
    color: var(--danger);
  }
  .act:focus-visible {
    outline-offset: -2px;
  }
  /* «Открыть черновик»: единственная кнопка панели, с подписью. */
  .act.open-draft {
    width: auto;
    gap: 6px;
    padding: 0 10px;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--fg-on-accent);
  }
  .act.open-draft:hover:not(:disabled) {
    background: var(--accent-hover);
    color: var(--fg-on-accent);
  }
  .act.open-draft:active:not(:disabled) {
    background: var(--accent-pressed);
  }
  .act.open-draft:disabled {
    opacity: 0.6;
    color: var(--fg-on-accent);
  }
  .act-sep {
    width: 1px;
    height: 16px;
    margin: 0 4px;
    background: var(--stroke-2);
  }
  .action-error {
    border-top: 1px solid var(--stroke-1);
  }

  /* --- встреча: баннер приглашения, отмены, ответа --- */
  .meeting {
    margin: 0 var(--space-4) var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--stroke-1);
    border-left: 3px solid var(--accent);
    border-radius: var(--radius-control);
    background: var(--accent-soft);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .meeting.kind-cancellation {
    border-left-color: var(--danger);
    background: var(--bg-layer);
  }
  .meeting.kind-response {
    border-left-color: var(--fg-4);
    background: var(--bg-layer);
  }
  .meeting-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    color: var(--fg-2);
  }
  .meeting-title {
    font: var(--text-body-strong);
    color: var(--fg-1);
    user-select: text;
  }
  .meeting-status {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .meeting-status .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--fg-4);
  }
  .meeting-status.ok .dot {
    background: var(--success);
  }
  .meeting-status.maybe .dot {
    background: var(--warning);
  }
  .meeting-status.no .dot {
    background: var(--danger);
  }
  .meeting-status.none .dot {
    background: transparent;
    border: 1px solid var(--fg-4);
  }
  .meeting-facts {
    display: flex;
    flex-direction: column;
    gap: 2px;
    user-select: text;
  }
  .meeting-facts > div {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .meeting-actions {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    margin-top: var(--space-1);
  }
  .mbtn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .mbtn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .mbtn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .mbtn:disabled {
    color: var(--fg-disabled);
  }
  .mbtn.on {
    background: var(--accent-soft);
    border-color: var(--accent);
    color: var(--accent);
  }
  .mbtn.decline.on {
    background: transparent;
    border-color: var(--danger);
    color: var(--danger);
  }
  .mbtn.tentative.on {
    background: transparent;
    border-color: var(--warning);
    color: var(--warning);
  }
  .meeting-msg {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font: var(--text-caption);
  }
  .meeting-msg.failed {
    color: var(--danger);
  }
  .meeting-msg-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }

  /* --- вложения --- */
  .files {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    padding: 0 var(--space-4) var(--space-3);
  }
  .file {
    display: inline-flex;
    align-items: stretch;
    height: 28px;
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    color: var(--fg-2);
    max-width: 360px;
    transition: border-color var(--dur-fast) var(--ease-in);
  }
  .file.cached {
    color: var(--fg-1);
  }
  .file.failed {
    border-color: var(--danger);
  }
  .file-open,
  .file-save {
    display: inline-flex;
    align-items: center;
    color: inherit;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .file-open {
    gap: 6px;
    padding: 0 8px 0 10px;
    min-width: 0;
    border-radius: var(--radius-control) 0 0 var(--radius-control);
  }
  .file-save {
    justify-content: center;
    width: 28px;
    color: var(--fg-3);
    border-left: 1px solid var(--stroke-1);
    border-radius: 0 var(--radius-control) var(--radius-control) 0;
  }
  .file-open:hover:not(:disabled),
  .file-save:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .file-open:active:not(:disabled),
  .file-save:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .file-open:disabled,
  .file-save:disabled {
    color: var(--fg-disabled);
  }
  .file-open:focus-visible,
  .file-save:focus-visible {
    outline-offset: -2px;
  }
  .file-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-size {
    flex: none;
  }
  .file-msg {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 0 var(--space-4) var(--space-2);
    font: var(--text-caption);
    color: var(--fg-2);
    min-width: 0;
  }
  .file-msg.failed {
    color: var(--danger);
  }
  .file-msg.ok {
    color: var(--success);
  }
  .file-msg-text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .file-msg-name {
    color: var(--fg-2);
  }

  /* --- тело --- */
  .body {
    border-top: 1px solid var(--stroke-1);
  }
  .status + :global(iframe) {
    border-top: 1px solid var(--stroke-1);
  }
  .note {
    padding: var(--space-3) var(--space-4);
  }
  .status {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    color: var(--fg-2);
  }
  .status.failed {
    color: var(--danger);
  }
  .status-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .preview-text {
    margin: 0;
    padding: 0 var(--space-4) var(--space-4);
    color: var(--fg-2);
    user-select: text;
  }

  /* --- общие элементы --- */
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
    flex: none;
    width: 14px;
    height: 14px;
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
