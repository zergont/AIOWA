<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import { errorMessage } from "../api";
  import { bytesText, dataApi, storeOwner, syncedText, type StoreInfo } from "../data";
  import { focusBack } from "../popover";

  /**
   * Все базы на диске (вкладка «Данные» настроек): ящик или логин, путь, размер (база с журналами
   * и кэш вложений), писем, последняя синхронизация, отметка текущей. Пока вкладка открыта, список
   * перечитывается раз в несколько секунд — цифры идут вместе с синхронизацией. Не текущую базу
   * можно открыть в Проводнике и удалить (каталог целиком, с подтверждением).
   */
  interface Props {
    /** Свежий размер текущей базы и её кэша вложений — для строк над списком. */
    onusage?: (dbBytes: number, cacheBytes: number) => void;
  }
  let { onusage }: Props = $props();

  const POLL_MS = 5000;
  const uid = $props.id();

  let stores = $state<StoreInfo[] | null>(null);
  let loadError = $state<string | null>(null);
  let loading = false;
  /** Идёт удаление: опрос ждёт, кнопки выключены. */
  let deleting = $state(false);

  async function load() {
    if (loading || deleting) return;
    loading = true;
    try {
      const list = await dataApi.stores();
      stores = list;
      loadError = null;
      const cur = list.find((s) => s.current);
      if (cur && !cur.error) onusage?.(cur.db_bytes, cur.cache_bytes);
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  const others = $derived(stores?.filter((s) => !s.current) ?? []);
  const othersBytes = $derived(others.reduce((n, s) => n + s.total_bytes, 0));

  // --- действия ----------------------------------------------------------------------------

  let actionError = $state<{ path: string; text: string } | null>(null);
  let notice = $state<string | null>(null);
  let asking = $state<string | null>(null);
  const deleteBtns: Record<string, HTMLButtonElement | null> = {};
  let cancelBtn = $state<HTMLButtonElement | null>(null);

  async function openDir(s: StoreInfo) {
    actionError = null;
    try {
      await dataApi.openDir(s.path);
    } catch (e) {
      actionError = { path: s.path, text: errorMessage(e) };
    }
  }

  function ask(s: StoreInfo) {
    asking = s.path;
    actionError = null;
    notice = null;
    // Действие необратимо: фокус — на «Отмена».
    void tick().then(() => cancelBtn?.focus());
  }

  function cancel() {
    const path = asking;
    asking = null;
    if (path) void tick().then(() => focusBack(deleteBtns[path]));
  }

  async function remove(s: StoreInfo) {
    if (deleting) return;
    deleting = true;
    actionError = null;
    try {
      const freed = await dataApi.remove(s.path);
      asking = null;
      notice = `База удалена, освобождено ${bytesText(freed)}`;
    } catch (e) {
      actionError = { path: s.path, text: errorMessage(e) };
    } finally {
      deleting = false;
    }
    await load();
  }

  /** На кнопках вопроса: Esc закрывает вопрос, а не окно настроек целиком. */
  function onConfirmKey(e: KeyboardEvent) {
    if (e.key === "Escape" && !deleting) {
      e.preventDefault();
      e.stopPropagation();
      cancel();
    }
  }

  const KIND_TEXT = (s: StoreInfo) => (s.has_guid ? "ящик записан в базе" : "база старой версии: ящик узнаётся по логину");

  let timer: ReturnType<typeof setInterval> | undefined;
  onMount(() => {
    void load();
    timer = setInterval(() => void load(), POLL_MS);
  });
  onDestroy(() => clearInterval(timer));
</script>

<section class="stores" aria-labelledby="{uid}-title">
  <h4 class="label" id="{uid}-title">Базы на диске</h4>
  <p class="hint">
    Каждая база — почта одного ящика на этом компьютере. Лишние (например, заведённые под другой формой логина) можно удалить: письма на сервере это не трогает. Цифры обновляются каждые несколько секунд.
  </p>
  {#if notice}
    <p class="ok-line" role="status">{notice}</p>
  {/if}
  {#if !stores}
    {#if loadError}
      <p class="err" role="alert">Список баз не загрузился: {loadError}</p>
    {:else}
      <p class="hint">Ищу базы…</p>
    {/if}
  {:else}
    <ul class="list">
      {#each stores as s (s.path)}
        <li class="store" class:current={s.current}>
          <div class="store-head">
            <span class="owner">{storeOwner(s)}</span>
            {#if s.current}<span class="badge">текущая</span>{/if}
            <span class="spacer"></span>
            <span class="size" title="База с журналами: {bytesText(s.db_bytes)}, кэш вложений: {bytesText(s.cache_bytes)}">{bytesText(s.total_bytes)}</span>
          </div>
          <p class="mono path">{s.dir}</p>
          {#if s.error}
            <p class="err">{s.error}</p>
          {:else}
            <p class="meta">
              {#if s.host}{s.host} · {/if}писем {s.items.toLocaleString("ru-RU")} · обновлена {syncedText(s.last_sync_ms)}<br />
              база {bytesText(s.db_bytes)}, вложения {bytesText(s.cache_bytes)} · {KIND_TEXT(s)}
            </p>
          {/if}
          {#if !s.current}
            <div class="row">
              <button class="btn" onclick={() => void openDir(s)} disabled={deleting}>Открыть папку</button>
              <button class="btn" bind:this={deleteBtns[s.path]} onclick={() => ask(s)} disabled={deleting || asking === s.path} aria-expanded={asking === s.path}>Удалить…</button>
            </div>
            {#if asking === s.path}
              <div class="box" role="alertdialog" aria-labelledby="{uid}-del-title" aria-describedby="{uid}-del-text">
                <p class="box-title" id="{uid}-del-title">Удалить эту базу?</p>
                <div id="{uid}-del-text">
                  <p class="text">Каталог базы будет удалён целиком — {bytesText(s.total_bytes)}:</p>
                  <p class="mono path">{s.dir}</p>
                  <p class="text">Письма на сервере это не трогает. Если база понадобится снова, почта скачается с сервера заново.</p>
                </div>
                {#if actionError?.path === s.path}
                  <p class="err" role="alert">{actionError.text}</p>
                {/if}
                <div class="row">
                  <button class="btn danger" onclick={() => void remove(s)} onkeydown={onConfirmKey} disabled={deleting}>
                    {#if deleting}<span class="spinner" aria-hidden="true"></span>{/if}
                    {deleting ? "Удаляю…" : "Удалить базу"}
                  </button>
                  <button class="btn" bind:this={cancelBtn} onclick={cancel} onkeydown={onConfirmKey} disabled={deleting}>Отмена</button>
                </div>
              </div>
            {:else if actionError?.path === s.path}
              <p class="err" role="alert">{actionError.text}</p>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
    {#if others.length > 0}
      <p class="hint">Другие базы занимают {bytesText(othersBytes)}.</p>
    {/if}
  {/if}
</section>

<style>
  .stores {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    width: 100%;
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
  .ok-line {
    margin: 0;
    color: var(--success);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .store {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .store.current {
    border-color: var(--accent);
  }
  .store-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .owner {
    font: var(--text-body-strong);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .badge {
    flex: none;
    padding: 0 6px;
    border-radius: 8px;
    background: var(--accent-soft);
    color: var(--accent);
    font: var(--text-caption);
  }
  .spacer {
    flex: 1;
  }
  .size {
    flex: none;
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  .mono {
    font-family: var(--font-mono);
    font-size: 12px;
  }
  .path {
    margin: 0;
    color: var(--fg-2);
    overflow-wrap: anywhere;
    user-select: text;
  }
  .meta {
    margin: 0;
    font: var(--text-caption);
    color: var(--fg-3);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-1);
  }
  .box {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin-top: var(--space-1);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--danger);
    border-radius: var(--radius-card);
    background: var(--bg-layer);
    outline: 0;
  }
  .box-title {
    margin: 0;
    font: var(--text-body-strong);
  }
  .box .text {
    margin: 0 0 var(--space-1);
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
  .btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--fg-on-accent);
  }
  .btn.danger:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .btn.danger:disabled {
    opacity: 0.6;
    color: var(--fg-on-accent);
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
