<script lang="ts">
  import { onDestroy, onMount, tick, untrack } from "svelte";
  import { api, errorMessage, failNetwork, failOriginalGone, failRetryable, formatSize, fullDate, isInlinePicture, messageNeedsFetch, onSyncProgress, personLabel, type ComposeContext, type ComposeRecipient, type ComposeRequest, type DraftAttachment, type DraftRef, type Importance, type LocalDraft, type Message, type PickedFile, type SignatureDto, type UploadProgress } from "../api";
  import { COMPOSE_TITLE, ICON } from "../actions";
  import { isValidEmail, mergeRecipients } from "../addresses";
  import { cleanDraftHtml, htmlToText, restoreDraftHtml } from "../html";
  import { recipientDrag } from "../recipientDrag.svelte";
  import { focusBack } from "../popover";
  import { hint, layerOpen } from "../hotkeys";
  import RecipientsInput from "./RecipientsInput.svelte";
  import Editor from "./Editor.svelte";
  import ChoicePopover from "./ChoicePopover.svelte";
  import SignatureDialog from "./SignatureDialog.svelte";
  import AdvicePanel from "./AdvicePanel.svelte";
  import type { AdviceKind, AiAdviceStatus } from "../api";

  /**
   * Почему композер закрылся: отправлено, сохранён черновик, черновик удалён, просто отменено;
   * `local` — связи нет, текст остался локальным черновиком (его предложат открыть); `window` —
   * письмо перенесено в отдельное окно (п. 32), его локальный черновик теперь у того окна.
   */
  export type CloseReason = "sent" | "draft" | "discard" | "cancel" | "local" | "window";

  interface Props {
    /** Заготовка от бэкенда; новая заготовка — новый экземпляр композера (родитель оборачивает в {#key}). */
    ctx: ComposeContext;
    /** Есть ли несохранённые правки (родитель спрашивает подтверждение, прежде чем открыть другое письмо). */
    dirty?: boolean;
    /** Идёт отправка или сохранение черновика: родитель не заменяет композер и не даёт закрыть окно. */
    busy?: "send" | "draft" | null;
    onclose: (reason: CloseReason) => void;
    /**
     * Композер — всё окно черновика (п. 32): без рамки карточки и без кнопки «Редактировать в
     * отдельном окне». Иначе эта кнопка переносит письмо в своё окно.
     */
    standalone?: boolean;
    /** Ключ локального черновика, если его выбрал родитель (окно черновика знает его по имени окна). */
    localDraftId?: string;
    /** Высота поля текста; окно черновика растягивает его на окно. */
    editorMinHeight?: number;
    /** Тема письма — для заголовка окна черновика. */
    heading?: string;
  }
  let { ctx, dirty = $bindable(false), busy = $bindable(null), onclose, standalone = false, localDraftId, editorMinHeight = 240, heading = $bindable("") }: Props = $props();

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

  /** Закрыть композер с причиной и сообщить её тому, кто ждёт askClose. Локальный черновик нужен только при `local` и `window`. */
  function finish(reason: CloseReason) {
    if (reason !== "local" && reason !== "window") dropLocal();
    onclose(reason);
    settleClose(reason);
  }

  /** Закрыть композер с вопросом, если есть правки; null — пользователь вернулся к письму (или идёт отправка). */
  export function askClose(): Promise<CloseReason | null> {
    if (busy) return Promise.resolve(null);
    if (!isDirty) {
      if (strayDraft) return dropStrayDraft();
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

  // Перенос адресата между «Кому», «Копией» и «Скрытой» (п. 34 обкатки): мышью — чип тянется в
  // другое поле (с Ctrl копируется), с клавиатуры — пункты «Переместить в…» в меню чипа.
  type Field = "to" | "cc" | "bcc";
  const MOVE_LABEL: Record<Field, string> = { to: "Переместить в «Кому»", cc: "Переместить в «Копию»", bcc: "Переместить в «Скрытую»" };
  const moveTargets = (own: Field) => (["to", "cc", "bcc"] as Field[]).filter((f) => f !== own).map((f) => ({ field: f, label: MOVE_LABEL[f] }));
  const MOVE_TARGETS: Record<Field, { field: string; label: string }[]> = { to: moveTargets("to"), cc: moveTargets("cc"), bcc: moveTargets("bcc") };
  /** Тянут чип этого письма: спрятанная «Скрытая» показывается, чтобы в неё тоже можно было бросить. */
  const draggingRecipient = $derived(recipientDrag.group === uid);

  function transferRecipient(from: Field, r: ComposeRecipient, target: string, index: number | null, copy: boolean) {
    if (target !== "to" && target !== "cc" && target !== "bcc") return;
    const key = r.email.trim().toLowerCase();
    const same = (x: ComposeRecipient) => x.email.trim().toLowerCase() === key;
    const get = (f: Field) => (f === "to" ? to : f === "cc" ? cc : bcc);
    const set = (f: Field, v: ComposeRecipient[]) => {
      if (f === "to") to = v;
      else if (f === "cc") cc = v;
      else bcc = v;
    };
    // Адресат, который в поле уже есть, второй раз не добавляется.
    const dst = get(target);
    if (!dst.some(same)) {
      const at = index == null ? dst.length : Math.max(0, Math.min(index, dst.length));
      set(target, [...dst.slice(0, at), r, ...dst.slice(at)]);
    }
    if (!copy && from !== target) set(from, get(from).filter((x) => !same(x)));
    if (target === "bcc") showBcc = true;
    // Чип, с которого начали, из прежнего поля убран: фокус — в поле, куда он переехал, иначе он
    // ушёл бы из композера, и клавиши окна сработали бы на беседу под ним.
    void tick().then(() => (target === "to" ? toField : target === "cc" ? ccField : bccField)?.focus());
  }
  let subject = $state(untrack(() => ctx.subject));
  /** Важность письма (п. 35 обкатки): уходит с письмом, пишется в черновик на сервере и в локальный. */
  let importance = $state<Importance>(untrack(() => ctx.importance ?? "Normal"));
  function toggleImportance(v: "High" | "Low") {
    importance = importance === v ? "Normal" : v;
  }
  const startBody = untrack(() => initialBody(ctx));
  let body = $state(startBody);
  let attachments = $state<PickedFile[]>(untrack(() => ctx.restore?.attachments ?? []));
  let signature = $state<SignatureDto>(untrack(() => ctx.signature));
  let original = $state<Message | null>(untrack(() => ctx.original));
  /**
   * Вид письма и исходное письмо для запросов. После «Развернуть» (expandQuote) письмо — черновик на
   * сервере с цитатой в тексте: дальше оно сохраняется и уходит как открытый черновик (вид «новое»),
   * и бэкенд не дописывает цитату к тексту окна.
   */
  let kind = $state(untrack(() => ctx.kind));
  let referencePk = $state(untrack(() => ctx.reference_item_pk));
  /** Цитата развёрнута в редактор: свёрнутая строка «Исходное письмо» больше не нужна. */
  let quoteInText = $state(false);
  let expanding = $state(false);

  // --- черновик на сервере -----------------------------------------------------------------
  // Открытый черновик приходит с серверным id и вложениями; новое письмо получает id после первого
  // автосохранения. Отправка идёт по этому же черновику (draft_server_id), чтобы не плодить копии.

  let draftRef = $state<DraftRef | null>(
    untrack(() => {
      if (ctx.draft) return { server_id: ctx.draft.server_id, change_key: ctx.draft.change_key ?? null, item_pk: ctx.draft.item_pk };
      // Восстановленное письмо уже создавало черновик на сервере: следующее сохранение обновит его.
      if (ctx.restore?.draft_server_id) return { server_id: ctx.restore.draft_server_id, change_key: null, item_pk: null };
      return null;
    }),
  );
  /**
   * Черновик на сервере появится в этом композере сам (автосохранение, «Развернуть»), а не придёт
   * открытым из «Черновиков» или с перенесённым письмом.
   */
  const draftMadeHere = untrack(() => draftRef == null);
  /** Автор сохранял письмо сам: «Сохранить черновик» или неудачная отправка, оставившая черновик. */
  let savedByAuthor = $state(false);
  /** Вложения черновика, уже лежащие на сервере (у перенесённого в окно письма — перечитанные окном). */
  let serverFiles = $state<DraftAttachment[]>(untrack(() => ctx.draft?.attachments ?? ctx.restore_files ?? []));
  /** Серверные вложения, убранные крестиком: уйдут в removed_attachment_ids при сохранении или отправке. */
  let removedServerIds = $state<string[]>(untrack(() => ctx.restore?.removed_attachment_ids ?? []));
  const keptServerFiles = $derived(serverFiles.filter((f) => !removedServerIds.includes(f.server_id)));
  /** Список вложений черновика известен с сервера (открытый черновик или после сохранения с файлами). */
  let serverListed = $state(untrack(() => ctx.draft != null || ctx.restore_files != null));
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
  /** Ход загрузки вложений, пока идёт отправка или сохранение (п. 39 обкатки); null — файлы не грузятся. */
  let upload = $state<UploadProgress | null>(null);
  const onUpload = (p: UploadProgress) => {
    if (alive && p.total > 0) upload = p;
  };
  const uploadPercent = $derived(upload ? Math.min(100, Math.floor((upload.sent / upload.total) * 100)) : 0);
  let lastAction = $state<"send" | "draft" | null>(null);
  /**
   * Оригинала ответа или пересылки больше нет, цитату не собрать (`original_gone`): какое действие
   * отказало — его кнопка «…без цитаты» повторит, когда автор согласится.
   */
  let quoteGone = $state<"send" | "draft" | "autosave" | null>(null);
  /** Автор согласился продолжать без цитаты: черновик и отправка дальше — одним текстом окна. */
  let withoutQuote = $state(false);
  let cancelAsk = $state(false);
  let sigOpen = $state(false);
  let origLoading = $state(false);
  let origError = $state<string | null>(null);

  let root = $state<HTMLElement | null>(null);

  // Пока идёт отправка или сохранение, поля и кнопки выключены, и Chromium уводит с них фокус на
  // body. Тогда клавиши окна (E, Ctrl+R, Ctrl+Z) сочли бы, что фокус вне композера, и сработали бы
  // на беседу под ним. Поэтому на время занятости фокус переходит на сам композер — до того, как
  // поля выключатся ($effect.pre), — а после возвращается туда, где был.
  $effect(() => {
    // Действие кончилось: следующая отправка начнёт отсчёт загрузки заново.
    if (busy == null) upload = null;
  });
  let focusBeforeBusy: HTMLElement | null = null;
  $effect.pre(() => {
    if (busy == null) return;
    untrack(() => {
      const ae = document.activeElement;
      if (root && ae instanceof HTMLElement && ae !== root && root.contains(ae)) {
        focusBeforeBusy = ae;
        root.focus({ preventScroll: true });
      }
    });
  });
  $effect(() => {
    if (busy != null) return;
    untrack(() => {
      const el = focusBeforeBusy;
      focusBeforeBusy = null;
      if (el?.isConnected && document.activeElement === root) el.focus({ preventScroll: true });
    });
  });
  // --- строка кнопок в одну строку (п. 38 обкатки) -------------------------------------------
  // Кнопок много, область чтения бывает узкой. Чтобы строка не переносилась, подписи по очереди
  // сменяются значками: «Сохранить черновик», затем «Отменить», затем «Отправить». Сколько
  // убрать, решает замер: уровень повышается, пока кнопки не встанут в одну строку (ширина
  // зависит от заголовка письма, кнопки «Совет» и надписи о ходе отправки — порогом её не задать).
  let barEl = $state<HTMLElement | null>(null);
  let squeeze = $state(0);
  function fitBar() {
    const bar = barEl;
    if (!bar) return;
    const wrapped = () => {
      const kids = Array.from(bar.children, (k) => k.getBoundingClientRect());
      return kids.length > 1 && kids.some((r) => r.top > kids[0].bottom - 2);
    };
    let level = 0;
    // Атрибут ставится сразу в DOM: замер следующего уровня нужен до перерисовки.
    for (bar.dataset.squeeze = "0"; level < 3 && wrapped(); bar.dataset.squeeze = String(level)) level++;
    squeeze = level;
  }
  $effect(() => {
    const bar = barEl;
    if (!bar) return;
    let width = -1;
    const ro = new ResizeObserver(() => {
      // Высота меняется от самого подбора (перенос пропал) — пересчитывать нужно только по ширине.
      const w = Math.round(bar.getBoundingClientRect().width);
      if (w === width) return;
      width = w;
      fitBar();
    });
    ro.observe(bar);
    return () => ro.disconnect();
  });
  $effect(() => {
    // Состав строки изменился (надпись о ходе, «Совет», заголовок): подобрать заново после перерисовки.
    void [busy, title, adviceStatus?.available, droppingStray];
    void tick().then(fitBar);
  });
  let cancelBtn = $state<HTMLButtonElement | null>(null);
  let sigBtn = $state<HTMLButtonElement | null>(null);
  let editor = $state<ReturnType<typeof Editor> | null>(null);
  let toField = $state<ReturnType<typeof RecipientsInput> | null>(null);
  let ccField = $state<ReturnType<typeof RecipientsInput> | null>(null);
  let bccField = $state<ReturnType<typeof RecipientsInput> | null>(null);

  const keyOf = (rs: ComposeRecipient[]) => rs.map((r) => r.email.trim().toLowerCase()).join(",");
  /** Разметка в том виде, как её сериализует браузер: правка одного форматирования (жирный, список) тоже правка. */
  const normHtml = (h: string) => new DOMParser().parseFromString(h, "text/html").body.innerHTML;
  // Восстановленный локальный черновик на сервере не сохранён: отсчёт правок — от пустого письма.
  const init = untrack(() => (ctx.restore ? { subject: "", bodyText: "", bodyHtml: normHtml("<div><br></div>"), to: "", cc: "", bcc: "", importance: "Normal" } : { subject: ctx.subject, bodyText: htmlToText(startBody), bodyHtml: normHtml(startBody), to: keyOf(to), cc: keyOf(cc), bcc: keyOf(bcc), importance }));
  /** Текст тела без разметки: считается раз на правку, нужен и «есть правки», и «письмо непустое». */
  const bodyText = $derived(htmlToText(body));
  const isDirty = $derived(subject !== init.subject || keyOf(to) !== init.to || keyOf(cc) !== init.cc || keyOf(bcc) !== init.bcc || importance !== init.importance || attachments.length > 0 || removedServerIds.length > 0 || bodyText !== init.bodyText || (body !== startBody && normHtml(body) !== init.bodyHtml));
  /**
   * Черновик, который создал сам композер и автор не сохранял (п. 33): при нетронутом письме он
   * лишний и при закрытии убирается. Для родителя письмо с ним — «с правками»: закрытие окна,
   * замена композера и установка обновления идут через askClose, а не мимо.
   */
  const strayDraft = $derived(draftMadeHere && draftRef != null && !savedByAuthor);
  $effect(() => {
    dirty = isDirty || strayDraft;
  });
  $effect(() => {
    heading = subject.trim();
  });

  /** Файлы исходного письма при пересылке: их добавит сервер, здесь только показываем (пока список с сервера не известен — там они уже есть). */
  const originalFiles = $derived(ctx.kind === "forward" && original && !serverListed ? original.attachments.filter((a) => !isInlinePicture(a)) : []);
  /** Подпись свёрнутой цитаты: от кого и когда. */
  const originalSummary = $derived(original ? [personLabel(original.from_name, original.from_email), fullDate(original.received_ms ?? original.sent_ms)].filter(Boolean).join(", ") : "");
  /**
   * «Ответить всем», а получатели исходного письма в базе неизвестны: они приходят только вместе с
   * телом, и пока оно не загружено (или сервер его не отдал), в заготовке один отправитель.
   */
  const recipientsUnknown = $derived(ctx.kind === "reply_all" && original != null && original.body_state !== 1 && original.recipients.length === 0 && !origLoading);

  // --- «Совет» (docs/ai-layer-design.md § 4) -------------------------------------------------
  // Для ответа, ответа всем и пересылки, если ИИ включён и модель назначена. Вид и исходное
  // письмо — из заготовки: после «Развернуть» письмо становится «новым», а совет по беседе нужен тот же.

  const adviceKind: AdviceKind | null = untrack(() => (ctx.reference_item_pk != null && (ctx.kind === "reply" || ctx.kind === "reply_all" || ctx.kind === "forward") ? ctx.kind : null));
  const adviceRef = untrack(() => ctx.reference_item_pk);
  let adviceStatus = $state<AiAdviceStatus | null>(null);
  let adviceOpen = $state(false);

  onMount(() => {
    if (!adviceKind) return;
    api
      .aiAdviceStatus()
      .then((s) => {
        if (alive) adviceStatus = s;
      })
      .catch(() => {});
  });

  /** Текст черновика «Совета» в разметку редактора: строки — абзацами, пустые — пустыми строками. */
  function adviceHtml(text: string): string {
    const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    return text
      .trim()
      .split("\n")
      .map((l) => (l.trim() ? `<div>${esc(l)}</div>` : "<div><br></div>"))
      .join("");
  }

  /**
   * «Вставить в письмо» — черновик в начало письма (над тем, что уже есть: подписью, цитатой);
   * «Заменить мой текст» — письмо заново: черновик и подпись, если она добавляется сама.
   */
  function insertAdvice(text: string, mode: "insert" | "replace") {
    const html = adviceHtml(text);
    if (!html) return;
    if (mode === "replace") {
      const s = signature.auto_add ? signature.html.trim() : "";
      body = s ? `${html}<div><br></div><div><br></div>${s}` : html;
    } else {
      // Пустые строки под каретку в начале письма (их две над подписью) заменяются черновиком:
      // между ним и подписью остаётся одна.
      const rest = body.replace(/^(?:<div><br><\/div>){1,2}/, "");
      body = `${html}<div><br></div>${rest}`;
    }
    adviceOpen = false;
    editor?.focus();
  }

  onMount(() => {
    root?.scrollIntoView({ block: "start" });
    // Ответу и черновику с адресатом нужен текст, остальным — адресат.
    if (ctx.kind === "reply" || ctx.kind === "reply_all" || (ctx.draft && to.length)) editor?.focus();
    // Новое письмо с адресатом заранее («Написать письмо» из карточки контакта): дальше — тема.
    else if (ctx.kind === "new" && !ctx.draft && to.length && !subject) document.getElementById(`${uid}-subject`)?.focus();
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
      kind,
      reference_item_pk: referencePk,
      to: clean(to),
      cc: clean(cc),
      bcc: clean(bcc),
      subject: subject.trim(),
      // Картинкам и стилям черновика возвращаются исходные src/style (cid:, внешние адреса).
      body_html: restoreDraftHtml(body),
      attachments: attachments.map((a) => a.path),
      draft_server_id: draftRef?.server_id ?? null,
      removed_attachment_ids: removedServerIds,
      local_draft_id: localId,
      without_quote: withoutQuote,
      importance,
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

  /** Что именно сохраняется: тема, получатели, важность и разметка тела (форматирование тоже правка). */
  const snapshot = $derived(JSON.stringify([subject.trim(), keyOf(to), keyOf(cc), keyOf(bcc), body, importance]));
  const nonEmpty = $derived(subject.trim() !== "" || to.length + cc.length + bcc.length > 0 || bodyText !== "");
  /** Снимок, который лежит на сервере; у открытого черновика — его начальное состояние. */
  let lastSavedKey = $state<string | null>(untrack(() => (ctx.draft ? snapshot : null)));
  /** Снимок, на котором автосохранение отказало: повтор только после правки. */
  let failedKey = $state<string | null>(null);
  let lastSaveAt = 0;
  let autosaving = $state(false);
  let autosaveError = $state<string | null>(null);
  /** Последний отказ автосохранения — из-за связи: окно повторит само, текст пока только здесь. */
  let autosaveNetwork = $state(false);
  /** Сетевых отказов подряд: пауза перед повтором растёт (60 с, 2, 4, 8 мин), сбрасывается удачей. */
  let netFailures = $state(0);
  let savedAt = $state<number | null>(null);
  let autosavePromise: Promise<void> | null = null;

  $effect(() => {
    const key = snapshot;
    if (busy || autosaving || !isDirty || !nonEmpty || key === lastSavedKey || key === failedKey) return;
    const gap = AUTOSAVE_GAP_MS * 2 ** Math.min(netFailures, 3);
    const wait = Math.max(AUTOSAVE_DELAY_MS, lastSaveAt + gap - Date.now());
    const t = setTimeout(autosave, wait);
    return () => clearTimeout(t);
  });

  // Связь вернулась (прогон синхронизации прошёл без ошибки): не ждать паузу, повторить сразу.
  onMount(() => {
    let unlisten: (() => void) | undefined;
    let gone = false;
    void onSyncProgress((s) => {
      if (!s.running && !s.last_error && autosaveNetwork && alive && !busy && !autosaving) autosave();
    }).then((u) => {
      // Композер закрыли, пока подписка оформлялась: снять её сразу, иначе она осталась бы навсегда.
      if (gone) u();
      else unlisten = u;
    });
    return () => {
      gone = true;
      unlisten?.();
    };
  });

  // --- локальный черновик ------------------------------------------------------------------
  // Текст, тема, получатели, файлы и ссылка на черновик сервера пишутся в базу приложения через
  // полторы секунды после правки, при любой связи. Убирается при закрытии композера, кроме
  // закрытия «связи нет, письмо осталось локально» (его предложат открыть).

  const LOCAL_SAVE_DELAY_MS = 1500;
  const localId = untrack(() => ctx.restore?.draft_id ?? localDraftId ?? crypto.randomUUID());
  const localSnapshot = $derived(JSON.stringify([snapshot, attachments.map((a) => a.path), removedServerIds, draftRef?.server_id ?? null]));
  let lastLocalKey = $state<string | null>(untrack(() => (ctx.restore ? JSON.stringify([snapshot, (ctx.restore.attachments ?? []).map((a) => a.path), ctx.restore.removed_attachment_ids ?? [], ctx.restore.draft_server_id ?? null]) : null)));

  $effect(() => {
    const key = localSnapshot;
    if (!isDirty || !nonEmpty || key === lastLocalKey) return;
    const t = setTimeout(() => void saveLocal(key), LOCAL_SAVE_DELAY_MS);
    return () => clearTimeout(t);
  });

  // Запись и удаление идут строго по очереди: команды бэкенда выполняются в пуле потоков, и без
  // очереди удаление могло бы обогнать ещё не законченную запись — черновик остался бы в базе и
  // всплыл бы полосой «Несохранённое письмо». Пустая очередь команду не задерживает.
  let localQueue: Promise<unknown> = Promise.resolve();
  let localPending = 0;
  /** Локальный черновик убран: запоздавшая запись его не вернёт. */
  let localDropped = false;
  function queueLocal<T>(job: () => Promise<T>): Promise<T> {
    const run = localPending === 0 ? job() : localQueue.then(job, job);
    localPending++;
    const done = () => {
      localPending--;
    };
    localQueue = run.then(done, done);
    return run;
  }
  /** Начатые записи и удаление локального черновика дошли до базы: окно ждёт этого перед закрытием. */
  export function localSettled(): Promise<void> {
    return localQueue.then(() => {});
  }

  /** Записать локальный черновик; null — записан, иначе текст ошибки. */
  async function saveLocal(key: string): Promise<string | null> {
    if (!alive) return "окно письма уже закрыто";
    if (localDropped) return "письмо уже закрыто";
    const draft: LocalDraft = {
      draft_id: localId,
      kind,
      reference_item_pk: referencePk,
      draft_server_id: draftRef?.server_id ?? null,
      subject: subject.trim(),
      to: clean(to),
      cc: clean(cc),
      bcc: clean(bcc),
      body_html: restoreDraftHtml(body),
      attachments: $state.snapshot(attachments),
      removed_attachment_ids: $state.snapshot(removedServerIds),
      importance,
      updated_ms: Date.now(),
    };
    try {
      await queueLocal(() => api.composeLocalSave(draft));
      if (alive) lastLocalKey = key;
      return null;
    } catch (e) {
      console.warn("compose_local_save:", errorMessage(e));
      return errorMessage(e);
    }
  }

  function dropLocal() {
    localDropped = true;
    void queueLocal(() => api.composeLocalDrop(localId)).catch((e) => console.warn("compose_local_drop:", errorMessage(e)));
  }

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
        const rep = await api.composeSaveDraft(saveRequest(), onUpload);
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
      autosaveNetwork = false;
      netFailures = 0;
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
        autosaveNetwork = failNetwork(e);
        if (failOriginalGone(e)) quoteGone = "autosave";
        if (autosaveNetwork) {
          // Связь: тот же снимок повторится сам (пауза растёт), правка не нужна.
          netFailures += 1;
          failedKey = null;
        } else {
          failedKey = key;
        }
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
      const rep = await api.composeSend(request(), onUpload);
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
        savedByAuthor = true;
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
      if (!failRetryable(e)) lastAction = null;
      if (failOriginalGone(e)) quoteGone = "send";
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
      if (alive) savedByAuthor = true;
      if (warn && alive) error = warn;
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
      if (!failRetryable(e)) lastAction = null;
      if (failOriginalGone(e)) quoteGone = "draft";
    } finally {
      if (alive) busy = null;
    }
  }

  function retry() {
    if (lastAction === "send") void send();
    else if (lastAction === "draft") void saveDraft();
  }

  /** Подпись кнопки «…без цитаты» по отказавшему действию. */
  const quoteGoneLabel = $derived(quoteGone === "send" ? "Отправить без цитаты" : quoteGone === "draft" ? "Сохранить без цитаты" : "Сохранять без цитаты");

  /**
   * Оригинала больше нет: по решению автора черновик и отправка дальше идут без цитаты (тело
   * черновика на сервере заменится текстом окна), и отказавшее действие повторяется.
   */
  function continueWithoutQuote() {
    const what = quoteGone;
    withoutQuote = true;
    quoteGone = null;
    error = null;
    autosaveError = null;
    failedKey = null;
    if (what === "send") void send();
    else if (what === "draft") void saveDraft();
    else autosave();
  }

  /**
   * «Развернуть» свёрнутую цитату (п. 27 обкатки): письмо сохраняется черновиком на сервере —
   * цитату (а при пересылке и вложения исходного письма) ставит сервер, как в OWA, — и тело
   * черновика целиком встаёт в редактор. Дальше это открытый черновик: цитата правится и удаляется
   * как обычный текст, а сохраняется и уходит то, что в окне. Тело не пришло — черновик всё равно
   * запомнен, цитата остаётся свёрнутой, причина — в строке ошибки.
   */
  async function expandQuote() {
    if (busy || quoteInText || referencePk == null) return;
    busy = "draft";
    expanding = true;
    error = null;
    lastAction = null;
    try {
      // Идущее автосохранение должно закончиться: разворачивается его черновик, а не новый.
      await autosavePromise;
      const key = snapshot;
      const rep = await api.composeExpandQuote(draftRequest());
      if (!alive) return;
      draftRef = rep.draft;
      if (rep.body_html == null) {
        lastSavedKey = key;
        error = rep.message ?? "Цитата не загружена";
        return;
      }
      applyServerFiles(rep.attachments);
      body = cleanDraftHtml(rep.body_html, rep.inline_images) || "<div><br></div>";
      kind = "new";
      referencePk = null;
      quoteInText = true;
      // Черновик на сервере — ровно то, что теперь в окне.
      lastSavedKey = snapshot;
      failedKey = null;
      savedAt = Date.now();
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
    } finally {
      if (alive) {
        busy = null;
        expanding = false;
      }
    }
  }

  function requestCancel() {
    if (busy) return;
    if (isDirty) cancelAsk = true;
    else if (strayDraft) void dropStrayDraft();
    else finish("cancel");
  }

  let droppingStray = $state(false);

  /**
   * Письмо вернулось к заготовке, а черновик на сервере остался от автосохранения (п. 33): убрать
   * его в «Удалённые» молча и закрыть композер. Не вышло (нет связи) — композер всё равно
   * закрывается: автор отменял письмо, о черновике он не знает.
   */
  async function dropStrayDraft(): Promise<CloseReason> {
    busy = "draft";
    droppingStray = true;
    try {
      await autosavePromise;
      const id = draftRef?.server_id;
      if (id) await api.composeDiscardDraft(id);
    } catch (e) {
      console.warn("compose_discard_draft (лишний черновик):", errorMessage(e));
    } finally {
      if (alive) {
        busy = null;
        droppingStray = false;
      }
    }
    if (alive) finish("cancel");
    return "cancel";
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
      (pendingFiles ? " Новые файлы и убранные вложения сохранятся вместе с текстом." : "") +
      (autosaveNetwork ? " Связи с сервером сейчас нет: при сохранении письмо останется в приложении и будет предложено позже." : ""),
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
      savedByAuthor = true;
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
      if (failNetwork(e)) {
        // Сервер недоступен: текст остаётся локальным черновиком, его предложат открыть позже.
        const localFailed = await saveLocal(localSnapshot);
        if (!alive) return;
        if (!localFailed) {
          finish("local");
          return;
        }
        // Не записался и локально: закрыть — значит потерять текст.
        error = `${errorMessage(e)}. Сохранить письмо в приложении тоже не вышло (${localFailed}) — оно есть только в этом окне.`;
        settleClose(null);
        return;
      }
      error = errorMessage(e);
      if (!failRetryable(e)) lastAction = null;
      if (failOriginalGone(e)) quoteGone = "draft";
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

  // --- отдельное окно (п. 32) ---------------------------------------------------------------

  let poppingOut = $state(false);

  /**
   * «Редактировать в отдельном окне»: письмо целиком (текст, получатели, файлы, черновик на сервере)
   * записывается локальным черновиком, окно черновика поднимает его, этот композер закрывается.
   * Идущее автосохранение сначала заканчивается: окно продолжит тот же черновик на сервере.
   */
  async function popOut() {
    if (busy || poppingOut) return;
    poppingOut = true;
    error = null;
    try {
      await autosavePromise;
      if (!alive) return;
      // Недонабранный адрес становится чипом: в локальный черновик идут только чипы.
      toField?.commit();
      ccField?.commit();
      bccField?.commit();
      const failed = await saveLocal(localSnapshot);
      if (failed) {
        error = `Письмо не перенесено в отдельное окно: ${failed}`;
        lastAction = null;
        return;
      }
      await api.composeWindow(localId, draftRef?.item_pk ?? null);
      if (alive) finish("window");
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
      lastAction = null;
    } finally {
      if (alive) poppingOut = false;
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

  // --- файлы, перетащенные в письмо (п. 37 обкатки) ------------------------------------------
  // Файл из Проводника, брошенный в любое место письма, становится вложением. Слушаем сам
  // композер на фазе перехвата: иначе файл, брошенный на текст, браузер вставил бы картинкой.

  /** Над письмом тянут файлы: показывается подсказка. Счётчик — входы минус выходы по вложенным элементам. */
  let dropDepth = 0;
  let dropActive = $state(false);
  /** Сколько брошенных файлов ещё сохраняется. */
  let dropSaving = $state(0);
  const carriesFiles = (e: DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes("Files");

  function onFilesEnter(e: DragEvent) {
    if (!carriesFiles(e)) return;
    e.preventDefault();
    dropDepth++;
    dropActive = true;
  }
  function onFilesOver(e: DragEvent) {
    if (!carriesFiles(e)) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = busy ? "none" : "copy";
  }
  function onFilesLeave(e: DragEvent) {
    if (!carriesFiles(e)) return;
    dropDepth = Math.max(0, dropDepth - 1);
    if (dropDepth === 0) dropActive = false;
  }
  function onFilesDrop(e: DragEvent) {
    if (!carriesFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    dropDepth = 0;
    dropActive = false;
    if (busy) return;
    // Файлы берутся из события сразу: после первого await список в нём уже пуст.
    const files: File[] = [];
    let folders = 0;
    for (const it of Array.from(e.dataTransfer?.items ?? [])) {
      if (it.kind !== "file") continue;
      if (it.webkitGetAsEntry?.()?.isDirectory) folders++;
      else {
        const f = it.getAsFile();
        if (f) files.push(f);
      }
    }
    void attachDropped(files, folders);
  }

  async function attachDropped(files: File[], folders: number) {
    const problems: string[] = [];
    if (folders) problems.push("папки не вкладываются, только файлы");
    let total = attachments.reduce((s, a) => s + a.size, 0);
    dropSaving += files.length;
    for (const f of files) {
      try {
        if (f.size > FILE_MAX) problems.push(`«${f.name}» больше 25 МБ`);
        else if (total + f.size > TOTAL_MAX) problems.push(`«${f.name}» не поместился: вложения вместе больше 35 МБ`);
        else {
          const picked = await api.attachmentDrop(f);
          if (!alive) return;
          attachments = [...attachments, picked];
          total += picked.size;
        }
      } catch (e) {
        problems.push(`«${f.name}»: ${errorMessage(e)}`);
      } finally {
        dropSaving--;
      }
    }
    if (problems.length && alive) {
      error = `Не всё вложено: ${problems.join("; ")}.`;
      lastAction = null;
    }
  }

  $effect(() => {
    const el = root;
    if (!el) return;
    el.addEventListener("dragenter", onFilesEnter, true);
    el.addEventListener("dragover", onFilesOver, true);
    el.addEventListener("dragleave", onFilesLeave, true);
    el.addEventListener("drop", onFilesDrop, true);
    return () => {
      el.removeEventListener("dragenter", onFilesEnter, true);
      el.removeEventListener("dragover", onFilesOver, true);
      el.removeEventListener("dragleave", onFilesLeave, true);
      el.removeEventListener("drop", onFilesDrop, true);
    };
  });
  function removeServerFile(serverId: string) {
    if (!removedServerIds.includes(serverId)) removedServerIds = [...removedServerIds, serverId];
  }

  // --- клавиши: Ctrl+Enter — отправить, Ctrl+S — сохранить черновик, Esc — отменить ----------
  // Слушаем окно и проверяем, что событие пришло изнутри композера: у всплывающих окон внутри
  // (подсказки адресатов, адрес ссылки) свой Esc, они его останавливают сами. Пока на экране слой
  // (подпись, вопрос об отмене, меню адресата, карточка контакта, настройки), клавиши — его:
  // Ctrl+Enter в меню чипа не отправляет письмо.

  function onWindowKey(e: KeyboardEvent) {
    // Клавишу уже обработала панель «Совета» (Ctrl+Enter там — «Посоветовать», а не «Отправить»).
    if (e.defaultPrevented) return;
    if (!root || !(e.target instanceof Node) || !root.contains(e.target)) return;
    if (sigOpen || cancelAsk || layerOpen()) return;
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

<section class="composer" class:standalone bind:this={root} tabindex="-1" aria-label={title} aria-busy={busy != null}>
  {#if dropActive}
    <!-- Подсказка, пока над письмом тянут файлы (п. 37); события мыши проходят сквозь неё. -->
    <div class="drop-veil" aria-hidden="true"><span class="drop-hint">{busy ? "Подожди: письмо отправляется или сохраняется" : "Отпусти файлы — они станут вложениями"}</span></div>
  {/if}
  <div class="bar" bind:this={barEl} data-squeeze={squeeze}>
    <span class="kind">{title}</span>
    <!-- В узкой области чтения подписи трёх кнопок по очереди уступают место значкам (fitBar): строка остаётся одной. -->
    <button class="btn primary act-send" onclick={send} disabled={busy != null} title={hint("Отправить", "composer-send")} aria-label="Отправить">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M2 8 14 2.5 11.5 14 8.5 9.5 2 8Zm6.5 1.5L12 5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" /></svg>
      <span class="act-label">Отправить</span>
    </button>
    <button class="btn act-save" onclick={saveDraft} disabled={busy != null} title={hint("Сохранить черновик на сервере", "composer-save")} aria-label="Сохранить черновик">
      <svg class="act-icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.save} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
      <span class="act-label">Сохранить черновик</span>
    </button>
    <button class="btn act-cancel" bind:this={cancelBtn} onclick={() => (cancelAsk ? keepWriting() : requestCancel())} disabled={busy != null} title={hint("Отменить", "composer-cancel")} aria-label="Отменить" aria-haspopup="dialog" aria-expanded={cancelAsk}>
      <svg class="act-icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.close} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /></svg>
      <span class="act-label">Отменить</span>
    </button>
    {#if busy}
      <span class="progress caption" aria-live="polite">
        <span class="spinner" aria-hidden="true"></span>
        {busy === "send" ? "Отправляю…" : droppingStray ? "Закрываю…" : "Сохраняю черновик…"}
      </span>
    {/if}
    <span class="spacer"></span>
    <!-- Важность (п. 35): нажата одна из двух или ни одной; значками, как и вложение с подписью. -->
    <button class="btn icon" class:on={importance === "High"} class:high={importance === "High"} onclick={() => toggleImportance("High")} disabled={busy != null} aria-pressed={importance === "High"} title="Высокая важность" aria-label="Высокая важность">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.important} fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" /></svg>
    </button>
    <button class="btn icon" class:on={importance === "Low"} onclick={() => toggleImportance("Low")} disabled={busy != null} aria-pressed={importance === "Low"} title="Низкая важность" aria-label="Низкая важность">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.lowImportance} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
    <button class="btn icon" onclick={pickFiles} disabled={busy != null} title="Вложить файл…" aria-label="Вложить файл">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
    </button>
    <button class="btn icon" bind:this={sigBtn} onclick={() => (sigOpen = true)} disabled={busy != null} title="Подпись: текст и автодобавление" aria-label="Подпись" aria-haspopup="dialog">
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.signature} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
    {#if adviceKind && adviceStatus?.available}
      <button class="btn" class:on={adviceOpen} onclick={() => (adviceOpen = !adviceOpen)} disabled={busy != null} aria-expanded={adviceOpen} title="Совет: суть переписки и черновик ответа">Совет</button>
    {/if}
    {#if !standalone}
      <button class="btn icon" onclick={() => void popOut()} disabled={busy != null || poppingOut} title="Редактировать в отдельном окне" aria-label="Редактировать в отдельном окне">
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.popout} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
    {/if}
  </div>

  {#if upload}
    <!-- Вложения уходят на сервер (п. 39): сколько уже отдано и полоса. -->
    <div class="upload caption" role="status">
      <span class="upload-text">{upload.sent < upload.total ? `Загружаю вложения: ${formatSize(upload.sent)} из ${formatSize(upload.total)} · ${uploadPercent} %` : "Вложения загружены, сервер принимает письмо…"}</span>
      <div class="upload-track" role="progressbar" aria-label="Загрузка вложений" aria-valuemin="0" aria-valuemax="100" aria-valuenow={uploadPercent}>
        <div class="upload-fill" style="width: {uploadPercent}%"></div>
      </div>
    </div>
  {/if}

  {#if error}
    <div class="error" role="alert">
      <span class="error-text">{error}</span>
      {#if lastAction}<button class="btn small" onclick={retry} disabled={busy != null}>Повторить</button>{/if}
      {#if quoteGone && quoteGone !== "autosave"}<button class="btn small" onclick={continueWithoutQuote} disabled={busy != null}>{quoteGoneLabel}</button>{/if}
      <button class="btn small" onclick={() => (error = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
    </div>
  {/if}

  <div class="fields">
    <div class="row">
      <label class="lbl" for="{uid}-to">Кому</label>
      <RecipientsInput bind:this={toField} id="{uid}-to" bind:recipients={to} disabled={busy != null} placeholder="Адрес или имя" field="to" dragGroup={uid} moveTargets={MOVE_TARGETS.to} ontransfer={(r, t, i, c) => transferRecipient("to", r, t, i, c)} />
      {#if !showBcc}
        <button class="link-btn" onclick={() => (showBcc = true)} disabled={busy != null}>Скрытая</button>
      {/if}
    </div>
    {#if recipientsUnknown}
      <div class="warn caption" role="status">Получатели исходного письма не загружены — проверь «Кому» и «Копия».</div>
    {/if}
    <div class="row">
      <label class="lbl" for="{uid}-cc">Копия</label>
      <RecipientsInput bind:this={ccField} id="{uid}-cc" bind:recipients={cc} disabled={busy != null} field="cc" dragGroup={uid} moveTargets={MOVE_TARGETS.cc} ontransfer={(r, t, i, c) => transferRecipient("cc", r, t, i, c)} />
    </div>
    {#if showBcc || draggingRecipient}
      <div class="row">
        <label class="lbl" for="{uid}-bcc">Скрытая</label>
        <RecipientsInput bind:this={bccField} id="{uid}-bcc" bind:recipients={bcc} disabled={busy != null} field="bcc" dragGroup={uid} moveTargets={MOVE_TARGETS.bcc} ontransfer={(r, t, i, c) => transferRecipient("bcc", r, t, i, c)} />
      </div>
    {/if}
    <div class="row">
      <label class="lbl" for="{uid}-subject">Тема</label>
      <input id="{uid}-subject" class="subject" type="text" bind:value={subject} disabled={busy != null} placeholder="Тема письма" autocomplete="off" />
    </div>
  </div>

  {#if attachments.length || keptServerFiles.length || originalFiles.length || dropSaving > 0}
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
      {#if dropSaving > 0}
        <span class="file adding" role="status">
          <span class="spinner small" aria-hidden="true"></span>
          <span class="file-name">{dropSaving > 1 ? `Добавляю файлы: ${dropSaving}…` : "Добавляю файл…"}</span>
        </span>
      {/if}
      {#each originalFiles as a (a.attachment_pk)}
        <span class="file inherited" title="Вложение исходного письма: сервер добавит его к пересылке сам">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
          <span class="file-name">{a.name ?? "вложение"}</span>
          <span class="caption file-size">{formatSize(a.size)}</span>
        </span>
      {/each}
    </div>
  {/if}

  {#if adviceOpen && adviceKind && adviceRef != null && adviceStatus}
    <AdvicePanel kind={adviceKind} referencePk={adviceRef} status={adviceStatus} typed={() => (isDirty ? bodyText : "")} canReplace={!quoteInText} oninsert={insertAdvice} onclose={() => (adviceOpen = false)} />
  {/if}

  <div class="editor-wrap">
    <Editor bind:this={editor} bind:value={body} disabled={busy != null} minHeight={editorMinHeight} />
  </div>

  <div class="foot caption" aria-live="polite">
    {#if autosaving}
      <span class="spinner small" aria-hidden="true"></span>
      <span>Сохраняю…</span>
    {:else if autosaveNetwork}
      <span class="foot-failed">Нет связи: черновик пока только в этом окне, повторю сам</span>
    {:else if autosaveError}
      <span class="foot-failed">Черновик не сохранён: {autosaveError}</span>
      {#if quoteGone === "autosave"}<button class="btn small" onclick={continueWithoutQuote} disabled={busy != null}>{quoteGoneLabel}</button>{/if}
    {:else if savedAt != null}
      <span>Черновик сохранён в {timeOnly.format(new Date(savedAt))}{pendingFiles ? " · изменения вложений ещё не сохранены" : ""}</span>
    {:else if draftRef}
      <span>Черновик с сервера{isDirty ? " · есть несохранённые правки" : ""}</span>
    {:else if isDirty && nonEmpty}
      <span>Черновик сохранится автоматически</span>
    {/if}
  </div>

  {#if original && !quoteInText}
    <!-- Цитата свёрнута: так она и уйдёт (её добавит сервер). «Развернуть» кладёт её в редактор,
         где её можно править и удалять (п. 27). -->
    <section class="original" aria-label="Исходное письмо">
      <button class="orig-toggle" onclick={() => void expandQuote()} disabled={busy != null} title="Вставить исходное письмо в текст: цитату можно будет править и удалять">
        {#if expanding}
          <span class="spinner small" aria-hidden="true"></span>
        {:else}
          <svg class="chev" width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M4 2.5 7.5 6 4 9.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {/if}
        <span class="orig-title">Исходное письмо</span>
        <span class="caption orig-hint">{originalSummary}</span>
        <span class="orig-action">{expanding ? "Разворачиваю…" : "Развернуть"}</span>
      </button>
      {#if origError}
        <div class="orig-note failed">{origError}</div>
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
  .composer:focus {
    outline: none;
  }
  .composer {
    flex: none;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-2);
    min-width: 0;
    /* Подсказка о брошенных файлах ложится поверх письма. */
    position: relative;
  }
  .drop-veil {
    position: absolute;
    inset: 0;
    z-index: 3;
    display: flex;
    align-items: center;
    justify-content: center;
    border: 2px dashed var(--accent);
    border-radius: inherit;
    background: color-mix(in srgb, var(--bg-card) 82%, transparent);
    pointer-events: none;
  }
  .drop-hint {
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-card);
    background: var(--bg-card);
    box-shadow: var(--shadow-8);
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  .file.adding {
    padding-right: 10px;
    color: var(--fg-2);
  }
  /* Окно черновика (п. 32): композер — всё окно, без рамки карточки. */
  .composer.standalone {
    border: 0;
    border-radius: 0;
    box-shadow: none;
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
  /*
   * Узкая область чтения (п. 38 обкатки): чтобы кнопки не переносились на вторую строку, подписи
   * уступают место значкам — сначала «Сохранить черновик» (уровень 1), потом «Отменить» (2), потом
   * «Отправить» (3). Уровень подбирает fitBar по настоящим размерам.
   */
  .act-icon {
    display: none;
  }
  .bar:is([data-squeeze="1"], [data-squeeze="2"], [data-squeeze="3"]) .act-save,
  .bar:is([data-squeeze="2"], [data-squeeze="3"]) .act-cancel {
    width: var(--control-height);
    padding: 0;
    justify-content: center;
  }
  .bar:is([data-squeeze="1"], [data-squeeze="2"], [data-squeeze="3"]) .act-save .act-label,
  .bar:is([data-squeeze="2"], [data-squeeze="3"]) .act-cancel .act-label,
  .bar[data-squeeze="3"] .act-send .act-label {
    display: none;
  }
  .bar:is([data-squeeze="1"], [data-squeeze="2"], [data-squeeze="3"]) .act-save .act-icon,
  .bar:is([data-squeeze="2"], [data-squeeze="3"]) .act-cancel .act-icon {
    display: block;
  }
  .bar[data-squeeze="3"] .act-send {
    width: 44px;
    padding: 0;
    justify-content: center;
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
  .btn.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  .btn.on.high {
    border-color: var(--danger);
    color: var(--danger);
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
  .btn.icon {
    width: var(--control-height);
    padding: 0;
    justify-content: center;
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
  /* Ход загрузки вложений (п. 39): строка под кнопками. */
  .upload {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
    color: var(--fg-2);
  }
  .upload-text {
    flex: none;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .upload-track {
    flex: 1;
    min-width: 60px;
    height: 4px;
    border-radius: 2px;
    background: var(--stroke-1);
    overflow: hidden;
  }
  .upload-fill {
    height: 100%;
    border-radius: 2px;
    background: var(--accent);
    transition: width var(--dur-normal) linear;
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
  .orig-toggle:disabled {
    background: none;
  }
  .chev {
    flex: none;
  }
  .orig-title {
    flex: none;
  }
  .orig-hint {
    flex: 1;
    min-width: 0;
    margin-left: var(--space-2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .orig-action {
    flex: none;
    color: var(--accent);
  }
  .orig-toggle:disabled .orig-action {
    color: var(--fg-3);
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
