<script lang="ts">
  import { untrack } from "svelte";
  import { api, errorMessage, isExternalLink } from "../api";
  import { DARK_SHEET_CSS, darkPaletteCss, isDarkTag } from "../darkmail.svelte";
  import { layerOpen } from "../hotkeys";

  interface Props {
    /** Тело письма (HTML уже с data:-картинками вместо cid). */
    html: string;
    title?: string;
    /** Ctrl+P при фокусе внутри фрейма: печатать письмо, а не окно целиком. Без него клавиша просто гасится. */
    onprint?: () => void;
    /** Показать в тёмных тонах; действует, только если у тела есть палитра `darkColors` и метка `darkTag`. */
    dark?: boolean;
    /** Тёмная палитра тела (`Message.dark_colors`); нет — только светлый лист. */
    darkColors?: string[] | null;
    /** Метка имён переменных палитры (`Message.dark_tag`); нет — только светлый лист. */
    darkTag?: string | null;
  }
  let { html, title = "Текст письма", onprint, dark = false, darkColors = null, darkTag = null }: Props = $props();

  // Тёмный лист — класс на корне документа рамки. Его ставит окно снаружи (рамка с
  // allow-same-origin), скриптов внутри рамки нет и CSP не меняется; смена темы не перезагружает письмо.
  const darkOn = $derived(dark && darkColors != null && isDarkTag(darkTag));
  const palette = $derived(darkPaletteCss(darkColors, darkTag));

  // Письмо живёт в изолированном фрейме: без скриптов, без внешних ресурсов, своя «бумага».
  // Инлайновые картинки приходят уже как data: (подставляет бэкенд), поэтому cid: в CSP не нужен.
  // Класс в разметке — только начальный (без мигания белым при загрузке); дальше его ведёт эффект.
  const srcdoc = $derived(`<!doctype html><html lang="ru"${untrack(() => darkOn) ? ' class="dark"' : ""}><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src 'none'; frame-src 'none'">
<style>
  html,body{margin:0;background:#fff;color:#1f1f1f}
  body{font:14px/1.5 "Segoe UI Variable Text","Segoe UI",Inter,system-ui,sans-serif;padding:16px 20px;word-wrap:break-word;overflow-wrap:anywhere}
  img{max-width:100%;height:auto}
  table{max-width:100%}
  pre{white-space:pre-wrap}
  blockquote{border-left:3px solid #ddd;margin:8px 0;padding:0 12px;color:#555}
  a{color:#005fb8}
${DARK_SHEET_CSS}
${palette}
</style></head><body>${html}</body></html>`);

  let frame = $state<HTMLIFrameElement | null>(null);
  let height = $state(120);
  let linkNotice = $state<string | null>(null);

  function fit() {
    const doc = frame?.contentDocument;
    if (!doc) return;
    height = Math.min(20000, Math.max(80, doc.documentElement.scrollHeight + 2));
  }
  $effect(() => {
    void srcdoc;
    // Высота пересчитывается после загрузки и при подгрузке картинок.
    const id = setTimeout(fit, 50);
    return () => clearTimeout(id);
  });

  /** Тёмный лист в загруженном документе рамки — по текущему выбору. */
  function applyDark(on: boolean) {
    frame?.contentDocument?.documentElement?.classList.toggle("dark", on);
  }
  $effect(() => {
    applyDark(darkOn);
  });

  /** Каждая загрузка srcdoc — новый документ, обработчики вешаются заново. */
  function onFrameLoad() {
    // Выбор мог смениться, пока документ грузился: класс в разметке тогда устарел.
    applyDark(darkOn);
    fit();
    // Ссылки из письма открываются снаружи: иначе клик увёл бы сам фрейм на чужой сайт.
    frame?.contentDocument?.addEventListener("click", onFrameClick, true);
    // Клавиши из фрейма до окна не доходят: F5 и Ctrl+R перезагрузили бы окно и потеряли набранное письмо.
    frame?.contentDocument?.addEventListener("keydown", onFrameKey, true);
  }

  function onFrameKey(e: KeyboardEvent) {
    const ctrl = e.ctrlKey || e.metaKey;
    if (e.key === "F5" || (ctrl && !e.altKey && (e.code === "KeyR" || e.key.toLowerCase() === "r"))) e.preventDefault();
    // Ctrl+P изнутри фрейма напечатал бы окно приложения целиком. Под открытым слоем (окно, вопрос)
    // письмо не печатается, как и Ctrl+P в самом окне.
    if (ctrl && !e.altKey && !e.shiftKey && (e.code === "KeyP" || e.key.toLowerCase() === "p")) {
      e.preventDefault();
      if (!layerOpen()) onprint?.();
    }
  }

  function onFrameClick(e: MouseEvent) {
    // Узлы фрейма из другого realm: instanceof Element здесь не работает, идём по nodeType.
    const node = e.target as Node | null;
    const el = node == null ? null : node.nodeType === 1 ? (node as Element) : node.parentElement;
    const link = el?.closest("a[href], area[href]");
    if (!link) return;
    e.preventDefault();
    e.stopPropagation();
    const href = link.getAttribute("href") ?? "";
    if (!isExternalLink(href)) return;
    api.openExternal(href).catch((err) => {
      linkNotice = errorMessage(err);
    });
  }
</script>

{#if linkNotice}
  <div class="notice" role="alert">
    <span class="notice-text">{linkNotice}</span>
    <button class="btn" onclick={() => (linkNotice = null)} aria-label="Скрыть сообщение">Скрыть</button>
  </div>
{/if}
<iframe bind:this={frame} {title} sandbox="allow-same-origin" {srcdoc} class:dark={darkOn} style="height: {height}px" onload={onFrameLoad}></iframe>

<style>
  iframe {
    display: block;
    width: 100%;
    border: 0;
    background: #fff;
  }
  /* Под тёмным листом — тот же цвет, что у листа (`DARK_SHEET_CSS`): без белой вспышки при загрузке. */
  iframe.dark {
    background: #2b2b2b;
  }
  .notice {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    color: var(--danger);
  }
  .notice-text {
    flex: 1;
    min-width: 0;
    user-select: text;
  }
  .btn {
    flex: none;
    height: 24px;
    padding: 0 10px;
    border: 1px solid var(--stroke-2);
    border-radius: var(--radius-control);
    background: var(--bg-layer-alt);
    color: var(--fg-2);
    font: var(--text-caption);
    white-space: nowrap;
    transition: background var(--dur-fast) var(--ease-in);
  }
  .btn:hover {
    background: var(--bg-hover);
    color: var(--fg-1);
  }
  .btn:active {
    background: var(--bg-pressed);
  }
</style>
