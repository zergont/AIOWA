<script lang="ts">
  import { untrack } from "svelte";
  import type { SyncStatus } from "../api";
  import { bytesText, dataApi, type DiskUsage } from "../data";

  /**
   * Размер базы на виду: маленькая подпись в строке состояния — база с журналами и кэш вложений
   * вместе, раздельно — в подсказке. Перечитывается при старте и после каждой синхронизации.
   */
  interface Props {
    status: SyncStatus | null;
  }
  let { status }: Props = $props();

  let usage = $state<DiskUsage | null>(null);

  async function refresh() {
    try {
      usage = await dataApi.usage();
    } catch {
      // Сбой чтения размера не мешает работе: подпись просто не обновится.
    }
  }

  // Синхронизация закончилась (или окно только открылось) — перечитать размер.
  $effect(() => {
    const running = status?.running ?? false;
    void status?.last_ok_ms;
    if (!running) untrack(() => void refresh());
  });

  const title = $derived(usage ? `База писем: ${bytesText(usage.db_bytes)}\nКэш вложений: ${bytesText(usage.cache_bytes)}\nВсе базы на диске — настройки, вкладка «Данные»` : "");
</script>

{#if usage}
  <span class="disk" {title}>
    <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><ellipse cx="8" cy="4" rx="5" ry="2" fill="none" stroke="currentColor" stroke-width="1.3" /><path d="M3 4v8c0 1.1 2.2 2 5 2s5-.9 5-2V4M3 8c0 1.1 2.2 2 5 2s5-.9 5-2" fill="none" stroke="currentColor" stroke-width="1.3" /></svg>
    <span class="sr-only">Размер базы:</span>
    {bytesText(usage.db_bytes + usage.cache_bytes)}
  </span>
{/if}

<style>
  .disk {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    white-space: nowrap;
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
