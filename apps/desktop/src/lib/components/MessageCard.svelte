<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api, errorMessage, formatSize, fullDate, isInlinePicture, listDate, messageNeedsFetch, personLabel, type Attachment, type ComposeKind, type Folder, type Message } from "../api";
  import { COMPOSE_ACTIONS, ICON, type ItemAction } from "../actions";
  import { focusBack } from "../popover";
  import FolderPicker from "./FolderPicker.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";
  import BodyFrame from "./BodyFrame.svelte";

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
    /** Действие над этим письмом; промис отклоняется текстом ошибки для пользователя. */
    onaction: (action: ItemAction) => Promise<void>;
    /** Открыть композер: ответ, ответ всем или пересылка этого письма. */
    oncompose: (kind: ComposeKind) => void;
  }
  let { message, expanded, ontoggle, onfetch, folders, deletedFolderPk, onaction, oncompose }: Props = $props();

  const to = $derived(message.recipients.filter((r) => r.kind === 1).map((r) => personLabel(r.name, r.email)));
  const cc = $derived(message.recipients.filter((r) => r.kind === 2).map((r) => personLabel(r.name, r.email)));
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
  let deleteBtn = $state<HTMLButtonElement | null>(null);

  async function run(action: ItemAction) {
    if (busy) return;
    // Ручная пометка отменяет автопометку: иначе таймер снова сделал бы письмо прочитанным.
    if (action.kind === "read") autoMarked = true;
    busy = true;
    actionError = null;
    try {
      await onaction(action);
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
  // Непрочитанное письмо, развёрнутое и видимое 1,5 с, помечается прочитанным (как в OWA).
  // Делается один раз на карточку: если пользователь вернул «непрочитано» (кнопкой карточки, меню
  // списка или из другого клиента), таймер не повторяется. Идёт «тихо»: отказ (нет связи, нет
  // пароля) только в консоль — это не действие пользователя, а строка состояния уже показывает
  // ту же проблему; локальная пометка на бэкенде откатывается, точка непрочитанного остаётся.

  let cardEl = $state<HTMLElement | null>(null);
  let onScreen = $state(false);
  let autoMarked = false;
  // Нужно именно начальное значение: дальше его ведёт эффект автопометки.
  let lastRead = untrack(() => message.is_read);
  const AUTO_READ_MS = 1500;

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
    // другой клиент): автопометку не повторяем, иначе через 1,5 с она отменила бы действие.
    if (lastRead && !read) autoMarked = true;
    lastRead = read;
    if (!expanded || !onScreen || read || autoMarked) return;
    const t = setTimeout(() => {
      if (document.visibilityState !== "visible" || autoMarked || busy) return;
      void autoRead();
    }, AUTO_READ_MS);
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

  function initials(name: string | null, email: string | null): string {
    const s = (name || email || "?").trim();
    const parts = s.split(/\s+/).filter(Boolean);
    return parts.length >= 2 ? (parts[0][0] + parts[1][0]).toUpperCase() : s.slice(0, 2).toUpperCase();
  }
</script>

<article class="card" class:expanded class:unread={!message.is_read} class:flagged={message.is_flagged} bind:this={cardEl}>
  <div class="head">
    <div class="head-main" onclick={ontoggle} role="button" tabindex="0" onkeydown={(e) => (e.key === "Enter" || e.key === " ") && ontoggle()} aria-expanded={expanded}>
      <div class="avatar" aria-hidden="true">{initials(message.from_name, message.from_email)}</div>
      <div class="who">
        <div class="from">
          <span class="name">{personLabel(message.from_name, message.from_email) || "(без отправителя)"}</span>
          {#if message.from_name && message.from_email}<span class="email caption">{message.from_email}</span>{/if}
          {#if message.importance === "High"}<span class="badge high">важное</span>{/if}
          {#if message.is_draft}<span class="badge">черновик</span>{/if}
        </div>
        {#if expanded}
          <div class="line caption">Кому: {to.length ? to.join(", ") : message.display_to || "—"}</div>
          {#if cc.length}<div class="line caption">Копия: {cc.join(", ")}</div>{/if}
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

    {#if expanded}
      <div class="actions" role="toolbar" aria-label="Действия с письмом" aria-busy={busy}>
        {#each COMPOSE_ACTIONS as a (a.kind)}
          <button class="act" onclick={() => oncompose(a.kind)} aria-label={a.label} title="{a.label} ({a.keys})">
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={a.icon} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
        {/each}
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

  {#if expanded}
    {#if files.length}
      <div class="files" aria-label="Вложения">
        {#each files as a (a.attachment_pk)}
          {@const st = status(a.attachment_pk)}
          {@const cached = a.cached || cachedHere.has(a.attachment_pk)}
          {@const name = a.name ?? "вложение"}
          <div class="file" class:cached class:busy={st.busy != null} class:failed={st.error != null} title={[a.content_type ?? "", cached ? "Файл в кэше" : "Файл будет скачан при открытии"].filter(Boolean).join("\n")}>
            <button class="file-open" onclick={() => openAttachment(a)} disabled={st.busy != null} aria-label="Открыть вложение {name}" aria-busy={st.busy === "open"}>
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
        <BodyFrame html={message.body_html ?? ""} />
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
  .act-sep {
    width: 1px;
    height: 16px;
    margin: 0 4px;
    background: var(--stroke-2);
  }
  .action-error {
    border-top: 1px solid var(--stroke-1);
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
