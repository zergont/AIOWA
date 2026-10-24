import type { ComposeKind, Conversation, Folder, WriteReport } from "./api";

/**
 * Действие над письмами: одно и то же для карточки письма, беседы в списке и массового выбора.
 * «В архив» — перенос в папку архива; какая это папка, решает App (distinguished archive или «Архив»).
 */
export type ItemAction = { kind: "read"; read: boolean } | { kind: "flag"; flagged: boolean } | { kind: "move"; folderPk: number } | { kind: "archive" } | { kind: "delete"; permanent: boolean };

/** Контуры значков действий (viewBox 0 0 16 16, обводка без заливки). */
export const ICON = {
  /** Закрытый конверт: «пометить непрочитанным». */
  unread: "M2 4h12v9H2V4Zm0 .5 6 4.5 6-4.5",
  /** Открытый конверт: «пометить прочитанным». */
  read: "M2 6.5 8 2l6 4.5V14H2V6.5Zm0 .5 6 4.5 6-4.5",
  flag: "M4 14V2.5h8l-2 3 2 3H4",
  move: "M2 4h4.5l1.5 1.5H14V13H2V4Zm4.5 5h4m-1.5-2 2 2-2 2",
  /** Коробка архива: «в архив». */
  archive: "M2 3h12v3H2V3Zm1 3h10v7H3V6Zm3 2.5h4",
  delete: "M3.5 4.5h9M6 4.5V3h4v1.5M4.5 4.5l.7 9h5.6l.7-9",
  /** Стрелка влево с хвостом: «ответить». */
  reply: "M6 4.5 2.5 8 6 11.5M2.5 8h6a4.5 4.5 0 0 1 4.5 4.5V13",
  /** Две стрелки влево: «ответить всем». */
  replyAll: "M5 4.5 1.5 8 5 11.5M8.5 4.5 5 8l3.5 3.5M5 8h4.5a4.5 4.5 0 0 1 4.5 4.5V13",
  /** Стрелка вправо с хвостом: «переслать». */
  forward: "M10 4.5 13.5 8 10 11.5M13.5 8h-6A4.5 4.5 0 0 0 3 12.5V13",
  /** Карандаш: «новое письмо». */
  compose: "M11.5 2.5l2 2-8 8H3.5v-2l8-8Zm-1.5 1.5 2 2",
  /** Скрепка: вложение. */
  attach: "M10.5 4.5 5.8 9.2a1.5 1.5 0 0 0 2.1 2.1l5-5a3 3 0 0 0-4.2-4.2l-5.3 5.3a4.5 4.5 0 0 0 6.4 6.4l4.2-4.2",
  /** Папка с плюсом: «новая папка». */
  folderAdd: "M2 4h4.5l1.5 1.5H14V13H2V4Zm6 3.5v4m-2-2h4",
  /** Карандаш у строки: «переименовать». */
  rename: "M3 13h10M4 10.5l6.5-6.5 1.5 1.5L5.5 12H4v-1.5Z",
  /** Крестик: «снять выбор». */
  close: "M4 4l8 8M12 4l-8 8",
  /** Стрелка-возврат: «отменить». */
  undo: "M6 4.5 2.5 8 6 11.5M2.5 8h7a4 4 0 0 1 0 8H8",
  /** Принтер: «печать». */
  print: "M5 6V2.5h6V6M3 6h10v5h-2v2.5H5V11H3V6Zm2 5h6",
  /** Лист с карандашом: «открыть черновик». */
  draft: "M3 2.5h7l3 3v3M3 2.5v11h4M10 2.5v3h3M13.5 9.5l-4.5 4.5H7.5V12.5L12 8l1.5 1.5Z",
  /** Шестерёнка: «настройки». */
  settings:
    "M12.7 6.4L14.5 6.7L14.5 9.3L12.7 9.6L12.5 10.2L13.5 11.7L11.7 13.5L10.2 12.5L9.6 12.7L9.3 14.5L6.7 14.5L6.4 12.7L5.8 12.5L4.3 13.5L2.5 11.7L3.5 10.2L3.3 9.6L1.5 9.3L1.5 6.7L3.3 6.4L3.5 5.8L2.5 4.3L4.3 2.5L5.8 3.5L6.4 3.3L6.7 1.5L9.3 1.5L9.6 3.3L10.2 3.5L11.7 2.5L13.5 4.3L12.5 5.8ZM10.2 8a2.2 2.2 0 1 1-4.4 0a2.2 2.2 0 1 1 4.4 0Z",
} as const;

/** Действия композера над письмом: подписи, клавиши и значки — в том порядке, что в панели карточки и в меню. */
export const COMPOSE_ACTIONS: { kind: Exclude<ComposeKind, "new">; label: string; keys: string; icon: string }[] = [
  { kind: "reply", label: "Ответить", keys: "Ctrl+R", icon: ICON.reply },
  { kind: "reply_all", label: "Ответить всем", keys: "Ctrl+Shift+R", icon: ICON.replyAll },
  { kind: "forward", label: "Переслать", keys: "Ctrl+Shift+F", icon: ICON.forward },
];

