<script lang="ts">
  /**
   * Вкладка настроек «О программе»: версия, проверка обновлений, «Проверять автоматически», что
   * нового в найденной версии и «Установить и перезапустить» с ходом загрузки. Состояние общее со
   * строкой состояния (updates.svelte.ts); вопрос о несохранённом письме задаёт App до установки.
   */
  import { onMount } from "svelte";
  import Logo from "./Logo.svelte";
  import { errorMessage } from "../api";
  import { checkUpdates, installUpdate, progressFraction, progressText, refreshUpdates, setAutoCheck, updates } from "../updates.svelte";

  const uid = $props.id();
  let loadError = $state<string | null>(null);
  let autoError = $state<string | null>(null);

  onMount(() => {
    refreshUpdates().catch((e) => (loadError = errorMessage(e)));
  });

  const avail = $derived(updates.available);
  const fraction = $derived(progressFraction(updates.progress));

  const timeOnly = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });
  const dayTime = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
  const dayFull = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "long", year: "numeric" });

  /** «сегодня в 14:32» или «16 окт, 14:32». */
  function when(ms: number): string {
    const d = new Date(ms);
    if (d.toDateString() === new Date().toDateString()) return `сегодня в ${timeOnly.format(d)}`;
    return dayTime.format(d).replace(".,", ",").replace(".", "");
  }

  const released = $derived(avail?.date_ms != null ? dayFull.format(new Date(avail.date_ms)).replace(" г.", "") : null);

  async function toggleAuto(e: Event & { currentTarget: HTMLInputElement }) {
    autoError = await setAutoCheck(e.currentTarget.checked);
  }
</script>

<div class="about">
  <h3 class="section-title">О программе</h3>
  <div class="product">
    <Logo size={40} />
    <div class="product-text">
      <p class="name">AIOWA</p>
      <p class="version">Версия {updates.current || "…"}</p>
    </div>
  </div>
  <p class="hint">Почта и календарь Exchange через Outlook Web Access.</p>
  {#if loadError}
    <p class="err" role="alert">Состояние обновлений не загрузилось: {loadError}</p>
  {/if}

  <h3 class="section-title">Обновления</h3>
  {#if updates.installing}
    <div class="box" role="status" aria-live="polite">
      <p class="box-title">{progressText(updates.progress)}</p>
      <span class="track" class:indeterminate={fraction == null} aria-hidden="true"><span class="fill" style="width: {fraction == null ? 30 : Math.round(fraction * 100)}%"></span></span>
      <p class="hint">Не закрывайте приложение: после загрузки оно закроется само, установщик покажет ход установки и запустит новую версию.</p>
    </div>
  {:else if avail}
    <div class="box">
      <p class="box-title">Доступна версия {avail.version}{released ? ` от ${released}` : ""}</p>
      {#if avail.notes}
        <p class="label" id="{uid}-notes">Что нового</p>
        <div class="notes" aria-labelledby="{uid}-notes">{avail.notes}</div>
      {/if}
      <div class="row">
        <button class="btn primary" onclick={() => void installUpdate()} aria-describedby="{uid}-install-hint">Установить и перезапустить</button>
      </div>
      <p class="hint" id="{uid}-install-hint">Приложение скачает установщик и проверит его подпись, отправит на сервер отложенные действия, закроет сессию и запустит установку; новая версия откроется сама. Несохранённое письмо в композере — сначала вопрос.</p>
    </div>
  {:else}
    <p class="state" role="status">
      {#if updates.checking}
        Проверяю обновления…
      {:else if updates.lastCheckMs == null}
        Обновления ещё не проверялись
      {:else if updates.checkError}
        Последняя удачная проверка — {when(updates.lastCheckMs)}
      {:else}
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
        Установлена последняя версия. Проверено {when(updates.lastCheckMs)}
      {/if}
    </p>
  {/if}
  {#if updates.installError}
    <p class="err" role="alert">Обновление не установлено: {updates.installError}</p>
  {/if}
  {#if updates.checkError}
    <p class="err" role="alert">Проверка не удалась: {updates.checkError}</p>
  {/if}
  <div class="row">
    <button class="btn" onclick={() => void checkUpdates()} disabled={updates.checking || updates.installing}>
      {#if updates.checking}<span class="spinner" aria-hidden="true"></span>{/if}
      {updates.checking ? "Проверяю…" : "Проверить обновления"}
    </button>
  </div>

  <label class="check">
    <input type="checkbox" checked={updates.autoCheck} disabled={!updates.canConfigure} onchange={toggleAuto} aria-describedby="{uid}-auto-hint" />
    Проверять обновления автоматически
  </label>
  <p class="hint" id="{uid}-auto-hint">
    {#if autoError}<span class="err">{autoError}</span>{:else}При запуске и раз в сутки приложение запрашивает у github.com описание последней версии; о почте и учётной записи ничего не передаётся. Найденная версия появится строкой внизу окна. Без автопроверки — только кнопкой.{/if}
  </p>
</div>

<style>
  .about {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-4);
    width: 100%;
  }
  .about > * {
    max-width: 100%;
  }
  .section-title {
    margin: 0;
    font: var(--text-body-large);
    font-weight: 600;
  }
  .product {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .product-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .name {
    margin: 0;
    font: var(--text-subtitle);
  }
  .version {
    margin: 0;
    color: var(--fg-2);
    user-select: text;
  }
  .label {
    margin: 0;
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  .hint {
    margin: 0;
    font: var(--text-caption);
    color: var(--fg-3);
    max-width: 60ch;
  }
  .err {
    margin: 0;
    color: var(--danger);
    user-select: text;
    overflow-wrap: anywhere;
  }
  .state {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    color: var(--fg-2);
  }
  .state svg {
    color: var(--success);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .box {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .box-title {
    margin: 0;
    font: var(--text-body-strong);
  }
  .box .row {
    margin-top: var(--space-1);
  }
  .notes {
    max-height: 180px;
    overflow: auto;
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--stroke-2);
    border-radius: 0 var(--radius-control) var(--radius-control) 0;
    background: var(--bg-layer);
    color: var(--fg-2);
    white-space: pre-line;
    overflow-wrap: anywhere;
    user-select: text;
  }
  .track {
    position: relative;
    display: block;
    width: min(360px, 100%);
    height: 4px;
    border-radius: 2px;
    background: var(--stroke-2);
    overflow: hidden;
  }
  .fill {
    display: block;
    height: 100%;
    background: var(--accent);
    transition: width var(--dur-normal) var(--ease-in);
  }
  .track.indeterminate .fill {
    position: absolute;
    animation: slide 1.4s ease-in-out infinite;
  }
  @keyframes slide {
    from {
      left: -30%;
    }
    to {
      left: 100%;
    }
  }
  .check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .check input {
    margin: 0;
    accent-color: var(--accent);
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
  .spinner {
    flex: none;
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
