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
  import { isTopLayer, layer } from "../hotkeys";

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

  /**
   * Вопрос модальный: Tab и Shift+Tab ходят по кругу по его кнопкам. Модальные окна под ним свою
   * ловушку Tab, пока вопрос сверху, не держат — без этой фокус ушёл бы за окна, вопрос закрылся
   * бы по focusout, а Enter нажал бы кнопку фонового окна.
   */
  function trapTab(e: KeyboardEvent) {
    e.preventDefault();
    if (!el) return;
    const items = Array.from(el.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
    if (!items.length) return;
    const i = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = i < 0 ? (e.shiftKey ? items.length - 1 : 0) : (i + (e.shiftKey ? -1 : 1) + items.length) % items.length;
    items[next].focus();
  }

  function onKey(e: KeyboardEvent) {
    // Esc и Tab — только у верхнего окна: вопрос, открытый поверх настроек, не закрывает и их.
    if (!isTopLayer(el)) return;
    if (e.key === "Escape") {
      e.stopPropagation();
      oncancel();
    } else if (e.key === "Tab" && !e.ctrlKey && !e.altKey && !e.metaKey) {
      trapTab(e);
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="popup choice" role="alertdialog" aria-modal="true" aria-labelledby="{uid}-title" aria-describedby={text ? `${uid}-text` : undefined} bind:this={el} {@attach layer} style="left: {pos.left}px; top: {pos.top}px">
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
