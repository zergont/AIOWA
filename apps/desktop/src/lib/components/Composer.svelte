<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, errorMessage, formatSize, fullDate, isInlinePicture, messageNeedsFetch, personLabel, type ComposeContext, type ComposeRecipient, type ComposeRequest, type DraftAttachment, type DraftRef, type Message, type PickedFile, type SignatureDto } from "../api";
  import { COMPOSE_TITLE, ICON } from "../actions";
  import { isValidEmail, mergeRecipients } from "../addresses";
  import { cleanDraftHtml, htmlToText, restoreDraftHtml } from "../html";
  import { focusBack } from "../popover";
  import RecipientsInput from "./RecipientsInput.svelte";
  import Editor from "./Editor.svelte";
  import BodyFrame from "./BodyFrame.svelte";
  import ChoicePopover from "./ChoicePopover.svelte";
  import SignatureDialog from "./SignatureDialog.svelte";

  /** Почему композер закрылся: отправлено, сохранён черновик, черновик удалён, просто отменено. */
  export type CloseReason = "sent" | "draft" | "discard" | "cancel";

  interface Props {
    /** Заготовка от бэкенда; новая заготовка — новый экземпляр композера (родитель оборачивает в {#key}). */
    ctx: ComposeContext;
    /** Есть ли несохранённые правки (родитель спрашивает подтверждение, прежде чем открыть другое письмо). */
    dirty?: boolean;
    /** Идёт отправка или сохранение черновика: родитель не заменяет композер и не даёт закрыть окно. */
    busy?: "send" | "draft" | null;
    onclose: (reason: CloseReason) => void;
  }
  let { ctx, dirty = $bindable(false), busy = $bindable(null), onclose }: Props = $props();

  // Экземпляр мог быть уничтожен, пока шёл запрос: его итог тогда никому не нужен, и уж точно
  // нельзя звать onclose — родитель закрыл бы уже другой композер.
  let alive = true;
  onDestroy(() => {
    alive = false;
    settleClose(null);
  });

  // --- закрытие по просьбе родителя ------------------------------------------------------
  // Родитель (замена композера другим письмом, закрытие окна) спрашивает через askClose: тот же
  // выбор, что у «Отменить» (сохранить, закрыть без сохранения, удалить, вернуться). Промис
  // разрешается причиной закрытия или null, если пользователь вернулся к письму.

  let closeWaiter: ((reason: CloseReason | null) => void) | null = null;

  function settleClose(reason: CloseReason | null) {
    const w = closeWaiter;
    closeWaiter = null;
    w?.(reason);
  }

  /** Закрыть композер с причиной и сообщить её тому, кто ждёт askClose. */
  function finish(reason: CloseReason) {
    onclose(reason);
    settleClose(reason);
  }

  /** Закрыть композер с вопросом, если есть правки; null — пользователь вернулся к письму (или идёт отправка). */
  export function askClose(): Promise<CloseReason | null> {
    if (busy) return Promise.resolve(null);
    if (!isDirty) {
      finish("cancel");
      return Promise.resolve("cancel");
    }
    settleClose(null);
    root?.scrollIntoView({ block: "nearest" });
    cancelAsk = true;
    return new Promise((resolve) => (closeWaiter = resolve));
  }

  const uid = $props.id();
  const title = $derived(ctx.draft ? "Черновик" : COMPOSE_TITLE[ctx.kind]);

  /** Лимиты вложений те же, что на бэкенде: 25 МиБ на файл, 35 МиБ всего. */
  const FILE_MAX = 25 * 1024 * 1024;
  const TOTAL_MAX = 35 * 1024 * 1024;

  // --- состояние письма (из заготовки, дальше живёт своей жизнью) -------------------------

  /**
   * Тело открытого черновика, если бэкенд его прислал (в заготовке или в сведениях о черновике).
   * Картинки `cid:` показываются из кэша, исходные `src` и стили спрятаны до сохранения (restoreDraftHtml).
   */
  function draftBody(c: ComposeContext): string | null {
    const html = c.body_html ?? c.draft?.body_html ?? null;
    return html == null ? null : cleanDraftHtml(html, c.draft?.inline_images ?? {}) || "<div><br></div>";
  }

  /**
   * Начальный текст: у черновика — его тело, иначе пустая строка под каретку и, если включено,
   * подпись через пустую строку (как в OWA).
   */
  function initialBody(c: ComposeContext): string {
    const saved = draftBody(c);
    if (saved != null) return saved;
    const s = c.signature.auto_add ? c.signature.html.trim() : "";
    return s ? `<div><br></div><div><br></div>${s}` : "<div><br></div>";
  }

  // Без дублей: чипы идут в keyed each по адресу.
  let to = $state<ComposeRecipient[]>(untrack(() => mergeRecipients([], ctx.to)));
  let cc = $state<ComposeRecipient[]>(untrack(() => mergeRecipients([], ctx.cc)));
  let bcc = $state<ComposeRecipient[]>(untrack(() => mergeRecipients([], ctx.bcc ?? ctx.draft?.bcc ?? [])));
  let showBcc = $state(untrack(() => bcc.length > 0));
  let subject = $state(untrack(() => ctx.subject));
  const startBody = untrack(() => initialBody(ctx));
  let body = $state(startBody);
  let attachments = $state<PickedFile[]>([]);
  let signature = $state<SignatureDto>(untrack(() => ctx.signature));
  let original = $state<Message | null>(untrack(() => ctx.original));

  // --- черновик на сервере -----------------------------------------------------------------
  // Открытый черновик приходит с серверным id и вложениями; новое письмо получает id после первого
  // автосохранения. Отправка идёт по этому же черновику (draft_server_id), чтобы не плодить копии.

  let draftRef = $state<DraftRef | null>(untrack(() => (ctx.draft ? { server_id: ctx.draft.server_id, change_key: ctx.draft.change_key ?? null, item_pk: ctx.draft.item_pk } : null)));
  /** Вложения черновика, уже лежащие на сервере. */
  let serverFiles = $state<DraftAttachment[]>(untrack(() => ctx.draft?.attachments ?? []));
  /** Серверные вложения, убранные крестиком: уйдут в removed_attachment_ids при сохранении или отправке. */
  let removedServerIds = $state<string[]>([]);
  const keptServerFiles = $derived(serverFiles.filter((f) => !removedServerIds.includes(f.server_id)));
  /** Список вложений черновика известен с сервера (открытый черновик или после сохранения с файлами). */
  let serverListed = $state(untrack(() => ctx.draft != null));
  /** Есть изменения вложений, которых ещё нет на сервере (автосохранение их не пишет). */
  const pendingFiles = $derived(attachments.length > 0 || removedServerIds.length > 0);
  /**
   * Вложения черновика на сервере после сохранения или неудачной отправки: серверный список
   * заменяет свой, загруженные файлы (то же имя без учёта регистра и размер, как сверяет бэкенд)
   * перестают быть локальными, уже удалённые пропадают из убранных.
   */
  function applyServerFiles(list: DraftAttachment[]) {
    const ids = new Set(list.map((f) => f.server_id));
    const stillRemoved = removedServerIds.filter((id) => ids.has(id));
    const uploaded = (a: PickedFile) => list.some((f) => !stillRemoved.includes(f.server_id) && f.size === a.size && (f.name ?? "").toLowerCase() === a.name.toLowerCase());
    serverFiles = list;
    removedServerIds = stillRemoved;
    attachments = attachments.filter((a) => !uploaded(a));
    serverListed = true;
  }

  let error = $state<string | null>(null);
  let lastAction = $state<"send" | "draft" | null>(null);
  let cancelAsk = $state(false);
  let sigOpen = $state(false);
  let showOriginal = $state(true);
  let origLoading = $state(false);
  let origError = $state<string | null>(null);

  let root = $state<HTMLElement | null>(null);
  let cancelBtn = $state<HTMLButtonElement | null>(null);
  let sigBtn = $state<HTMLButtonElement | null>(null);
  let editor = $state<ReturnType<typeof Editor> | null>(null);
  let toField = $state<ReturnType<typeof RecipientsInput> | null>(null);
  let ccField = $state<ReturnType<typeof RecipientsInput> | null>(null);
  let bccField = $state<ReturnType<typeof RecipientsInput> | null>(null);

  const keyOf = (rs: ComposeRecipient[]) => rs.map((r) => r.email.trim().toLowerCase()).join(",");
  /** Разметка в том виде, как её сериализует браузер: правка одного форматирования (жирный, список) тоже правка. */
  const normHtml = (h: string) => new DOMParser().parseFromString(h, "text/html").body.innerHTML;
  const init = untrack(() => ({ subject: ctx.subject, bodyText: htmlToText(startBody), bodyHtml: normHtml(startBody), to: keyOf(to), cc: keyOf(cc), bcc: keyOf(bcc) }));
  const isDirty = $derived(subject !== init.subject || keyOf(to) !== init.to || keyOf(cc) !== init.cc || keyOf(bcc) !== init.bcc || attachments.length > 0 || removedServerIds.length > 0 || htmlToText(body) !== init.bodyText || (body !== startBody && normHtml(body) !== init.bodyHtml));
  $effect(() => {
    dirty = isDirty;
  });

  /** Файлы исходного письма при пересылке: их добавит сервер, здесь только показываем (пока список с сервера не известен — там они уже есть). */
  const originalFiles = $derived(ctx.kind === "forward" && original && !serverListed ? original.attachments.filter((a) => !isInlinePicture(a)) : []);
  const originalTo = $derived(original ? original.recipients.filter((r) => r.kind === 1).map((r) => personLabel(r.name, r.email)) : []);
  /**
   * «Ответить всем», а получатели исходного письма в базе неизвестны: они приходят только вместе с
   * телом, и пока оно не загружено (или сервер его не отдал), в заготовке один отправитель.
   */
  const recipientsUnknown = $derived(ctx.kind === "reply_all" && original != null && original.body_state !== 1 && original.recipients.length === 0 && !origLoading);

  onMount(() => {
    root?.scrollIntoView({ block: "start" });
    // Ответу и черновику с адресатом нужен текст, остальным — адресат.
    if (ctx.kind === "reply" || ctx.kind === "reply_all" || (ctx.draft && to.length)) editor?.focus();
    else document.getElementById(`${uid}-to`)?.focus();
    void loadOriginal();
  });

  /** Исходное письмо без тела в базе: догрузить, как это делает карточка. */
  async function loadOriginal() {
    const o = original;
    if (!o || !messageNeedsFetch(o)) return;
    origLoading = true;
    origError = null;
    try {
      const fresh = await api.messageFetch(o.item_pk);
      if (!alive) return;
      original = fresh;
      await refreshRecipients();
    } catch (e) {
      if (!alive) return;
      origError = errorMessage(e);
    } finally {
      if (alive) origLoading = false;
    }
  }

  /**
   * Получатели «ответить всем» считаются по таблице получателей, которая заполняется только вместе
   * с телом письма: у письма без тела заготовка знала лишь отправителя. После догрузки заготовка
   * запрашивается заново и подставляется, если пользователь ещё не трогал «Кому» и «Копию».
   */
  async function refreshRecipients() {
    if (ctx.kind !== "reply_all" || ctx.reference_item_pk == null || ctx.draft) return;
    const untouched = () => keyOf(to) === init.to && keyOf(cc) === init.cc;
    if (!untouched()) return;
    const fresh = await api.composeContext(ctx.kind, ctx.reference_item_pk);
    if (!alive || !untouched()) return;
    to = mergeRecipients([], fresh.to);
    cc = mergeRecipients([], fresh.cc);
    // Подстановка заготовки — не правка пользователя.
    init.to = keyOf(to);
    init.cc = keyOf(cc);
  }

  // --- проверки и запрос -----------------------------------------------------------------

  function invalidOf(...lists: ComposeRecipient[][]): string | null {
    for (const l of lists) for (const r of l) if (!isValidEmail(r.email)) return r.email;
    return null;
  }

  /** Текст отказа до обращения к серверу; null — можно отправлять. Недонабранные адреса сначала становятся чипами. */
  function validate(forSend: boolean): string | null {
    toField?.commit();
    ccField?.commit();
    bccField?.commit();
    const bad = invalidOf(to, cc, bcc);
    if (bad) return `Неверный адрес: ${bad}`;
    if (forSend && to.length + cc.length + bcc.length === 0) return "Укажи хотя бы одного получателя.";
    let total = 0;
    for (const a of attachments) {
      if (a.size > FILE_MAX) return `Файл «${a.name}» больше 25 МБ.`;
      total += a.size;
    }
    if (total > TOTAL_MAX) return "Общий размер вложений больше 35 МБ.";
    return null;
  }

  function clean(rs: ComposeRecipient[]): ComposeRecipient[] {
    return rs.map((r) => ({ name: r.name?.trim() || null, email: r.email.trim() }));
  }

  function request(): ComposeRequest {
    return {
      kind: ctx.kind,
      reference_item_pk: ctx.reference_item_pk,
      to: clean(to),
      cc: clean(cc),
      bcc: clean(bcc),
      subject: subject.trim(),
      // Картинкам и стилям черновика возвращаются исходные src/style (cid:, внешние адреса).
      body_html: restoreDraftHtml(body),
      attachments: attachments.map((a) => a.path),
      draft_server_id: draftRef?.server_id ?? null,
      removed_attachment_ids: removedServerIds,
    };
  }

  /**
   * Запрос автосохранения: только текст, тема и получатели. Недонабранное в полях не трогается
   * (commit сорвал бы набор адреса), а чипы с неверным адресом не уходят — сервер бы их отверг.
   * Файлы черновика автосохранение не меняет.
   */
  function draftRequest(): ComposeRequest {
    const valid = (rs: ComposeRecipient[]) => clean(rs).filter((r) => isValidEmail(r.email));
    return { ...request(), to: valid(to), cc: valid(cc), bcc: valid(bcc), attachments: [], removed_attachment_ids: [] };
  }

  /** Запрос ручного сохранения: как у автосохранения, но с новыми файлами и убранными вложениями. */
  function saveRequest(): ComposeRequest {
    return { ...draftRequest(), attachments: attachments.map((a) => a.path), removed_attachment_ids: removedServerIds };
  }

  // --- автосохранение черновика ------------------------------------------------------------
  // Через 30 с после последней правки и не чаще раза в 60 с, пока письмо непустое и отличается от
  // сохранённого. Первый раз создаёт черновик на сервере, дальше обновляет его. Отказ показывается
  // в подвале и не повторяется до следующей правки. Отправка и ручное сохранение ждут идущее
  // автосохранение, чтобы работать с тем же черновиком. Вложения пишет только ручное сохранение
  // (saveNow(true)) и отправка.

  const AUTOSAVE_DELAY_MS = 30_000;
  const AUTOSAVE_GAP_MS = 60_000;
  const timeOnly = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });

  /** Что именно сохраняется: тема, получатели и разметка тела (форматирование тоже правка). */
  const snapshot = $derived(JSON.stringify([subject.trim(), keyOf(to), keyOf(cc), keyOf(bcc), body]));
  const nonEmpty = $derived(subject.trim() !== "" || to.length + cc.length + bcc.length > 0 || htmlToText(body) !== "");
  /** Снимок, который лежит на сервере; у открытого черновика — его начальное состояние. */
  let lastSavedKey = $state<string | null>(untrack(() => (ctx.draft ? snapshot : null)));
  /** Снимок, на котором автосохранение отказало: повтор только после правки. */
  let failedKey = $state<string | null>(null);
  let lastSaveAt = 0;
  let autosaving = $state(false);
  let autosaveError = $state<string | null>(null);
  let savedAt = $state<number | null>(null);
  let autosavePromise: Promise<void> | null = null;

  $effect(() => {
    const key = snapshot;
    if (busy || autosaving || !isDirty || !nonEmpty || key === lastSavedKey || key === failedKey) return;
    const wait = Math.max(AUTOSAVE_DELAY_MS, lastSaveAt + AUTOSAVE_GAP_MS - Date.now());
    const t = setTimeout(autosave, wait);
    return () => clearTimeout(t);
  });

  /**
   * Сохранить текущее состояние на сервере, если оно отличается от сохранённого (или черновика ещё
   * нет). `withFiles` — ручное сохранение: новые файлы прикладываются, убранные вложения удаляются.
   * Ошибку пробрасывает; текст сохранён, а с вложениями не вышло — возвращает причину.
   */
  async function saveNow(withFiles = false): Promise<string | null> {
    const key = snapshot;
    const files = withFiles && pendingFiles;
    if (draftRef && key === lastSavedKey && !files) return null;
    autosaving = true;
    autosaveError = null;
    lastSaveAt = Date.now();
    try {
      if (files) {
        const rep = await api.composeSaveDraft(saveRequest());
        if (!alive) return null;
        if (rep.draft) draftRef = rep.draft;
        if (rep.attachments) applyServerFiles(rep.attachments);
        lastSavedKey = key;
        failedKey = null;
        savedAt = Date.now();
        return rep.message;
      }
      const ref = await api.composeAutosave(draftRequest(), draftRef?.server_id ?? null);
      if (!alive) return null;
      draftRef = ref;
      lastSavedKey = key;
      failedKey = null;
      savedAt = Date.now();
      return null;
    } finally {
      if (alive) autosaving = false;
    }
  }

  function autosave() {
    if (autosavePromise || busy || !alive) return;
    const key = snapshot;
    autosavePromise = saveNow()
      .then(() => undefined)
      .catch((e) => {
        if (!alive) return;
        autosaveError = errorMessage(e);
        failedKey = key;
      })
      .finally(() => {
        autosavePromise = null;
      });
  }

  // --- отправка, сохранение, отмена -------------------------------------------------------

  async function send() {
    if (busy) return;
    const problem = validate(true);
    if (problem) {
      error = problem;
      lastAction = null;
      return;
    }
    busy = "send";
    error = null;
    lastAction = "send";
    try {
      // Идущее автосохранение должно закончиться: отправка пойдёт по его черновику.
      await autosavePromise;
      const key = snapshot;
      const rep = await api.composeSend(request());
      if (!alive) return;
      if (rep.sent) {
        finish("sent");
        return;
      }
      // Черновик остался на сервере: дальше (автосохранение, «Повторить», «Сохранить») — по нему,
      // а не новым CreateItem. Его состояние — то, что ушло в этой попытке.
      if (rep.draft) {
        draftRef = rep.draft;
        lastSavedKey = key;
      }
      if (rep.attachments) applyServerFiles(rep.attachments);
      // Исход SendItem неизвестен: письмо могло уйти, автосохранение до следующей правки не пишет в черновик.
      if (rep.maybe_sent) failedKey = key;
      error = rep.message ?? (rep.draft_saved ? "Письмо не отправлено, черновик сохранён на сервере в «Черновиках»" : "Письмо не отправлено.");
      // Черновик уже на сервере: «Повторить» без известного id создал бы второй, а при неизвестном
      // исходе SendItem — ещё и дубль адресатам. Осознанная повторная отправка остаётся через «Отправить».
      if ((rep.draft_saved && !draftRef) || rep.maybe_sent) lastAction = null;
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
    } finally {
      if (alive) busy = null;
    }
  }

  /** Кнопка «Сохранить черновик» и Ctrl+S: текст и вложения на сервер; композер остаётся открытым. */
  async function saveDraft() {
    if (busy) return;
    const problem = validate(false);
    if (problem) {
      error = problem;
      lastAction = null;
      return;
    }
    busy = "draft";
    error = null;
    lastAction = "draft";
    try {
      await autosavePromise;
      const warn = await saveNow(true);
      if (warn && alive) error = warn;
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
    } finally {
      if (alive) busy = null;
    }
  }

  function retry() {
    if (lastAction === "send") void send();
    else if (lastAction === "draft") void saveDraft();
  }

  function requestCancel() {
    if (busy) return;
    if (isDirty) cancelAsk = true;
    else finish("cancel");
  }
  function keepWriting() {
    cancelAsk = false;
    settleClose(null);
    focusBack(cancelBtn);
  }
  function pickCancel(id: string) {
    cancelAsk = false;
    if (id === "save") void saveAndClose();
    else if (id === "discard") void discardDraft();
    // Открытый черновик остаётся на сервере таким, каким был при последнем сохранении.
    else if (id === "close") finish("cancel");
  }

  /** Вопрос при отмене: тексты и варианты зависят от того, есть ли черновик на сервере. */
  const cancelTitle = $derived(draftRef ? "Закрыть черновик?" : "Отменить текущее письмо?");
  const cancelText = $derived(
    (draftRef ? "Черновик уже в «Черновиках». Сохранить последние правки, закрыть без них или удалить черновик (он уйдёт в «Удалённые»)?" : "На сервере письма ещё нет: сохранить его в «Черновики» или не сохранять?") +
      (pendingFiles ? " Новые файлы и убранные вложения сохранятся вместе с текстом." : ""),
  );
  const cancelOptions = $derived(
    draftRef
      ? [
          { id: "save", label: "Сохранить черновик", primary: true },
          { id: "close", label: "Закрыть без сохранения" },
          { id: "discard", label: "Удалить черновик", danger: true },
        ]
      : [
          { id: "save", label: "Сохранить черновик", primary: true },
          { id: "discard", label: "Не сохранять", danger: true },
        ],
  );

  /** «Сохранить черновик» при отмене: сохранить (с вложениями), если есть что, и закрыть. */
  async function saveAndClose() {
    if (busy) return;
    const problem = validate(false);
    if (problem) {
      error = problem;
      lastAction = null;
      settleClose(null);
      return;
    }
    busy = "draft";
    error = null;
    lastAction = null;
    try {
      await autosavePromise;
      const warn = await saveNow(true);
      if (!alive) return;
      // Текст сохранён, а вложения нет: не закрывать молча — пользователь решит, что делать.
      if (warn) {
        error = warn;
        lastAction = "draft";
        settleClose(null);
        return;
      }
      finish("draft");
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
      settleClose(null);
    } finally {
      if (alive) busy = null;
    }
  }

  /** «Удалить черновик»: на сервере — в «Удалённые», если он там есть; иначе просто закрыть. */
  async function discardDraft() {
    if (busy) return;
    busy = "draft";
    error = null;
    lastAction = null;
    try {
      await autosavePromise;
      const id = draftRef?.server_id ?? null;
      if (id) await api.composeDiscardDraft(id);
      if (alive) finish(id ? "discard" : "cancel");
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
      settleClose(null);
    } finally {
      if (alive) busy = null;
    }
  }

  // --- вложения ---------------------------------------------------------------------------

  async function pickFiles() {
    if (busy) return;
    try {
      const picked = await api.attachmentsPick();
      const have = new Set(attachments.map((a) => a.path));
      attachments = [...attachments, ...picked.filter((p) => !have.has(p.path))];
    } catch (e) {
      error = errorMessage(e);
      lastAction = null;
    }
  }
  function removeFile(path: string) {
    attachments = attachments.filter((a) => a.path !== path);
  }
  function removeServerFile(serverId: string) {
    if (!removedServerIds.includes(serverId)) removedServerIds = [...removedServerIds, serverId];
  }

  // --- клавиши: Ctrl+Enter — отправить, Ctrl+S — сохранить черновик, Esc — отменить ----------
  // Слушаем окно и проверяем, что событие пришло изнутри композера: у всплывающих окон внутри
  // (подсказки адресатов, адрес ссылки) свой Esc, они его останавливают сами.

  function onWindowKey(e: KeyboardEvent) {
    if (!root || !(e.target instanceof Node) || !root.contains(e.target)) return;
    if (sigOpen || cancelAsk) return;
    const ctrl = e.ctrlKey || e.metaKey;
    if (ctrl && e.key === "Enter") {
      e.preventDefault();
      void send();
    } else if (ctrl && !e.shiftKey && !e.altKey && (e.code === "KeyS" || e.key.toLowerCase() === "s")) {
      e.preventDefault();
      void saveDraft();
    } else if (e.key === "Escape") {
      e.preventDefault();
      requestCancel();
    }
  }
