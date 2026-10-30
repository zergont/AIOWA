<script lang="ts">
  import { onMount } from "svelte";
  import { api, errorMessage, formatSize, type StorageFolder, type StorageInfo } from "../api";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  /**
   * «Хранилище» (docs/obkatka.md п. 15): сколько ящик занимает на сервере — квота и размер каждой
   * папки, в том числе тех, что приложение не синхронизирует, — и сколько на компьютере (письма в
   * базе, скачанные вложения). Папки — по убыванию размера, с путём; «Удалённые» и «Нежелательную»
   * можно очистить отсюда (навсегда, в OWA их ещё можно восстановить).
   */

  let info = $state<StorageInfo | null>(null);
  let loading = $state(false);
  let failure = $state<string | null>(null);
  let status = $state<string | null>(null);
  let confirm = $state<{ f: StorageFolder; anchor: HTMLElement } | null>(null);
  let emptying = $state<string | null>(null);

  async function load() {
    loading = true;
    failure = null;
    try {
      info = await api.storageInfo();
    } catch (e) {
      failure = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  function bytes(n: number | null | undefined): string {
    if (n == null) return "—";
    if (n < 1024 * 1024) return formatSize(n);
    const mb = n / 1024 / 1024;
    if (mb < 1024) return `${mb.toLocaleString("ru-RU", { maximumFractionDigits: mb < 10 ? 1 : 0 })} МБ`;
    return `${(mb / 1024).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} ГБ`;
  }

  const byId = $derived(new Map((info?.folders ?? []).map((f) => [f.server_id, f])));

  /** Путь папки от верхнего уровня ящика: «Входящие / Проекты». */
  function path(f: StorageFolder): string {
    const parts = [f.name || "(без имени)"];
    let cur = f.parent_id ? byId.get(f.parent_id) : undefined;
    for (let hops = 0; cur && hops < 32; hops++) {
      parts.unshift(cur.name || "(без имени)");
      cur = cur.parent_id ? byId.get(cur.parent_id) : undefined;
    }
    return parts.join(" / ");
  }

  /** Папки с содержимым — по убыванию размера на сервере. */
  const rows = $derived((info?.folders ?? []).filter((f) => (f.size_bytes ?? 0) > 0 || (f.total_count ?? 0) > 0).sort((a, b) => (b.size_bytes ?? 0) - (a.size_bytes ?? 0)));
  const serverTotal = $derived((info?.folders ?? []).reduce((s, f) => s + (f.size_bytes ?? 0), 0));
  const quotaLimit = $derived.by(() => {
    const send = info?.quota_send ?? 0;
    const warn = info?.quota_warning ?? 0;
    return send > 0 ? send : warn > 0 ? warn : 0;
  });

  function emptiable(f: StorageFolder): boolean {
    return f.folder_pk != null && (f.distinguished === "deleteditems" || f.distinguished === "junkemail") && (f.total_count ?? 0) > 0;
  }

  async function emptyNow() {
    const c = confirm;
    confirm = null;
    if (!c || c.f.folder_pk == null) return;
    emptying = c.f.server_id;
    try {
      await api.folderEmpty(c.f.folder_pk, true);
      status = `Папка «${c.f.name}» очищена`;
      await load();
    } catch (e) {
      status = errorMessage(e);
    } finally {
      emptying = null;
    }
  }

  onMount(() => void load());
</script>

<div class="storage">
  <div class="top">
    {#if info}
      <dl class="props">
        <dt>На сервере</dt>
        <dd>
          {bytes(info.quota_used ?? serverTotal)}
          {#if quotaLimit > 0}
            из {bytes(quotaLimit)}
          {:else}
            <span class="caption">· ограничений квоты нет</span>
          {/if}
        </dd>
        <dt>На компьютере</dt>
        <dd>база {bytes(info.db_size_bytes)} · вложения {bytes(info.cache_size_bytes)}</dd>
      </dl>
      {#if quotaLimit > 0 && info.quota_used != null}
        <div class="bar" role="meter" aria-valuemin="0" aria-valuemax={quotaLimit} aria-valuenow={info.quota_used} aria-label="Заполнение квоты ящика">
          <span style="width: {Math.min(100, (info.quota_used / quotaLimit) * 100)}%"></span>
        </div>
      {/if}
    {/if}
    <button class="btn" onclick={() => void load()} disabled={loading} aria-busy={loading}>{loading ? "Считаю…" : "Обновить"}</button>
  </div>
  {#if failure}<p class="error" role="alert">{failure}</p>{/if}
  {#if status}<p class="caption" role="status">{status}</p>{/if}

  {#if info}
    <table>
      <thead>
        <tr><th>Папка</th><th class="num">Писем</th><th class="num">На сервере</th><th class="num">На компьютере</th><th></th></tr>
      </thead>
      <tbody>
        {#each rows as f (f.server_id)}
          <tr>
            <td class="name" title={path(f)}>{path(f)}</td>
            <td class="num">{(f.total_count ?? 0).toLocaleString("ru-RU")}</td>
            <td class="num">{bytes(f.size_bytes)}</td>
            <td class="num" title={f.folder_pk == null ? "Папку приложение не синхронизирует" : `Писем в базе: ${f.local_items.toLocaleString("ru-RU")}`}>
              {f.folder_pk == null ? "—" : f.local_cached_bytes > 0 ? `вложения ${bytes(f.local_cached_bytes)}` : f.local_items > 0 ? "только текст" : "—"}
            </td>
            <td class="act">
              {#if emptiable(f)}
                <button class="btn small danger" onclick={(e) => (confirm = { f, anchor: e.currentTarget })} disabled={emptying !== null} aria-haspopup="dialog">Очистить…</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    <p class="hint">Размеры — по данным сервера. «На компьютере» — скачанные вложения писем папки; тексты писем лежат в базе (её размер — вверху). «—» — на компьютере у папки нет ничего: её приложение не синхронизирует или в ней пусто.</p>
  {:else if loading}
    <p class="caption">Спрашиваю сервер о размерах папок…</p>
  {/if}
</div>

{#if confirm}
  <ConfirmPopover
    anchor={confirm.anchor}
    title={`Очистить «${confirm.f.name}»?`}
    text={`Письма (${(confirm.f.total_count ?? 0).toLocaleString("ru-RU")}) и подпапки удалятся навсегда — в OWA их ещё можно вернуть через «Восстановить удалённые элементы».`}
    confirmLabel="Очистить"
    onconfirm={() => void emptyNow()}
    oncancel={() => (confirm = null)}
  />
{/if}

<style>
  .storage {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .top {
    display: flex;
    align-items: flex-start;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .props {
    flex: 1;
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
  }
  .props dt {
    color: var(--fg-3);
  }
  .props dd {
    margin: 0;
  }
  .bar {
    flex-basis: 100%;
    height: 6px;
    border-radius: 3px;
    background: var(--bg-layer-alt);
    overflow: hidden;
  }
  .bar span {
    display: block;
    height: 100%;
    background: var(--accent);
  }
  table {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
  }
  th,
  td {
    padding: 4px 6px;
    border-bottom: 1px solid var(--stroke-1);
    text-align: left;
    font: var(--text-caption);
  }
  th {
    color: var(--fg-3);
    font-weight: normal;
  }
  th:first-child {
    width: 44%;
  }
  .num {
    text-align: right;
    white-space: nowrap;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .act {
    width: 90px;
    text-align: right;
  }
  .error {
    color: var(--danger);
    margin: 0;
  }
  .hint {
    margin: 0;
    color: var(--fg-3);
    font: var(--text-caption);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    height: var(--control-height);
    padding: 0 14px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .btn:disabled {
    color: var(--fg-disabled);
  }
  .btn.small {
    height: 24px;
    padding: 0 10px;
    font: var(--text-caption);
  }
  .btn.danger {
    color: var(--danger);
  }
</style>
