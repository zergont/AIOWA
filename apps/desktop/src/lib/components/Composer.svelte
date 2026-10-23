<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, errorMessage, formatSize, fullDate, isInlinePicture, messageNeedsFetch, personLabel, type ComposeContext, type ComposeRecipient, type ComposeRequest, type Message, type PickedFile, type SignatureDto } from "../api";
  import { COMPOSE_TITLE, ICON } from "../actions";
  import { isValidEmail, mergeRecipients } from "../addresses";
  import { htmlToText } from "../html";
  import { focusBack } from "../popover";
  import RecipientsInput from "./RecipientsInput.svelte";
  import Editor from "./Editor.svelte";
  import BodyFrame from "./BodyFrame.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";
  import SignatureDialog from "./SignatureDialog.svelte";

  interface Props {
    /** Заготовка от бэкенда; новая заготовка — новый экземпляр композера (родитель оборачивает в {#key}). */
    ctx: ComposeContext;
    /** Есть ли несохранённые правки (родитель спрашивает подтверждение, прежде чем открыть другое письмо). */
    dirty?: boolean;
    /** Идёт отправка или сохранение черновика: родитель не заменяет композер и не даёт закрыть окно. */
    busy?: "send" | "draft" | null;
    onclose: (reason: "sent" | "draft" | "cancel") => void;
  }
  let { ctx, dirty = $bindable(false), busy = $bindable(null), onclose }: Props = $props();

  // Экземпляр мог быть уничтожен, пока шёл запрос: его итог тогда никому не нужен, и уж точно
  // нельзя звать onclose — родитель закрыл бы уже другой композер.
  let alive = true;
  onDestroy(() => (alive = false));

  const uid = $props.id();
  const title = $derived(COMPOSE_TITLE[ctx.kind]);

  /** Лимиты вложений те же, что на бэкенде: 25 МиБ на файл, 35 МиБ всего. */
  const FILE_MAX = 25 * 1024 * 1024;
  const TOTAL_MAX = 35 * 1024 * 1024;

  // --- состояние письма (из заготовки, дальше живёт своей жизнью) -------------------------

  /** Начальный текст: пустая строка под каретку и, если включено, подпись через пустую строку (как в OWA). */
  function initialBody(sig: SignatureDto): string {
    const s = sig.auto_add ? sig.html.trim() : "";
    return s ? `<div><br></div><div><br></div>${s}` : "<div><br></div>";
  }

  // Без дублей: чипы идут в keyed each по адресу.
  let to = $state<ComposeRecipient[]>(untrack(() => mergeRecipients([], ctx.to)));
  let cc = $state<ComposeRecipient[]>(untrack(() => mergeRecipients([], ctx.cc)));
  let bcc = $state<ComposeRecipient[]>([]);
  let showBcc = $state(false);
  let subject = $state(untrack(() => ctx.subject));
  let body = $state(untrack(() => initialBody(ctx.signature)));
  let attachments = $state<PickedFile[]>([]);
  let signature = $state<SignatureDto>(untrack(() => ctx.signature));
  let original = $state<Message | null>(untrack(() => ctx.original));

  let error = $state<string | null>(null);
  let lastAction = $state<"send" | "draft" | null>(null);
  let confirmCancel = $state(false);
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
  const init = untrack(() => ({ subject: ctx.subject, bodyText: htmlToText(initialBody(ctx.signature)), to: keyOf(mergeRecipients([], ctx.to)), cc: keyOf(mergeRecipients([], ctx.cc)) }));
  const isDirty = $derived(subject !== init.subject || keyOf(to) !== init.to || keyOf(cc) !== init.cc || bcc.length > 0 || attachments.length > 0 || htmlToText(body) !== init.bodyText);
  $effect(() => {
    dirty = isDirty;
  });

  /** Файлы исходного письма при пересылке: их добавит сервер, здесь только показываем. */
  const originalFiles = $derived(ctx.kind === "forward" && original ? original.attachments.filter((a) => !isInlinePicture(a)) : []);
  const originalTo = $derived(original ? original.recipients.filter((r) => r.kind === 1).map((r) => personLabel(r.name, r.email)) : []);
  /**
   * «Ответить всем», а получатели исходного письма в базе неизвестны: они приходят только вместе с
   * телом, и пока оно не загружено (или сервер его не отдал), в заготовке один отправитель.
   */
  const recipientsUnknown = $derived(ctx.kind === "reply_all" && original != null && original.body_state !== 1 && original.recipients.length === 0 && !origLoading);

  onMount(() => {
    root?.scrollIntoView({ block: "start" });
    // Ответу нужен текст, остальным — адресат.
    if (ctx.kind === "reply" || ctx.kind === "reply_all") editor?.focus();
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
    if (ctx.kind !== "reply_all" || ctx.reference_item_pk == null) return;
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
      body_html: body,
      attachments: attachments.map((a) => a.path),
    };
  }

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
      const rep = await api.composeSend(request());
      if (!alive) return;
      if (rep.sent) {
        onclose("sent");
        return;
      }
      error = rep.message ?? (rep.draft_saved ? "Письмо не отправлено, черновик сохранён на сервере в «Черновиках»" : "Письмо не отправлено.");
      // Черновик уже на сервере: «Повторить» создал бы второй, а при неизвестном исходе SendItem —
      // ещё и дубль адресатам. Осознанная повторная отправка остаётся через «Отправить».
      if (rep.draft_saved || rep.maybe_sent) lastAction = null;
    } catch (e) {
      if (!alive) return;
      error = errorMessage(e);
    } finally {
      if (alive) busy = null;
    }
  }

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
      const rep = await api.composeSaveDraft(request());
      if (!alive) return;
      // Закрываем только при чистом успехе: с текстом (черновик без вложений) композер остаётся,
      // чтобы пользователь увидел, чего в черновике нет, и не потерял выбранные файлы.
      if (rep.draft_saved && !rep.message) {
        onclose("draft");
        return;
      }
      error = rep.message ?? "Черновик не сохранён.";
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
    if (isDirty) confirmCancel = true;
    else onclose("cancel");
  }
  function confirmCancelNow() {
    confirmCancel = false;
    onclose("cancel");
  }
  function keepWriting() {
    confirmCancel = false;
    focusBack(cancelBtn);
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

  // --- клавиши: Ctrl+Enter — отправить, Esc — отменить ------------------------------------
  // Слушаем окно и проверяем, что событие пришло изнутри композера: у всплывающих окон внутри
  // (подсказки адресатов, адрес ссылки) свой Esc, они его останавливают сами.

  function onWindowKey(e: KeyboardEvent) {
    if (!root || !(e.target instanceof Node) || !root.contains(e.target)) return;
    if (sigOpen || confirmCancel) return;
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      void send();
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
    <button class="btn" onclick={saveDraft} disabled={busy != null} title="Сохранить черновик на сервере">Сохранить черновик</button>
    <button class="btn" bind:this={cancelBtn} onclick={() => (confirmCancel ? keepWriting() : requestCancel())} disabled={busy != null} title="Отменить (Esc)" aria-haspopup="dialog" aria-expanded={confirmCancel}>Отменить</button>
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

  {#if attachments.length || originalFiles.length}
    <div class="files" aria-label="Вложения">
      {#each attachments as a (a.path)}
        <span class="file" title={a.path}>
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

  {#if confirmCancel && cancelBtn}
    <ConfirmPopover anchor={cancelBtn} align="start" title="Отменить текущее письмо?" text="Набранный текст будет потерян." confirmLabel="Отменить письмо" onconfirm={confirmCancelNow} oncancel={keepWriting} />
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

  /* --- редактор и оригинал --- */
  .editor-wrap {
    padding: var(--space-2) var(--space-3) var(--space-3);
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
