//! `aiowa`: консольная утилита этапа 1. Вход, папки, синхронизация в локальную базу,
//! список бесед, поиск с операторами, вложения, операции записи (прочитано, флаг, перенос, удаление),
//! отправка письма, подпись из OWA, подсказки адресов, календарь (окно событий, одно событие,
//! создание, изменение и удаление события, письмо о встрече).
//! Пароль хранится в диспетчере учётных данных Windows и не передаётся аргументом.

use aiowa_core::{AccountId, CalendarItemChanges, CalendarItemType, ComposeKind, FolderChange, FolderId, FolderRef, FreeBusy, Importance, ItemId, NewCalendarItem, OutgoingAttachment, OutgoingMessage, Participant, SyncState};
use aiowa_store::{AttachmentTodo, CalendarRow, Store};
use aiowa_sync::{Engine, FolderSelection, SyncOptions};
use aiowa_transport::{new_guid, Credentials, DeleteMode, MailTransport, NotificationEvent, NotificationKind, SubscriptionRequest, WriteResult};
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
    // Будущее `real_main` в отладочной сборке огромно (все команды — одна функция): на куче,
    // иначе переполняется стек главного потока ещё до разбора аргументов.
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(Box::pin(real_main()))
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
