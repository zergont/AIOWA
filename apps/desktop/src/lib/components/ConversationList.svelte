<script lang="ts">
  import { listDate, type Conversation, type Folder } from "../api";

  interface Props {
    folder: Folder | null;
    conversations: Conversation[];
    selected: Conversation | null;
    loading: boolean;
    hasMore: boolean;
    onselect: (c: Conversation) => void;
    onmore: () => void;
  }
  let { folder, conversations, selected, loading, hasMore, onselect, onmore }: Props = $props();

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

  function onKey(e: KeyboardEvent) {
    if (!conversations.length) return;
    const idx = selected ? conversations.findIndex((c) => c.conversation_pk === selected!.conversation_pk) : -1;
    if (e.key === "ArrowDown" || e.key === "j") {
      e.preventDefault();
      onselect(conversations[Math.min(conversations.length - 1, idx + 1)]);
    } else if (e.key === "ArrowUp" || e.key === "k") {
      e.preventDefault();
      onselect(conversations[Math.max(0, idx - 1)]);
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

<div class="scroller" bind:this={scroller} onscroll={onScroll} role="listbox" aria-label="Беседы" tabindex="0" onkeydown={onKey}>
  {#if conversations.length === 0 && !loading}
    <div class="empty">
      <p>В этой папке пока нет бесед.</p>
      <p class="caption">Если синхронизация ещё не выполнялась, нажми «Обновить» внизу окна.</p>
    </div>
  {:else}
    <div class="spacer" style="height: {conversations.length * ROW}px">
      {#each slice as c, i (c.conversation_pk)}
        {@const isSelected = selected?.conversation_pk === c.conversation_pk}
        <div class="row" class:selected={isSelected} class:unread={c.unread_count > 0} style="top: {(start + i) * ROW}px" role="option" aria-selected={isSelected} tabindex="-1" onclick={() => onselect(c)} onkeydown={(e) => e.key === "Enter" && onselect(c)}>
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
          </div>
        </div>
      {/each}
    </div>
    {#if loading}
      <div class="loading caption">Загрузка…</div>
    {/if}
  {/if}
</div>

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
  .scroller {
    flex: 1;
    overflow-y: auto;
    position: relative;
    outline: none;
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
