import type { Conversation, Folder, WriteReport } from "./api";

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
} as const;

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
