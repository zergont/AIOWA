<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, errorMessage, type ContactForm, type PersonalContact } from "../api";
  import { focusBack } from "../popover";
  import { isTopLayer, layer } from "../hotkeys";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  /**
   * Форма личного контакта: модально. Новый (`contactPk` null) создаётся в «Контактах»,
   * существующий правится — бэкенд пишет на сервер только изменившиеся поля и перечитывает контакт
   * (`contact_save`, docs/protocol-owa-json.md § 5r). Ctrl+Enter — сохранить, Esc — закрыть (при
   * несохранённых правках — с вопросом). Заметок и почтовых адресов в форме нет: на этом сервере
   * они через персоны не пишутся, у существующего контакта они не трогаются.
   */
  interface Props {
    contactPk: number | null;
    initial: ContactForm;
    /** Контакт записан: родитель закрывает форму и показывает его. */
    onsaved: (c: PersonalContact) => void;
    onclose: () => void;
  }
  let { contactPk, initial, onsaved, onclose }: Props = $props();

  // Экземпляр мог быть уничтожен, пока шёл запрос: его итог тогда никому не нужен.
  let alive = true;
  onDestroy(() => (alive = false));

  const uid = $props.id();
  const isNew = $derived(contactPk == null);
  const start = untrack(() => ({ ...initial }));
  let form = $state<ContactForm>({ ...start });
  let busy = $state(false);
  let error = $state<string | null>(null);
  let confirmCancel = $state(false);
  let dialogEl = $state<HTMLElement | null>(null);
  let cancelBtn = $state<HTMLButtonElement | null>(null);

  const dirty = $derived((Object.keys(start) as (keyof ContactForm)[]).some((k) => form[k].trim() !== start[k].trim()));

  type Field = { key: keyof ContactForm; label: string; type?: "email" | "tel" };
  const GROUPS: { title: string; fields: Field[] }[] = [
    {
      title: "",
      fields: [
        { key: "given_name", label: "Имя" },
        { key: "surname", label: "Фамилия" },
      ],
    },
    {
      title: "Связь",
      fields: [
        { key: "email", label: "Почта", type: "email" },
        { key: "mobile_phone", label: "Мобильный", type: "tel" },
        { key: "business_phone", label: "Рабочий", type: "tel" },
        { key: "home_phone", label: "Домашний", type: "tel" },
      ],
    },
    {
      title: "Работа",
      fields: [
        { key: "company", label: "Компания" },
        { key: "job_title", label: "Должность" },
        { key: "department", label: "Отдел" },
        { key: "office", label: "Офис" },
      ],
    },
  ];

  async function save() {
    if (busy) return;
    if (!isNew && !dirty) {
      onclose();
      return;
    }
    busy = true;
    error = null;
    try {
      const saved = await api.contactSave(contactPk, $state.snapshot(form));
      if (alive) onsaved(saved);
    } catch (e) {
      if (alive) error = errorMessage(e);
    } finally {
      if (alive) busy = false;
    }
  }

  function requestCancel() {
    if (busy) return;
    if (dirty) confirmCancel = true;
    else onclose();
  }
  function keepEditing() {
    confirmCancel = false;
    focusBack(cancelBtn);
  }

  const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), [tabindex]:not([tabindex="-1"])';

  /** Диалог модальный: Tab и Shift+Tab ходят по кругу внутри него. */
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

  function onKey(e: KeyboardEvent) {
    if (confirmCancel || !isTopLayer(dialogEl)) return;
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      requestCancel();
    } else if (e.key === "Tab") {
      trapTab(e);
    }
  }

  onMount(() => {
    document.getElementById(`${uid}-${form.given_name || !form.email ? "given_name" : "surname"}`)?.focus();
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="overlay">
  <div class="popup dialog" role="dialog" aria-modal="true" aria-labelledby="{uid}-title" aria-busy={busy} bind:this={dialogEl} {@attach layer}>
    <h2 class="title" id="{uid}-title">{isNew ? "Новый контакт" : "Изменить контакт"}</h2>
    {#if isNew}
      <div class="hint caption">Контакт появится в папке «Контакты» на сервере — его увидят OWA и телефон.</div>
    {/if}

    {#if error}
      <div class="error" role="alert">
        <span class="error-text">{error}</span>
        <button class="btn small" onclick={() => (error = null)} aria-label="Скрыть сообщение об ошибке">Скрыть</button>
      </div>
    {/if}

    {#each GROUPS as g (g.title)}
      <fieldset class="group" disabled={busy}>
        {#if g.title}<legend>{g.title}</legend>{/if}
        {#each g.fields as f (f.key)}
          <label class="lbl" for="{uid}-{f.key}">{f.label}</label>
          <input id="{uid}-{f.key}" class="field" type={f.type ?? "text"} bind:value={form[f.key]} autocomplete="off" spellcheck="false" />
        {/each}
      </fieldset>
    {/each}

    <div class="buttons">
      <button class="btn" bind:this={cancelBtn} onclick={requestCancel} disabled={busy}>Отмена</button>
      <button class="btn primary" onclick={() => void save()} disabled={busy} title="Сохранить (Ctrl+Enter)" aria-busy={busy}>
        {#if busy}<span class="spinner small" aria-hidden="true"></span>{/if}
        {isNew ? "Создать" : "Сохранить"}
      </button>
    </div>
  </div>
</div>

{#if confirmCancel && cancelBtn}
  <ConfirmPopover anchor={cancelBtn} title="Закрыть без сохранения?" text="Правки в форме пропадут." confirmLabel="Закрыть" onconfirm={onclose} oncancel={keepEditing} />
{/if}

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
    width: min(520px, calc(100vw - 48px));
    max-height: calc(100vh - 48px);
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
  }
  .hint {
    margin-top: calc(-1 * var(--space-2));
  }
  .error {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    color: var(--danger);
  }
  .error-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .group {
    display: grid;
    grid-template-columns: 110px minmax(0, 1fr);
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    border: none;
    min-width: 0;
  }
  .group legend {
    grid-column: 1 / -1;
    padding: var(--space-1) 0 0;
    color: var(--fg-3);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-size: 11px;
  }
  .lbl {
    font: var(--text-caption);
    color: var(--fg-3);
  }
  .field {
    height: var(--control-height);
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    outline: none;
    user-select: text;
    min-width: 0;
  }
  .field:focus {
    border-bottom: 2px solid var(--accent);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
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
  .btn.primary:active:not(:disabled) {
    background: var(--accent-pressed);
  }
  .btn.primary:disabled {
    opacity: 0.6;
    color: var(--fg-on-accent);
  }
  .btn.small {
    height: 24px;
    padding: 0 10px;
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .spinner {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: currentColor;
    animation: spin 0.9s linear infinite;
  }
  .spinner.small {
    width: 10px;
    height: 10px;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation-duration: 2.4s;
    }
  }
</style>
