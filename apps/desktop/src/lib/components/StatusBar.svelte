<script lang="ts">
  import type { SyncStage, SyncStatus } from "../api";

  interface Props {
    status: SyncStatus | null;
    /** Короткое уведомление («Отправлено»); родитель сам убирает его по таймеру. */
    notice?: string | null;
    onrefresh: () => void;
    oncancel: () => void;
  }
  let { status, notice = null, onrefresh, oncancel }: Props = $props();

  const STAGES: Record<SyncStage, string> = {
    login: "Вход на сервер",
    hierarchy: "Папки",
    items: "Список писем",
    bodies: "Тексты писем",
    attachments: "Вложения",
  };

  const timeOnly = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });
  const dayTime = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });

  /** «в 14:32» сегодня, иначе «16 окт, 14:32». */
  function when(ms: number): string {
    const d = new Date(ms);
    if (d.toDateString() === new Date().toDateString()) return `в ${timeOnly.format(d)}`;
    return dayTime.format(d).replace(".,", ",").replace(".", "");
  }

  const running = $derived(status?.running ?? false);

  const progressText = $derived.by(() => {
    if (!status || !status.running) return "";
    const stage = status.stage ? STAGES[status.stage] : "Синхронизация";
    const parts = [stage];
    if (status.folder) parts.push(status.folder);
    if (status.stage === "items" || status.stage === "bodies" || status.stage === "attachments") {
      if (status.total != null && status.total > 0) parts.push(`${status.done.toLocaleString("ru-RU")} из ${status.total.toLocaleString("ru-RU")}`);
      else if (status.done > 0) parts.push(status.done.toLocaleString("ru-RU"));
    }
    return parts.join(" · ");
  });

  /** Доля выполнения этапа, если объём известен; иначе null (полоска не рисуется). */
  const fraction = $derived.by(() => {
    if (!status?.running || status.total == null || status.total <= 0) return null;
    return Math.min(1, status.done / status.total);
  });

  const summary = $derived.by(() => {
    if (!status) return "";
    if (status.running) {
      const n = status.items_processed;
      return n > 0 ? `писем обработано: ${n.toLocaleString("ru-RU")}` : "";
    }
    if (status.last_ok_ms != null) return `Обновлено ${when(status.last_ok_ms)}`;
    return "Ещё не синхронизировалось";
  });

  const reportTitle = $derived.by(() => {
    const r = status?.last_report;
    if (!r) return "";
    const lines = [
      `Папок: ${r.folders_synced}`,
      `Писем: +${r.items_created}, изменено ${r.items_updated}, удалено ${r.items_deleted}`,
      `Текстов: ${r.bodies_fetched}${r.bodies_failed ? ` (ошибок ${r.bodies_failed})` : ""}`,
      `Вложений: ${r.attachments_fetched}${r.attachments_failed ? ` (ошибок ${r.attachments_failed})` : ""}`,
      `За ${(r.elapsed_ms / 1000).toFixed(1)} с`,
    ];
    return lines.join("\n");
  });

  const warningsTitle = $derived(status?.warnings.length ? status.warnings.join("\n") : "");
</script>

<div class="bar" role="status" aria-live="polite">
  {#if running}
    <button class="btn" onclick={oncancel} disabled={status?.cancelling}>
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><rect x="2.5" y="2.5" width="7" height="7" rx="1" fill="currentColor" /></svg>
      {status?.cancelling ? "Останавливаю…" : "Остановить"}
    </button>
  {:else}
    <button class="btn" onclick={onrefresh}>
      <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5v3h-3" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
      Обновить
    </button>
  {/if}

  <div class="middle">
    {#if notice}
      <span class="notice">
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {notice}
      </span>
    {/if}
    {#if running}
      <span class="progress-text">{progressText}</span>
      {#if fraction != null}
        <span class="track" aria-hidden="true"><span class="fill" style="width: {Math.round(fraction * 100)}%"></span></span>
      {/if}
    {:else if status?.last_error}
      <span class="error" title={status.last_error}>Ошибка: {status.last_error}</span>
    {/if}
  </div>

  <div class="right">
    {#if status?.warnings_total}
      <span class="warn" title={warningsTitle}>{status.warnings_total} {status.warnings_total === 1 ? "предупреждение" : status.warnings_total < 5 ? "предупреждения" : "предупреждений"}</span>
    {/if}
    <span class="summary" title={reportTitle}>{summary}</span>
  </div>
</div>

<style>
  .bar {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    height: 100%;
    padding: 0 var(--space-3) 0 var(--space-3);
    font: var(--text-caption);
    color: var(--fg-3);
    min-width: 0;
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 22px;
    padding: 0 8px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-2);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .btn:disabled {
    color: var(--fg-disabled);
  }
  .middle {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .progress-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-2);
  }
  .track {
    flex: none;
    width: 160px;
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
  .notice {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--success);
    white-space: nowrap;
  }
  .error {
    color: var(--danger);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .right {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    white-space: nowrap;
  }
  .warn {
    color: var(--warning);
  }
  .summary {
    color: var(--fg-3);
  }
</style>
