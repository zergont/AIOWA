//! AIOWA desktop: команды Tauri поверх локальной базы. Чтение из SQLite, синхронизация
//! с сервером по кнопке и по таймеру (`sync.rs`): один транспорт OWA на процесс, logoff при выходе.
//! Тело письма и вложения, которых ещё нет локально, догружаются по требованию (`ondemand.rs`).
//!
//! HTML писем очищается ammonia на стороне Rust и показывается во фрейме с sandbox и CSP;
//! удалённые картинки и внешние ресурсы не загружаются. Инлайновые картинки (`src="cid:…"`)
//! подставляются из кэша вложений как `data:`-URI прямо при очистке.
//!
//! Новое письмо, ответ, ответ всем и пересылка — команды композера (`compose.rs`): черновик на
//! сервере, вложения, отправка через тот же транспорт процесса. Автосохранение, открытие и
//! удаление черновиков — там же.
//!
//! Предпросмотр вложений (`preview.rs`): протокол `aiowa-att` отдаёт окну файлы из кэша вложений
//! (картинки, PDF, текст), команда `attachment_preview_url` при необходимости скачивает файл.
//!
//! Календарь (`calendar.rs`): события окна и подробности события из кэша в базе; сервер —
//! когда кэш не покрывает окно, устарел или окно просит обновить.
//!
//! Канал мгновенных уведомлений (`notify.rs`): после первой удачной синхронизации фоновая задача
//! держит поток `PendingNotificationRequest` и по событиям сервера синхронизирует папки, показывает
//! тосты о новых письмах и ведёт счётчик непрочитанных в заголовке окна.
//!
//! Запись в календарь и приглашения (`calendar_write.rs`): создание, правка и удаление события,
//! ответ на приглашение по событию и по письму, сведения о письме-приглашении. Напоминания
//! (`reminders.rs`): планировщик раз в 30 с смотрит кэш событий, шлёт окну событие `reminder`
//! и тост; «закрыть» и «отложить» хранятся в базе.
//!
//! Справочник организации (`directory.rs`): поиск адресатов (кэш справочника в базе, затем
//! `FindPeople` на сервере с кэшем запросов на 10 минут), карточка контакта (`ResolveNames` с
//! полными данными и счётчики переписки по базе) и участники списка рассылки (`ExpandDL`).
//!
//! Учётная запись при старте (`config.rs`): переменные `AIOWA_HOST`/`AIOWA_USER` (режим
//! разработки) → `%APPDATA%\AIOWA\config.json` → режим первого запуска. В нём нет базы и
//! состояний почты: управляется только `SetupState` и состояния без базы, окно показывает мастер
//! (`setup.rs`), команды почты отвечают ошибкой invoke «state not managed». Мастер пишет конфиг и
//! перезапускает процесс — тот поднимается в режиме почты. База учётной записи не открылась —
//! тоже мастер, с текстом ошибки (`app_mode().boot_error`), а не падение на каждом старте.
//! Настройки окна (`settings.rs`) лежат в `sync_state` базы.
//!
//! Обновление приложения (`updates.rs`): проверка `latest.json` релиза GitHub, загрузка установщика
//! с проверкой подписи ключом обновлений и установка. Перед запуском установщика — те же шаги
//! выхода, что в `RunEvent::Exit` (`MailServices::shutdown`): установщик закрывает процесс сам.
//! Одна база на ящик (`mailbox.rs`): после входа ящик сверяется с базой по `MailboxGuid`, мастер
//! находит базу ящика при любой форме логина. Все базы на диске, их размер и удаление лишних —
//! вкладка «Данные» (`data.rs`).

use aiowa_core::{AccountId, BodyType};
use aiowa_store::ui::{ConversationCursor, MessageHead};
use aiowa_store::{AttachmentRow, Store};
use base64::Engine as _;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tracing::{error, info, warn};

mod calendar;
mod calendar_write;
mod compose;
mod config;
mod data;
mod directory;
mod folders;
mod live;
mod logging;
mod mailbox;
mod navigation;
mod notify;
mod ondemand;
mod preview;
mod reminders;
mod settings;
mod setup;
mod staged;
mod sync;
mod updates;
mod write;

/// Лимит одной инлайновой картинки: больше не скачиваем и не подставляем.
pub(crate) const INLINE_IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Лимит суммарных подстановок `data:` в одно письмо.
pub(crate) const INLINE_TOTAL_MAX_BYTES: u64 = 16 * 1024 * 1024;

struct AppState {
    store: Arc<Store>,
    account: AccountId,
    host: String,
    user: String,
}

type CmdResult<T> = std::result::Result<T, String>;

