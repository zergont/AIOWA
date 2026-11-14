<script lang="ts">
  /**
   * «Кратко по найденному» (docs/ai-layer-design.md § 5.3): модель читает до двадцати верхних писем
   * выдачи и коротко отвечает на запрос из строки поиска со ссылками на письма. Ответ идёт кусками
   * и показывается только текстом; счётчики (сколько прочитано, за какие дни, от скольких людей)
   * считает код. «Что уйдёт в модель» показывает собранный промпт, ничего не отправляя.
   */
  import { onDestroy, onMount } from "svelte";
  import { api, errorMessage, type AiAdviceStatus, type AiPlace, type DigestEvent, type DigestRequest, type SearchHit } from "../api";

  interface Props {
    query: string;
    hits: SearchHit[];
    status: AiAdviceStatus;
    /** Чип письма: открыть его беседу. */
    onopen: (conversationPk: number) => void;
    onclose: () => void;
  }
  let { query, hits, status, onopen, onclose }: Props = $props();

  const PLACE: Record<AiPlace, string> = { local: "этот компьютер", own_server: "свой сервер", cloud: "облако" };
  /** Столько верхних писем читает модель (`DIGEST_MAX` в src-tauri/src/search.rs). */
  const READ_MAX = 20;

  let running = $state(false);
  let fetching = $state<{ done: number; total: number } | null>(null);
  let raw = $state("");
  let refs = $state<Record<number, string>>({});
  let context = $state<Extract<DigestEvent, { type: "context" }> | null>(null);
  let done = $state<Extract<DigestEvent, { type: "done" }> | null>(null);
  let error = $state<string | null>(null);
  let requestId: string | null = null;
  let stopped = false;
  let preview = $state<string | null>(null);
  let previewLoading = $state(false);

  function request(): DigestRequest {
    return { request_id: requestId ?? crypto.randomUUID(), query, item_pks: hits.slice(0, READ_MAX).map((h) => h.item_pk), total: hits.length };
  }

  async function run() {
    if (running) return;
    requestId = crypto.randomUUID();
    stopped = false;
    running = true;
    error = null;
    raw = "";
    done = null;
    context = null;
    fetching = null;
    try {
      await api.searchDigest(request(), (e) => {
        if (e.type === "progress") {
          fetching = { done: e.done, total: e.total };
        } else if (e.type === "context") {
          refs = Object.fromEntries(e.messages.map((m) => [m.id, m.label]));
          context = e;
          fetching = null;
        } else if (e.type === "text") {
          raw += e.text;
        } else {
          done = e;
        }
      });
    } catch (e) {
      if (!stopped) error = errorMessage(e);
    } finally {
      running = false;
      fetching = null;
      requestId = null;
    }
  }

  function stop() {
    const id = requestId;
    if (!id) return;
    stopped = true;
    void api.searchDigestCancel(id).catch(() => {});
  }

  onMount(() => void run());
  onDestroy(() => {
    if (requestId) void api.searchDigestCancel(requestId).catch(() => {});
  });

  async function showPreview() {
    if (preview != null) {
      preview = null;
      return;
    }
    previewLoading = true;
    try {
      const p = await api.searchDigestPreview(request());
      preview = `${p.truncated ? "(Писем много или они длинные — часть текста сокращена)\n\n" : ""}— Системный текст —\n${p.system}\n\n— Данные (${p.chars.toLocaleString("ru-RU")} символов, писем: ${p.read}) —\n${p.user}`;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      previewLoading = false;
    }
  }

  /** Пункты ответа: текст и ссылки на письма (`[msg:N]` → чип; номера не из контекста убраны). */
  const points = $derived(
    raw
      .split("\n")
      .map((l) => l.replace(/^\s*[-•*]\s*/, "").trim())
      .filter(Boolean)
      .map((l) => {
        const ids = [...l.matchAll(/\[msg:(\d+)\]/g)].map((m) => Number(m[1])).filter((id, i, a) => refs[id] != null && a.indexOf(id) === i);
        return { text: l.replace(/\s*\[msg:\d+\]/g, "").trim(), ids };
      }),
  );

  function showMessage(id: number) {
    const hit = hits.find((h) => h.item_pk === id);
    if (hit?.conversation_pk != null) onopen(hit.conversation_pk);
  }

  const day = (ms: number) => new Date(ms).toLocaleDateString("ru-RU", { day: "numeric", month: "short", year: "numeric" });
  const seconds = (ms: number) => `${(ms / 1000).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} с`;
  /** Что прочитала модель — числами, которые посчитал код. */
  const facts = $derived.by(() => {
    if (!context) return "";
    const read = context.messages.length;
    const parts = [`прочитано писем: ${read} из ${hits.length}${hits.length >= 100 ? "+" : ""} найденных`];
    if (context.first_ms != null && context.last_ms != null) parts.push(day(context.first_ms) === day(context.last_ms) ? day(context.first_ms) : `${day(context.first_ms)} – ${day(context.last_ms)}`);
    parts.push(`отправителей: ${context.senders}`);
    return parts.join(" · ");
  });
