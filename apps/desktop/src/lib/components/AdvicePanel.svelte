<script lang="ts">
  /**
   * Панель «Совета» над редактором ответа (docs/ai-layer-design.md § 4.1): что ответить и тон →
   * модель читает беседу и пишет «Суть» (2–4 пункта со ссылками на письма) и «Черновик».
   * Ответ идёт кусками; черновик попадает в письмо только по кнопке. Под черновиком — числа и
   * адреса, которых нет в переписке (их стоит проверить). «Что уйдёт в модель» показывает
   * собранный промпт, ничего не отправляя.
   */
  import { onDestroy } from "svelte";
  import { api, errorMessage, type AdviceEvent, type AdviceKind, type AdviceRequest, type AdviceTone, type AiAdviceStatus, type AiPlace } from "../api";

  interface Props {
    kind: AdviceKind;
    referencePk: number;
    status: AiAdviceStatus;
    /** Текст, уже набранный в письме (пусто — автор ещё ничего не менял). */
    typed: () => string;
    /** Можно заменить текст письма целиком (цитата не развёрнута в редактор). */
    canReplace: boolean;
    oninsert: (text: string, mode: "insert" | "replace") => void;
    onclose: () => void;
  }
  let { kind, referencePk, status, typed, canReplace, oninsert, onclose }: Props = $props();

  const uid = $props.id();
  const PLACE: Record<AiPlace, string> = { local: "этот компьютер", own_server: "свой сервер", cloud: "облако" };
  const TONES: { id: AdviceTone; label: string }[] = [
    { id: "shorter", label: "короче" },
    { id: "longer", label: "подробнее" },
    { id: "softer", label: "мягче" },
    { id: "formal", label: "официальнее" },
  ];

  let direction = $state("");
  let tones = $state<AdviceTone[]>([]);
  let running = $state(false);
  let phase = $state<"collect" | "model" | null>(null);
  let raw = $state("");
  let refs = $state<Record<number, string>>({});
  let truncated = $state(false);
  let done = $state<Extract<AdviceEvent, { type: "done" }> | null>(null);
  let error = $state<string | null>(null);
  let requestId: string | null = null;
  let stopped = false;

  let preview = $state<string | null>(null);
  let previewLoading = $state(false);

  function toggleTone(t: AdviceTone) {
    tones = tones.includes(t) ? tones.filter((x) => x !== t) : [...tones, t];
  }

  function request(): AdviceRequest {
    return { request_id: requestId ?? crypto.randomUUID(), kind, reference_item_pk: referencePk, direction: direction.trim(), tones: [...tones], typed_text: typed() };
  }

  async function run() {
    if (running) return;
    requestId = crypto.randomUUID();
    stopped = false;
    running = true;
    phase = "collect";
    error = null;
    raw = "";
    done = null;
    truncated = false;
    try {
      await api.aiAdvice(request(), (e) => {
        if (e.type === "context") {
          refs = Object.fromEntries(e.messages.map((m) => [m.id, m.label]));
          truncated = e.truncated;
          phase = "model";
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
      phase = null;
      requestId = null;
    }
  }

  function stop() {
    const id = requestId;
    if (!id) return;
    stopped = true;
    void api.aiAdviceCancel(id).catch(() => {});
  }

  onDestroy(() => {
    if (requestId) void api.aiAdviceCancel(requestId).catch(() => {});
  });

  async function showPreview() {
    if (preview != null) {
      preview = null;
      return;
    }
    previewLoading = true;
    try {
      const p = await api.aiAdvicePreview(request());
      preview = `${p.truncated ? "(Беседа длинная — часть писем сокращена)\n\n" : ""}— Системный текст —\n${p.system}\n\n— Данные (${p.chars.toLocaleString("ru-RU")} символов) —\n${p.user}`;
    } catch (e) {
      error = errorMessage(e);
    } finally {
      previewLoading = false;
    }
  }

  // --- разбор ответа ------------------------------------------------------------------------
  // Как `aiowa_ai::advice::split_output`: метки «СУТЬ» и «ЧЕРНОВИК» на своих строках (модель иногда
  // оформляет их жирным или с двоеточием); без меток весь текст — черновик.

  function isMarker(line: string, word: string): boolean {
    return line.trim().replace(/^[*#\s]+|[*#:\s]+$/g, "").toUpperCase() === word;
  }

  const parsed = $derived.by(() => {
    const lines = raw.split("\n");
    const s = lines.findIndex((l) => isMarker(l, "СУТЬ"));
    const d = lines.findIndex((l) => isMarker(l, "ЧЕРНОВИК"));
    if (s >= 0 && d > s) return { summary: lines.slice(s + 1, d).join("\n").trim(), draft: lines.slice(d + 1).join("\n").trim() };
    if (s >= 0) return { summary: lines.slice(s + 1).join("\n").trim(), draft: "" };
    if (d >= 0) return { summary: lines.slice(0, d).join("\n").trim(), draft: lines.slice(d + 1).join("\n").trim() };
    return { summary: "", draft: raw.trim() };
  });

  /** Пункт «Сути»: текст и ссылки на письма (`[msg:N]` → чип, чужие номера — убраны). */
  function points(summary: string): { text: string; ids: number[] }[] {
    return summary
      .split("\n")
      .map((l) => l.replace(/^\s*[-•*]\s*/, "").trim())
      .filter(Boolean)
      .map((l) => {
        const ids = [...l.matchAll(/\[msg:(\d+)\]/g)].map((m) => Number(m[1])).filter((id, i, a) => refs[id] != null && a.indexOf(id) === i);
        return { text: l.replace(/\s*\[msg:\d+\]/g, "").trim(), ids };
      });
  }

  /** Чип письма: прокрутить к его карточке, если беседа на экране. */
  function showMessage(id: number) {
    const card = document.querySelector(`[data-item-pk="${id}"]`);
    card?.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  function onKey(e: KeyboardEvent) {
    // Клавиши панели не уходят композеру: Ctrl+Enter там — «Отправить письмо».
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      void run();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      if (running) stop();
      else onclose();
    }
  }

  const seconds = (ms: number) => `${(ms / 1000).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} с`;
</script>

<section class="advice" aria-label="Совет">
  <div class="a-head">
    <span class="a-title">Совет</span>
    <span class="a-model caption">ИИ · {status.place ? PLACE[status.place] : ""} · {status.profile_name}{status.model ? ` (${status.model})` : ""}</span>
    <span class="spacer"></span>
    <button class="link" onclick={() => void showPreview()} disabled={previewLoading}>{preview != null ? "Скрыть промпт" : previewLoading ? "Собираю…" : "Что уйдёт в модель"}</button>
    <button class="x" onclick={() => (running ? stop() : onclose())} aria-label="Закрыть совет" title="Закрыть">×</button>
  </div>

  <div class="a-ask">
    <input id="{uid}-dir" class="dir" type="text" bind:value={direction} onkeydown={onKey} placeholder="Что ответить — например, «согласиться, но перенести на пятницу»" autocomplete="off" disabled={running} />
    <div class="tones" role="group" aria-label="Тон">
      {#each TONES as t (t.id)}
        <button class="tone" class:on={tones.includes(t.id)} aria-pressed={tones.includes(t.id)} onclick={() => toggleTone(t.id)} disabled={running}>{t.label}</button>
      {/each}
    </div>
    {#if running}
      <button class="btn" onclick={stop}>Остановить</button>
    {:else}
      <button class="btn primary" onclick={() => void run()} title="Ctrl+Enter">{raw || done ? "Ещё вариант" : "Посоветовать"}</button>
    {/if}
  </div>

  {#if preview != null}
    <pre class="preview">{preview}</pre>
  {/if}

  {#if running && !raw}
    <div class="progress caption" aria-live="polite"><span class="bar" aria-hidden="true"></span>{phase === "collect" ? "Собираю беседу…" : "Модель читает переписку…"}</div>
  {/if}

  {#if error}
    <div class="err" role="alert">
      <span>{error}</span>
      <button class="link" onclick={() => void run()}>Повторить</button>
    </div>
  {/if}

  {#if raw}
    {@const p = parsed}
    {#if p.summary}
      <div class="block">
        <div class="b-title caption">Суть</div>
        <ul class="points">
          {#each points(p.summary) as pt, i (i)}
            <li>
              {pt.text}
              {#each pt.ids as id (id)}
                <button class="chip" onclick={() => showMessage(id)} title="Показать письмо">{refs[id]}</button>
              {/each}
            </li>
          {/each}
        </ul>
      </div>
    {/if}
    {#if p.draft}
      <div class="block">
        <div class="b-title caption">Черновик</div>
        <div class="draft">{p.draft}</div>
      </div>
    {/if}
    {#if running}
      <div class="progress caption" aria-live="polite"><span class="bar" aria-hidden="true"></span>Пишет…</div>
    {/if}
  {/if}

  {#if done}
    {#if done.unverified.length}
      <p class="warn caption">Проверь: {done.unverified.join(", ")} — в переписке этого нет.</p>
    {/if}
    {#if done.stop === "max_tokens"}
      <p class="warn caption">Ответ обрезан по длине.</p>
    {:else if done.stop === "refusal"}
      <p class="warn caption">Модель отказалась отвечать.</p>
    {/if}
    {#if truncated}
      <p class="caption muted">Беседа длинная: старые письма в модель ушли сокращёнными.</p>
    {/if}
    <div class="a-actions">
      {#if parsed.draft}
        <button class="btn primary" onclick={() => oninsert(parsed.draft, "insert")}>Вставить в письмо</button>
        {#if canReplace}<button class="btn" onclick={() => oninsert(parsed.draft, "replace")}>Заменить мой текст</button>{/if}
      {/if}
      <span class="spacer"></span>
      <span class="caption muted">{seconds(done.ms)}{done.input_tokens != null ? ` · ${done.input_tokens.toLocaleString("ru-RU")} → ${(done.output_tokens ?? 0).toLocaleString("ru-RU")} токенов` : ""}</span>
    </div>
  {/if}
</section>

<style>
  .advice {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: var(--space-2) var(--space-3) 0;
    padding: var(--space-2) var(--space-3) var(--space-3);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .a-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .a-title {
    font: var(--text-body-strong);
  }
  .a-model {
    color: var(--fg-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .spacer {
    flex: 1;
  }
  .link {
    color: var(--accent);
    font: var(--text-caption);
    white-space: nowrap;
  }
  .link:hover:not(:disabled) {
    text-decoration: underline;
  }
  .x {
    width: 24px;
    height: 24px;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    font-size: 16px;
    line-height: 1;
  }
  .x:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .a-ask {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .dir {
    flex: 1 1 280px;
    min-width: 0;
    height: var(--control-height);
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    color: var(--fg-1);
    font: inherit;
    outline: 0;
  }
  .dir:focus {
    border-bottom: 2px solid var(--accent);
  }
  .tones {
    display: flex;
    gap: 4px;
  }
  .tone {
    height: 24px;
    padding: 0 10px;
    border: 1px solid var(--stroke-2);
    border-radius: 12px;
    font: var(--text-caption);
    color: var(--fg-2);
    background: var(--bg-layer);
  }
  .tone.on {
    border-color: var(--accent);
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: var(--control-height);
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    color: var(--fg-1);
    white-space: nowrap;
  }
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .preview {
    margin: 0;
    max-height: 280px;
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
  .block {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .b-title {
    color: var(--fg-3);
    text-transform: uppercase;
    letter-spacing: 0.04em;
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
  .draft {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--accent);
    background: var(--bg-layer);
    border-radius: 0 var(--radius-control) var(--radius-control) 0;
    white-space: pre-wrap;
    user-select: text;
  }
  .warn {
    margin: 0;
    color: var(--warning, #c58a00);
  }
  .muted {
    margin: 0;
    color: var(--fg-3);
  }
  .a-actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
</style>
