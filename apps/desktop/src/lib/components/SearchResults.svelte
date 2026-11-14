<script lang="ts">
  /**
   * Результаты поиска (docs/ai-layer-design.md § 5.2–5.3): над письмами — как понят запрос на
   * обычном языке (чипы условий и тот же запрос операторами), полоса доводки запроса моделью и
   * панель «Кратко по найденному». Правка чипа — правка текста запроса: скрытого состояния нет.
   */
  import { listDate, personLabel, type AiPlace, type SearchAiStatus, type SearchChip, type SearchHit, type SearchInfo, type SearchRefineState } from "../api";
  import { ICON } from "../actions";
  import SearchDigest from "./SearchDigest.svelte";

  interface Props {
    hits: SearchHit[];
    searching: boolean;
    query: string;
    info: SearchInfo | null;
    refine: SearchRefineState | null;
    /** Что из ИИ доступно поиску; null — ещё не известно или ИИ не настроен. */
    ai: SearchAiStatus | null;
    selectedPk: number | null;
    onopen: (conversationPk: number) => void;
    /** Двойной щелчок по найденному письму: его беседа отдельным окном (п. 32). */
    onopenwindow?: (conversationPk: number, subject: string | null) => void;
    onclear: () => void;
    /** Правка условия: заменить текст запроса с `start` по `end` на `text` (пусто — убрать) и искать заново. */
    onedit: (start: number, end: number, text: string) => void;
    /** Поставить в строку поиска запрос операторами и искать по нему. */
    onoperators: (text: string) => void;
    /** Убрать предложение модели. */
    onrefineclose: () => void;
  }
  let { hits, searching, query, info, refine, ai, selectedPk, onopen, onopenwindow, onclear, onedit, onoperators, onrefineclose }: Props = $props();

  const PLACE: Record<AiPlace, string> = { local: "этот компьютер", own_server: "свой сервер", cloud: "облако" };
  const refineModel = $derived(ai?.refine.available ? `ИИ · ${ai.refine.place ? PLACE[ai.refine.place] : ""} · ${ai.refine.profile_name}${ai.refine.model ? ` (${ai.refine.model})` : ""}` : "ИИ");

  /** Сниппет приходит с совпадениями в квадратных скобках; подсвечиваем их. */
  function parts(snippet: string): { text: string; hit: boolean }[] {
    const out: { text: string; hit: boolean }[] = [];
    const re = /\[([^\]]+)\]/g;
    let last = 0;
    for (const m of snippet.matchAll(re)) {
      if (m.index! > last) out.push({ text: snippet.slice(last, m.index), hit: false });
      out.push({ text: m[1], hit: true });
      last = m.index! + m[0].length;
    }
    if (last < snippet.length) out.push({ text: snippet.slice(last), hit: false });
    return out;
  }

  // --- чипы условий --------------------------------------------------------------------------
  /** Номер чипа с открытым меню вариантов. */
  let menu = $state<number | null>(null);
  let chipsEl = $state<HTMLElement | null>(null);

  // Другая выдача — другие чипы: меню прежнего чипа закрывается.
  $effect(() => {
    void info;
    menu = null;
  });
  // Щелчок мимо меню и Esc закрывают его.
  $effect(() => {
    if (menu === null) return;
    const away = (e: PointerEvent) => {
      if (!(e.target instanceof Node) || !chipsEl?.contains(e.target)) menu = null;
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        menu = null;
      }
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("keydown", key, true);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", key, true);
    };
  });

  function pick(c: SearchChip, text: string) {
    menu = null;
    onedit(c.start, c.end, text);
  }

  // --- «Кратко по найденному» --------------------------------------------------------------------
  /** Запрос, для выдачи которого открыта панель; другая выдача её закрывает. */
  let digestFor = $state<string | null>(null);
  const digestOpen = $derived(digestFor !== null && digestFor === (info?.query ?? query) && hits.length > 0);
  const canDigest = $derived(!!ai?.digest.available && hits.length > 0);

  const count = $derived(hits.length >= 100 ? "100+ результатов" : `${hits.length} результатов`);
</script>

