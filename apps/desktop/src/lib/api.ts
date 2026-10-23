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
  /** В беседе есть письмо с флагом (в любой папке). */
  has_flagged: boolean;
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
  /** Файл вложения реально лежит в кэше на диске. */
  cached: boolean;
  /** Content-ID инлайнового вложения; у обычных файлов null. */
  content_id: string | null;
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
  is_flagged: boolean;
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

/** Ключ состояния письма в беседе (без тел и картинок): для сравнения после синхронизации. */
export interface MessageKey {
  item_pk: number;
  body_state: number;
  is_read: boolean;
  /** Вложений с файлом в кэше по базе. */
  cached: number;
  /** Флаг; поле может отсутствовать в старом ответе — тогда в ключе не участвует. */
  is_flagged?: boolean;
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

/** Отказ по одному письму в команде записи; текст уже по-русски для показа пользователю. */
export interface WriteFailure {
  item_pk: number;
  message: string;
}

/** Итог команды записи: сколько писем обработано и какие отказали (по ним локальное изменение уже откачено). */
export interface WriteReport {
  done: number;
  failed: WriteFailure[];
}

/** Событие `items-changed`: какие беседы и папки затронуты записью (локально и на сервере). */
export interface ItemsChanged {
  conversation_pks: number[];
  folder_pks: number[];
}

export const api = {
  accountInfo: () => invoke<AccountInfo>("account_info"),
  folders: () => invoke<Folder[]>("folders"),
  conversations: (folderPk: number, after?: { ms: number; pk: number }, limit = 100) =>
    invoke<Conversation[]>("conversations", { folderPk, afterMs: after?.ms ?? null, afterPk: after?.pk ?? null, limit }),
  conversation: (conversationPk: number) => invoke<Message[]>("conversation", { conversationPk }),
  /** Дешёвый ключ беседы: один запрос без тел и картинок. */
  conversationKey: (conversationPk: number) => invoke<MessageKey[]>("conversation_key", { conversationPk }),
  search: (query: string, limit = 50) => invoke<SearchHit[]>("search", { query, limit }),
  /** Догрузить тело письма и инлайновые картинки с сервера (если их ещё нет) и вернуть свежий DTO. */
  messageFetch: (itemPk: number) => invoke<Message>("message_fetch", { itemPk }),
  /** Скачать вложение при необходимости и открыть системным приложением. */
  attachmentOpen: (attachmentPk: number) => invoke<void>("attachment_open", { attachmentPk }),
  /** Скачать при необходимости и показать диалог «Сохранить как»; null — пользователь отменил. */
  attachmentSaveAs: (attachmentPk: number) => invoke<string | null>("attachment_save_as", { attachmentPk }),
  /** Открыть ссылку (http, https, mailto) во внешнем приложении. */
  openExternal: (url: string) => invoke<void>("open_external", { url }),
  /**
   * Запуск инкрементальной синхронизации; если она уже идёт, просто возвращает текущий статус.
   * `folders` — только эти папки (folder_pk), иначе все почтовые.
   */
  syncStart: (days?: number, folders?: number[]) => invoke<SyncStatus>("sync_start", { days: days ?? null, folders: folders ?? null }),
  syncStatus: () => invoke<SyncStatus>("sync_status"),
  syncCancel: () => invoke<SyncStatus>("sync_cancel"),

  // --- запись: сначала локально, затем на сервер; отказ по письму приходит в failed -------------
  itemsMarkRead: (itemPks: number[], read: boolean) => invoke<WriteReport>("items_mark_read", { itemPks, read }),
  itemsSetFlag: (itemPks: number[], flagged: boolean) => invoke<WriteReport>("items_set_flag", { itemPks, flagged }),
  itemsMove: (itemPks: number[], folderPk: number) => invoke<WriteReport>("items_move", { itemPks, folderPk }),
  /** permanent=false — в «Удалённые» (из самих «Удалённых» откажет), permanent=true — навсегда. */
  itemsDelete: (itemPks: number[], permanent: boolean) => invoke<WriteReport>("items_delete", { itemPks, permanent }),
  /** Письма беседы; с folderPk — только лежащие в этой папке. */
  conversationItemPks: (conversationPk: number, folderPk: number | null) => invoke<number[]>("conversation_item_pks", { conversationPk, folderPk }),
};

/** Подписка на ход синхронизации. Возвращает функцию отписки. */
export function onSyncProgress(cb: (status: SyncStatus) => void): Promise<UnlistenFn> {
  return listen<SyncStatus>("sync-progress", (e) => cb(e.payload));
}

/** Подписка на изменения писем после команд записи. Возвращает функцию отписки. */
export function onItemsChanged(cb: (ev: ItemsChanged) => void): Promise<UnlistenFn> {
  return listen<ItemsChanged>("items-changed", (e) => cb(e.payload));
}

/** Текст ошибки команды для показа пользователю: команды Tauri отклоняют промис строкой по-русски. */
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  if (e && typeof e === "object" && "message" in e && typeof (e as { message: unknown }).message === "string") return (e as { message: string }).message;
  return String(e);
}

function isImageType(contentType: string | null): boolean {
  return contentType != null && /^image\//i.test(contentType);
}

/** Инлайновая картинка: живёт в теле письма по cid, в списке файлов не показывается. */
export function isInlinePicture(a: Attachment): boolean {
  return a.is_inline && isImageType(a.content_type);
}

/** Инлайновое вложение, которое message_fetch скачает и подставит в тело: есть content_id, нет файла, тип image/* или неизвестен. */
export function needsInlineFetch(a: Attachment): boolean {
  return a.is_inline && a.content_id != null && !a.cached && (a.content_type == null || isImageType(a.content_type));
}

/**
 * Строка-ключ состояния писем беседы: состав, тела, прочитанность, число вложений в кэше и флаг.
 * Флаг входит в ключ, только если он известен у всех писем (`withFlag`): так ключ сервера и ключ окна сравнимы
 * и когда бэкенд ещё не отдаёт `is_flagged` в `conversation_key`.
 */
export function conversationKeyOf(keys: Pick<MessageKey, "item_pk" | "body_state" | "is_read" | "cached" | "is_flagged">[], withFlag = keys.every((k) => k.is_flagged !== undefined)): string {
  return keys.map((k) => `${k.item_pk}:${k.body_state}:${k.is_read ? 1 : 0}:${k.cached}${withFlag ? (k.is_flagged ? ":f" : ":-") : ""}`).join(",");
}

/** Ключ открытой беседы по уже загруженным письмам (в тех же терминах, что `conversationKey`). */
export function messagesKey(ms: Message[], withFlag = true): string {
  return conversationKeyOf(
    ms.map((m) => ({ item_pk: m.item_pk, body_state: m.body_state, is_read: m.is_read, cached: m.attachments.filter((a) => a.cached).length, is_flagged: m.is_flagged })),
    withFlag,
  );
}

/** Письму нужна догрузка с сервера: нет тела (и сервер ещё не отказывал) или не хватает инлайновых картинок. */
export function messageNeedsFetch(m: Message): boolean {
  return (m.body_html == null && m.body_state !== 2) || m.attachments.some(needsInlineFetch);
}

/** Ссылки, которые можно открывать снаружи; остальное из письма блокируется. */
export function isExternalLink(href: string): boolean {
  return /^(https?:|mailto:)/i.test(href.trim());
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
