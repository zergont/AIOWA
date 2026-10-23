<script lang="ts">
  import { onDestroy } from "svelte";
  import { api, type ComposeRecipient, type RecipientSuggestion } from "../api";
  import { isValidEmail, mergeRecipients, parseAddresses, recipientLabel } from "../addresses";

  interface Props {
    /** Идентификатор поля ввода: на него указывает label снаружи. */
    id: string;
    /** Выбранные получатели; двусторонняя привязка. */
    recipients: ComposeRecipient[];
    disabled?: boolean;
    placeholder?: string;
  }
  let { id, recipients = $bindable([]), disabled = false, placeholder = "" }: Props = $props();

  let input = $state<HTMLInputElement | null>(null);
  let text = $state("");
  let suggestions = $state<RecipientSuggestion[]>([]);
  let active = $state(-1);
  const open = $derived(suggestions.length > 0);

  /** Номер последнего запроса подсказок: ответ устаревшего не показываем. */
  let request = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  function close() {
    request++;
    clearTimeout(timer);
    suggestions = [];
    active = -1;
  }

  async function suggest() {
    const q = text.trim();
    if (!q) {
      close();
      return;
    }
    const n = ++request;
    try {
      const got = await api.recipientsSuggest(q, 8);
      if (n !== request) return;
      const have = new Set(recipients.map((r) => r.email.trim().toLowerCase()));
      suggestions = got.filter((s) => !have.has(s.email.trim().toLowerCase()));
      active = suggestions.length ? 0 : -1;
    } catch {
      // Подсказки — удобство, не действие: отказ бэкенда молча оставляет поле без списка.
      if (n === request) close();
    }
  }

  function onInput() {
    // Разделитель в набранном: всё до него становится чипами.
    if (/[,;]/.test(text)) {
      commitText();
      return;
    }
    clearTimeout(timer);
    timer = setTimeout(() => void suggest(), 120);
  }

  function add(list: ComposeRecipient[]) {
    recipients = mergeRecipients(recipients, list);
  }

  /** Набранный текст → чипы (несколько адресов за раз). */
  function commitText() {
    const parsed = parseAddresses(text);
    if (parsed.length) add(parsed);
    text = "";
    close();
  }

  /** Превратить недонабранный адрес в чип: композер зовёт перед отправкой (Ctrl+Enter не ждёт Enter в поле). */
  export function commit() {
    if (text.trim()) commitText();
  }

  function pick(s: RecipientSuggestion) {
    add([{ name: s.name, email: s.email }]);
    text = "";
    close();
    input?.focus();
  }

  function remove(i: number) {
    recipients = recipients.filter((_, k) => k !== i);
    input?.focus();
  }

  function onKey(e: KeyboardEvent) {
    // Ctrl+Enter и прочие сочетания — дело композера.
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        if (open) active = (active + 1) % suggestions.length;
        else void suggest();
        break;
      case "ArrowUp":
        e.preventDefault();
        if (open) active = (active - 1 + suggestions.length) % suggestions.length;
        break;
      case "Enter":
        e.preventDefault();
        if (open && active >= 0) pick(suggestions[active]);
        else if (text.trim()) commitText();
        break;
      case "Tab":
        if (open && active >= 0) {
          e.preventDefault();
          pick(suggestions[active]);
        } else if (text.trim()) {
          // Чип создаётся, фокус идёт дальше по Tab.
          commitText();
        }
        break;
      case "Escape":
        if (open) {
          e.preventDefault();
          e.stopPropagation();
          close();
        }
        break;
      case "Backspace":
        if (!text && recipients.length) {
          e.preventDefault();
          remove(recipients.length - 1);
        }
        break;
      case ",":
      case ";":
        e.preventDefault();
        commitText();
        break;
    }
  }

  function onPaste(e: ClipboardEvent) {
    const t = e.clipboardData?.getData("text/plain") ?? "";
    if (!/[,;\n<>@]/.test(t)) return;
    e.preventDefault();
    const parsed = parseAddresses(text + t);
    if (parsed.length) add(parsed);
    text = "";
    close();
  }

  function onBlur() {
    if (text.trim()) commitText();
    else close();
  }

  function optId(i: number): string {
    return `${id}-opt-${i}`;
  }

  /** Нажатие по пустому месту рамки ставит фокус в поле; по самому полю и чипам — обычное поведение. */
  function onBoxPointerDown(e: PointerEvent) {
    if (disabled || e.target !== e.currentTarget) return;
    e.preventDefault();
    input?.focus();
  }

  onDestroy(() => clearTimeout(timer));
