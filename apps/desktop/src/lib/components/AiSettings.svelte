<script lang="ts">
  /**
   * Вкладка «ИИ» в настройках (docs/ai-layer-design.md § 3.4): выключатель ИИ-функций, режим
   * приватности, подключения к моделям (Anthropic и OpenAI-совместимые серверы) с ключами в
   * диспетчере учётных данных Windows, проверка подключения, назначения моделей функциям и журнал
   * обращений без содержимого. Ключ уходит в бэкенд только при сохранении; обратно окно получает
   * лишь «ключ задан».
   */
  import { onMount } from "svelte";
  import { api, errorMessage, type AiJournal, type AiMode, type AiPlace, type AiProfile, type AiProviderKind, type AiSettings, type AiSettingsPatch, type AiTest } from "../api";
  import ConfirmPopover from "./ConfirmPopover.svelte";

  const uid = $props.id();

  let s = $state<AiSettings | null>(null);
  let loadError = $state<string | null>(null);
  let status = $state<{ text: string; error: boolean } | null>(null);
  let statusTimer: ReturnType<typeof setTimeout> | undefined;

  function flash(text: string, error = false) {
    status = { text, error };
    clearTimeout(statusTimer);
    if (!error) statusTimer = setTimeout(() => (status = null), 2500);
  }

  async function load() {
    loadError = null;
    try {
      s = await api.aiSettings();
    } catch (e) {
      loadError = errorMessage(e);
    }
  }

  async function update(patch: AiSettingsPatch) {
    try {
      s = await api.aiSettingsSet(patch);
      flash("Сохранено");
    } catch (e) {
      flash(errorMessage(e), true);
      await load();
    }
  }

  onMount(() => {
    void load();
    void loadJournal();
    return () => clearTimeout(statusTimer);
  });

  // --- подписи ------------------------------------------------------------------------------

  const PLACE: Record<AiPlace, string> = { local: "этот компьютер", own_server: "свой сервер", cloud: "облако" };
  const KIND: Record<AiProviderKind, string> = { anthropic: "Anthropic", openai: "OpenAI-совместимый" };
  const MODES: { id: AiMode; label: string }[] = [
    { id: "any", label: "Любые модели — облачные и свои серверы" },
    { id: "own_only", label: "Только свои — этот компьютер и серверы в сети" },
  ];
  const OUTCOME: Record<string, string> = {
    ok: "готово",
    max_tokens: "обрезано по длине",
    refusal: "модель отказалась",
    other: "готово",
    auth: "ключ не подошёл",
    network: "нет связи",
    timeout: "таймаут",
    rate_limited: "перегрузка или лимит",
    bad_request: "запрос отклонён",
    server: "ошибка сервера",
    not_found: "не найдено",
    protocol: "непонятный ответ",
    cancelled: "отменено",
    policy: "запрещено режимом",
    no_model: "нет модели",
  };
  const nf = new Intl.NumberFormat("ru-RU");
  const tf = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
  const seconds = (ms: number) => `${(ms / 1000).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} с`;
  const profileName = (id: string) => s?.profiles.find((p) => p.id === id)?.name ?? "удалённое подключение";

  // --- форма подключения --------------------------------------------------------------------

  interface Form {
    id: string | null;
    name: string;
    kind: AiProviderKind;
    base_url: string;
    model: string;
    timeout_s: number;
    own_server: boolean;
    extra: string;
    key: string;
    clearKey: boolean;
    hasKey: boolean;
  }
  let form = $state<Form | null>(null);
  let showKey = $state(false);
  let saving = $state(false);
  let formError = $state<string | null>(null);

  /** Заготовки подключений: протокол, название и адрес; модель выбирается после «Проверить». */
  interface Preset {
    label: string;
    kind: AiProviderKind;
    name: string;
    base_url: string;
    model: string;
  }
  const PRESETS: Preset[] = [
    { label: "Anthropic", kind: "anthropic", name: "Anthropic", base_url: "", model: "claude-opus-5-5" },
    { label: "OpenAI", kind: "openai", name: "OpenAI", base_url: "", model: "" },
    { label: "DeepSeek", kind: "openai", name: "DeepSeek", base_url: "https://api.deepseek.com", model: "" },
    { label: "Gemini", kind: "openai", name: "Gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai", model: "" },
    { label: "Свой сервер", kind: "openai", name: "", base_url: "", model: "" },
  ];

  function newForm(pr: Preset) {
    form = { id: null, name: pr.name, kind: pr.kind, base_url: pr.base_url, model: pr.model, timeout_s: 120, own_server: false, extra: "", key: "", clearKey: false, hasKey: false };
    formError = null;
    showKey = false;
  }

  function editForm(p: AiProfile) {
    form = { id: p.id, name: p.name, kind: p.kind, base_url: p.base_url, model: p.model, timeout_s: p.timeout_s, own_server: p.own_server, extra: p.extra ?? "", key: "", clearKey: false, hasKey: p.has_key };
    formError = null;
    showKey = false;
  }

  function setKind(kind: AiProviderKind) {
    if (!form) return;
    const wasDefaultName = form.name === "" || form.name === "Anthropic";
    const wasDefaultModel = form.model === "" || form.model === "claude-opus-5-5";
    form.kind = kind;
    if (wasDefaultName) form.name = kind === "anthropic" ? "Anthropic" : "";
    if (wasDefaultModel) form.model = kind === "anthropic" ? "claude-opus-5-5" : "";
  }

  async function saveForm() {
    const f = form;
    if (!f || saving) return;
    saving = true;
    formError = null;
    try {
      const input = { id: f.id, name: f.name, kind: f.kind, base_url: f.base_url, model: f.model, timeout_s: Number(f.timeout_s) || 120, own_server: f.kind === "openai" && f.own_server, extra: f.kind === "openai" ? f.extra : "" };
      const fresh = await api.aiProfileSave(input, f.key.trim() || null, f.clearKey);
      s = fresh;
      // Ключ в окне не держим дольше нужного.
      f.key = "";
      form = null;
      flash("Подключение сохранено");
    } catch (e) {
      formError = errorMessage(e);
    } finally {
      saving = false;
    }
  }

  // --- проверка, выбор модели, удаление ------------------------------------------------------

  let tests = $state<Record<string, { busy: boolean; result: AiTest | null; error: string | null }>>({});

  async function test(p: AiProfile) {
    tests[p.id] = { busy: true, result: tests[p.id]?.result ?? null, error: null };
    try {
      const result = await api.aiProfileTest(p.id);
      tests[p.id] = { busy: false, result, error: null };
      void loadJournal();
    } catch (e) {
      tests[p.id] = { busy: false, result: null, error: errorMessage(e) };
    }
  }

  async function chooseModel(p: AiProfile, model: string) {
    try {
      s = await api.aiProfileSave({ id: p.id, name: p.name, kind: p.kind, base_url: p.base_url, model, timeout_s: p.timeout_s, own_server: p.own_server, extra: p.extra ?? "" }, null, false);
      flash(`Модель: ${model}`);
      void test({ ...p, model });
    } catch (e) {
      flash(errorMessage(e), true);
    }
  }

  let deleting = $state<{ p: AiProfile; anchor: HTMLElement } | null>(null);

  async function remove(p: AiProfile) {
    deleting = null;
    try {
      s = await api.aiProfileDelete(p.id);
      delete tests[p.id];
      if (form?.id === p.id) form = null;
      flash("Подключение удалено");
    } catch (e) {
      flash(errorMessage(e), true);
    }
  }

  // --- журнал -------------------------------------------------------------------------------

  let journal = $state<AiJournal | null>(null);
  let journalError = $state<string | null>(null);

  async function loadJournal() {
    try {
      journal = await api.aiJournal();
      journalError = null;
    } catch (e) {
      journalError = errorMessage(e);
    }
  }
