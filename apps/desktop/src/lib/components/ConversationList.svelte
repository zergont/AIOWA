<script lang="ts">
  import { SvelteSet } from "svelte/reactivity";
  import { errorMessage, listDate, type ComposeKind, type Conversation, type Folder } from "../api";
  import { COMPOSE_ACTIONS, ICON, conversationActionLabels, isDeletedFolder, type ItemAction } from "../actions";
  import { focusBack, type Anchor } from "../popover";
  import ContextMenu, { type MenuItem } from "./ContextMenu.svelte";
  import FolderPicker from "./FolderPicker.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  interface Props {
    folder: Folder | null;
    conversations: Conversation[];
    selected: Conversation | null;
    loading: boolean;
    hasMore: boolean;
    onselect: (c: Conversation) => void;
    onmore: () => void;
    /** Папки для «Переместить…». */
    folders: Folder[];
    /** Действие над всеми письмами беседы в текущей папке («снять флаг» — во всех папках); промис отклоняется текстом ошибки. */
    onaction: (c: Conversation, action: ItemAction) => Promise<void>;
    /** Ответ, ответ всем или пересылка самого свежего письма беседы в текущей папке. */
    oncompose: (c: Conversation, kind: ComposeKind) => void;
  }
  let { folder, conversations, selected, loading, hasMore, onselect, onmore, folders, onaction, oncompose }: Props = $props();

  // Простая виртуализация: рисуем только строки в окне прокрутки плюс запас.
  const ROW = 72;
  let scroller = $state<HTMLDivElement | null>(null);
  let scrollTop = $state(0);
  let viewport = $state(800);

  const start = $derived(Math.max(0, Math.floor(scrollTop / ROW) - 6));
  const end = $derived(Math.min(conversations.length, Math.ceil((scrollTop + viewport) / ROW) + 6));
  const slice = $derived(conversations.slice(start, end));

  function onScroll() {
    if (!scroller) return;
    scrollTop = scroller.scrollTop;
    viewport = scroller.clientHeight;
    // Меню привязано к месту клика: при прокрутке строка уезжает, меню закрываем.
    menu = null;
    if (hasMore && !loading && scroller.scrollTop + scroller.clientHeight > scroller.scrollHeight - ROW * 10) onmore();
  }

  // Ключ папки, а не объект: после синхронизации объект папки заменяется свежим (новые счётчики),
  // и список не должен прыгать в начало.
  const folderPk = $derived(folder?.folder_pk ?? null);
  $effect(() => {
    // При смене папки прокрутка в начало.
    void folderPk;
    if (scroller) {
      scroller.scrollTop = 0;
      scrollTop = 0;
      viewport = scroller.clientHeight;
    }
  });

  function participantsLabel(c: Conversation): string {
    if (c.participants.length === 0) return "";
    const names = c.participants.map((p) => p.split(",")[0].trim().split(" ").slice(0, 2).join(" "));
    return names.length > 3 ? `${names.slice(0, 3).join(", ")} +${names.length - 3}` : names.join(", ");
  }

  // --- действия над беседой: контекстное меню, перенос, удаление -------------------

  const inDeleted = $derived(isDeletedFolder(folder));
  let menu = $state<{ c: Conversation; anchor: Anchor } | null>(null);
  let picker = $state<{ c: Conversation; anchor: Anchor } | null>(null);
  let confirm = $state<{ c: Conversation; anchor: Anchor } | null>(null);
  let actionError = $state<string | null>(null);
  /**
   * Беседы, по которым идёт команда: повторный запуск по той же беседе не нужен, а действия над
   * другими не ждут (бэкенд всё равно выстраивает команды записи в очередь одним замком).
   */
  const busy = new SvelteSet<number>();

  const menuItems = $derived.by((): MenuItem[] => {
    if (!menu) return [];
    const l = conversationActionLabels(menu.c, inDeleted);
    return [
      ...COMPOSE_ACTIONS.map((a) => ({ id: a.kind, label: a.label, icon: a.icon })),
      { id: "read", label: l.read, icon: menu.c.unread_count > 0 ? ICON.read : ICON.unread, separator: true },
      { id: "flag", label: l.flag, icon: ICON.flag },
      { id: "move", label: l.move, icon: ICON.move },
      { id: "delete", label: l.delete, icon: ICON.delete, danger: true, separator: true },
    ];
  });

  async function run(c: Conversation, action: ItemAction) {
    const pk = c.conversation_pk;
    if (busy.has(pk)) return;
    busy.add(pk);
    actionError = null;
    try {
      await onaction(c, action);
    } catch (e) {
      actionError = errorMessage(e);
    } finally {
      busy.delete(pk);
    }
  }

  /** Якорь для окон, открытых с клавиатуры: точка у строки выбранной беседы, если она отрисована, иначе у верха списка. */
  function selectedAnchor(): Anchor {
    const row = scroller?.querySelector<HTMLElement>(".row.selected");
    const r = (row ?? scroller)?.getBoundingClientRect();
    return { x: (r?.left ?? 0) + 40, y: row && r ? r.bottom - 8 : (r?.top ?? 0) + 40 };
  }

  function openMenu(c: Conversation, anchor: Anchor) {
    if (selected?.conversation_pk !== c.conversation_pk) onselect(c);
    picker = null;
    confirm = null;
    menu = { c, anchor };
  }

  function onRowContextMenu(e: MouseEvent, c: Conversation) {
    e.preventDefault();
    openMenu(c, { x: e.clientX, y: e.clientY });
  }

  function closeMenu() {
    menu = null;
    focusBack(scroller);
  }

  function pickMenu(id: string) {
    const m = menu;
    menu = null;
    if (!m) return;
    const c = m.c;
    switch (id) {
      case "reply":
      case "reply_all":
      case "forward":
        focusBack(scroller);
        oncompose(c, id);
        break;
      case "read":
        focusBack(scroller);
        void run(c, { kind: "read", read: c.unread_count > 0 });
        break;
      case "flag":
        focusBack(scroller);
        void run(c, { kind: "flag", flagged: !c.has_flagged });
        break;
      case "move":
        picker = { c, anchor: m.anchor };
        break;
      case "delete":
        requestDelete(c, false, m.anchor);
        break;
    }
  }

  /** Удаление: из «Удалённых» или с Shift — навсегда, с подтверждением; иначе сразу в «Удалённые». */
  function requestDelete(c: Conversation, permanent: boolean, anchor: Anchor) {
    if (inDeleted || permanent) confirm = { c, anchor };
    else void run(c, { kind: "delete", permanent: false });
  }

  function confirmDelete() {
    const d = confirm;
    confirm = null;
    focusBack(scroller);
    if (d) void run(d.c, { kind: "delete", permanent: true });
  }

  function cancelPopups() {
    picker = null;
    confirm = null;
    focusBack(scroller);
  }

  function moveTo(f: Folder) {
    const p = picker;
    picker = null;
    focusBack(scroller);
    if (p) void run(p.c, { kind: "move", folderPk: f.folder_pk });
  }

  function onKey(e: KeyboardEvent) {
    if (!conversations.length) return;
    const idx = selected ? conversations.findIndex((c) => c.conversation_pk === selected!.conversation_pk) : -1;
    if (e.key === "ArrowDown" || e.key === "j") {
      e.preventDefault();
      onselect(conversations[Math.min(conversations.length - 1, idx + 1)]);
    } else if (e.key === "ArrowUp" || e.key === "k") {
      e.preventDefault();
      onselect(conversations[Math.max(0, idx - 1)]);
    } else if (e.key === "Delete" && selected && idx >= 0) {
      e.preventDefault();
      requestDelete(selected, e.shiftKey, selectedAnchor());
    } else if ((e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) && selected && idx >= 0) {
      e.preventDefault();
      openMenu(selected, selectedAnchor());
    }
  }
