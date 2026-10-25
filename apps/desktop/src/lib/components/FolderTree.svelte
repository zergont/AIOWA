<script lang="ts">
  import { tick } from "svelte";
  import { api, errorMessage, type Folder } from "../api";
  import { DND_CONVERSATIONS, ICON, folderNameError, isInsideDeleted, readDragPayload, type DragPayload } from "../actions";
  import { focusBack, type Anchor } from "../popover";
  import { layerOpen } from "../hotkeys";
  import ContextMenu, { type MenuItem } from "./ContextMenu.svelte";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  interface Props {
    folders: Folder[];
    selected: Folder | null;
    onselect: (f: Folder) => void;
    /** Синхронизировать только эту папку (кнопка появляется при наведении на строку). */
    onsync?: (f: Folder) => void;
    /** Идёт синхронизация: кнопки папок недоступны. */
    syncing?: boolean;
    /** Создать папку (parentPk null — в корне); промис отклоняется текстом ошибки для показа у строки. */
    oncreate?: (parentPk: number | null, name: string) => Promise<void>;
    onrename?: (f: Folder, name: string) => Promise<void>;
    /** Удалить папку: в «Удалённые» или навсегда (из самих «Удалённых»). */
    ondelete?: (f: Folder, permanent: boolean) => Promise<void>;
    /** На папку сброшены беседы из списка. */
    ondrop?: (target: Folder, payload: DragPayload) => void;
  }
  let { folders, selected, onselect, onsync, syncing = false, oncreate, onrename, ondelete, ondrop }: Props = $props();

  function syncFolder(f: Folder, e: Event) {
    e.stopPropagation();
    onsync?.(f);
  }

  const PINNED = ["inbox", "drafts", "sentitems", "deleteditems", "junkemail", "archive"];
  const HIDDEN_CLASSES_NOTE = "не почтовые папки скрыты";

  // --- свёрнутые ветки: хранятся в базе учётной записи (folders_collapsed_get / _set) ---------------
  // В базе — серверные идентификаторы папок, окну приходят локальные ключи; удалённые на сервере
  // папки бэкенд выбрасывает сам. Пока список не прочитан, строки дерева не рисуются: иначе дерево
  // на миг показалось бы развёрнутым целиком. Сохранение — сразу после щелчка (запись — одна строка
  // sync_state): отложенная запись терялась, если окно закрывали в первые доли секунды.
  //
  // Читается список, когда дерево из пустого стало непустым, — при старте, если папки уже есть, или
  // когда их принесла первая синхронизация. Пока папок в базе нет, серверные идентификаторы не во
  // что перевести, и прочитанный тогда список был бы пуст (в базе он при этом не теряется).

  let collapsed = $state<Set<number>>(new Set());
  let collapsedLoaded = $state(false);
  /** Запись в полёте и щелчок во время неё: свежий набор уходит следующей записью, по одной за раз. */
  let saveRunning = false;
  let savePending = false;
  /** Номер последнего чтения: ответ более раннего, пришедший позже, не применяется. */
  let readSeq = 0;
  /** Папки были при прошлом прогоне эффекта ниже (не реактивно: только для сравнения). */
  let hadFolders = false;

  $effect(() => {
    const has = folders.length > 0;
    if (has && !hadFolders) readCollapsed();
    hadFolders = has;
  });

  function readCollapsed() {
    const seq = ++readSeq;
    collapsedLoaded = false;
    api
      .foldersCollapsedGet()
      .then((pks) => {
        if (seq === readSeq) collapsed = new Set(pks);
      })
      .catch((e) => console.warn("folders_collapsed_get:", errorMessage(e)))
      .finally(() => {
        if (seq === readSeq) collapsedLoaded = true;
      });
  }

  function setCollapsed(next: Set<number>) {
    collapsed = next;
    savePending = true;
    void saveCollapsed();
  }

  /**
   * Записать свёрнутые ветки сейчас. Записи не обгоняют друг друга: пока одна в полёте, следующие
   * щелчки только помечают набор, и по её ответу уходит одна запись с последним состоянием.
   */
  async function saveCollapsed() {
    if (saveRunning) return;
    saveRunning = true;
    try {
      while (savePending) {
        savePending = false;
        await api.foldersCollapsedSet([...collapsed]).catch((e) => console.warn("folders_collapsed_set:", errorMessage(e)));
      }
    } finally {
      saveRunning = false;
    }
  }

  interface Node {
    folder: Folder;
    children: Node[];
  }

  const tree = $derived.by(() => {
    const byPk = new Map<number, Node>();
    for (const f of folders) byPk.set(f.folder_pk, { folder: f, children: [] });
    const roots: Node[] = [];
    for (const n of byPk.values()) {
      const parent = n.folder.parent_pk != null ? byPk.get(n.folder.parent_pk) : undefined;
      if (parent) parent.children.push(n);
      else roots.push(n);
    }
    const sortNodes = (nodes: Node[]) => {
      nodes.sort((a, b) => {
        const ai = PINNED.indexOf(a.folder.distinguished ?? "");
        const bi = PINNED.indexOf(b.folder.distinguished ?? "");
        if (ai !== bi) return (ai === -1 ? 99 : ai) - (bi === -1 ? 99 : bi);
        return a.folder.name.localeCompare(b.folder.name, "ru");
      });
      nodes.forEach((n) => sortNodes(n.children));
    };
    sortNodes(roots);
    // Корень ящика не показываем: его дети и есть верхний уровень.
    const top: Node[] = [];
    for (const r of roots) {
      if (r.folder.distinguished === "msgfolderroot" || (!r.folder.is_mail && r.children.length && r.folder.total === 0)) top.push(...r.children);
      else top.push(r);
    }
    sortNodes(top);
    return top;
  });

  function visible(n: Node): boolean {
    return n.folder.is_mail || n.children.some(visible);
  }

  function toggle(pk: number, e: MouseEvent) {
    e.stopPropagation();
    const next = new Set(collapsed);
    if (next.has(pk)) next.delete(pk);
    else next.add(pk);
    setCollapsed(next);
  }

  function expand(pk: number) {
    if (!collapsed.has(pk)) return;
    const next = new Set(collapsed);
    next.delete(pk);
    setCollapsed(next);
  }

  function icon(f: Folder): string {
    switch (f.distinguished) {
      case "inbox":
        return "M2 3.5h12l-1.5 6H3.5L2 3.5Zm0 6h3.5c0 1 1 2 2.5 2s2.5-1 2.5-2H14v3H2v-3Z";
      case "sentitems":
        return "M2 8 14 2.5 11.5 14 8.5 9.5 2 8Zm6.5 1.5L12 5";
      case "drafts":
        return "M3 2.5h7l3 3v8H3v-11Zm7 0v3h3";
      case "deleteditems":
        return "M3.5 4.5h9M6 4.5V3h4v1.5M4.5 4.5l.7 9h5.6l.7-9";
      case "junkemail":
        return "M8 2 14 13H2L8 2Zm0 4v4m0 1.5v1";
      case "archive":
        return "M2 3h12v3H2V3Zm1 3h10v7H3V6Zm3 2.5h4";
      default:
        return "M2 4h4.5l1.5 1.5H14V13H2V4Z";
    }
  }

  // --- меню папки: создать, переименовать, удалить --------------------------------------------------
  // Служебные папки (distinguished) не переименовываются и не удаляются: сервер откажет, пункты недоступны.

  const canManage = $derived(!!oncreate || !!onrename || !!ondelete);
  /** Меню на строке (f) или на пустом месте дерева (f null — только «Новая папка…» в корне). */
  let menu = $state<{ f: Folder | null; anchor: Anchor } | null>(null);
  /** Строка ввода имени: новая папка под parentPk (null — корень) или переименование pk. */
  let edit = $state<{ mode: "create"; parentPk: number | null } | { mode: "rename"; pk: number } | null>(null);
  let editValue = $state("");
  let editError = $state<string | null>(null);
  let editBusy = $state(false);
  let editInput = $state<HTMLInputElement | null>(null);
  let confirmDel = $state<{ f: Folder; permanent: boolean; anchor: Anchor } | null>(null);
  /** Ошибка удаления у строки папки. */
  let rowError = $state<{ pk: number; text: string } | null>(null);
  let treeEl = $state<HTMLDivElement | null>(null);
  /** Папка, со строки которой открыли меню, редактор или подтверждение: ей и вернётся фокус (null — корень). */
  let returnPk: number | null = null;

  /**
   * Вернуть фокус строке папки (первой найденной из `pks`), а не контейнеру дерева: клавиши
   * (F2, Delete, Shift+F10) обрабатываются только на строке. Строки нет (папка удалена, список
   * перерисован) — фокус контейнеру. После tick(), потому что строка переименования пересоздаётся.
   */
  async function focusRow(...pks: (number | null | undefined)[]) {
    await tick();
    for (const pk of pks) {
      if (pk == null) continue;
      const row = treeEl?.querySelector<HTMLElement>(`[data-pk="${pk}"]`);
      if (row) {
        focusBack(row);
        return;
      }
    }
    focusBack(treeEl);
  }

  const menuItems = $derived.by((): MenuItem[] => {
    if (!menu) return [];
    const f = menu.f;
    if (!f) return [{ id: "create", label: "Новая папка…", icon: ICON.folderAdd, disabled: !oncreate }];
    const service = f.distinguished != null;
    const items: MenuItem[] = [
      { id: "create", label: "Новая папка…", icon: ICON.folderAdd, disabled: !oncreate || !f.is_mail },
      { id: "rename", label: "Переименовать…", icon: ICON.rename, disabled: !onrename || service },
      { id: "delete", label: isInsideDeleted(f, folders) ? "Удалить папку навсегда…" : "Удалить папку…", icon: ICON.delete, danger: true, disabled: !ondelete || service },
    ];
    if (onsync && f.is_mail) items.push({ id: "sync", label: "Синхронизировать папку", separator: true, disabled: syncing });
    return items;
  });

  function openMenu(f: Folder | null, anchor: Anchor) {
    if (!canManage) return;
    confirmDel = null;
    returnPk = f?.folder_pk ?? null;
    menu = { f, anchor };
  }

  function onRowContextMenu(e: MouseEvent, f: Folder) {
    if (!canManage) return;
    e.preventDefault();
    e.stopPropagation();
    openMenu(f, { x: e.clientX, y: e.clientY });
  }

  /** Правый клик по пустому месту дерева (не по строке): новая папка в корне. */
  function onTreeContextMenu(e: MouseEvent) {
    if (!oncreate) return;
    e.preventDefault();
    openMenu(null, { x: e.clientX, y: e.clientY });
  }

  function closeMenu() {
    menu = null;
    void focusRow(returnPk);
  }

  function pickMenu(id: string) {
    const m = menu;
    menu = null;
    if (!m) return;
    switch (id) {
      case "create":
        void startCreate(m.f?.folder_pk ?? null);
        break;
      case "rename":
        if (m.f) void startRename(m.f);
        break;
      case "delete":
        if (m.f) requestDelete(m.f, m.anchor);
        break;
      case "sync":
        if (m.f) onsync?.(m.f);
        void focusRow(returnPk);
        break;
    }
  }

  async function startCreate(parentPk: number | null) {
    if (!oncreate) return;
    if (parentPk != null) expand(parentPk);
    returnPk = parentPk;
    edit = { mode: "create", parentPk };
    editValue = "";
    editError = null;
    await tick();
    editInput?.focus();
  }

  async function startRename(f: Folder) {
    if (!onrename || f.distinguished) return;
    returnPk = f.folder_pk;
    edit = { mode: "rename", pk: f.folder_pk };
    editValue = f.name;
    editError = null;
    await tick();
    editInput?.focus();
    editInput?.select();
  }

  function cancelEdit() {
    if (editBusy) return;
    edit = null;
    editError = null;
    void focusRow(returnPk);
  }

  async function submitEdit() {
    const e = edit;
    if (!e || editBusy) return;
    const name = editValue.trim();
    const bad = folderNameError(name);
    if (bad) {
      editError = bad;
      return;
    }
    if (e.mode === "rename") {
      const f = folders.find((x) => x.folder_pk === e.pk);
      if (!f || f.name === name) {
        cancelEdit();
        return;
      }
    }
    editBusy = true;
    editError = null;
    try {
      if (e.mode === "create") await oncreate?.(e.parentPk, name);
      else {
        const f = folders.find((x) => x.folder_pk === e.pk);
        if (f) await onrename?.(f, name);
      }
      edit = null;
      void focusRow(returnPk);
    } catch (err) {
      editError = errorMessage(err);
      editInput?.focus();
    } finally {
      editBusy = false;
    }
  }

  function onEditKey(e: KeyboardEvent) {
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      void submitEdit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelEdit();
    }
  }

  /**
   * Уход фокуса из поля: имя введено — создаём или переименовываем, иначе просто закрываем.
   * После отказа сервера клик мимо поля закрывает его, а не повторяет запрос.
   */
  function onEditBlur() {
    const e = edit;
    if (editBusy || !e) return;
    const name = editValue.trim();
    const unchanged = e.mode === "rename" && folders.find((x) => x.folder_pk === e.pk)?.name === name;
    if (editError || !name || unchanged || folderNameError(name)) cancelEdit();
    else void submitEdit();
  }

  function requestDelete(f: Folder, anchor: Anchor) {
    if (!ondelete || f.distinguished) return;
    rowError = null;
    returnPk = f.folder_pk;
    confirmDel = { f, permanent: isInsideDeleted(f, folders), anchor };
  }

  async function confirmDelete() {
    const d = confirmDel;
    confirmDel = null;
    void focusRow(returnPk);
    if (!d) return;
    try {
      await ondelete?.(d.f, d.permanent);
      // Строки удалённой папки больше нет (или она уехала в «Удалённые»): фокус — ей же, если
      // осталась, иначе родителю.
      void focusRow(d.f.folder_pk, d.f.parent_pk);
    } catch (e) {
      rowError = { pk: d.f.folder_pk, text: errorMessage(e) };
    }
  }

  function cancelDelete() {
    confirmDel = null;
    void focusRow(returnPk);
  }

  function onRowKey(e: KeyboardEvent, f: Folder) {
    // Под открытым слоем (окно, вопрос, меню) строка папки клавиш не ловит: Delete не удаляет папку.
    if (layerOpen()) return;
    if (e.key === "Enter") {
      onselect(f);
    } else if (e.key === "F2" && onrename) {
      e.preventDefault();
      void startRename(f);
    } else if (e.key === "Delete" && ondelete) {
      e.preventDefault();
      requestDelete(f, e.currentTarget as HTMLElement);
    } else if ((e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) && canManage) {
      e.preventDefault();
      openMenu(f, e.currentTarget as HTMLElement);
    }
  }

  // --- приём перетаскиваемых бесед --------------------------------------------------------------------

  /** Папка под курсором при перетаскивании, на которую можно бросить (подсветка цели). */
  let dropPk = $state<number | null>(null);
  /**
   * Папка под курсором независимо от того, можно ли на неё бросить: по ней считается таймер
   * раскрытия. Иначе над текущей или непочтовой папкой (dropPk остаётся null) каждое dragover
   * перезапускало бы таймер, и свёрнутая папка никогда не раскрывалась.
   */
  let hoverPk: number | null = null;
  let expandTimer: ReturnType<typeof setTimeout> | undefined;
  const EXPAND_HOVER_MS = 600;

  /** Бросить можно в почтовую папку, кроме той, откуда тащат (она же текущая). */
  function canDrop(f: Folder): boolean {
    return !!ondrop && f.is_mail && f.folder_pk !== selected?.folder_pk;
  }

  function hasPayload(e: DragEvent): boolean {
    return !!e.dataTransfer && Array.from(e.dataTransfer.types).includes(DND_CONVERSATIONS);
  }

  function onDragOver(e: DragEvent, n: Node) {
    if (!hasPayload(e)) return;
    const f = n.folder;
    if (hoverPk !== f.folder_pk) {
      hoverPk = f.folder_pk;
      clearTimeout(expandTimer);
      // Свёрнутая папка с подпапками раскрывается, если над ней задержаться, — и та, на которую
      // бросить нельзя: внутри могут быть подпапки-цели.
      if (n.children.length && collapsed.has(f.folder_pk)) expandTimer = setTimeout(() => expand(f.folder_pk), EXPAND_HOVER_MS);
    }
    // Без preventDefault бросок не разрешён: курсор «нельзя», событие drop не придёт.
    if (!canDrop(f)) {
      dropPk = null;
      return;
    }
    e.preventDefault();
    e.dataTransfer!.dropEffect = "move";
    dropPk = f.folder_pk;
  }

  function onDragLeave(e: DragEvent, f: Folder) {
    const to = e.relatedTarget;
    if (to instanceof Node && (e.currentTarget as HTMLElement).contains(to)) return;
    if (dropPk === f.folder_pk) dropPk = null;
    if (hoverPk === f.folder_pk) hoverPk = null;
    clearTimeout(expandTimer);
  }

  function onDrop(e: DragEvent, f: Folder) {
    clearTimeout(expandTimer);
    dropPk = null;
    hoverPk = null;
    const payload = readDragPayload(e.dataTransfer);
    if (!payload || !canDrop(f) || payload.folder_pk === f.folder_pk) return;
    e.preventDefault();
    ondrop?.(f, payload);
  }

  function onDragEnd() {
    dropPk = null;
    hoverPk = null;
    clearTimeout(expandTimer);
  }
