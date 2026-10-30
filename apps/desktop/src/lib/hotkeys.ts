/**
 * Горячие клавиши окна почты: одна таблица на всё приложение.
 *
 * Глобальные клавиши (GLOBAL_HOTKEYS) ловит один обработчик — App.onKeydown: он зовёт matchHotkey
 * и выполняет действие найденной клавиши, а клавиши календаря передаёт в CalendarView.hotkey.
 * Перезагрузку и печать окна целиком (prevent "always") ещё в фазе захвата гасит preventBrowserKeys:
 * компонент, остановивший всплытие, их не пропустит.
 * Клавиши, которые компоненты ловят сами (стрелки и Delete в списке, Ctrl+Enter в композере, Esc
 * окон), записаны справочно в LOCAL_HOTKEYS: они зависят от фокуса и состояния внутри компонента,
 * и переносить их в общий обработчик рискованно. По обеим таблицам собраны подсказки title
 * (keysOf, hint) и раздел «Горячие клавиши» в README.
 *
 * Слои — модальные окна, поповеры подтверждения, выбор папки, контекстные меню. Пока на экране
 * есть хоть один, глобальные клавиши действий не срабатывают (гасятся только перезагрузка и печать
 * окна целиком), а Esc, Tab и стрелки достаются верхнему слою (isTopLayer), а не окнам под ним.
 */

/** Где действует клавиша: везде или в одном разделе окна. */
export type HotkeyWhere = "everywhere" | "mail" | "calendar" | "people";

/** Раздел окна на экране. */
export type Section = "mail" | "calendar" | "people";

/** Как распознать нажатие. */
interface KeyMatch {
  /** e.code: по коду клавиши, а не по символу — в русской раскладке e.key даёт кириллицу. */
  readonly code?: string;
  /** e.key в нижнем регистре: подходит и он (латинская буква в другой раскладке). */
  readonly key?: string;
  /** Ctrl или ⌘, Alt при этом не нажат; не задан — модификаторы не проверяются вовсе. */
  readonly ctrl?: boolean;
  /** Shift; "any" — не важен. По умолчанию — не нажат. */
  readonly shift?: boolean | "any";
}

interface HotkeyInfo {
  readonly id: string;
  /** Сочетание для подсказок и справки. */
  readonly keys: string;
  readonly where: HotkeyWhere;
  /** Где именно — для справки: «поле поиска», «список бесед», «композер». */
  readonly place?: string;
  /** Действует, когда фокус в поле ввода (input, textarea, select). */
  readonly inInput: boolean;
  /** Действует, когда фокус в редакторе письма (contenteditable) или во фрейме письма. */
  readonly inEditor: boolean;
  readonly description: string;
}

interface GlobalSpec extends HotkeyInfo {
  readonly match: KeyMatch;
  /**
   * Когда гасить действие по умолчанию:
   * "always" — всегда, даже под модальным окном и в поле ввода (перезагрузка, печать окна целиком);
   * "match" — когда клавиша допустима при этом фокусе, даже если делать нечего (не тот раздел, нет письма);
   * "action" — только если действие выполнено: решает App.
   */
  readonly prevent: "always" | "match" | "action";
}

interface LocalSpec extends HotkeyInfo {
  /** Компонент (или несколько), который ловит клавишу сам. */
  readonly owner: string;
}

// --- глобальные клавиши: App.onKeydown -------------------------------------------------------
// Порядок строк — порядок справки. Одно нажатие может подходить нескольким строкам (Esc в поле
// поиска и Esc в календаре): берётся первая, что действует в этом разделе.

