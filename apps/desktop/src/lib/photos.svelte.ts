/**
 * Фото людей для аватаров (п. 9 обкатки, этап 6): кэш окна «адрес → data:-адрес или null (фото
 * нет)». Заполняется из карточки контакта (бэкенд отдаёт фото вместе с записью) и по запросу
 * `requestPhoto` для отправителя открытого письма: один запрос к серверу за раз, повтор по адресу —
 * не раньше `RECHECK_MS` (срок годности фото в кэше бэкенда — неделя — соблюдает бэкенд), после
 * сбоя — не раньше `RETRY_MS`. Подсказки и списки берут только фото, уже лежащие в кэше бэкенда.
 */
import { api } from "./api";

/** Через сколько снова спросить бэкенд по адресу, на который он уже ответил. */
const RECHECK_MS = 60 * 60 * 1000;
/** Через сколько повторить запрос, если сервер не ответил (нет связи, таймаут). */
const RETRY_MS = 60 * 1000;
/** Адресов в одном запросе к кэшу бэкенда: длинный список (все личные контакты) — частями. */
const PRIME_CHUNK = 200;

const cache = $state<Record<string, string | null>>({});
/** Адрес → момент, раньше которого `requestPhoto` его не спрашивает (не реактивно). */
const nextAsk = new Map<string, number>();
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

/**
 * Подгрузить фото из кэша бэкенда для списка адресов, без сети (заголовки, подсказки). Ложатся
 * только найденные фото: адрес без фото в кэше бэкенда мог ещё не спрашиваться — его спросит
 * `requestPhoto`, а не «фото нет» отсюда.
 */
export async function primePhotos(emails: (string | null | undefined)[]) {
  const keys = [...new Set(emails.map(keyOf).filter((k) => k && !(k in cache)))];
  for (let i = 0; i < keys.length; i += PRIME_CHUNK) {
    try {
      const found = await api.contactPhotos(keys.slice(i, i + PRIME_CHUNK));
      for (const [k, url] of Object.entries(found)) if (!(k in cache)) cache[k] = url;
    } catch {
      return; // Без кэша — инициалы.
    }
  }
}

/** Запросить фото у бэкенда (кэш или сервер) для одного адреса; результат ложится в кэш окна. */
export function requestPhoto(email: string | null | undefined) {
  const k = keyOf(email);
  if (!k || !k.includes("@") || pending.has(k) || (nextAsk.get(k) ?? 0) > Date.now()) return;
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
        nextAsk.set(k, Date.now() + RECHECK_MS);
      } catch {
        // Сервер не ответил — это не «фото нет»: в кэше остаётся что было, повтор позже.
        nextAsk.set(k, Date.now() + RETRY_MS);
      } finally {
        pending.delete(k);
      }
    }
  } finally {
    running = false;
  }
}
