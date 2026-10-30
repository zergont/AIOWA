<script lang="ts">
  import type { SuggestPerson } from "../api";
  import { primePhotos } from "../photos.svelte";
  import Avatar from "./Avatar.svelte";

  /**
   * Выпадающие подсказки под строкой поиска, как в OWA (п. 9 обкатки): раздел «От» с людьми
   * (частые корреспонденты из базы, затем справочник), недавние запросы и «Искать … во всей почте».
   * Нажатия внутри панели не уводят фокус из поля (pointerdown без действия по умолчанию).
   */
  interface Props {
    query: string;
    people: SuggestPerson[];
    recent: string[];
    loading: boolean;
    onfrom: (p: SuggestPerson) => void;
    onto: (p: SuggestPerson) => void;
    oncard: (p: SuggestPerson, anchor: HTMLElement) => void;
    onrecent: (q: string) => void;
    onall: () => void;
  }
  let { query, people, recent, loading, onfrom, onto, oncard, onrecent, onall }: Props = $props();

  const text = $derived(query.trim());
  const showRecent = $derived(!text && recent.length > 0);
  // Фото людей из кэша бэкенда (без сети), когда список меняется.
  $effect(() => {
    void primePhotos(people.map((p) => p.email));
  });
</script>

<div class="panel" role="listbox" aria-label="Подсказки поиска" tabindex="-1" onpointerdown={(e) => e.preventDefault()}>
  {#if people.length}
    <div class="sect caption">От</div>
    {#each people as p (p.email.toLowerCase())}
      <div class="row" role="option" aria-selected="false">
        <button type="button" class="pick" onclick={() => onfrom(p)} title="Письма от {p.email}">
          <Avatar name={p.name} email={p.email} size={28} />
          <span class="who">
            <span class="name">{p.name || p.email}</span>
            {#if p.name}<span class="email caption">{p.email}</span>{/if}
            {#if p.title}<span class="title caption">{p.title}</span>{/if}
          </span>
        </button>
        <button type="button" class="act" onclick={() => onto(p)} title="Письма, где этот адрес среди получателей">кому</button>
        <button type="button" class="act" onclick={(e) => oncard(p, e.currentTarget)} title="Карточка контакта" aria-haspopup="dialog">
          <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm-5 6a5 5 0 0 1 10 0" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
      </div>
    {/each}
  {:else if loading && text}
    <div class="sect caption">От · ищу в справочнике…</div>
  {/if}
  {#if showRecent}
    <div class="sect caption">Недавние запросы</div>
    {#each recent as r (r)}
      <button type="button" class="row pick recent" onclick={() => onrecent(r)}>
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 4v4l2.5 2.5M14 8A6 6 0 1 1 8 2a6 6 0 0 1 6 6Z" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
        <span class="recent-text">{r}</span>
      </button>
    {/each}
  {/if}
  {#if text}
    <button type="button" class="row pick all" onclick={onall}>
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M10.5 10.5 14 14" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      <span class="recent-text">Искать «{text}» во всей почте</span>
    </button>
  {/if}
</div>

<style>
  .panel {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    right: 0;
    z-index: 30;
    background: var(--bg-card);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
    padding: var(--space-1) 0;
    max-height: 60vh;
    overflow: auto;
  }
  .sect {
    padding: 6px var(--space-3) 2px;
    color: var(--fg-4);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-size: 11px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
    padding: 0 var(--space-2) 0 var(--space-3);
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .pick {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: 40px;
    text-align: left;
    color: var(--fg-1);
  }
  .row.pick {
    height: 32px;
    padding: 0 var(--space-3);
  }
  .who {
    min-width: 0;
    display: flex;
    flex-direction: column;
    line-height: 16px;
  }
  .who > span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .act {
    flex: none;
    height: 24px;
    padding: 0 8px;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    font: var(--text-caption);
    display: inline-flex;
    align-items: center;
  }
  .act:hover {
    background: var(--bg-pressed);
    color: var(--fg-1);
  }
  .recent-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .all {
    color: var(--accent);
  }
</style>
