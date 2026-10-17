<script lang="ts">
  import type { Folder } from "../api";

  interface Props {
    folders: Folder[];
    selected: Folder | null;
    onselect: (f: Folder) => void;
  }
  let { folders, selected, onselect }: Props = $props();

  const PINNED = ["inbox", "drafts", "sentitems", "deleteditems", "junkemail", "archive"];
  const HIDDEN_CLASSES_NOTE = "не почтовые папки скрыты";

  let collapsed = $state<Set<number>>(new Set());

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
    collapsed = next;
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
</script>

{#snippet node(n: Node, depth: number)}
  {#if visible(n)}
    {@const f = n.folder}
    {@const kids = n.children.filter(visible)}
    {@const isSelected = selected?.folder_pk === f.folder_pk}
    <div class="row" class:selected={isSelected} style="--depth: {depth}" role="treeitem" aria-selected={isSelected} aria-expanded={kids.length ? !collapsed.has(f.folder_pk) : undefined} tabindex="0" onclick={() => onselect(f)} onkeydown={(e) => e.key === "Enter" && onselect(f)}>
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
      {#if f.unread > 0}
        <span class="count">{f.unread}</span>
      {/if}
    </div>
    {#if kids.length && !collapsed.has(f.folder_pk)}
      {#each kids as child (child.folder.folder_pk)}
        {@render node(child, depth + 1)}
      {/each}
    {/if}
  {/if}
{/snippet}

<div class="tree" role="tree" aria-label="Папки почты">
  {#if folders.length === 0}
    <p class="caption empty">Папок ещё нет: они появятся после первой синхронизации (кнопка «Обновить» внизу).</p>
  {:else}
    {#each tree as n (n.folder.folder_pk)}
      {@render node(n, 0)}
    {/each}
    <p class="caption note">{HIDDEN_CLASSES_NOTE}</p>
  {/if}
</div>

<style>
  .tree {
    display: flex;
    flex-direction: column;
    gap: 1px;
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
    transition: background var(--dur-fast) var(--ease-in);
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
  .row.selected .icon {
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
  .count {
    font: var(--text-caption);
    color: var(--accent);
    font-weight: 600;
    min-width: 18px;
    text-align: right;
  }
  .empty,
  .note {
    padding: var(--space-3) var(--space-2);
    margin: 0;
  }
</style>
