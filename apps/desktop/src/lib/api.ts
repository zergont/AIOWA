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

/** Участник письма без вида: адрес из Reply-To. */
export interface Participant {
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
  /**
   * Класс элемента Exchange (IPM.Note, IPM.Schedule.Meeting.Request, …Canceled, …Resp.Pos/Neg/Tent):
   * по нему карточка узнаёт приглашение, отмену или ответ на встречу. Может отсутствовать в старом ответе.
   */
  item_class?: string | null;
  /**
   * Адреса Reply-To, если заголовок есть и отличается от From: ответ уходит сюда, карточка
   * показывает «Ответ на: …». Может отсутствовать в старом ответе.
   */
  reply_to?: Participant[] | null;
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
export type SyncStage = "login" | "hierarchy" | "recent" | "items" | "bodies" | "attachments";

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

/**
 * Событие `write-failed`: сервер отказал по отложенному действию, когда окно отмены уже закрыто.
 * `message` — текст по-русски для показа, `item_pks` — письма, по которым локальное изменение откачено.
 */
export interface WriteFailed {
  message: string;
  item_pks: number[];
}

/** Действие с окном отмены: удаление в «Удалённые», перенос, архив. */
export type StageAction = "delete" | "move" | "archive";

/**
 * Итог `items_stage`: локально уже применено, сервер ждёт до `undo_until_ms` (миллисекунды эпохи);
 * до этого срока `items_undo(stage_id)` откатывает, после — бэкенд сам отправляет на сервер.
 */
export interface StagedDto {
  stage_id: string;
  done: number;
  failed: WriteFailure[];
  undo_until_ms: number;
}

// --- композер: новое письмо, ответ, ответ всем, пересылка ---------------------

export type ComposeKind = "new" | "reply" | "reply_all" | "forward";

/** Получатель исходящего письма (в отличие от `Recipient` входящего — без вида, адрес обязателен). */
export interface ComposeRecipient {
  name: string | null;
  email: string;
}

export interface SignatureDto {
  html: string;
  auto_add: boolean;
}

/** Вложение черновика, уже лежащее на сервере (compose_open_draft). */
export interface DraftAttachment {
  /** Запись в локальной базе; null, если черновик известен только с сервера. */
  attachment_pk: number | null;
  /** Серверный AttachmentId — им вложение убирается (removed_attachment_ids). */
  server_id: string;
  name: string | null;
  size: number | null;
  content_type: string | null;
}

/** Открытый черновик в заготовке композера: откуда он и какие вложения уже на сервере. */
export interface DraftInfo {
  item_pk: number | null;
  server_id: string;
  change_key?: string | null;
  attachments: DraftAttachment[];
  /** Тело черновика, если бэкенд положил его сюда, а не в `ComposeContext.body_html`. */
  body_html?: string | null;
  bcc?: ComposeRecipient[];
  /**
   * Инлайновые картинки тела из кэша для показа в редакторе: нормализованный cid (normalizeCid) →
   * адрес aiowa-att. Тело хранит `cid:`; адрес подставляется только для показа.
   */
  inline_images?: Record<string, string>;
}

/** Ссылка на черновик на сервере после compose_autosave; item_pk — локальная запись, если она уже есть. */
export interface DraftRef {
  server_id: string;
  change_key: string | null;
  item_pk: number | null;
}

/** Заготовка композера от бэкенда: получатели и тема уже подобраны по виду письма. */
export interface ComposeContext {
  kind: ComposeKind;
  reference_item_pk: number | null;
  to: ComposeRecipient[];
  cc: ComposeRecipient[];
  subject: string;
  /** Исходное письмо для ответа/пересылки; у нового письма null. */
  original: Message | null;
  own_addresses: string[];
  signature: SignatureDto;
  /** Скрытые получатели: только у открытого черновика. */
  bcc?: ComposeRecipient[];
  /** Тело открытого черновика (HTML); у новой заготовки отсутствует — тело строится из подписи. */
  body_html?: string | null;
  /** Открыт существующий черновик (compose_open_draft); у новой заготовки отсутствует. */
  draft?: DraftInfo | null;
}

export interface ComposeRequest {
  kind: ComposeKind;
  reference_item_pk: number | null;
  to: ComposeRecipient[];
  cc: ComposeRecipient[];
  bcc: ComposeRecipient[];
  subject: string;
  body_html: string;
  /** Пути файлов-вложений. */
  attachments: string[];
  /**
   * Черновик уже на сервере (автосохранение или открытый из «Черновиков»): отправка идёт по нему —
   * update_draft → add_attachments для новых файлов → delete_attachments → send_draft.
   */
  draft_server_id?: string | null;
  /** Серверные вложения черновика, убранные в композере (AttachmentId). */
  removed_attachment_ids?: string[];
}

/** Ответ attachment_preview_url: адрес файла в кэше по протоколу aiowa-att и годится ли тип для показа в окне. */
export interface PreviewDto {
  url: string;
  content_type: string | null;
  previewable: boolean;
  /** Почему предпросмотра нет: тип окно не показывает или файл больше лимита; null — показывать можно. */
  reason?: "unsupported_type" | "too_large" | null;
  /** Можно ли открыть системным приложением; исполняемые типы — только «Сохранить как…». */
  openable?: boolean;
  name: string | null;
  size: number | null;
}

/**
 * Итог отправки или сохранения черновика: если не ушло, но черновик остался на сервере — draft_saved;
 * message — текст для пользователя. maybe_sent — ответа на SendItem нет (таймаут, обрыв связи): письмо
 * могло уйти, повтор дал бы дубль адресатам.
 */
export interface SendReport {
  sent: boolean;
  draft_saved: boolean;
  maybe_sent: boolean;
  message: string | null;
  /** Черновик на сервере, если он остался или сохранён: композер продолжает с ним, а не создаёт второй. */
  draft?: DraftRef | null;
  /** Файловые вложения черновика на сервере, если этот вызов их менял (null — не менял или не перечитаны). */
  attachments?: DraftAttachment[] | null;
}

export interface RecipientSuggestion {
  name: string | null;
  email: string;
  weight: number;
}

// --- справочник организации ----------------------------------------------------------------
// Бэкенд ищет сначала в кэше справочника (локальная база), затем на сервере (FindPeople, при отказе —
// ResolveNames) и складывает найденное в кэш. Поля, которых справочник не дал, приходят null или
// отсутствуют вовсе: окно не полагается ни на одно из них, кроме адреса.

/** Вид записи справочника: почтовый ящик, список рассылки, контакт, переговорная, прочее. */
export type DirectoryKind = "mailbox" | "list" | "contact" | "room" | "other";

export interface DirectoryPerson {
  name?: string | null;
  email?: string | null;
  kind: DirectoryKind;
  title?: string | null;
  department?: string | null;
  company?: string | null;
  office?: string | null;
  business_phone?: string | null;
  mobile_phone?: string | null;
}

/** Поиск по справочнику: найденное, взято ли оно из кэша, и текст отказа сервера (кэш при отказе всё равно отдаётся). */
export interface DirectorySearch {
  people: DirectoryPerson[];
  from_cache: boolean;
  error?: string | null;
}

/** Переписка с адресом по локальной базе: писем от него, писем ему и время последнего. */
export interface ContactLocal {
  messages_from: number;
  messages_to: number;
  last_ms?: number | null;
}

/** Карточка контакта: запись справочника (null — не нашлась), из кэша ли она, отказ сервера и переписка. */
export interface ContactCard {
  person: DirectoryPerson | null;
  from_cache: boolean;
  error?: string | null;
  local?: ContactLocal | null;
}

/** Запись справочника — список рассылки: его можно развернуть в участников. */
export function isDirectoryList(p: Pick<DirectoryPerson, "kind"> | null | undefined): boolean {
  return p?.kind === "list";
}

export interface PickedFile {
  path: string;
  name: string;
  size: number;
}

// --- календарь ------------------------------------------------------------------------------
// Миллисекунды — UTC; окно само переводит их в местное время. У событий «весь день» границы —
// полуночи в поясе организатора (сервер их не нормализует), поэтому день показа — местная дата
// ближайшей полуночи к границе (см. allDayDate в lib/calendar.ts; верно при разнице поясов до 12 ч).

export type FreeBusy = "free" | "tentative" | "busy" | "oof" | "working_elsewhere" | "no_data";
export type CalendarItemType = "single" | "occurrence" | "exception" | "recurring_master";
export type ResponseType = "unknown" | "organizer" | "tentative" | "accept" | "decline" | "no_response";

export interface CalendarPerson {
  name: string | null;
  email: string | null;
}

/** Событие в сетке; повторяющиеся приходят уже развёрнутыми в экземпляры. */
export interface CalendarEvent {
  calendar_pk: number;
  subject: string | null;
  start_ms: number;
  end_ms: number;
  is_all_day: boolean;
  location: string | null;
  organizer: CalendarPerson | null;
  free_busy: FreeBusy;
  calendar_type: CalendarItemType;
  is_meeting: boolean;
  is_cancelled: boolean;
  my_response: ResponseType;
  sensitivity: string | null;
  has_attachments: boolean;
  /** Подробности (тело, участники) уже в кэше. */
  has_details: boolean;
  /** Напоминание за N минут до начала; null — без напоминания. Может отсутствовать в старом ответе. */
  reminder_minutes?: number | null;
}

/** Ответ `calendar_events`: при ошибке сети — кэш (если есть) и текст ошибки по-русски. */
export interface CalendarRange {
  events: CalendarEvent[];
  from_cache: boolean;
  /** Когда окно последний раз забирали с сервера; null — кэш окно целиком не покрывает. */
  fetched_ms: number | null;
  error: string | null;
}

export interface Attendee {
  name: string | null;
  email: string | null;
  response: ResponseType;
  required: boolean;
}

export interface CalendarEventDetails {
  event: CalendarEvent;
  /** Тело уже очищено на бэкенде (без cid). */
  body_html: string | null;
  attendees: Attendee[];
  reminder_minutes: number | null;
  recurrence_text: string | null;
  from_cache: boolean;
  /** Обновить с сервера не удалось, показаны подробности из кэша. */
  error: string | null;
}

// --- календарь: запись, ответы на приглашения, напоминания ------------------------------------

/** Занятость, которую можно задать событию (без `no_data`). */
export type FreeBusyChoice = Exclude<FreeBusy, "no_data">;

/** Участник в запросе на создание или правку события. */
export interface EventParticipant {
  name: string | null;
  email: string;
}

/**
 * Запрос `calendar_create` и `calendar_update`: для правки окно шлёт все поля, бэкенд сам
 * сравнивает с кэшем и пишет на сервер только изменившиеся. Моменты — миллисекунды UTC;
 * у «весь день» — местные полуночи первого дня и дня после последнего.
 */
export interface EventRequest {
  /**
   * Окно видело участников и описание (редактор открыт с подробностями): пустые `required`,
   * `optional` и `body_html` значат «убрать». Без него бэкенд их не трогает.
   */
  details_known: boolean;
  subject: string;
  start_ms: number;
  end_ms: number;
  is_all_day: boolean;
  location: string | null;
  body_html: string | null;
  required: EventParticipant[];
  optional: EventParticipant[];
  /** За сколько минут напоминать; null — без напоминания. */
  reminder_minutes: number | null;
  free_busy: FreeBusyChoice;
  is_private: boolean;
}

export type MeetingResponse = "accept" | "tentative" | "decline";

/** Вид письма о встрече: приглашение, отмена, ответ участника, прочее. */
export type MeetingMessageKind = "request" | "cancellation" | "response" | "other";

/** Ответ `meeting_info`: письмо-приглашение (отмена, ответ) с полями встречи, GetItem по письму. */
export interface MeetingInfo {
  kind: MeetingMessageKind;
  start_ms: number | null;
  end_ms: number | null;
  is_all_day: boolean;
  location: string | null;
  organizer: CalendarPerson | null;
  /** Мой ответ на приглашение (у письма с сервера). */
  my_response: ResponseType;
  /** Приглашение устарело: организатор прислал более новое. */
  is_out_of_date: boolean;
  /** Встреча уже есть в календаре. */
  has_calendar_item: boolean;
  recurrence_text: string | null;
  /** Для ответа участника: что он ответил. */
  sender_response: ResponseType | null;
  /** Связанное событие в кэше календаря, если бэкенд его нашёл: для «Удалить из календаря». */
  calendar_pk?: number | null;
}

/** Напоминание о событии: приходит событием `reminder` и ответом `reminders_pending`. */
export interface ReminderDto {
  calendar_pk: number;
  subject: string | null;
  start_ms: number;
  end_ms: number;
  is_all_day: boolean;
  location: string | null;
  /** Минут до начала на момент отправки; отрицательное — уже началось. */
  minutes_left: number;
}

/** Событие `reminder`: напоминания, которые пора показать. */
export interface ReminderEvent {
  items: ReminderDto[];
}

// --- канал мгновенных уведомлений OWA ------------------------------------------------------
// Бэкенд держит открытым поток PendingNotificationRequest и по событиям сервера сам запускает
// синхронизацию папок. Окно только показывает состояние канала и реагирует на события.

/** Состояние канала; то же приходит событием `notify-status` при каждом изменении и ответом `notify_status`. */
export interface NotifyStatus {
  /** Поток открыт: изменения приходят от сервера сразу, таймер синхронизации — лишь страховка. */
  connected: boolean;
  /** С какого времени канал держится; null — не подключён. */
  since_ms: number | null;
  last_event_ms: number | null;
  /** Подписок, принятых сервером в текущем цикле. */
  subscriptions: number;
  /** Последняя ошибка канала (по-русски); при ней бэкенд переподключается с растущей паузой. */
  error: string | null;
  /** Тосты Windows о новых письмах (ключ desktop.notify_toasts; по умолчанию включены). */
  toasts_enabled: boolean;
}

/** Отправитель в событии `new-mail`: строка либо участник с именем и адресом. */
export type NewMailFrom = string | { name: string | null; email: string | null } | null;

/**
 * Событие `new-mail`: сервер сообщил о новом письме. Приходит до синхронизации папки, поэтому
 * письма в базе ещё может не быть; item_pk заполнен, только если бэкенд его уже нашёл.
 */
export interface NewMail {
  from: NewMailFrom;
  subject: string | null;
  preview: string | null;
  /** Серверный id письма (строкой или объектом ItemId); null, если сервер его не прислал. */
  item_id: string | { id: string; change_key?: string | null } | null;
  item_pk?: number | null;
  folder_pk: number | null;
}

// --- режим приложения, мастер первого запуска, настройки --------------------------------------
// Без учётной записи бэкенд стартует в режиме "setup": состояние почты не создаётся, окно показывает
// только мастер. Мастер проверяет вход, сохраняет пароль и config.json и перезапускает приложение.

/** Ответ `app_mode`: в каком режиме поднялось приложение и откуда взялась учётная запись. */
export interface AppMode {
  mode: "setup" | "mail";
  /** Хост и логин текущей учётной записи (в режиме setup — null). */
  host: string | null;
  user: string | null;
  /** Путь к config.json (показывается в мастере). */
  config_path: string;
  /** Учётная запись взята из переменных AIOWA_HOST/AIOWA_USER (режим разработки). */
  dev_env: boolean;
  /**
   * Запускать синхронизацию при старте окна: false только на первом старте после мастера со снятой
   * галочкой «Начать синхронизацию сразу». Нет поля (старый бэкенд) — запускать.
   */
  autostart_sync?: boolean;
  /**
   * Режим "setup" не из-за отсутствия учётной записи, а потому что база не открылась (например,
   * негодный db_path в config.json): текст ошибки для мастера. Нет поля или null — обычный первый запуск.
   */
  boot_error?: string | null;
}

/** Итог пробного входа (`setup_test_login`): ошибка уже по-русски. */
export interface LoginTest {
  ok: boolean;
  server_version: string | null;
  /** Почтовых папок в ящике, если запрос иерархии прошёл. */
  folders: number | null;
  error: string | null;
  /** Адрес сервера после нормализации (host[:port]), с которым шла проверка. */
  host?: string | null;
}

export type Theme = "system" | "light" | "dark";

/** Ответ `settings_get`; изменяемые поля меняются через `settings_set` по одному или пачкой. */
export interface Settings {
  host: string;
  user: string;
  /** Глубина загрузки тел писем при синхронизации, дней; 0 — все. */
  body_days: number;
  /** Задержка автопометки прочитанным, мс; 0 — выключена. */
  auto_read_ms: number;
  /** Тосты Windows о новых письмах (тот же ключ, что у переключателя в строке состояния). */
  toasts: boolean;
  theme: Theme;
  /** Интервал страховочной синхронизации, минут (при живом канале — не чаще раза в 10 минут). */
  sync_interval_min: number;
  /** Квота кэша вложений на диске, МБ. */
  attachment_cache_mb: number;
  /** Вложения больше этого размера синхронизация не скачивает (только по клику), МБ. */
  max_attachment_mb: number;
  db_path: string;
  db_size_bytes: number;
  cache_size_bytes: number;
  /** Пароль лежит в диспетчере учётных данных Windows. */
  password_saved: boolean;
  /** Учётная запись из переменных AIOWA_HOST/AIOWA_USER: «забыть» её из окна нельзя. */
  dev_env?: boolean;
  config_path?: string;
}

/** Изменяемые поля настроек; `settings_set` принимает любое их подмножество. */
export type SettingsPatch = Partial<Pick<Settings, "body_days" | "auto_read_ms" | "toasts" | "theme" | "sync_interval_min" | "attachment_cache_mb" | "max_attachment_mb">>;

/** Допустимые диапазоны числовых настроек (те же проверяет бэкенд в `settings_set`). */
export const SETTINGS_LIMITS = {
  body_days: { min: 0, max: 3650 },
  auto_read_ms: { min: 0, max: 10_000 },
  sync_interval_min: { min: 1, max: 120 },
  attachment_cache_mb: { min: 100, max: 102_400 },
  max_attachment_mb: { min: 1, max: 200 },
} as const;

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
  /** Скачать при необходимости и вернуть адрес файла в кэше по протоколу aiowa-att для предпросмотра. */
  attachmentPreviewUrl: (attachmentPk: number) => invoke<PreviewDto>("attachment_preview_url", { attachmentPk }),
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

