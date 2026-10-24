<script lang="ts">
  import type { Folder } from "../api";
  import { anchorRect, anchorToggle, dismissOn, placePopup, type Anchor } from "../popover";
  import { isTopLayer, layer } from "../hotkeys";

  interface Props {
    folders: Folder[];
    /** Папка, где письма уже лежат: показывается, но выбрать её нельзя. */
    currentFolderPk: number | null;
    anchor: Anchor;
    align?: "start" | "end";
    onpick: (f: Folder) => void;
    onclose: () => void;
  }
  let { folders, currentFolderPk, anchor, align = "end", onpick, onclose }: Props = $props();

  const uid = $props.id();
  const PINNED = ["inbox", "drafts", "sentitems", "deleteditems", "junkemail", "archive"];

  interface Row {
    folder: Folder;
    depth: number;
  }
  interface Node {
    folder: Folder;
    children: Node[];
  }

  /** Дерево папок сплющено в список с отступами: тот же порядок, что в навигации слева. */
  const rows = $derived.by((): Row[] => {
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
    const top: Node[] = [];
    for (const r of roots) {
      if (r.folder.distinguished === "msgfolderroot" || (!r.folder.is_mail && r.children.length && r.folder.total === 0)) top.push(...r.children);
      else top.push(r);
    }
    sortNodes(top);
    const visible = (n: Node): boolean => n.folder.is_mail || n.children.some(visible);
    const out: Row[] = [];
    const walk = (n: Node, depth: number) => {
      if (!visible(n)) return;
      out.push({ folder: n.folder, depth });
      for (const c of n.children) walk(c, depth + 1);
    };
    for (const n of top) walk(n, 0);
    return out;
  });

  let filter = $state("");
  const shown = $derived.by((): Row[] => {
    const q = filter.trim().toLocaleLowerCase("ru");
    if (!q) return rows;
    return rows.filter((r) => r.folder.is_mail && r.folder.name.toLocaleLowerCase("ru").includes(q)).map((r) => ({ folder: r.folder, depth: 0 }));
  });

  function selectable(r: Row): boolean {
    return r.folder.is_mail && r.folder.folder_pk !== currentFolderPk;
  }

  let active = $state(-1);
  // Смена фильтра: активной становится первая доступная строка.
  $effect(() => {
    active = shown.findIndex(selectable);
  });

  function move(step: 1 | -1) {
    const n = shown.length;
    if (!n) return;
    let i = active;
    for (let k = 0; k < n; k++) {
      i = (i + step + n) % n;
      if (selectable(shown[i])) {
        active = i;
        return;
      }
    }
  }

  function pick(r: Row) {
    if (selectable(r)) onpick(r.folder);
  }

  /**
   * Клавиши окна целиком (как у ConfirmPopover): Esc и стрелки работают, даже если фокус ушёл с поля
   * фильтра на строку списка. Только пока окно на экране и верхнее: выбор, оставшийся открытым в
   * спрятанном разделе почты, не переносит письма по Enter из календаря.
   */
  function onKey(e: KeyboardEvent) {
    if (!isTopLayer(el)) return;
    switch (e.key) {
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        onclose();
        break;
      case "ArrowDown":
        e.preventDefault();
        move(1);
        break;
      case "ArrowUp":
        e.preventDefault();
        move(-1);
        break;
      case "Enter":
        e.preventDefault();
        if (active >= 0 && active < shown.length) pick(shown[active]);
        break;
    }
  }

  /** Enter на сфокусированной строке выбирает её саму и не доходит до обработчика окна (иначе выбор был бы двойным). */
  function onOptKey(e: KeyboardEvent, r: Row) {
    if (e.key !== "Enter") return;
    e.preventDefault();
    e.stopPropagation();
    pick(r);
  }

  function optId(r: Row): string {
    return `${uid}-opt-${r.folder.folder_pk}`;
  }

  let el = $state<HTMLDivElement | null>(null);
  let input = $state<HTMLInputElement | null>(null);
  let list = $state<HTMLDivElement | null>(null);
  let pos = $state({ left: -9999, top: -9999 });

  $effect(() => {
    if (!el) return;
    // Высота зависит от числа строк: при фильтрации положение пересчитывается.
    void shown.length;
    pos = placePopup(anchorRect(anchor), el.offsetWidth, el.offsetHeight, align);
  });
  $effect(() => {
    if (!el) return;
    input?.focus();
    return dismissOn(el, onclose, anchorToggle(anchor));
  });
  $effect(() => {
    const r = active >= 0 ? shown[active] : undefined;
    if (!r || !list) return;
    document.getElementById(optId(r))?.scrollIntoView({ block: "nearest" });
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="popup picker" role="dialog" aria-modal="true" aria-label="Переместить в папку" bind:this={el} {@attach layer} style="left: {pos.left}px; top: {pos.top}px">
  <input
    class="filter"
    type="text"
    placeholder="Папка…"
    bind:this={input}
    bind:value={filter}
    autocomplete="off"
    spellcheck="false"
    aria-label="Фильтр папок по имени"
    aria-controls="{uid}-list"
    aria-activedescendant={active >= 0 && shown[active] ? optId(shown[active]) : undefined}
  />
  <div class="list" role="listbox" id="{uid}-list" aria-label="Папки" bind:this={list}>
    {#if shown.length === 0}
      <div class="empty caption">Папок с таким именем нет.</div>
    {/if}
    {#each shown as r, i (r.folder.folder_pk)}
      {@const ok = selectable(r)}
      {@const current = r.folder.folder_pk === currentFolderPk}
      <div
        class="opt"
        class:active={i === active}
        class:disabled={!ok}
        id={optId(r)}
        role="option"
        aria-selected={i === active}
        aria-disabled={!ok}
        tabindex="-1"
        style="--depth: {r.depth}"
        title={current ? "Письма уже в этой папке" : undefined}
        onclick={() => pick(r)}
        onkeydown={(e) => onOptKey(e, r)}
        onpointermove={() => ok && (active = i)}
      >
        <span class="indent"></span>
        <svg class="icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M2 4h4.5l1.5 1.5H14V13H2V4Z" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
        <span class="name">{r.folder.name}</span>
        {#if current}<span class="caption here">текущая</span>{/if}
      </div>
    {/each}
  </div>
</div>

<style>
  .picker {
    width: 280px;
    padding: var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .filter {
    height: 32px;
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    outline: none;
    user-select: text;
  }
  .filter:focus {
    border-bottom: 2px solid var(--accent);
  }
  .filter::placeholder {
    color: var(--fg-3);
  }
  .list {
    max-height: 320px;
    overflow-y: auto;
  }
  .opt {
    display: flex;
    align-items: center;
    gap: 6px;
    height: 32px;
    padding: 0 var(--space-2);
    border-radius: var(--radius-control);
    color: var(--fg-1);
  }
  .opt.active {
    background: var(--bg-selected);
  }
  .opt.disabled {
    color: var(--fg-disabled);
  }
  .indent {
    flex: none;
    width: calc(var(--depth) * 14px);
  }
  .icon {
    flex: none;
    color: var(--fg-3);
  }
  .opt.disabled .icon {
    color: var(--fg-disabled);
  }
  .name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .here {
    flex: none;
  }
  .empty {
    padding: var(--space-2);
  }
</style>