<div class="head">
  <h2 class="title">Поиск</h2>
  <span class="caption">{searching ? "ищу…" : count}</span>
  {#if canDigest && !digestOpen}
    <button class="clear" onclick={() => (digestFor = info?.query ?? query)} disabled={searching} title="Модель прочитает верхние письма и коротко ответит на запрос">Кратко</button>
  {/if}
  <button class="clear" onclick={onclear}>Закрыть</button>
</div>

{#if info && info.chips.length}
  <div class="understood" bind:this={chipsEl} role="group" aria-label="Как понят запрос">
    {#each info.chips as c, i (`${c.start}:${c.kind}`)}
      <span class="chip" class:open={menu === i}>
        <button class="chip-main" onclick={() => (menu = menu === i ? null : i)} aria-haspopup="menu" aria-expanded={menu === i} title={c.note ? `${c.label} — ${c.note}` : c.label}>
          <span class="chip-label">{c.label}</span>
          {#if c.note}<span class="chip-note">· {c.note}</span>{/if}
          <svg width="8" height="8" viewBox="0 0 8 8" aria-hidden="true"><path d="M1 2.5 4 5.5 7 2.5" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="chip-x" onclick={() => pick(c, "")} aria-label="Убрать условие «{c.label}»" title="Убрать условие">×</button>
        {#if menu === i}
          <div class="chip-menu" role="menu">
            {#each c.options as o (o.operator)}
              <button role="menuitem" onclick={() => pick(c, o.operator)}>{o.label}</button>
            {/each}
            <button role="menuitem" onclick={() => pick(c, c.operator)} title={c.operator}>записать оператором</button>
            <button role="menuitem" onclick={() => pick(c, "")}>убрать условие</button>
          </div>
        {/if}
      </span>
    {/each}
    <div class="ops caption" title={info.canonical}>
      <span class="ops-text">в операторах: {info.canonical}</span>
      <button class="link" onclick={() => onoperators(info.canonical)} title="Поставить этот запрос в строку поиска">Править</button>
    </div>
  </div>
{/if}

{#if refine}
  <div class="refine caption" aria-live="polite">
    {#if refine.state === "running"}
      <span class="bar" aria-hidden="true"></span><span class="r-text">Модель уточняет запрос…</span><span class="r-model">{refineModel}</span>
    {:else if refine.state === "error"}
      <span class="r-text">Модель не уточнила запрос: {refine.text}</span>
      <button class="x" onclick={onrefineclose} aria-label="Убрать сообщение" title="Убрать">×</button>
    {:else}
      {@const data = refine.data}
      {@const applied = refine.state === "applied"}
      <div class="r-body">
        <div class="r-text">
          {#if applied}По запросу как он набран ничего не нашлось. Показано по уточнению модели:{:else}Модель поняла запрос иначе (писем: {data.result.hits.length >= 100 ? "100+" : data.result.hits.length}):{/if}
        </div>
        <div class="r-labels">
          {#each data.labels as l (l)}<span class="r-label">{l}</span>{/each}
        </div>
        <div class="r-foot">
          <button class="link" onclick={() => onoperators(data.operators)} title={data.operators}>{applied ? "В строку поиска" : "Применить"}</button>
          <span class="r-model">{refineModel}{data.cached ? " · из памяти" : ""}</span>
        </div>
      </div>
      {#if !applied}
        <button class="x" onclick={onrefineclose} aria-label="Убрать предложение" title="Убрать">×</button>
      {/if}
    {/if}
  </div>
{/if}

{#if digestOpen && ai}
  {#key digestFor}
    <SearchDigest query={info?.query ?? query} {hits} status={ai.digest} {onopen} onclose={() => (digestFor = null)} />
  {/key}
{/if}

<div class="scroller" role="listbox" aria-label="Результаты поиска">
  {#if !searching && hits.length === 0}
    <div class="empty">
      <p>Ничего не найдено по запросу «{query}».</p>
      <p class="caption">Можно писать обычными словами: «письма от Иванова про ремонт за октябрь», «что прислал Вася на прошлой неделе с вложением». Слово в кавычках ищется буквально.</p>
      <p class="caption">Операторы: от:, кому:, переписка:, тема:, вложение:, папка:, после:, до:, дата:, прочитано:нет, флаг:да</p>
    </div>
  {/if}
  {#each hits as h, i (h.item_pk)}
    {#if info?.relaxedNote && i === info.strict}
      <div class="relaxed caption">{info.strict ? `По условию найдено: ${info.strict}.` : "По условию ничего не найдено."} Ниже {info.relaxedNote}.</div>
    {/if}
    <div class="row" class:selected={h.conversation_pk != null && h.conversation_pk === selectedPk} role="option" aria-selected={h.conversation_pk === selectedPk} tabindex="-1" onclick={() => h.conversation_pk != null && onopen(h.conversation_pk)} ondblclick={() => h.conversation_pk != null && onopenwindow?.(h.conversation_pk, h.subject)} onkeydown={(e) => e.key === "Enter" && h.conversation_pk != null && onopen(h.conversation_pk)}>
      <div class="line1">
        <span class="people">{personLabel(h.from_name, h.from_email)}</span>
        <!-- Скрепка — как в списке бесед (п. 29). -->
        {#if h.has_attachments}
          <svg class="clip" width="14" height="14" viewBox="0 0 16 16" aria-label="Есть вложения"><path d={ICON.attach} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" /></svg>
        {/if}
        <span class="date">{listDate(h.received_ms)}</span>
      </div>
      <div class="topic">{h.subject || "(без темы)"}</div>
      <div class="snippet caption">
        {#if h.attachment_name}<span class="file">{h.attachment_name}</span>{/if}
        {#if h.snippet.trim() || h.attachment_name}
          {#each parts(h.snippet) as p}{#if p.hit}<mark>{p.text}</mark>{:else}{p.text}{/if}{/each}
        {:else if h.preview}
          <!-- Фрагмента нет (совпадение в теме или отправителе) — начало текста письма. -->
          <span class="preview">{h.preview}</span>
        {/if}
      </div>
      {#if h.folder_name}<div class="folder caption">{h.folder_name}</div>{/if}
    </div>
  {/each}
</div>

<style>
  .head {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4) var(--space-2);
    border-bottom: 1px solid var(--stroke-1);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
    flex: 1;
  }
  .clear {
    color: var(--accent);
  }
  .clear:hover:not(:disabled) {
    text-decoration: underline;
  }
  .clear:disabled {
    color: var(--fg-4);
  }
  .link {
    color: var(--accent);
    font: var(--text-caption);
    white-space: nowrap;
  }
  .link:hover {
    text-decoration: underline;
  }

  /* Как понят запрос: чипы условий и тот же запрос операторами. */
  .understood {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--stroke-1);
  }
  .chip {
    position: relative;
    display: inline-flex;
    align-items: stretch;
    max-width: 100%;
    height: 24px;
    border: 1px solid var(--stroke-2);
    border-radius: 12px;
    background: var(--bg-layer);
    font: var(--text-caption);
    color: var(--fg-2);
  }
  .chip.open {
    border-color: var(--accent);
  }
  .chip-main {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    min-width: 0;
    padding: 0 4px 0 10px;
    border-radius: 12px 0 0 12px;
    color: inherit;
    font: inherit;
  }
  .chip-main:hover,
  .chip-x:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .chip-label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip-note {
    color: var(--fg-3);
    white-space: nowrap;
  }
  .chip-main svg {
    flex: none;
    color: var(--fg-3);
  }
  .chip-x {
    flex: none;
    width: 22px;
    border-radius: 0 12px 12px 0;
    color: var(--fg-3);
    font-size: 14px;
    line-height: 1;
  }
  .chip-menu {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    z-index: 20;
    display: flex;
    flex-direction: column;
    min-width: 180px;
    max-width: 320px;
    padding: 4px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    background: var(--bg-card);
    box-shadow: var(--shadow-flyout, 0 8px 16px rgb(0 0 0 / 0.14));
  }
  .chip-menu button {
    padding: 5px 10px;
    border-radius: var(--radius-control);
    text-align: left;
    color: var(--fg-1);
    font: var(--text-caption);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip-menu button:hover {
    background: var(--bg-hover);
  }
  .ops {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    flex: 1 1 100%;
    min-width: 0;
    color: var(--fg-3);
  }
  .ops-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }

  /* Доводка запроса моделью: тихая полоса, не диалог. */
  .refine {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4);
    border-bottom: 1px solid var(--stroke-1);
    background: var(--bg-layer-alt);
    color: var(--fg-2);
  }
  .r-body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .r-text {
    flex: 1;
    min-width: 0;
  }
  .r-labels {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .r-label {
    padding: 0 8px;
    border-radius: 10px;
    line-height: 20px;
    background: var(--bg-hover);
    color: var(--fg-1);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .r-foot {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
  }
  .r-model {
    color: var(--fg-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .x {
    flex: none;
    width: 20px;
    height: 20px;
    border-radius: var(--radius-control);
    color: var(--fg-3);
    font-size: 14px;
    line-height: 1;
  }
  .x:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  /* Тихий индикатор: линия, не спиннер (CONCEPT § 8). */
  .bar {
    flex: none;
    align-self: center;
    width: 32px;
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

  .scroller {
    flex: 1;
    overflow-y: auto;
  }
  .relaxed {
    padding: 6px var(--space-4) 6px 20px;
    border-bottom: 1px solid var(--stroke-1);
    background: var(--bg-layer-alt);
    color: var(--fg-3);
  }
  .row {
    position: relative;
    padding: 10px var(--space-4) 10px 20px;
    border-bottom: 1px solid var(--stroke-1);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .row:hover {
    background: var(--bg-hover);
  }
  .row.selected {
    background: var(--bg-selected);
  }
  .row.selected::before {
    content: "";
    position: absolute;
    left: 6px;
    top: 12px;
    bottom: 12px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }
  .line1 {
    display: flex;
    gap: var(--space-2);
  }
  .people {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .date {
    font: var(--text-caption);
    color: var(--fg-3);
    white-space: nowrap;
  }
  .clip {
    flex: none;
    align-self: center;
    color: var(--fg-3);
  }
  .topic {
    color: var(--fg-1);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .snippet {
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .preview {
    color: var(--fg-3);
  }
  mark {
    background: var(--accent-soft);
    color: var(--fg-1);
    border-radius: 2px;
    padding: 0 1px;
  }
  .file {
    display: inline-block;
    border: 1px solid var(--stroke-2);
    border-radius: 3px;
    padding: 0 4px;
    margin-right: 6px;
    color: var(--fg-2);
  }
  .folder {
    color: var(--fg-4);
  }
  .empty {
    padding: var(--space-6) var(--space-4);
  }
  .empty p {
    margin: 0 0 var(--space-2);
  }
</style>
