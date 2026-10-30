/**
 * Фото людей для аватаров (п. 9 обкатки, этап 6): кэш окна «адрес → data:-адрес или null (фото
 * нет)». Заполняется из карточки контакта (бэкенд отдаёт фото вместе с записью) и по запросу
 * `requestPhoto` для отправителя открытого письма: один запрос к серверу за раз, повторов по уже
 * известным адресам нет. Подсказки и списки берут только то, что уже в кэше.
 */
import { api } from "./api";

const cache = $state<Record<string, string | null>>({});
const pending = new Set<string>();
const queue: string[] = [];
let running = false;

function keyOf(email: string | null | undefined): string {
  return (email ?? "").trim().toLowerCase();
}

/** Фото из кэша окна: data:-адрес, null — известно, что фото нет, undefined — не спрашивали. */
export function photoUrl(email: string | null | undefined): string | null | undefined {
  const k = keyOf(email);
  return k ? cache[k] : undefined;
}

/** Запомнить фото (или его отсутствие), пришедшее вместе с карточкой контакта. */
export function notePhoto(email: string | null | undefined, url: string | null | undefined) {
  const k = keyOf(email);
  if (!k) return;
  cache[k] = url ?? null;
}

/** Подгрузить фото из кэша бэкенда для списка адресов, без сети (заголовки, подсказки). */
export async function primePhotos(emails: (string | null | undefined)[]) {
  const keys = [...new Set(emails.map(keyOf).filter((k) => k && !(k in cache)))];
  if (!keys.length) return;
  try {
    const found = await api.contactPhotos(keys);
    for (const k of keys) cache[k] = found[k] ?? cache[k] ?? null;
  } catch {
    // Без кэша — инициалы.
  }
}

/** Запросить фото у бэкенда (кэш или сервер) для одного адреса; результат ложится в кэш окна. */
export function requestPhoto(email: string | null | undefined) {
  const k = keyOf(email);
  if (!k || !k.includes("@") || k in cache || pending.has(k)) return;
  pending.add(k);
  queue.push(k);
  void drain();
}

async function drain() {
  if (running) return;
  running = true;
  try {
    while (queue.length) {
      const k = queue.shift()!;
      try {
        cache[k] = (await api.contactPhoto(k)) ?? null;
      } catch {
        cache[k] = null;
      } finally {
        pending.delete(k);
      }
    }
  } finally {
    running = false;
  }
}
