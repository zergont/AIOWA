//! `aiowa`: консольная утилита этапа 1. Вход, папки, синхронизация в локальную базу,
//! список бесед, поиск с операторами, вложения, операции записи (прочитано, флаг, перенос, удаление),
//! папки (создание, переименование, удаление, живая проверка), отправка письма, подпись из OWA,
//! подсказки адресов, справочник организации (FindPeople, ResolveNames, ExpandDL), календарь
//! (окно событий, одно событие, создание, изменение и удаление события, письмо о встрече),
//! проверка поля Reply-To, живая проверка черновиков (создание, чтение, изменение, удаление
//! вложения, удаление).
//! Пароль хранится в диспетчере учётных данных Windows и не передаётся аргументом.

use aiowa_core::{
    AccountId, CalendarItemChanges, CalendarItemType, ComposeKind, DirectoryPerson, DraftContent, FolderChange, FolderId, FolderRef, FreeBusy, Importance, ItemId, NewCalendarItem, OutgoingAttachment, OutgoingMessage,
    Participant, PersonKind, RecipientKind, SyncState,
};
use aiowa_store::{AttachmentTodo, CalendarRow, Store};
use aiowa_sync::{Engine, FolderSelection, SyncOptions};
use aiowa_transport::{new_guid, Credentials, DeleteMode, MailTransport, NotificationEvent, NotificationKind, SubscriptionRequest, TransportError, WriteResult};
use aiowa_transport_owa::{OwaConfig, OwaTransport};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// Дополняет строку пробелами до `width` символов (не байт), обрезая длинную.
fn pad(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

const KEYRING_SERVICE: &str = "AIOWA";

#[derive(Parser)]
#[command(name = "aiowa", version, about = "AIOWA: оффлайн клиент Exchange. Этап 1: синхронизация и поиск из консоли")]
struct Cli {
    /// Хост OWA, например mail.example.com
    #[arg(long, env = "AIOWA_HOST")]
    host: String,
    /// Логин как в форме OWA: DOMAIN\user или user@domain
    #[arg(long, env = "AIOWA_USER")]
    user: String,
    /// Путь к базе; по умолчанию %LOCALAPPDATA%\AIOWA\<host>\<user>\mail.db
    #[arg(long, env = "AIOWA_DB")]
    db: Option<PathBuf>,
    /// Часовой пояс в заголовке запросов к серверу; даты хранятся в UTC
    #[arg(long, default_value = "UTC")]
    tz: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Запросить пароль и сохранить в диспетчере учётных данных Windows
    SetPassword,
    /// Удалить сохранённый пароль
    ForgetPassword,
    /// Проверить вход: сборка сервера и число папок
    Login,
    /// Показать дерево папок с сервера
    Folders,
    /// Синхронизировать почту в локальную базу
    Sync {
        /// Глубина в днях для загрузки тел и вложений (метаданные грузятся за всё время); 0 = без ограничения
        #[arg(long, default_value_t = 90)]
        days: u32,
        /// Какие папки синхронизировать: all (все почтовые кроме служебных) или список через запятую
        /// известных имён (inbox, sentitems) и отображаемых имён
        #[arg(long, default_value = "all")]
        folders: String,
        /// Только метаданные, без тел и вложений
        #[arg(long)]
        no_bodies: bool,
        /// Не скачивать вложения
        #[arg(long)]
        no_attachments: bool,
        /// Лимит размера одного вложения, МБ
        #[arg(long, default_value_t = 25)]
        max_attachment_mb: u64,
        /// Квота кэша файлов вложений на диске, МБ (текст остаётся в базе и после вытеснения)
        #[arg(long, default_value_t = 2048)]
        cache_mb: u64,
        /// Предел пачек SyncFolderItems на папку, 0 = без предела (для проб)
        #[arg(long, default_value_t = 0)]
        pages: u32,
    },
    /// Список бесед папки, как в OWA: тема, участники, писем, непрочитанных, дата последнего
    List {
        /// Папка: известное имя (inbox, sentitems) или отображаемое
        #[arg(long, default_value = "inbox")]
        folder: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Поиск по локальной базе. Операторы: от:/from:, кому:/to:, тема:/subject:,
    /// вложение:/has:attachment, папка:/in:, после:/до: (даты), остальное ищется по словам
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Скачать вложения одного письма с сервера (проба пути скачивания) и показать извлечённый текст
    FetchAttachments {
        /// item_pk письма в локальной базе (см. `search` или `sql`)
        #[arg(long)]
        item_pk: i64,
        #[arg(long, default_value_t = 25)]
        max_mb: u64,
    },
    /// Размер базы и покрытие по папкам
    Stats,
    /// Отладка: выполнить SELECT или PRAGMA к локальной базе и напечатать строки
    Sql { query: String },
    /// Пометить письмо прочитанным или непрочитанным на сервере (UpdateItem) и в базе
    MarkRead {
        /// item_pk письма в локальной базе
        #[arg(long)]
        item_pk: i64,
        /// true — прочитано, false — не прочитано
        #[arg(long, action = clap::ArgAction::Set)]
        read: bool,
    },
    /// Поставить или снять флаг «к исполнению» на сервере (UpdateItem) и в базе
    Flag {
        #[arg(long)]
        item_pk: i64,
        /// true — поставить, false — снять
        #[arg(long, action = clap::ArgAction::Set)]
        flagged: bool,
    },
    /// Перенести письмо в папку на сервере (MoveItem) и в базе. Без --yes-i-know только показывает, что сделает
    Move {
        #[arg(long)]
        item_pk: i64,
        /// Папка назначения: известное имя (inbox, deleteditems) или отображаемое
        #[arg(long)]
        to: String,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Удалить письмо на сервере (DeleteItem): в «Удалённые» или навсегда. Без --yes-i-know только показывает, что сделает
    Delete {
        #[arg(long)]
        item_pk: i64,
        /// Удалить навсегда (HardDelete) вместо переноса в «Удалённые»
        #[arg(long)]
        permanent: bool,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Создать почтовую папку на сервере (CreateFolder) и в базе. Без --yes-i-know только показывает, что сделает
    FolderCreate {
        /// Имя новой папки (1–255 символов, без \ / : * ? " < > |)
        #[arg(long)]
        name: String,
        /// Родитель: известное имя (inbox) или отображаемое; без флага — корень ящика
        #[arg(long)]
        parent: Option<String>,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Переименовать папку на сервере (UpdateFolder folder:DisplayName) и в базе. Служебные папки не переименовываются
    FolderRename {
        /// folder_pk папки в локальной базе (см. `sql "SELECT folder_pk, display_name FROM folders"`)
        #[arg(long)]
        folder_pk: i64,
        #[arg(long)]
        name: String,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Удалить папку с содержимым на сервере (DeleteFolder): в «Удалённые» или навсегда. Служебные папки не удаляются
    FolderDelete {
        #[arg(long)]
        folder_pk: i64,
        /// Удалить навсегда (HardDelete) вместо переноса в «Удалённые»
        #[arg(long)]
        permanent: bool,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Живая проверка операций с папками одной сессией: CreateFolder «AIOWA тест» в корне →
    /// SyncFolderHierarchy → UpdateFolder «AIOWA тест 2» → SyncFolderHierarchy → DeleteFolder
    /// HardDelete → SyncFolderHierarchy. Письма не трогаются. Без --yes-i-know только печатает план
    FolderCheck {
        /// Заодно проверить Reply-To: один GetItem (ItemPart + message:ReplyTo) по N свежим письмам
        /// «Входящих» из базы, только чтение; печатаются счётчики и форма без содержимого
        #[arg(long)]
        probe_reply_to: Option<usize>,
        /// Подтверждение: это запись в настоящий ящик
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Проверка Reply-To без записи: GetItem в форме ItemPart с message:ReplyTo по свежим письмам
    /// папки; печатаются только счётчики, наличие ключа и форма значения без содержимого
    ReplyToProbe {
        /// Папка: известное имя (inbox) или отображаемое
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// Сколько свежих писем взять (не больше 25 — одна пачка GetItem)
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Отправить новое письмо: черновик (CreateItem) → вложения (CreateAttachment) → отправка (SendItem).
    /// Без --yes-i-know только печатает план
    Send {
        /// Получатель; можно повторять флаг или перечислять адреса через запятую
        #[arg(long)]
        to: Vec<String>,
        /// Адресовать самому себе: самый частый адрес отправителя в «Отправленных» локальной базы
        #[arg(long)]
        to_self: bool,
        /// Копия; можно повторять
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        subject: String,
        /// Текст письма (обычный текст, переводы строк сохраняются)
        #[arg(long)]
        body: String,
        /// Файл-вложение; можно повторять. Лимит 25 МиБ на файл и 35 МиБ всего
        #[arg(long)]
        attach: Vec<PathBuf>,
        /// Подтверждение: письмо уйдёт из настоящего ящика
        #[arg(long)]
        yes_i_know: bool,
        /// При отказе отправки оставить черновик на сервере (по умолчанию удаляется навсегда)
        #[arg(long)]
        keep_draft: bool,
    },
    /// Живая проверка черновиков одной сессией: CreateItem «[AIOWA] черновик» → CreateAttachment
    /// (маленький текстовый файл) → GetItem черновика → UpdateItem (тема, тело, получатель — свой адрес)
    /// → GetItem → DeleteAttachment → GetItem → DeleteItem HardDelete. Ничего не отправляется.
    /// Без --yes-i-know только печатает план
    DraftCheck {
        /// Между шагами синхронизировать папку «Черновики» в локальную базу (SyncFolderItems, GetItem тел,
        /// вложения) и сверить строку тестового черновика: is_draft, получатели, вложения, тело; после
        /// удаления — что строка помечена удалённой
        #[arg(long)]
        sync_drafts: bool,
        /// Подтверждение: это запись в настоящий ящик (один тестовый черновик, он удаляется навсегда)
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Показать подпись пользователя из настроек OWA (GetUserConfiguration), ничего не сохраняя
    Signature,
    /// Подсказки получателей из локальной базы по началу адреса или слова имени
    Suggest {
        /// Начало адреса или слова имени; пусто — самые частые адреса
        #[arg(default_value = "")]
        prefix: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Справочник организации (только чтение): FindPeople по запросу, с --resolve ещё ResolveNames,
    /// с --expand — ExpandDL. Печатает только счётчики, ключи полей, коды и длительности, без
    /// значений; найденное пишется в кэш справочника локальной базы
    Directory {
        /// Строка поиска (в вывод не печатается); без неё — первое слово имени владельца ящика из
        /// «Отправленных» локальной базы
        #[arg(long)]
        query: Option<String>,
        /// Также ResolveNames по запросу (с полными данными и без) и по адресу первой найденной
        /// записи (как карточка контакта)
        #[arg(long)]
        resolve: bool,
        /// ExpandDL по адресу списка рассылки; `first` — первый список из результатов поиска, а если
        /// его нет — первый из частых получателей локальной базы, который ResolveNames назвал списком
        #[arg(long)]
        expand: Option<String>,
        /// Повторить поиск по первым трём буквам запроса
        #[arg(long)]
        prefix3: bool,
        /// Сколько записей просить у FindPeople
        #[arg(long, default_value_t = 25)]
        max: usize,
        /// Проверить дополнительные свойства записи FindPeople (`persona:Title` и др., по одному и
        /// вместе) и форму `ContactDataShape: Default` у ResolveNames
        #[arg(long)]
        probe_shapes: bool,
        /// Сколько кандидатов в списки рассылки из локальной базы проверять ResolveNames
        #[arg(long, default_value_t = 5)]
        dl_candidates: usize,
        /// Дополнительные запросы FindPeople для поиска списков рассылки (печатаются только
        /// счётчики); можно повторять
        #[arg(long)]
        dl_query: Vec<String>,
    },
    /// Календарь: события окна с сервера (FindItem с CalendarView) в кэш локальной базы и сводка
    /// без тем и имён; с --details N — GetItem по N-му событию списка (только чтение)
    Calendar {
        /// Начало окна: дата YYYY-MM-DD, местная полночь
        #[arg(long)]
        from: String,
        /// Конец окна (не включая): дата YYYY-MM-DD, местная полночь
        #[arg(long)]
        to: String,
        /// Какое событие списка запросить целиком (GetItem): номер с 1, `recurring` (первый экземпляр
        /// повторяющейся встречи) или `meeting` (первая встреча); через запятую — пробовать по очереди
        #[arg(long)]
        details: Option<String>,
        /// Не ходить на сервер: показать только кэш локальной базы
        #[arg(long)]
        offline: bool,
        /// Вместе с --details: запросить то же событие формой AllProperties и напечатать форму
        /// ответа без содержимого (ключи и длины) — для документации протокола
        #[arg(long)]
        probe: bool,
    },
    /// Календарь: создать событие на сервере (CreateItem CalendarItem) и перечитать окно дня в кэш.
    /// Без --yes-i-know только печатает план
    CalendarCreate {
        #[arg(long)]
        subject: String,
        /// Начало: YYYY-MM-DDTHH:MM местного времени (с --all-day — дата YYYY-MM-DD)
        #[arg(long)]
        start: String,
        /// Конец (не включая): тот же формат; с --all-day — дата следующего дня
        #[arg(long)]
        end: String,
        /// Событие на весь день: границы — местные полуночи
        #[arg(long)]
        all_day: bool,
        #[arg(long)]
        location: Option<String>,
        /// Напоминание за N минут до начала; без флага — без напоминания
        #[arg(long)]
        reminder: Option<i64>,
        /// Занятость: free, tentative, busy, oof, working_elsewhere
        #[arg(long, default_value = "busy")]
        free_busy: String,
        /// Конфиденциальность Private
        #[arg(long)]
        private: bool,
        /// Описание (обычный текст)
        #[arg(long)]
        body: Option<String>,
        /// Обязательные участники: адреса через запятую, флаг можно повторять. Сервер разошлёт приглашения!
        #[arg(long)]
        required: Vec<String>,
        /// Необязательные участники (так же)
        #[arg(long)]
        optional: Vec<String>,
        /// Подтверждение: это запись в настоящий календарь
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Календарь: изменить событие из кэша (UpdateItem по изменённым полям) и перечитать окна дней.
    /// Без --yes-i-know только печатает план
    CalendarUpdate {
        /// calendar_pk события в кэше (см. `calendar --from --to`)
        #[arg(long)]
        calendar_pk: i64,
        #[arg(long)]
        subject: Option<String>,
        /// Новое начало: YYYY-MM-DDTHH:MM местного времени (у события на весь день — дата)
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        location: Option<String>,
        /// Снять место
        #[arg(long)]
        clear_location: bool,
        /// Напоминание за N минут
        #[arg(long)]
        reminder: Option<i64>,
        /// Выключить напоминание
        #[arg(long)]
        no_reminder: bool,
        /// Занятость: free, tentative, busy, oof, working_elsewhere
        #[arg(long)]
        free_busy: Option<String>,
        /// Разослать участникам обновление (SendToAllAndSaveCopy); без флага — SendToNone
        #[arg(long)]
        send_updates: bool,
        /// Подтверждение: это запись в настоящий календарь
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Календарь: удалить событие из кэша на сервере (DeleteItem) и из кэша. Без --yes-i-know только печатает план
    CalendarDelete {
        #[arg(long)]
        calendar_pk: i64,
        /// Навсегда (HardDelete) вместо переноса в «Удалённые»
        #[arg(long)]
        permanent: bool,
        /// Разослать участникам отмену (SendToAllAndSaveCopy); без флага — SendToNone. Только для своей встречи: у чужой сервер отправил бы организатору отказ
        #[arg(long)]
        send_cancellations: bool,
        /// Подтверждение: это запись в настоящий календарь
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Живая проверка записи в календарь одной сессией в поясе пользователя из OWA: создать
    /// «[AIOWA] проверка календаря» без участников на 09:00–09:30 (или на весь день) с местом и
    /// напоминанием 15 мин → FindItem дня → перенести на 10:00–10:45 (у всё-день — сменить тему) и
    /// сменить место → FindItem → снять место → FindItem → удалить навсегда → FindItem. Без
    /// --yes-i-know только печатает план
    CalendarCheck {
        /// Дата проверки YYYY-MM-DD; по умолчанию завтра
        #[arg(long)]
        date: Option<String>,
        /// Событие на весь день указанной даты вместо 09:00–09:30
        #[arg(long)]
        all_day: bool,
        /// После каждой записи — GetItem AllProperties с полями времени, поясов и места
        /// (расширенное свойство PidLidLocation) тестового события
        #[arg(long)]
        probe: bool,
        /// Подтверждение: это запись в настоящий календарь
        #[arg(long)]
        yes_i_know: bool,
    },
    /// Письмо о встрече (GetItem по письму-приглашению, отмене или ответу): вид, время, свой ответ,
    /// связанное событие — только чтение, без тем и имён
    MeetingInfo {
        /// item_pk писем в локальной базе, через запятую
        #[arg(long)]
        item_pk: String,
        /// Запросить то же письмо формой AllProperties и напечатать форму ответа без содержимого
        /// (ключи и длины) — для документации протокола
        #[arg(long)]
        probe: bool,
    },
    /// Канал уведомлений: подписаться (SubscribeToNotification), читать поток ev.owa2 заданное
    /// время и печатать типы событий и счётчики (без содержимого писем), затем завершить поток
    Notify {
        /// Сколько секунд читать поток
        #[arg(long, default_value_t = 90)]
        seconds: u64,
        /// Папки для подписки на строки (RowNotification): известные имена (inbox, sentitems) или
        /// отображаемые, через запятую
        #[arg(long, default_value = "inbox")]
        folders: String,
        /// Только подписки на папки: без NewMail, Hierarchy, Unseen и Calendar
        #[arg(long)]
        rows_only: bool,
    },
}

/// Событие канала одной строкой без содержимого: вид, папка по имени, длины полей.
fn describe_event(ev: &NotificationEvent, folder_names: &std::collections::HashMap<String, String>) -> String {
    let folder_name = |f: &Option<FolderId>| f.as_ref().map(|f| folder_names.get(&f.id).cloned().unwrap_or_else(|| format!("папка {} символов", f.id.len()))).unwrap_or_else(|| "папка не указана".into());
    match ev {
        NotificationEvent::FolderRows { folder, event_type, .. } => format!("FolderRows: {}, EventType {}", folder_name(folder), if event_type.is_empty() { "?" } else { event_type }),
        NotificationEvent::NewMail { item_id, folder, from, subject, preview, is_read, .. } => format!(
            "NewMail: ItemId {}, {}, отправитель {}, тема {} символов, превью {} символов, is_read {:?}",
            item_id.as_ref().map(|i| format!("{} символов", i.id.len())).unwrap_or_else(|| "нет".into()),
            folder_name(folder),
            match from {
                Some(p) => format!("{}{}", if p.name.is_some() { "имя" } else { "" }, if p.email.is_some() { "+адрес" } else { "" }),
                None => "нет".into(),
            },
            subject.as_ref().map(|s| s.chars().count()).unwrap_or(0),
            preview.as_ref().map(|s| s.chars().count()).unwrap_or(0),
            is_read
        ),
        NotificationEvent::Hierarchy { folder, event_type, .. } => format!("Hierarchy: {}, EventType {}", folder_name(folder), if event_type.is_empty() { "?" } else { event_type }),
        NotificationEvent::Unseen { count, .. } => format!("Unseen: счётчик {:?}", count),
        NotificationEvent::Calendar { folder, .. } => format!("Calendar: {}", folder_name(folder)),
        NotificationEvent::KeepAlive => "KeepAlive".into(),
        NotificationEvent::Reload => "Reload (подписки потеряны)".into(),
        NotificationEvent::Unknown { raw_kind } => format!("Unknown: {raw_kind}"),
    }
}

/// Подписки для `notify`: строки каждой папки и, если не `rows_only`, остальные типы.
fn notify_subscriptions(folders: &[(FolderId, String)], calendar: Option<&FolderId>, rows_only: bool) -> Vec<SubscriptionRequest> {
    let mut subs: Vec<SubscriptionRequest> = folders.iter().map(|(f, _)| SubscriptionRequest { id: new_guid(), kind: NotificationKind::FolderRows { folder: f.clone() } }).collect();
    if !rows_only {
        subs.push(SubscriptionRequest { id: new_guid(), kind: NotificationKind::NewMail });
        subs.push(SubscriptionRequest { id: new_guid(), kind: NotificationKind::Hierarchy });
        subs.push(SubscriptionRequest { id: new_guid(), kind: NotificationKind::Unseen });
        if let Some(c) = calendar {
            subs.push(SubscriptionRequest { id: new_guid(), kind: NotificationKind::Calendar { folder: c.clone() } });
        }
    }
    subs
}

/// Подписаться и напечатать итог по каждой подписке.
async fn subscribe_and_report(t: &aiowa_transport_owa::OwaTransport, subs: &[SubscriptionRequest], folder_names: &std::collections::HashMap<String, String>) -> Result<usize> {
    let started = std::time::Instant::now();
    let results = t.subscribe_notifications(subs).await.map_err(|e| anyhow!("SubscribeToNotification: {e}"))?;
    let created = results.iter().filter(|r| r.created).count();
    println!("SubscribeToNotification: {} из {} за {} мс", created, subs.len(), started.elapsed().as_millis());
    for (s, r) in subs.iter().zip(&results) {
        let target = match &s.kind {
            NotificationKind::FolderRows { folder } | NotificationKind::Calendar { folder } => format!(" ({})", folder_names.get(&folder.id).cloned().unwrap_or_else(|| "календарь".into())),
            _ => String::new(),
        };
        println!("  {}{}: {}", s.kind.name(), target, if r.created { "создана".to_string() } else { format!("отказ: {}", r.error.as_deref().unwrap_or("?")) });
    }
    Ok(created)
}

/// Местная полночь даты `YYYY-MM-DD` как момент UTC (границы окон календаря).
fn local_midnight(date: &str) -> Result<jiff::Timestamp> {
    let d: jiff::civil::Date = date.trim().parse().with_context(|| format!("дата «{date}» не в формате YYYY-MM-DD"))?;
    Ok(d.to_zoned(jiff::tz::TimeZone::system()).map_err(|e| anyhow!("дата «{date}»: {e}"))?.timestamp())
}

/// Граница события «весь день» для транспорта: полночь UTC даты `YYYY-MM-DD` — так дата кодируется
/// моментом, а транспорт шлёт её местной полуночью пояса пользователя (§ 5i документации протокола).
fn all_day_bound(date: &str) -> Result<jiff::Timestamp> {
    let d: jiff::civil::Date = date.trim().parse().with_context(|| format!("дата «{date}» не в формате YYYY-MM-DD"))?;
    all_day_bound_date(d)
}

fn all_day_bound_date(d: jiff::civil::Date) -> Result<jiff::Timestamp> {
    Ok(d.to_zoned(jiff::tz::TimeZone::UTC).map_err(|e| anyhow!("дата «{d}»: {e}"))?.timestamp())
}

/// Пояс пользователя для записи в календарь: из кэша базы (`Store::owa_time_zone`, ключ
/// `OWA_TIME_ZONE_KEY` в `sync_state`); когда записи нет или она старше суток
/// (`OWA_TIME_ZONE_TTL_MS`) — из настроек OWA (`GetUserConfiguration`) с записью в кэш («в OWA
/// пояс не задан» тоже кэшируется). Если сервер не ответил, а устаревший пояс в кэше есть,
/// остаётся он.
async fn calendar_time_zone(t: &OwaTransport, store: &Store, account: AccountId) -> Result<Option<String>> {
    let cached = store.owa_time_zone(account)?;
    if let Some(c) = &cached
        && !c.stale
    {
        match &c.zone {
            Some(tz) => println!("Пояс пользователя (кэш базы): {tz}"),
            None => println!("Пояс пользователя (кэш базы): в OWA не задан, запись в календарь пойдёт в контексте UTC"),
        }
        return Ok(c.zone.clone());
    }
    let refreshed = cached.is_some();
    let started = std::time::Instant::now();
    let tz = match t.user_time_zone().await {
        Ok(tz) => tz,
        Err(e) => {
            if let Some(tz) = cached.and_then(|c| c.zone) {
                println!("GetUserConfiguration OWA.UserOptions: {e}; остаётся пояс из кэша базы (старше суток): {tz}");
                return Ok(Some(tz));
            }
            return Err(anyhow!("GetUserConfiguration OWA.UserOptions: {e}"));
        }
    };
    store.set_owa_time_zone(account, tz.as_deref())?;
    let saved = if refreshed { "кэш обновлён" } else { "записан в кэш" };
    match &tz {
        Some(z) => println!("Пояс пользователя (OWA.UserOptions за {} мс, {saved}): {z}", started.elapsed().as_millis()),
        None => println!("Пояс пользователя в OWA.UserOptions не задан ({} мс, {saved}): запись в календарь пойдёт в контексте UTC", started.elapsed().as_millis()),
    }
    Ok(tz)
}

fn fmt_local(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%d.%m %a %H:%M").to_string()).unwrap_or_else(|_| ms.to_string())
}

/// Проверка «конец позже начала» после правки (`calendar-update`) и строка «начало – конец» для
/// плана. У события на весь день границы сравниваются по календарным датам
/// (`all_day_civil_date`, конец исключая), потому что они в разных кодировках: новые из
/// `--start/--end` — полуночи UTC дат (`all_day_bound`), а недостающая берётся из кэша, где
/// лежат местные полуночи пояса пользователя с сервера (`…T21:00:00Z`, § 5i); сравнение по
/// миллисекундам пропустило бы `--end` той же даты. У обычного события — по моментам.
fn updated_span(is_all_day: bool, start_ms: i64, end_ms: i64) -> Result<String> {
    if is_all_day {
        let day = |ms: i64| Ok::<_, anyhow::Error>(aiowa_core::all_day_civil_date(jiff::Timestamp::from_millisecond(ms)?));
        let (start, end) = (day(start_ms)?, day(end_ms)?);
        if end <= start {
            bail!("последний день события должен быть позже первого ({start} – {end}, конец исключая)");
        }
        Ok(format!("{start} – {end} (весь день, конец исключая)"))
    } else {
        if end_ms <= start_ms {
            bail!("конец должен быть позже начала ({} – {})", fmt_local(start_ms), fmt_local(end_ms));
        }
        Ok(format!("{} – {}", fmt_local(start_ms), fmt_local(end_ms)))
    }
}

/// Сводка окна календаря из кэша: только счётчики и флаги, без тем, мест и имён.
fn print_calendar_rows(rows: &[aiowa_store::CalendarRow]) {
    let count = |f: &dyn Fn(&aiowa_store::CalendarRow) -> bool| rows.iter().filter(|r| f(r)).count();
    println!(
        "Событий {}: весь день {}, повторяющихся {} (экземпляров {}, исключений {}), встреч {}, отменённых {}, с вложениями {}, с местом {}, с подробностями {}",
        rows.len(),
        count(&|r| r.is_all_day),
        count(&|r| CalendarItemType::parse(&r.calendar_type).is_recurring()),
        count(&|r| r.calendar_type == "Occurrence"),
        count(&|r| r.calendar_type == "Exception"),
        count(&|r| r.is_meeting),
        count(&|r| r.is_cancelled),
        count(&|r| r.has_attachments),
        count(&|r| r.location.is_some()),
        count(&|r| r.has_details),
    );
    let mut by_busy: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut by_response: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut by_sensitivity: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in rows {
        *by_busy.entry(&r.free_busy).or_default() += 1;
        *by_response.entry(&r.my_response).or_default() += 1;
        *by_sensitivity.entry(r.sensitivity.as_deref().unwrap_or("(нет)")).or_default() += 1;
    }
    println!("Занятость: {by_busy:?}; мой ответ: {by_response:?}; конфиденциальность: {by_sensitivity:?}");
    println!("{:>3}  {:<16} {:<16} {:>5} {:<15} {:<16} {:<18} {:>6} {:>6} {:>4} {:>5} {:>5}", "#", "Начало (местн.)", "Конец", "Длит", "Тип", "Занятость", "Мой ответ", "Встр.", "Отмен", "Влож", "Тема", "Орг.");
    for (i, r) in rows.iter().enumerate() {
        let minutes = (r.end_at - r.start_at) / 60_000;
        let duration = if r.is_all_day { "день".to_string() } else if minutes % 60 == 0 { format!("{}ч", minutes / 60) } else { format!("{minutes}м") };
        println!(
            "{:>3}  {:<16} {:<16} {:>5} {:<15} {:<16} {:<18} {:>6} {:>6} {:>4} {:>5} {:>5}",
            i + 1,
            fmt_local(r.start_at),
            fmt_local(r.end_at),
            duration,
            r.calendar_type,
            r.free_busy,
            r.my_response,
            if r.is_meeting { "да" } else { "" },
            if r.is_cancelled { "да" } else { "" },
            if r.has_attachments { "да" } else { "" },
            r.subject.as_ref().map(|s| s.chars().count()).unwrap_or(0),
            if r.organizer_email.is_some() { "есть" } else { "" },
        );
    }
}

/// Местное время `YYYY-MM-DDTHH:MM[:SS]` (или с пробелом вместо `T`) как момент UTC.
fn local_datetime(s: &str) -> Result<jiff::Timestamp> {
    let mut text = s.trim().replacen(' ', "T", 1);
    if text.len() == "2026-10-25T09:00".len() {
        text.push_str(":00");
    }
    let dt: jiff::civil::DateTime = text.parse().with_context(|| format!("время «{s}» не в формате YYYY-MM-DDTHH:MM"))?;
    Ok(dt.to_zoned(jiff::tz::TimeZone::system()).map_err(|e| anyhow!("время «{s}»: {e}"))?.timestamp())
}

/// Местные сутки, содержащие момент: `[полночь, следующая полночь)` как моменты UTC.
fn local_day_window(t: jiff::Timestamp) -> Result<(jiff::Timestamp, jiff::Timestamp)> {
    let tz = jiff::tz::TimeZone::system();
    let date = t.to_zoned(tz.clone()).date();
    let start = date.to_zoned(tz.clone()).map_err(|e| anyhow!("{e}"))?.timestamp();
    let end = date.tomorrow().map_err(|e| anyhow!("{e}"))?.to_zoned(tz).map_err(|e| anyhow!("{e}"))?.timestamp();
    Ok((start, end))
}

/// Занятость из ключа интерфейса; неизвестный ключ — ошибка (а не `NoData`).
fn parse_free_busy(s: &str) -> Result<FreeBusy> {
    let fb = FreeBusy::parse(s);
    if fb == FreeBusy::NoData && !s.eq_ignore_ascii_case("no_data") && !s.eq_ignore_ascii_case("nodata") {
        bail!("занятость «{s}»: ожидается free, tentative, busy, oof или working_elsewhere");
    }
    Ok(fb)
}

/// Адреса участников из флагов с проверкой формы.
fn attendee_participants(values: &[String], what: &str) -> Result<Vec<Participant>> {
    let addresses = split_addresses(values);
    if let Some(bad) = addresses.iter().find(|a| !looks_like_email(a)) {
        bail!("{what}: адрес «{bad}» не похож на почтовый");
    }
    Ok(participants(&addresses))
}

/// Длительность события для печати: «день», «1ч», «45м».
fn fmt_duration(r: &CalendarRow) -> String {
    let minutes = (r.end_at - r.start_at) / 60_000;
    if r.is_all_day {
        "день".to_string()
    } else if minutes % 60 == 0 {
        format!("{}ч", minutes / 60)
    } else {
        format!("{minutes}м")
    }
}

/// Строка кэша календаря одной строкой без темы, места и имён: времена, флаги, длины, напоминание.
fn describe_calendar_row(store: &Store, r: &CalendarRow) -> Result<String> {
    let reminder = store.calendar_reminder_minutes(r.calendar_pk)?;
    Ok(format!(
        "calendar_pk {}: {} – {} ({}), весь день {}, тип {}, занятость {}, мой ответ {}, встреча {}, отменено {}, конфиденциальность {}, тема {} символов, место {}, напоминание {}, ChangeKey {}",
        r.calendar_pk,
        fmt_local(r.start_at),
        fmt_local(r.end_at),
        fmt_duration(r),
        if r.is_all_day { "да" } else { "нет" },
        r.calendar_type,
        r.free_busy,
        r.my_response,
        if r.is_meeting { "да" } else { "нет" },
        if r.is_cancelled { "да" } else { "нет" },
        r.sensitivity.as_deref().unwrap_or("(нет)"),
        r.subject.as_ref().map(|s| s.chars().count()).unwrap_or(0),
        r.location.as_ref().map(|l| format!("есть ({} символов)", l.chars().count())).unwrap_or_else(|| "нет".into()),
        reminder.map(|m| format!("за {m} мин")).unwrap_or_else(|| "нет".into()),
        r.id.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into()),
    ))
}

/// Перечитывает окно `[start, end)` с сервера в кэш и печатает счётчики; возвращает строки кэша окна.
async fn refresh_calendar_window(t: &OwaTransport, store: &Store, account: AccountId, start: jiff::Timestamp, end: jiff::Timestamp) -> Result<Vec<CalendarRow>> {
    let started = std::time::Instant::now();
    let events = t.find_calendar_view(start, end).await.map_err(|e| anyhow!("FindItem CalendarView: {e}"))?;
    let (start_ms, end_ms) = (start.as_millisecond(), end.as_millisecond());
    store.replace_calendar_range(account, start_ms, end_ms, &events)?;
    let rows = store.calendar_events(account, start_ms, end_ms)?;
    println!("FindItem CalendarView {} – {}: {} событий за {} мс, в кэше окна {}", fmt_local(start_ms), fmt_local(end_ms), events.len(), started.elapsed().as_millis(), rows.len());
    Ok(rows)
}

/// Перечитывает окно дня события и печатает его строку из кэша (или что её нет).
async fn refresh_and_show(t: &OwaTransport, store: &Store, account: AccountId, at: jiff::Timestamp, id: &ItemId, expect_present: bool) -> Result<Option<CalendarRow>> {
    let (ds, de) = local_day_window(at)?;
    let rows = refresh_calendar_window(t, store, account, ds, de).await?;
    let found = rows.into_iter().find(|r| r.id.id == id.id);
    match (&found, expect_present) {
        (Some(r), true) => println!("  в окне дня есть: {}", describe_calendar_row(store, r)?),
        (Some(r), false) => println!("  ОШИБКА: событие всё ещё в окне дня: {}", describe_calendar_row(store, r)?),
        (None, true) => println!("  ОШИБКА: событие в окне дня не найдено"),
        (None, false) => println!("  в окне дня события нет (как и ожидалось)"),
    }
    Ok(found)
}

/// Тема события живой проверки записи в календарь.
const CALENDAR_CHECK_SUBJECT: &str = "[AIOWA] проверка календаря";
/// Места тестового события: при создании и после изменения.
const CALENDAR_CHECK_LOCATION: &str = "Кабинет AIOWA";
const CALENDAR_CHECK_LOCATION_2: &str = "Переговорная AIOWA 2";

/// Что проверяется у тестового события по `GetItem AllProperties` (`--probe`).
struct ProbeExpect<'a> {
    /// Ожидаемое место (`None` — места быть не должно).
    location: Option<&'a str>,
    /// Дата события на весь день: `StartWallClock` должен начинаться с её полуночи.
    all_day: Option<jiff::civil::Date>,
    /// Запись шла в контексте пояса пользователя: `StartTimeZoneId` не должен быть `UTC`.
    user_zone: bool,
}

/// Место из ответа: строка `Location` или `DisplayName` объекта, иначе значение расширенного
/// свойства `PidLidLocation` (`ExtendedProperty[].Value`).
fn probe_location(fields: &serde_json::Value) -> Option<String> {
    let clean = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(loc) = fields.get("Location") {
        let direct = loc.as_str().map(String::from).or_else(|| loc.get("DisplayName").and_then(|d| d.as_str()).map(String::from)).and_then(|s| clean(&s));
        if direct.is_some() {
            return direct;
        }
    }
    fields.get("ExtendedProperty").and_then(|e| e.as_array()).into_iter().flatten().find_map(|p| p.get("Value").and_then(|v| v.as_str()).and_then(clean))
}

/// `GetItem AllProperties` тестового события: печатает поля времени, поясов и места (это своё
/// событие с известным содержимым) и сверяет их с ожиданием. Возвращает, всё ли совпало.
async fn probe_fields(t: &OwaTransport, id: &ItemId, label: &str, expect: &ProbeExpect<'_>) -> Result<bool> {
    let started = std::time::Instant::now();
    let fields = t.probe_calendar_item_fields(id).await.map_err(|e| anyhow!("GetItem AllProperties ({label}): {e}"))?;
    println!("  GetItem AllProperties {label} за {} мс: {fields}", started.elapsed().as_millis());
    let zone_id = fields.get("StartTimeZoneId").and_then(|v| v.as_str()).unwrap_or("");
    let zone_ok = if expect.user_zone { zone_id.len() > 3 && fields.get("EndTimeZoneId").and_then(|v| v.as_str()).is_some_and(|z| z.len() > 3) } else { true };
    let all_day_ok = match expect.all_day {
        Some(d) => {
            let wall = fields.get("StartWallClock").and_then(|v| v.as_str()).unwrap_or("");
            fields.get("IsAllDayEvent").and_then(|v| v.as_bool()) == Some(true) && wall.starts_with(&format!("{d}T00:00:00"))
        }
        None => true,
    };
    let location = probe_location(&fields);
    let location_ok = location.as_deref() == expect.location;
    println!(
        "    пояс {} ({}), весь день {}, место {}",
        if zone_ok { "ок" } else { "НЕ ок" },
        if zone_id.is_empty() { "StartTimeZoneId нет" } else { zone_id },
        match expect.all_day {
            Some(_) if all_day_ok => "ок (StartWallClock — местная полночь даты)",
            Some(_) => "НЕ ок",
            None => "не проверяется",
        },
        match (expect.location, &location) {
            (Some(_), Some(_)) if location_ok => "совпадает",
            (Some(_), Some(_)) => "ДРУГОЕ",
            (Some(_), None) => "НЕТ",
            (None, None) => "снято",
            (None, Some(_)) => "НЕ снято",
        }
    );
    Ok(zone_ok && all_day_ok && location_ok)
}

/// `UpdateItem` тестового события без прерывания проверки: при отказе печатает ошибку и
/// возвращает `None`.
async fn try_update(t: &OwaTransport, id: &ItemId, label: &str, changes: &CalendarItemChanges, zone: Option<&str>) -> Option<ItemId> {
    let started = std::time::Instant::now();
    match t.update_calendar_item(id, changes, false, zone).await {
        Ok(updated) => {
            println!("UpdateItem ({label}): Ok за {} мс; ItemId {}, ChangeKey {}", started.elapsed().as_millis(), if updated.id == id.id { "тот же" } else { "другой" }, describe_change_key(id, &updated));
            Some(updated)
        }
        Err(e) => {
            println!("UpdateItem ({label}): ОТКАЗ за {} мс: {e}", started.elapsed().as_millis());
            None
        }
    }
}

/// Живая проверка одной сессией: пояс пользователя → создать (с местом) → FindItem → сменить
/// место и время (у всё-день — тему) → FindItem → снять место → FindItem → удалить навсегда →
/// FindItem. С `--probe` после каждой записи — `GetItem AllProperties` с `PidLidLocation`. При
/// сбое после создания событие всё равно удаляется. Четыре записи на сервер.
async fn calendar_check_flow(t: &OwaTransport, store: &Store, account: AccountId, date: jiff::civil::Date, all_day: bool, probe: bool) -> Result<()> {
    let tz = jiff::tz::TimeZone::system();
    let at = |h: i8, m: i8| -> Result<jiff::Timestamp> { Ok(date.at(h, m, 0, 0).to_zoned(tz.clone()).map_err(|e| anyhow!("{e}"))?.timestamp()) };
    let zone = calendar_time_zone(t, store, account).await?;
    let (start, end) = if all_day { (all_day_bound_date(date)?, all_day_bound_date(date.tomorrow().map_err(|e| anyhow!("{e}"))?)?) } else { (at(9, 0)?, at(9, 30)?) };
    let location = Some(CALENDAR_CHECK_LOCATION);
    let item = NewCalendarItem {
        subject: CALENDAR_CHECK_SUBJECT.into(),
        start,
        end,
        is_all_day: all_day,
        location: location.map(String::from),
        body_html: Some("<div>Проверка записи в календарь клиентом AIOWA. Событие будет удалено автоматически.</div>".into()),
        required: vec![],
        optional: vec![],
        reminder_minutes: Some(15),
        free_busy: FreeBusy::Tentative,
        sensitivity_private: false,
    };
    let started = std::time::Instant::now();
    let id = t.create_calendar_item(&item, zone.as_deref()).await.map_err(|e| anyhow!("CreateItem CalendarItem: {e}"))?;
    println!(
        "CreateItem CalendarItem: Ok за {} мс; ItemId {} символов, ChangeKey {}",
        started.elapsed().as_millis(),
        id.id.len(),
        id.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into())
    );
    let all_day_date_of = |ms: i64| jiff::Timestamp::from_millisecond(ms).ok().map(aiowa_core::all_day_civil_date);
    // Время строки кэша совпало с запросом: у всё-день — по датам ближайших полуночей, иначе по моментам.
    let time_matches = |row: &CalendarRow, s: jiff::Timestamp, e: jiff::Timestamp| if all_day { (all_day_date_of(row.start_at), all_day_date_of(row.end_at)) == (Some(aiowa_core::all_day_civil_date(s)), Some(aiowa_core::all_day_civil_date(e))) } else { row.start_at == s.as_millisecond() && row.end_at == e.as_millisecond() };
    let user_zone = zone.is_some();
    let outcome = async {
        let mut all_ok = true;
        if probe {
            all_ok &= probe_fields(t, &id, "после CreateItem", &ProbeExpect { location, all_day: all_day.then_some(date), user_zone }).await?;
        }
        let row = refresh_and_show(t, store, account, start, &id, true).await?.ok_or_else(|| anyhow!("созданное событие не пришло в FindItem окна дня"))?;
        let reminder_ok = store.calendar_reminder_minutes(row.calendar_pk)? == Some(15);
        let location_ok = row.location.as_deref() == location;
        println!(
            "  проверка создания: время {}, весь день {}, занятость Tentative {}, напоминание 15 {}, место {}",
            if time_matches(&row, start, end) { "совпало" } else { "НЕ совпало" },
            if row.is_all_day == all_day { "как задано" } else { "НЕ как задано" },
            if row.free_busy == "Tentative" { "принята" } else { "НЕ принята" },
            if reminder_ok { "есть" } else { "НЕТ" },
            match (location, &row.location) {
                (Some(_), Some(_)) if location_ok => "совпадает",
                (Some(_), Some(_)) => "ДРУГОЕ",
                (Some(_), None) => "НЕТ",
                (None, None) => "не задавалось",
                (None, Some(_)) => "ЕСТЬ, хотя не задавалось",
            }
        );
        all_ok &= time_matches(&row, start, end) && row.is_all_day == all_day && row.free_busy == "Tentative" && reminder_ok && location_ok;
        let mut current = row.id.clone();
        let mut calendar_pk = row.calendar_pk;
        // Изменение 1: место и время (у всё-день — тема, время не трогается).
        let (new_start, new_end) = if all_day { (start, end) } else { (at(10, 0)?, at(10, 45)?) };
        let changes = if all_day {
            CalendarItemChanges { subject: Some(format!("{CALENDAR_CHECK_SUBJECT} (изменено)")), location: Some(Some(CALENDAR_CHECK_LOCATION_2.into())), ..Default::default() }
        } else {
            CalendarItemChanges { start: Some(new_start), end: Some(new_end), location: Some(Some(CALENDAR_CHECK_LOCATION_2.into())), ..Default::default() }
        };
        match try_update(t, &current, "место и время", &changes, zone.as_deref()).await {
            Some(updated) => {
                current = updated;
                if probe {
                    all_ok &= probe_fields(t, &current, "после UpdateItem (место и время)", &ProbeExpect { location: Some(CALENDAR_CHECK_LOCATION_2), all_day: all_day.then_some(date), user_zone }).await?;
                }
                let row = refresh_and_show(t, store, account, new_start, &current, true).await?.ok_or_else(|| anyhow!("изменённое событие не пришло в FindItem окна дня"))?;
                let reminder_ok = store.calendar_reminder_minutes(row.calendar_pk)? == Some(15);
                let location_ok = row.location.as_deref() == Some(CALENDAR_CHECK_LOCATION_2);
                println!(
                    "  проверка изменения: время {}, место {}, напоминание {}",
                    if time_matches(&row, new_start, new_end) { "совпало" } else { "НЕ совпало" },
                    if location_ok { "совпадает" } else if row.location.is_some() { "ДРУГОЕ" } else { "НЕТ" },
                    if reminder_ok { "сохранилось" } else { "ПРОПАЛО" }
                );
                all_ok &= time_matches(&row, new_start, new_end) && location_ok && reminder_ok;
                current = row.id.clone();
                calendar_pk = row.calendar_pk;
            }
            None => all_ok = false,
        }
        // Изменение 2: снять место.
        match try_update(t, &current, "снятие места", &CalendarItemChanges { location: Some(None), ..Default::default() }, zone.as_deref()).await {
            Some(updated) => {
                current = updated;
                if probe {
                    all_ok &= probe_fields(t, &current, "после UpdateItem (место снято)", &ProbeExpect { location: None, all_day: all_day.then_some(date), user_zone }).await?;
                }
                let row = refresh_and_show(t, store, account, new_start, &current, true).await?.ok_or_else(|| anyhow!("событие без места не пришло в FindItem окна дня"))?;
                println!("  проверка снятия места: место {}", if row.location.is_none() { "снято" } else { "НЕ снято" });
                all_ok &= row.location.is_none();
                current = row.id.clone();
                calendar_pk = row.calendar_pk;
            }
            None => all_ok = false,
        }
        Ok::<(bool, ItemId, i64), anyhow::Error>((all_ok, current, calendar_pk))
    }
    .await;
    // Удаление навсегда — всегда, даже после сбоя проверок.
    let (checks_ok, current, calendar_pk) = match outcome {
        Ok(v) => v,
        Err(e) => {
            println!("Сбой после создания: {e}. Удаляю событие навсегда.");
            (false, ItemId { id: id.id.clone(), change_key: None }, 0)
        }
    };
    let started = std::time::Instant::now();
    t.delete_calendar_item_with(&current, DeleteMode::Hard, false).await.map_err(|e| anyhow!("DeleteItem HardDelete события: {e} (событие могло остаться в календаре!)"))?;
    println!("DeleteItem HardDelete: Ok за {} мс", started.elapsed().as_millis());
    if calendar_pk != 0 {
        store.remove_calendar_event(calendar_pk)?;
    }
    let gone = refresh_and_show(t, store, account, start, &current, false).await?.is_none();
    if checks_ok && gone {
        println!("Живая проверка календаря пройдена: создание, изменение, снятие места и удаление подтверждены.");
        Ok(())
    } else {
        bail!("живая проверка календаря не пройдена (см. строки с ОШИБКА/НЕ выше)")
    }
}

/// Имена тестовой папки живой проверки (`folder-check`).
const FOLDER_CHECK_NAME: &str = "AIOWA тест";
const FOLDER_CHECK_NAME_2: &str = "AIOWA тест 2";

/// Инкрементальная синхронизация иерархии в локальную базу (как `Engine::sync_hierarchy`), но с
/// возвратом самих изменений: по ним проверяется, что сервер показал созданную, переименованную
/// или удалённую папку. Недействительное состояние — одна полная выгрузка.
async fn refresh_hierarchy(t: &OwaTransport, store: &Store, account: AccountId) -> Result<Vec<FolderChange>> {
    let mut state = store.get_state(account, "hierarchy")?.map(SyncState);
    let mut out = Vec::new();
    let mut reset_once = false;
    for _ in 0..50 {
        let changes = match t.sync_folder_hierarchy(state.as_ref()).await {
            Ok(c) => c,
            Err(e) if e.invalidates_sync_state() && !reset_once => {
                println!("  SyncFolderHierarchy: состояние недействительно ({e}), полная выгрузка");
                reset_once = true;
                state = None;
                continue;
            }
            Err(e) => return Err(anyhow!("SyncFolderHierarchy: {e}")),
        };
        store.apply_hierarchy_batch(account, &changes.changes, &changes.state)?;
        let done = changes.includes_last || changes.changes.is_empty();
        state = Some(changes.state);
        out.extend(changes.changes);
        if done {
            break;
        }
    }
    Ok(out)
}

/// Что сервер сообщил о папке `id` в изменениях иерархии: `Create`, `Update`, `Delete` (последнее
/// упоминание) с именем папки у первых двух; `None` — папка не упоминалась.
fn hierarchy_mention<'a>(changes: &'a [FolderChange], id: &str) -> Option<(&'static str, Option<&'a str>)> {
    changes.iter().rev().find_map(|c| match c {
        FolderChange::Create(f) if f.id.id == id => Some(("Create", Some(f.display_name.as_str()))),
        FolderChange::Update(f) if f.id.id == id => Some(("Update", Some(f.display_name.as_str()))),
        FolderChange::Delete(fid) if fid.id == id => Some(("Delete", None)),
        _ => None,
    })
}

/// Строка папки для вставки в базу после `CreateFolder`: класс почтовый, счётчики нули.
fn new_mail_folder(id: &FolderId, parent_server_id: Option<&str>, display_name: &str) -> aiowa_core::Folder {
    aiowa_core::Folder {
        id: id.clone(),
        parent_id: parent_server_id.map(|p| FolderId { id: p.to_string(), change_key: None }),
        display_name: display_name.to_string(),
        distinguished: None,
        folder_class: Some(aiowa_transport_owa::protocol::MAIL_FOLDER_CLASS.to_string()),
        total_count: 0,
        unread_count: 0,
        child_folder_count: 0,
    }
}

/// Родитель для `CreateFolder`: по имени из базы или корень ящика (`msgfolderroot`). Возвращает
/// ссылку для сервера, `server_id` родителя для локальной строки и подпись для вывода.
fn resolve_parent(store: &Store, account: AccountId, parent: Option<&str>) -> Result<(FolderRef, Option<String>, String)> {
    match parent {
        Some(name) => {
            let f = store.resolve_folder(account, name)?.ok_or_else(|| anyhow!("папка «{name}» не найдена в локальной базе"))?;
            Ok((FolderRef::Id(FolderId { id: f.server_id.clone(), change_key: None }), Some(f.server_id), format!("папке «{}» (folder_pk {})", f.name, f.folder_pk)))
        }
        None => Ok((FolderRef::Distinguished(aiowa_core::DistinguishedFolder::MsgFolderRoot), store.msgfolderroot_server_id(account)?, "корне ящика (msgfolderroot)".to_string())),
    }
}

/// Лежит ли папка `folder_pk` внутри папки `ancestor_pk` (на любой глубине) по локальной базе.
fn folder_is_under(store: &Store, account: AccountId, folder_pk: i64, ancestor_pk: i64) -> Result<bool> {
    let folders = store.list_folders(account)?;
    let by_id: std::collections::HashMap<&str, &(i64, aiowa_core::Folder)> = folders.iter().map(|row| (row.1.id.id.as_str(), row)).collect();
    let mut cur = folders.iter().find(|(pk, _)| *pk == folder_pk).and_then(|(_, f)| f.parent_id.as_ref()).map(|p| p.id.as_str());
    for _ in 0..64 {
        let Some(pid) = cur else { return Ok(false) };
        let Some((pk, f)) = by_id.get(pid) else { return Ok(false) };
        if *pk == ancestor_pk {
            return Ok(true);
        }
        cur = f.parent_id.as_ref().map(|p| p.id.as_str());
    }
    Ok(false)
}

fn describe_folder_id(id: &FolderId) -> String {
    format!("FolderId {} символов, ChangeKey {}", id.id.len(), id.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into()))
}

/// Проверка Reply-To по свежим письмам папки (только чтение): один `GetItem` в форме `ItemPart` с
/// `message:ReplyTo` — принял ли сервер форму, у скольких писем есть ключ `ReplyTo`, у скольких он
/// непуст, форма значения без содержимого; затем обычный `get_items` по первым пяти — что разбор
/// доносит `reply_to` до `MessageFull`.
async fn reply_to_probe(t: &OwaTransport, store: &Store, account: AccountId, folder: &str, limit: usize) -> Result<()> {
    let f = store.resolve_folder(account, folder)?.ok_or_else(|| anyhow!("папка «{folder}» не найдена в локальной базе"))?;
    let rows = store.list_items(account, f.folder_pk, limit.clamp(1, 25))?;
    if rows.is_empty() {
        bail!("в папке «{}» нет писем в локальной базе", f.name);
    }
    let ids: Vec<ItemId> = rows.iter().map(|r| r.item_id.clone()).collect();
    let started = std::time::Instant::now();
    let probes = t.probe_reply_to(&ids).await.map_err(|e| anyhow!("GetItem ItemPart + message:ReplyTo отвергнут: {e}"))?;
    let errors = probes.iter().filter(|p| p.error.is_some()).count();
    let with_key = probes.iter().filter(|p| p.has_key).count();
    let non_empty = probes.iter().filter(|p| p.count > 0).count();
    let differing = probes.iter().filter(|p| p.differs_from_sender).count();
    println!(
        "GetItem ItemPart + message:ReplyTo по {} письмам «{}»: Ok за {} мс; отказов по письму {}, ключ ReplyTo есть у {}, непустой у {}, отличается от отправителя у {}; ключей у элемента {}",
        probes.len(),
        f.name,
        started.elapsed().as_millis(),
        errors,
        with_key,
        non_empty,
        differing,
        probes.iter().map(|p| p.item_keys).max().unwrap_or(0)
    );
    if let Some(p) = probes.iter().find(|p| p.count > 0) {
        println!("  форма непустого ReplyTo (строки — длинами): {}", p.shape);
    }
    if let Some(p) = probes.iter().find(|p| p.has_key && p.count == 0) {
        println!("  форма пустого ReplyTo: {}", p.shape);
    } else if with_key < probes.len() - errors {
        println!("  без Reply-To ключа ReplyTo в элементе нет (поле опускается)");
    }
    for p in probes.iter().filter(|p| p.error.is_some()).take(3) {
        println!("  отказ по письму: {}", p.error.as_deref().unwrap_or("?"));
    }
    let sample: Vec<ItemId> = ids.iter().take(5).cloned().collect();
    let started = std::time::Instant::now();
    let results = t.get_items(&sample, aiowa_transport::ItemFetchShape::WithUniqueBody).await.map_err(|e| anyhow!("GetItem (get_items): {e}"))?;
    let parsed_non_empty = results.iter().filter(|r| matches!(r, aiowa_transport::ItemResult::Ok(m) if !m.reply_to.is_empty())).count();
    println!("get_items WithUniqueBody по {} письмам: Ok за {} мс; с непустым reply_to {}", results.len(), started.elapsed().as_millis(), parsed_non_empty);
    Ok(())
}

/// Живая проверка папок одной сессией: создать «AIOWA тест» в корне → иерархия → переименовать в
/// «AIOWA тест 2» → иерархия → удалить навсегда → иерархия. Удаление выполняется и после сбоя
/// промежуточных шагов, чтобы тестовая папка не осталась в ящике.
async fn folder_check_flow(t: &OwaTransport, store: &Store, account: AccountId) -> Result<()> {
    let pre = refresh_hierarchy(t, store, account).await?;
    println!("SyncFolderHierarchy до проверки: изменений {}", pre.len());
    let root = store.msgfolderroot_server_id(account)?;
    if let Some(existing) = store.resolve_folder(account, FOLDER_CHECK_NAME)?.or(store.resolve_folder(account, FOLDER_CHECK_NAME_2)?) {
        bail!("папка «{}» уже есть в ящике (folder_pk {}): удалите её вручную или командой folder-delete --permanent", existing.name, existing.folder_pk);
    }
    let started = std::time::Instant::now();
    let id = t.create_folder(&FolderRef::Distinguished(aiowa_core::DistinguishedFolder::MsgFolderRoot), FOLDER_CHECK_NAME).await.map_err(|e| anyhow!("CreateFolder: {e}"))?;
    println!("CreateFolder «{FOLDER_CHECK_NAME}» в msgfolderroot: Ok за {} мс; {}", started.elapsed().as_millis(), describe_folder_id(&id));
    // Локальная вставка — внутри блока с остальными шагами: её сбой тоже не должен оставить
    // тестовую папку в ящике. Ключ запоминается снаружи, чтобы и после сбоя убрать строку локально.
    let mut pk: Option<i64> = None;
    let outcome = async {
        let new_pk = store.insert_folder(account, &new_mail_folder(&id, root.as_deref(), FOLDER_CHECK_NAME))?;
        pk = Some(new_pk);
        let pk = new_pk;
        println!("  локально folder_pk {pk}, родитель {}", if root.is_some() { "корень из базы" } else { "неизвестен (NULL)" });
        let mut all_ok = true;
        let changes = refresh_hierarchy(t, store, account).await?;
        let seen = hierarchy_mention(&changes, &id.id);
        println!("  SyncFolderHierarchy после создания: изменений {}, тестовая папка: {}", changes.len(), seen.map(|(k, n)| format!("{k}{}", if n == Some(FOLDER_CHECK_NAME) { ", имя совпало" } else { ", имя ДРУГОЕ" })).unwrap_or_else(|| "НЕ упомянута".into()));
        all_ok &= matches!(seen, Some(("Create" | "Update", Some(FOLDER_CHECK_NAME))));
        let local = store.folder_by_pk(account, pk)?;
        all_ok &= local.as_ref().map(|f| f.name == FOLDER_CHECK_NAME).unwrap_or(false);
        println!("  в базе после иерархии: {}", local.map(|f| format!("есть, имя {}", if f.name == FOLDER_CHECK_NAME { "совпало" } else { "ДРУГОЕ" })).unwrap_or_else(|| "НЕТ".into()));

        let started = std::time::Instant::now();
        let renamed = t.rename_folder(&id, FOLDER_CHECK_NAME_2).await.map_err(|e| anyhow!("UpdateFolder: {e}"))?;
        println!("UpdateFolder «{FOLDER_CHECK_NAME_2}»: Ok за {} мс; в ответе {}, Id {}", started.elapsed().as_millis(), describe_folder_id(&renamed), if renamed.id == id.id { "тот же" } else { "ДРУГОЙ" });
        store.rename_folder_local(pk, FOLDER_CHECK_NAME_2, Some(&renamed))?;
        let changes = refresh_hierarchy(t, store, account).await?;
        let seen = hierarchy_mention(&changes, &renamed.id);
        println!("  SyncFolderHierarchy после переименования: изменений {}, тестовая папка: {}", changes.len(), seen.map(|(k, n)| format!("{k}{}", if n == Some(FOLDER_CHECK_NAME_2) { ", имя совпало" } else { ", имя ДРУГОЕ" })).unwrap_or_else(|| "НЕ упомянута".into()));
        all_ok &= matches!(seen, Some(("Create" | "Update", Some(FOLDER_CHECK_NAME_2))));
        let local = store.folder_by_pk(account, pk)?;
        all_ok &= local.as_ref().map(|f| f.name == FOLDER_CHECK_NAME_2).unwrap_or(false);
        println!("  в базе после иерархии: {}", local.map(|f| format!("есть, имя {}", if f.name == FOLDER_CHECK_NAME_2 { "совпало" } else { "ДРУГОЕ" })).unwrap_or_else(|| "НЕТ".into()));
        Ok::<(bool, FolderId), anyhow::Error>((all_ok, renamed))
    }
    .await;
    let (checks_ok, current) = match outcome {
        Ok(v) => v,
        Err(e) => {
            println!("Сбой после создания: {e}. Удаляю тестовую папку навсегда.");
            (false, FolderId { id: id.id.clone(), change_key: None })
        }
    };
    let started = std::time::Instant::now();
    t.delete_folder(&current, true).await.map_err(|e| anyhow!("DeleteFolder HardDelete: {e} (папка «AIOWA тест» могла остаться в ящике!)"))?;
    println!("DeleteFolder HardDelete: Ok за {} мс", started.elapsed().as_millis());
    if let Some(pk) = pk {
        match store.delete_folder_local(pk, None) {
            Ok(st) => println!("  локально: папок помечено {}, писем {}", st.folders, st.items),
            Err(e) => println!("  локально удалить не удалось: {e} (иерархия ниже поправит)"),
        }
    }
    let changes = refresh_hierarchy(t, store, account).await?;
    let seen = hierarchy_mention(&changes, &current.id);
    let gone_by_pk = match pk {
        Some(pk) => store.folder_by_pk(account, pk)?.is_none(),
        None => true,
    };
    let gone_locally = gone_by_pk && store.resolve_folder(account, FOLDER_CHECK_NAME)?.is_none() && store.resolve_folder(account, FOLDER_CHECK_NAME_2)?.is_none();
    println!(
        "  SyncFolderHierarchy после удаления: изменений {}, тестовая папка: {}; в базе {}",
        changes.len(),
        seen.map(|(k, _)| k.to_string()).unwrap_or_else(|| "не упомянута".into()),
        if gone_locally { "отсутствует" } else { "ЕЩЁ ЕСТЬ" }
    );
    let delete_ok = matches!(seen, Some(("Delete", _)) | None) && gone_locally;
    if checks_ok && delete_ok {
        println!("Живая проверка папок пройдена: создание, переименование и удаление подтверждены иерархией.");
        Ok(())
    } else {
        bail!("живая проверка папок не пройдена (см. строки с НЕ/ДРУГОЕ выше)")
    }
}

/// Лимиты вложений при отправке, как в композере окна.
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ATTACHMENTS_TOTAL_BYTES: u64 = 35 * 1024 * 1024;

/// Грубая проверка почтового адреса: одна `@`, непустые части, точка в домене, без пробелов.
fn looks_like_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !s.chars().any(char::is_whitespace) && !domain.contains('@')
}

/// Адреса из флагов: каждый флаг может содержать несколько адресов через запятую или точку с запятой.
fn split_addresses(values: &[String]) -> Vec<String> {
    values.iter().flat_map(|v| v.split([',', ';'])).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn participants(addresses: &[String]) -> Vec<Participant> {
    addresses.iter().map(|a| Participant { name: None, email: Some(a.clone()), routing_type: None }).collect()
}

/// Обычный текст в HTML: экранирование и `<div>` на строку (пустая строка — `<div><br></div>`, как в OWA).
fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.lines() {
        let escaped: String = line.chars().map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            c => c.to_string(),
        }).collect();
        if escaped.trim().is_empty() {
            out.push_str("<div><br></div>");
        } else {
            out.push_str("<div>");
            out.push_str(&escaped);
            out.push_str("</div>");
        }
    }
    out
}

/// MIME-тип по расширению файла для частых случаев; остальное — `application/octet-stream`.
fn content_type_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("txt") | Some("log") | Some("md") => "text/plain",
        Some("csv") => "text/csv",
        Some("htm") | Some("html") => "text/html",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("zip") => "application/zip",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// Читает вложения с проверкой лимитов.
fn load_attachments(paths: &[PathBuf]) -> Result<Vec<OutgoingAttachment>> {
    let mut out = Vec::with_capacity(paths.len());
    let mut total = 0u64;
    for p in paths {
        let bytes = std::fs::read(p).with_context(|| format!("не удалось прочитать вложение {}", p.display()))?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            bail!("вложение {} больше {} МиБ", p.display(), MAX_ATTACHMENT_BYTES / 1024 / 1024);
        }
        total += bytes.len() as u64;
        if total > MAX_ATTACHMENTS_TOTAL_BYTES {
            bail!("вложения вместе больше {} МиБ", MAX_ATTACHMENTS_TOTAL_BYTES / 1024 / 1024);
        }
        let name = p.file_name().and_then(|n| n.to_str()).map(String::from).ok_or_else(|| anyhow!("у вложения {} нет имени файла", p.display()))?;
        out.push(OutgoingAttachment { name, content_type: Some(content_type_for(p).to_string()), bytes });
    }
    Ok(out)
}

/// Черновик → вложения → отправка. При отказе после создания черновика удаляет его навсегда
/// (если не `keep_draft`), чтобы в «Черновиках» не копились следы неудачных попыток.
async fn send_flow(t: &OwaTransport, msg: &OutgoingMessage, files: &[OutgoingAttachment], keep_draft: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let draft = t.create_draft(&ComposeKind::New, msg).await.map_err(|e| anyhow!("CreateItem: {e}"))?;
    println!(
        "CreateItem: Ok за {} мс; ItemId {} символов, ChangeKey {}",
        started.elapsed().as_millis(),
        draft.id.len(),
        draft.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into())
    );
    let result = async {
        let mut current = draft.clone();
        if !files.is_empty() {
            let started = std::time::Instant::now();
            current = t.add_attachments(&draft, files).await.map_err(|e| anyhow!("CreateAttachment: {e}"))?;
            println!(
                "CreateAttachment: Ok, файлов {} за {} мс; RootItemId {}, RootItemChangeKey {}",
                files.len(),
                started.elapsed().as_millis(),
                if current.id == draft.id { "тот же" } else { "другой" },
                describe_change_key(&draft, &current)
            );
        }
        let started = std::time::Instant::now();
        t.send_draft(&current).await.map_err(|e| anyhow!("SendItem: {e}"))?;
        println!("SendItem: Ok за {} мс. Письмо отправлено, копия в «Отправленных».", started.elapsed().as_millis());
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = result {
        println!("Отправка не состоялась: {e}");
        if keep_draft {
            println!("Черновик оставлен на сервере в «Черновиках».");
        } else {
            let id = ItemId { id: draft.id.clone(), change_key: None };
            match t.delete_items(std::slice::from_ref(&id), DeleteMode::Hard).await {
                Ok(r) => match r.first() {
                    Some(WriteResult::Ok { .. }) => println!("Черновик удалён навсегда (DeleteItem HardDelete)."),
                    Some(WriteResult::Failed { code, message, .. }) => println!("Черновик не удалён: {code}: {message}"),
                    None => println!("Черновик не удалён: пустой ответ DeleteItem"),
                },
                Err(e2) => println!("Черновик не удалён: DeleteItem: {e2}"),
            }
        }
        return Err(e);
    }
    Ok(())
}

const DRAFT_CHECK_SUBJECT: &str = "[AIOWA] черновик";
const DRAFT_CHECK_SUBJECT_2: &str = "[AIOWA] черновик 2";
const DRAFT_CHECK_ATTACHMENT: &str = "aiowa-draft-check.txt";
/// Маркеры в теле черновика: по ним проверяется, что сервер вернул именно записанное тело
/// (HTML сервер нормализует, поэтому сравнивается не строка целиком, а наличие маркера).
const DRAFT_CHECK_MARK_1: &str = "AIOWA draft check 1";
const DRAFT_CHECK_MARK_2: &str = "AIOWA draft check 2";

/// Ожидание от `GetItem` черновика на шаге проверки.
struct DraftExpect<'a> {
    subject: &'a str,
    body_mark: &'a str,
    to: usize,
    files: usize,
}

/// Печатает сверку содержимого черновика с ожиданием: только счётчики и совпадения, без текста.
/// Возвращает, всё ли совпало.
fn check_draft(label: &str, d: &DraftContent, expect: &DraftExpect<'_>) -> bool {
    let files: Vec<_> = d.file_attachments().collect();
    let subject_ok = d.subject == expect.subject;
    let body_ok = d.body_html.contains(expect.body_mark);
    let to_ok = d.to.len() == expect.to && d.cc.is_empty() && d.bcc.is_empty();
    let files_ok = files.len() == expect.files && files.iter().all(|a| a.name.as_deref() == Some(DRAFT_CHECK_ATTACHMENT));
    let ok = subject_ok && body_ok && to_ok && files_ok;
    println!(
        "  {label}: тема {}, тело {} символов (маркер {}), Кому {} / Копия {} / Скрытая {}{}, вложений {} (файловых {}{}), ChangeKey {} — {}",
        if subject_ok { "совпала" } else { "ДРУГАЯ" },
        d.body_html.chars().count(),
        if body_ok { "найден" } else { "НЕ найден" },
        d.to.len(),
        d.cc.len(),
        d.bcc.len(),
        if to_ok { "" } else { " — НЕ как ожидалось" },
        d.attachments.len(),
        files.len(),
        files.iter().map(|a| format!(", {} байт, тип {}", a.size.map(|s| s.to_string()).unwrap_or_else(|| "?".into()), a.content_type.as_deref().unwrap_or("?"))).collect::<String>(),
        d.id.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into()),
        if ok { "ок" } else { "РАСХОЖДЕНИЕ" }
    );
    ok
}

/// Синхронизация папки «Черновики» в локальную базу: иерархия, элементы, тела и вложения за 90 дней
/// (вложения до 1 МиБ). Печатает счётчики отчёта.
async fn sync_drafts_folder(t: &OwaTransport, store: &Arc<Store>, account: AccountId) -> Result<()> {
    let opts = SyncOptions {
        folders: FolderSelection::Named(vec!["drafts".into()]),
        max_attachment_bytes: 1024 * 1024,
        ..SyncOptions::default()
    };
    let engine = Engine::new(Arc::new(t.clone()), store.clone(), account, Arc::new(AtomicBool::new(false)));
    let started = std::time::Instant::now();
    let r = engine.run(&opts).await.map_err(|e| anyhow!("синхронизация «Черновиков»: {e}"))?;
    println!(
        "  Sync «Черновики» за {} мс: пачек {}, писем создано {} / обновлено {} / удалено {}, тел загружено {} (ошибок {}, исчезло {}), вложений скачано {} (ошибок {})",
        started.elapsed().as_millis(),
        r.pages,
        r.items_created,
        r.items_updated,
        r.items_deleted,
        r.bodies_fetched,
        r.bodies_failed,
        r.bodies_gone,
        r.attachments_fetched,
        r.attachments_failed
    );
    Ok(())
}

/// Сверка строки тестового черновика в локальной базе после синхронизации. `expect_present` —
/// черновик должен быть (не удалён) с `to` получателями и `files` файловыми вложениями.
fn check_local_draft(store: &Store, account: AccountId, server_id: &str, expect_present: bool, to: usize, files: usize) -> Result<bool> {
    let Some((pk, deleted)) = store.item_pk_by_server_id(account, server_id)? else {
        println!("  в базе: строки черновика нет{}", if expect_present { " — НЕ как ожидалось" } else { " (ок)" });
        return Ok(!expect_present);
    };
    if deleted {
        println!("  в базе: item_pk {pk} помечен удалённым{}", if expect_present { " — НЕ как ожидалось" } else { " (ок)" });
        return Ok(!expect_present);
    }
    if !expect_present {
        println!("  в базе: item_pk {pk} ЕЩЁ ЕСТЬ и не удалён");
        return Ok(false);
    }
    let head = store.message_head(pk)?.ok_or_else(|| anyhow!("message_head({pk}) пуст"))?;
    let recipients = store.recipients(pk)?;
    let to_n = recipients.iter().filter(|r| r.kind == RecipientKind::To.as_i64()).count();
    let atts = store.attachments_of_item(pk)?;
    let files_rows: Vec<_> = atts.iter().filter(|a| !a.is_inline).collect();
    let body = store.message_body(pk)?;
    let ok = head.is_draft && to_n == to && files_rows.len() == files && body.is_some();
    println!(
        "  в базе: item_pk {pk}, папка {}, is_draft {}, has_attachments {}, received_at {}, sent_at {}, получателей Кому {} (всего {}), вложений {} (файловых {}, скачано {}, с текстом {}), тело {} — {}",
        head.folder_name.as_deref().unwrap_or("?"),
        head.is_draft,
        head.has_attachments,
        if head.received_at.is_some() { "есть" } else { "НЕТ" },
        if head.sent_at.is_some() { "есть" } else { "нет" },
        to_n,
        recipients.len(),
        atts.len(),
        files_rows.len(),
        files_rows.iter().filter(|a| a.fetched_at.is_some()).count(),
        files_rows.iter().filter(|a| a.text_len > 0).count(),
        match &body {
            Some(b) => format!("есть ({} символов текста)", b.text.as_deref().map(|t| t.chars().count()).unwrap_or(0)),
            None => "НЕТ".into(),
        },
        if ok { "ок" } else { "РАСХОЖДЕНИЕ" }
    );
    Ok(ok)
}

/// Живая проверка черновиков: CreateItem → CreateAttachment → GetItem → UpdateItem → GetItem →
/// DeleteAttachment → GetItem → DeleteItem HardDelete. Сбой после создания всё равно заканчивается
/// удалением черновика навсегда. `owner` — свой адрес (единственный получатель после обновления).
async fn draft_check_flow(t: &OwaTransport, store: &Arc<Store>, account: AccountId, owner: &str, sync_drafts: bool) -> Result<()> {
    let msg1 = OutgoingMessage { subject: DRAFT_CHECK_SUBJECT.into(), body_html: text_to_html(&format!("{DRAFT_CHECK_MARK_1}\n\nПервая версия тела.")), ..OutgoingMessage::default() };
    let file = OutgoingAttachment { name: DRAFT_CHECK_ATTACHMENT.into(), content_type: Some("text/plain".into()), bytes: format!("AIOWA draft-check {}\n", jiff::Timestamp::now()).into_bytes() };
    let started = std::time::Instant::now();
    let draft = t.create_draft(&ComposeKind::New, &msg1).await.map_err(|e| anyhow!("CreateItem: {e}"))?;
    println!("CreateItem (черновик без получателей): Ok за {} мс; ItemId {} символов, ChangeKey {}", started.elapsed().as_millis(), draft.id.len(), draft.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into()));
    let mut all_ok = true;
    let outcome = async {
        let started = std::time::Instant::now();
        let current = t.add_attachments(&draft, std::slice::from_ref(&file)).await.map_err(|e| anyhow!("CreateAttachment: {e}"))?;
        println!("CreateAttachment ({} байт): Ok за {} мс; RootItemId {}, RootItemChangeKey {}", file.bytes.len(), started.elapsed().as_millis(), if current.id == draft.id { "тот же" } else { "ДРУГОЙ" }, describe_change_key(&draft, &current));

        let started = std::time::Instant::now();
        let d1 = t.get_draft(&current).await.map_err(|e| anyhow!("GetItem черновика: {e}"))?;
        println!("GetItem черновика после создания: Ok за {} мс", started.elapsed().as_millis());
        all_ok &= check_draft("содержимое", &d1, &DraftExpect { subject: DRAFT_CHECK_SUBJECT, body_mark: DRAFT_CHECK_MARK_1, to: 0, files: 1 });
        let att_id = d1.file_attachments().next().map(|a| a.id.clone()).ok_or_else(|| anyhow!("в черновике нет файлового вложения — нечего удалять"))?;

        let msg2 = OutgoingMessage {
            to: vec![Participant { name: None, email: Some(owner.to_string()), routing_type: None }],
            subject: DRAFT_CHECK_SUBJECT_2.into(),
            body_html: text_to_html(&format!("{DRAFT_CHECK_MARK_2}\n\nВторая версия тела: тема, тело и получатель изменены.")),
            ..OutgoingMessage::default()
        };
        let started = std::time::Instant::now();
        let updated = t.update_draft(&d1.id, &msg2).await.map_err(|e| anyhow!("UpdateItem черновика: {e}"))?;
        println!("UpdateItem (тема, тело, Кому = свой адрес): Ok за {} мс; Id {}, ChangeKey {}", started.elapsed().as_millis(), if updated.id == draft.id { "тот же" } else { "ДРУГОЙ" }, describe_change_key(&d1.id, &updated));
        let started = std::time::Instant::now();
        let d2 = t.get_draft(&updated).await.map_err(|e| anyhow!("GetItem черновика после UpdateItem: {e}"))?;
        println!("GetItem черновика после обновления: Ok за {} мс", started.elapsed().as_millis());
        all_ok &= check_draft("содержимое", &d2, &DraftExpect { subject: DRAFT_CHECK_SUBJECT_2, body_mark: DRAFT_CHECK_MARK_2, to: 1, files: 1 });
        all_ok &= d2.file_attachments().next().map(|a| a.id == att_id).unwrap_or(false);
        if d2.file_attachments().next().map(|a| a.id != att_id).unwrap_or(false) {
            println!("  AttachmentId после UpdateItem ДРУГОЙ (ожидался тот же)");
        }
        if sync_drafts {
            sync_drafts_folder(t, store, account).await?;
            all_ok &= check_local_draft(store, account, &draft.id, true, 1, 1)?;
        }

        let started = std::time::Instant::now();
        let after_delete = t.delete_attachments(std::slice::from_ref(&att_id)).await.map_err(|e| anyhow!("DeleteAttachment: {e}"))?;
        println!("DeleteAttachment: Ok за {} мс; RootItemId {}, RootItemChangeKey {}", started.elapsed().as_millis(), if after_delete.id == draft.id { "тот же" } else { "ДРУГОЙ" }, describe_change_key(&d2.id, &after_delete));
        all_ok &= after_delete.id == draft.id;
        let started = std::time::Instant::now();
        let d3 = t.get_draft(&after_delete).await.map_err(|e| anyhow!("GetItem черновика после DeleteAttachment: {e}"))?;
        println!("GetItem черновика после удаления вложения: Ok за {} мс", started.elapsed().as_millis());
        all_ok &= check_draft("содержимое", &d3, &DraftExpect { subject: DRAFT_CHECK_SUBJECT_2, body_mark: DRAFT_CHECK_MARK_2, to: 1, files: 0 });
        Ok::<ItemId, anyhow::Error>(d3.id)
    }
    .await;
    let current = match outcome {
        Ok(id) => id,
        Err(e) => {
            println!("Сбой после создания: {e}. Удаляю тестовый черновик навсегда.");
            all_ok = false;
            ItemId { id: draft.id.clone(), change_key: None }
        }
    };
    let started = std::time::Instant::now();
    let results = t.delete_items(std::slice::from_ref(&current), DeleteMode::Hard).await.map_err(|e| anyhow!("DeleteItem HardDelete: {e} (черновик «{DRAFT_CHECK_SUBJECT}» мог остаться в «Черновиках»!)"))?;
    match results.first() {
        Some(WriteResult::Ok { .. }) => println!("DeleteItem HardDelete: Ok за {} мс", started.elapsed().as_millis()),
        Some(WriteResult::Failed { code, message, .. }) => bail!("DeleteItem HardDelete: {code}: {message} (черновик мог остаться в «Черновиках»!)"),
        None => bail!("DeleteItem HardDelete: пустой ответ (черновик мог остаться в «Черновиках»!)"),
    }
    match t.get_draft(&ItemId { id: draft.id.clone(), change_key: None }).await {
        Err(aiowa_transport::TransportError::Server { code, .. }) if code == "ErrorItemNotFound" => println!("  GetItem по удалённому черновику: ErrorItemNotFound (ок)"),
        Err(e) => println!("  GetItem по удалённому черновику: {e} (ожидался ErrorItemNotFound)"),
        Ok(_) => {
            println!("  GetItem по удалённому черновику: ЕЩЁ ЧИТАЕТСЯ");
            all_ok = false;
        }
    }
    if sync_drafts {
        sync_drafts_folder(t, store, account).await?;
        all_ok &= check_local_draft(store, account, &draft.id, false, 0, 0)?;
    }
    if all_ok {
        println!("Живая проверка черновиков пройдена: создание, чтение, обновление, удаление вложения и удаление подтверждены.");
        Ok(())
    } else {
        bail!("живая проверка черновиков не пройдена (см. строки с НЕ/ДРУГОЙ/РАСХОЖДЕНИЕ выше)")
    }
}

fn keyring_entry(user: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|e| anyhow!("keyring: {e}"))
}

fn load_password(user: &str) -> Result<SecretString> {
    let pw = keyring_entry(user)?.get_password().map_err(|e| anyhow!("пароль не найден в диспетчере учётных данных ({e}); выполните: aiowa set-password"))?;
    Ok(SecretString::from(pw))
}

fn default_db_path(host: &str, user: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let safe_user: String = user.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
    base.join("AIOWA").join(host).join(safe_user).join("mail.db")
}

// ---------------------------------------------------------------------------
// Справочник организации (`aiowa directory`, § 5l): только чтение, без значений в выводе

/// Код ошибки транспорта без текста сервера (текст может повторять запрос или имя).
fn transport_error_code(e: &TransportError) -> String {
    match e {
        TransportError::Server { code, .. } => code.clone(),
        // Текст отказа сервера (FaultMessage) — с маской на всём, что похоже на адрес.
        TransportError::Protocol { op, status, raw } => {
            let masked: Vec<&str> = raw.split_whitespace().map(|w| if w.contains('@') { "<адрес>" } else { w }).collect();
            format!("{op}: HTTP {status}: {}", truncate(&masked.join(" "), 200))
        }
        TransportError::Throttled { retry_after } => format!("Throttled {retry_after:?}"),
        TransportError::Network(_) => "Network".into(),
        TransportError::AuthFailed(_) => "AuthFailed".into(),
        other => other.to_string(),
    }
}

/// Сводка записей справочника без значений: сколько записей с каждым полем и виды.
fn describe_people(people: &[DirectoryPerson]) -> String {
    let n = |f: &dyn Fn(&DirectoryPerson) -> bool| people.iter().filter(|p| f(p)).count();
    let mut kinds: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for p in people {
        *kinds.entry(p.kind.key()).or_default() += 1;
    }
    let kinds: Vec<String> = kinds.into_iter().map(|(k, c)| format!("{k} {c}")).collect();
    format!(
        "записей {}: SMTP {}, имя {}, должность {}, отдел {}, компания {}, офис {}, рабочий тел. {}, мобильный {}, псевдоним {}; виды: {}",
        people.len(),
        n(&|p| p.email_key().is_some()),
        n(&|p| p.display_name.is_some()),
        n(&|p| p.title.is_some()),
        n(&|p| p.department.is_some()),
        n(&|p| p.company.is_some()),
        n(&|p| p.office.is_some()),
        n(&|p| p.business_phone.is_some()),
        n(&|p| p.mobile_phone.is_some()),
        n(&|p| p.alias.is_some()),
        if kinds.is_empty() { "нет".to_string() } else { kinds.join(", ") }
    )
}

/// Ключи объектов с числом объектов, где ключ есть: `Ключ×N, …`.
fn keys_line(items: &[serde_json::Value]) -> String {
    let keys = aiowa_transport_owa::protocol::key_union(items);
    if keys.is_empty() {
        return "нет".into();
    }
    keys.iter().map(|(k, n)| format!("{k}×{n}")).collect::<Vec<_>>().join(", ")
}

/// Вложенные объекты `key` записей массива (для ключей `EmailAddress`, `Mailbox`, `Contact`).
fn nested(items: &[serde_json::Value], key: &str) -> Vec<serde_json::Value> {
    items.iter().filter_map(|v| v.get(key)).filter(|v| v.is_object()).cloned().collect()
}

/// Латиница для слова кириллицы (упрощённо): чтобы узнать фамилию в имени по адресу.
fn translit(word: &str) -> String {
    let mut out = String::new();
    for c in word.to_lowercase().chars() {
        if c.is_ascii_alphabetic() {
            out.push(c);
            continue;
        }
        out.push_str(match c {
            'а' => "a",
            'б' => "b",
            'в' => "v",
            'г' => "g",
            'д' => "d",
            'е' | 'ё' | 'э' => "e",
            'ж' => "zh",
            'з' => "z",
            'и' | 'й' => "i",
            'к' => "k",
            'л' => "l",
            'м' => "m",
            'н' => "n",
            'о' => "o",
            'п' => "p",
            'р' => "r",
            'с' => "s",
            'т' => "t",
            'у' => "u",
            'ф' => "f",
            'х' => "kh",
            'ц' => "ts",
            'ч' => "ch",
            'ш' => "sh",
            'щ' => "sch",
            'ы' => "y",
            'ю' => "yu",
            'я' => "ya",
            _ => "",
        });
    }
    out
}

/// Скалярные поля объекта без значений строк: `Ключ=str(N)`, числа и признаки как есть.
fn scalar_fields(v: &serde_json::Value) -> String {
    v.as_object()
        .map(|o| o.iter().filter(|(_, x)| !x.is_array() && !x.is_object()).map(|(k, x)| format!("{k}={}", if x.is_string() { aiowa_transport_owa::protocol::json_shape(x, 0) } else { x.to_string() })).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Фамилия владельца ящика для живой проверки: слово имени из «Отправленных» (три буквы и
/// длиннее), чья латиница дольше всех совпадает с началом собственного адреса; при равенстве —
/// первое. Возвращает слово и описание выбора без значений.
fn owner_surname(store: &Store, account: AccountId) -> Result<(String, String)> {
    let name = store.own_display_name(account)?.ok_or_else(|| anyhow!("имени владельца нет в «Отправленных» локальной базы: задайте --query"))?;
    let local = store.own_addresses(account)?.first().and_then(|a| a.split('@').next().map(str::to_string)).unwrap_or_default();
    let words: Vec<&str> = name.split(|c: char| c.is_whitespace() || c == ',').filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3).collect();
    let common = |w: &str| translit(w).chars().zip(local.chars()).take_while(|(a, b)| a == b).count();
    let (index, word) = words.iter().enumerate().max_by(|a, b| common(a.1).cmp(&common(b.1)).then(b.0.cmp(&a.0))).ok_or_else(|| anyhow!("в имени владельца нет слов длиннее двух букв: задайте --query"))?;
    let how = format!("слово {} из {} имени владельца из «Отправленных», {} символов, совпадение с адресом {} букв", index + 1, words.len(), word.chars().count(), common(word));
    Ok((word.to_string(), how))
}

/// Один поиск `FindPeople` с печатью формы ответа без значений; найденное — в кэш.
async fn directory_find(t: &OwaTransport, store: &Store, account: AccountId, label: &str, query: &str, max: usize) -> Vec<DirectoryPerson> {
    use aiowa_transport_owa::protocol;
    let started = std::time::Instant::now();
    let v = match t.find_people_value(query, max).await {
        Ok(v) => v,
        Err(e) => {
            println!("FindPeople ({label}, {} символов): отказ {} за {} мс", query.chars().count(), transport_error_code(&e), started.elapsed().as_millis());
            return Vec::new();
        }
    };
    let ms = started.elapsed().as_millis();
    let body = v.get("Body").cloned().unwrap_or_default();
    println!("FindPeople ({label}, {} символов, max {max}): {ms} мс, ответ {} байт", query.chars().count(), v.to_string().len());
    println!("  Body: ключи [{}]; ResponseClass {}, ResponseCode {}", body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default(), protocol::str_of(&body, "ResponseClass").unwrap_or("?"), protocol::str_of(&body, "ResponseCode").unwrap_or("?"));
    for key in ["TotalNumberOfPeopleInView", "FirstMatchingRowIndex", "FirstLoadedRowIndex"] {
        if let Some(n) = body.get(key) {
            println!("  {key}: {n}");
        }
    }
    let people_raw = protocol::find_people_array(&v).cloned().unwrap_or_default();
    let array_key = ["ResultSet", "People", "Personas"].into_iter().find(|k| body.get(*k).is_some_and(serde_json::Value::is_array)).unwrap_or("нет в Body");
    println!("  записи (массив {array_key}): {}; ключи: {}", people_raw.len(), keys_line(&people_raw));
    println!("  записи[].EmailAddress: ключи: {}", keys_line(&nested(&people_raw, "EmailAddress")));
    for key in ["EmailAddresses", "Titles", "Departments", "OfficeLocations", "BusinessPhoneNumbers", "MobilePhones", "PhoneNumber", "CompanyNames"] {
        if let Some(x) = people_raw.iter().find_map(|p| p.get(key)) {
            println!("  форма {key}: {}", protocol::json_shape(x, 3));
        }
    }
    let persona_types: std::collections::BTreeMap<String, usize> = people_raw.iter().fold(Default::default(), |mut m, p| {
        *m.entry(protocol::str_of(p, "PersonaType").or_else(|| protocol::str_of(p, "PersonaTypeString")).unwrap_or("нет").to_string()).or_default() += 1;
        m
    });
    let mailbox_types: std::collections::BTreeMap<String, usize> = people_raw.iter().fold(Default::default(), |mut m, p| {
        *m.entry(p.pointer("/EmailAddress/MailboxType").and_then(serde_json::Value::as_str).unwrap_or("нет").to_string()).or_default() += 1;
        m
    });
    println!("  PersonaType(String): {persona_types:?}; EmailAddress.MailboxType: {mailbox_types:?}");
    if let Some(first) = people_raw.first() {
        println!("  форма первой записи: {}", protocol::json_shape(first, 4));
    }
    match OwaTransport::find_people_result(&v, max) {
        Ok(people) => {
            println!("  разобрано: {}", describe_people(&people));
            match store.upsert_directory_people(account, &people) {
                Ok(n) => println!("  в кэш справочника записано {n}"),
                Err(e) => println!("  кэш справочника: ошибка {e}"),
            }
            people
        }
        Err(e) => {
            println!("  разбор: отказ {}", transport_error_code(&e));
            Vec::new()
        }
    }
}

/// `ResolveNames` с печатью формы ответа без значений; найденное — в кэш.
async fn directory_resolve(t: &OwaTransport, store: &Store, account: AccountId, label: &str, query: &str, full: bool) -> Vec<DirectoryPerson> {
    use aiowa_transport_owa::protocol;
    let started = std::time::Instant::now();
    let v = match t.resolve_names_value(query, full).await {
        Ok(v) => v,
        Err(e) => {
            println!("ResolveNames ({label}, полные данные: {full}): отказ {} за {} мс", transport_error_code(&e), started.elapsed().as_millis());
            return Vec::new();
        }
    };
    let ms = started.elapsed().as_millis();
    let msgs = protocol::response_messages(&v);
    let msg = msgs.first().map(|m| (*m).clone()).unwrap_or_default();
    println!(
        "ResolveNames ({label}, {} символов, полные данные: {full}): {ms} мс, ответ {} байт; сообщение {} {} {}",
        query.chars().count(),
        v.to_string().len(),
        protocol::str_of(&msg, "__type").unwrap_or("без __type"),
        protocol::str_of(&msg, "ResponseClass").unwrap_or("?"),
        protocol::str_of(&msg, "ResponseCode").unwrap_or("?")
    );
    println!("  ключи сообщения: [{}]", msg.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default());
    if let Some(set) = msg.get("ResolutionSet") {
        println!("  ResolutionSet: {} [{}]", if set.is_array() { "массив" } else { "объект" }, scalar_fields(set));
    }
    let res = protocol::resolutions_array(&msg).cloned().unwrap_or_default();
    println!("  Resolutions: {}; ключи: {}", res.len(), keys_line(&res));
    println!("  Mailbox: ключи: {}", keys_line(&nested(&res, "Mailbox")));
    let contacts = nested(&res, "Contact");
    println!("  Contact: ключи: {}", keys_line(&contacts));
    let mailbox_types: std::collections::BTreeMap<String, usize> = res.iter().fold(Default::default(), |mut m, r| {
        *m.entry(r.pointer("/Mailbox/MailboxType").and_then(serde_json::Value::as_str).unwrap_or("нет").to_string()).or_default() += 1;
        m
    });
    println!("  Mailbox.MailboxType: {mailbox_types:?}");
    if let Some(c) = contacts.first() {
        for key in ["PhoneNumbers", "EmailAddresses", "PhysicalAddresses", "ContactSource", "Manager", "DirectReports"] {
            if let Some(x) = c.get(key) {
                println!("  форма Contact.{key}: {}", protocol::json_shape(x, 3));
            }
        }
        let phone_keys: Vec<String> = c.get("PhoneNumbers").map(|p| match p {
            serde_json::Value::Array(a) => a.iter().filter_map(|e| protocol::str_of(e, "Key").map(String::from)).collect(),
            serde_json::Value::Object(o) => o.keys().cloned().collect(),
            _ => vec![],
        }).unwrap_or_default();
        if !phone_keys.is_empty() {
            println!("  виды телефонов первой записи: {}", phone_keys.join(", "));
        }
    }
    // Сколько записей с непустым телефоном каждого вида (только счётчики).
    let mut filled: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for c in &contacts {
        if let Some(a) = c.get("PhoneNumbers").and_then(serde_json::Value::as_array) {
            for e in a {
                let non_empty = e.get("PhoneNumber").or_else(|| e.get("Value")).and_then(serde_json::Value::as_str).is_some_and(|s| !s.trim().is_empty());
                *filled.entry(protocol::str_of(e, "Key").unwrap_or("?").to_string()).or_default() += usize::from(non_empty);
            }
        }
    }
    if !filled.is_empty() {
        println!("  непустые телефоны по видам (из {} записей): {filled:?}", contacts.len());
    }
    let filled_fields: Vec<String> = ["JobTitle", "Department", "CompanyName", "OfficeLocation", "Alias", "Manager", "Notes"]
        .iter()
        .map(|k| format!("{k} {}", contacts.iter().filter(|c| c.get(*k).and_then(serde_json::Value::as_str).is_some_and(|s| !s.trim().is_empty())).count()))
        .collect();
    if !contacts.is_empty() {
        println!("  непустые поля Contact: {}", filled_fields.join(", "));
    }
    match OwaTransport::resolve_names_result(&v) {
        Ok(people) => {
            println!("  разобрано: {}", describe_people(&people));
            match store.upsert_directory_people(account, &people) {
                Ok(n) => println!("  в кэш справочника записано {n}"),
                Err(e) => println!("  кэш справочника: ошибка {e}"),
            }
            people
        }
        Err(e) => {
            println!("  разбор: отказ {}", transport_error_code(&e));
            Vec::new()
        }
    }
}

/// Кандидаты в списки рассылки из локальной базы: частые получатели «Кому» и «Копии», от которых
/// нет ни одного письма (адреса не печатаются).
fn dl_candidates(store: &Store, account: AccountId, n: usize) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT lower(r.email) AS e, COUNT(*) AS n FROM recipients r JOIN items i ON i.item_pk = r.item_pk
          WHERE i.account_id = {a} AND r.kind IN (1, 2) AND r.email LIKE '%@%'
            AND NOT EXISTS (SELECT 1 FROM items x WHERE x.account_id = {a} AND lower(x.from_email) = lower(r.email))
          GROUP BY e ORDER BY n DESC LIMIT {n}",
        a = account.0
    );
    Ok(store.debug_query(&sql)?.into_iter().skip(1).filter_map(|line| line.split('\t').next().map(String::from)).collect())
}

/// Свойства записи `FindPeople`, которые проверяет `--probe-shapes`: по одному и вместе.
const PERSONA_PROBE_FIELDS: &[&str] = &["persona:Title", "persona:Department", "persona:OfficeLocations", "persona:BusinessPhoneNumbers", "persona:MobilePhones", "persona:Alias", "persona:PersonaType"];

/// Проверка форм справочника (`--probe-shapes`): какие `AdditionalProperties` записи `FindPeople`
/// сервер принимает и под какими ключами отдаёт, и сколько весит `ContactDataShape: Default`
/// у `ResolveNames` против `AllProperties`. Печатаются только коды, ключи, счётчики и длины.
async fn directory_probe_shapes(t: &OwaTransport, query: &str) {
    use aiowa_transport_owa::protocol;
    let base = match t.call("FindPeople", protocol::V_2017, protocol::find_people_body(query, 5)).await {
        Ok(v) => v,
        Err(e) => {
            println!("Проба форм: базовый FindPeople отказ {}", transport_error_code(&e));
            return;
        }
    };
    let base_keys: std::collections::BTreeSet<String> = protocol::find_people_array(&base).map(|a| protocol::key_union(a).into_iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let mut accepted: Vec<&str> = Vec::new();
    println!("Проба AdditionalProperties у FindPeople (3 буквы, max 5; базовых ключей {}):", base_keys.len());
    for &f in PERSONA_PROBE_FIELDS {
        if probe_persona_fields(t, query, &base_keys, &[f], f).await {
            accepted.push(f);
        }
    }
    if accepted.len() > 1 {
        probe_persona_fields(t, query, &base_keys, &accepted, "все принятые вместе").await;
    }
    for shape in ["Default", "AllProperties"] {
        let started = std::time::Instant::now();
        match t.call("ResolveNames", protocol::V_2017, protocol::resolve_names_body_shaped(query, Some(shape))).await {
            Ok(v) => {
                let ms = started.elapsed().as_millis();
                let msgs = protocol::response_messages(&v);
                let msg = msgs.first().map(|m| (*m).clone()).unwrap_or_default();
                let res = protocol::resolutions_array(&msg).cloned().unwrap_or_default();
                println!("ResolveNames ContactDataShape {shape}: {ms} мс, ответ {} байт, {} {}, записей {}; Contact: {}", v.to_string().len(), protocol::str_of(&msg, "ResponseClass").unwrap_or("?"), protocol::str_of(&msg, "ResponseCode").unwrap_or("?"), res.len(), keys_line(&nested(&res, "Contact")));
            }
            Err(e) => println!("ResolveNames ContactDataShape {shape}: отказ {} за {} мс", transport_error_code(&e), started.elapsed().as_millis()),
        }
    }
}

/// Один `FindPeople` с `AdditionalProperties` `fields`: принят ли, какие ключи добавились к
/// базовым `base_keys` и их форма (без значений).
async fn probe_persona_fields(t: &OwaTransport, query: &str, base_keys: &std::collections::BTreeSet<String>, fields: &[&str], label: &str) -> bool {
    use aiowa_transport_owa::protocol;
    let started = std::time::Instant::now();
    let v = match t.call("FindPeople", protocol::V_2017, protocol::find_people_body_with(query, 5, fields)).await {
        Ok(v) => v,
        Err(e) => {
            println!("  {label}: отказ {} за {} мс", transport_error_code(&e), started.elapsed().as_millis());
            return false;
        }
    };
    let ms = started.elapsed().as_millis();
    if let Some((code, _)) = protocol::body_error(&v) {
        println!("  {label}: отказ в ответе {code} за {ms} мс");
        return false;
    }
    let arr = protocol::find_people_array(&v).cloned().unwrap_or_default();
    let new_keys: Vec<(String, usize)> = protocol::key_union(&arr).into_iter().filter(|(k, _)| !base_keys.contains(k)).collect();
    let listed: Vec<String> = new_keys
        .iter()
        .map(|(k, n)| format!("{k}×{n} (непусто {})", arr.iter().filter(|p| p.get(k).is_some_and(|x| !x.is_null() && x != "" && x.as_array().is_none_or(|a| !a.is_empty()))).count()))
        .collect();
    println!("  {label}: принято, {ms} мс, записей {}, новые ключи [{}]", arr.len(), listed.join(", "));
    for (k, _) in &new_keys {
        if let Some(x) = arr.iter().find_map(|p| p.get(k).filter(|x| !x.is_null())) {
            println!("    форма {k}: {}", protocol::json_shape(x, 3));
        }
    }
    true
}

/// Параметры `aiowa directory`.
struct DirectoryOpts<'a> {
    query: &'a str,
    resolve: bool,
    expand: Option<&'a str>,
    prefix3: bool,
    max: usize,
    probe_shapes: bool,
    dl_candidates: usize,
    dl_queries: &'a [String],
}

/// Живая проверка справочника одной сессией (`aiowa directory`): только чтение.
async fn directory_flow(t: &OwaTransport, store: &Store, account: AccountId, opts: &DirectoryOpts<'_>) -> Result<()> {
    use aiowa_transport_owa::protocol;
    let DirectoryOpts { query, resolve, expand, prefix3, max, .. } = *opts;
    let mut found = directory_find(t, store, account, "запрос", query, max).await;
    let short: String = query.chars().take(3).collect();
    if prefix3 && short.chars().count() == 3 {
        found.extend(directory_find(t, store, account, "три буквы", &short, max).await);
    }
    for (i, q) in opts.dl_queries.iter().enumerate() {
        let people = match t.find_people(q, max).await {
            Ok(p) => p,
            Err(e) => {
                println!("FindPeople (поиск списков, запрос {}): отказ {}", i + 1, transport_error_code(&e));
                continue;
            }
        };
        println!("FindPeople (поиск списков, запрос {}, {} символов): {}", i + 1, q.chars().count(), describe_people(&people));
        found.extend(people);
    }
    if opts.probe_shapes {
        directory_probe_shapes(t, if short.chars().count() == 3 { &short } else { query }).await;
    }
    if resolve {
        found.extend(directory_resolve(t, store, account, "запрос", query, true).await);
        directory_resolve(t, store, account, "запрос", query, false).await;
        if prefix3 && short.chars().count() == 3 {
            found.extend(directory_resolve(t, store, account, "три буквы", &short, true).await);
        }
        // Как карточка контакта: ResolveNames по адресу первой найденной записи ящика.
        if let Some(email) = found.iter().find(|p| p.kind == PersonKind::Mailbox).and_then(|p| p.email_key()) {
            let started = std::time::Instant::now();
            match t.resolve_names(&email, true).await {
                Ok(people) => println!("ResolveNames по адресу первой записи (трейт): {} мс, {}", started.elapsed().as_millis(), describe_people(&people)),
                Err(e) => println!("ResolveNames по адресу первой записи (трейт): отказ {} за {} мс", transport_error_code(&e), started.elapsed().as_millis()),
            }
            match store.contact_local_stats(account, &email) {
                Ok(s) => println!("Переписка с этим адресом по локальной базе: писем от него {}, ему {}, последнее {}", s.messages_from, s.messages_to, if s.last_ms.is_some() { "есть" } else { "нет" }),
                Err(e) => println!("Переписка по локальной базе: ошибка {e}"),
            }
        }
    }
    if let Some(target) = expand {
        let email = if target.eq_ignore_ascii_case("first") {
            match found.iter().find(|p| p.kind.is_list()).and_then(|p| p.email_key()) {
                Some(e) => {
                    println!("ExpandDL: первый список рассылки из результатов");
                    Some(e)
                }
                None => {
                    println!("ExpandDL: списков рассылки с SMTP-адресом в результатах нет; кандидаты из локальной базы (частые получатели, которые сами не писали)");
                    let mut chosen = None;
                    for (i, cand) in dl_candidates(store, account, opts.dl_candidates)?.iter().enumerate() {
                        let started = std::time::Instant::now();
                        match t.resolve_names(cand, false).await {
                            Ok(people) => {
                                let kinds: Vec<&str> = people.iter().map(|p| p.kind.key()).collect();
                                println!("  кандидат {}: ResolveNames {} мс, записей {}, виды [{}]", i + 1, started.elapsed().as_millis(), people.len(), kinds.join(", "));
                                if people.len() == 1 && people[0].kind.is_list() {
                                    chosen = people[0].email_key();
                                    break;
                                }
                            }
                            Err(e) => println!("  кандидат {}: отказ {} за {} мс", i + 1, transport_error_code(&e), started.elapsed().as_millis()),
                        }
                    }
                    if chosen.is_none() {
                        println!("ExpandDL: список рассылки не найден, пропущено");
                    }
                    chosen
                }
            }
        } else {
            Some(target.to_string())
        };
        if let Some(email) = email {
            let started = std::time::Instant::now();
            match t.expand_dl_value(&email).await {
                Ok(v) => {
                    let ms = started.elapsed().as_millis();
                    let msgs = protocol::response_messages(&v);
                    let msg = msgs.first().map(|m| (*m).clone()).unwrap_or_default();
                    println!(
                        "ExpandDL: {ms} мс, ответ {} байт; сообщение {} {} {}",
                        v.to_string().len(),
                        protocol::str_of(&msg, "__type").unwrap_or("без __type"),
                        protocol::str_of(&msg, "ResponseClass").unwrap_or("?"),
                        protocol::str_of(&msg, "ResponseCode").unwrap_or("?")
                    );
                    println!("  ключи сообщения: [{}]", msg.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default());
                    if let Some(dl) = msg.get("DLExpansion") {
                        println!("  DLExpansion: {} [{}]", if dl.is_array() { "массив" } else { "объект" }, scalar_fields(dl));
                    }
                    let members = protocol::dl_expansion_array(&msg).cloned().unwrap_or_default();
                    println!("  участники: {}; ключи: {}", members.len(), keys_line(&members));
                    match OwaTransport::expand_dl_result(&v) {
                        Ok(people) => {
                            println!("  разобрано: {}", describe_people(&people));
                            if let Ok(n) = store.upsert_directory_people(account, &people) {
                                println!("  в кэш справочника записано {n}");
                            }
                        }
                        Err(e) => println!("  разбор: отказ {}", transport_error_code(&e)),
                    }
                }
                Err(e) => println!("ExpandDL: отказ {} за {} мс", transport_error_code(&e), started.elapsed().as_millis()),
            }
        }
    }
    for (label, q) in [("запрос", query.to_string()), ("три буквы", short.clone())] {
        if label == "три буквы" && !prefix3 {
            continue;
        }
        let started = std::time::Instant::now();
        let cached = store.directory_search_cached(account, &q, 50)?;
        println!("Кэш справочника ({label}): {} записей за {} мкс", cached.len(), started.elapsed().as_micros());
    }
    Ok(())
}

async fn connect(cli: &Cli) -> Result<OwaTransport> {
    let password = load_password(&cli.user)?;
    let creds = Credentials { host: cli.host.clone(), user: cli.user.clone(), password };
    let config = OwaConfig { time_zone: cli.tz.clone(), ..OwaConfig::default() };
    OwaTransport::connect(creds, config).await.map_err(|e| anyhow!("вход не удался: {e}"))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,pdf_extract=error,lopdf=error"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).without_time().init();
}

fn folder_selection(spec: &str) -> FolderSelection {
    let names: Vec<String> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    if names.is_empty() || (names.len() == 1 && names[0].eq_ignore_ascii_case("all")) { FolderSelection::AllMail } else { FolderSelection::Named(names) }
}

fn fmt_date(t: Option<jiff::Timestamp>) -> String {
    t.map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn main() -> Result<()> {
    // Будущее `real_main` в отладочной сборке огромно (все команды — одна функция): на куче и в
    // потоке с большим стеком, иначе главный поток (1 МиБ на Windows) переполняется ещё до
    // разбора аргументов: кадр опроса такого будущего растёт с каждой командой.
    let worker = std::thread::Builder::new()
        .name("aiowa-main".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(Box::pin(real_main())))
        .context("не удалось запустить главный поток")?;
    worker.join().map_err(|_| anyhow!("главный поток завершился аварийно"))?
}

async fn real_main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match &cli.cmd {
        Cmd::SetPassword => {
            let pw = rpassword::prompt_password(format!("Пароль для {} на {}: ", cli.user, cli.host)).context("не удалось прочитать пароль")?;
            if pw.is_empty() {
                bail!("пустой пароль не сохраняю");
            }
            keyring_entry(&cli.user)?.set_password(&pw).map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль сохранён (служба {KEYRING_SERVICE}, пользователь {}).", cli.user);
            return Ok(());
        }
        Cmd::ForgetPassword => {
            keyring_entry(&cli.user)?.delete_credential().map_err(|e| anyhow!("keyring: {e}"))?;
            println!("Пароль удалён.");
            return Ok(());
        }
        _ => {}
    }

    let db_path = cli.db.clone().unwrap_or_else(|| default_db_path(&cli.host, &cli.user));

    match &cli.cmd {
        Cmd::Login => {
            let t = connect(&cli).await?;
            let info = t.server_info();
            let started = std::time::Instant::now();
            let folders = t.sync_folder_hierarchy(None).await.map_err(|e| anyhow!("{e}"))?;
            println!("Сервер {} сборка {}; папок в иерархии {} (SyncState {} символов) за {} мс", info.host, info.build.as_deref().unwrap_or("?"), folders.changes.len(), folders.state.0.len(), started.elapsed().as_millis());
            t.logoff().await.ok();
        }
        Cmd::Folders => {
            let t = connect(&cli).await?;
            let changes = t.sync_folder_hierarchy(None).await.map_err(|e| anyhow!("{e}"))?;
            print_folder_tree(&changes.changes);
            t.logoff().await.ok();
        }
        Cmd::Sync { days, folders, no_bodies, no_attachments, max_attachment_mb, cache_mb, pages } => {
            let store = Arc::new(Store::open(&db_path)?);
            let account = store.account(&cli.host, &cli.user)?;
            let opts = SyncOptions {
                folders: folder_selection(folders),
                body_days: if *days == 0 { None } else { Some(*days) },
                fetch_bodies: !no_bodies,
                fetch_attachments: !no_bodies && !no_attachments,
                max_attachment_bytes: max_attachment_mb * 1024 * 1024,
                attachment_cache_bytes: cache_mb * 1024 * 1024,
                max_pages: *pages,
                ..SyncOptions::default()
            };
            let cancel = Arc::new(AtomicBool::new(false));
            {
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        eprintln!("\nОстанавливаюсь после текущей пачки...");
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            let transport = connect(&cli).await?;
            let engine = Engine::new(Arc::new(transport.clone()), store.clone(), account, cancel);
            let result = engine.run(&opts).await;
            transport.logoff().await.ok();
            let report = result.map_err(|e| anyhow!("синхронизация прервана: {e}"))?;
            println!(
                "Готово за {:.1} с: папок в иерархии {}, синхронизировано {}, пачек {}, писем создано {} / обновлено {} / удалено {}, флагов {}, тел загружено {} (ошибок {}, исчезло на сервере {}), вложений скачано {} ({:.1} МБ, с текстом {}, ошибок {}), из кэша вытеснено {} файлов ({:.1} МБ).\nБаза: {}",
                report.elapsed.as_secs_f32(),
                report.folders_in_hierarchy,
                report.folders_synced,
                report.pages,
                report.items_created,
                report.items_updated,
                report.items_deleted,
                report.read_flags,
                report.bodies_fetched,
                report.bodies_failed,
                report.bodies_gone,
                report.attachments_fetched,
                report.attachments_bytes as f64 / 1e6,
                report.attachments_with_text,
                report.attachments_failed,
                report.cache_files_evicted,
                report.cache_bytes_evicted as f64 / 1e6,
                db_path.display()
            );
        }
        Cmd::List { folder, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let f = store.resolve_folder(account, folder)?.ok_or_else(|| anyhow!("папка «{folder}» не найдена в локальной базе"))?;
            let started = std::time::Instant::now();
            let rows = store.list_conversations(account, f.folder_pk, *limit)?;
            println!("{}: {} бесед за {} мс\n", f.name, rows.len(), started.elapsed().as_millis());
            println!("{}  {}  {:>5} {:>6}  Тема", pad("Дата", 16), pad("Участники", 36), "Писем", "Непроч");
            for c in rows {
                let who = c.participants.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                let marker = if c.unread_count > 0 { "●" } else { " " };
                let clip = if c.has_attachments { " 📎" } else { "" };
                let count = if c.total_count != c.item_count { format!("{}/{}", c.item_count, c.total_count) } else { c.item_count.to_string() };
                println!("{}  {}  {:>5} {:>5}{}  {}{}", pad(&fmt_date(c.last_received_at), 16), pad(&who, 36), count, c.unread_count, marker, truncate(&c.topic.as_deref().unwrap_or("(без темы)").split_whitespace().collect::<Vec<_>>().join(" "), 70), clip);
            }
        }
        Cmd::Search { query, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let started = std::time::Instant::now();
            let hits = store.search(account, query, *limit)?;
            println!("{} результатов за {} мс\n", hits.len(), started.elapsed().as_millis());
            for h in hits {
                let from = h.from_name.or(h.from_email).unwrap_or_default();
                let snippet = h.snippet.replace('\n', " ");
                let snippet = match &h.attachment_name {
                    Some(name) => format!("📎 {name}: {snippet}"),
                    None => snippet,
                };
                println!("{}  {}  [{}]  #{}\n    {}\n    {}\n", fmt_date(h.received_at), pad(&from, 28), h.folder_name.unwrap_or_default(), h.item_pk, truncate(h.subject.as_deref().unwrap_or("(без темы)"), 90), snippet);
            }
        }
        Cmd::FetchAttachments { item_pk, max_mb } => {
            let store = Arc::new(Store::open(&db_path)?);
            let account = store.account(&cli.host, &cli.user)?;
            let rows = store.attachments_of_item(*item_pk)?;
            if rows.is_empty() {
                bail!("у письма #{item_pk} нет вложений в базе (тело ещё не загружено?)");
            }
            let transport = connect(&cli).await?;
            let engine = Engine::new(Arc::new(transport.clone()), store.clone(), account, Arc::new(AtomicBool::new(false)));
            let max_bytes = max_mb * 1024 * 1024;
            for a in rows.iter().filter(|a| !a.is_inline) {
                store.reset_attachment(a.attachment_pk)?;
                let todo = AttachmentTodo { attachment_pk: a.attachment_pk, item_pk: a.item_pk, server_id: a.server_id.clone(), name: a.name.clone(), content_type: a.content_type.clone(), size: a.size };
                let started = std::time::Instant::now();
                match engine.fetch_one_attachment(&todo, max_bytes, aiowa_extract_limit()).await {
                    Ok(f) => {
                        let text = store.attachment_text(a.attachment_pk)?.unwrap_or_default();
                        println!(
                            "{}: {} байт за {} мс, текст {} символов (состояние {}), файл {}\n    {}",
                            a.name.as_deref().unwrap_or("?"),
                            f.bytes,
                            started.elapsed().as_millis(),
                            f.text_chars,
                            f.text_state,
                            f.path.display(),
                            truncate(&text.replace('\n', " "), 300)
                        );
                    }
                    Err(e) => println!("{}: ошибка: {e}", a.name.as_deref().unwrap_or("?")),
                }
            }
            transport.logoff().await.ok();
        }
        Cmd::Stats => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let s = store.stats(account)?;
            println!(
                "База {} : {:.1} МБ, писем {}, тел {}, бесед {}, вложений {} (скачано {}, с текстом {}, в кэше {:.1} МБ)\n",
                db_path.display(),
                s.db_bytes as f64 / 1e6,
                s.items,
                s.bodies,
                s.conversations,
                s.attachments,
                s.attachments_fetched,
                s.attachments_with_text,
                s.attachment_cache_bytes as f64 / 1e6
            );
            println!("{} {:>8} {:>8} {:>8}  {:<10} {:<10} Синхр.", pad("Папка", 34), "Сервер", "Локально", "Тел", "Старейшее", "Новейшее");
            for f in s.folders {
                let fmt = |t: Option<jiff::Timestamp>| t.map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default();
                println!(
                    "{} {:>8} {:>8} {:>8}  {:<10} {:<10} {}",
                    pad(&format!("{}{}", f.display_name, f.distinguished.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()), 34),
                    f.server_total,
                    f.local_items,
                    f.local_bodies,
                    fmt(f.oldest),
                    fmt(f.newest),
                    if f.synced { "полностью" } else { "частично" }
                );
            }
        }
        Cmd::MarkRead { item_pk, read } => {
            let store = Store::open(&db_path)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            println!("#{item_pk}: папка {}, is_read {} → {}, ChangeKey {}", r.folder_pk, r.is_read, read, if r.id.change_key.is_some() { "есть" } else { "нет" });
            let t = connect(&cli).await?;
            let result = t.set_read(std::slice::from_ref(&r.id), *read).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("UpdateItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let convs = store.set_read_local(&[*item_pk], *read)?;
                    store.update_item_id(*item_pk, id)?;
                    println!("UpdateItem: Ok; ItemId в ответе {}, ChangeKey {}; локально is_read = {}, бесед затронуто {}", if id.id == r.id.id { "тот же" } else { "другой" }, describe_change_key(&r.id, id), read, convs.len());
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("UpdateItem: Failed {code}: {message}"),
                None => println!("UpdateItem: пустой ответ"),
            }
        }
        Cmd::Flag { item_pk, flagged } => {
            let store = Store::open(&db_path)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            println!("#{item_pk}: папка {}, is_flagged {} → {}", r.folder_pk, r.is_flagged, flagged);
            let t = connect(&cli).await?;
            let result = t.set_flag(std::slice::from_ref(&r.id), *flagged).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("UpdateItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let convs = store.set_flag_local(&[*item_pk], *flagged)?;
                    store.update_item_id(*item_pk, id)?;
                    println!("UpdateItem: Ok; ChangeKey {}; локально флаг = {}, бесед затронуто {}", describe_change_key(&r.id, id), flagged, convs.len());
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("UpdateItem: Failed {code}: {message}"),
                None => println!("UpdateItem: пустой ответ"),
            }
        }
        Cmd::Move { item_pk, to, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            let target = store.resolve_folder(account, to)?.ok_or_else(|| anyhow!("папка «{to}» не найдена в локальной базе"))?;
            if target.folder_pk == r.folder_pk {
                bail!("письмо #{item_pk} уже лежит в папке «{}»", target.name);
            }
            println!("#{item_pk}: из папки {} в «{}» (folder_pk {}) операцией MoveItem", r.folder_pk, target.name, target.folder_pk);
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let to_ref = FolderRef::Id(FolderId { id: target.server_id.clone(), change_key: None });
            let t = connect(&cli).await?;
            let result = t.move_items(std::slice::from_ref(&r.id), &to_ref).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("MoveItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let new_id = (id.id != r.id.id || id.change_key != r.id.change_key).then_some(id);
                    store.move_local(*item_pk, target.folder_pk, new_id)?;
                    println!("MoveItem: Ok; новый ItemId в ответе {}; локально письмо в папке {}", if new_id.is_some() { "есть" } else { "нет (остался прежний)" }, target.folder_pk);
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("MoveItem: Failed {code}: {message}"),
                None => println!("MoveItem: пустой ответ"),
            }
        }
        Cmd::Delete { item_pk, permanent, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let r = store.item_ref(*item_pk)?.ok_or_else(|| anyhow!("письмо #{item_pk} не найдено в базе или удалено"))?;
            let deleted_items = store.folder_by_distinguished(account, "deleteditems")?;
            let in_deleted = deleted_items.as_ref().map(|(pk, _)| *pk == r.folder_pk).unwrap_or(false);
            let mode = if *permanent { DeleteMode::Hard } else { DeleteMode::ToDeletedItems };
            if in_deleted && !permanent {
                bail!("письмо #{item_pk} уже в «Удалённых»: используйте --permanent для удаления навсегда");
            }
            println!("#{item_pk}: папка {}, DeleteItem {}", r.folder_pk, if *permanent { "HardDelete (навсегда)" } else { "MoveToDeletedItems (в «Удалённые»)" });
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let result = t.delete_items(std::slice::from_ref(&r.id), mode).await;
            t.logoff().await.ok();
            let results = result.map_err(|e| anyhow!("DeleteItem: {e}"))?;
            match results.first() {
                Some(WriteResult::Ok { id }) => {
                    let new_id = (id.id != r.id.id).then_some(id);
                    match (mode, deleted_items) {
                        (DeleteMode::Hard, _) | (DeleteMode::ToDeletedItems, None) => {
                            let convs = store.mark_deleted_local(&[*item_pk])?;
                            println!("DeleteItem: Ok; локально deleted = 1, бесед затронуто {}", convs.len());
                        }
                        (DeleteMode::ToDeletedItems, Some((trash_pk, _))) => {
                            store.move_local(*item_pk, trash_pk, new_id)?;
                            println!("DeleteItem: Ok; новый ItemId в ответе {}; локально письмо в «Удалённых» (folder_pk {trash_pk})", if new_id.is_some() { "есть" } else { "нет" });
                        }
                    }
                }
                Some(WriteResult::Failed { code, message, .. }) => println!("DeleteItem: Failed {code}: {message}"),
                None => println!("DeleteItem: пустой ответ"),
            }
        }
        Cmd::FolderCreate { name, parent, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let name = aiowa_core::validate_folder_name(name).map_err(|e| anyhow!(e))?;
            let (parent_ref, parent_server_id, parent_label) = resolve_parent(&store, account, parent.as_deref())?;
            if let Some(existing) = store.resolve_folder(account, &name)? {
                println!("Внимание: папка с именем «{}» уже есть в базе (folder_pk {}); сервер откажет, если она в том же родителе.", existing.name, existing.folder_pk);
            }
            println!("Создать папку «{name}» в {parent_label} операцией CreateFolder (класс IPF.Note)");
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let started = std::time::Instant::now();
            let result = t.create_folder(&parent_ref, &name).await;
            // Локальная правка и иерархия — без `?` до logoff: ошибка базы не должна оставлять сессию.
            let outcome = match &result {
                Ok(id) => Some(
                    async {
                        let pk = store.insert_folder(account, &new_mail_folder(id, parent_server_id.as_deref(), &name))?;
                        let changes = refresh_hierarchy(&t, &store, account).await?;
                        Ok::<_, anyhow::Error>((pk, changes))
                    }
                    .await,
                ),
                Err(_) => None,
            };
            t.logoff().await.ok();
            let id = result.map_err(|e| anyhow!("CreateFolder: {e}"))?;
            let (pk, changes) = outcome.expect("при успехе заполнено")?;
            println!(
                "CreateFolder: Ok за {} мс; {}; локально folder_pk {pk}; SyncFolderHierarchy: изменений {}, папка {}",
                started.elapsed().as_millis(),
                describe_folder_id(&id),
                changes.len(),
                hierarchy_mention(&changes, &id.id).map(|(k, _)| k).unwrap_or("не упомянута")
            );
        }
        Cmd::FolderRename { folder_pk, name, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let f = store.folder_by_pk(account, *folder_pk)?.ok_or_else(|| anyhow!("папка #{folder_pk} не найдена в базе или удалена"))?;
            if let Some(d) = &f.distinguished {
                bail!("папка «{}» служебная ({d}): переименовывать нельзя", f.name);
            }
            let name = aiowa_core::validate_folder_name(name).map_err(|e| anyhow!(e))?;
            if name == f.name {
                bail!("папка #{folder_pk} уже называется «{name}»");
            }
            let id = store.folder_id_by_pk(account, *folder_pk)?.ok_or_else(|| anyhow!("папка #{folder_pk} не найдена"))?;
            println!("Переименовать папку #{folder_pk} «{}» → «{name}» операцией UpdateFolder (folder:DisplayName)", f.name);
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let started = std::time::Instant::now();
            let result = t.rename_folder(&id, &name).await;
            // Локальная правка и иерархия — без `?` до logoff: ошибка базы не должна оставлять сессию.
            let changes = match &result {
                Ok(new_id) => Some(
                    async {
                        store.rename_folder_local(*folder_pk, &name, Some(new_id))?;
                        refresh_hierarchy(&t, &store, account).await
                    }
                    .await,
                ),
                Err(_) => None,
            };
            t.logoff().await.ok();
            let new_id = result.map_err(|e| anyhow!("UpdateFolder: {e}"))?;
            let changes = changes.expect("при успехе заполнено")?;
            println!(
                "UpdateFolder: Ok за {} мс; в ответе {}, Id {}; локально имя «{name}»; SyncFolderHierarchy: изменений {}, папка {}",
                started.elapsed().as_millis(),
                describe_folder_id(&new_id),
                if new_id.id == id.id { "тот же" } else { "другой" },
                changes.len(),
                hierarchy_mention(&changes, &new_id.id).map(|(k, n)| format!("{k}{}", if n == Some(name.as_str()) { ", имя совпало" } else { "" })).unwrap_or_else(|| "не упомянута".into())
            );
        }
        Cmd::FolderDelete { folder_pk, permanent, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let f = store.folder_by_pk(account, *folder_pk)?.ok_or_else(|| anyhow!("папка #{folder_pk} не найдена в базе или удалена"))?;
            if let Some(d) = &f.distinguished {
                bail!("папка «{}» служебная ({d}): удалять нельзя", f.name);
            }
            let trash = store.folder_by_distinguished(account, "deleteditems")?;
            let in_trash = match &trash {
                Some((trash_pk, _)) => folder_is_under(&store, account, *folder_pk, *trash_pk)?,
                None => false,
            };
            if in_trash && !permanent {
                bail!("папка #{folder_pk} уже в «Удалённых»: используйте --permanent для удаления навсегда");
            }
            if !permanent && trash.is_none() {
                bail!("папка «Удалённые» не найдена в локальной базе: выполните синхронизацию или используйте --permanent");
            }
            let id = store.folder_id_by_pk(account, *folder_pk)?.ok_or_else(|| anyhow!("папка #{folder_pk} не найдена"))?;
            println!("Удалить папку #{folder_pk} «{}» ({} писем по счётчику) операцией DeleteFolder {}", f.name, f.total_count, if *permanent { "HardDelete (навсегда, с содержимым и подпапками)" } else { "MoveToDeletedItems (в «Удалённые»)" });
            if !yes_i_know {
                println!("Это запись в настоящий ящик. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let started = std::time::Instant::now();
            let result = t.delete_folder(&id, *permanent).await;
            // Локальная правка и иерархия — без `?` до logoff: ошибка базы не должна оставлять сессию.
            let outcome = match &result {
                Ok(()) => Some(
                    async {
                        let st = store.delete_folder_local(*folder_pk, if *permanent { None } else { trash.as_ref().map(|(pk, _)| *pk) })?;
                        let changes = refresh_hierarchy(&t, &store, account).await?;
                        Ok::<_, anyhow::Error>((st, changes))
                    }
                    .await,
                ),
                Err(_) => None,
            };
            t.logoff().await.ok();
            result.map_err(|e| anyhow!("DeleteFolder: {e}"))?;
            let (st, changes) = outcome.expect("при успехе заполнено")?;
            println!(
                "DeleteFolder: Ok за {} мс; локально папок {}, писем помечено {}; SyncFolderHierarchy: изменений {}, папка {}",
                started.elapsed().as_millis(),
                st.folders,
                st.items,
                changes.len(),
                hierarchy_mention(&changes, &id.id).map(|(k, _)| k).unwrap_or("не упомянута")
            );
        }
        Cmd::FolderCheck { probe_reply_to, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            println!("План одной сессией: CreateFolder «{FOLDER_CHECK_NAME}» в корне ящика → SyncFolderHierarchy → UpdateFolder «{FOLDER_CHECK_NAME_2}» → SyncFolderHierarchy → DeleteFolder HardDelete → SyncFolderHierarchy{}. Письма не трогаются.", probe_reply_to.map(|n| format!(" → GetItem ItemPart + message:ReplyTo по {n} письмам «Входящих» (только чтение)")).unwrap_or_default());
            if !yes_i_know {
                println!("Это запись в настоящий ящик (одна тестовая папка). Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let result = folder_check_flow(&t, &store, account).await;
            let probe = match probe_reply_to {
                Some(n) => Some(reply_to_probe(&t, &store, account, "inbox", *n).await),
                None => None,
            };
            t.logoff().await.ok();
            result?;
            if let Some(p) = probe {
                p?;
            }
        }
        Cmd::ReplyToProbe { folder, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let t = connect(&cli).await?;
            let result = reply_to_probe(&t, &store, account, folder, *limit).await;
            t.logoff().await.ok();
            result?;
        }
        Cmd::Send { to, to_self, cc, subject, body, attach, yes_i_know, keep_draft } => {
            let mut recipients = split_addresses(to);
            if *to_self {
                let store = Store::open(&db_path)?;
                let account = store.account(&cli.host, &cli.user)?;
                let me = store.own_addresses(account)?.into_iter().next().ok_or_else(|| anyhow!("в локальной базе нет писем в «Отправленных»: собственный адрес неизвестен"))?;
                recipients.push(me);
            }
            let cc = split_addresses(cc);
            if recipients.is_empty() {
                bail!("нужен хотя бы один получатель: --to или --to-self");
            }
            if let Some(bad) = recipients.iter().chain(&cc).find(|a| !looks_like_email(a)) {
                bail!("адрес «{bad}» не похож на почтовый");
            }
            let files = load_attachments(attach)?;
            let msg = OutgoingMessage { to: participants(&recipients), cc: participants(&cc), bcc: vec![], subject: subject.clone(), body_html: text_to_html(body), importance: Importance::Normal };
            println!(
                "План: Кому {}{}, Копия {}, тема «{}», тело {} символов, вложений {} ({} байт)",
                recipients.len(),
                if *to_self { " (в том числе самому себе)" } else { "" },
                cc.len(),
                subject,
                body.chars().count(),
                files.len(),
                files.iter().map(|f| f.bytes.len()).sum::<usize>()
            );
            if !yes_i_know {
                println!("Письмо уйдёт из настоящего ящика. Повторите с --yes-i-know, чтобы отправить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let outcome = send_flow(&t, &msg, &files, *keep_draft).await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::DraftCheck { sync_drafts, yes_i_know } => {
            let store = Arc::new(Store::open(&db_path)?);
            let account = store.account(&cli.host, &cli.user)?;
            let owner = store.own_addresses(account)?.into_iter().next().ok_or_else(|| anyhow!("в локальной базе нет писем в «Отправленных»: собственный адрес неизвестен"))?;
            println!(
                "План одной сессией: CreateItem «{DRAFT_CHECK_SUBJECT}» без получателей → CreateAttachment «{DRAFT_CHECK_ATTACHMENT}» → GetItem → UpdateItem (тема «{DRAFT_CHECK_SUBJECT_2}», новое тело, Кому — свой адрес) → GetItem{} → DeleteAttachment → GetItem → DeleteItem HardDelete{}. Ничего не отправляется.",
                if *sync_drafts { " → Sync «Черновики» и сверка строки в базе" } else { "" },
                if *sync_drafts { " → Sync «Черновики» и сверка удаления" } else { "" }
            );
            if !yes_i_know {
                println!("Это запись в настоящий ящик (один тестовый черновик, удаляется навсегда). Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let result = draft_check_flow(&t, &store, account, &owner, *sync_drafts).await;
            t.logoff().await.ok();
            result?;
        }
        Cmd::Signature => {
            let t = connect(&cli).await?;
            let result = t.user_signature().await;
            t.logoff().await.ok();
            match result.map_err(|e| anyhow!("GetUserConfiguration: {e}"))? {
                Some(sig) => {
                    println!("Подпись в OWA: {} символов HTML, тегов {}, добавлять автоматически: {}", sig.html.chars().count(), sig.html.matches('<').count(), if sig.auto_add { "да" } else { "нет" });
                    println!("{}", sig.html);
                }
                None => println!("Подпись в OWA не задана."),
            }
        }
        Cmd::Suggest { prefix, limit } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let started = std::time::Instant::now();
            let rows = store.suggest_recipients(account, prefix, *limit)?;
            println!("{} подсказок за {} мс", rows.len(), started.elapsed().as_millis());
            for r in rows {
                println!("{:>6}  {}  {}", r.weight, pad(r.name.as_deref().unwrap_or(""), 30), r.email);
            }
        }
        Cmd::Directory { query, resolve, expand, prefix3, max, probe_shapes, dl_candidates, dl_query } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let query = match query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
                Some(q) => {
                    println!("Запрос из аргумента: {} символов", q.chars().count());
                    q.to_string()
                }
                None => {
                    let (word, how) = owner_surname(&store, account)?;
                    println!("Запрос: {how}");
                    word
                }
            };
            let t = connect(&cli).await?;
            let opts = DirectoryOpts { query: &query, resolve: *resolve, expand: expand.as_deref(), prefix3: *prefix3, max: *max, probe_shapes: *probe_shapes, dl_candidates: *dl_candidates, dl_queries: dl_query };
            let result = directory_flow(&t, &store, account, &opts).await;
            t.logoff().await.ok();
            result?;
        }
        Cmd::Calendar { from, to, details, offline, probe } => {
            let start = local_midnight(from)?;
            let end = local_midnight(to)?;
            if end <= start {
                bail!("конец окна должен быть позже начала");
            }
            let (start_ms, end_ms) = (start.as_millisecond(), end.as_millisecond());
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            match store.calendar_range_age(account, start_ms, end_ms)? {
                Some(age) => println!("Окно {from} – {to} (UTC {start} – {end}): в кэше, загружено {} с назад", age / 1000),
                None => println!("Окно {from} – {to} (UTC {start} – {end}): в кэше не покрыто"),
            }
            if *offline {
                print_calendar_rows(&store.calendar_events(account, start_ms, end_ms)?);
                return Ok(());
            }
            let t = connect(&cli).await?;
            let started = std::time::Instant::now();
            let fetched = t.find_calendar_view(start, end).await;
            let outcome = Box::pin(async {
                let events = fetched.map_err(|e| anyhow!("FindItem CalendarView: {e}"))?;
                println!("FindItem CalendarView: {} событий за {} мс", events.len(), started.elapsed().as_millis());
                let stored = store.replace_calendar_range(account, start_ms, end_ms, &events)?;
                let rows = store.calendar_events(account, start_ms, end_ms)?;
                println!("Кэш: записано {stored}, в окне из базы {}, возраст окна {:?} с", rows.len(), store.calendar_range_age(account, start_ms, end_ms)?.map(|a| a / 1000));
                print_calendar_rows(&rows);
                if let Some(sel) = details {
                    let mut index = None;
                    for s in sel.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        index = match s {
                            "recurring" => rows.iter().position(|r| CalendarItemType::parse(&r.calendar_type).is_recurring()),
                            "meeting" => rows.iter().position(|r| r.is_meeting),
                            s => Some(s.parse::<usize>().with_context(|| format!("--details: ожидается номер, recurring или meeting, получено «{s}»"))?.wrapping_sub(1)),
                        };
                        if index.is_some() {
                            break;
                        }
                    }
                    let Some(index) = index else {
                        println!("GetItem: в окне нет события вида «{sel}», подробности не запрошены.");
                        return Ok(());
                    };
                    let n = index + 1;
                    let row = rows.get(index).ok_or_else(|| anyhow!("события №{n} нет в списке из {}", rows.len()))?;
                    let started = std::time::Instant::now();
                    let d = t.get_calendar_item(&row.id).await.map_err(|e| anyhow!("GetItem события: {e}"))?;
                    let body_len = d.body.as_ref().and_then(|b| b.full_body.as_ref()).map(|s| s.chars().count()).unwrap_or(0);
                    let mut by_response: std::collections::BTreeMap<&'static str, usize> = Default::default();
                    for a in &d.attendees {
                        *by_response.entry(a.response.as_str()).or_default() += 1;
                    }
                    println!(
                        "GetItem №{n}: Ok за {} мс; тело {} символов{}, тип {:?}, участников {} (обязательных {}, по ответам {:?}), напоминание {}, повторение: {}, место {}, ChangeKey {}",
                        started.elapsed().as_millis(),
                        body_len,
                        d.body.as_ref().map(|b| if b.is_truncated { " (усечено)" } else { "" }).unwrap_or(" (нет)"),
                        d.body.as_ref().map(|b| b.body_type),
                        d.attendees.len(),
                        d.attendees.iter().filter(|a| a.required).count(),
                        by_response,
                        d.reminder_minutes.map(|m| format!("за {m} мин")).unwrap_or_else(|| "нет".into()),
                        d.recurrence_text.as_deref().unwrap_or("нет"),
                        d.event.location.as_ref().map(|l| format!("есть ({} символов)", l.chars().count())).unwrap_or_else(|| "нет".into()),
                        describe_change_key(&row.id, &d.event.id)
                    );
                    store.store_calendar_details(row.calendar_pk, &d)?;
                    let (body, attendees, reminder, recurrence) = store.calendar_details(row.calendar_pk)?.ok_or_else(|| anyhow!("подробности не сохранились"))?;
                    println!("Кэш подробностей: тело {} символов, участников {}, напоминание {:?}, повторение {}", body.map(|b| b.chars().count()).unwrap_or(0), attendees.len(), reminder, if recurrence.is_some() { "есть" } else { "нет" });
                    if *probe {
                        let started = std::time::Instant::now();
                        let shape = t.probe_calendar_item_shape(&row.id).await.map_err(|e| anyhow!("GetItem AllProperties: {e}"))?;
                        println!("GetItem AllProperties за {} мс, форма элемента (без содержимого):\n{shape}", started.elapsed().as_millis());
                    }
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::CalendarCreate { subject, start, end, all_day, location, reminder, free_busy, private, body, required, optional, yes_i_know } => {
            let (start_ts, end_ts) = if *all_day { (all_day_bound(start)?, all_day_bound(end)?) } else { (local_datetime(start)?, local_datetime(end)?) };
            if end_ts <= start_ts {
                bail!("конец должен быть позже начала");
            }
            if subject.trim().is_empty() {
                bail!("тема пуста");
            }
            if reminder.is_some_and(|m| m < 0) {
                bail!("напоминание не может быть отрицательным");
            }
            let item = NewCalendarItem {
                subject: subject.clone(),
                start: start_ts,
                end: end_ts,
                is_all_day: *all_day,
                location: location.clone(),
                body_html: body.as_deref().map(text_to_html),
                required: attendee_participants(required, "обязательные участники")?,
                optional: attendee_participants(optional, "необязательные участники")?,
                reminder_minutes: *reminder,
                free_busy: parse_free_busy(free_busy)?,
                sensitivity_private: *private,
            };
            let attendees = item.required.len() + item.optional.len();
            println!(
                "План: событие {} – {} (UTC {} – {}), весь день {}, тема {} символов, место {}, напоминание {}, занятость {}, частное {}, описание {} символов, участников {}",
                fmt_local(start_ts.as_millisecond()),
                fmt_local(end_ts.as_millisecond()),
                start_ts,
                end_ts,
                if *all_day { "да" } else { "нет" },
                subject.chars().count(),
                if location.as_deref().map(str::trim).is_some_and(|l| !l.is_empty()) { "есть" } else { "нет" },
                reminder.map(|m| format!("за {m} мин")).unwrap_or_else(|| "нет".into()),
                item.free_busy.as_str(),
                if *private { "да" } else { "нет" },
                body.as_ref().map(|b| b.chars().count()).unwrap_or(0),
                attendees
            );
            if attendees > 0 {
                println!("ВНИМАНИЕ: с участниками это встреча, сервер разошлёт им приглашения (SendToAllAndSaveCopy).");
            }
            if !yes_i_know {
                println!("Это запись в настоящий календарь. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let t = connect(&cli).await?;
            let outcome = async {
                let zone = calendar_time_zone(&t, &store, account).await?;
                let started = std::time::Instant::now();
                let id = t.create_calendar_item(&item, zone.as_deref()).await.map_err(|e| anyhow!("CreateItem CalendarItem: {e}"))?;
                println!("CreateItem CalendarItem: Ok за {} мс; ItemId {} символов, ChangeKey {}", started.elapsed().as_millis(), id.id.len(), id.change_key.as_ref().map(|c| format!("{} символов", c.len())).unwrap_or_else(|| "нет".into()));
                refresh_and_show(&t, &store, account, start_ts, &id, true).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::CalendarUpdate { calendar_pk, subject, start, end, location, clear_location, reminder, no_reminder, free_busy, send_updates, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let row = store.calendar_event(*calendar_pk)?.ok_or_else(|| anyhow!("события #{calendar_pk} нет в кэше календаря (сначала `calendar --from --to`)"))?;
            if location.is_some() && *clear_location {
                bail!("--location и --clear-location вместе не имеют смысла");
            }
            if reminder.is_some() && *no_reminder {
                bail!("--reminder и --no-reminder вместе не имеют смысла");
            }
            let parse_at = |s: &str| if row.is_all_day { all_day_bound(s) } else { local_datetime(s) };
            let mut changes = CalendarItemChanges::default();
            let mut fields: Vec<&str> = Vec::new();
            if let Some(s) = subject {
                if s.trim().is_empty() {
                    bail!("тема пуста");
                }
                changes.subject = Some(s.clone());
                fields.push("тема");
            }
            if let Some(s) = start {
                changes.start = Some(parse_at(s)?);
                fields.push("начало");
            }
            if let Some(s) = end {
                changes.end = Some(parse_at(s)?);
                fields.push("конец");
            }
            if row.is_all_day && (changes.start.is_some() || changes.end.is_some()) {
                // Признак рядом с границами: по нему транспорт шлёт местные полуночи дат.
                changes.is_all_day = Some(true);
            }
            if let Some(l) = location {
                changes.location = Some(Some(l.clone()));
                fields.push("место");
            } else if *clear_location {
                changes.location = Some(None);
                fields.push("место (снять)");
            }
            if let Some(m) = reminder {
                if *m < 0 {
                    bail!("напоминание не может быть отрицательным");
                }
                changes.reminder_minutes = Some(Some(*m));
                fields.push("напоминание");
            } else if *no_reminder {
                changes.reminder_minutes = Some(None);
                fields.push("напоминание (выключить)");
            }
            if let Some(fb) = free_busy {
                changes.free_busy = Some(parse_free_busy(fb)?);
                fields.push("занятость");
            }
            if changes.is_empty() {
                bail!("нечего менять: укажите хотя бы одно поле");
            }
            let new_start = changes.start.map(|t| t.as_millisecond()).unwrap_or(row.start_at);
            let new_end = changes.end.map(|t| t.as_millisecond()).unwrap_or(row.end_at);
            // У всё-день границы в разных кодировках (новые — полуночи UTC дат, из кэша — местные
            // полуночи пояса пользователя): `updated_span` сравнивает их по датам.
            let span = updated_span(row.is_all_day, new_start, new_end)?;
            println!("Сейчас: {}", describe_calendar_row(&store, &row)?);
            println!("План: UpdateItem полей [{}], станет {span}, рассылка участникам: {}", fields.join(", "), if *send_updates { "да (SendToAllAndSaveCopy)" } else { "нет (SendToNone)" });
            if row.is_meeting && !send_updates {
                println!("Это встреча с участниками: без --send-updates они не узнают об изменении.");
            }
            if !yes_i_know {
                println!("Это запись в настоящий календарь. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let outcome = async {
                let zone = calendar_time_zone(&t, &store, account).await?;
                let started = std::time::Instant::now();
                let updated = t.update_calendar_item(&row.id, &changes, *send_updates, zone.as_deref()).await.map_err(|e| anyhow!("UpdateItem CalendarItem: {e}"))?;
                println!("UpdateItem CalendarItem: Ok за {} мс; ItemId {}, ChangeKey {}", started.elapsed().as_millis(), if updated.id == row.id.id { "тот же" } else { "другой" }, describe_change_key(&row.id, &updated));
                let old_day = local_day_window(jiff::Timestamp::from_millisecond(row.start_at)?)?;
                let new_at = jiff::Timestamp::from_millisecond(new_start)?;
                if local_day_window(new_at)? != old_day {
                    // День сменился: старое окно перечитывается, чтобы событие ушло из него.
                    refresh_calendar_window(&t, &store, account, old_day.0, old_day.1).await?;
                }
                refresh_and_show(&t, &store, account, new_at, &updated, true).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::CalendarDelete { calendar_pk, permanent, send_cancellations, yes_i_know } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let row = store.calendar_event(*calendar_pk)?.ok_or_else(|| anyhow!("события #{calendar_pk} нет в кэше календаря (сначала `calendar --from --to`)"))?;
            let mode = if *permanent { DeleteMode::Hard } else { DeleteMode::ToDeletedItems };
            println!("Сейчас: {}", describe_calendar_row(&store, &row)?);
            println!("План: DeleteItem {}, рассылка отмены участникам: {}", if *permanent { "HardDelete (навсегда)" } else { "MoveToDeletedItems (в «Удалённые»)" }, if *send_cancellations { "да (SendToAllAndSaveCopy)" } else { "нет (SendToNone)" });
            if row.is_meeting && row.my_response != "Organizer" {
                if *send_cancellations {
                    bail!("--send-cancellations у чужой встречи: сервер отправил бы организатору отказ (Decline), а не отмену. Удалите без флага или ответьте «Отклонить» из окна.");
                }
                println!("ВНИМАНИЕ: это чужая встреча (вы не организатор): удаление уберёт её только из вашего календаря, организатор не узнает (SendToNone).");
            }
            if !yes_i_know {
                println!("Это запись в настоящий календарь. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let t = connect(&cli).await?;
            let outcome = async {
                let started = std::time::Instant::now();
                t.delete_calendar_item_with(&row.id, mode, *send_cancellations).await.map_err(|e| anyhow!("DeleteItem события: {e}"))?;
                println!("DeleteItem: Ok за {} мс", started.elapsed().as_millis());
                store.remove_calendar_event(*calendar_pk)?;
                refresh_and_show(&t, &store, account, jiff::Timestamp::from_millisecond(row.start_at)?, &row.id, false).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::CalendarCheck { date, all_day, probe, yes_i_know } => {
            let date: jiff::civil::Date = match date {
                Some(d) => d.trim().parse().with_context(|| format!("дата «{d}» не в формате YYYY-MM-DD"))?,
                None => jiff::Zoned::now().date().tomorrow().map_err(|e| anyhow!("{e}"))?,
            };
            println!(
                "План: пояс пользователя из OWA → {date} {} «{CALENDAR_CHECK_SUBJECT}» без участников, место, напоминание 15 мин, занятость Tentative → FindItem дня → {} и смена места → FindItem → снятие места → FindItem → HardDelete → FindItem. Записей на сервер: 4. Одна сессия, приглашения никому не уходят.",
                if *all_day { "весь день" } else { "09:00–09:30" },
                if *all_day { "смена темы" } else { "перенос на 10:00–10:45" }
            );
            if !yes_i_know {
                println!("Это запись в настоящий календарь. Повторите с --yes-i-know, чтобы выполнить.");
                return Ok(());
            }
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let t = connect(&cli).await?;
            let outcome = Box::pin(calendar_check_flow(&t, &store, account, date, *all_day, *probe)).await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::MeetingInfo { item_pk, probe } => {
            let store = Store::open(&db_path)?;
            let mut targets: Vec<(i64, ItemId, Option<String>)> = Vec::new();
            for s in item_pk.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let pk: i64 = s.parse().with_context(|| format!("item_pk «{s}» не число"))?;
                let r = store.item_ref(pk)?.ok_or_else(|| anyhow!("письмо #{pk} не найдено в базе или удалено"))?;
                let class = store.message_head(pk)?.and_then(|h| h.item_class);
                targets.push((pk, r.id, class));
            }
            if targets.is_empty() {
                bail!("нужен хотя бы один item_pk");
            }
            let t = connect(&cli).await?;
            let outcome = async {
                for (pk, id, class) in &targets {
                    let started = std::time::Instant::now();
                    match t.get_meeting_request(id).await {
                        Ok(info) => {
                            let duration = match (info.start, info.end) {
                                (Some(s), Some(e)) => format!("{} мин", e.duration_since(s).as_mins()),
                                _ => "?".into(),
                            };
                            println!(
                                "#{pk}: класс в базе {}; GetItem за {} мс: вид {}, начало {}, конец {}, длительность {}, весь день {}, место {}, организатор {}, мой ответ {}, устарело {}, связанное событие {}, повторение {}, ответ отправителя {}",
                                class.as_deref().unwrap_or("(нет)"),
                                started.elapsed().as_millis(),
                                info.kind.key(),
                                info.start.map(|s| fmt_local(s.as_millisecond())).unwrap_or_else(|| "нет".into()),
                                info.end.map(|e| fmt_local(e.as_millisecond())).unwrap_or_else(|| "нет".into()),
                                duration,
                                if info.is_all_day { "да" } else { "нет" },
                                info.location.as_ref().map(|l| format!("есть ({} символов)", l.chars().count())).unwrap_or_else(|| "нет".into()),
                                match &info.organizer {
                                    Some(p) => format!("есть ({}{})", if p.name.is_some() { "имя" } else { "" }, if p.email.is_some() { "+адрес" } else { "" }),
                                    None => "нет".into(),
                                },
                                info.my_response.as_str(),
                                if info.is_out_of_date { "да" } else { "нет" },
                                info.associated_calendar_item.as_ref().map(|i| format!("есть (Id {} символов)", i.id.len())).unwrap_or_else(|| "нет".into()),
                                if info.recurrence_text.is_some() { "есть" } else { "нет" },
                                info.response_type_of_sender.map(|r| r.as_str()).unwrap_or("нет"),
                            );
                        }
                        Err(e) => println!("#{pk}: класс в базе {}; GetItem: {e}", class.as_deref().unwrap_or("(нет)")),
                    }
                    if *probe {
                        let started = std::time::Instant::now();
                        match t.probe_meeting_request_shape(id).await {
                            Ok(shape) => println!("#{pk}: GetItem AllProperties за {} мс, форма элемента (без содержимого):\n{shape}", started.elapsed().as_millis()),
                            Err(e) => println!("#{pk}: GetItem AllProperties: {e}"),
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::Notify { seconds, folders, rows_only } => {
            let store = Store::open(&db_path)?;
            let account = store.account(&cli.host, &cli.user)?;
            let mut targets: Vec<(FolderId, String)> = Vec::new();
            for name in folders.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let f = store.resolve_folder(account, name)?.ok_or_else(|| anyhow!("папка «{name}» не найдена в локальной базе"))?;
                if !targets.iter().any(|(id, _)| id.id == f.server_id) {
                    targets.push((FolderId { id: f.server_id.clone(), change_key: None }, f.name.clone()));
                }
            }
            if targets.is_empty() {
                bail!("нужна хотя бы одна папка: --folders inbox");
            }
            let calendar = store.folder_by_distinguished(account, "calendar")?.map(|(_, server_id)| FolderId { id: server_id, change_key: None });
            if calendar.is_none() && !rows_only {
                println!("Папка календаря в локальной базе не найдена: подписка CalendarItemNotification пропущена.");
            }
            let mut folder_names: std::collections::HashMap<String, String> = targets.iter().map(|(f, n)| (f.id.clone(), n.clone())).collect();
            if let Some(c) = &calendar {
                folder_names.insert(c.id.clone(), "Календарь".into());
            }
            let subs = notify_subscriptions(&targets, calendar.as_ref(), *rows_only);
            let t = connect(&cli).await?;
            let outcome = async {
                subscribe_and_report(&t, &subs, &folder_names).await?;
                let cid = new_guid();
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<NotificationEvent>();
                let sink = move |ev: NotificationEvent| {
                    let _ = tx.send(ev);
                };
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(*seconds);
                let started = std::time::Instant::now();
                let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
                let mut cycles = 0usize;
                println!("Поток PendingNotificationRequest: читаю {seconds} с (cid {} символов)", cid.len());
                let mut timed_out = false;
                'outer: loop {
                    cycles += 1;
                    // Будущее потока живёт в этом блоке: выход из него по `break 'outer` закрывает GET.
                    {
                        let stream = t.notification_stream(&cid, &sink);
                        tokio::pin!(stream);
                        loop {
                            tokio::select! {
                                r = &mut stream => {
                                    match r {
                                        Ok(end) => println!("[{:>6.1} с] цикл {cycles} завершён: событий {}, {:.1} с, закрыт сервером: {}", started.elapsed().as_secs_f32(), end.events, end.elapsed.as_secs_f32(), end.finished_by_server),
                                        Err(e) => {
                                            println!("[{:>6.1} с] цикл {cycles} прерван: {e}", started.elapsed().as_secs_f32());
                                            break 'outer;
                                        }
                                    }
                                    break;
                                }
                                Some(ev) = rx.recv() => {
                                    *counts.entry(ev.kind_name()).or_default() += 1;
                                    println!("[{:>6.1} с] {}", started.elapsed().as_secs_f32(), describe_event(&ev, &folder_names));
                                    if matches!(ev, NotificationEvent::Reload) {
                                        subscribe_and_report(&t, &subs, &folder_names).await?;
                                    }
                                }
                                _ = tokio::time::sleep_until(deadline) => {
                                    println!("[{:>6.1} с] время вышло, закрываю поток", started.elapsed().as_secs_f32());
                                    timed_out = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                    // Между циклами сервер ждёт FinishNotificationRequest.
                    match t.finish_notification_stream(&cid).await {
                        Ok(()) => println!("[{:>6.1} с] FinishNotificationRequest: Ok", started.elapsed().as_secs_f32()),
                        Err(e) => println!("[{:>6.1} с] FinishNotificationRequest: {e}", started.elapsed().as_secs_f32()),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                }
                if timed_out {
                    match t.finish_notification_stream(&cid).await {
                        Ok(()) => println!("[{:>6.1} с] FinishNotificationRequest: Ok", started.elapsed().as_secs_f32()),
                        Err(e) => println!("[{:>6.1} с] FinishNotificationRequest: {e}", started.elapsed().as_secs_f32()),
                    }
                }
                // Что успело дойти, пока закрывали.
                while let Ok(ev) = rx.try_recv() {
                    *counts.entry(ev.kind_name()).or_default() += 1;
                    println!("[{:>6.1} с] {}", started.elapsed().as_secs_f32(), describe_event(&ev, &folder_names));
                }
                println!("Итого за {:.1} с: циклов {cycles}, событий по видам {counts:?}", started.elapsed().as_secs_f32());
                Ok::<(), anyhow::Error>(())
            }
            .await;
            t.logoff().await.ok();
            outcome?;
        }
        Cmd::Sql { query } => {
            let store = Store::open(&db_path)?;
            for line in store.debug_query(query)? {
                println!("{line}");
            }
        }
        Cmd::SetPassword | Cmd::ForgetPassword => unreachable!(),
    }
    Ok(())
}

fn aiowa_extract_limit() -> usize {
    SyncOptions::default().attachment_text_chars
}

/// Как изменился `ChangeKey` после операции записи (сами значения не печатаются).
fn describe_change_key(before: &aiowa_core::ItemId, after: &aiowa_core::ItemId) -> &'static str {
    match (&before.change_key, &after.change_key) {
        (_, None) => "в ответе отсутствует",
        (Some(b), Some(a)) if b == a => "тот же",
        _ => "новый",
    }
}

fn print_folder_tree(changes: &[FolderChange]) {
    let folders: Vec<&aiowa_core::Folder> = changes
        .iter()
        .filter_map(|c| match c {
            FolderChange::Create(f) | FolderChange::Update(f) => Some(f),
            FolderChange::Delete(_) => None,
        })
        .collect();
    let roots: Vec<&aiowa_core::Folder> = {
        let ids: std::collections::HashSet<&str> = folders.iter().map(|f| f.id.id.as_str()).collect();
        folders.iter().copied().filter(|f| f.parent_id.as_ref().map(|p| !ids.contains(p.id.as_str())).unwrap_or(true)).collect()
    };
    fn walk(f: &aiowa_core::Folder, all: &[&aiowa_core::Folder], depth: usize) {
        let marker = if f.is_mail() { "" } else { " ·" };
        println!(
            "{}{}{}  [{} / {} непроч.]{}",
            "  ".repeat(depth),
            f.display_name,
            f.distinguished.as_deref().map(|d| format!(" ({d})")).unwrap_or_default(),
            f.total_count,
            f.unread_count,
            marker
        );
        let mut children: Vec<&&aiowa_core::Folder> = all.iter().filter(|c| c.parent_id.as_ref().map(|p| p.id == f.id.id).unwrap_or(false)).collect();
        children.sort_by_key(|c| c.display_name.to_lowercase());
        for c in children {
            walk(c, all, depth + 1);
        }
    }
    let mut roots = roots;
    roots.sort_by_key(|f| f.display_name.to_lowercase());
    for r in roots {
        walk(r, &folders, 0);
    }
    println!("\nВсего папок: {} (· = не почтовая)", folders.len());
    let _ = SyncState(String::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        s.parse::<jiff::Timestamp>().unwrap().as_millisecond()
    }

    #[test]
    fn all_day_update_compares_civil_dates() {
        // Событие 26.10 в кэше — местные полуночи Москвы; `--end 2026-10-26` (полночь UTC) — та же дата: отказ.
        let cached_start = ms("2026-10-25T21:00:00Z");
        let cached_end = ms("2026-10-26T21:00:00Z");
        assert!(updated_span(true, cached_start, ms("2026-10-26T00:00:00Z")).is_err());
        assert_eq!(updated_span(true, cached_start, ms("2026-10-27T00:00:00Z")).unwrap(), "2026-10-26 – 2026-10-27 (весь день, конец исключая)");
        // `--start` без `--end`: конец из кэша (`26.10T21:00Z` — дата 27.10, конец исключая).
        assert!(updated_span(true, ms("2026-10-27T00:00:00Z"), cached_end).is_err());
        assert!(updated_span(true, ms("2026-10-26T00:00:00Z"), cached_end).is_ok());
        // Западный пояс в кэше (`…T04:00:00Z`) — те же даты.
        assert!(updated_span(true, ms("2026-10-26T04:00:00Z"), ms("2026-10-26T00:00:00Z")).is_err());
        assert!(updated_span(true, ms("2026-10-26T04:00:00Z"), ms("2026-10-27T00:00:00Z")).is_ok());
    }

    #[test]
    fn timed_update_compares_moments() {
        assert!(updated_span(false, 1_000, 1_000).is_err());
        assert!(updated_span(false, 2_000, 1_000).is_err());
        assert!(updated_span(false, 1_000, 2_000).is_ok());
    }
}
