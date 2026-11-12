<script lang="ts">
  /**
   * Окно беседы (п. 32 обкатки): письмо — своё окно Windows. Открывается кнопкой «Открыть в отдельном
   * окне» в области чтения и двойным щелчком по беседе в списке и в результатах поиска. Внутри — та
   * же область чтения: карточки писем с их действиями (прочитано, флаг, перенос, архив, удаление с
   * «Отменить»), ответ и пересылка — композером здесь же, его можно унести в своё окно. Беседа
   * перечитывается после записей и по ходу синхронизации.
   */
  import { onMount } from "svelte";
  import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
  import { api, composeFinished, conversationKeyOf, errorMessage, messagesKey, onCloseRequested, onFoldersChanged, onItemsChanged, onSyncLive, onWriteFailed, type ComposeContext, type ComposeKind, type Conversation, type Folder, type Message, type StageAction, type WriteReport } from "../api";
  import { failureText, findArchiveFolder, ICON, type ItemAction } from "../actions";
  import { preventBrowserKeys } from "../hotkeys";
  import Composer, { type CloseReason } from "./Composer.svelte";
  import ReadingPane from "./ReadingPane.svelte";
  import WindowTitleBar from "./WindowTitleBar.svelte";

  interface Props {
    conversationPk: number;
  }
  let { conversationPk }: Props = $props();

  const win = getCurrentWebviewWindow();

  let messages = $state<Message[]>([]);
  let loading = $state(true);
  let folders = $state<Folder[]>([]);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;
  let topExpandedPk = $state<number | null>(null);

  const deletedFolderPk = $derived(folders.find((f) => f.distinguished === "deleteditems")?.folder_pk ?? null);
  const draftsFolderPk = $derived(folders.find((f) => f.distinguished === "drafts")?.folder_pk ?? null);
  const archiveFolder = $derived(findArchiveFolder(folders));

  /** Тема беседы — по самому раннему письму: ответ или черновик сверху не делает её «RE: …». */
  const subject = $derived(messages[messages.length - 1]?.subject?.trim() || messages[0]?.subject?.trim() || "(без темы)");
  /** Заглушка беседы для области чтения. */
  const conversation = $derived<Conversation>({ conversation_pk: conversationPk, topic: subject, participants: [], item_count: messages.length, unread_count: messages.filter((m) => !m.is_read).length, total_count: messages.length, last_received_ms: null, has_attachments: false, has_flagged: false });
  $effect(() => {
    if (!messages.length) return;
    const t = subject;
    void win.setTitle(t).catch(() => {});
  });

  function showNotice(text: string) {
    notice = text;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), 5000);
  }

  // --- беседа -----------------------------------------------------------------------------

  async function load() {
    try {
      messages = await api.conversation(conversationPk);
    } catch (e) {
      error = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  async function loadFolders() {
    try {
      folders = await api.folders();
    } catch (e) {
      console.warn("folders:", errorMessage(e));
    }
  }

  /** Перечитать беседу целиком: после записи (перенос и его отмена ключ беседы не меняют — в нём нет папки). */
  async function reread() {
    if (loading) return;
    try {
      messages = await api.conversation(conversationPk);
    } catch (e) {
      console.warn("conversation:", errorMessage(e));
    }
  }

  /** По ходу синхронизации: перечитать беседу, если она изменилась (дешёвый ключ без тел, как у главного окна). */
  async function rereadIfChanged() {
    if (loading) return;
    try {
      const keys = await api.conversationKey(conversationPk);
      const withFlag = keys.every((k) => k.is_flagged !== undefined);
      if (conversationKeyOf(keys, withFlag) !== messagesKey(messages, withFlag)) messages = await api.conversation(conversationPk);
    } catch (e) {
      console.warn("conversation_key:", errorMessage(e));
    }
  }

  let rereadTimer: ReturnType<typeof setTimeout> | undefined;
  let rereadFull = false;
  function scheduleReread(ms: number, full: boolean) {
    rereadFull ||= full;
    clearTimeout(rereadTimer);
    rereadTimer = setTimeout(() => {
      const f = rereadFull;
      rereadFull = false;
      void (f ? reread() : rereadIfChanged());
    }, ms);
  }

  /** Запросы message_fetch в полёте: повторный клик не плодит сетевых вызовов. */
  const fetching = new Map<number, Promise<Message>>();
  function fetchMessage(itemPk: number): Promise<void> {
    let p = fetching.get(itemPk);
    if (!p) {
      p = api.messageFetch(itemPk).finally(() => fetching.delete(itemPk));
      fetching.set(itemPk, p);
    }
    return p.then((fresh) => {
      const i = messages.findIndex((m) => m.item_pk === itemPk);
      if (i >= 0) messages[i] = fresh;
    });
  }

  // --- действия над письмами -----------------------------------------------------------------
  // Как в главном окне: прочитано и флаг — сразу, удаление в «Удалённые», перенос и архив — с окном
  // отмены (бэкенд ждёт 6 с, «Отменить» откатывает), удаление навсегда — сразу.

  function targetFolderPk(a: ItemAction): number | null {
    if (a.kind === "move") return a.folderPk;
    if (a.kind === "archive") return archiveFolder?.folder_pk ?? null;
    if (a.kind === "delete" && !a.permanent) return deletedFolderPk;
    return null;
  }

  function stageActionOf(a: ItemAction): StageAction | null {
    if (a.kind === "delete") return a.permanent ? null : "delete";
    if (a.kind === "move" || a.kind === "archive") return a.kind;
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

  /** Окно меняется сразу; items-changed потом сверит с базой. */
  function applyToMessages(itemPks: number[], a: ItemAction) {
    const set = new Set(itemPks);
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
        m.folder_name = folders.find((f) => f.folder_pk === target)?.name ?? m.folder_name;
      }
    }
  }

  interface Staged {
    id: string;
    text: string;
  }
  let staged = $state<Staged | null>(null);
  let stageTimer: ReturnType<typeof setTimeout> | undefined;

  /** Предыдущее действие с окном отмены уходит на сервер сейчас. */
  function commitStaged() {
    const s = staged;
    if (!s) return;
    staged = null;
    clearTimeout(stageTimer);
    api
      .itemsCommit(s.id)
      .then((r) => {
        const failed = r ? failureText(r) : null;
        if (failed) error = failed;
      })
      .catch((e) => (error = errorMessage(e)));
  }

  async function undoStaged() {
    const s = staged;
    if (!s) return;
    staged = null;
    clearTimeout(stageTimer);
    try {
      const failed = failureText(await api.itemsUndo(s.id));
      if (failed) error = failed;
      showNotice("Отменено");
    } catch (e) {
      error = errorMessage(e);
    }
    await reread();
  }

  /** Действие над письмом с карточки; отказ — исключение с текстом, карточка покажет его у себя. */
  async function onaction(itemPk: number, a: ItemAction): Promise<void> {
    if (a.kind === "archive" && !archiveFolder) throw "Папки «Архив» нет: создайте её в главном окне AIOWA";
    const action = stageActionOf(a);
    applyToMessages([itemPk], a);
    if (!action) {
      let report: WriteReport;
      try {
        report = await callWrite([itemPk], a);
      } catch (e) {
        await reread();
        throw errorMessage(e);
      }
      const failed = failureText(report);
      if (failed) throw failed;
      // Удалено навсегда последнее письмо беседы: окну больше нечего показывать.
      if (a.kind === "delete" && !messages.length) void win.close().catch(() => {});
      return;
    }
    commitStaged();
    try {
      const r = await api.itemsStage(action, [itemPk], action === "delete" ? null : targetFolderPk(a), false);
      if (r.done > 0) {
        const name = folders.find((f) => f.folder_pk === targetFolderPk(a))?.name;
        const text = a.kind === "delete" ? "Письмо удалено" : name ? `Письмо перенесено в «${name}»` : "Письмо перенесено";
        staged = { id: r.stage_id, text };
        clearTimeout(stageTimer);
        const left = Number.isFinite(r.undo_until_ms - Date.now()) ? Math.min(Math.max(r.undo_until_ms - Date.now() - 250, 0), 120_000) : 5750;
        stageTimer = setTimeout(() => {
          if (staged?.id === r.stage_id) staged = null;
        }, left);
      }
      const failed = failureText(r);
      if (failed) throw failed;
    } catch (e) {
      await reread();
      throw errorMessage(e);
    }
  }

  // --- ответ и пересылка ------------------------------------------------------------------

  let compose = $state<ComposeContext | null>(null);
  let composeLoading = $state(false);
  let composerDirty = $state(false);
  let composerBusy = $state<"send" | "draft" | null>(null);
  let composerRef = $state<ReturnType<typeof Composer> | null>(null);

  async function oncompose(itemPk: number, kind: ComposeKind) {
    if (composeLoading || composerBusy) return;
    if (compose && composerDirty) {
      const reason = composerRef ? await composerRef.askClose() : "cancel";
      if (reason == null) return;
    }
    composeLoading = true;
    try {
      compose = await api.composeContext(kind, itemPk);
      composerDirty = false;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      composeLoading = false;
    }
  }

  function closeCompose(reason: CloseReason) {
    compose = null;
    composerDirty = false;
    composerBusy = null;
    if (reason === "sent") showNotice("Отправлено");
    else if (reason === "draft") showNotice("Черновик сохранён в «Черновиках» на сервере");
    else if (reason === "discard") showNotice("Черновик удалён");
    else if (reason === "local") showNotice("Нет связи: письмо сохранено в приложении, открыть его можно в главном окне");
    else if (reason === "window") showNotice("Письмо открыто в отдельном окне");
    // Синхронизация «Отправленных» и «Черновиков» — забота главного окна.
    if (reason !== "cancel" && reason !== "window") void composeFinished(reason).catch((e) => console.warn("compose-finished:", errorMessage(e)));
  }

  /** Черновик с карточки — в своё окно черновика. */
  async function openDraft(itemPk: number) {
    try {
      await api.composeWindow(null, itemPk);
    } catch (e) {
      error = errorMessage(e);
    }
  }

  // --- закрытие окна при несохранённом ответе ---------------------------------------------

  let closing = $state(false);
  const guarded = $derived(!closing && compose !== null && (composerDirty || composerBusy !== null));
  $effect(() => {
    const g = guarded;
    api.windowCloseGuard(g).catch((e) => console.warn("window_close_guard:", errorMessage(e)));
  });

  async function onCloseRequest() {
    if (guarded && composerRef) {
      const ref = composerRef;
      const reason = await ref.askClose();
      if (reason == null) return;
      // Удаление локального черновика должно дойти до базы раньше, чем окно закроется.
      await ref.localSettled();
    }
    closing = true;
    try {
      await api.windowCloseConfirmed();
    } catch {
      await win.close().catch(() => {});
    }
  }

  onMount(() => {
    void load();
    void loadFolders();
    const offs = [
      onCloseRequested(() => void onCloseRequest()),
      onItemsChanged((ev) => {
        if (ev.conversation_pks.includes(conversationPk)) scheduleReread(100, true);
        if (ev.folder_pks.length) void loadFolders();
      }),
      onSyncLive(() => scheduleReread(1500, false)),
      onFoldersChanged(() => void loadFolders()),
      onWriteFailed((ev) => {
        if (ev.item_pks.some((pk) => messages.some((m) => m.item_pk === pk))) {
          error = ev.message;
          scheduleReread(100, true);
        }
      }),
    ];
    return () => {
      clearTimeout(rereadTimer);
      clearTimeout(stageTimer);
      clearTimeout(noticeTimer);
      for (const off of offs) void off.then((f) => f());
    };
  });
</script>

<svelte:window onkeydowncapture={preventBrowserKeys} />

{#snippet composerSnippet()}
  {#key compose}
    {#if compose}
      <Composer bind:this={composerRef} ctx={compose} bind:dirty={composerDirty} bind:busy={composerBusy} onclose={closeCompose} />
    {/if}
  {/key}
{/snippet}

<div class="window">
  <WindowTitleBar title={messages.length ? subject : "Письмо"} />
  <main class="body">
    {#if !loading && !messages.length && !compose}
      <div class="empty">
        <p class="caption">{error ?? "В беседе больше нет писем"}</p>
        <button class="btn" onclick={() => void onCloseRequest()}>Закрыть</button>
      </div>
    {:else}
      <ReadingPane {conversation} {messages} {loading} onfetch={fetchMessage} {folders} {deletedFolderPk} archiveFolderPk={archiveFolder?.folder_pk ?? null} {draftsFolderPk} {onaction} {oncompose} onopendraft={(pk) => void openDraft(pk)} draftOpening={composeLoading} composer={compose ? composerSnippet : undefined} bind:topExpandedPk onmeeting={() => scheduleReread(100, true)} />
    {/if}
  </main>
  {#if staged || notice || (error && messages.length)}
    <footer class="status" role="status">
      {#if staged}
        <span class="status-text">{staged.text}</span>
        <button class="link" onclick={() => void undoStaged()}>
          <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.undo} fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
          Отменить
        </button>
      {:else if error}
        <span class="status-text failed">{error}</span>
        <button class="link" onclick={() => (error = null)}>Скрыть</button>
      {:else if notice}
        <span class="status-text">{notice}</span>
      {/if}
    </footer>
  {/if}
</div>

<style>
  .window {
    height: 100%;
    display: flex;
    flex-direction: column;
    background: var(--bg-layer);
  }
  .body {
    flex: 1;
    min-height: 0;
  }
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-3);
    padding: var(--space-6);
    text-align: center;
  }
  .btn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
  }
  .status {
    flex: none;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 32px;
    padding: 0 var(--space-4);
    border-top: 1px solid var(--stroke-1);
    background: var(--bg-base);
    font: var(--text-caption);
  }
  .status-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .status-text.failed {
    color: var(--danger);
  }
  .link {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--accent);
  }
  .link:hover {
    text-decoration: underline;
  }
</style>
