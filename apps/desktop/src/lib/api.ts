import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface AccountInfo {
  host: string;
  user: string;
  db_path: string;
}

export interface Folder {
  folder_pk: number;
  parent_pk: number | null;
  name: string;
  distinguished: string | null;
  total: number;
  unread: number;
  is_mail: boolean;
}

export interface Conversation {
  conversation_pk: number;
  topic: string | null;
  participants: string[];
  item_count: number;
  unread_count: number;
  total_count: number;
  last_received_ms: number | null;
  has_attachments: boolean;
}

export interface Recipient {
  kind: number;
  name: string | null;
  email: string | null;
}

export interface Attachment {
  attachment_pk: number;
  name: string | null;
  content_type: string | null;
  size: number | null;
  is_inline: boolean;
  cached: boolean;
}

export interface Message {
  item_pk: number;
  folder_pk: number;
  folder_name: string | null;
  subject: string | null;
  from_name: string | null;
  from_email: string | null;
  display_to: string | null;
  received_ms: number | null;
  sent_ms: number | null;
  is_read: boolean;
  is_draft: boolean;
  has_attachments: boolean;
  importance: string | null;
  preview: string | null;
  body_state: number;
  body_html: string | null;
  body_is_text: boolean;
  is_truncated: boolean;
  recipients: Recipient[];
  attachments: Attachment[];
}

export interface SearchHit {
  item_pk: number;
  conversation_pk: number | null;
  subject: string | null;
  from_name: string | null;
  from_email: string | null;
  received_ms: number | null;
  folder_name: string | null;
  snippet: string;
  attachment_name: string | null;
}

/** Этап синхронизации; совпадает с `StageDto` в src-tauri/src/sync.rs. */
export type SyncStage = "login" | "hierarchy" | "items" | "bodies" | "attachments";

export interface SyncReport {
  folders_synced: number;
  items_created: number;
  items_updated: number;
  items_deleted: number;
  bodies_fetched: number;
  bodies_failed: number;
  attachments_fetched: number;
  attachments_failed: number;
  elapsed_ms: number;
}

/** Состояние синхронизации: то же приходит событием `sync-progress` и ответом `sync_status`. */
export interface SyncStatus {
  running: boolean;
  cancelling: boolean;
  stage: SyncStage | null;
  folder: string | null;
  done: number;
  total: number | null;
  items_processed: number;
  folders_done: number;
  started_ms: number | null;
  last_ok_ms: number | null;
  last_error: string | null;
  warnings: string[];
  warnings_total: number;
  last_report: SyncReport | null;
}

export const api = {
  accountInfo: () => invoke<AccountInfo>("account_info"),
  folders: () => invoke<Folder[]>("folders"),
  conversations: (folderPk: number, after?: { ms: number; pk: number }, limit = 100) =>
    invoke<Conversation[]>("conversations", { folderPk, afterMs: after?.ms ?? null, afterPk: after?.pk ?? null, limit }),
  conversation: (conversationPk: number) => invoke<Message[]>("conversation", { conversationPk }),
  search: (query: string, limit = 50) => invoke<SearchHit[]>("search", { query, limit }),
  /** Запуск инкрементальной синхронизации; если она уже идёт, просто возвращает текущий статус. */
  syncStart: (days?: number) => invoke<SyncStatus>("sync_start", { days: days ?? null }),
  syncStatus: () => invoke<SyncStatus>("sync_status"),
  syncCancel: () => invoke<SyncStatus>("sync_cancel"),
};

/** Подписка на ход синхронизации. Возвращает функцию отписки. */
export function onSyncProgress(cb: (status: SyncStatus) => void): Promise<UnlistenFn> {
  return listen<SyncStatus>("sync-progress", (e) => cb(e.payload));
}

const dateFull = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short", year: "numeric" });
const dateShort = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "short" });
const timeOnly = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });
const dateTime = new Intl.DateTimeFormat("ru-RU", { day: "numeric", month: "long", year: "numeric", hour: "2-digit", minute: "2-digit" });

/** Дата в списке: сегодня — время, в этом году — день и месяц, иначе с годом. */
export function listDate(ms: number | null): string {
  if (ms == null) return "";
  const d = new Date(ms);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return timeOnly.format(d);
  if (d.getFullYear() === now.getFullYear()) return dateShort.format(d).replace(".", "");
  return dateFull.format(d).replace(" г.", "");
}

export function fullDate(ms: number | null): string {
  return ms == null ? "" : dateTime.format(new Date(ms)).replace(" г.", "");
}

export function formatSize(bytes: number | null): string {
  if (bytes == null) return "";
  if (bytes < 1024) return `${bytes} Б`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} КБ`;
  return `${(bytes / 1024 / 1024).toFixed(1)} МБ`;
}

export function personLabel(name: string | null, email: string | null): string {
  return name || email || "";
}