/** Заголовок композера по виду письма. */
export const COMPOSE_TITLE: Record<ComposeKind, string> = {
  new: "Новое письмо",
  reply: "Ответ",
  reply_all: "Ответ всем",
  forward: "Пересылка",
};

export function isDeletedFolder(f: Folder | null | undefined): boolean {
  return f?.distinguished === "deleteditems";
}

/**
 * Подписи действий для выбранных бесед в текущей папке (флаг — по всей беседе, как и has_flagged).
 * Для нескольких бесед: «прочитанным», если хоть одна непрочитана; «снять флаг», если хоть одна с флагом.
 */
export function conversationActionLabels(cs: Conversation[], inDeleted: boolean) {
  const anyUnread = cs.some((c) => c.unread_count > 0);
  const anyFlagged = cs.some((c) => c.has_flagged);
  const many = cs.length > 1;
  return {
    anyUnread,
    anyFlagged,
    read: anyUnread ? (many ? "Пометить прочитанными" : "Пометить прочитанным") : many ? "Пометить непрочитанными" : "Пометить непрочитанным",
    flag: anyFlagged ? (many ? "Снять флаги" : "Снять флаг") : many ? "Поставить флаги" : "Поставить флаг",
    move: "Переместить…",
    archive: "В архив",
    delete: inDeleted ? "Удалить навсегда…" : "Удалить",
  };
}

/** Текст отказа по отчёту команды записи: одно письмо — его сообщение, несколько — счётчик и первое сообщение. */
export function failureText(report: WriteReport): string | null {
  const n = report.failed.length;
  if (n === 0) return null;
  const first = report.failed[0].message;
  if (n === 1) return first;
  return `Не удалось для ${n} писем: ${first}`;
}

// --- перетаскивание бесед на папки дерева -------------------------------------------------------

/** Тип данных в dataTransfer: беседы из списка. Письма (item_pks) собираются при броске — у списка их нет. */
export const DND_CONVERSATIONS = "application/x-aiowa-conversations";

export interface DragPayload {
  conversation_pks: number[];
  /** Папка, из которой тащат: на неё бросить нельзя. */
  folder_pk: number;
}

export function readDragPayload(dt: DataTransfer | null): DragPayload | null {
  if (!dt) return null;
  try {
    const raw = dt.getData(DND_CONVERSATIONS);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<DragPayload>;
    if (!Array.isArray(v.conversation_pks) || typeof v.folder_pk !== "number") return null;
    return { conversation_pks: v.conversation_pks.filter((x): x is number => typeof x === "number"), folder_pk: v.folder_pk };
  } catch {
    return null;
  }
}

// --- папки ---------------------------------------------------------------------------------------

/** Ограничения Exchange на имя папки; null — имя годится. */
export function folderNameError(name: string): string | null {
  const t = name.trim();
  if (!t) return "Введи имя папки";
  if (t.length > 255) return "Имя папки длиннее 255 символов";
  if (/[\\/:*?"<>|]/.test(t)) return "В имени папки нельзя использовать \\ / : * ? \" < > |";
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(t)) return "В имени папки есть управляющие символы";
  return null;
}

/** Папка лежит в «Удалённых» (сама или через родителей): удаление из неё — только навсегда. */
export function isInsideDeleted(f: Folder, folders: Folder[]): boolean {
  const byPk = new Map(folders.map((x) => [x.folder_pk, x]));
  let cur: Folder | undefined = f;
  for (let i = 0; cur && i < 64; i++) {
    if (isDeletedFolder(cur)) return true;
    cur = cur.parent_pk != null ? byPk.get(cur.parent_pk) : undefined;
  }
  return false;
}

/**
 * Папка верхнего уровня: родителя нет, он не среди известных папок (корень ящика в базе не хранится,
 * у его детей parent_pk = null) или это сам корень. Та же семантика, что у `find_archive_folder`
 * бэкенда (staged.rs): вложенные «Входящие/Архив» или «Удалённые/Архив» верхним уровнем не считаются.
 */
export function isTopLevel(f: Folder, folders: Folder[]): boolean {
  if (f.parent_pk == null) return true;
  const parent = folders.find((x) => x.folder_pk === f.parent_pk);
  return !parent || parent.distinguished === "msgfolderroot";
}

/**
 * Папка архива: служебная archive, иначе почтовая папка «Архив» (или Archive) верхнего уровня;
 * null — нет ни той, ни другой, и App предложит создать.
 */
export function findArchiveFolder(folders: Folder[]): Folder | null {
  return folders.find((f) => f.distinguished === "archive" && f.is_mail) ?? folders.find((f) => f.is_mail && /^(архив|archive)$/i.test(f.name.trim()) && isTopLevel(f, folders)) ?? null;
}
