<script lang="ts">
  /**
   * Строка об обновлении в строке состояния: «Доступна версия X · Установить», ход загрузки и
   * установки, отказ установки. Ненавязчивая: скрывается крестиком до следующей найденной версии.
   * Подписки модуля обновлений поднимает при монтировании (initUpdates).
   */
  import { onMount } from "svelte";
  import { initUpdates, installUpdate, progressFraction, progressText, updates } from "../updates.svelte";

  onMount(() => {
    void initUpdates();
  });

  const offer = $derived(updates.available != null && updates.dismissed !== updates.available.version ? updates.available : null);
  const fraction = $derived(progressFraction(updates.progress));
  const offerTitle = $derived(offer?.notes ? `Что нового в версии ${offer.version}:\n${offer.notes}` : offer ? `Версия ${offer.version}` : "");
</script>

{#if updates.installing}
  <span class="upd busy">
    <span class="spinner" aria-hidden="true"></span>
    <span class="text">{progressText(updates.progress)}</span>
    {#if fraction != null}
      <span class="track" aria-hidden="true"><span class="fill" style="width: {Math.round(fraction * 100)}%"></span></span>
    {/if}
  </span>
{:else if updates.installError}
  <span class="upd failed">
    <span class="text" title={updates.installError}>Обновление не установлено: {updates.installError}</span>
    <button class="x" aria-label="Скрыть сообщение об обновлении" title="Скрыть" onclick={() => (updates.installError = null)}>
      <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /></svg>
    </button>
  </span>
{:else if offer}
  <span class="upd">
    <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2.5v8m-3.5-3.5L8 10.5 11.5 7M3 13.5h10" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
    <span class="text" title={offerTitle}>Доступна версия {offer.version}</span>
    <span class="dot" aria-hidden="true">·</span>
    <button class="link" onclick={() => void installUpdate()} title="Скачать обновление, закрыть приложение, установить новую версию и запустить её">Установить</button>
    <button class="x" aria-label="Скрыть строку об обновлении" title="Скрыть до следующей версии" onclick={() => (updates.dismissed = offer?.version ?? null)}>
      <svg width="10" height="10" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /></svg>
    </button>
  </span>
{/if}

<style>
  .upd {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    max-width: 360px;
    color: var(--accent);
  }
  .upd.busy {
    color: var(--fg-2);
  }
  .upd.failed {
    color: var(--danger);
  }
  .text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .failed .text {
    user-select: text;
  }
  .dot {
    color: var(--fg-4);
  }
  .link {
    height: 18px;
    padding: 0 4px;
    border-radius: 3px;
    color: var(--accent);
    font-weight: 600;
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .link:hover {
    background: var(--bg-hover);
  }
  .link:active {
    background: var(--bg-pressed);
  }
  .x {
    flex: none;
    width: 18px;
    height: 18px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 3px;
    color: var(--fg-3);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .x:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .track {
    flex: none;
    width: 80px;
    height: 3px;
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
  .spinner {
    flex: none;
    width: 10px;
    height: 10px;
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
