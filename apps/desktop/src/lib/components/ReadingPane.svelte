<script lang="ts">
  import { untrack } from "svelte";
  import type { Conversation, Message } from "../api";
  import MessageCard from "./MessageCard.svelte";

  interface Props {
    conversation: Conversation | null;
    messages: Message[];
    loading: boolean;
    /** Догрузить письмо с сервера; ошибку пробрасывает, карточка покажет её у себя. */
    onfetch: (itemPk: number) => Promise<void>;
  }
  let { conversation, messages, loading, onfetch }: Props = $props();

  // Последнее письмо развёрнуто, остальные свёрнуты до заголовка.
  // Раскрытие пересчитывается только при смене беседы: точечная замена письма (догрузка тела)
  // и перечитывание после синхронизации не должны сворачивать то, что открыл пользователь.
  let expanded = $state<Set<number>>(new Set());
  let expandedFor: number | null = null;
  let knownPks = new Set<number>();
  $effect(() => {
    const pk = conversation?.conversation_pk ?? null;
    const last = messages[messages.length - 1];
    if (pk === null) {
      expandedFor = null;
      knownPks = new Set();
      return;
    }
    if (expandedFor !== pk) {
      // Новая беседа: письма приходят чуть позже выбора, ждём первую непустую выдачу.
      if (!last) return;
      expandedFor = pk;
      knownPks = new Set(messages.map((m) => m.item_pk));
      expanded = new Set([last.item_pk]);
      return;
    }
    // Та же беседа: пришедшее по синхронизации новое последнее письмо раскрываем, остальное не трогаем.
    const fresh = messages.filter((m) => !knownPks.has(m.item_pk));
    if (!fresh.length) return;
    knownPks = new Set(messages.map((m) => m.item_pk));
    if (last && fresh.some((m) => m.item_pk === last.item_pk)) {
      untrack(() => {
        if (!expanded.has(last.item_pk)) expanded = new Set([...expanded, last.item_pk]);
      });
    }
  });
  function toggle(pk: number) {
    const next = new Set(expanded);
    if (next.has(pk)) next.delete(pk);
    else next.add(pk);
    expanded = next;
  }
  function expandAll() {
    expanded = new Set(messages.map((m) => m.item_pk));
  }
</script>

{#if !conversation}
  <div class="placeholder">
    <svg width="48" height="48" viewBox="0 0 48 48" aria-hidden="true"><rect x="6" y="12" width="36" height="26" rx="3" fill="none" stroke="currentColor" stroke-width="1.5" /><path d="m6 15 18 12 18-12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
    <p>Выбери беседу, чтобы прочитать её здесь.</p>
    <p class="caption">Стрелки или J и K листают список, Ctrl+K открывает поиск.</p>
  </div>
{:else}
  <div class="pane">
    <header class="head">
      <h1 class="topic">{conversation.topic || messages[0]?.subject || "(без темы)"}</h1>
      <div class="meta caption">
        {#if messages.length > 1}
          {messages.length} писем в беседе
          <button class="link" onclick={expandAll}>развернуть все</button>
        {:else if messages.length === 1}
          одно письмо
        {/if}
      </div>
    </header>
    <div class="messages">
      {#if loading && messages.length === 0}
        <div class="caption loading">Загрузка…</div>
      {/if}
      {#each messages as m (m.item_pk)}
        <MessageCard message={m} expanded={expanded.has(m.item_pk)} ontoggle={() => toggle(m.item_pk)} {onfetch} />
      {/each}
    </div>
  </div>
{/if}

<style>
  .placeholder {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: var(--fg-3);
    gap: var(--space-2);
    padding: var(--space-6);
    text-align: center;
  }
  .placeholder p {
    margin: 0;
  }
  .pane {
    height: 100%;
    display: flex;
    flex-direction: column;
  }
  .head {
    padding: var(--space-4) var(--space-6) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
  }
  .topic {
    margin: 0;
    font: var(--text-subtitle);
    user-select: text;
  }
  .meta {
    margin-top: 4px;
    display: flex;
    gap: var(--space-3);
  }
  .link {
    color: var(--accent);
  }
  .link:hover {
    text-decoration: underline;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-3) var(--space-6) var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .loading {
    padding: var(--space-4);
  }
</style>
