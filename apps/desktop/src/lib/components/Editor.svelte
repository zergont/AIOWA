<script lang="ts">
  import { untrack } from "svelte";
  import { escapeHtml, normalizeLink, sanitizeHtml } from "../html";
  import { hint } from "../hotkeys";

  interface Props {
    /** HTML редактора; двусторонняя привязка. */
    value: string;
    disabled?: boolean;
    /** Минимальная высота области текста, px. */
    minHeight?: number;
    label?: string;
  }
  let { value = $bindable(""), disabled = false, minHeight = 240, label = "Текст письма" }: Props = $props();

  const uid = $props.id();
  let el = $state<HTMLDivElement | null>(null);

  // Внешнее значение попадает в DOM только когда отличается: после собственного ввода они равны,
  // и каретка не сбивается.
  $effect(() => {
    if (el && el.innerHTML !== value) el.innerHTML = value;
  });

  function onInput() {
    if (el) value = el.innerHTML;
  }

  /** Поставить фокус; каретка — в начало текста (ответ пишется над подписью и цитатой). */
  export function focus() {
    const node = el;
    if (!node) return;
    node.focus();
    const sel = window.getSelection();
    if (!sel) return;
    const range = document.createRange();
    // Внутрь первого блока, а не перед ним: иначе набранное легло бы голым текстом вне абзаца.
    range.setStart(node.firstElementChild ?? node, 0);
    range.collapse(true);
    sel.removeAllRanges();
    sel.addRange(range);
  }

  // --- состояние кнопок панели -------------------------------------------------

  let fmt = $state({ bold: false, italic: false, underline: false, ul: false, ol: false });

  function refresh() {
    if (!el || document.activeElement !== el) return;
    fmt = {
      bold: document.queryCommandState("bold"),
      italic: document.queryCommandState("italic"),
      underline: document.queryCommandState("underline"),
      ul: document.queryCommandState("insertUnorderedList"),
      ol: document.queryCommandState("insertOrderedList"),
    };
  }
  $effect(() => {
    document.addEventListener("selectionchange", refresh);
    return () => document.removeEventListener("selectionchange", refresh);
  });

  function exec(cmd: string, arg?: string) {
    if (disabled || !el) return;
    el.focus();
    document.execCommand(cmd, false, arg);
    onInput();
    refresh();
  }

  /** Кнопки панели не забирают фокус у редактора: иначе пропадёт выделение, к которому применяется команда. */
  function keepFocus(e: MouseEvent) {
    e.preventDefault();
  }

  function onKey(e: KeyboardEvent) {
    if (!(e.ctrlKey || e.metaKey) || e.altKey || e.shiftKey) return;
    const code = e.code;
    if (code === "KeyB") exec("bold");
    else if (code === "KeyI") exec("italic");
    else if (code === "KeyU") exec("underline");
    else if (code === "KeyK") openLink();
    else return;
    e.preventDefault();
    // Ctrl+K в редакторе — ссылка, а не поиск по почте.
    e.stopPropagation();
  }

  // --- вставка: только текст или очищенный HTML ----------------------------------

  function onPaste(e: ClipboardEvent) {
    if (disabled) return;
    const data = e.clipboardData;
    if (!data) return;
    e.preventDefault();
    const html = data.getData("text/html");
    if (html) {
      const clean = sanitizeHtml(html);
      if (clean.trim()) {
        exec("insertHTML", clean);
        return;
      }
    }
    const text = data.getData("text/plain");
    if (text) exec("insertText", text);
  }

  // --- ссылка -------------------------------------------------------------------

  let linkOpen = $state(false);
  let linkUrl = $state("");
  let linkError = $state<string | null>(null);
  let linkInput = $state<HTMLInputElement | null>(null);
  let savedRange: Range | null = null;

  function openLink() {
    if (disabled) return;
    const sel = window.getSelection();
    savedRange = sel && sel.rangeCount > 0 && el?.contains(sel.anchorNode) ? sel.getRangeAt(0).cloneRange() : null;
    linkUrl = "";
    linkError = null;
    linkOpen = true;
  }
  $effect(() => {
    if (linkOpen) untrack(() => linkInput?.focus());
  });

  function applyLink() {
    const url = normalizeLink(linkUrl);
    if (!url) {
      linkError = "Нужен адрес вида https://… или mailto:…";
      return;
    }
    linkOpen = false;
    if (!el) return;
    el.focus();
    const sel = window.getSelection();
    if (sel && savedRange) {
      sel.removeAllRanges();
      sel.addRange(savedRange);
    }
    if (!savedRange || savedRange.collapsed) exec("insertHTML", `<a href="${escapeHtml(url)}">${escapeHtml(url)}</a>`);
    else exec("createLink", url);
    savedRange = null;
  }

  function closeLink() {
    linkOpen = false;
    savedRange = null;
    el?.focus();
  }

  function onLinkKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      applyLink();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeLink();
    }
  }