</script>

<div class="wrap">
  <div class="box" class:disabled role="presentation" onpointerdown={onBoxPointerDown}>
    {#each recipients as r, i (r.email.trim().toLowerCase())}
      {@const ok = isValidEmail(r.email)}
      <span class="chip" class:invalid={!ok} title={ok ? (r.name ? r.email : undefined) : "Неверный адрес"}>
        <span class="chip-text">{recipientLabel(r)}</span>
        <button class="chip-x" onclick={(e) => (e.stopPropagation(), remove(i))} {disabled} aria-label="Убрать {recipientLabel(r)}" tabindex="-1">×</button>
      </span>
    {/each}
    <input
      {id}
      bind:this={input}
      bind:value={text}
      type="text"
      role="combobox"
      aria-expanded={open}
      aria-autocomplete="list"
      aria-controls="{id}-list"
      aria-activedescendant={open && active >= 0 ? optId(active) : undefined}
      autocomplete="off"
      spellcheck="false"
      {disabled}
      placeholder={recipients.length ? "" : placeholder}
      oninput={onInput}
      onkeydown={onKey}
      onpaste={onPaste}
      onblur={onBlur}
    />
  </div>
  {#if open}
    <div class="list" role="listbox" id="{id}-list" aria-label="Подсказки адресатов">
      {#each suggestions as s, i (s.email)}
        <div class="opt" class:active={i === active} id={optId(i)} role="option" aria-selected={i === active} tabindex="-1" onmousedown={(e) => (e.preventDefault(), pick(s))} onpointermove={() => (active = i)}>
          {#if s.name}<span class="opt-name">{s.name}</span>{/if}
          <span class="opt-email caption">{s.email}</span>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .wrap {
    position: relative;
    min-width: 0;
  }
  .box {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    min-height: var(--control-height);
    padding: 3px 6px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    cursor: text;
    transition: border-color var(--dur-fast) var(--ease-in);
  }
  .box:focus-within {
    border-bottom: 2px solid var(--accent);
    padding-bottom: 2px;
  }
  .box.disabled {
    cursor: default;
    color: var(--fg-disabled);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    height: 24px;
    padding: 0 2px 0 8px;
    border-radius: 12px;
    background: var(--accent-soft);
    color: var(--fg-1);
    max-width: 320px;
  }
  .chip.invalid {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--danger);
    color: var(--danger);
  }
  .chip-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .chip-x {
    width: 20px;
    height: 20px;
    border-radius: 50%;
    color: var(--fg-3);
    font-size: 16px;
    line-height: 1;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .chip-x:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .chip-x:disabled {
    color: var(--fg-disabled);
  }
  input {
    flex: 1 1 140px;
    min-width: 60px;
    height: 24px;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 4px;
    user-select: text;
  }
  input::placeholder {
    color: var(--fg-3);
  }
  .list {
    position: absolute;
    left: 0;
    right: 0;
    top: calc(100% + 2px);
    z-index: 60;
    max-width: 480px;
    padding: var(--space-1);
    background: var(--bg-card);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
  }
  .opt {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    height: 32px;
    padding: 0 var(--space-2);
    border-radius: var(--radius-control);
    min-width: 0;
    line-height: 32px;
  }
  .opt.active {
    background: var(--bg-selected);
  }
  .opt-name {
    flex: none;
    max-width: 60%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .opt-email {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