</script>

<section class="digest" aria-label="Кратко по найденному">
  <div class="d-head">
    <span class="d-title">Кратко</span>
    <span class="d-model caption">ИИ · {status.place ? PLACE[status.place] : ""} · {status.profile_name}{status.model ? ` (${status.model})` : ""}</span>
    <span class="spacer"></span>
    <button class="x" onclick={() => (running ? stop() : onclose())} aria-label={running ? "Остановить" : "Закрыть «Кратко»"} title={running ? "Остановить" : "Закрыть"}>×</button>
  </div>

  {#if running && !raw}
    <div class="progress caption" aria-live="polite">
      <span class="bar" aria-hidden="true"></span>
      {#if fetching}Догружаю тексты писем: {fetching.done + 1} из {fetching.total}…{:else if context}Модель читает письма…{:else}Собираю письма…{/if}
    </div>
  {/if}

  {#if error}
    <div class="err" role="alert">
      <span>{error}</span>
      <button class="link" onclick={() => void run()}>Повторить</button>
    </div>
  {/if}

  {#if points.length}
    <ul class="points">
      {#each points as pt, i (i)}
        <li>
          {pt.text}
          {#each pt.ids as id (id)}
            <button class="chip" onclick={() => showMessage(id)} title="Открыть письмо">{refs[id]}</button>
          {/each}
        </li>
      {/each}
    </ul>
    {#if running}
      <div class="progress caption" aria-live="polite"><span class="bar" aria-hidden="true"></span>Пишет…</div>
    {/if}
  {/if}

  {#if done}
    {#if done.stop === "max_tokens"}
      <p class="warn caption">Ответ обрезан по длине.</p>
    {:else if done.stop === "refusal"}
      <p class="warn caption">Модель отказалась отвечать.</p>
    {:else if !raw.trim()}
      <p class="warn caption">Модель ответила пустым текстом.</p>
    {/if}
  {/if}

  {#if context && !running}
    <p class="caption muted">{facts}{context.truncated ? " · длинные письма сокращены" : ""}{done ? ` · ${seconds(done.ms)}` : ""}{done?.input_tokens != null ? ` · ${done.input_tokens.toLocaleString("ru-RU")} → ${(done.output_tokens ?? 0).toLocaleString("ru-RU")} токенов` : ""}</p>
  {/if}

  <div class="d-actions">
    <button class="link" onclick={() => void showPreview()} disabled={previewLoading}>{preview != null ? "Скрыть промпт" : previewLoading ? "Собираю…" : "Что уйдёт в модель"}</button>
    {#if !running && (done || error)}
      <button class="link" onclick={() => void run()}>Ещё раз</button>
    {/if}
  </div>
  {#if preview != null}
    <pre class="preview">{preview}</pre>
  {/if}
</section>

<style>
  .digest {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: var(--space-2) var(--space-3);
    padding: var(--space-2) var(--space-3) var(--space-3);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
    max-height: 55%;
    overflow-y: auto;
    flex: none;
  }
  .d-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .d-title {
    font: var(--text-body-strong);
  }
  .d-model {
    color: var(--fg-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .spacer {
    flex: 1;
  }
  .x {
    width: 24px;
    height: 24px;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    font-size: 16px;
    line-height: 1;
    flex: none;
  }
  .x:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .link {
    color: var(--accent);
    font: var(--text-caption);
    white-space: nowrap;
  }
  .link:hover:not(:disabled) {
    text-decoration: underline;
  }
  .progress {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--fg-3);
  }
  /* Тихий индикатор: линия, не спиннер (CONCEPT § 8). */
  .bar {
    width: 48px;
    height: 2px;
    border-radius: 1px;
    background: linear-gradient(90deg, transparent, var(--accent), transparent);
    background-size: 200% 100%;
    animation: slide 1.2s linear infinite;
    flex: none;
  }
  @keyframes slide {
    from {
      background-position: 100% 0;
    }
    to {
      background-position: -100% 0;
    }
  }
  .err {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    color: var(--danger);
    user-select: text;
  }
  .points {
    margin: 0;
    padding-left: 18px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    user-select: text;
  }
  .chip {
    margin-left: 4px;
    padding: 0 6px;
    border-radius: 10px;
    font: var(--text-caption);
    line-height: 18px;
    background: var(--bg-hover);
    color: var(--fg-2);
  }
  .chip:hover {
    color: var(--accent);
  }
  .warn {
    margin: 0;
    color: var(--warning, #c58a00);
  }
  .muted {
    margin: 0;
    color: var(--fg-3);
  }
  .d-actions {
    display: flex;
    gap: var(--space-3);
  }
  .preview {
    margin: 0;
    max-height: 240px;
    overflow: auto;
    padding: var(--space-2);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    font-family: var(--font-mono);
    font-size: 11px;
    white-space: pre-wrap;
    user-select: text;
  }
</style>
