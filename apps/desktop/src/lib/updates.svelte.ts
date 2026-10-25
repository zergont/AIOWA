/**
 * Обновление приложения: общее состояние для вкладки «О программе» (AboutUpdates) и строки в
 * строке состояния (UpdateNotice). Источник — команды бэкенда `updates_*` (`src-tauri/src/updates.rs`)
 * и его события: `update-available` (фоновая проверка нашла версию), `update-progress` (ход
 * загрузки и установки). Команд плагина обновлений окно не зовёт.
 *
 * Перед установкой спрашивается App (setUpdateGuard): несохранённое письмо в композере —
 * тот же выбор, что при закрытии окна. На Windows установка завершает процесс: установщик ставит
 * новую версию и запускает её сам.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { errorMessage } from "./api";

export interface UpdateInfo {
  version: string;
  /** Дата выпуска, мс; null — не указана. */
  date_ms: number | null;
  /** Что нового (обычный текст, строки через \n); null — пусто. */
  notes: string | null;
}

interface UpdateCheck {
  current: string;
  available: UpdateInfo | null;
  checked_ms: number;
}

interface UpdatesStatus {
  current: string;
  auto_check: boolean;
  last_check_ms: number | null;
  available: UpdateInfo | null;
  installing: boolean;
  /** Настройки в базе учётной записи; без неё переключатель недоступен. */
  can_configure: boolean;
}

export interface UpdateProgress {
  /** download — загрузка; install — загружено, приложение закрывается и запускает установщик. */
  stage: "download" | "install";
  downloaded: number;
  /** Размер, если сервер его сообщил. */
  total: number | null;
}

const call = {
  status: () => invoke<UpdatesStatus>("updates_status"),
  check: () => invoke<UpdateCheck>("updates_check"),
  setAuto: (enabled: boolean) => invoke<void>("updates_set_auto", { enabled }),
  install: () => invoke<void>("updates_install"),
};

export const updates = $state({
  /** Установленная версия; пусто, пока бэкенд не ответил. */
  current: "",
  autoCheck: true,
  canConfigure: false,
  lastCheckMs: null as number | null,
  available: null as UpdateInfo | null,
  checking: false,
  checkError: null as string | null,
  installing: false,
  progress: null as UpdateProgress | null,
  installError: null as string | null,
  /** Версия, строку о которой в строке состояния скрыли (до следующей найденной версии). */
  dismissed: null as string | null,
});

/** Спросить App перед установкой: false — установку отменить (письмо не закрыто, идёт отправка). */
export type UpdateGuard = () => Promise<boolean>;
let guard: UpdateGuard | null = null;

export function setUpdateGuard(fn: UpdateGuard | null) {
  guard = fn;
}

let started = false;

/** Состояние и подписки — один раз на окно (зовёт строка состояния при монтировании). */
export async function initUpdates(): Promise<void> {
  if (started) return;
  started = true;
  try {
    await listen<UpdateInfo>("update-available", (e) => {
      updates.available = e.payload;
      updates.lastCheckMs = Date.now();
    });
    await listen<UpdateProgress>("update-progress", (e) => {
      updates.progress = e.payload;
    });
    await refreshUpdates();
  } catch (e) {
    // Бэкенд без модуля обновлений (старая сборка) — строки и вкладки просто нет данных.
    console.warn("updates:", errorMessage(e));
  }
}

/** Перечитать состояние без сети (открытие вкладки «О программе»). */
export async function refreshUpdates(): Promise<void> {
  const s = await call.status();
  updates.current = s.current;
  updates.autoCheck = s.auto_check;
  updates.canConfigure = s.can_configure;
  updates.lastCheckMs = s.last_check_ms;
  updates.available = s.available;
  updates.installing = updates.installing || s.installing;
}

/** Проверить сейчас (кнопка). Ошибка — в checkError. */
export async function checkUpdates(): Promise<void> {
  if (updates.checking || updates.installing) return;
  updates.checking = true;
  updates.checkError = null;
  try {
    const r = await call.check();
    updates.current = r.current;
    updates.available = r.available;
    updates.lastCheckMs = r.checked_ms;
  } catch (e) {
    updates.checkError = errorMessage(e);
  } finally {
    updates.checking = false;
  }
}

/** Переключатель «Проверять автоматически»; отказ возвращает прежнее значение. */
export async function setAutoCheck(enabled: boolean): Promise<string | null> {
  const before = updates.autoCheck;
  updates.autoCheck = enabled;
  try {
    await call.setAuto(enabled);
    return null;
  } catch (e) {
    updates.autoCheck = before;
    return errorMessage(e);
  }
}

/**
 * Установить найденное обновление: сначала вопрос App (композер), затем загрузка с ходом и запуск
 * установщика. На Windows при удаче ответа нет — процесс завершается. Ошибка — в installError.
 */
export async function installUpdate(): Promise<void> {
  if (updates.installing) return;
  updates.installError = null;
  if (guard && !(await guard())) return;
  updates.installing = true;
  updates.progress = { stage: "download", downloaded: 0, total: null };
  try {
    await call.install();
  } catch (e) {
    updates.installError = errorMessage(e);
    updates.installing = false;
    updates.progress = null;
  }
}

/** Доля загрузки 0…1, если размер известен; иначе null. */
export function progressFraction(p: UpdateProgress | null): number | null {
  if (!p) return null;
  if (p.stage === "install") return 1;
  if (p.total == null || p.total <= 0) return null;
  return Math.min(1, p.downloaded / p.total);
}

/** «3,2 из 9,8 МБ» или «3,2 МБ», пока размер неизвестен. */
export function progressText(p: UpdateProgress | null): string {
  if (!p) return "";
  if (p.stage === "install") return "Завершаю работу и запускаю установщик…";
  const mb = (n: number) => (n / 1024 / 1024).toLocaleString("ru-RU", { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  if (p.total != null && p.total > 0) return `Загружаю обновление: ${mb(p.downloaded)} из ${mb(p.total)} МБ`;
  return p.downloaded > 0 ? `Загружаю обновление: ${mb(p.downloaded)} МБ` : "Загружаю обновление…";
}
