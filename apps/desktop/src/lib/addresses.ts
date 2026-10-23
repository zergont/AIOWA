/** Адреса получателей: разбор набранного текста в чипы и проверка формата. */
import type { ComposeRecipient } from "./api";

const EMAIL_RE = /^[^\s@<>"',;()]+@[^\s@<>"',;()]+\.[^\s@<>"',;()]+$/;

export function isValidEmail(s: string): boolean {
  return EMAIL_RE.test(s.trim());
}

export function sameEmail(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

/** Подпись чипа: имя, если есть, иначе адрес. */
export function recipientLabel(r: ComposeRecipient): string {
  return r.name?.trim() || r.email;
}

/**
 * Разбор строки с адресами: «a@b.c», «Имя <a@b.c>», «"Имя, Отчество" <a@b.c>», через запятую, точку
 * с запятой или с новой строки. Запятая внутри имени («Иванов, Иван <a@b.c>») не делит адрес: куски
 * без «@» приклеиваются к следующему. Что не разобралось, становится чипом с неверным адресом — его
 * покажет поле, а отправка откажет.
 */
export function parseAddresses(text: string): ComposeRecipient[] {
  const out: ComposeRecipient[] = [];
  for (const part of text.split(/[;\n]+/)) {
    let acc = "";
    for (const piece of part.split(",")) {
      acc = acc ? `${acc},${piece}` : piece;
      if (piece.includes("@")) {
        pushToken(out, acc);
        acc = "";
      }
    }
    if (acc.trim()) pushToken(out, acc);
  }
  return out;
}

function pushToken(out: ComposeRecipient[], token: string) {
  const t = token.trim();
  if (!t) return;
  let m = /^"?(.*?)"?\s*<([^<>]+)>$/.exec(t);
  if (m) {
    out.push({ name: m[1].trim().replace(/^"|"$/g, "").trim() || null, email: m[2].trim() });
    return;
  }
  m = /^<?([^<>\s]+)>?$/.exec(t);
  if (m) {
    out.push({ name: null, email: m[1] });
    return;
  }
  out.push({ name: null, email: t });
}

/** Добавить адреса без дублей (без учёта регистра) к уже выбранным. */
export function mergeRecipients(have: ComposeRecipient[], add: ComposeRecipient[]): ComposeRecipient[] {
  const seen = new Set(have.map((r) => r.email.trim().toLowerCase()));
  const out = [...have];
  for (const r of add) {
    const key = r.email.trim().toLowerCase();
    if (!key || seen.has(key)) continue;
    seen.add(key);
    out.push(r);
  }
  return out;
}
