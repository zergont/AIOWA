import type { ComposeKind, Conversation, Folder, WriteReport } from "./api";

/** Действие над письмами: одно и то же для карточки письма и для беседы в списке. */
export type ItemAction = { kind: "read"; read: boolean } | { kind: "flag"; flagged: boolean } | { kind: "move"; folderPk: number } | { kind: "delete"; permanent: boolean };

/** Контуры значков действий (viewBox 0 0 16 16, обводка без заливки). */
export const ICON = {
  /** Закрытый конверт: «пометить непрочитанным». */
  unread: "M2 4h12v9H2V4Zm0 .5 6 4.5 6-4.5",
  /** Открытый конверт: «пометить прочитанным». */
  read: "M2 6.5 8 2l6 4.5V14H2V6.5Zm0 .5 6 4.5 6-4.5",
  flag: "M4 14V2.5h8l-2 3 2 3H4",
  move: "M2 4h4.5l1.5 1.5H14V13H2V4Zm4.5 5h4m-1.5-2 2 2-2 2",
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

/** Подписи действий для беседы в текущей папке (флаг — по всей беседе, как и has_flagged). */
export function conversationActionLabels(c: Conversation, inDeleted: boolean) {
  return {
    read: c.unread_count > 0 ? "Пометить прочитанным" : "Пометить непрочитанным",
    flag: c.has_flagged ? "Снять флаг" : "Поставить флаг",
    move: "Переместить…",
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