</script>

<svelte:window ondragend={onDragEnd} ondrop={onDragEnd} />

{#snippet editRow(depth: number)}
  <div class="row editing" style="--depth: {depth}" role="none">
    <span class="indent"></span>
    <span class="chevron"></span>
    <svg class="icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={edit?.mode === "create" ? ICON.folderAdd : "M2 4h4.5l1.5 1.5H14V13H2V4Z"} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round" /></svg>
    <input
      class="name-input"
      type="text"
      bind:this={editInput}
      bind:value={editValue}
      placeholder={edit?.mode === "create" ? "Имя папки" : undefined}
      aria-label={edit?.mode === "create" ? "Имя новой папки" : "Новое имя папки"}
      aria-invalid={editError != null}
      aria-busy={editBusy}
      readonly={editBusy}
      autocomplete="off"
      spellcheck="false"
      maxlength="255"
      onkeydown={onEditKey}
      onblur={onEditBlur}
      onclick={(e) => e.stopPropagation()}
    />
  </div>
  {#if editError}
    <div class="row-error caption" style="--depth: {depth}" role="alert">{editError}</div>
  {/if}
{/snippet}

{#snippet node(n: Node, depth: number)}
  {#if visible(n)}
    {@const f = n.folder}
    {@const kids = n.children.filter(visible)}
    {@const isSelected = selected?.folder_pk === f.folder_pk}
    {@const renaming = edit?.mode === "rename" && edit.pk === f.folder_pk}
    {#if renaming}
      {@render editRow(depth)}
    {:else}
      <div
        class="row"
        class:selected={isSelected}
        class:drop={dropPk === f.folder_pk}
        style="--depth: {depth}"
        data-pk={f.folder_pk}
        role="treeitem"
        aria-selected={isSelected}
        aria-expanded={kids.length ? !collapsed.has(f.folder_pk) : undefined}
        tabindex="0"
        onclick={() => onselect(f)}
        onkeydown={(e) => onRowKey(e, f)}
        oncontextmenu={(e) => onRowContextMenu(e, f)}
        ondragover={(e) => onDragOver(e, n)}
        ondragleave={(e) => onDragLeave(e, f)}
        ondrop={(e) => onDrop(e, f)}
      >
        <span class="indent"></span>
        {#if kids.length}
          <button class="chevron" class:open={!collapsed.has(f.folder_pk)} onclick={(e) => toggle(f.folder_pk, e)} aria-label={collapsed.has(f.folder_pk) ? "Развернуть" : "Свернуть"} tabindex="-1">
            <svg width="12" height="12" viewBox="0 0 12 12"><path d="M4 2.5 7.5 6 4 9.5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
        {:else}
          <span class="chevron"></span>
        {/if}
        <svg class="icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d={icon(f)} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round" /></svg>
        <span class="name" class:unread={f.unread > 0}>{f.name}</span>
        {#if onsync && f.is_mail}
          <button class="sync" onclick={(e) => syncFolder(f, e)} onkeydown={(e) => e.stopPropagation()} disabled={syncing} aria-label="Синхронизировать папку «{f.name}»" title={syncing ? "Синхронизация уже идёт" : "Синхронизировать только эту папку"} tabindex="-1">
            <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5v3h-3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
        {/if}
        {#if f.unread > 0}
          <span class="count">{f.unread}</span>
        {/if}
      </div>
    {/if}
    {#if rowError?.pk === f.folder_pk}
      <div class="row-error caption" style="--depth: {depth}" role="alert">
        <span class="row-error-text">{rowError.text}</span>
        <button class="row-error-close" onclick={() => (rowError = null)} aria-label="Скрыть сообщение об ошибке">×</button>
      </div>
    {/if}
    {#if edit?.mode === "create" && edit.parentPk === f.folder_pk}
      {@render editRow(depth + 1)}
    {/if}
    {#if kids.length && !collapsed.has(f.folder_pk)}
      {#each kids as child (child.folder.folder_pk)}
        {@render node(child, depth + 1)}
      {/each}
    {/if}
  {/if}
{/snippet}

<div class="tree" role="tree" aria-label="Папки почты" tabindex="-1" bind:this={treeEl} oncontextmenu={onTreeContextMenu}>
  {#if folders.length === 0}
    <p class="caption empty">Папок ещё нет: они появятся после первой синхронизации (кнопка «Обновить» внизу).</p>
  {:else if collapsedLoaded}
    {#if oncreate}
      <div class="tree-head" role="none">
        <span class="caption tree-title">Папки</span>
        <button class="add" onclick={() => void startCreate(null)} aria-label="Новая папка в корне ящика" title="Новая папка">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
        </button>
      </div>
    {/if}
    {#if edit?.mode === "create" && edit.parentPk === null}
      {@render editRow(0)}
    {/if}
    {#each tree as n (n.folder.folder_pk)}
      {@render node(n, 0)}
    {/each}
    <p class="caption note">{HIDDEN_CLASSES_NOTE}</p>
  {/if}
</div>

{#if menu}
  <ContextMenu items={menuItems} anchor={menu.anchor} label={menu.f ? `Действия с папкой «${menu.f.name}»` : "Папки"} onpick={pickMenu} onclose={closeMenu} />
{/if}
{#if confirmDel}
  <ConfirmPopover
    anchor={confirmDel.anchor}
    align="start"
    title={confirmDel.permanent ? `Удалить папку «${confirmDel.f.name}» навсегда?` : `Удалить папку «${confirmDel.f.name}»?`}
    text={confirmDel.permanent ? "Папка с подпапками и письмами исчезнет с сервера, вернуть её будет нельзя." : "Папка с подпапками и письмами переместится в «Удалённые»."}
    confirmLabel={confirmDel.permanent ? "Удалить навсегда" : "Удалить"}
    onconfirm={confirmDelete}
    oncancel={cancelDelete}
  />
{/if}

<style>
  .tree {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-height: 100%;
    outline: none;
  }
  .tree-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 28px;
    padding: 0 var(--space-1) 0 var(--space-2);
  }
  .tree-title {
    letter-spacing: 0.02em;
  }
  .add {
    width: 24px;
    height: 24px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .add:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .add:active {
    background: var(--bg-pressed);
  }
  .row {
    position: relative;
    display: flex;
    align-items: center;
    height: 32px;
    padding: 0 var(--space-2) 0 var(--space-1);
    border-radius: var(--radius-control);
    gap: 4px;
    color: var(--fg-2);
    transition:
      background var(--dur-fast) var(--ease-in),
      box-shadow var(--dur-fast) var(--ease-in);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.selected {
    background: var(--bg-selected);
    color: var(--fg-1);
  }
  .row.selected::before {
    content: "";
    position: absolute;
    left: 0;
    top: 8px;
    bottom: 8px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }
  /* Цель перетаскивания: подложка акцента и тонкая рамка. */
  .row.drop {
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px var(--accent);
    color: var(--fg-1);
  }
  .row.editing {
    background: var(--bg-layer-alt);
    box-shadow: inset 0 0 0 1px var(--stroke-2);
  }
  .indent {
    width: calc(var(--depth) * 16px);
    flex: none;
  }
  .chevron {
    width: 16px;
    height: 16px;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--fg-3);
    border-radius: 3px;
    transition: transform var(--dur-fast) var(--ease-in);
  }
  .chevron.open {
    transform: rotate(90deg);
  }
  .icon {
    flex: none;
    color: var(--fg-3);
    margin: 0 2px;
  }
  .row.selected .icon,
  .row.drop .icon {
    color: var(--accent);
  }
  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .name.unread {
    font-weight: 600;
    color: var(--fg-1);
  }
  .name-input {
    flex: 1;
    min-width: 0;
    height: 24px;
    padding: 0 6px;
    border: 1px solid var(--stroke-2);
    border-bottom-color: var(--accent);
    border-radius: 3px;
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    outline: none;
    user-select: text;
  }
  .name-input[aria-invalid="true"] {
    border-color: var(--danger);
  }
  .name-input::placeholder {
    color: var(--fg-3);
  }
  .row-error {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 2px var(--space-2) 4px calc(var(--space-1) + 16px + 20px + var(--depth) * 16px);
    margin: 0;
    color: var(--danger);
    min-width: 0;
  }
  .row-error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .row-error-close {
    flex: none;
    color: var(--fg-3);
    font-size: 14px;
    line-height: 1;
    padding: 0 4px;
  }
  .count {
    font: var(--text-caption);
    color: var(--accent);
    font-weight: 600;
    min-width: 18px;
    text-align: right;
  }
  /* Кнопка синхронизации одной папки: видна при наведении или фокусе внутри строки. */
  .sync {
    flex: none;
    width: 20px;
    height: 20px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 4px;
    color: var(--fg-3);
    opacity: 0;
    transition:
      opacity var(--dur-fast) var(--ease-in),
      background var(--dur-fast) var(--ease-in);
  }
  .row:hover .sync,
  .row:focus-within .sync,
  .sync:focus-visible {
    opacity: 1;
  }
  .sync:hover:not(:disabled) {
    background: var(--bg-selected);
    color: var(--accent);
  }
  .sync:disabled {
    cursor: default;
    color: var(--fg-4);
  }
  .empty,
  .note {
    padding: var(--space-3) var(--space-2);
    margin: 0;
  }
</style>
