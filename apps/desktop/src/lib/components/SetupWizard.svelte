<script lang="ts">
  import { onDestroy, tick, untrack } from "svelte";
  import { api, errorMessage, type AppMode, type FoundStore, type LoginTest } from "../api";
  import { bytesText, syncedText } from "../data";
  import { BODY_DAYS_CHOICES, loginHint, owaUrl, parseHost } from "../setup";
  import Logo from "./Logo.svelte";

  /**
   * Мастер первого запуска: приложение поднялось без учётной записи (режим "setup"), почты ещё нет.
   * Три шага: сервер → логин и пароль с пробным входом → база и глубина синхронизации. «Готово»
   * доступно только после удачной проверки входа; бэкенд сохраняет пароль и config.json и
   * перезапускает приложение уже в режиме почты.
   *
   * Одна база на ящик: проверка входа узнаёт у сервера адрес ящика и ищет на диске его базы (при
   * любой форме логина). Найденная предлагается на шаге «Синхронизация» — «Продолжить с этой
   * базой» (по умолчанию) или «Начать новую».
   */
  interface Props {
    mode: AppMode | null;
  }
  let { mode }: Props = $props();

  type Step = 1 | 2 | 3;
  const STEPS: { n: Step; label: string }[] = [
    { n: 1, label: "Сервер" },
    { n: 2, label: "Учётная запись" },
    { n: 3, label: "Синхронизация" },
  ];

  const uid = $props.id();
  let step = $state<Step>(1);

  // --- шаг 1: сервер -------------------------------------------------------------------------

  let hostInput = $state(untrack(() => mode?.host ?? ""));
  /** Ошибку адреса показываем после попытки идти дальше, а не с первой буквы. */
  let hostTried = $state(false);
  const hostParse = $derived(parseHost(hostInput));
  let hostEl = $state<HTMLInputElement | null>(null);

  function submitHost(e: SubmitEvent) {
    e.preventDefault();
    hostTried = true;
    if (hostParse.host) step = 2;
    else hostEl?.focus();
  }

  // --- шаг 2: учётная запись и пробный вход --------------------------------------------------

  let userInput = $state(untrack(() => mode?.user ?? ""));
  let userTried = $state(false);
  const login = $derived(loginHint(userInput));
  /** Пароль живёт только здесь и только до сохранения: после «Готово» и при уходе из мастера стирается. */
  let password = $state("");
  let showPassword = $state(false);
  let userEl = $state<HTMLInputElement | null>(null);
  let passwordEl = $state<HTMLInputElement | null>(null);
  let nextEl = $state<HTMLButtonElement | null>(null);

  let testing = $state(false);
  let test = $state<LoginTest | null>(null);
  /** Номер пробного входа: ответ на устаревшие данные (поля уже поменяли) не засчитывается. */
  let testReq = 0;
  const verified = $derived(test?.ok === true);

  /** Любая правка сервера, логина или пароля снимает результат проверки. */
  function invalidateTest() {
    testReq++;
    test = null;
  }

  async function runTest(): Promise<boolean> {
    userTried = true;
    const host = hostParse.host;
    if (!host) {
      step = 1;
      return false;
    }
    if (login.error) {
      userEl?.focus();
      return false;
    }
    if (!password) {
      passwordEl?.focus();
      return false;
    }
    if (testing) return false;
    const req = ++testReq;
    testing = true;
    test = null;
    try {
      const r = await api.setupTestLogin(host, userInput.trim(), password);
      if (req !== testReq) return false;
      test = r;
      // По умолчанию — первая из найденных баз ящика. Бэкенд ставит впереди совпавшие по GUID или
      // адресу ящика (среди них — полнее, затем свежее); совпавшая только по логину без домена
      // ненадёжна и выбирается по умолчанию, лишь когда других нет.
      dbChoice = r.stores?.[0]?.path ?? NEW_DB;
      return r.ok;
    } catch (e) {
      if (req !== testReq) return false;
      test = { ok: false, server_version: null, folders: null, error: errorMessage(e) };
      return false;
    } finally {
      testing = false;
    }
  }

  /** Кнопка «Проверить вход»: фокус после проверки — на «Далее» при успехе, в пароль при отказе (поля были выключены). */
  async function testNow() {
    const ok = await runTest();
    await tick();
    if (test) (ok ? nextEl : passwordEl)?.focus();
  }

  /** Enter или «Далее»: вход уже проверен — дальше, иначе сначала проверить. */
  async function submitAccount(e: SubmitEvent) {
    e.preventDefault();
    if (testing) return;
    // Enter в поле логина при пустом пароле — просто перейти к паролю.
    if (!password && document.activeElement === userEl && !login.error) {
      passwordEl?.focus();
      return;
    }
    if (verified || (await runTest())) step = 3;
    else if (test) void tick().then(() => passwordEl?.focus());
  }

  const testText = $derived.by(() => {
    if (!test?.ok) return "";
    const parts = [test.host ? `Вход на ${test.host} выполнен` : "Вход выполнен"];
    if (test.mailbox?.email) parts.push(`ящик ${test.mailbox.email}`);
    if (test.server_version) parts.push(test.server_version);
    if (test.folders != null) parts.push(`почтовых папок: ${test.folders.toLocaleString("ru-RU")}`);
    return parts.join(" · ");
  });

  // --- шаг 3: синхронизация и сохранение -----------------------------------------------------

  let bodyDays = $state(90);
  let startSync = $state(true);
  let saving = $state(false);
  let saved = $state(false);
  let saveError = $state<string | null>(null);

  const BODY_NOTES: Record<number, string> = {
    30: "Тексты за месяц: первая загрузка быстрее, база меньше",
    90: "Рекомендуется: тексты за три месяца под рукой",
    365: "Тексты за год: первая синхронизация дольше, база заметно больше",
    0: "Все тексты за всё время: на большом ящике — гигабайты и долгая первая синхронизация",
  };

  /** Выбор базы: путь найденной базы ящика или новая. */
  const NEW_DB = "new";
  let dbChoice = $state<string>(NEW_DB);
  const found = $derived<FoundStore[]>(test?.ok ? (test.stores ?? []) : []);
  const chosenPath = $derived(dbChoice === NEW_DB ? (test?.new_db_path ?? null) : dbChoice);

  const KIND_TEXT: Record<FoundStore["kind"], string> = {
    guid: "тот же ящик",
    email: "совпал адрес ящика",
    login: "совпал логин",
  };

  async function finish(e: SubmitEvent) {
    e.preventDefault();
    const host = hostParse.host;
    if (!verified || !host || saving || saved) return;
    saving = true;
    saveError = null;
    try {
      await api.setupSave(host, userInput.trim(), password, bodyDays, startSync, dbChoice === NEW_DB ? null : dbChoice);
      // Бэкенд сохранил всё и перезапускает приложение: пароль окну больше не нужен.
      password = "";
      saved = true;
    } catch (err) {
      saveError = errorMessage(err);
    } finally {
      saving = false;
    }
  }

  onDestroy(() => {
    password = "";
  });

  // Фокус — в первое поле шага (и на старте мастера).
  $effect(() => {
    const s = step;
    void tick().then(() => {
      if (s === 1) hostEl?.focus();
      else if (s === 2) (userInput.trim() && !login.error ? passwordEl : userEl)?.focus();
      else (document.querySelector<HTMLInputElement>(`input[name="${uid}-db"]:checked`) ?? document.querySelector<HTMLInputElement>(`input[name="${uid}-days"]:checked`))?.focus();
    });
  });

  const busy = $derived(testing || saving || saved);