/// Нормализованный Content-ID → `data:`-URI картинки из кэша.
type InlineMap = HashMap<String, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Закрытие окна при несохранённом письме. Пока окно держит флаг (`window_close_guard`: в
/// композере есть правки или идёт отправка), крестик и Alt+F4 окно не закрывают, а шлют событие
/// `close-requested`; окно спрашивает пользователя и закрывает через `window_close_confirmed`.
/// Без флага окно закрывается как обычно — если фронтенд не поднялся, окно не «залипает».
#[derive(Default)]
struct CloseState {
    guarded: AtomicBool,
}

#[tauri::command]
fn window_close_guard(enabled: bool, st: State<'_, CloseState>) {
    st.guarded.store(enabled, Ordering::Relaxed);
}

/// Пользователь подтвердил закрытие: снять флаг и закрыть окно (через обычный `CloseRequested`,
/// чтобы сработал `RunEvent::Exit` с logoff).
#[tauri::command]
fn window_close_confirmed(window: tauri::WebviewWindow, st: State<'_, CloseState>) -> CmdResult<()> {
    st.guarded.store(false, Ordering::Relaxed);
    window.close().map_err(err)
}

fn ms(t: Option<Timestamp>) -> Option<i64> {
    t.map(|t| t.as_millisecond())
}

#[derive(Serialize)]
struct AccountDto {
    host: String,
    user: String,
    db_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct FolderDto {
    folder_pk: i64,
    parent_pk: Option<i64>,
    name: String,
    distinguished: Option<String>,
    total: i64,
    unread: i64,
    is_mail: bool,
}

/// DTO папок из строк базы: родитель ищется по идентификатору сервера среди тех же строк
/// (корень ящика `msgfolderroot` в базе не хранится, поэтому у папок верхнего уровня `parent_pk = None`).
pub(crate) fn folder_dtos(list: &[(i64, aiowa_core::Folder)]) -> Vec<FolderDto> {
    let by_server: HashMap<&str, i64> = list.iter().map(|(pk, f)| (f.id.id.as_str(), *pk)).collect();
    list.iter()
        .map(|(pk, f)| FolderDto {
            folder_pk: *pk,
            parent_pk: f.parent_id.as_ref().and_then(|p| by_server.get(p.id.as_str()).copied()),
            name: f.display_name.clone(),
            distinguished: f.distinguished.clone(),
            total: f.total_count,
            unread: f.unread_count,
            is_mail: f.is_mail(),
        })
        .collect()
}

#[derive(Serialize)]
struct ConversationDto {
    conversation_pk: i64,
    topic: Option<String>,
    participants: Vec<String>,
    item_count: i64,
    unread_count: i64,
    total_count: i64,
    last_received_ms: Option<i64>,
    has_attachments: bool,
    /// В беседе (в любой папке) есть письмо с флагом «к исполнению».
    has_flagged: bool,
}

#[derive(Serialize)]
struct RecipientDto {
    kind: i64,
    name: Option<String>,
    email: Option<String>,
}

/// Участник без вида получателя: адрес `Reply-To` письма.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ParticipantDto {
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
}

/// Получатель исходящего письма (композер): без вида, адрес обязателен. В обе стороны — в
/// заготовке `compose_context` и в запросе `compose_send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ComposeRecipientDto {
    pub(crate) name: Option<String>,
    pub(crate) email: String,
}

#[derive(Serialize)]
pub(crate) struct AttachmentDto {
    attachment_pk: i64,
    name: Option<String>,
    content_type: Option<String>,
    size: Option<i64>,
    is_inline: bool,
    /// Файл вложения есть в кэше на диске (открытие и сохранение не потребуют сети).
    cached: bool,
    content_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct MessageDto {
    item_pk: i64,
    folder_pk: i64,
    folder_name: Option<String>,
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    display_to: Option<String>,
    received_ms: Option<i64>,
    sent_ms: Option<i64>,
    is_read: bool,
    /// Флаг «к исполнению» поставлен.
    is_flagged: bool,
    is_draft: bool,
    has_attachments: bool,
    importance: Option<String>,
    preview: Option<String>,
    body_state: i64,
    /// Очищенный HTML тела без цитируемой истории; `None`, если тело не загружено.
    body_html: Option<String>,
    body_is_text: bool,
    is_truncated: bool,
    recipients: Vec<RecipientDto>,
    attachments: Vec<AttachmentDto>,
    /// Класс элемента Exchange (`IPM.Note`, приглашение `IPM.Schedule.Meeting.Request`, отмена
    /// `…Canceled`, ответ участника `…Resp.Pos`/`Neg`/`Tent`): по нему окно узнаёт письмо о встрече.
    item_class: Option<String>,
    /// Адреса `Reply-To`, если заголовок есть и отличается от отправителя: ответ уходит сюда, а
    /// карточка показывает «Ответ на: …». Пусто, если заголовка нет или он совпадает с `From`.
    pub(crate) reply_to: Vec<ParticipantDto>,
}

/// Ключ состояния письма для `conversation_key`: по нему окно решает, перечитывать ли беседу.
#[derive(Serialize)]
struct MessageKeyDto {
    item_pk: i64,
    body_state: i64,
    is_read: bool,
    /// Вложений с файлом в кэше по базе (без проверки диска).
    cached: i64,
    is_flagged: bool,
}

#[derive(Serialize)]
struct SearchHitDto {
    item_pk: i64,
    conversation_pk: Option<i64>,
    subject: Option<String>,
    from_name: Option<String>,
    from_email: Option<String>,
    received_ms: Option<i64>,
    folder_name: Option<String>,
    snippet: String,
    attachment_name: Option<String>,
}

/// Ключ сопоставления `src="cid:…"` с `Content-ID` вложения: без схемы `cid:`, URL-декодирован,
/// без угловых скобок и пробелов по краям, в нижнем регистре. На нашем сервере Content-ID приходит
/// без скобок (`image001.jpg@01DD5D86.89427E20`), а в теле стоит `src="cid:image001.jpg@…"`,
/// но у других отправителей бывают `<…>` и кодирование.
pub(crate) fn normalize_cid(raw: &str) -> String {
    let s = raw.trim();
    let s = if s.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("cid:")) { &s[4..] } else { s };
    let decoded = percent_encoding::percent_decode_str(s).decode_utf8_lossy();
    decoded.trim().trim_start_matches('<').trim_end_matches('>').trim().to_lowercase()
}

