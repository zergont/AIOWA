<script lang="ts">
  import type { NotifyStatus, SyncStage, SyncStatus } from "../api";
  import { ICON } from "../actions";
  import { hint } from "../hotkeys";

  interface Props {
    status: SyncStatus | null;
    /** Короткое уведомление («Отправлено»); родитель сам убирает его по таймеру. */
    notice?: string | null;
    /** Канал мгновенных уведомлений; null — бэкенд о нём ещё не сообщал. */
    notify?: NotifyStatus | null;
    /** Отправитель нового письма по уведомлению сервера; родитель убирает строку по таймеру. */
    newMail?: string | null;
    onrefresh: () => void;
    oncancel: () => void;
    /** Тосты включены: канал уведомлений или настройки (пока канал не ответил); null — по каналу. */
    toastsEnabled?: boolean | null;
    /** Текущий период синхронизации по таймеру, минут (для подсказки индикатора канала). */
    syncEveryMin?: number;
    /** Переключатель тостов Windows о новых письмах. */
    ontoasts?: (enabled: boolean) => void;
    /** Клик по строке «Новое письмо»: открыть его во «Входящих». */
    onnewmail?: () => void;
    /**
     * Окно отмены последнего действия (удаление, перенос, архив): текст и срок; полоска убывает
     * до `untilMs`, родитель убирает строку сам. `id` меняется с каждым новым действием — по нему
     * перезапускается анимация.
     */
    undo?: { id: string; text: string; sinceMs: number; untilMs: number } | null;
    onundo?: () => void;
  }
  let { status, notice = null, notify = null, toastsEnabled = null, syncEveryMin = 2, newMail = null, onrefresh, oncancel, ontoasts, onnewmail, undo = null, onundo }: Props = $props();

  const STAGES: Record<SyncStage, string> = {
    login: "Вход на сервер",
    hierarchy: "Папки",
    recent: "Свежие письма",
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

  // --- канал уведомлений -------------------------------------------------------------------

  const live = $derived(notify?.connected ?? false);
  const toasts = $derived(toastsEnabled ?? notify?.toasts_enabled ?? true);

  /** Подсказка индикатора: с какого времени канал открыт, последнее событие, ошибка. */
  const channelTitle = $derived.by(() => {
    if (!notify) return `Канал уведомлений ещё не запущен: обновление по таймеру раз в ${syncEveryMin} мин`;
    const lines: string[] = [];
    if (notify.connected) {
      lines.push(notify.since_ms != null ? `Канал уведомлений открыт ${when(notify.since_ms)}` : "Канал уведомлений открыт");
      lines.push(`Подписок: ${notify.subscriptions}`);
      lines.push(`Страховочная синхронизация раз в ${syncEveryMin} мин`);
    } else {
      lines.push(`Канал уведомлений не подключён: обновление по таймеру раз в ${syncEveryMin} мин`);
    }
    if (notify.last_event_ms != null) lines.push(`Последнее событие ${when(notify.last_event_ms)}`);
    if (notify.error) lines.push(`Ошибка: ${notify.error}`);
    return lines.join("\n");
  });
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
    {#if undo}
      {#key undo.id}
        <span class="undo">
          <span class="undo-text">{undo.text}</span>
          <span class="undo-dot" aria-hidden="true">·</span>
          <button class="undo-btn" onclick={onundo} title={hint("Отменить", "undo")}>
            <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d={ICON.undo} fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
            Отменить
          </button>
          <span class="undo-track" aria-hidden="true"><span class="undo-fill" style="animation-duration: {Math.max(0, undo.untilMs - undo.sinceMs)}ms"></span></span>
        </span>
      {/key}
    {/if}
    {#if notice}
      <span class="notice">
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {notice}
      </span>
    {/if}
    {#if newMail}
      <button class="new-mail" onclick={onnewmail} title="Открыть во «Входящих»">
        <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true"><path d="M2 4h12v9H2V4Zm0 .5 6 4.5 6-4.5" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" /></svg>
        <span class="new-mail-text">Новое письмо: {newMail}</span>
      </button>
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
    <span class="channel" class:live class:failed={!live && !!notify?.error} title={channelTitle}>
      <span class="dot" aria-hidden="true"></span>
      {live ? "мгновенные обновления" : "по таймеру"}
    </span>
    <button class="icon-btn" aria-pressed={toasts} aria-label="Уведомления о новых письмах" title={toasts ? "Уведомления о новых письмах включены" : "Уведомления о новых письмах выключены"} onclick={() => ontoasts?.(!toasts)} disabled={!ontoasts}>
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
        <path d="M8 2.5a3.6 3.6 0 0 0-3.6 3.6v2.4L3 10.6h10l-1.4-2.1V6.1A3.6 3.6 0 0 0 8 2.5Zm-1.5 10a1.5 1.5 0 0 0 3 0" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" />
        {#if !toasts}
          <path d="m3 2.5 10 11" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />
        {/if}
      </svg>
    </button>
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
  /* Окно отмены: текст, кнопка и полоска, убывающая до конца срока. */
  .undo {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    max-width: 60%;
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    border: 1px solid var(--stroke-1);
    color: var(--fg-2);
    overflow: hidden;
  }
  .undo-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .undo-dot {
    color: var(--fg-4);
  }
  .undo-btn {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    height: 18px;
    padding: 0 4px;
    border-radius: 3px;
    color: var(--accent);
    font-weight: 600;
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .undo-btn:hover {
    background: var(--bg-hover);
  }
  .undo-btn:active {
    background: var(--bg-pressed);
  }
  .undo-btn:focus-visible {
    outline-offset: -1px;
  }
  .undo-track {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    height: 2px;
    background: transparent;
  }
  .undo-fill {
    display: block;
    height: 100%;
    width: 100%;
    background: var(--accent);
    animation-name: drain;
    animation-timing-function: linear;
    animation-fill-mode: forwards;
  }
  @keyframes drain {
    from {
      width: 100%;
    }
    to {
      width: 0%;
    }
  }
  .new-mail {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    max-width: 50%;
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-control);
    color: var(--accent);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .new-mail:hover {
    background: var(--bg-hover);
  }
  .new-mail:active {
    background: var(--bg-pressed);
  }
  .new-mail-text {
    overflow: hidden;
    text-overflow: ellipsis;
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
  .channel {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--fg-3);
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--fg-4);
    transition: background var(--dur-normal) var(--ease-in);
  }
  .channel.live .dot {
    background: var(--success);
  }
  .channel.failed .dot {
    background: var(--warning);
  }
  .icon-btn {
    width: 22px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    transition:
      background var(--dur-fast) var(--ease-in),
      color var(--dur-fast) var(--ease-in);
  }
  .icon-btn:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .icon-btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .icon-btn[aria-pressed="false"] {
    color: var(--fg-4);
  }
  .icon-btn:disabled {
    color: var(--fg-disabled);
  }
</style>