</script>

<div class="ai">
  <div class="head-row">
    <h3 class="section-title">ИИ</h3>
    <span class="status caption" class:error={status?.error} role="status" aria-live="polite">{status?.text ?? ""}</span>
  </div>

  {#if !s}
    {#if loadError}
      <p class="err" role="alert">Настройки ИИ не загрузились: {loadError}</p>
      <button class="btn" onclick={() => void load()}>Повторить</button>
    {:else}
      <p class="loading caption"><span class="spinner" aria-hidden="true"></span>Загружаю…</p>
    {/if}
  {:else}
    <label class="check">
      <input type="checkbox" checked={s.enabled} onchange={(e) => void update({ enabled: e.currentTarget.checked })} aria-describedby="{uid}-enabled-hint" />
      ИИ-функции
    </label>
    <p class="hint" id="{uid}-enabled-hint">Включает «Совет» при ответе (кнопка в письме, которое пишешь); поиск на обычном языке появится в следующих выпусках. Пока выключено, AIOWA сама к моделям не обращается; «Проверить» работает только по кнопке.</p>

    <fieldset class="group">
      <legend class="label">Куда можно отправлять письма</legend>
      <div class="radios">
        {#each MODES as m (m.id)}
          <label class="radio">
            <input type="radio" name="{uid}-mode" value={m.id} checked={s.mode === m.id} onchange={() => void update({ mode: m.id })} />
            {m.label}
          </label>
        {/each}
      </div>
      <p class="hint">Каждое обращение к модели пишется в журнал ниже: куда, какая модель, сколько токенов — без текста писем и ответов.</p>
    </fieldset>

    <section class="block" aria-labelledby="{uid}-profiles">
      <h4 class="label" id="{uid}-profiles">Подключения</h4>
      {#if !s.profiles.length && !form}
        <p class="hint">Подключений пока нет. Anthropic — облачные модели Claude по ключу; OpenAI, DeepSeek, Gemini и свои серверы с моделями (vLLM, Ollama, LM Studio) — по OpenAI-совместимому протоколу.</p>
      {/if}
      {#each s.profiles as p (p.id)}
        {@const t = tests[p.id]}
        <div class="profile">
          <div class="p-head">
            <span class="p-name">{p.name}</span>
            <span class="chip" class:cloud={p.place === "cloud"}>{PLACE[p.place]}</span>
            {#if s.mode === "own_only" && p.place === "cloud"}<span class="chip off">выключено режимом</span>{/if}
          </div>
          <div class="p-meta caption">
            {KIND[p.kind]} · <span class="mono">{p.api_base || p.base_url}</span> · {p.model ? p.model : "модель не выбрана"} · {p.has_key ? "ключ задан" : "без ключа"}{#if p.extra}{" · "}<span class="mono">{p.extra}</span>{/if}
          </div>
          <div class="row">
            <button class="btn small" onclick={() => void test(p)} disabled={t?.busy}>
              {#if t?.busy}<span class="spinner" aria-hidden="true"></span>{/if}
              {t?.busy ? "Проверяю…" : "Проверить"}
            </button>
            <button class="btn small" onclick={() => editForm(p)} disabled={form?.id === p.id}>Изменить</button>
            <button class="btn small" onclick={(e) => (deleting = { p, anchor: e.currentTarget })}>Удалить</button>
          </div>
          {#if t?.error}
            <p class="err" role="alert">{t.error}</p>
          {:else if t?.result}
            {@const r = t.result}
            <div class="test" role="status">
              {#if r.models_error}
                <p class="err">Список моделей: {r.models_error}</p>
              {:else}
                <p class="t-line">Моделей на сервере: {nf.format(r.models.length)}{r.model_listed === false ? ` — выбранной «${p.model}» среди них нет` : ""}</p>
                {#if r.models.length && (!p.model || r.model_listed === false)}
                  <div class="models">
                    {#each r.models.slice(0, 40) as m (m.id)}
                      <button class="model" onclick={() => void chooseModel(p, m.id)} title={[m.name, m.context ? `контекст ${nf.format(m.context)} токенов` : ""].filter(Boolean).join(" · ") || m.id}>{m.id}</button>
                    {/each}
                  </div>
                  <p class="hint">Нажмите модель, чтобы выбрать её для подключения.</p>
                {/if}
              {/if}
              {#if r.reply_error}
                <p class="err">Ответ модели: {r.reply_error}</p>
              {:else if r.reply != null}
                <p class="ok-line">
                  <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
                  Ответ: «{r.reply}»
                </p>
                <p class="t-line caption">
                  {r.first_ms != null ? `первый кусок через ${seconds(r.first_ms)}, ` : ""}весь ответ {seconds(r.total_ms ?? 0)}
                  {#if r.input_tokens != null || r.output_tokens != null} · токенов: {nf.format(r.input_tokens ?? 0)} → {nf.format(r.output_tokens ?? 0)}{/if}
                  {#if r.served_model} · {r.served_model}{/if}
                </p>
              {/if}
            </div>
          {/if}
        </div>
      {/each}

      {#if form}
        {@const f = form}
        <form class="box" onsubmit={(e) => (e.preventDefault(), void saveForm())}>
          <p class="box-title">{f.id ? "Изменить подключение" : "Новое подключение"}</p>
          <fieldset class="group">
            <legend class="label">Протокол</legend>
            <div class="radios">
              <label class="radio"><input type="radio" name="{uid}-kind" checked={f.kind === "anthropic"} onchange={() => setKind("anthropic")} />Anthropic (Claude)</label>
              <label class="radio"><input type="radio" name="{uid}-kind" checked={f.kind === "openai"} onchange={() => setKind("openai")} />OpenAI-совместимый: OpenAI, vLLM, Ollama, LM Studio</label>
            </div>
          </fieldset>
          <div class="field">
            <label class="label" for="{uid}-name">Название</label>
            <input id="{uid}-name" type="text" bind:value={f.name} placeholder={f.kind === "anthropic" ? "Anthropic" : "Например, «GPU-сервер»"} autocomplete="off" spellcheck="false" />
          </div>
          <div class="field">
            <label class="label" for="{uid}-url">Адрес</label>
            <input id="{uid}-url" type="text" class="wide mono" bind:value={f.base_url} placeholder={f.kind === "anthropic" ? "https://api.anthropic.com (можно не заполнять)" : "http://сервер:8000/v1 · пусто — облако OpenAI"} autocomplete="off" spellcheck="false" />
            <p class="hint">{f.kind === "anthropic" ? "Пусто — адрес Anthropic. Свой адрес — только для прокси." : "Пусто — облако OpenAI. DeepSeek — https://api.deepseek.com, Gemini — https://generativelanguage.googleapis.com/v1beta/openai. Свой сервер — корень или путь с /v1: Ollama — http://сервер:11434, LM Studio — http://сервер:1234, vLLM — http://сервер:8000."}</p>
          </div>
          <div class="field">
            <label class="label" for="{uid}-key">Ключ</label>
            <div class="password">
              <input id="{uid}-key" type={showKey ? "text" : "password"} bind:value={f.key} disabled={f.clearKey} placeholder={f.hasKey ? "Задан — оставьте пустым, чтобы не менять" : f.kind === "anthropic" ? "sk-ant-…" : "Если сервер его требует"} autocomplete="off" spellcheck="false" />
              <button type="button" class="eye" onclick={() => (showKey = !showKey)} aria-pressed={showKey} aria-label={showKey ? "Скрыть ключ" : "Показать ключ"} title={showKey ? "Скрыть ключ" : "Показать ключ"}>
                <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8Z" fill="none" stroke="currentColor" stroke-width="1.2" /><circle cx="8" cy="8" r="2" fill="none" stroke="currentColor" stroke-width="1.2" /></svg>
              </button>
            </div>
            {#if f.hasKey}
              <label class="check caption"><input type="checkbox" bind:checked={f.clearKey} />Убрать сохранённый ключ</label>
            {/if}
            <p class="hint">Хранится в диспетчере учётных данных Windows. Сменили адрес без нового ключа — старый удаляется, чтобы не уйти на другой сервер.</p>
          </div>
          <div class="field">
            <label class="label" for="{uid}-model">Модель</label>
            <input id="{uid}-model" type="text" class="wide mono" bind:value={f.model} placeholder={f.kind === "anthropic" ? "claude-opus-5-5" : "можно выбрать после «Проверить»"} autocomplete="off" spellcheck="false" />
          </div>
          <div class="field">
            <label class="label" for="{uid}-timeout">Таймаут ответа, секунд</label>
            <input id="{uid}-timeout" type="number" class="num" min="10" max="900" bind:value={f.timeout_s} />
            <p class="hint">Сколько ждать первого куска ответа и паузы между кусками. Своим серверам с большой моделью — больше.</p>
          </div>
          {#if f.kind === "openai"}
            <label class="check"><input type="checkbox" bind:checked={f.own_server} />Это свой сервер (имя похоже на внешнее, но сервер свой)</label>
            <div class="field">
              <label class="label" for="{uid}-extra">Дополнительные поля запроса</label>
              <input id="{uid}-extra" type="text" class="wide mono" bind:value={f.extra} placeholder={'{"priority": "interactive", "notes": false}'} autocomplete="off" spellcheck="false" />
              <p class="hint">JSON-объект для расширений своего шлюза — добавляется к каждому запросу. Шлюз Горыныч: priority, notes, reasoning_effort (у smart не ставьте xhigh — это совет на 15–30 минут). Обычным серверам не нужно.</p>
            </div>
          {/if}
          {#if formError}<p class="err" role="alert">{formError}</p>{/if}
          <div class="row">
            <button class="btn primary" type="submit" disabled={saving}>
              {#if saving}<span class="spinner" aria-hidden="true"></span>{/if}
              Сохранить
            </button>
            <button class="btn" type="button" onclick={() => (form = null)} disabled={saving}>Отмена</button>
          </div>
        </form>
      {:else}
        <div class="row">
          <span class="caption add-label">Добавить:</span>
          {#each PRESETS as pr (pr.label)}
            <button class="btn" onclick={() => newForm(pr)}>{pr.label}</button>
          {/each}
        </div>
      {/if}
    </section>

    {#if s.profiles.length}
      <section class="block" aria-labelledby="{uid}-assign">
        <h4 class="label" id="{uid}-assign">Какая модель для чего</h4>
        <div class="field">
          <label class="label sub" for="{uid}-advice">«Совет» при ответе</label>
          <select id="{uid}-advice" value={s.advice_profile ?? ""} onchange={(e) => void update({ advice_profile: e.currentTarget.value || null })}>
            <option value="">— не назначено —</option>
            {#each s.profiles as p (p.id)}<option value={p.id}>{p.name}{p.model ? ` · ${p.model}` : ""}</option>{/each}
          </select>
          <p class="hint">Сильная модель: читает всю беседу и пишет черновик ответа.</p>
        </div>
        <div class="field">
          <label class="label sub" for="{uid}-parse">Разбор запросов поиска</label>
          <select id="{uid}-parse" value={s.parse_profile ?? ""} onchange={(e) => void update({ parse_profile: e.currentTarget.value || null })}>
            <option value="">— не назначено —</option>
            {#each s.profiles as p (p.id)}<option value={p.id}>{p.name}{p.model ? ` · ${p.model}` : ""}</option>{/each}
          </select>
          <p class="hint">Быстрая модель: доводит разбор запроса на обычном языке, когда разбор без модели не справился. Понадобится поиску на обычном языке — он ещё не выпущен.</p>
        </div>
      </section>
    {/if}

    <section class="block" aria-labelledby="{uid}-journal">
      <div class="head-row">
        <h4 class="label" id="{uid}-journal">Журнал обращений</h4>
        <button class="btn small" onclick={() => void loadJournal()}>Обновить</button>
      </div>
      {#if journalError}
        <p class="err">{journalError}</p>
      {:else if journal && !journal.recent.length}
        <p class="hint">Обращений к моделям ещё не было.</p>
      {:else if journal}
        {#if journal.month.length}
          <table class="table">
            <caption class="caption">За 30 дней</caption>
            <thead><tr><th>Подключение и модель</th><th class="n">Обращений</th><th class="n">Токенов: вход → выход</th></tr></thead>
            <tbody>
              {#each journal.month as u (u.profile_id + u.model)}
                <tr><td>{profileName(u.profile_id)} · <span class="mono">{u.model}</span> <span class="caption">({PLACE[u.place] ?? u.place})</span></td><td class="n">{nf.format(u.calls)}</td><td class="n">{nf.format(u.input_tokens)} → {nf.format(u.output_tokens)}</td></tr>
              {/each}
            </tbody>
          </table>
        {/if}
        <table class="table">
          <caption class="caption">Последние</caption>
          <thead><tr><th>Когда</th><th>Функция</th><th>Модель и адресат</th><th class="n">Токены</th><th class="n">Время</th><th>Итог</th></tr></thead>
          <tbody>
            {#each journal.recent as c, i (c.at_ms + ":" + i)}
              <tr>
                <td>{tf.format(new Date(c.at_ms))}</td>
                <td>{c.feature}</td>
                <td><span class="mono">{c.model}</span> <span class="caption">· {c.host}</span></td>
                <td class="n">{c.input_tokens != null || c.output_tokens != null ? `${nf.format(c.input_tokens ?? 0)} → ${nf.format(c.output_tokens ?? 0)}` : "—"}</td>
                <td class="n">{seconds(c.duration_ms)}</td>
                <td class:bad={!["ok", "other", "max_tokens"].includes(c.outcome)}>{OUTCOME[c.outcome] ?? c.outcome}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>
  {/if}
</div>

{#if deleting}
  {@const d = deleting}
  <ConfirmPopover anchor={d.anchor} title="Удалить подключение «{d.p.name}»?" text="Ключ тоже удалится из диспетчера учётных данных." confirmLabel="Удалить" onconfirm={() => void remove(d.p)} oncancel={() => (deleting = null)} />
{/if}

<style>
  .ai {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    width: 100%;
  }
  .head-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .section-title {
    margin: 0;
    font: var(--text-body-large);
    font-weight: 600;
  }
  .status {
    color: var(--fg-3);
  }
  .status.error {
    color: var(--danger);
  }
  .label,
  legend {
    margin: 0;
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  .label.sub {
    font: var(--text-body);
  }
  .block {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: 100%;
  }
  .hint {
    margin: 0;
    font: var(--text-caption);
    color: var(--fg-3);
    max-width: 64ch;
  }
  .err {
    margin: 0;
    color: var(--danger);
    user-select: text;
    overflow-wrap: anywhere;
  }
  .loading {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
  }
  .mono {
    font-family: var(--font-mono);
    font-size: 12px;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .ok-line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    color: var(--success);
    user-select: text;
  }
  .group {
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  legend {
    padding: 0;
    margin-bottom: var(--space-2);
  }
  .radios {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .radio,
  .check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .radio input,
  .check input {
    margin: 0;
    accent-color: var(--accent);
  }

  .profile {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .p-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .p-name {
    font: var(--text-body-strong);
  }
  .p-meta {
    color: var(--fg-3);
    overflow-wrap: anywhere;
  }
  .chip {
    padding: 0 8px;
    border-radius: 10px;
    font: var(--text-caption);
    line-height: 20px;
    background: var(--bg-hover);
    color: var(--fg-2);
  }
  .chip.cloud {
    background: color-mix(in srgb, var(--accent) 14%, transparent);
    color: var(--accent);
  }
  .chip.off {
    background: color-mix(in srgb, var(--danger) 14%, transparent);
    color: var(--danger);
  }
  .test {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding-top: var(--space-1);
    border-top: 1px solid var(--stroke-1);
  }
  .t-line {
    margin: 0;
    color: var(--fg-2);
  }
  .models {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .model {
    height: 24px;
    padding: 0 8px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer);
    font-family: var(--font-mono);
    font-size: 12px;
  }
  .model:hover {
    background: var(--bg-hover);
  }

  .box {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .box-title {
    margin: 0;
    font: var(--text-body-strong);
  }

  select,
  input[type="text"],
  input[type="password"],
  input[type="number"] {
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
  input.mono {
    font-family: var(--font-mono);
    font-size: 12px;
  }
  select {
    align-self: flex-start;
    min-width: 260px;
    padding-right: 6px;
  }
  select:focus-visible,
  input[type="text"]:focus,
  input[type="password"]:focus,
  input[type="number"]:focus {
    border-bottom: 2px solid var(--accent);
  }
  input[type="text"] {
    width: min(360px, 100%);
  }
  input.wide {
    width: min(480px, 100%);
  }
  .num {
    width: 140px;
  }
  .password {
    position: relative;
    display: flex;
    width: min(480px, 100%);
  }
  .password input {
    flex: 1;
    min-width: 0;
    padding-right: 40px;
    font-family: var(--font-mono);
    font-size: 12px;
  }
  .eye {
    position: absolute;
    right: 4px;
    top: 4px;
    width: 24px;
    height: 24px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-3);
  }
  .eye:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .eye[aria-pressed="true"] {
    color: var(--accent);
  }

  .add-label {
    color: var(--fg-3);
  }
  .table {
    width: 100%;
    border-collapse: collapse;
    font: var(--text-caption);
  }
  .table caption {
    text-align: left;
    padding-bottom: 4px;
    color: var(--fg-3);
  }
  .table th {
    text-align: left;
    font-weight: 600;
    color: var(--fg-2);
    padding: 4px 8px 4px 0;
    border-bottom: 1px solid var(--stroke-1);
  }
  .table td {
    padding: 4px 8px 4px 0;
    border-bottom: 1px solid var(--stroke-1);
    vertical-align: top;
    overflow-wrap: anywhere;
  }
  .table .n {
    text-align: right;
    white-space: nowrap;
  }
  .table td.bad {
    color: var(--danger);
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
  .btn.primary:disabled {
    opacity: 0.5;
    color: var(--fg-on-accent);
  }
  .btn.small {
    height: 24px;
    padding: 0 10px;
    font: var(--text-caption);
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
