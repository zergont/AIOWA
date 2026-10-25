/**
 * Письма в тёмных тонах (замечание 6 обкатки). Бэкенд уже заменил цвета письма на
 * `var(--aiowa-dN, исходный)` и прислал к ним тёмную палитру `dark_colors` (`mail_dark.rs`): без
 * палитры рамка показывает исходные цвета на белом листе, с ней — тёмный лист и перекрашенное
 * письмо. Здесь — когда письмо показывать тёмным и CSS тёмного листа для рамки (`BodyFrame`).
 * Печать (`print.ts`) палитру не подключает и всегда идёт в исходных цветах на белом.
 */
import { prefs } from "./settings.svelte";

// Системная тема Windows: нужна при теме окна «Как в Windows». Без matchMedia (тесты) — светлая.
const media = typeof window !== "undefined" && typeof window.matchMedia === "function" ? window.matchMedia("(prefers-color-scheme: dark)") : null;
const system = $state({ dark: media?.matches ?? false });
media?.addEventListener("change", (e) => {
  system.dark = e.matches;
});

/** Окно сейчас в тёмной теме: выбрана явно или «как в Windows» при тёмной Windows. */
export function windowIsDark(): boolean {
  return prefs.theme === "dark" || (prefs.theme === "system" && system.dark);
}

/** Письма в тёмной теме по умолчанию в тёмных тонах (настройка «Вид»); кнопка карточки меняет это для одного письма. */
export function darkMailByDefault(): boolean {
  return prefs.darkMail !== "light";
}

/**
 * Тёмный лист рамки: включается классом `dark` на корне документа рамки. Цвета — те же, что
 * `SHEET_*` в `mail_dark.rs`: от них бэкенд считает контраст перекрашенных цветов.
 */
export const DARK_SHEET_CSS = [
  "html.dark{color-scheme:dark}",
  "html.dark,html.dark body{background:#2b2b2b;color:#e6e6e6}",
  "html.dark blockquote{border-left-color:#4d4d4d;color:#adadad}",
  "html.dark a{color:#60cdff}",
].join("\n");

const HEX = /^#[0-9a-f]{6}(?:[0-9a-f]{2})?$/;

/**
 * Переменные палитры письма для тёмного листа. В `<style>` рамки попадают только цвета вида
 * `#rrggbb[aa]`: всё прочее пропускается, и там остаётся исходный цвет письма.
 */
export function darkPaletteCss(colors: readonly string[] | null | undefined): string {
  if (!colors?.length) return "";
  const vars = colors.flatMap((c, i) => (typeof c === "string" && HEX.test(c) ? [`--aiowa-d${i}:${c}`] : []));
  return vars.length ? `html.dark{${vars.join(";")}}` : "";
}

/** Значки переключателя на карточке: солнце — «показать как задумано», луна — «в тёмных тонах». */
export const SUN_ICON = "M8 5.25a2.75 2.75 0 1 0 0 5.5 2.75 2.75 0 0 0 0-5.5ZM8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1.06 1.06M11.54 11.54l1.06 1.06M3.4 12.6l1.06-1.06M11.54 4.46l1.06-1.06";
export const MOON_ICON = "M13.5 9.7A5.6 5.6 0 0 1 6.3 2.5a5.6 5.6 0 1 0 7.2 7.2Z";
