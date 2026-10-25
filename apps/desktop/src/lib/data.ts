/**
 * Базы на диске: вкладка «Данные» настроек (все базы `%LOCALAPPDATA%\AIOWA\*\*\mail.db`,
 * «Открыть папку», удаление лишней) и размер текущей базы в строке состояния.
 */
import { invoke } from "@tauri-apps/api/core";

/** База на диске (`data_stores`). */
export interface StoreInfo {
  /** Полный путь к mail.db. */
  path: string;
  /** Каталог базы: удаляется целиком. */
  dir: string;
  /** Сервер; у остатка удаления — имя каталога сервера. */
  host: string | null;
  /** Логины учётных записей базы (обычно один); у остатка удаления — имя каталога ящика. */
  logins: string[];
  /** Адрес ящика, если записан (базы новых версий). */
  email: string | null;
  /** В базе записан идентификатор ящика (MailboxGuid). */
  has_guid: boolean;
  items: number;
  last_sync_ms: number | null;
  /** База с журналами -wal и -shm, байт. */
  db_bytes: number;
  /** Кэш вложений этой базы, байт. */
  cache_bytes: number;
  total_bytes: number;
  /** С этой базой работает приложение: её не удалить. */
  current: boolean;
  /**
   * Остаток неполного удаления (`<ящик>.deleting-<время>`): не база, а занятое место. Строка
   * «Остаток удалённой базы», удаляется повторно; писем и синхронизации у неё нет.
   */
  leftover: boolean;
  /** База не читается или это остаток удаления: текст для окна. */
  error: string | null;
}

/** Место под текущую базу (`data_usage`). */
export interface DiskUsage {
  db_bytes: number;
  cache_bytes: number;
}

export const dataApi = {
  stores: () => invoke<StoreInfo[]>("data_stores"),
  usage: () => invoke<DiskUsage>("data_usage"),
  openDir: (path: string) => invoke<void>("data_store_open_dir", { path }),
  /** Удалить не текущую базу (каталог целиком); ответ — освобождено байт. */
  remove: (path: string) => invoke<number>("data_store_delete", { path }),
};

/** «812 КБ», «450 МБ», «1,1 ГБ». */
export function bytesText(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n < 1024) return `${n} Б`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} КБ`;
  const mb = n / 1024 / 1024;
  if (mb < 1024) return `${mb.toLocaleString("ru-RU", { maximumFractionDigits: mb < 10 ? 1 : 0 })} МБ`;
  return `${(mb / 1024).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} ГБ`;
}

const dayTime = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" });
const timeOnly = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });

/** «сегодня в 14:32», «16 окт. 2026 г., 14:32», «ещё не синхронизировалась». */
export function syncedText(ms: number | null | undefined): string {
  if (ms == null) return "ещё не синхронизировалась";
  const d = new Date(ms);
  if (d.toDateString() === new Date().toDateString()) return `сегодня в ${timeOnly.format(d)}`;
  return dayTime.format(d);
}

/** Кто владелец базы: адрес ящика, иначе логины. */
export function storeOwner(s: Pick<StoreInfo, "email" | "logins">): string {
  return s.email ?? (s.logins.length ? s.logins.join(", ") : "учётной записи нет");
}
