<script lang="ts">
  import { onDestroy, onMount, tick, untrack } from "svelte";
  import { api, errorMessage, formatSize, SETTINGS_LIMITS, type DarkMail, type Settings, type SettingsPatch, type SignatureDto, type Theme } from "../api";
  import { adoptSettings, AUTO_READ_DEFAULT_MS, AUTO_READ_INSTANT_MS, saveSettings, SYNC_INTERVAL_LIVE_MIN } from "../settings.svelte";
  import { bodyDaysText, owaUrl } from "../setup";
  import { focusBack } from "../popover";
  import { hint, isTopLayer, layer } from "../hotkeys";
  import SignatureDialog from "./SignatureDialog.svelte";
  import AboutUpdates from "./AboutUpdates.svelte";
  import DataStores from "./DataStores.svelte";

  /**
   * Окно настроек (шестерёнка в верхней панели, Ctrl+,). Значения применяются сразу через
   * settings_set — кнопки «Сохранить» нет; при отказе бэкенда поле возвращается к прежнему значению.
   */
  interface Props {
    /** Идёт синхронизация: «Синхронизировать сейчас» недоступна. */
    syncing: boolean;
    onsync: () => void;
    /** В композере несохранённое письмо: перезапуск после «Забыть учётную запись» его потеряет. */
    unsavedDraft?: boolean;
    /** «Забыть учётную запись» подтверждено: окно стирает своё о ящике (недавние запросы поиска). */
    onforget?: () => void;
    onclose: () => void;
  }
  let { syncing, onsync, unsavedDraft = false, onforget, onclose }: Props = $props();

  type SectionId = "account" | "sync" | "reading" | "notify" | "signature" | "view" | "data" | "about";
  const SECTIONS: { id: SectionId; label: string }[] = [
    { id: "account", label: "Учётная запись" },
    { id: "sync", label: "Синхронизация" },
    { id: "reading", label: "Чтение" },
    { id: "notify", label: "Уведомления" },
    { id: "signature", label: "Подпись" },
    { id: "view", label: "Вид" },
    { id: "data", label: "Данные" },
    { id: "about", label: "О программе" },
  ];

  const uid = $props.id();
  const tabId = (id: SectionId) => `${uid}-tab-${id}`;
  const panelId = `${uid}-panel`;
  let current = $state<SectionId>("account");
  let dialogEl = $state<HTMLDivElement | null>(null);

  // --- загрузка и запись --------------------------------------------------------------------

  let s = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);

  async function load() {
    loadError = null;
    try {
      const fresh = await api.settingsGet();
      adoptSettings(fresh);
      s = fresh;
    } catch (e) {
      loadError = errorMessage(e);
    }
  }

  /** Строка в шапке: «Сохранено» на пару секунд или текст отказа (до следующего действия). */
  let status = $state<{ text: string; error: boolean } | null>(null);
  let statusTimer: ReturnType<typeof setTimeout> | undefined;

  function flash(text: string, error = false) {
    status = { text, error };
    clearTimeout(statusTimer);
    if (!error) statusTimer = setTimeout(() => (status = null), 2500);
  }

  /**
   * Изменение применяется сразу: окно и общая копия (prefs) — до ответа, бэкенд — следом.
   * Отказ возвращает прежние значения только тех полей, что менялись.
   */
  async function update(patch: SettingsPatch) {
    const cur = s;
    if (!cur) return;
    const keys = Object.keys(patch) as (keyof SettingsPatch)[];
    const before = Object.fromEntries(keys.map((k) => [k, cur[k]])) as SettingsPatch;
    s = { ...cur, ...patch };
    adoptSettings(patch);
    try {
      const fresh = await saveSettings(patch);
      // Бэкенд мог поправить значение (диапазон): берём его, но только по своим полям —
      // ответы на быстрые изменения подряд не должны затирать друг друга.
      if (fresh && s) s = { ...s, ...(Object.fromEntries(keys.map((k) => [k, fresh[k]])) as SettingsPatch) };
      flash("Сохранено");
    } catch (e) {
      if (s) s = { ...s, ...before };
      adoptSettings(before);
      flash(errorMessage(e), true);
    }
  }

  // --- варианты ------------------------------------------------------------------------------

  /** Список вариантов с текущим значением, если оно нестандартное (задано вручную или старой версией). */
  function withCurrent(base: number[], value: number | undefined, order: (a: number, b: number) => number): number[] {
    if (value == null || base.includes(value)) return base;
    return [...base, value].sort(order);
  }
  /** «Все письма» (0) — в конце списка. */
  const byDays = (a: number, b: number) => (a === 0 ? 1 : b === 0 ? -1 : a - b);

  const bodyOptions = $derived(withCurrent([30, 90, 180, 365, 730, 0], s?.body_days, byDays));
  const intervalOptions = $derived(withCurrent([1, 2, 5, 10, 15, 30, 60], s?.sync_interval_min, (a, b) => a - b));

  function secondsText(ms: number): string {
    return `${(ms / 1000).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} с`;
  }
  const autoReadOptions = $derived.by(() => {
    const base = [
      { ms: 0, label: "Не помечать" },
      { ms: AUTO_READ_INSTANT_MS, label: "Сразу" },
      { ms: AUTO_READ_DEFAULT_MS, label: "Через 1,5 с" },
      { ms: 5000, label: "Через 5 с" },
    ];
    const v = s?.auto_read_ms;
    if (v == null || base.some((o) => o.ms === v)) return base;
    return [...base, { ms: v, label: `Через ${secondsText(v)}` }].sort((a, b) => a.ms - b.ms);
  });

  const THEMES: { id: Theme; label: string }[] = [
    { id: "system", label: "Как в Windows" },
    { id: "light", label: "Светлая" },
    { id: "dark", label: "Тёмная" },
  ];

  const DARK_MAIL: { id: DarkMail; label: string }[] = [
    { id: "dark", label: "В тёмных тонах" },
    { id: "light", label: "На светлом листе" },
  ];

  function bytesText(n: number | null | undefined): string {
    if (n == null) return "—";
    if (n < 1024 * 1024) return formatSize(n);
    const mb = n / 1024 / 1024;
    if (mb < 1024) return `${mb.toLocaleString("ru-RU", { maximumFractionDigits: mb < 10 ? 1 : 0 })} МБ`;
    return `${(mb / 1024).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} ГБ`;
  }
  function mbText(mb: number): string {
    return mb < 1024 ? `${mb.toLocaleString("ru-RU")} МБ` : `${(mb / 1024).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} ГБ`;
  }

  // --- лимиты (поля с числом) ---------------------------------------------------------------

  type MbKey = "attachment_cache_mb" | "max_attachment_mb";
  let numError = $state<Record<MbKey, string | null>>({ attachment_cache_mb: null, max_attachment_mb: null });

  /** Число применяется по Enter или уходу из поля; неверное — подсказка, значение не меняется. */
  function commitMb(key: MbKey, el: HTMLInputElement) {
    const lim = SETTINGS_LIMITS[key];
    const raw = el.value.trim();
    const v = Number(raw);
    if (!raw || !Number.isInteger(v) || v < lim.min || v > lim.max) {
      numError[key] = `Целое число от ${lim.min.toLocaleString("ru-RU")} до ${lim.max.toLocaleString("ru-RU")}`;
      return;
    }
    numError[key] = null;
    if (!s || v === s[key]) return;
    void update(key === "attachment_cache_mb" ? { attachment_cache_mb: v } : { max_attachment_mb: v });
  }

  // --- учётная запись: смена пароля ----------------------------------------------------------

  let pwOpen = $state(false);
  let pw = $state("");
  let pwShow = $state(false);
  let pwBusy = $state(false);
  let pwError = $state<string | null>(null);
  let pwDone = $state(false);
  let pwBtn = $state<HTMLButtonElement | null>(null);
  let pwInput = $state<HTMLInputElement | null>(null);

  function openPw() {
    pwOpen = true;
    pwDone = false;
    pwError = null;
    void tick().then(() => pwInput?.focus());
  }
  function cancelPw() {
    pwOpen = false;
    pw = "";
    pwError = null;
    void tick().then(() => focusBack(pwBtn));
  }

  async function changePassword(e: SubmitEvent) {
    e.preventDefault();
    if (!pw || pwBusy) return;
    pwBusy = true;
    pwError = null;
    try {
      await api.settingsChangePassword(pw);
      pw = "";
      pwOpen = false;
      pwDone = true;
      if (s) s = { ...s, password_saved: true };
      // Прошлая синхронизация могла упасть на старом пароле: проверить новый сразу.
      if (!syncing) onsync();
      await tick();
      focusBack(pwBtn);
    } catch (err) {
      pwError = errorMessage(err);
      // Поле было выключено на время проверки и потеряло фокус: вернуть его, выделив набранное.
      pwBusy = false;
      await tick();
      pwInput?.focus();
      pwInput?.select();
    } finally {
      pwBusy = false;
    }
  }

  // --- учётная запись: забыть ----------------------------------------------------------------

  let forgetAsk = $state(false);
  let forgetBusy = $state(false);
  let forgetError = $state<string | null>(null);
  let forgetBtn = $state<HTMLButtonElement | null>(null);
  let forgetCancelBtn = $state<HTMLButtonElement | null>(null);

  function askForget() {
    forgetAsk = true;
    forgetError = null;
    // Действие необратимо (пароль удаляется): фокус — на «Отмена».
    void tick().then(() => forgetCancelBtn?.focus());
  }
  function cancelForget() {
    forgetAsk = false;
    void tick().then(() => focusBack(forgetBtn));
  }
  async function forget() {
    if (forgetBusy) return;
    forgetBusy = true;
    forgetError = null;
    // До вызова: после успеха бэкенд сразу перезапускает приложение, ответа окно может не дождаться.
    onforget?.();
    try {
      await api.settingsForgetAccount();
      // Дальше бэкенд перезапускает приложение в мастер подключения; окно остаётся занятым.
    } catch (e) {
      forgetError = errorMessage(e);
      forgetBusy = false;
    }
  }

  // --- подпись -------------------------------------------------------------------------------

  let sig = $state<SignatureDto | null>(null);
  let sigLoading = $state(false);
  let sigError = $state<string | null>(null);
  let sigOpen = $state(false);
  let sigBtn = $state<HTMLButtonElement | null>(null);

  async function loadSignature() {
    if (sigLoading) return;
    sigLoading = true;
    sigError = null;
    try {
      sig = await api.signatureGet();
    } catch (e) {
      sigError = errorMessage(e);
    } finally {
      sigLoading = false;
    }
  }

  $effect(() => {
    if (current === "signature" && !sig && !sigError) untrack(() => void loadSignature());
  });

  /**
   * Текст подписи без разметки для превью (HTML подписи в окно настроек не вставляется): переносы
   * строк и абзацы остаются строками, не больше шести строк.
   */
  const sigText = $derived.by(() => {
    const html = sig?.html.trim();
    if (!html) return "";
    const doc = new DOMParser().parseFromString(html, "text/html");
    for (const br of Array.from(doc.querySelectorAll("br"))) br.replaceWith("\n");
    for (const el of Array.from(doc.querySelectorAll("p, div, li, tr, h1, h2, h3, h4, h5, h6, blockquote"))) el.append("\n");
    const lines = (doc.body.textContent ?? "")
      .split("\n")
      .map((l) => l.replace(/[\s ]+/g, " ").trim())
      .filter(Boolean);
    const shown = lines.slice(0, 6).map((l) => (l.length > 120 ? `${l.slice(0, 120)}…` : l));
    if (lines.length > 6) shown.push("…");
    return shown.join("\n");
  });

  async function openSignature() {
    if (!sig) await loadSignature();
    if (sig) sigOpen = true;
  }
  function closeSignature() {
    sigOpen = false;
    void tick().then(() => focusBack(sigBtn));
  }

  // --- данные --------------------------------------------------------------------------------

  async function openDataDir() {
    try {
      await api.settingsOpenDataDir();
    } catch (e) {
      flash(errorMessage(e), true);
    }
  }

  // --- окно: вкладки, клавиши, фокус ---------------------------------------------------------

  function select(id: SectionId, focus = false) {
    current = id;
    if (focus) void tick().then(() => document.getElementById(tabId(id))?.focus());
  }

  function onTabKey(e: KeyboardEvent) {
    const n = SECTIONS.length;
    const i = SECTIONS.findIndex((x) => x.id === current);
    let j = -1;
    if (e.key === "ArrowDown") j = (i + 1) % n;
    else if (e.key === "ArrowUp") j = (i - 1 + n) % n;
    else if (e.key === "Home") j = 0;
    else if (e.key === "End") j = n - 1;
    if (j < 0) return;
    e.preventDefault();
    select(SECTIONS[j].id, true);
  }

  const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

  /**
   * Элемент в порядке Tab: видимый, без tabindex=-1 (неактивные вкладки); из группы переключателей —
   * только выбранный (остальные Tab пропускает), иначе граница круга оказалась бы не там.
   */
  function tabbable(el: HTMLElement, root: HTMLElement): boolean {
    if (el.offsetParent === null || el.tabIndex < 0) return false;
    if (el instanceof HTMLInputElement && el.type === "radio" && !el.checked && el.name) {
      return !root.querySelector(`input[type="radio"][name="${CSS.escape(el.name)}"]:checked`);
    }
    return true;
  }

  /** Окно модальное: Tab ходит по кругу внутри него. */
  function trapTab(e: KeyboardEvent) {
    const root = dialogEl;
    if (!root) return;
    const items = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => tabbable(el, root));
    if (!items.length) {
      e.preventDefault();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    const inside = active instanceof Node && root.contains(active);
    if (e.shiftKey && (!inside || active === first)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (!inside || active === last)) {
      e.preventDefault();
      first.focus();
    }
  }

  function close() {
    if (forgetBusy) return;
    pw = "";
    onclose();
  }

  function onKey(e: KeyboardEvent) {
    // Поверх настроек другое окно — диалог подписи или вопрос о несохранённом письме (крестик окна
    // при открытых настройках): Esc, Tab и Ctrl+, достаются ему, настройки под ним не закрываются
    // и фокус к себе не забирают.
    if (sigOpen || !isTopLayer(dialogEl)) return;
    if (e.key === "Escape") {
      e.preventDefault();
      if (forgetBusy || pwBusy) return;
      if (forgetAsk) cancelForget();
      else if (pwOpen) cancelPw();
      else close();
    } else if (e.key === "Tab") {
      trapTab(e);
    } else if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && (e.code === "Comma" || e.key === ",")) {
      e.preventDefault();
      close();
    }
  }

  onMount(() => {
    void load();
    void tick().then(() => document.getElementById(tabId(current))?.focus());
  });

  onDestroy(() => {
    clearTimeout(statusTimer);
    pw = "";
  });

  const locked = $derived(forgetBusy);
