<script lang="ts">
  /**
   * Шапка отдельного окна письма и черновика (п. 32 обкатки): знак, заголовок, свои кнопки и кнопки
   * окна, как у главного окна (п. 14). Пустые места, знак и заголовок перетаскивают окно, двойной
   * щелчок по ним разворачивает.
   */
  import type { Snippet } from "svelte";
  import Logo from "./Logo.svelte";
  import WindowControls from "./WindowControls.svelte";

  interface Props {
    title: string;
    /** Кнопки окна справа от заголовка (перед «Свернуть»). */
    actions?: Snippet;
  }
  let { title, actions }: Props = $props();
</script>

<header class="titlebar" data-tauri-drag-region>
  <span class="brand" data-tauri-drag-region><Logo size={16} /></span>
  <span class="title" data-tauri-drag-region {title}>{title}</span>
  {#if actions}
    <div class="actions">{@render actions()}</div>
  {/if}
  <div class="controls"><WindowControls /></div>
</header>

<style>
  .titlebar {
    flex: none;
    height: 40px;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding-left: var(--space-3);
    background: var(--bg-base);
    border-bottom: 1px solid var(--stroke-1);
    user-select: none;
  }
  .brand {
    display: inline-flex;
  }
  .title {
    flex: 1;
    min-width: 0;
    font: var(--text-body);
    color: var(--fg-2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }
  .controls {
    align-self: stretch;
    display: flex;
    margin-left: var(--space-1);
  }
</style>
