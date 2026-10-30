/**
 * Настройки окна, которые нужны сразу в нескольких местах: задержка автопометки (карточка письма),
 * интервал страховочной синхронизации (App), тосты (строка состояния) и тема (корень документа).
 * Источник — `settings_get` бэкенда; здесь лежит реактивная копия, которую ведут App и окно настроек.
 */
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, errorMessage, type DarkMail, type ExternalImagesMode, type Settings, type SettingsPatch, type Theme } from "./api";

/** Значения по умолчанию — те же, что у бэкенда, пока ключа в базе нет. */
export const AUTO_READ_DEFAULT_MS = 1500;
/** «Сразу»: 0 значит «выключено», поэтому мгновенная пометка — минимальная ненулевая задержка. */
export const AUTO_READ_INSTANT_MS = 1;
export const SYNC_INTERVAL_DEFAULT_MIN = 2;
/** При открытом канале уведомлений изменения приходят от сервера сразу: таймер не чаще раза в 10 минут. */
export const SYNC_INTERVAL_LIVE_MIN = 10;

export const prefs = $state({
  /** 0 — автопометка выключена. */
  autoReadMs: AUTO_READ_DEFAULT_MS,
  syncIntervalMin: SYNC_INTERVAL_DEFAULT_MIN,
  toasts: true,
  theme: "system" as Theme,
  /** Письма в тёмной теме (`darkmail.svelte.ts`): в тёмных тонах или на светлом листе. */
  darkMail: "dark" as DarkMail,
  /** Внешние картинки писем (п. 18 обкатки): по кнопке или не загружать совсем. */
  externalImages: "ask" as ExternalImagesMode,
  /** Отправители, чьи внешние картинки показываются сразу (адреса в нижнем регистре). */
  trustedImageSenders: [] as string[],
});

/** Период таймера синхронизации, мс: при живом канале — не меньше 10 минут. */
export function syncEveryMs(live: boolean, intervalMin: number): number {
  const min = Math.max(1, Math.round(intervalMin) || SYNC_INTERVAL_DEFAULT_MIN);
  return (live ? Math.max(SYNC_INTERVAL_LIVE_MIN, min) : min) * 60_000;
}

// --- тема ------------------------------------------------------------------------------------
// Токены (tokens.css) переключаются атрибутом data-theme на корне: "light" и "dark" — явный выбор,
// без атрибута — как в Windows. Выбор дублируется в localStorage, чтобы main.ts поставил его до
// первой отрисовки (settings_get приходит позже, и окно не должно мигнуть системной темой). Рядом
// лежит выбор «письма в тёмной теме»: иначе письмо, открытое до ответа settings_get, на миг
// показалось бы в тёмных тонах (значение по умолчанию) и тут же перекрасилось на светлый лист.

const THEME_KEY = "aiowa.theme";
const DARK_MAIL_KEY = "aiowa.darkMail";

function isTheme(v: unknown): v is Theme {
  return v === "system" || v === "light" || v === "dark";
}

function isDarkMail(v: unknown): v is DarkMail {
  return v === "dark" || v === "light";
}

function setThemeAttr(theme: Theme) {
  const root = document.documentElement;
  const explicit = theme === "light" || theme === "dark" ? theme : null;
  if (explicit) root.dataset.theme = explicit;
  else delete root.dataset.theme;
  // Системная рамка окна (заголовок с кнопками) — тоже в выбранной теме; null — как в Windows.
  // Без Tauri (vite-превью) вызова нет — ошибку глушим.
  try {
    getCurrentWindow()
      .setTheme(explicit)
      .catch(() => {});
  } catch {
    /* окна Tauri нет */
  }
}

/** Тема и выбор «письма в тёмной теме» из прошлого запуска — до монтирования приложения. */
export function restoreTheme() {
  try {
    const t = localStorage.getItem(THEME_KEY);
    if (isTheme(t)) {
      setThemeAttr(t);
      // Письма (тёмные тона) тоже сразу по прошлой теме, а не по системной до ответа settings_get.
      prefs.theme = t;
    }
    const d = localStorage.getItem(DARK_MAIL_KEY);
    if (isDarkMail(d)) prefs.darkMail = d;
  } catch {
    /* хранилище недоступно — остаются системная тема и тёмные письма до ответа settings_get */
  }
}

export function applyTheme(theme: Theme) {
  prefs.theme = theme;
  setThemeAttr(theme);
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    /* не страшно: на следующем старте тему поставит settings_get */
  }
}

/** Письма в тёмной теме: в общую копию и в localStorage — для restoreTheme на следующем старте. */
function applyDarkMail(darkMail: DarkMail) {
  prefs.darkMail = darkMail;
  try {
    localStorage.setItem(DARK_MAIL_KEY, darkMail);
  } catch {
    /* не страшно: на следующем старте выбор поставит settings_get */
  }
}

/** Перенести в общую копию то, что пришло от бэкенда (целиком или изменённой частью). */
export function adoptSettings(s: SettingsPatch & { trusted_image_senders?: string[] }) {
  if (s.auto_read_ms != null) prefs.autoReadMs = s.auto_read_ms;
  if (s.sync_interval_min != null) prefs.syncIntervalMin = s.sync_interval_min;
  if (s.toasts != null) prefs.toasts = s.toasts;
  if (isTheme(s.theme)) applyTheme(s.theme);
  if (isDarkMail(s.dark_mail)) applyDarkMail(s.dark_mail);
  if (s.external_images === "ask" || s.external_images === "never") prefs.externalImages = s.external_images;
  if (Array.isArray(s.trusted_image_senders)) prefs.trustedImageSenders = s.trusted_image_senders;
}

/** Отправитель в список доверенных для внешних картинок или из него; новая копия списка — сразу. */
export async function trustImageSender(email: string, trusted: boolean): Promise<void> {
  const fresh = await api.settingsTrustImageSender(email, trusted);
  adoptSettings(fresh);
}

/** Настройки на старте окна; старый бэкенд без команды — остаются значения по умолчанию. */
export async function loadSettings(): Promise<Settings | null> {
  try {
    const s = await api.settingsGet();
    adoptSettings(s);
    return s;
  } catch (e) {
    console.warn("settings_get:", errorMessage(e));
    return null;
  }
}

/**
 * Записать изменение; свежие настройки от бэкенда, если он их вернул. Отказ — исключение.
 * Из ответа берутся только изменённые поля: ответ на предыдущее изменение, пришедший позже
 * следующего, не должен на миг вернуть уже переключённое (тему, задержку).
 */
export async function saveSettings(patch: SettingsPatch): Promise<Settings | null> {
  const r = await api.settingsSet(patch);
  const fresh = r && typeof r === "object" ? (r as Settings) : null;
  const keys = Object.keys(patch) as (keyof SettingsPatch)[];
  adoptSettings(fresh ? (Object.fromEntries(keys.map((k) => [k, fresh[k]])) as SettingsPatch) : patch);
  return fresh;
}
