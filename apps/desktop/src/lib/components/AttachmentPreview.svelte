<script lang="ts">
  import { api, errorMessage, formatSize, fullDate, personLabel, type Attachment, type EmbeddedMessage, type EmbeddedPerson, type PreviewDto } from "../api";
  import { hint, isTopLayer, keysOf, layer } from "../hotkeys";
  import BodyFrame from "./BodyFrame.svelte";
  import WindowControls from "./WindowControls.svelte";
  import { darkMailByDefault, windowIsDark } from "../darkmail.svelte";

  /**
   * Предпросмотр вложений письма поверх области чтения: картинка, PDF, текст (в том числе HTML — как
   * текст, без выполнения). Файл отдаёт протокол aiowa-att из кэша вложений; адрес и тип — у бэкенда
   * (`attachment_preview_url`, при необходимости он сначала скачивает файл). Стрелки листают вложения.
   */
  interface Props {
    /** Вложения письма, которые можно листать (без инлайновых картинок). */
    files: Attachment[];
    /** Индекс открытого вложения в `files`; двусторонняя привязка — родитель видит, что листают. */
    index: number;
    /** Открыть системным приложением / «Сохранить как…»: те же действия, что у чипов карточки. */
    onopen: (a: Attachment) => void;
    onsave: (a: Attachment) => void;
    /** Состояние этих действий для открытого вложения (у карточки оно под окном и не видно). */
    status?: { busy: "open" | "save" | null; error: string | null; saved: string | null };
    /** Скрыть ошибку действия. */
    onclearerror?: (a: Attachment) => void;
    onclose: () => void;
    /**
     * Своё окно Windows (п. 31, `AttachmentWindow`): без подложки, на всё окно, шапка — заголовок
     * окна (перетаскивание, кнопки окна вместо «Закрыть»).
     */
    standalone?: boolean;
  }
  let { files, index = $bindable(0), onopen, onsave, status, onclearerror, onclose, standalone = false }: Props = $props();
  /** Атрибут перетаскивания окна для шапки окна просмотра (у слоя поверх окна его нет). */
  const drag = $derived(standalone ? "" : undefined);

  const uid = $props.id();
  /** Текст больше этого в окно не грузится: предлагается открыть файл. */
  const TEXT_MAX = 2 * 1024 * 1024;

  const file = $derived(files[index] ?? null);
  /**
   * Ключ загрузки — номер вложения, а не объект: беседа перечитывается (синхронизация, новое письмо),
   * объект вложения новый, а файл тот же — PDF и текст не должны перезагружаться и терять прокрутку.
   */
  const pk = $derived(file?.attachment_pk ?? null);
  const actionBusy = $derived(status?.busy ?? null);
  const name = $derived(file?.name ?? "вложение");
  const hasPrev = $derived(index > 0);
  const hasNext = $derived(index < files.length - 1);

  let dto = $state<PreviewDto | null>(null);
  let text = $state<string | null>(null);
  /** Вложенное письмо .eml (п. 30): шапка, текст и его вложения. */
  let message = $state<EmbeddedMessage | null>(null);
  /** Вложение вложенного письма, с которым идёт действие, и итог действия. */
  let partBusy = $state<number | null>(null);
  let partNote = $state<{ text: string; failed: boolean } | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let dialogEl = $state<HTMLDivElement | null>(null);
  let closeBtn = $state<HTMLButtonElement | null>(null);

  /** Вид показа по типу от бэкенда. */
  const mode = $derived.by((): "image" | "pdf" | "text" | "message" | "none" => {
    if (!dto || !dto.previewable) return "none";
    const ct = (dto.content_type ?? "").toLowerCase().split(";")[0].trim();
    if (ct.startsWith("image/")) return "image";
    if (ct === "application/pdf") return "pdf";
    if (ct.startsWith("text/")) return "text";
    if (ct === "message/rfc822") return "message";
    return "none";
  });
  /**
   * «Открыть» системным приложением: для исполняемых типов бэкенд его не разрешает — только
   * «Сохранить как…»; вложенное письмо уже показано здесь, системе оно не отдаётся.
   */
  const canOpen = $derived(dto?.openable !== false && mode !== "message");

  const people = (ps: EmbeddedPerson[]) => ps.map((p) => personLabel(p.name, p.email)).filter(Boolean).join(", ");

  /** Вложение вложенного письма: открыть системным приложением или «Сохранить как…». */
  async function partAction(index: number, save: boolean) {
    const p = pk;
    if (p == null || partBusy != null) return;
    partBusy = index;
    partNote = null;
    try {
      const path = await api.attachmentMessagePart(p, index, save);
      if (p === pk && path) partNote = { text: `Сохранено: ${path}`, failed: false };
    } catch (e) {
      if (p === pk) partNote = { text: errorMessage(e), failed: true };
    } finally {
      partBusy = null;
    }
  }

  /**
   * Текст файла: UTF-16 по метке порядка байт (Блокнот «Юникод», Excel «Текст в Юникоде»), иначе
   * UTF-8 (метку UTF-8 декодер снимает сам), а если байты не годятся для него — windows-1251 (так
   * сохранены многие русские .txt и .csv).
   */
  function decodeText(buf: ArrayBuffer): string {
    const bom = new Uint8Array(buf, 0, Math.min(2, buf.byteLength));
    if (bom[0] === 0xff && bom[1] === 0xfe) return new TextDecoder("utf-16le").decode(buf);
    if (bom[0] === 0xfe && bom[1] === 0xff) return new TextDecoder("utf-16be").decode(buf);
    try {
      return new TextDecoder("utf-8", { fatal: true }).decode(buf);
    } catch {
      return new TextDecoder("windows-1251").decode(buf);
    }
  }

  /** Каждый ответ сверяется с номером запроса: быстрое листание не должно показать прошлый файл. */
  let request = 0;

  $effect(() => {
    const p = pk;
    dto = null;
    text = null;
    message = null;
    partNote = null;
    error = null;
    if (p == null) return;
    const req = ++request;
    loading = true;
    (async () => {
      try {
        const d = await api.attachmentPreviewUrl(p);
        if (req !== request) return;
        dto = d;
        const ct = (d.content_type ?? "").toLowerCase();
        if (d.previewable && ct.startsWith("text/")) {
          if ((d.size ?? 0) > TEXT_MAX) throw `Файл больше ${formatSize(TEXT_MAX)}: открой его в приложении`;
          const res = await fetch(d.url);
          if (req !== request) return;
          if (!res.ok) throw `Файл не отдан из кэша (код ${res.status})`;
          const buf = await res.arrayBuffer();
          if (req !== request) return;
          const body = decodeText(buf);
          text = body.length > TEXT_MAX ? body.slice(0, TEXT_MAX) : body;
        } else if (d.previewable && ct.startsWith("message/rfc822")) {
          const m = await api.attachmentMessage(p);
          if (req !== request) return;
          message = m;
        }
      } catch (e) {
        if (req === request) error = errorMessage(e);
      } finally {
        if (req === request) loading = false;
      }
    })();
  });

  $effect(() => {
    closeBtn?.focus();
  });

  function go(delta: number) {
    const next = index + delta;
    if (next >= 0 && next < files.length) index = next;
  }

  const FOCUSABLE = 'button:not(:disabled), [href], iframe, [tabindex]:not([tabindex="-1"])';

  /** Окно модальное: Tab ходит по кругу внутри него. */
  function trapTab(e: KeyboardEvent) {
    if (!dialogEl) return;
    const items = Array.from(dialogEl.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => el.offsetParent !== null);
    if (!items.length) {
      e.preventDefault();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    const inside = active instanceof Node && dialogEl.contains(active);
    if (e.shiftKey && (!inside || active === first)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (!inside || active === last)) {
      e.preventDefault();
      first.focus();
    }
  }

  function onKey(e: KeyboardEvent) {
    // Окно слушает всё окно приложения: клавиши — только пока оно верхнее и на экране.
    if (!isTopLayer(dialogEl)) return;
    switch (e.key) {
      case "Escape":
        e.preventDefault();
        e.stopPropagation();
        onclose();
        break;
      case "ArrowLeft":
        e.preventDefault();
        go(-1);
        break;
      case "ArrowRight":
        e.preventDefault();
        go(1);
        break;
      case "Tab":
        trapTab(e);
        break;
    }
  }

  function onBackdrop(e: MouseEvent) {
    if (e.target === e.currentTarget) onclose();
  }
</script>

<svelte:window onkeydown={onKey} />

<!-- Подложка только для мыши: закрытие с клавиатуры — Esc (обработчик окна выше), поэтому для читалок она «презентационная». -->
<div class="overlay" class:standalone role="presentation" onclick={standalone ? undefined : onBackdrop}>
  <div class="dialog" role="dialog" aria-modal={!standalone} aria-labelledby="{uid}-title" aria-busy={loading} bind:this={dialogEl} {@attach layer}>
    <header class="head" data-tauri-drag-region={drag}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 2.5h7l3 3v8H3v-11Zm7 0v3h3" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" /></svg>
      <h2 class="title" id="{uid}-title" title={name} data-tauri-drag-region={drag}>{name}</h2>
      <span class="caption size">{formatSize(file?.size ?? dto?.size ?? null)}</span>
      {#if files.length > 1}
        <span class="caption pos" aria-live="polite">{index + 1} из {files.length}</span>
      {/if}
      <span class="spacer" data-tauri-drag-region={drag}></span>
      {#if canOpen}
        <button class="btn" onclick={() => file && onopen(file)} disabled={!file || actionBusy != null} aria-busy={actionBusy === "open"} title="Открыть в системном приложении">
          {#if actionBusy === "open"}<span class="spinner" aria-hidden="true"></span>{/if}
          Открыть
        </button>
      {/if}
      <button class="btn" onclick={() => file && onsave(file)} disabled={!file || actionBusy != null} aria-busy={actionBusy === "save"} title="Сохранить файл…">
        {#if actionBusy === "save"}<span class="spinner" aria-hidden="true"></span>{/if}
        Сохранить как…
      </button>
      {#if standalone}
        <!-- Окно просмотра: кнопки окна, как у главного; Esc закрывает его так же. -->
        <div class="window-controls">
          <WindowControls />
        </div>
      {:else}
        <button class="btn" bind:this={closeBtn} onclick={onclose} title={`Закрыть (${keysOf("preview-close")}; пока фокус внутри PDF — кнопкой)`}>Закрыть</button>
      {/if}
    </header>

    {#if status?.error}
      <div class="action-note failed" role="alert">
        <span class="note-text">{status.error}</span>
        <button class="btn small" onclick={() => file && onclearerror?.(file)}>Скрыть</button>
      </div>
    {:else if status?.saved}
      <div class="action-note" role="status"><span class="note-text">Сохранено: {status.saved}</span></div>
    {/if}

    <div class="stage" class:paper={mode === "text" || mode === "pdf" || mode === "message"} class:navs={files.length > 1}>
      {#if files.length > 1}
        <button class="nav prev" onclick={() => go(-1)} disabled={!hasPrev} aria-label={hint("Предыдущее вложение", "preview-prev")} title={hint("Предыдущее", "preview-prev")}>
          <svg width="20" height="20" viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3 5 8l5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
        <button class="nav next" onclick={() => go(1)} disabled={!hasNext} aria-label={hint("Следующее вложение", "preview-next")} title={hint("Следующее", "preview-next")}>
          <svg width="20" height="20" viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>
        </button>
      {/if}

      {#if loading}
        <div class="note" aria-live="polite">
          <span class="spinner" aria-hidden="true"></span>
          Загружаю вложение…
        </div>
      {:else if error}
        <div class="note failed" role="alert">
          <span class="note-text">{error}</span>
          {#if canOpen}
            <button class="btn" onclick={() => file && onopen(file)} disabled={!file || actionBusy != null}>Открыть</button>
          {:else}
            <button class="btn" onclick={() => file && onsave(file)} disabled={!file || actionBusy != null}>Сохранить как…</button>
          {/if}
        </div>
      {:else if dto && mode === "image"}
        <img class="image" src={dto.url} alt={name} />
      {:else if dto && mode === "pdf"}
        <!-- Без sandbox: в рамке с sandbox Chromium/WebView2 не запускает встроенный просмотрщик PDF.
             Рамка и так на чужом origin (aiowa-att), ответ — application/pdf с nosniff. -->
        <iframe class="pdf" title={name} src={dto.url}></iframe>
      {:else if dto && mode === "text"}
        <pre class="text">{text ?? ""}</pre>
      {:else if dto && mode === "message" && message}
        <!-- Вложенное письмо (п. 30): как письмо в OWA — шапка, текст в той же рамке, его вложения. -->
        <article class="eml" aria-label="Вложенное письмо">
          <header class="eml-head">
            <h3 class="eml-subject">{message.subject || "(без темы)"}</h3>
            <dl class="eml-meta caption">
              {#if message.from.length}<dt>От:</dt><dd>{people(message.from)}</dd>{/if}
              {#if message.to.length}<dt>Кому:</dt><dd>{people(message.to)}</dd>{/if}
              {#if message.cc.length}<dt>Копия:</dt><dd>{people(message.cc)}</dd>{/if}
              {#if message.date_ms != null}<dt>Дата:</dt><dd>{fullDate(message.date_ms)}</dd>{/if}
            </dl>
            {#if message.attachments.length}
              <div class="eml-files" aria-label="Вложения письма">
                {#each message.attachments as part (part.index)}
                  {@const partName = part.name ?? "вложение"}
                  <span class="eml-file">
                    <button class="eml-open" onclick={() => partAction(part.index, false)} disabled={partBusy != null} title="Открыть в системном приложении">
                      {#if partBusy === part.index}<span class="spinner" aria-hidden="true"></span>{/if}
                      <span class="eml-file-name">{partName}</span>
                      <span class="caption">{formatSize(part.size)}</span>
                    </button>
                    <button class="eml-save" onclick={() => partAction(part.index, true)} disabled={partBusy != null} aria-label="Сохранить как… {partName}" title="Сохранить как…">
                      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2.5v8m0 0 3-3m-3 3-3-3M3 12.5h10" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" /></svg>
                    </button>
                  </span>
                {/each}
              </div>
            {/if}
            {#if partNote}
              <div class="eml-note caption" class:failed={partNote.failed} role={partNote.failed ? "alert" : "status"}>{partNote.text}</div>
            {/if}
          </header>
          <div class="eml-body">
            {#if message.body_html != null}
              <BodyFrame html={message.body_html} title="Текст вложенного письма" dark={windowIsDark() && darkMailByDefault()} darkColors={message.dark_colors} darkTag={message.dark_tag} />
            {:else}
              <p class="caption eml-empty">В письме нет текста.</p>
            {/if}
          </div>
        </article>
      {:else if dto}
        <div class="note">
          <span class="note-text">
            {#if dto.reason === "too_large"}
              Файл слишком большой для предпросмотра ({formatSize(dto.size)}).
            {:else}
              Предпросмотр для этого типа файла недоступен{dto.content_type ? ` (${dto.content_type})` : ""}.
            {/if}
            {#if !canOpen}Открывать такие файлы небезопасно — сохрани его.{/if}
          </span>
          {#if canOpen}
            <button class="btn" onclick={() => file && onopen(file)} disabled={!file || actionBusy != null}>Открыть</button>
          {:else}
            <button class="btn" onclick={() => file && onsave(file)} disabled={!file || actionBusy != null}>Сохранить как…</button>
          {/if}
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 90;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-6);
  }
  /* Своё окно просмотра (п. 31): без подложки и рамки диалога, на всё окно. */
  .overlay.standalone {
    position: static;
    height: 100%;
    padding: 0;
    background: var(--bg-card);
  }
  .standalone .dialog {
    width: 100%;
    height: 100%;
    border: 0;
    border-radius: 0;
    box-shadow: none;
  }
  .standalone .head {
    min-height: 40px;
    padding-block: 0;
    padding-right: 0;
  }
  .window-controls {
    align-self: stretch;
    display: flex;
    margin-left: var(--space-1);
  }
  .dialog {
    display: flex;
    flex-direction: column;
    width: min(1100px, 100%);
    height: min(860px, 100%);
    background: var(--bg-card);
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-16);
    color: var(--fg-1);
    overflow: hidden;
  }
  .head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
    color: var(--fg-2);
  }
  .title {
    margin: 0;
    font: var(--text-body-strong);
    color: var(--fg-1);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    user-select: text;
  }
  .size,
  .pos {
    flex: none;
    white-space: nowrap;
  }
  .spacer {
    flex: 1;
  }
  .btn {
    flex: none;
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
  .btn:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .btn:active:not(:disabled) {
    background: var(--bg-pressed);
  }
  .btn:disabled {
    color: var(--fg-disabled);
  }
  .btn .spinner {
    margin-right: var(--space-1);
  }
  .btn.small {
    height: 24px;
    padding: 0 8px;
  }
  .action-note {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
    color: var(--fg-2);
  }
  .action-note.failed {
    color: var(--danger);
  }

  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-layer);
    overflow: hidden;
  }
  .stage.paper {
    background: var(--bg-reader-paper);
    align-items: stretch;
    justify-content: stretch;
  }
  /* Стрелки листания стоят в боковых полях, а не поверх текста и PDF (их позиция — от padding box). */
  .stage.paper.navs {
    padding-inline: 64px;
  }
  .image {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    user-select: none;
  }
  .eml {
    flex: 1;
    min-width: 0;
    overflow: auto;
    display: flex;
    flex-direction: column;
    background: var(--bg-card);
  }
  .eml-head {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-4) var(--space-5) var(--space-3);
    border-bottom: 1px solid var(--stroke-1);
  }
  .eml-subject {
    margin: 0;
    font: var(--text-subtitle);
    color: var(--fg-1);
    user-select: text;
  }
  .eml-meta {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 2px var(--space-2);
    margin: 0;
    color: var(--fg-2);
    user-select: text;
  }
  .eml-meta dt {
    color: var(--fg-3);
  }
  .eml-meta dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .eml-files {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .eml-file {
    display: inline-flex;
    align-items: stretch;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    max-width: 100%;
  }
  .eml-open,
  .eml-save {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding: 4px 10px;
    color: var(--fg-1);
  }
  .eml-open {
    min-width: 0;
  }
  .eml-save {
    border-left: 1px solid var(--stroke-1);
    color: var(--fg-2);
  }
  .eml-open:hover:not(:disabled),
  .eml-save:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .eml-file-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .eml-note {
    color: var(--fg-2);
    user-select: text;
  }
  .eml-note.failed {
    color: var(--danger);
  }
  .eml-body {
    padding: var(--space-3) var(--space-5) var(--space-5);
  }
  .eml-empty {
    margin: 0;
  }
  .pdf {
    width: 100%;
    height: 100%;
    border: 0;
    background: #fff;
  }
  .text {
    flex: 1;
    margin: 0;
    padding: var(--space-4) var(--space-6);
    overflow: auto;
    font: 13px/1.5 var(--font-mono);
    color: #1f1f1f;
    background: #fff;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    user-select: text;
  }
  .nav {
    position: absolute;
    top: 50%;
    z-index: 1;
    width: 40px;
    height: 40px;
    margin-top: -20px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    border: 1px solid var(--stroke-2);
    background: var(--bg-card);
    color: var(--fg-1);
    box-shadow: var(--shadow-8);
    transition:
      background var(--dur-fast) var(--ease-in),
      opacity var(--dur-fast) var(--ease-in);
  }
  .nav.prev {
    left: var(--space-3);
  }
  .nav.next {
    right: var(--space-3);
  }
  .nav:hover:not(:disabled) {
    background: var(--bg-hover);
  }
  .nav:disabled {
    opacity: 0.35;
  }
  .note {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-4);
    color: var(--fg-2);
    max-width: 560px;
  }
  .note.failed {
    color: var(--danger);
  }
  .note-text {
    min-width: 0;
    user-select: text;
  }
  .spinner {
    flex: none;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    border: 2px solid var(--stroke-2);
    border-top-color: var(--accent);
    animation: spin 0.9s linear infinite;
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