  // --- запись с окном отмены: локально сразу, на сервер через 6 с (или по items_commit) --------
  /** `folderPk` — папка назначения для move и archive; `permanent` — только для delete. */
  itemsStage: (action: StageAction, itemPks: number[], folderPk: number | null, permanent = false) => invoke<StagedDto>("items_stage", { action, itemPks, folderPk, permanent }),
  /** Откатить, пока окно отмены не истекло; иначе отказ «Отмена уже невозможна». */
  itemsUndo: (stageId: string) => invoke<WriteReport>("items_undo", { stageId }),
  /** Отправить на сервер сейчас, не дожидаясь таймера. */
  itemsCommit: (stageId: string) => invoke<WriteReport | void>("items_commit", { stageId }),

  // --- папки -------------------------------------------------------------------------------
  /** Создать папку; parentPk null — в корне ящика. После — событие folders-changed. */
  folderCreate: (parentPk: number | null, name: string) => invoke<Folder>("folder_create", { parentPk, name }),
  folderRename: (folderPk: number, name: string) => invoke<Folder>("folder_rename", { folderPk, name }),
  /** permanent=false — в «Удалённые» вместе с подпапками и письмами, permanent=true — навсегда. */
  folderDelete: (folderPk: number, permanent: boolean) => invoke<void>("folder_delete", { folderPk, permanent }),