</script>

<svelte:window onkeydown={onWindowKey} />

<section class="composer" bind:this={root} aria-label={title} aria-busy={busy != null}>
  <div class="bar">
    <span class="kind">{title}</span>
    <button class="btn primary" onclick={send} disabled={busy != null} title="Отправить (Ctrl+Enter)">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M2 8 14 2.5 11.5 14 8.5 9.5 2 8Zm6.5 1.5L12 5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" /></svg>
      Отправить
    </button>
    <button class="btn" onclick={saveDraft} disabled={busy != null} title="Сохранить черновик на сервере (Ctrl+S)">Сохранить черновик</button>
    <button class="btn" bind:this={cancelBtn} onclick={() => (cancelAsk ? keepWriting() : requestCancel())} disabled={busy != null} title="Отменить (Esc)" aria-haspopup="dialog" aria-expanded={cancelAsk}>Отменить</button>
    {#if busy}
      <span class="progress caption" aria-live="polite">
        <span class="spinner" aria-hidden="true"></span>
        {busy === "send" ? "Отправляю…" : "Сохраняю черновик…"}
      </span>
    {/if}
    <span class="spacer"></span>
    <button class="btn" onclick={pickFiles} disabled={busy != null} title="Вложить файл…">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
      Вложить файл
    </button>
    <button class="btn" bind:this={sigBtn} onclick={() => (sigOpen = true)} disabled={busy != null} title="Подпись: текст и автодобавление" aria-haspopup="dialog">Подпись…</button>
  </div>

  {#if error}
    <div class="error" role="alert">
      <span class="error-text">{error}</span>
      {#if lastAction}<button class="btn small" onclick={retry} disabled={busy != null}>Повторить</button>{/if}
      <button class="btn small" onclick={() => (error = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
    </div>
  {/if}

  <div class="fields">
    <div class="row">
      <label class="lbl" for="{uid}-to">Кому</label>
      <RecipientsInput bind:this={toField} id="{uid}-to" bind:recipients={to} disabled={busy != null} placeholder="Адрес или имя" />
      {#if !showBcc}
        <button class="link-btn" onclick={() => (showBcc = true)} disabled={busy != null}>Скрытая</button>
      {/if}
    </div>
    {#if recipientsUnknown}
      <div class="warn caption" role="status">Получатели исходного письма не загружены — проверь «Кому» и «Копия».</div>
    {/if}
    <div class="row">
      <label class="lbl" for="{uid}-cc">Копия</label>
      <RecipientsInput bind:this={ccField} id="{uid}-cc" bind:recipients={cc} disabled={busy != null} />
    </div>
    {#if showBcc}
      <div class="row">
        <label class="lbl" for="{uid}-bcc">Скрытая</label>
        <RecipientsInput bind:this={bccField} id="{uid}-bcc" bind:recipients={bcc} disabled={busy != null} />
      </div>
    {/if}
    <div class="row">
      <label class="lbl" for="{uid}-subject">Тема</label>
      <input id="{uid}-subject" class="subject" type="text" bind:value={subject} disabled={busy != null} placeholder="Тема письма" autocomplete="off" />
    </div>
  </div>

  {#if attachments.length || keptServerFiles.length || originalFiles.length}
    <div class="files" aria-label="Вложения">
      {#each keptServerFiles as f (f.server_id)}
        <span class="file server" title={[f.content_type ?? "", "Вложение черновика уже на сервере"].filter(Boolean).join("\n")}>
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
          <span class="file-name">{f.name ?? "вложение"}</span>
          <span class="caption file-size">{formatSize(f.size)}</span>
          <button class="file-x" onclick={() => removeServerFile(f.server_id)} disabled={busy != null} aria-label="Убрать вложение {f.name ?? 'без имени'}">×</button>
        </span>
      {/each}
      {#each attachments as a (a.path)}
        <span class="file" title="{a.path}&#10;Файл прикрепится при сохранении черновика или отправке">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
          <span class="file-name">{a.name}</span>
          <span class="caption file-size">{formatSize(a.size)}</span>
          <button class="file-x" onclick={() => removeFile(a.path)} disabled={busy != null} aria-label="Убрать вложение {a.name}">×</button>
        </span>
      {/each}
      {#each originalFiles as a (a.attachment_pk)}
        <span class="file inherited" title="Вложение исходного письма: сервер добавит его к пересылке сам">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
          <span class="file-name">{a.name ?? "вложение"}</span>
          <span class="caption file-size">{formatSize(a.size)}</span>
        </span>
      {/each}
    </div>
  {/if}

  <div class="editor-wrap">
    <Editor bind:this={editor} bind:value={body} disabled={busy != null} minHeight={240} />
  </div>

  <div class="foot caption" aria-live="polite">
    {#if autosaving}
      <span class="spinner small" aria-hidden="true"></span>
      <span>Сохраняю…</span>
    {:else if autosaveError}
      <span class="foot-failed">Черновик не сохранён: {autosaveError}</span>
    {:else if savedAt != null}
      <span>Черновик сохранён в {timeOnly.format(new Date(savedAt))}{pendingFiles ? " · изменения вложений ещё не сохранены" : ""}</span>
    {:else if draftRef}
      <span>Черновик с сервера{isDirty ? " · есть несохранённые правки" : ""}</span>
    {:else if isDirty && nonEmpty}
      <span>Черновик сохранится автоматически</span>
    {/if}
  </div>

  {#if original}
    <section class="original" aria-label="Исходное письмо">
      <button class="orig-toggle" onclick={() => (showOriginal = !showOriginal)} aria-expanded={showOriginal}>
        <svg class="chev" class:open={showOriginal} width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M4 2.5 7.5 6 4 9.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
        Исходное письмо
        <span class="caption orig-hint">цитату добавит сервер</span>
      </button>
      {#if showOriginal}
        <div class="orig-meta caption">
          <div><span class="orig-key">От:</span> {personLabel(original.from_name, original.from_email) || "—"}</div>
          <div><span class="orig-key">Кому:</span> {originalTo.length ? originalTo.join(", ") : original.display_to || "—"}</div>
          <div><span class="orig-key">Дата:</span> {fullDate(original.received_ms ?? original.sent_ms) || "—"}</div>
          <div><span class="orig-key">Тема:</span> {original.subject || "(без темы)"}</div>
        </div>
        {#if original.body_html != null}
          <BodyFrame html={original.body_html} title="Исходное письмо" />
        {:else if origLoading}
          <div class="orig-note caption"><span class="spinner" aria-hidden="true"></span> Загружаю исходное письмо…</div>
        {:else if origError}
          <div class="orig-note failed">{origError}</div>
        {:else}
          <div class="orig-note caption">Текст письма не загружен.</div>
        {/if}
      {/if}
    </section>
  {/if}

  {#if cancelAsk && cancelBtn}
    <ChoicePopover
      anchor={cancelBtn}
      align="start"
      title={cancelTitle}
      text={cancelText}
      options={cancelOptions}
      cancelLabel="Вернуться"
      onpick={pickCancel}
      oncancel={keepWriting}
    />
  {/if}
  {#if sigOpen}
    <SignatureDialog {signature} onsaved={(s) => (signature = s)} onclose={() => ((sigOpen = false), focusBack(sigBtn))} />
  {/if}
</section>

<style>
  .composer {
    flex: none;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-2);
    min-width: 0;
  }

  /* --- верхняя панель --- */
  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
    flex-wrap: wrap;
  }
  .kind {
    font: var(--text-body-strong);
    margin-right: var(--space-2);
  }
  .spacer {
    flex: 1;
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
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
    padding: 0 14px;
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
  .progress {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--fg-2);
  }

  /* --- ошибка --- */
  .error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    color: var(--danger);
    border-bottom: 1px solid var(--stroke-1);
  }
  .error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }

  /* --- поля --- */
  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-2) var(--space-3) 0;
  }
  .row {
    display: grid;
    grid-template-columns: 64px minmax(0, 1fr) auto;
    align-items: start;
    gap: var(--space-2);
  }
  .lbl {
    font: var(--text-caption);
    color: var(--fg-3);
    line-height: var(--control-height);
  }
  .link-btn {
    height: var(--control-height);
    padding: 0 8px;
    color: var(--accent);
    border-radius: var(--radius-control);
  }
  .link-btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .link-btn:disabled {
    color: var(--fg-disabled);
  }
  .warn {
    padding-left: calc(64px + var(--space-2));
    color: var(--danger);
  }
  .subject {
    height: var(--control-height);
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    outline: none;
    user-select: text;
    min-width: 0;
  }
  .subject:focus {
    border-bottom: 2px solid var(--accent);
  }
  .subject::placeholder {
    color: var(--fg-3);
  }

  /* --- вложения --- */
  .files {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3) 0;
  }
  .file {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 28px;
    padding: 0 4px 0 10px;
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    color: var(--fg-1);
    max-width: 360px;
  }
  /* Вложение черновика с сервера: оно уже там, крестик лишь помечает его к удалению при отправке. */
  .file.server {
    border-color: var(--stroke-2);
    background: var(--bg-layer-alt);
  }
  .file.inherited {
    color: var(--fg-3);
    border-style: dashed;
    padding-right: 10px;
  }
  .file-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .file-size {
    flex: none;
  }
  .file-x {
    width: 20px;
    height: 20px;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    font-size: 16px;
    line-height: 1;
  }
  .file-x:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .file-x:disabled {
    color: var(--fg-disabled);
  }

  /* --- редактор, подвал и оригинал --- */
  .editor-wrap {
    padding: var(--space-2) var(--space-3) var(--space-2);
  }
  .foot {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 24px;
    padding: 0 var(--space-3) var(--space-2);
    color: var(--fg-3);
  }
  .foot-failed {
    color: var(--danger);
    user-select: text;
  }
  .original {
    border-top: 1px solid var(--stroke-1);
  }
  .orig-toggle {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    height: 36px;
    padding: 0 var(--space-3);
    color: var(--fg-2);
    text-align: left;
  }
  .orig-toggle:hover {
    background: var(--bg-hover);
  }
  .orig-toggle:focus-visible {
    outline-offset: -2px;
  }
  .chev {
    flex: none;
    transition: transform var(--dur-fast) var(--ease-in);
  }
  .chev.open {
    transform: rotate(90deg);
  }
  .orig-hint {
    margin-left: var(--space-2);
  }
  .orig-meta {
    padding: 0 var(--space-3) var(--space-2);
    display: flex;
    flex-direction: column;
    gap: 2px;
    user-select: text;
  }
  .orig-meta div {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .orig-key {
    color: var(--fg-4);
  }
  .orig-note {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-3);
    border-top: 1px solid var(--stroke-1);
  }
  .orig-note.failed {
    color: var(--danger);
    user-select: text;
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
  .spinner.small {
    width: 11px;
    height: 11px;
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
