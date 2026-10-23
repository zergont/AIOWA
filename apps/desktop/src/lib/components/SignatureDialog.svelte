<script lang="ts">
  import { untrack } from "svelte";
  import { api, errorMessage, type SignatureDto } from "../api";
  import Editor from "./Editor.svelte";

  interface Props {
    signature: SignatureDto;
    /** Подпись сохранена (кнопкой или импортом из OWA): композер обновляет свою копию. */
    onsaved: (s: SignatureDto) => void;
    onclose: () => void;
  }
  let { signature, onsaved, onclose }: Props = $props();

  const uid = $props.id();
  let html = $state(untrack(() => signature.html));
  let autoAdd = $state(untrack(() => signature.auto_add));
  let busy = $state<"save" | "import" | null>(null);
  let error = $state<string | null>(null);
  let editor = $state<ReturnType<typeof Editor> | null>(null);
  let dialogEl = $state<HTMLDivElement | null>(null);

  $effect(() => {
    untrack(() => editor?.focus());
  });

  const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), [contenteditable="true"], [tabindex]:not([tabindex="-1"])';

  /** Диалог модальный: Tab и Shift+Tab ходят по кругу внутри него, а не уходят в композер под оверлеем. */
  function trapTab(e: KeyboardEvent) {
    if (!dialogEl) return;
    const items = Array.from(dialogEl.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => el.offsetParent !== null);
    if (!items.length) {
      e.preventDefault();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    const inside = active instanceof Node && dialogEl.contains(active);
    if (e.shiftKey && (!inside || active === first)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (!inside || active === last)) {
      e.preventDefault();
      first.focus();
    }
  }

  async function save() {
    if (busy) return;
    busy = "save";
    error = null;
    try {
      await api.signatureSet(html, autoAdd);
      onsaved({ html, auto_add: autoAdd });
      onclose();
    } catch (e) {
      error = errorMessage(e);
    } finally {
      busy = null;
    }
  }

  /** Импорт сразу сохраняет подпись локально (так делает бэкенд), поэтому и композеру сообщаем сразу. */
  async function importOwa() {
    if (busy) return;
    busy = "import";
    error = null;
    try {
      const s = await api.signatureImportOwa();
      html = s.html;
      autoAdd = s.auto_add;
      onsaved(s);
    } catch (e) {
      error = errorMessage(e);
    } finally {
      busy = null;
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape" && !busy) {
      e.preventDefault();
      onclose();
    } else if (e.key === "Tab") {
      trapTab(e);
    }
  }
</script>

<svelte:window onkeydown={onKey} />

<div class="overlay">
  <div class="popup dialog" role="dialog" aria-modal="true" aria-labelledby="{uid}-title" aria-busy={busy != null} bind:this={dialogEl}>
    <h2 class="title" id="{uid}-title">Подпись</h2>
    <Editor bind:this={editor} bind:value={html} minHeight={120} disabled={busy != null} label="Текст подписи" />
    <label class="auto">
      <input type="checkbox" bind:checked={autoAdd} disabled={busy != null} />
      Добавлять автоматически в новые письма и ответы
    </label>
    {#if error}
      <div class="error" role="alert">{error}</div>
    {/if}
    <div class="buttons">
      <button class="btn" onclick={importOwa} disabled={busy != null} title="Взять подпись из настроек OWA на сервере">
        {#if busy === "import"}<span class="spinner" aria-hidden="true"></span>{/if}
        Взять из OWA
      </button>
      <span class="spacer"></span>
      <button class="btn primary" onclick={save} disabled={busy != null}>
        {#if busy === "save"}<span class="spinner" aria-hidden="true"></span>{/if}
        Сохранить
      </button>
      <button class="btn" onclick={onclose} disabled={busy != null}>Отмена</button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 90;
    background: rgba(0, 0, 0, 0.3);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .dialog {
    position: static;
    width: min(640px, calc(100vw - 48px));
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
  }
  .auto {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .error {
    color: var(--danger);
    user-select: text;
  }
  .buttons {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .spacer {
    flex: 1;
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 32px;
    padding: 0 14px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .btn:disabled {
    color: var(--fg-disabled);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn.primary:disabled {
    opacity: 0.6;
    color: var(--fg-on-accent);
  }
  .spinner {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: currentColor;
    animation: spin 0.9s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