  // --- композер ----------------------------------------------------------------------------
  /** Заготовка композера: для ответа/пересылки itemPk — исходное письмо. */
  composeContext: (kind: ComposeKind, itemPk: number | null) => invoke<ComposeContext>("compose_context", { kind, itemPk }),
  /** Черновик → вложения → отправка; отказ после создания черновика приходит в SendReport (draft_saved). */
  composeSend: (req: ComposeRequest) => invoke<SendReport>("compose_send", { req }),
  /** Сохранить черновик вместе с файлами (create или update по draft_server_id → новые файлы → удаление убранных), без отправки. */
  composeSaveDraft: (req: ComposeRequest) => invoke<SendReport>("compose_save_draft", { req }),
  /** Заготовка композера из черновика в базе (текст с сервера, если локального тела нет). */
  composeOpenDraft: (itemPk: number) => invoke<ComposeContext>("compose_open_draft", { itemPk }),
  /** Автосохранение: без draftServerId — create_draft, с ним — update_draft; вложения не трогает. */
  composeAutosave: (req: ComposeRequest, draftServerId: string | null) => invoke<DraftRef>("compose_autosave", { req, draftServerId }),
  /** Удалить черновик: на сервере — в «Удалённые», локально — сразу из «Черновиков». */
  composeDiscardDraft: (draftServerId: string) => invoke<void>("compose_discard_draft", { draftServerId }),
  /** Подсказки адресатов по началу адреса или слову имени. */
  recipientsSuggest: (prefix: string, limit?: number) => invoke<RecipientSuggestion[]>("recipients_suggest", { prefix, limit: limit ?? null }),
  /**
   * Поиск по справочнику организации: сервер с таймаутом 10 с (повтор запроса за 10 минут — его же
   * ответ из базы); `cacheOnly` — сразу и без сервера, кэш справочника на время, пока ищет сервер.
   * Запрос короче 2 символов — пусто.
   */
  directorySearch: (query: string, limit?: number, cacheOnly = false) => invoke<DirectorySearch>("directory_search", { query, limit: limit ?? null, cacheOnly }),
  /** Карточка контакта: справочник (кэш или сервер; `refresh` — заново с сервера) и счётчики переписки. */
  contactCard: (email: string, refresh = false) => invoke<ContactCard>("contact_card", { email, refresh }),
  /** Участники списка рассылки (ExpandDL на сервере). */
  directoryExpand: (email: string) => invoke<DirectoryPerson[]>("directory_expand", { email }),
  /** Что умеет справочник на этом сервере: раскрытие списков рассылки бывает не реализовано (§ 5l). */
  directoryCapabilities: () => invoke<{ can_expand: boolean }>("directory_capabilities"),
  ownAddresses: () => invoke<string[]>("own_addresses"),
  signatureGet: () => invoke<SignatureDto>("signature_get"),
  signatureSet: (html: string, autoAdd: boolean) => invoke<void>("signature_set", { html, autoAdd }),
  /** Подпись из настроек OWA; сохраняется локально; отказ строкой, если подписи нет. */
  signatureImportOwa: () => invoke<SignatureDto>("signature_import_owa"),
  /** Системный диалог выбора файлов; отмена — пустой список. */
  attachmentsPick: () => invoke<PickedFile[]>("attachments_pick"),