/// MIME-тип картинки для `data:`-URI: из записи вложения, а если он неизвестен — по сигнатуре файла.
/// Не картинка → `None` (в тело письма подставляются только изображения).
fn image_mime(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    if let Some(ct) = content_type {
        let ct = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
        if ct.starts_with("image/") {
            return Some(ct);
        }
        return None;
    }
    let sniffed = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else {
        return None;
    };
    Some(sniffed.to_string())
}

/// Content-ID, на которые ссылается HTML (`src="cid:…"`, `url(cid:…)`), в нормализованном виде.
/// Нужно, чтобы не читать и не кодировать картинки, которых в показываемом тексте нет: у ответов
/// Outlook картинки цитируемой истории перевложены в каждое письмо, а в UniqueBody их нет.
pub(crate) fn referenced_cids(html: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    // Только ASCII приводится к нижнему регистру, поэтому байтовые смещения совпадают с исходником.
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("cid:") {
        let start = from + pos + 4;
        let rest = &html[start..];
        let end = rest.find(|c: char| matches!(c, '"' | '\'' | ')' | '>' | '<') || c.is_whitespace()).unwrap_or(rest.len());
        let key = normalize_cid(&rest[..end]);
        if !key.is_empty() {
            out.insert(key);
        }
        from = start + end;
    }
    out
}

/// Вложения письма для показа: `cached` проверяется по диску (исчезнувший файл забывается в базе),
/// а инлайновые картинки из кэша, на которые ссылается тело (`wanted`), собираются в карту для
/// подстановки вместо `cid:`. `wanted = None` — тела в HTML нет, подставлять некуда. Сети нет.
pub(crate) fn prepare_attachments(store: &Store, item_pk: i64, wanted: Option<&HashSet<String>>) -> CmdResult<(Vec<AttachmentRow>, InlineMap)> {
    let mut rows = store.attachments_of_item(item_pk).map_err(err)?;
    let mut map = InlineMap::new();
    let mut total: u64 = 0;
    for a in rows.iter_mut() {
        let Some(path) = a.local_path.clone() else { continue };
        if !path.is_file() {
            a.local_path = None;
            if let Err(e) = store.forget_attachment_file(a.attachment_pk) {
                warn!(attachment_pk = a.attachment_pk, error = %e, "не удалось забыть исчезнувший файл кэша");
            }
            continue;
        }
        let Some(wanted) = wanted else { continue };
        if !a.is_inline {
            continue;
        }
        let Some(cid) = a.content_id.as_deref() else { continue };
        let key = normalize_cid(cid);
        if key.is_empty() || !wanted.contains(&key) || map.contains_key(&key) {
            continue;
        }
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
        if len > INLINE_IMAGE_MAX_BYTES || total.saturating_add(len) > INLINE_TOTAL_MAX_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Some(mime) = image_mime(a.content_type.as_deref(), &bytes) else { continue };
        total += bytes.len() as u64;
        map.insert(key, format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)));
    }
    Ok((rows, map))
}

/// Очистка HTML письма. Инлайновые стили разрешены: без них деловая почта нечитаема,
/// а внешние ресурсы из CSS всё равно блокирует CSP фрейма. `src="cid:…"` у картинок
/// заменяется на `data:` из карты; несопоставленные `cid:` теряют атрибут `src`.
fn sanitize_html(html: &str, inline: &Arc<InlineMap>) -> String {
    clean_html(html, Some(inline.clone()))
}

/// Очистка HTML, который уходит на сервер (тело письма, цитата, черновик для редактора): та же
/// ammonia, но `src="cid:…"` остаётся как есть — это ссылки на инлайновые вложения письма на
/// сервере, и подмена на `data:` (или потеря `src`) сломала бы картинки у адресата.
pub(crate) fn sanitize_keep_cid(html: &str) -> String {
    clean_html(html, None)
}

