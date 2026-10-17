<script lang="ts">
  import { listDate, personLabel, type SearchHit } from "../api";

  interface Props {
    hits: SearchHit[];
    searching: boolean;
    query: string;
    selectedPk: number | null;
    onopen: (conversationPk: number) => void;
    onclear: () => void;
  }
  let { hits, searching, query, selectedPk, onopen, onclear }: Props = $props();

  /** Сниппет приходит с совпадениями в квадратных скобках; подсвечиваем их. */
  function parts(snippet: string): { text: string; hit: boolean }[] {
    const out: { text: string; hit: boolean }[] = [];
    const re = /\[([^\]]+)\]/g;
    let last = 0;
    for (const m of snippet.matchAll(re)) {
      if (m.index! > last) out.push({ text: snippet.slice(last, m.index), hit: false });
      out.push({ text: m[1], hit: true });
      last = m.index! + m[0].length;
    }
    if (last < snippet.length) out.push({ text: snippet.slice(last), hit: false });
    return out;
  }
</script>

<div class="head">
  <h2 class="title">Поиск</h2>
  <span class="caption">{searching ? "ищу…" : `${hits.length} результатов`}</span>
  <button class="clear" onclick={onclear}>Закрыть</button>
</div>
<div class="scroller" role="listbox" aria-label="Результаты поиска">
  {#if !searching && hits.length === 0}
    <div class="empty">
      <p>Ничего не найдено по запросу «{query}».</p>
      <p class="caption">Операторы: от:, кому:, тема:, вложение:, папка:, после:, до:</p>
    </div>
  {/if}
  {#each hits as h (h.item_pk)}
    <div class="row" class:selected={h.conversation_pk != null && h.conversation_pk === selectedPk} role="option" aria-selected={h.conversation_pk === selectedPk} tabindex="-1" onclick={() => h.conversation_pk != null && onopen(h.conversation_pk)} onkeydown={(e) => e.key === "Enter" && h.conversation_pk != null && onopen(h.conversation_pk)}>
      <div class="line1">
        <span class="people">{personLabel(h.from_name, h.from_email)}</span>
        <span class="date">{listDate(h.received_ms)}</span>
      </div>
      <div class="topic">{h.subject || "(без темы)"}</div>
      <div class="snippet caption">
        {#if h.attachment_name}<span class="file">{h.attachment_name}</span>{/if}
        {#each parts(h.snippet) as p}{#if p.hit}<mark>{p.text}</mark>{:else}{p.text}{/if}{/each}
      </div>
      {#if h.folder_name}<div class="folder caption">{h.folder_name}</div>{/if}
    </div>
  {/each}
</div>

<style>
  .head {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4) var(--space-2);
    border-bottom: 1px solid var(--stroke-1);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
    flex: 1;
  }
  .clear {
    color: var(--accent);
  }
  .clear:hover {
    text-decoration: underline;
  }
  .scroller {
    flex: 1;
    overflow-y: auto;
  }
  .row {
    position: relative;
    padding: 10px var(--space-4) 10px 20px;
    border-bottom: 1px solid var(--stroke-1);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.selected {
    background: var(--bg-selected);
  }
  .row.selected::before {
    content: "";
    position: absolute;
    left: 6px;
    top: 12px;
    bottom: 12px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }
  .line1 {
    display: flex;
    gap: var(--space-2);
  }
  .people {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .date {
    font: var(--text-caption);
    color: var(--fg-3);
    white-space: nowrap;
  }
  .topic {
    color: var(--fg-1);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .snippet {
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  mark {
    background: var(--accent-soft);
    color: var(--fg-1);
    border-radius: 2px;
    padding: 0 1px;
  }
  .file {
    display: inline-block;
    border: 1px solid var(--stroke-2);
    border-radius: 3px;
    padding: 0 4px;
    margin-right: 6px;
    color: var(--fg-2);
  }
  .folder {
    color: var(--fg-4);
  }
  .empty {
    padding: var(--space-6) var(--space-4);
  }
  .empty p {
    margin: 0 0 var(--space-2);
  }
</style>