  // --- окно --------------------------------------------------------------------------------
  /** Пока в композере несохранённое письмо: крестик и Alt+F4 не закрывают окно, а шлют событие close-requested. */
  windowCloseGuard: (enabled: boolean) => invoke<void>("window_close_guard", { enabled }),
  /** Пользователь подтвердил закрытие окна. */
  windowCloseConfirmed: () => invoke<void>("window_close_confirmed"),

  // --- календарь ---------------------------------------------------------------------------
  /**
   * События окна [startMs, endMs) в UTC. Без `refresh` — только кэш, что есть (fetched_ms говорит,
   * когда окно забирали, null — не покрыто), о догрузке решает окно; с `refresh` — всегда сервер.
   */
  calendarEvents: (startMs: number, endMs: number, refresh: boolean) => invoke<CalendarRange>("calendar_events", { startMs, endMs, refresh }),
  /** Подробности события: тело, участники, напоминание, текст повторения; с `refresh` — перечитать с сервера. */
  calendarEventDetails: (calendarPk: number, refresh: boolean) => invoke<CalendarEventDetails>("calendar_event_details", { calendarPk, refresh }),

  // --- календарь: запись -------------------------------------------------------------------
  /** Создать событие; при участниках сервер сам рассылает приглашения. Возвращает событие из обновлённого кэша. */
  calendarCreate: (req: EventRequest) => invoke<CalendarEvent>("calendar_create", { req }),
  /** Изменить событие (у повторяющегося — именно этот экземпляр); `sendUpdates` — разослать обновление участникам. */
  calendarUpdate: (calendarPk: number, req: EventRequest, sendUpdates: boolean) => invoke<CalendarEvent>("calendar_update", { calendarPk, req, sendUpdates }),
  /** Удалить событие (в «Удалённые»); `sendCancellations` — разослать участникам отмену. */
  calendarDelete: (calendarPk: number, sendCancellations: boolean) => invoke<void>("calendar_delete", { calendarPk, sendCancellations }),
  /** Ответить на приглашение по событию календаря; `sendResponse` — отправить ответ организатору. */
  calendarRespond: (calendarPk: number, response: MeetingResponse, sendResponse: boolean) => invoke<void>("calendar_respond", { calendarPk, response, sendResponse }),
  /** Поля встречи из письма-приглашения (отмены, ответа): GetItem по письму, кэш в памяти бэкенда. */
  meetingInfo: (itemPk: number) => invoke<MeetingInfo>("meeting_info", { itemPk }),
  /** Ответить на приглашение по письму; после — события items-changed и calendar-changed. */
  meetingRespond: (itemPk: number, response: MeetingResponse, sendResponse: boolean) => invoke<void>("meeting_respond", { itemPk, response, sendResponse }),

