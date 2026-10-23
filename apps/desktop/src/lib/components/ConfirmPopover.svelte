<script lang="ts">
  import { anchorRect, anchorToggle, dismissOn, placePopup, type Anchor } from "../popover";

  interface Props {
    anchor: Anchor;
    align?: "start" | "end";
    title: string;
    text?: string;
    confirmLabel: string;
    onconfirm: () => void;
    oncancel: () => void;
  }
  let { anchor, align = "end", title, text, confirmLabel, onconfirm, oncancel }: Props = $props();

  const uid = $props.id();
  let el = $state<HTMLDivElement | null>(null);
  let confirmBtn = $state<HTMLButtonElement | null>(null);
  let pos = $state({ left: -9999, top: -9999 });

  $effect(() => {
    if (!el) return;
    pos = placePopup(anchorRect(anchor), el.offsetWidth, el.offsetHeight, align);
    confirmBtn?.focus();
    return dismissOn(el, oncancel, anchorToggle(anchor));
  });

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.stopPropagation();
      oncancel();
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="popup confirm" role="alertdialog" aria-modal="true" aria-labelledby="{uid}-title" aria-describedby={text ? `${uid}-text` : undefined} bind:this={el} style="left: {pos.left}px; top: {pos.top}px">
  <p class="title" id="{uid}-title">{title}</p>
  {#if text}<p class="text caption" id="{uid}-text">{text}</p>{/if}
  <div class="buttons">
    <button class="btn danger" bind:this={confirmBtn} onclick={onconfirm}>{confirmLabel}</button>
    <button class="btn" onclick={oncancel}>Отмена</button>
  </div>
</div>

<style>
  .confirm {
    width: 280px;
    padding: var(--space-3) var(--space-4);
  }
  .title {
    margin: 0;
    font: var(--text-body-strong);
  }
  .text {
    margin: var(--space-1) 0 0;
  }
  .buttons {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-3);
    justify-content: flex-end;
  }
  .btn {
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
  .btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--fg-on-accent);
  }
  .btn.danger:hover {
    filter: brightness(1.08);
  }
  .btn.danger:active {
    filter: brightness(0.94);
  }
</style>