</script>

<div class="head">
  <h2 class="title">{folder?.name ?? "Почта"}</h2>
  <span class="caption">
    {#if folder}
      {folder.total.toLocaleString("ru-RU")} писем{folder.unread ? `, ${folder.unread.toLocaleString("ru-RU")} непрочитанных` : ""}
    {/if}
  </span>
</div>

{#if actionError}
  <div class="action-error" role="alert">
    <span class="action-error-text">{actionError}</span>
    <button class="btn" onclick={() => (actionError = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
  </div>
{/if}

<div class="scroller" bind:this={scroller} onscroll={onScroll} role="listbox" aria-label="Беседы" tabindex="0" onkeydown={onKey} aria-busy={busy.size > 0}>
  {#if conversations.length === 0 && !loading}
    <div class="empty">
      <p>В этой папке пока нет бесед.</p>
      <p class="caption">Если синхронизация ещё не выполнялась, нажми «Обновить» внизу окна.</p>
    </div>
  {:else}
    <div class="spacer" style="height: {conversations.length * ROW}px">
      {#each slice as c, i (c.conversation_pk)}
        {@const isSelected = selected?.conversation_pk === c.conversation_pk}
        <div class="row" class:selected={isSelected} class:unread={c.unread_count > 0} class:busy={busy.has(c.conversation_pk)} style="top: {(start + i) * ROW}px" role="option" aria-selected={isSelected} tabindex="-1" onclick={() => onselect(c)} onkeydown={(e) => e.key === "Enter" && onselect(c)} oncontextmenu={(e) => onRowContextMenu(e, c)}>
          <div class="line1">
            <span class="people">{participantsLabel(c) || "(без отправителя)"}</span>
            {#if c.total_count > 1}<span class="count">{c.total_count}</span>{/if}
            <span class="date">{listDate(c.last_received_ms)}</span>
          </div>
          <div class="line2">
            <span class="topic">{c.topic || "(без темы)"}</span>
            {#if c.has_attachments}
              <svg class="clip" width="14" height="14" viewBox="0 0 16 16" aria-label="Есть вложения"><path d="M10.5 4.5 5.8 9.2a1.5 1.5 0 0 0 2.1 2.1l5-5a3 3 0 0 0-4.2-4.2l-5.3 5.3a4.5 4.5 0 0 0 6.4 6.4l4.2-4.2" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
            {/if}
            {#if c.has_flagged}
              <svg class="flag" width="13" height="13" viewBox="0 0 16 16" aria-label="С флагом"><path d={ICON.flag} fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
            {/if}
          </div>
        </div>
      {/each}
    </div>
    {#if loading}
      <div class="loading caption">Загрузка…</div>
    {/if}
  {/if}
</div>

{#if menu}
  <ContextMenu items={menuItems} anchor={menu.anchor} label="Действия с беседой" onpick={pickMenu} onclose={closeMenu} />
{/if}
{#if picker}
  <FolderPicker {folders} currentFolderPk={folderPk} anchor={picker.anchor} align="start" onpick={moveTo} onclose={cancelPopups} />
{/if}
{#if confirm}
  <ConfirmPopover anchor={confirm.anchor} align="start" title="Удалить беседу навсегда?" text="Письма этой беседы в текущей папке исчезнут с сервера, вернуть их будет нельзя." confirmLabel="Удалить навсегда" onconfirm={confirmDelete} oncancel={cancelPopups} />
{/if}

<style>
  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4) var(--space-2);
    border-bottom: 1px solid var(--stroke-1);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .action-error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-4);
    font: var(--text-caption);
    color: var(--danger);
    border-bottom: 1px solid var(--stroke-1);
    min-width: 0;
  }
  .action-error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
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
  .scroller {
    flex: 1;
    overflow-y: auto;
    position: relative;
    outline: none;
  }
  .scroller:focus-visible {
    box-shadow: inset 0 0 0 2px var(--stroke-focus);
  }
  .spacer {
    position: relative;
  }
  .row {
    position: absolute;
    left: 0;
    right: 0;
    height: 72px;
    padding: 12px var(--space-4) 12px 20px;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 2px;
    border-bottom: 1px solid var(--stroke-1);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.selected {
    background: var(--bg-selected);
  }
  .row.busy {
    opacity: 0.6;
  }
  .row.selected::before {
    content: "";
    position: absolute;
    left: 6px;
    top: 16px;
    bottom: 16px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }
  .row.unread::after {
    content: "";
    position: absolute;
    left: 8px;
    top: 50%;
    width: 7px;
    height: 7px;
    margin-top: -4px;
    border-radius: 50%;
    background: var(--unread);
  }
  .row.unread.selected::after {
    left: 12px;
  }
  .line1,
  .line2 {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .people {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-1);
  }
  .row.unread .people {
    font-weight: 600;
  }
  .count {
    font: var(--text-caption);
    color: var(--fg-3);
    border: 1px solid var(--stroke-2);
    border-radius: 9px;
    padding: 0 6px;
    line-height: 16px;
  }
  .date {
    font: var(--text-caption);
    color: var(--fg-3);
    white-space: nowrap;
  }
  .row.unread .date {
    color: var(--unread);
    font-weight: 600;
  }
  .topic {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-2);
  }
  .row.unread .topic {
    color: var(--fg-1);
  }
  .clip {
    color: var(--fg-3);
    flex: none;
  }
  .flag {
    color: var(--danger);
    flex: none;
  }
  .empty {
    padding: var(--space-6) var(--space-4);
    color: var(--fg-2);
  }
  .empty p {
    margin: 0 0 var(--space-2);
  }
  .loading {
    padding: var(--space-3) var(--space-4);
    text-align: center;
  }
</style>
