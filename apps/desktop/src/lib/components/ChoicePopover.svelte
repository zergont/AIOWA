<script lang="ts" module>
  /** Вариант ответа в окне выбора. */
  export interface ChoiceOption {
    id: string;
    label: string;
    /** Опасное действие (красная кнопка). */
    danger?: boolean;
    /** Главный вариант (акцентная кнопка). */
    primary?: boolean;
  }
</script>

<script lang="ts">
  import { anchorRect, anchorToggle, dismissOn, placePopup, type Anchor } from "../popover";

  /**
   * Вопрос с несколькими ответами у якоря (как ConfirmPopover, но вариантов больше двух):
   * «Отправить обновление участникам?» — отправить / не отправлять / отмена.
   */
  interface Props {
    anchor: Anchor;
    align?: "start" | "end";
    title: string;
    text?: string;
    options: ChoiceOption[];
    cancelLabel?: string;
    onpick: (id: string) => void;
    oncancel: () => void;
  }
  let { anchor, align = "end", title, text, options, cancelLabel = "Отмена", onpick, oncancel }: Props = $props();

  const uid = $props.id();
  let el = $state<HTMLDivElement | null>(null);
  let firstBtn = $state<HTMLButtonElement | null>(null);
  let pos = $state({ left: -9999, top: -9999 });

  $effect(() => {
    if (!el) return;
    pos = placePopup(anchorRect(anchor), el.offsetWidth, el.offsetHeight, align);
    firstBtn?.focus();
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

<div class="popup choice" role="alertdialog" aria-modal="true" aria-labelledby="{uid}-title" aria-describedby={text ? `${uid}-text` : undefined} bind:this={el} style="left: {pos.left}px; top: {pos.top}px">
  <p class="title" id="{uid}-title">{title}</p>
  {#if text}<p class="text caption" id="{uid}-text">{text}</p>{/if}
  <div class="buttons">
    {#each options as o, i (o.id)}
      {#if i === 0}
        <button class="btn" class:danger={o.danger} class:primary={o.primary} bind:this={firstBtn} onclick={() => onpick(o.id)}>{o.label}</button>
      {:else}
        <button class="btn" class:danger={o.danger} class:primary={o.primary} onclick={() => onpick(o.id)}>{o.label}</button>
      {/if}
    {/each}
    <button class="btn quiet" onclick={oncancel}>{cancelLabel}</button>
  </div>
</div>

<style>
  .choice {
    width: 300px;
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
    flex-direction: column;
    gap: var(--space-2);
    margin-top: var(--space-3);
  }
  .btn {
    height: 30px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    text-align: left;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover {
    background: var(--accent-hover);
  }
  .btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--fg-on-accent);
  }
  .btn.danger:hover {
    filter: brightness(1.08);
  }
  .btn.quiet {
    color: var(--fg-2);
    text-align: center;
  }
</style>