  // --- напоминания -------------------------------------------------------------------------
  /** Напоминания, которые пора показать (на старте окна). */
  remindersPending: () => invoke<ReminderDto[]>("reminders_pending"),
  reminderDismiss: (calendarPk: number) => invoke<void>("reminder_dismiss", { calendarPk }),
  reminderSnooze: (calendarPk: number, minutes: number) => invoke<void>("reminder_snooze", { calendarPk, minutes }),

  // --- канал уведомлений -------------------------------------------------------------------
  notifyStatus: () => invoke<NotifyStatus>("notify_status"),
  /** Включить или выключить тосты Windows о новых письмах; новое состояние придёт событием `notify-status`. */
  notifySetToasts: (enabled: boolean) => invoke<void>("notify_set_toasts", { enabled }),

  // --- режим, мастер первого запуска (работают в обоих режимах) ----------------------------
  /** Старый бэкенд без этой команды — считать режимом "mail". */
  appMode: () => invoke<AppMode>("app_mode"),
  /** Пробный вход с этим паролем: ничего не сохраняет. */
  setupTestLogin: (host: string, user: string, password: string) => invoke<LoginTest>("setup_test_login", { host, user, password }),
  /** Сохранить пароль и config.json и перезапустить приложение: ответ может не успеть прийти. */
  setupSave: (host: string, user: string, password: string, bodyDays: number, startSync: boolean) => invoke<void>("setup_save", { host, user, password, bodyDays, startSync }),