/// `inline = None` — `cid:` не подставляются и не снимаются.
fn clean_html(html: &str, inline: Option<Arc<InlineMap>>) -> String {
    let mut b = ammonia::Builder::default();
    b.add_generic_attributes(["style", "width", "height", "align", "valign", "bgcolor", "border", "cellpadding", "cellspacing", "color", "face", "size"])
        .add_tags(["font", "center", "u", "s", "strike", "tt", "big", "small", "caption", "colgroup", "col", "thead", "tbody", "tfoot", "span", "div", "img", "table", "tr", "td", "th", "hr"])
        .link_rel(Some("noopener noreferrer"))
        .url_schemes(["http", "https", "mailto", "cid", "data"].into_iter().collect());
    b.attribute_filter(move |element, attribute, value| {
        let is_cid = value.trim_start().get(..4).is_some_and(|p| p.eq_ignore_ascii_case("cid:"));
        if element == "img"
            && attribute == "src"
            && is_cid
            && let Some(map) = &inline
        {
            return map.get(&normalize_cid(value)).map(|uri| Cow::Owned(uri.clone()));
        }
        Some(Cow::Borrowed(value))
    });
    b.clean(html).to_string()
}

fn text_to_html(text: &str) -> String {
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<pre style=\"white-space:pre-wrap;font:inherit;margin:0\">{escaped}</pre>")
}

/// Собирает DTO письма из заголовка, тела и вложений (без сети). Читает файлы кэша и кодирует
/// картинки, поэтому вызывается из блокирующего потока, не из обработчика IPC.
pub(crate) fn build_message(store: &Store, h: MessageHead) -> CmdResult<MessageDto> {
    let body = store.message_body(h.item_pk).map_err(err)?;
    let wanted = match &body {
        Some(b) if b.body_type == BodyType::Html => b.unique.as_deref().or(b.full.as_deref()).map(referenced_cids),
        _ => None,
    };
    let (attachments, inline) = prepare_attachments(store, h.item_pk, wanted.as_ref())?;
    let inline = Arc::new(inline);
    let (body_html, body_is_text, is_truncated) = match body {
        Some(b) => {
            let raw = b.unique.or(b.full);
            match (raw, b.body_type) {
                (Some(html), BodyType::Html) => (Some(sanitize_html(&html, &inline)), false, b.is_truncated),
                (Some(text), BodyType::Text) => (Some(text_to_html(&text)), true, b.is_truncated),
                (None, _) => (b.text.as_deref().map(text_to_html), true, b.is_truncated),
            }
        }
        None => (None, false, false),
    };
    let recipients = store.recipients(h.item_pk).map_err(err)?.into_iter().map(|r| RecipientDto { kind: r.kind, name: r.name, email: r.email }).collect();
    let attachments = attachments
        .into_iter()
        .map(|a| AttachmentDto { attachment_pk: a.attachment_pk, name: a.name, content_type: a.content_type, size: a.size, is_inline: a.is_inline, cached: a.local_path.is_some(), content_id: a.content_id })
        .collect();
    let reply_to = effective_reply_to(h.from_name.as_deref(), h.from_email.as_deref(), &h.reply_to);
    Ok(MessageDto {
        item_pk: h.item_pk,
        folder_pk: h.folder_pk,
        folder_name: h.folder_name,
        subject: h.subject,
        from_name: h.from_name,
        from_email: h.from_email,
        display_to: h.display_to,
        received_ms: ms(h.received_at),
        sent_ms: ms(h.sent_at),
        is_read: h.is_read,
        is_flagged: h.is_flagged,
        is_draft: h.is_draft,
        has_attachments: h.has_attachments,
        importance: h.importance,
        preview: h.preview,
        body_state: h.body_state,
        body_html,
        body_is_text,
        is_truncated,
        recipients,
        attachments,
        item_class: h.item_class,
        reply_to,
    })
}

/// Адреса `Reply-To` для показа и ответа: только если заголовок есть и хотя бы один адрес в нём
/// отличается от отправителя (`aiowa_core::effective_reply_to`); иначе пусто — отвечать отправителю.
pub(crate) fn effective_reply_to(from_name: Option<&str>, from_email: Option<&str>, reply_to: &[aiowa_core::Participant]) -> Vec<ParticipantDto> {
    let from = from_email.map(|e| aiowa_core::Participant { name: from_name.map(String::from), email: Some(e.to_string()), routing_type: None });
    aiowa_core::effective_reply_to(from.as_ref(), reply_to).into_iter().map(|p| ParticipantDto { name: p.name, email: p.email }).collect()
}

#[tauri::command]
fn account_info(state: State<'_, AppState>) -> AccountDto {
    AccountDto { host: state.host.clone(), user: state.user.clone(), db_path: state.store.path().display().to_string() }
}

// Дерево, список, ключ беседы и поиск окно читает и во время синхронизации (живое окно): не в главном
// потоке, иначе ожидание базы, пока движок пишет пачку, замораживало бы окно.
#[tauri::command(async)]
fn folders(state: State<'_, AppState>) -> CmdResult<Vec<FolderDto>> {
    let list = state.store.list_folders(state.account).map_err(err)?;
    Ok(folder_dtos(&list))
}

