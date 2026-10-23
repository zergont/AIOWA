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
