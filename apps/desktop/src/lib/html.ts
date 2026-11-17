/** HTML редактора письма: очистка вставленного и текст без разметки. */
import { isExternalLink } from "./api";

/** Теги, которые остаются при вставке; всё прочее разворачивается до содержимого. */
const KEEP = new Set(["P", "BR", "DIV", "SPAN", "B", "STRONG", "I", "EM", "U", "S", "STRIKE", "DEL", "SUB", "SUP", "UL", "OL", "LI", "A", "BLOCKQUOTE", "H1", "H2", "H3", "H4", "H5", "H6", "PRE", "CODE", "HR", "TABLE", "THEAD", "TBODY", "TFOOT", "TR", "TD", "TH"]);
/** Теги, которые выбрасываются вместе с содержимым. */
const DROP = new Set(["SCRIPT", "STYLE", "HEAD", "TITLE", "META", "LINK", "IFRAME", "OBJECT", "EMBED", "SVG", "MATH", "TEMPLATE", "NOSCRIPT", "FORM", "INPUT", "BUTTON", "SELECT", "TEXTAREA", "IMG", "VIDEO", "AUDIO", "CANVAS"]);

export function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/**
 * Очищенная копия HTML из буфера обмена: только простая разметка (абзацы, списки, жирный/курсив,
 * ссылки http/https/mailto, таблицы), без атрибутов, стилей, скриптов и картинок.
 * Документ DOMParser инертный: ничего не выполняется и не загружается.
 */
export function sanitizeHtml(html: string): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  const out = doc.createElement("div");
  copyClean(doc.body, out);
  return out.innerHTML;
}

