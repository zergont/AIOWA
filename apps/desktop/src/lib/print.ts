/** Печать письма: скрытый фрейм с печатной версией (шапка и очищенное тело) и его window.print(). */
import { fullDate, personLabel, type Message } from "./api";
import { escapeHtml } from "./html";

/** Печатная версия письма: шапка (тема, от кого, кому, копия, дата) и тело. Картинки в теле уже data:. */
export function printDocument(m: Message): string {
  const names = (kind: number) =>
    m.recipients
      .filter((r) => r.kind === kind)
      .map((r) => personLabel(r.name, r.email))
      .filter(Boolean);
  const to = names(1);
  const cc = names(2);
  const rows: [string, string][] = [
    ["От", personLabel(m.from_name, m.from_email) || "—"],
    ["Кому", to.length ? to.join(", ") : m.display_to || "—"],
  ];
  if (cc.length) rows.push(["Копия", cc.join(", ")]);
  rows.push(["Дата", fullDate(m.received_ms ?? m.sent_ms) || "—"]);
  const subject = m.subject || "(без темы)";
  const body = m.body_html ?? (m.preview ? `<p>${escapeHtml(m.preview)}</p>` : "<p>(текст письма не загружен)</p>");
  const meta = rows.map(([k, v]) => `<tr><th>${escapeHtml(k)}:</th><td>${escapeHtml(v)}</td></tr>`).join("");
  // Скрипты, внешние ресурсы и формы запрещены политикой документа: в печать идёт только разметка и data:-картинки.
  return `<!doctype html><html lang="ru"><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; form-action 'none'">
<title>${escapeHtml(subject)}</title>
<style>
  @page { margin: 15mm; }
  html, body { margin: 0; background: #fff; color: #000; }
  body { font: 12pt/1.45 "Segoe UI", Arial, sans-serif; word-wrap: break-word; overflow-wrap: anywhere; }
  h1 { font-size: 16pt; margin: 0 0 8pt; }
  table.meta { border-collapse: collapse; margin: 0 0 10pt; font-size: 10.5pt; }
  table.meta th { text-align: left; font-weight: 600; padding: 1pt 8pt 1pt 0; vertical-align: top; white-space: nowrap; color: #333; }
  table.meta td { padding: 1pt 0; vertical-align: top; }
  hr { border: 0; border-top: 1px solid #999; margin: 0 0 10pt; }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  pre { white-space: pre-wrap; }
  blockquote { border-left: 2pt solid #999; margin: 6pt 0; padding: 0 10pt; color: #333; }
  a { color: #000; text-decoration: underline; }
</style></head><body>
<h1>${escapeHtml(subject)}</h1>
<table class="meta">${meta}</table>
<hr>
${body}
</body></html>`;
}

let current: HTMLIFrameElement | null = null;
let cleanupTimer: ReturnType<typeof setTimeout> | undefined;

function dropFrame() {
  clearTimeout(cleanupTimer);
  current?.remove();
  current = null;
}

/**
 * Напечатать письмо через скрытый фрейм. Промис завершается, когда диалог печати открыт (или
 * отклоняется текстом по-русски, если печать в этом окне недоступна). Фрейм живёт до события
 * afterprint — убрать его раньше значило бы закрыть диалог печати.
 */
export function printMessage(m: Message): Promise<void> {
  return new Promise((resolve, reject) => {
    dropFrame();
    const frame = document.createElement("iframe");
    frame.setAttribute("aria-hidden", "true");
    frame.tabIndex = -1;
    frame.title = "Печать письма";
    // Не display:none: невидимый в раскладке фрейм печатается пустым.
    frame.style.cssText = "position:fixed;right:0;bottom:0;width:0;height:0;border:0;opacity:0;pointer-events:none";
    current = frame;
    const timeout = setTimeout(() => {
      if (current === frame) dropFrame();
      reject("Печать не началась: фрейм письма не загрузился");
    }, 15_000);
    // Печать один раз и только по документу письма: срабатывания load до него (about:blank) не в счёт.
    let started = false;
    frame.onload = () => {
      if (started || frame.contentDocument?.URL !== "about:srcdoc") return;
      started = true;
      clearTimeout(timeout);
      const win = frame.contentWindow;
      if (!win || typeof win.print !== "function") {
        dropFrame();
        reject("Печать недоступна в этом окне");
        return;
      }
      win.addEventListener("afterprint", () => {
        if (current === frame) dropFrame();
      });
      // Если afterprint не придёт (диалог закрыт иначе), фрейм всё равно не останется навсегда.
      cleanupTimer = setTimeout(() => {
        if (current === frame) dropFrame();
      }, 10 * 60_000);
      // Раскладке нужен кадр: иначе первый вызов печатает пустую страницу.
      setTimeout(() => {
        try {
          win.focus();
          win.print();
          resolve();
        } catch (e) {
          dropFrame();
          reject(e instanceof Error ? e.message : "Печать не удалась");
        }
      }, 50);
    };
    // srcdoc — до вставки: иначе Chromium сразу (синхронно, внутри appendChild) отдаёт load по
    // пустому about:blank, и печать могла начаться раньше, чем загрузится письмо.
    frame.srcdoc = printDocument(m);
    document.body.appendChild(frame);
  });
}