  // --- настройки (режим mail) ----------------------------------------------------------------
  settingsGet: () => invoke<Settings>("settings_get"),
  /** Частичное обновление; бэкенд может вернуть свежие настройки целиком или ничего. */
  settingsSet: (patch: SettingsPatch) => invoke<Settings | null | void>("settings_set", { patch }),
  /** Проверить вход с новым паролем и при успехе сохранить его; при отказе пароль не меняется. */
  settingsChangePassword: (password: string) => invoke<void>("settings_change_password", { password }),
  /** Удалить пароль и учётную запись из конфига (база остаётся на диске) и перезапуститься в мастер. */
  settingsForgetAccount: () => invoke<void>("settings_forget_account"),
  /** Открыть каталог базы в Проводнике. */
  settingsOpenDataDir: () => invoke<void>("settings_open_data_dir"),
};

/** Подписка на запрос закрытия окна (крестик, Alt+F4) при поднятом windowCloseGuard. Возвращает функцию отписки. */
export function onCloseRequested(cb: () => void): Promise<UnlistenFn> {
  return listen<void>("close-requested", () => cb());
}

/** Подписка на ход синхронизации. Возвращает функцию отписки. */
export function onSyncProgress(cb: (status: SyncStatus) => void): Promise<UnlistenFn> {
  return listen<SyncStatus>("sync-progress", (e) => cb(e.payload));
}

