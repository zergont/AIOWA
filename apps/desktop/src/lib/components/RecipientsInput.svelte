<script lang="ts">
  import { onDestroy, tick } from "svelte";
  import { api, errorMessage, isDirectoryList, plural, type ComposeRecipient, type DirectoryPerson, type RecipientSuggestion } from "../api";
  import { ICON } from "../actions";
  import { isValidEmail, mergeRecipients, parseAddresses, recipientLabel } from "../addresses";
  import { CONTACT_ICON, contactCardOpener, copyText, directoryLine, emailKey, expandSupported, isKnownList, noteExpandError, personMatches, rememberKinds } from "../contact";
  import { focusBack } from "../popover";
  import ContextMenu, { type MenuItem } from "./ContextMenu.svelte";

  interface Props {
    /** Идентификатор поля ввода: на него указывает label снаружи. */
    id: string;
    /** Выбранные получатели; двусторонняя привязка. */
    recipients: ComposeRecipient[];
    disabled?: boolean;
    placeholder?: string;
  }
  let { id, recipients = $bindable([]), disabled = false, placeholder = "" }: Props = $props();

  /** Карточка контакта (её держит App); вне App чипы её не открывают. */
  const openContact = contactCardOpener();

  /** Локальные подсказки — почти сразу; справочник — когда набор притих. */
  const LOCAL_DELAY_MS = 120;
  const DIR_DELAY_MS = 300;
  /** Справочник спрашивается с этой длины набранного (короче бэкенд всё равно отдаёт пусто). */
  const DIR_MIN = 2;
  const DIR_LIMIT = 10;
  const LOCAL_LIMIT = 8;
  /** Список рассылки больше стольких участников разворачивается только после подтверждения. */
  const EXPAND_CONFIRM_OVER = 20;
  /** Как развернуть список из подсказок: и подпись кнопки для мыши, и описание варианта для диктора. */
  const EXPAND_HINT = "Shift+Enter — добавить участников списка вместо него";

  let box = $state<HTMLDivElement | null>(null);
  let input = $state<HTMLInputElement | null>(null);
  let text = $state("");
  /** Список подсказок показан (набор, стрелка вниз); закрывают его выбор, Esc и уход фокуса. */
  let listOpen = $state(false);
  let suggestions = $state<RecipientSuggestion[]>([]);
  /**
   * Подсвеченный вариант — по ключу, а не по номеру: ответ справочника перестраивает список, и
   * подсветка не должна молча переехать на другого человека. null или пропавший ключ — первый.
   */
  let activeKey = $state<string | null>(null);

  // --- справочник организации -----------------------------------------------------------------
  // Локальные подсказки (адреса из переписки) приходят мгновенно; справочник — следом, ниже, под
  // своим заголовком и без адресов, которые уже есть выше или среди получателей. Ответ устаревшего
  // запроса не показывается (номер запроса), а пока сервер ищет по новому тексту, из прежней
  // выдачи остаётся то, что к нему подходит.

  /** Последний ответ справочника и текст, по которому он получен (для «не найдено» и Enter по имени). */
  let dirResult = $state<DirectoryPerson[]>([]);
  let dirDoneFor = $state<string | null>(null);
  let dirError = $state<string | null>(null);
  /** Текст, по которому сейчас идёт запрос к справочнику; null — запроса нет. */
  let dirPendingFor = $state<string | null>(null);
  /** Кэш справочника по тексту dirPendingFor: показывается, пока сервер ищет. */
  let dirCached = $state<DirectoryPerson[]>([]);

  const query = $derived(text.trim());
  /**
   * Ответ по этому самому тексту — целиком, по прежнему тексту — только то, что подходит к новому;
   * пока сервер ищет, к нему добавляется кэш справочника по набранному.
   */
  const dirPeople = $derived.by(() => {
    const done = dirDoneFor === null ? [] : query === dirDoneFor ? dirResult : dirResult.filter((p) => personMatches(p, query));
    if (dirPendingFor === null || !dirCached.length) return done;
    const seen = new Set(done.map((p) => emailKey(p.email)));
    const extra = (query === dirPendingFor ? dirCached : dirCached.filter((p) => personMatches(p, query))).filter((p) => !seen.has(emailKey(p.email)));
    return [...done, ...extra];
  });
  const dirShownError = $derived(dirDoneFor === query ? dirError : null);
  const chosen = $derived(new Set(recipients.map((r) => emailKey(r.email))));
  /** Записи справочника по адресу: добавляют локальной подсказке должность, отдел и имя. */
  const dirByEmail = $derived(new Map(dirPeople.filter((p) => emailKey(p.email)).map((p) => [emailKey(p.email), p] as const)));
  const localShown = $derived(suggestions.filter((s) => !chosen.has(emailKey(s.email))));
  const dirShown = $derived.by(() => {
    const seen = new Set([...chosen, ...localShown.map((s) => emailKey(s.email))]);
    const out: DirectoryPerson[] = [];
    for (const p of dirPeople) {
      const k = emailKey(p.email);
      if (!k || seen.has(k)) continue;
      seen.add(k);
      out.push(p);
    }
    return out;
  });

  /** Вариант в списке: локальная подсказка или запись справочника. */
  interface Option {
    key: string;
    name: string | null;
    email: string;
    /** Должность · отдел из справочника (если известны). */
    line: string;
    /** Список рассылки: его можно развернуть в участников. */
    list: boolean;
  }
  const options = $derived<Option[]>([
    ...localShown.map((s) => {
      const d = dirByEmail.get(emailKey(s.email));
      return { key: `l:${emailKey(s.email)}`, name: s.name?.trim() || d?.name?.trim() || null, email: s.email.trim(), line: directoryLine(d), list: isDirectoryList(d) || isKnownList(s.email) };
    }),
    ...dirShown.map((p) => ({ key: `d:${emailKey(p.email)}`, name: p.name?.trim() || null, email: (p.email ?? "").trim(), line: directoryLine(p), list: isDirectoryList(p) })),
  ]);
  const localCount = $derived(localShown.length);
  const dirBusy = $derived(dirPendingFor !== null);
  /** Раздел справочника в списке: идёт поиск, есть находки или ошибка, либо не нашлось вообще ничего. */
  const dirNothing = $derived(!dirBusy && dirDoneFor === query && dirResult.length === 0 && dirShownError === null);
  const dirGroup = $derived(query.length >= DIR_MIN && (dirBusy || dirShown.length > 0 || dirShownError !== null || (dirNothing && localCount === 0)));
  const open = $derived(listOpen && !disabled && (options.length > 0 || dirGroup));
  /** Номер подсвеченного варианта в списке, который мог измениться под курсором. */
  const cur = $derived.by(() => {
    if (!options.length) return -1;
    const i = activeKey === null ? -1 : options.findIndex((o) => o.key === activeKey);
    return i >= 0 ? i : 0;
  });

  /** Номер последнего запроса подсказок: ответ устаревшего не показываем. */
  let localRequest = 0;
  let dirRequest = 0;
  let localTimer: ReturnType<typeof setTimeout> | undefined;
  let dirTimer: ReturnType<typeof setTimeout> | undefined;

  function close() {
    localRequest++;
    dirRequest++;
    clearTimeout(localTimer);
    clearTimeout(dirTimer);
    listOpen = false;
    suggestions = [];
    dirResult = [];
    dirCached = [];
    dirPendingFor = null;
    dirDoneFor = null;
    dirError = null;
    activeKey = null;
  }

  async function suggest() {
    const q = text.trim();
    if (!q) {
      close();
      return;
    }
    const n = ++localRequest;
    try {
      const got = await api.recipientsSuggest(q, LOCAL_LIMIT);
      if (n !== localRequest) return;
      suggestions = got.filter((s) => !chosen.has(emailKey(s.email)));
      activeKey = null;
    } catch {
      // Подсказки — удобство, не действие: отказ бэкенда молча оставляет поле без локального списка.
      if (n === localRequest) suggestions = [];
    }
  }

  function scheduleDirectory() {
    clearTimeout(dirTimer);
    const q = text.trim();
    if (q.length < DIR_MIN) {
      // Ответ на прежний запрос уже не нужен; раздел справочника на коротком тексте не показывается.
      dirRequest++;
      dirPendingFor = null;
      dirCached = [];
      return;
    }
    if (q === dirDoneFor || q === dirPendingFor) return;
    dirTimer = setTimeout(() => void searchDirectory(q), DIR_DELAY_MS);
  }

  async function searchDirectory(q: string) {
    clearTimeout(dirTimer);
    const n = ++dirRequest;
    dirPendingFor = q;
    dirCached = [];
    // Кэш справочника — сразу (без сети), ответ сервера — следом; кэш нужен, только пока сервер ищет.
    void api
      .directorySearch(q, DIR_LIMIT, true)
      .then((res) => {
        if (n !== dirRequest || dirPendingFor !== q) return;
        const people = res?.people ?? [];
        rememberKinds(people);
        dirCached = people;
      })
      .catch(() => {
        // Быстрый ответ — удобство: его отказ покажет запрос к серверу.
      });
    try {
      const res = await api.directorySearch(q, DIR_LIMIT);
      if (n !== dirRequest) return;
      const people = res?.people ?? [];
      rememberKinds(people);
      dirResult = people;
      dirError = res?.error ?? null;
    } catch (e) {
      if (n !== dirRequest) return;
      dirResult = dirCached;
      dirError = errorMessage(e);
    } finally {
      if (n === dirRequest) {
        dirPendingFor = null;
        dirCached = [];
        dirDoneFor = q;
      }
    }
  }

  /** Снова показать список по набранному (стрелка вниз после Esc). */
  function reopen() {
    if (!query) return;
    listOpen = true;
    void suggest();
    if (query.length >= DIR_MIN && dirDoneFor !== query && dirPendingFor !== query) void searchDirectory(query);
  }

  function onInput() {
    // Разделитель в набранном: всё до него становится чипами.
    if (/[,;]/.test(text)) {
      commitText();
      return;
    }
    note = null;
    listOpen = text.trim().length > 0;
    clearTimeout(localTimer);
    localTimer = setTimeout(() => void suggest(), LOCAL_DELAY_MS);
    scheduleDirectory();
  }

  function add(list: ComposeRecipient[]) {
    recipients = mergeRecipients(recipients, list);
  }

  /** Набранный текст → чипы (несколько адресов за раз). */
  function commitText() {
    const parsed = parseAddresses(text);
    if (parsed.length) add(parsed);
    text = "";
    close();
  }

  /** Превратить недонабранный адрес в чип: композер зовёт перед отправкой (Ctrl+Enter не ждёт Enter в поле). */
  export function commit() {
    if (text.trim()) commitText();
  }

  function pick(o: Option) {
    add([{ name: o.name, email: o.email }]);
    text = "";
    close();
    input?.focus();
  }

  /** «Развернуть» у списка в подсказках: в поле встают его участники, а не сам список. */
  function expandOption(o: Option) {
    // Другой список ещё разворачивается: набранное и подсказки остаются на месте.
    if (expanding) {
      showNote(`Дождитесь, пока развернётся «${expanding.label}»`, "info");
      return;
    }
    text = "";
    close();
    input?.focus();
    void expandList({ name: o.name, email: o.email }, false);
  }

  /** Убранный чип списка: вопрос о том, чем его заменить, больше не нужен. */
  function forgetPending(r: ComposeRecipient | undefined) {
    if (r && pendingExpand?.inPlace && emailKey(pendingExpand.list.email) === emailKey(r.email)) pendingExpand = null;
  }

  function remove(i: number) {
    forgetPending(recipients[i]);
    recipients = recipients.filter((_, k) => k !== i);
    input?.focus();
  }

  /** Убрать чип с клавиатуры: фокус переходит на соседний чип (или в поле, если чипов правее нет). */
  function removeChip(i: number) {
    forgetPending(recipients[i]);
    recipients = recipients.filter((_, k) => k !== i);
    void tick().then(() => focusChip(i));
  }

  function onKey(e: KeyboardEvent) {
    // Ctrl+Enter и прочие сочетания — дело композера.
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        if (open && options.length) activeKey = options[(cur + 1) % options.length].key;
        else if (!open) reopen();
        break;
      case "ArrowUp":
        e.preventDefault();
        if (open && options.length) activeKey = options[(cur - 1 + options.length) % options.length].key;
        break;
      case "ArrowLeft":
        // Пустое поле: стрелка влево уходит на последний чип (с него — карточка, меню, удаление).
        if (!text && recipients.length) {
          e.preventDefault();
          focusChip(recipients.length - 1);
        }
        break;
      case "Enter":
        e.preventDefault();
        if (open && cur >= 0) {
          const o = options[cur];
          if (e.shiftKey && o.list && expandSupported()) expandOption(o);
          else pick(o);
        } else if (query) {
          // Имя без адреса: сначала спросить справочник, а не делать из имени неверный чип.
          if (!query.includes("@") && query.length >= DIR_MIN && dirDoneFor !== query) {
            listOpen = true;
            if (dirPendingFor !== query) void searchDirectory(query);
          } else {
            commitText();
          }
        }
        break;
      case "Tab":
        if (open && cur >= 0) {
          e.preventDefault();
          pick(options[cur]);
        } else if (text.trim()) {
          // Чип создаётся, фокус идёт дальше по Tab.
          commitText();
        }
        break;
      case "Escape":
        if (open) {
          e.preventDefault();
          e.stopPropagation();
          close();
        }
        break;
      case "Backspace":
        if (!text && recipients.length) {
          e.preventDefault();
          remove(recipients.length - 1);
        }
        break;
      case ",":
      case ";":
        e.preventDefault();
        commitText();
        break;
    }
  }

  function onPaste(e: ClipboardEvent) {
    const t = e.clipboardData?.getData("text/plain") ?? "";
    if (!/[,;\n<>@]/.test(t)) return;
    e.preventDefault();
    const parsed = parseAddresses(text + t);
    if (parsed.length) add(parsed);
    text = "";
    close();
  }

  function onBlur() {
    if (text.trim()) commitText();
    else close();
  }

  function optId(i: number): string {
    return `${id}-opt-${i}`;
  }

  // Подсвеченный вариант с клавиатуры не должен уезжать за край прокручиваемого списка.
  $effect(() => {
    if (!open || cur < 0) return;
    document.getElementById(optId(cur))?.scrollIntoView({ block: "nearest" });
  });

  /** Нажатие по пустому месту рамки ставит фокус в поле; по самому полю и чипам — обычное поведение. */
  function onBoxPointerDown(e: PointerEvent) {
    if (disabled || e.target !== e.currentTarget) return;
    e.preventDefault();
    input?.focus();
  }

  // --- чипы: карточка контакта, меню, клавиатура ---------------------------------------------
  // Одиночный щелчок по чипу только выделяет его (крестик и Backspace по-прежнему удаляют), двойной
  // открывает карточку контакта, правый — меню. С клавиатуры: из пустого поля стрелкой влево на чип,
  // дальше Enter — карточка, Delete — убрать, Shift+F10 или клавиша меню — меню, Ctrl+C — адрес.

  function chipButtons(): HTMLButtonElement[] {
    return box ? Array.from(box.querySelectorAll<HTMLButtonElement>("button.chip-text")) : [];
  }

  function focusChip(i: number) {
    const bs = chipButtons();
    if (i >= 0 && i < bs.length) bs[i].focus();
    else input?.focus();
  }

  function showCard(r: ComposeRecipient, anchor: HTMLElement) {
    if (!openContact || !isValidEmail(r.email)) return;
    openContact({ name: r.name?.trim() || null, email: r.email.trim() }, anchor);
  }

  function onChipKey(e: KeyboardEvent, i: number, r: ComposeRecipient) {
    const ctrl = e.ctrlKey || e.metaKey;
    if (ctrl && !e.altKey && !e.shiftKey && (e.code === "KeyC" || e.key.toLowerCase() === "c")) {
      e.preventDefault();
      void copyAddress(r);
      return;
    }
    if (ctrl || e.altKey) return;
    switch (e.key) {
      case "ArrowLeft":
        e.preventDefault();
        if (i > 0) focusChip(i - 1);
        break;
      case "ArrowRight":
        e.preventDefault();
        focusChip(i + 1);
        break;
      case "Home":
        e.preventDefault();
        focusChip(0);
        break;
      case "End":
        e.preventDefault();
        input?.focus();
        break;
      case "Delete":
      case "Backspace":
        e.preventDefault();
        removeChip(i);
        break;
      case "Enter":
      case " ":
        e.preventDefault();
        showCard(r, e.currentTarget as HTMLElement);
        break;
    }
  }

  let menu = $state<{ index: number; anchor: HTMLElement } | null>(null);

  function openMenu(e: MouseEvent, i: number) {
    e.preventDefault();
    if (disabled) return;
    // Якорь — сам чип: меню встаёт под ним и с мыши, и с клавиши меню (у неё нет координат курсора).
    menu = { index: i, anchor: e.currentTarget as HTMLElement };
  }

  const menuItems = $derived.by((): MenuItem[] => {
    const r = menu ? recipients[menu.index] : undefined;
    if (!r) return [];
    const items: MenuItem[] = [];
    if (openContact) items.push({ id: "card", label: "Карточка контакта", icon: CONTACT_ICON.person, disabled: !isValidEmail(r.email) });
    if (isKnownList(r.email) && expandSupported()) items.push({ id: "expand", label: "Развернуть список", icon: CONTACT_ICON.people, disabled: expanding !== null });
    items.push({ id: "copy", label: "Копировать адрес", icon: CONTACT_ICON.copy });
    items.push({ id: "edit", label: "Изменить", icon: ICON.rename });
    items.push({ id: "remove", label: "Убрать", icon: ICON.close, separator: true });
    return items;
  });

  function onMenuPick(action: string) {
    const m = menu;
    menu = null;
    const r = m ? recipients[m.index] : undefined;
    if (!m || !r) {
      input?.focus();
      return;
    }
    switch (action) {
      case "card":
        focusBack(m.anchor);
        showCard(r, m.anchor);
        break;
      case "expand":
        focusBack(m.anchor);
        void expandList(r, true);
        break;
      case "copy":
        focusBack(m.anchor);
        void copyAddress(r);
        break;
      case "edit":
        // Чип обратно в текст: поправить адрес и снова Enter.
        forgetPending(r);
        recipients = recipients.filter((_, k) => k !== m.index);
        text = r.name?.trim() ? `${r.name.trim()} <${r.email.trim()}>` : r.email.trim();
        void tick().then(() => {
          input?.focus();
          input?.setSelectionRange(text.length, text.length);
        });
        break;
      case "remove":
        removeChip(m.index);
        break;
    }
  }

  function onMenuClose() {
    const m = menu;
    menu = null;
    focusBack(m?.anchor ?? input);
  }

  async function copyAddress(r: ComposeRecipient) {
    const ok = await copyText(r.email.trim());
    showNote(ok ? "Адрес скопирован" : "Не удалось скопировать адрес", ok ? "info" : "error");
  }

  // --- сообщение под полем и разворачивание списков рассылки ---------------------------------

  let note = $state<{ text: string; kind: "info" | "error" } | null>(null);
  let noteTimer: ReturnType<typeof setTimeout> | undefined;

  function showNote(message: string, kind: "info" | "error") {
    note = { text: message, kind };
    clearTimeout(noteTimer);
    // Справка уходит сама, ошибка — с новым вводом или следующим сообщением.
    if (kind === "info") noteTimer = setTimeout(() => (note = null), 4000);
  }

  /** Список, который сейчас разворачивается: на его чипе и под полем крутится индикатор. */
  let expanding = $state<{ key: string; label: string } | null>(null);
  /** Большой список ждёт подтверждения: участники уже получены. */
  let pendingExpand = $state<{ list: ComposeRecipient; members: ComposeRecipient[]; inPlace: boolean } | null>(null);
  let confirmBtn = $state<HTMLButtonElement | null>(null);

  function addressesText(n: number): string {
    return `${n} ${plural(n, "адрес", "адреса", "адресов")}`;
  }

  /**
   * Развернуть список рассылки: участники с сервера (ExpandDL). `inPlace` — список уже стоит чипом,
   * участники встают на его место; иначе (из подсказок) добавляются в конец. Больше
   * EXPAND_CONFIRM_OVER участников — сначала вопрос.
   */
  async function expandList(list: ComposeRecipient, inPlace: boolean) {
    if (expanding) return;
    const k = emailKey(list.email);
    const label = recipientLabel(list);
    expanding = { key: k, label };
    pendingExpand = null;
    note = null;
    try {
      const people = await api.directoryExpand(list.email.trim());
      rememberKinds(people);
      const members = mergeRecipients(
        [],
        people.flatMap((p) => {
          const email = p.email?.trim();
          return email && emailKey(email) !== k ? [{ name: p.name?.trim() || null, email }] : [];
        }),
      );
      if (!members.length) {
        showNote(`В списке «${label}» нет адресов, которые можно добавить`, "error");
        return;
      }
      if (members.length > EXPAND_CONFIRM_OVER) {
        pendingExpand = { list, members, inPlace };
        await tick();
        confirmBtn?.focus();
        return;
      }
      applyExpand(list, members, inPlace);
    } catch (e) {
      noteExpandError(errorMessage(e));
      showNote(`Не удалось развернуть «${label}»: ${errorMessage(e)}`, "error");
    } finally {
      expanding = null;
    }
  }

  function applyExpand(list: ComposeRecipient, members: ComposeRecipient[], inPlace: boolean) {
    const k = emailKey(list.email);
    const label = recipientLabel(list);
    const i = inPlace ? recipients.findIndex((r) => emailKey(r.email) === k) : -1;
    // Чип списка убрали, пока шёл запрос: его участники тоже не нужны.
    if (inPlace && i < 0) {
      showNote(`Список «${label}» уже убран из получателей`, "info");
      return;
    }
    const rest = i >= 0 ? recipients.filter((_, j) => j !== i) : recipients;
    const seen = new Set(rest.map((r) => emailKey(r.email)));
    const fresh = members.filter((m) => !seen.has(emailKey(m.email)));
    recipients = i >= 0 ? [...recipients.slice(0, i), ...fresh, ...recipients.slice(i + 1)] : [...recipients, ...fresh];
    if (i >= 0) {
      // Чип списка (а с ним, возможно, и фокус) исчез: фокус — на первого участника или в поле.
      void tick().then(() => {
        const a = document.activeElement;
        if (a && a !== document.body) return;
        if (fresh.length) focusChip(i);
        else input?.focus();
      });
    }
    const skipped = members.length - fresh.length;
    const tail = skipped ? `; ${addressesText(skipped)} уже среди получателей` : "";
    showNote(i >= 0 ? `Список «${label}» развёрнут: ${addressesText(fresh.length)}${tail}` : `Из списка «${label}» добавлено ${addressesText(fresh.length)}${tail}`, "info");
  }

  function confirmExpand() {
    const p = pendingExpand;
    pendingExpand = null;
    if (p) applyExpand(p.list, p.members, p.inPlace);
    input?.focus();
  }

  function cancelExpand() {
    pendingExpand = null;
    input?.focus();
  }

  function onConfirmKey(e: KeyboardEvent) {
    // Свой Esc: иначе он дошёл бы до композера и спросил бы об отмене всего письма.
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      cancelExpand();
    }
  }

  function chipTitle(r: ComposeRecipient, ok: boolean, list: boolean): string | undefined {
    if (!ok) return "Неверный адрес";
    const lines = [r.name ? r.email : null, list ? "Список рассылки: правый щелчок — развернуть" : null, openContact ? "Двойной щелчок — карточка контакта" : null].filter(Boolean);
    return lines.length ? lines.join("\n") : undefined;
  }

  onDestroy(() => {
    clearTimeout(localTimer);
    clearTimeout(dirTimer);
    clearTimeout(noteTimer);
  });