const GLOBAL = [
  {
    id: "settings",
    keys: "Ctrl+,",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "Comma", key: ",", ctrl: true },
    prevent: "match",
    description: "Открыть настройки (набранное письмо остаётся под окном); в открытых настройках — закрыть их",
  },
  {
    id: "section-mail",
    keys: "Ctrl+Shift+1",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "Digit1", ctrl: true, shift: true },
    prevent: "match",
    description: "Раздел «Почта»",
  },
  {
    id: "section-calendar",
    keys: "Ctrl+Shift+2",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "Digit2", ctrl: true, shift: true },
    prevent: "match",
    description: "Раздел «Календарь»",
  },
  {
    id: "section-people",
    keys: "Ctrl+Shift+3",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "Digit3", ctrl: true, shift: true },
    prevent: "match",
    description: "Раздел «Люди»: личные контакты и справочник",
  },
  {
    id: "search",
    keys: "Ctrl+K",
    where: "everywhere",
    inInput: true,
    inEditor: false,
    match: { code: "KeyK", key: "k", ctrl: true },
    prevent: "match",
    description: "Поиск в почте (из календаря — с переходом в почту); в редакторе письма Ctrl+K — ссылка",
  },
  {
    id: "new-mail",
    keys: "Ctrl+N",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "KeyN", key: "n", ctrl: true },
    prevent: "match",
    description: "Новое письмо (из календаря — с переходом в почту)",
  },
  {
    id: "undo",
    keys: "Ctrl+Z",
    where: "everywhere",
    inInput: false,
    inEditor: false,
    match: { code: "KeyZ", key: "z", ctrl: true },
    prevent: "action",
    description: "Отменить последнее удаление, перенос или архив, пока видна строка «Отменить»; в полях ввода — обычная отмена правки",
  },
  {
    id: "reply",
    keys: "Ctrl+R",
    where: "mail",
    inInput: false,
    inEditor: false,
    match: { code: "KeyR", key: "r", ctrl: true },
    prevent: "always",
    description: "Ответить на верхнее развёрнутое письмо; в остальных местах только гасится (перезагрузка окна)",
  },
  {
    id: "reply-all",
    keys: "Ctrl+Shift+R",
    where: "mail",
    inInput: false,
    inEditor: false,
    match: { code: "KeyR", key: "r", ctrl: true, shift: true },
    prevent: "always",
    description: "Ответить всем на верхнее развёрнутое письмо; в остальных местах только гасится",
  },
  {
    id: "forward",
    keys: "Ctrl+Shift+F",
    where: "mail",
    inInput: false,
    inEditor: false,
    match: { code: "KeyF", key: "f", ctrl: true, shift: true },
    prevent: "match",
    description: "Переслать верхнее развёрнутое письмо",
  },
  {
    id: "print",
    keys: "Ctrl+P",
    where: "mail",
    inInput: false,
    inEditor: false,
    match: { code: "KeyP", key: "p", ctrl: true },
    prevent: "always",
    description: "Напечатать верхнее развёрнутое письмо; печать окна целиком гасится везде",
  },
  {
    id: "archive",
    keys: "E",
    where: "mail",
    inInput: false,
    inEditor: false,
    match: { code: "KeyE", ctrl: false },
    prevent: "action",
    description: "В архив выбранные беседы (в списке бесед клавишу ловит сам список)",
  },
  {
    id: "search-clear",
    keys: "Esc",
    where: "mail",
    place: "поле поиска",
    inInput: true,
    inEditor: false,
    match: { key: "escape" },
    prevent: "action",
    description: "Очистить поиск и выйти из поля",
  },
  {
    id: "reload",
    keys: "F5",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { key: "f5" },
    prevent: "always",
    description: "Гасится, как и Ctrl+R: перезагрузка окна потеряла бы набранное письмо",
  },
  {
    id: "print-window",
    keys: "Ctrl+Shift+P",
    where: "everywhere",
    inInput: true,
    inEditor: true,
    match: { code: "KeyP", key: "p", ctrl: true, shift: true },
    prevent: "always",
    description: "Гасится: печать окна целиком (со списками и панелями) не нужна",
  },
  // Календарь: App передаёт их в CalendarView.hotkey, пока раздел на экране.
  {
    id: "cal-prev",
    keys: "←",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { key: "arrowleft", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Предыдущий период",
  },
  {
    id: "cal-next",
    keys: "→",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { key: "arrowright", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Следующий период",
  },
  {
    id: "cal-today",
    keys: "T",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyT", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Сегодня",
  },
  {
    id: "cal-day",
    keys: "D",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyD", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Вид «День»",
  },
  {
    id: "cal-work-week",
    keys: "Shift+W",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyW", ctrl: false, shift: true },
    prevent: "action",
    description: "Вид «Рабочая неделя»",
  },
  {
    id: "cal-week",
    keys: "W",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyW", ctrl: false },
    prevent: "action",
    description: "Вид «Неделя»",
  },
  {
    id: "cal-month",
    keys: "M",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyM", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Вид «Месяц»",
  },
  {
    id: "cal-new",
    keys: "N",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { code: "KeyN", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Новая встреча",
  },
  {
    id: "cal-deselect",
    keys: "Esc",
    where: "calendar",
    inInput: false,
    inEditor: false,
    match: { key: "escape", ctrl: false, shift: "any" },
    prevent: "action",
    description: "Закрыть карточку выбранного события",
  },
] as const satisfies readonly GlobalSpec[];

export type GlobalHotkeyId = (typeof GLOBAL)[number]["id"];
/** Клавиши календаря: их выполняет CalendarView.hotkey. */
export type CalendarHotkeyId = Extract<GlobalHotkeyId, `cal-${string}`>;

export const GLOBAL_HOTKEYS: readonly (GlobalSpec & { readonly id: GlobalHotkeyId })[] = GLOBAL;

// --- справочно: клавиши, которые компоненты ловят сами ----------------------------------------

const LOCAL = [
  // Список бесед (фокус на списке).
  { id: "list-next", keys: "↓ / J", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Следующая беседа" },
  { id: "list-prev", keys: "↑ / K", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Предыдущая беседа" },
  { id: "list-extend", keys: "Shift+↓ / Shift+↑", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Расширить выбор на соседнюю беседу" },
  { id: "list-edge", keys: "Home / End", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Первая или последняя беседа (с Shift — выбор до неё)" },
  { id: "list-select-all", keys: "Ctrl+A", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Выбрать все загруженные беседы" },
  { id: "list-open", keys: "Enter", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Открыть беседу; в «Черновиках» — черновик в композере" },
  { id: "list-collapse", keys: "Esc", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Снять множественный выбор (остаётся беседа под курсором)" },
  { id: "list-delete", keys: "Delete", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Удалить выбранные беседы в «Удалённые»; в «Удалённых» — навсегда, с подтверждением" },
  { id: "list-delete-forever", keys: "Shift+Delete", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Удалить выбранные беседы навсегда, с подтверждением" },
  { id: "list-menu", keys: "Menu / Shift+F10", where: "mail", place: "список бесед", inInput: false, inEditor: false, owner: "ConversationList", description: "Меню действий с выбранными беседами" },
  { id: "search-open", keys: "Enter", where: "mail", place: "результаты поиска", inInput: false, inEditor: false, owner: "SearchResults", description: "Открыть беседу найденного письма" },
  // Дерево папок (фокус на строке папки).
  { id: "tree-open", keys: "Enter", where: "mail", place: "дерево папок", inInput: false, inEditor: false, owner: "FolderTree", description: "Открыть папку" },
  { id: "tree-rename", keys: "F2", where: "mail", place: "дерево папок", inInput: false, inEditor: false, owner: "FolderTree", description: "Переименовать папку" },
  { id: "tree-delete", keys: "Delete", where: "mail", place: "дерево папок", inInput: false, inEditor: false, owner: "FolderTree", description: "Удалить папку (в «Удалённые», из них — навсегда), с подтверждением" },
  { id: "tree-menu", keys: "Menu / Shift+F10", where: "mail", place: "дерево папок", inInput: false, inEditor: false, owner: "FolderTree", description: "Меню папки" },
  { id: "tree-edit", keys: "Enter / Esc", where: "mail", place: "поле имени папки", inInput: true, inEditor: false, owner: "FolderTree", description: "Сохранить или отменить имя новой или переименованной папки" },
  // Область чтения.
  { id: "card-toggle", keys: "Enter / Пробел", where: "mail", place: "заголовок письма", inInput: false, inEditor: false, owner: "MessageCard", description: "Развернуть или свернуть письмо" },
  { id: "frame-print", keys: "Ctrl+P", where: "everywhere", place: "текст письма (фрейм)", inInput: false, inEditor: true, owner: "BodyFrame", description: "Напечатать это письмо, а не окно целиком; в описании события и в исходном письме под композером — только гасится, как F5 и Ctrl+R" },
  // Композер.
  { id: "composer-send", keys: "Ctrl+Enter", where: "mail", place: "композер", inInput: true, inEditor: true, owner: "Composer", description: "Отправить" },
  { id: "composer-save", keys: "Ctrl+S", where: "mail", place: "композер", inInput: true, inEditor: true, owner: "Composer", description: "Сохранить черновик на сервере" },
  { id: "composer-cancel", keys: "Esc", where: "mail", place: "композер", inInput: true, inEditor: true, owner: "Composer", description: "Отменить письмо; с правками — вопрос: сохранить, закрыть, удалить или вернуться" },
  { id: "editor-bold", keys: "Ctrl+B", where: "everywhere", place: "редактор письма и подписи", inInput: false, inEditor: true, owner: "Editor", description: "Жирный" },
  { id: "editor-italic", keys: "Ctrl+I", where: "everywhere", place: "редактор письма и подписи", inInput: false, inEditor: true, owner: "Editor", description: "Курсив" },
  { id: "editor-underline", keys: "Ctrl+U", where: "everywhere", place: "редактор письма и подписи", inInput: false, inEditor: true, owner: "Editor", description: "Подчёркнутый" },
  { id: "editor-link", keys: "Ctrl+K", where: "everywhere", place: "редактор письма и подписи", inInput: false, inEditor: true, owner: "Editor", description: "Вставить ссылку (вместо поиска по почте)" },
  { id: "editor-link-field", keys: "Enter / Esc", where: "everywhere", place: "поле адреса ссылки", inInput: true, inEditor: false, owner: "Editor", description: "Вставить ссылку или отменить" },
  // Поле адресатов (композер, редактор встречи).
  { id: "rcpt-add", keys: "Enter / Tab / , / ;", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "Добавить адрес; при открытых подсказках — выбранный вариант" },
  { id: "rcpt-suggest", keys: "↑ / ↓", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "Выбор в подсказках" },
  { id: "rcpt-expand", keys: "Shift+Enter", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "Развернуть выбранный в подсказках список рассылки" },
  { id: "rcpt-backspace", keys: "Backspace", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "В пустом поле — убрать последнего адресата" },
  { id: "rcpt-to-chips", keys: "←", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "В пустом поле — к чипам адресатов" },
  { id: "rcpt-chip-move", keys: "← / → / Home / End", where: "everywhere", place: "чип адресата", inInput: false, inEditor: false, owner: "RecipientsInput", description: "К соседнему, первому или последнему чипу (End — обратно в поле)" },
  { id: "rcpt-chip-remove", keys: "Delete / Backspace", where: "everywhere", place: "чип адресата", inInput: false, inEditor: false, owner: "RecipientsInput", description: "Убрать адресата" },
  { id: "rcpt-chip-card", keys: "Enter / Пробел", where: "everywhere", place: "чип адресата", inInput: false, inEditor: false, owner: "RecipientsInput", description: "Карточка контакта" },
  { id: "rcpt-chip-copy", keys: "Ctrl+C", where: "everywhere", place: "чип адресата", inInput: false, inEditor: false, owner: "RecipientsInput", description: "Копировать адрес" },
  { id: "rcpt-close", keys: "Esc", where: "everywhere", place: "поле адресатов", inInput: true, inEditor: false, owner: "RecipientsInput", description: "Закрыть подсказки (без них Esc достаётся окну: отмена письма или встречи)" },
  // Окна и всплывающие окна: клавиши достаются только верхнему из них.
  { id: "event-save", keys: "Ctrl+Enter", where: "calendar", place: "редактор встречи", inInput: true, inEditor: true, owner: "EventEditor", description: "Сохранить (встречу с участниками — разослать приглашения)" },
  { id: "event-cancel", keys: "Esc", where: "calendar", place: "редактор встречи", inInput: true, inEditor: true, owner: "EventEditor", description: "Отменить; с правками — вопрос" },
  { id: "settings-close", keys: "Esc", where: "everywhere", place: "окно настроек", inInput: true, inEditor: true, owner: "Settings", description: "Закрыть настройки (сначала — открытые в них вопрос или смену пароля)" },
  { id: "settings-tabs", keys: "↑ / ↓ / Home / End", where: "everywhere", place: "вкладки настроек", inInput: false, inEditor: false, owner: "Settings", description: "Переключить вкладку" },
  { id: "signature-close", keys: "Esc", where: "everywhere", place: "окно подписи", inInput: true, inEditor: true, owner: "SignatureDialog", description: "Закрыть без сохранения" },
  { id: "preview-prev", keys: "←", where: "mail", place: "предпросмотр вложения", inInput: false, inEditor: false, owner: "AttachmentPreview", description: "Предыдущее вложение письма" },
  { id: "preview-next", keys: "→", where: "mail", place: "предпросмотр вложения", inInput: false, inEditor: false, owner: "AttachmentPreview", description: "Следующее вложение письма" },
  { id: "preview-close", keys: "Esc", where: "mail", place: "предпросмотр вложения", inInput: false, inEditor: false, owner: "AttachmentPreview", description: "Закрыть предпросмотр (пока фокус внутри PDF — кнопкой)" },
  { id: "contact-close", keys: "Esc", where: "everywhere", place: "карточка контакта", inInput: false, inEditor: false, owner: "ContactCard", description: "Закрыть карточку" },
  { id: "contact-back", keys: "Backspace", where: "everywhere", place: "карточка контакта", inInput: false, inEditor: false, owner: "ContactCard", description: "Назад к предыдущей карточке (после перехода к участнику списка)" },
  { id: "dialog-tab", keys: "Tab / Shift+Tab", where: "everywhere", place: "модальные окна", inInput: true, inEditor: true, owner: "Settings, SignatureDialog, AttachmentPreview, EventEditor, ContactCard, ConfirmPopover, ChoicePopover", description: "По кругу внутри окна, наружу фокус не уходит" },
  { id: "popover-cancel", keys: "Esc", where: "everywhere", place: "вопрос-подтверждение", inInput: false, inEditor: false, owner: "ConfirmPopover, ChoicePopover", description: "Отмена" },
  { id: "picker-move", keys: "↑ / ↓", where: "mail", place: "выбор папки", inInput: true, inEditor: false, owner: "FolderPicker", description: "Выбрать папку" },
  { id: "picker-pick", keys: "Enter", where: "mail", place: "выбор папки", inInput: true, inEditor: false, owner: "FolderPicker", description: "Переместить в выбранную папку" },
  { id: "picker-close", keys: "Esc", where: "mail", place: "выбор папки", inInput: true, inEditor: false, owner: "FolderPicker", description: "Закрыть без переноса" },
  { id: "menu-move", keys: "↑ / ↓ / Home / End", where: "everywhere", place: "контекстное меню", inInput: false, inEditor: false, owner: "ContextMenu", description: "По пунктам меню" },
  { id: "menu-close", keys: "Esc / Tab", where: "everywhere", place: "контекстное меню", inInput: false, inEditor: false, owner: "ContextMenu", description: "Закрыть меню" },
] as const satisfies readonly LocalSpec[];

export type LocalHotkeyId = (typeof LOCAL)[number]["id"];
export type HotkeyId = GlobalHotkeyId | LocalHotkeyId;

export const LOCAL_HOTKEYS: readonly (LocalSpec & { readonly id: LocalHotkeyId })[] = LOCAL;

const BY_ID = new Map<string, HotkeyInfo>([...GLOBAL_HOTKEYS, ...LOCAL_HOTKEYS].map((h): [string, HotkeyInfo] => [h.id, h]));

/** Сочетание из таблицы: для подсказок title, чтобы они не расходились с клавишами. */
export function keysOf(id: HotkeyId): string {
  return BY_ID.get(id)?.keys ?? "";
}

/** Подсказка вида «Ответить (Ctrl+R)». */
export function hint(label: string, id: HotkeyId): string {
  return `${label} (${keysOf(id)})`;
}

// --- сопоставление ------------------------------------------------------------------------

function keyMatches(m: KeyMatch, e: KeyboardEvent): boolean {
  // У событий автозаполнения Chromium поля key может не быть.
  const key = typeof e.key === "string" ? e.key.toLowerCase() : "";
  if (!((m.code !== undefined && e.code === m.code) || (m.key !== undefined && key === m.key))) return false;
  if (m.ctrl === undefined) return true;
  if ((e.ctrlKey || e.metaKey) !== m.ctrl || e.altKey) return false;
  const shift = m.shift ?? false;
  return shift === "any" || e.shiftKey === shift;
}

/** Корень композера (Composer.svelte): класс в окне один. */
const COMPOSER = "section.composer";

/**
 * Где фокус: в поле ввода, в редакторе (contenteditable, фрейм письма) или ни там ни там.
 * Остальное внутри композера — чип адресата, кнопки, редактор, отключённый на время отправки, —
 * тоже набор письма, как поле ввода: E, Ctrl+R, Ctrl+Shift+F, Ctrl+Z не трогают беседу под ним.
 * Проверка — после редактора: Ctrl+K в нём остаётся ссылкой, а не поиском.
 */
export function focusKind(): "input" | "editor" | null {
  const el = document.activeElement;
  if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) return "input";
  if (el instanceof HTMLIFrameElement || (el instanceof HTMLElement && el.isContentEditable)) return "editor";
  if (el?.closest(COMPOSER)) return "input";
  return null;
}

/**
 * Глобальная клавиша для нажатия при разделе `section` на экране; null — клавиша не наша или сейчас
 * не действует: открыт слой (окно, поповер, меню), фокус в поле ввода или редакторе, не тот раздел.
 * Действие по умолчанию для prevent "always" и "match" гасит сама, для "action" — вызывающий,
 * если действие выполнено.
 */
export function matchHotkey(e: KeyboardEvent, section: Section): GlobalHotkeyId | null {
  let blocked: boolean | null = null;
  for (const h of GLOBAL_HOTKEYS) {
    if (!keyMatches(h.match, e)) continue;
    if (h.prevent === "always") e.preventDefault();
    // Под слоем — ни одного действия: свои клавиши (Esc, Tab, стрелки) окна ловят сами.
    if (blocked === null) blocked = layerOpen();
    if (blocked) continue;
    const focus = focusKind();
    if ((focus === "input" && !h.inInput) || (focus === "editor" && !h.inEditor)) continue;
    if (h.prevent === "match") e.preventDefault();
    if (h.where !== "everywhere" && h.where !== section) continue;
    return h.id;
  }
  return null;
}

/**
 * Для фазы захвата окна (App: onkeydowncapture): гасит клавиши с prevent "always" — F5, Ctrl+R,
 * Ctrl+P, Ctrl+Shift+P — ещё до компонентов. Компонент может остановить всплытие (поле имени папки
 * в дереве), и до App.onKeydown нажатие тогда не дойдёт, а окно перезагрузится или уйдёт в печать.
 * Только preventDefault: действие (ответить, напечатать письмо) выполняет App.onKeydown, и он на
 * defaultPrevented у этих клавиш не смотрит. Клавиши "match" и "action" не трогает: по
 * defaultPrevented App узнаёт, что нажатие уже обработал компонент (E в списке бесед).
 */
export function preventBrowserKeys(e: KeyboardEvent): void {
  if (GLOBAL_HOTKEYS.some((h) => h.prevent === "always" && keyMatches(h.match, e))) e.preventDefault();
}

// --- слои: модальные окна, поповеры, выбор папки, меню -------------------------------------------

/** Открытые слои в порядке появления: последний видимый — верхний. */
const layers: HTMLElement[] = [];

/** Модальное окно, которое не зарегистрировалось слоем, всё равно глушит глобальные клавиши. */
const MODAL = '[aria-modal="true"]';

/**
 * Вложение Svelte для корня слоя (`{@attach layer}`): слой регистрируется при появлении и снимается
 * при исчезновении. Окно, открытое из другого окна, появляется позже него — порядок регистрации и
 * есть порядок наложения.
 */
export function layer(node: HTMLElement): () => void {
  layers.push(node);
  return () => {
    const i = layers.lastIndexOf(node);
    if (i >= 0) layers.splice(i, 1);
  };
}

/** Слой на экране: окна спрятанного раздела (display: none у предка) клавиш не держат. */
function shown(el: Element): boolean {
  return el.isConnected && el.getClientRects().length > 0;
}

/** На экране есть слой: глобальные клавиши действий и клавиши списка, композера, дерева молчат. */
export function layerOpen(): boolean {
  return layers.some(shown) || Array.from(document.querySelectorAll(MODAL)).some(shown);
}

/**
 * Слой `el` — верхний из видимых: его клавиши (Esc, Tab, стрелки, Ctrl+Enter) срабатывают, а у окон
 * под ним — нет, даже если они слушают всё окно. Скрытый слой верхним не бывает.
 */
export function isTopLayer(el: Element | null | undefined): boolean {
  if (!el || !shown(el)) return false;
  for (let i = layers.length - 1; i >= 0; i--) {
    if (shown(layers[i])) return layers[i] === el;
  }
  return true;
}