</script>

<svelte:window onkeydown={onKey} />

<div class="overlay">
  <div class="popup dialog" role="dialog" aria-modal="true" aria-labelledby="{uid}-title" aria-busy={locked} bind:this={dialogEl} {@attach layer}>
    <header class="head">
      <h2 class="title" id="{uid}-title">Настройки</h2>
      <span class="status caption" class:error={status?.error} role="status" aria-live="polite">{status?.text ?? ""}</span>
      <button class="close" onclick={close} aria-label="Закрыть настройки" title={hint("Закрыть", "settings-close")} disabled={locked}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      </button>
    </header>

    <div class="tabs" role="tablist" aria-orientation="vertical" aria-label="Разделы настроек">
      {#each SECTIONS as sec (sec.id)}
        <button role="tab" id={tabId(sec.id)} aria-selected={current === sec.id} aria-controls={panelId} tabindex={current === sec.id ? 0 : -1} class:on={current === sec.id} onclick={() => select(sec.id)} onkeydown={onTabKey} disabled={locked}>{sec.label}</button>
      {/each}
    </div>

    <div class="panel" role="tabpanel" id={panelId} aria-labelledby={tabId(current)}>
      <!-- «О программе» не зависит от настроек: видна, даже если они не загрузились. -->
      {#if current === "about"}
        <AboutUpdates />
      {:else if !s}
        {#if loadError}
          <p class="err" role="alert">Настройки не загрузились: {loadError}</p>
          <button class="btn" onclick={() => void load()}>Повторить</button>
        {:else}
          <p class="loading caption"><span class="spinner" aria-hidden="true"></span>Загружаю настройки…</p>
        {/if}
      {:else if current === "account"}
        <h3 class="section-title">Учётная запись</h3>
        <dl class="props">
          <dt>Сервер</dt>
          <dd class="mono">{owaUrl(s.host)}</dd>
          <dt>Логин</dt>
          <dd>{s.user}</dd>
          <dt>Пароль</dt>
          <dd>{s.password_saved ? "Сохранён в диспетчере учётных данных Windows" : "Не сохранён"}</dd>
        </dl>
        {#if pwDone}
          <p class="ok-line" role="status">
            <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
            Пароль проверен и сохранён
          </p>
        {/if}
        <div class="row">
          <button class="btn" bind:this={pwBtn} onclick={openPw} disabled={pwOpen || locked} aria-expanded={pwOpen}>Сменить пароль…</button>
          <button class="btn" bind:this={forgetBtn} onclick={askForget} disabled={forgetAsk || locked || !!s.dev_env} aria-expanded={forgetAsk} aria-describedby={s.dev_env ? `${uid}-devenv` : undefined}>Забыть учётную запись…</button>
        </div>
        {#if s.dev_env}
          <p class="hint" id="{uid}-devenv">Учётная запись задана переменными окружения AIOWA_HOST и AIOWA_USER (режим разработки): забыть её из окна нельзя.</p>
        {/if}

        {#if pwOpen}
          <form class="box" onsubmit={changePassword} novalidate aria-label="Смена пароля">
            <label class="label" for="{uid}-pw">Новый пароль</label>
            <div class="password">
              <input id="{uid}-pw" bind:this={pwInput} bind:value={pw} oninput={() => (pwError = null)} type={pwShow ? "text" : "password"} autocomplete="off" spellcheck="false" disabled={pwBusy} aria-invalid={!!pwError} aria-describedby="{uid}-pw-hint" />
              <button type="button" class="eye" onclick={() => (pwShow = !pwShow)} aria-pressed={pwShow} aria-label="Показать пароль" title={pwShow ? "Скрыть пароль" : "Показать пароль"} disabled={pwBusy}>
                <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
                  <path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" />
                  <circle cx="8" cy="8" r="2" fill="none" stroke="currentColor" stroke-width="1.3" />
                  {#if pwShow}<path d="m2.5 2.5 11 11" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />{/if}
                </svg>
              </button>
            </div>
            {#if pwError}
              <p class="err" role="alert">{pwError}</p>
            {/if}
            <p class="hint" id="{uid}-pw-hint">Сначала выполняется пробный вход (до минуты); если сервер не примет пароль, сохранённый не изменится.</p>
            <div class="row">
              <button type="submit" class="btn primary" disabled={!pw || pwBusy}>
                {#if pwBusy}<span class="spinner" aria-hidden="true"></span>{/if}
                {pwBusy ? "Проверяю вход…" : "Проверить и сохранить"}
              </button>
              <button type="button" class="btn" onclick={cancelPw} disabled={pwBusy}>Отмена</button>
            </div>
          </form>
        {/if}

        {#if forgetAsk}
          <div class="box warn-box" role="alertdialog" aria-labelledby="{uid}-forget-title" aria-describedby="{uid}-forget-text">
            <p class="box-title" id="{uid}-forget-title">Забыть учётную запись?</p>
            <div id="{uid}-forget-text">
              <p class="text">Пароль будет удалён из диспетчера учётных данных Windows, сессия на сервере закроется, приложение перезапустится и откроет мастер подключения. База писем останется на диске:</p>
              <p class="mono path">{s.db_path}</p>
              {#if unsavedDraft}
                <p class="text danger-text">В композере есть несохранённое письмо — оно будет потеряно.</p>
              {/if}
            </div>
            {#if forgetError}
              <p class="err" role="alert">{forgetError}</p>
            {/if}
            <div class="row">
              <button class="btn danger" onclick={() => void forget()} disabled={forgetBusy}>
                {#if forgetBusy}<span class="spinner" aria-hidden="true"></span>{/if}
                {forgetBusy ? "Перезапускаю…" : "Забыть и перезапустить"}
              </button>
              <button class="btn" bind:this={forgetCancelBtn} onclick={cancelForget} disabled={forgetBusy}>Отмена</button>
            </div>
          </div>
        {/if}
      {:else if current === "sync"}
        <h3 class="section-title">Синхронизация</h3>
        <div class="field">
          <label class="label" for="{uid}-days">Загружать тексты писем</label>
          <select id="{uid}-days" value={s.body_days} onchange={(e) => void update({ body_days: Number(e.currentTarget.value) })} aria-describedby="{uid}-days-hint">
            {#each bodyOptions as d (d)}
              <option value={d}>{d === 0 ? "Все письма" : `За ${bodyDaysText(d)}`}</option>
            {/each}
          </select>
          <p class="hint" id="{uid}-days-hint">Папки и списки писем синхронизируются целиком; тексты старше срока подтягиваются при открытии письма. Чем больше срок, тем больше база и дольше синхронизация. Новый срок действует со следующей синхронизации.</p>
        </div>
        <div class="field">
          <label class="label" for="{uid}-interval">Страховочная синхронизация</label>
          <select id="{uid}-interval" value={s.sync_interval_min} onchange={(e) => void update({ sync_interval_min: Number(e.currentTarget.value) })} aria-describedby="{uid}-interval-hint">
            {#each intervalOptions as m (m)}
              <option value={m}>Раз в {m} мин</option>
            {/each}
          </select>
          <p class="hint" id="{uid}-interval-hint">Пока открыт канал мгновенных обновлений, изменения приходят сразу, а по таймеру — не чаще раза в {SYNC_INTERVAL_LIVE_MIN} минут. Синхронизация по таймеру идёт, только когда окно на экране.</p>
        </div>
        <div class="row">
          <button class="btn" onclick={onsync} disabled={syncing}>
            {#if syncing}<span class="spinner" aria-hidden="true"></span>{/if}
            {syncing ? "Синхронизация идёт…" : "Синхронизировать сейчас"}
          </button>
        </div>
      {:else if current === "reading"}
        <h3 class="section-title">Чтение</h3>
        <fieldset class="group">
          <legend class="label">Помечать письмо прочитанным, когда оно открыто</legend>
          <div class="radios">
            {#each autoReadOptions as o (o.ms)}
              <label class="radio">
                <input type="radio" name="{uid}-autoread" value={o.ms} checked={s.auto_read_ms === o.ms} onchange={() => void update({ auto_read_ms: o.ms })} />
                {o.label}
              </label>
            {/each}
          </div>
          <p class="hint">Отсчёт идёт, пока развёрнутое письмо видно на экране. «Не помечать» — только вручную: кнопкой на карточке или из меню списка.</p>
        </fieldset>
      {:else if current === "notify"}
        <h3 class="section-title">Уведомления</h3>
        <label class="check">
          <input type="checkbox" checked={s.toasts} onchange={(e) => void update({ toasts: e.currentTarget.checked })} aria-describedby="{uid}-toasts-hint" />
          Уведомления Windows о новых письмах
        </label>
        <p class="hint" id="{uid}-toasts-hint">Работают, пока открыт канал мгновенных обновлений. Тот же переключатель — колокольчик в строке состояния.</p>
      {:else if current === "signature"}
        <h3 class="section-title">Подпись</h3>
        {#if sigLoading && !sig}
          <p class="loading caption"><span class="spinner" aria-hidden="true"></span>Загружаю подпись…</p>
        {:else if sigError}
          <p class="err" role="alert">Подпись не загрузилась: {sigError}</p>
        {:else if sig}
          {#if sigText}
            <blockquote class="sig-preview">{sigText}</blockquote>
            <p class="hint">{sig.auto_add ? "Добавляется автоматически в новые письма и ответы" : "Автоматически не добавляется: вставляется кнопкой в композере"}</p>
          {:else}
            <p class="hint">Подпись не задана</p>
          {/if}
        {/if}
        <div class="row">
          <button class="btn" bind:this={sigBtn} onclick={() => void openSignature()} disabled={sigLoading}>Изменить подпись…</button>
          {#if sigError}<button class="btn" onclick={() => void loadSignature()}>Повторить</button>{/if}
        </div>
        <p class="hint">Уже открытое письмо в композере сохраняет прежнюю подпись; новая действует для следующих писем.</p>
      {:else if current === "view"}
        <h3 class="section-title">Вид</h3>
        <fieldset class="group">
          <legend class="label">Тема</legend>
          <div class="themes">
            {#each THEMES as t (t.id)}
              <label class="theme" class:on={s.theme === t.id}>
                <span class="swatch {t.id}" aria-hidden="true"><span class="swatch-bar"></span><span class="swatch-line"></span><span class="swatch-line short"></span></span>
                <span class="theme-label">
                  <input type="radio" name="{uid}-theme" value={t.id} checked={s.theme === t.id} onchange={() => void update({ theme: t.id })} />
                  {t.label}
                </span>
              </label>
            {/each}
          </div>
        </fieldset>
        <fieldset class="group">
          <legend class="label">Письма в тёмной теме</legend>
          <div class="radios">
            {#each DARK_MAIL as o (o.id)}
              <label class="radio">
                <input type="radio" name="{uid}-darkmail" value={o.id} checked={(s.dark_mail ?? "dark") === o.id} onchange={() => void update({ dark_mail: o.id })} />
                {o.label}
              </label>
            {/each}
          </div>
          <p class="hint">В тёмных тонах светлые фоны письма темнеют, тёмный текст светлеет, оттенки и картинки остаются. Если перекраска что-то испортила, кнопка с солнцем на карточке письма покажет его как задумано — на светлом листе. В светлой теме письма всегда на светлом листе, печать — всегда в исходных цветах на белом.</p>
        </fieldset>
      {:else if current === "data"}
        <h3 class="section-title">Данные</h3>
        <dl class="props">
          <dt>База</dt>
          <dd class="mono">{s.db_path}</dd>
          <dt>Размер базы</dt>
          <dd>{bytesText(s.db_size_bytes)}</dd>
          <dt>Кэш вложений</dt>
          <dd>{bytesText(s.cache_size_bytes)} из {mbText(s.attachment_cache_mb)}</dd>
        </dl>
        <div class="row">
          <button class="btn" onclick={() => void openDataDir()}>Открыть папку</button>
        </div>
        <div class="field">
          <label class="label" for="{uid}-cache">Размер кэша вложений, МБ</label>
          <input id="{uid}-cache" class="num" type="number" min={SETTINGS_LIMITS.attachment_cache_mb.min} max={SETTINGS_LIMITS.attachment_cache_mb.max} step="100" value={s.attachment_cache_mb} onchange={(e) => commitMb("attachment_cache_mb", e.currentTarget)} aria-invalid={!!numError.attachment_cache_mb} aria-describedby="{uid}-cache-hint" />
          <p class="hint" id="{uid}-cache-hint">
            {#if numError.attachment_cache_mb}<span class="err">{numError.attachment_cache_mb}</span>{:else}Когда кэш больше, самые старые файлы удаляются с диска; в почте они остаются и скачиваются снова по клику.{/if}
          </p>
        </div>
        <div class="field">
          <label class="label" for="{uid}-maxatt">Не скачивать при синхронизации вложения больше, МБ</label>
          <input id="{uid}-maxatt" class="num" type="number" min={SETTINGS_LIMITS.max_attachment_mb.min} max={SETTINGS_LIMITS.max_attachment_mb.max} step="1" value={s.max_attachment_mb} onchange={(e) => commitMb("max_attachment_mb", e.currentTarget)} aria-invalid={!!numError.max_attachment_mb} aria-describedby="{uid}-maxatt-hint" />
          <p class="hint" id="{uid}-maxatt-hint">
            {#if numError.max_attachment_mb}<span class="err">{numError.max_attachment_mb}</span>{:else}Такие вложения скачиваются, только когда их открывают или сохраняют.{/if}
          </p>
        </div>
        <!-- Все базы на диске; опрос раз в несколько секунд обновляет и размеры текущей выше. -->
        <DataStores onusage={(db, cache) => s && (s = { ...s, db_size_bytes: db, cache_size_bytes: cache })} />
      {/if}
    </div>
  </div>
</div>

{#if sigOpen && sig}
  <SignatureDialog signature={sig} onsaved={(x) => (sig = x)} onclose={closeSignature} />
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 90;
    background: rgba(0, 0, 0, 0.3);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .dialog {
    position: static;
    width: min(760px, calc(100vw - 48px));
    height: min(580px, calc(100vh - 48px));
    display: grid;
    grid-template-columns: 196px 1fr;
    grid-template-rows: auto 1fr;
    overflow: hidden;
  }
  .head {
    grid-column: 1 / -1;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-3) var(--space-3) var(--space-5);
    border-bottom: 1px solid var(--stroke-1);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
  }
  .status {
    flex: 1;
    min-width: 0;
    text-align: right;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .status.error {
    color: var(--danger);
    white-space: normal;
    user-select: text;
  }
  .close {
    flex: none;
    width: 32px;
    height: 32px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-control);
    color: var(--fg-2);
  }
  .close:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .close:active:not(:disabled) {
    background: var(--bg-pressed);
  }

  .tabs {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--space-2);
    border-right: 1px solid var(--stroke-1);
    background: var(--bg-layer);
    overflow: auto;
  }
  .tabs button {
    position: relative;
    height: 36px;
    padding: 0 var(--space-3);
    border-radius: var(--radius-control);
    text-align: left;
    color: var(--fg-2);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .tabs button:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .tabs button.on {
    background: var(--bg-selected);
    color: var(--fg-1);
    font-weight: 600;
  }
  .tabs button.on::before {
    content: "";
    position: absolute;
    left: 0;
    top: 10px;
    bottom: 10px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }

  .panel {
    min-width: 0;
    overflow: auto;
    padding: var(--space-4) var(--space-5) var(--space-5);
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-4);
  }
  .panel > * {
    max-width: 100%;
  }
  .section-title {
    margin: 0;
    font: var(--text-body-large);
    font-weight: 600;
  }
  .label,
  legend {
    font: var(--text-body-strong);
    color: var(--fg-1);
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
    max-width: 60ch;
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

  .props {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-2) var(--space-4);
    margin: 0;
    width: 100%;
  }
  .props dt {
    color: var(--fg-3);
  }
  .props dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
    user-select: text;
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
  }

  .box {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
  }
  .warn-box {
    border-color: var(--danger);
  }
  .box-title {
    margin: 0;
    font: var(--text-body-strong);
  }
  .box .text {
    margin: 0 0 var(--space-1);
  }
  .path {
    margin: 0 0 var(--space-1);
    overflow-wrap: anywhere;
    user-select: text;
  }
  .danger-text {
    color: var(--danger);
  }
  .box .row {
    margin-top: var(--space-1);
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
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    font: inherit;
    outline: 0;
  }
  select {
    align-self: flex-start;
    min-width: 220px;
    padding-right: 6px;
  }
  select:focus-visible,
  input[type="text"]:focus,
  input[type="password"]:focus,
  input[type="number"]:focus {
    border-bottom: 2px solid var(--accent);
    outline: 0;
  }
  input[type="text"],
  input[type="password"],
  input[type="number"] {
    user-select: text;
  }
  input[aria-invalid="true"] {
    border-bottom-color: var(--danger);
  }
  .num {
    width: 140px;
  }
  .password {
    position: relative;
    display: flex;
    width: min(360px, 100%);
  }
  .password input {
    flex: 1;
    min-width: 0;
    padding-right: 40px;
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
  .eye:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .eye[aria-pressed="true"] {
    color: var(--accent);
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
  .check input,
  .theme input {
    margin: 0;
    accent-color: var(--accent);
  }

  .sig-preview {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--stroke-2);
    background: var(--bg-layer-alt);
    border-radius: 0 var(--radius-control) var(--radius-control) 0;
    color: var(--fg-2);
    max-width: 60ch;
    white-space: pre-line;
    overflow-wrap: anywhere;
    user-select: text;
  }

  .themes {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
  }
  .theme {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-2);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    background: var(--bg-layer-alt);
    transition: border-color var(--dur-fast) var(--ease-in);
  }
  .theme:hover {
    border-color: var(--stroke-2);
  }
  .theme.on {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .theme-label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  /* Мини-окно темы: цвета нарочно фиксированные — превью должно показывать именно эту тему, а не текущую. */
  .swatch {
    position: relative;
    display: block;
    width: 132px;
    height: 72px;
    border-radius: var(--radius-control);
    border: 1px solid var(--stroke-1);
    overflow: hidden;
    background: #f3f3f3;
  }
  .swatch.dark {
    background: #202020;
  }
  .swatch.system {
    background: linear-gradient(135deg, #f3f3f3 50%, #202020 50%);
  }
  .swatch-bar {
    position: absolute;
    left: 8px;
    right: 8px;
    top: 8px;
    height: 10px;
    border-radius: 2px;
    background: #005fb8;
  }
  .swatch.dark .swatch-bar {
    background: #60cdff;
  }
  .swatch-line {
    position: absolute;
    left: 8px;
    right: 24px;
    top: 28px;
    height: 6px;
    border-radius: 2px;
    background: rgba(127, 127, 127, 0.45);
  }
  .swatch-line.short {
    top: 42px;
    right: 52px;
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
