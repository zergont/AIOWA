<script lang="ts" module>
  export interface MenuItem {
    id: string;
    label: string;
    /** Контур значка (viewBox 0 0 16 16). */
    icon?: string;
    danger?: boolean;
    disabled?: boolean;
    /** Разделитель над пунктом. */
    separator?: boolean;
  }
</script>

<script lang="ts">
  import { anchorRect, dismissOn, placePopup, type Anchor } from "../popover";
  import { layer } from "../hotkeys";

  interface Props {
    items: MenuItem[];
    anchor: Anchor;
    label?: string;
    onpick: (id: string) => void;
    onclose: () => void;
  }
  let { items, anchor, label = "Действия", onpick, onclose }: Props = $props();

  let el = $state<HTMLDivElement | null>(null);
  let pos = $state({ left: -9999, top: -9999 });

  $effect(() => {
    if (!el) return;
    pos = placePopup(anchorRect(anchor), el.offsetWidth, el.offsetHeight);
    focusItem(0);
    return dismissOn(el, onclose);
  });

  function buttons(): HTMLButtonElement[] {
    return el ? Array.from(el.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)')) : [];
  }

  /** Фокус на доступный пункт с индексом i (по кругу). */
  function focusItem(i: number) {
    const bs = buttons();
    if (!bs.length) return;
    bs[((i % bs.length) + bs.length) % bs.length].focus();
  }

  function onKey(e: KeyboardEvent) {
    const bs = buttons();
    const cur = bs.findIndex((b) => b === document.activeElement);
    switch (e.key) {
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        onclose();
        break;
      case "ArrowDown":
        e.preventDefault();
        focusItem(cur + 1);
        break;
      case "ArrowUp":
        e.preventDefault();
        focusItem(cur - 1);
        break;
      case "Home":
        e.preventDefault();
        focusItem(0);
        break;
      case "End":
        e.preventDefault();
        focusItem(bs.length - 1);
        break;
      case "Tab":
        // Меню — одна остановка табуляции: Tab закрывает его, фокус возвращается туда, откуда открыли.
        e.preventDefault();
        onclose();
        break;
    }
  }
</script>

<!-- Меню — слой: пока оно открыто, глобальные клавиши (E, Ctrl+R, Ctrl+Enter композера) не действуют на то, что под ним. -->
<div class="popup menu" role="menu" aria-label={label} tabindex="-1" bind:this={el} {@attach layer} style="left: {pos.left}px; top: {pos.top}px" onkeydown={onKey} oncontextmenu={(e) => e.preventDefault()}>
  {#each items as it (it.id)}
    {#if it.separator}<div class="sep" role="separator"></div>{/if}
    <button class="item" class:danger={it.danger} role="menuitem" tabindex="-1" disabled={it.disabled} onclick={() => onpick(it.id)}>
      <span class="icon" aria-hidden="true">
        {#if it.icon}
          <svg width="16" height="16" viewBox="0 0 16 16"><path d={it.icon} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {/if}
      </span>
      <span class="label">{it.label}</span>
    </button>
  {/each}
</div>

<style>
  .menu {
    min-width: 220px;
    padding: var(--space-1);
    outline: none;
  }
  .item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    height: 32px;
    padding: 0 var(--space-2);
    border-radius: var(--radius-control);
    color: var(--fg-1);
    text-align: left;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .item:hover:not(:disabled),
  .item:focus-visible {
    background: var(--bg-hover);
  }
  .item:focus-visible {
    outline-offset: -2px;
  }
  .item:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .item:disabled {
    color: var(--fg-disabled);
  }
  .item.danger:not(:disabled) {
    color: var(--danger);
  }
  .icon {
    width: 16px;
    height: 16px;
    flex: none;
    display: inline-flex;
    color: var(--fg-3);
  }
  .item.danger .icon {
    color: inherit;
  }
  .label {
    flex: 1;
    white-space: nowrap;
  }
  .sep {
    height: 1px;
    margin: var(--space-1) var(--space-2);
    background: var(--stroke-1);
  }
</style>