function copyClean(src: Node, dst: Element) {
  const doc = dst.ownerDocument;
  for (const child of Array.from(src.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      dst.appendChild(doc.createTextNode(child.nodeValue ?? ""));
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue;
    const el = child as Element;
    const tag = el.tagName.toUpperCase();
    if (DROP.has(tag)) continue;
    if (!KEEP.has(tag)) {
      copyClean(el, dst);
      continue;
    }
    const ne = doc.createElement(tag.toLowerCase());
    if (tag === "A") {
      const href = (el.getAttribute("href") ?? "").trim();
      if (isExternalLink(href)) ne.setAttribute("href", href);
    } else if (tag === "TD" || tag === "TH") {
      for (const a of ["colspan", "rowspan"]) {
        const v = el.getAttribute(a);
        if (v && /^\d+$/.test(v)) ne.setAttribute(a, v);
      }
    }
    copyClean(el, ne);
    dst.appendChild(ne);
  }
}

/** Элементы, которым не место в редакторе: код, стили документа, внешние ресурсы и формы. */
const DRAFT_DROP = "script, style, link, meta, title, base, iframe, frame, object, embed, applet, template, noscript, form, input, button, select, textarea, svg, math";

/** Исходный `src` картинки и исходный `style`, спрятанные на время правки (см. cleanDraftHtml). */
const KEEP_SRC = "data-aiowa-src";
const KEEP_STYLE = "data-aiowa-style";

/** Ключ cid как у бэкенда (`normalize_cid`): без `cid:` и `<>`, %-последовательности раскрыты, нижний регистр. */
export function normalizeCid(raw: string): string {
  let s = raw.trim();
  if (/^cid:/i.test(s)) s = s.slice(4);
  try {
    s = decodeURIComponent(s);
  } catch {
    // Битая %-последовательность: как есть (бэкенд в этом случае тоже оставляет символы).
  }
  return s.trim().replace(/^<+/, "").replace(/>+$/, "").trim().toLowerCase();
}

/**
 * Тело черновика с сервера для редактора: разметка и форматирование остаются (редактор живёт в
 * окне приложения, а не в изолированном фрейме, поэтому стили документа, скрипты и обработчики
 * выбрасываются, а внешние картинки и ссылки не http/https/mailto не загружаются). Документ
 * DOMParser инертный.
 *
 * Картинки и стили с `url(…)` не теряются: исходный `src` (`cid:` инлайнового вложения, внешний
 * адрес) и исходный `style` прячутся в data-атрибуты, а для показа картинке `cid:` подставляется
 * адрес из кэша (`inline`: нормализованный cid → адрес aiowa-att), внешней — ничего. Перед
 * сохранением restoreDraftHtml возвращает исходные значения: на сервер черновик уходит таким, каким
 * пришёл, кроме правок пользователя.
 */
export function cleanDraftHtml(html: string, inline: Record<string, string> = {}): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll(DRAFT_DROP).forEach((el) => el.remove());
  // Чужие служебные атрибуты — до разбора: иначе restoreDraftHtml вернул бы то, чего в письме не было.
  for (const el of Array.from(doc.body.querySelectorAll(`[${KEEP_SRC}], [${KEEP_STYLE}]`))) {
    el.removeAttribute(KEEP_SRC);
    el.removeAttribute(KEEP_STYLE);
  }
  for (const el of Array.from(doc.body.querySelectorAll("*"))) {
    for (const attr of Array.from(el.attributes)) {
      const name = attr.name.toLowerCase();
      const value = attr.value.trim();
      if (name.startsWith("on") || name === "srcset" || name === "formaction") el.removeAttribute(attr.name);
      else if (name === "href" && !isExternalLink(value) && !value.startsWith("#")) el.removeAttribute(attr.name);
      else if (name === "src") {
        if (/^data:image\//i.test(value)) continue;
        el.removeAttribute(attr.name);
        if (el.tagName !== "IMG" || !value) continue;
        el.setAttribute(KEEP_SRC, value);
        const shown = /^cid:/i.test(value) ? inline[normalizeCid(value)] : undefined;
        if (shown) el.setAttribute("src", shown);
      } else if (name === "style" && /url\s*\(|expression\s*\(|position\s*:|behavior\s*:/i.test(value)) {
        el.removeAttribute(attr.name);
        el.setAttribute(KEEP_STYLE, attr.value);
      }
    }
  }
  return doc.body.innerHTML;
}

/**
 * Картинка, вставленная в текст письма из буфера (п. 40 обкатки): в редакторе показывается по
 * адресу `url`, а в письмо уйдёт ссылкой `cid:` на встроенное вложение (restoreDraftHtml).
 * `width` — ширина для слишком широкой картинки (снимок экрана), иначе — как есть.
 */
export function inlineImageHtml(url: string, cid: string, width: number | null): string {
  return `<img src="${escapeHtml(url)}" ${KEEP_SRC}="cid:${escapeHtml(cid)}" alt=""${width ? ` width="${width}"` : ""}>`;
}

/**
 * Адреса показа для картинок текста, на которые тело ссылается по `cid:` (`urls`: cid → адрес):
 * после восстановления письма или «Развернуть» у таких картинок остаётся только спрятанный `cid:`.
 */
export function showInlineImages(html: string, urls: Map<string, string>): string {
  if (!urls.size || !html.includes(KEEP_SRC)) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  let changed = false;
  for (const img of Array.from(doc.body.querySelectorAll(`img[${KEEP_SRC}]`))) {
    const url = urls.get(normalizeCid(img.getAttribute(KEEP_SRC) ?? ""));
    if (url && img.getAttribute("src") !== url) {
      img.setAttribute("src", url);
      changed = true;
    }
  }
  return changed ? doc.body.innerHTML : html;
}

/** Убрать из текста картинку с этим Content-ID: её копию не удалось сохранить, уйти ей не с чем. */
export function dropInlineImage(html: string, cid: string): string {
  if (!html.includes(KEEP_SRC)) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  const key = normalizeCid(cid);
  const found = Array.from(doc.body.querySelectorAll(`img[${KEEP_SRC}]`)).filter((img) => normalizeCid(img.getAttribute(KEEP_SRC) ?? "") === key);
  for (const img of found) img.remove();
  return found.length ? doc.body.innerHTML : html;
}

/**
 * HTML редактора для сохранения на сервере: картинкам возвращается исходный `src`, элементам —
 * исходный `style` (если стиль поменяли в редакторе, новые свойства идут после исходных и
 * перекрывают их). Без спрятанных атрибутов HTML возвращается как есть.
 */
export function restoreDraftHtml(html: string): string {
  if (!html.includes(KEEP_SRC) && !html.includes(KEEP_STYLE)) return html;
  const doc = new DOMParser().parseFromString(html, "text/html");
  for (const el of Array.from(doc.body.querySelectorAll(`[${KEEP_SRC}]`))) {
    el.setAttribute("src", el.getAttribute(KEEP_SRC) ?? "");
    el.removeAttribute(KEEP_SRC);
  }
  for (const el of Array.from(doc.body.querySelectorAll(`[${KEEP_STYLE}]`))) {
    const kept = el.getAttribute(KEEP_STYLE) ?? "";
    const now = el.getAttribute("style");
    el.setAttribute("style", now ? `${kept};${now}` : kept);
    el.removeAttribute(KEEP_STYLE);
  }
  return doc.body.innerHTML;
}

/** Текст без разметки, пробелы схлопнуты: для сравнения «изменилось ли письмо». */
export function htmlToText(html: string): string {
  if (!html) return "";
  const doc = new DOMParser().parseFromString(html, "text/html");
  return (doc.body.textContent ?? "").replace(/\s+/g, " ").trim();
}

/** Ссылка из набранного в редакторе: без схемы считается https. */
export function normalizeLink(input: string): string | null {
  const s = input.trim();
  if (!s) return null;
  if (isExternalLink(s)) return s;
  if (/^[\w.-]+@[\w.-]+\.\w+$/.test(s)) return `mailto:${s}`;
  if (/^[\w-]+(\.[\w-]+)+(\/|$|\?|#)/.test(s)) return `https://${s}`;
  return null;
}
