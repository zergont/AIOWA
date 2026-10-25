<script lang="ts">
  import { SvelteSet } from "svelte/reactivity";
  import { conversationsText, errorMessage, listDate, type ComposeKind, type Conversation, type Folder } from "../api";
  import { COMPOSE_ACTIONS, DND_CONVERSATIONS, ICON, conversationActionLabels, isDeletedFolder, type DragPayload, type ItemAction } from "../actions";
  import { focusBack, type Anchor } from "../popover";
  import { hint, layerOpen } from "../hotkeys";
  import ContextMenu, { type MenuItem } from "./ContextMenu.svelte";
  import FolderPicker from "./FolderPicker.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  interface Props {
    folder: Folder | null;
    conversations: Conversation[];
    /** Беседа в фокусе: открыта в области чтения, когда выбор одиночный; при множественном — строка-курсор. */
    selected: Conversation | null;
    /** Множественный выбор (Ctrl/Shift+клик, Ctrl+A): все выбранные беседы, включая `selected`. */
    selectedPks: Set<number>;
    loading: boolean;
    hasMore: boolean;
    /** Одиночный выбор: открыть беседу. */
    onselect: (c: Conversation) => void;
    /** Множественный выбор (две и больше): `focus` — строка-курсор для клавиатуры. */
    onselection: (pks: number[], focus: Conversation) => void;
    onmore: () => void;
    /** Папки для «Переместить…». */
    folders: Folder[];
    /** Папка архива (для «В архив» в ней самой кнопка недоступна); null — папки нет, App предложит создать. */
    archiveFolderPk: number | null;
    /**
     * Действие над всеми письмами выбранных бесед в текущей папке («снять флаг» — во всех папках);
     * `anchor` — откуда вызвано (для вопросов вроде «создать папку архива?»); промис отклоняется текстом ошибки.
     */
    onaction: (targets: Conversation[], action: ItemAction, anchor: Anchor) => Promise<void>;
    /** Ответ, ответ всем или пересылка самого свежего письма беседы в текущей папке. */
    oncompose: (c: Conversation, kind: ComposeKind) => void;
    /** В «Черновиках»: двойной клик или Enter по беседе открывает её черновик в композере. */
    onopendraft?: (c: Conversation) => void;
  }
  let { folder, conversations, selected, selectedPks, loading, hasMore, onselect, onselection, onmore, folders, archiveFolderPk, onaction, oncompose, onopendraft }: Props = $props();

  const inDrafts = $derived(folder?.distinguished === "drafts");

  /** Enter по строке: в «Черновиках» — открыть черновик, иначе — выбрать беседу. */
  function activate(c: Conversation) {
    if (inDrafts && onopendraft) onopendraft(c);
    else selectOne(c);
  }
  function onRowDblClick(c: Conversation) {
    if (inDrafts && onopendraft) onopendraft(c);
  }

  // Простая виртуализация: рисуем только строки в окне прокрутки плюс запас. Высота строки одна на
  // все строки (три строки текста: отправитель, тема, начало текста) — от неё считаются окно,
  // прокрутка к строке-курсору и подгрузка страниц; в CSS она приходит переменной --row.
  const ROW = 84;
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

  /** Строка с индексом idx целиком в окне прокрутки (после хода клавиатурой). */
  function ensureVisible(idx: number) {
    if (!scroller || idx < 0) return;
    const top = idx * ROW;
    if (top < scroller.scrollTop) scroller.scrollTop = top;
    else if (top + ROW > scroller.scrollTop + scroller.clientHeight) scroller.scrollTop = top + ROW - scroller.clientHeight;
  }

  // Ключ папки, а не объект: после синхронизации объект папки заменяется свежим (новые счётчики),
  // и список не должен прыгать в начало.
  const folderPk = $derived(folder?.folder_pk ?? null);
  $effect(() => {
    // При смене папки прокрутка в начало; якорь диапазона из прежней папки не годится.
    void folderPk;
    anchorPk = null;
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

  // --- выбор: одиночный, Ctrl/Shift+клик, Ctrl+A, Shift+стрелки ---------------------------------

  /** Выбранные беседы: если родитель ещё не дал набор, это одна открытая беседа. */
  const sel = $derived<Set<number>>(selectedPks.size ? selectedPks : selected ? new Set([selected.conversation_pk]) : new Set());
  const multi = $derived(sel.size > 1);
  /** Выбранные беседы в порядке списка. */
  const targets = $derived.by((): Conversation[] => (multi ? conversations.filter((c) => sel.has(c.conversation_pk)) : selected ? [selected] : []));
  /** Строка, от которой считается диапазон Shift+клика и Shift+стрелок. */
  let anchorPk: number | null = null;
  $effect(() => {
    // Одиночный выбор извне (сосед после удаления, новое письмо из тоста, восстановление) переносит
    // якорь на открытую беседу; при множественном выборе якорь остаётся — от него идут Shift+стрелки.
    if (selectedPks.size <= 1) anchorPk = selected?.conversation_pk ?? null;
  });

  function indexOf(pk: number | null | undefined): number {
    return pk == null ? -1 : conversations.findIndex((c) => c.conversation_pk === pk);
  }

  /** Отдать набор родителю: одна беседа — обычный выбор, больше — множественный. */
  function emitSelection(pks: number[], focus: Conversation) {
    if (pks.length <= 1) {
      const only = conversations.find((c) => c.conversation_pk === pks[0]) ?? focus;
      anchorPk = only.conversation_pk;
      onselect(only);
    } else onselection(pks, focus);
  }

  function selectOne(c: Conversation) {
    anchorPk = c.conversation_pk;
    onselect(c);
  }

  /** Диапазон от якоря до c (включительно); якорь остаётся, курсор — c. Якоря в списке нет — от курсора. */
  function selectRange(c: Conversation) {
    let a = indexOf(anchorPk);
    if (a < 0) a = indexOf(selected?.conversation_pk);
    const b = indexOf(c.conversation_pk);
    if (a < 0 || b < 0) {
      selectOne(c);
      return;
    }
    const [lo, hi] = a < b ? [a, b] : [b, a];
    emitSelection(
      conversations.slice(lo, hi + 1).map((x) => x.conversation_pk),
      c,
    );
  }

  /** Ctrl+клик: добавить или снять; последнюю выбранную снять нельзя. */
  function toggleOne(c: Conversation) {
    const next = new Set(sel);
    const pk = c.conversation_pk;
    if (next.has(pk)) {
      if (next.size === 1) return;
      next.delete(pk);
    } else next.add(pk);
    anchorPk = pk;
    const focus = next.has(pk) ? c : selected && next.has(selected.conversation_pk) ? selected : (conversations.find((x) => next.has(x.conversation_pk)) ?? c);
    emitSelection([...next], focus);
  }

  function selectAll() {
    if (!conversations.length) return;
    emitSelection(
      conversations.map((c) => c.conversation_pk),
      selected ?? conversations[0],
    );
  }

  /** Esc: оставить только беседу-курсор. */
  function collapseSelection() {
    const focus = selected ?? targets[0];
    if (focus) selectOne(focus);
  }

  function onRowClick(e: MouseEvent, c: Conversation) {
    if (e.shiftKey) selectRange(c);
    else if (e.ctrlKey || e.metaKey) toggleOne(c);
    else selectOne(c);
  }

  // --- действия над беседами: контекстное меню, перенос, архив, удаление -----------------------

  const inDeleted = $derived(isDeletedFolder(folder));
  const inArchive = $derived(archiveFolderPk != null && archiveFolderPk === folderPk);
  let menu = $state<{ targets: Conversation[]; anchor: Anchor } | null>(null);
  let picker = $state<{ targets: Conversation[]; anchor: Anchor } | null>(null);
  let confirm = $state<{ targets: Conversation[]; anchor: Anchor } | null>(null);
  let actionError = $state<string | null>(null);
  /**
   * Беседы, по которым идёт команда: повторный запуск по той же беседе не нужен, а действия над
   * другими не ждут (бэкенд всё равно выстраивает команды записи в очередь одним замком).
   */
  const busy = new SvelteSet<number>();

  const labels = $derived(conversationActionLabels(targets, inDeleted));

  const menuItems = $derived.by((): MenuItem[] => {
    if (!menu) return [];
    const l = conversationActionLabels(menu.targets, inDeleted);
    const single = menu.targets.length === 1;
    return [
      ...(single ? COMPOSE_ACTIONS.map((a) => ({ id: a.kind, label: a.label, icon: a.icon })) : []),
      { id: "read", label: l.read, icon: l.anyUnread ? ICON.read : ICON.unread, separator: single },
      { id: "flag", label: l.flag, icon: ICON.flag },
      { id: "move", label: l.move, icon: ICON.move },
      { id: "archive", label: l.archive, icon: ICON.archive, disabled: inArchive },
      { id: "delete", label: l.delete, icon: ICON.delete, danger: true, separator: true },
    ];
  });

  async function run(ts: Conversation[], action: ItemAction, anchor: Anchor) {
    const free = ts.filter((c) => !busy.has(c.conversation_pk));
    if (!free.length) return;
    for (const c of free) busy.add(c.conversation_pk);
    actionError = null;
    try {
      await onaction(free, action, anchor);
    } catch (e) {
      actionError = errorMessage(e);
    } finally {
      for (const c of free) busy.delete(c.conversation_pk);
    }
  }

  /** Якорь для окон, открытых с клавиатуры: точка у строки-курсора, если она отрисована, иначе у верха списка. */
  function selectedAnchor(): Anchor {
    const row = scroller?.querySelector<HTMLElement>(".row.focused");
    const r = (row ?? scroller)?.getBoundingClientRect();
    return { x: (r?.left ?? 0) + 40, y: row && r ? r.bottom - 8 : (r?.top ?? 0) + 40 };
  }

  /** Меню для строки: по беседе вне выбора — выбор переходит к ней; по выбранной — ко всему выбору. */
  function openMenu(c: Conversation, anchor: Anchor) {
    let ts: Conversation[];
    if (sel.has(c.conversation_pk)) ts = targets;
    else {
      selectOne(c);
      ts = [c];
    }
    picker = null;
    confirm = null;
    menu = { targets: ts, anchor };
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
    const ts = m.targets;
    const l = conversationActionLabels(ts, inDeleted);
    switch (id) {
      case "reply":
      case "reply_all":
      case "forward":
        focusBack(scroller);
        if (ts.length === 1) oncompose(ts[0], id);
        break;
      case "read":
        focusBack(scroller);
        void run(ts, { kind: "read", read: l.anyUnread }, m.anchor);
        break;
      case "flag":
        focusBack(scroller);
        void run(ts, { kind: "flag", flagged: !l.anyFlagged }, m.anchor);
        break;
      case "move":
        picker = { targets: ts, anchor: m.anchor };
        break;
      case "archive":
        focusBack(scroller);
        void run(ts, { kind: "archive" }, m.anchor);
        break;
      case "delete":
        requestDelete(ts, false, m.anchor);
        break;
    }
  }

  /** Удаление: из «Удалённых» или с Shift — навсегда, с подтверждением; иначе сразу в «Удалённые» (с окном отмены). */
  function requestDelete(ts: Conversation[], permanent: boolean, anchor: Anchor) {
    if (!ts.length) return;
    if (inDeleted || permanent) confirm = { targets: ts, anchor };
    else void run(ts, { kind: "delete", permanent: false }, anchor);
  }

  function confirmDelete() {
    const d = confirm;
    confirm = null;
    focusBack(scroller);
    if (d) void run(d.targets, { kind: "delete", permanent: true }, d.anchor);
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
    if (p) void run(p.targets, { kind: "move", folderPk: f.folder_pk }, p.anchor);
  }

  /**
   * Кнопки панели «Выбрано N»: якорь окон — сама кнопка, фокус — списку, чтобы обещанные подсказками
   * клавиши (Esc, Delete, стрелки, Ctrl+A) работали и после клика по панели (onKey висит на списке).
   */
  function bulk(e: MouseEvent, action: ItemAction) {
    const btn = e.currentTarget as HTMLElement;
    focusBack(scroller);
    if (action.kind === "delete") requestDelete(targets, false, btn);
    else void run(targets, action, btn);
  }

  /** «Снять выбор» с панели: фокус списку до коллапса — после него панель исчезает вместе с кнопкой. */
  function bulkCollapse() {
    focusBack(scroller);
    collapseSelection();
  }

  function bulkMove(e: MouseEvent) {
    picker = picker ? null : { targets, anchor: e.currentTarget as HTMLElement };
  }

  // --- значки строки: флажок выбора слева, быстрые действия справа ------------------------------
  // Видны при наведении на строку; быстрые действия — ещё и у строки-курсора, пока по списку ходят
  // клавишами (`kbd`), флажки — у всех строк, пока выбор множественный. Значки лежат поверх строки
  // (position: absolute) и текст не сдвигают: под быстрыми действиями гаснет конец начала текста.

  /** По списку ходят клавишами: у строки-курсора видны быстрые действия. Мышь над списком — снова по наведению. */
  let kbd = $state(false);

  /** Флажок слева: добавить беседу к выбору или убрать из него — как Ctrl+щелчок. */
  function onCheck(e: MouseEvent, c: Conversation) {
    e.stopPropagation();
    menu = null;
    focusBack(scroller);
    toggleOne(c);
  }

  /**
   * Быстрое действие справа — над одной этой беседой, даже если она в множественном выборе (значок
   * стоит на строке). Те же действия и та же обработка, что у меню: окно отмены, вопрос о папке архива,
   * ошибка над списком. Удаление с Shift и в «Удалённых» — навсегда, с подтверждением.
   */
  function quick(e: MouseEvent, c: Conversation, kind: "read" | "flag" | "archive" | "delete") {
    e.stopPropagation();
    const btn = e.currentTarget as HTMLElement;
    const l = conversationActionLabels([c], inDeleted);
    menu = null;
    picker = null;
    focusBack(scroller);
    switch (kind) {
      case "read":
        void run([c], { kind: "read", read: l.anyUnread }, btn);
        break;
      case "flag":
        void run([c], { kind: "flag", flagged: !l.anyFlagged }, btn);
        break;
      case "archive":
        if (!inArchive) void run([c], { kind: "archive" }, btn);
        break;
      case "delete":
        requestDelete([c], e.shiftKey, btn);
        break;
    }
  }

  function checkTitle(checked: boolean): string {
    if (!checked) return "Добавить к выбору";
    return sel.size > 1 ? "Убрать из выбора" : "Беседа выбрана; отметь ещё, чтобы выбрать несколько";
  }

  const deleteTitle = $derived(inDeleted ? hint("Удалить навсегда…", "list-delete") : `${hint("Удалить", "list-delete")}; с Shift — навсегда`);

  const confirmTitle = $derived(confirm && confirm.targets.length > 1 ? `Удалить ${conversationsText(confirm.targets.length)} навсегда?` : "Удалить беседу навсегда?");
  const confirmText = $derived(confirm && confirm.targets.length > 1 ? "Письма этих бесед в текущей папке исчезнут с сервера, вернуть их будет нельзя." : "Письма этой беседы в текущей папке исчезнут с сервера, вернуть их будет нельзя.");

  // --- клавиатура ---------------------------------------------------------------------------------

  function onKey(e: KeyboardEvent) {
    // Над списком открыт слой (окно, вопрос, выбор папки, меню): клавиши списка (Delete, E, стрелки)
    // молчат, даже если фокус остался на списке, — действия не уходят на беседы под окном.
    if (!conversations.length || layerOpen()) return;
    const ctrl = e.ctrlKey || e.metaKey;
    const plain = !ctrl && !e.altKey;
    const idx = indexOf(selected?.conversation_pk);
    if (e.key === "ArrowDown" || (plain && !e.shiftKey && e.code === "KeyJ")) {
      e.preventDefault();
      moveFocus(Math.min(conversations.length - 1, idx + 1), e.shiftKey);
    } else if (e.key === "ArrowUp" || (plain && !e.shiftKey && e.code === "KeyK")) {
      e.preventDefault();
      moveFocus(Math.max(0, idx - 1), e.shiftKey);
    } else if (e.key === "Home" || e.key === "End") {
      e.preventDefault();
      moveFocus(e.key === "Home" ? 0 : conversations.length - 1, e.shiftKey);
    } else if (ctrl && !e.shiftKey && !e.altKey && e.code === "KeyA") {
      e.preventDefault();
      selectAll();
    } else if (e.key === "Escape" && multi) {
      e.preventDefault();
      collapseSelection();
    } else if (e.key === "Delete" && targets.length) {
      e.preventDefault();
      requestDelete(targets, e.shiftKey, selectedAnchor());
    } else if (plain && !e.shiftKey && e.code === "KeyE" && targets.length && !inArchive) {
      e.preventDefault();
      void run(targets, { kind: "archive" }, selectedAnchor());
    } else if ((e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) && selected && idx >= 0) {
      e.preventDefault();
      openMenu(selected, selectedAnchor());
    } else if (e.key === "Enter" && plain && !e.shiftKey && e.target === scroller && selected && idx >= 0 && !multi) {
      // Только когда фокус на самом списке: у строки свой обработчик Enter, и событие от неё всплывает сюда.
      e.preventDefault();
      activate(selected);
    }
  }

  /** Стрелки: курсор на строку next; с Shift — диапазон от якоря до неё. */
  function moveFocus(next: number, extend: boolean) {
    const c = conversations[next];
    if (!c) return;
    kbd = true;
    ensureVisible(next);
    if (extend) selectRange(c);
    else selectOne(c);
  }

  // --- перетаскивание бесед на папки дерева --------------------------------------------------------

  function onDragStart(e: DragEvent, c: Conversation) {
    if (!e.dataTransfer || folderPk == null) return;
    // Тащат беседу вне выбора — выбор переходит к ней; выбранную — тащится весь выбор.
    const pks = sel.has(c.conversation_pk) ? [...sel] : [c.conversation_pk];
    if (!sel.has(c.conversation_pk)) selectOne(c);
    const payload: DragPayload = { conversation_pks: pks, folder_pk: folderPk };
    e.dataTransfer.setData(DND_CONVERSATIONS, JSON.stringify(payload));
    e.dataTransfer.effectAllowed = "move";
    const ghost = document.createElement("div");
    ghost.className = "drag-ghost";
    ghost.textContent = pks.length > 1 ? conversationsText(pks.length) : c.topic || "(без темы)";
    document.body.appendChild(ghost);
    e.dataTransfer.setDragImage(ghost, 12, 14);
    setTimeout(() => ghost.remove(), 0);
    menu = null;
  }
</script>

{#if multi}
  <div class="head bulk" role="toolbar" aria-label="Действия с выбранными беседами">
    <span class="bulk-title" aria-live="polite">Выбрано {sel.size.toLocaleString("ru-RU")}</span>
    <span class="bulk-actions">
      <button class="act" onclick={(e) => bulk(e, { kind: "read", read: labels.anyUnread })} aria-label={labels.read} title={labels.read}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={labels.anyUnread ? ICON.read : ICON.unread} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="act" class:on={labels.anyFlagged} onclick={(e) => bulk(e, { kind: "flag", flagged: !labels.anyFlagged })} aria-label={labels.flag} title={labels.flag}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.flag} fill={labels.anyFlagged ? "currentColor" : "none"} stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="act" onclick={bulkMove} aria-label={labels.move} title={labels.move} aria-haspopup="dialog" aria-expanded={picker !== null}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.move} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="act" onclick={(e) => bulk(e, { kind: "archive" })} disabled={inArchive} aria-label={labels.archive} title={inArchive ? "Беседы уже в архиве" : hint(labels.archive, "archive")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.archive} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="act danger" onclick={(e) => bulk(e, { kind: "delete", permanent: false })} aria-label={labels.delete} title={hint(labels.delete, "list-delete")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.delete} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <span class="act-sep" aria-hidden="true"></span>
      <button class="act" onclick={bulkCollapse} aria-label="Снять выбор" title={hint("Снять выбор", "list-collapse")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.close} fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /></svg>
      </button>
    </span>
  </div>
{:else}
  <div class="head">
    <h2 class="title">{folder?.name ?? "Почта"}</h2>
    <span class="caption">
      {#if folder}
        {folder.total.toLocaleString("ru-RU")} писем{folder.unread ? `, ${folder.unread.toLocaleString("ru-RU")} непрочитанных` : ""}
      {/if}
    </span>
  </div>
{/if}

{#if actionError}
  <div class="action-error" role="alert">
    <span class="action-error-text">{actionError}</span>
    <button class="btn" onclick={() => (actionError = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
  </div>
{/if}

<div
  class="scroller"
  class:multi
  class:kbd
  style="--row: {ROW}px"
  bind:this={scroller}
  onscroll={onScroll}
  onpointermove={() => (kbd = false)}
  role="listbox"
  aria-label="Беседы"
  aria-multiselectable="true"
  tabindex="0"
  onkeydown={onKey}
  aria-busy={busy.size > 0}
>
  {#if conversations.length === 0 && !loading}
    <div class="empty">
      <p>В этой папке пока нет бесед.</p>
      <p class="caption">Если синхронизация ещё не выполнялась, нажми «Обновить» внизу окна.</p>
    </div>
  {:else}
    <div class="spacer" style="height: {conversations.length * ROW}px">
      {#each slice as c, i (c.conversation_pk)}
        {@const inSel = sel.has(c.conversation_pk)}
        {@const focused = selected?.conversation_pk === c.conversation_pk}
        {@const l = conversationActionLabels([c], inDeleted)}
        {@const rowBusy = busy.has(c.conversation_pk)}
        <div
          class="row"
          class:selected={inSel}
          class:focused
          class:unread={c.unread_count > 0}
          class:busy={rowBusy}
          style="top: {(start + i) * ROW}px"
          role="option"
          aria-selected={inSel}
          tabindex="-1"
          draggable="true"
          onclick={(e) => onRowClick(e, c)}
          ondblclick={() => onRowDblClick(c)}
          onkeydown={(e) => e.key === "Enter" && !layerOpen() && activate(c)}
          oncontextmenu={(e) => onRowContextMenu(e, c)}
          ondragstart={(e) => onDragStart(e, c)}
        >
          <button class="check" class:on={inSel} role="checkbox" aria-checked={inSel} aria-label={inSel ? "Убрать беседу из выбора" : "Добавить беседу к выбору"} title={checkTitle(inSel)} tabindex="-1" onclick={(e) => onCheck(e, c)} ondblclick={(e) => e.stopPropagation()}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M2.5 6.2 5 8.7l4.5-5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
          <div class="line1">
            <span class="people">{participantsLabel(c) || "(без отправителя)"}</span>
            {#if c.total_count > 1}<span class="count">{c.total_count}</span>{/if}
            <span class="date">{listDate(c.last_received_ms)}</span>
          </div>
          <div class="line2">
            <span class="topic">{c.topic || "(без темы)"}</span>
            {#if c.has_attachments}
              <svg class="clip" width="14" height="14" viewBox="0 0 16 16" aria-label="Есть вложения"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
            {/if}
            {#if c.has_flagged}
              <svg class="flag" width="13" height="13" viewBox="0 0 16 16" aria-label="С флагом"><path d={ICON.flag} fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
            {/if}
          </div>
          <div class="line3">
            <span class="preview">{c.preview ?? ""}</span>
          </div>
          <span class="quick">
            <button class="act" onclick={(e) => quick(e, c, "read")} ondblclick={(e) => e.stopPropagation()} disabled={rowBusy} aria-label={l.read} title={l.read} tabindex="-1">
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={l.anyUnread ? ICON.read : ICON.unread} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            </button>
            <button class="act" class:on={l.anyFlagged} onclick={(e) => quick(e, c, "flag")} ondblclick={(e) => e.stopPropagation()} disabled={rowBusy} aria-label={l.flag} title={l.flag} tabindex="-1">
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.flag} fill={l.anyFlagged ? "currentColor" : "none"} stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            </button>
            <button class="act" onclick={(e) => quick(e, c, "archive")} ondblclick={(e) => e.stopPropagation()} disabled={rowBusy || inArchive} aria-label={l.archive} title={inArchive ? "Беседа уже в архиве" : hint(l.archive, "archive")} tabindex="-1">
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.archive} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            </button>
            <button class="act danger" onclick={(e) => quick(e, c, "delete")} ondblclick={(e) => e.stopPropagation()} disabled={rowBusy} aria-label={l.delete} title={deleteTitle} tabindex="-1">
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.delete} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            </button>
          </span>
        </div>
      {/each}
    </div>
    {#if loading}
      <div class="loading caption">Загрузка…</div>
    {/if}
  {/if}
</div>

{#if menu}
  <ContextMenu items={menuItems} anchor={menu.anchor} label={menu.targets.length > 1 ? "Действия с выбранными беседами" : "Действия с беседой"} onpick={pickMenu} onclose={closeMenu} />
{/if}
{#if picker}
  <FolderPicker {folders} currentFolderPk={folderPk} anchor={picker.anchor} align="start" onpick={moveTo} onclose={cancelPopups} />
{/if}
{#if confirm}
  <ConfirmPopover anchor={confirm.anchor} align="start" title={confirmTitle} text={confirmText} confirmLabel="Удалить навсегда" onconfirm={confirmDelete} oncancel={cancelPopups} />
{/if}

<style>
  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
    min-height: 48px;
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
  /* Панель массовых действий на месте заголовка: счётчик слева, значки справа. */
  .head.bulk {
    align-items: center;
    padding: var(--space-2) var(--space-3) var(--space-2) var(--space-4);
    background: var(--accent-soft);
  }
  .bulk-title {
    font: var(--text-body-strong);
    color: var(--fg-1);
    white-space: nowrap;
  }
  .bulk-actions {
    display: inline-flex;
    align-items: center;
    gap: 2px;
  }
  .act {
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-2);
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
  /*
   * Строка беседы: три строки текста (20 + 20 + 24 px с зазорами 2 px, поля 8 px сверху и 7 px
   * снизу плюс граница 1 px — это ROW в скрипте). Слева поле 36 px: полоска курсора, флажок
   * выбора (у первой строки) и точка непрочитанного (посередине). Третья строка выше текста в ней:
   * на её месте справа встают быстрые действия, не задевая тему.
   */
  .row {
    position: absolute;
    left: 0;
    right: 0;
    height: var(--row);
    padding: 8px var(--space-4) 7px 36px;
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
  /* Полоска акцента — у строки-курсора (она же открыта в области чтения при одиночном выборе). */
  .row.focused::before {
    content: "";
    position: absolute;
    left: 3px;
    top: 16px;
    bottom: 16px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }
  .row.unread::after {
    content: "";
    position: absolute;
    left: 17px;
    top: 50%;
    width: 7px;
    height: 7px;
    margin-top: -4px;
    border-radius: 50%;
    background: var(--unread);
  }
  .line1,
  .line2,
  .line3 {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .line3 {
    height: 24px;
  }
  .preview {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font: var(--text-caption);
    color: var(--fg-3);
  }
  /* Флажок выбора: у первой строки, над точкой непрочитанного; кнопка 24 px, квадрат 14 px. */
  .check {
    position: absolute;
    left: 8px;
    top: 6px;
    width: 24px;
    height: 24px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: transparent;
    opacity: 0;
    transition: opacity var(--dur-fast) var(--ease-in);
  }
  .check::before {
    content: "";
    position: absolute;
    left: 5px;
    top: 5px;
    width: 14px;
    height: 14px;
    box-sizing: border-box;
    border: 1px solid var(--fg-3);
    border-radius: 3px;
    background: var(--bg-layer-alt);
  }
  .check svg {
    position: relative;
  }
  .check.on {
    color: var(--fg-on-accent);
  }
  .check.on::before {
    border-color: var(--accent);
    background: var(--accent);
  }
  .check:hover::before {
    border-color: var(--fg-1);
  }
  .check.on:hover::before {
    border-color: var(--accent-hover);
    background: var(--accent-hover);
  }
  .row:hover .check,
  .scroller.multi .check {
    opacity: 1;
  }
  /* Быстрые действия: справа на месте конца третьей строки; начало текста под ними гаснет. */
  .quick {
    position: absolute;
    right: var(--space-3);
    bottom: 7px;
    height: 24px;
    display: inline-flex;
    align-items: center;
    gap: 2px;
    visibility: hidden;
  }
  .quick .act {
    width: 24px;
    height: 24px;
  }
  .row:hover .quick,
  .scroller.kbd:focus-within .row.focused .quick {
    visibility: visible;
  }
  .row:hover .preview,
  .scroller.kbd:focus-within .row.focused .preview {
    mask-image: linear-gradient(to left, transparent 108px, #000 132px);
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