/**
 * Событие `sync-live` (src-tauri/src/live.rs): что записано в базу по ходу прогона с прошлого
 * события — дерево папок и папки, в которых записаны письма. Приходит не чаще раза в 2,5 с.
 */
export interface SyncLive {
  folders: boolean;
  folder_pks: number[];
}

/** Подписка на записи в базу по ходу синхронизации (живое окно). Возвращает функцию отписки. */
export function onSyncLive(cb: (ev: SyncLive) => void): Promise<UnlistenFn> {
  return listen<SyncLive>("sync-live", (e) => cb(e.payload));
}

/** Подписка на изменения писем после команд записи. Возвращает функцию отписки. */
export function onItemsChanged(cb: (ev: ItemsChanged) => void): Promise<UnlistenFn> {
  return listen<ItemsChanged>("items-changed", (e) => cb(e.payload));
}

/** Подписка на отказ сервера по отложенному действию (после окна отмены). Возвращает функцию отписки. */
export function onWriteFailed(cb: (ev: WriteFailed) => void): Promise<UnlistenFn> {
  return listen<WriteFailed>("write-failed", (e) => cb(e.payload));
}

/** Подписка на изменения дерева папок (создание, переименование, удаление, синхронизация иерархии). Возвращает функцию отписки. */
export function onFoldersChanged(cb: () => void): Promise<UnlistenFn> {
  return listen<void>("folders-changed", () => cb());
}

/** Подписка на состояние канала уведомлений. Возвращает функцию отписки. */
export function onNotifyStatus(cb: (status: NotifyStatus) => void): Promise<UnlistenFn> {
  return listen<NotifyStatus>("notify-status", (e) => cb(e.payload));
}

/** Подписка на новые письма по уведомлению сервера. Возвращает функцию отписки. */
export function onNewMail(cb: (mail: NewMail) => void): Promise<UnlistenFn> {
  return listen<NewMail>("new-mail", (e) => cb(e.payload));
}