#[tauri::command(async)]
fn conversations(state: State<'_, AppState>, folder_pk: i64, after_ms: Option<i64>, after_pk: Option<i64>, limit: Option<usize>) -> CmdResult<Vec<ConversationDto>> {
    let cursor = match (after_ms, after_pk) {
        (Some(last_received_at), Some(conversation_pk)) => Some(ConversationCursor { last_received_at, conversation_pk }),
        _ => None,
    };
    let rows = state.store.conversations_page(state.account, folder_pk, cursor, limit.unwrap_or(100).clamp(1, 500)).map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|r| ConversationDto {
            conversation_pk: r.conversation_pk,
            topic: r.topic,
            participants: r.participants,
            item_count: r.item_count,
            unread_count: r.unread_count,
            total_count: r.total_count,
            last_received_ms: ms(r.last_received_at),
            has_attachments: r.has_attachments,
            has_flagged: r.has_flagged,
        })
        .collect())
}

/// Письма беседы из локальной базы, без сети: тела, которых нет, приходят с `body_html = null`
/// и догружаются командой `message_fetch`; инлайновые картинки подставляются только из кэша.
/// Чтение файлов кэша и base64 идут в блокирующем потоке: синхронная команда выполнялась бы
/// в потоке окна и замораживала бы его на длинных беседах с картинками.
#[tauri::command]
async fn conversation(state: State<'_, AppState>, conversation_pk: i64) -> CmdResult<Vec<MessageDto>> {
    let store = state.store.clone();
    let account = state.account;
    tauri::async_runtime::spawn_blocking(move || {
        let heads = store.conversation_messages(account, conversation_pk).map_err(err)?;
        heads.into_iter().map(|h| build_message(&store, h)).collect::<CmdResult<Vec<_>>>()
    })
    .await
    .map_err(err)?
}

/// Дешёвый ключ беседы (один запрос без тел и файлов): окно сравнивает его с открытой беседой
/// после синхронизации и перечитывает письма только при реальных изменениях.
#[tauri::command(async)]
fn conversation_key(state: State<'_, AppState>, conversation_pk: i64) -> CmdResult<Vec<MessageKeyDto>> {
    let keys = state.store.conversation_keys(state.account, conversation_pk).map_err(err)?;
    Ok(keys.into_iter().map(|k| MessageKeyDto { item_pk: k.item_pk, body_state: k.body_state, is_read: k.is_read, cached: k.cached, is_flagged: k.is_flagged }).collect())
}

#[tauri::command(async)]
fn search(state: State<'_, AppState>, query: String, limit: Option<usize>) -> CmdResult<Vec<SearchHitDto>> {
    let hits = state.store.search(state.account, &query, limit.unwrap_or(50).clamp(1, 200)).map_err(err)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        out.push(SearchHitDto {
            item_pk: h.item_pk,
            conversation_pk: h.conversation_pk,
            subject: h.subject,
            from_name: h.from_name,
            from_email: h.from_email,
            received_ms: ms(h.received_at),
            folder_name: h.folder_name,
            snippet: h.snippet,
            attachment_name: h.attachment_name,
        });
    }
    Ok(out)
}

/// Состояния почты, которым нужна база: есть только в режиме почты.
struct MailParts {
    state: AppState,
    sync: Arc<sync::SyncState>,
    notify: Arc<notify::NotifyState>,
    reminders: Arc<reminders::RemindersState>,
}

/// Фоновые службы почты для запуска в `setup` и остановки при выходе.
#[derive(Clone)]
struct MailServices {
    sync: Arc<sync::SyncState>,
    notify: Arc<notify::NotifyState>,
    reminders: Arc<reminders::RemindersState>,
}

impl MailServices {
    /// Штатный выход служб почты. Сессия на сервере одна на процесс: закрыть канал уведомлений,
    /// отправить на сервер отложенные действия (окно отмены ещё не истекло), остановить
    /// синхронизацию и выйти из сессии, иначе сервер накапливает сессии до MapiExceptionSessionLimit.
    /// Блокирует: вызывается с главного потока (`RunEvent::Exit`) или из блокирующего потока перед
    /// запуском установщика обновления (`updates.rs`), который закрывает процесс без `RunEvent::Exit`.
    /// Повторный вызов ничего не делает: каждая служба уже остановлена.
    fn shutdown(&self, app: &tauri::AppHandle) {
        self.reminders.shutdown();
        self.notify.shutdown();
        staged::commit_all_blocking(app);
        self.sync.shutdown();
    }
}

/// Открывает базу учётной записи и строит состояния почты. Блок мастера (`initial`) применяется
/// к базе здесь, до первого чтения настроек. Ошибка — текст для окна: процесс не падает, а
/// стартует мастером с этим текстом (иначе негодный путь в конфиге ронял бы каждый запуск).
fn open_mail(boot: &config::MailBoot, config_dir: &Path) -> Result<MailParts, String> {
    let store = Arc::new(Store::open(&boot.db_path).map_err(|e| format!("Не удалось открыть локальную базу {}: {e}", boot.db_path.display()))?);
    // Логин в базе мог быть записан в другой форме того же ящика (`savvidi@ngenergo` вместо `savvidi`).
    let account = store.resolve_account(&boot.host, &boot.user).map_err(|e| format!("Не удалось прочитать учётную запись в базе {}: {e}", boot.db_path.display()))?;
    if let Some(initial) = boot.initial {
        settings::apply_initial(&store, account, config_dir, initial);
    }
    let sync = Arc::new(sync::SyncState::new(store.clone(), account, boot.host.clone(), boot.user.clone()));
    let notify = Arc::new(notify::NotifyState::new(store.clone(), account, sync.clone()));
    let reminders = Arc::new(reminders::RemindersState::new(store.clone(), account));
    Ok(MailParts { state: AppState { store, account, host: boot.host.clone(), user: boot.user.clone() }, sync, notify, reminders })
}