</script>

<div class="wrap">
  <div class="box" class:disabled role="presentation" bind:this={box} onpointerdown={onBoxPointerDown}>
    {#each recipients as r, i (emailKey(r.email))}
      {@const ok = isValidEmail(r.email)}
      {@const list = ok && isKnownList(r.email)}
      {@const busy = expanding?.key === emailKey(r.email)}
      <span class="chip" class:invalid={!ok} class:list title={chipTitle(r, ok, list)}>
        {#if busy}
          <span class="spinner" aria-hidden="true"></span>
        {:else if list}
          <svg class="chip-icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d={CONTACT_ICON.people} fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" /></svg>
        {/if}
        <button
          type="button"
          class="chip-text"
          tabindex="-1"
          {disabled}
          aria-label="{recipientLabel(r)}{r.name ? `, ${r.email}` : ''}{list ? ', список рассылки' : ''}{ok ? '' : ', неверный адрес'}"
          aria-haspopup="menu"
          aria-busy={busy}
          ondblclick={(e) => showCard(r, e.currentTarget)}
          oncontextmenu={(e) => openMenu(e, i)}
          onkeydown={(e) => onChipKey(e, i, r)}>{recipientLabel(r)}</button
        >
        <button class="chip-x" onclick={(e) => (e.stopPropagation(), remove(i))} {disabled} aria-label="Убрать {recipientLabel(r)}" tabindex="-1">×</button>
      </span>
    {/each}
    <input
      {id}
      bind:this={input}
      bind:value={text}
      type="text"
      role="combobox"
      aria-expanded={open}
      aria-autocomplete="list"
      aria-controls="{id}-list"
      aria-activedescendant={open && cur >= 0 ? optId(cur) : undefined}
      aria-busy={dirBusy}
      autocomplete="off"
      spellcheck="false"
      {disabled}
      placeholder={recipients.length ? "" : placeholder}
      oninput={onInput}
      onkeydown={onKey}
      onpaste={onPaste}
      onblur={onBlur}
    />
  </div>
  {#if open}
    <!-- Нажатие в любом месте списка (заголовок, подсказка, отступ, полоса прокрутки) не уводит фокус из поля: иначе уход фокуса сделал бы из набранного имени неверный чип. -->
    <div class="list" role="listbox" id="{id}-list" aria-label="Подсказки адресатов" tabindex="-1" onmousedown={(e) => e.preventDefault()}>
      {#snippet option(o: Option, i: number)}
        <div
          class="opt"
          class:active={i === cur}
          id={optId(i)}
          role="option"
          aria-selected={i === cur}
          aria-keyshortcuts={o.list ? "Shift+Enter" : undefined}
          aria-describedby={o.list ? `${id}-xhint` : undefined}
          tabindex="-1"
          onmousedown={(e) => (e.preventDefault(), pick(o))}
          onpointermove={() => (activeKey = o.key)}
        >
          <div class="opt-main">
            <span class="opt-name">{o.name || o.email}</span>
            {#if o.name}<span class="opt-email caption">{o.email}</span>{/if}
            {#if o.list}
              <span class="badge">список</span>
            {/if}
            {#if o.list && expandSupported()}
              <!-- Дубль Shift+Enter для мыши: диктору о нём говорит описание варианта. -->
              <button
                type="button"
                class="opt-expand"
                tabindex="-1"
                aria-hidden="true"
                class:off={expanding !== null}
                title={EXPAND_HINT}
                onmousedown={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  expandOption(o);
                }}>Развернуть</button
              >
            {/if}
          </div>
          {#if o.line}<div class="opt-line caption">{o.line}</div>{/if}
        </div>
      {/snippet}
      {#each options.slice(0, localCount) as o, i (o.key)}
        {@render option(o, i)}
      {/each}
      {#if dirGroup}
        <div class="group" role="group" aria-labelledby="{id}-dir">
          <div class="group-label caption" id="{id}-dir">
            Справочник организации
            {#if dirBusy}<span class="spinner small" aria-hidden="true"></span>{/if}
          </div>
          {#each options.slice(localCount) as o, j (o.key)}
            {@render option(o, localCount + j)}
          {/each}
          {#if dirBusy && !dirShown.length}
            <div class="hint caption" role="status">Ищу в справочнике…</div>
          {/if}
          {#if dirShownError}
            <div class="hint caption error" role="status">{dirShownError}</div>
          {:else if dirNothing}
            <div class="hint caption" role="status">В справочнике ничего не найдено</div>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
  <!-- Описание вариантов-списков (aria-describedby): вне listbox, там только варианты. -->
  <span class="sr-only" id="{id}-xhint">{EXPAND_HINT}</span>
  {#if pendingExpand}
    {@const p = pendingExpand}
    <div class="confirm" role="alertdialog" aria-labelledby="{id}-xc" tabindex="-1" onkeydown={onConfirmKey}>
      <span class="confirm-text" id="{id}-xc">
        {p.inPlace ? `В списке «${recipientLabel(p.list)}» ${addressesText(p.members.length)}. Поставить их вместо списка?` : `Добавить ${addressesText(p.members.length)} из списка «${recipientLabel(p.list)}»?`}
      </span>
      <span class="confirm-buttons">
        <button class="btn primary" bind:this={confirmBtn} onclick={confirmExpand}>Развернуть</button>
        <button class="btn" onclick={cancelExpand}>Отмена</button>
      </span>
    </div>
  {:else}
    {#if expanding}
      <div class="note caption busy" role="status"><span class="spinner small" aria-hidden="true"></span>Получаю участников «{expanding.label}»…</div>
    {/if}
    {#if note}
      <div class="note caption" class:error={note.kind === "error"} role={note.kind === "error" ? "alert" : "status"}>{note.text}</div>
    {/if}
  {/if}
  {#if menu}
    <ContextMenu items={menuItems} anchor={menu.anchor} label="Действия с адресатом" onpick={onMenuPick} onclose={onMenuClose} />
  {/if}
</div>

<style>
  .wrap {
    position: relative;
    min-width: 0;
  }
  .box {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    min-height: var(--control-height);
    padding: 3px 6px;
    border: 1px solid var(--stroke-1);
    border-bottom-color: var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    cursor: text;
    transition: border-color var(--dur-fast) var(--ease-in);
  }
  .box:focus-within {
    border-bottom: 2px solid var(--accent);
    padding-bottom: 2px;
  }
  .box.disabled {
    cursor: default;
    color: var(--fg-disabled);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    height: 24px;
    padding: 0 2px 0 8px;
    border-radius: 12px;
    background: var(--accent-soft);
    color: var(--fg-1);
    max-width: 320px;
  }
  .chip.list {
    padding-left: 6px;
  }
  /* Выбранный (в фокусе) чип: с него работают Enter, Delete и меню. */
  .chip:has(.chip-text:focus) {
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .chip.invalid {
    background: transparent;
    box-shadow: inset 0 0 0 1px var(--danger);
    color: var(--danger);
  }
  .chip-icon {
    flex: none;
    color: var(--accent);
  }
  .chip-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: inherit;
    border-radius: 10px;
  }
  .chip-text:focus-visible {
    outline: none;
  }
  .chip-x {
    width: 20px;
    height: 20px;
    flex: none;
    border-radius: 50%;
    color: var(--fg-3);
    font-size: 16px;
    line-height: 1;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .chip-x:hover:not(:disabled) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .chip-x:disabled {
    color: var(--fg-disabled);
  }
  input {
    flex: 1 1 140px;
    min-width: 60px;
    height: 24px;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 4px;
    user-select: text;
  }
  input::placeholder {
    color: var(--fg-3);
  }
  .list {
    position: absolute;
    left: 0;
    right: 0;
    top: calc(100% + 2px);
    z-index: 60;
    max-width: 520px;
    max-height: min(420px, 55vh);
    overflow-y: auto;
    padding: var(--space-1);
    background: var(--bg-card);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
  }
  .opt {
    display: flex;
    flex-direction: column;
    justify-content: center;
    min-height: 32px;
    padding: 4px var(--space-2);
    border-radius: var(--radius-control);
    min-width: 0;
  }
  .opt.active {
    background: var(--bg-selected);
  }
  .opt-main {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
  }
  .opt-name {
    flex: none;
    max-width: 60%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .opt-email {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .opt-line {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    flex: none;
    align-self: center;
    font: var(--text-caption);
    line-height: 16px;
    padding: 0 6px;
    border: 1px solid var(--stroke-2);
    border-radius: 9px;
    color: var(--fg-3);
  }
  .opt-expand {
    flex: none;
    align-self: center;
    height: 22px;
    padding: 0 8px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    font: var(--text-caption);
    color: var(--fg-2);
    transition: background var(--dur-fast) var(--ease-in);
  }
  .opt-expand:hover:not(.off) {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  /* Другой список ещё разворачивается. Не disabled: нажатие по выключенной кнопке увело бы фокус из поля. */
  .opt-expand.off {
    color: var(--fg-disabled);
  }
  .group {
    margin-top: var(--space-1);
  }
  /* Описание для диктора, на экране не видно. */
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .group:first-child {
    margin-top: 0;
  }
  .group-label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: 24px;
    padding: 0 var(--space-2);
    font-weight: 600;
    color: var(--fg-3);
  }
  .list > .opt + .group {
    border-top: 1px solid var(--stroke-1);
    padding-top: var(--space-1);
  }
  .hint {
    padding: 4px var(--space-2);
  }
  .hint.error,
  .note.error {
    color: var(--danger);
  }
  .note {
    margin-top: 2px;
    padding: 0 2px;
    overflow-wrap: anywhere;
  }
  .note.busy {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .confirm {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-top: var(--space-1);
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-card);
    outline: none;
  }
  .confirm-text {
    flex: 1 1 220px;
  }
  .confirm-buttons {
    display: inline-flex;
    gap: var(--space-2);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    height: 28px;
    padding: 0 12px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-1);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
  .btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--fg-on-accent);
  }
  .btn.primary:hover {
    background: var(--accent-hover);
  }
  .btn.primary:active {
    background: var(--accent-pressed);
  }
  .spinner {
    flex: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: var(--accent);
    animation: spin 0.9s linear infinite;
  }
  .spinner.small {
    width: 10px;
    height: 10px;
    border-width: 1.5px;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spinner {
      animation-duration: 2.4s;
    }
  }
</style>