</script>

<main class="setup" aria-labelledby="{uid}-title">
  <div class="card" aria-busy={busy}>
    <header class="head">
      <Logo size={32} />
      <div>
        <h1 class="title" id="{uid}-title">Подключение к почте</h1>
        <p class="caption sub">AIOWA работает с Exchange через Outlook Web Access</p>
      </div>
    </header>

    {#if mode?.boot_error}
      <div class="boot" role="alert">
        <p class="err">{mode.boot_error}</p>
        <p class="caption">Путь к базе (db_path) задаётся в файле настроек, указанном внизу. После входа приложение попробует открыть базу заново.</p>
      </div>
    {/if}

    <ol class="steps" aria-label="Шаги">
      {#each STEPS as s (s.n)}
        <li class:on={step === s.n} class:done={step > s.n} aria-current={step === s.n ? "step" : undefined}>
          <span class="num" aria-hidden="true">
            {#if step > s.n}
              <svg width="12" height="12" viewBox="0 0 16 16"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>
            {:else}{s.n}{/if}
          </span>
          <span class="step-label">{s.label}</span>
        </li>
      {/each}
    </ol>

    {#if step === 1}
      <form class="body" onsubmit={submitHost} novalidate>
        <div class="field">
          <label for="{uid}-host">Адрес OWA</label>
          <input id="{uid}-host" bind:this={hostEl} bind:value={hostInput} oninput={invalidateTest} type="text" inputmode="url" placeholder="mail.example.ru" autocomplete="off" spellcheck="false" aria-invalid={hostTried && !!hostParse.error} aria-describedby="{uid}-host-hint" />
          <p class="hint" id="{uid}-host-hint">
            {#if hostTried && hostParse.error}
              <span class="err">{hostParse.error}</span>
            {:else if hostParse.host}
              Вход пойдёт на <span class="mono">{owaUrl(hostParse.host)}</span>{#if hostParse.note}. {hostParse.note}{/if}
            {:else}
              Можно вставить адрес из браузера: https://mail.example.ru/owa, имя сервера или сервер:порт
            {/if}
          </p>
        </div>
        <div class="buttons">
          <span class="spacer"></span>
          <button type="submit" class="btn primary">Далее</button>
        </div>
      </form>
    {:else if step === 2}
      <form class="body" onsubmit={submitAccount} novalidate>
        <div class="field">
          <label for="{uid}-user">Логин</label>
          <input id="{uid}-user" bind:this={userEl} bind:value={userInput} oninput={invalidateTest} type="text" placeholder="ivanov@example.ru или ДОМЕН\ivanov" autocomplete="off" spellcheck="false" disabled={busy} aria-invalid={userTried && !!login.error} aria-describedby="{uid}-user-hint" />
          <p class="hint" id="{uid}-user-hint">
            {#if userTried && login.error}<span class="err">{login.error}</span>{:else if login.note}{login.note}{:else}Тот же логин, что при входе в OWA в браузере{/if}
          </p>
        </div>
        <div class="field">
          <label for="{uid}-password">Пароль</label>
          <div class="password">
            <input id="{uid}-password" bind:this={passwordEl} bind:value={password} oninput={invalidateTest} type={showPassword ? "text" : "password"} autocomplete="off" spellcheck="false" disabled={busy} aria-invalid={userTried && !password} aria-describedby="{uid}-password-hint" />
            <button type="button" class="eye" onclick={() => (showPassword = !showPassword)} aria-pressed={showPassword} aria-label="Показать пароль" title={showPassword ? "Скрыть пароль" : "Показать пароль"} disabled={busy}>
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
                <path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8Z" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" />
                <circle cx="8" cy="8" r="2" fill="none" stroke="currentColor" stroke-width="1.3" />
                {#if showPassword}<path d="m2.5 2.5 11 11" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />{/if}
              </svg>
            </button>
          </div>
          <p class="hint" id="{uid}-password-hint">
            {#if userTried && !password}<span class="err">Введите пароль</span>{:else}Хранится в диспетчере учётных данных Windows, не в файлах приложения{/if}
          </p>
        </div>

        <div class="test" role="status" aria-live="polite">
          {#if testing}
            <span class="spinner" aria-hidden="true"></span>
            <span>Вхожу на {hostParse.host}… это может занять до минуты</span>
          {:else if test?.ok}
            <svg class="ok" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
            <span>{testText}</span>
          {:else if test}
            <span class="err" role="alert">{test.error ?? "Войти не удалось"}</span>
          {/if}
        </div>

        <div class="buttons">
          <button type="button" class="btn" onclick={() => (step = 1)} disabled={busy}>Назад</button>
          <span class="spacer"></span>
          <button type="button" class="btn" onclick={() => void testNow()} disabled={busy}>
            {#if testing}<span class="spinner" aria-hidden="true"></span>{/if}
            Проверить вход
          </button>
          <button type="submit" class="btn primary" bind:this={nextEl} disabled={testing}>Далее</button>
        </div>
      </form>
    {:else}
      <form class="body" onsubmit={finish} novalidate>
        {#if found.length > 0}
          <fieldset class="group" disabled={busy}>
            <legend>База писем</legend>
            <p class="hint">На этом компьютере уже есть база этого ящика: с ней почту не придётся скачивать заново.</p>
            <div class="dbs">
              {#each found as f, i (f.path)}
                <label class="choice db" class:on={dbChoice === f.path}>
                  <input type="radio" name="{uid}-db" value={f.path} bind:group={dbChoice} />
                  <span class="choice-label">{found.length > 1 ? `Продолжить с базой ${i + 1}` : "Продолжить с этой базой"}</span>
                  <span class="mono path">{f.path}</span>
                  <span class="caption">
                    {bytesText(f.total_bytes)} · писем {f.items.toLocaleString("ru-RU")} · обновлена {syncedText(f.last_sync_ms)} · {KIND_TEXT[f.kind]}
                  </span>
                </label>
              {/each}
              <label class="choice db" class:on={dbChoice === NEW_DB}>
                <input type="radio" name="{uid}-db" value={NEW_DB} bind:group={dbChoice} />
                <span class="choice-label">Начать новую базу</span>
                {#if test?.new_db_path}<span class="mono path">{test.new_db_path}</span>{/if}
                <span class="caption">Почта скачается с сервера заново. Найденная база останется на диске; удалить её можно в настройках, вкладка «Данные».</span>
              </label>
            </div>
          </fieldset>
        {/if}

        <fieldset class="group" disabled={busy}>
          <legend>Загружать тексты писем</legend>
          <div class="choices">
            {#each BODY_DAYS_CHOICES as c (c.days)}
              <label class="choice" class:on={bodyDays === c.days}>
                <input type="radio" name="{uid}-days" value={c.days} bind:group={bodyDays} />
                <span class="choice-label">{c.label}</span>
                <span class="caption">{BODY_NOTES[c.days]}</span>
              </label>
            {/each}
          </div>
          <p class="hint">
            Срок — только для текстов писем. Список писем всех папок (отправитель, тема, дата, флаги) загружается целиком при любом сроке: на большом ящике первая синхронизация займёт время. Тексты старше срока подтянутся при открытии письма. Срок можно поменять в настройках.
          </p>
        </fieldset>

        <label class="check">
          <input type="checkbox" bind:checked={startSync} disabled={busy} />
          Начать синхронизацию сразу
        </label>

        <dl class="summary caption">
          <dt>Сервер</dt>
          <dd class="mono">{hostParse.host ? owaUrl(hostParse.host) : "—"}</dd>
          <dt>Логин</dt>
          <dd>{userInput.trim()}</dd>
          {#if test?.mailbox?.email}
            <dt>Ящик</dt>
            <dd>{test.mailbox.email}</dd>
          {/if}
          {#if chosenPath}
            <dt>База</dt>
            <dd class="mono">{chosenPath}{dbChoice === NEW_DB ? " (новая)" : ""}</dd>
          {/if}
        </dl>

        {#if saveError}
          <p class="err" role="alert">{saveError}</p>
        {:else if saved}
          <p class="test" role="status"><span class="spinner" aria-hidden="true"></span>Сохранено, приложение перезапускается…</p>
        {:else if !verified}
          <p class="err" role="alert">Сначала проверьте вход на шаге «Учётная запись»</p>
        {/if}

        <div class="buttons">
          <button type="button" class="btn" onclick={() => (step = 2)} disabled={busy}>Назад</button>
          <span class="spacer"></span>
          <button type="submit" class="btn primary" disabled={!verified || busy}>
            {#if saving}<span class="spinner" aria-hidden="true"></span>{/if}
            Готово
          </button>
        </div>
      </form>
    {/if}

    {#if mode?.config_path}
      <p class="caption foot">Настройки: <span class="mono">{mode.config_path}</span></p>
    {/if}
  </div>
</main>

<style>
  .setup {
    height: 100%;
    overflow: auto;
    display: grid;
    place-items: center;
    padding: var(--space-6);
    background: var(--bg-base);
  }
  .card {
    width: min(520px, 100%);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
    padding: var(--space-6);
    background: var(--bg-layer);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-8);
  }
  .head {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .title {
    margin: 0;
    font: var(--text-subtitle);
  }
  .sub {
    margin: 0;
  }

  .steps {
    display: flex;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .steps li {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding-bottom: var(--space-2);
    border-bottom: 2px solid var(--stroke-1);
    color: var(--fg-3);
    font: var(--text-caption);
    min-width: 0;
  }
  .steps li.on {
    border-bottom-color: var(--accent);
    color: var(--fg-1);
    font-weight: 600;
  }
  .steps li.done {
    color: var(--fg-2);
  }
  .num {
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    border: 1px solid var(--stroke-2);
    font-size: 11px;
  }
  .steps li.on .num {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .steps li.done .num {
    color: var(--success);
    border-color: var(--success);
  }
  .step-label {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .field label,
  legend {
    font: var(--text-body-strong);
    color: var(--fg-1);
  }
  input[type="text"],
  input[type="password"] {
    height: var(--control-height);
    padding: 0 10px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    outline: 0;
    user-select: text;
  }
  input[type="text"]:focus,
  input[type="password"]:focus {
    border-bottom: 2px solid var(--accent);
  }
  input[aria-invalid="true"] {
    border-bottom-color: var(--danger);
  }
  input:disabled {
    color: var(--fg-3);
  }
  input::placeholder {
    color: var(--fg-4);
  }
  .password {
    position: relative;
    display: flex;
  }
  .password input {
    flex: 1;
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
  .hint {
    margin: 0;
    font: var(--text-caption);
    color: var(--fg-3);
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--danger);
    user-select: text;
    margin: 0;
  }
  .mono {
    font-family: var(--font-mono);
    user-select: text;
  }

  .test {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 20px;
    margin: 0;
    overflow-wrap: anywhere;
  }
  .ok {
    flex: none;
    color: var(--success);
  }

  .group {
    margin: 0;
    padding: 0;
    border: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
  }
  legend {
    padding: 0;
    margin-bottom: var(--space-2);
  }
  .choices {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
  }
  .choice {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: var(--space-2);
    row-gap: 2px;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--stroke-1);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    transition:
      border-color var(--dur-fast) var(--ease-in),
      background var(--dur-fast) var(--ease-in);
  }
  .choice:hover {
    background: var(--bg-hover);
  }
  .choice.on {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  .choice input {
    margin: 2px 0 0;
    accent-color: var(--accent);
  }
  .choice-label {
    font: var(--text-body-strong);
  }
  .choice .caption {
    grid-column: 2;
  }
  .dbs {
    display: grid;
    gap: var(--space-2);
  }
  .choice.db .path {
    grid-column: 2;
    font-size: 12px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .check {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .check input {
    margin: 0;
    accent-color: var(--accent);
  }
  .summary {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px var(--space-3);
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    border: 1px solid var(--stroke-1);
  }
  .summary dt {
    color: var(--fg-3);
  }
  .summary dd {
    margin: 0;
    color: var(--fg-1);
    overflow-wrap: anywhere;
    user-select: text;
  }

  .buttons {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .spacer {
    flex: 1;
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
    min-width: 96px;
    justify-content: center;
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn.primary:active:not(:disabled) {
    background: var(--accent-pressed);
  }
  .btn.primary:disabled {
    opacity: 0.5;
    color: var(--fg-on-accent);
  }
  .foot {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .boot {
    display: grid;
    gap: var(--space-1);
    overflow-wrap: anywhere;
  }
  .boot p {
    margin: 0;
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