/// Выключает браузерные клавиши WebView2 в главном окне (`AreBrowserAcceleratorKeysEnabled`):
/// F5 и Ctrl+R (перезагрузка), Ctrl+F и F3 (поиск по странице), Ctrl+P (печать окна), F12 и
/// Ctrl+Shift+C (DevTools). Обработчики окна гасят F5 и Ctrl+R сами, но пока фокус внутри
/// PDF-предпросмотра (рамка с чужого источника `aiowa-att.localhost`), их keydown окно не видит, и
/// браузер перезагружал бы окно вместе с набранным письмом.
///
/// Клавиши страницы это не отнимает. По документации WebView2 настройка выключает действия
/// браузера по клавишам, а не сами события: правка и перемещение по тексту (Ctrl+C/V/X/A/Z,
/// Home/End) остаются, событие хоста `AcceleratorKeyPressed` приходит для всех клавиш. Действие
/// браузера выполняется после страницы, если та не сделала `preventDefault` (так окно и сейчас
/// гасит F5 и Ctrl+R из JS); без него keydown приходит в JS как прежде. Поэтому Ctrl+R «ответить»,
/// Ctrl+P «печать письма» (`window.print()` печатной рамки, а не клавиша браузера), Ctrl+K и
/// Ctrl+N работают. Масштаб Ctrl+плюс/минус выключен и без этого (`zoomHotkeysEnabled` = false).
///
/// Вызывается из `setup` сразу после создания окна, в том же проходе главного потока: хост ещё не
/// получил `NavigationStarting` первой загрузки, а изменения настроек до этого события WebView2
/// применяет к этой же загрузке (после — только со следующей). В старых сборках WebView2 без
/// `ICoreWebView2Settings3` интерфейса нет; это и любая другая ошибка уходят в журнал, окно
/// работает с клавишами браузера.
#[cfg(windows)]
fn disable_browser_accelerator_keys(app: &tauri::AppHandle) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
    use windows::core::Interface as _;

    let Some(window) = app.get_webview_window("main") else {
        warn!("главного окна нет: браузерные клавиши WebView2 не выключены");
        return;
    };
    // Замыкание выполняется в главном потоке, где живёт контроллер WebView2.
    let sent = window.with_webview(|wv| {
        // SAFETY: вызовы COM на живом контроллере окна в его потоке; результаты проверяются.
        let res = unsafe { wv.controller().CoreWebView2().and_then(|w| w.Settings()).and_then(|s| s.cast::<ICoreWebView2Settings3>()).and_then(|s| s.SetAreBrowserAcceleratorKeysEnabled(false)) };
        match res {
            Ok(()) => info!("браузерные клавиши WebView2 выключены"),
            Err(e) => warn!(error = %e, "не удалось выключить браузерные клавиши WebView2"),
        }
    });
    if let Err(e) = sent {
        warn!(error = %e, "не удалось выключить браузерные клавиши WebView2: окно недоступно");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_dir = logging::init();
    info!(version = env!("CARGO_PKG_VERSION"), logs = ?log_dir, "AIOWA запущен");
    let config_dir = config::config_dir();
    let env = config::EnvAccount::from_env();
    // В режиме разработки (обе переменные заданы) конфиг не читается: файл не трогаем.
    let cfg = if env.host.is_some() && env.user.is_some() { None } else { config::load(&config_dir) };
    let (setup_state, mail) = match config::resolve(&env, cfg.as_ref()) {
        config::Boot::Mail(boot) => {
            info!(host = %boot.host, user = %boot.user, db = %boot.db_path.display(), dev_env = boot.dev_env, "старт в режиме почты");
            match open_mail(&boot, &config_dir) {
                Ok(parts) => (setup::SetupState::mail(&boot, config_dir), Some(parts)),
                Err(e) => {
                    error!(error = %e, "база не открылась: старт мастера с текстом ошибки");
                    (setup::SetupState::boot_failed(&boot, config_dir, e), None)
                }
            }
        }
        config::Boot::Setup => {
            info!(config = %config::config_path(&config_dir).display(), "учётная запись не настроена: старт мастера первого запуска");
            (setup::SetupState::first_run(config_dir), None)
        }
    };

    let mut builder = tauri::Builder::default()
        // Первым плагином: второй запуск (ярлык, повторный клик) не открывает окно и сессию OWA
        // заново, а поднимает уже открытое окно и выходит. Блокировка снимается на штатном выходе
        // (RunEvent::Exit) — до того, как перезапуск после мастера запустит процесс снова.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            info!("повторный запуск: поднимаю уже открытое окно");
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        // Окно не уходит со страницы приложения: внешние адреса открываются в браузере.
        .plugin(navigation::guard())
        // Тосты Windows о новых письмах показывает канал уведомлений из Rust; команд плагина окно не зовёт.
        .plugin(tauri_plugin_notification::init())
        // Обновление приложения: окно зовёт команды `updates_*` (`updates.rs`), не команды плагина.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Предпросмотр вложений: файл и база читаются не в потоке окна. Отдаются только файлы кэша
        // вложений текущей учётной записи по числовому ключу (`preview::serve`). В режиме первого
        // запуска базы нет — 503.
        .register_asynchronous_uri_scheme_protocol(preview::SCHEME, |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let method = request.method().clone();
            let path = request.uri().path().to_string();
            let origin = request.headers().get(tauri::http::header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_owned);
            tauri::async_runtime::spawn_blocking(move || {
                let response = match app.try_state::<AppState>() {
                    Some(state) => {
                        let dev = if tauri::is_dev() { app.config().build.dev_url.as_ref().map(|u| u.origin().ascii_serialization()) } else { None };
                        preview::serve(&state.store, state.account, &method, &path, preview::allowed_origin(origin.as_deref(), dev.as_deref()))
                    }
                    None => preview::unavailable(),
                };
                responder.respond(response);
            });
        })
        .manage(setup_state)
        // Состояния без базы — в обоих режимах (обработчик закрытия окна читает `CloseState`).
        .manage(ondemand::OnDemand::default())
        .manage(write::WriteState::default())
        .manage(staged::StagedState::default())
        .manage(compose::ComposeState::default())
        .manage(calendar::CalendarState::default())
        .manage(calendar_write::CalendarWriteState::default())
        .manage(directory::DirectoryState::default())
        .manage(updates::UpdatesState::default())
        .manage(CloseState::default());
    let services = match mail {
        Some(parts) => {
            let services = MailServices { sync: parts.sync.clone(), notify: parts.notify.clone(), reminders: parts.reminders.clone() };
            builder = builder.manage(parts.state).manage(parts.sync).manage(parts.notify).manage(parts.reminders);
            // Шаги выхода перед установщиком обновления (`updates.rs`) берут службы отсюда.
            builder = builder.manage(services.clone());
            Some(services)
        }
        None => None,
    };

    let app = builder
        // Окно из конфигурации к этому моменту уже создано: заголовок «AIOWA (N)» и значок
        // непрочитанных выставляются по базе сразу, не дожидаясь первой удачной синхронизации.
        // Планировщик напоминаний стартует здесь же: он читает только кэш, сети ему не нужно.
        // Браузерные клавиши WebView2 выключаются здесь же, до первой загрузки страницы окна.
        .setup({
            let services = services.clone();
            move |app| {
                #[cfg(windows)]
                disable_browser_accelerator_keys(app.handle());
                if let Some(s) = &services {
                    s.notify.refresh_unread(app.handle());
                    s.reminders.start(app.handle().clone());
                    // Фоновая проверка обновлений: через минуту и раз в сутки, если не выключена.
                    updates::start(app.handle().clone());
                }
                Ok(())
            }
        })
        .on_window_event(|w, ev| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = ev
                && w.state::<CloseState>().guarded.load(Ordering::Relaxed)
            {
                api.prevent_close();
                if let Err(e) = w.emit("close-requested", ()) {
                    warn!(error = %e, "не удалось отправить событие close-requested");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            account_info,
            folders,
            conversations,
            conversation,
            conversation_key,
            search,
            sync::sync_start,
            sync::sync_status,
            sync::sync_cancel,
            ondemand::message_fetch,
            ondemand::attachment_open,
            ondemand::attachment_save_as,
            preview::attachment_preview_url,
            ondemand::open_external,
            write::items_mark_read,
            write::items_set_flag,
            write::items_move,
            write::items_delete,
            write::conversation_item_pks,
            staged::items_stage,
            staged::items_undo,
            staged::items_commit,
            folders::folder_create,
            folders::folder_rename,
            folders::folder_delete,
            compose::compose_context,
            compose::compose_send,
            compose::compose_save_draft,
            compose::compose_open_draft,
            compose::compose_autosave,
            compose::compose_discard_draft,
            compose::recipients_suggest,
            compose::own_addresses,
            compose::signature_get,
            compose::signature_set,
            compose::signature_import_owa,
            compose::attachments_pick,
            directory::directory_search,
            directory::contact_card,
            directory::directory_expand,
            directory::directory_capabilities,
            calendar::calendar_events,
            calendar::calendar_event_details,
            calendar_write::calendar_create,
            calendar_write::calendar_update,
            calendar_write::calendar_delete,
            calendar_write::calendar_respond,
            calendar_write::meeting_info,
            calendar_write::meeting_respond,
            reminders::reminders_pending,
            reminders::reminder_dismiss,
            reminders::reminder_snooze,
            notify::notify_status,
            notify::notify_set_toasts,
            window_close_guard,
            window_close_confirmed,
            setup::app_mode,
            setup::setup_normalize_host,
            setup::setup_test_login,
            setup::setup_save,
            settings::settings_get,
            settings::settings_set,
            settings::settings_change_password,
            settings::settings_forget_account,
            settings::settings_open_data_dir,
            // обновление приложения (updates.rs)
            updates::updates_status,
            updates::updates_check,
            updates::updates_set_auto,
            updates::updates_install,
            // Одна база на ящик, раздел «Данные» (data.rs, mailbox.rs).
            data::data_stores,
            data::data_usage,
            data::data_store_open_dir,
            data::data_store_delete
        ])
        .build(tauri::generate_context!())
        .expect("ошибка запуска приложения");
    app.run(move |app, event| {
        // Сессия на сервере одна на процесс: при выходе остановить службы почты и выйти из сессии
        // (`MailServices::shutdown`). В режиме первого запуска останавливать нечего. Перезапуск
        // после мастера и «Забыть учётную запись» идёт через этот же выход
        // (`AppHandle::request_restart`); установка обновления — нет, она зовёт те же шаги сама.
        if let tauri::RunEvent::Exit = event
            && let Some(s) = &services
        {
            s.shutdown(app);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_keeps_layout_and_drops_scripts() {
        let html = r##"<div style="color:red"><script>alert(1)</script><a href="javascript:x" onclick="y">ссылка</a><img src="https://t.example/p.gif"><table><tr><td bgcolor="#eee">ячейка</td></tr></table></div>"##;
        let out = sanitize_html(html, &Arc::new(InlineMap::new()));
        assert!(!out.contains("script"));
        assert!(!out.contains("onclick"));
        assert!(!out.contains("javascript:"));
        assert!(out.contains("style=\"color:red\""));
        assert!(out.contains("bgcolor"));
        assert!(out.contains("<img"));
    }

    #[test]
    fn cid_normalization_is_tolerant() {
        assert_eq!(normalize_cid("image001.jpg@01DD5D86.89427E20"), "image001.jpg@01dd5d86.89427e20");
        assert_eq!(normalize_cid("cid:Image001.jpg@01DD5D86.89427E20"), "image001.jpg@01dd5d86.89427e20");
        assert_eq!(normalize_cid("CID:<image001.jpg@01DD5D86.89427E20>"), "image001.jpg@01dd5d86.89427e20");
        assert_eq!(normalize_cid("cid:%3Cimage001.jpg%4001DD5D86.89427E20%3E"), "image001.jpg@01dd5d86.89427e20");
        assert_eq!(normalize_cid(" <4b41b9e6-0a9e-427f-a922-e45e31ac31a0> "), "4b41b9e6-0a9e-427f-a922-e45e31ac31a0");
        assert_eq!(normalize_cid("cid:"), "");
    }

    #[test]
    fn sanitizer_inlines_known_cids_and_drops_unknown() {
        let mut map = InlineMap::new();
        map.insert("image001.jpg@01dd5d86.89427e20".to_string(), "data:image/jpeg;base64,/9j/".to_string());
        let html = r#"<p><img src="cid:Image001.jpg@01DD5D86.89427E20" width="10"><img src="CID:%3Cimage001.jpg@01DD5D86.89427E20%3E"><img src="cid:nope@x" alt="нет"><img src="https://t.example/p.gif"></p>"#;
        let out = sanitize_html(html, &Arc::new(map));
        assert_eq!(out.matches("src=\"data:image/jpeg;base64,/9j/\"").count(), 2, "{out}");
        assert!(!out.contains("cid:"), "{out}");
        assert!(out.contains("alt=\"нет\""), "картинка без источника остаётся, но без src: {out}");
        assert!(out.contains("https://t.example/p.gif"));
    }

    #[test]
    fn referenced_cids_are_collected_from_html() {
        let html = r#"<p><img src="cid:Image001.jpg@01DD5D86.89427E20"><img src='CID:%3Cimage002.png%3E'><div style="background:url(cid:bg@x)">
        <img src="https://t.example/p.gif"><img src="cid:"></div>"#;
        let got = referenced_cids(html);
        let want: HashSet<String> = ["image001.jpg@01dd5d86.89427e20", "image002.png", "bg@x"].into_iter().map(String::from).collect();
        assert_eq!(got, want);
        assert!(referenced_cids("<p>без картинок</p>").is_empty());
        assert!(referenced_cids("<img src=\"cid:я@кириллица\">").contains("я@кириллица"), "не-ASCII не ломает смещения");
    }

    #[test]
    fn image_mime_from_type_or_signature() {
        assert_eq!(image_mime(Some("image/PNG; charset=binary"), b""), Some("image/png".into()));
        assert_eq!(image_mime(Some("application/pdf"), b"\x89PNG"), None, "не картинка по типу — не подставляем");
        assert_eq!(image_mime(None, b"\x89PNG\r\n"), Some("image/png".into()));
        assert_eq!(image_mime(None, b"\xFF\xD8\xFF\xE0"), Some("image/jpeg".into()));
        assert_eq!(image_mime(None, b"GIF89a"), Some("image/gif".into()));
        assert_eq!(image_mime(None, b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp".into()));
        assert_eq!(image_mime(None, b"%PDF-1.4"), None);
    }
}