</script>

<div class="editor" class:disabled>
  <div class="toolbar" role="toolbar" aria-label="Форматирование" aria-controls="{uid}-text">
    <button class="tb" class:on={fmt.bold} onmousedown={keepFocus} onclick={() => exec("bold")} {disabled} aria-label="Жирный" title={hint("Жирный", "editor-bold")} aria-pressed={fmt.bold}><span class="glyph b">Ж</span></button>
    <button class="tb" class:on={fmt.italic} onmousedown={keepFocus} onclick={() => exec("italic")} {disabled} aria-label="Курсив" title={hint("Курсив", "editor-italic")} aria-pressed={fmt.italic}><span class="glyph i">К</span></button>
    <button class="tb" class:on={fmt.underline} onmousedown={keepFocus} onclick={() => exec("underline")} {disabled} aria-label="Подчёркнутый" title={hint("Подчёркнутый", "editor-underline")} aria-pressed={fmt.underline}><span class="glyph u">Ч</span></button>
    <span class="sep" aria-hidden="true"></span>
    <button class="tb" class:on={fmt.ul} onmousedown={keepFocus} onclick={() => exec("insertUnorderedList")} {disabled} aria-label="Маркированный список" title="Маркированный список" aria-pressed={fmt.ul}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M6 4h8M6 8h8M6 12h8" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /><circle cx="3" cy="4" r="1" fill="currentColor" /><circle cx="3" cy="8" r="1" fill="currentColor" /><circle cx="3" cy="12" r="1" fill="currentColor" /></svg>
    </button>
    <button class="tb" class:on={fmt.ol} onmousedown={keepFocus} onclick={() => exec("insertOrderedList")} {disabled} aria-label="Нумерованный список" title="Нумерованный список" aria-pressed={fmt.ol}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M6.5 4h7M6.5 8h7M6.5 12h7" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /><path d="M2.2 3.2 3.2 2.5v3M2 7.2c.3-.5 1.8-.6 1.8.2 0 .7-1.8 1.2-1.8 2.1h2M2 11c.5-.4 1.9-.4 1.9.3 0 .4-.5.6-.9.6.5 0 1 .2 1 .7 0 .7-1.4.8-2 .4" fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round" stroke-linejoin="round" /></svg>
    </button>
    <span class="sep" aria-hidden="true"></span>
    <span class="link-wrap">
      <button class="tb" onmousedown={keepFocus} onclick={openLink} {disabled} aria-label="Ссылка…" title={hint("Ссылка", "editor-link")} aria-haspopup="dialog" aria-expanded={linkOpen}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M6.5 9.5a3 3 0 0 0 4.2 0l2-2a3 3 0 0 0-4.2-4.2l-1 1M9.5 6.5a3 3 0 0 0-4.2 0l-2 2a3 3 0 0 0 4.2 4.2l1-1" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /></svg>
      </button>
      {#if linkOpen}
        <div class="popup link-pop" role="dialog" aria-label="Адрес ссылки">
          <input bind:this={linkInput} bind:value={linkUrl} type="text" placeholder="https://…" aria-label="Адрес ссылки" autocomplete="off" spellcheck="false" onkeydown={onLinkKey} oninput={() => (linkError = null)} />
          <button class="btn primary" onclick={applyLink}>Вставить</button>
          <button class="btn" onclick={closeLink}>Отмена</button>
          {#if linkError}<div class="link-error caption" role="alert">{linkError}</div>{/if}
        </div>
      {/if}
    </span>
    <button class="tb" onmousedown={keepFocus} onclick={() => exec("removeFormat")} {disabled} aria-label="Очистить формат" title="Очистить формат">
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 3h8M7 3l-2.5 8M10 9.5l4 4m0-4-4 4" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" /></svg>
    </button>
  </div>
  <div
    class="text"
    id="{uid}-text"
    bind:this={el}
    contenteditable={disabled ? "false" : "true"}
    role="textbox"
    aria-multiline="true"
    aria-label={label}
    aria-disabled={disabled}
    tabindex={disabled ? -1 : 0}
    spellcheck="true"
    style="min-height: {minHeight}px"
    oninput={onInput}
    onkeydown={onKey}
    onpaste={onPaste}
    onfocus={refresh}
  ></div>
</div>

<style>
  .editor {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    overflow: visible;
  }
  .editor:focus-within {
    border-bottom: 2px solid var(--accent);
  }
  .editor.disabled {
    opacity: 0.7;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 2px;
    padding: 4px 6px;
    border-bottom: 1px solid var(--stroke-1);
  }
  .tb {
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-2);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .tb:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .tb:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .tb.on {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .tb:disabled {
    color: var(--fg-disabled);
  }
  .tb:focus-visible {
    outline-offset: -2px;
  }
  .glyph {
    font: var(--text-body);
    line-height: 1;
  }
  .glyph.b {
    font-weight: 700;
  }
  .glyph.i {
    font-style: italic;
  }
  .glyph.u {
    text-decoration: underline;
  }
  .sep {
    width: 1px;
    height: 16px;
    margin: 0 4px;
    background: var(--stroke-2);
  }
  .link-wrap {
    position: relative;
    display: inline-flex;
  }
  .link-pop {
    position: absolute;
    left: 0;
    top: 32px;
    width: 360px;
    padding: var(--space-2);
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    align-items: center;
  }
  .link-pop input {
    flex: 1;
    min-width: 140px;
    height: 28px;
    padding: 0 8px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    outline: none;
    user-select: text;
  }
  .link-pop input:focus {
    border-bottom: 2px solid var(--accent);
  }
  .link-error {
    width: 100%;
    color: var(--danger);
  }
  .btn {
    height: 28px;
    padding: 0 10px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover {
    background: var(--accent-hover);
  }

  /* Текст письма — светлая «бумага» в любой теме, как фрейм прочитанного письма. */
  .text {
    flex: 1;
    padding: 12px 16px;
    background: #fff;
    color: #1f1f1f;
    font: 14px/1.5 "Segoe UI Variable Text", "Segoe UI", Inter, system-ui, sans-serif;
    outline: none;
    overflow-wrap: anywhere;
    user-select: text;
    cursor: text;
    border-radius: 0 0 var(--radius-control) var(--radius-control);
  }
  .text:focus-visible {
    outline: none;
  }
  .text[contenteditable="false"] {
    cursor: default;
  }
  .text :global(a) {
    color: #005fb8;
  }
  .text :global(blockquote) {
    border-left: 3px solid #ddd;
    margin: 8px 0;
    padding: 0 12px;
    color: #555;
  }
  .text :global(img) {
    max-width: 100%;
    height: auto;
  }
  .text :global(table) {
    max-width: 100%;
  }
</style>
