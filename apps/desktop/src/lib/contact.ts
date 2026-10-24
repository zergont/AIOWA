/**
 * Карточка контакта и справочник организации: общее для автодополнения адресатов, карточки письма
 * и самой карточки. Карточка одна на окно — её держит App и раздаёт функцию открытия через контекст,
 * чтобы имена в письме и чипы в композере не тянули обработчики через все промежуточные компоненты.
 */
import { getContext, setContext } from "svelte";
import { SvelteSet } from "svelte/reactivity";
import { api, type DirectoryKind, type DirectoryPerson } from "./api";

/** Кого показать: имя (как в письме или чипе) и адрес. */
export interface ContactRef {
  name: string | null;
  email: string;
}

/** Открыть карточку у элемента-якоря; повторный вызов с тем же якорем закрывает её. */
export type OpenContact = (who: ContactRef, anchor: HTMLElement) => void;

const KEY = Symbol("aiowa-contact-card");

/** App: сделать карточку доступной потомкам. */
export function provideContactCard(open: OpenContact) {
  setContext(KEY, open);
}

/** Функция открытия карточки или null, если компонент живёт вне App (тогда имена не кликаются). */
export function contactCardOpener(): OpenContact | null {
  return getContext<OpenContact | undefined>(KEY) ?? null;
}

/** Ключ адреса: без регистра и пробелов по краям. */
export function emailKey(email: string | null | undefined): string {
  return (email ?? "").trim().toLowerCase();
}

/** Текст для сравнения: без регистра и с «ё» как «е» (так же сравнивает кэш справочника на бэкенде). */
export function foldText(s: string): string {
  return s.toLowerCase().replace(/ё/g, "е");
}

const WORD_SPLIT = /[\s.,;:@_\-()"'«»<>]+/;

/**
 * Запись подходит к набранному: каждое слово запроса — начало какого-то слова имени или адреса
 * («ива пет» находит «Петров Иван»). Нужна, чтобы, пока сервер ищет по новому тексту, оставить
 * на экране подходящее из прежней выдачи.
 */
export function personMatches(p: Pick<DirectoryPerson, "name" | "email">, query: string): boolean {
  const needles = foldText(query.trim()).split(/\s+/).filter(Boolean);
  if (!needles.length) return false;
  const email = foldText(p.email ?? "");
  const words = foldText(`${p.name ?? ""} ${email}`).split(WORD_SPLIT).filter(Boolean);
  return needles.every((n) => email.startsWith(n) || words.some((w) => w.startsWith(n)));
}

/** Строка под именем: должность и отдел через точку. */
export function directoryLine(p: Pick<DirectoryPerson, "title" | "department"> | null | undefined): string {
  if (!p) return "";
  return [p.title, p.department]
    .map((s) => s?.trim())
    .filter(Boolean)
    .join(" · ");
}

/** Контуры значков карточки и меню чипа (viewBox 0 0 16 16, обводка без заливки). */
export const CONTACT_ICON = {
  /** Человек: «карточка контакта». */
  person: "M8 8a2.75 2.75 0 1 0 0-5.5A2.75 2.75 0 0 0 8 8Zm-5 5.5c0-2.5 2.2-4.5 5-4.5s5 2 5 4.5",
  /** Двое: список рассылки, «развернуть». */
  people: "M6 7.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Zm-4.5 6c0-2.3 2-4 4.5-4s4.5 1.7 4.5 4M10.5 2.6a2.5 2.5 0 0 1 0 4.8m1.6 2.4c1.4.6 2.4 2 2.4 3.7",
  /** Два листа: «копировать». */
  copy: "M5.5 5.5h7v8h-7v-8Zm-2 5.5V2.5h7",
  /** Круговая стрелка: «обновить». */
  refresh: "M13 8a5 5 0 1 1-1.5-3.6M13 2.5v2.5h-2.5",
  /** Лупа: «найти письма». */
  search: "M7 11.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9Zm3.2-1.3L14 14",
  /** Трубка: телефон. */
  phone: "M5.5 2.5 3 3c-.3 4.9 5 10.3 10 10l.5-2.5-2.8-1.3-1.4 1.3C7.6 9.8 6.2 8.4 5.5 6.7l1.3-1.4L5.5 2.5Z",
  /** Стрелка назад. */
  back: "M7 3.5 2.5 8 7 12.5M2.5 8h11",
} as const;

/** Подпись вида записи для карточки; у обычного ящика подписи нет. */
export const KIND_LABEL: Record<DirectoryKind, string | null> = {
  mailbox: null,
  list: "Список рассылки",
  contact: "Личный контакт",
  room: "Переговорная",
  other: null,
};

/** Инициалы для кружка: первые буквы двух слов имени, иначе две буквы адреса. */
export function initials(name: string | null | undefined, email: string | null | undefined): string {
  const s = (name || email || "?").trim();
  const parts = s.split(/\s+/).filter(Boolean);
  return parts.length >= 2 ? (parts[0][0] + parts[1][0]).toUpperCase() : s.slice(0, 2).toUpperCase();
}

/** Ссылка tel: из номера в том виде, как его хранит справочник («+7 (495) 123-45-67 доб. 12»). */
export function telHref(phone: string): string {
  // Добавочный номер — после запятой (пауза набора), как принято в tel:.
  const m = /^(.*?)(?:\s*(?:доб\.?|ext\.?|x|#)\s*(\d+))?\s*$/i.exec(phone.trim());
  const main = (m?.[1] ?? phone).replace(/[^\d+]/g, "");
  const ext = m?.[2] ? `,${m[2]}` : "";
  return `tel:${main}${ext}`;
}

/** Скопировать текст в буфер обмена; false — не вышло ни через Clipboard API, ни через execCommand. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Запасной путь: скрытое поле и команда копирования (окну без фокуса Clipboard API отказывает).
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    const prev = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    ta.remove();
    prev?.focus();
    return ok;
  }
}

/**
 * Адреса, про которые справочник сказал «список рассылки», на всё время работы окна: чип такого
 * адреса можно развернуть в участников. Реактивное множество — чипы перерисуются, когда узнаем.
 */
const knownLists = new SvelteSet<string>();

export function rememberKinds(people: readonly DirectoryPerson[]) {
  for (const p of people) {
    const k = emailKey(p.email);
    if (!k) continue;
    if (p.kind === "list") knownLists.add(k);
    else knownLists.delete(k);
  }
}

export function isKnownList(email: string | null | undefined): boolean {
  return knownLists.has(emailKey(email));
}

/**
 * Отключённые возможности справочника на этом сервере. Сейчас одна: раскрытие списков рассылки
 * (`ExpandDL` в OWA JSON на сервере может быть не реализован — тогда кнопки «Развернуть» не нужны).
 */
const disabledCaps = new SvelteSet<string>();
const EXPAND_UNSUPPORTED_MARK = "не поддерживает раскрытие списков";

/** Можно ли разворачивать списки рассылки. */
export function expandSupported(): boolean {
  return !disabledCaps.has("expand");
}

/** Узнать возможности у бэкенда (один раз при старте окна). */
export async function loadDirectoryCaps(): Promise<void> {
  try {
    const caps = await api.directoryCapabilities();
    if (!caps.can_expand) disabledCaps.add("expand");
  } catch {
    // Старый бэкенд без команды: оставляем как есть, отказ придёт при первой попытке.
  }
}

/** Отказ раскрытия списка: если сервер сказал «не реализовано», кнопки больше не показываем. */
export function noteExpandError(message: string): void {
  if (message.includes(EXPAND_UNSUPPORTED_MARK)) disabledCaps.add("expand");
}