/** Подписка на изменения в календаре на сервере (без подробностей: окно перечитывает текущий период). */
export function onCalendarChanged(cb: () => void): Promise<UnlistenFn> {
  return listen<void>("calendar-changed", () => cb());
}

/** Подписка на изменение настроек (свежий снимок после settings_set). Возвращает функцию отписки. */
export function onSettingsChanged(cb: (s: Settings) => void): Promise<UnlistenFn> {
  return listen<Settings>("settings-changed", (e) => cb(e.payload));
}

/** Подписка на напоминания планировщика: пора показать события из списка. Возвращает функцию отписки. */
export function onReminder(cb: (ev: ReminderEvent) => void): Promise<UnlistenFn> {
  return listen<ReminderEvent>("reminder", (e) => cb(e.payload));
}

/**
 * Вид письма о встрече по классу элемента: приглашение (IPM.Schedule.Meeting.Request), отмена
 * (…Canceled), ответ участника (…Resp.Pos, …Resp.Neg, …Resp.Tent); null — обычное письмо.
 */
export function meetingKindOf(itemClass: string | null | undefined): MeetingMessageKind | null {
  const c = (itemClass ?? "").toLowerCase();
  if (!c.startsWith("ipm.schedule.meeting")) return null;
  if (c.startsWith("ipm.schedule.meeting.request")) return "request";
  if (c.startsWith("ipm.schedule.meeting.canceled") || c.startsWith("ipm.schedule.meeting.cancelled")) return "cancellation";
  if (c.startsWith("ipm.schedule.meeting.resp")) return "response";
  return "other";
}

/** Ответ участника по классу письма-ответа (…Resp.Pos/Neg/Tent); null — не ответ или класс незнаком. */
export function senderResponseOf(itemClass: string | null | undefined): ResponseType | null {
  const c = (itemClass ?? "").toLowerCase();
  if (c.startsWith("ipm.schedule.meeting.resp.pos")) return "accept";
  if (c.startsWith("ipm.schedule.meeting.resp.neg")) return "decline";
  if (c.startsWith("ipm.schedule.meeting.resp.tent")) return "tentative";
  return null;
}

/** Отправитель нового письма для строки состояния: имя, иначе адрес, иначе «(без отправителя)». */
export function newMailSender(m: NewMail): string {
  if (typeof m.from === "string") return m.from || "(без отправителя)";
  return personLabel(m.from?.name ?? null, m.from?.email ?? null) || "(без отправителя)";
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

const PREVIEW_EXT = /\.(png|jpe?g|gif|webp|bmp|svg|ico|avif|pdf|txt|csv|log|md|html?)$/i;
/** Те же типы, что у бэкенда (`preview::is_previewable`): картинки, которые рисует WebView, PDF и текст. */
const PREVIEW_TYPES = new Set([
  "image/png",
  "image/jpeg",
  "image/jpg",
  "image/pjpeg",
  "image/gif",
  "image/webp",
  "image/bmp",
  "image/svg+xml",
  "image/x-icon",
  "image/vnd.microsoft.icon",
  "image/avif",
  "application/pdf",
  "text/plain",
  "text/csv",
  "text/html",
]);

/**
 * Тип вложения годится для предпросмотра в окне: картинки, PDF, простой текст, CSV и HTML (как текст).
 * Как у бэкенда (`preview::resolved_type`): без Content-Type или с общим application/octet-stream —
 * по расширению имени. Окончательно решает бэкенд (`PreviewDto.previewable`, ещё и по размеру).
 */
export function isPreviewable(a: Pick<Attachment, "name" | "content_type">): boolean {
  const ct = (a.content_type ?? "").toLowerCase().split(";")[0].trim();
  if (ct && ct !== "application/octet-stream") return PREVIEW_TYPES.has(ct);
  return PREVIEW_EXT.test(a.name ?? "");
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

/** Русская форма слова по числу: plural(3, "беседа", "беседы", "бесед") → «беседы». */
export function plural(n: number, one: string, few: string, many: string): string {
  const abs = Math.abs(n) % 100;
  const d = abs % 10;
  if (abs > 10 && abs < 20) return many;
  if (d === 1) return one;
  if (d >= 2 && d <= 4) return few;
  return many;
}

/** «1 беседа», «3 беседы», «12 бесед». */
export function conversationsText(n: number): string {
  return `${n.toLocaleString("ru-RU")} ${plural(n, "беседа", "беседы", "бесед")}`;
}
