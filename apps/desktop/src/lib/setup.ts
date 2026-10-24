/**
 * Мастер первого запуска: разбор адреса сервера и логина. Бэкенд нормализует хост так же
 * (без схемы, без /owa, только host[:port]); здесь — чтобы сразу показать, куда пойдёт вход.
 */
import { plural } from "./api";

export interface HostParse {
  /** host или host:port в нижнем регистре; null — адрес не годится. */
  host: string | null;
  error: string | null;
  /** Подсказка без ошибки (например, про http). */
  note: string | null;
}

const LABEL = /^[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,61}[\p{L}\p{N}])?$/u;
const IPV6 = /^\[[0-9a-f:.]+\]$/i;

/**
 * «https://mail.example.ru/owa/», «mail.example.ru», «mail.example.ru:8443» → «mail.example.ru[:порт]».
 * Схема, путь, запрос и якорь отбрасываются; порт проверяется.
 */
export function parseHost(input: string): HostParse {
  let s = input.trim();
  if (!s) return { host: null, error: "Укажите адрес сервера", note: null };
  let note: string | null = null;
  const scheme = /^([a-z][a-z0-9+.-]*):\/\//i.exec(s);
  if (scheme) {
    if (scheme[1].toLowerCase() === "http") note = "Подключение всегда идёт по HTTPS";
    else if (scheme[1].toLowerCase() !== "https") return { host: null, error: "Адрес сервера должен начинаться с https:// или быть просто именем сервера", note: null };
    s = s.slice(scheme[0].length);
  }
  s = s.split(/[/?#\\]/, 1)[0];
  if (s.includes("@")) return { host: null, error: "Это похоже на адрес почты, а не сервера: укажите имя сервера OWA, например mail.example.ru", note: null };
  if (/\s/.test(s)) return { host: null, error: "В имени сервера не должно быть пробелов", note: null };
  if (!s) return { host: null, error: "Укажите имя сервера, например mail.example.ru", note: null };

  let name = s;
  let port: string | null = null;
  const v6 = /^(\[[^\]]*\])(?::(\d*))?$/.exec(s);
  if (v6) {
    name = v6[1];
    port = v6[2] ?? null;
  } else {
    const i = s.lastIndexOf(":");
    if (i >= 0) {
      name = s.slice(0, i);
      port = s.slice(i + 1);
    }
  }
  if (port != null) {
    const n = Number(port);
    if (!/^\d{1,5}$/.test(port) || n < 1 || n > 65535) return { host: null, error: "Порт — число от 1 до 65535", note: null };
  }
  name = name.toLowerCase().replace(/\.$/, "");
  const ok = IPV6.test(name) || (name.length <= 253 && name.split(".").every((l) => LABEL.test(l)));
  if (!ok) return { host: null, error: "Имя сервера выглядит неправильно: только буквы, цифры, точки и дефисы", note: null };
  // Порт 443 — порт HTTPS по умолчанию: в адресе он не нужен.
  const host = port && port !== "443" ? `${name}:${Number(port)}` : name;
  return { host, error: null, note };
}

/** Адрес OWA, на который пойдёт вход. */
export function owaUrl(host: string): string {
  return `https://${host}/owa`;
}

/**
 * Проверка логина (как normalize_user бэкенда): пусто, пробелы, пустая часть до или после @ и \ —
 * ошибка; просто имя без домена — подсказка (вход всё равно можно попробовать).
 */
export function loginHint(user: string): { error: string | null; note: string | null } {
  const s = user.trim();
  if (!s) return { error: "Укажите логин", note: null };
  if (/\s/.test(s)) return { error: "В логине не должно быть пробелов", note: null };
  const partsOk = (sep: string) => {
    const i = s.indexOf(sep);
    if (i < 0) return true;
    const rest = s.slice(i + 1);
    return i > 0 && rest.length > 0 && !rest.includes(sep);
  };
  if (!partsOk("@") || !partsOk("\\")) return { error: "Логин указывается как user@domain или DOMAIN\\user", note: null };
  // user@domain (в том числе без точки: user@corp — UPN с NetBIOS-суффиксом) или DOMAIN\user.
  if (/^[^@\\]+@[^@\\]+$/.test(s) || /^[^@\\]+\\[^@\\]+$/.test(s)) return { error: null, note: null };
  return { error: null, note: "Обычно логин — это адрес почты (ivanov@example.ru) или ДОМЕН\\логин" };
}

/** Варианты глубины загрузки тел писем: дней; 0 — все. */
export const BODY_DAYS_CHOICES: { days: number; label: string }[] = [
  { days: 30, label: "30 дней" },
  { days: 90, label: "90 дней" },
  { days: 365, label: "Год" },
  { days: 0, label: "Всё" },
];

/** «90 дней», «1 год», «все письма». */
export function bodyDaysText(days: number): string {
  if (days <= 0) return "все письма";
  if (days % 365 === 0) {
    const y = days / 365;
    return `${y} ${plural(y, "год", "года", "лет")}`;
  }
  return `${days} ${plural(days, "день", "дня", "дней")}`;
}
